//! Per-render context: normalization, redaction, budget, and an optional
//! configured compactor matched for this command.

use crate::config::CompactorRuleSet;
use crate::normalize::NormalizeOptions;
use crate::redact::{redact, RedactOptions};
use crate::{normalize, BudgetOptions};

#[derive(Debug, Clone, Default)]
pub struct CompactionContext {
    pub normalize: NormalizeOptions,
    pub redact: RedactOptions,
    pub budget: BudgetOptions,
    /// A user-configured `[[compactor]]` matched for this command, if any.
    pub compactor: Option<CompactorRuleSet>,
}

impl CompactionContext {
    /// A context with default normalization, default token-pattern redaction,
    /// default budget, and no configured compactor.
    pub fn defaults() -> Self {
        Self {
            normalize: NormalizeOptions::default(),
            redact: RedactOptions::with_defaults(Vec::new()),
            budget: BudgetOptions::default(),
            compactor: None,
        }
    }

    /// Normalize then redact a block of text for the observation stream.
    pub fn clean_text(&self, input: &str) -> String {
        redact(&normalize::normalize(input, &self.normalize), &self.redact)
    }

    /// Redact without normalizing. Command metadata should retain its spelling
    /// and paths while still removing literal and token-shaped secrets.
    pub fn redact_text(&self, input: &str) -> String {
        redact(input, &self.redact)
    }

    /// The tiny-output fast path: when a command's combined output is at most
    /// [`TINY_LINES`] short lines, no observation is more compact than the
    /// output itself, so return it verbatim — cleaned (ANSI strip, control
    /// sanitization, redaction) but WITHOUT home/workspace path shortening,
    /// which can erase the entire answer of commands like `pwd` (whose output
    /// literally IS the workspace path, previously rendered as just `.`).
    /// `None` when the output is too large for the fast path.
    pub fn verbatim_tiny(&self, stdout: &str, stderr: &str) -> Option<String> {
        let mut options = self.normalize.clone();
        options.shorten_home = false;
        options.shorten_workspace = false;
        let combined = format!("{stdout}{stderr}");
        let cleaned = redact(&normalize::normalize(&combined, &options), &self.redact);
        let content = cleaned.trim_end_matches('\n');
        if content.lines().count() <= TINY_LINES && content.chars().count() <= TINY_CHARS {
            Some(cleaned)
        } else {
            None
        }
    }
}

/// Bounds for [`CompactionContext::verbatim_tiny`]: an output this small is its
/// own most-compact representation (scaffolding would be larger than it).
const TINY_LINES: usize = 3;
const TINY_CHARS: usize = 400;
