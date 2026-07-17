//! rio-ptyd CLI: spawn | attach [--stdio] | list [--json] | kill | gc

#[cfg(unix)]
fn main() {
    let code = run(std::env::args().skip(1).collect());
    std::process::exit(code);
}

#[cfg(not(unix))]
fn main() {
    eprintln!("rio-ptyd is unix-only");
    std::process::exit(1);
}

#[cfg(unix)]
fn run(args: Vec<String>) -> i32 {
    use rio_ptyd::{attach_cli, daemon, sockdir};

    let usage = || {
        eprintln!(
            "usage:\n  rio-ptyd spawn [--pane-id HEX32] [--cwd DIR] [--session NAME] [--ring-size BYTES] [--env K=V]... -- PROGRAM [ARGS...]\n  rio-ptyd attach [--stdio] [--no-replay] <pane-id | socket.sock>\n  rio-ptyd list [--json] [--full] [--sort session|created|id|pid]\n  rio-ptyd kill <pane-id>\n  rio-ptyd kill-session [--dry-run] <name | --unnamed>\n  rio-ptyd gc [--dry-run]"
        );
        2
    };

    let Some(cmd) = args.first() else {
        return usage();
    };

    match cmd.as_str() {
        "spawn" => {
            let mut it = args[1..].iter().peekable();
            let mut spawn = daemon::SpawnArgs {
                pane_id: None,
                cwd: None,
                ring_size: rio_ptyd::ring::DEFAULT_RING_BYTES,
                env: Vec::new(),
                session: None,
                program: String::new(),
                args: Vec::new(),
            };
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--pane-id" => spawn.pane_id = it.next().cloned(),
                    "--cwd" => spawn.cwd = it.next().cloned(),
                    "--session" => spawn.session = it.next().cloned(),
                    "--ring-size" => {
                        spawn.ring_size = it
                            .next()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(rio_ptyd::ring::DEFAULT_RING_BYTES)
                    }
                    "--env" => {
                        if let Some(kv) = it.next() {
                            if let Some((k, v)) = kv.split_once('=') {
                                spawn.env.push((k.to_string(), v.to_string()));
                            }
                        }
                    }
                    "--" => {
                        spawn.program = it.next().cloned().unwrap_or_default();
                        spawn.args = it.cloned().collect();
                        break;
                    }
                    _ => return usage(),
                }
            }
            if spawn.program.is_empty() {
                spawn.program = std::env::var("SHELL").unwrap_or("/bin/sh".into());
            }
            match daemon::spawn(spawn) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("rio-ptyd spawn: {e}");
                    1
                }
            }
        }
        "attach" => {
            let flags: Vec<&str> = args[1..]
                .iter()
                .take_while(|a| a.starts_with("--"))
                .map(|a| a.as_str())
                .collect();
            let stdio = flags.contains(&"--stdio");
            let no_replay = flags.contains(&"--no-replay");
            let target = args.get(1 + flags.len());
            let Some(target) = target else {
                return usage();
            };
            let res = if stdio {
                attach_cli::attach_stdio(target)
            } else {
                attach_cli::attach_interactive(target, no_replay)
            };
            match res {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("rio-ptyd attach: {e}");
                    1
                }
            }
        }
        "list" => {
            let mut json = false;
            let mut full_id = false;
            let mut sort_key = "session"; // session | created | id | pid
            let mut it = args[1..].iter().peekable();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => json = true,
                    "--full" => full_id = true,
                    "--sort" => match it.next().map(|s| s.as_str()) {
                        Some(k @ ("session" | "created" | "id" | "pid")) => sort_key = k,
                        _ => {
                            eprintln!(
                                "rio-ptyd list: --sort expects session|created|id|pid"
                            );
                            return 2;
                        }
                    },
                    _ => return usage(),
                }
            }
            let base = match sockdir::base_dir() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("rio-ptyd list: {e}");
                    return 1;
                }
            };
            let panes = sockdir::list_panes(&base).unwrap_or_default();
            if json {
                let entries: Vec<serde_json::Value> = panes
                    .iter()
                    .map(|m| {
                        let alive = sockdir::pid_alive(m.daemon_pid);
                        serde_json::json!({
                            "pane_id": m.pane_id,
                            "daemon_pid": m.daemon_pid,
                            "shell_pid": m.shell_pid,
                            "program": m.program,
                            "args": m.args,
                            "cwd": m.cwd,
                            "created_at": m.created_at,
                            "alive": alive,
                            "exited_status": m.exited_status,
                            "session": m.session,
                            // Same liveness guard as the table path: a
                            // dead daemon's shell_pid may already belong
                            // to an unrelated process.
                            "foreground": (m.exited_status.is_none() && alive)
                                .then(|| sockdir::foreground_activity(m.shell_pid))
                                .flatten(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
                );
            } else {
                // Strip control bytes from attacker-influenced fields
                // (comm via PR_SET_NAME, cwd, session) so a crafted name
                // can't inject escape sequences into this terminal.
                let sanitize = |s: &str| -> String {
                    s.chars()
                        .map(|c| if c.is_control() { '?' } else { c })
                        .collect()
                };
                // Show the full 32-hex id when asked (--full) or when two
                // panes share the short prefix (a collision would make the
                // short form ambiguous for kill/attach), so the displayed
                // id is always safe to act on. Otherwise show the short
                // prefix.
                let short_collision = {
                    let mut seen = std::collections::HashSet::new();
                    let n = sockdir::SHORT_PANE_ID_LEN;
                    panes
                        .iter()
                        .any(|m| !seen.insert(&m.pane_id[..n.min(m.pane_id.len())]))
                };
                let id_w = if full_id || short_collision { 32 } else { 8 };

                if !panes.is_empty() {
                    // Header and rows share these column widths so they
                    // line up: pane id (id_w), state (12), pid (right 8),
                    // session (14, the bracketed tag), activity (40), then
                    // created (ISO "YYYY-MM-DD HH:MM") last.
                    println!(
                        "{:<id_w$}  {:<12}  {:>8}  {:<14}  {:<40}  CREATED",
                        "PANE ID", "STATE", "PID", "SESSION", "ACTIVITY"
                    );
                }
                // JSON output stays in creation order for stable machine
                // consumption; the table honors --sort (default: session,
                // grouping tagged sessions first and unnamed last).
                let mut ordered = panes.clone();
                ordered.sort_by(|a, b| {
                    use std::cmp::Ordering;
                    let by_created = a.created_at.cmp(&b.created_at);
                    match sort_key {
                        "created" => by_created,
                        "id" => a.pane_id.cmp(&b.pane_id),
                        "pid" => a.shell_pid.cmp(&b.shell_pid).then(by_created),
                        _ => {
                            let ka = a.session.as_deref().filter(|s| !s.is_empty());
                            let kb = b.session.as_deref().filter(|s| !s.is_empty());
                            match (ka, kb) {
                                (Some(x), Some(y)) => x.cmp(y).then(by_created),
                                (Some(_), None) => Ordering::Less,
                                (None, Some(_)) => Ordering::Greater,
                                (None, None) => by_created,
                            }
                        }
                    }
                });
                for m in &ordered {
                    let alive = sockdir::pid_alive(m.daemon_pid);
                    let state = if let Some(code) = m.exited_status {
                        format!("exited({code})")
                    } else if alive {
                        "running".into()
                    } else {
                        "dead".into()
                    };
                    // Show what the pane is doing: the foreground
                    // program when one is running, else the cwd (an
                    // idle prompt). Only a live daemon has a foreground
                    // — for a dead one shell_pid may already be reused
                    // by an unrelated process.
                    let activity = (m.exited_status.is_none() && alive)
                        .then(|| sockdir::foreground_activity(m.shell_pid))
                        .flatten()
                        .or_else(|| m.cwd.clone())
                        .map(|s| sanitize(&s))
                        .unwrap_or_else(|| "-".into());
                    let created = sockdir::format_epoch_local(m.created_at);
                    let id = &m.pane_id[..id_w.min(m.pane_id.len())];
                    let session =
                        format!("[{}]", sanitize(m.session.as_deref().unwrap_or("-")));
                    println!(
                        "{id:<id_w$}  {state:<12}  {:>8}  {session:<14}  {activity:<40}  {created}",
                        m.shell_pid,
                    );
                }
            }
            0
        }
        "kill" => {
            let Some(id) = args.get(1) else {
                return usage();
            };
            let base = match sockdir::base_dir() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("rio-ptyd kill: {e}");
                    return 1;
                }
            };
            // Accept the full id or the short prefix `list` shows.
            let Some(full) = sockdir::resolve_pane_id(&base, id) else {
                eprintln!("rio-ptyd kill: no unique pane matches '{id}'");
                return 2;
            };
            kill_pane(&base, &full);
            0
        }
        "kill-session" => {
            // rio-ptyd kill-session [--dry-run] <name | --unnamed>
            let mut dry = false;
            let mut unnamed = false;
            let mut name: Option<String> = None;
            for a in &args[1..] {
                match a.as_str() {
                    "--dry-run" => dry = true,
                    "--unnamed" => unnamed = true,
                    other => name = Some(other.to_string()),
                }
            }
            if unnamed == name.is_some() {
                // Neither, or both, a name and --unnamed were given.
                eprintln!(
                    "rio-ptyd kill-session: give exactly one of <name> or --unnamed"
                );
                return 2;
            }
            let base = match sockdir::base_dir() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("rio-ptyd kill-session: {e}");
                    return 1;
                }
            };
            // An empty or absent session tag counts as unnamed.
            let want = |m: &sockdir::PaneMeta| -> bool {
                let tag = m.session.as_deref().filter(|s| !s.is_empty());
                match &name {
                    Some(n) => tag == Some(n.as_str()),
                    None => tag.is_none(),
                }
            };
            for m in sockdir::list_panes(&base).unwrap_or_default() {
                if !want(&m) {
                    continue;
                }
                if dry {
                    println!("would kill {}", m.pane_id);
                } else {
                    kill_pane(&base, &m.pane_id);
                }
            }
            0
        }
        "gc" => {
            let dry = args.get(1).map(|a| a == "--dry-run").unwrap_or(false);
            let base = match sockdir::base_dir() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("rio-ptyd gc: {e}");
                    return 1;
                }
            };
            let now = sockdir::now_epoch();
            for m in sockdir::list_panes(&base).unwrap_or_default() {
                let daemon_dead = !sockdir::pid_alive(m.daemon_pid);
                let old_enough = now.saturating_sub(m.created_at) > 60;
                let sock = sockdir::socket_path(&base, &m.pane_id);
                let unreachable = std::os::unix::net::UnixStream::connect(&sock).is_err();

                // Case 1: the daemon process itself is gone, leaving only
                // stale socket/meta files. Unlink them.
                if daemon_dead && unreachable && old_enough {
                    if dry {
                        println!("would remove {} (dead daemon)", m.pane_id);
                    } else if std::os::unix::net::UnixStream::connect(&sock).is_err() {
                        // Re-check liveness right before unlinking: on
                        // pane-id reuse a fresh daemon may have bound this
                        // socket since the scan above, and removing its
                        // files would orphan a healthy pane.
                        sockdir::remove_pane_files(&base, &m.pane_id);
                    }
                    continue;
                }

                // Case 2: the shell exited but the daemon is still alive,
                // lingering with its ring for a reattach that may never
                // come. Reap it so it does not leak a process + PTY
                // indefinitely. Only when reachable (so we can ask it to
                // exit cleanly) and long enough after the EXIT — gc runs
                // at every rio startup, and a detached job that finished
                // moments ago must keep its ring for the reattach that
                // reads its output, however old the pane itself is.
                if m.exited_status.is_some() && !unreachable && linger_reap_due(&m, now) {
                    if dry {
                        println!("would reap {} (exited, lingering)", m.pane_id);
                    } else {
                        kill_pane(&base, &m.pane_id);
                    }
                }
            }

            // Case 3: a bound socket with no metadata file. The daemon
            // binds before it writes meta, so one that dies in between
            // leaves a socket list_panes never sees. Reap only when it
            // is both unconnectable (no daemon owns it) and old by its
            // own mtime — the bind-to-meta window of a healthy daemon
            // is milliseconds, never a minute.
            if let Ok(entries) = std::fs::read_dir(&base) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".sock"))
                    else {
                        continue;
                    };
                    if !sockdir::is_valid_pane_id(id)
                        || sockdir::meta_path(&base, id).exists()
                    {
                        continue;
                    }
                    let old_enough = entry
                        .metadata()
                        .and_then(|md| md.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age.as_secs() > 60);
                    if !old_enough
                        || std::os::unix::net::UnixStream::connect(entry.path()).is_ok()
                    {
                        continue;
                    }
                    if dry {
                        println!("would remove {id} (orphaned socket)");
                    } else {
                        sockdir::remove_pane_files(&base, id);
                    }
                }
            }
            0
        }
        _ => usage(),
    }
}

/// How long a lingering exited daemon keeps its ring before gc reaps
/// it, measured from the shell's exit. Generous on purpose: rio runs
/// gc at every startup, and the whole point of lingering is that the
/// user can still come back and read a finished job's output.
#[cfg(unix)]
const LINGER_REAP_GRACE_SECS: u64 = 300;

/// Whether gc may reap a lingering exited daemon. Grace runs from
/// `exited_at`; metadata from older daemons lacks the stamp, so fall
/// back to spawn age with the same grace — such panes stay reapable
/// without a fresh exit being reaped on the next gc pass.
#[cfg(unix)]
fn linger_reap_due(m: &rio_ptyd::sockdir::PaneMeta, now: u64) -> bool {
    let since = match m.exited_at {
        Some(t) => now.saturating_sub(t),
        None => now.saturating_sub(m.created_at),
    };
    since > LINGER_REAP_GRACE_SECS
}

/// Terminate one pane's daemon and remove its files. Prefers the
/// protocol (ClientHello + Kill), which lets the daemon flush its client
/// and exit cleanly; falls back to signalling the recorded pids (only
/// when they still match the daemon we started, so a recycled pid is not
/// hit) and unlinking the socket/meta. Shared by the `kill` command and
/// `gc`'s reaping of lingering exited daemons.
#[cfg(unix)]
fn kill_pane(base: &std::path::Path, id: &str) {
    use rio_ptyd::sockdir;
    let sock = sockdir::socket_path(base, id);
    let killed = std::os::unix::net::UnixStream::connect(&sock)
        .and_then(|mut s| {
            use std::io::Read;
            use std::time::Duration;
            // Bound every socket op: a wedged or protocol-incompatible
            // daemon must never hang the caller. Without this, killing a
            // whole session (many panes in sequence) froze forever on the
            // first daemon that accepted the connection but never closed
            // it. On timeout we fall through to the signal+unlink path.
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            s.set_write_timeout(Some(Duration::from_secs(2)))?;
            rio_ptyd::protocol::write_frame(
                &mut s,
                rio_ptyd::protocol::FrameType::ClientHello,
                &rio_ptyd::protocol::encode_client_hello(),
            )?;
            rio_ptyd::protocol::write_frame(
                &mut s,
                rio_ptyd::protocol::FrameType::Kill,
                &[],
            )?;
            // A daemon that actually processed the Kill exits and closes
            // the socket. Read until EOF so we don't treat a write into a
            // doomed connection as success and skip the fallback; drain to
            // a small scratch buffer. A read timeout surfaces as an error
            // (WouldBlock/TimedOut) and drops us into the fallback.
            let mut scratch = [0u8; 256];
            loop {
                match s.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
        .is_ok();
    if !killed {
        if let Ok(meta) = sockdir::read_meta(base, id) {
            // The pids in stale metadata may have been recycled by
            // unrelated same-uid processes; only signal one that still
            // matches the daemon we recorded.
            unsafe {
                if sockdir::pid_matches_start(meta.shell_pid, meta.created_at) {
                    libc::kill(meta.shell_pid, libc::SIGHUP);
                }
                if sockdir::pid_matches_start(meta.daemon_pid, meta.created_at) {
                    libc::kill(meta.daemon_pid, libc::SIGTERM);
                }
            }
        }
        sockdir::remove_pane_files(base, id);
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use rio_ptyd::sockdir::{PaneMeta, METADATA_VERSION};

    fn meta(created_at: u64, exited_at: Option<u64>) -> PaneMeta {
        PaneMeta {
            version: METADATA_VERSION,
            pane_id: "a".into(),
            daemon_pid: 1,
            shell_pid: 2,
            program: "/bin/sh".into(),
            args: Vec::new(),
            cwd: None,
            created_at,
            exited_status: Some(0),
            exited_at,
            session: None,
        }
    }

    #[test]
    fn linger_reap_grace_runs_from_exit() {
        let now = 100_000;
        // Old pane, shell exited just now: not reapable yet.
        assert!(!linger_reap_due(&meta(now - 10_000, Some(now - 5)), now));
        // Exit past the grace: reapable.
        assert!(linger_reap_due(
            &meta(now - 10_000, Some(now - LINGER_REAP_GRACE_SECS - 1)),
            now
        ));
        assert!(!linger_reap_due(
            &meta(now - 10_000, Some(now - LINGER_REAP_GRACE_SECS)),
            now
        ));
    }

    #[test]
    fn linger_reap_without_exit_stamp_falls_back_to_spawn_age() {
        let now = 100_000;
        assert!(!linger_reap_due(&meta(now - 30, None), now));
        assert!(linger_reap_due(
            &meta(now - LINGER_REAP_GRACE_SECS - 1, None),
            now
        ));
    }
}
