//! `keep` — run a command on a broker-held PTY so it survives this shell.
//!
//! A kept job's lifetime belongs to the `agshd` broker (see `agsh-broker`),
//! not to the terminal: close the window, lose the SSH connection, crash the
//! shell — the job keeps running, its output journaled, reattachable from any
//! later shell.
//!
//!   keep -- CMD ARGS…    start CMD kept; on a terminal, attach immediately
//!   keep list            kept jobs (id, state, age, command)
//!   keep attach ID       reattach (Ctrl-] detaches; the job keeps running)
//!   keep tail ID [N]     print the last N bytes of the job's output log
//!   keep kill ID [SIG]   signal the job's process group (default TERM)
//!   keep rm ID           drop a finished job from the list
//!   keep stop            stop the broker (hangs up all kept jobs!)

use std::io::IsTerminal;

use agsh_broker::attach::term_size;
use agsh_broker::{attach_interactive, AttachOutcome, Client, JobInfo, JobKind, SpawnSpec};

use crate::state::ShellState;
use crate::CommandOutcome;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ago(from: u64) -> String {
    let secs = unix_now().saturating_sub(from);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn fail(code: i32, message: impl Into<String>) -> CommandOutcome {
    let mut message = message.into();
    message.push('\n');
    CommandOutcome::captured(code, Vec::new(), message.into_bytes())
}

/// A client for an already-running broker, or a friendly error message.
fn existing_broker() -> Result<Client, String> {
    let client = Client::from_env().map_err(|e| format!("keep: {e}"))?;
    if client.ping().is_err() {
        return Err(
            "keep: broker not running — no kept jobs (start one with `keep -- CMD`)".to_string(),
        );
    }
    Ok(client)
}

/// The exported environment a kept job starts with. The spawning session's
/// identity is stripped: the job belongs to the broker now, not this session.
fn job_env(state: &ShellState) -> Vec<(String, String)> {
    state
        .exported_env()
        .iter()
        .filter(|(k, _)| k.as_str() != "AGSH_SESSION")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn builtin_keep(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let first = args.first().map(String::as_str);
    match first {
        None | Some("list") | Some("ls") => list(),
        Some("attach") => match args.get(1) {
            Some(id) => attach(id),
            None => fail(2, "keep attach: which job? (see `keep list`)"),
        },
        Some("tail") => match args.get(1) {
            Some(id) => {
                let bytes = args
                    .get(2)
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(4096);
                tail(id, bytes)
            }
            None => fail(2, "keep tail: which job? (see `keep list`)"),
        },
        Some("kill") => match args.get(1) {
            Some(id) => kill(id, args.get(2).map(String::as_str)),
            None => fail(2, "keep kill: which job? (see `keep list`)"),
        },
        Some("rm") | Some("remove") => match args.get(1) {
            Some(id) => remove(id),
            None => fail(2, "keep rm: which job? (see `keep list`)"),
        },
        Some("status") => match args.get(1) {
            Some(id) => status(id),
            None => fail(2, "keep status: which job? (see `keep list`)"),
        },
        Some("stop") => stop(),
        Some("--") => spawn(&args[1..], state),
        // Anything else is a command to keep. `--` disambiguates a command
        // that happens to be named like a subcommand (`keep -- list`).
        Some(_) => spawn(args, state),
    }
}

fn spawn(cmd: &[String], state: &mut ShellState) -> CommandOutcome {
    if cmd.is_empty() {
        return fail(2, "keep: nothing to run (usage: keep -- CMD ARGS…)");
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return fail(1, format!("keep: cannot find agsh binary: {e}")),
    };
    let client = match Client::connect_or_start(&exe) {
        Ok(client) => client,
        Err(e) => return fail(1, format!("keep: {e}")),
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (rows, cols) = if interactive { term_size() } else { (24, 80) };
    let info = match client.spawn_job(SpawnSpec {
        cmd: cmd.to_vec(),
        cwd: state.cwd().display().to_string(),
        env: job_env(state),
        rows,
        cols,
        kind: JobKind::Job,
        title: cmd.join(" "),
    }) {
        Ok(info) => info,
        Err(e) => return fail(1, format!("keep: {e}")),
    };

    if !interactive {
        return CommandOutcome::captured(
            0,
            format!(
                "keep: [{id}] {title} (pid {pid}) — running detached under the broker\n\
                 keep: output: keep tail {id}   ·   reattach: keep attach {id}\n",
                id = info.id,
                title = info.title,
                pid = info.pid,
            )
            .into_bytes(),
            Vec::new(),
        );
    }

    // On a terminal: attach right away (shpool-style). The hint goes above the
    // job's output so it stays visible when the job takes the screen.
    eprintln!(
        "keep: [{}] started (pid {}) — Ctrl-] detaches; it survives this shell",
        info.id, info.pid
    );
    finish_attach(&client, &info.id, attach_interactive(&client, &info.id))
}

fn attach(id: &str) -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return fail(
            1,
            format!("keep attach: needs a terminal (use `keep tail {id}` for the output)"),
        );
    }
    finish_attach(&client, id, attach_interactive(&client, id))
}

/// Turn an attach result into the builtin's outcome (summary on stdout).
fn finish_attach(
    client: &Client,
    id: &str,
    outcome: std::io::Result<AttachOutcome>,
) -> CommandOutcome {
    match outcome {
        Ok(AttachOutcome::Detached) => CommandOutcome::captured(
            0,
            format!("keep: detached — [{id}] keeps running (reattach: keep attach {id})\n")
                .into_bytes(),
            Vec::new(),
        ),
        Ok(AttachOutcome::Ended) => {
            let code = client
                .status(id)
                .ok()
                .and_then(|info| info.exit_code)
                .unwrap_or(0);
            CommandOutcome::captured(
                code,
                format!("keep: [{id}] exited (code {code})\n").into_bytes(),
                Vec::new(),
            )
        }
        Err(e) => fail(1, format!("keep attach: {e}")),
    }
}

fn render_job(job: &JobInfo) -> String {
    let state = if job.running {
        if job.attached {
            "running*".to_string()
        } else {
            "running".to_string()
        }
    } else {
        format!("exit {}", job.exit_code.unwrap_or(0))
    };
    format!(
        "{:<4} {:<9} {:>4}  {}\n",
        job.id,
        state,
        ago(job.started_at),
        job.title,
    )
}

fn list() -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    match client.list() {
        Ok(jobs) if jobs.is_empty() => CommandOutcome::captured(
            0,
            b"keep: no kept jobs (start one with `keep -- CMD`)\n".to_vec(),
            Vec::new(),
        ),
        Ok(jobs) => {
            let mut out = String::new();
            for job in &jobs {
                out.push_str(&render_job(job));
            }
            out.push_str("(* = attached · keep attach ID · keep tail ID · Ctrl-] detaches)\n");
            CommandOutcome::captured(0, out.into_bytes(), Vec::new())
        }
        Err(e) => fail(1, format!("keep list: {e}")),
    }
}

fn status(id: &str) -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    match client.status(id) {
        Ok(job) => CommandOutcome::captured(0, render_job(&job).into_bytes(), Vec::new()),
        Err(e) => fail(1, format!("keep status: {e}")),
    }
}

fn tail(id: &str, bytes: u64) -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    match client.tail(id, bytes) {
        Ok(bytes) => CommandOutcome::captured(0, bytes, Vec::new()),
        Err(e) => fail(1, format!("keep tail: {e}")),
    }
}

fn kill(id: &str, signal: Option<&str>) -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    let signal = normalize_signal(signal.unwrap_or("TERM"));
    match client.signal(id, &signal) {
        Ok(()) => CommandOutcome::captured(
            0,
            format!("keep: sent SIG{signal} to [{id}]\n").into_bytes(),
            Vec::new(),
        ),
        Err(e) => fail(1, format!("keep kill: {e}")),
    }
}

/// Accept `-9`, `9`, `-KILL`, `KILL`, `SIGKILL` — the broker wants a bare name.
fn normalize_signal(signal: &str) -> String {
    let signal = signal.trim_start_matches('-');
    match signal {
        "1" => "HUP".into(),
        "2" => "INT".into(),
        "3" => "QUIT".into(),
        "9" => "KILL".into(),
        "15" => "TERM".into(),
        "18" | "19" if cfg!(target_os = "linux") => {
            if signal == "18" { "CONT" } else { "STOP" }.into()
        }
        other => other.to_ascii_uppercase(),
    }
}

fn remove(id: &str) -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    match client.remove(id) {
        Ok(()) => CommandOutcome::captured(0, Vec::new(), Vec::new()),
        Err(e) => fail(1, format!("keep rm: {e}")),
    }
}

fn stop() -> CommandOutcome {
    let client = match existing_broker() {
        Ok(client) => client,
        Err(message) => return fail(1, message),
    };
    let running = client
        .list()
        .map(|jobs| jobs.iter().filter(|j| j.running).count())
        .unwrap_or(0);
    match client.shutdown() {
        Ok(()) => {
            let note = if running > 0 {
                format!("keep: broker stopped — {running} running job(s) were hung up\n")
            } else {
                "keep: broker stopped\n".to_string()
            };
            CommandOutcome::captured(0, note.into_bytes(), Vec::new())
        }
        Err(e) => fail(1, format!("keep stop: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_spellings_normalize() {
        assert_eq!(normalize_signal("TERM"), "TERM");
        assert_eq!(normalize_signal("-9"), "KILL");
        assert_eq!(normalize_signal("9"), "KILL");
        assert_eq!(normalize_signal("-KILL"), "KILL");
        assert_eq!(normalize_signal("sigterm"), "SIGTERM"); // broker strips SIG
        assert_eq!(normalize_signal("2"), "INT");
    }

    #[test]
    fn spawning_session_identity_is_not_inherited() {
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_SESSION", "s-123");
        state.export_var("KEEP_ME", "yes");
        let env = job_env(&state);
        assert!(env.iter().any(|(k, v)| k == "KEEP_ME" && v == "yes"));
        assert!(!env.iter().any(|(k, _)| k == "AGSH_SESSION"));
    }
}
