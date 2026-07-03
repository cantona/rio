//! Client-side attach: interactive (raw local tty) and `--stdio`
//! (dumb byte relay for remote transports like ssh).

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use crate::protocol::{self, Decoder, FrameType};
use crate::sockdir;

pub fn resolve_socket(target: &str) -> io::Result<PathBuf> {
    if sockdir::is_valid_pane_id(target) {
        Ok(sockdir::socket_path(&sockdir::base_dir()?, target))
    } else if target.ends_with(".sock") && Path::new(target).exists() {
        Ok(PathBuf::from(target))
    } else {
        Err(io::Error::other(
            "expected a 32-hex pane id or an existing .sock path",
        ))
    }
}

/// `attach --stdio`: splice bytes between our stdin/stdout and the
/// pane socket, both directions, until either side EOFs. No frame
/// parsing — the real client sits at the far end of the pipe.
pub fn attach_stdio(target: &str) -> io::Result<i32> {
    let sock = resolve_socket(target)?;
    let stream = UnixStream::connect(sock)?;
    stream.set_nonblocking(true)?;

    let stdin_fd = io::stdin().as_raw_fd();
    let stdout_fd = io::stdout().as_raw_fd();
    set_nonblocking(stdin_fd)?;

    let sfd = stream.as_raw_fd();
    let mut to_sock: Vec<u8> = Vec::new();
    let mut to_stdout: Vec<u8> = Vec::new();
    let mut stdin_open = true;

    loop {
        let mut fds = [
            pollfd(
                stdin_fd,
                if stdin_open && to_sock.len() < 1 << 20 {
                    libc::POLLIN
                } else {
                    0
                },
            ),
            pollfd(sfd, {
                let mut e = libc::POLLIN;
                if !to_sock.is_empty() {
                    e |= libc::POLLOUT;
                }
                e
            }),
            pollfd(
                stdout_fd,
                if to_stdout.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
            ),
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_fd(stdin_fd, &mut to_sock) {
                Ok(0) => stdin_open = false, // ssh closed our stdin
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => stdin_open = false,
            }
        }
        if fds[1].revents & libc::POLLOUT != 0 {
            write_drain(sfd, &mut to_sock)?;
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_fd(sfd, &mut to_stdout) {
                Ok(0) => {
                    // daemon gone: flush what we have and stop
                    flush_all(stdout_fd, &mut to_stdout)?;
                    return Ok(0);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return Ok(0),
            }
        }
        if !to_stdout.is_empty() {
            let _ = write_drain(stdout_fd, &mut to_stdout);
        }
        if !stdin_open && to_sock.is_empty() {
            // Upstream is gone and everything is flushed: detach.
            return Ok(0);
        }
    }
}

/// Interactive attach from a plain terminal: raw mode, frames spoken
/// locally. Detach with Ctrl-\ Ctrl-\ (double SIGQUIT-key literal).
pub fn attach_interactive(target: &str, no_replay: bool) -> io::Result<i32> {
    let sock = resolve_socket(target)?;
    let mut stream = UnixStream::connect(sock)?;

    let features = if no_replay {
        protocol::FEATURE_NO_REPLAY
    } else {
        0
    };
    protocol::write_frame(
        &mut stream,
        FrameType::ClientHello,
        &protocol::encode_client_hello_with(features),
    )?;
    if let Some((rows, cols)) = tty_size() {
        protocol::write_frame(
            &mut stream,
            FrameType::Resize,
            &protocol::encode_resize(rows, cols, 0, 0),
        )?;
    }

    let raw = RawTty::enable()?;
    let result = interactive_loop(&mut stream);
    drop(raw);
    // Undo any terminal modes the replayed stream switched on — the
    // pane's application state (mouse, alt screen, paste, keypad)
    // must not leak into the console we return to.
    const TTY_RESET: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\x1b[?25h\x1b>";
    let _ = io::stdout().write_all(TTY_RESET);
    let _ = io::stdout().flush();
    match &result {
        Ok(code) => {
            if *code >= 0 {
                eprintln!("\n[rio-ptyd: shell exited with status {code}]");
            } else {
                eprintln!("\n[rio-ptyd: detached]");
            }
        }
        Err(e) => eprintln!("\n[rio-ptyd: {e}]"),
    }
    result.map(|c| c.max(0))
}

fn interactive_loop(stream: &mut UnixStream) -> io::Result<i32> {
    stream.set_nonblocking(true)?;
    // stdin stays BLOCKING: fds 0/1/2 usually share one tty open-file
    // description, so O_NONBLOCK on stdin would poison stdout writes
    // with EAGAIN mid-replay. poll() gates every single read instead.
    let stdin_fd = io::stdin().as_raw_fd();
    let sfd = stream.as_raw_fd();
    let mut dec = Decoder::new();
    let mut out = io::stdout();
    let mut prev_quit = false;
    let mut pending_out: Vec<u8> = Vec::new();

    loop {
        let mut fds = [
            pollfd(stdin_fd, libc::POLLIN),
            pollfd(sfd, {
                let mut e = libc::POLLIN;
                if !pending_out.is_empty() {
                    e |= libc::POLLOUT;
                }
                e
            }),
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 4096];
            // Single blocking read: poll guaranteed readiness, and a
            // tty read returns whatever is available without blocking.
            match read_raw(stdin_fd, &mut buf) {
                Ok(0) => {
                    let _ = protocol::write_frame(stream, FrameType::Detach, &[]);
                    return Ok(-1);
                }
                Ok(n) => {
                    let data = &buf[..n];
                    // Detach chord: two consecutive Ctrl-\ bytes.
                    for &b in data {
                        if b == 0x1C {
                            if prev_quit {
                                let _ =
                                    protocol::write_frame(stream, FrameType::Detach, &[]);
                                return Ok(-1);
                            }
                            prev_quit = true;
                        } else {
                            prev_quit = false;
                        }
                    }
                    queue_frame(&mut pending_out, FrameType::Stdin, data);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    // Possibly SIGWINCH via poll EINTR: resend size.
                    if let Some((rows, cols)) = tty_size() {
                        queue_frame(
                            &mut pending_out,
                            FrameType::Resize,
                            &protocol::encode_resize(rows, cols, 0, 0),
                        );
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }

        if fds[1].revents & libc::POLLOUT != 0 && !pending_out.is_empty() {
            write_drain(sfd, &mut pending_out)?;
        }

        if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buf = [0u8; 65536];
            loop {
                match read_raw(sfd, &mut buf) {
                    Ok(0) => return Ok(-1), // link lost
                    Ok(n) => dec.feed(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Ok(-1),
                }
            }
            loop {
                match dec.next_frame() {
                    Ok(Some((h, payload))) => match FrameType::from_u8(h.typ) {
                        Some(FrameType::Output) => out.write_all(&payload)?,
                        Some(FrameType::Exited) => {
                            out.flush()?;
                            let code = payload
                                .get(..4)
                                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                                .unwrap_or(-1);
                            return Ok(code);
                        }
                        Some(FrameType::Detached) => return Ok(-1),
                        Some(FrameType::Error) => {
                            let msg =
                                String::from_utf8_lossy(&payload[1.min(payload.len())..])
                                    .into_owned();
                            return Err(io::Error::other(msg));
                        }
                        _ => {}
                    },
                    Ok(None) => break,
                    Err(e) => return Err(io::Error::other(e.to_string())),
                }
            }
            out.flush()?;
        }
    }
}

fn queue_frame(buf: &mut Vec<u8>, typ: FrameType, payload: &[u8]) {
    struct W<'a>(&'a mut Vec<u8>);
    impl io::Write for W<'_> {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let _ = protocol::write_frame(&mut W(buf), typ, payload);
}

struct RawTty {
    orig: libc::termios,
}

/// Original termios for the signal path: Drop never runs when an
/// external SIGTERM/SIGHUP kills the process, which would strand the
/// user's terminal in raw mode until `stty sane`.
static SIGNAL_RESTORE: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

extern "C" fn restore_tty_and_exit(sig: libc::c_int) {
    if let Some(orig) = SIGNAL_RESTORE.get() {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, orig);
        }
        const RESET: &[u8] = b"\x1b[?1049l\x1b[?25h";
        unsafe {
            libc::write(1, RESET.as_ptr() as *const _, RESET.len());
        }
    }
    unsafe { libc::_exit(128 + sig) }
}

impl RawTty {
    fn enable() -> io::Result<RawTty> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(io::Error::other("stdin is not a tty"));
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            let _ = SIGNAL_RESTORE.set(orig);
            let handler = restore_tty_and_exit as extern "C" fn(libc::c_int);
            // SIGINT too: raw mode makes keyboard ^C a literal byte, but
            // an external `kill -INT` would otherwise take the default
            // disposition and leave the terminal stuck in raw mode.
            for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
                libc::signal(sig, handler as usize as libc::sighandler_t);
            }
            Ok(RawTty { orig })
        }
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}

fn tty_size() -> Option<(u16, u16)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
            Some((ws.ws_row, ws.ws_col))
        } else {
            None
        }
    }
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    crate::pty::set_nonblocking(fd)
}

fn pollfd(fd: i32, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn read_raw(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn read_fd(fd: i32, into: &mut Vec<u8>) -> io::Result<usize> {
    let mut buf = [0u8; 65536];
    let n = read_raw(fd, &mut buf)?;
    into.extend_from_slice(&buf[..n]);
    Ok(n)
}

fn write_drain(fd: i32, buf: &mut Vec<u8>) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
        if n > 0 {
            buf.drain(..n as usize);
        } else {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                break;
            }
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
    }
    Ok(())
}

fn flush_all(fd: i32, buf: &mut Vec<u8>) -> io::Result<()> {
    while !buf.is_empty() {
        write_drain(fd, buf)?;
        if !buf.is_empty() {
            let mut fds = [pollfd(fd, libc::POLLOUT)];
            unsafe { libc::poll(fds.as_mut_ptr(), 1, 1000) };
        }
    }
    Ok(())
}
