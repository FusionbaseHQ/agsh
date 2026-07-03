use std::path::{Path, PathBuf};
use std::process::Command;

use agsh_compat::{CommandResolution, Resolver};
use agsh_core::{CommandInvocation, ShellError};
use rustix::process::Signal;

use crate::state::LoopControlKind;
use crate::{CommandOutcome, ShellState};

const DEFAULT_COMMAND_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Names of all shell builtins, for command suggestions and completion.
pub fn builtin_names() -> &'static [&'static str] {
    &[
        "cd",
        "pwd",
        "export",
        "unset",
        "set",
        "exit",
        "echo",
        "printf",
        "true",
        "false",
        "type",
        "which",
        "command",
        "external",
        "builtin",
        "alias",
        "unalias",
        "abbr",
        "unabbr",
        "source",
        ".",
        "eval",
        "history",
        "jobs",
        "fg",
        "bg",
        "wait",
        "agjob",
        "kill",
        "exec",
        "ulimit",
        "umask",
        "break",
        "continue",
        "return",
        "local",
        "shift",
        "readonly",
        ":",
        "read",
        "test",
        "[",
        // agsh tools that shadow a common CLI are ag-prefixed (bare name freed for
        // the real tool); conflict-free ones keep the clean bare name.
        "agtrace",
        "pty",
        "agz",
        "agjump",
        "agtrust",
        "agview",
        "agcontext",
        "peek",
        "agpatch",
        "risk",
        "snapshot",
        "let",
        "times",
        "shopt",
        "agmath",
        "complete",
        "getopts",
        "trap",
        "declare",
        "typeset",
        "confine",
        "mode",
        "mode:output",
        "mode:intercept",
        "sessions",
        "resume",
        "keep",
        "help",
    ]
}

/// `trust`: mark the current directory's `.env` as trusted so it auto-activates
/// on entry. Without trust, project `.env` files are never sourced.
/// `times` (POSIX special built-in): print accumulated CPU times. The shell's
/// total process CPU time is reported (user field); per-stream user/sys split
/// and child accounting are best-effort given the available clocks.
fn builtin_times(_args: &[String], _state: &mut ShellState) -> CommandOutcome {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    let secs = ts.tv_sec as f64 + ts.tv_nsec as f64 / 1_000_000_000.0;
    let fmt = |s: f64| {
        let minutes = (s / 60.0).floor() as u64;
        format!("{}m{:.3}s", minutes, s - (minutes as f64) * 60.0)
    };
    let zero = fmt(0.0);
    let out = format!("{} {}\n{} {}\n", fmt(secs), zero, zero, zero);
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

/// `mode` — show or set session default modes, a namespaced family so more mode
/// aspects can be added over time:
///
/// * `mode:output compact`  — set an aspect's default (every command renders in it,
///   so `ls` behaves like `compact ls`)
/// * `mode:output`          — show one aspect
/// * `mode`                 — show all aspects
/// * `mode compact`         — shorthand for `mode:output compact`
/// * value `off`/`reset`    — clear that aspect back to the startup default
///
/// Per-command wrappers (`raw ls`) and `--output` still override per command.
/// Aspects: `output` (session default mode) and `intercept` (route the agent's
/// child shells through agsh at runtime, e.g. `mode:intercept compact:deep`).
fn builtin_mode(name: &str, args: &[String], state: &mut ShellState) -> CommandOutcome {
    let value = args.first().map(String::as_str);
    match name.strip_prefix("mode:") {
        // `mode:<aspect> [value]`
        Some("output") => mode_output(value, state),
        Some("intercept") => mode_intercept(value, state),
        Some(other) => CommandOutcome::captured(
            2,
            Vec::new(),
            format!("mode: unknown aspect '{other}' (known: output, intercept)\n").into_bytes(),
        ),
        // bare `mode`
        None => match value {
            // `mode` → show every aspect's current value.
            None => {
                let out = format!(
                    "output: {}\nintercept: {}\n",
                    current_output(state),
                    if crate::executor::intercept_active(state) {
                        "on"
                    } else {
                        "off"
                    },
                );
                CommandOutcome::captured(0, out.into_bytes(), Vec::new())
            }
            // `mode compact` → shorthand for the output aspect.
            Some(_) => mode_output(value, state),
        },
    }
}

/// Show or set the `intercept` aspect at runtime — route the session's future child
/// shells (`bash -c …`) through agsh. `mode:intercept compact[:native][:deep]` turns
/// it on; `mode:intercept off` turns it off. Takes effect for newly launched
/// commands (already-running processes keep their environment).
fn mode_intercept(value: Option<&str>, state: &mut ShellState) -> CommandOutcome {
    match value {
        None => {
            let cur = if crate::executor::intercept_active(state) {
                "on"
            } else {
                "off"
            };
            CommandOutcome::captured(0, format!("{cur}\n").into_bytes(), Vec::new())
        }
        Some("off" | "reset" | "default") => {
            crate::executor::uninstall_intercept(state);
            CommandOutcome::captured(0, b"shell interception off\n".to_vec(), Vec::new())
        }
        Some(spec) => match crate::executor::parse_intercept_spec(spec) {
            Some((mode, native, deep)) => {
                // Re-install from a clean slate so a re-toggle can't stack shim dirs.
                crate::executor::uninstall_intercept(state);
                let _ = crate::executor::install_intercept_shims(state, mode, native);
                let mut msg = format!("shell interception on: {}", mode.as_str());
                if native {
                    msg.push_str(":native");
                }
                if deep {
                    if crate::executor::install_deep_intercept(state, mode) {
                        msg.push_str(":deep");
                    } else {
                        msg.push_str(" (deep unavailable: interposer library not found)");
                    }
                }
                msg.push_str(" — applies to newly launched commands\n");
                CommandOutcome::captured(0, msg.into_bytes(), Vec::new())
            }
            None => CommandOutcome::captured(
                2,
                Vec::new(),
                format!(
                    "mode: unknown interception spec '{spec}' \
                     (e.g. compact, compact:deep, compact:native, off)\n"
                )
                .into_bytes(),
            ),
        },
    }
}

/// The session default for the `output` aspect (the `mode` builtin's name for it).
fn current_output(state: &ShellState) -> &'static str {
    state
        .default_output_mode()
        .map(agsh_output::OutputMode::as_str)
        .unwrap_or("raw")
}

/// Show or set the `output` mode aspect.
fn mode_output(value: Option<&str>, state: &mut ShellState) -> CommandOutcome {
    use std::str::FromStr;
    match value {
        None => CommandOutcome::captured(
            0,
            format!("{}\n", current_output(state)).into_bytes(),
            Vec::new(),
        ),
        Some("off" | "reset" | "default") => {
            state.set_default_output_mode(None);
            CommandOutcome::captured(0, Vec::new(), Vec::new())
        }
        Some(name) => match agsh_output::OutputMode::from_str(name) {
            Ok(mode) => {
                state.set_default_output_mode(Some(mode));
                CommandOutcome::captured(0, Vec::new(), Vec::new())
            }
            Err(_) => CommandOutcome::captured(
                2,
                Vec::new(),
                format!(
                    "mode: unknown output mode '{name}' \
                     (raw, clean, compact, semantic, lossless-ref, silent, rich)\n"
                )
                .into_bytes(),
            ),
        },
    }
}

/// `let expr...`: evaluate each arithmetic expression (assignments included).
/// Exit status is 0 if the last expression is non-zero, else 1 (bash semantics).
fn builtin_let(args: &[String], state: &mut ShellState) -> CommandOutcome {
    if args.is_empty() {
        return CommandOutcome::captured(2, Vec::new(), b"let: expression expected\n".to_vec());
    }
    let mut last = 0i64;
    for arg in args {
        match crate::executor::eval_arithmetic(arg, state) {
            Ok(value) => last = value,
            Err(error) => {
                return CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("let: {error}\n").into_bytes(),
                );
            }
        }
    }
    CommandOutcome::captured(i32::from(last == 0), Vec::new(), Vec::new())
}

/// `getopts optstring name [args...]`: parse one option per call, setting
/// `name`, `$OPTARG`, and `$OPTIND`. Returns 1 when options are exhausted.
fn builtin_getopts(args: &[String], state: &mut ShellState) -> CommandOutcome {
    if args.len() < 2 {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"getopts: usage: getopts optstring name [arg...]\n".to_vec(),
        );
    }
    let optstring = args[0].clone();
    let name = args[1].clone();
    let silent = optstring.starts_with(':');
    let operands: Vec<String> = if args.len() > 2 {
        args[2..].to_vec()
    } else {
        state.positionals()
    };

    let set = |state: &mut ShellState, var: &str, val: &str| state.set_var(var, val.to_string());
    let mut optind: usize = state
        .lookup("OPTIND")
        .and_then(|v| v.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(1);
    let mut charpos = state.getopts_char();

    let result = loop {
        let Some(arg) = operands.get(optind - 1) else {
            break 1;
        };
        let chars: Vec<char> = arg.chars().collect();
        if charpos == 0 {
            if chars.first() != Some(&'-') || arg == "-" {
                break 1; // non-option argument
            }
            if arg == "--" {
                optind += 1;
                break 1;
            }
            charpos = 1;
        }
        if charpos >= chars.len() {
            optind += 1;
            charpos = 0;
            continue;
        }
        let opt = chars[charpos];
        charpos += 1;
        let advance_arg = charpos >= chars.len();

        // Look up `opt` in the optstring (which may lead with ':' for silent mode).
        let spec = optstring.find(opt);
        let valid = spec.is_some() && opt != ':';
        if !valid {
            set(state, &name, "?");
            if silent {
                set(state, "OPTARG", &opt.to_string());
            } else {
                state.unset("OPTARG");
                eprintln!("getopts: illegal option -- {opt}");
            }
            if advance_arg {
                optind += 1;
                charpos = 0;
            }
            break 0;
        }
        let wants_arg = optstring[spec.unwrap() + opt.len_utf8()..].starts_with(':');
        if wants_arg {
            if !advance_arg {
                set(
                    state,
                    "OPTARG",
                    &chars[charpos..].iter().collect::<String>(),
                );
                optind += 1;
                charpos = 0;
            } else if let Some(next) = operands.get(optind).cloned() {
                set(state, "OPTARG", &next);
                optind += 2;
                charpos = 0;
            } else {
                // Missing required argument.
                optind += 1;
                charpos = 0;
                if silent {
                    set(state, &name, ":");
                    set(state, "OPTARG", &opt.to_string());
                } else {
                    set(state, &name, "?");
                    state.unset("OPTARG");
                    eprintln!("getopts: option requires an argument -- {opt}");
                }
                break 0;
            }
            set(state, &name, &opt.to_string());
            break 0;
        }
        set(state, &name, &opt.to_string());
        state.unset("OPTARG");
        if advance_arg {
            optind += 1;
            charpos = 0;
        }
        break 0;
    };

    state.set_var("OPTIND", optind.to_string());
    state.set_getopts_char(charpos);
    CommandOutcome::captured(result, Vec::new(), Vec::new())
}

/// `trap [action] sig...` / `trap -p` / `trap - sig...`: install, list, or reset
/// trap handlers. EXIT/ERR/signal firing is driven by the main loop.
fn builtin_trap(args: &[String], state: &mut ShellState) -> CommandOutcome {
    // List traps: no args, or `-p [sig...]`.
    if args.is_empty() || args[0] == "-p" {
        let filter = if args.first().map(String::as_str) == Some("-p") {
            &args[1..]
        } else {
            args
        };
        let mut out = String::new();
        for (cond, action) in state.trap_entries() {
            let matched = filter.is_empty()
                || filter
                    .iter()
                    .any(|s| crate::state::normalize_trap_signal(s) == cond);
            if matched {
                let escaped = action.replace('\'', "'\\''");
                out.push_str(&format!("trap -- '{escaped}' {cond}\n"));
            }
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }

    let action = &args[0];
    let signals = &args[1..];
    if signals.is_empty() {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"trap: usage: trap [-p] [action] signal...\n".to_vec(),
        );
    }
    for signal in signals {
        if action == "-" {
            state.set_trap(signal, None);
        } else {
            state.set_trap(signal, Some(action.clone()));
        }
    }
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

fn builtin_trust(_args: &[String], state: &mut ShellState) -> CommandOutcome {
    match state.trust_current_env() {
        Some(count) => CommandOutcome::captured(
            0,
            format!(
                "trust: activated .env ({count} variables) for {}\n",
                state.cwd().display()
            )
            .into_bytes(),
            Vec::new(),
        ),
        None => CommandOutcome::captured(
            1,
            Vec::new(),
            b"trust: no .env file in the current directory\n".to_vec(),
        ),
    }
}

pub fn is_builtin(name: &str) -> bool {
    // `(( expr ))` arithmetic command is dispatched on the builtin path.
    if name.starts_with("((") && name.ends_with("))") {
        return true;
    }
    // `mode:<aspect>` (e.g. `mode:output`) is the session-mode builtin family.
    if name.starts_with("mode:") {
        return true;
    }
    matches!(
        name,
        "cd" | "pwd"
            | "export"
            | "unset"
            | "set"
            | "exit"
            | "echo"
            | "printf"
            | "true"
            | "false"
            | "type"
            | "which"
            | "command"
            | "external"
            | "builtin"
            | "alias"
            | "unalias"
            | "abbr"
            | "unabbr"
            | "source"
            | "."
            | "eval"
            | "history"
            | "jobs"
            | "fg"
            | "bg"
            | "wait"
            | "agjob"
            | "kill"
            | "exec"
            | "ulimit"
            | "umask"
            | "break"
            | "continue"
            | "return"
            | "local"
            | "shift"
            | "readonly"
            | ":"
            | "read"
            | "test"
            | "["
            | "[["
            | "agtrace"
            | "agtrust"
            | "agz"
            | "agjump"
            | "agcontext"
            | "peek"
            | "agpeek"
            | "agpatch"
            | "risk"
            | "agrisk"
            | "snapshot"
            | "agsnapshot"
            | "let"
            | "times"
            | "shopt"
            | "agmath"
            | "complete"
            | "getopts"
            | "trap"
            | "declare"
            | "typeset"
            | "confine"
            | "agconfine"
            | "mode"
            | "sessions"
            | "resume"
            | "keep"
            | "help"
    )
}

/// Readable overview of agsh's own commands, shown by bare `help`. Focused on the
/// agsh-specific tools; standard POSIX builtins are listed by name (`type NAME`).
const HELP_OVERVIEW: &str = "\
agsh — built-in commands            (help <command> for details · --help for CLI flags)

Output modes — how command output is shown to an agent
  mode                    show the current output / intercept mode
  mode:output MODE        set the default view: raw clean compact semantic lossless-ref silent rich
  mode:output off         reset to the session startup default
  mode:intercept SPEC     route the agent's own bash/sh through agsh (SPEC, or: off)

Rich rendering — human display, TTY only (pipes & redirects still get exact bytes)
  agview FILE...          render markdown, source code, JSON, CSV/TSV, diffs, and images

Sandbox
  confine PRESET -- CMD   kernel-enforced sandbox: read-only | workspace | offline | <exec-allowlist>
  pty CMD                 run CMD under a pseudo-terminal

Agent & workflow tools
  sessions [N]            list / resume the Claude & Codex sessions for this folder
  resume [list | N]       restore the shell state of a session that died (crash/HUP)
  keep -- CMD             run CMD under the keep broker: it survives this terminal
  agtrace [id]            list, or print, raw captured command output (trace://)
  agz DIR   (agjump)      jump to a frecently-used directory by substring
  agtrust                 trust this project's .env so it auto-activates here
  agcontext [--json]      structured shell / project context for an agent
  peek FILE               print a line-numbered slice of a file
  agpatch FILE            apply a unified diff (read from stdin / a heredoc)
  risk 'CMD'              rate how dangerous a command is before you run it
  snapshot                take a git snapshot as a rollback point
  agmath EXPR             evaluate integer or floating-point math
  agjob CMD               run CMD in the background with its output captured

Standard POSIX builtins (use `type NAME` for one)
  cd pwd export unset set alias jobs fg bg wait kill trap read test [ [[ source . eval
  exec local return shift readonly getopts declare printf echo history ulimit umask …

Naming: agsh's own tools are ag-prefixed only where a bare name would shadow a real
CLI (agview, agpatch, agmath, agz); conflict-free ones keep the bare name
(confine, peek, risk, snapshot, pty) and also accept an ag-alias (agpeek, agrisk,
agsnapshot). Type `help <command>` for usage and examples.
";

/// Detailed, example-driven help for one agsh command; `None` for unknown names.
fn help_topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "mode" | "mode:output" | "mode:intercept" => {
            "\
mode — control, for the session, how command output is presented to an agent.

  mode                    show all current mode aspects
  mode:output MODE        set the default output view (below); `mode MODE` is shorthand
  mode:output             show just the output aspect
  mode:output off         reset to the session startup default
  mode:intercept SPEC     route the agent's own bash/sh through agsh; `off` to stop

Output MODEs:
  raw           exact bytes, streamed (the default)
  clean         raw with ANSI / control noise removed
  compact       trimmed + de-duplicated
  semantic      a structured observation of recognized commands
  lossless-ref  compact view + a trace:// pointer to the full raw output
  silent        suppress display, keep exit status + trace
  rich          human rich rendering (see `help agview`)

The default applies to interactive sessions only; piped `agsh -c` and scripts stay
raw, so pipelines are never transformed.
"
        }
        "agview" => {
            "\
agview FILE... — render a file for the human terminal, detecting its type. Rich
rendering is TTY-only: piped or redirected output is always the exact bytes.

  agview README.md      markdown            agview data.json      pretty JSON
  agview report.csv     aligned table       agview change.diff     colored diff
  agview src/main.rs    syntax highlight     agview photo.png       inline image
                        (py rs js ts go c …) (crisp in iTerm2/Kitty; half-blocks elsewhere)
"
        }
        "confine" => {
            "\
confine — run a payload under a kernel-enforced capability sandbox (macOS Seatbelt;
Linux Landlock is planned and fails closed until then).

  confine read-only -- python x.py   read + run; no writes, network, or secret reads
  confine workspace -- ./build.sh     writes only within $PWD (+ a private scratch dir)
  confine offline -- npm test         network off; filesystem unchanged
  confine convert -- ./thumb.sh       exec-allowlist: the payload may only run `convert`
  confine ls,df                       confine the CURRENT session (sticky) to an allowlist

  --rw PATH   add a writable root     --net / --no-net   toggle network
  --explain   show the capabilities   --dry-run          print the profile, don't run
  --force     run a refused agent     --best-effort      shim layer if no kernel backend

Self-managing agents (claude, …) are refused — use their own tool-permission systems.
See docs/CONFINE.md for the guarantees and non-guarantees.
"
        }
        "sessions" => {
            "\
sessions — find and resume the Claude Code / Codex sessions that ran in this folder.

  sessions        list sessions here, newest first (agent, age, id, summary)
  sessions N      resume the Nth listed session (claude --resume / codex resume)
  sessions --all  every folder, not just this one
"
        }
        "keep" => {
            "\
keep — run a command on a broker-held PTY so it survives this terminal. The
`agshd` broker (auto-started, per user) owns the job's pseudo-terminal; close
the window, drop the SSH connection, or crash the shell and the job keeps
running, its output journaled to a log.

  keep -- CMD ARGS…    start CMD kept; on a terminal, attach immediately
  keep list            kept jobs (id, state, age, command; * = attached)
  keep attach ID       reattach with scrollback replay (Ctrl-] detaches)
  keep tail ID [N]     print the last N bytes (default 4096) of the output log
  keep kill ID [SIG]   signal the job's process group (default TERM)
  keep rm ID           drop a finished job from the list
  keep stop            stop the broker (hangs up every kept job)

The job gets a real controlling terminal (Ctrl-C works), your exported env,
and your cwd. Detaching never kills it; exiting your shell never kills it.
"
        }
        "resume" => {
            "\
resume — restore the shell state of a session that died without a clean exit
(crash, closed terminal / SIGHUP, reboot). Interactive sessions journal their
state deltas as they happen, so nothing is saved at exit — and nothing is lost
without one.

  resume          restore the most recent dead session
  resume list     show restorable sessions (age, cwd, changes, what was running)
  resume N        restore the Nth listed session

A startup banner pointing at restorable sessions is OFF by default; opt in
with `[session] restore_banner = true` in ~/.config/agsh/token.toml (or
AGSH_RESUME_BANNER=1). Even enabled, it only fires for likely lost work — a
crash, or a hangup while something was running — never for a terminal window
closed at an idle prompt.

Restores cwd, exported vars, shell vars, aliases, abbreviations, functions, and
set/shopt options by replaying journaled deltas — commands are never re-run. If
an agent (claude/codex) was running when the session died, `sessions` can resume
its conversation too. A restored journal is consumed (never offered twice).
"
        }
        "agtrace" | "trace" => {
            "\
agtrace — inspect the raw output captured in capturing modes (addressable as trace://).

  agtrace                       list recent captured commands
  agtrace <id>                  print a command's full raw stdout
  agtrace <id> --grep PATTERN   filter that output
  agtrace <id> START:END        print a line range
"
        }
        "agz" | "agjump" => {
            "\
agz DIR  (alias: agjump) — jump to a directory you've used before, ranked by
frecency (frequency + recency). DIR is a substring of the target path.
  agz proj      cd to the best-matching frecent directory containing \"proj\"
"
        }
        "agtrust" => {
            "\
agtrust — mark the current project directory as trusted, so its .env is sourced
automatically when you enter it (project environments require explicit trust).
"
        }
        "agcontext" => {
            "\
agcontext [--json] — print a structured snapshot of the shell / project context
(cwd, git, recent commands, last result) for an agent. --json for machine parsing.
"
        }
        "peek" | "agpeek" => {
            "\
peek FILE  (alias: agpeek) — print a file with line numbers.
  peek FILE                 the whole file, numbered
  peek FILE --range 20:40   only lines 20-40
"
        }
        "agpatch" => {
            "\
agpatch FILE — apply a unified diff to FILE, read from stdin or a heredoc.
  agpatch src.rs <<'EOF'
  @@ -1,3 +1,3 @@
   a
  -b
  +B
   c
  EOF
"
        }
        "risk" | "agrisk" => {
            "\
risk 'CMD'  (alias: agrisk) — rate how dangerous a command is BEFORE running it,
flagging things like recursive deletes. Advisory only — it does not block.
  risk 'rm -rf /'    → flags fs.recursive_delete
"
        }
        "snapshot" | "agsnapshot" => {
            "\
snapshot  (alias: agsnapshot) — take a git snapshot of the working tree as a
rollback point, without disturbing your index or current branch.
"
        }
        "pty" => {
            "\
pty CMD — run CMD under a pseudo-terminal, so tools that check for a TTY (color,
interactive prompts, progress bars) behave as they would in a real terminal.
"
        }
        "agmath" => {
            "\
agmath EXPR — evaluate an arithmetic or floating-point expression.
  agmath '2 + 3 * 4'    → 14
  agmath 'sqrt(2)'      → 1.4142135…
"
        }
        "agjob" => {
            "\
agjob CMD — run CMD in the background with its stdout/stderr captured to a log,
so a non-blocking command's output stays recoverable (unlike a bare `&`).
"
        }
        _ => return None,
    })
}

/// `help` — a readable overview of agsh's own commands, or `help <command>` detail.
fn builtin_help(args: &[String]) -> CommandOutcome {
    match args.first() {
        None => CommandOutcome::captured(0, HELP_OVERVIEW.as_bytes().to_vec(), Vec::new()),
        Some(topic) => match help_topic(topic) {
            Some(text) => CommandOutcome::captured(0, text.as_bytes().to_vec(), Vec::new()),
            None => CommandOutcome::captured(
                1,
                Vec::new(),
                format!(
                    "help: no help topic for `{topic}`. Run `help` for the command list, \
                     or `type {topic}` to see what it resolves to.\n"
                )
                .into_bytes(),
            ),
        },
    }
}

pub fn run_builtin(
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<CommandOutcome, ShellError> {
    let name = invocation.command_name().unwrap_or_default();
    let args = &invocation.argv[1..];
    match name {
        "cd" => builtin_cd(args, state),
        "pwd" => Ok(CommandOutcome::captured(
            0,
            format!("{}\n", state.cwd().display()).into_bytes(),
            Vec::new(),
        )),
        "export" => builtin_export(args, state),
        "unset" => builtin_unset(args, state),
        "set" => Ok(builtin_set(args, state)),
        "exit" => {
            let code = match args.first() {
                None => state.last_status(),
                Some(arg) => match arg.parse::<i64>() {
                    Ok(value) => value.rem_euclid(256) as i32,
                    Err(_) => {
                        return Ok(CommandOutcome::captured(
                            2,
                            Vec::new(),
                            format!("exit: {arg}: numeric argument required\n").into_bytes(),
                        ));
                    }
                },
            };
            state.request_exit();
            Ok(CommandOutcome::captured(code, Vec::new(), Vec::new()))
        }
        "echo" => Ok(builtin_echo(args)),
        "printf" => builtin_printf(args),
        "true" => Ok(CommandOutcome::captured(0, Vec::new(), Vec::new())),
        "false" => Ok(CommandOutcome::captured(1, Vec::new(), Vec::new())),
        "type" => builtin_type(args, state),
        "which" => builtin_which(args, state),
        "command" => builtin_command(args, state),
        "alias" => builtin_alias(args, state),
        "unalias" => builtin_unalias(args, state),
        "abbr" => builtin_abbr(args, state),
        "unabbr" => builtin_unabbr(args, state),
        "history" => Ok(builtin_history(args, state)),
        "jobs" => Ok(builtin_jobs(args, state)),
        "fg" => Ok(builtin_fg(args, state)),
        "bg" => Ok(builtin_bg(args, state)),
        "wait" => Ok(builtin_wait(args, state)),
        "agjob" => Ok(builtin_agjob(args, state)),
        "kill" => builtin_kill(args, state),
        "exec" => Err(ShellError::execution(
            "exec: process replacement is handled by executor",
        )),
        "ulimit" => builtin_ulimit(args),
        "umask" => builtin_umask(args),
        "break" => Ok(builtin_loop_control(
            "break",
            args,
            state,
            LoopControlKind::Break,
        )),
        "continue" => Ok(builtin_loop_control(
            "continue",
            args,
            state,
            LoopControlKind::Continue,
        )),
        "return" => Ok(builtin_return(args, state)),
        "local" => Ok(builtin_local(args, state)),
        "shift" => Ok(builtin_shift(args, state)),
        "readonly" => Ok(builtin_readonly(args, state)),
        ":" => Ok(CommandOutcome::captured(0, Vec::new(), Vec::new())),
        "test" => Ok(builtin_test(args, "test", state.cwd())),
        "[" => Ok(builtin_bracket_test(args, state.cwd())),
        "[[" => Ok(crate::executor::eval_double_bracket(args, state)),
        "agtrace" => Ok(builtin_trace(args, state)),
        "agtrust" => Ok(builtin_trust(args, state)),
        "let" => Ok(builtin_let(args, state)),
        "times" => Ok(builtin_times(args, state)),
        "shopt" => Ok(builtin_shopt(args, state)),
        "agmath" => Ok(builtin_math(args, state)),
        "complete" => Ok(builtin_complete(args, state)),
        "getopts" => Ok(builtin_getopts(args, state)),
        "trap" => Ok(builtin_trap(args, state)),
        "declare" | "typeset" => Ok(builtin_declare(args, state)),
        "agcontext" => Ok(crate::agent::context(args, state)),
        "agpeek" | "peek" => Ok(crate::agent::peek(args, state)),
        "agrisk" | "risk" => Ok(crate::agent::risk(args, state)),
        "agsnapshot" | "snapshot" => Ok(crate::agent::snapshot(args, state)),
        "agz" | "agjump" => builtin_z(args, state),
        n if n == "mode" || n.starts_with("mode:") => Ok(builtin_mode(n, args, state)),
        "sessions" => Ok(crate::sessions::builtin_sessions(args, state)),
        "resume" => Ok(crate::journal::builtin_resume(args, state)),
        "keep" => Ok(crate::keep::builtin_keep(args, state)),
        "help" => Ok(builtin_help(args)),
        "external" => Err(ShellError::execution("external: missing command")),
        "builtin" => Err(ShellError::execution("builtin: missing command")),
        other => Err(ShellError::unsupported(format!(
            "builtin not implemented: {other}"
        ))),
    }
}

fn builtin_loop_control(
    name: &str,
    args: &[String],
    state: &mut ShellState,
    kind: LoopControlKind,
) -> CommandOutcome {
    let levels = match args {
        [] => 1,
        [level] => match level.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                return CommandOutcome::captured(
                    2,
                    Vec::new(),
                    format!("{name}: loop count must be a positive number\n").into_bytes(),
                )
            }
        },
        _ => {
            return CommandOutcome::captured(
                2,
                Vec::new(),
                format!("{name}: too many arguments\n").into_bytes(),
            )
        }
    };

    if state.loop_depth() == 0 {
        // bash/sh treat break/continue outside a loop as a successful no-op.
        return CommandOutcome::captured(0, Vec::new(), Vec::new());
    }

    state.request_loop_control(kind, levels);
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

fn builtin_return(args: &[String], state: &mut ShellState) -> CommandOutcome {
    if !state.in_function() && !state.in_source() {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            b"return: can only be used in a function or sourced script\n".to_vec(),
        );
    }
    let code = match args.first() {
        None => state.last_status(),
        Some(arg) => match arg.parse::<i64>() {
            Ok(value) => value.rem_euclid(256) as i32,
            Err(_) => {
                return CommandOutcome::captured(
                    2,
                    Vec::new(),
                    format!("return: {arg}: numeric argument required\n").into_bytes(),
                );
            }
        },
    };
    state.request_return(code);
    CommandOutcome::captured(code, Vec::new(), Vec::new())
}

fn builtin_local(args: &[String], state: &mut ShellState) -> CommandOutcome {
    if !state.in_function() {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            b"local: can only be used in a function\n".to_vec(),
        );
    }

    let mut err = String::new();
    let mut exit_code = 0;
    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            if !is_identifier(name) {
                err.push_str(&format!("local: {name}: not a valid identifier\n"));
                exit_code = 1;
                continue;
            }
            state.declare_local(name);
            state.set_var(name, value);
        } else if is_identifier(arg) {
            state.declare_local(arg);
        } else {
            err.push_str(&format!("local: {arg}: not a valid identifier\n"));
            exit_code = 1;
        }
    }

    CommandOutcome::captured(exit_code, Vec::new(), err.into_bytes())
}

fn builtin_cd(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    let mut print_target = false;
    let target = match args.first().map(String::as_str) {
        Some("-") => {
            let Some(old) = state.lookup("OLDPWD").map(str::to_string) else {
                return Ok(CommandOutcome::captured(
                    1,
                    Vec::new(),
                    b"agsh: cd: OLDPWD not set\n".to_vec(),
                ));
            };
            print_target = true;
            PathBuf::from(old)
        }
        Some(first) => PathBuf::from(first),
        None => {
            let Some(home) = state.lookup("HOME").map(str::to_string) else {
                return Ok(CommandOutcome::captured(
                    1,
                    Vec::new(),
                    b"agsh: cd: HOME not set\n".to_vec(),
                ));
            };
            PathBuf::from(home)
        }
    };

    let absolute = if target.is_absolute() {
        target
    } else {
        state.cwd().join(target)
    };
    // Default to logical (-L) path handling like bash: normalize `.`/`..`
    // textually rather than resolving symlinks.
    let logical = normalize_logical_path(&absolute);
    if let Err(error) = std::env::set_current_dir(&logical) {
        return Ok(CommandOutcome::captured(
            1,
            Vec::new(),
            format!("agsh: cd: {}: {error}\n", logical.display()).into_bytes(),
        ));
    }
    let previous = state.cwd().display().to_string();
    state.set_cwd(logical.clone());
    state.export_var("OLDPWD", previous);
    state.export_var("PWD", logical.display().to_string());
    // Apply the new directory's trusted project env (no-op unless trusted).
    state.activate_project_env();
    let out = if print_target {
        format!("{}\n", logical.display()).into_bytes()
    } else {
        Vec::new()
    };
    Ok(CommandOutcome::captured(0, out, Vec::new()))
}

/// Resolve `.` and `..` components lexically, without touching the filesystem
/// (logical / `-L` semantics).
fn normalize_logical_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `z <terms...>` / `jump <terms...>`: change to the most frecent directory
/// whose path contains all the query terms (case-insensitive). With no args it
/// behaves like `cd` to HOME. Deterministic: ranks by the history frecency score.
fn builtin_z(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    if args.is_empty() {
        return builtin_cd(&[], state);
    }
    let terms: Vec<String> = args.iter().map(|a| a.to_lowercase()).collect();
    let target = state.frecent_dirs().into_iter().find(|(dir, _)| {
        let lower = dir.to_lowercase();
        terms.iter().all(|t| lower.contains(t)) && Path::new(dir).is_dir()
    });
    match target {
        Some((dir, _)) => builtin_cd(&[dir], state),
        None => Ok(CommandOutcome::captured(
            1,
            Vec::new(),
            format!("z: no frecent directory matches: {}\n", args.join(" ")).into_bytes(),
        )),
    }
}

fn builtin_trace(args: &[String], state: &ShellState) -> CommandOutcome {
    // `agtrace grep <pattern> <ref>` — search a captured trace for a bounded,
    // structured result instead of re-running the command or dumping the whole raw.
    if args.first().map(String::as_str) == Some("grep") {
        return trace_grep(&args[1..], state);
    }
    let (opts, positional, flags) = match crate::agent::parse_slice_flags(args) {
        Ok(parts) => parts,
        Err(e) => {
            return CommandOutcome::captured(2, Vec::new(), format!("trace: {e}\n").into_bytes())
        }
    };

    // No id: list recent captured traces.
    let Some(reference) = positional.first() else {
        let mut out = String::new();
        for (id, exit, command) in state.trace_summaries() {
            out.push_str(&format!("{id}\texit {exit}\t{command}\n"));
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    };

    // Stream: --stderr flag, or a trailing stdout/stderr positional.
    let stderr = flags.iter().any(|f| f == "--stderr")
        || positional.get(1).map(String::as_str) == Some("stderr");
    let base = reference
        .trim_end_matches("/stdout")
        .trim_end_matches("/stderr");
    let resolved = format!("{base}/{}", if stderr { "stderr" } else { "stdout" });

    match state.resolve_trace(&resolved) {
        Some(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let sliced = crate::agent::apply_slice(&text, &opts);
            CommandOutcome::captured(0, sliced.into_bytes(), Vec::new())
        }
        None => CommandOutcome::captured(
            1,
            Vec::new(),
            format!("trace: {reference}: not found\n").into_bytes(),
        ),
    }
}

/// `agtrace grep <pattern> <ref> [--stderr]` — search a captured trace (a
/// `trace://<id>` reference resolved in-session, or a disk-backed `raw:` file path)
/// and return a bounded, structured result: total match count plus the first N
/// numbered matching lines. Lets an agent query a large output without re-running
/// the command or reading back the whole raw stream. grep-style exit: 0 = matches,
/// 1 = none, 2 = usage/not-found.
fn trace_grep(args: &[String], state: &ShellState) -> CommandOutcome {
    let want_stderr = args.iter().any(|a| a == "--stderr");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (Some(pattern), Some(reference)) = (positional.first(), positional.get(1)) else {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"trace: usage: agtrace grep <pattern> <ref> [--stderr]\n".to_vec(),
        );
    };
    // Resolve the bytes: a disk-backed file path, else a `trace://<id>` reference.
    let bytes = if std::path::Path::new(reference.as_str()).is_file() {
        std::fs::read(reference.as_str()).ok()
    } else {
        let base = reference
            .trim_end_matches("/stdout")
            .trim_end_matches("/stderr");
        let resolved = format!("{base}/{}", if want_stderr { "stderr" } else { "stdout" });
        state.resolve_trace(&resolved)
    };
    let Some(bytes) = bytes else {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            format!("trace: {reference}: not found\n").into_bytes(),
        );
    };
    // Bounded regex (guards against pathological patterns); literal fallback.
    let re = regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .ok();
    const MAX_SHOWN: usize = 100;
    let text = String::from_utf8_lossy(&bytes);
    let mut total = 0usize;
    let mut shown = String::new();
    for (i, line) in text.lines().enumerate() {
        let hit = match &re {
            Some(r) => r.is_match(line),
            None => line.contains(pattern.as_str()),
        };
        if !hit {
            continue;
        }
        total += 1;
        if total <= MAX_SHOWN {
            let clipped: String = line.chars().take(300).collect();
            shown.push_str(&format!("{}: {clipped}\n", i + 1));
        }
    }
    let header = if total > MAX_SHOWN {
        format!("[{total} matches, showing first {MAX_SHOWN}]\n")
    } else {
        format!("[{total} match{}]\n", if total == 1 { "" } else { "es" })
    };
    let exit = i32::from(total == 0);
    CommandOutcome::captured(exit, format!("{header}{shown}").into_bytes(), Vec::new())
}

fn builtin_shift(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let count = match args.first() {
        None => 1,
        Some(arg) => match arg.parse::<usize>() {
            Ok(value) => value,
            Err(_) => {
                return CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("shift: {arg}: numeric argument required\n").into_bytes(),
                );
            }
        },
    };
    if state.shift_positionals(count) {
        CommandOutcome::captured(0, Vec::new(), Vec::new())
    } else {
        CommandOutcome::captured(1, Vec::new(), Vec::new())
    }
}

fn builtin_readonly(args: &[String], state: &mut ShellState) -> CommandOutcome {
    for arg in args {
        if arg == "-p" {
            continue;
        }
        if let Some((key, value)) = arg.split_once('=') {
            state.set_var(key, value);
            state.mark_readonly(key);
        } else {
            state.mark_readonly(arg);
        }
    }
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

/// `math [-p PREC] EXPR...`: evaluate a floating-point expression (bash has no
/// float arithmetic). Supports + - * / % **, parens, variables, constants
/// (pi/e), and functions (sqrt/sin/cos/log/pow/abs/floor/ceil/round/min/max/…).
fn builtin_math(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut precision: Option<usize> = None;
    let mut rest = args;
    if let [flag, p, tail @ ..] = args {
        if flag == "-p" {
            precision = p.parse::<usize>().ok();
            rest = tail;
        }
    }
    let expr = rest.join(" ");
    if expr.trim().is_empty() {
        return CommandOutcome::captured(2, Vec::new(), b"math: missing expression\n".to_vec());
    }
    let vars = crate::executor::StateFloatVars(state);
    match crate::math::eval(&expr, &vars) {
        Ok(value) => {
            let out = match precision {
                Some(p) => format!("{value:.p$}\n"),
                None => format!("{}\n", crate::math::format_result(value)),
            };
            CommandOutcome::captured(0, out.into_bytes(), Vec::new())
        }
        Err(e) => CommandOutcome::captured(1, Vec::new(), format!("math: {e}\n").into_bytes()),
    }
}

/// `complete [-W wordlist] [-r] CMD...`: register programmable completion word
/// lists for commands (consumed by the line editor). `-W` words may be
/// `word:description`. `-r` removes a command's spec. `-p`/no-args lists specs.
fn builtin_complete(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut wordlist: Option<String> = None;
    let mut remove = false;
    let mut commands = Vec::new();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "-W" => {
                wordlist = args.get(i + 1).cloned();
                i += 2;
            }
            "-r" => {
                remove = true;
                i += 1;
            }
            "-p" => i += 1,
            other => {
                commands.push(other.to_string());
                i += 1;
            }
        }
    }

    if commands.is_empty() {
        // List current specs in re-inputtable form.
        let mut out = String::new();
        for (cmd, words) in state.completion_specs() {
            out.push_str(&format!("complete -W '{}' {cmd}\n", words.join(" ")));
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }
    for cmd in &commands {
        if remove {
            state.remove_completion_spec(cmd);
        } else if let Some(words) = &wordlist {
            let list = words.split_whitespace().map(String::from).collect();
            state.register_completion_spec(cmd, list);
        }
    }
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

/// Known `shopt` options agsh honors.
pub(crate) const SHOPT_NAMES: &[&str] =
    &["globstar", "extglob", "nullglob", "dotglob", "nocaseglob"];

/// `shopt [-s|-u|-q] [name...]`: set/unset/query shell options. With no `-s/-u`,
/// lists option states (`name on|off`).
fn builtin_shopt(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut set = false;
    let mut unset = false;
    let mut quiet = false;
    let mut names = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-s" => set = true,
            "-u" => unset = true,
            "-q" => quiet = true,
            other => names.push(other.to_string()),
        }
    }
    if set || unset {
        for name in &names {
            state.set_shopt(name, set);
        }
        return CommandOutcome::captured(0, Vec::new(), Vec::new());
    }
    // Query / list.
    let listed: Vec<&str> = if names.is_empty() {
        SHOPT_NAMES.to_vec()
    } else {
        names.iter().map(String::as_str).collect()
    };
    let mut out = String::new();
    let mut all_on = true;
    for name in &listed {
        let on = state.shopt(name);
        all_on &= on;
        if !quiet {
            out.push_str(&format!("{name}\t{}\n", if on { "on" } else { "off" }));
        }
    }
    CommandOutcome::captured(i32::from(!all_on), out.into_bytes(), Vec::new())
}

/// Parse an associative-array literal body: `[k]=v [k2]=v2` or alternating
/// `k v k2 v2`. (Whitespace-split; no per-element expansion.)
fn parse_assoc_pairs(inner: &str) -> Vec<(String, String)> {
    let toks: Vec<&str> = inner.split_whitespace().collect();
    let mut pairs = Vec::new();
    if toks.iter().any(|t| t.starts_with('[')) {
        for t in toks {
            if let Some(rest) = t.strip_prefix('[') {
                if let Some((k, v)) = rest.split_once("]=") {
                    pairs.push((k.to_string(), v.to_string()));
                }
            }
        }
    } else {
        let mut it = toks.into_iter();
        while let Some(k) = it.next() {
            pairs.push((k.to_string(), it.next().unwrap_or("").to_string()));
        }
    }
    pairs
}

/// `declare`/`typeset [-i -x -r -p -a -A] [name[=value]]`: `-i` integer, `-x`
/// export, `-r` readonly (enforced), `-p` print, `-a` indexed array, `-A`
/// associative array.
fn builtin_declare(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let (mut export, mut integer, mut print, mut readonly, mut assoc) =
        (false, false, false, false, false);
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg.len() > 1 && arg.starts_with('-') && arg != "--" {
            for ch in arg[1..].chars() {
                match ch {
                    'x' => export = true,
                    'i' => integer = true,
                    'p' => print = true,
                    'r' => readonly = true,
                    'A' => assoc = true,
                    // -r (readonly, not enforced), -g/-l (scope), -a/-A (arrays,
                    // unsupported), -f/-F (functions): accepted, no-op here.
                    _ => {}
                }
            }
            i += 1;
        } else if arg == "--" {
            i += 1;
            break;
        } else {
            break;
        }
    }
    let names = &args[i..];

    if print {
        let mut out = String::new();
        let print_one = |out: &mut String, key: &str, value: &str, state: &ShellState| {
            let flag = if state.exported_env().contains_key(key) {
                "-x"
            } else {
                "--"
            };
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("declare {flag} {key}=\"{escaped}\"\n"));
        };
        if names.is_empty() {
            for (key, value) in state.vars().clone() {
                print_one(&mut out, &key, &value, state);
            }
        } else {
            for name in names {
                if let Some(value) = state.lookup(name).map(str::to_string) {
                    print_one(&mut out, name, &value, state);
                }
            }
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }

    for name in names {
        if let Some((key, raw)) = name.split_once('=') {
            // `declare -a a=(x y z)` / `declare -A h=([k]=v ...)`: array literal.
            if raw.starts_with('(') && raw.ends_with(')') {
                let inner = &raw[1..raw.len() - 1];
                if assoc {
                    state.set_assoc(key, parse_assoc_pairs(inner), false);
                } else {
                    let elements = inner.split_whitespace().map(String::from).collect();
                    state.set_array(key, elements, false);
                }
                if readonly {
                    state.mark_readonly(key);
                }
                continue;
            }
            let value = if integer {
                crate::executor::eval_arithmetic(raw, state)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|_| "0".to_string())
            } else {
                raw.to_string()
            };
            if export {
                state.export_var(key, value);
            } else {
                state.set_var(key, value);
            }
            if readonly {
                state.mark_readonly(key);
            }
        } else {
            if assoc {
                state.declare_assoc(name);
            }
            if export {
                let value = state.lookup(name).map(str::to_string).unwrap_or_default();
                state.export_var(name.as_str(), value);
            }
            if readonly {
                state.mark_readonly(name);
            }
        }
    }
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

fn builtin_export(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    // `export -p`: list exported variables in re-inputtable form.
    if args.first().map(String::as_str) == Some("-p") {
        let mut out = String::new();
        for (key, value) in state.exported_env().clone() {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            out.push_str(&format!("declare -x {key}=\"{escaped}\"\n"));
        }
        return Ok(CommandOutcome::captured(0, out.into_bytes(), Vec::new()));
    }
    for arg in args {
        if let Some((key, value)) = arg.split_once('=') {
            state.export_var(key, value);
        } else if let Some(value) = state.lookup(arg).map(str::to_string) {
            state.export_var(arg.as_str(), value);
        } else {
            state.export_var(arg.as_str(), "");
        }
    }
    Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
}

fn builtin_unset(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    let mut functions_only = false;
    let mut vars_only = false;
    for arg in args {
        match arg.as_str() {
            "-f" => functions_only = true,
            "-v" => vars_only = true,
            name => {
                if functions_only {
                    state.remove_function(name);
                } else if vars_only {
                    state.unset(name);
                } else {
                    // Without a flag, remove a variable, falling back to a
                    // function of the same name (matching bash precedence).
                    if state.lookup(name).is_some() {
                        state.unset(name);
                    } else {
                        state.remove_function(name);
                    }
                }
            }
        }
    }
    Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
}

fn builtin_set(args: &[String], state: &mut ShellState) -> CommandOutcome {
    if args.is_empty() {
        let mut out = String::new();
        for (name, value) in state.vars() {
            out.push_str(&format!("{name}='{}'\n", shell_single_quote(value)));
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }

    // Process leading option words: `-e`, `+e`, bundled short flags (`-eu`,
    // `-euo pipefail`), and `-o NAME` / `+o NAME` (getopt-style: the name is the
    // rest of the current token if any, else the next word). Stop at `--` or the
    // first non-option word; the remainder (if any) replaces the positionals.
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let on = match arg.as_bytes().first() {
            Some(b'-') => true,  // `-` turns options on
            Some(b'+') => false, // `+` turns options off
            _ => break,          // first non-option word
        };
        if arg == "--" {
            state.set_positionals(&args[i + 1..]);
            return CommandOutcome::captured(0, Vec::new(), Vec::new());
        }
        let letters: Vec<char> = arg[1..].chars().collect();
        let mut k = 0;
        while k < letters.len() {
            match letters[k] {
                'a' => state.set_allexport(on),
                'e' => state.set_errexit(on),
                'f' => state.set_noglob(on),
                'u' => state.set_nounset(on),
                'C' => state.set_noclobber(on),
                'x' => state.set_xtrace(on),
                'o' => {
                    let name = if k + 1 < letters.len() {
                        letters[k + 1..].iter().collect::<String>()
                    } else if let Some(next) = args.get(i + 1) {
                        i += 1; // consume the name word
                        next.clone()
                    } else {
                        // `set -o` / `set +o` alone: print the current options.
                        return print_set_options(on, state);
                    };
                    if !apply_named_option(&name, on, state) {
                        return CommandOutcome::captured(
                            2,
                            Vec::new(),
                            format!("set: unsupported option name: {name}\n").into_bytes(),
                        );
                    }
                    break; // `o` consumed the remainder of this token
                }
                other => {
                    return CommandOutcome::captured(
                        2,
                        Vec::new(),
                        format!(
                            "set: unsupported option: {}{other}\n",
                            if on { '-' } else { '+' }
                        )
                        .into_bytes(),
                    );
                }
            }
            k += 1;
        }
        i += 1;
    }

    // Any remaining words replace the positional parameters (`set -e a b`,
    // `set a b`). With only options and no operands, positionals are untouched.
    if i < args.len() {
        state.set_positionals(&args[i..]);
    }
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

/// Apply a long option name (`set -o pipefail` / `set +o pipefail`). Returns
/// false for an unknown name so the caller can report it.
fn apply_named_option(name: &str, on: bool, state: &mut ShellState) -> bool {
    match name {
        "allexport" => state.set_allexport(on),
        "errexit" => state.set_errexit(on),
        "nounset" => state.set_nounset(on),
        "noclobber" => state.set_noclobber(on),
        "noglob" => state.set_noglob(on),
        "pipefail" => state.set_pipefail(on),
        "xtrace" => state.set_xtrace(on),
        _ => return false,
    }
    true
}

/// Print the current option settings: `set -o` is a `name<TAB>on|off` table;
/// `set +o` is the re-readable `set -o NAME` / `set +o NAME` form.
fn print_set_options(dash: bool, state: &ShellState) -> CommandOutcome {
    let opts = [
        ("allexport", state.allexport()),
        ("errexit", state.errexit()),
        ("nounset", state.nounset()),
        ("noclobber", state.noclobber()),
        ("noglob", state.noglob()),
        ("pipefail", state.pipefail()),
        ("xtrace", state.xtrace()),
    ];
    let mut out = String::new();
    for (name, is_on) in opts {
        if dash {
            out.push_str(&format!("{name}\t{}\n", if is_on { "on" } else { "off" }));
        } else {
            out.push_str(&format!("set {}o {name}\n", if is_on { '-' } else { '+' }));
        }
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn builtin_alias(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    if args.is_empty() {
        let mut out = String::new();
        for (name, value) in state.aliases() {
            out.push_str(&format!("alias {name}='{}'\n", shell_single_quote(value)));
        }
        return Ok(CommandOutcome::captured(0, out.into_bytes(), Vec::new()));
    }

    let mut out = String::new();
    let mut err = String::new();
    let mut exit_code = 0;

    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            if !is_identifier(name) {
                err.push_str(&format!("alias: invalid name: {name}\n"));
                exit_code = 1;
                continue;
            }
            state.set_alias(name, value);
        } else if let Some(value) = state.alias(arg) {
            out.push_str(&format!("alias {arg}='{}'\n", shell_single_quote(value)));
        } else {
            err.push_str(&format!("alias: {arg}: not found\n"));
            exit_code = 1;
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        out.into_bytes(),
        err.into_bytes(),
    ))
}

fn builtin_unalias(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    let mut err = String::new();
    let mut exit_code = 0;

    for arg in args {
        if !state.remove_alias(arg) {
            err.push_str(&format!("unalias: {arg}: not found\n"));
            exit_code = 1;
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        Vec::new(),
        err.into_bytes(),
    ))
}

fn builtin_abbr(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    if args.is_empty() {
        let mut out = String::new();
        for (name, value) in state.abbreviations() {
            out.push_str(&format!("abbr {name}='{}'\n", shell_single_quote(value)));
        }
        return Ok(CommandOutcome::captured(0, out.into_bytes(), Vec::new()));
    }

    let mut out = String::new();
    let mut err = String::new();
    let mut exit_code = 0;
    let mut index = 0;

    while index < args.len() {
        if args[index] == "-e" || args[index] == "--erase" {
            index += 1;
            let Some(name) = args.get(index) else {
                err.push_str("abbr: missing name for erase\n");
                exit_code = 2;
                break;
            };
            if !state.remove_abbreviation(name) {
                err.push_str(&format!("abbr: {name}: not found\n"));
                exit_code = 1;
            }
            index += 1;
            continue;
        }

        let arg = &args[index];
        if let Some((name, value)) = arg.split_once('=') {
            if !is_identifier(name) {
                err.push_str(&format!("abbr: invalid name: {name}\n"));
                exit_code = 1;
            } else {
                state.set_abbreviation(name, value);
            }
        } else if let Some(value) = state.abbreviation(arg) {
            out.push_str(&format!("abbr {arg}='{}'\n", shell_single_quote(value)));
        } else {
            err.push_str(&format!("abbr: {arg}: not found\n"));
            exit_code = 1;
        }
        index += 1;
    }

    Ok(CommandOutcome::captured(
        exit_code,
        out.into_bytes(),
        err.into_bytes(),
    ))
}

fn builtin_unabbr(args: &[String], state: &mut ShellState) -> Result<CommandOutcome, ShellError> {
    let mut err = String::new();
    let mut exit_code = 0;

    for arg in args {
        if !state.remove_abbreviation(arg) {
            err.push_str(&format!("unabbr: {arg}: not found\n"));
            exit_code = 1;
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        Vec::new(),
        err.into_bytes(),
    ))
}

fn builtin_history(args: &[String], state: &mut ShellState) -> CommandOutcome {
    match args {
        [] => history_list(state, state.history_len()),
        [arg] if arg == "-c" => {
            state.clear_history();
            CommandOutcome::captured(0, Vec::new(), Vec::new())
        }
        [arg] if arg.starts_with('-') => CommandOutcome::captured(
            2,
            Vec::new(),
            format!("history: unsupported option: {arg}\n").into_bytes(),
        ),
        [arg] => match arg.parse::<usize>() {
            Ok(limit) => history_list(state, limit),
            Err(_) => CommandOutcome::captured(
                2,
                Vec::new(),
                format!("history: invalid count: {arg}\n").into_bytes(),
            ),
        },
        _ => CommandOutcome::captured(2, Vec::new(), b"history: too many arguments\n".to_vec()),
    }
}

fn history_list(state: &ShellState, limit: usize) -> CommandOutcome {
    let history = state.history_commands();
    let start = history.len().saturating_sub(limit);
    let mut out = String::new();
    for (index, entry) in history.iter().enumerate().skip(start) {
        out.push_str(&format!("{:>5}  {entry}\n", index + 1));
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn builtin_jobs(args: &[String], state: &ShellState) -> CommandOutcome {
    for arg in args {
        if arg.starts_with('-') && !matches!(arg.as_str(), "-l" | "-p" | "-r" | "-s") {
            return CommandOutcome::captured(
                2,
                Vec::new(),
                format!("jobs: unsupported option: {arg}\n").into_bytes(),
            );
        }
    }
    let mut out = state.job_listing().join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn builtin_fg(args: &[String], state: &ShellState) -> CommandOutcome {
    let spec = args.first().map(String::as_str).unwrap_or("%+");
    let Some(pgid) = state.job_pgid(spec) else {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            format!("fg: {spec}: no such job\n").into_bytes(),
        );
    };
    let _ = signal_process_group(pgid, Signal::CONT);
    state.set_job_running(spec);
    let code = state.wait_for_jobs(Some(spec)).unwrap_or(0);
    CommandOutcome::captured(code, Vec::new(), Vec::new())
}

fn builtin_bg(args: &[String], state: &ShellState) -> CommandOutcome {
    let spec = args.first().map(String::as_str).unwrap_or("%+");
    let Some(pgid) = state.job_pgid(spec) else {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            format!("bg: {spec}: no such job\n").into_bytes(),
        );
    };
    let _ = signal_process_group(pgid, Signal::CONT);
    state.set_job_running(spec);
    CommandOutcome::captured(0, Vec::new(), Vec::new())
}

/// `agjob <command…>` — run a command in the BACKGROUND with its output CAPTURED to
/// a retrievable log file, returning immediately with a job id + the log path. Solves
/// the #1 agent-blocking failure (long `cargo build` / `npm test` / dev servers):
/// instead of blocking the tool call or losing `&`'s terminal output, the agent gets
/// a non-blocking handle it can poll with `jobs` and query with `agtrace grep <log>`.
fn builtin_agjob(args: &[String], state: &ShellState) -> CommandOutcome {
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    if args.is_empty() {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"agjob: usage: agjob <command> [args...]\n".to_vec(),
        );
    }
    // Single-quote each arg so the reconstructed command line preserves quoting
    // exactly (a bare join would mangle `agjob sh -c "a; b"`).
    let source = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let dir = std::env::var_os("AGSH_TRACE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("agsh-jobs"));
    if std::fs::create_dir_all(&dir).is_err() {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            b"agjob: could not create the job log directory\n".to_vec(),
        );
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let log = dir.join(format!("job-{}-{seq}.log", std::process::id()));
    let file = match std::fs::File::create(&log) {
        Ok(file) => file,
        Err(error) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("agjob: {error}\n").into_bytes(),
            )
        }
    };
    // stdout + stderr merge into one raw log (a child `agsh -c` runs raw by default,
    // so the file holds exact bytes). Detached process group, no terminal stdin.
    let err_handle = match file.try_clone() {
        Ok(handle) => handle,
        Err(error) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("agjob: {error}\n").into_bytes(),
            )
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("agjob: {error}\n").into_bytes(),
            )
        }
    };
    let mut command = Command::new(exe);
    command.arg("-c").arg(&source);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    command.process_group(0);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::from(file));
    command.stderr(std::process::Stdio::from(err_handle));
    match command.spawn() {
        Ok(child) => {
            let pid = child.id();
            state.set_last_bg_pid(pid);
            let (id, _pgid) = state.register_job(child, source);
            CommandOutcome::captured(
                0,
                format!("[{id}] {pid}  output: {}\n", log.display()).into_bytes(),
                Vec::new(),
            )
        }
        Err(error) => {
            CommandOutcome::captured(1, Vec::new(), format!("agjob: {error}\n").into_bytes())
        }
    }
}

fn builtin_wait(args: &[String], state: &ShellState) -> CommandOutcome {
    if args.is_empty() {
        state.wait_for_jobs(None);
        return CommandOutcome::captured(0, Vec::new(), Vec::new());
    }
    let mut exit_code = 0;
    let mut err = String::new();
    for arg in args {
        match state.wait_for_jobs(Some(arg)) {
            Some(code) => exit_code = code,
            None => {
                err.push_str(&format!("wait: {arg}: no such job\n"));
                exit_code = 127;
            }
        }
    }
    CommandOutcome::captured(exit_code, Vec::new(), err.into_bytes())
}

fn builtin_kill(args: &[String], state: &ShellState) -> Result<CommandOutcome, ShellError> {
    if args.is_empty() {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | %job ...\n".to_vec(),
        ));
    }
    // `kill -l` lists signal names via the platform kill.
    if args.iter().any(|arg| arg == "-l") {
        let Some(path) = resolve_default_path_command("kill") else {
            return Err(ShellError::not_found("kill: platform kill not found"));
        };
        let output = Command::new(path).args(args).output()?;
        return Ok(CommandOutcome::captured(
            output.status.code().unwrap_or(128),
            output.stdout,
            output.stderr,
        ));
    }

    let mut signal = Signal::TERM;
    let mut index = 0;
    // Optional leading signal: `-9`, `-TERM`, `-SIGTERM`, or `-s NAME`.
    if let Some(first) = args.first() {
        if first == "-s" || first == "-n" {
            if let Some(spec) = args.get(1) {
                match parse_signal(spec) {
                    Some(sig) => signal = sig,
                    None => {
                        return Ok(CommandOutcome::captured(
                            1,
                            Vec::new(),
                            format!("kill: {spec}: invalid signal specification\n").into_bytes(),
                        ))
                    }
                }
                index = 2;
            }
        } else if first.starts_with('-') && first.len() > 1 {
            match parse_signal(first) {
                Some(sig) => signal = sig,
                None => {
                    return Ok(CommandOutcome::captured(
                        1,
                        Vec::new(),
                        format!("kill: {first}: invalid signal specification\n").into_bytes(),
                    ))
                }
            }
            index = 1;
        }
    }

    let mut err = String::new();
    let mut exit_code = 0;
    for target in &args[index..] {
        let result = if let Some(spec) = target.strip_prefix('%') {
            match state.job_pgid(&format!("%{spec}")) {
                Some(pgid) => signal_process_group(pgid, signal),
                None => {
                    err.push_str(&format!("kill: {target}: no such job\n"));
                    exit_code = 1;
                    continue;
                }
            }
        } else {
            match target.parse::<i32>() {
                Ok(pid) => signal_process(pid, signal),
                Err(_) => {
                    err.push_str(&format!(
                        "kill: {target}: arguments must be process or job IDs\n"
                    ));
                    exit_code = 1;
                    continue;
                }
            }
        };
        if result.is_err() {
            err.push_str(&format!("kill: ({target}) - no such process\n"));
            exit_code = 1;
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        Vec::new(),
        err.into_bytes(),
    ))
}

fn signal_process_group(pgid: i32, signal: Signal) -> Result<(), ()> {
    let pid = rustix::process::Pid::from_raw(pgid).ok_or(())?;
    rustix::process::kill_process_group(pid, signal).map_err(|_| ())
}

fn signal_process(pid: i32, signal: Signal) -> Result<(), ()> {
    let pid = rustix::process::Pid::from_raw(pid).ok_or(())?;
    rustix::process::kill_process(pid, signal).map_err(|_| ())
}

fn parse_signal(spec: &str) -> Option<Signal> {
    let name = spec
        .trim_start_matches('-')
        .trim_start_matches("SIG")
        .trim_start_matches("sig")
        .to_ascii_uppercase();
    let by_name = match name.as_str() {
        "HUP" => Some(Signal::HUP),
        "INT" => Some(Signal::INT),
        "QUIT" => Some(Signal::QUIT),
        "KILL" => Some(Signal::KILL),
        "TERM" => Some(Signal::TERM),
        "STOP" => Some(Signal::STOP),
        "TSTP" => Some(Signal::TSTP),
        "CONT" => Some(Signal::CONT),
        "USR1" => Some(Signal::USR1),
        "USR2" => Some(Signal::USR2),
        "ALRM" => Some(Signal::ALARM),
        _ => None,
    };
    by_name.or_else(|| {
        spec.trim_start_matches('-')
            .parse::<i32>()
            .ok()
            .and_then(Signal::from_named_raw)
    })
}

fn builtin_ulimit(args: &[String]) -> Result<CommandOutcome, ShellError> {
    if args.iter().any(|arg| !arg.starts_with('-')) {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"ulimit: setting limits is unsupported without a safe resource-limit backend\n"
                .to_vec(),
        ));
    }
    run_shell_query_builtin("ulimit", args)
}

fn builtin_umask(args: &[String]) -> Result<CommandOutcome, ShellError> {
    match args {
        [] => run_shell_query_builtin("umask", args),
        [arg] if arg == "-S" => run_shell_query_builtin("umask", args),
        [arg] if arg.starts_with('-') => Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            format!("umask: unsupported option: {arg}\n").into_bytes(),
        )),
        _ => Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"umask: setting the process umask is unsupported without a safe umask backend\n"
                .to_vec(),
        )),
    }
}

fn run_shell_query_builtin(name: &str, args: &[String]) -> Result<CommandOutcome, ShellError> {
    let Some(path) = resolve_default_path_command("sh") else {
        return Err(ShellError::not_found(
            "platform sh command not found in default path",
        ));
    };
    let output = Command::new(path)
        .arg("-c")
        .arg(format!("{name} \"$@\""))
        .arg(format!("agsh-{name}"))
        .args(args)
        .output()?;
    Ok(CommandOutcome::captured(
        output.status.code().unwrap_or(128),
        output.stdout,
        output.stderr,
    ))
}

fn resolve_default_path_command(name: &str) -> Option<PathBuf> {
    match Resolver::default().resolve_external_only(name, Some(DEFAULT_COMMAND_PATH)) {
        Some(CommandResolution::External(path)) => Some(path),
        _ => None,
    }
}

fn builtin_echo(args: &[String]) -> CommandOutcome {
    let mut trailing_newline = true;
    let mut interpret_escapes = false;
    let mut start = 0;

    // Parse combined flags from {n, e, E}. Stop at the first non-flag argument.
    while let Some(arg) = args.get(start) {
        if arg.len() < 2 || !arg.starts_with('-') {
            break;
        }
        if !arg[1..].chars().all(|ch| matches!(ch, 'n' | 'e' | 'E')) {
            break;
        }
        for ch in arg[1..].chars() {
            match ch {
                'n' => trailing_newline = false,
                'e' => interpret_escapes = true,
                'E' => interpret_escapes = false,
                _ => unreachable!(),
            }
        }
        start += 1;
    }

    let joined = args[start..].join(" ");
    let mut out = if interpret_escapes {
        let (decoded, stop) = decode_echo_escapes(&joined);
        if stop {
            trailing_newline = false;
        }
        decoded
    } else {
        joined.into_bytes()
    };
    if trailing_newline {
        out.push(b'\n');
    }
    CommandOutcome::captured(0, out, Vec::new())
}

/// Decode `echo -e`-style escapes. Returns the bytes and whether `\c` requested
/// truncation of further output (including the trailing newline).
fn decode_echo_escapes(input: &str) -> (Vec<u8>, bool) {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\\' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        match chars[i] {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\u{07}'),
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0C}'),
            'v' => out.push('\u{0B}'),
            '\\' => out.push('\\'),
            'c' => return (out.into_bytes(), true),
            '0' => {
                i += 1;
                let (value, consumed) = parse_octal(&chars[i..], 3);
                i += consumed;
                push_byte_char(&mut out, value);
                continue;
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
        i += 1;
    }
    (out.into_bytes(), false)
}

fn parse_octal(chars: &[char], max: usize) -> (u32, usize) {
    let mut value = 0u32;
    let mut consumed = 0;
    while consumed < max {
        match chars.get(consumed) {
            Some(ch) if ('0'..='7').contains(ch) => {
                value = value * 8 + ch.to_digit(8).unwrap_or(0);
                consumed += 1;
            }
            _ => break,
        }
    }
    (value, consumed)
}

fn push_byte_char(out: &mut String, value: u32) {
    out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
}

fn builtin_printf(args: &[String]) -> Result<CommandOutcome, ShellError> {
    if args.is_empty() {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"printf: usage: printf format [arguments]\n".to_vec(),
        ));
    }

    let format = &args[0];
    let operands = &args[1..];
    let mut out = String::new();
    let mut exit_code = 0;

    if operands.is_empty() {
        render_printf_format(format, operands, &mut out, &mut exit_code);
    } else {
        let mut index = 0;
        while index < operands.len() {
            let consumed =
                render_printf_format(format, &operands[index..], &mut out, &mut exit_code);
            if consumed == 0 {
                break;
            }
            index += consumed;
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        out.into_bytes(),
        Vec::new(),
    ))
}

/// Render one pass of the printf format string, consuming operands as needed.
/// Returns how many operands this pass consumed.
fn render_printf_format(
    format: &str,
    operands: &[String],
    out: &mut String,
    exit_code: &mut i32,
) -> usize {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0;
    let mut arg_index = 0;

    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                i += decode_printf_backslash(&chars, i, out);
            }
            '%' if chars.get(i + 1) == Some(&'%') => {
                out.push('%');
                i += 2;
            }
            '%' => {
                let (spec_len, consumed) =
                    render_printf_conversion(&chars[i..], operands, &mut arg_index, out, exit_code);
                if spec_len == 0 {
                    out.push('%');
                    i += 1;
                } else {
                    i += spec_len;
                    let _ = consumed;
                }
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }

    arg_index
}

/// Decode a backslash escape inside a printf format. `i` points past the `\`.
/// Returns how many characters after the backslash were consumed.
fn decode_printf_backslash(chars: &[char], i: usize, out: &mut String) -> usize {
    let Some(&ch) = chars.get(i) else {
        out.push('\\');
        return 0;
    };
    match ch {
        'n' => out.push('\n'),
        't' => out.push('\t'),
        'r' => out.push('\r'),
        'a' => out.push('\u{07}'),
        'b' => out.push('\u{08}'),
        'f' => out.push('\u{0C}'),
        'v' => out.push('\u{0B}'),
        '\\' => out.push('\\'),
        '0' => {
            let (value, consumed) = parse_octal(&chars[i + 1..], 3);
            push_byte_char(out, value);
            return 1 + consumed;
        }
        other => {
            out.push('\\');
            out.push(other);
        }
    }
    1
}

/// Render a `%` conversion at the start of `chars`. Returns (spec_length, consumed_arg).
fn render_printf_conversion(
    chars: &[char],
    operands: &[String],
    arg_index: &mut usize,
    out: &mut String,
    exit_code: &mut i32,
) -> (usize, bool) {
    // chars[0] == '%'
    let mut j = 1;
    let flags_start = j;
    while chars
        .get(j)
        .is_some_and(|c| matches!(c, '-' | '+' | ' ' | '#' | '0'))
    {
        j += 1;
    }
    let flags: String = chars[flags_start..j].iter().collect();

    let mut width = String::new();
    while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
        width.push(chars[j]);
        j += 1;
    }

    let mut precision: Option<String> = None;
    if chars.get(j) == Some(&'.') {
        j += 1;
        let mut p = String::new();
        while chars.get(j).is_some_and(|c| c.is_ascii_digit()) {
            p.push(chars[j]);
            j += 1;
        }
        precision = Some(p);
    }

    let Some(&conv) = chars.get(j) else {
        return (0, false);
    };
    let spec_len = j + 1;

    let next_operand = |arg_index: &mut usize| -> String {
        let value = operands.get(*arg_index).cloned().unwrap_or_default();
        *arg_index += 1;
        value
    };

    let width_num = width.parse::<usize>().ok();
    let left = flags.contains('-');
    let zero = flags.contains('0') && !left;

    let rendered = match conv {
        's' => {
            let mut value = next_operand(arg_index);
            if let Some(p) = precision.as_ref().and_then(|p| p.parse::<usize>().ok()) {
                value = value.chars().take(p).collect();
            }
            pad(&value, width_num, left, false)
        }
        'b' => {
            let raw = next_operand(arg_index);
            let (bytes, _) = decode_echo_escapes(&raw);
            pad(&String::from_utf8_lossy(&bytes), width_num, left, false)
        }
        'c' => {
            let value = next_operand(arg_index);
            let ch: String = value.chars().take(1).collect();
            pad(&ch, width_num, left, false)
        }
        'd' | 'i' => {
            let raw = next_operand(arg_index);
            let value = parse_printf_int(&raw, exit_code);
            let mut body = value.unsigned_abs().to_string();
            if let Some(p) = precision.as_ref().and_then(|p| p.parse::<usize>().ok()) {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            let sign = if value < 0 {
                "-"
            } else if flags.contains('+') {
                "+"
            } else if flags.contains(' ') {
                " "
            } else {
                ""
            };
            pad_number(sign, &body, width_num, left, zero && precision.is_none())
        }
        'u' | 'o' | 'x' | 'X' => {
            let raw = next_operand(arg_index);
            let value = parse_printf_int(&raw, exit_code);
            let unsigned = value as u64;
            let mut body = match conv {
                'o' => format!("{unsigned:o}"),
                'x' => format!("{unsigned:x}"),
                'X' => format!("{unsigned:X}"),
                _ => unsigned.to_string(),
            };
            if let Some(p) = precision.as_ref().and_then(|p| p.parse::<usize>().ok()) {
                while body.len() < p {
                    body.insert(0, '0');
                }
            }
            pad_number("", &body, width_num, left, zero && precision.is_none())
        }
        'f' | 'e' | 'g' | 'E' | 'G' => {
            let raw = next_operand(arg_index);
            let value = raw.parse::<f64>().unwrap_or_else(|_| {
                if !raw.is_empty() {
                    *exit_code = 1;
                }
                0.0
            });
            let prec = precision
                .as_ref()
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(6);
            let body = match conv {
                'e' | 'E' => format!("{value:.prec$e}"),
                _ => format!("{value:.prec$}"),
            };
            pad_number("", &body, width_num, left, zero)
        }
        _ => {
            return (0, false);
        }
    };

    out.push_str(&rendered);
    (spec_len, true)
}

fn parse_printf_int(raw: &str, exit_code: &mut i32) -> i64 {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        return rest.chars().next().map_or(0, |c| c as i64);
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return i64::from_str_radix(hex, 16).unwrap_or(0);
    }
    match trimmed.parse::<i64>() {
        Ok(value) => value,
        Err(_) => {
            *exit_code = 1;
            0
        }
    }
}

fn pad(value: &str, width: Option<usize>, left: bool, _zero: bool) -> String {
    let Some(width) = width else {
        return value.to_string();
    };
    let len = value.chars().count();
    if len >= width {
        return value.to_string();
    }
    let padding = " ".repeat(width - len);
    if left {
        format!("{value}{padding}")
    } else {
        format!("{padding}{value}")
    }
}

fn pad_number(sign: &str, body: &str, width: Option<usize>, left: bool, zero: bool) -> String {
    let combined = format!("{sign}{body}");
    let Some(width) = width else {
        return combined;
    };
    let len = combined.chars().count();
    if len >= width {
        return combined;
    }
    let fill = width - len;
    if left {
        format!("{combined}{}", " ".repeat(fill))
    } else if zero {
        format!("{sign}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{combined}", " ".repeat(fill))
    }
}

fn builtin_type(args: &[String], state: &ShellState) -> Result<CommandOutcome, ShellError> {
    let mut out = String::new();
    let mut err = String::new();
    let mut exit_code = 0;
    let resolver = Resolver::default();
    for arg in args {
        if state.function(arg).is_some() {
            out.push_str(&format!("{arg} is a function\n"));
            continue;
        }

        if let Some(value) = state.alias(arg) {
            out.push_str(&format!("{arg} is aliased to {value}\n"));
            continue;
        }

        if let Some(value) = state.abbreviation(arg) {
            out.push_str(&format!("{arg} is abbreviated to {value}\n"));
            continue;
        }

        // Builtins take precedence at execution, so report them as such — using
        // the authoritative `is_builtin` set rather than the resolver's narrower
        // list (which omitted getopts/trap/local/…, so `type getopts` wrongly
        // reported the external /usr/bin/getopts).
        if is_builtin(arg) {
            out.push_str(&format!("{arg} is an agsh builtin\n"));
            continue;
        }

        match resolver.resolve(arg, state.lookup("PATH")) {
            CommandResolution::Builtin(name) => {
                out.push_str(&format!("{name} is an agsh builtin\n"))
            }
            CommandResolution::External(path) => {
                out.push_str(&format!("{arg} is {}\n", path.display()))
            }
            CommandResolution::Function(name) => out.push_str(&format!("{name} is a function\n")),
            CommandResolution::Alias(value) => {
                out.push_str(&format!("{arg} is aliased to {value}\n"))
            }
            CommandResolution::Abbreviation(value) => {
                out.push_str(&format!("{arg} is abbreviated to {value}\n"))
            }
            CommandResolution::Plugin(name) => out.push_str(&format!("{name} is an agsh plugin\n")),
            CommandResolution::NotFound(name) => {
                err.push_str(&format!("agsh: type: {name}: not found\n"));
                exit_code = 1;
            }
        }
    }
    Ok(CommandOutcome::captured(
        exit_code,
        out.into_bytes(),
        err.into_bytes(),
    ))
}

fn builtin_which(args: &[String], state: &ShellState) -> Result<CommandOutcome, ShellError> {
    let resolver = Resolver::default();
    let mut out = String::new();
    let mut exit_code = 0;

    for arg in args {
        match resolver.resolve_external_only(arg, state.lookup("PATH")) {
            Some(CommandResolution::External(path)) => {
                out.push_str(&format!("{}\n", path.display()))
            }
            _ => exit_code = 1,
        }
    }

    Ok(CommandOutcome::captured(
        exit_code,
        out.into_bytes(),
        Vec::new(),
    ))
}

fn builtin_command(args: &[String], state: &ShellState) -> Result<CommandOutcome, ShellError> {
    let options = parse_command_builtin_options(args);
    if let Some(option) = options.unsupported_option {
        return Err(ShellError::execution(format!(
            "command: unsupported option: {option}"
        )));
    }

    if options.describe {
        Ok(command_describe(
            &args[options.command_index..],
            state,
            options.verbose,
            options.default_path,
        ))
    } else if args.get(options.command_index).is_none() {
        Err(ShellError::execution("command: missing command"))
    } else {
        Err(ShellError::execution(
            "command: execution wrapper is handled by executor",
        ))
    }
}

#[derive(Debug, Clone)]
struct CommandBuiltinOptions {
    default_path: bool,
    describe: bool,
    verbose: bool,
    unsupported_option: Option<String>,
    command_index: usize,
}

fn parse_command_builtin_options(args: &[String]) -> CommandBuiltinOptions {
    let mut options = CommandBuiltinOptions {
        default_path: false,
        describe: false,
        verbose: false,
        unsupported_option: None,
        command_index: 0,
    };

    while let Some(arg) = args.get(options.command_index) {
        match arg.as_str() {
            "--" => {
                options.command_index += 1;
                break;
            }
            "-p" => options.default_path = true,
            "-v" => options.describe = true,
            "-V" => {
                options.describe = true;
                options.verbose = true;
            }
            other if other.starts_with('-') => {
                options.unsupported_option = Some(other.to_string());
                break;
            }
            _ => break,
        }
        options.command_index += 1;
    }

    options
}

fn command_describe(
    args: &[String],
    state: &ShellState,
    verbose: bool,
    default_path: bool,
) -> CommandOutcome {
    let mut out = String::new();
    let mut exit_code = 0;

    for arg in args {
        if let Some(line) = command_description(arg, state, verbose, default_path) {
            out.push_str(&line);
            out.push('\n');
        } else {
            exit_code = 1;
        }
    }

    CommandOutcome::captured(exit_code, out.into_bytes(), Vec::new())
}

fn command_description(
    name: &str,
    state: &ShellState,
    verbose: bool,
    default_path: bool,
) -> Option<String> {
    let resolver = Resolver::default();
    if state.function(name).is_some() {
        return Some(if verbose {
            format!("{name} is a function")
        } else {
            name.to_string()
        });
    }

    if let Some(value) = state.alias(name) {
        return Some(if verbose {
            format!("{name} is aliased to {}", shell_single_quote(value))
        } else {
            format!("alias {name}='{}'", shell_single_quote(value))
        });
    }

    if let Some(value) = state.abbreviation(name) {
        return Some(if verbose {
            format!("{name} is abbreviated to {}", shell_single_quote(value))
        } else {
            format!("abbr {name}='{}'", shell_single_quote(value))
        });
    }

    // Builtins take execution precedence — report them from the authoritative
    // `is_builtin` set (the resolver's list omitted many, so `command -v getopts`
    // returned the external path).
    if is_builtin(name) {
        return Some(if verbose {
            format!("{name} is an agsh builtin")
        } else {
            name.to_string()
        });
    }

    let path = if default_path {
        Some(DEFAULT_COMMAND_PATH)
    } else {
        state.lookup("PATH")
    };

    match resolver.resolve(name, path) {
        CommandResolution::Builtin(resolved) => Some(if verbose {
            format!("{resolved} is an agsh builtin")
        } else {
            resolved
        }),
        CommandResolution::External(path) => Some(if verbose {
            format!("{name} is {}", path.display())
        } else {
            path.display().to_string()
        }),
        CommandResolution::Function(resolved) => Some(if verbose {
            format!("{resolved} is a function")
        } else {
            resolved
        }),
        CommandResolution::Alias(value) => Some(if verbose {
            format!("{name} is aliased to {value}")
        } else {
            value
        }),
        CommandResolution::Abbreviation(value) => Some(if verbose {
            format!("{name} is abbreviated to {value}")
        } else {
            value
        }),
        CommandResolution::Plugin(resolved) => Some(if verbose {
            format!("{resolved} is an agsh plugin")
        } else {
            resolved
        }),
        CommandResolution::NotFound(_) => None,
    }
}

fn builtin_bracket_test(args: &[String], cwd: &Path) -> CommandOutcome {
    let Some(last) = args.last() else {
        return test_syntax_error("[: missing ]");
    };
    if last != "]" {
        return test_syntax_error("[: missing ]");
    }
    builtin_test(&args[..args.len().saturating_sub(1)], "[", cwd)
}

pub(crate) fn builtin_test(args: &[String], name: &str, cwd: &Path) -> CommandOutcome {
    match eval_test_expr(args, cwd) {
        Ok(true) => CommandOutcome::captured(0, Vec::new(), Vec::new()),
        Ok(false) => CommandOutcome::captured(1, Vec::new(), Vec::new()),
        Err(message) => test_syntax_error(format!("{name}: {message}")),
    }
}

fn test_syntax_error(message: impl Into<String>) -> CommandOutcome {
    CommandOutcome::captured(2, Vec::new(), format!("{}\n", message.into()).into_bytes())
}

fn eval_test_expr(args: &[String], cwd: &Path) -> Result<bool, String> {
    // `test` / `[ ]` with no expression is false (not an error).
    if args.is_empty() {
        return Ok(false);
    }
    let mut parser = TestParser {
        tokens: args,
        pos: 0,
        cwd,
    };
    let value = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err("unexpected operator".to_string());
    }
    Ok(value)
}

/// Recursive-descent evaluator for `test`/`[` expressions with `-a`/`-o`
/// precedence, `!` negation, and `( )` grouping.
struct TestParser<'a> {
    tokens: &'a [String],
    pos: usize,
    cwd: &'a Path,
}

impl TestParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.pos).map(String::as_str)
    }

    fn parse_or(&mut self) -> Result<bool, String> {
        let mut value = self.parse_and()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            value = self.parse_and()? || value;
        }
        Ok(value)
    }

    fn parse_and(&mut self) -> Result<bool, String> {
        let mut value = self.parse_unary()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            value = self.parse_unary()? && value;
        }
        Ok(value)
    }

    fn parse_unary(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") && self.has_operand_after_negation() {
            self.pos += 1;
            return Ok(!self.parse_unary()?);
        }
        self.parse_primary()
    }

    fn has_operand_after_negation(&self) -> bool {
        match self.tokens.get(self.pos + 1).map(String::as_str) {
            Some(")") | Some("-a") | Some("-o") | None => false,
            Some(_) => true,
        }
    }

    fn parse_primary(&mut self) -> Result<bool, String> {
        if self.peek() == Some("(") {
            self.pos += 1;
            let value = self.parse_or()?;
            if self.peek() != Some(")") {
                return Err("missing ) in expression".to_string());
            }
            self.pos += 1;
            return Ok(value);
        }

        let count = self.primary_token_count();
        match count {
            0 => Err("unary operator expected".to_string()),
            1 => {
                let value = !self.tokens[self.pos].is_empty();
                self.pos += 1;
                Ok(value)
            }
            2 => {
                let op = self.tokens[self.pos].clone();
                let operand = self.tokens[self.pos + 1].clone();
                if is_unary_test_operator(&op) {
                    self.pos += 2;
                    eval_unary_test(&op, &operand, self.cwd)
                } else {
                    Err(format!("unary operator expected: {op}"))
                }
            }
            3 => {
                let left = self.tokens[self.pos].clone();
                let op = self.tokens[self.pos + 1].clone();
                let right = self.tokens[self.pos + 2].clone();
                if is_binary_test_operator(&op) {
                    self.pos += 3;
                    eval_binary_test(&left, &op, &right)
                } else {
                    Err(format!("binary operator expected: {op}"))
                }
            }
            _ => Err("too many arguments".to_string()),
        }
    }

    /// Count tokens forming the current primary, bounded by `-a`/`-o`/`)`.
    fn primary_token_count(&self) -> usize {
        let mut count = 0;
        while let Some(token) = self.tokens.get(self.pos + count) {
            if matches!(token.as_str(), "-a" | "-o" | ")") {
                break;
            }
            count += 1;
        }
        count
    }
}

fn is_unary_test_operator(op: &str) -> bool {
    matches!(
        op,
        "-n" | "-z" | "-e" | "-f" | "-d" | "-s" | "-r" | "-w" | "-x" | "-L" | "-h"
    )
}

fn eval_unary_test(op: &str, value: &str, cwd: &Path) -> Result<bool, String> {
    let path = resolve_test_path(value, cwd);
    match op {
        "-n" => Ok(!value.is_empty()),
        "-z" => Ok(value.is_empty()),
        "-e" => Ok(path.exists()),
        "-f" => Ok(path.is_file()),
        "-d" => Ok(path.is_dir()),
        "-s" => Ok(std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0)),
        "-r" => Ok(std::fs::File::open(&path).is_ok()),
        "-w" => Ok(std::fs::OpenOptions::new().write(true).open(&path).is_ok()),
        "-x" => Ok(is_executable(&path)),
        "-L" | "-h" => Ok(std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())),
        _ => Err(format!("unsupported unary operator: {op}")),
    }
}

fn is_binary_test_operator(op: &str) -> bool {
    matches!(
        op,
        "=" | "==" | "!=" | "-eq" | "-ne" | "-gt" | "-ge" | "-lt" | "-le"
    )
}

fn eval_binary_test(left: &str, op: &str, right: &str) -> Result<bool, String> {
    match op {
        "=" | "==" => Ok(left == right),
        "!=" => Ok(left != right),
        "-eq" => Ok(parse_test_int(left)? == parse_test_int(right)?),
        "-ne" => Ok(parse_test_int(left)? != parse_test_int(right)?),
        "-gt" => Ok(parse_test_int(left)? > parse_test_int(right)?),
        "-ge" => Ok(parse_test_int(left)? >= parse_test_int(right)?),
        "-lt" => Ok(parse_test_int(left)? < parse_test_int(right)?),
        "-le" => Ok(parse_test_int(left)? <= parse_test_int(right)?),
        _ => Err(format!("unsupported binary operator: {op}")),
    }
}

fn parse_test_int(value: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|_| format!("integer expression expected: {value}"))
}

fn resolve_test_path(value: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}
