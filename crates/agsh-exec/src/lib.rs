pub mod agent;
pub mod builtins;
pub mod confine;
pub mod executor;
pub mod journal;
pub mod keep;
pub mod math;
pub mod sessions;
pub mod state;
pub mod suggest;

pub use agsh_store::history::{HistoryEntry, HistoryMatch, HistoryQuery, HistoryScope, SearchMode};
pub use confine::{
    parse_spec as confine_parse_spec, plan as confine_plan, ConfineOpts, ConfinePlan, Preset,
};
pub use executor::{
    install_confine_shims, install_deep_intercept, install_intercept_shims, intercept_active,
    parse_intercept_spec, print_captured_if_needed, uninstall_intercept, CommandOutcome,
    ExecutionOptions, Executor,
};
pub use state::{ShellFunction, ShellState};

/// History syntax highlighting is intentionally bounded so large histories stay
/// cheap to print and scroll. Row indexes are 1-based, matching displayed
/// history numbers.
pub const HISTORY_SYNTAX_HIGHLIGHT_RECENT_LIMIT: usize = 20;

pub fn history_index_allows_syntax_highlight(index: usize, total: usize) -> bool {
    index != 0 && index > total.saturating_sub(HISTORY_SYNTAX_HIGHLIGHT_RECENT_LIMIT)
}

/// Install signal handlers for interactive use: SIGINT/SIGQUIT set the shell's
/// interrupt flag so the shell interrupts loops/commands instead of dying.
/// Foreground children still receive terminal signals directly.
///
/// Known gaps (see SHIP_READINESS_PLAN P0-10): SIGTERM/SIGHUP are not handled, so
/// a kill at the raw-mode prompt can leave the terminal non-canonical until the
/// next `reset`; and SIGTTOU/SIGTTIN are not yet ignored (only relevant once
/// job-control terminal handoff lands).
pub fn install_signal_handlers(state: &ShellState) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGQUIT};
    signal_hook::flag::register(SIGINT, state.interrupt_flag())?;
    signal_hook::flag::register(SIGQUIT, state.interrupt_flag())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::history_index_allows_syntax_highlight;

    #[test]
    fn history_highlight_window_is_the_newest_twenty_one_based_rows() {
        assert!(!history_index_allows_syntax_highlight(0, 3));
        assert!(history_index_allows_syntax_highlight(1, 3));
        assert!(history_index_allows_syntax_highlight(3, 3));

        assert!(!history_index_allows_syntax_highlight(1, 21));
        assert!(history_index_allows_syntax_highlight(2, 21));
        assert!(history_index_allows_syntax_highlight(21, 21));

        assert!(!history_index_allows_syntax_highlight(80, 100));
        assert!(history_index_allows_syntax_highlight(81, 100));
        assert!(history_index_allows_syntax_highlight(100, 100));
    }
}
