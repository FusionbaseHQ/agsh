//! Deterministic secret redaction for observations. Applied only to the
//! observation stream, never to raw process output. No LLM judgment is used.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

pub const REDACTED: &str = "[REDACTED]";

/// Config files are bounded separately, but a short regex can still expand
/// into a large automaton (notably through Unicode classes). Keep each
/// user-supplied pattern within a predictable compilation/search budget.
pub(crate) const MAX_CONFIG_REGEX_BYTES: usize = 16 * 1024;
pub(crate) const MAX_CONFIG_REGEX_COUNT: usize = 64;
const CONFIG_REGEX_COMPILED_BYTES: usize = 512 * 1024;
const CONFIG_REGEX_DFA_BYTES: usize = 128 * 1024;
const CONFIG_REGEX_NEST_LIMIT: u32 = 64;
const MAX_TOTAL_CONFIG_REGEX_BYTES: usize = 64 * 1024;
const CONFIG_PATTERN_CACHE_ENTRIES: usize = 2;

const DEFAULT_PATTERN_STRINGS: &[&str] = &[
    r"ghp_[A-Za-z0-9_]{20,}", // GitHub personal access token
    r"gho_[A-Za-z0-9_]{20,}", // GitHub OAuth token
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"sk-[A-Za-z0-9_-]{20,}",        // OpenAI-style API key
    r"AKIA[0-9A-Z]{16}",             // AWS access key id
    r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack token
    r"AIza[0-9A-Za-z_\-]{30,}",      // Google API key
];

static DEFAULT_PATTERNS: LazyLock<Vec<Regex>> =
    LazyLock::new(|| compile_patterns(DEFAULT_PATTERN_STRINGS));
static CONFIG_PATTERN_CACHE: LazyLock<Mutex<PatternCache>> =
    LazyLock::new(|| Mutex::new(PatternCache::default()));

#[derive(Default)]
struct PatternCache {
    entries: VecDeque<(Vec<String>, Vec<Regex>)>,
}

impl PatternCache {
    fn get(&mut self, key: &[String]) -> Option<Vec<Regex>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let patterns = entry.1.clone();
        self.entries.push_back(entry);
        Some(patterns)
    }

    fn insert(&mut self, key: Vec<String>, patterns: Vec<Regex>) -> Vec<Regex> {
        if let Some(existing) = self.get(&key) {
            return existing;
        }
        if self.entries.len() == CONFIG_PATTERN_CACHE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, patterns.clone()));
        patterns
    }
}

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
    DEFAULT_PATTERNS.clone()
}

/// Compile pattern strings under the same count/byte limits used for config,
/// skipping any that fail to compile (deterministic, never panics).
pub fn compile_patterns(patterns: &[&str]) -> Vec<Regex> {
    let mut total_bytes = 0usize;
    patterns
        .iter()
        .take(MAX_CONFIG_REGEX_COUNT)
        .filter(|pattern| reserve_pattern_bytes(pattern, &mut total_bytes))
        .filter_map(|pattern| compile_config_regex(pattern))
        .collect()
}

pub fn compile_pattern_strings(patterns: &[String]) -> Vec<Regex> {
    let mut total_bytes = 0usize;
    let key = patterns
        .iter()
        .take(MAX_CONFIG_REGEX_COUNT)
        .filter(|pattern| reserve_pattern_bytes(pattern, &mut total_bytes))
        .cloned()
        .collect::<Vec<_>>();
    if let Ok(mut cache) = CONFIG_PATTERN_CACHE.lock() {
        if let Some(compiled) = cache.get(&key) {
            return compiled;
        }
    }

    let compiled = key
        .iter()
        .filter_map(|pattern| compile_config_regex(pattern))
        .collect::<Vec<_>>();
    match CONFIG_PATTERN_CACHE.lock() {
        Ok(mut cache) => cache.insert(key, compiled),
        Err(_) => compiled,
    }
}

fn reserve_pattern_bytes(pattern: &str, total_bytes: &mut usize) -> bool {
    if pattern.len() > MAX_CONFIG_REGEX_BYTES {
        return false;
    }
    let Some(next) = total_bytes.checked_add(pattern.len()) else {
        return false;
    };
    if next > MAX_TOTAL_CONFIG_REGEX_BYTES {
        return false;
    }
    *total_bytes = next;
    true
}

/// Compile one configuration-provided regex under fixed parser, automaton and
/// lazy-DFA limits. Invalid or over-budget patterns are ignored exactly like
/// malformed patterns were before, but cannot reserve the regex crate's much
/// larger defaults for every entry in a hostile config.
pub(crate) fn compile_config_regex(pattern: &str) -> Option<Regex> {
    if pattern.len() > MAX_CONFIG_REGEX_BYTES {
        return None;
    }
    RegexBuilder::new(pattern)
        .size_limit(CONFIG_REGEX_COMPILED_BYTES)
        .dfa_size_limit(CONFIG_REGEX_DFA_BYTES)
        .nest_limit(CONFIG_REGEX_NEST_LIMIT)
        .build()
        .ok()
}

/// Compile a set of configuration patterns under the same aggregate limits.
/// `RegexSet` shares one automaton, avoiding a search per strip/keep pattern.
pub(crate) fn compile_config_regex_set(patterns: &[String]) -> Option<RegexSet> {
    if patterns.is_empty()
        || patterns.len() > MAX_CONFIG_REGEX_COUNT
        || patterns
            .iter()
            .any(|pattern| pattern.len() > MAX_CONFIG_REGEX_BYTES)
    {
        return None;
    }
    RegexSetBuilder::new(patterns)
        .size_limit(CONFIG_REGEX_COMPILED_BYTES)
        .dfa_size_limit(CONFIG_REGEX_DFA_BYTES)
        .nest_limit(CONFIG_REGEX_NEST_LIMIT)
        .build()
        .ok()
}

/// Deterministically classify environment names whose values should be treated
/// as secrets in observations. This is deliberately name-based: it never asks a
/// model to infer sensitivity and avoids broad substrings such as `TOKEN` in
/// `TOKENIZERS_PARALLELISM`.
pub fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const EXACT: &[&str] = &[
        "DATABASE_URL",
        "MONGODB_URI",
        "REDIS_URL",
        "SQLALCHEMY_DATABASE_URI",
        "SSH_AUTH_SOCK",
    ];
    const WORDS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "ACCESS_KEY",
        "CREDENTIAL",
        "CREDENTIALS",
        "COOKIE",
    ];

    EXACT.contains(&upper.as_str())
        || WORDS.iter().any(|word| {
            upper == *word || upper.strip_suffix(word).is_some_and(|p| p.ends_with('_'))
        })
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
        if secret.len() >= 4 && text.contains(secret) {
            text = text.replace(secret.as_str(), REDACTED);
        }
    }
    for pattern in &options.patterns {
        if let Cow::Owned(redacted) = pattern.replace_all(&text, REDACTED) {
            text = redacted;
        }
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

    #[test]
    fn recognizes_sensitive_environment_names() {
        for name in [
            "GITHUB_TOKEN",
            "MY_PRIVATE_TOKEN",
            "SERVICE_PASSWORD",
            "OPENAI_API_KEY",
            "DEPLOY_CREDENTIALS",
            "DATABASE_URL",
        ] {
            assert!(is_sensitive_env_name(name), "expected sensitive: {name}");
        }
        for name in ["PATH", "HOME", "RUST_LOG", "TOKENIZERS_PARALLELISM"] {
            assert!(!is_sensitive_env_name(name), "false positive: {name}");
        }
    }

    #[test]
    fn configured_patterns_have_count_and_size_limits() {
        let oversized = "x".repeat(MAX_CONFIG_REGEX_BYTES + 1);
        assert!(compile_config_regex(&oversized).is_none());

        let patterns = (0..MAX_CONFIG_REGEX_COUNT + 10)
            .map(|i| format!("token-{i}"))
            .collect::<Vec<_>>();
        assert_eq!(
            compile_pattern_strings(&patterns).len(),
            MAX_CONFIG_REGEX_COUNT
        );
    }

    #[test]
    fn default_patterns_are_compiled_once_and_cloned_cheaply() {
        let first = default_patterns();
        let second = default_patterns();
        assert_eq!(first.len(), DEFAULT_PATTERN_STRINGS.len());
        assert_eq!(first[0].as_str(), second[0].as_str());
        assert!(first[0].is_match("ghp_abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn configured_pattern_cache_is_exact_lru_and_bounded() {
        let mut cache = PatternCache::default();
        for i in 0..CONFIG_PATTERN_CACHE_ENTRIES + 2 {
            let key = vec![format!("^token-{i}$")];
            let compiled = vec![compile_config_regex(&key[0]).unwrap()];
            let inserted = cache.insert(key.clone(), compiled);
            assert_eq!(inserted[0].as_str(), key[0]);
            assert_eq!(cache.get(&key).unwrap()[0].as_str(), key[0]);
        }
        assert_eq!(cache.entries.len(), CONFIG_PATTERN_CACHE_ENTRIES);
        assert!(cache.get(&["^token-0$".to_string()]).is_none());
    }
}
