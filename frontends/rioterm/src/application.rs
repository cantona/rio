use crate::event::{ClickState, EventPayload, EventProxy, RioEvent, RioEventType};
use crate::ime::Preedit;
use crate::renderer::utils::update_colors_based_on_theme;
use crate::router::{routes::RoutePath, Router};
use crate::scheduler::{Scheduler, TimerId, Topic};
use crate::screen::touch::on_touch;
use crate::watcher::configuration_file_updates;
#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use raw_window_handle::HasDisplayHandle;
use rio_backend::clipboard::{Clipboard, ClipboardType};
use rio_backend::config::colors::{ColorRgb, NamedColor};
use rio_window::application::ApplicationHandler;
use rio_window::event::{
    ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, TouchPhase, WindowEvent,
};
use rio_window::event_loop::ActiveEventLoop;
use rio_window::event_loop::ControlFlow;
use rio_window::event_loop::{DeviceEvents, EventLoop};
#[cfg(target_os = "macos")]
use rio_window::platform::macos::ActiveEventLoopExtMacOS;
#[cfg(target_os = "macos")]
use rio_window::platform::macos::WindowExtMacOS;
use rio_window::window::WindowId;
use rio_window::window::{CursorIcon, Fullscreen};
use std::error::Error;
use std::time::{Duration, Instant};

pub struct Application<'a> {
    config: rio_backend::config::Config,
    event_proxy: EventProxy,
    router: Router<'a>,
    scheduler: Scheduler,
    app_id: Option<String>,
    session_name: Option<String>,
    /// Windows persisted per session file during THIS run, keyed by live
    /// WindowId. The accumulation/ordering/write policy lives in
    /// `session::SavedWindows`; this only holds the state.
    saved_windows: crate::session::SavedWindows<WindowId>,
    /// Daemon fate stashed while a SaveOnExit prompt is open: the close
    /// kind (kill for exit/close-tab, keep for the WM close button or a
    /// busy window) is decided where the close starts, but prompt mode
    /// defers the teardown to the answer — this carries the decision
    /// across so both modes tear down identically.
    pending_close_keeps: rustc_hash::FxHashMap<WindowId, bool>,
}

impl Application<'_> {
    pub fn new<'app>(
        config: rio_backend::config::Config,
        config_error: Option<rio_backend::config::ConfigError>,
        event_loop: &EventLoop<EventPayload>,
        app_id: Option<String>,
        session_name: Option<String>,
    ) -> Application<'app> {
        // SAFETY: Since this takes a pointer to the winit event loop, it MUST be dropped first,
        // which is done in `exiting`.
        let clipboard =
            unsafe { Clipboard::new(event_loop.display_handle().unwrap().as_raw()) };

        let mut router = Router::new(config.fonts.to_owned(), clipboard);
        if let Some(error) = config_error {
            router.propagate_error_to_next_route(error.into());
        }
        // A broken triggers.toml must be as visible as a broken
        // config.toml; a config error and a triggers error can't both
        // be pending here (a failed config load never loads triggers).
        if let Some(message) = &config.triggers_load_error {
            router.propagate_error_to_next_route(rio_backend::error::RioError {
                report: rio_backend::error::RioErrorType::InvalidTriggersFormat(
                    message.clone(),
                ),
                level: rio_backend::error::RioErrorLevel::Warning,
            });
        }

        let proxy = event_loop.create_proxy();
        let event_proxy = EventProxy::new(proxy.clone());
        let _ = configuration_file_updates(
            rio_backend::config::config_dir_path(),
            event_proxy.clone(),
        );
        let scheduler = Scheduler::new(proxy);
        event_loop.listen_device_events(DeviceEvents::Never);

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        event_loop.set_confirm_before_quit(config.confirm_before_quit);

        rio_notifier::request_authorization();

        Application {
            config,
            event_proxy,
            router,
            scheduler,
            app_id,
            session_name,
            saved_windows: crate::session::SavedWindows::new(),
            pending_close_keeps: rustc_hash::FxHashMap::default(),
        }
    }

    fn skip_window_event(event: &WindowEvent) -> bool {
        matches!(
            event,
            WindowEvent::KeyboardInput {
                is_synthetic: true,
                ..
            } | WindowEvent::ActivationTokenDone { .. }
                | WindowEvent::DoubleTapGesture { .. }
                | WindowEvent::TouchpadPressure { .. }
                | WindowEvent::RotationGesture { .. }
                | WindowEvent::CursorEntered { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::AxisMotion { .. }
                | WindowEvent::PanGesture { .. }
                | WindowEvent::HoveredFileCancelled
                | WindowEvent::Destroyed
                | WindowEvent::HoveredFile(_)
                | WindowEvent::Moved(_)
        )
    }

    fn handle_audio_bell(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // Use system bell sound on macOS
            unsafe {
                #[link(name = "AppKit", kind = "framework")]
                extern "C" {
                    fn NSBeep();
                }
                NSBeep();
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Use MessageBeep on Windows with MB_OK (0x00000000) for default beep
            unsafe {
                windows_sys::Win32::System::Diagnostics::Debug::MessageBeep(0x00000000);
            }
        }

        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            #[cfg(feature = "audio")]
            {
                std::thread::spawn(|| {
                    if let Err(e) = play_bell_sound() {
                        tracing::warn!("Failed to play bell sound: {}", e);
                    }
                });
            }
            #[cfg(not(feature = "audio"))]
            {
                tracing::debug!("Audio bell requested but audio feature is not enabled");
            }
        }
    }

    fn handle_desktop_notification(&self, title: &str, body: &str, urgency: u8) {
        rio_notifier::send_notification(title, body, urgency);
    }

    /// Capture every open UNNAMED window into the implicit last-session
    /// slot, focused window first so it restores into the launch route.
    /// A single per-window writer would drop all the other windows; this
    /// is the accumulating writer for window close and quit.
    /// `merge_kept_daemons` carries forward any still-live daemon the
    /// old file referenced but no live window reattached.
    fn save_last_session(&mut self, focused: Option<WindowId>) {
        let path = rio_backend::config::session_file_path();
        self.persist_session(&path, None, focused, crate::session::DaemonCheck::Probe);
    }

    /// Capture every currently-open window matching `filter`, keyed by
    /// WindowId, and hand them to the session accumulator to write. The
    /// router walk is Application's job; the accumulate/order/write policy
    /// lives in `session::SavedWindows`. Windows closed earlier this run
    /// stay recorded there, so a multi-window quit — where each window's
    /// close-save sees a shrinking open set — is never reduced to the
    /// still-open subset. Used by the close and quit paths only; saves
    /// driven by live user activity (explicit save, autosave) mirror the
    /// open windows via `snapshot_open_windows` instead. `preferred` is
    /// written first so restore lands it in the launch route. `check`
    /// picks whether the carry-forward merge probes candidate daemons.
    fn persist_session(
        &mut self,
        path: &std::path::Path,
        filter: Option<&str>,
        preferred: Option<WindowId>,
        check: crate::session::DaemonCheck,
    ) {
        let captured = self.capture_by_id(filter);
        self.saved_windows
            .accumulate_and_write(path, captured, preferred, check);
    }

    /// Snapshot exactly the currently-open windows matching `filter` to
    /// `path`, discarding any windows this run's accumulator recorded for
    /// it. For explicit "save now" actions and autosave, where the file
    /// should mirror what is open, not resurrect earlier-closed windows.
    fn snapshot_open_windows(
        &mut self,
        path: &std::path::Path,
        filter: Option<&str>,
        preferred: WindowId,
        check: crate::session::DaemonCheck,
        keep_pinned: bool,
    ) {
        let captured = self.capture_by_id(filter);
        self.saved_windows.replace_and_write(
            path,
            captured,
            Some(preferred),
            check,
            keep_pinned,
        );
    }

    /// True when any persistent pane of the window has a program in the
    /// foreground of its shell. A remote pane counts as busy — its
    /// foreground is not inspectable from here, and keeping it is the
    /// safe default. Plain-local panes never count: their shell dies
    /// with the window regardless, so there is nothing to preserve.
    #[cfg(unix)]
    fn window_has_running_process(route: &crate::router::Route) -> bool {
        route.window.screen.ctx().grids().iter().any(|grid| {
            grid.contexts()
                .values()
                .any(|item| match &item.val.backend {
                    crate::context::PaneBackend::Ptyd { host: None, .. } => {
                        rio_ptyd::sockdir::foreground_activity(item.val.shell_pid as i32)
                            .is_some()
                    }
                    crate::context::PaneBackend::Ptyd { host: Some(_), .. } => true,
                    crate::context::PaneBackend::Local => false,
                })
        })
    }

    /// Capture (WindowId, WindowState) for every open window matching
    /// `filter`. `None` selects the implicit slot — only windows with no
    /// session name — so named workspaces never bleed into session.json
    /// (a plain launch would restore them unnamed and steal their
    /// single-client daemons from the named instance).
    fn capture_by_id(
        &self,
        filter: Option<&str>,
    ) -> Vec<(WindowId, crate::session::WindowState)> {
        let max = self.config.session.max_scrollback_lines;
        let matches = |name: Option<&str>| name == filter;
        self.router
            .routes
            .iter()
            // Ephemeral windows (the settings editor) are tooling, not
            // workspace: saving one would restore it as a junk shell.
            .filter(|(_, route)| {
                !route.ephemeral && matches(route.session_name.as_deref())
            })
            .map(|(id, route)| {
                (
                    *id,
                    crate::session::capture_window(
                        route.window.screen.ctx(),
                        max,
                        &route.window.winit_window,
                    ),
                )
            })
            .collect()
    }

    /// Distinct session names bound to open windows.
    fn distinct_session_names(&self) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        self.router
            .routes
            .values()
            .filter_map(|r| r.session_name.clone())
            .filter(|n| seen.insert(n.clone()))
            .collect()
    }

    /// Ids of the open windows bound to `name` (None = the implicit slot).
    fn windows_of(&self, name: Option<&str>) -> Vec<WindowId> {
        self.router
            .routes
            .iter()
            .filter(|(_, r)| r.session_name.as_deref() == name)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Persist every distinct named workspace open in this process, one
    /// write per name — all windows sharing a name land in that name's
    /// file, so a multi-window `--session NAME` restores whole instead of
    /// collapsing to a single window. Gated on the named close
    /// disposition: Save writes silently, Prompt only with the quit
    /// prompt's yes-answer (`prompted_consent`) — a named file whose mode
    /// promised to ask is never silently overwritten.
    fn save_named_sessions(&mut self, prompted_consent: bool) {
        if !crate::session::write_on_quit(
            true,
            self.config.session.restore,
            prompted_consent,
        ) {
            return;
        }
        for name in self.distinct_session_names() {
            let focused = self
                .router
                .routes
                .iter()
                .find(|(_, r)| r.session_name.as_deref() == Some(name.as_str()))
                .map(|(id, _)| *id);
            let path = rio_backend::config::session_named_path(&name);
            self.persist_session(
                &path,
                Some(&name),
                focused,
                crate::session::DaemonCheck::Probe,
            );
        }
    }

    /// Whether a quit needs the save prompt: some open session — the
    /// implicit slot or a named workspace — has a Prompt disposition.
    /// Checked process-wide, not per the requesting window: a quit takes
    /// every window down, so a Prompt session anywhere must be asked
    /// about before anything exits.
    fn quit_needs_prompt(&self) -> bool {
        use crate::session::CloseDisposition;
        let mode = self.config.session.restore;
        let mut named = false;
        let mut unnamed = false;
        for route in self.router.routes.values() {
            if route.session_name.is_some() {
                named = true;
            } else {
                unnamed = true;
            }
        }
        (named
            && crate::session::close_disposition(true, mode) == CloseDisposition::Prompt)
            || (unnamed
                && crate::session::close_disposition(false, mode)
                    == CloseDisposition::Prompt)
    }

    /// Quit entry point (no-confirm Quit and the QuitConfirmed answer to
    /// "want to quit?"). When any open session's disposition is Prompt,
    /// raise the quit-scoped save prompt on the requesting window and
    /// defer the exit to its answer (QuitSaveAnswered) — closing only
    /// that window would silently drop the quit for the rest. Otherwise
    /// persist per disposition and exit now.
    fn quit_now(&mut self, window_id: WindowId) {
        use crate::renderer::session_prompt::SessionPromptKind;
        if self.quit_needs_prompt() {
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                route.window.screen.renderer.confirm_quit.set_active(false);
                route
                    .window
                    .screen
                    .renderer
                    .session_prompt
                    .set_active(Some(SessionPromptKind::SaveOnQuit));
                route.request_overlay_redraw();
                return;
            }
        }
        self.quit_all(Some(window_id), None);
    }

    /// Complete a quit for every window: settle each session per its
    /// close disposition, then exit the process. `consent` is the quit
    /// prompt's answer when one was shown; it resolves Prompt
    /// dispositions — save on yes, discard (file and daemons) on no.
    /// With no prompt asked (`None`) a Prompt session is left untouched:
    /// neither silently overwritten nor silently discarded.
    fn quit_all(&mut self, focused: Option<WindowId>, consent: Option<bool>) {
        use crate::session::CloseDisposition;
        let mode = self.config.session.restore;
        #[cfg(unix)]
        crate::context::set_quit_detaching();
        if consent == Some(false) {
            if crate::session::close_disposition(true, mode) == CloseDisposition::Prompt {
                for name in self.distinct_session_names() {
                    let dying = self.windows_of(Some(&name));
                    self.decline_session(
                        &rio_backend::config::session_named_path(&name),
                        &dying,
                    );
                }
            }
            if crate::session::close_disposition(false, mode) == CloseDisposition::Prompt
            {
                let dying = self.windows_of(None);
                if !dying.is_empty() {
                    self.decline_session(
                        &rio_backend::config::session_file_path(),
                        &dying,
                    );
                }
            }
        } else {
            let consented = consent == Some(true);
            self.save_named_sessions(consented);
            if crate::session::write_on_quit(false, mode, consented) {
                self.save_last_session(focused);
            }
        }
        std::process::exit(0);
    }

    /// "Don't save" for the session at `path`: the discard must reap
    /// every process the session owns, not just the answering window's.
    /// Kill the live panes of the `dying` windows, then every daemon the
    /// saved file still references — for windows closed earlier this run
    /// the file holds the only handle on their detached daemons — while
    /// sparing daemons attached to a window that stays open (its own
    /// close will ask again). Then drop the file and this run's
    /// accumulated windows for it.
    fn decline_session(&mut self, path: &std::path::Path, dying: &[WindowId]) {
        #[cfg(unix)]
        {
            for id in dying {
                if let Some(route) = self.router.routes.get(id) {
                    crate::session::kill_persistent_panes(route.window.screen.ctx());
                }
            }
            if let Some(state) = crate::session::SessionState::load(path) {
                let mut spare = std::collections::HashSet::new();
                for (id, route) in self.router.routes.iter() {
                    if !dying.contains(id) {
                        crate::session::live_local_sockets(
                            route.window.screen.ctx(),
                            &mut spare,
                        );
                    }
                }
                crate::session::kill_saved_session_daemons(&state, &spare);
            }
        }
        #[cfg(not(unix))]
        let _ = dying;
        crate::session::SessionState::discard(path);
        self.saved_windows.forget(path);
    }

    /// A window's last tab closed (WM-close or shell `exit`). Persist per
    /// the restore mode. Returns true when a save PROMPT was shown, so
    /// the caller must NOT close the window yet — the prompt's answer
    /// (SaveOnExit handler) removes it. Returns false when the close may
    /// proceed immediately (saved silently in `always`/named, or nothing
    /// to save in `disable`).
    fn prompt_or_save_on_close(&mut self, window_id: WindowId, keeps: bool) -> bool {
        use crate::renderer::session_prompt::SessionPromptKind;
        use crate::session::CloseDisposition;
        // An ephemeral window is never captured, so there is nothing
        // to save or ask about when it closes.
        if self
            .router
            .routes
            .get(&window_id)
            .is_some_and(|r| r.ephemeral)
        {
            return false;
        }
        let named = self
            .router
            .routes
            .get(&window_id)
            .and_then(|r| r.session_name.clone());
        match crate::session::close_disposition(
            named.is_some(),
            self.config.session.restore,
        ) {
            CloseDisposition::Save => {
                self.save_session_now(window_id, named);
                false
            }
            CloseDisposition::Prompt => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .renderer
                        .session_prompt
                        .set_active(Some(SessionPromptKind::SaveOnExit));
                    route.request_overlay_redraw();
                    self.pending_close_keeps.insert(window_id, keeps);
                    true
                } else {
                    false
                }
            }
            CloseDisposition::Nothing => false,
        }
    }

    /// Save the window's session now (silent path): all windows of its
    /// name to `<name>.json`, or every unnamed window to the implicit
    /// slot. The single silent-save helper close paths share. The closing
    /// window is still in routes here (removed only after this save), so
    /// persist_session captures it along with its still-open siblings.
    fn save_session_now(&mut self, window_id: WindowId, named: Option<String>) {
        let (path, filter) = match &named {
            Some(name) => (
                rio_backend::config::session_named_path(name),
                Some(name.clone()),
            ),
            None => (rio_backend::config::session_file_path(), None),
        };
        self.persist_session(
            &path,
            filter.as_deref(),
            Some(window_id),
            crate::session::DaemonCheck::Probe,
        );
    }

    /// The one close sequence every path shares: tear the window down
    /// with the kill-vs-detach choice, then — for a kill-close that is
    /// allowed to write (silent mode, or the user just consented) —
    /// mirror the surviving windows so the closed one drops out of its
    /// slot. The kill in Context::drop waits for the daemons to die,
    /// so the mirror's probe never reads a dying daemon as alive.
    fn teardown_closed_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        keeps: bool,
        consented: bool,
    ) {
        let named = self
            .router
            .routes
            .get(&window_id)
            .and_then(|r| r.session_name.clone());
        self.close_window_now(event_loop, window_id, keeps);
        if !keeps && (self.config.session.restore.enabled() || consented) {
            let (path, filter) = match &named {
                Some(name) => (
                    rio_backend::config::session_named_path(name),
                    Some(name.clone()),
                ),
                None => (rio_backend::config::session_file_path(), None),
            };
            self.snapshot_open_windows(
                &path,
                filter.as_deref(),
                window_id,
                crate::session::DaemonCheck::AssumeAlive,
                true,
            );
        }
    }

    /// Remove a window and exit the loop if it was the last one. Detaches
    /// its persistent panes (daemons survive) rather than killing them.
    /// The save/prompt decision already happened; this only tears down.
    fn close_window_now(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        keep_daemons: bool,
    ) {
        // Kill-vs-detach pairs with the session write: a close that
        // stays in the session (WM close button, save-prompt yes)
        // detaches so the next launch reattaches; a close that prunes
        // the window from the session (exit, close-tab) kills, or the
        // daemons would linger unreferenced and a probed save would
        // carry them back as rescued tabs.
        #[cfg(unix)]
        {
            if keep_daemons {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.detach_on_close();
                }
            }
        }
        #[cfg(not(unix))]
        let _ = keep_daemons;
        self.router.routes.remove(&window_id);
        #[cfg(unix)]
        crate::context::clear_quit_detaching();
        if self.router.routes.is_empty() {
            // macOS keeps the app alive with no windows (dock); everywhere
            // else the last window closing must exit the process.
            if cfg!(not(target_os = "macos")) || !self.config.confirm_before_quit {
                event_loop.exit();
            }
        }
    }

    /// Restore each leftover saved window into a freshly created window.
    /// The launch route consumed the first saved window; without this
    /// the rest would be silently dropped. After opening them all,
    /// re-save (always mode) so the implicit file references the live
    /// daemons across every window instead of the pre-restore ones.
    /// Returns whether any of them restored a tab.
    /// The one multi-window restore both modes share: window[0] into
    /// `window_id`'s route, every leftover saved window into a fresh OS
    /// window, then one save so the file references the daemons the
    /// session just reattached (skipped when nothing restored, so a
    /// transient spawn failure leaves the file intact for a retry).
    fn restore_full_session(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        state: crate::session::SessionState,
    ) {
        let (leftover, restored) = self
            .router
            .routes
            .get_mut(&window_id)
            .map(|route| route.restore_session(state))
            .unwrap_or_default();
        let had_extra = !leftover.is_empty();
        let extra_restored = self.restore_extra_windows(event_loop, leftover);
        if (restored && had_extra) || extra_restored {
            let focused = self.router.get_focused_route();
            self.save_last_session(focused);
        } else if restored {
            // Single saved window: re-save just it, referencing the
            // reattached daemons (the always path does this inside
            // restore_session; the prompt path defers it to here).
            if let Some(route) = self.router.routes.get_mut(&window_id) {
                route.save_session();
            }
        }
    }

    fn restore_extra_windows(
        &mut self,
        event_loop: &ActiveEventLoop,
        leftover: Vec<crate::session::WindowState>,
    ) -> bool {
        let name = self
            .session_name
            .clone()
            .map(|n| crate::session::sanitize_name(&n));
        let mut restored_any = false;
        for win in leftover {
            let before: std::collections::HashSet<WindowId> =
                self.router.routes.keys().copied().collect();
            self.router.create_window(
                event_loop,
                self.event_proxy.clone(),
                &self.config,
                None,
                self.app_id.as_deref(),
                name.clone(),
            );
            let new_id = self
                .router
                .routes
                .keys()
                .find(|id| !before.contains(*id))
                .copied();
            if let Some(id) = new_id {
                if let Some(route) = self.router.routes.get_mut(&id) {
                    restored_any |= route.restore_window_state(win);
                }
            }
        }
        restored_any
    }

    pub fn run(
        &mut self,
        event_loop: EventLoop<EventPayload>,
    ) -> Result<(), Box<dyn Error>> {
        let result = event_loop.run_app(self);
        result.map_err(Into::into)
    }
}

impl ApplicationHandler<EventPayload> for Application<'_> {
    fn resumed(&mut self, _active_event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init
            && cause != StartCause::CreateWindow
            && cause != StartCause::MacOSReopen
        {
            return;
        }

        if cause == StartCause::MacOSReopen && !self.router.routes.is_empty() {
            return;
        }

        #[cfg(all(
            any(feature = "x11", feature = "wayland"),
            unix,
            not(any(target_os = "redox", target_family = "wasm", target_os = "macos"))
        ))]
        if cause == StartCause::Init
            && self.config.adaptive_colors.is_some()
            && self.config.force_theme.is_none()
        {
            use rio_window::platform::linux::ActiveEventLoopExtLinux;
            event_loop.start_system_theme_monitor();
        }

        let theme = self
            .config
            .force_theme
            .map(|t| t.to_window_theme())
            .or_else(|| event_loop.system_theme());
        update_colors_based_on_theme(&mut self.config, theme);

        // Sanitize the `--session` name before the launch window is built
        // so the initial tab's daemon spawns already tagged with it
        // (rio-ptyd groups panes by this tag). Applying it after the
        // window was created left the first pane untagged.
        let launch_session = self
            .session_name
            .clone()
            .map(|n| crate::session::sanitize_name(&n));

        self.router.create_window(
            event_loop,
            self.event_proxy.clone(),
            &self.config,
            None,
            self.app_id.as_deref(),
            launch_session.clone(),
        );

        // Reap dead daemons' stale sockets in the background.
        #[cfg(unix)]
        {
            let _ = std::process::Command::new(crate::ptyd::ptyd_binary())
                .arg("gc")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }

        if let Some(name) = launch_session {
            // `rio --session <name>`: explicit binding restores the
            // named session (when it exists) regardless of the
            // configured restore mode, and saves write back to it.
            let mut leftover = Vec::new();
            if let Some(route) = self.router.routes.values_mut().next() {
                route.set_session_name(Some(name.clone()));
                if let Some(state) = crate::session::SessionState::load(
                    &rio_backend::config::session_named_path(&name),
                ) {
                    leftover = route.restore_session(state).0;
                }
            }
            // A named workspace that saved several windows reopens them
            // all; each carries its own name binding.
            let _ = self.restore_extra_windows(event_loop, leftover);
            for route in self.router.routes.values_mut() {
                if route.session_name.is_none() {
                    route.set_session_name(Some(name.clone()));
                }
            }
        } else {
            use rio_backend::config::session::SessionRestore;
            match self.config.session.restore {
                SessionRestore::Never => {}
                mode => {
                    if let Some(state) = crate::session::SessionState::load(
                        &rio_backend::config::session_file_path(),
                    ) {
                        if mode == SessionRestore::Always {
                            if let Some(id) = self.router.routes.keys().next().copied() {
                                self.restore_full_session(event_loop, id, state);
                            }
                        } else if let Some(route) = self.router.routes.values_mut().next()
                        {
                            route.prompt_session_resume(state);
                        }
                    }
                }
            }
        }

        // Schedule title updates every 2s
        let timer_id = TimerId::new(Topic::UpdateTitles, 0);
        if !self.scheduler.scheduled(timer_id) {
            self.scheduler.schedule(
                EventPayload::new(RioEventType::Rio(RioEvent::UpdateTitles), unsafe {
                    rio_window::window::WindowId::dummy()
                }),
                Duration::from_secs(2),
                true,
                timer_id,
            );
        }

        tracing::info!("Initialisation complete");
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EventPayload) {
        let window_id = event.window_id;
        match event.payload {
            RioEventType::Rio(RioEvent::Render) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // Skip rendering for unfocused windows if configured
                    if self.config.renderer.disable_unfocused_render
                        && !route.window.is_focused
                    {
                        return;
                    }

                    // Skip rendering for occluded windows if configured, unless we need to render after occlusion
                    if self.config.renderer.disable_occluded_render
                        && route.window.is_occluded
                        && !route.window.needs_render_after_occlusion
                    {
                        return;
                    }

                    // Clear the one-time render flag if it was set
                    if route.window.needs_render_after_occlusion {
                        route.window.needs_render_after_occlusion = false;
                    }

                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::RenderRoute(route_id)) => {
                if self.config.renderer.strategy.is_event_based() {
                    if let Some(route) = self.router.routes.get_mut(&window_id) {
                        // Skip rendering for unfocused windows if configured
                        if self.config.renderer.disable_unfocused_render
                            && !route.window.is_focused
                        {
                            if route.window.screen.renderer.scrollbar.needs_redraw() {
                                route.request_redraw();
                            }
                            return;
                        }

                        // Skip rendering for occluded windows if configured, unless we need to render after occlusion
                        if self.config.renderer.disable_occluded_render
                            && route.window.is_occluded
                            && !route.window.needs_render_after_occlusion
                        {
                            return;
                        }

                        // Clear the one-time render flag if it was set
                        if route.window.needs_render_after_occlusion {
                            route.window.needs_render_after_occlusion = false;
                        }

                        // Mark the renderable content as needing to render
                        if let Some(ctx_item) =
                            route.window.screen.ctx_mut().get_by_route_id(route_id)
                        {
                            ctx_item.val.renderable_content.pending_update.set_dirty();
                        }

                        // Check if we need to throttle based on timing
                        if let Some(wait_duration) = route.window.wait_until() {
                            // We need to wait before rendering again
                            let timer_id = TimerId::new(Topic::RenderRoute, route_id);
                            let event = EventPayload::new(
                                RioEventType::Rio(RioEvent::Render),
                                window_id,
                            );

                            // Only schedule if not already scheduled
                            if !self.scheduler.scheduled(timer_id) {
                                self.scheduler.schedule(
                                    event,
                                    wait_duration,
                                    false,
                                    timer_id,
                                );
                            }
                        } else {
                            // We can render immediately
                            route.request_redraw();
                        }
                    }
                }
            }

            RioEventType::Rio(RioEvent::TerminalDamaged(route_id)) => {
                if self.config.renderer.strategy.is_event_based() {
                    if let Some(route) = self.router.routes.get_mut(&window_id) {
                        if self.config.renderer.disable_unfocused_render
                            && !route.window.is_focused
                        {
                            return;
                        }
                        if self.config.renderer.disable_occluded_render
                            && route.window.is_occluded
                            && !route.window.needs_render_after_occlusion
                        {
                            return;
                        }

                        if let Some(ctx_item) =
                            route.window.screen.ctx_mut().get_by_route_id(route_id)
                        {
                            // Just mark dirty — damage will be extracted from
                            // the terminal when the renderer locks it.
                            ctx_item.val.renderable_content.pending_update.set_dirty();
                            route.request_redraw();
                        }
                    }
                }
            }
            RioEventType::Rio(RioEvent::UpdateGraphics { route_id, queues }) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // Process graphics directly in sugarloaf
                    let sugarloaf = &mut route.window.screen.sugarloaf;

                    // Removals before inserts: an evict-then-retransmit of
                    // the same kitty id lands in one batch, and the fresh
                    // pixels must survive the eviction of the old ones.
                    // The store is shared by every terminal in the window
                    // while both protocols allocate ids per terminal, so
                    // every key is scoped to the originating route and its
                    // id space — a kitty removal must not delete an atlas
                    // entry (or another tab's image) and leak the texture
                    // the cells still reference.
                    for removal in queues.remove_queue {
                        let key = match removal {
                            rio_backend::ansi::graphics::GraphicRemoval::Atlas(id) => {
                                rio_backend::sugarloaf::ImageKey::atlas(
                                    route_id,
                                    id.get(),
                                )
                            }
                            rio_backend::ansi::graphics::GraphicRemoval::Kitty(id) => {
                                rio_backend::sugarloaf::ImageKey::kitty(route_id, id)
                            }
                        };
                        sugarloaf.image_data.remove(&key);
                    }

                    // Atlas graphics (sixel/iTerm2) → the same per-image
                    // texture store the overlay pipeline draws from.
                    for graphic_data in queues.pending {
                        let key = rio_backend::sugarloaf::ImageKey::atlas(
                            route_id,
                            graphic_data.id.get(),
                        );
                        sugarloaf.image_data.insert(
                            key,
                            rio_backend::sugarloaf::GraphicDataEntry::from_graphic_data(
                                graphic_data,
                            ),
                        );
                    }

                    // Image textures (kitty) → same store, no clone
                    for (image_id, graphic_data) in queues.pending_images {
                        sugarloaf.image_data.insert(
                            rio_backend::sugarloaf::ImageKey::kitty(route_id, image_id),
                            rio_backend::sugarloaf::GraphicDataEntry::from_graphic_data(
                                graphic_data,
                            ),
                        );
                    }

                    // Mark the panel dirty — the renderer skips non-dirty
                    // panels, so a bare redraw after the pixels arrive
                    // would no-op and leave the image blank until the
                    // next unrelated damage.
                    if let Some(ctx_item) =
                        route.window.screen.ctx_mut().get_by_route_id(route_id)
                    {
                        ctx_item.val.renderable_content.pending_update.set_dirty();
                    }

                    // Request a redraw to display the updated graphics
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::PrepareUpdateConfig) => {
                let timer_id = TimerId::new(Topic::UpdateConfig, 0);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::UpdateConfig),
                    window_id,
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(250),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::ReportToAssistant(error)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.report_error(&error);
                }
            }
            RioEventType::Rio(RioEvent::UpdateConfig) => {
                // A config.toml typo mid-edit must not reset the live
                // config (fonts, colors, binds, session) to defaults:
                // keep running on the current one, surface the error,
                // and pick up the next save. Same rule as triggers.toml
                // below. Startup is different — there is nothing live to
                // keep, so main's load still falls back to defaults.
                let config = match rio_backend::config::Config::try_load() {
                    Ok(config) => config,
                    Err(error) => {
                        tracing::warn!(
                            "config.toml failed to parse; keeping the previous config"
                        );
                        for (_id, route) in self.router.routes.iter_mut() {
                            route.report_error(&error.to_owned().into());
                            route.request_redraw();
                        }
                        return;
                    }
                };

                let has_font_updates = self.config.fonts != config.fonts;

                let font_library_errors = if has_font_updates {
                    let new_font_library = rio_backend::sugarloaf::font::FontLibrary::new(
                        config.fonts.to_owned(),
                    );
                    *self.router.font_library = new_font_library.0;
                    new_font_library.1
                } else {
                    None
                };

                // A triggers.toml typo mid-edit must not silently wipe the
                // live rules (and their highlights): the failed load left
                // the fresh config's triggers empty, so keep the running
                // ones until the file parses again, and surface the parse
                // error the same way a config.toml one is surfaced.
                let previous_triggers = if config.triggers_load_error.is_some() {
                    Some(std::mem::take(&mut self.config.triggers))
                } else {
                    None
                };
                self.config = config;
                if let Some(triggers) = previous_triggers {
                    tracing::warn!(
                        "triggers.toml failed to parse; keeping the previous rules"
                    );
                    self.config.triggers = triggers;
                }

                let mut has_checked_adaptive_colors = false;
                for (_id, route) in self.router.routes.iter_mut() {
                    // Apply system theme to ensure colors are consistent
                    if !has_checked_adaptive_colors {
                        let system_theme = event_loop.system_theme();
                        let theme = self
                            .config
                            .force_theme
                            .map(|t| t.to_window_theme())
                            .or(system_theme);
                        update_colors_based_on_theme(&mut self.config, theme);
                        has_checked_adaptive_colors = true;
                    }

                    if has_font_updates {
                        if let Some(ref err) = font_library_errors {
                            route
                                .window
                                .screen
                                .context_manager
                                .report_error_fonts_not_found(
                                    err.fonts_not_found.clone(),
                                );
                        }
                    }

                    route.update_config(
                        &self.config,
                        &self.router.font_library,
                        has_font_updates,
                    );
                    route.window.configure_window(&self.config);

                    // The reload parsed: drop any error screen left from a
                    // previously broken save.
                    route.clear_errors();

                    route.request_redraw();
                }

                // After clear_errors and the renderer rebuilds above, or
                // the overlay would be wiped by the very reload that
                // should show it: a triggers.toml typo must be as visible
                // as a config.toml one.
                if let Some(message) = &self.config.triggers_load_error {
                    let error = rio_backend::error::RioError {
                        report: rio_backend::error::RioErrorType::InvalidTriggersFormat(
                            message.clone(),
                        ),
                        level: rio_backend::error::RioErrorLevel::Warning,
                    };
                    for (_id, route) in self.router.routes.iter_mut() {
                        route.report_error(&error);
                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::SaveSession { explicit }) => {
                // Named or implicit, the session spans every window of
                // this process: save them all so a multi-window
                // workspace comes back whole.
                let named = self
                    .router
                    .routes
                    .get(&window_id)
                    .and_then(|route| route.session_name.clone());
                let (path, filter) = match &named {
                    Some(name) => (
                        rio_backend::config::session_named_path(name),
                        Some(name.as_str()),
                    ),
                    None => (rio_backend::config::session_file_path(), None),
                };
                // Both mirror the open windows: a window the user closed
                // (exit, close-tab, close button) must drop out of the
                // session on the next save, or resume resurrects it.
                // Multi-window quit stays whole regardless — the close
                // and quit paths accumulate, and no autosave fires
                // between the teardowns of a quit. Explicit saves probe
                // every daemon; autosave assumes (escalated to a probe
                // inside replace_and_write when a window dropped out).
                if explicit {
                    self.snapshot_open_windows(
                        &path,
                        filter,
                        window_id,
                        crate::session::DaemonCheck::Probe,
                        false,
                    );
                } else {
                    self.snapshot_open_windows(
                        &path,
                        filter,
                        window_id,
                        crate::session::DaemonCheck::AssumeAlive,
                        true,
                    );
                }
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .renderer
                        .session_prompt
                        .set_saved_notice(true);
                    route.request_redraw();
                    self.scheduler.schedule(
                        EventPayload::new(
                            RioEventType::Rio(RioEvent::ClearSessionNotice),
                            window_id,
                        ),
                        Duration::from_millis(1500),
                        false,
                        TimerId::new(Topic::ClearSessionNotice, 0),
                    );
                }
            }
            RioEventType::Rio(RioEvent::SaveSessionAs(name)) => {
                // Bind the focused window to the name, then save the whole
                // workspace (every window that now shares it) under it.
                let bound = self
                    .router
                    .routes
                    .get_mut(&window_id)
                    .and_then(|route| route.bind_session_name(&name));
                if let Some(name) = bound {
                    let path = rio_backend::config::session_named_path(&name);
                    self.snapshot_open_windows(
                        &path,
                        Some(&name),
                        window_id,
                        crate::session::DaemonCheck::Probe,
                        false,
                    );
                }
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .renderer
                        .session_prompt
                        .set_saved_notice(true);
                    route.request_redraw();
                    self.scheduler.schedule(
                        EventPayload::new(
                            RioEventType::Rio(RioEvent::ClearSessionNotice),
                            window_id,
                        ),
                        Duration::from_millis(1500),
                        false,
                        TimerId::new(Topic::ClearSessionNotice, 0),
                    );
                }
            }
            RioEventType::Rio(RioEvent::RestoreSessionByName(name)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.restore_session_named(&name);
                }
            }
            #[cfg(unix)]
            RioEventType::Rio(RioEvent::RemotePanesListed { host, panes, error }) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .renderer
                        .command_palette
                        .set_remote_result(&host, panes, error);
                    route.request_overlay_redraw();
                }
            }
            RioEventType::Rio(RioEvent::CloseButtonArmed) => {
                self.scheduler.schedule(
                    EventPayload::new(
                        RioEventType::Rio(RioEvent::DisarmCloseButton),
                        window_id,
                    ),
                    crate::renderer::island::Island::ARM_TIMEOUT
                        + Duration::from_millis(100),
                    false,
                    TimerId::new(Topic::DisarmCloseButton, 0),
                );
            }
            RioEventType::Rio(RioEvent::DisarmCloseButton) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let changed = route
                        .window
                        .screen
                        .renderer
                        .island
                        .as_mut()
                        .is_some_and(|island| island.disarm_stale());
                    if changed {
                        route.window.screen.mark_dirty();
                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::ClearSessionNotice) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .renderer
                        .session_prompt
                        .set_saved_notice(false);
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::Exit | RioEvent::Quit) => {
                if self.config.confirm_before_quit {
                    // Show "want to quit?"; the y answer fires QuitConfirmed
                    // which runs quit_now(). Without that, answering y in
                    // any non-prompt mode (always/disable) just dismissed
                    // the overlay and nothing exited — the window froze.
                    if let Some(route) = self.router.routes.get_mut(&window_id) {
                        route.confirm_quit();
                    }
                } else {
                    self.quit_now(window_id);
                }
            }
            RioEventType::Rio(RioEvent::QuitConfirmed) => {
                self.quit_now(window_id);
            }
            RioEventType::Rio(RioEvent::QuitSaveAnswered(save)) => {
                self.quit_all(Some(window_id), Some(save));
            }
            RioEventType::Rio(RioEvent::GlyphProtocolInstalled {
                route_id,
                registry,
            }) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    route
                        .window
                        .screen
                        .sugarloaf
                        .font_library()
                        .install_glyph_registry(route_id, registry);
                }
            }
            RioEventType::Rio(RioEvent::GlyphProtocolQuery { route_id, cp }) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    use rio_backend::ansi::glyph_protocol::{
                        format_query_response, QueryStatus,
                    };
                    let library = route.window.screen.sugarloaf.font_library();
                    let in_glossary = library
                        .glyph_registry_for(route_id)
                        .is_some_and(|r| r.contains(cp));
                    let in_system = library.covers_codepoint(cp);
                    let status = match (in_glossary, in_system) {
                        (true, true) => QueryStatus::Both,
                        (true, false) => QueryStatus::Glossary,
                        (false, true) => QueryStatus::System,
                        (false, false) => QueryStatus::Free,
                    };
                    let resp = format_query_response(cp, status);
                    if let Some(item) = route
                        .window
                        .screen
                        .context_manager
                        .current_grid_mut()
                        .get_by_route_id(route_id)
                    {
                        item.context_mut().messenger.send_bytes(resp.into_bytes());
                    }
                }
            }
            RioEventType::Rio(RioEvent::CloseTerminal(route_id)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .screen
                        .sugarloaf
                        .font_library()
                        .remove_glyph_registry(route_id);

                    // Drop this route's trigger dedup state so it doesn't
                    // accumulate for the window's lifetime.
                    route.window.screen.triggers.forget_route(route_id);

                    // Decide whether this exit closes the whole window
                    // WITHOUT removing the dead context yet. Removing it
                    // (as should_close_context_manager does) empties the
                    // context vec; if we then defer the close to show a
                    // prompt, the next render indexes contexts[0] on an
                    // empty vec and panics. So peek first, remove later.
                    if route
                        .window
                        .screen
                        .context_manager
                        .would_close_window(route_id)
                    {
                        // The last tab's shell exited, so this window is
                        // closing. Persist first — a shell `exit`/Ctrl+D
                        // must save the session the same as a deliberate
                        // window close, or the workspace is silently lost.
                        // In prompt mode this shows the save prompt and
                        // defers the close until it is answered; the dead
                        // context stays put so the prompt has a surface to
                        // render on, and CloseWindowConfirmed tears down
                        // the whole route once answered.
                        if !self.prompt_or_save_on_close(window_id, false) {
                            self.scheduler.unschedule_window(route_id);
                            self.teardown_closed_window(
                                event_loop, window_id, false, false,
                            );
                        }
                    } else if route
                        .window
                        .screen
                        .context_manager
                        .should_close_context_manager(
                            route_id,
                            &mut route.window.screen.sugarloaf,
                        )
                    {
                        // Race guard: would_close_window said no, but the
                        // destructive pass emptied the manager anyway (e.g.
                        // concurrent teardown). Close without prompting
                        // rather than leave an empty live window.
                        self.scheduler.unschedule_window(route_id);
                        self.teardown_closed_window(event_loop, window_id, false, false);
                    } else {
                        let size = route.window.screen.context_manager.len();
                        route.window.screen.resize_top_or_bottom_line(size);
                        // A tab just closed: indices shifted, so any armed
                        // close button or pending close-confirm now points
                        // at the wrong tab. Drop both.
                        if let Some(island) = route.window.screen.renderer.island.as_mut()
                        {
                            island.disarm();
                        }
                        route.window.screen.renderer.confirm_close.set_pending(None);
                        // Force a repaint of the post-close state. The PTY
                        // thread also queues a separate Render, but if that is
                        // processed before this CloseTerminal (or coalesced),
                        // the closed tab lingers on screen until some later
                        // event — looking "frozen" until you click another
                        // tab. mark_dirty + redraw makes the close show now.
                        route.window.screen.mark_dirty();
                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::CursorBlinkingChange) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::CursorBlinkingChangeOnRoute(route_id)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if route_id == route.window.screen.ctx().current_route() {
                        // Cursor blink toggles the cursor sprite (a
                        // separate quad), not cell content — so we
                        // signal `CursorOnly` and the GPU emit skips
                        // per-row rebuild while the cursor uniform
                        // updates downstream.
                        route
                            .window
                            .screen
                            .ctx_mut()
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_terminal_damage(
                                rio_backend::event::TerminalDamage::CursorOnly,
                            );

                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::ProgressReport(report)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(island) = &mut route.window.screen.renderer.island {
                        island.set_progress_report(report);
                        route.request_redraw();
                    }
                }
            }
            RioEventType::Rio(RioEvent::SelectionScrollTick) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.screen.selection_scroll_tick();
                    route.request_redraw();
                }
            }
            RioEventType::Rio(RioEvent::Bell) => {
                // Handle audio bell
                if self.config.bell.audio {
                    self.handle_audio_bell();
                }
            }
            RioEventType::Rio(RioEvent::DesktopNotification { title, body }) => {
                self.handle_desktop_notification(&title, &body, 1);
            }
            RioEventType::Rio(RioEvent::TriggerFired { route_id, action }) => {
                use rio_backend::event::TriggerEventAction as Action;
                match action {
                    Action::Notify {
                        title,
                        body,
                        urgency,
                    } => {
                        self.handle_desktop_notification(&title, &body, urgency);
                    }
                    Action::Run { program, args } => {
                        if let Some(route) = self.router.routes.get(&window_id) {
                            route.window.screen.exec(&program, &args);
                        }
                    }
                    Action::SendText { text } => {
                        if let Some(route) = self.router.routes.get_mut(&window_id) {
                            if let Some(item) = route
                                .window
                                .screen
                                .context_manager
                                .get_by_route_id(route_id)
                            {
                                item.val.messenger.send_bytes(text.into_bytes());
                            }
                        }
                    }
                    Action::Coprocess {
                        program,
                        args,
                        stdin,
                    } => {
                        // Capture stdout off-thread so a slow command never
                        // blocks the UI, then write it into the PTY. When
                        // `stdin` is set, the visible screen is piped to the
                        // command (small payload, no deadlock risk). The
                        // child is bounded by a deadline: a hung coprocess
                        // would otherwise leak this thread + pipes forever,
                        // and its eventual stdout would be typed into
                        // whatever runs in the pane much later.
                        const COPROCESS_DEADLINE: std::time::Duration =
                            std::time::Duration::from_secs(10);
                        let proxy = self.event_proxy.clone();
                        std::thread::spawn(move || {
                            use std::io::{Read, Write};
                            use std::process::Stdio;
                            let mut command = std::process::Command::new(&program);
                            command
                                .args(&args)
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null());
                            if stdin.is_some() {
                                command.stdin(Stdio::piped());
                            }
                            let mut child = match command.spawn() {
                                Ok(child) => child,
                                Err(err) => {
                                    tracing::warn!(
                                        "trigger coprocess {program:?} failed: {err}"
                                    );
                                    return;
                                }
                            };
                            // Write stdin on its own thread so we can drain
                            // stdout concurrently: a coprocess that emits more
                            // than one pipe buffer before reading its input
                            // would otherwise deadlock against a blocking
                            // write_all here.
                            if let Some(input) = stdin {
                                if let Some(mut pipe) = child.stdin.take() {
                                    std::thread::spawn(move || {
                                        let _ = pipe.write_all(input.as_bytes());
                                    });
                                }
                            }
                            // Drain stdout on its own thread too, so the
                            // deadline loop below never blocks on a pipe;
                            // the reader finishes when the pipe closes
                            // (child exit or kill).
                            let stdout_rx = child.stdout.take().map(|mut pipe| {
                                let (tx, rx) = std::sync::mpsc::channel();
                                std::thread::spawn(move || {
                                    let mut buf = Vec::new();
                                    let _ = pipe.read_to_end(&mut buf);
                                    let _ = tx.send(buf);
                                });
                                rx
                            });
                            let deadline = std::time::Instant::now() + COPROCESS_DEADLINE;
                            let timed_out = loop {
                                match child.try_wait() {
                                    Ok(Some(_)) => break false,
                                    Ok(None) => {}
                                    Err(err) => {
                                        tracing::warn!(
                                            "trigger coprocess {program:?} failed: {err}"
                                        );
                                        break true;
                                    }
                                }
                                if std::time::Instant::now() >= deadline {
                                    tracing::warn!(
                                        "trigger coprocess {program:?} exceeded {COPROCESS_DEADLINE:?}; killing"
                                    );
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    break true;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            };
                            if timed_out {
                                return;
                            }
                            if let Some(rx) = stdout_rx {
                                if let Ok(buf) = rx.recv() {
                                    if !buf.is_empty() {
                                        let text =
                                            String::from_utf8_lossy(&buf).into_owned();
                                        proxy.send_event(
                                            RioEvent::PtyWrite(route_id, text).into(),
                                            window_id,
                                        );
                                    }
                                }
                            }
                        });
                    }
                }
            }
            RioEventType::Rio(RioEvent::PrepareRender(millis)) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    let timer_id = TimerId::new(
                        Topic::Render,
                        route.window.screen.ctx().current_route(),
                    );
                    let event =
                        EventPayload::new(RioEventType::Rio(RioEvent::Render), window_id);

                    if !self.scheduler.scheduled(timer_id) {
                        self.scheduler.schedule(
                            event,
                            Duration::from_millis(millis),
                            false,
                            timer_id,
                        );
                    }
                }
            }
            RioEventType::Rio(RioEvent::PrepareRenderOnRoute(millis, route_id)) => {
                let timer_id = TimerId::new(Topic::ScheduledRenderRoute, route_id);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::RenderRoute(route_id)),
                    window_id,
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(millis),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::BlinkCursor(millis, route_id)) => {
                let timer_id = TimerId::new(Topic::CursorBlinking, route_id);
                let event = EventPayload::new(
                    RioEventType::Rio(RioEvent::CursorBlinkingChangeOnRoute(route_id)),
                    window_id,
                );

                if !self.scheduler.scheduled(timer_id) {
                    self.scheduler.schedule(
                        event,
                        Duration::from_millis(millis),
                        false,
                        timer_id,
                    );
                }
            }
            RioEventType::Rio(RioEvent::Title(title)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.set_window_title(&title);
                }
            }
            RioEventType::Rio(RioEvent::TitleWithSubtitle(title, subtitle)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.set_window_title(&title);
                    route.set_window_subtitle(&subtitle);
                }
            }
            RioEventType::Rio(RioEvent::UpdateTitles) => {
                self.router.update_titles();
            }
            RioEventType::Rio(RioEvent::MouseCursorDirty) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.screen.reset_mouse();
                }
            }
            RioEventType::Rio(RioEvent::Scroll(scroll)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let mut terminal = route
                        .window
                        .screen
                        .context_manager
                        .current_mut()
                        .terminal
                        .lock();
                    terminal.scroll_display(scroll);
                    drop(terminal);
                }
            }
            RioEventType::Rio(RioEvent::ClipboardLoad(
                route_id,
                clipboard_type,
                format,
            )) => {
                let Router {
                    routes, clipboard, ..
                } = &mut self.router;
                if let Some(route) = routes.get_mut(&window_id) {
                    if route.window.is_focused {
                        let text = format(clipboard.get(clipboard_type).as_str());
                        // Route the paste back to the panel that asked for it
                        // (OSC 52 reply), not whichever panel happens to be
                        // focused now.
                        if let Some(item) = route
                            .window
                            .screen
                            .context_manager
                            .get_by_route_id(route_id)
                        {
                            item.val.messenger.send_bytes(text.into_bytes());
                        }
                    }
                }
            }
            RioEventType::Rio(RioEvent::ClipboardStore(clipboard_type, content)) => {
                let Router {
                    routes, clipboard, ..
                } = &mut self.router;
                if let Some(route) = routes.get_mut(&window_id) {
                    if route.window.is_focused {
                        clipboard.set(clipboard_type, content);
                    }
                }
            }
            RioEventType::Rio(RioEvent::PtyWrite(route_id, text)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // Route reply bytes (CSI / OSC responses) back to the
                    // PTY of the panel that emitted them, not whichever
                    // panel happens to be focused.
                    if let Some(item) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    {
                        item.val.messenger.send_bytes(text.into_bytes());
                    }
                }
            }
            RioEventType::Rio(RioEvent::TextAreaSizeRequest(route_id, format)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    if let Some(item) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    {
                        let dimension = item.val.dimension;
                        let text = format(crate::renderer::utils::terminal_dimensions(
                            &dimension,
                        ));
                        item.val.messenger.send_bytes(text.into_bytes());
                    }
                }
            }
            RioEventType::Rio(RioEvent::ColorRequest(route_id, index, format)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    // Read the originating panel's terminal colors and
                    // route the reply back to that same panel — color
                    // theme overrides via OSC 4 / OSC 10-19 are
                    // per-context, so reading from `current()` would
                    // mis-report when the user has focused a different
                    // split mid-flight.
                    let renderer_color = route.window.screen.renderer.colors[index];
                    let Some(item) = route
                        .window
                        .screen
                        .context_manager
                        .get_by_route_id(route_id)
                    else {
                        return;
                    };
                    let terminal = item.val.terminal.lock();
                    let color: ColorRgb = match terminal.colors()[index] {
                        Some(color) => ColorRgb::from_color_arr(color),
                        // Ignore cursor color requests unless it was changed.
                        None if index
                            == crate::crosswords::NamedColor::Cursor as usize =>
                        {
                            return
                        }
                        None => ColorRgb::from_color_arr(renderer_color),
                    };
                    drop(terminal);

                    item.val.messenger.send_bytes(format(color).into_bytes());
                }
            }
            RioEventType::Rio(RioEvent::CreateWindow) => {
                // A window opened from a named-session window joins the
                // same workspace, so `--session NAME` with several windows
                // saves and restores them all (a fresh route defaults to
                // an unnamed session, which would otherwise be excluded
                // from the name's file). Passing the name into
                // create_window tags the new window's first daemon at
                // spawn, rather than after (which left it untagged).
                let inherit = self
                    .router
                    .routes
                    .get(&window_id)
                    .and_then(|r| r.session_name.clone());
                self.router.create_window(
                    event_loop,
                    self.event_proxy.clone(),
                    &self.config,
                    None,
                    self.app_id.as_deref(),
                    inherit,
                );
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::CreateNativeTab(working_dir_overwrite)) => {
                if let Some(route) = self.router.routes.get(&window_id) {
                    // This case happens only for native tabs
                    // every time that a new tab is created through context
                    // it also reaches for the foreground process path if
                    // config.use_current_path is true
                    // For these case we need to make a workaround
                    let config = if working_dir_overwrite.is_some() {
                        rio_backend::config::Config {
                            working_dir: working_dir_overwrite,
                            ..self.config.clone()
                        }
                    } else {
                        self.config.clone()
                    };

                    self.router.create_native_tab(
                        event_loop,
                        self.event_proxy.clone(),
                        &config,
                        Some(&route.window.winit_window.tabbing_identifier()),
                        None,
                    );
                }
            }
            RioEventType::Rio(RioEvent::CreateConfigEditor) => {
                if self.config.navigation.open_config_with_split {
                    self.router.open_config_split(&self.config);
                } else {
                    self.router.open_config_window(
                        event_loop,
                        self.event_proxy.clone(),
                        &self.config,
                    );
                }
            }
            // Cross-platform: `close_current_context` sends this on every
            // OS when the last tab of a window closes, so the handler must
            // not be macOS-gated (else Linux last-tab-close would send an
            // event nobody consumes and the window would never close).
            RioEventType::Rio(RioEvent::CloseWindow) => {
                // Save (always/named) or ask (prompt) before closing. If a
                // prompt is shown, defer the close — its answer fires
                // CloseWindowConfirmed. Otherwise close now. A window
                // whose shell still runs a program is kept resumable —
                // daemons detach and the window is pinned so the pruning
                // autosave that follows a tab-close keeps it in the file;
                // an idle window is pruned and its daemons killed.
                #[cfg(unix)]
                let busy = self
                    .router
                    .routes
                    .get(&window_id)
                    .is_some_and(Self::window_has_running_process);
                #[cfg(not(unix))]
                let busy = false;
                if busy {
                    let path = match self
                        .router
                        .routes
                        .get(&window_id)
                        .and_then(|r| r.session_name.as_deref())
                    {
                        Some(name) => rio_backend::config::session_named_path(name),
                        None => rio_backend::config::session_file_path(),
                    };
                    self.saved_windows.pin(&path, window_id);
                }
                if !self.prompt_or_save_on_close(window_id, busy) {
                    self.teardown_closed_window(event_loop, window_id, busy, false);
                }
            }
            RioEventType::Rio(RioEvent::ResumeSessionAnswered) => {
                // The resume prompt's "r": run the identical restore
                // always mode runs at launch — every saved window
                // reopens, not just the one the prompt lived in.
                if let Some(state) = self
                    .router
                    .routes
                    .get_mut(&window_id)
                    .and_then(|route| route.pending_session.take())
                {
                    self.restore_full_session(event_loop, window_id, state);
                }
            }
            RioEventType::Rio(RioEvent::CloseWindowConfirmed(save)) => {
                // The save-on-exit prompt was answered. `true` = save the
                // whole session now (all windows of the name, or the
                // implicit slot); `false` = discard the whole slot: file,
                // this window's live panes, and the daemons of windows
                // closed earlier this run that only the file still
                // references. Either way close without re-running the
                // prompt logic.
                let named = self
                    .router
                    .routes
                    .get(&window_id)
                    .and_then(|r| r.session_name.clone());
                // The close kind stashed when the prompt was shown: a
                // yes must tear down exactly like always mode would —
                // a kill-close (exit, idle close-tab) saves the OTHER
                // windows and kills this one's daemons; a keep-close
                // (WM close, busy window) saves everything and
                // detaches. Absent (stale event) defaults to keep.
                let keeps = self.pending_close_keeps.remove(&window_id).unwrap_or(true);
                if save {
                    // Consent = do exactly what always mode does for
                    // this close kind: close-save with the window still
                    // present (a last-window close leaves the file
                    // resumable), then the shared teardown — which for
                    // a kill-close prunes the window from the file and
                    // for a keep-close detaches its daemons.
                    self.save_session_now(window_id, named);
                    self.teardown_closed_window(event_loop, window_id, keeps, true);
                } else {
                    let path = match &named {
                        Some(name) => rio_backend::config::session_named_path(name),
                        None => rio_backend::config::session_file_path(),
                    };
                    self.decline_session(&path, &[window_id]);
                    self.close_window_now(event_loop, window_id, true);
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabByIndex(tab_index)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_tab_at_index(tab_index);
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabLast) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route
                        .window
                        .winit_window
                        .select_tab_at_index(route.window.winit_window.num_tabs() - 1);
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabNext) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_next_tab();
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::SelectNativeTabPrev) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.select_previous_tab();
                }
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::Hide) => {
                event_loop.hide_application();
            }
            #[cfg(target_os = "macos")]
            RioEventType::Rio(RioEvent::HideOtherApplications) => {
                event_loop.hide_other_applications();
            }
            RioEventType::Rio(RioEvent::Minimize(set_minimize)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    route.window.winit_window.set_minimized(set_minimize);
                }
            }
            RioEventType::Rio(RioEvent::ToggleFullScreen) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    match route.window.winit_window.fullscreen() {
                        None => route
                            .window
                            .winit_window
                            .set_fullscreen(Some(Fullscreen::Borderless(None))),
                        _ => route.window.winit_window.set_fullscreen(None),
                    }
                }
            }
            RioEventType::Rio(RioEvent::ToggleMaximized) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let maximized = route.window.winit_window.is_maximized();
                    route.window.winit_window.set_maximized(!maximized);
                }
            }
            RioEventType::Rio(RioEvent::ToggleAppearanceTheme) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    use rio_backend::config::theme::AppearanceTheme;
                    let current = self
                        .config
                        .force_theme
                        .or_else(|| {
                            route
                                .window
                                .winit_window
                                .theme()
                                .map(AppearanceTheme::from_window_theme)
                        })
                        .unwrap_or(AppearanceTheme::Dark);
                    let toggled = current.toggled();
                    self.config.force_theme = Some(toggled);
                    update_colors_based_on_theme(
                        &mut self.config,
                        Some(toggled.to_window_theme()),
                    );
                    route.window.screen.update_config(
                        &self.config,
                        &self.router.font_library,
                        false,
                    );
                    route.window.configure_window(&self.config);
                }
            }
            RioEventType::Rio(RioEvent::ColorChange(route_id, index, color)) => {
                if let Some(route) = self.router.routes.get_mut(&window_id) {
                    let screen = &mut route.window.screen;
                    // Background color is index 1 relative to NamedColor::Foreground
                    if index == NamedColor::Foreground as usize + 1 {
                        let grid = screen.context_manager.current_grid_mut();
                        // The event carries a `route_id: usize` (global
                        // counter). `ContextGrid::get_mut` is keyed on
                        // taffy `NodeId` — a different identifier space,
                        // so `get_mut(route_id.into())` effectively
                        // never matches. Look the panel up by its
                        // actual route id.
                        if let Some(context_item) = grid.get_by_route_id(route_id) {
                            use crate::context::renderable::BackgroundState;
                            context_item.context_mut().renderable_content.background =
                                Some(match color {
                                    Some(c) => BackgroundState::Set(c.to_wgpu()),
                                    None => BackgroundState::Reset,
                                });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    fn open_urls(&mut self, active_event_loop: &ActiveEventLoop, urls: Vec<String>) {
        if !self.config.navigation.is_native() {
            let config = &self.config;
            for url in urls {
                self.router.create_window(
                    active_event_loop,
                    self.event_proxy.clone(),
                    config,
                    Some(url),
                    self.app_id.as_deref(),
                    None,
                );
            }
            return;
        }

        let mut tab_id = None;

        // In case only have one window
        for (_, route) in self.router.routes.iter() {
            if tab_id.is_none() {
                tab_id = Some(route.window.winit_window.tabbing_identifier());
            }

            if route.window.is_focused {
                tab_id = Some(route.window.winit_window.tabbing_identifier());
                break;
            }
        }

        if tab_id.is_some() {
            let config = &self.config;
            for url in urls {
                self.router.create_native_tab(
                    active_event_loop,
                    self.event_proxy.clone(),
                    config,
                    tab_id.as_deref(),
                    Some(url),
                );
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        // Ignore all events we do not care about.
        if Self::skip_window_event(&event) {
            return;
        }

        let route = match self.router.routes.get_mut(&window_id) {
            Some(window) => window,
            None => return,
        };

        match event {
            WindowEvent::CloseRequested => {
                // macOS: Cmd+Q quit confirmation is handled by
                // `applicationShouldTerminate` in rio-window.
                // Windows: per-window close confirmation is handled
                // by `MessageBoxW` in rio-window's WM_CLOSE handler
                // (see `set_confirm_before_quit` plumbing).
                // Either way, by the time we see `CloseRequested`
                // the user has already confirmed — just close.
                if cfg!(any(target_os = "macos", target_os = "windows")) {
                    // Save (always/named) or prompt (prompt mode); a
                    // shown prompt defers the close to its answer.
                    if !self.prompt_or_save_on_close(window_id, true) {
                        self.teardown_closed_window(event_loop, window_id, true, false);
                    }
                    return;
                }

                if self.config.confirm_before_quit {
                    route.confirm_quit();
                } else if !self.prompt_or_save_on_close(window_id, true) {
                    self.teardown_closed_window(event_loop, window_id, true, false);
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                route.window.screen.set_modifiers(modifiers);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                    || route.window.screen.renderer.session_prompt.is_active()
                    || route.window.screen.renderer.confirm_close.is_active()
                {
                    #[cfg(target_os = "macos")]
                    if state == ElementState::Pressed
                        && button == MouseButton::Left
                        && route.window.screen.allow_manual_dragging
                    {
                        let tab_bar_height =
                            route.window.screen.renderer.navigation.tab_bar_height;
                        let scale = route.window.screen.sugarloaf.scale_factor();
                        if route.window.screen.mouse.y <= (tab_bar_height * scale) as f64
                        {
                            let _ = route.window.winit_window.drag_window();
                        }
                    }
                    if state == ElementState::Pressed {
                        let _ = route.window.screen.take_chrome_press();
                    } else if state == ElementState::Released
                        && button == MouseButton::Left
                    {
                        route.window.screen.mouse.left_button_state =
                            ElementState::Released;
                        if let Some(ref mut island) = route.window.screen.renderer.island
                        {
                            island.cancel_drag();
                        }
                        route.window.screen.renderer.scrollbar.end_drag();
                        route.window.screen.resize_state = None;
                    }
                    return;
                }

                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                match button {
                    MouseButton::Left => {
                        route.window.screen.mouse.left_button_state = state
                    }
                    MouseButton::Middle => {
                        route.window.screen.mouse.middle_button_state = state
                    }
                    MouseButton::Right => {
                        route.window.screen.mouse.right_button_state = state
                    }
                    _ => (),
                }

                match state {
                    ElementState::Pressed => {
                        // Calculate time since the last click to handle double/triple clicks.
                        // Do this early so island clicks can use the click state
                        let now = Instant::now();
                        let elapsed =
                            now - route.window.screen.mouse.last_click_timestamp;
                        route.window.screen.mouse.last_click_timestamp = now;

                        let threshold = crate::constants::MULTI_CLICK_THRESHOLD;
                        let mouse = &route.window.screen.mouse;
                        route.window.screen.mouse.click_state = match mouse.click_state {
                            // Reset click state if button has changed.
                            _ if button != mouse.last_click_button => {
                                route.window.screen.mouse.last_click_button = button;
                                ClickState::Click
                            }
                            ClickState::Click if elapsed < threshold => {
                                ClickState::DoubleClick
                            }
                            ClickState::DoubleClick if elapsed < threshold => {
                                ClickState::TripleClick
                            }
                            _ => ClickState::Click,
                        };

                        let chrome_press = route.window.screen.take_chrome_press();

                        if let MouseButton::Left = button {
                            // Check if clicking on a panel border to start resize
                            {
                                let mx = route.window.screen.mouse.x as f32;
                                let my = route.window.screen.mouse.y as f32;
                                let grid =
                                    route.window.screen.context_manager.current_grid();
                                if let Some(border) = grid.find_border_at_position(mx, my)
                                {
                                    let start_pos = match border.direction {
                                        crate::layout::BorderDirection::Vertical => mx,
                                        crate::layout::BorderDirection::Horizontal => my,
                                    };
                                    let size_a = grid.get_panel_size(
                                        border.left_or_top,
                                        border.direction,
                                    );
                                    let size_b = grid.get_panel_size(
                                        border.right_or_bottom,
                                        border.direction,
                                    );
                                    route.window.screen.resize_state =
                                        Some(crate::layout::ResizeState {
                                            border,
                                            start_pos,
                                            original_sizes: (size_a, size_b),
                                        });
                                    return;
                                }
                            }

                            if route.window.screen.handle_assistant_click() {
                                route.request_redraw();
                                return;
                            }

                            if route
                                .window
                                .screen
                                .handle_palette_click(&mut self.router.clipboard)
                            {
                                route.request_redraw();
                                return;
                            }

                            if route
                                .window
                                .screen
                                .handle_search_click(&mut self.router.clipboard)
                            {
                                route.request_redraw();
                                return;
                            }

                            let handled_by_island =
                                route.window.screen.handle_island_click(
                                    &route.window.winit_window,
                                    &mut self.router.clipboard,
                                    false,
                                    chrome_press,
                                );

                            if handled_by_island {
                                route.request_redraw();
                                return;
                            }

                            #[cfg(target_os = "macos")]
                            if route.window.screen.allow_manual_dragging {
                                let tab_bar_height = route
                                    .window
                                    .screen
                                    .renderer
                                    .navigation
                                    .tab_bar_height;
                                let scale = route.window.screen.sugarloaf.scale_factor();
                                if route.window.screen.mouse.y
                                    <= (tab_bar_height * scale) as f64
                                {
                                    route
                                        .window
                                        .screen
                                        .start_window_drag(&route.window.winit_window);
                                }
                            }

                            if route.window.screen.handle_scrollbar_click() {
                                route.request_redraw();
                                return;
                            }
                        } else if let MouseButton::Right = button {
                            let handled_by_island =
                                route.window.screen.handle_island_click(
                                    &route.window.winit_window,
                                    &mut self.router.clipboard,
                                    true,
                                    chrome_press,
                                );

                            if handled_by_island {
                                route.request_redraw();
                                return;
                            }
                        }

                        // Always try panel switching first: if the click
                        // targets a different panel, switch to it regardless
                        // of mouse mode (e.g. neovim capturing clicks).
                        if route.window.screen.select_current_based_on_mouse() {
                            route.request_redraw();
                        } else if !route.window.screen.modifiers.state().shift_key()
                            && route.window.screen.mouse_mode()
                        {
                            // Process mouse press before bindings to update the `click_state`.
                            route.window.screen.mouse.click_state = ClickState::None;

                            let code = match button {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                                // Can't properly report more than three buttons..
                                MouseButton::Back
                                | MouseButton::Forward
                                | MouseButton::Other(_) => return,
                            };

                            route
                                .window
                                .screen
                                .mouse_report(code, ElementState::Pressed);

                            route.window.screen.process_mouse_bindings(
                                button,
                                &mut self.router.clipboard,
                            );
                        } else {
                            if route.window.screen.trigger_hyperlink() {
                                return;
                            }

                            // Load mouse point, treating message bar and padding as the closest square.
                            let display_offset = route.window.screen.display_offset();

                            if let MouseButton::Left = button {
                                let pos =
                                    route.window.screen.mouse_position(display_offset);
                                route
                                    .window
                                    .screen
                                    .on_left_click(pos, &mut self.router.clipboard);
                            }

                            route.request_redraw();
                        }
                        route
                            .window
                            .screen
                            .process_mouse_bindings(button, &mut self.router.clipboard);
                    }
                    ElementState::Released => {
                        // Stop selection auto-scroll on button release.
                        if let MouseButton::Left | MouseButton::Right = button {
                            let scroll_timer_id =
                                route.window.screen.ctx().current_route();
                            let timer_id =
                                TimerId::new(Topic::SelectionScrolling, scroll_timer_id);
                            self.scheduler.unschedule(timer_id);
                        }

                        if button == MouseButton::Left
                            && route
                                .window
                                .screen
                                .renderer
                                .island
                                .as_ref()
                                .is_some_and(|i| i.is_dragging())
                        {
                            let started = route.window.screen.handle_tab_drag_release();
                            if started {
                                route.request_redraw();
                                return;
                            }
                        }

                        if route.window.screen.renderer.scrollbar.is_dragging() {
                            route.window.screen.handle_scrollbar_release();
                            route.request_redraw();
                            return;
                        }

                        if route.window.screen.resize_state.is_some() {
                            route.window.screen.resize_state = None;
                            route.window.winit_window.set_cursor(CursorIcon::Default);
                            return;
                        }

                        if !route.window.screen.modifiers.state().shift_key()
                            && route.window.screen.mouse_mode()
                        {
                            let code = match button {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                                // Can't properly report more than three buttons.
                                MouseButton::Back
                                | MouseButton::Forward
                                | MouseButton::Other(_) => return,
                            };
                            route
                                .window
                                .screen
                                .mouse_report(code, ElementState::Released);
                            return;
                        }

                        // Trigger hints highlighted by the mouse
                        if button == MouseButton::Left
                            && route
                                .window
                                .screen
                                .trigger_hint(&mut self.router.clipboard)
                        {
                            return;
                        }

                        if let MouseButton::Left | MouseButton::Right = button {
                            // Always mirror the selection into the primary
                            // buffer so middle-click paste works (standard
                            // X11/Wayland behavior). copy_selection is a
                            // no-op when the selection is empty.
                            route.window.screen.copy_selection(
                                ClipboardType::Selection,
                                &mut self.router.clipboard,
                            );
                            if self.config.copy_on_select {
                                route.window.screen.copy_selection(
                                    ClipboardType::Clipboard,
                                    &mut self.router.clipboard,
                                );
                            }
                        }
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                if route.window.screen.clear_close_button_hover() {
                    route.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                let layout = route.window.screen.sugarloaf.window_size();

                // Keep f64 precision all the way to the cell-grid
                // divide. The old `as usize` cast here dropped
                // subpixel info from HiDPI events.
                let x = position.x.clamp(0.0, (layout.width as i32 - 1) as f64);
                let y = position.y.clamp(0.0, (layout.height as i32 - 1) as f64);

                route.window.screen.mouse.x = x;
                route.window.screen.mouse.y = y;
                route.window.screen.mouse.raw_y = position.y;

                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                    || route.window.screen.renderer.session_prompt.is_active()
                    || route.window.screen.renderer.confirm_close.is_active()
                {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle assistant overlay hover
                if route.window.screen.renderer.assistant.is_active() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let win_w = route.window.screen.sugarloaf.window_size().width;
                    let mx = x as f32 / scale;
                    let my = y as f32 / scale;
                    if route
                        .window
                        .screen
                        .renderer
                        .assistant
                        .hover(mx, my, win_w, scale)
                    {
                        route.request_overlay_redraw();
                    }

                    if route
                        .window
                        .screen
                        .renderer
                        .assistant
                        .hovered_button()
                        .is_some()
                    {
                        route.window.winit_window.set_cursor(CursorIcon::Pointer);
                    } else {
                        route.window.winit_window.set_cursor(CursorIcon::Default);
                    }
                    return;
                }

                // Handle command palette hover
                if route.window.screen.renderer.command_palette.is_enabled() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let win_w = route.window.screen.sugarloaf.window_size().width;
                    let mx = x as f32 / scale;
                    let my = y as f32 / scale;
                    if route
                        .window
                        .screen
                        .renderer
                        .command_palette
                        .hover(mx, my, win_w, scale)
                    {
                        route.request_overlay_redraw();
                    }
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle search overlay hover
                if route.window.screen.renderer.search.is_active() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let win_w = route.window.screen.sugarloaf.window_size().width;
                    let mx = x as f32 / scale;
                    let my = y as f32 / scale;
                    if route
                        .window
                        .screen
                        .renderer
                        .search
                        .hover(mx, my, win_w, scale)
                    {
                        // UI-only change (hover highlight). `set_dirty`
                        // passes `Renderer::run`'s per-context gate;
                        // the inner damage match hits
                        // `(None, None) => TerminalDamage::Noop` so
                        // no rows rebuild. The search overlay itself
                        // is drawn unconditionally after the per-context
                        // loop in `Renderer::run`.
                        route
                            .window
                            .screen
                            .ctx_mut()
                            .current_mut()
                            .renderable_content
                            .pending_update
                            .set_dirty();
                        route.request_redraw();
                    }
                }

                if route.window.screen.mouse.left_button_state == ElementState::Pressed
                    && route
                        .window
                        .screen
                        .renderer
                        .island
                        .as_ref()
                        .is_some_and(|i| i.is_dragging())
                {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    route.window.screen.handle_tab_drag_move(x as f32 / scale);
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    route.request_redraw();
                    return;
                }

                if route.window.screen.update_close_button_hover(x, y) {
                    route.request_redraw();
                }

                // Only force the default cursor while the island is
                // visible — when it's hidden (hide_if_single + single
                // tab on macOS) the band at the top has no tabs to
                // hover, and the I-beam from the terminal grid below
                // should stay during top-edge drags.
                let scale_factor = route.window.screen.sugarloaf.scale_factor();
                let island_height_px =
                    (route.window.screen.renderer.navigation.tab_bar_height
                        * scale_factor) as f64;
                let num_tabs = route.window.screen.ctx().len();
                let nav = &route.window.screen.renderer.navigation;
                if nav.island_visible(num_tabs) && y <= island_height_px {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Handle scrollbar drag
                if route.window.screen.renderer.scrollbar.is_dragging() {
                    let scale = route.window.screen.sugarloaf.scale_factor();
                    let mouse_y = y as f32 / scale;
                    route.window.screen.handle_scrollbar_drag(mouse_y);
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    route.request_redraw();
                    return;
                }

                // Handle panel border resize
                if route.window.screen.resize_state.is_some() {
                    let state = route.window.screen.resize_state.unwrap();
                    let current_pos = match state.border.direction {
                        crate::layout::BorderDirection::Vertical => x as f32,
                        crate::layout::BorderDirection::Horizontal => y as f32,
                    };
                    let delta = current_pos - state.start_pos;
                    let border = state.border;
                    let original_sizes = state.original_sizes;
                    route
                        .window
                        .screen
                        .context_manager
                        .current_grid_mut()
                        .resize_border(
                            &border,
                            original_sizes,
                            delta,
                            &mut route.window.screen.sugarloaf,
                        );
                    let cursor = match border.direction {
                        crate::layout::BorderDirection::Vertical => CursorIcon::ColResize,
                        crate::layout::BorderDirection::Horizontal => {
                            CursorIcon::RowResize
                        }
                    };
                    route.window.winit_window.set_cursor(cursor);
                    route.window.screen.context_manager.request_render();
                    route.request_redraw();
                    return;
                }

                // Check if hovering over a panel border
                {
                    let grid = route.window.screen.context_manager.current_grid();
                    if let Some(border) = grid.find_border_at_position(x as f32, y as f32)
                    {
                        let cursor = match border.direction {
                            crate::layout::BorderDirection::Vertical => {
                                CursorIcon::ColResize
                            }
                            crate::layout::BorderDirection::Horizontal => {
                                CursorIcon::RowResize
                            }
                        };
                        route.window.winit_window.set_cursor(cursor);
                        route.window.screen.mouse.on_border = true;
                        return;
                    }
                }

                // Check if hovering over scrollbar
                if route.window.screen.is_hovering_scrollbar() {
                    route.window.winit_window.set_cursor(CursorIcon::Default);
                    return;
                }

                // Track leaving a border to force cursor reset below
                let was_on_border = route.window.screen.mouse.on_border;
                route.window.screen.mouse.on_border = false;

                let lmb_pressed =
                    route.window.screen.mouse.left_button_state == ElementState::Pressed;
                let rmb_pressed =
                    route.window.screen.mouse.right_button_state == ElementState::Pressed;

                let has_selection = !route.window.screen.selection_is_empty();
                if has_selection && (lmb_pressed || rmb_pressed) {
                    // Only start the timer when the mouse enters the scroll
                    // zone. Once running, the tick reads mouse.raw_y each
                    // iteration so it keeps scrolling after CursorMoved
                    // stops (mouse left window). Cancelled on button release.
                    let delta = route.window.screen.selection_scroll_delta(position.y);
                    if delta != 0 {
                        let scroll_timer_id = route.window.screen.ctx().current_route();
                        let timer_id =
                            TimerId::new(Topic::SelectionScrolling, scroll_timer_id);
                        if !self.scheduler.scheduled(timer_id) {
                            let event = EventPayload::new(
                                RioEventType::Rio(RioEvent::SelectionScrollTick),
                                window_id,
                            );
                            self.scheduler.schedule(
                                event,
                                Duration::from_millis(15),
                                true,
                                timer_id,
                            );
                        }
                    }
                }

                let display_offset = route.window.screen.display_offset();
                let point = route.window.screen.mouse_position(display_offset);

                // Compare *cell* coordinates, not pixel coordinates, so
                // subpixel HiDPI jitter inside the same cell doesn't
                // re-fire hint / OSC-8 / hyperlink work every event.
                let prev_cell = route.window.screen.mouse.last_cell;
                let cell_changed = prev_cell != Some(point);
                route.window.screen.mouse.last_cell = Some(point);

                let inside_text_area = route.window.screen.contains_point(x, y);
                let square_side = route.window.screen.side_by_pos(x);

                // If the cursor hasn't changed cells, do nothing.
                // Force update when transitioning off a border so the cursor resets.
                if !cell_changed
                    && !was_on_border
                    && route.window.screen.mouse.square_side == square_side
                    && route.window.screen.mouse.inside_text_area == inside_text_area
                {
                    return;
                }

                // Skip hint/hyperlink highlighting during active selection
                // drag to avoid unnecessary terminal locks and regex matching.
                let is_selecting = (lmb_pressed || rmb_pressed)
                    && (route.window.screen.modifiers.state().shift_key()
                        || !route.window.screen.mouse_mode());

                if !is_selecting && route.window.screen.update_highlighted_hints() {
                    route.window.winit_window.set_cursor(CursorIcon::Pointer);
                    route.window.screen.context_manager.request_render();
                } else if !is_selecting {
                    let cursor_icon =
                        if !route.window.screen.modifiers.state().shift_key()
                            && route.window.screen.mouse_mode()
                        {
                            CursorIcon::Default
                        } else {
                            CursorIcon::Text
                        };

                    route.window.winit_window.set_cursor(cursor_icon);

                    // In case hyperlink range has cleaned trigger one more render
                    if route
                        .window
                        .screen
                        .context_manager
                        .current()
                        .has_hyperlink_range()
                    {
                        route
                            .window
                            .screen
                            .context_manager
                            .current_mut()
                            .set_hyperlink_range(None);
                        route.window.screen.context_manager.request_render();
                    }
                }

                route.window.screen.mouse.inside_text_area = inside_text_area;
                route.window.screen.mouse.square_side = square_side;

                if is_selecting {
                    route.window.screen.update_selection(point, square_side);
                    route.window.screen.context_manager.request_render();
                } else if cell_changed && route.window.screen.has_mouse_motion_and_drag()
                {
                    if lmb_pressed {
                        route.window.screen.mouse_report(32, ElementState::Pressed);
                    } else if route.window.screen.mouse.middle_button_state
                        == ElementState::Pressed
                    {
                        route.window.screen.mouse_report(33, ElementState::Pressed);
                    } else if route.window.screen.mouse.right_button_state
                        == ElementState::Pressed
                    {
                        route.window.screen.mouse_report(34, ElementState::Pressed);
                    } else if route.window.screen.has_mouse_motion() {
                        route.window.screen.mouse_report(35, ElementState::Pressed);
                    }
                }
            }

            WindowEvent::MouseWheel { delta, phase, .. } => {
                if route.path != RoutePath::Terminal
                    || route.window.screen.renderer.confirm_quit.is_active()
                    || route.window.screen.renderer.session_prompt.is_active()
                    || route.window.screen.renderer.confirm_close.is_active()
                {
                    return;
                }

                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                match delta {
                    MouseScrollDelta::LineDelta(columns, lines) => {
                        let font_size =
                            route.window.screen.ctx().current().dimension.font_size;
                        if font_size > 0.0 {
                            let new_scroll_px_x = columns * font_size;
                            let new_scroll_px_y = lines * font_size;
                            route
                                .window
                                .screen
                                .scroll(new_scroll_px_x as f64, new_scroll_px_y as f64);
                        }
                    }
                    MouseScrollDelta::PixelDelta(mut lpos) => {
                        match phase {
                            TouchPhase::Started => {
                                // Reset offset to zero.
                                route.window.screen.mouse.accumulated_scroll =
                                    Default::default();
                            }
                            TouchPhase::Moved => {
                                // When the angle between (x, 0) and (x, y) is lower than ~25 degrees
                                // (cosine is larger that 0.9) we consider this scrolling as horizontal.
                                if lpos.x.abs() / lpos.x.hypot(lpos.y) > 0.9 {
                                    lpos.y = 0.;
                                } else {
                                    lpos.x = 0.;
                                }

                                route.window.screen.scroll(lpos.x, lpos.y);
                            }
                            _ => (),
                        }
                    }
                }

                route.request_redraw();
            }

            WindowEvent::KeyboardInput {
                is_synthetic: false,
                event: key_event,
                ..
            } => {
                if route.has_key_wait(&key_event, &mut self.router.clipboard) {
                    if route.path != RoutePath::Terminal
                        && key_event.state == ElementState::Released
                    {
                        // Scheduler must be cleaned after leave the terminal route
                        self.scheduler.unschedule(TimerId::new(
                            Topic::Render,
                            route.window.screen.ctx().current_route(),
                        ));
                    }
                    return;
                }

                route.window.screen.context_manager.set_last_typing();
                route
                    .window
                    .screen
                    .process_key_event(&key_event, &mut self.router.clipboard);
                // `process_key_event` used to call `self.render()` for
                // local-only keystrokes (VI mode, search input, hint
                // mode). Now it just marks `pending_update.set_dirty()`
                // through `mark_dirty`. Request a redraw so the next
                // vsync fires `RedrawRequested` — PTY-bound keystrokes
                // also flow through here but their render is idempotent
                // with the PTY-damage-driven redraw.
                route.request_redraw();

                if key_event.state == ElementState::Released
                    && self.config.hide_cursor_when_typing
                {
                    route.window.winit_window.set_cursor_visible(false);
                }
            }

            WindowEvent::Ime(ime) => {
                if route.window.screen.renderer.assistant.is_active() {
                    return;
                }

                match ime {
                    Ime::Commit(text) => {
                        // Don't use bracketed paste for single char input.
                        route.window.screen.paste(&text, text.chars().count() > 1);
                    }
                    Ime::Preedit(text, cursor_offset) => {
                        let preedit = if text.is_empty() {
                            None
                        } else {
                            Some(Preedit::new(text, cursor_offset.map(|offset| offset.0)))
                        };

                        if route.window.screen.context_manager.current().ime.preedit()
                            != preedit.as_ref()
                        {
                            route
                                .window
                                .screen
                                .context_manager
                                .current_mut()
                                .ime
                                .set_preedit(preedit);
                            route.request_redraw();
                        }
                    }
                    Ime::Enabled => {
                        route
                            .window
                            .screen
                            .context_manager
                            .current_mut()
                            .ime
                            .set_enabled(true);
                    }
                    Ime::Disabled => {
                        route
                            .window
                            .screen
                            .context_manager
                            .current_mut()
                            .ime
                            .set_enabled(false);
                    }
                }
            }
            WindowEvent::Touch(touch) => {
                on_touch(route, touch, &mut self.router.clipboard);
            }

            WindowEvent::Focused(focused) => {
                if self.config.hide_cursor_when_typing {
                    route.window.winit_window.set_cursor_visible(true);
                }

                let focus_changed = route.window.is_focused != focused;
                route.window.is_focused = focused;

                if focus_changed {
                    route.request_redraw();
                }

                route.window.screen.on_focus_change(focused);
            }

            WindowEvent::Occluded(occluded) => {
                let was_occluded = route.window.is_occluded;
                route.window.is_occluded = occluded;

                // If window was occluded and is now visible, mark for one-time render
                if was_occluded && !occluded {
                    route.window.needs_render_after_occlusion = true;
                }
            }

            WindowEvent::ThemeChanged(new_theme) => {
                if self.config.force_theme.is_some() {
                    return;
                }
                update_colors_based_on_theme(&mut self.config, Some(new_theme));
                route.window.screen.update_config(
                    &self.config,
                    &self.router.font_library,
                    false,
                );
                route.window.configure_window(&self.config);
                route.request_redraw();
            }

            WindowEvent::DroppedFile(path) => {
                if route.window.screen.renderer.assistant.is_active() {
                    return;
                }

                let path: String = path.to_string_lossy().into();
                route.window.screen.paste(&(path + " "), true);
            }

            WindowEvent::Resized(new_size) => {
                if new_size.width == 0 || new_size.height == 0 {
                    return;
                }

                route.window.screen.resize(new_size);
                route.request_redraw();
            }

            WindowEvent::ScaleFactorChanged {
                inner_size_writer: _,
                scale_factor,
            } => {
                let scale = scale_factor as f32;
                route
                    .window
                    .screen
                    .set_scale(scale, route.window.winit_window.inner_size());
                route.window.update_vblank_interval();
                route.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                route.begin_render();

                match route.path {
                    RoutePath::Welcome => {
                        route.window.screen.render_welcome();
                    }
                    RoutePath::Terminal => {
                        if let Some(window_update) = route.window.screen.render() {
                            use crate::context::renderable::{
                                BackgroundState, WindowUpdate,
                            };
                            match window_update {
                                WindowUpdate::Background(bg_state) => {
                                    // for now setting this as allowed because it fails on linux builds
                                    #[allow(unused_variables)]
                                    let bg_color = match bg_state {
                                        BackgroundState::Set(color) => color,
                                        BackgroundState::Reset => {
                                            self.config.colors.background.1
                                        }
                                    };

                                    #[cfg(target_os = "macos")]
                                    {
                                        route.window.winit_window.set_background_color(
                                            bg_color.r, bg_color.g, bg_color.b,
                                            bg_color.a,
                                        );
                                    }

                                    #[cfg(target_os = "windows")]
                                    {
                                        use rio_window::platform::windows::WindowExtWindows;
                                        route
                                            .window
                                            .winit_window
                                            .set_title_bar_background_color(
                                                bg_color.r, bg_color.g, bg_color.b,
                                                bg_color.a,
                                            );
                                    }
                                }
                            }
                        }

                        // Update IME cursor position after rendering to ensure it's current
                        route.window.screen.update_ime_cursor_position_if_needed(
                            &route.window.winit_window,
                        );
                    }
                }

                // let duration = start.elapsed();
                // println!("Time elapsed in render() is: {:?}", duration);
                // }

                // Game mode = unlocked framerate, so keep the event loop
                // spinning. Every other case is vsync-paced: a
                // `request_redraw` tells winit to deliver
                // `RedrawRequested` at the next platform vsync, and the
                // OS parks the thread until that event arrives. Busy-
                // polling between vsyncs here would burn CPU without
                // delivering more frames.
                if self.config.renderer.strategy.is_game() {
                    route.request_redraw();
                    event_loop.set_control_flow(ControlFlow::Poll);
                } else {
                    if route
                        .window
                        .screen
                        .ctx()
                        .current()
                        .renderable_content
                        .pending_update
                        .is_dirty()
                    {
                        route.request_redraw();
                    }
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let control_flow = match self.scheduler.update() {
            Some(instant) => ControlFlow::WaitUntil(instant),
            None => ControlFlow::Wait,
        };
        event_loop.set_control_flow(control_flow);
    }

    fn open_config(&mut self, event_loop: &ActiveEventLoop) {
        if self.config.navigation.open_config_with_split {
            self.router.open_config_split(&self.config);
        } else {
            self.router.open_config_window(
                event_loop,
                self.event_proxy.clone(),
                &self.config,
            );
        }
    }

    fn hook_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        key: &rio_window::event::KeyEvent,
        modifiers: &rio_window::event::Modifiers,
    ) {
        let window_id = match self.router.get_focused_route() {
            Some(window_id) => window_id,
            None => return,
        };

        let route = match self.router.routes.get_mut(&window_id) {
            Some(window) => window,
            None => return,
        };

        // For menu-triggered events, we need to temporarily set the correct modifiers
        // since menu events don't trigger ModifiersChanged events.
        let original_modifiers = route.window.screen.modifiers;

        // Use the modifiers passed from the menu action
        route.window.screen.set_modifiers(*modifiers);

        // Process the key event
        route
            .window
            .screen
            .process_key_event(key, &mut self.router.clipboard);

        // Restore the original modifiers
        route.window.screen.set_modifiers(original_modifiers);
    }

    // Emitted when the event loop is being shut down.
    // This is irreversible - if this event is emitted, it is guaranteed to be the last event that gets emitted.
    // You generally want to treat this as an “do on quit” event.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Quitting must detach persistent panes, not kill them: the
        // flag flips Context::drop from Kill to Shutdown for ptyd
        // panes before the routes (and their contexts) are dropped.
        #[cfg(unix)]
        crate::context::set_quit_detaching();

        // Only a Save disposition writes on exit without an explicit
        // yes; Prompt must never silently overwrite a session — named or
        // implicit — because the SaveOnExit/SaveOnQuit overlays already
        // handled its consent (each per-window yes wrote through the
        // accumulator). Use the shared all-windows + merge_kept_daemons
        // writer so no window is dropped.
        self.save_named_sessions(false);
        if crate::session::write_on_quit(false, self.config.session.restore, false) {
            let focused = self.router.get_focused_route();
            self.save_last_session(focused);
        }

        // Ensure that all the windows are dropped, so the destructors for
        // Renderer and contexts ran.
        self.router.routes.clear();

        // SAFETY: The clipboard must be dropped before the event loop, so
        // replace it with a safe no-op placeholder.
        self.router.clipboard = Clipboard::new_nop();

        std::process::exit(0);
    }
}

#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
fn play_bell_sound() -> Result<(), Box<dyn Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No output device available")?;

    let config = device.default_output_config()?;

    match config.sample_format() {
        cpal::SampleFormat::F32 => run_bell::<f32>(&device, &config.into()),
        cpal::SampleFormat::I16 => run_bell::<i16>(&device, &config.into()),
        cpal::SampleFormat::U16 => run_bell::<u16>(&device, &config.into()),
        _ => Err("Unsupported sample format".into()),
    }
}

#[cfg(all(
    feature = "audio",
    not(target_os = "macos"),
    not(target_os = "windows")
))]
fn run_bell<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
) -> Result<(), Box<dyn Error>>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    let sample_rate = config.sample_rate.0 as f32;
    let channels = config.channels as usize;
    let duration_secs = crate::constants::BELL_DURATION.as_secs_f32();
    let total_samples = (sample_rate * duration_secs) as usize;

    let mut sample_clock = 0f32;
    let mut samples_played = 0usize;

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                if samples_played >= total_samples {
                    for sample in frame.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                } else {
                    let value = (sample_clock * 440.0 * 2.0 * std::f32::consts::PI
                        / sample_rate)
                        .sin()
                        * 0.2;
                    for sample in frame.iter_mut() {
                        *sample = T::from_sample(value);
                    }
                    sample_clock += 1.0;
                    samples_played += 1;
                }
            }
        },
        |err| tracing::error!("Audio stream error: {}", err),
        None,
    )?;

    stream.play()?;
    std::thread::sleep(crate::constants::BELL_DURATION);

    Ok(())
}
