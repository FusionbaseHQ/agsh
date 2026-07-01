//! Semantic compactor for the `search` family: line-oriented grep-style tools
//! (`grep`/`egrep`/`fgrep`, `rg`/`ripgrep`, `ag`, `ack`).
//!
//! These tools emit one match per line, usually prefixed with the file that
//! contained it (`path:line:text`, `path:text`, or `path:count` with
//! `--count`). With `-h`/`--no-filename`, or when a single file is searched,
//! the prefix is absent and the raw matching line is printed.
//!
//! The compactor distills that into how many matches were found, how many
//! distinct files they came from, a bounded sample of those files, and a few
//! sample matching lines. Exit-code semantics are honoured carefully: for these
//! tools exit code 1 means "no matches" (a normal, successful outcome), while
//! exit code >= 2 (or exit 1 accompanied by an error on stderr) is a genuine
//! failure.

use std::collections::{BTreeMap, HashSet};

use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};

/// Maximum width of any single sample/detail line we keep.
const MAX_LINE: usize = 200;
/// Maximum number of distinct files sampled into `paths`.
const FILE_SAMPLE: usize = 20;
/// Maximum number of sample matching lines kept in `notes`.
const NOTE_SAMPLE: usize = 5;
/// Maximum number of stderr diagnostic lines collected.
const MAX_DIAGS: usize = 50;

/// Substrings (lower-cased) that mark an stderr line as a real diagnostic.
const ERROR_NEEDLES: &[&str] = &[
    "error",
    "no such file",
    "permission denied",
    "cannot",
    "not found",
    "is a directory",
    "invalid",
    "unrecognized",
];

/// Relevant command-line flags affecting how output is shaped.
#[derive(Default, Clone, Copy)]
struct Flags {
    /// `-c` / `--count` / `--count-matches`: lines are `path:count` or `count`.
    count: bool,
    /// `-l` / `-L` / `--files-with-matches`: output is a plain list of files.
    files_only: bool,
    /// `-h` (grep) / `-I` (rg) / `--no-filename`: no path prefix on lines.
    no_filename: bool,
    /// `-q` / `--quiet` / `--silent`: output suppressed, exit code is the signal.
    quiet: bool,
}

pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let prog = command_basename(cx.argv);
    let is_rg = matches!(prog, "rg" | "ripgrep" | "ag");
    let family = if is_rg { "rg" } else { "grep" };
    let mut summary = SemanticSummary::new(cx, family);

    // Collect any error-like diagnostics from stderr up front.
    let diags = collect_diagnostics(cx.stderr);
    let has_stderr_error = !diags.is_empty();

    // --- Failure path -------------------------------------------------------
    // exit >= 2 is always an error; exit 1 with stderr noise is too.
    if cx.exit_code >= 2 || (cx.exit_code == 1 && has_stderr_error) {
        summary.status = "failed".to_string();
        for d in &diags {
            summary.add_failure(d.clone());
        }
        if summary.failures.is_empty() {
            summary.add_failure(format!("{prog} exited with code {}", cx.exit_code));
        }
        summary
            .set_count("matches", 0)
            .set_headline(format!("search error (exit {})", cx.exit_code));
        return summary;
    }

    // --- No matches ---------------------------------------------------------
    // exit 1 with a clean stderr is the conventional "nothing matched".
    if cx.exit_code == 1 {
        summary.status = "ok".to_string();
        summary
            .set_count("matches", 0)
            .set_count("files", 0)
            .set_headline("no matches");
        return summary;
    }

    // --- Matches found (exit 0) --------------------------------------------
    let flags = parse_flags(cx.argv, is_rg);

    // Non-fatal stderr lines (e.g. ripgrep permission warnings) are surfaced
    // as warnings since the command still succeeded overall.
    for d in &diags {
        summary.add_warning(d.clone());
    }

    // `-q` suppresses output; a zero exit simply means a match was found.
    if flags.quiet {
        summary.set_headline("match found");
        return summary;
    }

    let mut total_matches: i64 = 0;
    let mut files: HashSet<String> = HashSet::new();
    let mut file_sample: Vec<String> = Vec::new();
    let mut note_sample: Vec<String> = Vec::new();
    // Per-file grouping (rtk-style): file -> (match count, first snippet).
    let mut per_file: BTreeMap<String, (i64, Option<String>)> = BTreeMap::new();

    for raw in cx.stdout.lines() {
        if raw.is_empty() || raw == "--" {
            continue;
        }

        if flags.files_only {
            let path = raw.trim();
            if !path.is_empty() {
                record_file(path, &mut files, &mut file_sample);
            }
            continue;
        }

        if flags.count {
            if let Some((path, n)) = parse_count_line(raw) {
                total_matches += n;
                if let Some(p) = path {
                    record_file(&p, &mut files, &mut file_sample);
                    per_file.entry(p).or_insert((0, None)).0 += n;
                }
            }
            continue;
        }

        // Normal match line.
        total_matches += 1;
        let (path, text) = parse_match_line(raw, flags.no_filename);
        let snippet = {
            let t = text.trim();
            (!t.is_empty()).then(|| clip(t, MAX_LINE))
        };
        if let Some(p) = path {
            record_file(&p, &mut files, &mut file_sample);
            let entry = per_file.entry(p).or_insert((0, None));
            entry.0 += 1;
            if entry.1.is_none() {
                entry.1 = snippet.clone();
            }
        } else if note_sample.len() < NOTE_SAMPLE {
            // No path (single-file / -h): keep a flat sample of matching lines.
            if let Some(s) = snippet {
                note_sample.push(s);
            }
        }
    }

    // Build per-file grouped notes: the busiest files first, capped.
    if !per_file.is_empty() {
        let mut grouped: Vec<(String, i64, Option<String>)> =
            per_file.into_iter().map(|(f, (c, s))| (f, c, s)).collect();
        grouped.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (file, count, snippet) in grouped.into_iter().take(NOTE_SAMPLE) {
            let note = match snippet {
                Some(s) => format!("{file} ({count}): {s}"),
                None => format!("{file} ({count})"),
            };
            note_sample.push(clip(&note, MAX_LINE));
        }
    }

    let file_count = files.len() as i64;

    if flags.files_only {
        summary
            .set_count("files", file_count)
            .set_headline(format!("{file_count} files with matches"));
    } else {
        summary.set_count("matches", total_matches);
        if file_count > 0 {
            summary
                .set_count("files", file_count)
                .set_headline(format!("{total_matches} matches in {file_count} files"));
        } else {
            summary.set_headline(format!("{total_matches} matches"));
        }
    }

    for p in file_sample {
        summary.add_path(clip(&p, MAX_LINE));
    }
    for n in note_sample {
        summary.add_note(n);
    }

    summary
}

/// Insert a file into the dedup set, keeping a bounded ordered sample.
fn record_file(path: &str, files: &mut HashSet<String>, sample: &mut Vec<String>) {
    if files.insert(path.to_string()) && sample.len() < FILE_SAMPLE {
        sample.push(path.to_string());
    }
}

/// Pull error-like lines out of stderr, clipped and bounded.
fn collect_diagnostics(stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        if out.len() >= MAX_DIAGS {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if ERROR_NEEDLES.iter().any(|n| lower.contains(n)) {
            out.push(clip(trimmed, MAX_LINE));
        }
    }
    out
}

/// Parse relevant flags from argv. `is_rg` selects the no-filename short flag
/// (`-I` for ripgrep, `-h` for grep, where `-h` is help in ripgrep).
fn parse_flags(argv: &[String], is_rg: bool) -> Flags {
    let mut f = Flags::default();
    let mut positional_only = false;
    for arg in argv.iter().skip(1) {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            match name {
                "count" | "count-matches" => f.count = true,
                "files-with-matches" | "files-without-match" => f.files_only = true,
                "no-filename" | "nofilename" => f.no_filename = true,
                "quiet" | "silent" => f.quiet = true,
                _ => {}
            }
        } else if let Some(short) = arg.strip_prefix('-') {
            if short.is_empty() {
                continue; // a lone "-" operand
            }
            for ch in short.chars() {
                match ch {
                    'c' => f.count = true,
                    'l' | 'L' => f.files_only = true,
                    'q' => f.quiet = true,
                    'h' if !is_rg => f.no_filename = true,
                    'I' if is_rg => f.no_filename = true,
                    _ => {}
                }
            }
        }
        // Bare operands (pattern, files) are ignored.
    }
    f
}

/// Parse a `--count` line: `path:count` or a bare `count`.
fn parse_count_line(line: &str) -> Option<(Option<String>, i64)> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(idx) = t.rfind(':') {
        let (left, right) = (&t[..idx], &t[idx + 1..]);
        if !left.is_empty() && is_all_digits(right) {
            return Some((Some(left.to_string()), right.parse().unwrap_or(0)));
        }
    }
    if is_all_digits(t) {
        return Some((None, t.parse().unwrap_or(0)));
    }
    None
}

/// Parse a normal match line into an optional file path and the matched text.
///
/// Recognises `path:line:text` (the common `-n`/ripgrep form) and `path:text`.
/// A leading all-numeric field is treated as a line number, not a path, so
/// single-file `grep -n` output is not mistaken for a filename.
fn parse_match_line(line: &str, no_filename: bool) -> (Option<String>, String) {
    if !no_filename {
        if let Some(rest) = line.strip_prefix("Binary file ") {
            if let Some(path) = rest.strip_suffix(" matches") {
                if !path.is_empty() {
                    return (Some(path.to_string()), line.to_string());
                }
            }
        }
    }
    if no_filename {
        return (None, line.to_string());
    }

    if let Some(c1) = line.find(':') {
        let head = &line[..c1];
        let rest = &line[c1 + 1..];
        if !head.is_empty() && !is_all_digits(head) {
            // path:line:text
            if let Some(c2) = rest.find(':') {
                let mid = &rest[..c2];
                if is_all_digits(mid) {
                    return (Some(head.to_string()), rest[c2 + 1..].to_string());
                }
            }
            // path:text — only when the head looks like a file path.
            if !head.contains(char::is_whitespace) && (head.contains('/') || head.contains('.')) {
                return (Some(head.to_string()), rest.to_string());
            }
        }
    }
    (None, line.to_string())
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

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

    #[test]
    fn rg_recursive_matches() {
        let a = argv(&["rg", "needle"]);
        let out = "src/main.rs:10:    let needle = find();\n\
                   src/main.rs:42:    needle.process();\n\
                   src/lib.rs:7:// needle helper\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.family, "rg");
        assert_eq!(s.counts.get("matches"), Some(&3));
        assert_eq!(s.counts.get("files"), Some(&2));
        assert_eq!(s.headline, "3 matches in 2 files");
        assert!(s.paths.iter().any(|p| p == "src/main.rs"));
        assert!(s.paths.iter().any(|p| p == "src/lib.rs"));
        assert!(s.notes.iter().any(|n| n.contains("needle")));
    }

    #[test]
    fn groups_matches_per_file_with_counts() {
        let a = argv(&["rg", "needle"]);
        let out = "src/main.rs:10:let needle = find();\n\
                   src/main.rs:42:needle.go();\n\
                   src/lib.rs:7:// needle helper\n";
        let s = summarize(&cx(&a, 0, out, ""));
        // The busiest file appears first with its match count and a snippet.
        assert!(
            s.notes.iter().any(|n| n.starts_with("src/main.rs (2):")),
            "{:?}",
            s.notes
        );
        assert!(s.notes.iter().any(|n| n.starts_with("src/lib.rs (1):")));
    }

    #[test]
    fn grep_count_mode_sums_counts() {
        let a = argv(&["grep", "-rc", "needle", "."]);
        let s = summarize(&cx(&a, 0, "src/a.rs:3\nsrc/b.rs:5\n", ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.family, "grep");
        assert_eq!(s.counts.get("matches"), Some(&8));
        assert_eq!(s.counts.get("files"), Some(&2));
        assert_eq!(s.headline, "8 matches in 2 files");
    }

    #[test]
    fn files_only_mode_lists_files() {
        let a = argv(&["rg", "-l", "needle"]);
        let s = summarize(&cx(&a, 0, "src/a.rs\nsrc/b.rs\nsrc/c.rs\n", ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("files"), Some(&3));
        assert!(!s.counts.contains_key("matches"));
        assert_eq!(s.headline, "3 files with matches");
        assert_eq!(s.paths.len(), 3);
    }

    #[test]
    fn no_matches_is_ok() {
        let a = argv(&["grep", "-rn", "needle", "."]);
        let s = summarize(&cx(&a, 1, "", ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.headline, "no matches");
        assert_eq!(s.counts.get("matches"), Some(&0));
        assert_eq!(s.counts.get("files"), Some(&0));
        assert!(s.failures.is_empty());
    }

    #[test]
    fn exit_two_is_failure() {
        let a = argv(&["grep", "needle", "missing.txt"]);
        let s = summarize(&cx(
            &a,
            2,
            "",
            "grep: missing.txt: No such file or directory\n",
        ));
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("No such file")));
        assert!(s.headline.contains("error"));
        assert_eq!(s.counts.get("matches"), Some(&0));
    }

    #[test]
    fn exit_one_with_stderr_error_is_failure() {
        let a = argv(&["grep", "-r", "needle", "noperm/"]);
        let s = summarize(&cx(&a, 1, "", "grep: noperm/secret: Permission denied\n"));
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("Permission denied")));
    }

    #[test]
    fn single_file_line_numbers_have_no_path() {
        // grep -n on one file: leading field is the line number, not a path.
        let a = argv(&["grep", "-n", "needle", "main.rs"]);
        let out = "10:    let needle = 1;\n20:    needle();\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("matches"), Some(&2));
        assert!(!s.counts.contains_key("files"));
        assert_eq!(s.headline, "2 matches");
        assert!(s.paths.is_empty());
    }

    #[test]
    fn no_filename_flag_yields_bare_matches() {
        let a = argv(&["grep", "-rh", "needle", "."]);
        let out = "    let needle = 1;\n    needle();\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts.get("matches"), Some(&2));
        assert!(s.paths.is_empty());
        assert_eq!(s.headline, "2 matches");
    }

    #[test]
    fn exit_zero_with_stderr_warning() {
        // ripgrep can hit an unreadable file yet still succeed elsewhere.
        let a = argv(&["rg", "needle"]);
        let out = "src/a.rs:1:needle\n";
        let s = summarize(&cx(
            &a,
            0,
            out,
            "rg: ./locked: Permission denied (os error 13)\n",
        ));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("matches"), Some(&1));
        assert!(s.warnings.iter().any(|w| w.contains("Permission denied")));
    }
}
