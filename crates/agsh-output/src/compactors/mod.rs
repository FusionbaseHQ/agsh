//! Family compactors: turn a command's output into a structured summary.

pub mod compilers;
pub mod container;
pub mod generic;
pub mod git;
pub mod pkg;
pub mod search;
pub mod tests;

use crate::classify::{classify, CommandFamily};
use crate::summary::{CommandContext, SemanticSummary};

/// Maximum detail lines kept per section in a summary.
const MAX_SECTION: usize = 50;

/// Produce a semantic summary for a command by dispatching to its family
/// compactor, capping section sizes for a bounded result.
pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    let mut summary = match classify(cx.argv) {
        CommandFamily::Git => git::summarize(cx),
        CommandFamily::Tests => tests::summarize(cx),
        CommandFamily::Compilers => compilers::summarize(cx),
        CommandFamily::Search => search::summarize(cx),
        CommandFamily::Package => pkg::summarize(cx),
        CommandFamily::Container => container::summarize(cx),
        CommandFamily::Generic => generic::summarize(cx),
    };
    summary.cap_sections(MAX_SECTION);
    summary
}
