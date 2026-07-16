//! Per-OS process introspection: a process's start time, foreground
//! program, and current working directory. Each is a thin platform
//! backend behind a common signature so the rest of the crate is
//! OS-agnostic. Linux uses /proc (also covers WSL/WSL2 and Cygwin-like
//! /proc layouts); macOS uses proc_pidinfo; the BSDs use sysctl KERN_PROC
//! records; Solaris/illumos use procfs psinfo. Anything else degrades to
//! None (callers fall back to liveness / the recorded cwd).
//!
//! Only the Linux backends are runtime-tested. The macOS/BSD/Solaris
//! backends are written from the documented kernel structures and are
//! compile-checked per target where a std target exists; treat them as
//! best-effort until run on real hardware.

#![cfg(unix)]

#[cfg(any(target_os = "linux", target_os = "solaris", target_os = "illumos"))]
use std::fs;

/// Wall-clock epoch (seconds) at which `pid` started, or None when it
/// can't be read. Used to reject a recycled pid whose start time is far
/// later than the pane's recorded `created_at`.
///
/// Linux: /proc/<pid>/stat field 22 (starttime ticks) + /proc/stat btime.
#[cfg(target_os = "linux")]
pub fn start_epoch(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesized and may hold spaces/parens; split
    // on the last ')' to reach the space-delimited tail. After it the
    // fields are state(3)..starttime(22), so starttime is token index 19.
    let tail = &stat[stat.rfind(')')? + 1..];
    let starttime_ticks: u64 = tail.split_whitespace().nth(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let btime = fs::read_to_string("/proc/stat").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("btime "))
            .and_then(|v| v.trim().parse::<u64>().ok())
    })?;
    Some(btime + starttime_ticks / hz as u64)
}

/// macOS: proc_pidinfo(PROC_PIDTBSDINFO) -> proc_bsdinfo.pbi_start_tvsec
/// (a wall-clock epoch already).
#[cfg(target_os = "macos")]
pub fn start_epoch(pid: i32) -> Option<u64> {
    let info = macos_bsdinfo(pid)?;
    Some(info.pbi_start_tvsec)
}

/// FreeBSD/DragonFly: sysctl KERN_PROC_PID -> kinfo_proc.ki_start (a
/// timeval wall-clock epoch). libc provides the struct + MIB constants.
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
pub fn start_epoch(pid: i32) -> Option<u64> {
    let kp = bsd_kinfo_proc(pid)?;
    Some(kp.ki_start.tv_sec as u64)
}

/// NetBSD: sysctl KERN_PROC2 -> kinfo_proc2.p_ustart_sec (u32 epoch).
#[cfg(target_os = "netbsd")]
pub fn start_epoch(pid: i32) -> Option<u64> {
    let kp = bsd_kinfo_proc2(pid)?;
    Some(kp.p_ustart_sec as u64)
}

/// OpenBSD: sysctl KERN_PROC_PID -> kinfo_proc.p_ustart_sec (u64 epoch).
#[cfg(target_os = "openbsd")]
pub fn start_epoch(pid: i32) -> Option<u64> {
    let kp = bsd_kinfo_proc(pid)?;
    Some(kp.p_ustart_sec)
}

/// Solaris/illumos: procfs psinfo carries pr_start (a wall-clock epoch).
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
pub fn start_epoch(pid: i32) -> Option<u64> {
    solaris_psinfo(pid).map(|p| p.pr_start.tv_sec as u64)
}

/// Any other unix without a specific backend: liveness only.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
pub fn start_epoch(_pid: i32) -> Option<u64> {
    None
}

/// Name of the program currently in the foreground of `shell_pid`'s
/// controlling terminal, or None when the shell itself is foreground
/// (idle prompt) or nothing can be read. Lets `list` show "vim"/"ssh"
/// instead of the login shell when a pane is busy.
///
/// Linux: /proc/<pid>/stat tpgid (field 8) -> /proc/<tpgid>/comm.
#[cfg(target_os = "linux")]
pub fn foreground(shell_pid: i32) -> Option<String> {
    // comm (field 2) is parenthesized and may contain spaces/parens, so
    // split on the LAST ')' to reach the space-delimited tail; after it
    // the fields are state(3) ppid(4) pgrp(5) session(6) tty_nr(7)
    // tpgid(8), so tpgid is the 6th token (index 5).
    let stat = fs::read_to_string(format!("/proc/{shell_pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    let tpgid: i32 = tail.split_whitespace().nth(5)?.parse().ok()?;
    if tpgid <= 0 || tpgid == shell_pid {
        return None;
    }
    let leader = fs::read_to_string(format!("/proc/{tpgid}/comm")).ok()?;
    let leader = leader.trim();
    if leader.is_empty() {
        None
    } else {
        Some(leader.to_string())
    }
}

/// macOS: proc_bsdinfo.e_tpgid is the tty foreground pgrp of shell_pid,
/// the pid-only equivalent of Linux's /proc stat tpgid (the `list`
/// process has no fd to the daemon's pty, so a tcgetpgrp(fd) path doesn't
/// apply here). proc_name() on that pgrp leader gives the program name.
#[cfg(target_os = "macos")]
pub fn foreground(shell_pid: i32) -> Option<String> {
    let info = macos_bsdinfo(shell_pid)?;
    let tpgid = info.e_tpgid as i32;
    if tpgid <= 0 || tpgid == shell_pid {
        return None;
    }
    let mut name = [0u8; 2 * libc::MAXCOMLEN];
    let got =
        unsafe { libc::proc_name(tpgid, name.as_mut_ptr().cast(), name.len() as u32) };
    if got <= 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&name[..got as usize])
        .trim_end_matches('\0')
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The BSDs and Solaris: resolving the tty foreground program from a pid
/// alone means walking the process table for the pane's tty, which is
/// costly for a cosmetic `list` column. Skip it — `list` falls back to
/// the cwd.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn foreground(_shell_pid: i32) -> Option<String> {
    None
}

/// Live working directory of `pid`, or None. `list` and the session
/// capture prefer this over the pane's attach-time cwd.
///
/// Linux: readlink /proc/<pid>/cwd.
#[cfg(target_os = "linux")]
pub fn cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// macOS: proc_pidinfo(PROC_PIDVNODEPATHINFO) -> pvi_cdir.vip_path.
#[cfg(target_os = "macos")]
pub fn cwd(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            sz,
        )
    };
    if n != sz {
        return None;
    }
    cstr_field(unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast(),
            std::mem::size_of_val(&info.pvi_cdir.vip_path),
        )
    })
}

/// Solaris/illumos: the live cwd is the symlink /proc/<pid>/path/cwd.
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
pub fn cwd(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/path/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// FreeBSD: sysctl {CTL_KERN, KERN_PROC, KERN_PROC_CWD, pid} -> a
/// kinfo_file whose kf_path is the cwd.
#[cfg(target_os = "freebsd")]
pub fn cwd(pid: i32) -> Option<String> {
    let mut mib: [libc::c_int; 4] =
        [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_CWD, pid];
    let mut kf: libc::kinfo_file = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::kinfo_file>();
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut kf as *mut libc::kinfo_file).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || len == 0 {
        return None;
    }
    cstr_field(unsafe {
        std::slice::from_raw_parts(
            kf.kf_path.as_ptr().cast(),
            std::mem::size_of_val(&kf.kf_path),
        )
    })
}

/// Other unix (NetBSD/OpenBSD/etc.): no cwd probe wired up; the pane's
/// attach-time cwd and OSC 7 updates cover the common case.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
pub fn cwd(_pid: i32) -> Option<String> {
    None
}

// ---- shared platform helpers ---------------------------------------

/// Interpret a fixed C char buffer as a NUL-terminated path string.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn cstr_field(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

/// macOS proc_bsdinfo for `pid` via proc_pidinfo(PROC_PIDTBSDINFO).
#[cfg(target_os = "macos")]
fn macos_bsdinfo(pid: i32) -> Option<libc::proc_bsdinfo> {
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            sz,
        )
    };
    if n == sz {
        Some(info)
    } else {
        None
    }
}

/// One kinfo_proc for `pid` via sysctl {CTL_KERN, KERN_PROC,
/// KERN_PROC_PID, pid}. FreeBSD/DragonFly and OpenBSD both name the
/// record `kinfo_proc` (fields differ); None on any failure so a wrong
/// assumption degrades instead of reading garbage.
#[cfg(any(target_os = "freebsd", target_os = "dragonfly", target_os = "openbsd"))]
fn bsd_kinfo_proc(pid: i32) -> Option<libc::kinfo_proc> {
    let mut mib: [libc::c_int; 4] =
        [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut kp: libc::kinfo_proc = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::kinfo_proc>();
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut kp as *mut libc::kinfo_proc).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || len == 0 {
        return None;
    }
    Some(kp)
}

/// NetBSD variant: KERN_PROC2 with a 6-element MIB and explicit element
/// size + count, filling one kinfo_proc2.
#[cfg(target_os = "netbsd")]
fn bsd_kinfo_proc2(pid: i32) -> Option<libc::kinfo_proc2> {
    let mut kp: libc::kinfo_proc2 = unsafe { std::mem::zeroed() };
    let elem = std::mem::size_of::<libc::kinfo_proc2>();
    let mut mib: [libc::c_int; 6] = [
        libc::CTL_KERN,
        libc::KERN_PROC2,
        libc::KERN_PROC_PID,
        pid,
        elem as libc::c_int,
        1,
    ];
    let mut len = elem;
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&mut kp as *mut libc::kinfo_proc2).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || len == 0 {
        return None;
    }
    Some(kp)
}

/// Solaris/illumos: read /proc/<pid>/psinfo into a psinfo_t.
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
fn solaris_psinfo(pid: i32) -> Option<libc::psinfo_t> {
    let bytes = fs::read(format!("/proc/{pid}/psinfo")).ok()?;
    if bytes.len() < std::mem::size_of::<libc::psinfo_t>() {
        return None;
    }
    let mut p: libc::psinfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (&mut p as *mut libc::psinfo_t).cast(),
            std::mem::size_of::<libc::psinfo_t>(),
        );
    }
    Some(p)
}
