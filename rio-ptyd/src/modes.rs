//! Minimal streaming terminal-mode tracker.
//!
//! Not an emulator: a small state machine that watches the output
//! stream for the handful of mode toggles that must be re-established
//! when a client attaches mid-stream — alternate screen, cursor
//! visibility, mouse reporting, bracketed paste, application keypad
//! and cursor keys, and the window title. Everything else is left to
//! the replay ring itself.
//!
//! Accepted v1 limitations: G0/G1 charset designation, scroll region
//! and SGR are not re-established after the DECSTR in replay_prefix
//! (full-screen apps re-assert them on redraw); C1 8-bit control
//! introducers (0x9B CSI etc.) are not recognized; ring eviction can
//! split a multi-byte UTF-8 character (one garbage glyph at replay
//! start, self-correcting).

const MAX_PARAMS: usize = 16;
const MAX_TITLE: usize = 512;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Esc,
    /// After an ESC intermediate byte (0x20..=0x2F, e.g. charset
    /// designators `ESC ( B`): the next final byte dispatches to
    /// Ground and is NOT re-read as `[`/`]`/`c`. Without this state a
    /// `cat` of binary data (`ESC ( c`, `ESC ( [ …`) would wrongly hit
    /// the reset / CSI arms and corrupt tracked modes.
    EscIntermediate,
    Csi,
    Osc,
    OscEsc,
    /// DCS/SOS/PM/APC body: absorbed until ST so in_sequence() holds
    /// across e.g. a whole sixel payload — the ring must never evict
    /// to a point inside one.
    StrSeq,
    StrEsc,
}

/// DECSET private modes tracked for replay. `AltScreen1049` also
/// remembers which variant entered the alternate screen so replay
/// re-enters it the same way.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modes {
    pub app_cursor_keys: bool,   // ?1
    pub cursor_hidden: bool,     // ?25 (tracked inverted: default visible)
    pub autowrap_off: bool,      // ?7 (tracked inverted: DECSTR turns it on)
    pub alt_screen: Option<u16>, // 47 | 1047 | 1049 (1048 is cursor-save only)
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
            self.step(b);
        }
    }

    /// Advance one byte. When an ESC terminates a string/OSC/CSI, the
    /// arm re-enters `State::Esc` and re-invokes `step` on the same byte
    /// so it is read as the new escape's second byte (bounded, one level
    /// of recursion). The `bool` return feeds that internal reprocess;
    /// the top-level `advance` caller ignores it.
    fn step(&mut self, b: u8) -> bool {
        match self.state {
            State::Ground => {
                if b == 0x1B {
                    self.state = State::Esc;
                }
                false
            }
            State::Esc => {
                match b {
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
                    // DCS / SOS / PM / APC start a string that runs to ST.
                    b'P' | b'X' | b'^' | b'_' => self.state = State::StrSeq,
                    // Intermediate byte (charset designators etc.): the
                    // FINAL byte after it must not be re-read as [/]/c.
                    0x20..=0x2F => self.state = State::EscIntermediate,
                    // C0 controls execute and stay in Escape (rio does
                    // the same); DEL is ignored. Anything else is a
                    // two-byte escape final — back to Ground.
                    0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x7F => {}
                    _ => self.state = State::Ground,
                }
                false
            }
            State::EscIntermediate => {
                match b {
                    // Further intermediates accumulate; stay here.
                    0x20..=0x2F => {}
                    // C0 executes and stays; DEL ignored.
                    0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x7F => {}
                    // Any final byte (0x30..=0x7E, incl. charset codes
                    // and `c`) dispatches to Ground — NOT tracked, and
                    // never re-read as a CSI/OSC/reset introducer.
                    _ => self.state = State::Ground,
                }
                false
            }
            State::Csi => {
                match b {
                    b'?' if !self.has_param && self.nparams == 0 => self.private = true,
                    b'0'..=b'9' => {
                        self.has_param = true;
                        // Fully saturating: the trailing add must not
                        // overflow u16 either — a garbage/hostile pty
                        // write like `CSI ?999992h` would otherwise panic
                        // in an overflow-checked build (and wrap to a
                        // false mode number in release).
                        self.cur_param = self
                            .cur_param
                            .saturating_mul(10)
                            .saturating_add(u16::from(b - b'0'));
                    }
                    b';' => self.push_param(),
                    0x40..=0x7E => {
                        self.push_param();
                        self.dispatch_csi(b);
                        self.state = State::Ground;
                    }
                    // CAN / SUB abort the sequence from any state.
                    0x18 | 0x1A => self.state = State::Ground,
                    // ESC ends this sequence and starts a new one.
                    0x1B => self.state = State::Esc,
                    // intermediates / other param bytes: ignore content,
                    // keep consuming until final byte
                    _ => {}
                }
                false
            }
            State::Osc => {
                match b {
                    0x07 => {
                        self.dispatch_osc();
                        self.state = State::Ground;
                    }
                    0x1B => self.state = State::OscEsc,
                    // Match rio's own parser (the client): CAN/SUB END
                    // the OSC and dispatch it (not discard), and other
                    // C0 controls are IGNORED (the OSC keeps
                    // accumulating). Diverging here would make the
                    // replayed title differ from what the live client
                    // actually showed.
                    0x18 | 0x1A => {
                        self.dispatch_osc();
                        self.state = State::Ground;
                    }
                    _ if b < 0x20 => {}
                    _ => {
                        if self.osc_acc.len() < MAX_TITLE + 8 {
                            self.osc_acc.push(b);
                        }
                    }
                }
                false
            }
            State::OscEsc => {
                // We are here because an ESC was seen inside an OSC.
                self.dispatch_osc();
                if b == b'\\' {
                    // ST: string terminator, consumed.
                    self.state = State::Ground;
                    false
                } else {
                    // The ESC began a fresh escape sequence. Re-enter Esc
                    // and reprocess THIS byte as the escape's second byte.
                    self.state = State::Esc;
                    self.step(b)
                }
            }
            State::StrSeq => {
                match b {
                    0x1B => self.state = State::StrEsc,
                    // CAN / SUB abort the string.
                    0x18 | 0x1A => self.state = State::Ground,
                    _ => {}
                }
                false
            }
            State::StrEsc => {
                if b == b'\\' {
                    // ST terminates the string sequence, consumed.
                    self.state = State::Ground;
                    false
                } else {
                    // The ESC began a fresh escape sequence. Re-enter Esc
                    // and reprocess THIS byte as the escape's second byte.
                    self.state = State::Esc;
                    self.step(b)
                }
            }
        }
    }

    fn push_param(&mut self) {
        // Params beyond MAX_PARAMS are dropped.
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
            // DECSTR re-enables autowrap, so an app that turned it off
            // needs the toggle re-applied on replay.
            7 => self.modes.autowrap_off = !set,
            25 => self.modes.cursor_hidden = !set,
            // 1048 is cursor save/restore only — treating it as an
            // alt-screen variant would replay the wrong buffer.
            47 | 1047 | 1049 => {
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
            // Truncate the bytes before the lossy conversion:
            // String::truncate panics when the cut lands inside a
            // multi-byte character, which pty output fully controls.
            let rest = &rest[..rest.len().min(MAX_TITLE)];
            self.modes.title = Some(String::from_utf8_lossy(rest).into_owned());
        }
    }

    /// Escape sequences that re-establish the tracked state on a fresh
    /// terminal, replayed before the ring contents.
    ///
    /// Every mode is emitted EXPLICITLY (both the set and the default
    /// value), not left to DECSTR: rio's own emulator no-ops `CSI ! p`,
    /// so relying on it to restore the unset defaults (cursor visible,
    /// autowrap on, normal keys) would leave stale modes on a dirty-grid
    /// reattach. DECSTR is still emitted first as a best-effort reset for
    /// terminals that do implement it (the CLI `attach` into a foreign
    /// terminal).
    pub fn replay_prefix(&self) -> Vec<u8> {
        let m = &self.modes;
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(b"\x1b[!p");
        // Alt screen first so subsequent state lands on the right
        // screen. Emit BOTH directions: on a dirty-grid reattach the
        // client may already be on the alt screen from a prior life, so
        // "not in alt screen" must be asserted, not just omitted (else
        // e.g. a vim that exited while detached leaves the client stuck
        // on the alt buffer once its `?1049l` has evicted from the ring).
        if let Some(variant) = m.alt_screen {
            out.extend_from_slice(format!("\x1b[?{variant}h").as_bytes());
        } else {
            out.extend_from_slice(b"\x1b[?1049l");
        }
        // Application vs normal cursor keys — set both ways explicitly.
        if m.app_cursor_keys {
            out.extend_from_slice(b"\x1b[?1h");
        } else {
            out.extend_from_slice(b"\x1b[?1l");
        }
        // Cursor visibility.
        if m.cursor_hidden {
            out.extend_from_slice(b"\x1b[?25l");
        } else {
            out.extend_from_slice(b"\x1b[?25h");
        }
        // Autowrap.
        if m.autowrap_off {
            out.extend_from_slice(b"\x1b[?7l");
        } else {
            out.extend_from_slice(b"\x1b[?7h");
        }
        // Mouse tracking: the tracker keeps only the SET modes, so reset
        // every mouse mode this reattach doesn't want, then set the
        // active ones. Without the resets a stale mouse mode from the
        // client's prior life survives a dirty-grid reattach.
        for mode in [1000u16, 1002, 1003, 1005, 1006, 1015] {
            if !m.mouse.contains(&mode) {
                out.extend_from_slice(format!("\x1b[?{mode}l").as_bytes());
            }
        }
        for mode in &m.mouse {
            out.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
        }
        // Focus reporting / bracketed paste / keypad — both directions.
        if m.focus_reporting {
            out.extend_from_slice(b"\x1b[?1004h");
        } else {
            out.extend_from_slice(b"\x1b[?1004l");
        }
        if m.bracketed_paste {
            out.extend_from_slice(b"\x1b[?2004h");
        } else {
            out.extend_from_slice(b"\x1b[?2004l");
        }
        if m.app_keypad {
            out.extend_from_slice(b"\x1b=");
        } else {
            out.extend_from_slice(b"\x1b>");
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
    fn title_truncation_never_panics_on_utf8_boundary() {
        let mut t = ModeTracker::new();
        let mut osc = b"\x1b]2;".to_vec();
        osc.extend(std::iter::repeat_n(b'a', MAX_TITLE - 1));
        osc.extend("€".as_bytes());
        osc.push(0x07);
        t.advance(&osc);
        let title = t.modes.title.as_deref().unwrap();
        assert!(title.len() <= MAX_TITLE + 3);
        assert!(title.starts_with("aaa"));
    }

    #[test]
    fn dcs_apc_bodies_count_as_in_sequence() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1bPq#0;2;0;0;0#0~~@@vv@@~~@@~~$");
        assert!(t.in_sequence(), "mid-sixel must be in_sequence");
        t.advance(b"\x1b\\");
        assert!(!t.in_sequence());
        t.advance(b"\x1b_Ga=T\x1b\\after");
        assert!(!t.in_sequence());
        // A byte inside the body must not be parsed as a mode toggle;
        // only ST (ESC \\) terminates cleanly.
        let mut t2 = ModeTracker::new();
        t2.advance(b"\x1bPq[?1049h\x1b\\");
        assert_eq!(t2.modes.alt_screen, None);
    }

    #[test]
    fn esc_inside_string_terminates_and_restarts() {
        // ESC (not ST) inside a DCS ends the string and begins a fresh
        // escape — the following CSI must be parsed, matching xterm.
        let mut t = ModeTracker::new();
        t.advance(b"\x1bPq\x1b[?1049h");
        assert_eq!(t.modes.alt_screen, Some(1049));
        assert!(!t.in_sequence());
    }

    #[test]
    fn esc_terminates_osc_then_new_sequence_parsed() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b]2;mytitle\x1b[?1049h");
        assert_eq!(t.modes.title.as_deref(), Some("mytitle"));
        assert_eq!(t.modes.alt_screen, Some(1049));
    }

    #[test]
    fn can_sub_abort_csi() {
        let mut t = ModeTracker::new();
        // CAN mid-CSI aborts: the 1049 must not register.
        t.advance(b"\x1b[?10\x1849h");
        assert_eq!(t.modes.alt_screen, None);
        // (OSC CAN behavior — dispatch, not abort — is covered by
        // osc_can_dispatches_title_like_rio, matching rio's parser.)
    }

    #[test]
    fn autowrap_off_survives_replay() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?7l");
        assert!(t.modes.autowrap_off);
        let p = t.replay_prefix();
        assert!(String::from_utf8_lossy(&p).contains("\x1b[?7l"));
        t.advance(b"\x1b[?7h");
        assert!(!t.modes.autowrap_off);
    }

    #[test]
    fn mode_1048_is_not_an_alt_screen_switch() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?1049h\x1b[?1048h");
        assert_eq!(t.modes.alt_screen, Some(1049));
        t.advance(b"\x1b[?1048l");
        assert_eq!(t.modes.alt_screen, Some(1049));
    }

    #[test]
    fn ordinary_csi_ignored() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[31mred\x1b[0m\x1b[2J\x1b[H");
        assert_eq!(t.modes, Modes::default());
    }

    #[test]
    fn csi_param_does_not_overflow_u16() {
        // Must not panic (overflow-checked builds) nor wrap to a false
        // mode number: a huge private-mode param is clamped.
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?999992h");
        assert!(!t.modes.app_cursor_keys);
        t.advance(b"\x1b[?655358l");
        assert!(!t.modes.autowrap_off);
    }

    #[test]
    fn charset_designator_does_not_corrupt_modes() {
        let mut t = ModeTracker::new();
        t.advance(b"\x1b[?1049h"); // enter alt screen
        assert_eq!(t.modes.alt_screen, Some(1049));
        // ESC ( c : charset designate 'c' — must NOT hit the ESC c
        // full-reset arm and wipe alt_screen.
        t.advance(b"\x1b(c");
        assert_eq!(t.modes.alt_screen, Some(1049));
        // ESC ( [ ?25l : the '[' after an intermediate is a charset
        // final, NOT a CSI introducer — cursor_hidden stays false and
        // the ?25l is printed as text, not parsed.
        let mut t2 = ModeTracker::new();
        t2.advance(b"\x1b([?25l");
        assert!(!t2.modes.cursor_hidden);
    }

    #[test]
    fn replay_prefix_resets_unset_modes() {
        // A tracker with nothing set must still emit the OFF direction
        // for every mode, so a dirty-grid reattach can't inherit a
        // stale alt-screen / mouse / paste mode from the client's
        // prior life.
        let t = ModeTracker::new();
        let p = String::from_utf8_lossy(&t.replay_prefix()).into_owned();
        assert!(p.contains("\x1b[?1049l"), "alt-screen off missing");
        assert!(p.contains("\x1b[?1000l"), "mouse 1000 off missing");
        assert!(p.contains("\x1b[?2004l"), "bracketed-paste off missing");
        assert!(p.contains("\x1b[?1004l"), "focus off missing");
        assert!(p.contains("\x1b>"), "keypad-normal missing");
    }

    #[test]
    fn osc_can_dispatches_title_like_rio() {
        // rio's parser dispatches the OSC on CAN/SUB; the tracker must
        // match so the replayed title equals what the client showed.
        let mut t = ModeTracker::new();
        t.advance(b"\x1b]2;hello\x18");
        assert_eq!(t.modes.title.as_deref(), Some("hello"));
    }
}
