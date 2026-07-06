use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy, Default)]
pub enum SessionRestore {
    #[serde(alias = "never")]
    #[default]
    Never,
    #[serde(alias = "prompt")]
    Prompt,
    #[serde(alias = "always")]
    Always,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct Session {
    #[serde(default)]
    pub restore: SessionRestore,
    /// Upper bound of history+screen lines dumped per pane on save.
    #[serde(
        default = "default_max_scrollback_lines",
        rename = "max-scrollback-lines"
    )]
    pub max_scrollback_lines: usize,
    /// Run each pane's shell behind a rio-ptyd daemon so it survives
    /// rio exiting; session restore reattaches to the live shells.
    #[serde(default)]
    pub persistent: bool,
    /// Replay buffer size per persistent pane, in bytes.
    #[serde(default = "default_ring_bytes", rename = "persistent-ring-bytes")]
    pub ring_bytes: usize,
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
            max_scrollback_lines: default_max_scrollback_lines(),
            persistent: false,
            ring_bytes: default_ring_bytes(),
        }
    }
}
