//! Compilers/linters family compactor.
//!
//! Parses build and lint diagnostics into structured errors (failures) and
//! warnings. The concrete tool is detected from `argv` so the right diagnostic
//! grammar is applied and a precise `family` label is recorded
//! (`"gcc"`, `"clang"`, `"cargo-build"`, `"tsc"`, `"mypy"`, `"ruff"`,
//! `"eslint"`, else `"compilers"`).
//!
//! Supported formats:
//! - gcc/clang: `file:line:col: error|warning: msg` (plus driver-level
//!   `tool: error: msg` lines without a location).
//! - cargo build/clippy: `error[E1234]: msg` / `error: msg` / `warning: msg`,
//!   with `--> file:line:col` location lines attached as paths. Aggregate
//!   summary lines (`aborting due to …`, `… generated N warnings`) are skipped.
//! - tsc: `file(line,col): error|warning TS1234: msg`.
//! - mypy: `file:line[:col]: error|warning: msg` and a trailing
//!   `Found N errors …` summary (authoritative for the error count).
//! - ruff: `file:line:col: CODE msg` (`W…` codes are warnings, else errors).
//! - eslint: the default "stylish" report (a bare file header followed by
//!   indented `line:col  severity  message  rule` rows and a
//!   `✖ N problems (X errors, Y warnings)` summary).

use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};

/// Maximum characters kept per detail line.
const MAX_LINE: usize = 200;
/// Maximum detail entries collected per section before the framework caps it.
const MAX_DETAIL: usize = 50;

static_regex!(
    CC_DIAG_RE,
    r"^(.*?):(\d+):(?:(\d+):)?\s*(error|warning|note|fatal error):\s*(.*)$"
);
static_regex!(CC_DRIVER_RE, r"^(?:.+?:\s+)?(error|warning):\s+(.*)$");
static_regex!(CARGO_ERROR_RE, r"^error(\[[^\]]+\])?:\s*(.*)$");
static_regex!(CARGO_WARNING_RE, r"^warning(\[[^\]]+\])?:\s*(.*)$");
static_regex!(CARGO_LOCATION_RE, r"^\s*-->\s*(\S+)");
static_regex!(
    TSC_DIAG_RE,
    r"^(.*?)\((\d+),(\d+)\):\s*(error|warning)\s+TS\d+:\s*(.*)$"
);
static_regex!(FOUND_ERRORS_RE, r"^Found (\d+) error");
static_regex!(
    MYPY_DIAG_RE,
    r"^(.*?):(\d+):(?:(\d+):)?\s*(error|warning|note):\s*(.*)$"
);
static_regex!(
    LINT_DIAG_RE,
    r"^(.*?):(\d+):(\d+):\s+([A-Za-z]+\d+):?\s+(.*)$"
);
static_regex!(ESLINT_DIAG_RE, r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.*)$");
static_regex!(
    ESLINT_SUMMARY_RE,
    r"(\d+)\s+errors?\s*,\s*(\d+)\s+warnings?"
);
static_regex!(STRIP_LINE_COL_RE, r"^(.*?):\d+(?::\d+)?$");

#[derive(Debug, Clone, Copy)]
enum Tool {
    Gcc,
    Clang,
    Cargo,
    Tsc,
    Mypy,
    Ruff,
    Eslint,
    Other,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Gcc => "gcc",
            Tool::Clang => "clang",
            Tool::Cargo => "cargo-build",
            Tool::Tsc => "tsc",
            Tool::Mypy => "mypy",
            Tool::Ruff => "ruff",
            Tool::Eslint => "eslint",
            Tool::Other => "compilers",
        }
    }
}

fn detect_tool(argv: &[String]) -> Tool {
    match command_basename(argv) {
        "gcc" | "g++" | "cc" | "c++" => Tool::Gcc,
        "clang" | "clang++" => Tool::Clang,
        "cargo" => Tool::Cargo,
        "tsc" => Tool::Tsc,
        "mypy" => Tool::Mypy,
        "ruff" => Tool::Ruff,
        "eslint" => Tool::Eslint,
        _ => Tool::Other,
    }
}

pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let tool = detect_tool(cx.argv);
    let mut summary = SemanticSummary::new(cx, "compilers");
    summary.family = tool.label().to_string();

    match tool {
        Tool::Gcc | Tool::Clang => parse_cc(cx, &mut summary),
        Tool::Cargo => parse_cargo(cx, &mut summary),
        Tool::Tsc => parse_tsc(cx, &mut summary),
        Tool::Mypy => parse_mypy(cx, &mut summary),
        Tool::Ruff => parse_ruff(cx, &mut summary),
        Tool::Eslint => parse_eslint(cx, &mut summary),
        Tool::Other => parse_generic(cx, &mut summary),
    }

    finalize(&mut summary);
    summary
}

// ---------------------------------------------------------------------------
// Per-tool parsers
// ---------------------------------------------------------------------------

fn parse_cc(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;

    for line in cx.all_lines() {
        if let Some(c) = CC_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            match &c[4] {
                "warning" => {
                    warnings += 1;
                    push_warning(summary, line);
                }
                "note" => {}
                _ => {
                    errors += 1;
                    push_failure(summary, line);
                }
            }
        } else if let Some(c) = CC_DRIVER_RE.captures(line) {
            match &c[1] {
                "warning" => {
                    warnings += 1;
                    push_warning(summary, line);
                }
                _ => {
                    errors += 1;
                    push_failure(summary, line);
                }
            }
        }
    }

    summary.set_count("errors", errors);
    summary.set_count("warnings", warnings);
}

fn parse_cargo(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;

    for line in cx.all_lines() {
        if let Some(c) = CARGO_LOCATION_RE.captures(line) {
            let file = strip_line_col(&c[1]);
            push_path(summary, &file);
            continue;
        }
        if let Some(c) = CARGO_ERROR_RE.captures(line) {
            if !is_aggregate_error(&c[2]) {
                errors += 1;
                push_failure(summary, line);
            }
            continue;
        }
        if let Some(c) = CARGO_WARNING_RE.captures(line) {
            if !is_aggregate_warning(&c[2]) {
                warnings += 1;
                push_warning(summary, line);
            }
        }
    }

    summary.set_count("errors", errors);
    summary.set_count("warnings", warnings);
}

fn parse_tsc(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;

    for line in cx.all_lines() {
        if let Some(c) = TSC_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            if &c[4] == "warning" {
                warnings += 1;
                push_warning(summary, line);
            } else {
                errors += 1;
                push_failure(summary, line);
            }
        } else if FOUND_ERRORS_RE.is_match(line) && summary.notes.len() < MAX_DETAIL {
            summary.add_note(clip(line, MAX_LINE));
        }
    }

    summary.set_count("errors", errors);
    summary.set_count("warnings", warnings);
}

fn parse_mypy(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;
    let mut found: Option<i64> = None;

    for line in cx.all_lines() {
        if let Some(c) = MYPY_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            match &c[4] {
                "warning" => {
                    warnings += 1;
                    push_warning(summary, line);
                }
                "note" => {}
                _ => {
                    errors += 1;
                    push_failure(summary, line);
                }
            }
        } else if let Some(c) = FOUND_ERRORS_RE.captures(line) {
            found = c[1].parse::<i64>().ok();
            if summary.notes.len() < MAX_DETAIL {
                summary.add_note(clip(line, MAX_LINE));
            }
        }
    }

    summary.set_count("errors", found.unwrap_or(errors));
    summary.set_count("warnings", warnings);
}

fn parse_ruff(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;

    for line in cx.all_lines() {
        if let Some(c) = LINT_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            if c[4].starts_with('W') {
                warnings += 1;
                push_warning(summary, line);
            } else {
                errors += 1;
                push_failure(summary, line);
            }
        } else if FOUND_ERRORS_RE.is_match(line) && summary.notes.len() < MAX_DETAIL {
            summary.add_note(clip(line, MAX_LINE));
        }
    }

    summary.set_count("errors", errors);
    summary.set_count("warnings", warnings);
}

fn parse_eslint(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;
    let mut totals: Option<(i64, i64)> = None;
    let mut current: Option<String> = None;

    for line in cx.all_lines() {
        if let Some(c) = ESLINT_DIAG_RE.captures(line) {
            let detail = match &current {
                Some(file) => format!("{}:{}:{} {}", file, &c[1], &c[2], c[4].trim_end()),
                None => line.trim().to_string(),
            };
            if &c[3] == "warning" {
                warnings += 1;
                push_warning(summary, &detail);
            } else {
                errors += 1;
                push_failure(summary, &detail);
            }
            continue;
        }
        if let Some(c) = ESLINT_SUMMARY_RE.captures(line) {
            let e = c[1].parse::<i64>().unwrap_or(0);
            let w = c[2].parse::<i64>().unwrap_or(0);
            totals = Some((e, w));
            continue;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && !line.starts_with(|ch: char| ch.is_whitespace())
            && looks_like_path(trimmed)
        {
            current = Some(trimmed.to_string());
            push_path(summary, trimmed);
        }
    }

    let (e, w) = totals.unwrap_or((errors, warnings));
    summary.set_count("errors", e);
    summary.set_count("warnings", w);
}

/// Best-effort parser for other compilers/linters (flake8, pylint, …) that
/// emit either gcc-style `error:`/`warning:` lines or `file:line:col: CODE`
/// lint diagnostics.
fn parse_generic(cx: &CommandContext, summary: &mut SemanticSummary) {
    let mut errors = 0i64;
    let mut warnings = 0i64;

    for line in cx.all_lines() {
        if let Some(c) = CC_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            match &c[4] {
                "warning" => {
                    warnings += 1;
                    push_warning(summary, line);
                }
                "note" => {}
                _ => {
                    errors += 1;
                    push_failure(summary, line);
                }
            }
        } else if let Some(c) = LINT_DIAG_RE.captures(line) {
            let file = c[1].trim();
            if !file.is_empty() {
                push_path(summary, file);
            }
            if c[4].starts_with('W') {
                warnings += 1;
                push_warning(summary, line);
            } else {
                errors += 1;
                push_failure(summary, line);
            }
        }
    }

    summary.set_count("errors", errors);
    summary.set_count("warnings", warnings);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn finalize(summary: &mut SemanticSummary) {
    let errors = summary.counts.get("errors").copied().unwrap_or(0);
    let warnings = summary.counts.get("warnings").copied().unwrap_or(0);
    summary.set_headline(format!(
        "{}, {}",
        pluralize(errors, "error"),
        pluralize(warnings, "warning")
    ));
}

fn pluralize(n: i64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Strip a trailing `:line` or `:line:col` suffix from a location token.
fn strip_line_col(loc: &str) -> String {
    match STRIP_LINE_COL_RE.captures(loc) {
        Some(c) => c[1].to_string(),
        None => loc.to_string(),
    }
}

/// Cargo/rustc aggregate error lines that should not be counted as diagnostics.
fn is_aggregate_error(msg: &str) -> bool {
    msg.starts_with("aborting due to") || msg.starts_with("could not compile")
}

/// Cargo/rustc aggregate warning lines that should not be counted.
fn is_aggregate_warning(msg: &str) -> bool {
    (msg.contains("generated") && msg.contains("warning"))
        || msg.ends_with("warning emitted")
        || msg.ends_with("warnings emitted")
}

fn looks_like_path(s: &str) -> bool {
    const EXTS: &[&str] = &[
        ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".cts", ".mts", ".vue",
    ];
    s.contains('/') || s.contains('\\') || EXTS.iter().any(|&e| s.ends_with(e))
}

fn push_failure(summary: &mut SemanticSummary, line: &str) {
    if summary.failures.len() < MAX_DETAIL {
        summary.add_failure(clip(line, MAX_LINE));
    }
}

fn push_warning(summary: &mut SemanticSummary, line: &str) {
    if summary.warnings.len() < MAX_DETAIL {
        summary.add_warning(clip(line, MAX_LINE));
    }
}

fn push_path(summary: &mut SemanticSummary, path: &str) {
    if summary.paths.len() < MAX_DETAIL {
        summary.add_path(path.to_string());
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
    fn parses_gcc_diagnostics() {
        let a = argv(&["gcc", "-c", "main.c"]);
        let stderr = "main.c: In function 'main':\n\
main.c:10:5: error: 'x' undeclared (first use in this function)\n\
main.c:10:5: note: each undeclared identifier is reported only once for each function it appears in\n\
main.c:12:9: warning: unused variable 'y' [-Wunused-variable]\n";
        let s = summarize(&cx(&a, 1, "", stderr));
        assert_eq!(s.family, "gcc");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts.get("errors"), Some(&1));
        assert_eq!(s.counts.get("warnings"), Some(&1));
        assert!(s.failures.iter().any(|f| f.contains("undeclared")));
        assert!(s.warnings.iter().any(|w| w.contains("unused variable")));
        assert!(s.paths.iter().any(|p| p == "main.c"));
        assert_eq!(s.headline, "1 error, 1 warning");
    }

    #[test]
    fn parses_clang_with_driver_error() {
        let a = argv(&["clang++", "-std=c++17", "foo.cpp"]);
        let stderr = "foo.cpp:5:10: error: use of undeclared identifier 'bar'\n\
foo.cpp:8:3: warning: unused variable 'z' [-Wunused-variable]\n\
1 error generated.\n";
        let s = summarize(&cx(&a, 1, "", stderr));
        assert_eq!(s.family, "clang");
        // "1 error generated." has no colon after "error" -> not double counted.
        assert_eq!(s.counts.get("errors"), Some(&1));
        assert_eq!(s.counts.get("warnings"), Some(&1));
    }

    #[test]
    fn parses_cargo_build() {
        let a = argv(&["cargo", "build"]);
        let stderr = "   Compiling demo v0.1.0 (/tmp/demo)\n\
error[E0425]: cannot find value `x` in this scope\n\
 --> src/main.rs:3:13\n\
  |\n\
3 |     println!(\"{}\", x);\n\
  |                    ^ not found in this scope\n\
\n\
warning: unused variable: `y`\n\
 --> src/main.rs:2:9\n\
\n\
error: aborting due to previous error; 1 warning emitted\n";
        let s = summarize(&cx(&a, 101, "", stderr));
        assert_eq!(s.family, "cargo-build");
        assert_eq!(s.status, "failed");
        // The "aborting due to" line must not inflate the error count.
        assert_eq!(s.counts.get("errors"), Some(&1));
        assert_eq!(s.counts.get("warnings"), Some(&1));
        assert!(s.failures.iter().any(|f| f.contains("cannot find value")));
        assert!(s.paths.iter().any(|p| p == "src/main.rs"));
        // Both --> lines reference the same file, so it appears once.
        assert_eq!(s.paths.iter().filter(|p| *p == "src/main.rs").count(), 1);
    }

    #[test]
    fn parses_tsc() {
        let a = argv(&["tsc", "--noEmit"]);
        let stdout = "src/index.ts(10,5): error TS2304: Cannot find name 'foo'.\n\
src/index.ts(12,3): error TS6133: 'x' is declared but its value is never read.\n\
\n\
Found 2 errors in 1 file.\n";
        let s = summarize(&cx(&a, 2, stdout, ""));
        assert_eq!(s.family, "tsc");
        assert_eq!(s.counts.get("errors"), Some(&2));
        assert_eq!(s.counts.get("warnings"), Some(&0));
        assert!(s.paths.iter().any(|p| p == "src/index.ts"));
        assert!(s.notes.iter().any(|n| n.contains("Found 2 errors")));
        assert_eq!(s.headline, "2 errors, 0 warnings");
    }

    #[test]
    fn parses_mypy_with_found_summary() {
        let a = argv(&["mypy", "pkg"]);
        let stdout = "foo.py:10: error: Incompatible return value type (got \"int\", expected \"str\")  [return-value]\n\
foo.py:14: error: Name \"bar\" is not defined  [name-defined]\n\
foo.py:16: note: See https://example.invalid for more info\n\
Found 2 errors in 1 file (checked 3 source files)\n";
        let s = summarize(&cx(&a, 1, stdout, ""));
        assert_eq!(s.family, "mypy");
        assert_eq!(s.counts.get("errors"), Some(&2));
        assert_eq!(s.counts.get("warnings"), Some(&0));
        assert!(s.failures.iter().any(|f| f.contains("not defined")));
        assert!(s.paths.iter().any(|p| p == "foo.py"));
        assert!(s.notes.iter().any(|n| n.contains("Found 2 errors")));
    }

    #[test]
    fn parses_ruff() {
        let a = argv(&["ruff", "check", "."]);
        let stdout = "app.py:1:1: F401 `os` imported but unused\n\
app.py:3:5: E225 missing whitespace around operator\n\
app.py:5:80: W291 trailing whitespace\n\
Found 3 errors.\n";
        let s = summarize(&cx(&a, 1, stdout, ""));
        assert_eq!(s.family, "ruff");
        // F401 and E225 are errors; W291 is a warning.
        assert_eq!(s.counts.get("errors"), Some(&2));
        assert_eq!(s.counts.get("warnings"), Some(&1));
        assert!(s.warnings.iter().any(|w| w.contains("W291")));
        assert!(s.paths.iter().any(|p| p == "app.py"));
        assert_eq!(s.headline, "2 errors, 1 warning");
    }

    #[test]
    fn parses_eslint_stylish() {
        let a = argv(&["eslint", "src"]);
        // NB: the diagnostic rows must keep their leading indentation (eslint
        // emits two spaces); embedding it after "\n" avoids the "\<newline>"
        // continuation that would strip it and stop the parser matching rows.
        let stdout = "/home/user/app.js\n\
\u{20}\u{20}1:10  error    'foo' is not defined          no-undef\n\
\u{20}\u{20}2:1   warning  Unexpected console statement  no-console\n\
\n\
\u{2716} 2 problems (1 error, 1 warning)\n";
        let s = summarize(&cx(&a, 1, stdout, ""));
        assert_eq!(s.family, "eslint");
        assert_eq!(s.counts.get("errors"), Some(&1));
        assert_eq!(s.counts.get("warnings"), Some(&1));
        assert!(s
            .failures
            .iter()
            .any(|f| f.contains("'foo' is not defined")));
        assert!(s.failures.iter().any(|f| f.contains("/home/user/app.js")));
        assert!(s.paths.iter().any(|p| p == "/home/user/app.js"));
        assert_eq!(s.headline, "1 error, 1 warning");
    }

    #[test]
    fn parses_generic_flake8() {
        let a = argv(&["flake8", "."]);
        let stdout = "module.py:1:1: F401 'sys' imported but unused\n\
module.py:2:80: E501 line too long (88 > 79 characters)\n\
module.py:3:1: W391 blank line at end of file\n";
        let s = summarize(&cx(&a, 1, stdout, ""));
        // flake8 is not in the named set, so the label stays "compilers".
        assert_eq!(s.family, "compilers");
        assert_eq!(s.counts.get("errors"), Some(&2));
        assert_eq!(s.counts.get("warnings"), Some(&1));
        assert!(s.paths.iter().any(|p| p == "module.py"));
    }

    #[test]
    fn clean_build_reports_zero() {
        let a = argv(&["cargo", "build"]);
        let s = summarize(&cx(
            &a,
            0,
            "",
            "    Finished dev [unoptimized] target(s) in 0.12s\n",
        ));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("errors"), Some(&0));
        assert_eq!(s.counts.get("warnings"), Some(&0));
        assert_eq!(s.headline, "0 errors, 0 warnings");
    }
}
