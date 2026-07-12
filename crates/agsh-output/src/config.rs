//! Token-economy configuration loaded from `~/.config/agsh/token.toml`.
//!
//! The config is auditable and deterministic: parse failures fall back to
//! built-in defaults rather than panicking, and no value is interpreted by an
//! LLM. It drives normalization, redaction, budgeting, and the configurable
//! `[[compactor]]` rule engine.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::budget::BudgetOptions;
use crate::normalize::NormalizeOptions;
use crate::redact::{compile_pattern_strings, default_patterns, RedactOptions};
use crate::OutputMode;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Default and absolute ceiling for one command's combined persisted stdout and
/// stderr. The hard ceiling is intentionally not configurable: a malformed or
/// hostile local config must not turn trace capture back into an unbounded disk
/// sink.
pub const DEFAULT_MAX_RAW_BYTES: u64 = 100 * 1024 * 1024;
pub const HARD_MAX_RAW_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawStorageOptions {
    pub enabled: bool,
    pub max_bytes: u64,
}

fn read_config_file(path: &Path) -> io::Result<String> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "config path is not a regular file",
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config exceeds {MAX_CONFIG_BYTES} bytes"),
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config exceeds {MAX_CONFIG_BYTES} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CompactorConfig {
    pub mode: ModeConfig,
    pub storage: StorageConfig,
    pub budget: BudgetConfig,
    pub normalization: NormalizationConfig,
    pub security: SecurityConfig,
    pub session: SessionConfig,
    #[serde(rename = "compactor")]
    pub compactors: Vec<CompactorRuleSet>,
}

/// `[session]` — session-resilience knobs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Show the startup banner when a dead session likely lost work. Off by
    /// default — even a good heuristic interrupts people who end sessions by
    /// closing windows all day; `resume list` always works regardless.
    /// `AGSH_RESUME_BANNER=1|0` overrides this at runtime.
    pub restore_banner: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModeConfig {
    /// An explicit session default that overrides the context-specific ones
    /// (e.g. `default = "compact"`). Applied to interactive sessions only.
    pub default: Option<String>,
    pub human_default: String,
    pub agent_default: String,
    pub ci_default: String,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            default: None,
            human_default: "raw".to_string(),
            agent_default: "semantic".to_string(),
            ci_default: "raw".to_string(),
        }
    }
}

impl ModeConfig {
    /// The default output mode for an interactive session: the explicit `default`
    /// if set, otherwise `human_default`. Returns `None` if the configured name
    /// can't be parsed (callers then keep their own default, e.g. `raw`).
    pub fn interactive_default(&self) -> Option<OutputMode> {
        let name = self.default.as_deref().unwrap_or(&self.human_default);
        <OutputMode as std::str::FromStr>::from_str(name).ok()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub store_raw: bool,
    pub raw_retention: String,
    pub max_raw_per_command: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            store_raw: true,
            raw_retention: "14d".to_string(),
            max_raw_per_command: "100mb".to_string(),
        }
    }
}

impl StorageConfig {
    fn max_raw_bytes(&self) -> u64 {
        parse_byte_size(&self.max_raw_per_command)
            .unwrap_or(DEFAULT_MAX_RAW_BYTES)
            .min(HARD_MAX_RAW_BYTES)
    }
}

fn parse_byte_size(value: &str) -> Option<u64> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "");
    let digit_end = normalized
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(normalized.len());
    if digit_end == 0 {
        return None;
    }
    let number = normalized[..digit_end]
        .parse::<u64>()
        .unwrap_or(HARD_MAX_RAW_BYTES);
    let multiplier = match normalized[digit_end..].trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    number.checked_mul(multiplier).or(Some(HARD_MAX_RAW_BYTES))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub default_tokens: usize,
    pub max_tokens: usize,
    pub fallback: String,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            default_tokens: 2000,
            max_tokens: 8000,
            fallback: "lossless-ref".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NormalizationConfig {
    pub strip_ansi: bool,
    pub collapse_progress: bool,
    pub dedupe_repeated_lines: bool,
    pub shorten_home: bool,
    pub shorten_workspace: bool,
}

impl Default for NormalizationConfig {
    fn default() -> Self {
        Self {
            strip_ansi: true,
            collapse_progress: true,
            dedupe_repeated_lines: true,
            shorten_home: true,
            shorten_workspace: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    pub redact_secrets: bool,
    pub redact_env_names: Vec<String>,
    pub redact_patterns: Vec<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            redact_env_names: vec![
                "AWS_SECRET_ACCESS_KEY".to_string(),
                "AWS_SESSION_TOKEN".to_string(),
                "GITHUB_TOKEN".to_string(),
                "GH_TOKEN".to_string(),
                "OPENAI_API_KEY".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ],
            redact_patterns: Vec::new(),
        }
    }
}

/// A configurable `[[compactor]]` matching a command family with ordered rules.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CompactorRuleSet {
    pub name: String,
    #[serde(rename = "match")]
    pub match_spec: MatchSpec,
    pub mode: Option<String>,
    pub priority: i64,
    pub rule: Vec<RuleSpec>,
    // Declarative line-reduction (ported from rtk's TOML filter DSL). When any
    // of these is set, a reduced `body` is produced from the command output.
    /// Strip ANSI escapes before filtering (default true when reducing).
    pub strip_ansi: Option<bool>,
    /// Regex substitutions applied to each line, in order.
    pub replace: Vec<ReplaceRule>,
    /// Drop lines matching any of these regexes.
    pub strip_lines: Vec<String>,
    /// Keep ONLY lines matching any of these regexes (applied after strip).
    pub keep_lines: Vec<String>,
    /// Clip each line to at most N characters.
    pub truncate_lines_at: Option<usize>,
    /// Keep the first N lines.
    pub head_lines: Option<usize>,
    /// Keep the last N lines.
    pub tail_lines: Option<usize>,
    /// Absolute cap on body lines.
    pub max_lines: Option<usize>,
    /// Headline to use when the reduced body is empty (e.g. "make: ok").
    pub on_empty: Option<String>,
    /// Short-circuit rules: collapse the whole output to a message when matched.
    pub match_output: Vec<MatchOutputRule>,
    /// Also feed stderr (merged after stdout) into the reducer.
    pub filter_stderr: bool,
}

impl CompactorRuleSet {
    /// Whether this ruleset declares any line-reduction operations.
    pub fn has_reduce(&self) -> bool {
        self.strip_ansi.is_some()
            || !self.replace.is_empty()
            || !self.strip_lines.is_empty()
            || !self.keep_lines.is_empty()
            || self.truncate_lines_at.is_some()
            || self.head_lines.is_some()
            || self.tail_lines.is_some()
            || self.max_lines.is_some()
            || self.on_empty.is_some()
            || !self.match_output.is_empty()
            || self.filter_stderr
    }
}

/// A regex substitution applied to each line (rtk `replace` rule).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReplaceRule {
    pub pattern: String,
    pub replacement: String,
}

/// Short-circuit the whole output to `message` when `pattern` matches — unless
/// the optional `unless` pattern also matches (so errors are never swallowed).
/// Ported from rtk's `match_output`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MatchOutputRule {
    pub pattern: String,
    pub message: String,
    pub unless: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MatchSpec {
    pub argv: Vec<String>,
    pub line_regex: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RuleSpec {
    pub name: String,
    #[serde(rename = "match")]
    pub match_spec: MatchSpec,
    pub action: String,
    pub limit: Option<String>,
    pub group_name: Option<String>,
    pub lines: Option<usize>,
}

impl CompactorConfig {
    /// Load config from `$AGSH_TOKEN_CONFIG` or `$HOME/.config/agsh/token.toml`,
    /// then merge the built-in command presets (low priority, so user/project
    /// compactors always win). A missing file yields defaults silently; a malformed
    /// file is salvaged per-section (a bad `[[compactor]]` rule or `[budget]` is
    /// dropped with a named warning while the rest still loads) so one typo can't
    /// silently revert the whole token economy.
    pub fn load() -> Self {
        let (mut cfg, warnings) = match Self::config_path() {
            Some(path) => match read_config_file(&path) {
                Ok(text) => Self::parse_resilient(&text),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    (Self::default(), Vec::new())
                }
                Err(error) => (
                    Self::default(),
                    vec![format!("cannot read {} ({error})", path.display())],
                ),
            },
            None => (Self::default(), Vec::new()),
        };
        for warning in warnings {
            eprintln!("agsh: token.toml: {warning}");
        }
        cfg.merge_builtin_presets();
        cfg
    }

    /// Append the bundled command presets at a very low priority, so any
    /// user-defined `[[compactor]]` (default priority 0) overrides them.
    pub fn merge_builtin_presets(&mut self) {
        const BUILTIN: &str = include_str!("presets.toml");
        if let Ok(parsed) = toml::from_str::<CompactorConfig>(BUILTIN) {
            for mut preset in parsed.compactors {
                preset.priority = -1_000;
                self.compactors.push(preset);
            }
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("AGSH_TOKEN_CONFIG") {
            return Some(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/agsh/token.toml"))
    }

    /// Parse a TOML string, falling back to defaults on error.
    pub fn from_toml(text: &str) -> Self {
        Self::parse_resilient(text).0
    }

    /// Parse resiliently: if the whole document is valid, use it; otherwise salvage
    /// each recognized section (`[mode]`, `[storage]`, `[budget]`, `[normalization]`,
    /// `[security]`, `[session]`) and each `[[compactor]]` entry independently,
    /// dropping only the malformed ones and returning a named warning for each.
    /// Preserves compactor order (and thus the user-rules-beat-presets precedence).
    pub fn parse_resilient(text: &str) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        // Fast path: a fully valid document.
        if let Ok(cfg) = toml::from_str::<Self>(text) {
            return (cfg, warnings);
        }
        // Otherwise parse to a generic table and rebuild section by section.
        let table: toml::Table = match toml::from_str(text) {
            Ok(table) => table,
            Err(error) => {
                warnings.push(format!("not valid TOML, using defaults ({error})"));
                return (Self::default(), warnings);
            }
        };

        fn section<T: for<'de> Deserialize<'de>>(
            table: &toml::Table,
            key: &str,
            warnings: &mut Vec<String>,
        ) -> Option<T> {
            let value = table.get(key)?;
            match value.clone().try_into::<T>() {
                Ok(parsed) => Some(parsed),
                Err(error) => {
                    warnings.push(format!("[{key}] ignored, using defaults for it ({error})"));
                    None
                }
            }
        }

        let mut cfg = Self::default();
        if let Some(v) = section(&table, "mode", &mut warnings) {
            cfg.mode = v;
        }
        if let Some(v) = section(&table, "storage", &mut warnings) {
            cfg.storage = v;
        }
        if let Some(v) = section(&table, "budget", &mut warnings) {
            cfg.budget = v;
        }
        if let Some(v) = section(&table, "normalization", &mut warnings) {
            cfg.normalization = v;
        }
        if let Some(v) = section(&table, "security", &mut warnings) {
            cfg.security = v;
        }
        if let Some(v) = section(&table, "session", &mut warnings) {
            cfg.session = v;
        }
        if let Some(array) = table.get("compactor").and_then(|v| v.as_array()) {
            for (i, entry) in array.iter().enumerate() {
                match entry.clone().try_into::<CompactorRuleSet>() {
                    Ok(rule) => cfg.compactors.push(rule),
                    Err(error) => {
                        warnings.push(format!("[[compactor]] #{} ignored ({error})", i + 1));
                    }
                }
            }
        }
        (cfg, warnings)
    }

    pub fn budget_options(&self) -> BudgetOptions {
        BudgetOptions {
            default_tokens: self.budget.default_tokens,
            max_tokens: self.budget.max_tokens,
            fallback: self
                .budget
                .fallback
                .parse()
                .unwrap_or(OutputMode::LosslessRef),
        }
    }

    /// Effective persisted raw-trace policy. `store_raw = false` is represented
    /// as a zero-byte disabled sink so capture readers can continue draining
    /// children without writing their bytes anywhere.
    pub fn raw_storage_options(&self) -> RawStorageOptions {
        if !self.storage.store_raw {
            return RawStorageOptions {
                enabled: false,
                max_bytes: 0,
            };
        }
        RawStorageOptions {
            enabled: true,
            max_bytes: self.storage.max_raw_bytes(),
        }
    }

    pub fn normalize_options(
        &self,
        home: Option<String>,
        workspace: Option<String>,
    ) -> NormalizeOptions {
        NormalizeOptions {
            strip_ansi: self.normalization.strip_ansi,
            collapse_progress: self.normalization.collapse_progress,
            dedupe_repeated_lines: self.normalization.dedupe_repeated_lines,
            shorten_home: self.normalization.shorten_home,
            shorten_workspace: self.normalization.shorten_workspace,
            home,
            workspace,
        }
    }

    pub fn redact_options(&self, literal_secrets: Vec<String>) -> RedactOptions {
        let mut patterns = default_patterns();
        patterns.extend(compile_pattern_strings(&self.security.redact_patterns));
        RedactOptions {
            enabled: self.security.redact_secrets,
            literal_secrets,
            patterns,
        }
    }

    /// The highest-priority compactor whose argv matcher matches the command.
    pub fn matching_compactor(&self, argv: &[String]) -> Option<&CompactorRuleSet> {
        self.compactors
            .iter()
            .filter(|c| argv_matches(&c.match_spec.argv, argv))
            .max_by_key(|c| c.priority)
    }
}

/// Match a command's argv against a `match.argv` pattern list. Each pattern
/// element is glob-matched (`*` wildcard) against the corresponding argv element
/// (the first against the program basename). A trailing `*` matches any
/// remaining args.
pub fn argv_matches(pattern: &[String], argv: &[String]) -> bool {
    if pattern.is_empty() || argv.is_empty() {
        return false;
    }
    // Compare the program by basename for the first element.
    let prog = argv[0].rsplit(['/', '\\']).next().unwrap_or(&argv[0]);
    if !glob_match(&pattern[0], prog) {
        return false;
    }
    for (i, pat) in pattern.iter().enumerate().skip(1) {
        if pat == "*" {
            // A trailing `*` matches the rest; a middle `*` matches one token.
            if i + 1 == pattern.len() {
                return true;
            }
            continue;
        }
        match argv.get(i) {
            Some(arg) if glob_match(pat, arg) => {}
            _ => return false,
        }
    }
    true
}

/// Minimal glob: `*` matches any run of characters. No other metacharacters.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    // A trailing non-empty part must match the end.
    if let Some(last) = parts.last() {
        if !last.is_empty() && !text.ends_with(last) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agsh-output-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    #[test]
    fn config_file_read_is_bounded() {
        let path = temporary_file("oversized-config");
        std::fs::write(&path, vec![b'x'; MAX_CONFIG_BYTES + 1]).unwrap();
        let error = read_config_file(&path).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn config_file_rejects_non_regular_files() {
        let error = read_config_file(std::path::Path::new("/dev/zero")).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error}");
    }

    #[test]
    fn defaults_when_empty() {
        let cfg = CompactorConfig::from_toml("");
        assert_eq!(cfg.budget.default_tokens, 2000);
        assert_eq!(cfg.mode.agent_default, "semantic");
        assert!(cfg.security.redact_secrets);
        assert!(
            !cfg.session.restore_banner,
            "restore banner must be OPT-IN (it interrupts every new shell)"
        );
    }

    #[test]
    fn raw_trace_limit_parses_units_and_clamps_absurd_values() {
        let cfg = CompactorConfig::from_toml("[storage]\nmax_raw_per_command = \"2kb\"\n");
        assert_eq!(cfg.raw_storage_options().max_bytes, 2 * 1024);

        let cfg = CompactorConfig::from_toml(
            "[storage]\nmax_raw_per_command = \"999999999999999999999gb\"\n",
        );
        assert_eq!(cfg.raw_storage_options().max_bytes, HARD_MAX_RAW_BYTES);

        let cfg = CompactorConfig::from_toml("[storage]\nmax_raw_per_command = \"not-a-size\"\n");
        assert_eq!(cfg.raw_storage_options().max_bytes, DEFAULT_MAX_RAW_BYTES);
    }

    #[test]
    fn disabling_raw_storage_produces_a_zero_byte_policy() {
        let cfg = CompactorConfig::from_toml(
            "[storage]\nstore_raw = false\nmax_raw_per_command = \"10mb\"\n",
        );
        let storage = cfg.raw_storage_options();
        assert!(!storage.enabled);
        assert_eq!(storage.max_bytes, 0);
    }

    #[test]
    fn session_section_enables_the_restore_banner() {
        let cfg = CompactorConfig::from_toml("[session]\nrestore_banner = true\n");
        assert!(cfg.session.restore_banner);
    }

    #[test]
    fn bad_compactor_rule_is_dropped_but_the_rest_survives() {
        // A valid [budget]/[mode] plus one good and one malformed [[compactor]].
        let text = r#"
[budget]
default_tokens = 1234

[mode]
default = "compact"

[[compactor]]
name = "cargo-rule"
match.argv = ["cargo", "*"]
mode = "semantic"

[[compactor]]
name = "bad-rule"
match.argv = "not-an-array"
"#;
        let (cfg, warnings) = CompactorConfig::parse_resilient(text);
        // The good sections still load (no silent full revert).
        assert_eq!(
            cfg.budget.default_tokens, 1234,
            "good [budget] must survive"
        );
        assert_eq!(cfg.mode.default.as_deref(), Some("compact"));
        // The good rule loads; the malformed one is dropped with a named warning.
        assert_eq!(cfg.compactors.len(), 1, "only the good rule survives");
        assert!(
            warnings.iter().any(|w| w.contains("[[compactor]] #2")),
            "expected a named warning for the bad rule: {warnings:?}"
        );
    }

    #[test]
    fn a_bad_section_falls_back_only_for_that_section() {
        let text = r#"
[budget]
default_tokens = "not-a-number"

[mode]
default = "compact"
"#;
        let (cfg, warnings) = CompactorConfig::parse_resilient(text);
        assert_eq!(
            cfg.budget.default_tokens, 2000,
            "bad [budget] → its defaults"
        );
        assert_eq!(
            cfg.mode.default.as_deref(),
            Some("compact"),
            "[mode] still loads"
        );
        assert!(
            warnings.iter().any(|w| w.contains("[budget]")),
            "{warnings:?}"
        );
    }

    #[test]
    fn valid_config_produces_no_warnings() {
        let (_, warnings) = CompactorConfig::parse_resilient("[budget]\ndefault_tokens = 500\n");
        assert!(warnings.is_empty());
    }

    #[test]
    fn interactive_default_from_mode_config() {
        // Empty config → human_default ("raw").
        assert_eq!(
            CompactorConfig::from_toml("").mode.interactive_default(),
            Some(OutputMode::Raw)
        );
        // Explicit [mode] default overrides human_default.
        let cfg = CompactorConfig::from_toml("[mode]\ndefault = \"compact\"\n");
        assert_eq!(cfg.mode.interactive_default(), Some(OutputMode::Compact));
        // human_default applies when no explicit default.
        let cfg = CompactorConfig::from_toml("[mode]\nhuman_default = \"semantic\"\n");
        assert_eq!(cfg.mode.interactive_default(), Some(OutputMode::Semantic));
        // Garbage → None (caller keeps its own default).
        let cfg = CompactorConfig::from_toml("[mode]\ndefault = \"bogus\"\n");
        assert_eq!(cfg.mode.interactive_default(), None);
    }

    #[test]
    fn builtin_presets_parse_and_match() {
        let mut cfg = CompactorConfig::default();
        cfg.merge_builtin_presets();
        // The bundle is non-trivial and parsed cleanly.
        assert!(
            cfg.compactors.len() >= 50,
            "presets didn't load: {}",
            cfg.compactors.len()
        );
        // A long-tail command resolves to a preset (matches bare and with args).
        assert!(cfg.matching_compactor(&["df".into()]).is_some());
        assert!(cfg
            .matching_compactor(&["df".into(), "-h".into()])
            .is_some());
        assert!(cfg
            .matching_compactor(&["systemctl".into(), "status".into(), "nginx".into()])
            .is_some());
        // Presets carry reduction ops (not just keep/group/tail).
        assert!(cfg
            .matching_compactor(&["rsync".into(), "-a".into()])
            .unwrap()
            .has_reduce());
    }

    #[test]
    fn user_compactor_overrides_builtin_preset() {
        let mut cfg = CompactorConfig::from_toml(
            r#"
[[compactor]]
name = "my-df"
match.argv = ["df", "*"]
max_lines = 3
"#,
        );
        cfg.merge_builtin_presets();
        // User compactor (priority 0) beats the preset (priority -1000).
        let chosen = cfg.matching_compactor(&["df".into(), "-h".into()]).unwrap();
        assert_eq!(chosen.name, "my-df");
    }

    #[test]
    fn parses_compactor_rules() {
        let toml = r#"
[budget]
default_tokens = 500

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
name = "tail"
action = "keep_tail"
lines = 80
"#;
        let cfg = CompactorConfig::from_toml(toml);
        assert_eq!(cfg.budget.default_tokens, 500);
        assert_eq!(cfg.compactors.len(), 1);
        let c = &cfg.compactors[0];
        assert_eq!(c.name, "mybuild");
        assert_eq!(c.priority, 100);
        assert_eq!(c.rule.len(), 2);
        assert_eq!(c.rule[1].lines, Some(80));
    }

    #[test]
    fn matches_argv_globs() {
        assert!(argv_matches(
            &["make".to_string(), "*".to_string()],
            &["/usr/bin/make".to_string(), "all".to_string()]
        ));
        assert!(!argv_matches(&["make".to_string()], &["cmake".to_string()]));
        assert!(glob_match("cargo*", "cargo-nextest"));
        assert!(!glob_match("cargo", "cargo-nextest"));
    }

    #[test]
    fn picks_highest_priority() {
        let toml = r#"
[[compactor]]
name = "low"
match.argv = ["make", "*"]
priority = 1
[[compactor]]
name = "high"
match.argv = ["make", "*"]
priority = 50
"#;
        let cfg = CompactorConfig::from_toml(toml);
        let m = cfg
            .matching_compactor(&["make".to_string(), "build".to_string()])
            .unwrap();
        assert_eq!(m.name, "high");
    }
}
