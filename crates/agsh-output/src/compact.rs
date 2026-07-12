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
    if mode == OutputMode::Silent {
        return OutputObservation {
            display: String::new(),
            token_estimate: 0,
            raw: raw_ref(cmd_id),
        };
    }

    let stdout_text = String::from_utf8_lossy(stdout);
    let stderr_text = String::from_utf8_lossy(stderr);
    // Raw output never pays the cost of observation-only argv redaction.
    if matches!(mode, OutputMode::Raw | OutputMode::Rich) {
        let display = format!("{stdout_text}{stderr_text}");
        return OutputObservation {
            token_estimate: estimate_tokens(&display),
            display,
            raw: raw_ref(cmd_id),
        };
    }

    let redacted_argv = argv
        .iter()
        .map(|arg| ctx.redact_text(arg))
        .collect::<Vec<_>>();
    let observation_argv = redacted_argv.as_slice();

    match mode {
        OutputMode::Raw | OutputMode::Rich | OutputMode::Silent => unreachable!(),
        OutputMode::Clean => {
            let clean = ctx.clean_text(&format!("{stdout_text}{stderr_text}"));
            // A clean dump can still be huge; fall back to refs over budget.
            if estimate_tokens(&clean) > ctx.budget.max_tokens {
                lossless_ref(cmd_id, observation_argv, exit_code)
            } else {
                OutputObservation {
                    token_estimate: estimate_tokens(&clean),
                    display: clean,
                    raw: raw_ref(cmd_id),
                }
            }
        }
        OutputMode::Compact | OutputMode::Semantic => {
            // Tiny-output fast path (Compact only): a successful command whose
            // whole output is a few short lines has no representation more
            // compact than the output itself — headline/counts scaffolding
            // would be LARGER, and path shortening can erase the entire answer
            // (`compact pwd` used to render just "."). ANSI-stripping and
            // secret redaction still apply. Semantic (machine-parsed JSON) and
            // user-configured [[compactor]] rules are unaffected.
            if mode == OutputMode::Compact && exit_code == 0 && ctx.compactor.is_none() {
                if let Some(display) = ctx.verbatim_tiny(&stdout_text, &stderr_text) {
                    return OutputObservation {
                        token_estimate: estimate_tokens(&display),
                        display,
                        raw: raw_ref(cmd_id),
                    };
                }
            }
            let out = ctx.clean_text(&stdout_text);
            let err = ctx.clean_text(&stderr_text);
            let cx = CommandContext {
                cmd_id: cmd_id.to_string(),
                argv: observation_argv,
                exit_code,
                stdout: &out,
                stderr: &err,
            };
            // Homogeneous JSON array-of-objects → emit the column keys ONCE plus a
            // one-line `table<col:type,…> (N rows)` shape signature, instead of
            // repeating every key on every row. A big lossless token cut on the most
            // agent-read surface, and the signature lets an agent learn schema +
            // row-count without fetching the raw. A user [[compactor]] still wins.
            if ctx.compactor.is_none() {
                if let Some(summary) = json_table_summary(&cx, &out) {
                    return budgeted_summary(
                        ctx,
                        mode,
                        cmd_id,
                        observation_argv,
                        exit_code,
                        summary,
                        (&out, &err),
                    );
                }
            }
            // A configured [[compactor]] takes precedence over the built-in
            // family parsers for matching commands.
            let summary = match &ctx.compactor {
                Some(ruleset) => crate::rules::apply_compactor(ruleset, &cx),
                None => compactors::summarize(&cx),
            };
            budgeted_summary(
                ctx,
                mode,
                cmd_id,
                observation_argv,
                exit_code,
                summary,
                (&out, &err),
            )
        }
        OutputMode::LosslessRef => lossless_ref(cmd_id, observation_argv, exit_code),
    }
}

/// Detect a homogeneous JSON array-of-objects and summarize it as a header-once
/// table with a `table<col:type,…> (N rows)` shape signature. Returns `None` when
/// the output isn't such a table, so the generic path handles everything else.
/// Strict gate: every row must share the identical key set (never null-fill ragged
/// rows) so the encoding stays lossless.
fn json_table_summary(cx: &CommandContext, stdout: &str) -> Option<SemanticSummary> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let array = value.as_array()?;
    if array.len() < 2 {
        return None;
    }
    let first = array.first()?.as_object()?;
    if first.is_empty() {
        return None;
    }
    let columns: Vec<String> = first.keys().cloned().collect();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(array.len());
    for element in array {
        let obj = element.as_object()?;
        if obj.len() != columns.len() || !columns.iter().all(|c| obj.contains_key(c)) {
            return None;
        }
        rows.push(columns.iter().map(|c| json_scalar(&obj[c])).collect());
    }
    let signature = columns
        .iter()
        .map(|c| format!("{c}:{}", json_type(&first[c])))
        .collect::<Vec<_>>()
        .join(", ");
    let mut summary = SemanticSummary::new(cx, "json-table");
    summary.set_headline(format!("table<{signature}> ({} rows)", rows.len()));
    let mut body = Vec::with_capacity(rows.len() + 1);
    body.push(columns.join("\t"));
    for row in rows {
        body.push(row.join("\t"));
    }
    summary.set_body(body);
    Some(summary)
}

/// A JSON scalar as a bare string (strings unquoted); nested values as compact JSON.
fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::String(_) => "string",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Null => "null",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
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
    fn json_array_of_objects_becomes_a_header_once_table() {
        let id = CommandId::new();
        // Large enough to clear the tiny-output fast path (which would show a
        // small JSON verbatim — already its most compact form).
        let rows: Vec<String> = (0..24)
            .map(|i| format!(r#"{{"name":"item-number-{i}","size":{i}}}"#))
            .collect();
        let json = format!("[{}]", rows.join(","));
        let obs = render_observation(
            OutputMode::Compact,
            &id,
            &["cmd".to_string()],
            0,
            json.as_bytes(),
            b"",
        );
        assert!(
            obs.display
                .contains("table<name:string, size:number> (24 rows)"),
            "shape signature missing:\n{}",
            obs.display
        );
        assert!(
            obs.display.contains("name\tsize"),
            "header row missing:\n{}",
            obs.display
        );
        assert!(
            obs.display.contains("item-number-0\t0") && obs.display.contains("item-number-23\t23")
        );
    }

    #[test]
    fn tiny_success_output_is_shown_verbatim() {
        // `compact pwd`: the whole output is one line — the observation IS the
        // output. No headline, no counts, and crucially no workspace-path
        // shortening (which used to render the path as just ".").
        let id = CommandId::new();
        let mut ctx = CompactionContext::defaults();
        ctx.normalize.workspace = Some("/home/u/proj".to_string());
        ctx.normalize.home = Some("/home/u".to_string());
        let obs = render_observation_with(
            &ctx,
            OutputMode::Compact,
            &id,
            &["pwd".to_string()],
            0,
            b"/home/u/proj\n",
            b"",
        );
        assert_eq!(obs.display, "/home/u/proj\n");

        // Failures keep the full observation (status, digest, counts).
        let obs = render_observation_with(
            &ctx,
            OutputMode::Compact,
            &id,
            &["pwd".to_string()],
            1,
            b"/home/u/proj\n",
            b"",
        );
        assert!(obs.display.contains("[failed]"), "{}", obs.display);

        // Semantic stays structured JSON even for tiny outputs (machine-parsed).
        let obs = render_observation_with(
            &ctx,
            OutputMode::Semantic,
            &id,
            &["pwd".to_string()],
            0,
            b"/home/u/proj\n",
            b"",
        );
        assert!(obs.display.contains("\"exit_code\": 0"), "{}", obs.display);
    }

    #[test]
    fn tiny_fast_path_still_redacts_and_strips_ansi() {
        let id = CommandId::new();
        let obs = render_observation(
            OutputMode::Compact,
            &id,
            &["printenv".to_string()],
            0,
            b"\x1b[32mtoken=ghp_abcdefghijklmnopqrstuvwxyz0123\x1b[0m\n",
            b"",
        );
        assert!(!obs.display.contains('\x1b'), "{}", obs.display);
        assert!(obs.display.contains("[REDACTED]"), "{}", obs.display);
        assert!(!obs.display.contains("ghp_"), "{}", obs.display);
        assert!(
            !obs.display.contains("counts:"),
            "tiny output must skip scaffolding:\n{}",
            obs.display
        );
    }

    #[test]
    fn ragged_or_scalar_json_uses_the_generic_path() {
        let id = CommandId::new();
        // Array of scalars: not a table.
        let a = render_observation(OutputMode::Compact, &id, &["c".into()], 0, b"[1,2,3]", b"");
        assert!(!a.display.contains("table<"));
        // Ragged objects (different key sets): not lossless → generic path.
        let ragged = r#"[{"a":1},{"b":2}]"#;
        let b = render_observation(
            OutputMode::Compact,
            &id,
            &["c".into()],
            0,
            ragged.as_bytes(),
            b"",
        );
        assert!(
            !b.display.contains("table<"),
            "ragged must not be a table:\n{}",
            b.display
        );
    }

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

    #[test]
    fn command_metadata_is_redacted_in_semantic_and_lossless_ref_modes() {
        let id = CommandId::new();
        let secret = "supersecretvalue";
        let mut ctx = CompactionContext::defaults();
        ctx.redact.literal_secrets.push(secret.to_string());
        let argv = ["printf".to_string(), secret.to_string()];

        for mode in [OutputMode::Semantic, OutputMode::LosslessRef] {
            let obs = render_observation_with(&ctx, mode, &id, &argv, 0, b"", b"");
            assert!(
                obs.display.contains("[REDACTED]"),
                "display={:?}",
                obs.display
            );
            assert!(
                !obs.display.contains(secret),
                "secret leaked: {:?}",
                obs.display
            );
        }
    }
}
