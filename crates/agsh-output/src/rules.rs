//! Apply a configurable `[[compactor]]` rule set to a command's output.
//!
//! Rules are deterministic and auditable:
//! - `keep`   — keep lines matching `match.line_regex` (up to `limit`).
//! - `group`  — collapse matching lines into a `group_name` count.
//! - `keep_tail` — keep the last `lines` lines of output.

use regex::{Regex, RegexSet};

use crate::config::{CompactorRuleSet, RuleSpec};
use crate::reduce::{reduce, ReduceOptions};
use crate::summary::{CommandContext, SemanticSummary};
use crate::util::clip;

const MAX_LINE: usize = 200;

/// Build a semantic summary by applying a compactor's rules to the output.
pub fn apply_compactor(ruleset: &CompactorRuleSet, cx: &CommandContext) -> SemanticSummary {
    let mut summary = SemanticSummary::new(cx, &ruleset.name);
    let lines: Vec<&str> = cx.all_lines().collect();

    for rule in &ruleset.rule {
        match rule.action.as_str() {
            "keep" => apply_keep(rule, &lines, &mut summary),
            "group" => apply_group(rule, &lines, &mut summary),
            "keep_tail" => apply_keep_tail(rule, &lines, &mut summary),
            _ => {}
        }
    }

    let mut headline = if cx.exit_code == 0 {
        format!("{}: ok", ruleset.name)
    } else {
        format!("{}: exit {}", ruleset.name, cx.exit_code)
    };

    // Declarative line reduction (rtk-style): produce a reduced body.
    if ruleset.has_reduce() {
        if let Some(empty_msg) = apply_declarative_reduce(ruleset, cx, &mut summary) {
            headline = empty_msg;
        }
    }

    summary.set_headline(headline);
    summary
}

/// Apply the declarative reduction pipeline (replace → strip/keep lines →
/// strip-ANSI/truncate/window) to the command output, setting `summary.body`.
/// Returns `Some(on_empty headline)` if the result is empty and `on_empty` is set.
fn apply_declarative_reduce(
    ruleset: &CompactorRuleSet,
    cx: &CommandContext,
    summary: &mut SemanticSummary,
) -> Option<String> {
    // `filter_stderr` merges stderr into the reducer input; otherwise prefer
    // stdout (falling back to stderr when stdout is empty).
    let merged;
    let source: &str = if ruleset.filter_stderr {
        merged = format!("{}\n{}", cx.stdout, cx.stderr);
        &merged
    } else if cx.stdout.trim().is_empty() {
        cx.stderr
    } else {
        cx.stdout
    };

    // Stage 0: match_output short-circuit — collapse the whole output to a
    // message when matched, unless an error guard also matches.
    for rule in &ruleset.match_output {
        let Ok(re) = Regex::new(&rule.pattern) else {
            continue;
        };
        if !re.is_match(source) {
            continue;
        }
        let guarded = rule
            .unless
            .as_deref()
            .and_then(|u| Regex::new(u).ok())
            .is_some_and(|g| g.is_match(source));
        if !guarded {
            return Some(rule.message.clone());
        }
    }

    // Stage A: per-line regex substitutions, in order.
    let replacers: Vec<(Regex, &str)> = ruleset
        .replace
        .iter()
        .filter_map(|r| {
            Regex::new(&r.pattern)
                .ok()
                .map(|re| (re, r.replacement.as_str()))
        })
        .collect();
    let mut text_lines: Vec<String> = source
        .lines()
        .map(|line| {
            let mut s = line.to_string();
            for (re, repl) in &replacers {
                s = re.replace_all(&s, *repl).into_owned();
            }
            s
        })
        .collect();

    // Stage B: strip_lines (drop matching), then keep_lines (keep only matching).
    if let Ok(strip) = RegexSet::new(&ruleset.strip_lines) {
        if !ruleset.strip_lines.is_empty() {
            text_lines.retain(|l| !strip.is_match(l));
        }
    }
    if let Ok(keep) = RegexSet::new(&ruleset.keep_lines) {
        if !ruleset.keep_lines.is_empty() {
            text_lines.retain(|l| keep.is_match(l));
        }
    }

    // Stage C: generic reduce with the declared knobs (no auto noise-drop/dedup —
    // the user controls filtering explicitly here).
    let (head, tail) = match (ruleset.head_lines, ruleset.tail_lines) {
        (None, None) => (ruleset.max_lines.unwrap_or(0), 0),
        (h, t) => (h.unwrap_or(0), t.unwrap_or(0)),
    };
    let max_lines = ruleset.max_lines.unwrap_or(head + tail);
    let opts = ReduceOptions {
        strip_ansi: ruleset.strip_ansi.unwrap_or(true),
        collapse_blanks: true,
        dedup_consecutive: false,
        collapse_cr: true,
        drop_noise: false,
        truncate_line: ruleset.truncate_lines_at,
        head,
        tail,
        max_lines: if max_lines == 0 {
            usize::MAX
        } else {
            max_lines
        },
    };
    let reduced = reduce(&text_lines.join("\n"), &opts);

    if reduced.lines.is_empty() {
        return ruleset.on_empty.clone();
    }
    // Count everything removed (strip + window), relative to the original output.
    let kept_real = reduced
        .lines
        .iter()
        .filter(|l| !l.starts_with("… ("))
        .count();
    let dropped = source.lines().count().saturating_sub(kept_real);
    if dropped > 0 {
        summary.set_count("reduced_out", dropped as i64);
    }
    summary.set_body(reduced.lines);
    None
}

fn apply_keep(rule: &RuleSpec, lines: &[&str], summary: &mut SemanticSummary) {
    let Some(re) = compiled(rule) else { return };
    let limit = parse_limit(rule.limit.as_deref());
    let errorish = rule_targets_errors(rule);
    let mut kept = 0usize;
    for line in lines {
        if kept >= limit {
            break;
        }
        if re.is_match(line) {
            if errorish {
                summary.add_failure(clip(line, MAX_LINE));
            } else {
                summary.add_note(clip(line, MAX_LINE));
            }
            kept += 1;
        }
    }
}

fn apply_group(rule: &RuleSpec, lines: &[&str], summary: &mut SemanticSummary) {
    let Some(re) = compiled(rule) else { return };
    let count = lines.iter().filter(|l| re.is_match(l)).count();
    if count > 0 {
        let name = rule.group_name.clone().unwrap_or_else(|| rule.name.clone());
        summary.set_count(&name, count as i64);
    }
}

fn apply_keep_tail(rule: &RuleSpec, lines: &[&str], summary: &mut SemanticSummary) {
    let n = rule.lines.unwrap_or(80);
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        if !line.trim().is_empty() {
            summary.add_note(clip(line, MAX_LINE));
        }
    }
}

fn compiled(rule: &RuleSpec) -> Option<Regex> {
    rule.match_spec
        .line_regex
        .as_deref()
        .and_then(|p| Regex::new(p).ok())
}

fn parse_limit(limit: Option<&str>) -> usize {
    match limit {
        None | Some("all") => usize::MAX,
        Some(value) => value.parse().unwrap_or(usize::MAX),
    }
}

fn rule_targets_errors(rule: &RuleSpec) -> bool {
    let name = rule.name.to_ascii_lowercase();
    let regex = rule
        .match_spec
        .line_regex
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    name.contains("error")
        || name.contains("fail")
        || regex.contains("error")
        || regex.contains("fail")
        || regex.contains("fatal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompactorConfig;

    fn ruleset() -> CompactorRuleSet {
        let toml = r#"
[[compactor]]
name = "mybuild"
match.argv = ["mybuild", "*"]
mode = "semantic"
priority = 100

[[compactor.rule]]
name = "keep-errors"
match.line_regex = "^ERROR"
action = "keep"
limit = "all"

[[compactor.rule]]
name = "collapse-progress"
match.line_regex = "^(Building|Compiling) "
action = "group"
group_name = "progress"

[[compactor.rule]]
name = "tail"
action = "keep_tail"
lines = 2
"#;
        CompactorConfig::from_toml(toml).compactors.remove(0)
    }

    fn reduce_ruleset() -> CompactorRuleSet {
        let toml = r#"
[[compactor]]
name = "noisy"
match.argv = ["noisy", "*"]
mode = "compact"
strip_ansi = true
strip_lines = ["^Entering directory", "^Compiling "]
replace = [{ pattern = "v[0-9]+\\.[0-9]+", replacement = "vX" }]
truncate_lines_at = 40
max_lines = 3
on_empty = "noisy: ok"
"#;
        CompactorConfig::from_toml(toml).compactors.remove(0)
    }

    #[test]
    fn declarative_reduce_strips_replaces_windows() {
        let argv = vec!["noisy".to_string()];
        let stdout = "Entering directory /x\nCompiling foo v1.2\nkeep one v3.4\nkeep two\nkeep three\nkeep four\n";
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 0,
            stdout,
            stderr: "",
        };
        let s = apply_compactor(&reduce_ruleset(), &cx);
        // Stripped lines gone; replace applied; windowed to 3 + omit marker.
        assert!(!s.body.iter().any(|l| l.starts_with("Entering")));
        assert!(s.body.iter().any(|l| l.contains("keep one vX")));
        assert!(s.body.iter().any(|l| l.contains("omitted")));
        assert!(*s.counts.get("reduced_out").unwrap_or(&0) >= 2);
    }

    fn match_output_ruleset() -> CompactorRuleSet {
        let toml = r#"
[[compactor]]
name = "rsync"
match.argv = ["rsync", "*"]
strip_lines = ["^sending incremental", "^sent "]
match_output = [{ pattern = "total size is", message = "ok (synced)", unless = "error|failed" }]
"#;
        CompactorConfig::from_toml(toml).compactors.remove(0)
    }

    #[test]
    fn match_output_short_circuits_on_success() {
        let argv = vec!["rsync".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 0,
            stdout: "sending incremental file list\nfile1\nsent 123 bytes\ntotal size is 999\n",
            stderr: "",
        };
        let s = apply_compactor(&match_output_ruleset(), &cx);
        assert_eq!(s.headline, "ok (synced)");
        assert!(s.body.is_empty());
    }

    #[test]
    fn match_output_unless_guard_keeps_errors() {
        let argv = vec!["rsync".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 11,
            // "total size is" present, but so is an error — must NOT short-circuit.
            stdout: "rsync error: failed to write\ntotal size is 999\n",
            stderr: "",
        };
        let s = apply_compactor(&match_output_ruleset(), &cx);
        assert_ne!(s.headline, "ok (synced)");
        assert!(s.body.iter().any(|l| l.contains("rsync error")));
    }

    #[test]
    fn declarative_on_empty_headline() {
        let argv = vec!["noisy".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 0,
            stdout: "Entering directory /x\nCompiling bar v9.9\n",
            stderr: "",
        };
        let s = apply_compactor(&reduce_ruleset(), &cx);
        assert_eq!(s.headline, "noisy: ok");
        assert!(s.body.is_empty());
    }

    #[test]
    fn applies_keep_group_tail() {
        let argv = vec!["mybuild".to_string()];
        let stdout = "Building a\nCompiling b\nERROR: x failed\nstep done\nfinal line\n";
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 1,
            stdout,
            stderr: "",
        };
        let s = apply_compactor(&ruleset(), &cx);
        assert!(s.failures.iter().any(|f| f.contains("ERROR: x failed")));
        assert_eq!(s.counts.get("progress"), Some(&2));
        assert!(s.notes.iter().any(|n| n.contains("final line")));
    }
}
