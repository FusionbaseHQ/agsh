use agsh_core::CommandId;

use crate::budget::estimate_tokens;
use crate::compactors;
use crate::context::CompactionContext;
use crate::summary::{CommandContext, SemanticSummary};
use crate::util::shell_join;
use crate::{OutputMode, OutputObservation, RawStreamRef};

/// Render an observation with default normalization/redaction/budget.
pub fn render_observation(
    mode: OutputMode,
    cmd_id: &CommandId,
    argv: &[String],
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> OutputObservation {
    render_observation_with(
        &CompactionContext::defaults(),
        mode,
        cmd_id,
        argv,
        exit_code,
        stdout,
        stderr,
    )
}

/// Render an observation using the given compaction context.
pub fn render_observation_with(
    ctx: &CompactionContext,
    mode: OutputMode,
    cmd_id: &CommandId,
    argv: &[String],
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> OutputObservation {
    let stdout_text = String::from_utf8_lossy(stdout);
    let stderr_text = String::from_utf8_lossy(stderr);

    match mode {
        // Raw is exact: never normalize or redact it. Rich rendering is handled
        // by the executor (it needs the type renderers + theme); here it falls
        // back to raw passthrough.
        OutputMode::Raw | OutputMode::Rich => {
            let display = format!("{stdout_text}{stderr_text}");
            OutputObservation {
                token_estimate: estimate_tokens(&display),
                display,
                raw: raw_ref(cmd_id),
            }
        }
        OutputMode::Clean => {
            let clean = ctx.clean_text(&format!("{stdout_text}{stderr_text}"));
            // A clean dump can still be huge; fall back to refs over budget.
            if estimate_tokens(&clean) > ctx.budget.max_tokens {
                lossless_ref(cmd_id, argv, exit_code)
            } else {
                OutputObservation {
                    token_estimate: estimate_tokens(&clean),
                    display: clean,
                    raw: raw_ref(cmd_id),
                }
            }
        }
        OutputMode::Compact | OutputMode::Semantic => {
            let out = ctx.clean_text(&stdout_text);
            let err = ctx.clean_text(&stderr_text);
            let cx = CommandContext {
                cmd_id: cmd_id.to_string(),
                argv,
                exit_code,
                stdout: &out,
                stderr: &err,
            };
            // A configured [[compactor]] takes precedence over the built-in
            // family parsers for matching commands.
            let summary = match &ctx.compactor {
                Some(ruleset) => crate::rules::apply_compactor(ruleset, &cx),
                None => compactors::summarize(&cx),
            };
            budgeted_summary(ctx, mode, cmd_id, argv, exit_code, summary, (&out, &err))
        }
        OutputMode::LosslessRef => lossless_ref(cmd_id, argv, exit_code),
        OutputMode::Silent => OutputObservation {
            display: String::new(),
            token_estimate: 0,
            raw: raw_ref(cmd_id),
        },
    }
}

/// Render a summary as compact text or JSON, enforcing the token budget:
/// over the default budget, sections are capped harder; over the max budget,
/// the observation degrades to a lossless reference.
fn budgeted_summary(
    ctx: &CompactionContext,
    mode: OutputMode,
    cmd_id: &CommandId,
    argv: &[String],
    exit_code: i32,
    mut summary: SemanticSummary,
    raw_output: (&str, &str),
) -> OutputObservation {
    let (stdout, stderr) = raw_output;
    let render = |s: &SemanticSummary| {
        if mode == OutputMode::Semantic {
            s.to_json()
        } else {
            s.to_compact()
        }
    };

    let mut text = render(&summary);
    if estimate_tokens(&text) > ctx.budget.default_tokens {
        summary.cap_sections(10);
        text = render(&summary);
    }
    if estimate_tokens(&text) > ctx.budget.default_tokens {
        summary.cap_sections(3);
        text = render(&summary);
    }
    if estimate_tokens(&text) > ctx.budget.max_tokens {
        return lossless_ref(cmd_id, argv, exit_code);
    }

    // Attach a raw-output pointer ONLY when the compact view actually dropped
    // content — otherwise the raw is already shown above and a ref is just noise.
    if !output_fully_shown(&text, stdout, stderr) {
        let (out_ref, err_ref) = raw_ref_strings(cmd_id);
        summary.set_raw_refs(out_ref, err_ref);
        text = render(&summary);
    }

    OutputObservation {
        token_estimate: estimate_tokens(&text),
        display: text,
        raw: raw_ref(cmd_id),
    }
}

fn lossless_ref(cmd_id: &CommandId, argv: &[String], exit_code: i32) -> OutputObservation {
    let (out_ref, err_ref) = raw_ref_strings(cmd_id);
    let display = format!(
        "command: {}\nexit: {}\nraw_stdout: {}\nraw_stderr: {}\n",
        shell_join(argv),
        exit_code,
        out_ref,
        err_ref,
    );
    OutputObservation {
        token_estimate: estimate_tokens(&display),
        display,
        raw: raw_ref(cmd_id),
    }
}

/// Whether the rendered `display` already contains all of the raw output, so a
/// pointer back to it would be redundant. Empty output counts as fully shown.
fn output_fully_shown(display: &str, stdout: &str, stderr: &str) -> bool {
    stdout.lines().chain(stderr.lines()).all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || display.contains(trimmed)
    })
}

/// The two strings for a `raw:` reference. When `$AGSH_TRACE_DIR` is set — the
/// interception/observe path persists each command's raw bytes there — these are
/// catable file paths that resolve from any process (`<dir>/<pid>_<id>.out`).
/// Otherwise they're in-session `trace://` references (resolved by the `trace`
/// builtin within the same live shell).
fn raw_ref_strings(cmd_id: &CommandId) -> (String, String) {
    if let Some(dir) = std::env::var_os("AGSH_TRACE_DIR") {
        let dir = std::path::Path::new(&dir);
        let pid = std::process::id();
        (
            dir.join(format!("{pid}_{cmd_id}.out"))
                .display()
                .to_string(),
            dir.join(format!("{pid}_{cmd_id}.err"))
                .display()
                .to_string(),
        )
    } else {
        (
            format!("trace://{cmd_id}/stdout"),
            format!("trace://{cmd_id}/stderr"),
        )
    }
}

fn raw_ref(cmd_id: &CommandId) -> Option<RawStreamRef> {
    Some(RawStreamRef {
        stdout: format!("trace://{cmd_id}/stdout"),
        stderr: format!("trace://{cmd_id}/stderr"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_core::CommandId;

    #[test]
    fn compact_omits_ref_when_output_fully_shown() {
        // A generic command's short output is shown verbatim in the body, so a
        // pointer back to the raw would be redundant noise.
        let id = CommandId::new();
        let obs = render_observation(
            OutputMode::Compact,
            &id,
            &["mycmd".to_string()],
            0,
            b"only line one\nonly line two\n",
            b"",
        );
        assert!(obs.display.contains("only line one"));
        assert!(
            !obs.display.contains("raw:"),
            "fully-shown output must omit the raw ref:\n{}",
            obs.display
        );
    }

    #[test]
    fn compact_emits_ref_only_when_output_is_elided() {
        let id = CommandId::new();
        let big: String = (0..600).map(|n| format!("line {n}\n")).collect();
        let obs = render_observation(
            OutputMode::Compact,
            &id,
            &["seq".to_string()],
            0,
            big.as_bytes(),
            b"",
        );
        assert!(
            obs.display.contains("raw:"),
            "elided output should carry a raw ref:\n{}",
            obs.display
        );
    }

    #[test]
    fn semantic_is_json_with_exit_code() {
        let id = CommandId::new();
        let obs = render_observation(
            OutputMode::Semantic,
            &id,
            &["false".to_string()],
            1,
            b"",
            b"",
        );
        assert!(obs.display.contains("\"exit_code\": 1"));
        assert!(obs.display.contains("\"status\": \"failed\""));
    }

    #[test]
    fn clean_strips_ansi_and_redacts() {
        let id = CommandId::new();
        let obs = render_observation(
            OutputMode::Clean,
            &id,
            &["echo".to_string()],
            0,
            b"\x1b[31mtoken=ghp_abcdefghijklmnopqrstuvwxyz0123\x1b[0m\n",
            b"",
        );
        assert!(!obs.display.contains('\x1b'));
        assert!(obs.display.contains("[REDACTED]"));
        assert!(!obs.display.contains("ghp_"));
    }

    #[test]
    fn over_max_budget_falls_back_to_lossless_ref() {
        let id = CommandId::new();
        let mut ctx = CompactionContext::defaults();
        ctx.budget.default_tokens = 5;
        ctx.budget.max_tokens = 10;
        let big = "error here\n".repeat(500);
        let obs = render_observation_with(
            &ctx,
            OutputMode::Semantic,
            &id,
            &["make".to_string()],
            1,
            big.as_bytes(),
            b"",
        );
        assert!(obs.display.contains("raw_stdout: trace://"));
        assert!(obs.token_estimate <= 60);
    }
}
