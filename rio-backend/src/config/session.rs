use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy, Default)]
pub enum SessionRestore {
    #[serde(alias = "never", alias = "disable", alias = "disabled")]
    #[default]
    Never,
    #[serde(alias = "prompt")]
    Prompt,
    #[serde(alias = "always", alias = "enable", alias = "enabled")]
    Always,
}

impl SessionRestore {
    /// Whether the session is saved/restored at all (not `Never`).
    #[inline]
    pub fn enabled(self) -> bool {
        !matches!(self, SessionRestore::Never)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct Session {
    /// Level 1 — session restore mode: disable / prompt / always.
    #[serde(default)]
    pub restore: SessionRestore,
    /// Level 2 — restore implementation. `false` (default) = v1:
    /// shells die with rio; restore repaints saved scrollback into
    /// fresh shells at the saved directory. `true` = v2: each pane runs
    /// behind a rio-ptyd daemon so its shell survives rio exiting, and
    /// restore reattaches to the live shells. Only meaningful when
    /// `restore` is not `disable`.
    #[serde(default)]
    pub persistent: bool,
    /// Upper bound of history+screen lines dumped per pane on save.
    #[serde(
        default = "default_max_scrollback_lines",
        rename = "max-scrollback-lines"
    )]
    pub max_scrollback_lines: usize,
    /// Replay buffer size per persistent (v2) pane, in bytes.
    #[serde(default = "default_ring_bytes", rename = "persistent-ring-bytes")]
    pub ring_bytes: usize,
}

impl Session {
    /// v2 (live daemons) is active: the session is on AND the
    /// implementation is the persistent one.
    #[inline]
    pub fn uses_daemons(&self) -> bool {
        self.restore.enabled() && self.persistent
    }
}

fn default_ring_bytes() -> usize {
    1024 * 1024
}

fn default_max_scrollback_lines() -> usize {
    2000
}

impl Default for Session {
    fn default() -> Session {
        Session {
            restore: SessionRestore::default(),
            persistent: false,
            max_scrollback_lines: default_max_scrollback_lines(),
            ring_bytes: default_ring_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_v1() {
        let s = Session::default();
        assert_eq!(s.restore, SessionRestore::Never);
        assert!(!s.persistent);
        assert!(!s.restore.enabled());
        assert!(!s.uses_daemons());
    }

    #[test]
    fn two_level_parse() {
        let s: Session =
            toml::from_str("restore = \"prompt\"\npersistent = true").unwrap();
        assert_eq!(s.restore, SessionRestore::Prompt);
        assert!(s.persistent);
        assert!(s.uses_daemons());

        // v1: restore on, persistent off -> no daemons.
        let s: Session = toml::from_str("restore = \"always\"").unwrap();
        assert_eq!(s.restore, SessionRestore::Always);
        assert!(!s.persistent);
        assert!(!s.uses_daemons());

        // disable ignores persistent entirely.
        let s: Session =
            toml::from_str("restore = \"disable\"\npersistent = true").unwrap();
        assert!(!s.uses_daemons());
    }

    #[test]
    fn legacy_aliases_parse() {
        // The pre-rename value `restore = "never"` still maps to
        // Never; `persistent` stays a plain bool as before.
        let s: Session =
            toml::from_str("restore = \"never\"\npersistent = false").unwrap();
        assert_eq!(s.restore, SessionRestore::Never);
        assert!(!s.persistent);
    }
}
