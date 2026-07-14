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
}
