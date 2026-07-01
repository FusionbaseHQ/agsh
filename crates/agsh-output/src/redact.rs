//! Deterministic secret redaction for observations. Applied only to the
//! observation stream, never to raw process output. No LLM judgment is used.

use regex::Regex;

pub const REDACTED: &str = "[REDACTED]";

/// Redaction options: literal secret values (e.g. the value of `$GITHUB_TOKEN`)
/// and compiled token-shaped patterns.
///
/// `Debug` is hand-written to NEVER print the cleartext `literal_secrets` — a
/// derived `Debug` would leak every live secret through any stray `{:?}` in a
/// panic message, `log::debug!`, or serde dump, which is exactly the material this
/// type concentrates.
#[derive(Clone, Default)]
pub struct RedactOptions {
    pub enabled: bool,
    pub literal_secrets: Vec<String>,
    pub patterns: Vec<Regex>,
}

impl std::fmt::Debug for RedactOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedactOptions")
            .field("enabled", &self.enabled)
            .field(
                "literal_secrets",
                &format_args!("<{} hidden>", self.literal_secrets.len()),
            )
            .field(
                "patterns",
                &format_args!("<{} patterns>", self.patterns.len()),
            )
            .finish()
    }
}

impl RedactOptions {
    pub fn with_defaults(literal_secrets: Vec<String>) -> Self {
        Self {
            enabled: true,
            literal_secrets,
            patterns: default_patterns(),
        }
    }
}

/// Built-in token-shaped patterns redacted by default.
pub fn default_patterns() -> Vec<Regex> {
    compile_patterns(&[
        r"ghp_[A-Za-z0-9_]{20,}", // GitHub personal access token
        r"gho_[A-Za-z0-9_]{20,}", // GitHub OAuth token
        r"github_pat_[A-Za-z0-9_]{20,}",
        r"sk-[A-Za-z0-9_-]{20,}",        // OpenAI-style API key
        r"AKIA[0-9A-Z]{16}",             // AWS access key id
        r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack token
        r"AIza[0-9A-Za-z_\-]{30,}",      // Google API key
    ])
}

/// Compile pattern strings, skipping any that fail to compile (deterministic,
/// never panics on bad config).
pub fn compile_patterns(patterns: &[&str]) -> Vec<Regex> {
    patterns.iter().filter_map(|p| Regex::new(p).ok()).collect()
}

pub fn compile_pattern_strings(patterns: &[String]) -> Vec<Regex> {
    patterns.iter().filter_map(|p| Regex::new(p).ok()).collect()
}

/// Redact secrets from `input`. Literal secret values are matched first, then
/// token-shaped patterns.
pub fn redact(input: &str, options: &RedactOptions) -> String {
    if !options.enabled {
        return input.to_string();
    }
    let mut text = input.to_string();
    for secret in &options.literal_secrets {
        // Only redact substantial values to avoid masking short, common strings.
        if secret.len() >= 4 {
            text = text.replace(secret.as_str(), REDACTED);
        }
    }
    for pattern in &options.patterns {
        text = pattern.replace_all(&text, REDACTED).into_owned();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_literal_secrets() {
        let options = RedactOptions::with_defaults(vec![
            "supersecretvalue".to_string(),
            "ghp_realtokenmaterial12345".to_string(),
        ]);
        let shown = format!("{options:?}");
        assert!(
            !shown.contains("supersecretvalue"),
            "Debug leaked a secret: {shown}"
        );
        assert!(
            !shown.contains("ghp_realtokenmaterial"),
            "Debug leaked a secret: {shown}"
        );
        assert!(
            shown.contains("<2 hidden>"),
            "expected a hidden-count: {shown}"
        );
    }

    #[test]
    fn redacts_token_patterns() {
        let options = RedactOptions::with_defaults(Vec::new());
        let out = redact("token=ghp_abcdefghijklmnopqrstuvwxyz0123", &options);
        assert_eq!(out, "token=[REDACTED]");
        let out = redact("key sk-abcdefghijklmnopqrstuvwx done", &options);
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_literal_env_values() {
        let options = RedactOptions {
            enabled: true,
            literal_secrets: vec!["supersecretvalue".to_string()],
            patterns: Vec::new(),
        };
        assert_eq!(
            redact("export X=supersecretvalue", &options),
            "export X=[REDACTED]"
        );
    }

    #[test]
    fn disabled_passes_through() {
        let options = RedactOptions {
            enabled: false,
            literal_secrets: vec!["secret".to_string()],
            patterns: default_patterns(),
        };
        assert_eq!(
            redact("secret ghp_aaaaaaaaaaaaaaaaaaaaaa", &options),
            "secret ghp_aaaaaaaaaaaaaaaaaaaaaa"
        );
    }
}
