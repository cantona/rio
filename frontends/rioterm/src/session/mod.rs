//! Session save/restore: tabs, split layout, per-pane working directory
//! and styled scrollback, persisted across runs (`[session]` config).

use crate::context::{self, ContextManager};
use rio_backend::event::EventListener;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped whenever the on-disk shape changes; mismatched files are
/// discarded rather than migrated.
pub const SESSION_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub windows: Vec<WindowState>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WindowState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    /// Physical inner size; (0, 0) when unknown.
    #[serde(default)]
    pub size: (u32, u32),
    /// Physical outer position. Absent on Wayland (compositor-placed).
    #[serde(default)]
    pub position: Option<(i32, i32)>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TabState {
    pub layout: LayoutNode,
    pub custom_title: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub enum LayoutNode {
    Leaf(PaneState),
    /// Weight is the child's taffy `flex_grow` — proportional share of
    /// the container, not an absolute size.
    Split {
        direction: SplitDir,
        children: Vec<(f32, LayoutNode)>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PaneState {
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub is_active: bool,
    /// Fallback repaint for panes without a live daemon.
    pub scrollback: String,
    /// rio-ptyd binding: present for persistent panes; restore
    /// reattaches to the daemon (its replay repaints the pane).
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub socket: Option<String>,
    /// ssh destination when the daemon lives on another machine.
    #[serde(default)]
    pub host: Option<String>,
}

impl PaneState {
    /// Attach target when this pane recorded a daemon binding.
    pub fn attach_target(&self) -> Option<context::AttachTarget> {
        let pane_id = self.pane_id.clone()?;
        match &self.host {
            Some(host) => Some(context::AttachTarget::Ssh {
                host: host.clone(),
                pane_id,
            }),
            None => self
                .socket
                .as_ref()
                .map(|socket| context::AttachTarget::Unix {
                    pane_id: pane_id.clone(),
                    socket: std::path::PathBuf::from(socket),
                }),
        }
    }

    pub fn spawn(&self) -> context::PaneSpawn {
        context::PaneSpawn {
            cwd: self.cwd.clone(),
            attach: self.attach_target(),
        }
    }
}

impl LayoutNode {
    /// True when the tree is structurally usable: every `Split` has at
    /// least one child, and every child weight is finite and positive.
    /// A hand-edited / corrupt file with an empty `Split` would
    /// otherwise panic in `first_leaf`'s `children[0]`, and a NaN /
    /// infinite / non-positive weight flows straight into taffy
    /// `flex_grow`, degenerating the pane rects.
    fn well_formed(&self) -> bool {
        match self {
            LayoutNode::Leaf(_) => true,
            LayoutNode::Split { children, .. } => {
                !children.is_empty()
                    && children
                        .iter()
                        .all(|(w, c)| w.is_finite() && *w > 0.0 && c.well_formed())
            }
        }
    }

    /// The pane a subtree's first split-off context should spawn as.
    /// Safe because `SessionState::load` rejects trees that aren't
    /// `well_formed` (every Split has ≥1 child).
    pub fn first_leaf(&self) -> &PaneState {
        match self {
            LayoutNode::Leaf(p) => p,
            LayoutNode::Split { children, .. } => children[0].1.first_leaf(),
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            LayoutNode::Leaf(_) => 1,
            LayoutNode::Split { children, .. } => {
                children.iter().map(|(_, c)| c.leaf_count()).sum()
            }
        }
    }
}

impl SessionState {
    /// Reject a whole file larger than this; a legitimate session of
    /// scrollback dumps stays well under it, so anything bigger is
    /// corrupt or hostile and is not worth parsing into memory.
    const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
    /// Per-pane scrollback is truncated to this many bytes on load so a
    /// tampered file can't replay an unbounded stream into a terminal.
    const MAX_PANE_SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
    /// Every leaf spawns a shell (or daemon attach) on restore; a
    /// tampered file with thousands of leaves would fork-bomb an
    /// `always` launch. Far above any real window, so exceeding it
    /// means the file is not worth restoring at all.
    const MAX_WINDOW_PANES: usize = 256;

    pub fn load(path: &Path) -> Option<SessionState> {
        // Bound the read before allocating: an oversized file is
        // rejected without slurping it whole.
        if std::fs::metadata(path).ok()?.len() > Self::MAX_FILE_BYTES {
            tracing::warn!("session: discarding oversized file");
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let mut state: SessionState = serde_json::from_slice(&bytes).ok()?;
        if state.version != SESSION_VERSION || state.windows.is_empty() {
            return None;
        }
        // Reject a structurally-broken tree (an empty Split, a
        // non-finite or non-positive weight) rather than panic later in
        // first_leaf or hand taffy degenerate ratios.
        let all_ok = state
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .all(|t| t.layout.well_formed());
        if !all_ok {
            tracing::warn!("session: discarding file with a malformed layout tree");
            return None;
        }
        let flood = state.windows.iter().any(|w| {
            w.tabs.iter().map(|t| t.layout.leaf_count()).sum::<usize>()
                > Self::MAX_WINDOW_PANES
        });
        if flood {
            tracing::warn!("session: discarding file with too many panes per window");
            return None;
        }
        // Cap per-pane scrollback so a tampered file can't replay an
        // unbounded stream on restore.
        for w in &mut state.windows {
            for t in &mut w.tabs {
                Self::cap_scrollback(&mut t.layout);
            }
        }
        Some(state)
    }

    fn cap_scrollback(node: &mut LayoutNode) {
        match node {
            LayoutNode::Leaf(p) => {
                if p.scrollback.len() > Self::MAX_PANE_SCROLLBACK_BYTES {
                    // Truncate to the most recent bytes on a char
                    // boundary; older history is the safe part to drop.
                    let start = p.scrollback.len() - Self::MAX_PANE_SCROLLBACK_BYTES;
                    let start = (start..p.scrollback.len())
                        .find(|i| p.scrollback.is_char_boundary(*i))
                        .unwrap_or(p.scrollback.len());
                    p.scrollback = p.scrollback.split_off(start);
                }
            }
            LayoutNode::Split { children, .. } => {
                for (_, c) in children {
                    Self::cap_scrollback(c);
                }
            }
        }
    }

    /// Write via a sibling temp file renamed over the target: the
    /// rename is atomic on one filesystem, so a crash mid-write or a
    /// concurrent instance never leaves a truncated file — which
    /// `load` would reject, losing the whole session and with it the
    /// only handle on its detached daemons. Concurrent writers still
    /// race whole files (last one wins), but each lands intact. The
    /// pid suffix keeps two instances off the same temp path.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp.{}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, bytes)
            .and_then(|()| std::fs::rename(&tmp, path))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
    }

    pub fn discard(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

/// What a window close (exit / alt+w / WM-X / quit) should do with the
/// session, given whether the window is bound to a name and the restore
/// mode. The single source of truth for the close-save table; every
/// close path routes through it so they stay consistent.
///
/// | session | disable | prompt  | always |
/// |---------|---------|---------|--------|
/// | named   | Prompt  | Prompt  | Save   |
/// | unnamed | Nothing | Prompt  | Save   |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDisposition {
    /// Save silently, then close.
    Save,
    /// Show the save prompt; the answer saves-or-discards, then closes.
    Prompt,
    /// Close immediately, save nothing.
    Nothing,
}

pub fn close_disposition(
    named: bool,
    mode: rio_backend::config::session::SessionRestore,
) -> CloseDisposition {
    use rio_backend::config::session::SessionRestore;
    match mode {
        // Automatic: everyone saves silently.
        SessionRestore::Always => CloseDisposition::Save,
        // Ask: everyone is prompted.
        SessionRestore::Prompt => CloseDisposition::Prompt,
        // Off: a named workspace still persists (the name is intent to
        // keep it) — asked so it isn't silently overwritten; an unnamed
        // session is dropped.
        SessionRestore::Never => {
            if named {
                CloseDisposition::Prompt
            } else {
                CloseDisposition::Nothing
            }
        }
    }
}

/// Whether a quit/exit tail may write the session covering `named`
/// windows without further interaction. `consented` carries the quit
/// prompt's yes-answer when one was shown: Prompt-disposition sessions
/// write only with it — a file whose mode promised to ask is never
/// silently overwritten.
pub fn write_on_quit(
    named: bool,
    mode: rio_backend::config::session::SessionRestore,
    consented: bool,
) -> bool {
    match close_disposition(named, mode) {
        CloseDisposition::Save => true,
        CloseDisposition::Prompt => consented,
        CloseDisposition::Nothing => false,
    }
}

/// How a save's carry-forward merge verifies candidate daemons.
/// Probing costs one blocking socket connect per not-reattached pane
/// on the UI thread; autosave fires on every tab/split change, so it
/// assumes instead. A stale carry is self-healing: the next probed
/// save (close/quit/explicit) drops it, and restoring a dead daemon
/// falls back to a fresh spawn with the saved scrollback.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DaemonCheck {
    /// Connect to each candidate's socket; carry only the live ones.
    Probe,
    /// Carry every candidate; used by autosave.
    AssumeAlive,
}

/// Assemble captured windows into a session and persist it to `path`,
/// carrying forward any still-live daemon the old file referenced but no
/// captured window reattached (so overwriting never orphans a daemon).
/// The session-write policy — every caller that has gathered a window
/// list routes through here rather than re-implementing assemble +
/// merge + write. No-op on an empty list.
pub fn write_windows(path: &Path, windows: Vec<WindowState>, check: DaemonCheck) {
    if windows.is_empty() {
        return;
    }
    #[allow(unused_mut)]
    let mut state = SessionState {
        version: SESSION_VERSION,
        windows,
    };
    #[cfg(unix)]
    merge_kept_daemons(&mut state, path, check);
    #[cfg(not(unix))]
    let _ = check;
    if let Err(err) = state.save(path) {
        tracing::warn!("session save failed: {err}");
    }
}

/// Per-run record of the windows saved to each session file, keyed by a
/// stable, unique per-window handle `K` (the live WindowId). A
/// multi-window session is often persisted across several saves — one
/// window closes, then another — and each save only sees the windows
/// still open. Keying accumulated windows by `K` keeps the earlier-closed
/// ones so the file is never shrunk to the still-open subset, and
/// re-saving a window replaces its entry rather than appending a second
/// copy (the bug a pane-set-identity merge had: a window whose tab set
/// changed between two saves looked like a different window and was
/// duplicated). Empty at process start, so a previous run's file never
/// leaks stale windows into this one.
#[derive(Default)]
pub struct SavedWindows<K: Eq + std::hash::Hash> {
    per_file: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<K, WindowState>,
    >,
    /// Windows that must survive mirror saves after they close: a
    /// close-tab on a window with a running process keeps it resumable
    /// (daemons detached), so the pruning autosave that follows the
    /// close must not drop it. Explicit save-now clears the pins — the
    /// user asked for the file to mirror what is open.
    pinned: std::collections::HashMap<std::path::PathBuf, std::collections::HashSet<K>>,
}

impl<K: Eq + std::hash::Hash + Copy> SavedWindows<K> {
    pub fn new() -> Self {
        SavedWindows {
            per_file: std::collections::HashMap::new(),
            pinned: std::collections::HashMap::new(),
        }
    }

    /// Keep `key`'s recorded window across mirror saves of `path` even
    /// once it is no longer open.
    pub fn pin(&mut self, path: &Path, key: K) {
        self.pinned
            .entry(path.to_path_buf())
            .or_default()
            .insert(key);
    }

    /// Record `captured` (key, window) pairs for `path`, replacing any
    /// prior entry for the same key. Then write the union of everything
    /// recorded for `path` this run, ordering `preferred` first so restore
    /// lands it in the launch route.
    pub fn accumulate_and_write<I>(
        &mut self,
        path: &Path,
        captured: I,
        preferred: Option<K>,
        check: DaemonCheck,
    ) where
        I: IntoIterator<Item = (K, WindowState)>,
    {
        let windows = self.accumulate(path, captured, preferred);
        write_windows(path, windows, check);
    }

    /// Record `captured` for `path` (replacing same-key entries) and
    /// return the union recorded so far, `preferred` first. The pure part
    /// of `accumulate_and_write`, split out so the ordering/dedup is
    /// testable without touching the filesystem.
    fn accumulate<I>(
        &mut self,
        path: &Path,
        captured: I,
        preferred: Option<K>,
    ) -> Vec<WindowState>
    where
        I: IntoIterator<Item = (K, WindowState)>,
    {
        let acc = self.per_file.entry(path.to_path_buf()).or_default();
        for (key, window) in captured {
            acc.insert(key, window);
        }
        let mut windows: Vec<WindowState> = Vec::with_capacity(acc.len());
        if let Some(pref) = preferred {
            if let Some(w) = acc.get(&pref) {
                windows.push(w.clone());
            }
        }
        for (key, w) in acc.iter() {
            if Some(*key) != preferred {
                windows.push(w.clone());
            }
        }
        windows
    }

    /// Replace the recorded set for `path` with exactly `captured` and
    /// write it. Used by explicit "save now" actions (Ctrl+Shift+S,
    /// Save As), where the saved file should mirror the windows currently
    /// open, not resurrect ones closed earlier this run.
    pub fn replace_and_write<I>(
        &mut self,
        path: &Path,
        captured: I,
        preferred: Option<K>,
        check: DaemonCheck,
        keep_pinned: bool,
    ) where
        I: IntoIterator<Item = (K, WindowState)>,
    {
        let (windows, dropped) = self.replace(path, captured, preferred, keep_pinned);
        // A shrunken capture means a window closed since the last
        // write: its daemons were killed, and only a probe keeps the
        // carry-forward merge from resurrecting them as rescued tabs
        // (while still carrying another instance's live daemons). The
        // escalation is rare — one probed write per close — so an
        // AssumeAlive caller keeps its cheap steady state.
        let check = if dropped { DaemonCheck::Probe } else { check };
        write_windows(path, windows, check);
    }

    /// Drop everything recorded for `path` this run. Used when the
    /// session is declined ("don't save"): a later save through the
    /// accumulator must not resurrect windows whose daemons the
    /// decline just killed.
    pub fn forget(&mut self, path: &Path) {
        self.per_file.remove(path);
        self.pinned.remove(path);
    }

    /// The pure part of `replace_and_write`: forget everything recorded
    /// for `path` this run, then record and return `captured` — plus
    /// any pinned windows, which stay resumable across mirrors (unless
    /// `keep_pinned` is false, which also clears the pins). Also
    /// reports whether the previous record held windows the new
    /// capture no longer has — the signal that a window closed since
    /// the last write of `path`.
    fn replace<I>(
        &mut self,
        path: &Path,
        captured: I,
        preferred: Option<K>,
        keep_pinned: bool,
    ) -> (Vec<WindowState>, bool)
    where
        I: IntoIterator<Item = (K, WindowState)>,
    {
        let previous = self.per_file.remove(path);
        if !keep_pinned {
            self.pinned.remove(path);
        }
        if let Some(old) = &previous {
            if let Some(pins) = self.pinned.get(path) {
                let acc = self.per_file.entry(path.to_path_buf()).or_default();
                for (k, w) in old {
                    if pins.contains(k) {
                        acc.insert(*k, w.clone());
                    }
                }
            }
        }
        let windows = self.accumulate(path, captured, preferred);
        let dropped = previous.is_some_and(|old| {
            let acc = self.per_file.entry(path.to_path_buf()).or_default();
            old.keys().any(|k| !acc.contains_key(k))
        });
        (windows, dropped)
    }
}

/// Synchronously kill the local persistent daemons of the CURRENT tab
/// only, unlinking their sockets so nothing lingers. Used when session
/// restore discards the window's throwaway default tab: that tab's
/// daemon was spawned at launch (persistent=true) but is never part of
/// the restored session, so it must die cleanly — not be SIGHUP'd into
/// an `exited(129)` lingering record, and not be detached into a
/// leaked `running` orphan. Killing while we still hold a client
/// connection makes the daemon flush and unlink instead of linger.
#[cfg(unix)]
pub fn kill_current_tab_daemons<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
) {
    for item in ctx_manager.current_grid().contexts().values() {
        if let context::PaneBackend::Ptyd {
            socket, host: None, ..
        } = &item.val.backend
        {
            context::kill_local_daemon(socket);
        }
    }
}

/// Synchronously kill every local v2 daemon of this window: used when
/// the user declines the quit-time save prompt — "don't save" means
/// discard the session including its processes. Remote (ssh-hosted)
/// panes are left alone: their lifecycle belongs to the remote machine.
#[cfg(unix)]
pub fn kill_persistent_panes<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
) {
    for grid in ctx_manager.grids() {
        for item in grid.contexts().values() {
            if let context::PaneBackend::Ptyd {
                socket, host: None, ..
            } = &item.val.backend
            {
                context::kill_local_daemon(socket);
            }
        }
    }
}

/// Sockets a saved-session discard must kill: every LOCAL daemon the
/// state references except those in `spare`. Remote (ssh-hosted) panes
/// never qualify — their lifecycle belongs to the other machine. Split
/// from the kill so target selection is testable without live daemons.
#[cfg(unix)]
fn saved_session_kill_targets<'a>(
    state: &'a SessionState,
    spare: &std::collections::HashSet<String>,
) -> Vec<&'a str> {
    fn walk<'a>(
        node: &'a LayoutNode,
        spare: &std::collections::HashSet<String>,
        out: &mut Vec<&'a str>,
    ) {
        match node {
            LayoutNode::Leaf(pane) => {
                if pane.host.is_some() {
                    return;
                }
                let Some(socket) = &pane.socket else {
                    return;
                };
                if !spare.contains(socket) {
                    out.push(socket);
                }
            }
            LayoutNode::Split { children, .. } => {
                for (_, c) in children {
                    walk(c, spare, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    for win in &state.windows {
        for tab in &win.tabs {
            walk(&tab.layout, spare, &mut out);
        }
    }
    out
}

/// Synchronously kill every LOCAL daemon named in a saved session,
/// unlinking its socket. Used when the user discards a saved session
/// (declined resume, declined save-on-close): the file may hold the
/// only handle on daemons whose windows are long closed. Daemons whose
/// socket is in `spare` are left alone — they are attached to a window
/// that stays open, and that window's own close will decide their fate.
#[cfg(unix)]
pub fn kill_saved_session_daemons(
    state: &SessionState,
    spare: &std::collections::HashSet<String>,
) {
    for socket in saved_session_kill_targets(state, spare) {
        context::kill_local_daemon(std::path::Path::new(socket));
    }
}

/// Collect the socket paths of a window's local ptyd panes into `out`.
/// Used to spare a still-open window's daemons when a shared session
/// file it appears in is discarded.
#[cfg(unix)]
pub fn live_local_sockets<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
    out: &mut std::collections::HashSet<String>,
) {
    for grid in ctx_manager.grids() {
        for item in grid.contexts().values() {
            if let context::PaneBackend::Ptyd {
                socket, host: None, ..
            } = &item.val.backend
            {
                out.insert(socket.to_string_lossy().into_owned());
            }
        }
    }
}

/// Keep names filesystem-safe: anything outside [A-Za-z0-9._-]
/// becomes '-'.
pub fn sanitize_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// A local rio-ptyd daemon is alive iff its socket still accepts a
/// connection. Used by the save merge to tell a keep-worthy daemon
/// (still running, just not reattached this session) from a dead one
/// (nothing to preserve).
#[cfg(unix)]
fn daemon_alive(socket: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Collect every `pane_id` referenced by a state (all tabs, all leaves).
fn pane_ids(state: &SessionState) -> std::collections::HashSet<String> {
    fn walk(node: &LayoutNode, out: &mut std::collections::HashSet<String>) {
        match node {
            LayoutNode::Leaf(p) => {
                if let Some(id) = &p.pane_id {
                    out.insert(id.clone());
                }
            }
            LayoutNode::Split { children, .. } => {
                for (_, c) in children {
                    walk(c, out);
                }
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    for w in &state.windows {
        for t in &w.tabs {
            walk(&t.layout, &mut out);
        }
    }
    out
}

/// True when a single leaf still points at a live local daemon this
/// session did not reattach; `AssumeAlive` trusts every candidate
/// instead of probing its socket.
#[cfg(unix)]
fn leaf_alive(
    p: &PaneState,
    already: &std::collections::HashSet<String>,
    check: DaemonCheck,
) -> bool {
    let (Some(id), Some(sock)) = (&p.pane_id, &p.socket) else {
        return false;
    };
    p.host.is_none()
        && !already.contains(id)
        && (check == DaemonCheck::AssumeAlive || daemon_alive(sock))
}

/// True when an entire tab is worth carrying forward: every leaf points
/// at a live local daemon this session did not reattach. A single-pane
/// tab qualifies when its leaf is live; a split qualifies only when ALL
/// its leaves are live, so it can be faithfully reattached as a whole.
/// A split with any dead leaf is dropped — but that means its still-live
/// leaves would be stranded, so those are handled separately by the
/// caller. Here we only accept fully-live tabs.
#[cfg(unix)]
fn tab_is_fully_live(
    node: &LayoutNode,
    already: &std::collections::HashSet<String>,
    check: DaemonCheck,
) -> bool {
    match node {
        LayoutNode::Leaf(p) => leaf_alive(p, already, check),
        LayoutNode::Split { children, .. } => {
            !children.is_empty()
                && children
                    .iter()
                    .all(|(_, c)| tab_is_fully_live(c, already, check))
        }
    }
}

/// Collect every still-live local leaf of a subtree that this session
/// did not reattach. Used to rescue the live panes of a split whose
/// other leaves died: the split can't be reattached faithfully, but its
/// surviving daemons must not be silently stranded.
#[cfg(unix)]
fn collect_live_leaves<'a>(
    node: &'a LayoutNode,
    already: &std::collections::HashSet<String>,
    check: DaemonCheck,
    out: &mut Vec<&'a PaneState>,
) {
    match node {
        LayoutNode::Leaf(p) => {
            if leaf_alive(p, already, check) {
                out.push(p);
            }
        }
        LayoutNode::Split { children, .. } => {
            for (_, c) in children {
                collect_live_leaves(c, already, check, out);
            }
        }
    }
}

/// Merge still-alive daemons from a previously saved session into a
/// freshly captured one before it overwrites the file. Without this,
/// saving a new session over an old file silently orphans any daemon
/// the old file recorded but the new session never reattached — no file
/// references it, so no future launch can reclaim it. This is what
/// makes the resume prompt's "new + keep old" honest: the kept daemons
/// survive AND stay resumable across the overwrite.
///
/// A fully-live tab (single pane or a split whose leaves are all live)
/// is carried forward whole so its layout reattaches intact. A split
/// with some dead leaves can't be rebuilt faithfully, so each of its
/// surviving leaves is carried as its own single-pane tab — dropping
/// them would strand their live daemons. Dead daemons are dropped.
#[cfg(unix)]
pub fn merge_kept_daemons(
    new_state: &mut SessionState,
    old_path: &Path,
    check: DaemonCheck,
) {
    let Some(old) = SessionState::load(old_path) else {
        return;
    };
    let already = pane_ids(new_state);
    let mut kept: Vec<TabState> = Vec::new();
    for w in old.windows {
        for tab in w.tabs {
            if tab_is_fully_live(&tab.layout, &already, check) {
                kept.push(tab);
            } else {
                // Rescue the still-live leaves of a partially-dead
                // split as standalone single-pane tabs.
                let mut live = Vec::new();
                collect_live_leaves(&tab.layout, &already, check, &mut live);
                for pane in live {
                    kept.push(TabState {
                        custom_title: None,
                        layout: LayoutNode::Leaf(PaneState {
                            cwd: pane.cwd.clone(),
                            title: pane.title.clone(),
                            is_active: true,
                            scrollback: pane.scrollback.clone(),
                            pane_id: pane.pane_id.clone(),
                            socket: pane.socket.clone(),
                            host: pane.host.clone(),
                        }),
                    });
                }
            }
        }
    }
    if kept.is_empty() {
        return;
    }
    // Append kept tabs to the first window so a later launch restores
    // both the current session and the preserved daemons.
    if let Some(win) = new_state.windows.first_mut() {
        win.tabs.extend(kept);
    }
}

/// Saved session names (sessions/*.json), sorted.
pub fn list_sessions() -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(rio_backend::config::sessions_dir_path())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    path.file_stem().map(|s| s.to_string_lossy().into_owned())
                } else {
                    None
                }
            })
            .collect();
    names.sort();
    names
}

/// Capture one window's tabs from the live context tree.
pub fn capture_window<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &ContextManager<T>,
    max_scrollback_lines: usize,
    winit_window: &rio_window::window::Window,
) -> WindowState {
    let size = winit_window.inner_size();
    let position = winit_window.outer_position().ok().map(|p| (p.x, p.y));
    let tabs = ctx_manager
        .grids()
        .iter()
        .map(|grid| TabState {
            custom_title: grid.custom_title.clone(),
            layout: grid.to_layout_node(&mut |ctx, is_active| {
                capture_pane(ctx, is_active, max_scrollback_lines)
            }),
        })
        .collect();

    WindowState {
        tabs,
        active_tab: ctx_manager.current_index(),
        size: (size.width, size.height),
        position,
    }
}

fn capture_pane<T: EventListener>(
    ctx: &context::Context<T>,
    is_active: bool,
    max_scrollback_lines: usize,
) -> PaneState {
    #[cfg(not(target_os = "windows"))]
    let mut cwd = match &ctx.backend {
        // shell_pid belongs to the remote host; probing the local
        // /proc with it would read an unrelated process's cwd. Local
        // Ptyd panes fall through on purpose: their placeholder
        // main_fd fails tcgetpgrp, and the probe's shell_pid fallback
        // resolves the daemon-owned shell's live cwd on this host.
        context::PaneBackend::Ptyd { host: Some(_), .. } => None,
        _ => teletypewriter::foreground_process_path(*ctx.main_fd, ctx.shell_pid)
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    #[cfg(target_os = "windows")]
    let mut cwd: Option<String> = None;

    let terminal = ctx.terminal.lock();
    if cwd.is_none() {
        cwd = terminal
            .current_directory
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
    }
    let scrollback = terminal.scrollback_to_ansi(max_scrollback_lines);
    drop(terminal);

    let (pane_id, socket, host) = match &ctx.backend {
        context::PaneBackend::Local => (None, None, None),
        context::PaneBackend::Ptyd {
            pane_id,
            socket,
            host,
            reported_cwd,
            ..
        } => {
            // Last resort only: the daemon's attach-time cwd, used when
            // the shell never emitted OSC 7 (no live cwd known).
            if cwd.is_none() {
                cwd.clone_from(reported_cwd);
            }
            (
                Some(pane_id.clone()),
                Some(socket.to_string_lossy().into_owned()).filter(|s| !s.is_empty()),
                host.clone(),
            )
        }
    };

    PaneState {
        cwd,
        title: Some(ctx.title.content.clone()).filter(|t| !t.is_empty()),
        is_active,
        scrollback,
        pane_id,
        socket,
        host,
    }
}

/// Rebuild one tab's split tree inside the current (single-pane) grid.
/// The grid's sole pane must already have been spawned for
/// `layout.first_leaf()`; this splits out the remaining panes and
/// injects each leaf's scrollback.
pub fn restore_tab_layout<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) {
    let active = build_node(ctx_manager, layout, sugarloaf);
    ctx_manager.current_grid_mut().apply_layout_weights(layout);
    // The saved active leaf is reselected by the live node id captured
    // while it was built, never by ordinal: the saved tree counts
    // leaves depth-first while pane navigation orders them visually
    // (y, then x), and the two disagree for nested splits. When the
    // active leaf's split failed there is nothing to select and the
    // build's last pane stays current.
    if let Some(node) = active {
        ctx_manager.select_pane_node(node);
    }
}

/// Returns the live node of the leaf flagged `is_active`, when its
/// pane was built.
fn build_node<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) -> Option<taffy::NodeId> {
    match layout {
        LayoutNode::Leaf(pane) => {
            inject_scrollback(ctx_manager, pane);
            // `current` is this leaf's pane: the caller selected it
            // before recursing (or it is the tab's base pane).
            pane.is_active.then(|| ctx_manager.current_grid().current)
        }
        LayoutNode::Split {
            direction,
            children,
        } => {
            // Split the subtree's base leaf once per extra child (rio
            // produces binary trees; >2 children rebuild as a nested
            // chain, which keeps content and approximates ratios).
            let base = ctx_manager.current_grid().current;
            let mut leaves = vec![Some(base)];
            for (_, child) in children.iter().skip(1) {
                let created = ctx_manager.split_with_dir(
                    context::next_rich_text_id(),
                    *direction == SplitDir::Vertical,
                    sugarloaf,
                    child.first_leaf().spawn(),
                );
                leaves.push(created.then(|| ctx_manager.current_grid().current));
            }
            let mut active = None;
            for (i, (_, child)) in children.iter().enumerate() {
                // A failed split has no leaf; restoring its subtree
                // would land in whichever pane is still current.
                let Some(leaf) = leaves[i] else { continue };
                ctx_manager.current_grid_mut().set_current(leaf);
                if let Some(node) = build_node(ctx_manager, child, sugarloaf) {
                    active = Some(node);
                }
            }
            active
        }
    }
}

fn inject_scrollback<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    pane: &PaneState,
) {
    if pane.scrollback.is_empty() {
        return;
    }
    let ctx = ctx_manager.current_mut();
    // A pane that reattached to a live daemon repaints from the
    // daemon's replay stream; injecting saved scrollback would
    // duplicate it. A plain-local pane or a freshly spawned daemon
    // (dead-daemon fallback, empty ring) replays nothing, so it needs
    // the saved scrollback.
    if matches!(
        ctx.backend,
        context::PaneBackend::Ptyd { replayed: true, .. }
    ) {
        return;
    }
    let mut processor = rio_backend::performer::handler::Processor::default();
    let mut terminal = ctx.terminal.lock();
    // Saved scrollback is replayed history, not live input: any query
    // it contains (DA, cursor-position, …) must not generate a reply,
    // or a stale answer would be fed back as input. Mirrors the daemon
    // replay path (performer::mod suppress_replies).
    let prev_suppress = terminal.suppress_replies;
    terminal.suppress_replies = true;
    processor.advance(&mut *terminal, pane.scrollback.as_bytes());
    terminal.suppress_replies = prev_suppress;
}

#[cfg(test)]
mod saved_windows_tests {
    use super::{LayoutNode, PaneState, SavedWindows, TabState, WindowState};
    use std::path::Path;

    fn window(pane_ids: &[&str]) -> WindowState {
        let tabs = pane_ids
            .iter()
            .map(|id| TabState {
                custom_title: None,
                layout: LayoutNode::Leaf(PaneState {
                    cwd: None,
                    title: None,
                    is_active: true,
                    scrollback: String::new(),
                    pane_id: Some((*id).to_string()),
                    socket: None,
                    host: None,
                }),
            })
            .collect();
        WindowState {
            tabs,
            active_tab: 0,
            size: (0, 0),
            position: None,
        }
    }

    fn pane_ids(w: &WindowState) -> Vec<String> {
        w.tabs
            .iter()
            .filter_map(|t| match &t.layout {
                LayoutNode::Leaf(p) => p.pane_id.clone(),
                _ => None,
            })
            .collect()
    }

    // Two windows closed one at a time restore as two, not more: the
    // regression where re-saving a window whose tab set had changed
    // between saves left duplicate windows in the file (a 1-tab snapshot
    // AND a 2-tab snapshot of the same window). Keying by WindowId, the
    // second save of window 1 replaces the first instead of appending.
    #[test]
    fn per_window_close_does_not_duplicate() {
        let mut sw: SavedWindows<u32> = SavedWindows::new();
        let path = Path::new("unused-in-accumulate");

        // Close window 1 while both are open: window 1 has one tab so far,
        // window 2 has two.
        let first = sw.accumulate(
            path,
            vec![(1u32, window(&["a"])), (2u32, window(&["b", "c"]))],
            Some(1),
        );
        assert_eq!(first.len(), 2);

        // Close window 2; window 1 meanwhile grew a second tab. Only
        // window 2 is captured now, but the accumulator still holds
        // window 1 — and the fresh capture of window 1 (if any) must
        // replace, never duplicate.
        let second = sw.accumulate(
            path,
            vec![(1u32, window(&["a", "d"])), (2u32, window(&["b", "c"]))],
            Some(2),
        );
        assert_eq!(second.len(), 2, "same two windows, no duplicates");

        // Window 2 is preferred (first), window 1 carries its latest tabs.
        assert_eq!(pane_ids(&second[0]), vec!["b", "c"]);
        let w1 = second.iter().find(|w| pane_ids(w) == vec!["a", "d"]);
        assert!(w1.is_some(), "window 1 present with its grown tab set");
    }

    // Autosave after a window closed: the save only captures the
    // still-open window, but the union must keep the closed one — this
    // is what makes autosave-on-change safe for multi-window sessions.
    #[test]
    fn accumulate_keeps_earlier_closed_windows() {
        let mut sw: SavedWindows<u32> = SavedWindows::new();
        let path = Path::new("unused");
        sw.accumulate(
            path,
            vec![(1u32, window(&["a"])), (2u32, window(&["b"]))],
            Some(2),
        );
        // Window 2 is gone; a tab change in window 1 triggers a save
        // that sees only window 1.
        let out = sw.accumulate(path, vec![(1u32, window(&["a", "c"]))], Some(1));
        assert_eq!(out.len(), 2, "closed window 2 still recorded");
        assert_eq!(pane_ids(&out[0]), vec!["a", "c"]);
        assert!(out.iter().any(|w| pane_ids(w) == vec!["b"]));
    }

    #[test]
    fn replace_drops_earlier_closed_windows() {
        let mut sw: SavedWindows<u32> = SavedWindows::new();
        let path = Path::new("unused");
        sw.accumulate(path, vec![(1u32, window(&["a"]))], Some(1));
        // Mirror snapshot with only window 2 open forgets window 1 and
        // reports the drop, so the write escalates to a daemon probe.
        let (out, dropped) =
            sw.replace(path, vec![(2u32, window(&["b"]))], Some(2), true);
        assert_eq!(out.len(), 1);
        assert_eq!(pane_ids(&out[0]), vec!["b"]);
        assert!(dropped, "window 1 dropping out must be reported");

        // Steady state: same window again, nothing dropped.
        let (out, dropped) =
            sw.replace(path, vec![(2u32, window(&["b"]))], Some(2), true);
        assert_eq!(out.len(), 1);
        assert!(!dropped, "no drop when the open set is unchanged");
    }

    /// A pinned window (closed while its shell still ran a program)
    /// survives mirror saves; an explicit save-now clears the pin.
    #[test]
    fn pinned_window_survives_mirror_saves() {
        let mut sw: SavedWindows<u32> = SavedWindows::new();
        let path = Path::new("unused");
        sw.accumulate(path, vec![(1u32, window(&["a"]))], Some(1));
        sw.pin(path, 1);

        let (out, _) = sw.replace(path, vec![(2u32, window(&["b"]))], Some(2), true);
        assert_eq!(out.len(), 2, "pinned window 1 must survive the mirror");
        assert!(out.iter().any(|w| pane_ids(w) == vec!["a"]));

        // Explicit save-now: mirror exactly, dropping the pin for good.
        let (out, _) = sw.replace(path, vec![(2u32, window(&["b"]))], Some(2), false);
        assert_eq!(out.len(), 1);
        assert_eq!(pane_ids(&out[0]), vec!["b"]);
        let (out, _) = sw.replace(path, vec![(2u32, window(&["b"]))], Some(2), true);
        assert_eq!(out.len(), 1, "pin stays cleared after the explicit save");
    }

    // A declined session must not come back through the accumulator: a
    // later save for the same path starts from scratch.
    #[test]
    fn forget_clears_accumulated_windows() {
        let mut sw: SavedWindows<u32> = SavedWindows::new();
        let path = Path::new("unused");
        sw.accumulate(path, vec![(1u32, window(&["a"]))], Some(1));
        sw.forget(path);
        let out = sw.accumulate(path, vec![(2u32, window(&["b"]))], Some(2));
        assert_eq!(out.len(), 1);
        assert_eq!(pane_ids(&out[0]), vec!["b"]);
    }
}

#[cfg(test)]
mod close_disposition_tests {
    use super::{close_disposition, write_on_quit, CloseDisposition};
    use rio_backend::config::session::SessionRestore;

    #[test]
    fn table() {
        // (named, mode) -> disposition. Locks the confirmed close-save
        // table: named always persists (prompt in disable/prompt, silent
        // in always); unnamed follows the mode (nothing/prompt/silent).
        let d = close_disposition;
        assert_eq!(d(true, SessionRestore::Never), CloseDisposition::Prompt);
        assert_eq!(d(true, SessionRestore::Prompt), CloseDisposition::Prompt);
        assert_eq!(d(true, SessionRestore::Always), CloseDisposition::Save);
        assert_eq!(d(false, SessionRestore::Never), CloseDisposition::Nothing);
        assert_eq!(d(false, SessionRestore::Prompt), CloseDisposition::Prompt);
        assert_eq!(d(false, SessionRestore::Always), CloseDisposition::Save);
    }

    #[test]
    fn quit_write_gate() {
        // Prompt-disposition sessions write only with the quit prompt's
        // consent; in particular a named file under Never/Prompt is
        // never overwritten by a silent quit/exit tail.
        let w = write_on_quit;
        assert!(!w(true, SessionRestore::Never, false));
        assert!(w(true, SessionRestore::Never, true));
        assert!(!w(true, SessionRestore::Prompt, false));
        assert!(w(true, SessionRestore::Prompt, true));
        assert!(w(true, SessionRestore::Always, false));
        assert!(!w(false, SessionRestore::Prompt, false));
        assert!(w(false, SessionRestore::Prompt, true));
        assert!(w(false, SessionRestore::Always, false));
        // Nothing writes nothing, consent or not.
        assert!(!w(false, SessionRestore::Never, false));
        assert!(!w(false, SessionRestore::Never, true));
    }
}

#[cfg(test)]
mod load_validation_tests {
    use super::{
        LayoutNode, PaneState, SessionState, SplitDir, TabState, WindowState,
        SESSION_VERSION,
    };
    use std::path::PathBuf;

    fn leaf() -> LayoutNode {
        LayoutNode::Leaf(PaneState {
            cwd: None,
            title: None,
            is_active: true,
            scrollback: String::new(),
            pane_id: None,
            socket: None,
            host: None,
        })
    }

    fn state_with_tabs(tabs: Vec<TabState>) -> SessionState {
        SessionState {
            version: SESSION_VERSION,
            windows: vec![WindowState {
                tabs,
                active_tab: 0,
                size: (0, 0),
                position: None,
            }],
        }
    }

    fn tmp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rio-session-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn save_load_round_trip() {
        let state = state_with_tabs(vec![TabState {
            custom_title: None,
            layout: LayoutNode::Split {
                direction: SplitDir::Horizontal,
                children: vec![(0.25, leaf()), (0.75, leaf())],
            },
        }]);
        let path = tmp_file("round-trip");
        state.save(&path).unwrap();
        let loaded = SessionState::load(&path).expect("valid file loads");
        assert_eq!(loaded.windows[0].tabs[0].layout.leaf_count(), 2);
        SessionState::discard(&path);
    }

    #[test]
    fn load_rejects_bad_weights() {
        // -1 and 0 deserialize fine; 1e999 either fails to parse or
        // lands as infinity — all four must reject the file.
        for (name, weight) in [("neg", "-1.0"), ("zero", "0.0"), ("inf", "1e999")] {
            let json = format!(
                concat!(
                    r#"{{"version":2,"windows":[{{"active_tab":0,"tabs":[{{"custom_title":null,"#,
                    r#""layout":{{"Split":{{"direction":"Horizontal","children":["#,
                    r#"[{},{{"Leaf":{{"cwd":null,"title":null,"is_active":true,"scrollback":""}}}}],"#,
                    r#"[1.0,{{"Leaf":{{"cwd":null,"title":null,"is_active":false,"scrollback":""}}}}]"#,
                    r#"]}}}}}}]}}]}}"#
                ),
                weight
            );
            let path = tmp_file(name);
            std::fs::write(&path, json).unwrap();
            assert!(
                SessionState::load(&path).is_none(),
                "weight {weight} must reject the file"
            );
            SessionState::discard(&path);
        }
    }

    #[test]
    fn load_rejects_pane_flood() {
        let tabs: Vec<TabState> = (0..=SessionState::MAX_WINDOW_PANES)
            .map(|_| TabState {
                custom_title: None,
                layout: leaf(),
            })
            .collect();
        let state = state_with_tabs(tabs);
        let path = tmp_file("flood");
        state.save(&path).unwrap();
        assert!(SessionState::load(&path).is_none());
        SessionState::discard(&path);
    }
}

#[cfg(all(test, unix))]
mod kill_target_tests {
    use super::{
        saved_session_kill_targets, LayoutNode, PaneState, SessionState, SplitDir,
        TabState, WindowState, SESSION_VERSION,
    };

    fn pane(socket: Option<&str>, host: Option<&str>) -> PaneState {
        PaneState {
            cwd: None,
            title: None,
            is_active: true,
            scrollback: String::new(),
            pane_id: Some("p".into()),
            socket: socket.map(str::to_string),
            host: host.map(str::to_string),
        }
    }

    #[test]
    fn spares_live_windows_and_remote_panes() {
        let state = SessionState {
            version: SESSION_VERSION,
            windows: vec![WindowState {
                tabs: vec![
                    TabState {
                        custom_title: None,
                        layout: LayoutNode::Split {
                            direction: SplitDir::Horizontal,
                            children: vec![
                                (1.0, LayoutNode::Leaf(pane(Some("/s/dead"), None))),
                                (1.0, LayoutNode::Leaf(pane(Some("/s/live"), None))),
                            ],
                        },
                    },
                    TabState {
                        custom_title: None,
                        layout: LayoutNode::Leaf(pane(Some("/s/remote"), Some("host"))),
                    },
                    TabState {
                        custom_title: None,
                        layout: LayoutNode::Leaf(pane(None, None)),
                    },
                ],
                active_tab: 0,
                size: (0, 0),
                position: None,
            }],
        };
        let spare = std::collections::HashSet::from(["/s/live".to_string()]);
        // Only the local, unspared socket is a kill target: the spared
        // one belongs to a still-open window, the remote one to another
        // machine, and a socketless pane has nothing to kill.
        assert_eq!(saved_session_kill_targets(&state, &spare), vec!["/s/dead"]);
    }
}
