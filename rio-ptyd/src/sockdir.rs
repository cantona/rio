//! Per-uid socket directory: resolution, validation, pane metadata.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const METADATA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PaneMeta {
    pub version: u32,
    pub pane_id: String,
    pub daemon_pid: i32,
    pub shell_pid: i32,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub created_at: u64,
    pub exited_status: Option<i32>,
    /// Session name the spawning rio tagged this pane with (e.g.
    /// "com", "work"), so `list` can group panes by session. Absent
    /// for panes spawned outside a named session or by older daemons.
    #[serde(default)]
    pub session: Option<String>,
}

/// `$XDG_RUNTIME_DIR/rio-ptyd`, else `${TMPDIR:-/tmp}/rio-ptyd-<uid>`.
/// Created 0700; validated tmux-style on every use (must be a dir we
/// own with no group/world permissions).
pub fn base_dir() -> io::Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(run) if !run.is_empty() => PathBuf::from(run).join("rio-ptyd"),
        _ => {
            let tmp = std::env::var_os("TMPDIR")
                .filter(|t| !t.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            let uid = unsafe { libc::getuid() };
            tmp.join(format!("rio-ptyd-{uid}"))
        }
    };
    ensure_secure_dir(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn ensure_secure_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = fs::symlink_metadata(dir)?;
    if !md.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if md.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::other(format!(
            "{} has unsafe ownership",
            dir.display()
        )));
    }
    if md.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} has unsafe permissions (group/world access)",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secure_dir(_dir: &Path) -> io::Result<()> {
    Err(io::Error::other("rio-ptyd is unix-only"))
}

pub fn is_valid_pane_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub fn new_pane_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| io::Read::read_exact(&mut f, &mut bytes))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn socket_path(base: &Path, pane_id: &str) -> PathBuf {
    base.join(format!("{pane_id}.sock"))
}

pub fn meta_path(base: &Path, pane_id: &str) -> PathBuf {
    base.join(format!("{pane_id}.json"))
}

pub fn write_meta(base: &Path, meta: &PaneMeta) -> io::Result<()> {
    let tmp = base.join(format!("{}.json.tmp", meta.pane_id));
    fs::write(&tmp, serde_json::to_vec(meta).map_err(io::Error::other)?)?;
    fs::rename(&tmp, meta_path(base, &meta.pane_id))
}

pub fn read_meta(base: &Path, pane_id: &str) -> io::Result<PaneMeta> {
    let bytes = fs::read(meta_path(base, pane_id))?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub fn remove_pane_files(base: &Path, pane_id: &str) {
    let _ = fs::remove_file(socket_path(base, pane_id));
    let _ = fs::remove_file(meta_path(base, pane_id));
}

/// All pane ids that have a metadata file, sorted by creation time.
pub fn list_panes(base: &Path) -> io::Result<Vec<PaneMeta>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(id) = name.strip_suffix(".json") {
            if is_valid_pane_id(id) {
                if let Ok(meta) = read_meta(base, id) {
                    out.push(meta);
                }
            }
        }
    }
    out.sort_by_key(|m| m.created_at);
    Ok(out)
}

pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// True when `pid` still looks like the process the pane recorded, so a
/// blind signal can't hit an unrelated same-uid process that recycled
/// the pid. A recycled pid started long after the pane's `created_at`;
/// the pane's own daemon/shell started at or before it. Linux reads the
/// process start-time from `/proc`; other platforms only check liveness.
pub fn pid_matches_start(pid: i32, created_at: u64) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        match proc_start_epoch(pid) {
            // A generous slack absorbs clock skew and the gap between
            // fork and the meta write; a recycled pid is off by far more.
            Some(start) => start <= created_at.saturating_add(5),
            // Can't read start-time: fall back to liveness alone.
            None => true,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = created_at;
        true
    }
}

/// Wall-clock epoch (seconds) at which `pid` started, from
/// `/proc/<pid>/stat` field 22 (starttime, in clock ticks since boot)
/// plus the system boot time from `/proc/stat` `btime`.
#[cfg(target_os = "linux")]
fn proc_start_epoch(pid: i32) -> Option<u64> {
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

/// Name of the program currently in the foreground of `shell_pid`'s
/// controlling terminal, or None when the shell itself is foreground
/// (idle prompt) or nothing can be read. Lets `list` show "vim"/"ssh"
/// instead of the login shell when a pane is busy. Linux-only
/// (`/proc`); other platforms fall back to the cwd at the call site.
pub fn foreground_activity(shell_pid: i32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // stat field 8 (1-indexed) is tpgid: the foreground process
        // group of the process's controlling terminal. comm (field 2)
        // is parenthesized and may contain spaces/parens, so split on
        // the LAST ')' to reach the space-delimited tail; after it the
        // fields are state(3) ppid(4) pgrp(5) session(6) tty_nr(7)
        // tpgid(8), so tpgid is the 6th token (index 5).
        let stat = fs::read_to_string(format!("/proc/{shell_pid}/stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 1..];
        let tpgid: i32 = tail.split_whitespace().nth(5)?.parse().ok()?;
        if tpgid <= 0 || tpgid == shell_pid {
            return None;
        }
        // The foreground pgid equals its leader's pid; read that
        // leader's comm. If it is the shell itself, treat as idle.
        let leader = fs::read_to_string(format!("/proc/{tpgid}/comm")).ok()?;
        let leader = leader.trim();
        if leader.is_empty() {
            return None;
        }
        Some(leader.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = shell_pid;
        None
    }
}

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_id_validation() {
        assert!(is_valid_pane_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_pane_id("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_valid_pane_id("short"));
        assert!(!is_valid_pane_id("../../../../etc/passwd00000000000"));
        let id = new_pane_id().unwrap();
        assert!(is_valid_pane_id(&id));
    }

    #[test]
    fn meta_session_defaults_when_absent() {
        // Metadata written by an older daemon has no `session` key;
        // #[serde(default)] must decode it as None, not error.
        let legacy = r#"{"version":1,"pane_id":"a","daemon_pid":1,
            "shell_pid":2,"program":"/bin/sh","args":[],"cwd":null,
            "created_at":0,"exited_status":null}"#;
        let m: PaneMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(m.session, None);

        let tagged = r#"{"version":1,"pane_id":"a","daemon_pid":1,
            "shell_pid":2,"program":"/bin/sh","args":[],"cwd":null,
            "created_at":0,"exited_status":null,"session":"work"}"#;
        let m: PaneMeta = serde_json::from_str(tagged).unwrap();
        assert_eq!(m.session.as_deref(), Some("work"));
    }

    #[test]
    fn foreground_of_self_is_idle() {
        // Our own process is its terminal's foreground (or has no tty
        // in CI); either way it must not report a foreign program.
        let me = std::process::id() as i32;
        assert_eq!(foreground_activity(me), None);
    }
}
