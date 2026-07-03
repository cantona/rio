//! Minimal streaming terminal-mode tracker.
//!
//! Not an emulator: a small state machine that watches the output
//! stream for the handful of mode toggles that must be re-established
//! when a client attaches mid-stream — alternate screen, cursor
//! visibility, mouse reporting, bracketed paste, application keypad
//! and cursor keys, and the window title. Everything else is left to
//! the replay ring itself.

const MAX_PARAMS: usize = 16;
const MAX_TITLE: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// DECSET private modes tracked for replay. `AltScreen1049` also
/// remembers which variant entered the alternate screen so replay
/// re-enters it the same way.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modes {
    pub app_cursor_keys: bool,   // ?1
    pub cursor_hidden: bool,     // ?25 (tracked inverted: default visible)
    pub alt_screen: Option<u16>, // 47 | 1047 | 1048 | 1049
    pub mouse: Vec<u16>,         // 1000/1002/1003/1005/1006/1015, set order
    pub focus_reporting: bool,   // ?1004
    pub bracketed_paste: bool,   // ?2004
    pub app_keypad: bool,        // ESC = / ESC >
    pub title: Option<String>,   // OSC 0 / OSC 2
}

pub struct ModeTracker {
    state: State,
    private: bool,
    params: [u16; MAX_PARAMS],
    nparams: usize,
    cur_param: u16,
    has_param: bool,
    osc_acc: Vec<u8>,
    /// Set while inside any non-ground state — used by the ring to
    /// align eviction to sequence boundaries.
    pub modes: Modes,
}

impl Default for ModeTracker {
    fn default() -> Self {
        ModeTracker {
            state: State::Ground,
            private: false,
            params: [0; MAX_PARAMS],
            nparams: 0,
            cur_param: 0,
            has_param: false,
            osc_acc: Vec::new(),
            modes: Modes::default(),
        }
    }
}

impl ModeTracker {
    pub fn new() -> ModeTracker {
        ModeTracker::default()
    }

    #[inline]
    pub fn in_sequence(&self) -> bool {
        self.state != State::Ground
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match self.state {
            State::Ground => {
                if b == 0x1B {
                    self.state = State::Esc;
                }
            }
            State::Esc => match b {
                b'[' => {
                    self.state = State::Csi;
                    self.private = false;
                    self.nparams = 0;
                    self.cur_param = 0;
                    self.has_param = false;
                }
                b']' => {
                    self.state = State::Osc;
                    self.osc_acc.clear();
                }
                b'=' => {
                    self.modes.app_keypad = true;
                    self.state = State::Ground;
                }
                b'>' => {
                    self.modes.app_keypad = false;
                    self.state = State::Ground;
                }
                // ESC c: full reset clears everything we track.
                b'c' => {
                    self.modes = Modes::default();
                    self.state = State::Ground;
                }
                0x1B => {}
                // Other two-byte escapes end immediately; anything in
                // 0x20..=0x2F is an intermediate (charset selection),
                // consume one more final byte via a cheap trick: stay
                // in Esc only for intermediates.
                0x20..=0x2F => {}
                _ => self.state = State::Ground,
            },
            State::Csi => match b {
                b'?' if !self.has_param && self.nparams == 0 => self.private = true,
                b'0'..=b'9' => {
                    self.has_param = true;
                    self.cur_param =
                        self.cur_param.saturating_mul(10) + u16::from(b - b'0');
                }
                b';' => self.push_param(),
                0x40..=0x7E => {
                    self.push_param();
                    self.dispatch_csi(b);
                    self.state = State::Ground;
                }
                // intermediates / other param bytes: ignore content,
                // keep consuming until final byte
                _ => {}
            },
            State::Osc => match b {
                0x07 => {
                    self.dispatch_osc();
                    self.state = State::Ground;
                }
                0x1B => self.state = State::OscEsc,
                _ => {
                    if self.osc_acc.len() < MAX_TITLE + 8 {
                        self.osc_acc.push(b);
                    }
                }
            },
            State::OscEsc => {
                if b == b'\\' {
                    self.dispatch_osc();
                    self.state = State::Ground;
                } else {
                    // Not ST: treat the ESC as aborting the OSC.
                    self.state = if b == 0x1B {
                        State::OscEsc
                    } else {
                        State::Ground
                    };
                }
            }
        }
    }

    fn push_param(&mut self) {
        if self.has_param && self.nparams < MAX_PARAMS {
            self.params[self.nparams] = self.cur_param;
            self.nparams += 1;
        }
        self.cur_param = 0;
        self.has_param = false;
    }

    fn dispatch_csi(&mut self, final_byte: u8) {
        if !self.private {
            return;
        }
        let set = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        for i in 0..self.nparams {
            self.apply_private_mode(self.params[i], set);
        }
    }

    fn apply_private_mode(&mut self, mode: u16, set: bool) {
        match mode {
            1 => self.modes.app_cursor_keys = set,
            25 => self.modes.cursor_hidden = !set,
            47 | 1047 | 1048 | 1049 => {
                if set {
                    self.modes.alt_screen = Some(mode);
                } else if self.modes.alt_screen.is_some() {
                    self.modes.alt_screen = None;
                }
            }
            1000 | 1002 | 1003 | 1005 | 1006 | 1015 => {
                if set {
                    if !self.modes.mouse.contains(&mode) {
                        self.modes.mouse.push(mode);
                    }
                } else {
                    self.modes.mouse.retain(|&m| m != mode);
                }
            }
            1004 => self.modes.focus_reporting = set,
            2004 => self.modes.bracketed_paste = set,
            _ => {}
        }
    }

    fn dispatch_osc(&mut self) {
        // "0;title" or "2;title"
        let acc = std::mem::take(&mut self.osc_acc);
        if let Some(rest) = acc.strip_prefix(b"0;").or_else(|| acc.strip_prefix(b"2;")) {
            let mut title = String::from_utf8_lossy(rest).into_owned();
            title.truncate(MAX_TITLE);
            self.modes.title = Some(title);
        }
    }

    /// Escape sequences that re-establish the tracked state on a fresh
    /// terminal, replayed before the ring contents. Starts with DECSTR
    /// so a reconnect onto an existing grid resets cleanly too.
    pub fn replay_prefix(&self) -> Vec<u8> {
        let m = &self.modes;
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(b"\x1b[!p");
        // Alt screen first so subsequent state lands on the right screen.
        if let Some(variant) = m.alt_screen {
            out.extend_from_slice(format!("\x1b[?{variant}h").as_bytes());
        }
        if m.app_cursor_keys {
            out.extend_from_slice(b"\x1b[?1h");
        }
        if m.cursor_hidden {
            out.extend_from_slice(b"\x1b[?25l");
        }
        for mode in &m.mouse {
            out.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
        }
        if m.focus_reporting {
            out.extend_from_slice(b"\x1b[?1004h");
        }
        if m.bracketed_paste {
            out.extend_from_slice(b"\x1b[?2004h");
        }
        if m.app_keypad {
            out.extend_from_slice(b"\x1b=");
        }
        if let Some(title) = &m.title {
            out.extend_from_slice(b"\x1b]2;");
            out.extend_from_slice(title.as_bytes());
            out.extend_from_slice(b"\x1b\\");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_alt_screen_and_cursor() {
        let mut t = ModeTracker::new();
        t.advance(b"hello \x1b[?1049h\x1b[?25lvim content");
        assert_eq!(t.modes.alt_screen, Some(1049));
        assert!(t.modes.cursor_hidden);
        let p = t.replay_prefix();
        let s = String::from_utf8_lossy(&p);
        assert!(s.contains("\x1b[?1049h"));
        assert!(s.contains("\x1b[?25l"));
        assert!(s.starts_with("\x1b[!p"));

        t.advance(b"\x1b[?1049l\x1b[?25h");
        assert_eq!(t.modes.alt_screen, None);
        assert!(!t.modes.cursor_hidden);
    }

    #[test]
    fn tracks_multi_param_and_mouse() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?1000;1006h");
        assert_eq!(t.modes.mouse, vec![1000, 1006]);
        t.advance(b"\x1b[?1000l");
        assert_eq!(t.modes.mouse, vec![1006]);
    }

    #[test]
    fn tracks_title_and_keypad_and_reset() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b]2;my title\x07\x1b=");
        assert_eq!(t.modes.title.as_deref(), Some("my title"));
        assert!(t.modes.app_keypad);
        t.advance(b"\x1b]0;other\x1b\\");
        assert_eq!(t.modes.title.as_deref(), Some("other"));
        t.advance(b"\x1bc");
        assert_eq!(t.modes, Modes::default());
    }

    #[test]
    fn split_sequences_across_feeds() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?10");
        assert!(t.in_sequence());
        t.advance(b"49h");
        assert!(!t.in_sequence());
        assert_eq!(t.modes.alt_screen, Some(1049));
    }

    #[test]
    fn ordinary_csi_ignored() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[31mred\x1b[0m\x1b[2J\x1b[H");
        assert_eq!(t.modes, Modes::default());
    }
}
