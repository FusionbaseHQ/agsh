//! Agent-helper builtins (Milestone 4, shell-level): token-efficient, clearer
//! introspection and editing for an AI agent driving the shell.
//!
//! - `context`  — one-shot structured shell state (cwd/git/last/jobs/env/recent).
//! - `trace`    — stream bounded captured-output head/tail/range/grep slices.
//! - `peek`     — read file slices with line numbers (file.read_range).
//! - `patch`    — apply a unified diff from stdin to a file (file.patch).
//!
//! No socket server or MCP: a terminal app drives the shell directly; these are
//! ordinary commands the agent runs.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{CommandOutcome, ShellState};

/// Largest number of lines any of these builtins will emit (runaway guard).
const MAX_LINES: usize = 5000;
const MAX_AGENT_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATCH_DIFF_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const GIT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_CAPTURE_POST_EXIT_BYTES: usize = 1024 * 1024;
const GIT_CAPTURE_POST_EXIT_TIME: Duration = Duration::from_millis(100);

fn read_bounded_regular_file(
    path: &Path,
    limit: usize,
) -> std::io::Result<(Vec<u8>, std::fs::Permissions)> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {limit} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {limit} bytes"),
        ));
    }
    Ok((bytes, metadata.permissions()))
}

/// Line-selection options shared by `trace` and `peek`.
#[derive(Debug, Default, Clone)]
pub struct SliceOpts {
    pub head: Option<usize>,
    pub tail: Option<usize>,
    pub range: Option<(usize, usize)>,
    pub grep: Option<String>,
    pub number: bool,
}

/// Parse `--head N`, `--tail N`, `--range A:B`, `--grep STR`, `--lines`/`-n`
/// flags out of `args`, returning the options, the positional arguments, and the
/// set of plain (bool) flags consumed elsewhere (e.g. `--stderr`).
pub fn parse_slice_flags(args: &[String]) -> Result<(SliceOpts, Vec<String>, Vec<String>), String> {
    let mut opts = SliceOpts::default();
    let mut positional = Vec::new();
    let mut bool_flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--head" => {
                opts.head = Some(take_num(args, &mut i, "--head")?);
            }
            "--tail" => {
                opts.tail = Some(take_num(args, &mut i, "--tail")?);
            }
            "--grep" => {
                i += 1;
                opts.grep = Some(args.get(i).ok_or("--grep needs a pattern")?.clone());
            }
            "--range" => {
                i += 1;
                let spec = args.get(i).ok_or("--range needs A:B")?;
                opts.range = Some(parse_range(spec)?);
            }
            "--lines" | "-n" => opts.number = true,
            other if other.starts_with("--") => bool_flags.push(other.to_string()),
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    Ok((opts, positional, bool_flags))
}

fn take_num(args: &[String], i: &mut usize, flag: &str) -> Result<usize, String> {
    *i += 1;
    args.get(*i)
        .ok_or_else(|| format!("{flag} needs a number"))?
        .parse()
        .map_err(|_| format!("{flag}: invalid number"))
}

fn parse_range(spec: &str) -> Result<(usize, usize), String> {
    let (a, b) = spec.split_once(':').ok_or("--range must be A:B")?;
    let a: usize = a.parse().map_err(|_| "--range: invalid start")?;
    let b: usize = b.parse().map_err(|_| "--range: invalid end")?;
    if a == 0 || b < a {
        return Err("--range: need 1 <= A <= B".to_string());
    }
    Ok((a, b))
}

/// Apply the slice options to `text`. Lines keep their original 1-based numbers:
/// `--grep` keeps matching lines, `--range` selects by original line number, and
/// `--head`/`--tail` take from the (possibly grep-filtered) sequence.
pub fn apply_slice(text: &str, opts: &SliceOpts) -> String {
    let mut lines: Vec<(usize, &str)> = text.lines().enumerate().map(|(i, l)| (i + 1, l)).collect();

    if let Some(pat) = &opts.grep {
        lines.retain(|(_, l)| l.contains(pat.as_str()));
    }
    if let Some((a, b)) = opts.range {
        lines.retain(|(n, _)| *n >= a && *n <= b);
    } else if let Some(n) = opts.head {
        lines.truncate(n);
    } else if let Some(n) = opts.tail {
        let drop = lines.len().saturating_sub(n);
        lines.drain(0..drop);
    }

    let truncated = lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);

    let mut out = String::new();
    for (n, line) in &lines {
        if opts.number {
            out.push_str(&format!("{n:>6}  "));
        }
        out.push_str(line);
        out.push('\n');
    }
    if truncated {
        out.push_str(&format!("… (truncated to {MAX_LINES} lines)\n"));
    }
    out
}

/// `context [--json]`: a one-shot snapshot of shell state so an agent doesn't
/// have to run pwd + git status + jobs + env separately.
pub fn context(args: &[String], state: &ShellState) -> CommandOutcome {
    let json = args.iter().any(|a| a == "--json");
    // `context` reports shell state and takes no subject — reject stray args
    // rather than silently ignoring them, and point at the right tool.
    let unexpected: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--json")
        .collect();
    if !unexpected.is_empty() {
        let msg = format!(
            "context: unexpected argument: {}\n\
             usage: context [--json]   (shell state; takes no subject)\n\
             for a file use `peek <file>`, a command `risk`/`type <cmd>`, output `trace <id>`\n",
            unexpected.join(" ")
        );
        return CommandOutcome::captured(2, Vec::new(), msg.into_bytes());
    }
    let cwd = short_cwd(state);
    let git = state.git_context();
    let exit = state.last_status();
    let duration_ms = state.last_duration_ms().unwrap_or(0);
    let jobs = state.job_listing();
    let recent = state.recent_commands(10);
    let venv = state.lookup("VIRTUAL_ENV").and_then(base_name);
    let aws = state
        .lookup("AWS_PROFILE")
        .filter(|p| !p.is_empty())
        .map(str::to_string);

    if json {
        let git_json = git.as_ref().map(|g| {
            serde_json::json!({
                "branch": g.branch,
                "dirty": g.dirty,
                "ahead": g.ahead,
                "behind": g.behind,
            })
        });
        let value = serde_json::json!({
            "cwd": cwd,
            "git": git_json,
            "last": { "exit": exit, "duration_ms": duration_ms },
            "jobs": jobs,
            "env": { "virtualenv": venv, "aws_profile": aws },
            "recent": recent.iter().map(|(c, e, d)| serde_json::json!({
                "command": c, "exit": e, "duration_ms": d,
            })).collect::<Vec<_>>(),
        });
        let body = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into());
        return CommandOutcome::captured(0, format!("{body}\n").into_bytes(), Vec::new());
    }

    let mut out = String::new();
    out.push_str(&format!("cwd: {cwd}\n"));
    if let Some(g) = &git {
        let dirty = if g.dirty.unwrap_or(false) {
            " *dirty"
        } else {
            ""
        };
        let branch = g.branch.as_deref().unwrap_or("(detached)");
        out.push_str(&format!(
            "git: {branch}{dirty} ahead {} behind {}\n",
            g.ahead, g.behind
        ));
    }
    out.push_str(&format!("last: exit {exit} ({duration_ms}ms)\n"));
    if !jobs.is_empty() {
        out.push_str(&format!("jobs: {}\n", jobs.len()));
    }
    if let Some(v) = &venv {
        out.push_str(&format!("venv: {v}\n"));
    }
    if let Some(a) = &aws {
        out.push_str(&format!("aws: {a}\n"));
    }
    if !recent.is_empty() {
        out.push_str("recent:\n");
        for (cmd, ex, _) in &recent {
            let mark = match ex {
                Some(0) | None => " ",
                Some(_) => "✗",
            };
            out.push_str(&format!("  {mark} {cmd}\n"));
        }
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

/// `peek <file> [--range A:B|--head N|--tail N] [--grep STR] [--lines]`: read a
/// file (or a slice) with line numbers, so an agent can fetch just the part it
/// needs and reference lines precisely.
pub fn peek(args: &[String], state: &ShellState) -> CommandOutcome {
    let (mut opts, positional, _flags) = match parse_slice_flags(args) {
        Ok(parts) => parts,
        Err(e) => {
            return CommandOutcome::captured(2, Vec::new(), format!("peek: {e}\n").into_bytes())
        }
    };
    // Line numbers are on by default for peek (use `--no-lines` to disable).
    if !args.iter().any(|a| a == "--no-lines") {
        opts.number = true;
    }
    let Some(file) = positional.first() else {
        return CommandOutcome::captured(2, Vec::new(), b"peek: usage: peek <file> [--range A:B] [--head N] [--tail N] [--grep STR] [--lines]\n".to_vec());
    };
    let path = resolve_path(state, file);
    match read_bounded_regular_file(&path, MAX_AGENT_FILE_BYTES) {
        Ok((bytes, _)) => {
            if bytes.iter().take(8192).any(|&b| b == 0) {
                return CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("peek: {file}: binary file (use `view` or `trace`)\n").into_bytes(),
                );
            }
            let text = String::from_utf8_lossy(&bytes);
            CommandOutcome::captured(0, apply_slice(&text, &opts).into_bytes(), Vec::new())
        }
        Err(e) => {
            CommandOutcome::captured(1, Vec::new(), format!("peek: {file}: {e}\n").into_bytes())
        }
    }
}

/// `patch <file>`: apply a unified diff (read from stdin) to `file`.
pub fn patch(args: &[String], state: &ShellState, stdin: Option<&[u8]>) -> CommandOutcome {
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(file) = positional.first() else {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"patch: usage: patch <file>  (unified diff on stdin)\n".to_vec(),
        );
    };
    let Some(diff_bytes) = stdin else {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"patch: no diff on stdin (pipe a unified diff or use a heredoc)\n".to_vec(),
        );
    };
    if diff_bytes.len() > MAX_PATCH_DIFF_BYTES {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            format!("patch: diff exceeds {MAX_PATCH_DIFF_BYTES} bytes\n").into_bytes(),
        );
    }
    let diff = match std::str::from_utf8(diff_bytes) {
        Ok(diff) => diff,
        Err(e) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: diff is not valid UTF-8: {e}\n").into_bytes(),
            )
        }
    };
    let path = resolve_path(state, file);
    let target = match std::fs::canonicalize(&path) {
        Ok(target) => target,
        Err(e) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: {e}\n").into_bytes(),
            )
        }
    };
    let (original_bytes, permissions) =
        match read_bounded_regular_file(&target, MAX_AGENT_FILE_BYTES) {
            Ok(result) => result,
            Err(e) => {
                return CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("patch: {file}: {e}\n").into_bytes(),
                )
            }
        };
    let original = match std::str::from_utf8(&original_bytes) {
        Ok(original) => original,
        Err(e) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: file is not valid UTF-8: {e}\n").into_bytes(),
            )
        }
    };

    match apply_unified_diff(original, diff) {
        Ok(patched) => match atomic_replace(&target, patched.as_bytes(), permissions) {
            Ok(()) => {
                let (plus, minus) = diff_stats(diff);
                CommandOutcome::captured(
                    0,
                    format!("patched {file} (+{plus} -{minus})\n").into_bytes(),
                    Vec::new(),
                )
            }
            Err(e) => CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: {e}\n").into_bytes(),
            ),
        },
        Err(e) => {
            CommandOutcome::captured(1, Vec::new(), format!("patch: {file}: {e}\n").into_bytes())
        }
    }
}

static PATCH_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Replace an existing file only after its complete new contents are durable.
/// The temporary is created in the destination directory so the final rename is
/// atomic on the supported Unix platforms.
fn atomic_replace(
    path: &Path,
    contents: &[u8],
    permissions: std::fs::Permissions,
) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    let mut last_collision = None;

    for _ in 0..128 {
        let id = PATCH_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".agsh-patch-{}-{id}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // New contents may be more sensitive than the source file. Keep the
            // temporary private until the original permissions are restored.
            options.mode(0o600);
        }
        let mut temp = match options.open(&temp_path) {
            Ok(temp) => temp,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };

        let result = (|| {
            temp.write_all(contents)?;
            temp.set_permissions(permissions)?;
            temp.sync_all()?;
            drop(temp);
            std::fs::rename(&temp_path, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp_path);
        }
        return result;
    }

    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not create a unique patch temporary file",
        )
    }))
}

/// `risk <command>`: run the deterministic risk analysis on a command WITHOUT
/// executing it, so an agent can check before running. Joins its arguments into
/// a command line (quote complex commands, e.g. `risk 'rm -rf /'`).
pub fn risk(args: &[String], state: &ShellState) -> CommandOutcome {
    use std::io::IsTerminal;
    if args.is_empty() {
        return CommandOutcome::captured(
            2,
            Vec::new(),
            b"risk: usage: risk <command>  (quote pipelines/redirects)\n".to_vec(),
        );
    }
    let source = args.join(" ");
    let graph = match agsh_core::parse_line(&source) {
        Ok(g) => g,
        Err(e) => {
            return CommandOutcome::captured(
                2,
                Vec::new(),
                format!("risk: parse error: {e}\n").into_bytes(),
            )
        }
    };
    let findings = agsh_policy::analyze_graph(&graph);
    if findings.is_empty() {
        return CommandOutcome::captured(0, b"risk: no findings\n".to_vec(), Vec::new());
    }
    let tty = std::io::stdout().is_terminal();
    let theme = state.theme();
    let mut out = String::new();
    for f in &findings {
        let label = risk_label(f.level);
        let line = format!("{label} {}: {}", f.code, f.message);
        if tty {
            out.push_str(&theme.paint(risk_role(f.level), &line));
        } else {
            out.push_str(&line);
        }
        out.push('\n');
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn risk_label(level: agsh_policy::RiskLevel) -> &'static str {
    use agsh_policy::RiskLevel::*;
    match level {
        Info => "info",
        Low => "low",
        Medium => "med",
        High => "HIGH",
        Critical => "CRIT",
    }
}

fn risk_role(level: agsh_policy::RiskLevel) -> agsh_style::Role {
    use agsh_policy::RiskLevel::*;
    use agsh_style::Role;
    match level {
        Info | Low => Role::Muted,
        Medium => Role::Warn,
        High | Critical => Role::Error,
    }
}

/// `snapshot [msg] | list | restore [ref]`: lightweight git checkpoints of the
/// working tree (git.snapshot / git.rollback) so an agent can checkpoint before
/// risky edits. Uses `git stash create`+`store` (does NOT disturb the tree).
/// Git stdout/stderr and runtime are bounded, and checkpoint-store failures are
/// reported instead of claiming that an unavailable snapshot was saved.
pub fn snapshot(args: &[String], state: &ShellState) -> CommandOutcome {
    if let Some(denied) = crate::confined_external_denial(state, "git") {
        return denied;
    }
    let Some(git) = crate::resolve_shell_external(state, "git") else {
        return CommandOutcome::captured(
            127,
            Vec::new(),
            b"snapshot: git: command not found\n".to_vec(),
        );
    };
    match args.first().map(String::as_str) {
        Some("list") => git_run(&git, &["stash", "list"], state, "snapshot"),
        Some("restore") => {
            // Overwrite tracked files in cwd with the snapshot's working-tree
            // state (the stash commit's tree). Unlike `stash apply`, this is an
            // exact restore, not a 3-way merge against divergent content.
            let target = args.get(1).map(String::as_str).unwrap_or("stash@{0}");
            let outcome = git_run(&git, &["checkout", target, "--", "."], state, "snapshot");
            if outcome.exit_code == 0 {
                CommandOutcome::captured(
                    0,
                    format!("snapshot: restored working tree from {target}\n").into_bytes(),
                    Vec::new(),
                )
            } else {
                outcome
            }
        }
        _ => {
            let msg = if args.is_empty() {
                "snapshot".to_string()
            } else {
                args.join(" ")
            };
            match git_capture(&git, &["stash", "create"], state) {
                Ok(sha) if !sha.trim().is_empty() => {
                    let sha = sha.trim();
                    if !matches!(sha.len(), 40 | 64)
                        || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        return CommandOutcome::captured(
                            1,
                            Vec::new(),
                            b"snapshot: git returned an invalid snapshot object id\n".to_vec(),
                        );
                    }
                    if let Err(error) = git_capture(
                        &git,
                        &["stash", "store", "-m", &format!("agsh: {msg}"), sha],
                        state,
                    ) {
                        return CommandOutcome::captured(
                            1,
                            Vec::new(),
                            format!("snapshot: {error}\n").into_bytes(),
                        );
                    }
                    let short = &sha[..12];
                    CommandOutcome::captured(
                        0,
                        format!("snapshot saved: {short} (agsh: {msg})\n").into_bytes(),
                        Vec::new(),
                    )
                }
                Ok(_) => CommandOutcome::captured(
                    0,
                    b"snapshot: nothing to save (clean working tree)\n".to_vec(),
                    Vec::new(),
                ),
                Err(e) => {
                    CommandOutcome::captured(1, Vec::new(), format!("snapshot: {e}\n").into_bytes())
                }
            }
        }
    }
}

/// Run `git <args>` in `cwd`, passing its stdout/stderr/exit through as the
/// command outcome.
fn git_run(git: &Path, args: &[&str], state: &ShellState, who: &str) -> CommandOutcome {
    let mut command = std::process::Command::new(git);
    command.args(args).current_dir(state.cwd());
    state.configure_child_env(&mut command);
    match capture_command_with_limits(&mut command, MAX_GIT_CAPTURE_BYTES, GIT_CAPTURE_TIMEOUT) {
        Ok(output) => git_run_output(output, who),
        Err(e) => {
            CommandOutcome::captured(1, Vec::new(), format!("{who}: git: {e}\n").into_bytes())
        }
    }
}

fn git_run_output(output: BoundedCommandOutput, who: &str) -> CommandOutcome {
    let incomplete_streams = output.incomplete_streams();
    let child_exit = command_exit_code(output.status);
    let stdout = output.stdout.bytes;
    let mut stderr = output.stderr.bytes;
    if let Some(streams) = incomplete_streams {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(
            format!("{who}: git: {}\n", incomplete_git_capture_message(streams)).as_bytes(),
        );
        return CommandOutcome::captured(child_exit.max(1), stdout, stderr);
    }
    CommandOutcome::captured(child_exit, stdout, stderr)
}

fn command_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Run `git <args>` in `cwd`, returning trimmed stdout or an error string.
fn git_capture(git: &Path, args: &[&str], state: &ShellState) -> Result<String, String> {
    let mut command = std::process::Command::new(git);
    command.args(args).current_dir(state.cwd());
    state.configure_child_env(&mut command);
    let out = capture_command_with_limits(&mut command, MAX_GIT_CAPTURE_BYTES, GIT_CAPTURE_TIMEOUT)
        .map_err(|e| format!("git: {e}"))?;
    git_capture_output(out)
}

fn git_capture_output(out: BoundedCommandOutput) -> Result<String, String> {
    if let Some(streams) = out.incomplete_streams() {
        return Err(incomplete_git_capture_message(streams));
    }
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout.bytes).into_owned())
    } else {
        let message = String::from_utf8_lossy(&out.stderr.bytes)
            .trim()
            .to_string();
        if message.is_empty() {
            Err(format!("git exited with status {}", out.status))
        } else {
            Err(message)
        }
    }
}

fn incomplete_git_capture_message(streams: &str) -> String {
    format!(
        "output capture incomplete after the direct child exited; retained {streams} descriptor(s) did not reach EOF"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureCompleteness {
    Complete,
    Incomplete,
}

#[derive(Debug)]
struct PipeCapture {
    bytes: Vec<u8>,
    completeness: CaptureCompleteness,
}

impl PipeCapture {
    fn complete(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            completeness: CaptureCompleteness::Complete,
        }
    }

    fn incomplete(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            completeness: CaptureCompleteness::Incomplete,
        }
    }
}

#[derive(Debug)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: PipeCapture,
    stderr: PipeCapture,
}

impl BoundedCommandOutput {
    fn incomplete_streams(&self) -> Option<&'static str> {
        match (self.stdout.completeness, self.stderr.completeness) {
            (CaptureCompleteness::Complete, CaptureCompleteness::Complete) => None,
            (CaptureCompleteness::Incomplete, CaptureCompleteness::Complete) => Some("stdout"),
            (CaptureCompleteness::Complete, CaptureCompleteness::Incomplete) => Some("stderr"),
            (CaptureCompleteness::Incomplete, CaptureCompleteness::Incomplete) => {
                Some("stdout/stderr")
            }
        }
    }
}

fn capture_pipe<R: Read + std::os::fd::AsFd + Send + 'static>(
    mut pipe: R,
    limit: usize,
    abort: Arc<AtomicBool>,
    direct_child_exited: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<PipeCapture>>> {
    let flags = rustix::fs::fcntl_getfl(&pipe)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    std::thread::Builder::new().spawn(move || {
        let mut output = Vec::with_capacity(limit.min(64 * 1024));
        let mut chunk = [0u8; 16 * 1024];
        let mut post_exit_started = None;
        let mut post_exit_bytes = 0usize;
        loop {
            if abort.load(Ordering::Acquire) {
                return Ok(PipeCapture::incomplete(output));
            }
            let exited = direct_child_exited.load(Ordering::Acquire);
            if exited {
                let started = *post_exit_started.get_or_insert_with(Instant::now);
                if post_exit_bytes >= GIT_CAPTURE_POST_EXIT_BYTES
                    || started.elapsed() >= GIT_CAPTURE_POST_EXIT_TIME
                {
                    return Ok(PipeCapture::incomplete(output));
                }
            }
            match pipe.read(&mut chunk) {
                Ok(0) => return Ok(PipeCapture::complete(output)),
                Ok(read) if read <= limit.saturating_sub(output.len()) => {
                    output.extend_from_slice(&chunk[..read]);
                    if exited {
                        post_exit_bytes = post_exit_bytes.saturating_add(read);
                    }
                }
                Ok(_) => {
                    abort.store(true, Ordering::Release);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("command output limit of {limit} bytes exceeded"),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if exited {
                        return Ok(PipeCapture::incomplete(output));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    abort.store(true, Ordering::Release);
                    return Err(error);
                }
            }
        }
    })
}

fn kill_capture_group(child: &mut std::process::Child) -> std::io::Result<ExitStatus> {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    child.wait()
}

fn join_capture(
    handle: std::thread::JoinHandle<std::io::Result<PipeCapture>>,
) -> std::io::Result<PipeCapture> {
    handle
        .join()
        .map_err(|_| std::io::Error::other("command output reader panicked"))?
}

fn capture_command_with_limits(
    command: &mut std::process::Command,
    stream_limit: usize,
    timeout: Duration,
) -> std::io::Result<BoundedCommandOutput> {
    use std::os::unix::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = kill_capture_group(&mut child);
            return Err(std::io::Error::other("command stdout pipe is unavailable"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = kill_capture_group(&mut child);
            return Err(std::io::Error::other("command stderr pipe is unavailable"));
        }
    };
    let abort = Arc::new(AtomicBool::new(false));
    let direct_child_exited = Arc::new(AtomicBool::new(false));
    let stdout_handle = match capture_pipe(
        stdout,
        stream_limit,
        Arc::clone(&abort),
        Arc::clone(&direct_child_exited),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            drop(stderr);
            let _ = kill_capture_group(&mut child);
            return Err(error);
        }
    };
    let stderr_handle = match capture_pipe(
        stderr,
        stream_limit,
        Arc::clone(&abort),
        Arc::clone(&direct_child_exited),
    ) {
        Ok(handle) => handle,
        Err(error) => {
            abort.store(true, Ordering::Release);
            let _ = kill_capture_group(&mut child);
            let _ = join_capture(stdout_handle);
            return Err(error);
        }
    };
    let started = Instant::now();

    let status = loop {
        if abort.load(Ordering::Acquire) {
            let _ = kill_capture_group(&mut child);
            let stdout = join_capture(stdout_handle);
            let stderr = join_capture(stderr_handle);
            return Err(stdout
                .err()
                .or_else(|| stderr.err())
                .unwrap_or_else(|| std::io::Error::other("command output capture aborted")));
        }
        if started.elapsed() >= timeout {
            abort.store(true, Ordering::Release);
            let _ = kill_capture_group(&mut child);
            let _ = join_capture(stdout_handle);
            let _ = join_capture(stderr_handle);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command exceeded {} second timeout", timeout.as_secs_f64()),
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                abort.store(true, Ordering::Release);
                let _ = kill_capture_group(&mut child);
                let _ = join_capture(stdout_handle);
                let _ = join_capture(stderr_handle);
                return Err(error);
            }
        }
    };

    // A child that daemonized a descendant must not leave our reader threads
    // blocked forever on inherited pipe descriptors.
    direct_child_exited.store(true, Ordering::Release);
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let stdout = join_capture(stdout_handle)?;
    let stderr = join_capture(stderr_handle)?;
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let plus = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    let minus = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    (plus, minus)
}

/// Apply a unified diff to `original`, returning the patched text. Context and
/// removed lines must match exactly at each hunk's location (no fuzz); on any
/// mismatch it errors rather than corrupting the file.
pub fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    let src: Vec<&str> = original.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut src_idx = 0usize; // 0-based position in src consumed so far
    let mut lines = diff.lines().peekable();

    let mut applied_any = false;
    while let Some(line) = lines.next() {
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ") {
            continue;
        }
        let Some(hunk) = line.strip_prefix("@@") else {
            continue; // ignore preamble / unrelated lines
        };
        applied_any = true;
        // Parse "@@ -l,s +l,s @@": the old start line (1-based).
        let old_start = parse_hunk_old_start(hunk)?;
        let old_start0 = old_start.saturating_sub(1);
        // Emit untouched source up to the hunk start.
        if old_start0 < src_idx {
            return Err("overlapping or out-of-order hunks".to_string());
        }
        while src_idx < old_start0 && src_idx < src.len() {
            out.push(src[src_idx].to_string());
            src_idx += 1;
        }
        // Apply hunk body lines until the next hunk/header or EOF.
        while let Some(peek) = lines.peek() {
            if peek.starts_with("@@") || peek.starts_with("diff ") {
                break;
            }
            let body = lines.next().unwrap();
            match body.chars().next() {
                Some(' ') | None => {
                    // Context line: must match source.
                    let want = &body.get(1..).unwrap_or("");
                    let got = src.get(src_idx).copied().unwrap_or("");
                    if want != &got {
                        return Err(format!(
                            "context mismatch at line {}: expected {want:?}, found {got:?}",
                            src_idx + 1
                        ));
                    }
                    out.push(got.to_string());
                    src_idx += 1;
                }
                Some('-') => {
                    let want = &body[1..];
                    let got = src.get(src_idx).copied().unwrap_or("");
                    if want != got {
                        return Err(format!(
                            "removed-line mismatch at line {}: expected {want:?}, found {got:?}",
                            src_idx + 1
                        ));
                    }
                    src_idx += 1;
                }
                Some('+') => {
                    out.push(body[1..].to_string());
                }
                Some('\\') => {} // "\ No newline at end of file"
                Some(_) => break,
            }
        }
    }
    if !applied_any {
        return Err("no hunks found in diff".to_string());
    }
    // Emit the remaining source.
    while src_idx < src.len() {
        out.push(src[src_idx].to_string());
        src_idx += 1;
    }
    let mut result = out.join("\n");
    if !result.is_empty() && (original.is_empty() || original.ends_with('\n')) {
        result.push('\n');
    }
    Ok(result)
}

fn parse_hunk_old_start(hunk: &str) -> Result<usize, String> {
    // hunk looks like " -l,s +l,s @@ optional"
    let minus = hunk
        .split_whitespace()
        .find(|t| t.starts_with('-'))
        .ok_or("malformed hunk header")?;
    let nums = &minus[1..];
    let start = nums.split(',').next().unwrap_or("0");
    start
        .parse::<usize>()
        .map_err(|_| "malformed hunk start".to_string())
}

fn resolve_path(state: &ShellState, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        state.cwd().join(p)
    }
}

fn short_cwd(state: &ShellState) -> String {
    let cwd = state.cwd().display().to_string();
    if let Some(home) = state.lookup("HOME").filter(|h| !h.is_empty()) {
        if cwd == home {
            return "~".to_string();
        }
        if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    cwd
}

fn base_name(path: &str) -> Option<String> {
    let t = path.trim_end_matches('/');
    (!t.is_empty()).then(|| t.rsplit('/').next().unwrap_or(t).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_fixture(label: &str) -> (PathBuf, ShellState) {
        let id = PATCH_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agsh-agent-{label}-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut state = ShellState::from_current_process();
        state.set_cwd(dir.clone());
        (dir, state)
    }

    #[test]
    fn slice_head_tail_range_grep() {
        let text = "a\nb\nc\nd\ne\n";
        let opts = SliceOpts {
            head: Some(2),
            ..Default::default()
        };
        assert_eq!(apply_slice(text, &opts), "a\nb\n");
        let opts = SliceOpts {
            tail: Some(2),
            ..Default::default()
        };
        assert_eq!(apply_slice(text, &opts), "d\ne\n");
        let opts = SliceOpts {
            range: Some((2, 3)),
            ..Default::default()
        };
        assert_eq!(apply_slice(text, &opts), "b\nc\n");
        let opts = SliceOpts {
            grep: Some("c".into()),
            number: true,
            ..Default::default()
        };
        assert_eq!(apply_slice(text, &opts), "     3  c\n");
    }

    #[test]
    fn parse_flags() {
        let args: Vec<String> = ["f.txt", "--range", "2:5", "--lines"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (opts, pos, _) = parse_slice_flags(&args).unwrap();
        assert_eq!(pos, vec!["f.txt"]);
        assert_eq!(opts.range, Some((2, 5)));
        assert!(opts.number);
    }

    #[test]
    fn context_rejects_stray_args() {
        let state = ShellState::from_current_process();
        let bad = context(&["ls".to_string()], &state);
        assert_eq!(bad.exit_code, 2);
        assert!(String::from_utf8_lossy(&bad.stderr).contains("unexpected argument"));
        // No subject is fine.
        assert_eq!(context(&[], &state).exit_code, 0);
        assert_eq!(context(&["--json".to_string()], &state).exit_code, 0);
    }

    #[test]
    fn applies_unified_diff() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -1,3 +1,3 @@\n line1\n-line2\n+LINE-TWO\n line3\n";
        let patched = apply_unified_diff(original, diff).unwrap();
        assert_eq!(patched, "line1\nLINE-TWO\nline3\n");
    }

    #[test]
    fn patch_context_mismatch_errors() {
        let original = "a\nb\nc\n";
        let diff = "@@ -1,3 +1,3 @@\n a\n-WRONG\n+x\n c\n";
        assert!(apply_unified_diff(original, diff).is_err());
    }

    #[test]
    fn patch_pure_addition() {
        let original = "a\nb\n";
        let diff = "@@ -2,1 +2,2 @@\n b\n+c\n";
        assert_eq!(apply_unified_diff(original, diff).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn patch_missing_file_fails_without_creating_it() {
        let (dir, state) = patch_fixture("patch-missing");
        let path = dir.join("missing.txt");
        let outcome = patch(
            &["missing.txt".to_string()],
            &state,
            Some(b"@@ -0,0 +1 @@\n+created\n"),
        );

        assert_eq!(outcome.exit_code, 1);
        assert!(!path.exists());
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("missing.txt"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patch_non_utf8_file_fails_without_modifying_it() {
        let (dir, state) = patch_fixture("patch-non-utf8-file");
        let path = dir.join("binary.dat");
        let original = b"\xff\xfeoriginal\0bytes";
        std::fs::write(&path, original).unwrap();

        let outcome = patch(
            &["binary.dat".to_string()],
            &state,
            Some(b"@@ -0,0 +1 @@\n+replacement\n"),
        );

        assert_eq!(outcome.exit_code, 1);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("not valid UTF-8"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peek_and_patch_reject_oversized_files_without_reading_them() {
        let (dir, state) = patch_fixture("agent-oversized-file");
        let path = dir.join("huge.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_AGENT_FILE_BYTES + 1) as u64).unwrap();
        drop(file);

        let peeked = peek(&["huge.txt".to_string()], &state);
        assert_eq!(peeked.exit_code, 1);
        assert!(String::from_utf8_lossy(&peeked.stderr).contains("exceeds"));

        let patched = patch(
            &["huge.txt".to_string()],
            &state,
            Some(b"@@ -0,0 +1 @@\n+replacement\n"),
        );
        assert_eq!(patched.exit_code, 1);
        assert!(String::from_utf8_lossy(&patched.stderr).contains("exceeds"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (MAX_AGENT_FILE_BYTES + 1) as u64
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patch_non_utf8_diff_fails_without_modifying_file() {
        let (dir, state) = patch_fixture("patch-non-utf8-diff");
        let path = dir.join("source.txt");
        let original = b"original\n";
        std::fs::write(&path, original).unwrap();

        let outcome = patch(&["source.txt".to_string()], &state, Some(b"\xff\xfe"));

        assert_eq!(outcome.exit_code, 1);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("diff is not valid UTF-8"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn patch_unreadable_file_fails_without_modifying_it() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, state) = patch_fixture("patch-unreadable");
        let path = dir.join("source.txt");
        let original = b"original\n";
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        // An elevated test process can read mode-000 files, so it cannot exercise
        // the permission-denied path reliably.
        if std::fs::File::open(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }

        let outcome = patch(
            &["source.txt".to_string()],
            &state,
            Some(b"@@ -1 +1 @@\n-original\n+replacement\n"),
        );

        assert_eq!(outcome.exit_code, 1);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o000
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patch_atomically_replaces_file_and_preserves_permissions() {
        let (dir, state) = patch_fixture("patch-atomic");
        let path = dir.join("source.txt");
        std::fs::write(&path, b"alpha\nbeta\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }

        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).unwrap().ino()
        };

        let outcome = patch(
            &["source.txt".to_string()],
            &state,
            Some(b"@@ -1,2 +1,2 @@\n alpha\n-beta\n+BETA\n"),
        );

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nBETA\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            assert_ne!(std::fs::metadata(&path).unwrap().ino(), original_inode);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o640
            );
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_respects_sticky_confinement_before_spawning_git() {
        let (dir, mut state) = patch_fixture("snapshot-confined");
        state.set_confine(&["true".to_string()]);

        let outcome = snapshot(&["list".to_string()], &state);

        assert_eq!(outcome.exit_code, 126);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("git: not permitted"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_resolves_git_from_the_shell_path() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, mut state) = patch_fixture("snapshot-path");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(&git, "#!/bin/sh\nprintf '%s' \"$SNAPSHOT_TEST\"").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        state.set_var("PATH", bin.display().to_string());
        state.export_var("SNAPSHOT_TEST", "shell-env-git");

        let outcome = snapshot(&["list".to_string()], &state);

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"shell-env-git");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_propagates_stash_store_failure() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, mut state) = patch_fixture("snapshot-store-failure");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\ncase \"$1 $2\" in\n  'stash create') printf '%040d' 0 ;;\n  'stash store') echo 'store rejected' >&2; exit 9 ;;\n  *) exit 8 ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        state.set_var("PATH", bin.display().to_string());

        let outcome = snapshot(&["before-risk".to_string()], &state);

        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.stdout.is_empty());
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("store rejected"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_capture_kills_and_reaps_on_output_limit() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "while :; do printf '0123456789abcdef0123456789abcdef'; done",
        ]);
        let started = std::time::Instant::now();

        let error =
            capture_command_with_limits(&mut command, 32 * 1024, std::time::Duration::from_secs(2))
                .unwrap_err();

        assert!(error.to_string().contains("output limit"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_capture_kills_and_reaps_on_timeout() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "while :; do :; done"]);
        let started = std::time::Instant::now();

        let error =
            capture_command_with_limits(&mut command, 1024, std::time::Duration::from_millis(50))
                .unwrap_err();

        assert!(error.kind() == std::io::ErrorKind::TimedOut, "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn capture_pipe_stops_when_a_descendant_keeps_the_descriptor_open() {
        use std::os::unix::net::UnixStream;

        let (reader, mut retained_writer) = UnixStream::pair().unwrap();
        let abort = Arc::new(AtomicBool::new(false));
        let direct_child_exited = Arc::new(AtomicBool::new(false));
        let handle = capture_pipe(
            reader,
            1024,
            Arc::clone(&abort),
            Arc::clone(&direct_child_exited),
        )
        .unwrap();
        retained_writer.write_all(b"captured").unwrap();
        direct_child_exited.store(true, Ordering::Release);

        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || send.send(join_capture(handle)).unwrap());
        let output = receive
            .recv_timeout(Duration::from_secs(1))
            .expect("reader must not wait for the retained writer")
            .unwrap();
        assert_eq!(output.bytes, b"captured");
        assert_eq!(output.completeness, CaptureCompleteness::Incomplete);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_valid_looking_sha_from_incomplete_git_capture() {
        use std::os::unix::process::ExitStatusExt;

        let stash_output = BoundedCommandOutput {
            status: ExitStatus::from_raw(0),
            stdout: PipeCapture {
                bytes: vec![b'0'; 40],
                completeness: CaptureCompleteness::Incomplete,
            },
            stderr: PipeCapture {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Complete,
            },
        };

        let error = git_capture_output(stash_output).unwrap_err();
        assert!(error.contains("incomplete"), "{error}");
        assert!(error.contains("stdout"), "{error}");

        let list_output = BoundedCommandOutput {
            status: ExitStatus::from_raw(0),
            stdout: PipeCapture {
                bytes: b"partial list".to_vec(),
                completeness: CaptureCompleteness::Incomplete,
            },
            stderr: PipeCapture {
                bytes: Vec::new(),
                completeness: CaptureCompleteness::Complete,
            },
        };
        let outcome = git_run_output(list_output, "snapshot");
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.stdout, b"partial list");
        let diagnostic = String::from_utf8_lossy(&outcome.stderr);
        assert!(diagnostic.contains("incomplete"), "{diagnostic}");
        assert!(diagnostic.contains("stdout"), "{diagnostic}");
    }

    #[cfg(unix)]
    #[test]
    fn git_command_exit_mapping_preserves_signal_status() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(command_exit_code(ExitStatus::from_raw(7 << 8)), 7);
        assert_eq!(command_exit_code(ExitStatus::from_raw(9)), 137);
    }
}
