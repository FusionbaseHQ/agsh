//! `sessions` — discover and resume Claude Code / Codex sessions for the current
//! folder (and its subfolders).
//!
//! Claude stores a session as `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`,
//! where `<encoded-cwd>` is the project's absolute path with `/` → `-`. Codex
//! stores `~/.codex/sessions/Y/M/D/rollout-*.jsonl`, whose first line is a
//! `session_meta` record carrying the real `cwd` and `id`. We match by the actual
//! cwd recorded inside each session, so the lossy path encoding can't cause false
//! matches. Resume runs `claude --resume <id>` / `codex resume <id>` from the
//! session's directory, inheriting the terminal.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agsh_style::{Color, Role, Style, Theme};
use serde_json::Value;

use crate::state::ShellState;
use crate::CommandOutcome;

/// Cap the Codex scan (date-bucketed, can be thousands) to the most recent files.
const CODEX_SCAN_LIMIT: usize = 2000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
    /// The (program, args) that resumes a session by id.
    fn resume_command(self, id: &str) -> (&'static str, Vec<String>) {
        match self {
            Agent::Claude => ("claude", vec!["--resume".into(), id.into()]),
            Agent::Codex => ("codex", vec!["resume".into(), id.into()]),
        }
    }
}

struct Session {
    agent: Agent,
    id: String,
    cwd: String,
    modified: SystemTime,
    summary: String,
    file: PathBuf,
}

/// `sessions` — list sessions for this folder; `sessions N` resumes the Nth;
/// `sessions --all` lists sessions from every folder.
pub fn builtin_sessions(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let all = args.iter().any(|a| a == "--all" || a == "-a");
    let resume_n = args.iter().find_map(|a| a.parse::<usize>().ok());
    let sessions = find_sessions(state.cwd(), all);

    if let Some(n) = resume_n {
        return resume_nth(&sessions, n, state);
    }

    use std::io::IsTerminal;
    let tty = std::io::stdout().is_terminal();
    let cwd = state.cwd().to_string_lossy().into_owned();
    let body = render(&sessions, &state.theme(), tty, &cwd, all);
    let code = if sessions.is_empty() { 1 } else { 0 };
    CommandOutcome::captured(code, body.into_bytes(), Vec::new())
}

/// A readable folder label for a session. In the default (this-folder) listing it
/// is compact — `.` for the cwd, a relative path for a subfolder. With `--all`
/// every row shows the full `~/…`-abbreviated path so the locations are
/// consistent and comparable.
fn display_path(scwd: &str, cwd: &str, home: &str, all: bool) -> String {
    if scwd.is_empty() {
        return "?".to_string();
    }
    if !all {
        if scwd == cwd {
            return ".".to_string();
        }
        if let Some(rel) = scwd.strip_prefix(&format!("{cwd}/")) {
            return rel.to_string();
        }
    }
    if !home.is_empty() {
        if let Some(rest) = scwd.strip_prefix(home) {
            return format!("~{rest}");
        }
    }
    scwd.to_string()
}

/// Fit a path to `max` chars, keeping the (more informative) tail with a leading `…`.
fn path_fit(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        p.to_string()
    } else {
        let tail: String = p.chars().skip(n - (max - 1)).collect();
        format!("…{tail}")
    }
}

/// The brand color, glyph, and display name for an agent.
fn brand(agent: Agent) -> (Color, &'static str, &'static str) {
    match agent {
        // Anthropic clay/coral.
        Agent::Claude => (Color::rgb(0xD9, 0x77, 0x57), "✳", "Claude"),
        // OpenAI green.
        Agent::Codex => (Color::rgb(0x10, 0xA3, 0x7F), "◆", "Codex"),
    }
}

/// Right-pad a string to `width` visible chars (for column alignment before color
/// is applied, so the ANSI bytes don't break the width).
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn find_sessions(cwd: &Path, all: bool) -> Vec<Session> {
    let mut out = Vec::new();
    let Some(home) = std::env::var_os("HOME") else {
        return out;
    };
    let home = PathBuf::from(home);
    let cwd_s = cwd.to_string_lossy().into_owned();
    find_claude(&home, &cwd_s, all, &mut out);
    find_codex(&home, &cwd_s, all, &mut out);
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

fn under(cwd: &str, p: &str) -> bool {
    p == cwd || p.starts_with(&format!("{cwd}/"))
}

fn find_claude(home: &Path, cwd: &str, all: bool, out: &mut Vec<Session>) {
    let projects = home.join(".claude/projects");
    let encoded = cwd.replace('/', "-");
    let prefix = format!("{encoded}-");
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        return;
    };
    for d in dirs.flatten() {
        let name = d.file_name().to_string_lossy().into_owned();
        // The cwd's own dir, or a subfolder's dir (encoded + "-…"). Verified by the
        // real recorded cwd below, so the lossy encoding can't cause false matches.
        if !all && name != encoded && !name.starts_with(&prefix) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(d.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let modified = f
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let head = read_head(&p, 80);
            let scwd = head
                .iter()
                .find_map(|r| r.get("cwd").and_then(Value::as_str))
                .map(String::from);
            // If we could read the real cwd, require it to be under `cwd`; if not,
            // trust the directory match.
            if !all {
                if let Some(c) = &scwd {
                    if !under(cwd, c) {
                        continue;
                    }
                }
            }
            out.push(Session {
                agent: Agent::Claude,
                id,
                cwd: scwd.unwrap_or_default(),
                modified,
                summary: claude_summary(&head),
                file: p,
            });
        }
    }
}

fn find_codex(home: &Path, cwd: &str, all: bool, out: &mut Vec<Session>) {
    let root = home.join(".codex/sessions");
    let mut files: Vec<(PathBuf, SystemTime)> = Vec::new();
    collect_jsonl(&root, &mut files);
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.truncate(CODEX_SCAN_LIMIT);
    for (p, modified) in files {
        // Cheap filter: only the first record (session_meta) has the cwd + id, so
        // read a single line per candidate; the full head is read only on a match.
        let Some(meta) = read_first_record(&p).and_then(|r| r.get("payload").cloned()) else {
            continue;
        };
        let scwd = meta.get("cwd").and_then(Value::as_str).map(String::from);
        if !all {
            match &scwd {
                Some(c) if under(cwd, c) => {}
                _ => continue,
            }
        }
        let Some(id) = meta.get("id").and_then(Value::as_str) else {
            continue;
        };
        out.push(Session {
            agent: Agent::Codex,
            id: id.to_string(),
            cwd: scwd.unwrap_or_default(),
            modified,
            // Read a bit deeper here (only for cwd-matched sessions): Codex prepends
            // the AGENTS.md context before the real first user prompt.
            summary: codex_summary(&read_head(&p, 120)),
            file: p,
        });
    }
}

/// Parse just the first JSONL record (cheap cwd/id filter for Codex).
fn read_first_record(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Recursively collect `*.jsonl` files with their mtimes.
fn collect_jsonl(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else {
            continue;
        };
        let p = e.path();
        if ft.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            let m = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((p, m));
        }
    }
}

/// Parse the first `n` JSONL records of a session file.
fn read_head(path: &Path, n: usize) -> Vec<Value> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .take(n)
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn claude_summary(head: &[Value]) -> String {
    for r in head {
        if r.get("type").and_then(Value::as_str) == Some("summary") {
            if let Some(s) = r.get("summary").and_then(Value::as_str) {
                return s.to_string();
            }
        }
    }
    for r in head {
        if r.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(t) = message_text(r.get("message")) {
                let t = t.trim();
                if !t.is_empty() && !t.starts_with('<') {
                    return t.to_string();
                }
            }
        }
    }
    String::new()
}

fn message_text(msg: Option<&Value>) -> Option<String> {
    let content = msg?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    for part in content.as_array()? {
        if part.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn codex_summary(head: &[Value]) -> String {
    for r in head {
        let Some(pl) = r.get("payload") else {
            continue;
        };
        if pl.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(arr) = pl.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in arr {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                let t = t.trim();
                // Skip Codex's injected context: <environment_context> tags and the
                // AGENTS.md / instructions blocks it prepends as user messages.
                if t.is_empty()
                    || t.starts_with('<')
                    || t.starts_with("# AGENTS.md")
                    || t.starts_with("# Instructions")
                    || t.starts_with("<user_instructions>")
                {
                    continue;
                }
                return t.to_string();
            }
        }
    }
    String::new()
}

fn render(sessions: &[Session], theme: &Theme, tty: bool, cwd: &str, all: bool) -> String {
    if sessions.is_empty() {
        return "sessions: no Claude or Codex sessions for this folder (try `sessions --all`)\n"
            .to_string();
    }
    let lvl = theme.level;
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = std::path::Path::new(cwd)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string());
    // Width of the folder column: widest path label, clamped to a sane range.
    let path_w = sessions
        .iter()
        .map(|s| display_path(&s.cwd, cwd, &home, all).chars().count())
        .max()
        .unwrap_or(1)
        .clamp(4, 32);
    let brand_word = |a: Agent| Style::new().fg(brand(a).0).bold().paint(brand(a).2, lvl);
    let mut out = String::new();

    // Header: branded "Claude & Codex", the folder, and a dim hint line.
    out.push('\n');
    out.push_str(&format!(
        "  {} {} {} sessions in {}\n",
        brand_word(Agent::Claude),
        theme.paint(Role::Muted, "&"),
        brand_word(Agent::Codex),
        theme.paint(Role::Path, &folder),
    ));
    out.push_str(&theme.paint(
        Role::Muted,
        "  resume: sessions N   ·   click a row to open the transcript   ·   sessions --all\n\n",
    ));

    for (i, s) in sessions.iter().enumerate() {
        let n = i + 1;
        let (color, glyph, name) = brand(s.agent);
        let badge = Style::new()
            .fg(color)
            .bold()
            .paint(&pad(&format!("{glyph} {name}"), 9), lvl);
        let path = theme.paint(
            Role::Path,
            &pad(
                &path_fit(&display_path(&s.cwd, cwd, &home, all), path_w),
                path_w,
            ),
        );
        let summary = if s.summary.is_empty() {
            theme.paint(Role::Muted, "(no summary)")
        } else {
            truncate(&s.summary, 64)
        };
        // Only the index is the hyperlink — wrapping the whole row makes terminals
        // that underline links turn every space into an underscore (unreadable).
        let index = theme.paint(Role::Accent, &format!("{n:>2}"));
        let index = if tty {
            let uri = format!("file://{}", s.file.display());
            format!("\x1b]8;;{uri}\x1b\\{index}\x1b]8;;\x1b\\")
        } else {
            index
        };
        out.push_str(&format!(
            "  {index}  {badge}  {}  {path}  {summary}\n",
            theme.paint(Role::Muted, &pad(&ago(s.modified), 8)),
        ));
    }
    out
}

fn resume_nth(sessions: &[Session], n: usize, state: &ShellState) -> CommandOutcome {
    let Some(s) = n.checked_sub(1).and_then(|i| sessions.get(i)) else {
        return CommandOutcome::captured(
            1,
            Vec::new(),
            format!("sessions: no session #{n} (have {})\n", sessions.len()).into_bytes(),
        );
    };
    let (prog, args) = s.agent.resume_command(&s.id);
    if let Some(denied) = crate::confined_external_denial(state, prog) {
        return denied;
    }
    let Some(path) = crate::resolve_shell_external(state, prog) else {
        return CommandOutcome::captured(
            127,
            Vec::new(),
            format!("sessions: failed to run {prog}: command not found\n").into_bytes(),
        );
    };
    let id8: String = s.id.chars().take(8).collect();
    eprintln!(
        "sessions: resuming {} {} in {}…",
        s.agent.label(),
        id8,
        s.cwd
    );
    let mut cmd = match crate::executor::prepare_internal_external_command(&path, &args, state) {
        Ok(command) => command,
        Err(error) => {
            return CommandOutcome::captured(
                126,
                Vec::new(),
                format!("sessions: failed to run {prog}: {error}\n").into_bytes(),
            )
        }
    };
    if !s.cwd.is_empty() {
        cmd.current_dir(&s.cwd);
    }
    match cmd.status() {
        Ok(status) => CommandOutcome::captured(status.code().unwrap_or(0), Vec::new(), Vec::new()),
        Err(e) => CommandOutcome::captured(
            127,
            Vec::new(),
            format!("sessions: failed to run {prog}: {e}\n").into_bytes(),
        ),
    }
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 7 {
        format!("{}d ago", secs / 86_400)
    } else {
        format!("{}w ago", secs / (86_400 * 7))
    }
}

fn truncate(s: &str, max: usize) -> String {
    let oneline = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if oneline.chars().count() <= max {
        oneline
    } else {
        let head: String = oneline.chars().take(max - 1).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_matches_dir_and_subdirs() {
        assert!(under("/a/b", "/a/b"));
        assert!(under("/a/b", "/a/b/c"));
        assert!(!under("/a/b", "/a/bc"));
        assert!(!under("/a/b", "/a"));
    }

    #[test]
    fn truncate_is_char_safe_and_oneline() {
        assert_eq!(truncate("hi  there\nyou", 64), "hi there you");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn render_links_when_tty_and_plain_otherwise() {
        let theme = Theme::plain();
        let s = Session {
            agent: Agent::Claude,
            id: "abcd1234efgh".into(),
            cwd: "/proj".into(),
            modified: SystemTime::now(),
            summary: "do the thing".into(),
            file: PathBuf::from("/proj/s.jsonl"),
        };
        let tty = render(std::slice::from_ref(&s), &theme, true, "proj", false);
        assert!(
            tty.contains("\x1b]8;;file:///proj/s.jsonl\x1b\\"),
            "missing OSC 8 link: {tty:?}"
        );
        assert!(tty.contains("Claude") && tty.contains("do the thing") && tty.contains("proj"));
        let piped = render(&[s], &theme, false, "proj", false);
        assert!(
            !piped.contains("\x1b]8;;"),
            "piped output must not be clickable"
        );

        assert!(render(&[], &theme, true, "proj", false).contains("no Claude or Codex sessions"));
    }

    #[test]
    fn claude_summary_prefers_summary_then_skips_xml() {
        let recs: Vec<Value> = vec![
            serde_json::json!({"type":"user","message":{"content":"<env>stuff</env>"}}),
            serde_json::json!({"type":"user","message":{"content":[{"type":"text","text":"real prompt"}]}}),
        ];
        assert_eq!(claude_summary(&recs), "real prompt");
        let with_summary: Vec<Value> =
            vec![serde_json::json!({"type":"summary","summary":"a nice summary"})];
        assert_eq!(claude_summary(&with_summary), "a nice summary");
    }

    #[test]
    fn resume_respects_sticky_confinement_before_spawning_agent() {
        let session = Session {
            agent: Agent::Claude,
            id: "session-id".into(),
            cwd: "/tmp".into(),
            modified: SystemTime::now(),
            summary: String::new(),
            file: PathBuf::from("/tmp/session.jsonl"),
        };
        let mut state = ShellState::from_current_process();
        state.set_confine(&["true".to_string()]);

        let outcome = resume_nth(&[session], 1, &state);

        assert_eq!(outcome.exit_code, 126);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("claude: not permitted"));
    }
}
