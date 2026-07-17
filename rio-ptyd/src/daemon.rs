//! The per-pane daemon: owns the PTY master, the replay ring, and the
//! pane's unix socket; serves at most one attached client at a time.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::protocol::{self, Decoder, FrameType, ServerHello};
use crate::pty::{self, PtyChild, SpawnSpec};
use crate::ring::ReplayRing;
use crate::sockdir::{self, PaneMeta};

const PTY_INBUF_CAP: usize = 1024 * 1024;
const CLIENT_OUTBUF_CAP: usize = 8 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;

static mut SIGNAL_PIPE_W: RawFd = -1;

extern "C" fn signal_handler(_sig: libc::c_int) {
    unsafe {
        if SIGNAL_PIPE_W >= 0 {
            let b = [0u8; 1];
            libc::write(SIGNAL_PIPE_W, b.as_ptr() as *const _, 1);
        }
    }
}

pub struct SpawnArgs {
    pub pane_id: Option<String>,
    pub cwd: Option<String>,
    pub ring_size: usize,
    pub env: Vec<(String, String)>,
    pub session: Option<String>,
    pub program: String,
    pub args: Vec<String>,
}

/// Foreground half of `rio-ptyd spawn`: prepare socket + daemonize,
/// print the handshake JSON line from the daemon, exit. Never returns
/// in the daemon branch.
pub fn spawn(args: SpawnArgs) -> io::Result<()> {
    let base = sockdir::base_dir()?;
    let pane_id = match &args.pane_id {
        Some(id) if sockdir::is_valid_pane_id(id) => id.clone(),
        Some(_) => return Err(io::Error::other("invalid --pane-id (want 32 hex)")),
        None => sockdir::new_pane_id()?,
    };
    let sock_path = sockdir::socket_path(&base, &pane_id);
    // Refuse to clobber a still-live daemon on a caller-chosen id: if
    // its socket still accepts a connection, reusing the id would run
    // two daemons on one socket path, and the old one's cleanup() would
    // later unlink THIS daemon's files, orphaning a healthy pane.
    if args.pane_id.is_some() && UnixStream::connect(&sock_path).is_ok() {
        return Err(io::Error::other(
            "pane-id already has a live daemon; refusing to clobber",
        ));
    }
    let _ = std::fs::remove_file(&sock_path);

    // Bind + listen BEFORE forking: once `spawn` exits 0 the socket is
    // guaranteed connectable — no attach race by construction.
    let old_umask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&sock_path);
    unsafe { libc::umask(old_umask) };
    let listener = listener?;

    // Handshake pipe daemon -> foreground.
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (pipe_r, pipe_w) = unsafe {
        (
            OwnedFd::from_raw_fd(pipe_fds[0]),
            OwnedFd::from_raw_fd(pipe_fds[1]),
        )
    };

    match unsafe { libc::fork() } {
        n if n < 0 => Err(io::Error::last_os_error()),
        0 => {
            // Daemon child.
            drop(pipe_r);
            daemon_main(base, pane_id, sock_path, listener, pipe_w, args);
            // daemon_main never returns; belt and braces:
            std::process::exit(0);
        }
        _daemon_pid => {
            // Foreground: relay the daemon's handshake line to stdout.
            drop(pipe_w);
            drop(listener);
            let mut line = String::new();
            let mut f = std::fs::File::from(pipe_r);
            // The daemon writes one line then closes; a 5s guard via
            // poll would be nicer but read-until-EOF on a pipe whose
            // writer either writes or dies is adequate here.
            f.read_to_string(&mut line)?;
            if line.trim().is_empty() {
                return Err(io::Error::other("daemon failed to start"));
            }
            println!("{}", line.trim());
            Ok(())
        }
    }
}

fn daemonize(log_path: Option<&std::path::Path>) {
    unsafe {
        libc::setsid();
        // Reset dispositions; unblock everything.
        for sig in [
            libc::SIGCHLD,
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGPIPE,
        ] {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());

        // stdin/stdout/stderr -> /dev/null (stderr optionally logged).
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            match log_path {
                Some(path) if std::env::var_os("RIO_PTYD_LOG").is_some() => {
                    if let Ok(cpath) =
                        std::ffi::CString::new(path.to_string_lossy().as_bytes())
                    {
                        let lf = libc::open(
                            cpath.as_ptr(),
                            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                            0o600,
                        );
                        if lf >= 0 {
                            libc::dup2(lf, 2);
                            if lf > 2 {
                                libc::close(lf);
                            }
                        } else {
                            libc::dup2(devnull, 2);
                        }
                    }
                }
                _ => {
                    libc::dup2(devnull, 2);
                }
            }
            if devnull > 2 {
                libc::close(devnull);
            }
        }
        let _ = libc::chdir(c"/".as_ptr());
    }
}

fn install_signal_handlers(pipe_w: RawFd) {
    unsafe {
        SIGNAL_PIPE_W = pipe_w;
        libc::signal(
            libc::SIGCHLD,
            signal_handler as *const () as libc::sighandler_t,
        );
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            libc::signal(sig, term_handler as *const () as libc::sighandler_t);
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn daemon_main(
    base: PathBuf,
    pane_id: String,
    sock_path: PathBuf,
    listener: UnixListener,
    handshake: OwnedFd,
    args: SpawnArgs,
) {
    let log_path = base.join(format!("{pane_id}.log"));
    daemonize(Some(&log_path));

    // Signal self-pipe.
    let mut sp = [0i32; 2];
    if unsafe { libc::pipe(sp.as_mut_ptr()) } != 0 {
        std::process::exit(1);
    }
    let (sig_r, sig_w) =
        unsafe { (OwnedFd::from_raw_fd(sp[0]), OwnedFd::from_raw_fd(sp[1])) };
    let _ = pty::set_nonblocking(sig_r.as_raw_fd());
    let _ = pty::set_nonblocking(sig_w.as_raw_fd());
    install_signal_handlers(sig_w.as_raw_fd());
    std::mem::forget(sig_w); // handler owns it for the daemon's lifetime

    let mut env = args.env.clone();
    if !env.iter().any(|(k, _)| k == "TERM") {
        env.push(("TERM".into(), "xterm-256color".into()));
    }
    if !env.iter().any(|(k, _)| k == "COLORTERM") {
        env.push(("COLORTERM".into(), "truecolor".into()));
    }

    let child = match pty::spawn_shell(&SpawnSpec {
        program: &args.program,
        args: &args.args,
        cwd: args.cwd.as_deref(),
        env: &env,
        rows: 25,
        cols: 80,
    }) {
        Ok(c) => c,
        Err(e) => {
            let line = serde_json::json!({ "error": e.to_string() }).to_string();
            let _ = writeln_fd(&handshake, &line);
            std::process::exit(1);
        }
    };

    let meta = PaneMeta {
        version: sockdir::METADATA_VERSION,
        pane_id: pane_id.clone(),
        daemon_pid: std::process::id() as i32,
        shell_pid: child.shell_pid,
        program: args.program.clone(),
        args: args.args.clone(),
        cwd: args.cwd.clone(),
        created_at: sockdir::now_epoch(),
        exited_status: None,
        session: args.session.clone(),
    };
    if sockdir::write_meta(&base, &meta).is_err() {
        std::process::exit(1);
    }

    let hello = serde_json::json!({
        "pane_id": pane_id,
        "socket": sock_path.display().to_string(),
        "daemon_pid": meta.daemon_pid,
        "shell_pid": meta.shell_pid,
    })
    .to_string();
    let _ = writeln_fd(&handshake, &hello);
    drop(handshake);

    let code = Daemon {
        base,
        pane_id,
        listener,
        sig_r,
        pty: child,
        meta,
        ring: ReplayRing::new(args.ring_size),
        client: None,
        pending: Vec::new(),
        pty_inbuf: Vec::new(),
        shell_exit: None,
        kill_requested: false,
    }
    .run();
    std::process::exit(code);
}

fn writeln_fd(fd: &OwnedFd, s: &str) -> io::Result<()> {
    let dup = unsafe { libc::dup(fd.as_raw_fd()) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(dup) };
    writeln!(f, "{s}")
}

struct ClientConn {
    stream: UnixStream,
    dec: Decoder,
    outbuf: Vec<u8>,
    outbuf_off: usize,
    hello_done: bool,
    features: u32,
    /// When this connection was accepted; a pending connection that
    /// hasn't completed its hello by PENDING_HELLO_DEADLINE is dropped.
    accepted_at: std::time::Instant,
}

/// Pre-hello connections are capped and time-boxed: without both, a
/// same-uid process could park enough silent or malformed connections
/// to exhaust fds or block legitimate reattaches.
const MAX_PENDING: usize = 8;
/// Unconsumed decoder backlog beyond which a peer is dropped: valid
/// frames are drained after every read burst, so a backlog this deep
/// means the peer is streaming garbage-in-frame-form faster than it
/// can ever be consumed.
const DECODER_BACKLOG_CAP: usize = 4 * (protocol::MAX_PAYLOAD + protocol::HEADER_LEN);
const PENDING_HELLO_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

impl ClientConn {
    fn queue(&mut self, typ: FrameType, payload: &[u8]) {
        let _ = protocol::write_frame(&mut VecWriter(&mut self.outbuf), typ, payload);
    }

    fn queue_output_chunked(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(protocol::MAX_PAYLOAD) {
            self.queue(FrameType::Output, chunk);
        }
    }
}

struct VecWriter<'a>(&'a mut Vec<u8>);
impl io::Write for VecWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Daemon {
    base: PathBuf,
    pane_id: String,
    listener: UnixListener,
    sig_r: OwnedFd,
    pty: PtyChild,
    meta: PaneMeta,
    ring: ReplayRing,
    client: Option<ClientConn>,
    pending: Vec<ClientConn>,
    pty_inbuf: Vec<u8>,
    shell_exit: Option<i32>,
    /// A client requested Kill: the daemon must exit when the shell
    /// reaps, even if that client already disconnected (the tab-close
    /// path sends Kill and drops the socket immediately).
    kill_requested: bool,
}

impl Daemon {
    fn run(mut self) -> i32 {
        let _ = self.listener.set_nonblocking(true);
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(4 + MAX_PENDING);
        let mut pending_re: Vec<libc::c_short> = Vec::with_capacity(MAX_PENDING);
        loop {
            fds.clear();
            let listener_idx = fds.len();
            fds.push(pollfd(self.listener.as_raw_fd(), libc::POLLIN));
            let sig_idx = fds.len();
            fds.push(pollfd(self.sig_r.as_raw_fd(), libc::POLLIN));

            let pty_idx = if self.shell_exit.is_none() {
                let mut ev = libc::POLLIN;
                if !self.pty_inbuf.is_empty() {
                    ev |= libc::POLLOUT;
                }
                fds.push(pollfd(self.pty.master.as_raw_fd(), ev));
                Some(fds.len() - 1)
            } else {
                None
            };

            // POLLIN is disarmed while pty_inbuf is over the cap:
            // pump_client_in won't read then, and unread socket data
            // would make level-triggered poll return instantly forever
            // while a non-reading foreground never drains the pty.
            // Hangup detection survives — POLLHUP/POLLERR are reported
            // regardless of requested events, even with events == 0.
            let inbuf_full = self.pty_inbuf.len() > PTY_INBUF_CAP;
            let client_idx = self.client.as_ref().map(|c| {
                let mut ev: libc::c_short = 0;
                if !inbuf_full {
                    ev |= libc::POLLIN;
                }
                if c.outbuf_off < c.outbuf.len() {
                    ev |= libc::POLLOUT;
                }
                fds.push(pollfd(c.stream.as_raw_fd(), ev));
                fds.len() - 1
            });

            let pending_base = fds.len();
            for p in &self.pending {
                fds.push(pollfd(p.stream.as_raw_fd(), libc::POLLIN));
            }

            // Finite timeout only while pre-hello connections are
            // parked: a silent peer generates no events, so the
            // deadline sweep in pump_pending needs a periodic wake.
            let timeout: libc::c_int = if self.pending.is_empty() { -1 } else { 1000 };
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    // fall through to signal drain below anyway
                } else {
                    self.cleanup();
                    return 1;
                }
            }

            if revents(&fds, sig_idx) & libc::POLLIN != 0 || n < 0 {
                if let Some(code) = self.handle_signals() {
                    return code;
                }
            }

            if let Some(idx) = pty_idx {
                let re = revents(&fds, idx);
                if re & (libc::POLLIN | libc::POLLHUP) != 0 && self.pump_pty_out() {
                    // EIO/HUP: shell side gone; SIGCHLD will finalize.
                }
                if re & libc::POLLOUT != 0 {
                    self.pump_pty_in();
                }
            }

            if let Some(idx) = client_idx {
                let re = revents(&fds, idx);
                if re & (libc::POLLIN | libc::POLLHUP) != 0 {
                    let hangup = re & (libc::POLLHUP | libc::POLLERR) != 0;
                    self.pump_client_in(hangup);
                }
                if re & libc::POLLOUT != 0 {
                    self.pump_client_out();
                }
            }

            // Service pending (pre-hello) connections: a probe that
            // closed without a hello is dropped; one that completes its
            // ClientHello is promoted, replacing the live client only
            // then.
            pending_re.clear();
            pending_re
                .extend((0..self.pending.len()).map(|i| revents(&fds, pending_base + i)));
            self.pump_pending(&pending_re);

            if revents(&fds, listener_idx) & libc::POLLIN != 0 {
                self.accept_client();
            }
        }
    }

    fn handle_signals(&mut self) -> Option<i32> {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::read(
                    self.sig_r.as_raw_fd(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }
        // Reap children.
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if pid == self.meta.shell_pid {
                let code = decode_wait_status(status);
                self.shell_exit = Some(code);
                // The kernel may still hold output written just before
                // death, and the master leaves the poll set once
                // shell_exit is set — drain it through the normal path
                // (ring + client queue) now or the tail of the session
                // is lost. Bounded: reads stop at EAGAIN/EIO and the
                // ring caps memory.
                self.pump_pty_out();
                // Queued stdin has no consumer anymore; dropping it
                // also releases the read gate keyed on pty_inbuf.
                self.pty_inbuf.clear();
                if let Some(c) = &mut self.client {
                    c.queue(FrameType::Exited, &code.to_le_bytes());
                    // Flush what we can; then shut down.
                    let _ = flush_blocking(c);
                    self.cleanup();
                    return Some(0);
                }
                if self.kill_requested {
                    // Deliberate kill: never linger, even when the
                    // killing client already dropped the socket.
                    self.cleanup();
                    return Some(0);
                }
                // Detached: linger with the ring; record status.
                self.meta.exited_status = Some(code);
                let _ = sockdir::write_meta(&self.base, &self.meta);
            }
        }
        // Any termination signal shuts the daemon down and cleans up —
        // including in the exited-and-lingering state (shell_exit set,
        // ring kept for a future reattach). Gating this on
        // shell_exit.is_none() made a lingering daemon ignore
        // SIGTERM/SIGHUP forever, leaking the process and its socket/
        // meta files until a SIGKILL that skips cleanup().
        if termination_requested() {
            if let Some(c) = &mut self.client {
                c.queue(FrameType::Detached, &[protocol::DETACH_SERVER_SHUTDOWN]);
                let _ = flush_blocking(c);
            }
            self.cleanup();
            return Some(0);
        }
        None
    }

    fn accept_client(&mut self) {
        while let Ok((stream, _)) = self.listener.accept() {
            let _ = stream.set_nonblocking(true);
            let conn = ClientConn {
                stream,
                dec: Decoder::new(),
                outbuf: Vec::new(),
                outbuf_off: 0,
                hello_done: false,
                features: 0,
                accepted_at: std::time::Instant::now(),
            };
            // Park as pending: a bare probe (e.g. gc liveness check)
            // that connects and closes without a hello must NOT evict
            // the live client. Eviction happens only when the newcomer
            // completes its ClientHello (see pump_pending).
            if self.client.is_none() {
                self.client = Some(conn);
            } else if self.pending.len() < MAX_PENDING {
                self.pending.push(conn);
            }
            // Beyond the cap the connection drops here; the peer sees
            // EOF and retries after existing slots expire.
        }
    }

    /// Advance pre-hello connections. Drop those that closed without a
    /// hello (bare liveness probes); promote the first that completes
    /// its ClientHello into the active client, evicting the old one.
    fn pump_pending(&mut self, revents: &[libc::c_short]) {
        let mut promote: Option<usize> = None;
        let mut drop_idx: Vec<usize> = Vec::new();
        for (i, conn) in self.pending.iter_mut().enumerate() {
            // Expire a connection that never completed its hello. Done
            // inside this loop (not a pre-pass retain) so indices stay
            // aligned with the revents slice built before the call.
            if conn.accepted_at.elapsed() >= PENDING_HELLO_DEADLINE {
                drop_idx.push(i);
                continue;
            }
            let re = revents.get(i).copied().unwrap_or(0);
            if re & (libc::POLLIN | libc::POLLHUP) == 0 {
                continue;
            }
            let mut buf = [0u8; 4096];
            let mut closed = false;
            loop {
                match conn.stream.read(&mut buf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => {
                        conn.dec.feed(&buf[..n]);
                        if conn.dec.buffered() > DECODER_BACKLOG_CAP {
                            closed = true;
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            let mut hello = false;
            loop {
                match conn.dec.next_frame() {
                    Ok(Some((h, payload))) => {
                        if FrameType::from_u8(h.typ) == Some(FrameType::ClientHello)
                            && protocol::decode_client_hello(&payload)
                                == Some(protocol::PROTOCOL_VERSION)
                        {
                            conn.features =
                                protocol::decode_client_hello_features(&payload);
                            hello = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    // A malformed frame parked forever would pin the
                    // fd and its poll slot until process exit.
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            if hello && promote.is_none() {
                promote = Some(i);
            } else if hello || closed {
                // A second connection that also completed its hello in
                // this pass has had its ClientHello consumed and can no
                // longer be promoted; drop it now (the peer retries)
                // rather than leave it parked until the hello deadline.
                drop_idx.push(i);
            }
        }
        // Remove promoted and dropped connections together, highest
        // index first, so earlier removals never shift a pending index.
        let mut removals: Vec<usize> = drop_idx;
        if let Some(i) = promote {
            removals.push(i);
        }
        removals.sort_unstable();
        removals.dedup();
        let mut promoted = None;
        for i in removals.into_iter().rev() {
            let conn = self.pending.remove(i);
            if Some(i) == promote {
                promoted = Some(conn);
            }
        }
        if let Some(conn) = promoted {
            if let Some(mut old) = self.client.take() {
                old.queue(FrameType::Detached, &[protocol::DETACH_REPLACED]);
                let _ = flush_blocking(&mut old);
            }
            // Keep the promoted connection's decoder: frames the client
            // already sent after its ClientHello (a reattach Resize, or
            // a `kill`'s Kill frame written in one burst with the hello)
            // are still buffered there and must be handled, not dropped.
            self.client = Some(conn);
            self.service_hello();
            // Drain those already-buffered frames now (no hangup yet).
            self.pump_client_in(false);
        }
    }

    fn service_hello(&mut self) {
        let shell_state =
            if self.shell_exit.is_some() || self.meta.exited_status.is_some() {
                protocol::SHELL_EXITED
            } else {
                protocol::SHELL_RUNNING
            };
        let exit_status = self.shell_exit.or(self.meta.exited_status).unwrap_or(0);
        // Prefer the shell's LIVE cwd (/proc/<pid>/cwd) over the
        // spawn-time cwd — the client caches it so a later fresh-spawn
        // fallback (dead daemon) lands in the right directory.
        let live_cwd =
            crate::osproc::cwd(self.meta.shell_pid).or_else(|| self.meta.cwd.clone());
        let meta_json = serde_json::json!({
            "pane_id": self.pane_id,
            "program": self.meta.program,
            "cwd": live_cwd,
        })
        .to_string();
        let hello = ServerHello {
            version: protocol::PROTOCOL_VERSION,
            daemon_pid: self.meta.daemon_pid as u32,
            shell_pid: self.meta.shell_pid as u32,
            shell_state,
            exit_status,
            meta_json,
        }
        .encode();
        let no_replay = self
            .client
            .as_ref()
            .is_some_and(|c| c.features & protocol::FEATURE_NO_REPLAY != 0);
        let replay = if no_replay {
            Vec::new()
        } else {
            self.ring.replay()
        };
        let exited = shell_state == protocol::SHELL_EXITED;
        if let Some(c) = &mut self.client {
            c.queue(FrameType::ServerHello, &hello);
            c.queue(FrameType::ReplayBegin, &(replay.len() as u64).to_le_bytes());
            c.queue_output_chunked(&replay);
            c.queue(FrameType::ReplayEnd, &[]);
            if exited {
                let code = exit_status;
                c.queue(FrameType::Exited, &code.to_le_bytes());
                let _ = flush_blocking(c);
                self.cleanup();
                std::process::exit(0);
            }
            c.hello_done = true;
        }
    }

    fn pump_pty_out(&mut self) -> bool {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            let n = unsafe {
                libc::read(
                    self.pty.master.as_raw_fd(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                )
            };
            if n > 0 {
                let data = &buf[..n as usize];
                self.ring.push(data);
                if let Some(c) = &mut self.client {
                    if c.hello_done {
                        c.queue_output_chunked(data);
                        if c.outbuf.len() - c.outbuf_off > CLIENT_OUTBUF_CAP {
                            c.queue(
                                FrameType::Detached,
                                &[protocol::DETACH_SLOW_CONSUMER],
                            );
                            let _ = flush_blocking(c);
                            self.client = None;
                        } else {
                            self.pump_client_out();
                        }
                    }
                }
                continue;
            }
            let e = io::Error::last_os_error();
            return !(n < 0 && e.kind() == io::ErrorKind::WouldBlock);
        }
    }

    fn pump_pty_in(&mut self) {
        while !self.pty_inbuf.is_empty() {
            let n = unsafe {
                libc::write(
                    self.pty.master.as_raw_fd(),
                    self.pty_inbuf.as_ptr() as *const _,
                    self.pty_inbuf.len(),
                )
            };
            if n > 0 {
                self.pty_inbuf.drain(..n as usize);
            } else {
                break;
            }
        }
    }

    fn pump_client_in(&mut self, hangup: bool) {
        let mut drop_client = false;
        let mut got_hello = false;
        if let Some(c) = &mut self.client {
            let mut buf = [0u8; READ_CHUNK];
            loop {
                // Backpressure: stop READING while the pty can't drink,
                // to bound pty_inbuf. But a hangup must still be acted
                // on even when full — the caller passes `hangup` from
                // POLLHUP so a closed tab (whose Kill/EOF we can't read
                // here) is dropped below instead of spun on forever.
                if self.pty_inbuf.len() > PTY_INBUF_CAP {
                    break;
                }
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        drop_client = true;
                        break;
                    }
                    Ok(n) => {
                        c.dec.feed(&buf[..n]);
                        // A backlog this deep with no complete frame to
                        // consume means the peer is streaming garbage;
                        // a legit large paste is many complete frames, so
                        // break to drain them rather than severing.
                        if c.dec.buffered() > DECODER_BACKLOG_CAP {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        drop_client = true;
                        break;
                    }
                }
            }
            loop {
                match c.dec.next_frame() {
                    Ok(Some((h, payload))) => match FrameType::from_u8(h.typ) {
                        // A second hello on an already-serviced client
                        // (e.g. a promoted connection re-drained here)
                        // would re-send the whole replay; ignore it.
                        Some(FrameType::ClientHello) if c.hello_done => {}
                        Some(FrameType::ClientHello) => {
                            let v = protocol::decode_client_hello(&payload);
                            c.features = protocol::decode_client_hello_features(&payload);
                            if v != Some(protocol::PROTOCOL_VERSION) {
                                let mut msg = vec![protocol::ERROR_VERSION_MISMATCH];
                                msg.extend_from_slice(
                                    format!(
                                        "server speaks v{}, client sent v{:?}",
                                        protocol::PROTOCOL_VERSION,
                                        v
                                    )
                                    .as_bytes(),
                                );
                                c.queue(FrameType::Error, &msg);
                                let _ = flush_blocking(c);
                                drop_client = true;
                                break;
                            }
                            got_hello = true;
                        }
                        Some(FrameType::Stdin) => {
                            if self.shell_exit.is_none() {
                                self.pty_inbuf.extend_from_slice(&payload);
                            }
                        }
                        Some(FrameType::Resize) => {
                            if let Some((rows, cols, xp, yp)) =
                                protocol::decode_resize(&payload)
                            {
                                // 0x0 or absurd sizes wedge apps that
                                // trust the winsize blindly.
                                pty::set_winsize(
                                    self.pty.master.as_raw_fd(),
                                    rows.clamp(1, 10000),
                                    cols.clamp(1, 10000),
                                    xp,
                                    yp,
                                );
                                // dtach trick: some full-screen apps
                                // only repaint on SIGWINCH. Guard on
                                // liveness like Kill does — a reaped
                                // shell_pid may already be recycled.
                                if self.shell_exit.is_none()
                                    && self.meta.exited_status.is_none()
                                {
                                    unsafe {
                                        libc::killpg(self.meta.shell_pid, libc::SIGWINCH);
                                    }
                                }
                            }
                        }
                        Some(FrameType::Kill) => {
                            self.kill_requested = true;
                            if self.shell_exit.is_none()
                                && self.meta.exited_status.is_none()
                            {
                                unsafe {
                                    libc::killpg(self.meta.shell_pid, libc::SIGHUP);
                                }
                                // SIGCHLD path finalizes and exits.
                            } else {
                                // Already exited + lingering: consume.
                                self.cleanup();
                                std::process::exit(0);
                            }
                        }
                        Some(FrameType::Detach) => {
                            drop_client = true;
                            break;
                        }
                        Some(FrameType::Ping) => c.queue(FrameType::Pong, &[]),
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(_) => {
                        drop_client = true;
                        break;
                    }
                }
            }
            // Still over the cap after draining every complete frame:
            // the remainder is an unparseable partial that will never
            // complete — a genuinely stuck or hostile peer. Drop it.
            if c.dec.buffered() > DECODER_BACKLOG_CAP {
                drop_client = true;
            }
            // Bound the reply queue on the input path too: a peer that
            // floods Ping (each answered with a queued Pong) but never
            // reads would grow outbuf without limit. The pty-out path
            // has its own cap; this covers control-frame replies.
            if c.outbuf.len() - c.outbuf_off > CLIENT_OUTBUF_CAP {
                drop_client = true;
            }
        }
        // A hangup while pty-backpressured: we skipped the read that
        // would have seen EOF, so drop the client here rather than
        // spin on the level-triggered POLLHUP forever.
        if hangup && self.pty_inbuf.len() > PTY_INBUF_CAP {
            drop_client = true;
        }
        if got_hello {
            self.service_hello();
        }
        if drop_client {
            self.client = None;
        } else {
            self.pump_pty_in();
            self.pump_client_out();
        }
    }

    fn pump_client_out(&mut self) {
        let mut drop_client = false;
        if let Some(c) = &mut self.client {
            while c.outbuf_off < c.outbuf.len() {
                match c.stream.write(&c.outbuf[c.outbuf_off..]) {
                    Ok(0) => {
                        drop_client = true;
                        break;
                    }
                    Ok(n) => c.outbuf_off += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        drop_client = true;
                        break;
                    }
                }
            }
            if c.outbuf_off == c.outbuf.len() {
                c.outbuf.clear();
                c.outbuf_off = 0;
            } else if c.outbuf_off > c.outbuf.len() / 2 {
                c.outbuf.drain(..c.outbuf_off);
                c.outbuf_off = 0;
            }
        }
        if drop_client {
            self.client = None;
        }
    }

    fn cleanup(&mut self) {
        sockdir::remove_pane_files(&self.base, &self.pane_id);
    }
}

use std::sync::atomic::{AtomicBool, Ordering};

static TERM_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn term_handler(_sig: libc::c_int) {
    TERM_FLAG.store(true, Ordering::SeqCst);
    unsafe {
        if SIGNAL_PIPE_W >= 0 {
            let b = [1u8; 1];
            libc::write(SIGNAL_PIPE_W, b.as_ptr() as *const _, 1);
        }
    }
}

fn termination_requested() -> bool {
    TERM_FLAG.load(Ordering::SeqCst)
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn revents(fds: &[libc::pollfd], idx: usize) -> libc::c_short {
    fds.get(idx).map(|p| p.revents).unwrap_or(0)
}

fn decode_wait_status(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    }
}

/// Best-effort final flush of a client's queued bytes, bounded by a
/// wall-clock deadline. A stuck or malicious client must never wedge
/// the single-threaded event loop (which also services SIGCHLD/SIGTERM),
/// so this polls for writability rather than doing an unbounded
/// blocking write_all — the guarantee is "delivered if the peer drains
/// promptly", not "delivered no matter what".
fn flush_blocking(c: &mut ClientConn) -> io::Result<()> {
    use std::io::Write;
    let fd = c.stream.as_raw_fd();
    let deadline = Instant::now() + Duration::from_millis(500);
    while c.outbuf_off < c.outbuf.len() {
        match c.stream.write(&c.outbuf[c.outbuf_off..]) {
            Ok(0) => break,
            Ok(n) => c.outbuf_off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let r = unsafe { libc::poll(&mut pfd, 1, ms) };
                if r < 0 {
                    // EINTR (a SIGCHLD during shutdown flush): retry
                    // against the same deadline rather than abandon the
                    // final Detached/Exited frame the client needs.
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if r == 0 {
                    break; // deadline hit
                }
            }
            Err(e) => {
                c.outbuf.clear();
                c.outbuf_off = 0;
                return Err(e);
            }
        }
    }
    c.outbuf.clear();
    c.outbuf_off = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output the shell wrote right before exiting must survive into
    /// the ring: the reap path drains the master itself, because the
    /// master leaves the poll set once shell_exit is set.
    #[test]
    fn shell_exit_drains_final_pty_output_into_ring() {
        let base = std::env::temp_dir()
            .join(format!("rio-ptyd-drain-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let sock_path = base.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let mut sp = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(sp.as_mut_ptr()) }, 0);
        let (sig_r, _sig_w) =
            unsafe { (OwnedFd::from_raw_fd(sp[0]), OwnedFd::from_raw_fd(sp[1])) };
        pty::set_nonblocking(sig_r.as_raw_fd()).unwrap();

        let child = pty::spawn_shell(&SpawnSpec {
            program: "/bin/sh",
            args: &["-c".into(), "printf bye".into()],
            cwd: None,
            env: &[],
            rows: 25,
            cols: 80,
        })
        .unwrap();
        let shell_pid = child.shell_pid;

        let pane_id = "0123456789abcdef0123456789abcdef".to_string();
        let mut d = Daemon {
            base: base.clone(),
            pane_id: pane_id.clone(),
            listener,
            sig_r,
            pty: child,
            meta: PaneMeta {
                version: sockdir::METADATA_VERSION,
                pane_id,
                daemon_pid: std::process::id() as i32,
                shell_pid,
                program: "/bin/sh".into(),
                args: Vec::new(),
                cwd: None,
                created_at: sockdir::now_epoch(),
                exited_status: None,
                session: None,
            },
            ring: ReplayRing::new(64 * 1024),
            client: None,
            pending: Vec::new(),
            pty_inbuf: Vec::new(),
            shell_exit: None,
            kill_requested: false,
        };

        // No poll loop runs here, so the ring can only see the shell's
        // output through the reap-time drain in handle_signals.
        let deadline = Instant::now() + Duration::from_secs(10);
        while d.shell_exit.is_none() {
            assert!(d.handle_signals().is_none(), "detached daemon must linger");
            assert!(Instant::now() < deadline, "shell did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }

        let replay = d.ring.replay();
        assert!(
            replay.windows(3).any(|w| w == b"bye"),
            "final shell output lost: {:?}",
            String::from_utf8_lossy(&replay)
        );
        assert!(d.pty_inbuf.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
