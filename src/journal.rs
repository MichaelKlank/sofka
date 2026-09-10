//! Session-local action journal.
//!
//! A bounded session history with optional JSON Lines persistence.
//! Entries record actions started, not confirmed results. Callers supply
//! identifiers only. File output uses the standard redaction rules.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use crate::config::JournalConfig;
use k8s_openapi::jiff::Timestamp;

struct Sink {
    tx: Option<mpsc::SyncSender<String>>,
    done: mpsc::Receiver<()>,
    error: Arc<Mutex<Option<String>>>,
    max_bytes: u64,
}

impl Sink {
    fn new(cfg: &JournalConfig) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<String>(4096);
        let (done_tx, done) = mpsc::channel();
        let error = Arc::new(Mutex::new(None));
        let worker_error = error.clone();
        let path = cfg.path();
        let max_bytes = cfg.max_bytes();
        std::thread::Builder::new()
            .name("sofka-journal".into())
            .spawn(move || {
                let mut writer = None;
                for line in rx {
                    let result = (|| {
                        if writer.is_none() {
                            writer = Some(crate::applog::Writer::open(&path, max_bytes)?);
                        }
                        writer.as_mut().unwrap().write(&line)
                    })();
                    if let Err(e) = result {
                        *worker_error.lock().unwrap() = Some(format!(
                            "journal entry not saved: {}",
                            crate::redact::text(&e.to_string())
                        ));
                    }
                }
                let _ = done_tx.send(());
            })
            .map_err(|e| format!("journal writer could not start: {e}"))?;
        Ok(Self {
            tx: Some(tx),
            done,
            error,
            max_bytes,
        })
    }

    fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.tx.take();
        if self.done.recv_timeout(Duration::from_millis(300)).is_err() {
            *self.error.lock().unwrap() =
                Some("journal shutdown timed out; some entries may not be saved".into());
        }
    }
}

/// How many entries to keep before dropping the oldest.
const MAX_ENTRIES: usize = 500;

/// One recorded action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Epoch seconds when the action was taken.
    pub at: i64,
    pub context: String,
    /// The verb (`delete`, `scale to 3`, `shell`, `plugin: argocd-sync`, …).
    pub action: String,
    /// The object(s) acted on (`pods/api in prod`, `3 deployments`, …).
    pub target: String,
}

/// The session's action log, newest last.
#[derive(Default)]
pub struct Journal {
    entries: VecDeque<Entry>,
    sink: Option<Sink>,
}

impl Journal {
    pub fn configure(&mut self, cfg: &JournalConfig) -> Result<(), String> {
        if let Some(error) = self.shutdown() {
            return Err(error);
        }
        if cfg.enabled {
            self.sink = Some(Sink::new(cfg)?);
        }
        Ok(())
    }

    pub fn take_error(&self) -> Option<String> {
        self.sink.as_ref().and_then(Sink::take_error)
    }

    pub fn shutdown(&mut self) -> Option<String> {
        let sink = self.sink.take()?;
        let error = sink.error.clone();
        drop(sink);
        error.lock().unwrap().take()
    }

    /// Record an action. Callers pass only identifiers — never secret input or
    /// decoded Secret values.
    pub fn record(&mut self, context: &str, action: impl Into<String>, target: impl Into<String>) {
        let (action, target) = (action.into(), target.into());
        crate::log_info!(
            "action",
            context = context,
            action = action,
            target = target
        );
        let entry = Entry {
            at: Timestamp::now().as_second(),
            context: context.to_string(),
            action,
            target,
        };
        if let Some(sink) = &self.sink {
            let line = serde_json::json!({
                "at": Timestamp::from_second(entry.at).unwrap().to_string(),
                "context": crate::redact::text(&entry.context),
                "action": crate::redact::text(&entry.action),
                "target": crate::redact::text(&entry.target),
            })
            .to_string()
                + "\n";
            if line.len() as u64 > sink.max_bytes {
                *sink.error.lock().unwrap() =
                    Some("journal entry not saved: entry exceeds the file size limit".into());
            } else if sink.tx.as_ref().unwrap().try_send(line).is_err() {
                *sink.error.lock().unwrap() =
                    Some("journal entry not saved: writer queue is full or closed".into());
            }
        }
        self.entries.push_back(entry);
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The journal as display lines, newest first, with a header.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{:<19}  {:<24}  {:<18}  TARGET",
            "WHEN", "ACTION", "CONTEXT"
        )];
        if self.entries.is_empty() {
            out.push("(no actions recorded this session)".into());
            return out;
        }
        for e in self.entries.iter().rev() {
            out.push(format!(
                "{:<19}  {:<24}  {:<18}  {}",
                clock(e.at),
                e.action,
                e.context,
                e.target
            ));
        }
        out
    }
}

/// Format an epoch second as `MM-DD HH:MM:SS` (UTC, like the events view).
fn clock(at: i64) -> String {
    match Timestamp::from_second(at) {
        Ok(ts) => {
            let s = ts.to_string(); // 2026-07-13T08:45:12Z
            let date = s.split('T').next().unwrap_or("");
            let day = date.get(5..).unwrap_or(date); // MM-DD
            let time = s
                .split_once('T')
                .map(|(_, t)| t.split(['.', 'Z']).next().unwrap_or(t))
                .unwrap_or("");
            format!("{day} {time}")
        }
        Err(_) => at.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(name: &str) -> JournalConfig {
        let dir = std::env::temp_dir().join(format!("sofka-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        JournalConfig {
            enabled: true,
            file: Some(dir.join("actions.jsonl")),
            max_size_mb: 0,
        }
    }

    #[test]
    fn file_redacts_credentials_and_escapes_newlines() {
        let cfg = test_config("redaction");
        let mut journal = Journal::default();
        journal.configure(&cfg).unwrap();
        journal.record(
            "https://user:password@example.com",
            "plugin: test\nnext",
            "Bearer secret-token",
        );
        assert!(journal.shutdown().is_none());
        let text = std::fs::read_to_string(cfg.path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains("password"));
        assert!(!text.contains("secret-token"));
        let entry: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(entry["action"], "plugin: test\nnext");
        std::fs::remove_dir_all(cfg.path().parent().unwrap()).unwrap();
    }

    #[test]
    fn journal_rotation_is_bounded_and_keeps_complete_json() {
        let cfg = test_config("rotation");
        let mut journal = Journal::default();
        journal.configure(&cfg).unwrap();
        for i in 0..150 {
            journal.record("prod", format!("edit {i}"), "x".repeat(1024));
        }
        assert!(journal.shutdown().is_none());
        let current = std::fs::read_to_string(cfg.path()).unwrap();
        let previous = std::fs::read_to_string(cfg.path().with_extension("jsonl.1")).unwrap();
        for text in [&current, &previous] {
            assert!(text.len() as u64 <= cfg.max_bytes());
            for line in text.lines() {
                serde_json::from_str::<serde_json::Value>(line).unwrap();
            }
        }
        assert!(current.contains("edit 149"));
        assert!(!cfg.path().with_extension("jsonl.2").exists());
        std::fs::remove_dir_all(cfg.path().parent().unwrap()).unwrap();
    }

    #[test]
    fn oversized_entries_report_failure_and_stay_in_memory() {
        let cfg = test_config("oversize");
        let mut journal = Journal::default();
        journal.configure(&cfg).unwrap();
        journal.record("prod", "edit", "x".repeat(cfg.max_bytes() as usize));
        assert!(
            journal
                .shutdown()
                .unwrap()
                .contains("exceeds the file size limit")
        );
        assert_eq!(journal.len(), 1);
        assert!(!cfg.path().exists());
    }

    #[test]
    fn records_and_formats_newest_first() {
        let mut j = Journal::default();
        assert!(j.is_empty());
        j.record("prod", "delete", "pods/a in default");
        j.record("prod", "scale to 3", "deployments/api in prod");
        assert_eq!(j.len(), 2);
        let lines = j.lines();
        // Header, then newest first.
        assert!(lines[0].contains("ACTION"));
        assert!(lines[1].contains("scale to 3"), "{:?}", lines);
        assert!(lines[2].contains("delete"), "{:?}", lines);
    }

    #[test]
    fn bounded_to_the_cap() {
        let mut j = Journal::default();
        for i in 0..(MAX_ENTRIES + 25) {
            j.record("c", "delete", format!("pods/p{i}"));
        }
        assert_eq!(j.len(), MAX_ENTRIES);
        // Oldest dropped: the newest is p<max+24>.
        assert!(j.lines()[1].contains(&format!("p{}", MAX_ENTRIES + 24)));
    }

    #[test]
    fn empty_journal_says_so() {
        let j = Journal::default();
        assert!(j.lines().iter().any(|l| l.contains("no actions recorded")));
    }
}
