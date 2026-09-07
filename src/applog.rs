//! Structured application logging.
//!
//! Off by default. When `[logging] level` (or `SOFKA_LOG`) turns it on, every
//! event is one logfmt line — `ts=… level=… event=… key=value …` — appended to
//! a rotating file under the state directory. Machine-parseable by design: the
//! point of a log from a TUI is that you read it *after* the session, usually
//! with `grep`.
//!
//! Two properties matter more than the format:
//!
//! * **It never stalls the UI.** Lines are handed to a writer thread through a
//!   bounded queue; a full queue drops the line and counts the drop rather than
//!   blocking the event loop behind a filesystem that has stopped answering.
//!   Same reasoning as [`crate::state_writer`], same shutdown grace.
//! * **It never writes a credential.** Every field value goes through
//!   [`crate::redact::text`] on the way in, so a bearer token, a kubeconfig
//!   credential, or a URL with userinfo is replaced before it can reach the
//!   disk — including when it arrives inside an error string from a library
//!   that echoed the request back.

use std::fmt::{Display, Write as _};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use k8s_openapi::jiff::Timestamp;

/// Lines the writer may fall behind by before it starts dropping them. A
/// session logging at `debug` writes a few hundred lines a minute; 4096 is
/// several minutes of backlog, far past the point where a stalled disk is the
/// real problem.
const QUEUE_DEPTH: usize = 4096;

/// How long [`shutdown`] waits for the writer to drain before giving up. The
/// terminal is already restored by then; nothing may hold the process open.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

/// Environment override for the configured level, e.g. `SOFKA_LOG=debug`.
pub const LEVEL_ENV: &str = "SOFKA_LOG";

/// Verbosity. Ordered: a level is emitted when it is at most the active one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    #[default]
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    /// Parse a configured level name. `None` for anything else, so the caller
    /// can warn and fall back rather than silently logging the wrong amount.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Some(Self::Off),
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

static LEVEL: AtomicU8 = AtomicU8::new(Level::Off as u8);

/// Whether `level` would be written. One relaxed load — the macros call this
/// before formatting anything, so a disabled log site costs a compare.
#[inline]
pub fn enabled(level: Level) -> bool {
    level != Level::Off && (level as u8) <= LEVEL.load(Ordering::Relaxed)
}

/// The active level (`Off` when logging is disabled).
pub fn level() -> Level {
    match LEVEL.load(Ordering::Relaxed) {
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        4 => Level::Debug,
        5 => Level::Trace,
        _ => Level::Off,
    }
}

/// The effective level: `SOFKA_LOG` overrides the config file for one run.
///
/// The returned warning is only ever about the environment variable — an
/// unparseable *config* level is [`crate::config::logging_warnings`]'s to
/// report, so a typo is named once and by the file it is in.
pub fn resolve_level(configured: &str) -> (Level, Option<String>) {
    resolve_level_from(configured, std::env::var(LEVEL_ENV).ok().as_deref())
}

/// [`resolve_level`] with the environment passed in, so the precedence rule is
/// testable without mutating process state.
fn resolve_level_from(configured: &str, env: Option<&str>) -> (Level, Option<String>) {
    let from_config = Level::parse(configured).unwrap_or_default();
    match env {
        Some(raw) if !raw.trim().is_empty() => match Level::parse(raw) {
            Some(level) => (level, None),
            None => (
                from_config,
                Some(format!(
                    "{LEVEL_ENV}: level '{raw}' is not off/error/warn/info/debug/trace; ignored"
                )),
            ),
        },
        _ => (from_config, None),
    }
}

enum Entry {
    Line(String),
    /// Drain-and-flush marker; the acknowledgement is what [`shutdown`] waits
    /// on, bounded by [`SHUTDOWN_GRACE`].
    Flush(SyncSender<()>),
}

struct Sink {
    tx: SyncSender<Entry>,
    path: PathBuf,
    written: AtomicU64,
    dropped: AtomicU64,
}

static SINK: OnceLock<Sink> = OnceLock::new();

/// Start logging at `level`, appending to `path` and rotating it at
/// `max_bytes`. A no-op at `Level::Off` — no file is created and no thread is
/// spawned, so the default configuration costs nothing.
///
/// Called once, from startup. A second call is ignored (the first sink stays),
/// which keeps the "one log file per process" invariant that rotation assumes.
pub fn init(level: Level, path: PathBuf, max_bytes: u64) -> Result<(), String> {
    if level == Level::Off {
        LEVEL.store(Level::Off as u8, Ordering::Relaxed);
        return Ok(());
    }
    let mut writer = Writer::open(&path, max_bytes)
        .map_err(|e| format!("opening log file {}: {e}", path.display()))?;
    let (tx, rx) = sync_channel(QUEUE_DEPTH);
    std::thread::Builder::new()
        .name("sofka-log".into())
        .spawn(move || run_writer(&mut writer, rx))
        .map_err(|e| format!("starting log writer: {e}"))?;
    let _ = SINK.set(Sink {
        tx,
        path,
        written: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
    });
    LEVEL.store(level as u8, Ordering::Relaxed);
    Ok(())
}

/// Flush the queue and stop logging. Bounded by [`SHUTDOWN_GRACE`]: a log file
/// on a wedged filesystem must not outlive the TUI it was recording.
pub fn shutdown() {
    LEVEL.store(Level::Off as u8, Ordering::Relaxed);
    let Some(sink) = SINK.get() else { return };
    let (ack_tx, ack_rx) = sync_channel(1);
    if sink.tx.try_send(Entry::Flush(ack_tx)).is_ok() {
        let _ = ack_rx.recv_timeout(SHUTDOWN_GRACE);
    }
}

/// What the diagnostics view reports about logging.
pub struct Status {
    pub level: Level,
    /// `None` when logging is off (no file is opened).
    pub path: Option<PathBuf>,
    pub written: u64,
    /// Lines dropped because the writer fell behind — always zero in practice,
    /// and the number to look at first if the log has gaps.
    pub dropped: u64,
}

pub fn status() -> Status {
    let sink = SINK.get();
    Status {
        level: level(),
        path: sink.map(|s| s.path.clone()),
        written: sink.map_or(0, |s| s.written.load(Ordering::Relaxed)),
        dropped: sink.map_or(0, |s| s.dropped.load(Ordering::Relaxed)),
    }
}

/// Emit one event. Prefer the [`log_info!`](crate::log_info) family, which
/// skips formatting entirely when the level is disabled.
pub fn emit(level: Level, event: &str, fields: &[(&str, &dyn Display)]) {
    let Some(sink) = SINK.get() else { return };
    let line = format_line(level, event, fields);
    match sink.tx.try_send(Entry::Line(line)) {
        Ok(()) => sink.written.fetch_add(1, Ordering::Relaxed),
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            sink.dropped.fetch_add(1, Ordering::Relaxed)
        }
    };
}

/// Render one logfmt line, redacting every value.
fn format_line(level: Level, event: &str, fields: &[(&str, &dyn Display)]) -> String {
    let mut line = String::with_capacity(96 + fields.len() * 24);
    // jiff honours the precision flag: milliseconds is enough to line a log up
    // against a watch event without three extra digits of noise per line.
    let _ = write!(
        line,
        "ts={:.3} level={} event={event}",
        Timestamp::now(),
        level.as_str()
    );
    // One scratch buffer for every field: values have to be rendered before
    // they can be redacted, and redaction borrows when there is nothing to
    // replace, which is almost always.
    let mut scratch = String::new();
    for (key, value) in fields {
        scratch.clear();
        let _ = write!(scratch, "{value}");
        line.push(' ');
        line.push_str(key);
        line.push('=');
        push_value(&mut line, &scratch);
    }
    line.push('\n');
    line
}

/// Append one redacted, logfmt-quoted value.
fn push_value(out: &mut String, raw: &str) {
    let value = crate::redact::text(raw);
    let plain = !value.is_empty()
        && !value
            .bytes()
            .any(|b| b <= b' ' || matches!(b, b'"' | b'=' | b'\\'));
    if plain {
        out.push_str(&value);
        return;
    }
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The log file and its rotation. All sessions use the same lock file.
/// Only writer threads wait for this lock.
struct Writer {
    path: PathBuf,
    max_bytes: u64,
    file: File,
    lock: File,
    size: u64,
}

impl Writer {
    fn open(path: &Path, max_bytes: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(lock_path))?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1),
            file,
            lock,
            size,
        })
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        // The lock file stays in place when any session rotates the log.
        self.lock.lock()?;
        let result = self.write_locked(line);
        let unlocked = self.lock.unlock();
        result.and(unlocked)
    }

    fn write_locked(&mut self, line: &str) -> std::io::Result<()> {
        // Another session can have replaced the file since the previous line.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = file;
        if self.size + line.len() as u64 > self.max_bytes && self.size > 0 {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.size += line.len() as u64;
        Ok(())
    }

    /// Keep exactly one previous generation (`<file>.1`). A log this small is
    /// read by a human right after the session that produced it; an archive
    /// policy would only make the state directory grow without bound.
    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        std::fs::rename(&self.path, PathBuf::from(rotated))?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.file = file;
        self.size = 0;
        Ok(())
    }
}

/// Drain the queue on the writer thread. Each line is flushed while the
/// rotation lock is held, so other sessions see its size before they write.
fn run_writer(writer: &mut Writer, rx: Receiver<Entry>) {
    while let Ok(entry) = rx.recv() {
        let mut ack = handle(writer, entry);
        while let Ok(entry) = rx.try_recv() {
            ack = handle(writer, entry).or(ack);
        }
        let _ = writer.file.flush();
        if let Some(ack) = ack {
            let _ = ack.try_send(());
        }
    }
    let _ = writer.file.flush();
}

fn handle(writer: &mut Writer, entry: Entry) -> Option<SyncSender<()>> {
    match entry {
        // A write error is unreportable from here (the UI owns the screen and
        // the log is what would have carried the news). Dropping is correct:
        // the next line retries, and `:info` shows the file's line count.
        Entry::Line(line) => {
            let _ = writer.write(&line);
            None
        }
        Entry::Flush(ack) => Some(ack),
    }
}

/// Emit a structured event at `level`, formatting the fields only if that
/// level is enabled.
///
/// ```ignore
/// log_event!(Level::Info, "watch.start", kind = "pods", ns = "default");
/// ```
#[macro_export]
macro_rules! log_event {
    ($level:expr, $event:expr $(, $key:ident = $value:expr)* $(,)?) => {
        if $crate::applog::enabled($level) {
            $crate::applog::emit(
                $level,
                $event,
                &[$((stringify!($key), &$value as &dyn ::std::fmt::Display)),*],
            );
        }
    };
}

/// `log_error!("watch.failed", kind = kind, error = err)` and friends.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log_event!($crate::applog::Level::Error, $($arg)*) };
}
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log_event!($crate::applog::Level::Warn, $($arg)*) };
}
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log_event!($crate::applog::Level::Info, $($arg)*) };
}
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::log_event!($crate::applog::Level::Debug, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_names_levels() {
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse(" debug "), Some(Level::Debug));
        assert_eq!(Level::parse("off"), Some(Level::Off));
        assert_eq!(Level::parse("verbose"), None);
        assert_eq!(Level::Trace.as_str(), "trace");
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Trace);
    }

    #[test]
    fn env_overrides_config_and_a_bad_env_value_is_ignored() {
        assert_eq!(resolve_level_from("info", None), (Level::Info, None));
        assert_eq!(resolve_level_from("info", Some("")), (Level::Info, None));
        assert_eq!(resolve_level_from("off", Some("trace")).0, Level::Trace);
        // An unparseable config level is the config's warning to report, not
        // this function's — it just falls back to off.
        assert_eq!(resolve_level_from("chatty", None), (Level::Off, None));
        let (level, warning) = resolve_level_from("warn", Some("chatty"));
        assert_eq!(
            level,
            Level::Warn,
            "a bad override keeps the configured level"
        );
        assert!(warning.is_some_and(|w| w.contains("chatty")));
    }

    #[test]
    fn renders_logfmt_with_timestamp_and_fields() {
        let out = format_line(
            Level::Info,
            "watch.start",
            &[("kind", &"pods"), ("ns", &"default")],
        );
        assert!(out.starts_with("ts="), "{out}");
        assert!(out.contains(" level=info event=watch.start "), "{out}");
        assert!(out.ends_with("kind=pods ns=default\n"), "{out}");
        // Milliseconds, not nanoseconds: one line, one timestamp width.
        let ts = out["ts=".len()..out.find(' ').unwrap()].to_string();
        assert!(ts.ends_with('Z') && ts.contains('.'), "{ts}");
        assert_eq!(ts.split('.').next_back().map(str::len), Some(4), "{ts}");
    }

    #[test]
    fn quotes_values_that_need_it() {
        let out = format_line(Level::Warn, "action.failed", &[("error", &"a \"b\" c")]);
        assert!(out.ends_with("error=\"a \\\"b\\\" c\"\n"), "{out}");
        let empty = format_line(Level::Warn, "e", &[("v", &"")]);
        assert!(empty.ends_with("v=\"\"\n"), "{empty}");
    }

    #[test]
    fn redacts_credentials_in_field_values() {
        let out = format_line(
            Level::Debug,
            "request",
            &[("error", &"401 for Bearer eyJhbGciOiJIUzI1NiJ9.e30.sig")],
        );
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"), "{out}");
        assert!(out.contains(crate::redact::REDACTED), "{out}");
    }

    #[test]
    fn redaction_preserves_logfmt_fields() {
        let line = format_line(
            Level::Warn,
            "request.failed",
            &[
                (
                    "server",
                    &"https://api.example/?token_value=private123&limit=5",
                ),
                (
                    "error",
                    &r#"{"password":"abc\"private456","status":"denied"}"#,
                ),
                ("attempt", &2),
            ],
        );
        assert!(
            !line.contains("private123") && !line.contains("private456"),
            "{line}"
        );
        assert!(
            line.contains("limit=5") && line.ends_with(" attempt=2\n"),
            "{line}"
        );
        assert!(line.contains(r#"\"status\":\"denied\""#), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn disabled_level_emits_nothing() {
        // The global sink is never initialised in tests, so `emit` is inert;
        // what matters here is that the gate itself is closed by default.
        assert!(!enabled(Level::Error));
        assert!(!enabled(Level::Off));
        assert_eq!(level(), Level::Off);
        let status = status();
        assert!(status.path.is_none() && status.written == 0 && status.dropped == 0);
    }

    #[test]
    fn a_second_writer_uses_the_current_file_after_rotation() {
        let dir = std::env::temp_dir().join(format!("sofka-shared-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sofka.log");
        let mut first = Writer::open(&path, 10).unwrap();
        let mut second = Writer::open(&path, 10).unwrap();
        first.write("session-a\n").unwrap();
        first.write("rotate-a\n").unwrap();
        first.write("rotate-a\n").unwrap();
        second.write("b\n").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("b\n"));
        assert!(std::fs::metadata(&path).unwrap().len() <= 10);
        drop((first, second));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_writers_keep_complete_lines_and_respect_the_size_limit() {
        let dir = std::env::temp_dir().join(format!("sofka-concurrent-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sofka.log");
        let first = Writer::open(&path, 128).unwrap();
        let second = Writer::open(&path, 128).unwrap();
        std::thread::scope(|scope| {
            for (mut writer, prefix) in [(first, 'a'), (second, 'b')] {
                scope.spawn(move || {
                    for n in 0..100 {
                        writer.write(&format!("{prefix}{n:03}\n")).unwrap();
                    }
                });
            }
        });
        for file in [&path, &dir.join("sofka.log.1")] {
            let text = std::fs::read_to_string(file).unwrap();
            assert!(text.len() <= 128);
            for line in text.lines() {
                assert!(
                    line.len() == 4 && matches!(line.as_bytes()[0], b'a' | b'b'),
                    "{line}"
                );
                assert!(line[1..].bytes().all(|b| b.is_ascii_digit()), "{line}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_appends_and_rotates() {
        let dir = std::env::temp_dir().join(format!("sofka-applog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("sofka.log");
        let mut writer = Writer::open(&path, 64).expect("open");
        for i in 0..8 {
            writer
                .write(&format!("line {i} padded out to force a rotation\n"))
                .expect("write");
        }
        writer.file.flush().expect("flush");

        let current = std::fs::read_to_string(&path).expect("current");
        let rotated =
            std::fs::read_to_string(dir.join("logs").join("sofka.log.1")).expect("rotated");
        assert!(current.contains("line 7"), "{current}");
        assert!(!current.contains("line 0"), "{current}");
        assert!(rotated.contains("line 6"), "{rotated}");
        // Reopening appends rather than truncating, and picks the size back up.
        let reopened = Writer::open(&path, 64).expect("reopen");
        assert_eq!(reopened.size, current.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
