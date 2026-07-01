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
            budgeted_summary(ctx, mode, cmd_id, argv, exit_code, summary)
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
) -> OutputObservation {
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

    OutputObservation {
        token_estimate: estimate_tokens(&text),
        display: text,
        raw: raw_ref(cmd_id),
    }
}

fn lossless_ref(cmd_id: &CommandId, argv: &[String], exit_code: i32) -> OutputObservation {
    let display = format!(
        "command: {}\nexit: {}\nraw_stdout: trace://{}/stdout\nraw_stderr: trace://{}/stderr\n",
        shell_join(argv),
        exit_code,
        cmd_id,
        cmd_id
    );
    OutputObservation {
        token_estimate: estimate_tokens(&display),
        display,
        raw: raw_ref(cmd_id),
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
    fn compact_keeps_raw_refs_and_failures() {
        let id = CommandId::new();
        let obs = render_observation(
            OutputMode::Compact,
            &id,
            &["pytest".to_string(), "-q".to_string()],
            1,
            b"failed test_a\n",
            b"AssertionError\n",
        );
        assert!(obs.display.contains("trace://"));
        assert!(obs.display.to_lowercase().contains("fail"));
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
