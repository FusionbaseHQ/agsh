use std::io::IsTerminal;
use std::path::PathBuf;
use std::str::FromStr;

use agsh_core::parse_line;
use agsh_exec::{print_captured_if_needed, ExecutionOptions, Executor, ShellState};
use agsh_output::OutputMode;
use agsh_policy::{analyze_graph, RiskLevel};
use agsh_tty::{read_line, render_prompt};

#[derive(Debug, Default)]
struct CliOptions {
    command: Option<String>,
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
    show_help: bool,
    show_version: bool,
}

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("agsh: {message}");
            std::process::exit(2);
        }
    };

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

    let mut state = ShellState::from_current_process();
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

    // Load and persist rich history for the interactive session.
    state.load_persistent_history();
    // Activate a trusted project .env for the starting directory (no-op if none).
    state.activate_project_env();
    // On a terminal, let the real `ls` colorize files vs directories (themed).
    seed_color_env(&mut state);

    // Apply command confinement (the `confine` guardrail). Order matters:
    // inherited env first (a child agsh self-confines), then `--allow`.
    apply_confinement(&mut state, &options);

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
            Err(error) => {
                eprintln!("agsh: {error}");
                1
            }
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
            let _ = agsh_exec::install_intercept_shims(&mut state, mode, native);
            if deep && !agsh_exec::install_deep_intercept(&mut state, mode) {
                eprintln!(
                    "agsh: deep interception unavailable (interposer not found); PATH shims only"
                );
            }
        }
    }

    // `--run CMD`: run CMD as a confined leaf payload (OS-enforced via the confine
    // backend; self-managing agents are refused; falls back to shims only with
    // --best-effort).
    if let Some(command) = options.run.clone() {
        if !options.script_args.is_empty() {
            state.set_positionals(&options.script_args);
        }
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
        let final_command = if names.is_empty() {
            command
        } else {
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
                    command: wrapped, ..
                } => {
                    // NOTE: launch `--run` exec-replaces the shell, so the temp
                    // profile/scratch are cleaned by the OS on TMPDIR rotation
                    // (exec-only launch has no scratch); the interactive `confine`
                    // builtin cleans up explicitly.
                    wrapped
                }
                agsh_exec::ConfinePlan::BestEffort => {
                    // Weaker shim layer (route the payload's shells through agsh).
                    let effective = match state.confine_policy() {
                        Some(p) => p.intersect(&names),
                        None => agsh_policy::AllowPolicy::from_names(&names),
                    };
                    state.export_var("AGSH_CONFINE", effective.to_list());
                    let _ = agsh_exec::install_confine_shims(&mut state);
                    command
                }
            }
        };
        let code = match run_one(&final_command, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("agsh: {error}");
                1
            }
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    if let Some(command) = options.command {
        if !options.script_args.is_empty() {
            state.set_positionals(&options.script_args);
        }
        let code = match run_one(&command, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("agsh: {error}");
                1
            }
        };
        run_exit_trap(&mut executor, &mut state, &exec_options);
        std::process::exit(code);
    }

    // Non-interactive script file: `agsh FILE [args...]`.
    if let Some(path) = options.script {
        let source = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("agsh: {path}: {error}");
                std::process::exit(127);
            }
        };
        state.set_positionals(&options.script_args);
        let code = match run_one(&source, &mut executor, &mut state, &exec_options) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("agsh: {error}");
                1
            }
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
        source_rc(&mut executor, &mut state, &options, &exec_options);
        // Journal this session's state deltas (crash-safe restore via `resume`).
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
        let line = match read_line(&prompt, &state) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                eprintln!("agsh: read error: {error}");
                break;
            }
        };

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
            Err(error) => {
                eprintln!("agsh: {error}");
                1
            }
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
    // Clean end: mark the journal so this session is never offered for restore.
    if let Some(recorder) = &session_recorder {
        recorder.finish(state.last_status());
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

/// Emit OSC 133 prompt-start (`A`) plus the window title set to the cwd. These
/// shell-integration sequences let terminals navigate between prompts and show
/// command status; they are only emitted on a TTY.
fn shell_integration_prompt(state: &ShellState) {
    let cwd = state.cwd().display().to_string();
    let cwd = state
        .lookup("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|home| cwd.strip_prefix(home).map(|rest| format!("~{rest}")))
        .unwrap_or(cwd);
    emit_osc(&format!(
        "\x1b]133;A\x07\x1b]2;agsh: {}\x07",
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
fn apply_confinement(state: &mut ShellState, options: &CliOptions) {
    fn parse_list(list: &str) -> Vec<String> {
        list.split([',', ' ', '\t'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    if let Ok(list) = std::env::var("AGSH_CONFINE") {
        let names = parse_list(&list);
        if !names.is_empty() {
            state.set_confine(&names);
        }
    }

    // `--allow LIST` without `--run`: confine THIS interactive session (the
    // agsh-routed gate) and propagate to children. The `--run` payload case is
    // handled separately (OS-enforced confine / agent refusal) in `main`.
    if let Some(list) = &options.allow {
        if options.run.is_some() {
            return;
        }
        let names = parse_list(list);
        if names.is_empty() {
            return;
        }
        state.set_confine(&names);
        if let Some(p) = state.confine_policy() {
            let list = p.to_list();
            state.export_var("AGSH_CONFINE", list);
        }
        // Shell shims so a child's own `bash -c '…'` is routed back through agsh.
        let _ = agsh_exec::install_confine_shims(state);
    }
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
    let source = match std::fs::read_to_string(&path) {
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

fn run_one(
    line: &str,
    executor: &mut Executor,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<i32, agsh_core::ShellError> {
    state.record_history(line);
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
    let findings = analyze_graph(&graph);
    for finding in findings
        .iter()
        .filter(|finding| finding.level >= RiskLevel::High)
    {
        eprintln!("agsh risk: {}: {}", finding.code, finding.message);
    }
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
                // Remaining args become positional parameters ($0, $1, …).
                options.script_args = args.by_ref().collect();
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
                options.script_args = args.by_ref().collect();
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
        "agsh - Aegis Shell\n\nUSAGE:\n  agsh [--output MODE] [--allow LIST] [--run COMMAND] [--rcfile FILE] [--norc] [-c COMMAND]\n\nSTARTUP:\n  interactive sessions source ~/.config/agsh/agshrc (or ~/.agshrc); --norc skips,\n  --rcfile FILE / $AGSH_RC picks another, $AGSH_NORC=1 disables\n\nINTERCEPTION (route the agent's own shell through agsh; off by default):\n  AGSH_INTERCEPT=compact agsh …   shim bash/sh/… to `agsh --observe` (real shell,\n                                  captured+rendered); nested shells pass through\n    :native  agsh interprets the command    :deep  also catch absolute-path\n    (instead of running the real shell)     /bin/bash + posix_spawn (preload)\n  toggle at runtime: `mode:intercept compact:deep` / `mode:intercept off`\n\nMODES:\n  raw | clean | compact | semantic | lossless-ref | silent | rich\n\nCONFINE (kernel-enforced capability sandbox for a leaf payload):\n  confine read-only -- python x.py  read+run; no writes/network/secret-reads\n  confine workspace -- ./build.sh   writes only within $PWD (+ a scratch dir)\n  confine offline -- npm test       network off; filesystem unchanged\n  confine convert -- ./batch.sh     exec-allowlist: may only exec `convert`\n  confine ls,df                     confine the current agsh session (sticky)\n    --rw PATH  add a writable root    --net/--no-net  toggle network\n    --explain  show capabilities      --dry-run  print profile, don't run\n    --force    run a refused agent    --best-effort  shim layer if no sandbox\n  enforced via sandbox-exec (macOS); Linux Landlock planned, fails closed\n  elsewhere. Self-managing agents (claude, …) are refused — use --allowedTools.\n\nAGSH TOOLS (ag-prefixed where a common CLI shares the name; bare otherwise):\n  agview FILE…   rich render (markdown, code, images)   agz DIR    frecent jump\n  agpatch        structured patch        agtrace/agtrust/agcontext/agmath/agjump\n  confine, peek, risk, snapshot, pty     stay bare (no common CLI conflict)\n  sessions       list/resume Claude & Codex sessions for this folder (sessions N)\n  mode:output M  set the session default output mode\n\nMODE SELECTION (highest priority first):\n  per-command wrapper   semantic git diff\n  --output flag         agsh --output compact -c 'pytest -q'\n  mode builtin          mode:output compact   (session default; `mode` shows all)\n  AGSH_OUTPUT_MODE env  AGSH_OUTPUT_MODE=semantic agsh -c 'cargo test'\n  ~/.config/agsh/token.toml  [mode] default = \"compact\"  (interactive sessions)\n  default               raw\n  (the config/`mode` default makes plain `ls` render like `compact ls`; it applies\n   to interactive sessions only — piped `agsh -c`/scripts stay raw)\n\nRICH RENDERING (human display, TTY only; raw bytes still pipe/redirect):\n  agview FILE...        render by type (markdown, JSON, CSV/TSV, diff, binary)\n  agview main.py        syntax-highlight source code (py, rs, js, ts, go, c, …)\n  agview image.png      show images inline (any terminal; crisp in iTerm2/Kitty)\n  AGSH_OUTPUT_MODE=rich  auto-render recognized command output\n\nTRACE:\n  raw output is captured in capturing modes and addressable via trace://<id>/...\n  trace                 list recent captured commands\n  trace <id>            print a command's raw stdout\n\nEXAMPLES:\n  agsh -c 'echo hello'\n  agsh --output semantic -c 'git status'\n  view README.md\n  semantic git diff"
    );
}

#[cfg(test)]
mod tests {
    use super::is_terminal_control_line;

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
}
