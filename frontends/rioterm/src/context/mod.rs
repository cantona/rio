pub mod renderable;
pub mod title;

use crate::ansi::CursorShape;
use crate::context::title::{update_title, ContextTitle};
use crate::event::sync::FairMutex;
use crate::event::{Msg, RioEvent};
pub use crate::layout::{ContextDimension, ContextGrid, ContextGridItem};
use crate::messenger::Messenger;
use crate::performer::Machine;
use renderable::Cursor;
use renderable::RenderableContent;
use rio_backend::config::layout::Margin;
use rio_backend::config::Shell;
use smallvec::{smallvec, SmallVec};

use rio_backend::crosswords::{Crosswords, MIN_COLUMNS, MIN_LINES};
use rio_backend::error::{RioError, RioErrorLevel, RioErrorType};
use rio_backend::event::EventListener;
use rio_backend::event::WindowId;
use rio_backend::selection::SelectionRange;
use rio_backend::sugarloaf::{font::SugarloafFont, Rect, Sugarloaf, SugarloafErrors};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

// Global atomic counter for generating unique route IDs
static ROUTE_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

// Global atomic counter for generating unique rich text IDs
static RICH_TEXT_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Generate a unique rich text ID for terminal contexts
pub fn next_rich_text_id() -> usize {
    RICH_TEXT_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
use teletypewriter::create_pty;
#[cfg(not(target_os = "windows"))]
use teletypewriter::{create_pty_with_fork, create_pty_with_spawn};

/// How this pane's shell is hosted. `Ptyd` panes live behind a
/// rio-ptyd session daemon and survive rio exiting.
#[derive(Clone, Debug, PartialEq)]
pub enum PaneBackend {
    Local,
    Ptyd {
        pane_id: String,
        socket: std::path::PathBuf,
        /// ssh destination when the daemon lives on another machine.
        host: Option<String>,
        /// cwd the daemon reported at attach (from the shell's
        /// /proc/<pid>/cwd). Cached here because a ptyd pane's
        /// `main_fd` is a placeholder, so the usual foreground-process
        /// cwd probe can't run — this is what session capture stores
        /// for the dead-daemon fresh-spawn fallback.
        reported_cwd: Option<String>,
        /// True when this pane reattached to a pre-existing daemon
        /// (its ring replays the prior screen). False for a freshly
        /// spawned daemon — including the dead-daemon restore fallback,
        /// whose empty ring replays nothing, so saved scrollback must
        /// still be injected just like a plain-local pane.
        replayed: bool,
    },
}

/// Where a restored pane should attach instead of spawning fresh.
#[derive(Clone, Debug)]
pub enum AttachTarget {
    Unix {
        pane_id: String,
        socket: std::path::PathBuf,
    },
    Ssh {
        host: String,
        pane_id: String,
    },
}

/// What a restored pane needs to come back: where to reattach (a
/// live daemon) and the cwd for the fresh-spawn fallback.
#[derive(Clone, Debug, Default)]
pub struct PaneSpawn {
    pub cwd: Option<String>,
    pub attach: Option<AttachTarget>,
}

/// Set while the application is quitting: persistent panes then
/// detach (daemon keeps the shell) instead of killing.
static QUIT_DETACHING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_quit_detaching() {
    QUIT_DETACHING.store(true, Ordering::SeqCst);
}

/// Clear the detach flag. Used to bracket a single-window close so its
/// detach semantics don't leak into other windows' later tab-closes
/// (the flag is a process global read by every `Context::drop`).
pub fn clear_quit_detaching() {
    QUIT_DETACHING.store(false, Ordering::SeqCst);
}

/// Synchronously tell a local rio-ptyd daemon to kill its shell and
/// unlink, over a fresh short-lived client connection. Sent on a
/// deliberate close (tab close, declined save/resume, discarded restore
/// tab) so the daemon flushes and cleanup()s instead of reaping a dead
/// shell detached and lingering as `exited(129)`. Best effort: a daemon
/// that already went away just refuses the connection. The single place
/// the client side speaks Kill over a unix socket — all callers route
/// here rather than re-encoding the frame sequence.
#[cfg(unix)]
pub(crate) fn kill_local_daemon(socket: &std::path::Path) {
    use rio_ptyd::protocol as p;
    use std::time::Duration;
    // Runs on the UI thread inside Context::drop while a window closes.
    // A shell `exit` makes the daemon self-clean and drop its socket at
    // the same moment; connecting or writing to a peer that is tearing
    // down can otherwise block that thread and freeze the close. Bound
    // both the connect and the writes so a racing/dead daemon can never
    // hang the UI — if the daemon is already gone the shell is dead
    // anyway, which is the outcome we wanted.
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = p::write_frame(
            &mut stream,
            p::FrameType::ClientHello,
            &p::encode_client_hello(),
        );
        let _ = p::write_frame(&mut stream, p::FrameType::Kill, &[]);
        // Wait (bounded) for the daemon to actually die: drain until it
        // closes this connection, then until the socket is unlinked. A
        // save that runs right after a close probes the socket to decide
        // which daemons to carry forward — probing a daemon that is
        // mid-cleanup reads "alive" and resurrects the killed panes as
        // rescued tabs. Deliberate closes are rare, the daemon reacts in
        // milliseconds, and both waits are capped so a wedged daemon
        // can't hang the UI.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let mut sink = [0u8; 256];
        loop {
            match std::io::Read::read(&mut stream, &mut sink) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        for _ in 0..40 {
            if !socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub struct Context<T: EventListener> {
    pub route_id: usize,
    pub terminal: Arc<FairMutex<Crosswords<T>>>,
    pub renderable_content: RenderableContent,
    pub messenger: Messenger,
    #[cfg(not(target_os = "windows"))]
    pub main_fd: Arc<i32>,
    #[cfg(not(target_os = "windows"))]
    pub shell_pid: u32,
    #[cfg(not(target_os = "windows"))]
    child_terminator: teletypewriter::ChildTerminator,
    pub rich_text_id: usize,
    pub dimension: ContextDimension,
    pub title: ContextTitle,
    /// An OSC title change arrived while this pane was hidden: the
    /// render was skipped and must run when the pane surfaces.
    pub title_dirty: bool,
    /// Display name of the command this pane spawned (the configured
    /// shell or program), fixed for the pane's lifetime. Fills
    /// `{{ program }}` and the empty-title strip fallback without ever
    /// inspecting the foreground process.
    pub spawned_program: String,
    pub backend: PaneBackend,
    _io_thread: Option<JoinHandle<()>>,
}

impl<T: rio_backend::event::EventListener> Drop for Context<T> {
    fn drop(&mut self) {
        match &self.backend {
            PaneBackend::Local => {
                // Shutdown the terminal's PTY.
                let _ = self.messenger.channel.send(Msg::Shutdown);

                // Also hang up synchronously: quit paths exit right after
                // dropping routes, before the reader thread can run its
                // shutdown escalation. The handle is a no-op once the child
                // was reaped, so no stale PID is ever signaled.
                #[cfg(not(target_os = "windows"))]
                let _ = self.child_terminator.hangup();
            }
            PaneBackend::Ptyd { host, socket, .. } => {
                if QUIT_DETACHING.load(Ordering::SeqCst) {
                    // Detach: the daemon keeps the shell; even an
                    // unprocessed Shutdown is fine — process exit
                    // closing the socket IS a detach.
                    let _ = self.messenger.channel.send(Msg::Shutdown);
                } else {
                    // Deliberate close: kill the shell and unlink the
                    // daemon. For a LOCAL daemon send the Kill frame
                    // synchronously over the socket while this client is
                    // still connected — the daemon then flushes and
                    // cleanup()s (unlink), rather than lingering as
                    // exited(129). NEVER kill_pid the shell here: the
                    // daemon owns it in its own session, and racing it
                    // with a direct SIGHUP is what left the daemon
                    // reaping a dead shell with no client attached, i.e.
                    // the lingering-exited leak. A remote daemon has no
                    // local socket, so its Kill still rides the io
                    // thread's ssh transport.
                    #[cfg(unix)]
                    if host.is_none() {
                        kill_local_daemon(socket);
                    } else {
                        let _ = self.messenger.channel.send(Msg::Kill);
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = host;
                        let _ = self.messenger.channel.send(Msg::Kill);
                    }
                }
            }
        }
    }
}

impl<T: EventListener> Context<T> {
    /// Foreground-process path via the pty, but never for a remote
    /// ptyd pane: its `shell_pid` belongs to another machine, so
    /// probing the local `/proc` with it would read an unrelated local
    /// process (wrong title, wrong cwd inheritance). `None` there lets
    /// callers fall back to their own default.
    #[cfg(not(target_os = "windows"))]
    #[inline]
    pub fn foreground_path(&self) -> Option<std::path::PathBuf> {
        if matches!(self.backend, PaneBackend::Ptyd { host: Some(_), .. }) {
            return None;
        }
        teletypewriter::foreground_process_path(*self.main_fd, self.shell_pid).ok()
    }

    #[inline]
    pub fn set_selection(&mut self, selection_range: Option<SelectionRange>) {
        let old_selection = self.renderable_content.selection_range;
        let has_updated = old_selection != selection_range;

        if has_updated {
            // Selection affects terminal line rendering, so use terminal damage
            self.renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
        }

        self.renderable_content.selection_range = selection_range;
    }

    #[inline]
    pub fn cursor_from_ref(&self) -> Cursor {
        Cursor {
            state: self.renderable_content.cursor.state.new_from_self(),
            content: self.renderable_content.cursor.content,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PersistenceOptions {
    pub ring_bytes: usize,
}

#[derive(Clone, Default)]
pub struct ContextManagerConfig {
    /// Build contexts without spawning a PTY (see
    /// `create_dead_context`). Unit tests fork one real `$SHELL` per
    /// context otherwise, which is slow and flaky under the parallel
    /// test runner (fork failures surface as random test panics).
    #[cfg(test)]
    pub dead_pty: bool,
    pub shell: Shell,
    /// Some = run every pane behind a rio-ptyd daemon so it survives
    /// rio exiting. None on Windows or when [session] persistent=false.
    pub persistence: Option<PersistenceOptions>,
    /// Persist the session on structural changes. True only for
    /// `restore = "always"` (automatic mode): `prompt` never saves
    /// without the user's yes, so it must not autosave.
    pub autosave: bool,
    /// Named session this instance is bound to (`rio --session NAME`),
    /// tagged onto each spawned daemon so `rio-ptyd list` can group
    /// panes by session. None for the default/unnamed session.
    pub session_name: Option<String>,
    #[cfg(not(target_os = "windows"))]
    pub use_fork: bool,
    pub shell_integration: bool,
    pub working_dir: Option<String>,
    pub spawn_performer: bool,
    pub cwd: bool,
    pub is_native: bool,
    pub split_color: [f32; 4],
    pub split_active_color: [f32; 4],
    pub panel: rio_backend::config::layout::Panel,
    pub title: rio_backend::config::title::Title,
    pub keyboard: rio_backend::config::keyboard::Keyboard,
    pub scrollback_history_limit: usize,
    pub grapheme_clustering: bool,
}

const DEFAULT_CONTEXT_CAPACITY: usize = 28;

pub struct ContextManager<T: EventListener> {
    contexts: SmallVec<[ContextGrid<T>; DEFAULT_CONTEXT_CAPACITY]>,
    current_index: usize,
    current_route: usize,
    #[allow(unused)]
    capacity: usize,
    event_proxy: T,
    window_id: WindowId,
    pub config: ContextManagerConfig,
}

/// Display name for the command a pane spawns: the configured program,
/// else the user's shell (what the PTY spawn itself falls back to),
/// reduced to its basename.
fn spawned_program_name(config: &ContextManagerConfig) -> String {
    let program = match config.shell.program.as_deref() {
        Some(program) if !program.is_empty() => program.to_string(),
        _ => {
            #[cfg(unix)]
            {
                teletypewriter::default_shell_program()
            }
            #[cfg(not(unix))]
            {
                String::from("powershell")
            }
        }
    };
    program_display_name(&program)
}

/// The daemon knows what it runs; the configured shell is only the
/// fallback, since it describes the wrong one on a remote host and a
/// stale one after a config edit.
#[cfg(unix)]
fn ptyd_program_name(reported: Option<&str>, configured: String) -> String {
    reported
        .filter(|p| !p.is_empty())
        .map(program_display_name)
        .unwrap_or(configured)
}

/// The shell a persistent pane is asked to run: the configured program,
/// else $SHELL. `None` leaves the choice to spawn_local's resolver, the
/// same one the shell integration uses, so the two agree.
#[cfg(unix)]
fn ptyd_shell(configured: Option<&str>, env_shell: Option<&str>) -> Option<String> {
    configured
        .filter(|s| !s.is_empty())
        .or_else(|| env_shell.filter(|s| !s.is_empty()))
        .map(str::to_string)
}

/// What the daemon execs: the integration's rewritten command when it
/// has one (PowerShell), else the shell and arguments as configured.
#[cfg(unix)]
fn ptyd_command<'a>(
    integration: &'a crate::shell_integration::SpawnIntegration,
    shell: Option<&'a str>,
    args: &'a [String],
) -> (Option<&'a str>, &'a [String]) {
    match &integration.command {
        Some((program, args)) => (program.as_deref(), args.as_slice()),
        None => (shell, args),
    }
}

#[cfg(all(test, unix))]
mod ptyd_spawn_tests {
    use super::{ptyd_command, ptyd_program_name, ptyd_shell};
    use crate::shell_integration::SpawnIntegration;

    #[test]
    fn daemon_program_wins_over_the_configured_shell() {
        assert_eq!(ptyd_program_name(Some("/bin/bash"), "zsh".into()), "bash");
        assert_eq!(ptyd_program_name(Some(""), "zsh".into()), "zsh");
        assert_eq!(ptyd_program_name(None, "zsh".into()), "zsh");
    }

    #[test]
    fn configured_shell_then_env_then_the_resolver() {
        assert_eq!(
            ptyd_shell(Some("/bin/fish"), Some("/bin/zsh")).as_deref(),
            Some("/bin/fish")
        );
        assert_eq!(
            ptyd_shell(Some(""), Some("/bin/zsh")).as_deref(),
            Some("/bin/zsh")
        );
        assert_eq!(
            ptyd_shell(None, Some("")),
            None,
            "an empty $SHELL is not a program"
        );
        assert_eq!(ptyd_shell(None, None), None);
    }

    #[test]
    fn integration_command_replaces_the_configured_one() {
        let args = vec!["-l".to_string()];
        let rewritten = SpawnIntegration {
            env: Vec::new(),
            command: Some((
                Some("pwsh".to_string()),
                vec!["-NoExit".to_string(), "-EncodedCommand".to_string()],
            )),
        };
        let (program, got) = ptyd_command(&rewritten, Some("/bin/zsh"), &args);
        assert_eq!(program, Some("pwsh"));
        assert_eq!(got, ["-NoExit", "-EncodedCommand"]);

        let plain = SpawnIntegration::default();
        let (program, got) = ptyd_command(&plain, Some("/bin/zsh"), &args);
        assert_eq!(program, Some("/bin/zsh"));
        assert_eq!(got, ["-l"]);
    }
}

fn program_display_name(program: &str) -> String {
    std::path::Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string())
}

pub fn create_dead_context<T: rio_backend::event::EventListener>(
    event_proxy: T,
    window_id: WindowId,
    route_id: usize,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T> {
    let terminal = Crosswords::new(
        dimension,
        CursorShape::Block,
        event_proxy,
        window_id,
        route_id,
        // Dead context never sees new input — no scrollback needed.
        0,
    );
    let terminal: Arc<FairMutex<Crosswords<T>>> = Arc::new(FairMutex::new(terminal));
    let (sender, _receiver) = corcovado::channel::channel();

    Context {
        route_id,
        #[cfg(not(target_os = "windows"))]
        main_fd: Arc::new(-1),
        #[cfg(not(target_os = "windows"))]
        shell_pid: 1,
        #[cfg(not(target_os = "windows"))]
        child_terminator: teletypewriter::ChildTerminator::retired(),
        messenger: Messenger::new(sender),
        renderable_content: RenderableContent::new(Cursor::default()),
        terminal,
        rich_text_id,
        dimension,
        title: ContextTitle::default(),
        backend: PaneBackend::Local,
        title_dirty: false,
        spawned_program: String::new(),
        _io_thread: None,
    }
}

#[cfg(test)]
pub fn create_mock_context<
    T: rio_backend::event::EventListener + Clone + std::marker::Send + 'static,
>(
    event_proxy: T,
    window_id: WindowId,
    rich_text_id: usize,
    dimension: ContextDimension,
) -> Context<T> {
    let config = ContextManagerConfig {
        dead_pty: true,
        ..ContextManagerConfig::default()
    };
    ContextManager::create_context(
        (&Cursor::default(), false),
        event_proxy.clone(),
        window_id,
        rich_text_id,
        dimension,
        &config,
        None,
        false,
    )
    .unwrap()
}

/// Where `current_index` lands after removing the tab at `removed`:
/// the focused tab falls back to its left neighbor, and a background
/// removal shifts the focused index left only when the removed tab
/// sat before it. Pure, so the arithmetic the tmux SIGHUP crash
/// hinged on stays testable without a GPU.
#[inline]
fn current_index_after_tab_removal(current: usize, removed: usize) -> usize {
    if removed <= current {
        current.saturating_sub(1)
    } else {
        current
    }
}

impl<T: EventListener + Clone + std::marker::Send + 'static> ContextManager<T> {
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn create_context(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        rich_text_id: usize,
        dimension: ContextDimension,
        config: &ContextManagerConfig,
        attach: Option<&AttachTarget>,
        force_local: bool,
    ) -> Result<Context<T>, Box<dyn Error>> {
        #[cfg(unix)]
        if !force_local && (attach.is_some() || config.persistence.is_some()) {
            let ring_bytes = config
                .persistence
                .as_ref()
                .map(|p| p.ring_bytes)
                .unwrap_or(rio_ptyd::ring::DEFAULT_RING_BYTES);
            match Self::create_ptyd_context(
                cursor_state,
                event_proxy.clone(),
                window_id,
                rich_text_id,
                dimension,
                config,
                attach,
                ring_bytes,
            ) {
                Ok(ctx) => return Ok(ctx),
                // Explicit attach requests propagate their failure —
                // the session layer owns the fallback (fresh spawn +
                // saved scrollback).
                Err(e) if attach.is_some() => return Err(Box::new(e)),
                // Persistence-on spawn failures degrade gracefully to
                // a plain local pty: the terminal always opens.
                Err(e) => {
                    tracing::warn!(
                        "persistence unavailable, falling back to local pty: {e}"
                    );
                }
            }
        }
        #[cfg(not(unix))]
        let _ = attach;

        let route_id = ROUTE_ID_COUNTER.fetch_add(1, Ordering::SeqCst);

        #[cfg(test)]
        if config.dead_pty {
            return Ok(create_dead_context(
                event_proxy,
                window_id,
                route_id,
                rich_text_id,
                dimension,
            ));
        }

        let cols: u16 = dimension.columns.try_into().unwrap_or(MIN_COLUMNS as u16);
        let rows: u16 = dimension.lines.try_into().unwrap_or(MIN_LINES as u16);
        #[cfg(not(target_os = "windows"))]
        let initial_winsize = crate::renderer::utils::terminal_dimensions(&dimension);

        let mut terminal = Crosswords::new(
            dimension,
            CursorShape::from_char(cursor_state.0.content),
            event_proxy.clone(),
            window_id,
            route_id,
            config.scrollback_history_limit,
        );
        terminal.set_grapheme_clustering(config.grapheme_clustering);
        terminal.blinking_cursor = cursor_state.1;
        // rioterm reports mouse positions in pixels, so mode 1016 is honoured.
        terminal.set_sgr_pixels_mouse_supported(true);
        let terminal: Arc<FairMutex<Crosswords<T>>> = Arc::new(FairMutex::new(terminal));

        let integration = if config.shell_integration {
            crate::shell_integration::prepare(
                config.shell.program.as_deref(),
                &config.shell.args,
            )
        } else {
            crate::shell_integration::SpawnIntegration::default()
        };
        let (shell_program, shell_args) = match &integration.command {
            Some((program, args)) => (program.as_deref(), args.as_slice()),
            None => (
                config.shell.program.as_deref(),
                config.shell.args.as_slice(),
            ),
        };

        let pty;
        #[cfg(not(target_os = "windows"))]
        {
            if config.use_fork {
                tracing::info!("rio -> teletypewriter: create_pty_with_fork");
                pty = match create_pty_with_fork(
                    shell_program,
                    shell_args,
                    &integration.env,
                    cols,
                    rows,
                    initial_winsize.width,
                    initial_winsize.height,
                ) {
                    Ok(created_pty) => created_pty,
                    Err(err) => {
                        tracing::error!("{err:?}");
                        return Err(Box::new(err));
                    }
                }
            } else {
                tracing::info!("rio -> teletypewriter: create_pty_with_spawn");
                pty = match create_pty_with_spawn(
                    shell_program,
                    shell_args.to_vec(),
                    &config.working_dir,
                    (!integration.env.is_empty()).then(|| integration.env.clone()),
                    cols,
                    rows,
                    initial_winsize.width,
                    initial_winsize.height,
                ) {
                    Ok(created_pty) => created_pty,
                    Err(err) => {
                        tracing::error!("{err:?}");
                        return Err(Box::new(err));
                    }
                }
            };
        }

        #[cfg(not(target_os = "windows"))]
        let main_fd = pty.child.id.clone();
        #[cfg(not(target_os = "windows"))]
        let shell_pid = *pty.child.pid.clone() as u32;
        #[cfg(not(target_os = "windows"))]
        let child_terminator = pty.child.terminator();

        #[cfg(target_os = "windows")]
        {
            pty = match create_pty(
                shell_program,
                shell_args.to_vec(),
                &config.working_dir,
                (!integration.env.is_empty()).then(|| integration.env.clone()),
                cols,
                rows,
            ) {
                Ok(created_pty) => created_pty,
                Err(err) => {
                    tracing::error!("{err:?}");
                    return Err(Box::new(err));
                }
            }
        }

        let (messenger, io_thread) = Self::finish_machine(
            pty,
            &terminal,
            event_proxy.clone(),
            window_id,
            route_id,
            config.spawn_performer,
        )?;

        let mut context = Context {
            route_id,
            #[cfg(not(target_os = "windows"))]
            main_fd,
            #[cfg(not(target_os = "windows"))]
            shell_pid,
            #[cfg(not(target_os = "windows"))]
            child_terminator,
            messenger,
            terminal,
            rich_text_id,
            renderable_content: RenderableContent::new(cursor_state.0.clone()),
            dimension,
            title: ContextTitle::default(),
            backend: PaneBackend::Local,
            title_dirty: false,
            spawned_program: spawned_program_name(config),
            _io_thread: io_thread,
        };
        context.title = ContextTitle {
            content: update_title(&config.title.content, &context, None),
        };
        Ok(context)
    }

    /// Shared tail for any pty backend: build the io Machine, hand
    /// back the messenger + (optionally spawned) io thread.
    #[allow(clippy::type_complexity)]
    fn finish_machine<P>(
        pty: P,
        terminal: &Arc<FairMutex<Crosswords<T>>>,
        event_proxy: T,
        window_id: WindowId,
        route_id: usize,
        spawn_performer: bool,
    ) -> Result<(Messenger, Option<JoinHandle<()>>), Box<dyn Error>>
    where
        P: teletypewriter::EventedPty + Send + 'static,
    {
        let machine =
            Machine::new(Arc::clone(terminal), pty, event_proxy, window_id, route_id)?;
        let channel = machine.channel();
        let io_thread = if spawn_performer {
            Some(machine.spawn_detached())
        } else {
            None
        };
        Ok((Messenger::new(channel), io_thread))
    }

    /// Create a pane hosted by a rio-ptyd daemon: either attach to an
    /// existing one (session restore / remote pane) or spawn a fresh
    /// local daemon. On any failure the caller decides the fallback.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn create_ptyd_context(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        rich_text_id: usize,
        dimension: ContextDimension,
        config: &ContextManagerConfig,
        attach: Option<&AttachTarget>,
        ring_bytes: usize,
    ) -> Result<Context<T>, crate::ptyd::AttachError> {
        use std::time::Duration;

        let route_id = ROUTE_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let winsize = crate::renderer::utils::terminal_dimensions(&dimension);

        let (pty, hello, pane_id, socket, host) = match attach {
            Some(AttachTarget::Unix { pane_id, socket }) => {
                let (pty, hello) = crate::ptyd::RemotePty::attach_unix(
                    socket,
                    &winsize,
                    Duration::from_secs(2),
                )?;
                (pty, hello, pane_id.clone(), socket.clone(), None)
            }
            Some(AttachTarget::Ssh { host, pane_id }) => {
                // This handshake runs on the event loop, so its
                // timeout is the worst-case UI freeze when the host
                // dies between listing and attaching. BatchMode never
                // waits on prompts, so a healthy attach is sub-second.
                let (pty, hello) = crate::ptyd::RemotePty::attach_ssh(
                    host,
                    pane_id,
                    &winsize,
                    Duration::from_secs(6),
                )?;
                (
                    pty,
                    hello,
                    pane_id.clone(),
                    std::path::PathBuf::new(),
                    Some(host.clone()),
                )
            }
            None => {
                // Same integration as a local pane, carried to the daemon
                // as env + command. The shell is resolved once here so the
                // env is prepared for the program the daemon execs.
                let shell_program = ptyd_shell(
                    config.shell.program.as_deref(),
                    std::env::var("SHELL").ok().as_deref(),
                );
                let integration = if config.shell_integration {
                    crate::shell_integration::prepare(
                        shell_program.as_deref(),
                        &config.shell.args,
                    )
                } else {
                    crate::shell_integration::SpawnIntegration::default()
                };
                let (program, args) = ptyd_command(
                    &integration,
                    shell_program.as_deref(),
                    &config.shell.args,
                );
                let (pty, hello, pane_id, socket) = crate::ptyd::RemotePty::spawn_local(
                    program,
                    args,
                    &config.working_dir,
                    config.session_name.as_deref(),
                    &winsize,
                    ring_bytes,
                    &integration.env,
                )?;
                (pty, hello, pane_id, socket, None)
            }
        };

        let mut terminal = Crosswords::new(
            dimension,
            CursorShape::from_char(cursor_state.0.content),
            event_proxy.clone(),
            window_id,
            route_id,
            config.scrollback_history_limit,
        );
        terminal.blinking_cursor = cursor_state.1;
        terminal.set_sgr_pixels_mouse_supported(true);
        let terminal: Arc<FairMutex<Crosswords<T>>> = Arc::new(FairMutex::new(terminal));

        let (messenger, io_thread) = Self::finish_machine(
            pty,
            &terminal,
            event_proxy,
            window_id,
            route_id,
            config.spawn_performer,
        )
        .map_err(|e| {
            crate::ptyd::AttachError::Io(std::io::Error::other(e.to_string()))
        })?;

        let mut context = Context {
            route_id,
            main_fd: Arc::new(-1),
            shell_pid: hello.shell_pid,
            messenger,
            terminal,
            rich_text_id,
            renderable_content: RenderableContent::new(cursor_state.0.clone()),
            dimension,
            title: ContextTitle::default(),
            backend: PaneBackend::Ptyd {
                pane_id,
                socket,
                host,
                reported_cwd: hello.cwd.clone(),
                replayed: attach.is_some(),
            },
            title_dirty: false,
            spawned_program: ptyd_program_name(
                hello.program.as_deref(),
                spawned_program_name(config),
            ),
            child_terminator: teletypewriter::ChildTerminator::retired(),
            _io_thread: io_thread,
        };
        context.title = ContextTitle {
            content: update_title(&config.title.content, &context, None),
        };
        Ok(context)
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        cursor_state: (&Cursor, bool),
        event_proxy: T,
        window_id: WindowId,
        route_id: usize,
        rich_text_id: usize,
        ctx_config: ContextManagerConfig,
        size: ContextDimension,
        scaled_margin: Margin,
        sugarloaf_errors: Option<SugarloafErrors>,
    ) -> Result<Self, Box<dyn Error>> {
        let initial_context = match ContextManager::create_context(
            cursor_state,
            event_proxy.clone(),
            window_id,
            rich_text_id,
            size,
            &ctx_config,
            None,
            false,
        ) {
            Ok(context) => context,
            Err(err_message) => {
                tracing::error!("{:?}", err_message);

                event_proxy.send_event(
                    RioEvent::ReportToAssistant(RioError {
                        report: RioErrorType::InitializationError(
                            err_message.to_string(),
                        ),
                        level: RioErrorLevel::Error,
                    }),
                    window_id,
                );

                create_dead_context(
                    event_proxy.clone(),
                    window_id,
                    route_id,
                    0,
                    ContextDimension::default(),
                )
            }
        };

        // Sugarloaf has found errors and context need to notify it for the user
        if let Some(errors) = sugarloaf_errors {
            if !errors.fonts_not_found.is_empty() {
                event_proxy.send_event(
                    RioEvent::ReportToAssistant({
                        RioError {
                            report: RioErrorType::FontsNotFound(errors.fonts_not_found),
                            level: RioErrorLevel::Warning,
                        }
                    }),
                    window_id,
                );
            }
        }

        let mut manager = ContextManager {
            current_index: 0,
            current_route: 0,
            contexts: smallvec![ContextGrid::new(
                initial_context,
                scaled_margin,
                ctx_config.split_color,
                ctx_config.split_active_color,
                ctx_config.panel,
            )],
            capacity: DEFAULT_CONTEXT_CAPACITY,
            event_proxy,
            window_id,
            config: ctx_config,
        };
        // The native titlebar starts as the placeholder; one poke makes
        // it converge on the displayed title even for shells that never
        // emit an OSC title or OSC 7.
        manager.sync_window_title();
        Ok(manager)
    }

    #[cfg(test)]
    pub fn start_with_capacity(
        capacity: usize,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<Self, Box<dyn Error>> {
        let config = ContextManagerConfig {
            #[cfg(test)]
            dead_pty: true,
            ..ContextManagerConfig::default()
        };
        let initial_context = ContextManager::create_context(
            (&Cursor::default(), false),
            event_proxy.clone(),
            window_id,
            0,
            ContextDimension::default(),
            &config,
            None,
            false,
        )?;

        Ok(ContextManager {
            current_index: 0,
            current_route: 0,
            contexts: smallvec![ContextGrid::new(
                initial_context,
                Margin::default(),
                config.split_color,
                config.split_active_color,
                config.panel,
            )],
            capacity,
            event_proxy,
            window_id,
            config,
        })
    }

    #[inline]
    pub fn should_close_context_manager(
        &mut self,
        route_id: usize,
        sugarloaf: &mut Sugarloaf,
    ) -> bool {
        // Called when terminal.exit() fires: a tab-close action (the
        // context is already gone, nothing found below, return false)
        // or a PTY that exited on its own. The exiting PTY can live
        // ANYWHERE: a background tab, or a background split of any
        // tab (`tmux new -X` SIGHUPs a shell the user is not even
        // looking at), so the search covers every panel of every tab
        // rather than assuming the focused one.
        let Some(tab_index) = self.tab_index_for_route(route_id) else {
            return self.contexts.is_empty();
        };

        // A split dies: remove just that panel, keep the tab. When
        // the tab is focused and its focused panel was the one that
        // died, the sibling selection becomes the current route.
        if self.contexts[tab_index].len() > 1 {
            self.contexts[tab_index].remove_by_route(route_id, sugarloaf);
            if tab_index == self.current_index {
                self.sync_current_route();
            }
            return false;
        }

        // A whole tab dies.
        self.contexts[tab_index].remove_from_sugarloaf(sugarloaf);
        self.contexts.remove(tab_index);

        if self.contexts.is_empty() {
            return true;
        }

        // Removing a tab shifts every index after it. Adjusting only
        // when the focused tab itself died leaves `current_index`
        // past the end when a background tab exits first (the tmux
        // SIGHUP crash: len 1, index 1).
        let new_index = current_index_after_tab_removal(self.current_index, tab_index);
        if tab_index == self.current_index {
            self.set_current(new_index);
        } else {
            self.current_index = new_index;
        }

        self.keep_only_active_context_visible(sugarloaf);
        false
    }

    /// Whether a shell exit on `route_id` would close the whole window,
    /// without mutating anything. Mirrors the last-context branch of
    /// `should_close_context_manager`: the window closes only when the
    /// route is the sole pane of the sole context. Used to decide whether
    /// to show the save-on-exit prompt *before* the dead context is
    /// removed, so the prompt still has a live surface to render on.
    #[inline]
    pub fn would_close_window(&self, route_id: usize) -> bool {
        if self.contexts.is_empty() {
            return true;
        }
        if self.current_grid().len() > 1 {
            return false;
        }
        self.contexts.len() == 1
            && self
                .contexts
                .iter()
                .any(|ctx| ctx.current().route_id == route_id)
    }

    #[inline]
    pub fn request_render(&mut self) {
        self.event_proxy
            .send_event(RioEvent::RenderRoute(self.current_route), self.window_id);
    }

    #[inline]
    pub fn blink_cursor(&mut self, scheduled_time: u64) {
        // PrepareRender will force a render for any route that is focused on window
        // PrepareRenderOnRoute only call render function for specific route ids.
        self.event_proxy.send_event(
            RioEvent::BlinkCursor(scheduled_time, self.current_route),
            self.window_id,
        );
    }

    #[inline]
    pub fn schedule_render_on_route(&mut self, millis: u64) {
        self.event_proxy.send_event(
            RioEvent::PrepareRenderOnRoute(millis, self.current_route),
            self.window_id,
        );
    }

    #[inline]
    pub fn report_error_fonts_not_found(&mut self, fonts_not_found: Vec<SugarloafFont>) {
        if !fonts_not_found.is_empty() {
            self.event_proxy.send_event(
                RioEvent::ReportToAssistant({
                    RioError {
                        report: RioErrorType::FontsNotFound(fonts_not_found),
                        level: RioErrorLevel::Warning,
                    }
                }),
                self.window_id,
            );
        }
    }

    #[inline]
    pub fn create_new_window(&self) {
        self.event_proxy
            .send_event(RioEvent::CreateWindow, self.window_id);
    }

    #[inline]
    pub fn toggle_quake(&self) {
        self.event_proxy
            .send_event(RioEvent::ToggleQuake, self.window_id);
    }

    #[inline]
    pub fn close_unfocused_tabs(&mut self, sugarloaf: &mut Sugarloaf) {
        let current_route_id = self.current().route_id;
        self.contexts.retain(|ctx| {
            let keep = ctx.current().route_id == current_route_id;
            if !keep {
                ctx.remove_from_sugarloaf(sugarloaf);
            }
            keep
        });
        self.current_route = self.contexts[0].current().route_id;
        self.set_current(0);
    }

    #[inline]
    pub fn set_last_typing(&mut self) {
        self.current_mut().renderable_content.last_typing = Some(Instant::now());
    }

    #[inline]
    pub fn select_next_split(&mut self) {
        self.contexts[self.current_index].select_next_split();
        self.sync_current_route();
    }

    #[inline]
    pub fn select_prev_split(&mut self) {
        self.contexts[self.current_index].select_prev_split();
        self.sync_current_route();
    }

    #[inline]
    pub fn switch_to_next_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_next_split_no_loop() {
            self.sync_current_route();
            return;
        }
        self.switch_to_next();
        // Make sure first split is selected - get the root key
        let current_tab = &mut self.contexts[self.current_index];
        if let Some(root) = current_tab.root {
            current_tab.current = root;
        }
        self.sync_current_route();
    }

    #[inline]
    pub fn switch_to_prev_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_prev_split_no_loop() {
            self.sync_current_route();
            return;
        }
        self.switch_to_prev();
        // Make sure last split is selected - get the last key in order
        let current_tab = &mut self.contexts[self.current_index];
        let ordered_keys = current_tab.get_ordered_keys();
        if let Some(&last_key) = ordered_keys.last() {
            current_tab.current = last_key;
        }
        self.sync_current_route();
    }

    #[inline]
    pub fn move_divider_up(&mut self, amount: f32, sugarloaf: &mut Sugarloaf) -> bool {
        self.contexts[self.current_index].move_divider_up(amount, sugarloaf)
    }

    #[inline]
    pub fn move_divider_down(&mut self, amount: f32, sugarloaf: &mut Sugarloaf) -> bool {
        self.contexts[self.current_index].move_divider_down(amount, sugarloaf)
    }

    #[inline]
    pub fn move_divider_left(&mut self, amount: f32, sugarloaf: &mut Sugarloaf) -> bool {
        self.contexts[self.current_index].move_divider_left(amount, sugarloaf)
    }

    #[inline]
    pub fn move_divider_right(&mut self, amount: f32, sugarloaf: &mut Sugarloaf) -> bool {
        self.contexts[self.current_index].move_divider_right(amount, sugarloaf)
    }

    #[inline]
    pub fn select_tab(&mut self, tab_index: usize) {
        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabByIndex(tab_index), self.window_id);
            return;
        }

        self.set_current(tab_index);
    }

    #[inline]
    pub fn toggle_full_screen(&mut self) {
        self.event_proxy
            .send_event(RioEvent::ToggleFullScreen, self.window_id);
    }

    #[inline]
    pub fn toggle_maximize_window(&mut self) {
        self.event_proxy
            .send_event(RioEvent::ToggleMaximized, self.window_id);
    }

    #[inline]
    pub fn toggle_appearance_theme(&mut self) {
        self.event_proxy
            .send_event(RioEvent::ToggleAppearanceTheme, self.window_id);
    }

    #[inline]
    pub fn minimize(&mut self) {
        self.event_proxy
            .send_event(RioEvent::Minimize(true), self.window_id);
    }

    #[inline]
    pub fn hide(&mut self) {
        self.event_proxy.send_event(RioEvent::Hide, self.window_id);
    }

    #[inline]
    pub fn quit(&mut self) {
        self.event_proxy.send_event(RioEvent::Quit, self.window_id);
    }

    /// Explicit user "save now" (keybinding): the saved file mirrors
    /// the currently-open windows.
    #[inline]
    pub fn request_save_session(&mut self) {
        self.event_proxy
            .send_event(RioEvent::SaveSession { explicit: true }, self.window_id);
    }

    /// Persist the session after a structural change (tab/pane
    /// open/close/split) when in automatic (`restore = "always"`) mode,
    /// so a crash — which runs no clean-exit save — still leaves a
    /// current session. No-op in `prompt` mode: that mode never saves
    /// without the user's yes. Structural changes are rare user
    /// actions, so the extra write is not on any hot path. The save
    /// mirrors the open windows: a structural change proves the run
    /// continued past any earlier window close, so a window the user
    /// killed (exit, close-tab) drops out of the session instead of
    /// resurrecting on resume.
    #[inline]
    pub fn autosave_on_change(&mut self) {
        if self.config.autosave {
            self.event_proxy
                .send_event(RioEvent::SaveSession { explicit: false }, self.window_id);
        }
    }

    #[inline]
    pub fn notify_close_armed(&self) {
        self.event_proxy
            .send_event(RioEvent::CloseButtonArmed, self.window_id);
    }

    #[inline]
    pub fn request_save_session_as(&mut self, name: String) {
        self.event_proxy
            .send_event(RioEvent::SaveSessionAs(name), self.window_id);
    }

    #[inline]
    pub fn request_restore_session_named(&mut self, name: String) {
        self.event_proxy
            .send_event(RioEvent::RestoreSessionByName(name), self.window_id);
    }

    /// Palette "Attach Remote Pane": list the destination's panes on
    /// a worker thread (even BatchMode ssh takes seconds) and deliver
    /// the result through the event loop.
    #[cfg(unix)]
    pub fn request_remote_pane_list(&self, host: String) {
        let event_proxy = self.event_proxy.clone();
        let window_id = self.window_id;
        std::thread::spawn(move || {
            let (panes, error) = match crate::ptyd::list_remote_panes(&host) {
                Ok(panes) => (panes, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            event_proxy.send_event(
                RioEvent::RemotePanesListed { host, panes, error },
                window_id,
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[inline]
    pub fn hide_other_apps(&mut self) {
        self.event_proxy
            .send_event(RioEvent::HideOtherApplications, self.window_id);
    }

    #[inline]
    pub fn select_last_tab(&mut self) {
        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabLast, self.window_id);
            return;
        }

        self.set_current(self.contexts.len() - 1);
    }

    #[inline]
    pub fn switch_to_settings(&mut self) {
        self.event_proxy
            .send_event(RioEvent::CreateConfigEditor, self.window_id);
    }

    #[inline]
    pub fn select_route_from_current_grid(&mut self) {
        self.sync_current_route();
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    #[cfg(test)]
    pub fn title(&self, index: usize) -> Option<&ContextTitle> {
        self.contexts.get(index).map(|grid| &grid.current().title)
    }

    #[inline]
    pub fn custom_title(&self, index: usize) -> Option<&str> {
        self.contexts
            .get(index)
            .and_then(|grid| grid.custom_title.as_deref())
    }

    #[inline]
    /// A rename changes what `displayed_title_for_tab` returns, so the
    /// native titlebar gets the same convergence poke every other
    /// displayed-title mutation sends.
    pub fn set_custom_title(&mut self, index: usize, title: Option<String>) {
        if let Some(grid) = self.contexts.get_mut(index) {
            grid.custom_title = title;
        }
        self.sync_window_title();
    }

    /// Drop every tab's bell mark, used when the indicator is disabled
    /// by a live config reload: the ring-time gate stops new marks but
    /// cannot retract ones already set.
    pub fn clear_all_bells(&mut self) {
        for grid in self.contexts.iter_mut() {
            grid.bell = false;
        }
    }

    #[inline]
    pub fn custom_color(&self, index: usize) -> Option<[f32; 4]> {
        self.contexts.get(index).and_then(|grid| grid.custom_color)
    }

    #[inline]
    pub fn set_custom_color(&mut self, index: usize, color: Option<[f32; 4]>) {
        if let Some(grid) = self.contexts.get_mut(index) {
            grid.custom_color = color;
        }
    }

    /// Index of the tab containing `route_id`'s pane. Per-route state
    /// can live ANYWHERE (a background split of a background tab
    /// included), so the search covers every panel of every tab.
    #[inline]
    pub fn tab_index_for_route(&mut self, route_id: usize) -> Option<usize> {
        self.contexts
            .iter_mut()
            .position(|grid| grid.get_by_route_id(route_id).is_some())
    }

    /// A pane rang the bell. Flags its tab so the strip can surface it.
    /// The current tab is skipped only while the window has focus: the
    /// user is already looking at it then, but a ring in the visible
    /// tab of an UNFOCUSED window would otherwise leave no trace at
    /// all. Edge-triggered: a BEL flood marks once and repaints once.
    #[inline]
    pub fn ring_bell(&mut self, route_id: usize, window_focused: bool) -> bool {
        let Some(tab_index) = self.tab_index_for_route(route_id) else {
            return false;
        };

        if window_focused && tab_index == self.current_index {
            return false;
        }

        !std::mem::replace(&mut self.contexts[tab_index].bell, true)
    }

    #[inline]
    pub fn bell(&self, index: usize) -> bool {
        self.contexts.get(index).is_some_and(|grid| grid.bell)
    }

    /// Clears the focused tab's bell flag. Called every FOCUSED frame so
    /// the mark drops the moment the tab is shown to the user, whatever
    /// brought it to the front; an unfocused window still renders on PTY
    /// damage, and clearing there would wipe a mark nobody has seen yet.
    #[inline]
    pub fn clear_current_bell(&mut self) -> bool {
        let grid = &mut self.contexts[self.current_index];
        std::mem::replace(&mut grid.bell, false)
    }

    #[inline]
    pub fn resize_all_grids(
        &mut self,
        width: f32,
        height: f32,
        sugarloaf: &mut Sugarloaf,
    ) {
        for context_grid in self.contexts.iter_mut() {
            context_grid.resize(width, height, sugarloaf);
        }
    }

    /// Re-render one pane's title. Returns whether the displayed text
    /// changed (the empty-content fallback is the pane's static
    /// spawned program, so displayed text changes exactly when the
    /// content does).
    fn refresh_item_title(
        template: &str,
        context: &mut Context<T>,
        prefetched_title: Option<&str>,
    ) -> bool {
        let content = update_title(template, context, prefetched_title);
        if content == context.title.content {
            return false;
        }
        context.title = ContextTitle { content };
        true
    }

    /// A pane's title data changed: an OSC 0/2 title (carried in
    /// `raw_title`, so the common `{{ title }}` render never locks the
    /// terminal) or an OSC 7 working directory (`raw_title` None: the
    /// render re-reads the stored directory). A DISPLAYED pane (its
    /// tab's current) re-renders immediately; a hidden pane is only
    /// marked dirty (one flag write, no locks, no render) and renders
    /// when it surfaces, so a background split streaming titles costs
    /// nothing visible. One route scan serves every decision. Returns
    /// whether the strip must repaint.
    pub fn on_title_change(&mut self, route_id: usize, raw_title: Option<&str>) -> bool {
        let Some(tab_index) = self.tab_index_for_route(route_id) else {
            return false;
        };
        if self.contexts[tab_index].current().route_id != route_id {
            if let Some(item) = self.contexts[tab_index].get_by_route_id(route_id) {
                item.context_mut().title_dirty = true;
            }
            return false;
        }
        let template = self.config.title.content.clone();
        let context = self.contexts[tab_index].current_mut();
        context.title_dirty = false;
        Self::refresh_item_title(&template, context, raw_title)
    }

    /// Mark every pane's title stale. For changes that affect panes no
    /// walk re-renders (a resize changing `{{columns}}`, a config
    /// reload changing the template), hidden splits and unwalked tabs
    /// re-render lazily when they surface via `sync_current_route`.
    pub fn mark_all_titles_dirty(&mut self) {
        for grid in self.contexts.iter_mut() {
            for item in grid.contexts_mut().values_mut() {
                item.context_mut().title_dirty = true;
            }
        }
    }

    /// Re-render tab titles from local state: a config reload can
    /// change the template, and a resize changes `{{columns}}`/
    /// `{{lines}}`. OSC title and OSC 7 changes arrive via
    /// `on_title_change` instead; nothing calls this on a timer. The
    /// chrome repaint rides the returned flag, and one unconditional
    /// titlebar poke per run makes the native title CONVERGE on the
    /// displayed text (the poke is payload-less and deduped at the
    /// sink, so a run that changed nothing costs one no-op event).
    /// `only_current` restricts the walk to the displayed tab: with
    /// the tab strip absent (navigation disabled) background tabs'
    /// titles render nowhere, so refreshing them buys nothing.
    pub fn update_titles(&mut self, only_current: bool) -> bool {
        let template = self.config.title.content.clone();
        let mut repaint = false;
        let range = if only_current {
            self.current_index..self.current_index + 1
        } else {
            0..self.contexts.len()
        };
        for index in range {
            let context = self.contexts[index].current_mut();
            context.title_dirty = false;
            repaint |= Self::refresh_item_title(&template, context, None);
        }
        self.sync_window_title();
        repaint
    }

    /// The title the strip displays for `index`'s tab: the user rename,
    /// else the rendered content, else the foreground program, else
    /// "~". The native titlebar reads the same chain, so the two can
    /// never disagree.
    pub fn displayed_title_for_tab(&self, index: usize) -> String {
        if let Some(custom) = self.custom_title(index) {
            return custom.to_string();
        }
        if let Some(grid) = self.contexts.get(index) {
            let context = grid.current();
            if !context.title.content.is_empty() {
                return context.title.content.clone();
            }
            if !context.spawned_program.is_empty() {
                return context.spawned_program.clone();
            }
        }
        String::from("~")
    }

    #[inline]
    pub fn displayed_title_for_current_tab(&self) -> String {
        self.displayed_title_for_tab(self.current_index)
    }

    /// Ask the event loop to refresh the native titlebar. Payload-less
    /// on purpose: the handler re-reads the displayed title at handling
    /// time, so a queued poke can never overwrite a newer title with a
    /// stale snapshot, and redundant pokes dedupe at the sink.
    fn sync_window_title(&mut self) {
        self.event_proxy
            .send_event(RioEvent::SyncWindowTitle, self.window_id);
    }

    /// Point `current_route` at the pane the user now sees and poke the
    /// titlebar. Every displayed-pane change (tab switch, split
    /// selection, split death, new splits) funnels here so the titlebar
    /// can never be left showing a pane that is not on screen.
    fn sync_current_route(&mut self) {
        self.current_route = self.current().route_id;
        if self.current().title_dirty {
            let template = self.config.title.content.clone();
            let context = self.contexts[self.current_index].current_mut();
            context.title_dirty = false;
            Self::refresh_item_title(&template, context, None);
        }
        self.sync_window_title();
    }

    #[inline]
    pub fn get_by_route_id(
        &mut self,
        route_id: usize,
    ) -> Option<&mut ContextGridItem<T>> {
        // Search every tab, current first: per-route events (damage marks,
        // titles, color/size requests) must reach panes in background tabs,
        // otherwise their state is silently dropped until the pane's own
        // PTY speaks again.
        let current = self.current_index;
        if self.contexts[current].get_by_route_id(route_id).is_some() {
            return self.contexts[current].get_by_route_id(route_id);
        }
        self.contexts
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| *i != current)
            .find_map(|(_, grid)| grid.get_by_route_id(route_id))
    }

    #[inline]
    pub fn contexts_mut(
        &mut self,
    ) -> &mut SmallVec<[ContextGrid<T>; DEFAULT_CONTEXT_CAPACITY]> {
        &mut self.contexts
    }

    #[inline]
    pub fn current_grid_len(&self) -> usize {
        self.contexts[self.current_index].len()
    }

    #[inline]
    pub fn remove_current_grid(&mut self, sugarloaf: &mut Sugarloaf) {
        self.contexts[self.current_index].remove_current(sugarloaf);
        self.sync_current_route();
    }

    #[inline]
    pub fn current_grid_mut(&mut self) -> &mut ContextGrid<T> {
        &mut self.contexts[self.current_index]
    }

    #[inline]
    pub fn current_grid(&self) -> &ContextGrid<T> {
        &self.contexts[self.current_index]
    }

    /// All tabs, for session capture.
    #[inline]
    pub fn grids(&self) -> &[ContextGrid<T>] {
        &self.contexts
    }

    /// Select a pane of the current grid by its live node id. Used by
    /// session restore to reselect the saved active pane; selecting by
    /// identity instead of ordinal keeps it correct for nested splits,
    /// where visual (y, x) order and the saved tree's depth-first
    /// order disagree.
    pub fn select_pane_node(&mut self, node: taffy::NodeId) {
        let grid = &mut self.contexts[self.current_index];
        if grid.set_current(node) {
            self.current_route = grid.current().route_id;
        }
    }

    /// Attach-first creation shared by the restore paths: try the
    /// saved daemon, fall back to a fresh spawn at the saved cwd.
    /// The fallback forces the non-fork pty path (fork has no
    /// working-directory support) and stays plain-local: only a Local
    /// backend runs inject_scrollback and honors the saved cwd.
    fn create_restored_context(
        &self,
        rich_text_id: usize,
        dimension: ContextDimension,
        spawn: &PaneSpawn,
    ) -> Result<Context<T>, Box<dyn Error>> {
        let current = self.current();
        let cursor = current.cursor_from_ref();
        let blinking = current.renderable_content.has_blinking_enabled;

        if let Some(target) = &spawn.attach {
            match ContextManager::create_context(
                (&cursor, blinking),
                self.event_proxy.clone(),
                self.window_id,
                rich_text_id,
                dimension,
                &self.config,
                Some(target),
                false,
            ) {
                Ok(ctx) => return Ok(ctx),
                Err(e) => {
                    tracing::warn!("session reattach failed, spawning fresh: {e}");
                }
            }
        }

        let mut cloned_config = self.config.clone();
        if let Some(cwd) = &spawn.cwd {
            // A saved cwd can be stale (deleted since the save) or
            // foreign (a remote pane's path when its host is
            // unreachable); spawning there fails and would lose the
            // whole tab. Fall back to the default working dir.
            if std::path::Path::new(cwd).is_dir() {
                cloned_config.working_dir = spawn.cwd.clone();
                #[cfg(not(target_os = "windows"))]
                {
                    cloned_config.use_fork = false;
                }
            } else {
                tracing::warn!(
                    "session restore: saved cwd {cwd} unavailable, using default"
                );
            }
        }
        // force_local stays false: a dead saved daemon must be
        // replaced by a *fresh* daemon (persistence on), not a plain
        // pty. Otherwise the pane silently drops to PaneBackend::Local
        // and the next save records no v2 binding — persistence would
        // be lost for good after the daemon dies once. A fresh daemon
        // replays nothing, so inject_scrollback still repaints the
        // saved screen (gated on `replayed`, not on Local-vs-Ptyd).
        // Only a genuine daemon-spawn failure degrades to local.
        ContextManager::create_context(
            (&cursor, blinking),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            dimension,
            &cloned_config,
            None,
            false,
        )
    }

    /// Split the current pane for session restore. Returns whether the
    /// new pane was created; on `false` the grid is unchanged and the
    /// caller must not treat the current pane as the new leaf.
    pub fn split_with_dir(
        &mut self,
        rich_text_id: usize,
        split_down: bool,
        sugarloaf: &mut Sugarloaf,
        spawn: PaneSpawn,
    ) -> bool {
        match self.create_restored_context(rich_text_id, self.current().dimension, &spawn)
        {
            Ok(new_context) => {
                let new_route_id = new_context.route_id;
                if split_down {
                    self.contexts[self.current_index].split_down(new_context, sugarloaf);
                } else {
                    self.contexts[self.current_index].split_right(new_context, sugarloaf);
                }
                self.current_route = new_route_id;
                true
            }
            Err(..) => {
                tracing::error!("session restore: not able to create a split context");
                false
            }
        }
    }

    /// Add a tab attached to an explicit target (palette remote
    /// attach). No fresh-spawn fallback: the user asked for this
    /// exact pane, so failure must surface instead of materializing
    /// a look-alike local shell.
    #[cfg(unix)]
    pub fn add_context_attach(
        &mut self,
        rich_text_id: usize,
        attach: &AttachTarget,
    ) -> Result<(), String> {
        if self.contexts.len() >= self.capacity {
            return Err("tab limit reached".into());
        }
        let last_index = self.contexts.len();
        let mut dimension = self.current().dimension;
        if self.current_grid().len() > 1 {
            dimension = self.current_grid().grid_dimension();
        }
        let current = self.current();
        let cursor = current.cursor_from_ref();
        let blinking = current.renderable_content.has_blinking_enabled;
        match ContextManager::create_context(
            (&cursor, blinking),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            dimension,
            &self.config,
            Some(attach),
            false,
        ) {
            Ok(new_context) => {
                let previous_scaled_margin =
                    self.contexts[self.current_index].scaled_margin;
                self.contexts.push(ContextGrid::new(
                    new_context,
                    previous_scaled_margin,
                    self.config.split_color,
                    self.config.split_active_color,
                    self.config.panel,
                ));
                self.current_index = last_index;
                self.current_route = self.current().route_id;
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Add a tab for session restore: attach to the saved daemon when
    /// possible, else spawn at the saved cwd. Returns whether the tab
    /// was created; on `false` the previous tab is still current.
    pub fn add_context_with_dir(
        &mut self,
        rich_text_id: usize,
        spawn: PaneSpawn,
    ) -> bool {
        if self.contexts.len() >= self.capacity {
            return false;
        }
        let last_index = self.contexts.len();
        let mut dimension = self.current().dimension;
        if self.current_grid().len() > 1 {
            dimension = self.current_grid().grid_dimension();
        }

        match self.create_restored_context(rich_text_id, dimension, &spawn) {
            Ok(new_context) => {
                let previous_scaled_margin =
                    self.contexts[self.current_index].scaled_margin;
                self.contexts.push(ContextGrid::new(
                    new_context,
                    previous_scaled_margin,
                    self.config.split_color,
                    self.config.split_active_color,
                    self.config.panel,
                ));
                self.current_index = last_index;
                self.current_route = self.current().route_id;
                true
            }
            Err(..) => {
                tracing::error!("session restore: not able to create a tab context");
                false
            }
        }
    }

    #[inline]
    pub fn get_panel_borders(&self) -> Vec<Rect> {
        self.contexts[self.current_index].get_panel_borders()
    }

    #[inline]
    pub fn get_current_grid_scaled_margin(&self) -> rio_backend::config::layout::Margin {
        self.contexts[self.current_index].get_scaled_margin()
    }

    #[cfg(test)]
    pub fn increase_capacity(&mut self, inc_val: usize) {
        self.capacity += inc_val;
    }

    #[inline]
    pub fn set_current(&mut self, context_id: usize) {
        if context_id < self.contexts.len() {
            self.current_index = context_id;
            self.sync_current_route();
        }
    }

    #[inline]
    pub fn close_current_context(&mut self, sugarloaf: &mut Sugarloaf) {
        if self.contexts.len() == 1 {
            // Closing the last tab closes THIS window, not the whole
            // app: CloseWindow removes just this route and only exits
            // the loop when it was the last window. Sending Quit here
            // (the old non-macOS behavior) ran process::exit(0),
            // tearing down every other window's shells with no prompt
            // or session save.
            self.event_proxy
                .send_event(RioEvent::CloseWindow, self.window_id);
            return;
        }

        let index_to_remove = self.current_index;
        let mut should_set_current = false;
        if index_to_remove > 1 {
            self.set_current(self.current_index - 1);
        } else {
            should_set_current = true;
        }

        // Remove all rich text from the grid before removing the context
        self.contexts[index_to_remove].remove_from_sugarloaf(sugarloaf);
        self.contexts.remove(index_to_remove);

        if should_set_current {
            self.set_current(0);
        }

        self.keep_only_active_context_visible(sugarloaf);
    }

    #[inline]
    pub fn current_index(&self) -> usize {
        self.current_index
    }

    #[inline]
    pub fn event_proxy(&self) -> &T {
        &self.event_proxy
    }

    #[inline]
    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    #[inline]
    pub fn current_route(&self) -> usize {
        self.current_route
    }

    #[inline]
    pub fn current(&self) -> &Context<T> {
        self.contexts[self.current_index].current()
    }

    #[inline]
    pub fn current_mut(&mut self) -> &mut Context<T> {
        self.contexts[self.current_index].current_mut()
    }

    #[inline]
    pub fn switch_to_next(&mut self) {
        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabNext, self.window_id);
            return;
        }

        if self.contexts.len() - 1 == self.current_index {
            self.current_index = 0;
        } else {
            self.current_index += 1;
        }

        self.sync_current_route();
    }

    #[inline]
    pub fn switch_to_prev(&mut self) {
        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::SelectNativeTabPrev, self.window_id);
            return;
        }

        if self.current_index == 0 {
            self.current_index = self.contexts.len() - 1;
        } else {
            self.current_index -= 1;
        }

        self.sync_current_route();
    }

    #[inline]
    pub fn move_current_to_prev(&mut self) {
        let len = self.contexts.len();
        if len <= 1 {
            return;
        }

        let current = self.current_index;
        let target_index = if current == 0 { len - 1 } else { current - 1 };
        self.contexts.swap(current, target_index);
        self.select_tab(target_index);
    }

    #[inline]
    pub fn move_current_to_next(&mut self) {
        let len = self.contexts.len();
        if len <= 1 {
            return;
        }

        let current = self.current_index;
        let target_index = if current == len - 1 { 0 } else { current + 1 };
        self.contexts.swap(current, target_index);
        self.select_tab(target_index);
    }

    #[inline]
    pub fn move_current_tab_to(&mut self, target: usize) {
        if self.config.is_native {
            return;
        }

        let current = self.current_index;
        if target == current || target >= self.contexts.len() {
            return;
        }

        let grid = self.contexts.remove(current);
        self.contexts.insert(target, grid);
        self.set_current(target);
    }

    pub fn split(
        &mut self,
        rich_text_id: usize,
        split_down: bool,
        sugarloaf: &mut Sugarloaf,
    ) {
        let mut working_dir = self.config.working_dir.clone();
        if self.config.cwd {
            #[cfg(not(target_os = "windows"))]
            {
                if let Some(path) = self.current().foreground_path() {
                    working_dir = Some(path.to_string_lossy().to_string());
                }
            }

            #[cfg(target_os = "windows")]
            {
                // if let Ok(path) = teletypewriter::foreground_process_path() {
                //     working_dir =
                //         Some(path.to_string_lossy().to_string());
                // }
                working_dir = None;
            }
        }

        let mut cloned_config = self.config.clone();
        if working_dir.is_some() {
            cloned_config.working_dir = working_dir;
        }

        let current = self.current();
        let cursor = current.cursor_from_ref();

        match ContextManager::create_context(
            (&cursor, current.renderable_content.has_blinking_enabled),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            self.current().dimension,
            &cloned_config,
            None,
            false,
        ) {
            Ok(new_context) => {
                let new_route_id = new_context.route_id;
                if split_down {
                    self.contexts[self.current_index].split_down(new_context, sugarloaf);
                } else {
                    self.contexts[self.current_index].split_right(new_context, sugarloaf);
                }

                self.current_route = new_route_id;
                self.sync_window_title();
            }
            Err(..) => {
                tracing::error!("not able to create a new context");
            }
        }
    }

    pub fn split_from_config(
        &mut self,
        rich_text_id: usize,
        split_down: bool,
        config: rio_backend::config::Config,
        sugarloaf: &mut Sugarloaf,
    ) {
        let (shell, working_dir) = process_open_url(
            config.shell.to_owned(),
            config.working_dir.to_owned(),
            config.editor.to_owned(),
            None,
        );

        let context_manager_config = ContextManagerConfig {
            #[cfg(test)]
            dead_pty: false,
            cwd: config.navigation.current_working_directory,
            shell,
            working_dir,
            spawn_performer: true,
            persistence: if cfg!(unix) && config.session.uses_daemons() {
                Some(PersistenceOptions {
                    ring_bytes: config.session.ring_bytes,
                })
            } else {
                None
            },
            autosave: config.session.restore
                == rio_backend::config::session::SessionRestore::Always,
            session_name: None,
            #[cfg(not(target_os = "windows"))]
            use_fork: config.use_fork,
            shell_integration: config.shell_integration,
            is_native: config.navigation.is_native(),
            // When navigation is collapsed and does not contain any color rule
            // does not make sense fetch for foreground process names
            split_color: config.colors.split,
            split_active_color: config.colors.split_active,
            panel: config.panel,
            title: config.title,
            keyboard: config.keyboard,
            scrollback_history_limit: config.scrollback_history_limit,
            grapheme_clustering: config.grapheme_clustering,
        };

        let current = self.current();
        let cursor = current.cursor_from_ref();

        match ContextManager::create_context(
            (&cursor, current.renderable_content.has_blinking_enabled),
            self.event_proxy.clone(),
            self.window_id,
            rich_text_id,
            self.current().dimension,
            &context_manager_config,
            None,
            false,
        ) {
            Ok(new_context) => {
                let new_route_id = new_context.route_id;
                if split_down {
                    self.contexts[self.current_index].split_down(new_context, sugarloaf);
                } else {
                    self.contexts[self.current_index].split_right(new_context, sugarloaf);
                }

                self.current_route = new_route_id;
                self.sync_window_title();
            }
            Err(..) => {
                tracing::error!("not able to create a new context");
            }
        }
    }

    #[inline]
    pub fn add_context(&mut self, redirect: bool, rich_text_id: usize) {
        let mut working_dir = self.config.working_dir.clone();
        if self.config.cwd {
            #[cfg(not(target_os = "windows"))]
            {
                if let Some(path) = self.current().foreground_path() {
                    working_dir = Some(path.to_string_lossy().to_string());
                }
            }

            #[cfg(target_os = "windows")]
            {
                // if let Ok(path) = teletypewriter::foreground_process_path() {
                //     working_dir =
                //         Some(path.to_string_lossy().to_string());
                // }
                working_dir = None;
            }
        }

        if self.config.is_native {
            self.event_proxy
                .send_event(RioEvent::CreateNativeTab(working_dir), self.window_id);
            return;
        }

        let size = self.contexts.len();
        if size < self.capacity {
            let last_index = self.contexts.len();

            let mut cloned_config = self.config.clone();
            if working_dir.is_some() {
                cloned_config.working_dir = working_dir;
            }

            let current = self.current();
            let cursor = current.cursor_from_ref();
            let mut dimension = current.dimension;

            // If current has splits then shouldn't use that dimension
            if self.current_grid().len() > 1 {
                dimension = self.current_grid().grid_dimension();
            }

            match ContextManager::create_context(
                (&cursor, current.renderable_content.has_blinking_enabled),
                self.event_proxy.clone(),
                self.window_id,
                rich_text_id,
                dimension,
                &cloned_config,
                None,
                false,
            ) {
                Ok(new_context) => {
                    let previous_scaled_margin =
                        self.contexts[self.current_index].scaled_margin;
                    self.contexts.push(ContextGrid::new(
                        new_context,
                        previous_scaled_margin,
                        self.config.split_color,
                        self.config.split_active_color,
                        self.config.panel,
                    ));
                    if redirect {
                        self.current_index = last_index;
                        self.sync_current_route();
                    }
                }
                Err(..) => {
                    tracing::error!("not able to create a new context");
                }
            }
        }
    }

    /// Hide all rich text components except for the current tab
    #[inline]
    pub fn keep_only_active_context_visible(&self, sugarloaf: &mut Sugarloaf) {
        for (idx, context) in self.contexts.iter().enumerate() {
            // Skip the current tab
            if idx == self.current_index {
                context.set_all_rich_text_visibility(sugarloaf, true);
                continue;
            }

            context.set_all_rich_text_visibility(sugarloaf, false);
        }
    }

    /// Switch visibility between two contexts (hide old, show new)
    #[inline]
    pub fn switch_context_visibility(
        &self,
        sugarloaf: &mut Sugarloaf,
        old_index: usize,
        new_index: usize,
    ) {
        if let Some(old_context) = self.contexts.get(old_index) {
            old_context.set_all_rich_text_visibility(sugarloaf, false);
        }
        if let Some(new_context) = self.contexts.get(new_index) {
            new_context.set_all_rich_text_visibility(sugarloaf, true);
        }
    }
}

pub fn process_open_url(
    mut shell: Shell,
    mut working_dir: Option<String>,
    editor: Shell,
    open_url: Option<&str>,
) -> (Shell, Option<String>) {
    if open_url.is_none() {
        return (shell, working_dir);
    }

    if let Ok(url) = url::Url::parse(open_url.unwrap_or_default()) {
        if let Ok(path_buf) = url.to_file_path() {
            if path_buf.exists() {
                if path_buf.is_file() {
                    let mut args = editor.args;
                    args.push(path_buf.display().to_string());
                    shell = Shell {
                        program: editor.program,
                        args,
                    }
                } else if path_buf.is_dir() {
                    working_dir = Some(path_buf.display().to_string());
                }
            }
        }
    }

    (shell, working_dir)
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::event::VoidListener;

    #[test]
    fn test_capacity() {
        let window_id: WindowId = WindowId::from(0);

        let context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 5);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        context_manager.increase_capacity(3);
        assert_eq!(context_manager.capacity, 8);
    }

    #[test]
    fn test_add_context() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 0);

        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 0);

        let should_redirect = true;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.capacity, 5);
        assert_eq!(context_manager.current_index, 2);
    }

    #[test]
    fn test_add_context_start_with_capacity_limit() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(3, VoidListener {}, window_id).unwrap();
        assert_eq!(context_manager.capacity, 3);
        assert_eq!(context_manager.current_index, 0);
        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 2);
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 3);

        for _ in 0..20 {
            context_manager.add_context(should_redirect, 0);
        }

        assert_eq!(context_manager.len(), 3);
        assert_eq!(context_manager.capacity, 3);
    }

    /// The tmux `new -ADXs` crash (#1848): SIGHUP kills a BACKGROUND
    /// tab's shell, the tab at a lower index is removed, and the
    /// focused index must shift left with it or the very next
    /// `contexts[current_index]` is out of bounds (len 1, index 1).
    #[test]
    fn current_index_adjusts_after_tab_removal() {
        // Background tab before the focused one dies: shift left.
        assert_eq!(current_index_after_tab_removal(1, 0), 0);
        assert_eq!(current_index_after_tab_removal(3, 1), 2);
        // Background tab after the focused one: nothing shifts.
        assert_eq!(current_index_after_tab_removal(0, 1), 0);
        assert_eq!(current_index_after_tab_removal(2, 3), 2);
        // The focused tab itself: fall back to the left neighbor.
        assert_eq!(current_index_after_tab_removal(0, 0), 0);
        assert_eq!(current_index_after_tab_removal(2, 2), 1);
    }

    #[test]
    fn test_set_current() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(8, VoidListener {}, window_id).unwrap();
        let should_redirect = true;

        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.current_index, 1);
        context_manager.set_current(0);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.len(), 2);
        assert_eq!(context_manager.capacity, 8);

        let should_redirect = false;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.set_current(3);
        assert_eq!(context_manager.current_index, 3);

        context_manager.set_current(8);
        assert_eq!(context_manager.current_index, 3);
    }

    #[test]
    fn bell_marks_background_tabs_only_and_clears_on_focus() {
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, WindowId::from(0))
                .unwrap();
        cm.add_context(true, 0);
        cm.add_context(true, 0);
        cm.set_current(0);

        let background_route = cm.contexts[2].current().route_id;
        let focused_route = cm.contexts[0].current().route_id;

        assert!(cm.ring_bell(background_route, true));
        assert!(cm.bell(2));

        assert!(!cm.ring_bell(background_route, true));
        assert!(cm.bell(2));

        assert!(!cm.ring_bell(focused_route, true));
        assert!(!cm.bell(0));

        assert!(cm.ring_bell(focused_route, false));
        assert!(cm.bell(0));
        cm.contexts[0].bell = false;

        // Unknown routes (already-closed panes) are a no-op.
        assert!(!cm.ring_bell(usize::MAX, true));

        // Focusing the tab drops the mark on the next rendered frame.
        cm.set_current(2);
        assert!(cm.bell(2));
        assert!(cm.clear_current_bell());
        assert!(!cm.bell(2));
    }

    #[test]
    fn update_titles_only_current_leaves_other_tabs_dirty() {
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, WindowId::from(0))
                .unwrap();
        cm.add_context(false, 0);
        cm.config.title.content = "{{ columns }}".to_string();

        cm.mark_all_titles_dirty();
        assert!(cm.contexts[0].current().title_dirty);
        assert!(cm.contexts[1].current().title_dirty);

        // A current-only walk (tab strip absent) must not silently
        // clear panes it never re-rendered.
        assert!(cm.update_titles(true));
        assert!(!cm.contexts[0].current().title_dirty);
        assert!(cm.contexts[1].current().title_dirty);
        let columns = cm.contexts[0].current().dimension.columns.to_string();
        assert_eq!(cm.contexts[0].current().title.content, columns);

        // Surfacing the stale tab re-renders it via sync_current_route.
        cm.set_current(1);
        assert!(!cm.contexts[1].current().title_dirty);
        let columns = cm.contexts[1].current().dimension.columns.to_string();
        assert_eq!(cm.contexts[1].current().title.content, columns);
    }

    #[test]
    fn on_title_change_renders_displayed_pane_from_event_string() {
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, WindowId::from(0))
                .unwrap();
        cm.add_context(false, 0);
        cm.config.title.content = "{{ title }}".to_string();

        let background_route = cm.contexts[1].current().route_id;
        cm.contexts[1].current_mut().title_dirty = true;
        assert!(cm.on_title_change(background_route, Some("hello")));
        assert_eq!(cm.contexts[1].current().title.content, "hello");
        assert!(!cm.contexts[1].current().title_dirty);

        // The same title again changes nothing: no repaint requested.
        assert!(!cm.on_title_change(background_route, Some("hello")));

        // Unknown routes (already-closed panes) are a no-op.
        assert!(!cm.on_title_change(usize::MAX, Some("x")));
    }

    fn set_tab_title(cm: &mut ContextManager<VoidListener>, index: usize, content: &str) {
        cm.contexts[index].current_mut().title.content = content.to_string();
    }

    fn tab_titles(cm: &ContextManager<VoidListener>) -> Vec<String> {
        (0..cm.len())
            .map(|i| cm.title(i).unwrap().content.clone())
            .collect()
    }

    #[test]
    fn test_title_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        assert_eq!(cm.len(), 4);
        for (i, label) in ["a", "b", "c", "d"].iter().enumerate() {
            set_tab_title(&mut cm, i, label);
        }

        // Drag tab 1 to slot 3 (rotate). The title must track the moved
        // tab immediately, without waiting on the next update_titles tick.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(tab_titles(&cm), ["a", "c", "d", "b"]);
        assert_eq!(cm.current().title.content, "b");
    }

    #[test]
    fn test_title_follows_tab_swap() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        for (i, label) in ["a", "b", "c", "d"].iter().enumerate() {
            set_tab_title(&mut cm, i, label);
        }

        // Swap current (0) with its neighbor (1).
        cm.set_current(0);
        cm.move_current_to_next();

        assert_eq!(tab_titles(&cm), ["b", "a", "c", "d"]);
    }

    #[test]
    fn test_custom_title_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        cm.set_custom_title(2, Some("work".to_string()));

        // Move tab 1 → 3 (rotate): the override on tab 2 shifts to slot 1,
        // with no remap bookkeeping.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(cm.custom_title(1), Some("work"));
        assert_eq!(cm.custom_title(2), None);

        // Clearing with None removes the override.
        cm.set_custom_title(1, None);
        assert_eq!(cm.custom_title(1), None);
    }

    #[test]
    fn test_custom_color_follows_tab_move() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        for _ in 0..3 {
            cm.add_context(false, 0);
        }
        let red = [1.0, 0.0, 0.0, 1.0];
        cm.set_custom_color(2, Some(red));

        // Move tab 1 → 3 (rotate): the color on tab 2 shifts to slot 1.
        cm.set_current(1);
        cm.move_current_tab_to(3);

        assert_eq!(cm.custom_color(1), Some(red));
        assert_eq!(cm.custom_color(2), None);

        cm.set_custom_color(1, None);
        assert_eq!(cm.custom_color(1), None);
    }

    #[test]
    fn test_switch_to_next() {
        let window_id: WindowId = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);

        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 1);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 2);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 3);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 4);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 0);
        context_manager.switch_to_next();
        assert_eq!(context_manager.current_index, 1);
    }

    #[test]
    fn test_would_close_window() {
        let window_id = WindowId::from(0);
        let mut cm =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();

        // Sole context, sole pane: its exit closes the window.
        let only = cm.current().route_id;
        assert!(cm.would_close_window(only));

        // A second tab: neither tab's exit closes the window, and the
        // peek must not mutate the context set (regression guard for the
        // empty-vec panic when the shell-death close was deferred).
        cm.add_context(false, 0);
        assert_eq!(cm.len(), 2);
        let first = cm.contexts[0].current().route_id;
        let second = cm.contexts[1].current().route_id;
        assert!(!cm.would_close_window(first));
        assert!(!cm.would_close_window(second));
        assert_eq!(cm.len(), 2);

        // Unknown route never closes the window.
        assert!(!cm.would_close_window(usize::MAX));
    }

    #[test]
    fn test_move_current_to_next() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.current_mut().rich_text_id = 1;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_next();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);
    }

    #[test]
    fn test_move_current_to_prev() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.current_mut().rich_text_id = 1;
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        assert_eq!(context_manager.len(), 5);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 1);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);

        context_manager.move_current_to_prev();
        assert_eq!(context_manager.current_index, 4);
        assert_eq!(context_manager.current().rich_text_id, 1);
    }

    #[test]
    fn test_move_current_tab_to() {
        let window_id = WindowId::from(0);

        let mut context_manager =
            ContextManager::start_with_capacity(5, VoidListener {}, window_id).unwrap();
        let should_redirect = false;

        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);
        context_manager.add_context(should_redirect, 0);

        // Tag every tab with its starting position.
        for i in 0..5 {
            context_manager.set_current(i);
            context_manager.current_mut().rich_text_id = i;
        }

        let order = |cm: &mut ContextManager<VoidListener>| -> Vec<usize> {
            (0..5)
                .map(|i| {
                    cm.set_current(i);
                    cm.current().rich_text_id
                })
                .collect()
        };

        // Multi-slot jump forward: tabs in between shift left by one.
        context_manager.set_current(1);
        context_manager.move_current_tab_to(3);
        assert_eq!(context_manager.current_index, 3);
        assert_eq!(context_manager.current().rich_text_id, 1);
        assert_eq!(order(&mut context_manager), vec![0, 2, 3, 1, 4]);

        // Multi-slot jump backward: tabs in between shift right by one.
        context_manager.set_current(3);
        context_manager.move_current_tab_to(0);
        assert_eq!(context_manager.current_index, 0);
        assert_eq!(context_manager.current().rich_text_id, 1);
        assert_eq!(order(&mut context_manager), vec![1, 0, 2, 3, 4]);

        // No-op cases: same index and out-of-bounds target.
        context_manager.set_current(2);
        context_manager.move_current_tab_to(2);
        assert_eq!(context_manager.current_index, 2);
        context_manager.move_current_tab_to(5);
        assert_eq!(context_manager.current_index, 2);
        assert_eq!(order(&mut context_manager), vec![1, 0, 2, 3, 4]);
    }
}
