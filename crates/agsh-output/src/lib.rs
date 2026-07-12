pub mod budget;
pub mod classify;
pub mod compact;
pub mod compactors;
pub mod config;
pub mod context;
pub mod mode;
pub mod normalize;
pub mod observation;
pub mod redact;
pub mod reduce;
pub mod rules;
pub mod summary;
pub mod util;

pub use budget::{estimate_tokens, BudgetOptions};
pub use classify::{classify, CommandFamily};
pub use compact::{
    finalize_trace_status, render_observation, render_observation_with,
    render_observation_with_raw_ref,
};
pub use config::{
    CompactorConfig, CompactorRuleSet, RawStorageOptions, DEFAULT_MAX_RAW_BYTES, HARD_MAX_RAW_BYTES,
};
pub use context::CompactionContext;
pub use mode::OutputMode;
pub use normalize::{normalize, NormalizeOptions};
pub use observation::{ObservationStreams, OutputObservation, RawStreamRef, RawTraceStatus};
pub use redact::{is_sensitive_env_name, redact, RedactOptions};
pub use summary::{CommandContext, SemanticSummary};
