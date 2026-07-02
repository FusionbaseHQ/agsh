pub mod agent;
pub mod builtins;
pub mod confine;
pub mod executor;
pub mod math;
pub mod sessions;
pub mod state;
pub mod suggest;

pub use confine::{
    parse_spec as confine_parse_spec, plan as confine_plan, ConfineOpts, ConfinePlan, Preset,
};
pub use executor::{
    install_confine_shims, install_deep_intercept, install_intercept_shims, intercept_active,
    parse_intercept_spec, print_captured_if_needed, uninstall_intercept, CommandOutcome,
    ExecutionOptions, Executor,
};
pub use state::{ShellFunction, ShellState};

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
