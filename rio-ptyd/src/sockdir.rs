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
    /// When the shell exited (epoch seconds), recorded by the daemon as
    /// it enters the lingering state. Absent in metadata written by
    /// older daemons and while the shell is still running.
    #[serde(default)]
    pub exited_at: Option<u64>,
    /// Session name the spawning rio tagged this pane with (e.g.
    /// "com", "work"), so `list` can group panes by session. Absent
    /// for panes spawned outside a named session or by older daemons.
    #[serde(default)]
    pub session: Option<String>,
}

/// `$XDG_RUNTIME_DIR/rio-ptyd`, else `${TMPDIR:-/tmp}/rio-ptyd-<uid>`.
/// Created 0700; validated on every use (must be a dir we own with no
/// group/world permissions).
pub fn base_dir() -> io::Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(run) if !run.is_empty() => PathBuf::from(run).join("rio-ptyd"),
        _ => {
            let tmp = std::env::var_os("TMPDIR")
                .filter(|t| !t.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            #[cfg(unix)]
            let uid = unsafe { libc::getuid() };
            #[cfg(not(unix))]
            let uid = 0;
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

/// A shorter hex prefix that `list` shows and that resolve_pane_id
/// expands. 8 hex = 32 bits, unique in practice for a per-user set of
/// daemons.
pub const SHORT_PANE_ID_LEN: usize = 8;

fn is_hex_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Expand a full pane id or a unique hex prefix to the full pane id.
/// A full 32-hex id resolves to itself without touching disk. A shorter
/// prefix must match exactly one existing pane, else it is rejected
/// (None): ambiguous prefixes and typos never act on the wrong daemon.
pub fn resolve_pane_id(base: &Path, id_or_prefix: &str) -> Option<String> {
    if is_valid_pane_id(id_or_prefix) {
        return Some(id_or_prefix.to_string());
    }
    if !is_hex_prefix(id_or_prefix) {
        return None;
    }
    let mut hit = None;
    for m in list_panes(base).ok()? {
        if m.pane_id.starts_with(id_or_prefix) {
            if hit.is_some() {
                return None; // ambiguous
            }
            hit = Some(m.pane_id);
        }
    }
    hit
}

/// Unix-only, like the daemon that consumes it: the id's entropy
/// comes from /dev/urandom, and no non-unix code path mints panes.
#[cfg(unix)]
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

/// Daemon stderr log, written only when RIO_PTYD_LOG is set at spawn.
pub fn log_path(base: &Path, pane_id: &str) -> PathBuf {
    base.join(format!("{pane_id}.log"))
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
    let _ = fs::remove_file(log_path(base, pane_id));
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

#[cfg(unix)]
pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: i32) -> bool {
    false
}

/// True when `pid` still looks like the process the pane recorded, so a
/// blind signal can't hit an unrelated same-uid process that recycled
/// the pid. A recycled pid started long after the pane's `created_at`;
/// the pane's own daemon/shell started at or before it. Every supported
/// OS backend reads the start-time; one that can't (or another unix with
/// only the fallback) returns None and we accept liveness alone.
#[cfg(unix)]
pub fn pid_matches_start(pid: i32, created_at: u64) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match crate::osproc::start_epoch(pid) {
        // A generous slack absorbs clock skew and the gap between fork
        // and the meta write; a recycled pid is off by far more.
        Some(start) => start <= created_at.saturating_add(5),
        // Can't read start-time: fall back to liveness alone.
        None => true,
    }
}

#[cfg(not(unix))]
pub fn pid_matches_start(_pid: i32, _created_at: u64) -> bool {
    false
}

/// Name of the program in the foreground of `shell_pid`'s tty, or None
/// (idle prompt / not readable). Delegates to the per-OS backend; on
/// platforms without one `list` falls back to the cwd at the call site.
#[cfg(unix)]
pub fn foreground_activity(shell_pid: i32) -> Option<String> {
    crate::osproc::foreground(shell_pid)
}

#[cfg(not(unix))]
pub fn foreground_activity(_shell_pid: i32) -> Option<String> {
    None
}
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a unix epoch as local ISO 8601 "YYYY-MM-DD HH:MM" for the list
/// display: unambiguous across regions and text-sortable. Uses libc
/// localtime_r/strftime to avoid a time-formatting dependency (the crate
/// keeps to libc + serde). Empty string on failure or on non-unix (the
/// list command is unix-only anyway).
#[cfg(unix)]
pub fn format_epoch_local(epoch: u64) -> String {
    unsafe {
        let t = epoch as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 32];
        let fmt = c"%Y-%m-%d %H:%M";
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

#[cfg(not(unix))]
pub fn format_epoch_local(_epoch: u64) -> String {
    String::new()
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
        #[cfg(unix)]
        {
            let id = new_pane_id().unwrap();
            assert!(is_valid_pane_id(&id));
        }
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
        assert_eq!(m.exited_at, None);

        let tagged = r#"{"version":1,"pane_id":"a","daemon_pid":1,
            "shell_pid":2,"program":"/bin/sh","args":[],"cwd":null,
            "created_at":0,"exited_status":null,"session":"work"}"#;
        let m: PaneMeta = serde_json::from_str(tagged).unwrap();
        assert_eq!(m.session.as_deref(), Some("work"));
    }

    #[test]
    fn meta_exited_at_round_trip() {
        let m = PaneMeta {
            version: METADATA_VERSION,
            pane_id: "a".into(),
            daemon_pid: 1,
            shell_pid: 2,
            program: "/bin/sh".into(),
            args: Vec::new(),
            cwd: None,
            created_at: 100,
            exited_status: Some(0),
            exited_at: Some(160),
            session: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: PaneMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exited_at, Some(160));
        assert_eq!(back.exited_status, Some(0));
    }

    #[test]
    fn foreground_of_self_is_idle() {
        // Our own process is its terminal's foreground (or has no tty
        // in CI); either way it must not report a foreign program.
        let me = std::process::id() as i32;
        assert_eq!(foreground_activity(me), None);
    }
}
