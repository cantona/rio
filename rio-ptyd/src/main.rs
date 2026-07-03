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
            "usage:\n  rio-ptyd spawn [--pane-id HEX32] [--cwd DIR] [--session NAME] [--ring-size BYTES] [--env K=V]... -- PROGRAM [ARGS...]\n  rio-ptyd attach [--stdio] [--no-replay] <pane-id | socket.sock>\n  rio-ptyd list [--json]\n  rio-ptyd kill <pane-id>\n  rio-ptyd gc [--dry-run]"
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
            let json = args.get(1).map(|a| a == "--json").unwrap_or(false);
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
                        serde_json::json!({
                            "pane_id": m.pane_id,
                            "daemon_pid": m.daemon_pid,
                            "shell_pid": m.shell_pid,
                            "program": m.program,
                            "args": m.args,
                            "cwd": m.cwd,
                            "created_at": m.created_at,
                            "alive": sockdir::pid_alive(m.daemon_pid),
                            "exited_status": m.exited_status,
                            "session": m.session,
                            "foreground": m
                                .exited_status
                                .is_none()
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
                for m in &panes {
                    let state = if let Some(code) = m.exited_status {
                        format!("exited({code})")
                    } else if sockdir::pid_alive(m.daemon_pid) {
                        "running".into()
                    } else {
                        "dead".into()
                    };
                    // Show what the pane is doing: the foreground
                    // program when one is running, else the cwd (an
                    // idle prompt). Exited panes have no live foreground.
                    let activity = m
                        .exited_status
                        .is_none()
                        .then(|| sockdir::foreground_activity(m.shell_pid))
                        .flatten()
                        .or_else(|| m.cwd.clone())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{}  {:8}  pid {:>7}  [{}]  {}",
                        m.pane_id,
                        state,
                        m.shell_pid,
                        m.session.as_deref().unwrap_or("-"),
                        activity,
                    );
                }
            }
            0
        }
        "kill" => {
            let Some(id) = args.get(1) else {
                return usage();
            };
            if !sockdir::is_valid_pane_id(id) {
                eprintln!("rio-ptyd kill: invalid pane id");
                return 2;
            }
            let base = match sockdir::base_dir() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("rio-ptyd kill: {e}");
                    return 1;
                }
            };
            // Prefer the protocol (lets the daemon clean up and exit).
            let sock = sockdir::socket_path(&base, id);
            let killed = std::os::unix::net::UnixStream::connect(&sock)
                .and_then(|mut s| {
                    rio_ptyd::protocol::write_frame(
                        &mut s,
                        rio_ptyd::protocol::FrameType::ClientHello,
                        &rio_ptyd::protocol::encode_client_hello(),
                    )?;
                    rio_ptyd::protocol::write_frame(
                        &mut s,
                        rio_ptyd::protocol::FrameType::Kill,
                        &[],
                    )
                })
                .is_ok();
            if !killed {
                if let Ok(meta) = sockdir::read_meta(&base, id) {
                    unsafe {
                        libc::kill(meta.shell_pid, libc::SIGHUP);
                        libc::kill(meta.daemon_pid, libc::SIGTERM);
                    }
                }
                sockdir::remove_pane_files(&base, id);
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
                if daemon_dead && unreachable && old_enough {
                    if dry {
                        println!("would remove {}", m.pane_id);
                    } else {
                        sockdir::remove_pane_files(&base, &m.pane_id);
                    }
                }
            }
            0
        }
        _ => usage(),
    }
}
