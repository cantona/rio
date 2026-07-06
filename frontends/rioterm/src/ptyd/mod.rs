//! Client side of rio-ptyd persistence: `RemotePty` plugs a session
//! daemon into the existing `Machine` io loop by implementing the same
//! `ProcessReadWrite`/`EventedPty` traits the local `Pty` does.
//!
//! The transport carries framed protocol bytes (rio_ptyd::protocol)
//! over a local unix socket or any child process bridging its
//! stdin/stdout to a remote daemon (`ssh host rio-ptyd attach --stdio`).
//! Only an `Exited` frame means the shell died; transport EOF without
//! it means the link was lost (v1 surfaces both as pane exit, but the
//! distinction is kept for a future reconnect flow).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use corcovado::unix::EventedFd;
use rio_ptyd::protocol::{self, Decoder, FrameType, ServerHello};
use teletypewriter::{ChildEvent, EventedPty, ProcessReadWrite, WinsizeBuilder};

pub enum Transport {
    Unix(UnixStream),
    /// A helper process whose stdin/stdout bridge to the daemon
    /// (`ssh <dest> rio-ptyd attach --stdio <pane>`); dropped = killed.
    Child {
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        /// Set by `flush_before_kill`: on drop, give the child a brief
        /// window to forward a just-queued frame (a deliberate Kill)
        /// over the network before falling back to SIGKILL.
        graceful: bool,
    },
}

impl Transport {
    fn read_fd(&self) -> RawFd {
        match self {
            Transport::Unix(s) => s.as_raw_fd(),
            Transport::Child { stdout, .. } => stdout.as_raw_fd(),
        }
    }

    fn write_fd(&self) -> RawFd {
        match self {
            Transport::Unix(s) => s.as_raw_fd(),
            Transport::Child { stdin, .. } => stdin.as_raw_fd(),
        }
    }

    /// A deliberate Kill frame was just queued into the transport.
    /// For an ssh child the frame is in the local stdin pipe; mark the
    /// child for a graceful close-and-linger on drop so ssh forwards it
    /// to the remote daemon before we tear the process down. A Unix
    /// socket needs nothing — the daemon reads it synchronously.
    fn flush_before_kill(&mut self) {
        if let Transport::Child { graceful, .. } = self {
            *graceful = true;
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Transport::Child {
            child, graceful, ..
        } = self
        {
            if *graceful {
                // Poll for a clean exit for up to ~500ms so a queued
                // Kill frame reaches the remote; SIGKILL only if ssh
                // overstays. Bounded so a wedged link can't hang close.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(500);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        _ => break,
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellState {
    Running,
    Exited,
}

#[derive(Clone, Debug)]
pub struct HelloInfo {
    #[allow(dead_code)]
    pub daemon_pid: u32,
    pub shell_pid: u32,
    pub shell_state: ShellState,
    pub exit_status: i32,
    /// Live cwd the daemon reported (from /proc/<shell>/cwd), used to
    /// cache the pane's directory for the dead-daemon fallback.
    pub cwd: Option<String>,
}

enum ReadState {
    /// Collecting the 8-byte frame header.
    Header { have: usize, hdr: [u8; HEADER_LEN] },
    /// Streaming an Output payload straight into the caller's buffer —
    /// the hot path: zero userspace copies for terminal bytes.
    Output { remaining: usize },
    /// Collecting a small control payload (Exited status etc.).
    Control { typ: u8, need: usize, got: Vec<u8> },
}

pub struct RemoteReader {
    fd: RawFd,
    state: ReadState,
    exit_pipe_w: OwnedFd,
    exit_signaled: bool,
    pub exited: Option<i32>,
    pub link_lost: bool,
    /// Between ReplayBegin and ReplayEnd frames.
    in_replay: bool,
    /// Replay bytes were delivered since the io loop last asked.
    replay_dirty: bool,
}

const HEADER_LEN: usize = protocol::HEADER_LEN;

impl RemoteReader {
    fn signal_exit(&mut self) {
        if !self.exit_signaled {
            self.exit_signaled = true;
            let b = [1u8; 1];
            unsafe {
                libc::write(self.exit_pipe_w.as_raw_fd(), b.as_ptr() as *const _, 1);
            }
        }
    }

    fn handle_control(&mut self, typ: u8, payload: &[u8]) {
        match FrameType::from_u8(typ) {
            Some(FrameType::Exited) => {
                let code = payload
                    .get(..4)
                    .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .unwrap_or(-1);
                self.exited = Some(code);
                self.signal_exit();
            }
            Some(FrameType::Detached) | Some(FrameType::Error) => {
                self.link_lost = true;
                self.signal_exit();
            }
            Some(FrameType::ReplayBegin) => self.in_replay = true,
            Some(FrameType::ReplayEnd) => self.in_replay = false,
            // Pong, stray hello: nothing to do.
            _ => {}
        }
    }
}

fn fd_read_raw(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

impl Read for RemoteReader {
    /// The io loop reads until WouldBlock. Contract: Ok(0) means the
    /// stream ended (shell exit or link loss) — control frames never
    /// surface it, they are consumed internally and the loop continues
    /// to the next frame or a genuine WouldBlock.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match &mut self.state {
                ReadState::Header { have, hdr } => {
                    let n = fd_read_raw(self.fd, &mut hdr[*have..])?;
                    if n == 0 {
                        if self.exited.is_none() {
                            self.link_lost = true;
                        }
                        self.signal_exit();
                        return Ok(0);
                    }
                    *have += n;
                    if *have < HEADER_LEN {
                        continue;
                    }
                    let len =
                        u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
                    let typ = hdr[4];
                    if len > protocol::MAX_PAYLOAD || FrameType::from_u8(typ).is_none() {
                        self.link_lost = true;
                        self.signal_exit();
                        return Ok(0);
                    }
                    self.state = if FrameType::from_u8(typ) == Some(FrameType::Output)
                        && len > 0
                    {
                        ReadState::Output { remaining: len }
                    } else if len == 0 {
                        self.handle_control(typ, &[]);
                        if self.exited.is_some() || self.link_lost {
                            return Ok(0);
                        }
                        ReadState::Header {
                            have: 0,
                            hdr: [0; HEADER_LEN],
                        }
                    } else {
                        ReadState::Control {
                            typ,
                            need: len,
                            got: Vec::with_capacity(len),
                        }
                    };
                }
                ReadState::Output { remaining } => {
                    let want = (*remaining).min(buf.len());
                    let n = fd_read_raw(self.fd, &mut buf[..want])?;
                    if n == 0 {
                        if self.exited.is_none() {
                            self.link_lost = true;
                        }
                        self.signal_exit();
                        return Ok(0);
                    }
                    *remaining -= n;
                    if *remaining == 0 {
                        self.state = ReadState::Header {
                            have: 0,
                            hdr: [0; HEADER_LEN],
                        };
                    }
                    // Output delivered inside the replay window must mark
                    // the batch as replayed, so query replies stay
                    // suppressed even when several reads (under lock
                    // contention) are consumed between take_replay_pending
                    // calls and in_replay has already flipped false.
                    if self.in_replay {
                        self.replay_dirty = true;
                    }
                    return Ok(n);
                }
                ReadState::Control { typ, need, got } => {
                    let mut tmp = [0u8; 256];
                    let want = (*need - got.len()).min(tmp.len());
                    let typ = *typ;
                    let n = fd_read_raw(self.fd, &mut tmp[..want])?;
                    if n == 0 {
                        if self.exited.is_none() {
                            self.link_lost = true;
                        }
                        self.signal_exit();
                        return Ok(0);
                    }
                    got.extend_from_slice(&tmp[..n]);
                    if got.len() == *need {
                        let payload = std::mem::take(got);
                        self.state = ReadState::Header {
                            have: 0,
                            hdr: [0; HEADER_LEN],
                        };
                        self.handle_control(typ, &payload);
                        if self.exited.is_some() || self.link_lost {
                            return Ok(0);
                        }
                    }
                }
            }
        }
    }
}

pub struct RemoteWriter {
    fd: RawFd,
    /// Encoded-but-not-yet-sent frame bytes. The transport fd is
    /// nonblocking (it is the same fd as the reader for a unix socket),
    /// so a large write can stop mid-frame with EAGAIN. Splicing a new
    /// frame header into a half-written frame would desync the daemon's
    /// decoder and duplicate input; instead the tail is held here and
    /// drained (flushed) before any new frame is encoded.
    pending: Vec<u8>,
}

impl RemoteWriter {
    /// Push out as much of `pending` as the socket accepts. Returns
    /// `WouldBlock` while bytes remain unsent.
    fn drain_pending(&mut self) -> io::Result<()> {
        while !self.pending.is_empty() {
            let n = unsafe {
                libc::write(
                    self.fd,
                    self.pending.as_ptr() as *const _,
                    self.pending.len(),
                )
            };
            if n > 0 {
                self.pending.drain(..n as usize);
                continue;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            }
        }
        Ok(())
    }
}

impl Write for RemoteWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // A prior write left an un-flushed frame tail: try once more to
        // push it, and if the socket still blocks, refuse new input
        // (WouldBlock) so the io loop retries these same bytes later —
        // encoding a new frame now would splice into the half-sent one.
        if !self.pending.is_empty() {
            match self.drain_pending() {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                Err(e) => return Err(e),
            }
        }
        // Encode the whole input into pending, then drain. The input is
        // fully captured in `pending` either way, so it counts as
        // consumed; any tail the socket wouldn't take is drained by the
        // next writable event (reregister keeps write interest live
        // while pending is non-empty).
        for chunk in buf.chunks(protocol::MAX_PAYLOAD) {
            protocol::write_frame(&mut self.pending, FrameType::Stdin, chunk)?;
        }
        match self.drain_pending() {
            Ok(()) => Ok(buf.len()),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(buf.len()),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain_pending()
    }
}

struct FdWriter(RawFd);

impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { libc::write(self.0, buf.as_ptr() as *const _, buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct RemotePty {
    // Field order matters: reader/writer borrow the transport's fds,
    // transport must outlive them within the struct (drop order is
    // declaration order; fds are raw so only Transport's Drop acts).
    reader: RemoteReader,
    writer: RemoteWriter,
    _transport: Transport,
    exit_pipe_r: OwnedFd,
    token: corcovado::Token,
    exit_token: corcovado::Token,
}

impl ProcessReadWrite for RemotePty {
    type Reader = RemoteReader;
    type Writer = RemoteWriter;

    fn reader(&mut self) -> &mut RemoteReader {
        &mut self.reader
    }

    fn read_token(&self) -> corcovado::Token {
        self.token
    }

    fn writer(&mut self) -> &mut RemoteWriter {
        &mut self.writer
    }

    fn write_token(&self) -> corcovado::Token {
        self.token
    }

    fn set_winsize(&mut self, ws: WinsizeBuilder) -> io::Result<()> {
        // Append after any buffered Stdin so frames stay whole and
        // ordered, then drain. Enqueuing ahead of a half-written frame
        // would desync the daemon decoder.
        protocol::write_frame(
            &mut self.writer.pending,
            FrameType::Resize,
            &protocol::encode_resize(ws.rows, ws.cols, ws.width, ws.height),
        )?;
        match self.writer.drain_pending() {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn kill(&mut self) {
        let _ = protocol::write_frame(&mut self.writer.pending, FrameType::Kill, &[]);
        // Push the Kill frame all the way into the transport with a
        // short blocking deadline. The write fd may be a full pipe (a
        // flow-stopped remote), in which case a single non-blocking
        // drain leaves the 8-byte frame stranded in `pending` — then
        // the graceful linger below would wait on an ssh child that
        // has nothing to forward, and the remote shell would survive a
        // deliberate close. Block briefly so the frame actually leaves.
        let wfd = self.writer.fd;
        let deadline = Instant::now() + Duration::from_millis(300);
        while !self.writer.pending.is_empty() && Instant::now() < deadline {
            match self.writer.drain_pending() {
                Ok(()) => break,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut pfd = libc::pollfd {
                        fd: wfd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let ms = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(i32::MAX as u128)
                        as libc::c_int;
                    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Only linger on drop if the frame actually made it into the
        // transport — otherwise the 500ms wait is pointless.
        if self.writer.pending.is_empty() {
            self._transport.flush_before_kill();
        }
    }

    fn take_replay_pending(&mut self) -> bool {
        // Sticky across the replay window plus a buffer that straddles
        // ReplayEnd: over-suppressing a few first live bytes is
        // harmless (apps re-query); under-suppressing injects stale
        // query replies into the app as input.
        let pending = self.reader.in_replay || self.reader.replay_dirty;
        self.reader.replay_dirty = self.reader.in_replay;
        pending
    }

    fn register(
        &mut self,
        poll: &corcovado::Poll,
        token: &mut dyn Iterator<Item = corcovado::Token>,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        self.token = token.next().unwrap();
        poll.register(&EventedFd(&self.reader.fd), self.token, interest, poll_opts)?;
        self.exit_token = token.next().unwrap();
        poll.register(
            &EventedFd(&self.exit_pipe_r.as_raw_fd()),
            self.exit_token,
            corcovado::Ready::readable(),
            corcovado::PollOpt::level(),
        )
    }

    fn reregister(
        &mut self,
        poll: &corcovado::Poll,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        // Keep writable interest asserted while our own frame buffer
        // still has bytes: the Machine's write queue may be empty (we
        // reported the input fully consumed) yet the socket blocked
        // mid-flush, so the tail — including a paste's bracketed-paste
        // terminator — must be drained on the next writable event, not
        // wait for the next keystroke.
        let mut interest = interest;
        if !self.writer.pending.is_empty() {
            interest.insert(corcovado::Ready::writable());
        }
        poll.reregister(&EventedFd(&self.reader.fd), self.token, interest, poll_opts)?;
        poll.reregister(
            &EventedFd(&self.exit_pipe_r.as_raw_fd()),
            self.exit_token,
            corcovado::Ready::readable(),
            corcovado::PollOpt::level(),
        )
    }

    fn deregister(&mut self, poll: &corcovado::Poll) -> io::Result<()> {
        let _ = poll.deregister(&EventedFd(&self.reader.fd));
        poll.deregister(&EventedFd(&self.exit_pipe_r.as_raw_fd()))
    }
}

impl EventedPty for RemotePty {
    fn child_event_token(&self) -> corcovado::Token {
        self.exit_token
    }

    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut buf = [0u8; 16];
        unsafe {
            libc::read(
                self.exit_pipe_r.as_raw_fd(),
                buf.as_mut_ptr() as *mut _,
                buf.len(),
            );
        }
        (self.reader.exited.is_some() || self.reader.link_lost)
            .then_some(ChildEvent::Exited)
    }
}

#[derive(Debug)]
pub enum AttachError {
    ShellExited(i32),
    Io(io::Error),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::ShellExited(code) => {
                write!(f, "shell already exited with status {code}")
            }
            AttachError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<io::Error> for AttachError {
    fn from(e: io::Error) -> AttachError {
        AttachError::Io(e)
    }
}

impl RemotePty {
    fn finish_attach(
        transport: Transport,
        ws: &WinsizeBuilder,
        timeout: Duration,
    ) -> Result<(RemotePty, HelloInfo), AttachError> {
        // Hello exchange in blocking mode with a poll() deadline.
        let wfd = transport.write_fd();
        let rfd = transport.read_fd();
        protocol::write_frame(
            &mut FdWriter(wfd),
            FrameType::ClientHello,
            &protocol::encode_client_hello(),
        )?;
        protocol::write_frame(
            &mut FdWriter(wfd),
            FrameType::Resize,
            &protocol::encode_resize(ws.rows, ws.cols, ws.width, ws.height),
        )?;

        let mut dec = Decoder::new();
        let deadline = Instant::now() + timeout;
        let hello: ServerHello = loop {
            match dec.next_frame() {
                Ok(Some((h, payload))) => match FrameType::from_u8(h.typ) {
                    Some(FrameType::ServerHello) => match ServerHello::decode(&payload) {
                        Some(sh) => break sh,
                        None => {
                            return Err(AttachError::Io(io::Error::other(
                                "malformed server hello",
                            )))
                        }
                    },
                    Some(FrameType::Error) => {
                        let msg =
                            String::from_utf8_lossy(payload.get(1..).unwrap_or_default())
                                .into_owned();
                        return Err(AttachError::Io(io::Error::other(msg)));
                    }
                    _ => {
                        return Err(AttachError::Io(io::Error::other(
                            "unexpected frame before hello",
                        )))
                    }
                },
                Ok(None) => {
                    let remain = deadline.saturating_duration_since(Instant::now());
                    if remain.is_zero() {
                        return Err(AttachError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "daemon hello timeout",
                        )));
                    }
                    let mut fds = [libc::pollfd {
                        fd: rfd,
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    let n = unsafe {
                        libc::poll(fds.as_mut_ptr(), 1, remain.as_millis() as i32)
                    };
                    if n == 0 {
                        return Err(AttachError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "daemon hello timeout",
                        )));
                    }
                    if n < 0 {
                        // EINTR (a SIGCHLD from any tab's child fires
                        // constantly in a terminal): retry against the
                        // recomputed deadline. rfd is still BLOCKING here,
                        // so falling through to read() on a spurious wake
                        // would hang the winit loop indefinitely.
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(AttachError::Io(e));
                    }
                    // Read on readiness OR hangup/error: a dead ssh
                    // child reports POLLHUP with no POLLIN, and skipping
                    // it here would busy-spin the UI thread for the whole
                    // timeout. Falling through to read() yields 0/err,
                    // which fails fast with the correct "closed" error.
                    if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)
                        == 0
                    {
                        continue;
                    }
                    // Byte-at-a-time on purpose: everything after the
                    // hello frame (the replay stream) must stay in the
                    // socket buffer so the io loop's epoll sees it the
                    // moment the fd registers — bytes hoarded in our
                    // decoder would wake nothing and leave the pane
                    // blank until fresh output arrives. Hello is ~40
                    // bytes; the syscall cost is irrelevant.
                    let mut tmp = [0u8; 1];
                    let r =
                        unsafe { libc::read(rfd, tmp.as_mut_ptr() as *mut _, tmp.len()) };
                    if r > 0 {
                        dec.feed(&tmp[..r as usize]);
                    } else if r == 0 {
                        return Err(AttachError::Io(io::Error::other(
                            "transport closed during hello",
                        )));
                    } else {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::Interrupted
                            && e.kind() != io::ErrorKind::WouldBlock
                        {
                            return Err(AttachError::Io(e));
                        }
                    }
                }
                Err(e) => return Err(AttachError::Io(io::Error::other(e.to_string()))),
            }
        };

        let info = HelloInfo {
            daemon_pid: hello.daemon_pid,
            shell_pid: hello.shell_pid,
            shell_state: if hello.shell_state == protocol::SHELL_EXITED {
                ShellState::Exited
            } else {
                ShellState::Running
            },
            exit_status: hello.exit_status,
            cwd: parse_hello_cwd(&hello.meta_json),
        };
        if info.shell_state == ShellState::Exited {
            return Err(AttachError::ShellExited(info.exit_status));
        }

        // Live phase: read side nonblocking. For a unix socket the write
        // fd IS the read fd, so it's now nonblocking too; for an ssh
        // child the write fd is a distinct (blocking) stdin pipe — make
        // it nonblocking as well, so a flow-stopped remote (Ctrl-S, a
        // stopped foreground job) can't wedge the io thread inside a
        // blocking write and block Msg::Kill/Shutdown. The RemoteWriter
        // pending-buffer + writable-reregister path drains the tail.
        set_nonblocking(rfd)?;
        if wfd != rfd {
            set_nonblocking(wfd)?;
        }

        let mut pipe = [0i32; 2];
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            return Err(AttachError::Io(io::Error::last_os_error()));
        }
        let (exit_r, exit_w) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        set_nonblocking(exit_r.as_raw_fd())?;
        set_nonblocking(exit_w.as_raw_fd())?;

        Ok((
            RemotePty {
                reader: RemoteReader {
                    fd: rfd,
                    state: ReadState::Header {
                        have: 0,
                        hdr: [0; HEADER_LEN],
                    },
                    exit_pipe_w: exit_w,
                    exit_signaled: false,
                    exited: None,
                    link_lost: false,
                    in_replay: false,
                    replay_dirty: false,
                },
                writer: RemoteWriter {
                    fd: wfd,
                    pending: Vec::new(),
                },
                _transport: transport,
                exit_pipe_r: exit_r,
                token: corcovado::Token(0),
                exit_token: corcovado::Token(0),
            },
            info,
        ))
    }

    pub fn attach_unix(
        socket: &Path,
        ws: &WinsizeBuilder,
        timeout: Duration,
    ) -> Result<(RemotePty, HelloInfo), AttachError> {
        let stream = UnixStream::connect(socket)?;
        Self::finish_attach(Transport::Unix(stream), ws, timeout)
    }

    pub fn attach_ssh(
        dest: &str,
        pane_id: &str,
        ws: &WinsizeBuilder,
        timeout: Duration,
    ) -> Result<(RemotePty, HelloInfo), AttachError> {
        if !rio_ptyd::sockdir::is_valid_pane_id(pane_id) {
            return Err(AttachError::Io(io::Error::other("invalid pane id")));
        }
        let mut child = Command::new("ssh")
            .arg("-oBatchMode=yes")
            .arg(dest)
            .arg("rio-ptyd")
            .arg("attach")
            .arg("--stdio")
            .arg(pane_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Self::finish_attach(
            Transport::Child {
                child,
                stdin,
                stdout,
                graceful: false,
            },
            ws,
            timeout,
        )
    }

    /// Run `rio-ptyd spawn`, parse its handshake line, attach locally.
    pub fn spawn_local(
        program: &str,
        args: &[String],
        cwd: &Option<String>,
        session: Option<&str>,
        ws: &WinsizeBuilder,
        ring_bytes: usize,
    ) -> Result<(RemotePty, HelloInfo, String, PathBuf), AttachError> {
        let mut cmd = Command::new(ptyd_binary());
        cmd.arg("spawn")
            .arg("--ring-size")
            .arg(ring_bytes.to_string());
        if let Some(name) = session {
            cmd.arg("--session").arg(name);
        }
        // Resolve the working directory explicitly. The daemon
        // chdir("/")s itself when it daemonizes, so unlike a plain
        // local pty (which inherits rio's cwd for free) a persistent
        // pane with no configured working-dir would otherwise start the
        // shell in "/". Fall back to rio's own current dir so a fresh
        // pane inherits the launch directory, matching non-persistent
        // panes.
        let resolved_cwd = cwd.clone().or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });
        if let Some(dir) = &resolved_cwd {
            cmd.arg("--cwd").arg(dir);
        }
        cmd.arg("--").arg(program).args(args);
        let out = cmd.output()?;
        if !out.status.success() {
            return Err(AttachError::Io(io::Error::other(format!(
                "rio-ptyd spawn failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))));
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).map_err(io::Error::other)?;
        let pane_id = v["pane_id"].as_str().unwrap_or_default().to_string();
        let socket = PathBuf::from(v["socket"].as_str().unwrap_or_default());
        if pane_id.is_empty() || socket.as_os_str().is_empty() {
            return Err(AttachError::Io(io::Error::other(
                "rio-ptyd spawn returned no socket",
            )));
        }
        let (pty, info) = Self::attach_unix(&socket, ws, Duration::from_secs(2))?;
        Ok((pty, info, pane_id, socket))
    }
}

/// Pull "cwd":"..." out of the ServerHello meta json without a full
/// parse. Returns None for null/absent.
fn parse_hello_cwd(meta: &str) -> Option<String> {
    let key = "\"cwd\":";
    let i = meta.find(key)? + key.len();
    let rest = meta[i..].trim_start();
    if rest.starts_with("null") {
        return None;
    }
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\\"", "\"").replace("\\\\", "\\"))
}

pub fn ptyd_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("rio-ptyd")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("rio-ptyd"))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
