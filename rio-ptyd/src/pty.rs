//! PTY allocation + shell fork/exec. Self-contained (libc only).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub struct PtyChild {
    pub master: OwnedFd,
    pub shell_pid: i32,
}

pub struct SpawnSpec<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub rows: u16,
    pub cols: u16,
}

fn cerr(context: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{context}: {e}"))
}

/// Open a PTY pair and fork the shell onto the slave side. The child
/// becomes a session leader with the slave as controlling terminal,
/// resets signal dispositions, closes every inherited fd above stderr,
/// applies env/cwd, and execs. Never returns in the child.
pub fn spawn_shell(spec: &SpawnSpec) -> io::Result<PtyChild> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(cerr("posix_openpt"));
        }
        let master = OwnedFd::from_raw_fd(master);
        if libc::grantpt(master.as_raw_fd()) != 0 {
            return Err(cerr("grantpt"));
        }
        if libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(cerr("unlockpt"));
        }
        let slave_name = {
            let p = libc::ptsname(master.as_raw_fd());
            if p.is_null() {
                return Err(cerr("ptsname"));
            }
            std::ffi::CStr::from_ptr(p).to_owned()
        };

        let ws = libc::winsize {
            ws_row: spec.rows,
            ws_col: spec.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);

        // Prepare exec arguments before forking (no allocation after).
        let prog = CString::new(spec.program).map_err(io::Error::other)?;
        let mut argv_owned = vec![prog.clone()];
        for a in spec.args {
            argv_owned.push(CString::new(a.as_str()).map_err(io::Error::other)?);
        }
        let mut argv: Vec<*const libc::c_char> =
            argv_owned.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let cwd = spec
            .cwd
            .map(|c| CString::new(c).map_err(io::Error::other))
            .transpose()?;
        let env_owned: Vec<(CString, CString)> = spec
            .env
            .iter()
            .map(|(k, v)| {
                Ok((
                    CString::new(k.as_str()).map_err(io::Error::other)?,
                    CString::new(v.as_str()).map_err(io::Error::other)?,
                ))
            })
            .collect::<io::Result<_>>()?;

        // Block signals across the fork (tmux discipline).
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_BLOCK, &all, &mut old);

        let pid = libc::fork();
        if pid < 0 {
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            return Err(cerr("fork"));
        }

        if pid == 0 {
            // ---- child: only async-signal-safe calls from here ----
            libc::setsid();
            let slave = libc::open(slave_name.as_ptr(), libc::O_RDWR);
            if slave < 0 {
                libc::_exit(1);
            }
            #[allow(clippy::useless_conversion)]
            if libc::ioctl(slave, libc::TIOCSCTTY.into(), 0) != 0 {
                libc::_exit(1);
            }
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            // Close everything else (listener socket, pipes, master).
            let max_fd = libc::sysconf(libc::_SC_OPEN_MAX).max(1024) as i32;
            for fd in 3..max_fd {
                libc::close(fd);
            }
            // Default signal dispositions + clear mask.
            for sig in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
                libc::SIGPIPE,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());

            for (k, v) in &env_owned {
                libc::setenv(k.as_ptr(), v.as_ptr(), 1);
            }
            if let Some(dir) = &cwd {
                let _ = libc::chdir(dir.as_ptr());
            }
            libc::execvp(prog.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }

        // ---- parent (daemon) ----
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        set_nonblocking(master.as_raw_fd())?;
        Ok(PtyChild {
            master,
            shell_pid: pid,
        })
    }
}

pub fn set_nonblocking(fd: i32) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(cerr("fcntl F_GETFL"));
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(cerr("fcntl F_SETFL"));
        }
    }
    Ok(())
}

pub fn set_winsize(master: i32, rows: u16, cols: u16, xpixel: u16, ypixel: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: xpixel,
        ws_ypixel: ypixel,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
}
