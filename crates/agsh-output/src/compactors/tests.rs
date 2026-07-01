//! Test-runner family compactor: detects pytest, cargo test (incl. nextest),
//! go test, and jest/vitest from `argv`, parses each runner's summary/result
//! lines into pass/fail/skip counts, and collects the individual failing test
//! names so an agent sees what broke without re-reading the full log.

use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};
use regex::Regex;

/// Truncate any single collected line to this many characters.
const MAX_LINE: usize = 200;
/// Stop collecting failure detail lines past this many entries.
const MAX_FAILURES: usize = 50;

/// Which test runner produced the output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Runner {
    Pytest,
    CargoTest,
    GoTest,
    Jest,
}

impl Runner {
    /// The `family` label written into the summary.
    fn family(self) -> &'static str {
        match self {
            Runner::Pytest => "pytest",
            Runner::CargoTest => "cargo-test",
            Runner::GoTest => "go-test",
            Runner::Jest => "jest",
        }
    }
}

/// Identify the runner from argv. Mirrors the families routed here by
/// `classify`, with sensible fallbacks for `npm/yarn/pnpm/bun test`.
fn detect_runner(argv: &[String]) -> Runner {
    // An explicit pytest invocation (e.g. `python -m pytest`) wins.
    if argv.iter().any(|a| a == "pytest" || a == "py.test") {
        return Runner::Pytest;
    }
    match command_basename(argv) {
        "pytest" | "py.test" | "python" | "python3" => Runner::Pytest,
        "go" => Runner::GoTest,
        "cargo" => Runner::CargoTest,
        "jest" | "vitest" | "mocha" | "ava" => Runner::Jest,
        _ => {
            // npm/pnpm/yarn/bun/npx test, or an unknown wrapper: inspect argv.
            if argv
                .iter()
                .any(|a| a == "jest" || a == "vitest" || a == "mocha" || a == "ava")
            {
                Runner::Jest
            } else if argv.iter().any(|a| a == "cargo") {
                Runner::CargoTest
            } else if argv.iter().any(|a| a == "go") {
                Runner::GoTest
            } else {
                Runner::Jest
            }
        }
    }
}

/// Entry point: dispatch to the per-runner parser, then finalize the headline,
/// status, and any "couldn't parse" note.
pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let runner = detect_runner(cx.argv);
    let mut summary = SemanticSummary::new(cx, runner.family());

    match runner {
        Runner::Pytest => parse_pytest(cx, &mut summary),
        Runner::CargoTest => parse_cargo(cx, &mut summary),
        Runner::GoTest => parse_go(cx, &mut summary),
        Runner::Jest => parse_jest(cx, &mut summary),
    }

    finalize(cx, &mut summary);
    summary
}

/// pytest: the bordered summary line `=== N passed, M failed, K skipped ... ===`
/// plus `FAILED path::test - reason` / `ERROR path - reason` detail lines.
fn parse_pytest(cx: &CommandContext, summary: &mut SemanticSummary) {
    let count_re = Regex::new(r"(\d+)\s+(passed|failed|skipped|errors?)").unwrap();

    let mut passed = 0i64;
    let mut failed = 0i64;
    let mut skipped = 0i64;
    let mut errors = 0i64;
    let mut found_summary = false;

    for line in cx.all_lines() {
        let trimmed = line.trim();

        // Detail lines from the "short test summary info" section.
        if let Some(rest) = trimmed.strip_prefix("FAILED ") {
            if (rest.contains("::") || rest.contains(".py"))
                && summary.failures.len() < MAX_FAILURES
            {
                summary.add_failure(clip(trimmed, MAX_LINE));
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("ERROR ") {
            if (rest.contains("::") || rest.contains(".py"))
                && summary.failures.len() < MAX_FAILURES
            {
                summary.add_failure(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        // The pytest result line is bracketed by '=' and names a status keyword.
        let is_summary = trimmed.len() > 2
            && trimmed.starts_with('=')
            && trimmed.ends_with('=')
            && (trimmed.contains("passed")
                || trimmed.contains("failed")
                || trimmed.contains("error")
                || trimmed.contains("skipped"));
        if is_summary {
            found_summary = true;
            for cap in count_re.captures_iter(trimmed) {
                let n: i64 = cap[1].parse().unwrap_or(0);
                match &cap[2] {
                    "passed" => passed += n,
                    "failed" => failed += n,
                    "skipped" => skipped += n,
                    "error" | "errors" => errors += n,
                    _ => {}
                }
            }
        }
    }

    if found_summary {
        summary.set_count("passed", passed);
        summary.set_count("failed", failed);
        summary.set_count("skipped", skipped);
    } else if !summary.failures.is_empty() {
        summary.set_count("failed", summary.failures.len() as i64);
    }
    if errors > 0 {
        summary.set_count("errors", errors);
    }
}

/// cargo test: libtest `test result: ok|FAILED. N passed; M failed; K ignored`
/// and `test mod::name ... FAILED`, plus cargo-nextest summary/`FAIL [..]` lines.
/// Counts are summed across every result line (lib, bins, doctests).
fn parse_cargo(cx: &CommandContext, summary: &mut SemanticSummary) {
    let result_re = Regex::new(
        r"test result:\s+(?:ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored",
    )
    .unwrap();
    let fail_re = Regex::new(r"^test\s+(\S+)\s+\.\.\.\s+FAILED").unwrap();
    let nextest_re = Regex::new(r"(\d+)\s+tests run:\s+(\d+)\s+passed.*?(\d+)\s+failed").unwrap();
    let nextest_fail_re = Regex::new(r"^FAIL\s+\[[^\]]*\]\s+(.+)$").unwrap();

    let mut passed = 0i64;
    let mut failed = 0i64;
    let mut ignored = 0i64;
    let mut saw_result = false;

    for line in cx.all_lines() {
        let trimmed = line.trim();

        if let Some(cap) = result_re.captures(trimmed) {
            saw_result = true;
            passed += cap[1].parse::<i64>().unwrap_or(0);
            failed += cap[2].parse::<i64>().unwrap_or(0);
            ignored += cap[3].parse::<i64>().unwrap_or(0);
            continue;
        }
        if let Some(cap) = nextest_re.captures(trimmed) {
            saw_result = true;
            passed += cap[2].parse::<i64>().unwrap_or(0);
            failed += cap[3].parse::<i64>().unwrap_or(0);
            continue;
        }
        if let Some(cap) = nextest_fail_re.captures(trimmed) {
            if summary.failures.len() < MAX_FAILURES {
                summary.add_failure(clip(cap[1].trim(), MAX_LINE));
            }
            continue;
        }
        if let Some(cap) = fail_re.captures(trimmed) {
            if summary.failures.len() < MAX_FAILURES {
                summary.add_failure(clip(&cap[1], MAX_LINE));
            }
        }
    }

    if saw_result {
        summary.set_count("passed", passed);
        summary.set_count("failed", failed);
        if ignored > 0 {
            summary.set_count("ignored", ignored);
        }
    } else if !summary.failures.is_empty() {
        summary.set_count("failed", summary.failures.len() as i64);
    }
}

/// go test: `--- FAIL: Name` / `--- PASS: Name` per test and `ok|FAIL  pkg dur`
/// package result lines.
fn parse_go(cx: &CommandContext, summary: &mut SemanticSummary) {
    let fail_re = Regex::new(r"^---\s+FAIL:\s+(\S+)").unwrap();
    let pass_re = Regex::new(r"^---\s+PASS:\s+(\S+)").unwrap();
    let pkg_re = Regex::new(r"^(ok|FAIL)\s+(\S+)\s+(?:[\d.]+s|\(cached\))").unwrap();

    let mut passed = 0i64;
    let mut failed = 0i64;
    let mut pkg_failed = 0i64;

    for line in cx.all_lines() {
        let trimmed = line.trim_start();

        if let Some(cap) = fail_re.captures(trimmed) {
            failed += 1;
            if summary.failures.len() < MAX_FAILURES {
                summary.add_failure(clip(&cap[1], MAX_LINE));
            }
            continue;
        }
        if pass_re.is_match(trimmed) {
            passed += 1;
            continue;
        }
        if let Some(cap) = pkg_re.captures(trimmed) {
            summary.add_path(clip(&cap[2], MAX_LINE));
            if &cap[1] == "FAIL" {
                pkg_failed += 1;
            }
        }
    }

    summary.set_count("passed", passed);
    summary.set_count("failed", failed);
    if pkg_failed > 0 {
        summary.set_count("packages_failed", pkg_failed);
    }
}

/// jest/vitest: the `Tests: X failed, Y passed, Z total` summary (jest) or
/// `Tests  X failed | Y passed (Z)` (vitest), plus `✕`/`×`/`●` failure markers.
fn parse_jest(cx: &CommandContext, summary: &mut SemanticSummary) {
    let count_re = Regex::new(r"(\d+)\s+(failed|passed|skipped|todo|pending|total)").unwrap();
    let timing_re = Regex::new(r"\s*\(\d+(?:\.\d+)?\s*m?s\)\s*$").unwrap();

    let mut passed = 0i64;
    let mut failed = 0i64;
    let mut skipped = 0i64;
    let mut total = 0i64;
    let mut found_summary = false;

    for line in cx.all_lines() {
        let trimmed = line.trim();

        // Failure markers: jest `✕`/`●`, vitest `×`/`✗`.
        if let Some(m) = trimmed
            .chars()
            .next()
            .filter(|&c| matches!(c, '✕' | '×' | '✗' | '●'))
        {
            let stripped = timing_re.replace(trimmed[m.len_utf8()..].trim(), "");
            let name = stripped.trim();
            if !name.is_empty() && summary.failures.len() < MAX_FAILURES {
                summary.add_failure(clip(name, MAX_LINE));
            }
            continue;
        }

        // The aggregate test line (not the "Test Files" line).
        if trimmed.starts_with("Tests:") || trimmed.starts_with("Tests ") {
            found_summary = true;
            for cap in count_re.captures_iter(trimmed) {
                let n: i64 = cap[1].parse().unwrap_or(0);
                match &cap[2] {
                    "passed" => passed += n,
                    "failed" => failed += n,
                    "skipped" | "todo" | "pending" => skipped += n,
                    "total" => total += n,
                    _ => {}
                }
            }
        }
    }

    if found_summary {
        summary.set_count("passed", passed);
        summary.set_count("failed", failed);
        if skipped > 0 {
            summary.set_count("skipped", skipped);
        }
        if total == 0 {
            total = passed + failed + skipped;
        }
        summary.set_count("total", total);
    } else if !summary.failures.is_empty() {
        summary.set_count("failed", summary.failures.len() as i64);
    }
}

/// Build the headline from parsed counts and reconcile status: any reported
/// failure means "failed", and a non-zero exit with nothing parsed still fails
/// (with an explanatory note).
fn finalize(cx: &CommandContext, summary: &mut SemanticSummary) {
    let failed = summary.counts.get("failed").copied().unwrap_or(0);
    let errors = summary.counts.get("errors").copied().unwrap_or(0);

    let mut parts: Vec<String> = Vec::new();
    for key in ["passed", "failed", "skipped", "ignored", "errors"] {
        let v = summary.counts.get(key).copied().unwrap_or(0);
        if v > 0 {
            parts.push(format!("{v} {key}"));
        }
    }

    let headline = if !parts.is_empty() {
        parts.join(", ")
    } else if !summary.failures.is_empty() {
        format!("{} failing test(s)", summary.failures.len())
    } else if cx.exit_code == 0 {
        "all tests passed".to_string()
    } else {
        format!("exit {}", cx.exit_code)
    };
    summary.set_headline(headline);

    if failed > 0 || errors > 0 || !summary.failures.is_empty() {
        summary.status = "failed".to_string();
    }

    if cx.exit_code != 0 && failed == 0 && errors == 0 && summary.failures.is_empty() {
        summary.status = "failed".to_string();
        summary.add_note(format!(
            "exit {} but no test failures were parsed from output",
            cx.exit_code
        ));
    }
}

#[cfg(test)]
mod runner_tests {
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
    fn pytest_failures() {
        let a = argv(&["pytest", "-q"]);
        let out = "\
=========================== short test summary info ============================
FAILED tests/test_math.py::test_add - assert 3 == 4
FAILED tests/test_math.py::test_div - ZeroDivisionError: division by zero
=================== 2 failed, 5 passed, 1 skipped in 0.42s ====================
";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.family, "pytest");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["passed"], 5);
        assert_eq!(s.counts["failed"], 2);
        assert_eq!(s.counts["skipped"], 1);
        assert_eq!(s.failures.len(), 2);
        assert!(s.failures.iter().any(|f| f.contains("test_add")));
        assert_eq!(s.headline, "5 passed, 2 failed, 1 skipped");
    }

    #[test]
    fn pytest_passing() {
        let a = argv(&["pytest"]);
        let out = "\
============================= test session starts ==============================
collected 3 items

tests/test_ok.py ...                                                      [100%]

============================== 3 passed in 0.10s ===============================
";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["passed"], 3);
        assert_eq!(s.counts["failed"], 0);
        assert_eq!(s.headline, "3 passed");
        assert!(s.failures.is_empty());
    }

    #[test]
    fn pytest_with_errors() {
        let a = argv(&["pytest"]);
        let out = "\
ERROR tests/test_fix.py::test_x - fixture 'db' not found
==================== 1 failed, 2 passed, 1 error in 0.20s =====================
";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.counts["errors"], 1);
        assert_eq!(s.counts["failed"], 1);
        assert!(s.failures.iter().any(|f| f.contains("fixture")));
    }

    #[test]
    fn cargo_test_failures() {
        let a = argv(&["cargo", "test"]);
        let out = "\
running 4 tests
test tests::it_works ... ok
test tests::it_adds ... FAILED
test tests::it_subtracts ... ok
test tests::it_divides ... FAILED

failures:

---- tests::it_adds stdout ----
thread 'tests::it_adds' panicked at src/lib.rs:20:9:
assertion `left == right` failed

test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
";
        let s = summarize(&cx(&a, 101, out, ""));
        assert_eq!(s.family, "cargo-test");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["passed"], 2);
        assert_eq!(s.counts["failed"], 2);
        assert!(s.failures.iter().any(|f| f.contains("tests::it_adds")));
        assert!(s.failures.iter().any(|f| f.contains("tests::it_divides")));
    }

    #[test]
    fn cargo_test_passing_multiple_results() {
        let a = argv(&["cargo", "test"]);
        let out = "\
test result: ok. 10 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

   Doc-tests mycrate

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts["passed"], 12);
        assert_eq!(s.counts["failed"], 0);
        assert_eq!(s.counts["ignored"], 1);
        assert_eq!(s.headline, "12 passed, 1 ignored");
    }

    #[test]
    fn cargo_nextest_failures() {
        let a = argv(&["cargo", "nextest", "run"]);
        let out = "\
    Starting 3 tests across 1 binary
        FAIL [   0.005s] mycrate tests::it_adds
        PASS [   0.004s] mycrate tests::it_works
------------
     Summary [   0.020s] 3 tests run: 2 passed, 1 failed, 0 skipped
";
        let s = summarize(&cx(&a, 100, out, ""));
        assert_eq!(s.family, "cargo-test");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["passed"], 2);
        assert_eq!(s.counts["failed"], 1);
        assert!(s.failures.iter().any(|f| f.contains("tests::it_adds")));
    }

    #[test]
    fn go_test_failures() {
        let a = argv(&["go", "test", "./..."]);
        let out = "\
=== RUN   TestAdd
--- FAIL: TestAdd (0.00s)
    math_test.go:12: expected 3, got 4
=== RUN   TestSub
--- PASS: TestSub (0.00s)
FAIL
FAIL\tgithub.com/foo/bar\t0.013s
ok  \tgithub.com/foo/baz\t0.005s
";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.family, "go-test");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["passed"], 1);
        assert_eq!(s.counts["failed"], 1);
        assert_eq!(s.headline, "1 passed, 1 failed");
        assert!(s.failures.iter().any(|f| f.contains("TestAdd")));
        assert!(s.paths.iter().any(|p| p.contains("github.com/foo/bar")));
    }

    #[test]
    fn jest_failures() {
        let a = argv(&["jest"]);
        let out = "\
 FAIL  src/sum.test.js
  Calculator
    \u{2713} adds numbers (3 ms)
    \u{2715} subtracts numbers (2 ms)

  \u{25cf} Calculator \u{203a} subtracts numbers

    expect(received).toBe(expected)

Tests:       1 failed, 1 passed, 2 total
Snapshots:   0 total
Time:        1.234 s
";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.family, "jest");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["passed"], 1);
        assert_eq!(s.counts["failed"], 1);
        assert_eq!(s.counts["total"], 2);
        assert_eq!(s.headline, "1 passed, 1 failed");
        assert!(s.failures.iter().any(|f| f.contains("subtracts numbers")));
    }

    #[test]
    fn vitest_failures() {
        let a = argv(&["vitest", "run"]);
        let out = "\
 \u{2771} src/math.test.ts (2)
   \u{00d7} math > adds
   \u{2713} math > subs
 Test Files  1 failed (1)
      Tests  1 failed | 1 passed (2)
   Start at  10:00:00
";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.family, "jest");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts["failed"], 1);
        assert_eq!(s.counts["passed"], 1);
        assert_eq!(s.counts["total"], 2);
        assert!(s.failures.iter().any(|f| f.contains("math > adds")));
    }

    #[test]
    fn nonzero_exit_without_parsed_failures() {
        let a = argv(&["pytest"]);
        let out = "usage: pytest [options]\nerror: unrecognized arguments: --nope\n";
        let s = summarize(&cx(&a, 4, out, ""));
        assert_eq!(s.status, "failed");
        assert!(s.failures.is_empty());
        assert!(s.notes.iter().any(|n| n.contains("no test failures")));
    }
}
