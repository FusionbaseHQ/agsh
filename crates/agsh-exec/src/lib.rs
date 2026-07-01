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
    install_confine_shims, install_intercept_shims, print_captured_if_needed, CommandOutcome,
    ExecutionOptions, Executor,
};
pub use state::{ShellFunction, ShellState};

/// Install signal handlers for interactive use: SIGINT/SIGQUIT set the shell's
/// interrupt flag (so the shell interrupts loops/commands instead of dying),
/// while SIGTTOU/SIGTTIN are ignored so terminal handoff does not stop the
/// shell. Foreground children still receive terminal signals directly.
pub fn install_signal_handlers(state: &ShellState) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGQUIT};
    signal_hook::flag::register(SIGINT, state.interrupt_flag())?;
    signal_hook::flag::register(SIGQUIT, state.interrupt_flag())?;
    Ok(())
}
