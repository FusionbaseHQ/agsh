//! Apply a configurable `[[compactor]]` rule set to a command's output.
//!
//! Rules are deterministic and auditable:
//! - `keep`   — keep lines matching `match.line_regex` (up to `limit`).
//! - `group`  — collapse matching lines into a `group_name` count.
//! - `keep_tail` — keep the last `lines` lines of output.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

use regex::{Regex, RegexSet};

use crate::config::{CompactorRuleSet, RuleSpec};
use crate::redact::{
    compile_config_regex, compile_config_regex_set, MAX_CONFIG_REGEX_BYTES, MAX_CONFIG_REGEX_COUNT,
};
use crate::reduce::strip_ansi;
use crate::summary::{CommandContext, SemanticSummary};
use crate::util::clip;

const MAX_LINE: usize = 200;
const MAX_RULE_ACTIONS: usize = 64;
const MAX_RULE_DETAILS: usize = 50;
const MAX_TOTAL_REGEX_BYTES: usize = 128 * 1024;
const MAX_RULESET_CACHE_ENTRIES: usize = 4;
const MAX_REPLACEMENT_BYTES: usize = 256;
const MAX_REPLACEMENTS_PER_LINE: usize = 128;
const MAX_INTERMEDIATE_LINE_CHARS: usize = 16 * 1024;
const MAX_REDUCED_LINE_CHARS: usize = 4 * 1024;
const MAX_REDUCED_BODY_LINES: usize = 512;

static RULESET_CACHE: LazyLock<Mutex<RuleProgramCache>> =
    LazyLock::new(|| Mutex::new(RuleProgramCache::default()));

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuleProgramKey {
    line_rules: Vec<Option<String>>,
    match_output: Vec<Option<(String, Option<String>)>>,
    replacers: Vec<Option<String>>,
    strip_lines: Vec<String>,
    keep_lines: Vec<String>,
}

impl RuleProgramKey {
    fn new(ruleset: &CompactorRuleSet) -> Self {
        let mut budget = PatternBudget::default();
        let line_rules = ruleset
            .rule
            .iter()
            .take(MAX_RULE_ACTIONS)
            .map(|rule| {
                if !matches!(rule.action.as_str(), "keep" | "group") {
                    return None;
                }
                let pattern = rule.match_spec.line_regex.as_ref()?;
                budget
                    .reserve(std::slice::from_ref(&pattern.as_str()))
                    .then(|| pattern.clone())
            })
            .collect();

        let match_output = ruleset
            .match_output
            .iter()
            .take(MAX_RULE_ACTIONS)
            .map(|rule| {
                let accepted = match rule.unless.as_deref() {
                    Some(unless) => budget.reserve(&[rule.pattern.as_str(), unless]),
                    None => budget.reserve(&[rule.pattern.as_str()]),
                };
                accepted.then(|| (rule.pattern.clone(), rule.unless.clone()))
            })
            .collect();

        let replacers = ruleset
            .replace
            .iter()
            .take(MAX_RULE_ACTIONS)
            .map(|rule| {
                (rule.replacement.len() <= MAX_REPLACEMENT_BYTES
                    && budget.reserve(std::slice::from_ref(&rule.pattern.as_str())))
                .then(|| rule.pattern.clone())
            })
            .collect();

        let strip_lines = ruleset
            .strip_lines
            .iter()
            .filter(|pattern| budget.reserve(std::slice::from_ref(&pattern.as_str())))
            .cloned()
            .collect();
        let keep_lines = ruleset
            .keep_lines
            .iter()
            .filter(|pattern| budget.reserve(std::slice::from_ref(&pattern.as_str())))
            .cloned()
            .collect();

        Self {
            line_rules,
            match_output,
            replacers,
            strip_lines,
            keep_lines,
        }
    }
}

#[derive(Default)]
struct PatternBudget {
    count: usize,
    bytes: usize,
}

impl PatternBudget {
    /// Reserve a group atomically. In particular, a `match_output` pattern is
    /// never enabled when its `unless` error guard could not also be compiled.
    fn reserve(&mut self, patterns: &[&str]) -> bool {
        let Some(bytes) = patterns
            .iter()
            .try_fold(0usize, |total, pattern| total.checked_add(pattern.len()))
        else {
            return false;
        };
        if patterns
            .iter()
            .any(|pattern| pattern.len() > MAX_CONFIG_REGEX_BYTES)
            || self.count.saturating_add(patterns.len()) > MAX_CONFIG_REGEX_COUNT
            || self.bytes.saturating_add(bytes) > MAX_TOTAL_REGEX_BYTES
        {
            return false;
        }
        self.count += patterns.len();
        self.bytes += bytes;
        true
    }
}

struct CompiledMatchOutput {
    pattern: Regex,
    unless: Option<Regex>,
}

struct CompiledRuleProgram {
    line_rules: Vec<Option<Regex>>,
    match_output: Vec<Option<CompiledMatchOutput>>,
    replacers: Vec<Option<Regex>>,
    strip_lines: Option<RegexSet>,
    keep_lines: Option<RegexSet>,
}

impl CompiledRuleProgram {
    fn new(key: &RuleProgramKey) -> Self {
        let line_rules = key
            .line_rules
            .iter()
            .map(|pattern| pattern.as_deref().and_then(compile_config_regex))
            .collect();
        let match_output = key
            .match_output
            .iter()
            .map(|entry| {
                let (pattern, unless) = entry.as_ref()?;
                let pattern = compile_config_regex(pattern)?;
                let unless = match unless {
                    Some(unless) => Some(compile_config_regex(unless)?),
                    None => None,
                };
                Some(CompiledMatchOutput { pattern, unless })
            })
            .collect();
        let replacers = key
            .replacers
            .iter()
            .map(|pattern| pattern.as_deref().and_then(compile_config_regex))
            .collect();
        Self {
            line_rules,
            match_output,
            replacers,
            strip_lines: compile_config_regex_set(&key.strip_lines),
            keep_lines: compile_config_regex_set(&key.keep_lines),
        }
    }
}

#[derive(Default)]
struct RuleProgramCache {
    entries: VecDeque<(RuleProgramKey, Arc<CompiledRuleProgram>)>,
}

impl RuleProgramCache {
    fn get(&mut self, key: &RuleProgramKey) -> Option<Arc<CompiledRuleProgram>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let compiled = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(compiled)
    }

    fn insert(
        &mut self,
        key: RuleProgramKey,
        compiled: Arc<CompiledRuleProgram>,
    ) -> Arc<CompiledRuleProgram> {
        if let Some(existing) = self.get(&key) {
            return existing;
        }
        if self.entries.len() == MAX_RULESET_CACHE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, Arc::clone(&compiled)));
        compiled
    }
}

fn compiled_program(ruleset: &CompactorRuleSet) -> Arc<CompiledRuleProgram> {
    let key = RuleProgramKey::new(ruleset);
    if let Ok(mut cache) = RULESET_CACHE.lock() {
        if let Some(compiled) = cache.get(&key) {
            return compiled;
        }
    }

    // Compile outside the global cache lock. A first-use custom grammar may be
    // moderately expensive even under the hard regex limits; unrelated output
    // rendering should not wait for it.
    let compiled = Arc::new(CompiledRuleProgram::new(&key));
    match RULESET_CACHE.lock() {
        Ok(mut cache) => cache.insert(key, compiled),
        Err(_) => compiled,
    }
}

/// Build a semantic summary by applying a compactor's rules to the output.
pub fn apply_compactor(ruleset: &CompactorRuleSet, cx: &CommandContext) -> SemanticSummary {
    let mut summary = SemanticSummary::new(cx, &ruleset.name);
    let compiled = compiled_program(ruleset);

    for (index, rule) in ruleset.rule.iter().take(MAX_RULE_ACTIONS).enumerate() {
        match rule.action.as_str() {
            "keep" => {
                if let Some(regex) = compiled.line_rules.get(index).and_then(Option::as_ref) {
                    apply_keep(rule, regex, cx, &mut summary);
                }
            }
            "group" => {
                if let Some(regex) = compiled.line_rules.get(index).and_then(Option::as_ref) {
                    apply_group(rule, regex, cx, &mut summary);
                }
            }
            "keep_tail" => apply_keep_tail(rule, cx, &mut summary),
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
        if let Some(empty_msg) = apply_declarative_reduce(ruleset, &compiled, cx, &mut summary) {
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
    compiled: &CompiledRuleProgram,
    cx: &CommandContext,
    summary: &mut SemanticSummary,
) -> Option<String> {
    // Treat both streams as reducer input without concatenating another copy of
    // potentially multi-megabyte observations. Regexes still see multiline
    // content within each stream; only a match spanning the artificial stream
    // boundary is intentionally unsupported.
    let both = [cx.stdout, cx.stderr];
    let primary = [if cx.stdout.trim().is_empty() {
        cx.stderr
    } else {
        cx.stdout
    }];
    let sources: &[&str] = if ruleset.filter_stderr {
        &both
    } else {
        &primary
    };

    // Stage 0: match_output short-circuit — collapse the whole output to a
    // message when matched, unless an error guard also matches.
    for (index, rule) in ruleset
        .match_output
        .iter()
        .take(MAX_RULE_ACTIONS)
        .enumerate()
    {
        let Some(entry) = compiled.match_output.get(index).and_then(Option::as_ref) else {
            continue;
        };
        if !sources.iter().any(|source| entry.pattern.is_match(source)) {
            continue;
        }
        let guarded = entry
            .unless
            .as_ref()
            .is_some_and(|guard| sources.iter().any(|source| guard.is_match(source)));
        if !guarded {
            return Some(rule.message.clone());
        }
    }

    let (capacity, overflow_head, overflow_tail) = window_limits(ruleset);
    let mut window = LineWindow::new(capacity, overflow_head, overflow_tail);
    let mut original_lines = 0usize;
    let mut seen_content = false;
    let mut pending_blank = false;

    for source in sources {
        for raw_line in source.lines() {
            original_lines = original_lines.saturating_add(1);
            let Some(line) = transform_line(ruleset, compiled, raw_line) else {
                continue;
            };

            // Collapse blank runs while streaming. Delaying the blank until a
            // following content line also removes leading/trailing blanks
            // without ever retaining the whole input.
            if line.trim().is_empty() {
                pending_blank |= seen_content;
                continue;
            }
            if pending_blank {
                window.push(String::new());
                pending_blank = false;
            }
            seen_content = true;
            window.push(line);
        }
    }

    let reduced = window.finish();
    if reduced.is_empty() {
        return ruleset.on_empty.clone();
    }
    // Count everything removed (strip + window), relative to the original output.
    let kept_real = reduced.iter().filter(|l| !l.starts_with("… (")).count();
    let dropped = original_lines.saturating_sub(kept_real);
    if dropped > 0 {
        summary.set_count("reduced_out", i64::try_from(dropped).unwrap_or(i64::MAX));
    }
    summary.set_body(reduced);
    None
}

fn transform_line(
    ruleset: &CompactorRuleSet,
    compiled: &CompiledRuleProgram,
    raw_line: &str,
) -> Option<String> {
    // Cap before substitution so `$0$0...` replacements cannot amplify one
    // enormous captured line into an equally enormous temporary allocation.
    let mut line = clip(raw_line, MAX_INTERMEDIATE_LINE_CHARS);
    for (index, rule) in ruleset.replace.iter().take(MAX_RULE_ACTIONS).enumerate() {
        let Some(regex) = compiled.replacers.get(index).and_then(Option::as_ref) else {
            continue;
        };
        line = regex
            .replacen(&line, MAX_REPLACEMENTS_PER_LINE, rule.replacement.as_str())
            .into_owned();
        line = clip(&line, MAX_INTERMEDIATE_LINE_CHARS);
    }

    if compiled
        .strip_lines
        .as_ref()
        .is_some_and(|set| set.is_match(&line))
    {
        return None;
    }
    if compiled
        .keep_lines
        .as_ref()
        .is_some_and(|set| !set.is_match(&line))
    {
        return None;
    }

    if let Some(last) = line.rsplit('\r').next() {
        if last.len() != line.len() {
            line = last.to_string();
        }
    }
    if ruleset.strip_ansi.unwrap_or(true) {
        line = strip_ansi(&line);
    }
    let truncate_at = ruleset
        .truncate_lines_at
        .unwrap_or(MAX_REDUCED_LINE_CHARS)
        .min(MAX_REDUCED_LINE_CHARS);
    Some(clip(&line, truncate_at))
}

fn window_limits(ruleset: &CompactorRuleSet) -> (usize, usize, usize) {
    let has_window = ruleset.head_lines.is_some() || ruleset.tail_lines.is_some();
    let requested = ruleset.max_lines.unwrap_or_else(|| {
        if has_window {
            ruleset
                .head_lines
                .unwrap_or(0)
                .saturating_add(ruleset.tail_lines.unwrap_or(0))
        } else {
            0
        }
    });
    if requested == 0 {
        return (MAX_REDUCED_BODY_LINES, MAX_REDUCED_BODY_LINES, 0);
    }

    let capacity = requested.min(MAX_REDUCED_BODY_LINES);
    if !has_window {
        return (capacity, capacity, 0);
    }
    let head = ruleset.head_lines.unwrap_or(0).min(capacity);
    let tail = ruleset
        .tail_lines
        .unwrap_or(0)
        .min(capacity.saturating_sub(head));
    (capacity, head, tail)
}

struct LineWindow {
    capacity: usize,
    overflow_head: usize,
    overflow_tail: usize,
    total: usize,
    prefix: Vec<String>,
    tail: VecDeque<String>,
}

impl LineWindow {
    fn new(capacity: usize, overflow_head: usize, overflow_tail: usize) -> Self {
        Self {
            capacity,
            overflow_head,
            overflow_tail,
            total: 0,
            prefix: Vec::with_capacity(capacity),
            tail: VecDeque::with_capacity(overflow_tail),
        }
    }

    fn push(&mut self, line: String) {
        self.total = self.total.saturating_add(1);
        if self.prefix.len() < self.capacity {
            if self.overflow_tail == 0 {
                self.prefix.push(line);
                return;
            }
            self.prefix.push(line.clone());
        }
        if self.overflow_tail > 0 {
            if self.tail.len() == self.overflow_tail {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
    }

    fn finish(self) -> Vec<String> {
        if self.total <= self.capacity {
            return self.prefix;
        }

        let head = self.overflow_head.min(self.prefix.len());
        let tail = self.overflow_tail.min(self.tail.len());
        let omitted = self.total.saturating_sub(head).saturating_sub(tail);
        let mut lines = Vec::with_capacity(head + tail + usize::from(omitted > 0));
        lines.extend(self.prefix.into_iter().take(head));
        if omitted > 0 {
            lines.push(format!("… ({omitted} lines omitted) …"));
        }
        let tail_skip = self.tail.len().saturating_sub(tail);
        lines.extend(self.tail.into_iter().skip(tail_skip));
        lines
    }
}

fn apply_keep(rule: &RuleSpec, regex: &Regex, cx: &CommandContext, summary: &mut SemanticSummary) {
    let limit = parse_limit(rule.limit.as_deref()).min(MAX_RULE_DETAILS);
    let errorish = rule_targets_errors(rule);
    let mut kept = 0usize;
    for line in cx.all_lines() {
        if kept >= limit {
            break;
        }
        if regex.is_match(line) {
            if errorish {
                if summary.failures.len() < MAX_RULE_DETAILS {
                    summary.add_failure(clip(line, MAX_LINE));
                }
            } else {
                if summary.notes.len() < MAX_RULE_DETAILS {
                    summary.add_note(clip(line, MAX_LINE));
                }
            }
            kept += 1;
        }
    }
}

fn apply_group(rule: &RuleSpec, regex: &Regex, cx: &CommandContext, summary: &mut SemanticSummary) {
    let count = cx
        .all_lines()
        .filter(|line| regex.is_match(line))
        .fold(0i64, |count, _| count.saturating_add(1));
    if count > 0 {
        let name = rule.group_name.clone().unwrap_or_else(|| rule.name.clone());
        summary.set_count(&name, count);
    }
}

fn apply_keep_tail(rule: &RuleSpec, cx: &CommandContext, summary: &mut SemanticSummary) {
    let n = rule.lines.unwrap_or(80).min(MAX_RULE_DETAILS);
    let mut tail = VecDeque::with_capacity(n);
    for line in cx.all_lines() {
        if !line.trim().is_empty() {
            if tail.len() == n {
                tail.pop_front();
            }
            if n > 0 {
                tail.push_back(line);
            }
        }
    }
    for line in tail {
        if summary.notes.len() < MAX_RULE_DETAILS {
            summary.add_note(clip(line, MAX_LINE));
        }
    }
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

    #[test]
    fn simple_rule_details_are_bounded_while_group_counts_remain_exact() {
        let toml = r#"
[[compactor]]
name = "bounded"
match.argv = ["bounded"]

[[compactor.rule]]
name = "keep-errors"
match.line_regex = "^ERROR"
action = "keep"
limit = "all"

[[compactor.rule]]
name = "count-errors"
match.line_regex = "^ERROR"
action = "group"
group_name = "errors_seen"

[[compactor.rule]]
name = "tail"
action = "keep_tail"
lines = 1000000
"#;
        let ruleset = CompactorConfig::from_toml(toml).compactors.remove(0);
        let stdout = (0..10_000)
            .map(|i| format!("ERROR unique-{i}\n"))
            .collect::<String>();
        let argv = vec!["bounded".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 1,
            stdout: &stdout,
            stderr: "",
        };

        let summary = apply_compactor(&ruleset, &cx);
        assert!(summary.failures.len() <= MAX_RULE_DETAILS);
        assert!(summary.notes.len() <= MAX_RULE_DETAILS);
        assert_eq!(summary.counts.get("errors_seen"), Some(&10_000));
        assert!(summary
            .notes
            .last()
            .is_some_and(|line| line.contains("9999")));
    }

    #[test]
    fn declarative_window_keeps_exact_head_tail_and_omission_count() {
        let toml = r#"
[[compactor]]
name = "window"
match.argv = ["window"]
head_lines = 2
tail_lines = 2
max_lines = 4
"#;
        let ruleset = CompactorConfig::from_toml(toml).compactors.remove(0);
        let stdout = (0..1_000)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        let argv = vec!["window".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 0,
            stdout: &stdout,
            stderr: "",
        };

        let summary = apply_compactor(&ruleset, &cx);
        assert_eq!(
            summary.body,
            [
                "line-0",
                "line-1",
                "… (996 lines omitted) …",
                "line-998",
                "line-999"
            ]
        );
        assert_eq!(summary.counts.get("reduced_out"), Some(&996));
    }

    #[test]
    fn replacement_expansion_and_default_body_are_hard_bounded() {
        let replacement = "$1".repeat(MAX_REPLACEMENT_BYTES / 2);
        let toml = format!(
            r#"
[[compactor]]
name = "replace"
match.argv = ["replace"]
replace = [{{ pattern = "(.+)", replacement = {replacement:?} }}]
"#
        );
        let ruleset = CompactorConfig::from_toml(&toml).compactors.remove(0);
        let long_line = "x".repeat(MAX_INTERMEDIATE_LINE_CHARS * 4);
        let remaining = (0..MAX_REDUCED_BODY_LINES + 100)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        let stdout = format!("{long_line}\n{remaining}");
        let argv = vec!["replace".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 0,
            stdout: &stdout,
            stderr: "",
        };

        let summary = apply_compactor(&ruleset, &cx);
        assert!(summary.body.len() <= MAX_REDUCED_BODY_LINES + 1);
        assert!(summary
            .body
            .iter()
            .filter(|line| !line.starts_with("… ("))
            .all(|line| line.chars().count() <= MAX_REDUCED_LINE_CHARS));
    }

    #[test]
    fn over_budget_unless_guard_disables_the_short_circuit() {
        let oversized_guard = "x".repeat(MAX_CONFIG_REGEX_BYTES + 1);
        let toml = format!(
            r#"
[[compactor]]
name = "guarded"
match.argv = ["guarded"]
max_lines = 10
match_output = [{{ pattern = "success", message = "hidden", unless = {oversized_guard:?} }}]
"#
        );
        let ruleset = CompactorConfig::from_toml(&toml).compactors.remove(0);
        let argv = vec!["guarded".to_string()];
        let cx = CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv: &argv,
            exit_code: 1,
            stdout: "success\nERROR: must remain visible\n",
            stderr: "",
        };

        let summary = apply_compactor(&ruleset, &cx);
        assert_ne!(summary.headline, "hidden");
        assert!(summary.body.iter().any(|line| line.contains("ERROR")));
    }

    #[test]
    fn compiled_rule_cache_is_exact_lru_and_bounded() {
        let mut cache = RuleProgramCache::default();
        let mut first_key = None;
        for i in 0..MAX_RULESET_CACHE_ENTRIES + 2 {
            let toml = format!(
                r#"
[[compactor]]
name = "cache-{i}"
match.argv = ["cache"]
strip_lines = ["^{i}$"]
"#
            );
            let ruleset = CompactorConfig::from_toml(&toml).compactors.remove(0);
            let key = RuleProgramKey::new(&ruleset);
            if i == 0 {
                first_key = Some(key.clone());
            }
            let compiled = Arc::new(CompiledRuleProgram::new(&key));
            let inserted = cache.insert(key.clone(), Arc::clone(&compiled));
            assert!(Arc::ptr_eq(&inserted, &compiled));
            assert!(Arc::ptr_eq(&cache.get(&key).unwrap(), &compiled));
        }

        assert_eq!(cache.entries.len(), MAX_RULESET_CACHE_ENTRIES);
        assert!(cache.get(&first_key.unwrap()).is_none());
    }
}
