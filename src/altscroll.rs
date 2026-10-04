//! Repairs cursor-key escape sequences split at a read boundary.
//!
//! While mouse capture is released — document and log views release it so
//! click-drag uses the terminal's native text selection (#133) — terminals
//! translate the wheel into cursor-key escape sequences in the alternate
//! screen ("alternate scroll"): `ESC [ A`/`ESC [ B`, or `ESC O A`/`ESC O B`
//! in application cursor mode.
//!
//! Under a fast scroll burst a read can end exactly on the `ESC` byte. With
//! nothing else buffered the parser has to commit, so it reports a bare `Esc`
//! and then re-starts on the tail, which no longer looks like a sequence and
//! comes through as plain `[` and `B` characters. In the log view that stray
//! `Esc` closed the view and the tail then hit table bindings, landing on some
//! unrelated resource (#152).
//!
//! [`Repair`] puts the sequence back together: an `Esc` is held back for one
//! event (or a few milliseconds — see [`Repair::TIMEOUT`]), and if the
//! introducer and a final letter follow, the three are turned back into the
//! cursor key they came from.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the repair has consumed so far and is waiting to interpret.
enum State {
    /// Nothing held: keys pass straight through.
    Idle,
    /// A bare `Esc` is held, waiting to see whether a sequence follows it.
    Esc(KeyEvent),
    /// `Esc` plus a `[`/`O` introducer; the next letter finishes the sequence.
    Intro(KeyEvent),
}

/// Escape-sequence reassembly for one input stream.
///
/// Feed every key press through [`push`](Self::push) and dispatch whatever it
/// returns. When [`pending`](Self::pending) is true the caller must call
/// [`flush`](Self::flush) after [`TIMEOUT`](Self::TIMEOUT) so a real `Esc`
/// press is not held forever waiting for a sequence that will never arrive.
pub struct Repair {
    state: State,
}

impl Default for Repair {
    fn default() -> Self {
        Self { state: State::Idle }
    }
}

impl Repair {
    /// How long a held `Esc` may wait for the rest of a sequence. Split reads
    /// arrive back-to-back, so this only has to outlast one poll — short
    /// enough that a real `Esc` press still feels instant.
    pub const TIMEOUT: Duration = Duration::from_millis(15);

    /// Whether an `Esc` is currently held back and needs a timeout flush.
    pub fn pending(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// Feed one key press; returns the keys to dispatch, in order.
    pub fn push(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        // Only the bare, unmodified forms below can come from a split
        // sequence; anything carrying modifiers is a real chord.
        let plain = key.modifiers.is_empty();
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => {
                if plain && key.code == KeyCode::Esc {
                    self.state = State::Esc(key);
                    return Vec::new();
                }
                vec![key]
            }
            State::Esc(esc) => {
                if plain && matches!(key.code, KeyCode::Char('[') | KeyCode::Char('O')) {
                    self.state = State::Intro(esc);
                    return Vec::new();
                }
                // A second `Esc` starts a fresh wait; the first one was real.
                if plain && key.code == KeyCode::Esc {
                    self.state = State::Esc(key);
                    return vec![esc];
                }
                vec![esc, key]
            }
            State::Intro(esc) => {
                // crossterm reports the final letter as an uppercase char,
                // which carries SHIFT, so SHIFT alone still counts as plain.
                if (key.modifiers - KeyModifiers::SHIFT).is_empty()
                    && let Some(code) = final_byte(key.code)
                {
                    return vec![KeyEvent::new(code, KeyModifiers::NONE)];
                }
                // Not a sequence after all: replay what was swallowed. The
                // introducer is regenerated rather than stored — the only two
                // it can be are indistinguishable here in effect, and `[` is
                // what a user typing `Esc [` actually pressed.
                vec![
                    esc,
                    KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE),
                    key,
                ]
            }
        }
    }

    /// Give up on a held `Esc` and release what was swallowed.
    pub fn flush(&mut self) -> Vec<KeyEvent> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle => Vec::new(),
            State::Esc(esc) => vec![esc],
            State::Intro(esc) => vec![esc, KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)],
        }
    }
}

/// The cursor key a `CSI`/`SS3` final byte stands for.
fn final_byte(code: KeyCode) -> Option<KeyCode> {
    match code {
        KeyCode::Char('A') => Some(KeyCode::Up),
        KeyCode::Char('B') => Some(KeyCode::Down),
        KeyCode::Char('C') => Some(KeyCode::Right),
        KeyCode::Char('D') => Some(KeyCode::Left),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn codes(keys: Vec<KeyEvent>) -> Vec<KeyCode> {
        keys.into_iter().map(|k| k.code).collect()
    }

    #[test]
    fn ordinary_keys_pass_straight_through() {
        let mut r = Repair::default();
        for code in [KeyCode::Char('j'), KeyCode::Down, KeyCode::Enter] {
            assert_eq!(codes(r.push(press(code))), vec![code]);
            assert!(!r.pending());
        }
    }

    #[test]
    fn split_csi_and_ss3_sequences_become_cursor_keys() {
        for intro in ['[', 'O'] {
            for (final_byte, want) in [
                ('A', KeyCode::Up),
                ('B', KeyCode::Down),
                ('C', KeyCode::Right),
                ('D', KeyCode::Left),
            ] {
                let mut r = Repair::default();
                assert!(r.push(press(KeyCode::Esc)).is_empty());
                assert!(r.pending(), "esc is held while the tail is awaited");
                assert!(r.push(press(KeyCode::Char(intro))).is_empty());
                assert_eq!(codes(r.push(press(KeyCode::Char(final_byte)))), vec![want]);
                assert!(!r.pending());
            }
        }
    }

    #[test]
    fn a_burst_of_split_scrolls_never_yields_an_esc() {
        let mut r = Repair::default();
        let mut out = Vec::new();
        // What crossterm actually emits for the tail `[B`: `B` carries SHIFT.
        let b = KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT);
        for _ in 0..50 {
            out.extend(codes(r.push(press(KeyCode::Esc))));
            out.extend(codes(r.push(press(KeyCode::Char('[')))));
            out.extend(codes(r.push(b)));
        }
        assert_eq!(out, vec![KeyCode::Down; 50]);
        assert!(!r.pending());
    }

    #[test]
    fn a_real_esc_press_survives_the_timeout_flush() {
        let mut r = Repair::default();
        assert!(r.push(press(KeyCode::Esc)).is_empty());
        assert_eq!(codes(r.flush()), vec![KeyCode::Esc]);
        assert!(!r.pending());
        assert!(r.flush().is_empty(), "flushing twice is a no-op");
    }

    #[test]
    fn esc_followed_by_another_key_releases_both_in_order() {
        let mut r = Repair::default();
        assert!(r.push(press(KeyCode::Esc)).is_empty());
        assert_eq!(
            codes(r.push(press(KeyCode::Char('j')))),
            vec![KeyCode::Esc, KeyCode::Char('j')]
        );
        assert!(!r.pending());
    }

    #[test]
    fn repeated_esc_presses_all_get_through() {
        let mut r = Repair::default();
        assert!(r.push(press(KeyCode::Esc)).is_empty());
        assert_eq!(codes(r.push(press(KeyCode::Esc))), vec![KeyCode::Esc]);
        assert_eq!(codes(r.flush()), vec![KeyCode::Esc]);
    }

    #[test]
    fn an_unfinished_sequence_replays_what_it_swallowed() {
        let mut r = Repair::default();
        assert!(r.push(press(KeyCode::Esc)).is_empty());
        assert!(r.push(press(KeyCode::Char('['))).is_empty());
        assert_eq!(
            codes(r.push(press(KeyCode::Char('x')))),
            vec![KeyCode::Esc, KeyCode::Char('['), KeyCode::Char('x')]
        );

        let mut r = Repair::default();
        assert!(r.push(press(KeyCode::Esc)).is_empty());
        assert!(r.push(press(KeyCode::Char('['))).is_empty());
        assert_eq!(codes(r.flush()), vec![KeyCode::Esc, KeyCode::Char('[')]);
    }

    #[test]
    fn modified_keys_are_never_mistaken_for_a_sequence() {
        let mut r = Repair::default();
        let ctrl_esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL);
        assert_eq!(codes(r.push(ctrl_esc)), vec![KeyCode::Esc]);
        assert!(!r.pending());

        assert!(r.push(press(KeyCode::Esc)).is_empty());
        let alt_bracket = KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT);
        assert_eq!(
            codes(r.push(alt_bracket)),
            vec![KeyCode::Esc, KeyCode::Char('[')]
        );
    }
}
