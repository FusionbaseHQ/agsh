//! git family compactor: turn `git` subcommand output into a structured,
//! token-economical summary.
//!
//! Handles status (long + `--porcelain`), diff / diff --stat, log /
//! log --oneline, commit, push, pull, fetch, merge, add and checkout/switch.
//! Unknown subcommands fall back to a light line-count summary. Non-zero exit
//! is treated as a failure, and `fatal:` / `error:` lines on stderr are
//! captured as failures regardless of subcommand.

use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};

/// Longest single detail line kept before truncation.
const MAX_LINE: usize = 200;
/// Cap on how many entries we collect into any one detail list.
const CAP: usize = 50;

static_regex!(
    GIT_STAT_RE,
    r"(\d+) files? changed(?:, (\d+) insertions?\(\+\))?(?:, (\d+) deletions?\(-\))?"
);
static_regex!(DIFF_HEADER_RE, r"^diff --git a/(.+) b/(.+)$");
static_regex!(COMMIT_HEADER_RE, r"^\[(.+?) ([0-9a-f]{7,40})\] (.+)$");
static_regex!(SYNC_UPDATE_RE, r"[0-9a-f]{7,}\.\.\.?[0-9a-f]{7,}");

/// git subcommands this compactor understands; used to locate the subcommand
/// even when global options like `git -C <dir>` shift its argv position.
const KNOWN: &[&str] = &[
    "status", "diff", "log", "commit", "push", "pull", "fetch", "add", "checkout", "switch",
    "merge",
];

pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let mut s = SemanticSummary::new(cx, "git");

    // Defensive: only treat this as git if argv[0] really is git.
    if command_basename(cx.argv) != "git" {
        generic_git(cx, &mut s, None);
        collect_errors(cx, &mut s);
        return s;
    }

    let sub = git_subcommand(cx.argv);
    match sub {
        Some("status") => status(cx, &mut s),
        Some("diff") => diff(cx, &mut s),
        Some("log") => log(cx, &mut s),
        Some("commit") => commit(cx, &mut s),
        Some(sb @ ("push" | "pull" | "fetch" | "merge")) => sync(cx, &mut s, sb),
        Some("add") => add(cx, &mut s),
        Some(sb @ ("checkout" | "switch")) => checkout(cx, &mut s, sb),
        other => generic_git(cx, &mut s, other),
    }

    // fatal:/error:/warning: always come on stderr; capture them everywhere.
    collect_errors(cx, &mut s);

    s.family = match sub {
        Some(sb) => format!("git {sb}"),
        None => "git".to_string(),
    };
    s
}

/// Find the git subcommand, scanning past global flags and their values.
fn git_subcommand(argv: &[String]) -> Option<&str> {
    argv.iter()
        .skip(1)
        .map(String::as_str)
        .find(|t| KNOWN.contains(t))
}

fn cap_group(c: &regex::Captures<'_>, i: usize) -> i64 {
    c.get(i)
        .and_then(|m| m.as_str().parse::<i64>().ok())
        .unwrap_or(0)
}

// Bounded, clipped pushes into the summary's detail lists.

fn push_note(s: &mut SemanticSummary, line: &str) {
    if s.notes.len() < CAP {
        s.add_note(clip(line, MAX_LINE));
    }
}

fn push_fail(s: &mut SemanticSummary, line: &str) {
    if s.failures.len() < CAP {
        s.add_failure(clip(line, MAX_LINE));
    }
}

fn push_warn(s: &mut SemanticSummary, line: &str) {
    if s.warnings.len() < CAP {
        s.add_warning(clip(line, MAX_LINE));
    }
}

fn push_path(s: &mut SemanticSummary, p: &str) {
    if !p.is_empty() && s.paths.len() < CAP {
        s.add_path(clip(p, MAX_LINE));
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Section {
    Other,
    Untracked,
    Unmerged,
}

fn status(cx: &CommandContext, s: &mut SemanticSummary) {
    let porcelain = looks_porcelain(cx.stdout)
        || cx
            .argv
            .iter()
            .any(|a| a.starts_with("--porcelain") || a == "-s" || a == "--short");

    let mut branch: Option<String> = None;
    let (mut modified, mut added, mut deleted, mut renamed, mut untracked, mut unmerged) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);

    if porcelain {
        for line in cx.stdout.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("## ") {
                let b = rest.split("...").next().unwrap_or(rest);
                let b = b.split_whitespace().next().unwrap_or(b);
                if !b.is_empty() {
                    branch = Some(b.to_string());
                }
                if let Some(open) = rest.find('[') {
                    if let Some(rel) = rest[open + 1..].find(']') {
                        let ab = rest[open + 1..open + 1 + rel].trim();
                        if !ab.is_empty() {
                            push_note(s, ab);
                        }
                    }
                }
                continue;
            }
            if let Some((x, y)) = porcelain_codes(line) {
                let path_part: String = line.chars().skip(3).collect();
                let path = if x == 'R' || y == 'R' || x == 'C' || y == 'C' {
                    path_part
                        .split(" -> ")
                        .last()
                        .unwrap_or(path_part.as_str())
                        .trim()
                        .to_string()
                } else {
                    path_part.trim().to_string()
                };

                if x == '?' && y == '?' {
                    untracked += 1;
                } else if x == 'U' || y == 'U' || (x == 'D' && y == 'D') || (x == 'A' && y == 'A') {
                    unmerged += 1;
                    push_fail(s, &format!("unmerged: {path}"));
                } else if x == 'R' || y == 'R' {
                    renamed += 1;
                } else if x == 'A' || y == 'A' {
                    added += 1;
                } else if x == 'D' || y == 'D' {
                    deleted += 1;
                } else if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
                    modified += 1;
                } else if x == 'C' || y == 'C' {
                    added += 1;
                }

                push_path(s, &path);
            }
        }
    } else {
        let mut section = Section::Other;
        for line in cx.stdout.lines() {
            if let Some(b) = line.strip_prefix("On branch ") {
                branch = Some(b.trim().to_string());
                continue;
            }
            if line.starts_with("HEAD detached") {
                branch = Some("HEAD (detached)".to_string());
                continue;
            }
            if line.starts_with("Changes to be committed") {
                section = Section::Other;
                continue;
            }
            if line.starts_with("Changes not staged for commit") {
                section = Section::Other;
                continue;
            }
            if line.starts_with("Untracked files") {
                section = Section::Untracked;
                continue;
            }
            if line.starts_with("Unmerged paths") {
                section = Section::Unmerged;
                continue;
            }
            if line.starts_with("Your branch") {
                push_note(s, line.trim());
                continue;
            }
            if let Some(entry) = line.strip_prefix('\t') {
                match section {
                    Section::Untracked => {
                        untracked += 1;
                        push_path(s, entry.trim());
                    }
                    Section::Unmerged => {
                        unmerged += 1;
                        let p = if let Some((_, rest)) = entry.split_once(':') {
                            entry_path(rest)
                        } else {
                            entry_path(entry)
                        };
                        push_fail(s, entry.trim());
                        push_path(s, &p);
                    }
                    Section::Other => {
                        if let Some((prefix, rest)) = entry.split_once(':') {
                            let p = entry_path(rest);
                            match prefix.trim() {
                                "modified" | "typechange" => modified += 1,
                                "new file" | "added" => added += 1,
                                "deleted" => deleted += 1,
                                "renamed" => renamed += 1,
                                "copied" => added += 1,
                                other => {
                                    if other.contains("both")
                                        || other.contains("by us")
                                        || other.contains("by them")
                                    {
                                        unmerged += 1;
                                        push_fail(s, entry.trim());
                                    }
                                }
                            }
                            push_path(s, &p);
                        }
                    }
                }
            }
        }
    }

    if modified > 0 {
        s.set_count("modified", modified);
    }
    if added > 0 {
        s.set_count("added", added);
    }
    if deleted > 0 {
        s.set_count("deleted", deleted);
    }
    if renamed > 0 {
        s.set_count("renamed", renamed);
    }
    if untracked > 0 {
        s.set_count("untracked", untracked);
    }
    if unmerged > 0 {
        s.set_count("unmerged", unmerged);
    }

    if cx.exit_code != 0 {
        s.set_headline("git status failed");
    } else {
        let mut parts: Vec<String> = Vec::new();
        if modified > 0 {
            parts.push(format!("{modified} modified"));
        }
        if added > 0 {
            parts.push(format!("{added} added"));
        }
        if deleted > 0 {
            parts.push(format!("{deleted} deleted"));
        }
        if renamed > 0 {
            parts.push(format!("{renamed} renamed"));
        }
        if untracked > 0 {
            parts.push(format!("{untracked} untracked"));
        }
        if unmerged > 0 {
            parts.push(format!("{unmerged} unmerged"));
        }
        let body = if parts.is_empty() {
            "clean".to_string()
        } else {
            parts.join(", ")
        };
        let head = match &branch {
            Some(b) => format!("branch {b}: {body}"),
            None => format!("status: {body}"),
        };
        s.set_headline(head);
    }
}

/// Decide whether status output is in `--porcelain`/`--short` form by looking
/// at the first non-empty line.
fn looks_porcelain(stdout: &str) -> bool {
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        return line.starts_with("## ") || porcelain_codes(line).is_some();
    }
    false
}

/// Return the two porcelain status codes (X, Y) if `line` is a porcelain entry.
fn porcelain_codes(line: &str) -> Option<(char, char)> {
    let mut it = line.chars();
    let x = it.next()?;
    let y = it.next()?;
    let third = it.next()?;
    if third != ' ' {
        return None;
    }
    const CODES: &str = " MTADRCU?!";
    if CODES.contains(x) && CODES.contains(y) && !(x == ' ' && y == ' ') {
        Some((x, y))
    } else {
        None
    }
}

/// Extract a path from a long-format status entry value, handling renames.
fn entry_path(raw: &str) -> String {
    let t = raw.trim();
    t.split(" -> ").last().unwrap_or(t).trim().to_string()
}

// ---------------------------------------------------------------------------
// diff / diff --stat
// ---------------------------------------------------------------------------

fn diff(cx: &CommandContext, s: &mut SemanticSummary) {
    let unified = cx.stdout.lines().any(|l| l.starts_with("diff --git"));
    let (mut files, mut ins, mut del) = (0i64, 0i64, 0i64);

    if unified {
        for line in cx.stdout.lines() {
            if let Some(c) = DIFF_HEADER_RE.captures(line) {
                files += 1;
                push_path(s, &c[2]);
            } else if line.starts_with("+++") || line.starts_with("---") {
                continue; // file headers, not content
            } else if line.starts_with('+') {
                ins += 1;
            } else if line.starts_with('-') {
                del += 1;
            }
        }
    } else {
        for line in cx.stdout.lines() {
            if let Some(c) = GIT_STAT_RE.captures(line) {
                files = c[1].parse::<i64>().unwrap_or(0);
                ins = cap_group(&c, 2);
                del = cap_group(&c, 3);
            } else if let Some(idx) = line.find(" | ") {
                push_path(s, line[..idx].trim());
            }
        }
    }

    s.set_count("files_changed", files);
    s.set_count("insertions", ins);
    s.set_count("deletions", del);
    s.set_headline(format!("{files} files changed, +{ins} -{del}"));
}

// ---------------------------------------------------------------------------
// log / log --oneline
// ---------------------------------------------------------------------------

fn log(cx: &CommandContext, s: &mut SemanticSummary) {
    let full = cx.stdout.lines().any(|l| l.starts_with("commit "));
    let mut commits = 0i64;

    if full {
        let mut pending: Option<String> = None;
        for line in cx.stdout.lines() {
            if let Some(rest) = line.strip_prefix("commit ") {
                commits += 1;
                let hash: String = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(12)
                    .collect();
                pending = Some(hash);
            } else if pending.is_some() {
                if let Some(subj) = line.strip_prefix("    ") {
                    let subj = subj.trim();
                    if !subj.is_empty() {
                        let h = pending.take().unwrap_or_default();
                        push_note(s, &format!("{h} {subj}"));
                    }
                }
            }
        }
    } else {
        for line in cx.stdout.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            commits += 1;
            push_note(s, t);
        }
    }

    s.set_count("commits", commits);
    let head = if commits == 1 {
        "1 commit".to_string()
    } else {
        format!("{commits} commits")
    };
    s.set_headline(head);
}

// ---------------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------------

fn commit(cx: &CommandContext, s: &mut SemanticSummary) {
    let mut found = false;
    let mut nothing = false;

    for line in cx.all_lines() {
        let t = line.trim();
        if let Some(c) = COMMIT_HEADER_RE.captures(t) {
            found = true;
            s.set_headline(clip(t, MAX_LINE));
            push_note(s, &format!("{} {}", &c[2], &c[3]));
        } else if let Some(c) = GIT_STAT_RE.captures(t) {
            s.set_count("files_changed", c[1].parse::<i64>().unwrap_or(0));
            s.set_count("insertions", cap_group(&c, 2));
            s.set_count("deletions", cap_group(&c, 3));
            push_note(s, t);
        } else if t.contains("nothing to commit") {
            nothing = true;
        }
    }

    if !found {
        let head = if nothing {
            "nothing to commit".to_string()
        } else if cx.exit_code != 0 {
            "git commit failed".to_string()
        } else {
            "committed".to_string()
        };
        s.set_headline(head);
    }
}

// ---------------------------------------------------------------------------
// push / pull / fetch / merge
// ---------------------------------------------------------------------------

fn sync(cx: &CommandContext, s: &mut SemanticSummary, sub: &str) {
    let mut remote: Option<String> = None;
    let mut up_to_date = false;
    let mut conflict = false;
    let mut had_stat = false;
    let (mut files, mut ins, mut del) = (0i64, 0i64, 0i64);

    for line in cx.all_lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let low = t.to_ascii_lowercase();

        if t.contains("CONFLICT")
            || low.contains("automatic merge failed")
            || t.contains("Merge conflict")
        {
            conflict = true;
            push_fail(s, t);
        } else if low.contains("up to date") || low.contains("up-to-date") {
            up_to_date = true;
            push_note(s, t);
        } else if let Some(r) = t.strip_prefix("To ") {
            remote = Some(r.trim().to_string());
            push_note(s, t);
        } else if let Some(r) = t.strip_prefix("From ") {
            remote = Some(r.trim().to_string());
            push_note(s, t);
        } else if SYNC_UPDATE_RE.is_match(t)
            || t.contains("[new branch]")
            || t.contains("[new tag]")
        {
            push_note(s, t);
        } else if let Some(c) = GIT_STAT_RE.captures(t) {
            files = c[1].parse::<i64>().unwrap_or(0);
            ins = cap_group(&c, 2);
            del = cap_group(&c, 3);
            had_stat = true;
            push_note(s, t);
        } else if t == "Fast-forward"
            || t.starts_with("Merge made")
            || t.starts_with("Updating ")
            || t.starts_with("Switched")
            || t.starts_with("Your branch")
        {
            push_note(s, t);
        }
    }

    if had_stat {
        s.set_count("files_changed", files);
        s.set_count("insertions", ins);
        s.set_count("deletions", del);
    }

    let head = if conflict {
        format!("{sub}: merge conflict")
    } else if up_to_date {
        format!("{sub}: already up to date")
    } else {
        let st = if cx.exit_code == 0 { "ok" } else { "failed" };
        match &remote {
            Some(r) => format!("{sub} {st}: {r}"),
            None => format!("{sub} {st}"),
        }
    };
    s.set_headline(head);
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

fn add(cx: &CommandContext, s: &mut SemanticSummary) {
    let n = cx
        .argv
        .iter()
        .position(|a| a == "add")
        .map(|p| {
            cx.argv[p + 1..]
                .iter()
                .filter(|a| !a.starts_with('-'))
                .count()
        })
        .unwrap_or(0);

    if n > 0 {
        s.set_count("pathspecs", n as i64);
    }

    let head = if cx.exit_code != 0 {
        "git add failed".to_string()
    } else if n > 0 {
        format!("staged {n} pathspec(s)")
    } else {
        "staged changes".to_string()
    };
    s.set_headline(head);
}

// ---------------------------------------------------------------------------
// checkout / switch
// ---------------------------------------------------------------------------

fn checkout(cx: &CommandContext, s: &mut SemanticSummary, sub: &str) {
    let mut branch: Option<String> = None;

    for line in cx.all_lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(rest) = t.strip_prefix("Switched to a new branch ") {
            branch = Some(unquote(rest));
            push_note(s, t);
        } else if let Some(rest) = t.strip_prefix("Switched to branch ") {
            branch = Some(unquote(rest));
            push_note(s, t);
        } else if let Some(rest) = t.strip_prefix("Already on ") {
            branch = Some(unquote(rest));
            push_note(s, t);
        } else if t.starts_with("Your branch") {
            push_note(s, t);
        } else if let Some((code, path)) = t.split_once('\t') {
            if code.len() == 1 && "MADRCU".contains(code) {
                push_path(s, path.trim());
            }
        }
    }

    let head = if cx.exit_code != 0 {
        format!("{sub} failed")
    } else {
        match &branch {
            Some(b) => format!("switched to {b}"),
            None => format!("{sub} ok"),
        }
    };
    s.set_headline(head);
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_end_matches('.')
        .trim_matches('\'')
        .to_string()
}

// ---------------------------------------------------------------------------
// fallback + shared error capture
// ---------------------------------------------------------------------------

fn generic_git(cx: &CommandContext, s: &mut SemanticSummary, sub: Option<&str>) {
    s.set_count("stdout_lines", cx.stdout.lines().count() as i64);
    s.set_count("stderr_lines", cx.stderr.lines().count() as i64);
    let code = cx.exit_code;
    let head = match sub {
        Some(sb) if code == 0 => format!("git {sb}: ok"),
        Some(sb) => format!("git {sb}: exit {code}"),
        None if code == 0 => "git: ok".to_string(),
        None => format!("git: exit {code}"),
    };
    s.set_headline(head);
}

/// Capture `fatal:` / `error:` / `warning:` lines (always emitted on stderr).
fn collect_errors(cx: &CommandContext, s: &mut SemanticSummary) {
    for line in cx.stderr.lines() {
        let t = line.trim_start();
        let low = t.to_ascii_lowercase();
        if low.starts_with("fatal:") || low.starts_with("error:") {
            push_fail(s, t.trim());
        } else if low.starts_with("warning:") {
            push_warn(s, t.trim());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx<'a>(
        argv: &'a [String],
        exit: i32,
        stdout: &'a str,
        stderr: &'a str,
    ) -> CommandContext<'a> {
        CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv,
            exit_code: exit,
            stdout,
            stderr,
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn status_long_counts_and_branch() {
        let out = "On branch main\n\
Your branch is up to date with 'origin/main'.\n\
\n\
Changes to be committed:\n\
  (use \"git restore --staged <file>...\" to unstage)\n\
\tnew file:   bar.rs\n\
\n\
Changes not staged for commit:\n\
  (use \"git add <file>...\" to update what will be committed)\n\
\tmodified:   foo.rs\n\
\tmodified:   src/lib.rs\n\
\tdeleted:    old.rs\n\
\n\
Untracked files:\n\
  (use \"git add <file>...\" to include in what will be committed)\n\
\tnewthing.txt\n";
        let a = argv(&["git", "status"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["modified"], 2);
        assert_eq!(s.counts["added"], 1);
        assert_eq!(s.counts["deleted"], 1);
        assert_eq!(s.counts["untracked"], 1);
        assert!(s.headline.contains("branch main"));
        assert!(s.headline.contains("2 modified"));
        assert!(s.paths.iter().any(|p| p == "foo.rs"));
        assert!(s.paths.iter().any(|p| p == "newthing.txt"));
    }

    #[test]
    fn status_porcelain_classifies() {
        let out = " M foo.rs\nA  bar.rs\n?? new.txt\n D gone.rs\nR  old.rs -> new.rs\n";
        let a = argv(&["git", "status", "--porcelain"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["modified"], 1);
        assert_eq!(s.counts["added"], 1);
        assert_eq!(s.counts["deleted"], 1);
        assert_eq!(s.counts["renamed"], 1);
        assert_eq!(s.counts["untracked"], 1);
        assert!(s.paths.iter().any(|p| p == "new.rs"));
        assert!(s.headline.starts_with("status:"));
    }

    #[test]
    fn diff_unified_counts() {
        let out = "diff --git a/foo.rs b/foo.rs\n\
index 83db48f..bf3a2c1 100644\n\
--- a/foo.rs\n\
+++ b/foo.rs\n\
@@ -1,3 +1,4 @@\n\
 fn main() {\n\
-    println!(\"old\");\n\
+    println!(\"new\");\n\
+    println!(\"extra\");\n\
 }\n";
        let a = argv(&["git", "diff"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts["files_changed"], 1);
        assert_eq!(s.counts["insertions"], 2);
        assert_eq!(s.counts["deletions"], 1);
        assert!(s.paths.iter().any(|p| p == "foo.rs"));
        assert_eq!(s.headline, "1 files changed, +2 -1");
    }

    #[test]
    fn diff_stat_summary() {
        let out =
            " foo.rs |  5 +++--\n bar.rs |  2 +-\n 2 files changed, 4 insertions(+), 3 deletions(-)\n";
        let a = argv(&["git", "diff", "--stat"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts["files_changed"], 2);
        assert_eq!(s.counts["insertions"], 4);
        assert_eq!(s.counts["deletions"], 3);
        assert!(s.paths.iter().any(|p| p == "foo.rs"));
        assert!(s.paths.iter().any(|p| p == "bar.rs"));
        assert_eq!(s.headline, "2 files changed, +4 -3");
    }

    #[test]
    fn log_oneline_counts_commits() {
        let out = "1a2b3c4 Fix the parser bug\n5d6e7f8 Add new feature\n90abcde Initial commit\n";
        let a = argv(&["git", "log", "--oneline"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts["commits"], 3);
        assert_eq!(s.notes.len(), 3);
        assert!(s.notes[0].contains("Fix the parser bug"));
        assert_eq!(s.headline, "3 commits");
    }

    #[test]
    fn log_full_extracts_subjects() {
        // NB: literal "\n    " (newline + 4 spaces) for the indented commit
        // subjects; a "\<newline>" continuation would strip that indentation,
        // which is exactly what git emits and the parser keys on.
        let out = "commit 1a2b3c4d5e6f7a8b9c0d (HEAD -> main)\n\
Author: Jane <jane@example.com>\n\
Date:   Mon Jun 1 10:00:00 2026 +0000\n\
\n    Fix the parser bug\n\
\ncommit 5d6e7f8a9b0c1d2e3f4a\n\
Author: Jane <jane@example.com>\n\
Date:   Mon May 30 09:00:00 2026 +0000\n\
\n    Add new feature\n";
        let a = argv(&["git", "log"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts["commits"], 2);
        assert!(s.notes.iter().any(|n| n.contains("Fix the parser bug")));
        assert!(s.notes.iter().any(|n| n.starts_with("1a2b3c4d5e6f")));
    }

    #[test]
    fn commit_extracts_bracket_and_stat() {
        let out = "[main 1a2b3c4] Add parser module\n\
 3 files changed, 42 insertions(+), 5 deletions(-)\n\
 create mode 100644 src/parser.rs\n";
        let a = argv(&["git", "commit", "-m", "Add parser module"]);
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["files_changed"], 3);
        assert_eq!(s.counts["insertions"], 42);
        assert_eq!(s.counts["deletions"], 5);
        assert!(s.headline.contains("Add parser module"));
    }

    #[test]
    fn push_success_notes_remote() {
        let err = "Enumerating objects: 5, done.\n\
Counting objects: 100% (5/5), done.\n\
Writing objects: 100% (3/3), 290 bytes | 290.00 KiB/s, done.\n\
Total 3 (delta 2), reused 0 (delta 0)\n\
To github.com:user/repo.git\n\
   1a2b3c4..5d6e7f8  main -> main\n";
        let a = argv(&["git", "push"]);
        let s = summarize(&cx(&a, 0, "", err));
        assert_eq!(s.status, "ok");
        assert!(s.headline.contains("push"));
        assert!(s.notes.iter().any(|n| n.contains("main -> main")));
        assert!(s.failures.is_empty());
    }

    #[test]
    fn pull_conflict_is_failure() {
        let out = "Auto-merging foo.rs\n\
CONFLICT (content): Merge conflict in foo.rs\n\
Automatic merge failed; fix conflicts and then commit the result.\n";
        let a = argv(&["git", "pull"]);
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("CONFLICT")));
        assert!(s.headline.contains("conflict"));
    }

    #[test]
    fn checkout_new_branch() {
        let err = "Switched to a new branch 'feature/login'\n";
        let a = argv(&["git", "checkout", "-b", "feature/login"]);
        let s = summarize(&cx(&a, 0, "", err));
        assert_eq!(s.status, "ok");
        assert!(s.headline.contains("feature/login"));
    }

    #[test]
    fn status_fatal_captured() {
        let err = "fatal: not a git repository (or any of the parent directories): .git\n";
        let a = argv(&["git", "status"]);
        let s = summarize(&cx(&a, 128, "", err));
        assert_eq!(s.status, "failed");
        assert!(s
            .failures
            .iter()
            .any(|f| f.contains("not a git repository")));
        assert_eq!(s.headline, "git status failed");
    }

    #[test]
    fn add_counts_pathspecs() {
        let a = argv(&["git", "add", "src/main.rs", "src/lib.rs"]);
        let s = summarize(&cx(&a, 0, "", ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["pathspecs"], 2);
        assert!(s.headline.contains("2 pathspec"));
    }

    #[test]
    fn handles_git_dash_c_position() {
        // `git -C <dir> status` must still resolve the status subcommand.
        let a = argv(&["git", "-C", "/repo", "status", "--porcelain"]);
        let s = summarize(&cx(&a, 0, " M foo.rs\n", ""));
        assert_eq!(s.counts["modified"], 1);
        assert_eq!(s.family, "git status");
    }
}
