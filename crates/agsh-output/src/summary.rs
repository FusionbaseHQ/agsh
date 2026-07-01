//! The structured semantic summary produced by family compactors.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::budget::estimate_tokens;
use crate::util::shell_join;

/// Inputs available to a family compactor. Text is already normalized/redacted.
pub struct CommandContext<'a> {
    pub cmd_id: String,
    pub argv: &'a [String],
    pub exit_code: i32,
    pub stdout: &'a str,
    pub stderr: &'a str,
}

impl CommandContext<'_> {
    /// stdout and stderr lines chained together.
    pub fn all_lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines().chain(self.stderr.lines())
    }
}

/// A compact, structured observation of a command's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticSummary {
    pub command: String,
    pub family: String,
    pub exit_code: i32,
    pub status: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub headline: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Reduced output lines (for generic/long-tail commands without a rich
    /// structured summary). Rendered verbatim, not as a bulleted section.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<String>,
    /// Reference to the full raw stdout — set only when the compact view actually
    /// elides output (otherwise the raw is already shown, so no pointer is useful).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_stderr: Option<String>,
}

impl SemanticSummary {
    pub fn new(cx: &CommandContext, family: &str) -> Self {
        Self {
            command: shell_join(cx.argv),
            family: family.to_string(),
            exit_code: cx.exit_code,
            status: if cx.exit_code == 0 { "ok" } else { "failed" }.to_string(),
            headline: String::new(),
            counts: BTreeMap::new(),
            failures: Vec::new(),
            warnings: Vec::new(),
            paths: Vec::new(),
            notes: Vec::new(),
            body: Vec::new(),
            // Populated by the render layer only when output is actually elided.
            raw_stdout: None,
            raw_stderr: None,
        }
    }

    /// Attach raw-output references (shown as a `raw:` line). Called by the render
    /// layer when the compact view dropped content the caller might want back.
    pub fn set_raw_refs(&mut self, stdout: impl Into<String>, stderr: impl Into<String>) {
        self.raw_stdout = Some(stdout.into());
        self.raw_stderr = Some(stderr.into());
    }

    pub fn set_headline(&mut self, headline: impl Into<String>) -> &mut Self {
        self.headline = headline.into();
        self
    }

    pub fn set_count(&mut self, key: &str, value: i64) -> &mut Self {
        self.counts.insert(key.to_string(), value);
        self
    }

    pub fn add_failure(&mut self, line: impl Into<String>) -> &mut Self {
        push_unique(&mut self.failures, line.into());
        self
    }

    pub fn add_warning(&mut self, line: impl Into<String>) -> &mut Self {
        push_unique(&mut self.warnings, line.into());
        self
    }

    pub fn add_path(&mut self, path: impl Into<String>) -> &mut Self {
        push_unique(&mut self.paths, path.into());
        self
    }

    pub fn add_note(&mut self, note: impl Into<String>) -> &mut Self {
        push_unique(&mut self.notes, note.into());
        self
    }

    /// Set the reduced output body (lines rendered verbatim).
    pub fn set_body(&mut self, lines: Vec<String>) -> &mut Self {
        self.body = lines;
        self
    }

    /// Cap the per-section detail lists to keep summaries bounded.
    pub fn cap_sections(&mut self, max: usize) {
        truncate_with_note(&mut self.failures, max);
        truncate_with_note(&mut self.warnings, max);
        truncate_with_note(&mut self.paths, max);
        truncate_with_note(&mut self.notes, max);
        // The body is the bulk; allow more lines than the bulleted sections.
        truncate_with_note(&mut self.body, max.saturating_mul(3).max(max));
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// A human-readable compact rendering of the summary.
    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} [{}]\n", self.command, self.status));
        if !self.headline.is_empty() {
            out.push_str(&self.headline);
            out.push('\n');
        }
        if !self.counts.is_empty() {
            let counts = self
                .counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("counts: {counts}\n"));
        }
        render_list(&mut out, "failures", &self.failures);
        render_list(&mut out, "warnings", &self.warnings);
        render_list(&mut out, "paths", &self.paths);
        render_list(&mut out, "notes", &self.notes);
        // Reduced output body, rendered verbatim (no bullet prefix).
        for line in &self.body {
            out.push_str(line);
            out.push('\n');
        }
        if let (Some(stdout), Some(stderr)) = (&self.raw_stdout, &self.raw_stderr) {
            out.push_str(&format!("raw: {stdout} {stderr}\n"));
        }
        out
    }

    pub fn token_estimate(&self) -> usize {
        estimate_tokens(&self.to_json())
    }
}

fn render_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    out.push_str(&format!("{label}:\n"));
    for item in items {
        out.push_str("- ");
        out.push_str(item);
        out.push('\n');
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if value.is_empty() || list.iter().any(|v| v == &value) {
        return;
    }
    list.push(value);
}

fn truncate_with_note(list: &mut Vec<String>, max: usize) {
    if list.len() > max {
        let dropped = list.len() - max;
        list.truncate(max);
        list.push(format!("… (+{dropped} more)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(argv: &'a [String], exit: i32) -> CommandContext<'a> {
        CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv,
            exit_code: exit,
            stdout: "",
            stderr: "",
        }
    }

    #[test]
    fn json_includes_status_and_refs() {
        let argv = vec!["pytest".to_string()];
        let mut s = SemanticSummary::new(&ctx(&argv, 1), "pytest");
        s.set_headline("1 failed")
            .set_count("failed", 1)
            .add_failure("test_a");
        // Refs are attached by the render layer only when output is elided.
        assert!(!s.to_json().contains("trace://"), "no ref until attached");
        s.set_raw_refs("trace://cmd_1/stdout", "trace://cmd_1/stderr");
        let json = s.to_json();
        assert!(json.contains("\"status\": \"failed\""));
        assert!(json.contains("trace://cmd_1/stdout"));
        assert!(json.contains("\"failed\": 1"));
    }

    #[test]
    fn caps_sections() {
        let argv = vec!["x".to_string()];
        let mut s = SemanticSummary::new(&ctx(&argv, 0), "generic");
        for i in 0..10 {
            s.add_failure(format!("f{i}"));
        }
        s.cap_sections(3);
        assert_eq!(s.failures.len(), 4); // 3 + the "+N more" note
        assert!(s.failures.last().unwrap().contains("more"));
    }
}
