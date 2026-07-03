//! The per-pane daemon: owns the PTY master, the replay ring, and the
//! pane's unix socket; serves at most one attached client at a time.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

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
            let _ = writeln_fd(&handshake, &format!("{{\"error\":\"{e}\"}}"));
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
    };
    if sockdir::write_meta(&base, &meta).is_err() {
        std::process::exit(1);
    }

    let hello = format!(
        "{{\"pane_id\":\"{}\",\"socket\":\"{}\",\"daemon_pid\":{},\"shell_pid\":{}}}",
        pane_id,
        sock_path.display(),
        meta.daemon_pid,
        meta.shell_pid
    );
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
    let mut f = unsafe { std::fs::File::from_raw_fd(libc::dup(fd.as_raw_fd())) };
    writeln!(f, "{s}")
}

struct ClientConn {
    stream: UnixStream,
    dec: Decoder,
    outbuf: Vec<u8>,
    outbuf_off: usize,
    hello_done: bool,
    features: u32,
}

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
        loop {
            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(4);
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

            let client_idx = self.client.as_ref().map(|c| {
                let mut ev = libc::POLLIN;
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

            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
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
                    self.pump_client_in();
                }
                if re & libc::POLLOUT != 0 {
                    self.pump_client_out();
                }
            }

            // Service pending (pre-hello) connections: a probe that
            // closed without a hello is dropped; one that completes its
            // ClientHello is promoted, replacing the live client only
            // then.
            let pending_re: Vec<libc::c_short> = (0..self.pending.len())
                .map(|i| revents(&fds, pending_base + i))
                .collect();
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
        if self.shell_exit.is_none() && termination_requested() {
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
            };
            // Park as pending: a bare probe (e.g. gc liveness check)
            // that connects and closes without a hello must NOT evict
            // the live client. Eviction happens only when the newcomer
            // completes its ClientHello (see pump_pending).
            if self.client.is_none() {
                self.client = Some(conn);
            } else {
                self.pending.push(conn);
            }
        }
    }

    /// Advance pre-hello connections. Drop those that closed without a
    /// hello (bare liveness probes); promote the first that completes
    /// its ClientHello into the active client, evicting the old one.
    fn pump_pending(&mut self, revents: &[libc::c_short]) {
        let mut promote: Option<usize> = None;
        let mut drop_idx: Vec<usize> = Vec::new();
        for (i, conn) in self.pending.iter_mut().enumerate() {
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
                    Ok(n) => conn.dec.feed(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            let mut hello = false;
            while let Ok(Some((h, payload))) = conn.dec.next_frame() {
                if FrameType::from_u8(h.typ) == Some(FrameType::ClientHello)
                    && protocol::decode_client_hello(&payload)
                        == Some(protocol::PROTOCOL_VERSION)
                {
                    conn.features = protocol::decode_client_hello_features(&payload);
                    hello = true;
                    break;
                }
            }
            if hello && promote.is_none() {
                promote = Some(i);
            } else if closed {
                drop_idx.push(i);
            }
        }
        if let Some(i) = promote {
            let mut conn = self.pending.remove(i);
            conn.dec = Decoder::new();
            if let Some(mut old) = self.client.take() {
                old.queue(FrameType::Detached, &[protocol::DETACH_REPLACED]);
                let _ = flush_blocking(&mut old);
            }
            // Re-run the hello handshake for the promoted client.
            self.client = Some(conn);
            self.service_hello();
        }
        for i in drop_idx.into_iter().rev() {
            self.pending.remove(i);
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
        let live_cwd = live_cwd(self.meta.shell_pid).or_else(|| self.meta.cwd.clone());
        let meta_json = format!(
            "{{\"pane_id\":\"{}\",\"program\":\"{}\",\"cwd\":{}}}",
            self.pane_id,
            self.meta.program,
            live_cwd
                .as_ref()
                .map(|c| format!("\"{}\"", c.replace('\\', "\\\\").replace('"', "\\\"")))
                .unwrap_or_else(|| "null".into()),
        );
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

    fn pump_client_in(&mut self) {
        let mut drop_client = false;
        let mut got_hello = false;
        if let Some(c) = &mut self.client {
            let mut buf = [0u8; READ_CHUNK];
            loop {
                // Backpressure: stop reading while the pty can't drink.
                if self.pty_inbuf.len() > PTY_INBUF_CAP {
                    break;
                }
                match c.stream.read(&mut buf) {
                    Ok(0) => {
                        drop_client = true;
                        break;
                    }
                    Ok(n) => c.dec.feed(&buf[..n]),
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
                                pty::set_winsize(
                                    self.pty.master.as_raw_fd(),
                                    rows,
                                    cols,
                                    xp,
                                    yp,
                                );
                                // dtach trick: some full-screen apps
                                // only repaint on SIGWINCH.
                                unsafe {
                                    libc::killpg(self.meta.shell_pid, libc::SIGWINCH);
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

fn live_cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
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

fn flush_blocking(c: &mut ClientConn) -> io::Result<()> {
    let _ = c.stream.set_nonblocking(false);
    let buf = &c.outbuf[c.outbuf_off..];
    let res = c.stream.write_all(buf);
    c.outbuf.clear();
    c.outbuf_off = 0;
    let _ = c.stream.set_nonblocking(true);
    res
}
