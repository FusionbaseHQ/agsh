//! Token budgeting for observations.

use crate::OutputMode;

/// Rough token estimate: ~4 characters per token.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Budget options, mirroring the `[budget]` config.
#[derive(Debug, Clone)]
pub struct BudgetOptions {
    pub default_tokens: usize,
    pub max_tokens: usize,
    pub fallback: OutputMode,
}

impl Default for BudgetOptions {
    fn default() -> Self {
        Self {
            default_tokens: 2000,
            max_tokens: 8000,
            fallback: OutputMode::LosslessRef,
        }
    }
}
