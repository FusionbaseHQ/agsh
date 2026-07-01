//! Generic reducer used when no family-specific parser applies. Surfaces a
//! failure/warning digest, then a generically *reduced* body of the output
//! (rtk-style: noise dropped, duplicates/progress collapsed, long output
//! windowed) so `compact` mode shrinks the long tail of commands too — not just
//! the known families.

use crate::reduce::{reduce, ReduceOptions};
use crate::summary::{CommandContext, SemanticSummary};
use crate::util::clip;

const FAIL_NEEDLES: &[&str] = &[
    "error",
    "failed",
    "failure",
    "panic",
    "exception",
    "fatal",
    "traceback",
];
const WARN_NEEDLES: &[&str] = &["warning", "warn:", " warn "];
const MAX_LINE: usize = 200;

pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let mut summary = SemanticSummary::new(cx, "generic");
    let stdout_lines = cx.stdout.lines().count();
    let stderr_lines = cx.stderr.lines().count();
    summary
        .set_count("stdout_lines", stdout_lines as i64)
        .set_count("stderr_lines", stderr_lines as i64);

    // Failure/warning digest (a quick "what broke" scan over both streams).
    for line in cx.all_lines() {
        let lower = line.to_ascii_lowercase();
        if FAIL_NEEDLES.iter().any(|n| lower.contains(n)) {
            summary.add_failure(clip(line, MAX_LINE));
        } else if WARN_NEEDLES.iter().any(|n| lower.contains(n)) {
            summary.add_warning(clip(line, MAX_LINE));
        }
    }

    // Reduced body: the actual output, with progress/noise/dupes stripped and a
    // head/tail window when large. Prefer stdout; fall back to stderr.
    let opts = ReduceOptions::default();
    let source = if cx.stdout.trim().is_empty() {
        cx.stderr
    } else {
        cx.stdout
    };
    let reduced = reduce(source, &opts);
    if reduced.dropped > 0 {
        summary.set_count("reduced_out", reduced.dropped as i64);
    }
    summary.set_body(reduced.lines);

    let headline = if cx.exit_code == 0 {
        format!("ok: {stdout_lines} stdout / {stderr_lines} stderr lines")
    } else {
        format!(
            "exit {}: {} failure-like line(s)",
            cx.exit_code,
            summary.failures.len()
        )
    };
    summary.set_headline(headline);
    summary
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

    #[test]
    fn keeps_failures() {
        let argv = vec!["mybuild".to_string()];
        let s = summarize(&cx(&argv, 1, "ok line\nError: boom\n", ""));
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("boom")));
    }

    #[test]
    fn reduces_clean_output_into_body() {
        let argv = vec!["echo".to_string()];
        let body = (0..200)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = summarize(&cx(&argv, 0, &body, ""));
        // Body keeps a head/tail window with an omit marker (not all 200 lines).
        assert!(s.body.iter().any(|l| l.contains("line0")));
        assert!(s.body.iter().any(|l| l.contains("line199")));
        assert!(s.body.iter().any(|l| l.contains("omitted")));
        assert!(s.body.len() < 200);
        assert!(*s.counts.get("reduced_out").unwrap_or(&0) > 0);
    }

    #[test]
    fn drops_progress_noise_from_body() {
        let argv = vec!["mybuild".to_string()];
        let out = "Compiling foo\nDownloading bar\nactual result\n";
        let s = summarize(&cx(&argv, 0, out, ""));
        assert!(s.body.iter().any(|l| l == "actual result"));
        assert!(!s.body.iter().any(|l| l.starts_with("Compiling")));
    }
}
