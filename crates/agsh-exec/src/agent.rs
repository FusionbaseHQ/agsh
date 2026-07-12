//! Agent-helper builtins (Milestone 4, shell-level): token-efficient, clearer
//! introspection and editing for an AI agent driving the shell.
//!
//! - `context`  — one-shot structured shell state (cwd/git/last/jobs/env/recent).
//! - `trace`    — slice captured output (head/tail/range/grep/lines) — see
//!   `builtins::builtin_trace`, which uses [`apply_slice`] here.
//! - `peek`     — read file slices with line numbers (file.read_range).
//! - `patch`    — apply a unified diff from stdin to a file (file.patch).
//!
//! No socket server or MCP: a terminal app drives the shell directly; these are
//! ordinary commands the agent runs.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{CommandOutcome, ShellState};

/// Largest number of lines any of these builtins will emit (runaway guard).
const MAX_LINES: usize = 5000;

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
    match std::fs::read(&path) {
        Ok(bytes) => {
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
    let mut source = match std::fs::File::open(&target) {
        Ok(source) => source,
        Err(e) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: {e}\n").into_bytes(),
            )
        }
    };
    let permissions = match source.metadata() {
        Ok(metadata) if metadata.is_file() => metadata.permissions(),
        Ok(_) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: not a regular file\n").into_bytes(),
            )
        }
        Err(e) => {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                format!("patch: {file}: {e}\n").into_bytes(),
            )
        }
    };
    let mut original_bytes = Vec::new();
    if let Err(e) = source.read_to_end(&mut original_bytes) {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            format!("patch: {file}: {e}\n").into_bytes(),
        );
    }
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
        let mut temp = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
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
pub fn snapshot(args: &[String], state: &ShellState) -> CommandOutcome {
    let cwd = state.cwd();
    match args.first().map(String::as_str) {
        Some("list") => git_run(&["stash", "list"], cwd, "snapshot"),
        Some("restore") => {
            // Overwrite tracked files in cwd with the snapshot's working-tree
            // state (the stash commit's tree). Unlike `stash apply`, this is an
            // exact restore, not a 3-way merge against divergent content.
            let target = args.get(1).map(String::as_str).unwrap_or("stash@{0}");
            let outcome = git_run(&["checkout", target, "--", "."], cwd, "snapshot");
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
            match git_capture(&["stash", "create"], cwd) {
                Ok(sha) if !sha.trim().is_empty() => {
                    let sha = sha.trim().to_string();
                    let _ = git_capture(
                        &["stash", "store", "-m", &format!("agsh: {msg}"), &sha],
                        cwd,
                    );
                    let short = &sha[..sha.len().min(12)];
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
fn git_run(args: &[&str], cwd: &Path, who: &str) -> CommandOutcome {
    match std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
    {
        Ok(o) => CommandOutcome::captured(o.status.code().unwrap_or(1), o.stdout, o.stderr),
        Err(e) => {
            CommandOutcome::captured(1, Vec::new(), format!("{who}: git: {e}\n").into_bytes())
        }
    }
}

/// Run `git <args>` in `cwd`, returning trimmed stdout or an error string.
fn git_capture(args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
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
}
