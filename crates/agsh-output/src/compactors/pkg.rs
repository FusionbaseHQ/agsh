//! Package-manager and build-tool family compactor.
//!
//! Handles dependency installers (`npm`, `pnpm`, `yarn`, `bun`), the Python
//! installer (`pip`/`pip3`), and build drivers (`make`, `cmake`, `ninja`,
//! `gradle`, `mvn`, `bazel`). Repetitive progress chatter is collapsed into
//! counts while the lines an agent actually needs are surfaced: how many
//! packages were added/removed/changed, reported vulnerabilities, installed
//! Python packages, and the errors that broke a build.

use crate::compactors::generic;
use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};
use regex::Regex;

/// Longest single detail line we keep before clipping.
const MAX_LINE: usize = 200;
/// Maximum entries kept in any one detail list.
const MAX_DETAIL: usize = 50;

/// Summarize a package-manager / build-tool invocation by dispatching on the
/// program name to a tool-group parser.
pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    match command_basename(cx.argv) {
        "npm" | "pnpm" | "yarn" | "bun" => summarize_node(cx),
        "pip" | "pip3" => summarize_pip(cx),
        "make" | "cmake" | "ninja" | "gradle" | "mvn" | "bazel" => summarize_build(cx),
        _ => {
            // Unknown member of the package family: fall back to the generic
            // reducer but keep the family label honest.
            let mut summary = generic::summarize(cx);
            summary.family = "pkg".to_string();
            summary
        }
    }
}

/// npm / pnpm / yarn / bun dependency installs.
fn summarize_node(cx: &CommandContext) -> SemanticSummary {
    let label = match command_basename(cx.argv) {
        "pnpm" => "pnpm",
        "yarn" => "yarn",
        "bun" => "bun",
        _ => "npm",
    };
    let mut s = SemanticSummary::new(cx, label);

    let re_added = Regex::new(r"added (\d+)").unwrap();
    let re_removed = Regex::new(r"removed (\d+)").unwrap();
    let re_changed = Regex::new(r"changed (\d+)").unwrap();
    let re_audited = Regex::new(r"audited (\d+)").unwrap();
    let re_vuln = Regex::new(r"(\d+) vulnerabilit").unwrap();

    let mut errors = 0i64;
    for line in cx.all_lines() {
        let trimmed = line.trim();
        let lead = line.trim_start();

        // Failures: npm `npm ERR!`, yarn `error `, pnpm `ERR_PNPM_*`.
        if lead.starts_with("npm ERR!")
            || lead.starts_with("error ")
            || lead == "error"
            || trimmed.contains("ERR_PNPM")
        {
            errors += 1;
            if s.failures.len() < MAX_DETAIL {
                s.add_failure(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        // Warnings: deprecation / peer-dep notices.
        if lead.starts_with("npm WARN") || lead.starts_with("warning ") || lead.starts_with("WARN ")
        {
            if s.warnings.len() < MAX_DETAIL {
                s.add_warning(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        // Counts may all appear on a single npm summary line.
        if let Some(n) = cap1(&re_added, line) {
            s.set_count("added", n);
        }
        if let Some(n) = cap1(&re_removed, line) {
            s.set_count("removed", n);
        }
        if let Some(n) = cap1(&re_changed, line) {
            s.set_count("changed", n);
        }
        if let Some(n) = cap1(&re_audited, line) {
            s.set_count("audited", n);
        }
        if let Some(n) = cap1(&re_vuln, line) {
            s.set_count("vulnerabilities", n);
            if n > 0 && s.warnings.len() < MAX_DETAIL {
                s.add_warning(clip(trimmed, MAX_LINE));
            }
        }
    }

    let vuln = s.counts.get("vulnerabilities").copied().unwrap_or(0);
    let headline = if s.status == "failed" {
        format!("{label} failed: {errors} error line(s)")
    } else {
        let mut parts = Vec::new();
        for key in ["added", "removed", "changed"] {
            if let Some(&v) = s.counts.get(key) {
                parts.push(format!("{v} {key}"));
            }
        }
        let mut head = if parts.is_empty() {
            format!("{label}: ok")
        } else {
            format!("{label}: {}", parts.join(", "))
        };
        if vuln > 0 {
            head.push_str(&format!("; {vuln} vulnerabilities"));
        }
        head
    };
    s.set_headline(headline);
    s
}

/// pip / pip3 installs.
fn summarize_pip(cx: &CommandContext) -> SemanticSummary {
    let mut s = SemanticSummary::new(cx, "pip");

    let mut installed = 0i64;
    let mut uninstalled = 0i64;
    let mut satisfied = 0i64;
    let mut errors = 0i64;

    for line in cx.all_lines() {
        let trimmed = line.trim();
        let lead = line.trim_start();

        if lead.starts_with("ERROR:") {
            errors += 1;
            if s.failures.len() < MAX_DETAIL {
                s.add_failure(clip(trimmed, MAX_LINE));
            }
            continue;
        }
        if lead.starts_with("WARNING:") {
            if s.warnings.len() < MAX_DETAIL {
                s.add_warning(clip(trimmed, MAX_LINE));
            }
            continue;
        }
        // Collapse the (often dozens of) already-satisfied lines into a count.
        if lead.starts_with("Requirement already satisfied") {
            satisfied += 1;
            continue;
        }
        if let Some(rest) = lead.strip_prefix("Successfully installed ") {
            for pkg in rest.split_whitespace() {
                installed += 1;
                if s.notes.len() < MAX_DETAIL {
                    s.add_note(format!("installed {pkg}"));
                }
            }
            continue;
        }
        if let Some(rest) = lead.strip_prefix("Successfully uninstalled ") {
            uninstalled += rest.split_whitespace().count() as i64;
            continue;
        }
    }

    if installed > 0 {
        s.set_count("installed", installed);
    }
    if uninstalled > 0 {
        s.set_count("uninstalled", uninstalled);
    }
    if satisfied > 0 {
        s.set_count("already_satisfied", satisfied);
    }

    let headline = if s.status == "failed" {
        format!("pip failed: {errors} error line(s)")
    } else if installed > 0 {
        let mut head = format!("pip: installed {installed} package(s)");
        if satisfied > 0 {
            head.push_str(&format!(", {satisfied} already satisfied"));
        }
        head
    } else if uninstalled > 0 {
        format!("pip: uninstalled {uninstalled} package(s)")
    } else if satisfied > 0 {
        format!("pip: {satisfied} requirement(s) already satisfied")
    } else {
        "pip: ok".to_string()
    };
    s.set_headline(headline);
    s
}

/// make / cmake / ninja / gradle / mvn / bazel build drivers.
fn summarize_build(cx: &CommandContext) -> SemanticSummary {
    let label = match command_basename(cx.argv) {
        "cmake" => "cmake",
        "ninja" => "ninja",
        "gradle" => "gradle",
        "mvn" => "mvn",
        "bazel" => "bazel",
        _ => "make",
    };
    let mut s = SemanticSummary::new(cx, label);

    let re_ninja = Regex::new(r"^\[\d+/\d+\]").unwrap();
    let re_cmake_pct = Regex::new(r"^\[\s*\d+%\]").unwrap();
    let re_make_err = Regex::new(r"\bError\s+\d+\b").unwrap();

    let mut progress = 0i64;
    let mut errors = 0i64;
    let mut nothing = false;

    for line in cx.all_lines() {
        let trimmed = line.trim();
        let lead = line.trim_start();
        let lower = line.to_ascii_lowercase();

        if lower.contains("nothing to be done") {
            nothing = true;
            if s.notes.len() < MAX_DETAIL {
                s.add_note(clip(trimmed, MAX_LINE));
            }
            continue;
        }
        if lower.contains("is up to date") {
            if s.notes.len() < MAX_DETAIL {
                s.add_note(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        let is_failure = (trimmed.contains("*** ") && lower.contains("error"))
            || lower.contains("error:")
            || trimmed.contains("[ERROR]")
            || trimmed.contains("CMake Error")
            || lower.contains("build failed")
            || lower.contains("build failure")
            || lead.starts_with("ninja: build stopped")
            || re_make_err.is_match(line);
        if is_failure {
            errors += 1;
            if s.failures.len() < MAX_DETAIL {
                s.add_failure(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        if lower.contains("warning:")
            || trimmed.contains("[WARNING]")
            || trimmed.contains("CMake Warning")
        {
            if s.warnings.len() < MAX_DETAIL {
                s.add_warning(clip(trimmed, MAX_LINE));
            }
            continue;
        }

        if is_progress(lead, &re_ninja, &re_cmake_pct) {
            progress += 1;
            continue;
        }
    }

    if progress > 0 {
        s.set_count("progress", progress);
    }
    if errors > 0 {
        s.set_count("errors", errors);
    }

    let headline = if s.status == "failed" {
        if errors > 0 {
            format!("{label} failed: {errors} error line(s)")
        } else {
            format!("{label} failed (exit {})", cx.exit_code)
        }
    } else if nothing {
        format!("{label}: nothing to be done")
    } else if progress > 0 {
        format!("{label}: {progress} build step(s)")
    } else {
        format!("{label}: ok")
    };
    s.set_headline(headline);
    s
}

/// Parse the first capture group of `re` against `line` as an `i64`.
fn cap1(re: &Regex, line: &str) -> Option<i64> {
    re.captures(line)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<i64>().ok())
}

/// Recognize a build-progress line we want to collapse into a count.
fn is_progress(lead: &str, re_ninja: &Regex, re_cmake_pct: &Regex) -> bool {
    const PREFIXES: &[&str] = &[
        "Building",
        "Compiling",
        "Linking",
        "Generating",
        "Scanning dependencies",
        "Built target",
        "Install the project",
        "CMakeFiles",
        "> Task ",
        "make[",
        "[INFO]",
    ];
    PREFIXES.iter().any(|p| lead.starts_with(p))
        || re_ninja.is_match(lead)
        || re_cmake_pct.is_match(lead)
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
    fn npm_install_counts_and_vulns() {
        let a = argv(&["npm", "install"]);
        let out = "\nadded 25 packages, removed 3 packages, changed 2 packages, and audited 30 packages in 4s\n\n5 packages are looking for funding\n  run `npm fund` for details\n\n2 vulnerabilities (1 moderate, 1 high)\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "npm");
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("added"), Some(&25));
        assert_eq!(s.counts.get("removed"), Some(&3));
        assert_eq!(s.counts.get("changed"), Some(&2));
        assert_eq!(s.counts.get("audited"), Some(&30));
        assert_eq!(s.counts.get("vulnerabilities"), Some(&2));
        assert!(s.warnings.iter().any(|w| w.contains("vulnerabilities")));
        assert!(s.headline.contains("vulnerabilities"));
    }

    #[test]
    fn npm_err_lines_are_failures() {
        let a = argv(&["npm", "install", "leftpadx"]);
        let err = "npm ERR! code E404\nnpm ERR! 404 Not Found - GET https://registry.npmjs.org/leftpadx\nnpm ERR! 404 'leftpadx@*' is not in this registry.";
        let s = summarize(&cx(&a, 1, "", err));
        assert_eq!(s.status, "failed");
        assert_eq!(s.failures.len(), 3);
        assert!(s.headline.contains("error"));
    }

    #[test]
    fn pip_install_success() {
        let a = argv(&["pip3", "install", "requests"]);
        let out = "Collecting requests\n  Downloading requests-2.31.0-py3-none-any.whl (62 kB)\nRequirement already satisfied: urllib3 in /usr/lib (from requests) (2.0.4)\nRequirement already satisfied: certifi in /usr/lib (from requests) (2023.7.22)\nInstalling collected packages: certifi, urllib3, requests\nSuccessfully installed certifi-2023.7.22 requests-2.31.0 urllib3-2.0.4";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "pip");
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("installed"), Some(&3));
        assert_eq!(s.counts.get("already_satisfied"), Some(&2));
        assert!(s.notes.iter().any(|n| n == "installed requests-2.31.0"));
        assert!(s.headline.contains("installed 3"));
    }

    #[test]
    fn pip_errors_are_failures() {
        let a = argv(&["pip", "install", "nonexistent"]);
        let err = "ERROR: Could not find a version that satisfies the requirement nonexistent (from versions: none)\nERROR: No matching distribution found for nonexistent";
        let s = summarize(&cx(&a, 1, "", err));
        assert_eq!(s.status, "failed");
        assert_eq!(s.failures.len(), 2);
        assert!(s.headline.contains("failed"));
    }

    #[test]
    fn make_failure_captures_errors() {
        let a = argv(&["make"]);
        let out = "gcc -c main.c\nmain.c: In function 'main':\nmain.c:5:5: error: 'x' undeclared (first use in this function)\nmake: *** [Makefile:8: main.o] Error 1";
        let s = summarize(&cx(&a, 2, out, ""));
        assert_eq!(s.family, "make");
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts.get("errors"), Some(&2));
        assert!(s.failures.iter().any(|f| f.contains("undeclared")));
        assert!(s.failures.iter().any(|f| f.contains("Error 1")));
        assert!(s.headline.contains("failed"));
    }

    #[test]
    fn ninja_progress_collapsed() {
        let a = argv(&["ninja"]);
        let out = "[1/3] Compiling foo.c\n[2/3] Compiling bar.c\n[3/3] Linking app";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "ninja");
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("progress"), Some(&3));
        assert!(s.headline.contains("build step"));
    }

    #[test]
    fn make_nothing_to_be_done() {
        let a = argv(&["make"]);
        let out = "make: Nothing to be done for 'all'.";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.status, "ok");
        assert!(s.notes.iter().any(|n| n.contains("Nothing to be done")));
        assert!(s.headline.contains("nothing to be done"));
    }
}
