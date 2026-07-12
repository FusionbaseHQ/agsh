use std::path::PathBuf;

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
    parse_intercept_spec, print_captured_if_needed, set_capture_drain_helper, uninstall_intercept,
    CommandOutcome, ExecutionOptions, Executor, CAPTURE_DRAIN_READY,
};
pub use state::{
    restore_background_snapshot_stdin, ResolvedTrace, ShellFunction, ShellState, TraceReader,
};

/// Enforce the sticky session allowlist for builtins that intentionally launch
/// an external process without re-entering the main executor.
pub(crate) fn confined_external_denial(
    state: &ShellState,
    command: &str,
) -> Option<CommandOutcome> {
    let policy = state.confine_policy()?;
    if policy.allows(command) {
        return None;
    }
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    Some(CommandOutcome::captured(
        126,
        Vec::new(),
        format!(
            "agsh: {base}: not permitted in this confined session (allowed: {})\n",
            policy.display_list()
        )
        .into_bytes(),
    ))
}

/// Resolve a direct builtin-launched executable against the shell's effective
/// PATH rather than the host process's stale environment.
pub(crate) fn resolve_shell_external(state: &ShellState, command: &str) -> Option<PathBuf> {
    use agsh_compat::{CommandResolution, Resolver};

    match Resolver::default().resolve_external_only(command, state.lookup("PATH")) {
        Some(CommandResolution::External(path)) => Some(path),
        _ => None,
    }
}

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
