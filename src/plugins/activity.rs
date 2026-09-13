//! Bounded, plain-text diagnostics snapshots; pipe readers never await the UI.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const TAIL_BYTES: usize = 64 * 1024;
const TAIL_LINES: usize = 256;
const LINE_BYTES: usize = 2048;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub lines: VecDeque<String>,
    pub dropped: usize,
}

#[derive(Clone)]
pub struct Activity {
    tail: Arc<Mutex<Tail>>,
    published: watch::Sender<Arc<Snapshot>>,
}

impl Activity {
    pub fn new() -> (Self, watch::Receiver<Arc<Snapshot>>) {
        let (published, receiver) = watch::channel(Arc::new(Snapshot::default()));
        (
            Self {
                tail: Arc::new(Mutex::new(Tail::default())),
                published,
            },
            receiver,
        )
    }

    pub fn append(&self, label: &str, text: &str) {
        // Only producers lock the tail. The UI clones an Arc from the watch
        // slot, then releases that borrow before doing any layout or rendering.
        let mut tail = self.tail.lock().unwrap();
        if tail.label != label {
            tail.append("\n");
            tail.label = label.chars().take(256).collect();
        }
        tail.append(text);
        self.published.send_replace(Arc::new(tail.snapshot.clone()));
    }
}

#[derive(Default)]
struct Tail {
    snapshot: Snapshot,
    bytes: usize,
    label: String,
    pending_cr: bool,
}

impl Tail {
    fn append(&mut self, text: &str) {
        for c in text.chars() {
            if self.snapshot.lines.is_empty() {
                self.snapshot.lines.push_back(String::new());
            }
            if c == '\r' {
                self.pending_cr = true;
                continue;
            }
            if self.pending_cr {
                // Delay a reset until the next character so split CRLF commits
                // the existing line, while a bare CR replaces the whole update.
                if c != '\n' {
                    let current = self.snapshot.lines.back_mut().unwrap();
                    self.bytes -= current.len();
                    current.clear();
                }
                self.pending_cr = false;
            }
            if c == '\n' {
                self.snapshot.lines.push_back(String::new());
            } else {
                let current = self.snapshot.lines.back_mut().unwrap();
                if current.is_empty() && !self.label.is_empty() {
                    *current = format!("[{}] ", self.label);
                    self.bytes += current.len();
                }
                // A redraw is one transient line, never wrapped fragments in
                // history. Keep a bounded prefix and let the viewport clip it.
                if current.len() + c.len_utf8() <= LINE_BYTES {
                    current.push(c);
                    self.bytes += c.len_utf8();
                }
            }
            while self.bytes > TAIL_BYTES || self.snapshot.lines.len() > TAIL_LINES {
                self.bytes -= self.snapshot.lines.pop_front().unwrap().len();
                self.snapshot.dropped += 1;
            }
        }
    }
}

#[derive(Default)]
enum Escape {
    #[default]
    Text,
    Start,
    Csi,
    String,
    StringEnd,
}

#[derive(Default)]
pub(super) struct Sanitizer {
    pending: Vec<u8>,
    escape: Escape,
}

impl Sanitizer {
    pub(super) fn feed(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    text.push_str(std::str::from_utf8(&self.pending[consumed..end]).unwrap());
                    consumed = end;
                    if let Some(len) = error.error_len() {
                        text.push('\u{fffd}');
                        consumed += len;
                    } else if eof {
                        text.push('\u{fffd}');
                        consumed = self.pending.len();
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        let mut output = String::new();
        for c in text.chars() {
            match self.escape {
                Escape::Start => {
                    self.escape = match c {
                        '[' => Escape::Csi,
                        ']' | 'P' | 'X' | '^' | '_' => Escape::String,
                        '\u{1b}' => Escape::Start,
                        '\u{20}'..='\u{2f}' => Escape::Start,
                        _ => Escape::Text,
                    }
                }
                Escape::Csi => {
                    if ('@'..='~').contains(&c) {
                        self.escape = Escape::Text;
                    }
                }
                Escape::String => match c {
                    '\u{7}' | '\u{9c}' => self.escape = Escape::Text,
                    '\u{1b}' => self.escape = Escape::StringEnd,
                    _ => {}
                },
                Escape::StringEnd => {
                    self.escape = if c == '\\' {
                        Escape::Text
                    } else {
                        Escape::String
                    }
                }
                Escape::Text => match c {
                    '\u{1b}' => self.escape = Escape::Start,
                    '\u{9b}' => self.escape = Escape::Csi,
                    '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                        self.escape = Escape::String
                    }
                    '\r' | '\n' => output.push(c),
                    '\t' => output.push(' '),
                    c if c.is_control()
                        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {}
                    c => output.push(c),
                },
            }
        }
        output
    }
}

pub fn plain_text(bytes: &[u8]) -> String {
    let mut sanitizer = Sanitizer::default();
    let mut text = String::new();
    for chunk in bytes.chunks(4096) {
        text.push_str(&sanitizer.feed(chunk, false));
    }
    text.push_str(&sanitizer.feed(&[], true));
    text.replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_handles_split_utf8_ansi_osc_controls_and_crlf() {
        let bytes =
            "é\u{1b}[31mred\u{1b}[0m\rnext\r\n\u{1b}]0;hidden\u{1b}\\ok\u{7}\u{202e}".as_bytes();
        for width in 1..bytes.len() {
            let mut sanitizer = Sanitizer::default();
            let mut text = String::new();
            for chunk in bytes.chunks(width) {
                text.push_str(&sanitizer.feed(chunk, false));
            }
            text.push_str(&sanitizer.feed(&[], true));
            assert_eq!(text, "éred\rnext\r\nok", "chunk width {width}");
        }
        let mut sanitizer = Sanitizer::default();
        assert_eq!(sanitizer.feed(&[0xf0], false), "");
        assert_eq!(sanitizer.feed(&[], true), "�");
    }

    #[test]
    fn trivy_redraws_replace_whole_clipped_lines_and_split_crlf_commits_logs() {
        let input = format!(
            "Starting scan\nINFO scanning é\n2 / 81 [{}] 2.47%\r\x1b[2K3 / 81 [{}] 3.70%\r\x1b[2K8 / 81 [---->] 9.88%",
            "-".repeat(6000),
            "-".repeat(3000),
        );
        for width in [1, 2, 3, 7, 64, 4096] {
            let (activity, receiver) = Activity::new();
            let mut sanitizer = Sanitizer::default();
            for bytes in input.as_bytes().chunks(width) {
                activity.append("", &sanitizer.feed(bytes, false));
            }
            assert_eq!(
                receiver.borrow().lines,
                ["Starting scan", "INFO scanning é", "8 / 81 [---->] 9.88%"]
            );
            activity.append("", &sanitizer.feed(b"\r", false));
            assert_eq!(receiver.borrow().lines.len(), 3);
            activity.append("", &sanitizer.feed(b"\nnormal log\r", false));
            activity.append("", &sanitizer.feed(b"\nnext log", false));
            assert_eq!(
                receiver.borrow().lines,
                [
                    "Starting scan",
                    "INFO scanning é",
                    "8 / 81 [---->] 9.88%",
                    "normal log",
                    "next log"
                ]
            );
            assert_eq!(
                receiver.borrow().dropped,
                0,
                "redraws must not accumulate history"
            );
        }
    }

    #[test]
    fn activity_tail_bounds_lines_bytes_and_unterminated_lines() {
        let (activity, receiver) = Activity::new();
        for _ in 0..1024 {
            activity.append("", &"é".repeat(2048));
        }
        let snapshot = receiver.borrow().clone();
        assert_eq!(snapshot.lines.len(), 1);
        assert_eq!(snapshot.lines[0].len(), LINE_BYTES);
        assert!(snapshot.lines.iter().all(|line| line.len() <= LINE_BYTES));
        assert!(snapshot.lines.iter().map(String::len).sum::<usize>() <= TAIL_BYTES);
        activity.append("", &"\n".repeat(1024));
        assert!(receiver.borrow().lines.len() <= TAIL_LINES);
        assert!(receiver.borrow().dropped > 0);
        for _ in 0..128 {
            activity.append("", &format!("{}\n", "x".repeat(LINE_BYTES)));
        }
        assert!(
            receiver
                .borrow()
                .lines
                .iter()
                .map(String::len)
                .sum::<usize>()
                <= TAIL_BYTES
        );
    }
}
