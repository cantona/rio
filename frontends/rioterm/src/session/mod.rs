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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub layout: LayoutNode,
    pub custom_title: Option<String>,
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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
    /// least one child. A hand-edited / corrupt file with an empty
    /// `Split` would otherwise panic in `first_leaf`'s `children[0]`.
    fn well_formed(&self) -> bool {
        match self {
            LayoutNode::Leaf(_) => true,
            LayoutNode::Split { children, .. } => {
                !children.is_empty() && children.iter().all(|(_, c)| c.well_formed())
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
    pub fn load(path: &Path) -> Option<SessionState> {
        let bytes = std::fs::read(path).ok()?;
        let state: SessionState = serde_json::from_slice(&bytes).ok()?;
        if state.version != SESSION_VERSION || state.windows.is_empty() {
            return None;
        }
        // Reject a structurally-broken tree (an empty Split) rather
        // than panic later in first_leaf on a corrupt/hostile file.
        let all_ok = state
            .windows
            .iter()
            .flat_map(|w| &w.tabs)
            .all(|t| t.layout.well_formed());
        if !all_ok {
            tracing::warn!("session: discarding file with a malformed layout tree");
            return None;
        }
        Some(state)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }

    pub fn discard(path: &Path) {
        let _ = std::fs::remove_file(path);
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

/// Synchronously kill every LOCAL daemon named in a saved session,
/// unlinking its socket. Used when the user declines a resume with
/// "start new + discard old": the daemons were never restored into any
/// context, so the only handle on them is the session file's saved
/// sockets. Remote (ssh-hosted) panes are left alone — their lifecycle
/// belongs to the other machine. Walk every tab's layout tree.
#[cfg(unix)]
pub fn kill_saved_session_daemons(state: &SessionState) {
    fn kill_leaf(pane: &PaneState) {
        if pane.host.is_some() {
            return;
        }
        let Some(socket) = &pane.socket else {
            return;
        };
        context::kill_local_daemon(std::path::Path::new(socket));
    }
    fn walk(node: &LayoutNode) {
        match node {
            LayoutNode::Leaf(pane) => kill_leaf(pane),
            LayoutNode::Split { children, .. } => {
                for (_, c) in children {
                    walk(c);
                }
            }
        }
    }
    for win in &state.windows {
        for tab in &win.tabs {
            walk(&tab.layout);
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

/// True when a layout leaf still points at a live daemon this session
/// did not reattach — a "kept" pane worth carrying forward so the file
/// keeps a handle on it. A leaf is kept only if it is a single-pane
/// tab: a partially-live split can't be faithfully reattached, so it is
/// dropped (its dead panes are gone anyway).
#[cfg(unix)]
fn leaf_is_kept_alive(
    node: &LayoutNode,
    already: &std::collections::HashSet<String>,
) -> bool {
    let LayoutNode::Leaf(p) = node else {
        return false;
    };
    let (Some(id), Some(sock)) = (&p.pane_id, &p.socket) else {
        return false;
    };
    p.host.is_none() && !already.contains(id) && daemon_alive(sock)
}

/// Merge still-alive daemons from a previously saved session into a
/// freshly captured one before it overwrites the file. Without this,
/// saving a new session over an old file silently orphans any daemon
/// the old file recorded but the new session never reattached — no file
/// references it, so no future launch can reclaim it. This is what
/// makes the resume prompt's "new + keep old" honest: the kept daemons
/// survive AND stay resumable across the overwrite. Only single-pane
/// tabs on local, live daemons are carried; dead ones are dropped.
#[cfg(unix)]
pub fn merge_kept_daemons(new_state: &mut SessionState, old_path: &Path) {
    let Some(old) = SessionState::load(old_path) else {
        return;
    };
    let already = pane_ids(new_state);
    let mut kept: Vec<TabState> = Vec::new();
    for w in old.windows {
        for tab in w.tabs {
            if leaf_is_kept_alive(&tab.layout, &already) {
                kept.push(tab);
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
    build_node(ctx_manager, layout, sugarloaf);
    ctx_manager.current_grid_mut().apply_layout_weights(layout);
    // A skipped (failed) split leaf shifts every later leaf's ordinal,
    // so the saved active index would land on the wrong pane. Only
    // reselect when the live tree matches the saved one; weights above
    // carry their own equivalent guard.
    if ctx_manager.current_grid().len() == layout.leaf_count() {
        restore_active_pane(ctx_manager, layout);
    }
}

fn build_node<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
    sugarloaf: &mut rio_backend::sugarloaf::Sugarloaf,
) {
    match layout {
        LayoutNode::Leaf(pane) => inject_scrollback(ctx_manager, pane),
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
            for (i, (_, child)) in children.iter().enumerate() {
                // A failed split has no leaf; restoring its subtree
                // would land in whichever pane is still current.
                let Some(leaf) = leaves[i] else { continue };
                ctx_manager.current_grid_mut().set_current(leaf);
                build_node(ctx_manager, child, sugarloaf);
            }
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
    processor.advance(&mut *terminal, pane.scrollback.as_bytes());
}

fn restore_active_pane<T: EventListener + Clone + Send + 'static>(
    ctx_manager: &mut ContextManager<T>,
    layout: &LayoutNode,
) {
    // Leaves were built depth-first in the same order to_layout_node
    // walks them, so re-walk and select the one flagged active.
    fn find_active_index(node: &LayoutNode, next: &mut usize) -> Option<usize> {
        match node {
            LayoutNode::Leaf(p) => {
                let idx = *next;
                *next += 1;
                p.is_active.then_some(idx)
            }
            LayoutNode::Split { children, .. } => children
                .iter()
                .find_map(|(_, c)| find_active_index(c, next)),
        }
    }
    let mut counter = 0;
    if let Some(idx) = find_active_index(layout, &mut counter) {
        ctx_manager.select_pane_by_order(idx);
    }
}
