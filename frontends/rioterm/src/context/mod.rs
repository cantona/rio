pub mod renderable;
pub mod title;

use crate::ansi::CursorShape;
use crate::context::title::{
    create_title_extra_from_context, update_title, ContextTitle,
};
use crate::event::sync::FairMutex;
use crate::event::{Msg, RioEvent};
use crate::ime::Ime;
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
use std::borrow::Cow;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) {
        let _ = p::write_frame(
            &mut stream,
            p::FrameType::ClientHello,
            &p::encode_client_hello(),
        );
        let _ = p::write_frame(&mut stream, p::FrameType::Kill, &[]);
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
    pub rich_text_id: usize,
    pub dimension: ContextDimension,
    pub title: ContextTitle,
    pub ime: Ime,
    pub backend: PaneBackend,
    _io_thread: Option<JoinHandle<()>>,
}

impl<T: rio_backend::event::EventListener> Drop for Context<T> {
    fn drop(&mut self) {
        match &self.backend {
            PaneBackend::Local => {
                // Shutdown the terminal's PTY.
                let _ = self.messenger.channel.send(Msg::Shutdown);

                #[cfg(not(target_os = "windows"))]
                teletypewriter::kill_pid(self.shell_pid as i32);
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

    /// Foreground-process name, guarded like [`Context::foreground_path`]
    /// against probing a remote pane's foreign `shell_pid`.
    #[cfg(unix)]
    #[inline]
    pub fn foreground_name(&self) -> String {
        if matches!(self.backend, PaneBackend::Ptyd { host: Some(_), .. }) {
            return String::default();
        }
        teletypewriter::foreground_process_name(*self.main_fd, self.shell_pid)
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
    pub fn set_hyperlink_range(&mut self, hyperlink_range: Option<SelectionRange>) {
        let old_hyperlink = self.renderable_content.hyperlink_range;

        if old_hyperlink != hyperlink_range {
            // Hyperlinks affect terminal line rendering, so use terminal damage
            self.renderable_content
                .pending_update
                .set_terminal_damage(rio_backend::event::TerminalDamage::Full);
        }

        self.renderable_content.hyperlink_range = hyperlink_range;
    }

    #[inline]
    pub fn has_hyperlink_range(&self) -> bool {
        self.renderable_content.hyperlink_range.is_some()
    }

    #[inline]
    pub fn cursor_from_ref(&self) -> Cursor {
        Cursor {
            state: self.renderable_content.cursor.state.new_from_self(),
            content: self.renderable_content.cursor.content_ref,
            content_ref: self.renderable_content.cursor.content_ref,
            is_ime_enabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PersistenceOptions {
    pub ring_bytes: usize,
}

#[derive(Clone, Default)]
pub struct ContextManagerConfig {
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
    pub working_dir: Option<String>,
    pub spawn_performer: bool,
    pub cwd: bool,
    pub is_native: bool,
    pub should_update_title_extra: bool,
    pub split_color: [f32; 4],
    pub split_active_color: [f32; 4],
    pub panel: rio_backend::config::layout::Panel,
    pub title: rio_backend::config::title::Title,
    pub keyboard: rio_backend::config::keyboard::Keyboard,
    pub scrollback_history_limit: usize,
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
    last_title_update: Option<Instant>,
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
        messenger: Messenger::new(sender),
        renderable_content: RenderableContent::new(Cursor::default()),
        terminal,
        rich_text_id,
        dimension,
        title: ContextTitle::default(),
        ime: Ime::new(),
        backend: PaneBackend::Local,
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
        #[cfg(not(target_os = "windows"))]
        use_fork: true,
        working_dir: None,
        shell: Shell {
            program: std::env::var("SHELL").unwrap_or("bash".to_string()),
            args: vec![],
        },
        spawn_performer: false,
        is_native: false,
        should_update_title_extra: false,
        cwd: false,
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
        terminal.blinking_cursor = cursor_state.1;
        let terminal: Arc<FairMutex<Crosswords<T>>> = Arc::new(FairMutex::new(terminal));

        let pty;
        #[cfg(not(target_os = "windows"))]
        {
            if config.use_fork {
                tracing::info!("rio -> teletypewriter: create_pty_with_fork");
                pty = match create_pty_with_fork(
                    &Cow::Borrowed(&config.shell.program),
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
                    &Cow::Borrowed(&config.shell.program),
                    config.shell.args.clone(),
                    &config.working_dir,
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

        #[cfg(target_os = "windows")]
        {
            pty = match create_pty(
                &Cow::Borrowed(&config.shell.program),
                config.shell.args.clone(),
                &config.working_dir,
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

        Ok(Context {
            route_id,
            #[cfg(not(target_os = "windows"))]
            main_fd,
            #[cfg(not(target_os = "windows"))]
            shell_pid,
            messenger,
            terminal,
            rich_text_id,
            renderable_content: RenderableContent::new(cursor_state.0.clone()),
            dimension,
            title: ContextTitle::default(),
            ime: Ime::new(),
            backend: PaneBackend::Local,
            _io_thread: io_thread,
        })
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
            Some(machine.spawn())
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
                let (pty, hello, pane_id, socket) = crate::ptyd::RemotePty::spawn_local(
                    &config.shell.program,
                    &config.shell.args,
                    &config.working_dir,
                    config.session_name.as_deref(),
                    &winsize,
                    ring_bytes,
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

        Ok(Context {
            route_id,
            main_fd: Arc::new(-1),
            shell_pid: hello.shell_pid,
            messenger,
            terminal,
            rich_text_id,
            renderable_content: RenderableContent::new(cursor_state.0.clone()),
            dimension,
            title: ContextTitle::default(),
            ime: Ime::new(),
            backend: PaneBackend::Ptyd {
                pane_id,
                socket,
                host,
                reported_cwd: hello.cwd.clone(),
                replayed: attach.is_some(),
            },
            _io_thread: io_thread,
        })
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

        Ok(ContextManager {
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
            last_title_update: None,
        })
    }

    #[cfg(test)]
    pub fn start_with_capacity(
        capacity: usize,
        event_proxy: T,
        window_id: WindowId,
    ) -> Result<Self, Box<dyn Error>> {
        let config = ContextManagerConfig {
            #[cfg(not(target_os = "windows"))]
            use_fork: true,
            working_dir: None,
            shell: Shell {
                program: std::env::var("SHELL").unwrap_or("bash".to_string()),
                args: vec![],
            },
            spawn_performer: false,
            is_native: false,
            should_update_title_extra: false,
            cwd: false,
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
            last_title_update: None,
        })
    }

    #[inline]
    pub fn should_close_context_manager(
        &mut self,
        route_id: usize,
        sugarloaf: &mut Sugarloaf,
    ) -> bool {
        let requires_change_route = self.current_route == route_id;

        // should_close_context_manager is only called when terminal.exit()
        // is triggered. The terminal.exit() happens for any drop on context
        // by tab removal or if the Pty is exited (e.g: exit/control+d)
        //
        // In the tab case we already have removed the context with the
        // specified route_id so isn't gonna find anything. Then will be false.
        //
        // However if the tab is killed by Pty and not a tab action then
        // it means we need to clean the context with the specified route_id.
        // If there's no context then should return true and kill the window.
        if !self.contexts.is_empty() {
            // In case Grid has more than one item
            if self.current_grid().len() > 1 {
                if self.current().route_id == route_id {
                    self.remove_current_grid(sugarloaf);
                }

                return false;
            }

            // In case Grid has only one item
            if let Some(index_to_remove) = self
                .contexts
                .iter()
                .position(|ctx| ctx.current().route_id == route_id)
            {
                let mut should_set_current = false;
                if requires_change_route {
                    if index_to_remove > 1 {
                        self.set_current(index_to_remove - 1);
                    } else {
                        should_set_current = true;
                    }
                }
                self.contexts[index_to_remove].remove_all_rich_text(sugarloaf);
                self.contexts.remove(index_to_remove);

                if should_set_current {
                    self.set_current(0);
                }

                if !self.contexts.is_empty() {
                    self.keep_only_active_context_visible(sugarloaf);
                }
            };
        }

        self.contexts.is_empty()
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
    pub fn close_unfocused_tabs(&mut self) {
        let current_route_id = self.current().route_id;
        self.contexts
            .retain(|ctx| ctx.current().route_id == current_route_id);
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
        self.current_route = self.current().route_id;
    }

    #[inline]
    pub fn select_prev_split(&mut self) {
        self.contexts[self.current_index].select_prev_split();
        self.current_route = self.current().route_id;
    }

    #[inline]
    pub fn switch_to_next_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_next_split_no_loop() {
            self.current_route = self.current().route_id;
            return;
        }
        self.switch_to_next();
        // Make sure first split is selected - get the root key
        let current_tab = &mut self.contexts[self.current_index];
        if let Some(root) = current_tab.root {
            current_tab.current = root;
        }
        self.current_route = self.current().route_id;
    }

    #[inline]
    pub fn switch_to_prev_split_or_tab(&mut self) {
        if self.contexts[self.current_index].select_prev_split_no_loop() {
            self.current_route = self.current().route_id;
            return;
        }
        self.switch_to_prev();
        // Make sure last split is selected - get the last key in order
        let current_tab = &mut self.contexts[self.current_index];
        let ordered_keys = current_tab.get_ordered_keys();
        if let Some(&last_key) = ordered_keys.last() {
            current_tab.current = last_key;
        }
        self.current_route = self.current().route_id;
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

    #[inline]
    pub fn request_save_session(&mut self) {
        self.event_proxy
            .send_event(RioEvent::SaveSession, self.window_id);
    }

    /// Persist the session after a structural change (tab/pane
    /// open/close/split) when in automatic (`restore = "always"`) mode,
    /// so a crash — which runs no clean-exit save — still leaves a
    /// current session. No-op in `prompt` mode: that mode never saves
    /// without the user's yes. Structural changes are rare user
    /// actions, so the extra write is not on any hot path.
    #[inline]
    pub fn autosave_on_change(&mut self) {
        if self.config.autosave {
            self.request_save_session();
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
        self.current_route = self.current().route_id;
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.contexts.len()
    }

    #[inline]
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
    pub fn set_custom_title(&mut self, index: usize, title: Option<String>) {
        if let Some(grid) = self.contexts.get_mut(index) {
            grid.custom_title = title;
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

    pub fn update_titles(&mut self) {
        let interval_time = Duration::from_secs(2);
        if self
            .last_title_update
            .map(|i| i.elapsed() > interval_time)
            .unwrap_or(true)
        {
            self.last_title_update = Some(Instant::now());
            for grid in self.contexts.iter_mut() {
                let content = update_title(&self.config.title.content, grid.current());

                self.event_proxy
                    .send_event(RioEvent::Title(content.to_owned()), self.window_id);

                let extra = if self.config.should_update_title_extra {
                    create_title_extra_from_context(grid.current())
                } else {
                    None
                };

                grid.current_mut().title = ContextTitle { content, extra };
            }
        }
    }

    #[inline]
    pub fn get_by_route_id(
        &mut self,
        route_id: usize,
    ) -> Option<&mut ContextGridItem<T>> {
        self.contexts[self.current_index].get_by_route_id(route_id)
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
        self.current_route = self.contexts[self.current_index].current().route_id;
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

    /// Select the current grid's pane by visual order index. Used by
    /// session restore to reselect the saved active pane.
    pub fn select_pane_by_order(&mut self, index: usize) {
        let grid = &mut self.contexts[self.current_index];
        if let Some(&node) = grid.get_ordered_keys().get(index) {
            grid.set_current(node);
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
            self.current_route = self.current().route_id;
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
        self.contexts[index_to_remove].remove_all_rich_text(sugarloaf);
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

        self.current_route = self.current().route_id;
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

        self.current_route = self.current().route_id;
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
            is_native: config.navigation.is_native(),
            // When navigation is collapsed and does not contain any color rule
            // does not make sense fetch for foreground process names
            should_update_title_extra: !config.navigation.color_automation.is_empty(),
            split_color: config.colors.split,
            split_active_color: config.colors.split_active,
            panel: config.panel,
            title: config.title,
            keyboard: config.keyboard,
            scrollback_history_limit: config.scrollback_history_limit,
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
                        self.current_route = self.current().route_id;
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
