use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::str::FromStr;

use agsh_core::{parse_line, ShellError, ShellErrorKind};
use agsh_exec::{print_captured_if_needed, ExecutionOptions, Executor, ShellState};
use agsh_output::OutputMode;
use agsh_tty::{pick_history, read_line, read_line_with_initial, render_prompt, HistorySelection};

const MAX_SHELL_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RC_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct CliOptions {
    command: Option<String>,
    /// `$0` for `-c`/`--run`; the first trailing argument names the command.
    command_name: Option<String>,
    /// Script file to execute non-interactively (`agsh FILE [args...]`).
    script: Option<String>,
    /// Positional arguments ($1, $2, …) for `-c` or a script file.
    script_args: Vec<String>,
    output_mode: Option<OutputMode>,
    /// `--allow ls,df`: confine the session to these external commands.
    allow: Option<String>,
    /// `--run CMD`: run CMD as a confined leaf payload.
    run: Option<String>,
    /// `--force`: bypass the self-managing-agent refusal.
    force: bool,
    /// `--best-effort`: fall back to the shim layer when no OS sandbox exists.
    best_effort: bool,
    /// `--norc`: skip sourcing the interactive startup rc file.
    norc: bool,
    /// `--rcfile PATH`: source PATH at startup instead of the default rc file.
    rcfile: Option<String>,
    /// `--observe CMD ARGS…`: run CMD as a captured/observed external command
    /// (the compacting proxy behind shell interception). Consumes the rest of argv.
    observe: Option<Vec<String>>,
    /// `--broker-daemon`: run the keep broker (agshd) in the foreground.
    broker_daemon: bool,
    /// `--broker-launch`: spawn a detached broker daemon and exit (autostart).
    broker_launch: bool,
    /// `--supervise -- CMD…`: setsid + adopt the PTY on stdin as controlling
    /// terminal, then exec CMD (the broker's per-job leader shim).
    supervise: Option<Vec<String>>,
    /// `--keep`: run this interactive session under the keep broker, so it
    /// survives the terminal (detach instead of dying on hangup).
    keep_session: bool,
    /// `--attach [ID]`: reattach to a detached kept agsh session.
    attach: Option<Option<String>>,
    /// Internal, bounded stdin state handoff for a POSIX asynchronous subshell.
    background_state_stdin: bool,
    capture_drain_run: bool,
    show_help: bool,
    show_version: bool,
}

fn main() {
    let cli_args = match collect_cli_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("agsh: {message}");
            std::process::exit(2);
        }
    };
    let options = match parse_args(cli_args.into_iter()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("agsh: {message}");
            std::process::exit(2);
        }
    };

    if options.capture_drain_run {
        std::process::exit(run_capture_drain());
    }
    if options.show_help {
        print_help();
        return;
    }
    if options.show_version {
        println!("agsh {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // Keep-broker plumbing (internal modes; see docs/SESSIONS.md). Handled
    // before any shell setup — the daemon and the per-job supervisor are not
    // shells and must not load history, rc files, or signal handlers.
    if options.broker_launch {
        match std::env::current_exe().and_then(|exe| agsh_broker::daemon::launch_detached(&exe)) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("agsh: broker launch: {error}");
                std::process::exit(1);
            }
        }
    }
    if options.broker_daemon {
        let Some(socket) = agsh_broker::paths::socket_path() else {
            eprintln!("agsh: broker: cannot resolve socket path (HOME unset?)");
            std::process::exit(1);
        };
        if let Err(error) = agsh_broker::daemon::run(&socket) {
            eprintln!("agsh: broker: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(argv) = options.supervise.clone() {
        let error = agsh_broker::supervise_exec(&argv);
        eprintln!(
            "agsh: supervise: {}: {error}",
            argv.first().map(String::as_str).unwrap_or("")
        );
        std::process::exit(127);
    }
    // `--keep` / `--attach`: this process is a thin attach client; the real
    // interactive session lives under the broker and survives us.
    if options.keep_session {
        std::process::exit(run_kept_session(&options));
    }
    if let Some(id) = options.attach.clone() {
        std::process::exit(run_attach(id.as_deref()));
    }

    if let Ok(exe) = std::env::current_exe() {
        agsh_exec::set_capture_drain_helper(exe);
    }
    let mut state = ShellState::from_current_process();
    if options.background_state_stdin {
        if let Err(error) = agsh_exec::restore_background_snapshot_stdin(&mut state) {
            eprintln!("agsh: background state: {error}");
            std::process::exit(1);
        }
    }
    let mut executor = Executor::new().with_stdout_flush(true);

    // Session default output mode, highest priority first:
    //   --output flag  >  AGSH_OUTPUT_MODE env  >  ~/.config/agsh/token.toml [mode]
    //   >  raw.
    // The config default applies to INTERACTIVE sessions only (stdout is a TTY);
    // non-interactive `agsh -c` / scripts stay raw unless a flag/env says otherwise,
    // so piped output and the differential tests are never silently transformed.
    let output_mode = options
        .output_mode
        .or_else(|| {
            std::env::var("AGSH_OUTPUT_MODE")
                .ok()
                .and_then(|value| OutputMode::from_str(&value).ok())
        })
        .or_else(|| state.default_output_mode())
        .or_else(|| {
            std::io::stdout()
                .is_terminal()
                .then(|| state.output_config().mode.interactive_default())
                .flatten()
        })
        .unwrap_or(OutputMode::Raw);
    // Seed the runtime default so the `mode` builtin can change it mid-session.
    state.set_default_output_mode(Some(output_mode));

    let exec_options = ExecutionOptions {
        output_mode,
        allow_process_replacement: true,
    };

    // SIGINT/SIGQUIT interrupt the shell's loops/commands instead of killing it.
    if let Err(error) = agsh_exec::install_signal_handlers(&state) {
        eprintln!("agsh: failed to install signal handlers: {error}");
    }

    // A background child already received the parent's exact active overlay and
    // saved baseline. Re-reading the trust store or `.env` here would introduce
    // a startup race and could silently discard the inherited overlay.
    if !options.background_state_stdin {
        state.activate_project_env();
    }
    // On a terminal, let the real `ls` colorize files vs directories (themed).
    seed_color_env(&mut state);

    // Apply command confinement (the `confine` guardrail). Order matters:
    // inherited env first (a child agsh self-confines), then `--allow`.
    if let Err(error) = apply_confinement(&mut state, &options) {
        eprintln!("agsh: cannot install confinement shell shims: {error}");
        std::process::exit(1);
    }

    // `--observe CMD ARGS…`: run CMD as a captured/observed external command whose
    // output is rendered in the session output mode (the compacting proxy behind
    // shell interception). Children pass straight through — `AGSH_INTERCEPT_ACTIVE`
    // makes nested shells skip re-observation.
    if let Some(argv) = options.observe.clone() {
        if argv.is_empty() {
            eprintln!("agsh: --observe requires a command");
            std::process::exit(2);
        }
        state.export_var("AGSH_INTERCEPT_ACTIVE", "1");
        // Also set it in *this* process's env so the exec-interposition layer (if
        // preloaded here) sees it and passes through when we spawn the real shell —
        // otherwise it would re-observe our own child. Safe: single-threaded here.
        std::env::set_var("AGSH_INTERCEPT_ACTIVE", "1");
        let source = argv
            .iter()
            .map(|a| single_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let code = match run_one(&source, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    // Optional shell interception (off by default): route the agent's own
    // `bash`/`sh`/… invocations through `agsh --observe` so their output is
    // compacted/observed. Opt-in via `AGSH_INTERCEPT=<mode>`. Skipped inside an
    // already-observed subtree to avoid re-installing / re-entrancy.
    if std::env::var_os("AGSH_INTERCEPT_ACTIVE").is_none() {
        if let Some((mode, native, deep)) = intercept_mode() {
            match agsh_exec::install_intercept_shims(&mut state, mode, native) {
                Ok(_) if deep && !agsh_exec::install_deep_intercept(&mut state, mode) => {
                    eprintln!(
                        "agsh: deep interception unavailable (interposer not found); PATH shims only"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("agsh: shell interception unavailable: {error}");
                }
            }
        }
    }

    // `--run CMD`: run CMD as a confined leaf payload (OS-enforced via the confine
    // backend; self-managing agents are refused; falls back to shims only with
    // --best-effort).
    if let Some(command) = options.run.clone() {
        state.set_arg0(options.command_name.as_deref().unwrap_or("agsh"));
        state.set_positionals(&options.script_args);
        let names: Vec<String> = options
            .allow
            .as_deref()
            .map(|l| {
                l.split([',', ' ', '\t'])
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            eprintln!(
                "agsh: --run requires a non-empty --allow capability list; use -c for ordinary execution"
            );
            std::process::exit(2);
        }
        let mut cleanup = Vec::new();
        let mut removed_env = Vec::new();
        let final_command = {
            let opts = agsh_exec::ConfineOpts {
                force: options.force,
                best_effort: options.best_effort,
                ..Default::default()
            };
            let payload_tokens: Vec<String> =
                command.split_whitespace().map(str::to_string).collect();
            match agsh_exec::confine_plan(&state, &names, &payload_tokens, &command, &opts) {
                agsh_exec::ConfinePlan::Refuse { message, code } => {
                    eprint!("{message}");
                    std::process::exit(code);
                }
                agsh_exec::ConfinePlan::Sandboxed {
                    command: wrapped,
                    cleanup: paths,
                    env_remove,
                    ..
                } => {
                    removed_env = env_remove
                        .into_iter()
                        .filter_map(|name| {
                            state.take_exported_env(&name).map(|value| (name, value))
                        })
                        .collect();
                    cleanup = paths;
                    wrapped
                }
                agsh_exec::ConfinePlan::BestEffort => {
                    // Weaker shim layer (route the payload's shells through agsh).
                    let effective = match state.confine_policy() {
                        Some(p) => p.intersect(&names),
                        None => agsh_policy::AllowPolicy::from_names(&names),
                    };
                    if let Err(error) = agsh_exec::install_confine_shims(&mut state) {
                        eprintln!("agsh: cannot install confinement shell shims: {error}");
                        std::process::exit(1);
                    }
                    state.export_var("AGSH_CONFINE", effective.to_list());
                    command
                }
            }
        };
        let code = match run_one(&final_command, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        for path in cleanup {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
        for (name, value) in removed_env {
            state.restore_exported_env(name, value);
        }
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    if let Some(command) = options.command {
        if !options.background_state_stdin {
            state.set_arg0(options.command_name.as_deref().unwrap_or("agsh"));
            state.set_positionals(&options.script_args);
        }
        let code = match run_one(&command, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    // Non-interactive script file: `agsh FILE [args...]`.
    if let Some(path) = options.script {
        let source = match std::fs::File::open(&path)
            .and_then(|file| read_utf8_limited(file, MAX_SHELL_SOURCE_BYTES, "shell source"))
        {
            Ok(source) => source,
            Err(error) => {
                eprintln!("agsh: {path}: {error}");
                std::process::exit(127);
            }
        };
        state.set_arg0(&path);
        state.set_positionals(&options.script_args);
        let code = match run_one(&source, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    // With no `-c` command or script file, non-terminal stdin is itself the
    // script source. Execute it as one graph so multiline constructs and the
    // final command status behave exactly like a script file, without ever
    // entering the prompt/editor loop.
    if !std::io::stdin().is_terminal() {
        let source = match read_utf8_limited(
            std::io::stdin(),
            MAX_SHELL_SOURCE_BYTES,
            "stdin shell source",
        ) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("agsh: stdin: {error}");
                std::process::exit(1);
            }
        };
        let code = match run_one(&source, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    // Interactive startup: source the rc file (aliases, functions, exports, prompt
    // hooks, `mode:…`) into the live session. Gated on stdin being a TTY — `-c`,
    // script files, and piped input returned or are excluded above, so scripts and
    // the differential tests are never affected.
    let mut session_recorder = None;
    if std::io::stdin().is_terminal() {
        // File-backed history is strictly interactive. Until this point the
        // state uses its in-memory store, so `-c`, script files, `--run`,
        // `--observe`, and piped-stdin scripts cannot read or persist the user's
        // command history.
        state.load_persistent_history();
        source_rc(&mut executor, &mut state, &options, &exec_options);
        // Best-effort journal this session's state deltas for `resume`.
        // Begun after the rc file, so only state typed into THIS session is
        // journaled — rc state is recreated by the rc file on the next start.
        session_recorder = agsh_exec::journal::SessionRecorder::begin(&mut state);
        // Restore the terminal on SIGTERM/SIGHUP so a kill at the raw-mode prompt
        // doesn't leave the tty non-canonical (SHIP_READINESS_PLAN P0-10); with a
        // journal, also record *why* the session died (`hup` on terminal close).
        match &session_recorder {
            Some(recorder) => {
                agsh_tty::arm_terminal_restore_on_signals_with(recorder.hangup_hook());
            }
            None => agsh_tty::arm_terminal_restore_on_signals(),
        }
        if session_recorder.is_some() {
            if let Some(banner) = agsh_exec::journal::restore_banner(&state) {
                eprintln!("{banner}");
            }
        }
        // Breadcrumb for kept sessions left running (never autostarts a broker).
        if let Some(hint) = agsh_exec::keep::detached_sessions_hint(&state) {
            eprintln!("{hint}");
        }
    }

    let integrate = std::io::stdout().is_terminal();

    // Wake-from-standby detection (interactive TTY sessions only): the wall
    // clock advances during sleep, the monotonic clock does not, so a large
    // divergence between prompts means the machine slept. Saying so makes
    // dropped ssh connections and stale state legible instead of mysterious.
    let mut sleep_watch = session_recorder
        .as_ref()
        .map(|_| (std::time::SystemTime::now(), std::time::Instant::now()));

    loop {
        let slept = sleep_watch.as_mut().and_then(|(wall, mono)| {
            let wall_delta = std::time::SystemTime::now()
                .duration_since(*wall)
                .unwrap_or_default();
            let gap = wall_delta.saturating_sub(mono.elapsed());
            *wall = std::time::SystemTime::now();
            *mono = std::time::Instant::now();
            (gap.as_secs() >= 30).then_some(gap.as_secs())
        });

        // Report any background jobs that finished since the last prompt (after
        // a sleep was detected, so the wake note below explains any deaths).
        let notices = state.reap_finished_jobs();
        if let Some(secs) = slept {
            eprintln!("{}", agsh_exec::journal::wake_note(&state, secs));
        }
        for notice in notices {
            eprintln!("{notice}");
        }

        // Shell integration: prompt-start mark + window title (cwd).
        if integrate {
            shell_integration_prompt(&state);
        }

        // precmd hook: runs before each prompt (zsh `precmd` / `precmd_functions`).
        run_hooks(&mut executor, &mut state, &exec_options, "precmd", None);

        let mut prompt = render_prompt(&state);
        // Shell integration: prompt-end mark (`B`) rendered with the prompt, so
        // everything after it on the line is user input. Embedded in the prompt
        // string (rather than emitted separately) so it always sits exactly at
        // the prompt/input boundary, including across line-editor repaints.
        if integrate {
            prompt.push_str("\x1b]133;B\x07");
        }
        let mut line = match read_line(&prompt, &state) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("agsh: read error: {error}");
                break;
            }
        };

        if line.trim() == "history tui" {
            let Some(selected) = resolve_history_tui_line(&state, integrate) else {
                continue;
            };
            line = selected;
        }

        if line.trim().is_empty() {
            continue;
        }

        // preexec hook: runs after a command is read, before it executes; the
        // command line is passed as $1 (zsh `preexec` / `preexec_functions`).
        run_hooks(
            &mut executor,
            &mut state,
            &exec_options,
            "preexec",
            Some(&line),
        );
        let cwd_before = state.cwd().to_path_buf();

        // Shell integration: output-start mark + window title (running command).
        if integrate {
            emit_osc(&format!("\x1b]133;C\x07\x1b]2;{}\x07", title_text(&line)));
        }

        // Flight recorder: journal the command line before it runs, so a session
        // that dies mid-command knows what was running.
        if let Some(recorder) = &session_recorder {
            recorder.command_started(&line);
        }

        let exit = match run_one(&line, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => report_shell_error(&error),
        };
        // Shell integration: command-end mark with exit status.
        if integrate {
            emit_osc(&format!("\x1b]133;D;{exit}\x07"));
        }
        // A SIGINT during the command interrupts it, not the next prompt.
        state.clear_interrupt();

        // Journal any state deltas the command produced (cwd, exports, aliases,
        // …), so a crash after this point loses nothing.
        if let Some(recorder) = &mut session_recorder {
            recorder.command_finished(&state, exit);
        }

        // chpwd hook: runs after the command if the working directory changed
        // (covers cd/pushd/popd) — zsh `chpwd` / `chpwd_functions`.
        if state.cwd() != cwd_before {
            run_hooks(&mut executor, &mut state, &exec_options, "chpwd", None);
        }

        if state.should_exit() {
            break;
        }
    }

    run_exit_trap(&mut executor, &mut state, &exec_options);
    // Clean end: best-effort mark the journal so it is not offered for restore.
    if let Some(recorder) = &session_recorder {
        recorder.finish(state.last_status());
    }
}

fn collect_cli_args() -> Result<Vec<String>, String> {
    std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|argument| format!("argument is not valid UTF-8: {argument:?}"))
        })
        .collect()
}

fn read_utf8_limited(
    reader: impl Read,
    limit: usize,
    description: &str,
) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    reader.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} exceeds {limit} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} is not valid UTF-8: {error}"),
        )
    })
}

fn resolve_history_tui_line(state: &ShellState, integrate: bool) -> Option<String> {
    match pick_history(state) {
        Ok(Some(HistorySelection::Run(command))) => Some(command),
        Ok(Some(HistorySelection::Edit(command))) => {
            if integrate {
                shell_integration_prompt(state);
            }
            let mut prompt = render_prompt(state);
            if integrate {
                prompt.push_str("\x1b]133;B\x07");
            }
            match read_line_with_initial(&prompt, state, &command) {
                Ok(Some(line)) if !line.trim().is_empty() => Some(line),
                Ok(_) => None,
                Err(error) => {
                    eprintln!("agsh: read error: {error}");
                    None
                }
            }
        }
        Ok(None) => None,
        Err(error) => {
            eprintln!("agsh: history tui: {error}");
            None
        }
    }
}

/// `agsh --keep`: start a NEW interactive agsh session under the keep broker
/// and attach this terminal to it. The session survives this client — closing
/// the terminal (or SIGHUP during standby) merely detaches; `agsh --attach`
/// from any later terminal resumes it exactly where it was.
fn run_kept_session(options: &CliOptions) -> i32 {
    use std::io::IsTerminal;
    use std::os::unix::ffi::OsStringExt;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("agsh: --keep needs a terminal");
        return 2;
    }
    if std::env::var_os("AGSH_KEPT").is_some() {
        eprintln!("agsh: already inside a kept session (Ctrl-] detaches it)");
        return 2;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("agsh: --keep: cannot find agsh binary: {error}");
            return 1;
        }
    };
    let client = match agsh_broker::Client::connect_or_start(&exe) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("agsh: --keep: {error}");
            return 1;
        }
    };
    // The inner agsh: same binary, interactive, marked kept. Startup flags
    // that shape the session are propagated; session identity is not.
    let mut cmd = vec![exe.display().to_string()];
    if options.norc {
        cmd.push("--norc".into());
    }
    if let Some(rcfile) = &options.rcfile {
        cmd.push("--rcfile".into());
        cmd.push(rcfile.clone());
    }
    let mut env = Vec::new();
    let mut opaque_env = Vec::new();
    for (key, value) in std::env::vars_os() {
        if key == std::ffi::OsStr::new("AGSH_SESSION") {
            continue;
        }
        match key.into_string() {
            Ok(key) => match value.into_string() {
                Ok(value) => env.push((key, value)),
                Err(value) => opaque_env.push((key.into_bytes(), value.into_vec())),
            },
            Err(key) => opaque_env.push((key.into_vec(), value.into_vec())),
        }
    }
    env.push(("AGSH_KEPT".into(), "1".into()));
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let (rows, cols) = agsh_broker::attach::term_size();
    let info = match client.spawn_job(agsh_broker::SpawnSpec {
        cmd,
        cwd,
        env,
        opaque_env,
        rows,
        cols,
        kind: agsh_broker::JobKind::Session,
        title: "agsh".into(),
    }) {
        Ok(info) => info,
        Err(error) => {
            eprintln!("agsh: --keep: {error}");
            return 1;
        }
    };
    eprintln!(
        "agsh: kept session [{}] — closing this terminal only detaches it; \
         `agsh --attach` resumes it",
        info.id
    );
    finish_session_attach(&client, &info.id)
}

/// `agsh --attach [ID]`: reattach to a detached kept agsh session.
fn run_attach(id: Option<&str>) -> i32 {
    use std::io::IsTerminal;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        eprintln!("agsh: --attach needs a terminal");
        return 2;
    }
    let client = match agsh_broker::Client::from_env() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("agsh: --attach: {error}");
            return 1;
        }
    };
    if client.ping().is_err() {
        eprintln!("agsh: --attach: broker not running — no kept sessions");
        return 1;
    }
    let id = match id {
        Some(id) => id.to_string(),
        None => {
            // Pick the newest detached session; list the rest if ambiguous.
            let jobs = client.list().unwrap_or_default();
            let mut detached: Vec<_> = jobs
                .iter()
                .filter(|j| j.kind == agsh_broker::JobKind::Session && j.running && !j.attached)
                .collect();
            detached.sort_by_key(|j| std::cmp::Reverse(j.started_at));
            match detached.len() {
                0 => {
                    eprintln!("agsh: no detached kept sessions (start one with `agsh --keep`)");
                    return 1;
                }
                1 => {}
                n => {
                    eprintln!("agsh: {n} detached sessions — attaching the newest:");
                    for job in &detached {
                        eprintln!("  agsh --attach {}   ({})", job.id, job.title);
                    }
                }
            }
            detached[0].id.clone()
        }
    };
    finish_session_attach(&client, &id)
}

/// Attach the terminal to a kept session and translate how it ended.
fn finish_session_attach(client: &agsh_broker::Client, id: &str) -> i32 {
    match agsh_broker::attach_interactive(client, id) {
        Ok(agsh_broker::AttachOutcome::Detached) => {
            eprintln!("agsh: detached — session [{id}] keeps running (`agsh --attach {id}`)");
            0
        }
        Ok(agsh_broker::AttachOutcome::Ended) => {
            // Stream closed: session exit, or another terminal took the attach
            // over (last attach wins) — the session's status tells them apart.
            match client.status(id) {
                Ok(info) if info.running => {
                    eprintln!(
                        "agsh: attach taken over by another terminal — session [{id}] keeps \
                         running (`agsh --attach {id}`)"
                    );
                    0
                }
                status => {
                    let code = status.ok().and_then(|info| info.exit_code).unwrap_or(0);
                    eprintln!("agsh: kept session [{id}] ended (code {code})");
                    code
                }
            }
        }
        Err(error) => {
            eprintln!("agsh: attach: {error}");
            1
        }
    }
}

/// Run the `EXIT` trap action, if any, exactly once. Driven from the shell's
/// exit points (interactive end, `-c`, and script end).
fn run_exit_trap(executor: &mut Executor, state: &mut ShellState, options: &ExecutionOptions) {
    if let Some(action) = state.trap_action("EXIT") {
        state.set_trap("EXIT", None);
        let _ = run_one_inner(&action, executor, state, options);
    }
}

/// Emit OSC 133 prompt-start (`A`), OSC 7 (current directory as a file URL,
/// so terminals track `cd` live), and the window title set to the cwd. These
/// shell-integration sequences let terminals navigate between prompts and show
/// command status; they are only emitted on a TTY.
fn shell_integration_prompt(state: &ShellState) {
    let abs = state.cwd().display().to_string();
    let cwd = state
        .lookup("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|home| abs.strip_prefix(home).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| abs.clone());
    emit_osc(&format!(
        "\x1b]133;A\x07\x1b]7;file://{}\x07\x1b]2;agsh: {}\x07",
        title_text(&abs),
        title_text(&cwd)
    ));
}

/// Sanitize text for an OSC title (strip control characters; cap length).
fn title_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(120).collect()
}

fn emit_osc(seq: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// On a terminal, seed color environment so the real `ls` (and friends) colorize
/// files vs directories to match agsh's theme. It never clobbers a value the user
/// already set, and never forces color (so piped/captured output stays byte-clean
/// — `ls`/`--color=auto`/`CLICOLOR` all key off `isatty`). No-op when stdout is
/// not a TTY, so non-interactive `agsh -c` (and the test harnesses) are unchanged.
fn seed_color_env(state: &mut ShellState) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let theme = state.theme();
    let defaults = [
        ("CLICOLOR", "1".to_string()), // BSD/macOS `ls` auto-colors on a TTY
        ("LSCOLORS", theme.bsd_lscolors()), // BSD/macOS palette
        ("LS_COLORS", theme.ls_colors()), // GNU palette
    ];
    for (key, value) in defaults {
        if state.lookup(key).is_none() {
            state.export_var(key, value);
        }
    }
    // GNU `ls` only colors with `--color=auto` (there is no env equivalent of
    // CLICOLOR), so alias it on Linux. `--color=auto` keys off isatty, so piped
    // output stays clean. (BSD `ls` would error on `--color`, so macOS relies on
    // CLICOLOR instead and gets no alias.)
    #[cfg(target_os = "linux")]
    if state.alias("ls").is_none() {
        state.set_alias("ls", "ls --color=auto");
    }
}

/// Run a shell hook: every function named in `${hook}_functions` (in order),
/// then the `hook` function itself if defined. For `preexec`, `arg` (the command
/// line) is passed as `$1`. Failures and output are not special-cased; history is
/// not recorded for hook invocations.
fn run_hooks(
    executor: &mut Executor,
    state: &mut ShellState,
    options: &ExecutionOptions,
    hook: &str,
    arg: Option<&str>,
) {
    let mut targets: Vec<String> = state
        .array(&format!("{hook}_functions"))
        .map(<[String]>::to_vec)
        .unwrap_or_default();
    if state.function(hook).is_some() {
        targets.push(hook.to_string());
    }
    for name in targets {
        if state.function(&name).is_none() {
            continue;
        }
        let line = match arg {
            Some(a) => format!("{name} {}", single_quote(a)),
            None => name.clone(),
        };
        if let Ok(graph) = parse_line(&line) {
            if let Ok(outcome) = executor.run_graph(&graph, state, options) {
                let _ = print_captured_if_needed(&outcome, options);
            }
        }
    }
}

/// Single-quote a string so it survives re-parsing as one argument.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The opt-in shell-interception setting from `$AGSH_INTERCEPT`, as
/// `(output mode, native)`, or `None` when disabled. The value is `<mode>[:native]`:
/// the mode is a mode name (or `1`/`on`/`true` ⇒ `compact`; unset/`0`/`off`
/// disables). The `:native` suffix selects flavor A (agsh *interprets* the command)
/// instead of the default proxy (agsh runs the real shell and observes it). Read
/// from the environment, so it can be set in your `agshrc`.
fn intercept_mode() -> Option<(OutputMode, bool, bool)> {
    agsh_exec::parse_intercept_spec(&std::env::var("AGSH_INTERCEPT").ok()?)
}

/// Set up command confinement before running anything:
/// 1. Inherit `AGSH_CONFINE` from the environment (a child agsh self-confines so
///    descendants of a confined session stay confined).
/// 2. Apply `--allow LIST`: with `--run` the payload is exempt and only its
///    descendants are confined (export the env); without `--run` the session
///    itself is confined.
fn apply_confinement(state: &mut ShellState, options: &CliOptions) -> std::io::Result<()> {
    fn parse_list(list: &str) -> Vec<String> {
        list.split([',', ' ', '\t'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    if let Ok(list) = std::env::var("AGSH_CONFINE") {
        let names = parse_list(&list);
        // Presence is significant: an empty serialized allowlist means
        // deny-all, while an absent variable means the session is unconfined.
        state.set_confine(&names);
    }

    // `--allow LIST` without `--run`: confine THIS interactive session (the
    // agsh-routed gate) and propagate to children. The `--run` payload case is
    // handled separately (OS-enforced confine / agent refusal) in `main`.
    if let Some(list) = &options.allow {
        if options.run.is_some() {
            return Ok(());
        }
        let names = parse_list(list);
        if names.is_empty() {
            return Ok(());
        }
        // Provision the generic shims before committing the narrowed policy.
        // A failure leaves this process unmodified and the caller refuses to run.
        agsh_exec::install_confine_shims(state)?;
        state.set_confine(&names);
        if let Some(p) = state.confine_policy() {
            let list = p.to_list();
            state.export_var("AGSH_CONFINE", list);
        }
    }
    Ok(())
}

/// Source the interactive startup rc file into the live session, if one exists.
/// Runs like commands typed at startup — but without a history entry — so it can
/// define aliases, functions, exports, prompt hooks, and set `mode:…`. Errors are
/// non-fatal: a broken rc warns and still drops the user at a prompt.
fn source_rc(
    executor: &mut Executor,
    state: &mut ShellState,
    options: &CliOptions,
    exec_options: &ExecutionOptions,
) {
    let Some((path, explicit)) = resolve_rc(options) else {
        return;
    };
    let source = match open_regular_config(&path)
        .and_then(|file| read_utf8_limited(file, MAX_RC_SOURCE_BYTES, "rc file"))
    {
        Ok(source) => source,
        Err(error) => {
            // Only complain when the user explicitly named the file.
            if explicit {
                eprintln!("agsh: cannot read rc file {}: {error}", path.display());
            }
            return;
        }
    };
    if let Err(error) = run_one_inner(&source, executor, state, exec_options) {
        eprintln!("agsh: {}: {error}", path.display());
    }
}

fn open_regular_config(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "configuration input is not a regular file",
        ));
    }
    Ok(file)
}

/// Resolve the rc file to source, as `(path, explicit)` — `explicit` is true when
/// the user named it (so a missing file warrants a warning). Precedence:
/// `--norc`/`AGSH_NORC` disables; else `--rcfile`/`$AGSH_RC`; else the first of
/// `~/.config/agsh/agshrc` (XDG) or `~/.agshrc` (dotfile) that exists.
fn resolve_rc(options: &CliOptions) -> Option<(PathBuf, bool)> {
    if options.norc || std::env::var_os("AGSH_NORC").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    if let Some(path) = &options.rcfile {
        return Some((PathBuf::from(path), true));
    }
    if let Some(path) = std::env::var_os("AGSH_RC").filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(path), true));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let xdg = home.join(".config/agsh/agshrc");
    if xdg.exists() {
        return Some((xdg, false));
    }
    let dot = home.join(".agshrc");
    if dot.exists() {
        return Some((dot, false));
    }
    None
}

fn report_shell_error(error: &ShellError) -> i32 {
    eprintln!("agsh: {error}");
    if error.kind == ShellErrorKind::Parse {
        2
    } else {
        1
    }
}

fn run_one(
    line: &str,
    executor: &mut Executor,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<i32, agsh_core::ShellError> {
    state.record_history_with_mode(line, Some(options.output_mode));
    let started = std::time::Instant::now();
    let result = run_one_inner(line, executor, state, options);
    // Remove any temp files created for process substitutions on this line.
    for path in state.take_proc_sub_temps() {
        let _ = std::fs::remove_file(path);
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let exit = match &result {
        Ok(code) => *code,
        Err(_) => 1,
    };
    state.finalize_history(exit, duration_ms);
    result
}

fn run_one_inner(
    line: &str,
    executor: &mut Executor,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<i32, agsh_core::ShellError> {
    // `agview <files>` is sugar for rendering files by type: run `cat` over them in
    // the rich display mode. Otherwise honor a per-command output-mode wrapper
    // (e.g. `semantic git diff`, `raw npm test`).
    let (mode_override, effective_line): (Option<OutputMode>, String) =
        if let Some(rest) = view_target(line) {
            (Some(OutputMode::Rich), format!("cat -- {rest}"))
        } else {
            let (mode, rest) = peel_output_mode(line);
            (mode, rest.to_string())
        };
    let mut effective_options = match mode_override {
        // A per-command wrapper (`compact ls`, `raw npm test`) wins outright.
        Some(output_mode) => ExecutionOptions {
            output_mode,
            allow_process_replacement: options.allow_process_replacement,
        },
        // Otherwise use the session default — the runtime `mode` builtin can
        // change it mid-session; falls back to the startup mode.
        None => ExecutionOptions {
            output_mode: state.default_output_mode().unwrap_or(options.output_mode),
            allow_process_replacement: options.allow_process_replacement,
        },
    };

    // `clear`/`reset` exist only to emit terminal-control escapes to the TTY. A
    // capturing mode (compact/semantic) would swallow those escapes — leaving the
    // screen untouched and printing a useless "clear [ok]" summary — so on a
    // terminal, run a standalone terminal-control command raw so it actually works.
    if effective_options.output_mode.should_capture()
        && std::io::stdout().is_terminal()
        && is_terminal_control_line(&effective_line)
    {
        effective_options.output_mode = OutputMode::Raw;
    }

    let graph = parse_line(&effective_line)?;
    let outcome = executor.run_graph(&graph, state, &effective_options)?;
    print_captured_if_needed(&outcome, &effective_options)?;
    Ok(outcome.exit_code)
}

/// Whether `line` is a standalone terminal-control command whose output is escape
/// sequences meant for the TTY (`clear`, `reset`, `cls`, `tput clear|reset|…`).
/// Only a bare command qualifies — a pipeline/list/redirect is left to normal mode.
fn is_terminal_control_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.contains(['|', ';', '&', '<', '>', '\n', '`', '$']) {
        return false;
    }
    let mut words = trimmed.split_whitespace();
    match words.next() {
        Some("clear") | Some("reset") | Some("cls") => true,
        Some("tput") => matches!(
            words.next(),
            Some("clear")
                | Some("reset")
                | Some("init")
                | Some("civis")
                | Some("cnorm")
                | Some("smcup")
                | Some("rmcup")
        ),
        _ => false,
    }
}

/// If `line` is an `agview` command, return the file arguments (possibly empty).
/// `agview` renders its files by type in the rich display mode. (Prefixed because
/// bare `view` is vim's read-only mode on most systems.)
fn view_target(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed == "agview" {
        return Some("");
    }
    trimmed.strip_prefix("agview ").map(str::trim)
}

/// Peel a leading output-mode keyword used as a per-command wrapper. Returns the
/// requested mode (if the first whitespace-separated token names a mode and is
/// followed by a command) and the remaining command line.
fn peel_output_mode(line: &str) -> (Option<OutputMode>, &str) {
    let trimmed = line.trim_start();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        if let Ok(mode) = OutputMode::from_str(first) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                return (Some(mode), rest);
            }
        }
    }
    (None, line)
}

fn run_capture_drain() -> i32 {
    use std::io::Write;

    let mut input = std::io::stdin().lock();
    if rustix::fs::fcntl_getfl(&input).is_err() {
        return 1;
    }
    let mut acknowledgement = std::io::stdout().lock();
    if acknowledgement
        .write_all(&[agsh_exec::CAPTURE_DRAIN_READY])
        .and_then(|()| acknowledgement.flush())
        .is_err()
    {
        return 1;
    }
    drop(acknowledgement);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return 1,
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--command" => {
                let Some(command) = args.next() else {
                    return Err("missing command after -c/--command".to_string());
                };
                options.command = Some(command);
                // POSIX: first trailing arg is `$0`; the rest are `$1`, `$2`, ….
                let mut trailing: Vec<String> = args.by_ref().collect();
                if !trailing.is_empty() {
                    options.command_name = Some(trailing.remove(0));
                }
                options.script_args = trailing;
            }
            "--output" => {
                let Some(value) = args.next() else {
                    return Err("missing mode after --output".to_string());
                };
                options.output_mode = Some(OutputMode::from_str(&value)?);
            }
            "--allow" => {
                let Some(value) = args.next() else {
                    return Err("missing command list after --allow".to_string());
                };
                options.allow = Some(value);
            }
            "--run" => {
                let Some(value) = args.next() else {
                    return Err("missing command after --run".to_string());
                };
                options.run = Some(value);
                let mut trailing: Vec<String> = args.by_ref().collect();
                if !trailing.is_empty() {
                    options.command_name = Some(trailing.remove(0));
                }
                options.script_args = trailing;
            }
            "--force" => options.force = true,
            "--best-effort" => options.best_effort = true,
            "--norc" => options.norc = true,
            "--rcfile" => {
                let Some(value) = args.next() else {
                    return Err("missing path after --rcfile".to_string());
                };
                options.rcfile = Some(value);
            }
            "--observe" => {
                // Everything after `--observe` is the command to observe; tolerate
                // an optional `--` separator (`--observe -- cmd` == `--observe cmd`).
                let mut rest: Vec<String> = args.by_ref().collect();
                if rest.first().is_some_and(|a| a == "--") {
                    rest.remove(0);
                }
                options.observe = Some(rest);
            }
            "--broker-daemon" => options.broker_daemon = true,
            "--broker-launch" => options.broker_launch = true,
            "--background-state-stdin" => options.background_state_stdin = true,
            "--capture-drain-run" => options.capture_drain_run = true,
            "--keep" => options.keep_session = true,
            "--attach" => {
                // Optional session id: `--attach` or `--attach s1`.
                let has_id = args.peek().is_some_and(|a| !a.starts_with('-'));
                let id = if has_id { args.next() } else { None };
                options.attach = Some(id);
            }
            "--supervise" => {
                // Everything after `--supervise` is the command to exec.
                let mut rest: Vec<String> = args.by_ref().collect();
                if rest.first().is_some_and(|a| a == "--") {
                    rest.remove(0);
                }
                options.supervise = Some(rest);
            }
            "-h" | "--help" => options.show_help = true,
            "--version" => options.show_version = true,
            "--" => {
                // Everything after `--` is the script file then its arguments.
                if let Some(path) = args.next() {
                    options.script = Some(path);
                    options.script_args = args.by_ref().collect();
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}"));
            }
            other => {
                // First non-flag argument: a script file to run; the rest are
                // its positional parameters.
                options.script = Some(other.to_string());
                options.script_args = args.by_ref().collect();
            }
        }
    }

    Ok(options)
}

fn print_help() {
    println!(
        "agsh - Aegis Shell\n\nUSAGE:\n  agsh [--output MODE] [--allow LIST] [--run COMMAND] [--rcfile FILE] [--norc] [-c COMMAND]\n  agsh --keep | --attach [ID]     kept sessions (survive the terminal; below)\n\nKEPT SESSIONS & JOBS (the keep broker):\n  agsh --keep         run this whole session under the per-user broker: closing\n                      the terminal (or losing SSH during standby) only DETACHES it\n  agsh --attach [ID]  reattach a detached session (newest, or by id)\n  keep -- CMD…        keep a single command instead (builtin; `help keep`)\n  Ctrl-]              detach key while attached\n\nSTARTUP:\n  interactive sessions source ~/.config/agsh/agshrc (or ~/.agshrc); --norc skips,\n  --rcfile FILE / $AGSH_RC picks another, $AGSH_NORC=1 disables\n\nINTERCEPTION (route the agent's own shell through agsh; off by default):\n  AGSH_INTERCEPT=compact agsh …   shim bash/sh/… to `agsh --observe` (real shell,\n                                  captured+rendered); nested shells pass through\n    :native  agsh interprets the command    :deep  also catch absolute-path\n    (instead of running the real shell)     /bin/bash + posix_spawn (preload)\n  toggle at runtime: `mode:intercept compact:deep` / `mode:intercept off`\n\nMODES:\n  raw | clean | compact | semantic | lossless-ref | silent | rich\n\nCONFINE (macOS kernel sandbox for a leaf payload; unsupported platforms fail closed):\n  confine read-only -- python x.py  deny writes/network/common credential paths\n  confine workspace -- ./build.sh   writes only within $PWD (+ a scratch dir)\n  confine offline -- npm test       network off; filesystem unchanged\n  confine convert -- ./batch.sh     exec-allowlist: may only exec `convert`\n  confine ls,df                     sticky command-name guardrail (not a sandbox)\n    --rw PATH  add a writable root    --net/--no-net  toggle network\n    --explain  show capabilities      --dry-run  print profile, don't run\n    --force    run a refused agent    --best-effort  non-security shim layer\n  leaf presets use sandbox-exec on macOS; Linux Landlock is planned. Credential\n  filtering is finite, not complete secret isolation. Self-managing agents\n  (claude, …) are refused — use their own tool-permission systems.\n\nAGSH TOOLS (ag-prefixed where a common CLI shares the name; bare otherwise):\n  agview FILE…   rich render (markdown, code, images)   agz DIR    frecent jump\n  agpatch        structured patch        agtrace/agtrust/agcontext/agmath/agjump\n  confine, peek, risk, snapshot, pty     stay bare (no common CLI conflict)\n  sessions       list/resume Claude & Codex sessions for this folder (sessions N)\n  agenv          view/set exported env vars; `agenv restore` re-applies exports\n                 recorded in history (spaced `export XYZ = 123` works too)\n  mode:output M  set the session default output mode\n\nMODE SELECTION (highest priority first):\n  per-command wrapper   semantic git diff\n  --output flag         agsh --output compact -c 'pytest -q'\n  mode builtin          mode:output compact   (session default; `mode` shows all)\n  AGSH_OUTPUT_MODE env  AGSH_OUTPUT_MODE=semantic agsh -c 'cargo test'\n  ~/.config/agsh/token.toml  [mode] default = \"compact\"  (interactive sessions)\n  default               raw\n  (the config/`mode` default makes plain `ls` render like `compact ls`; it applies\n   to interactive sessions only — piped `agsh -c`/scripts stay raw unless an\n   explicit --output flag or AGSH_OUTPUT_MODE selects an observation mode)\n\nRICH RENDERING (human display, TTY only; raw bytes still pipe/redirect):\n  agview FILE...        render by type (markdown, JSON, CSV/TSV, diff, binary)\n  agview main.py        syntax-highlight source code (py, rs, js, ts, go, c, …)\n  agview image.png      show images inline (any terminal; crisp in iTerm2/Kitty)\n  AGSH_OUTPUT_MODE=rich  auto-render recognized command output\n\nTRACE:\n  successful `complete` captures are addressable via trace://<id>/...\n  agtrace               list recent captured commands\n  agtrace <id>          print up to 16 MiB; incomplete/unavailable returns status 2\n\nEXAMPLES:\n  agsh -c 'echo hello'\n  agsh --output semantic -c 'git status'\n  agview README.md\n  semantic git diff"
    );
}

#[cfg(test)]
mod tests {
    use super::{is_terminal_control_line, open_regular_config, read_utf8_limited};

    #[test]
    fn terminal_control_lines_are_recognized() {
        assert!(is_terminal_control_line("clear"));
        assert!(is_terminal_control_line("  reset  "));
        assert!(is_terminal_control_line("cls"));
        assert!(is_terminal_control_line("tput clear"));
        assert!(is_terminal_control_line("tput reset"));
        // Not standalone control commands.
        assert!(!is_terminal_control_line("ls"));
        assert!(!is_terminal_control_line("tput cols")); // queries, not a screen op
        assert!(!is_terminal_control_line("clear && ls")); // list: leave to normal mode
        assert!(!is_terminal_control_line("clear | tee x"));
        assert!(!is_terminal_control_line("echo clear"));
    }

    #[test]
    fn source_reader_rejects_oversized_and_invalid_utf8_input() {
        let oversized = std::io::Cursor::new(vec![b'x'; 9]);
        let error = read_utf8_limited(oversized, 8, "test source").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"));

        let invalid = std::io::Cursor::new(vec![0xff]);
        let error = read_utf8_limited(invalid, 8, "test source").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn rc_reader_rejects_symlinks_and_special_files_without_blocking() {
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!(
            "agsh-rc-input-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let regular = dir.join("regular");
        std::fs::write(&regular, b"echo safe\n").unwrap();
        let symlink = dir.join("symlink");
        std::os::unix::fs::symlink(&regular, &symlink).unwrap();
        assert!(open_regular_config(&symlink).is_err());

        let socket = dir.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(open_regular_config(&socket).is_err());
        drop(listener);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
