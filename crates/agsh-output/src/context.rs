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
}
