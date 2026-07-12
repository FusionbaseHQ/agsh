//! Deterministic capability, allowlist, and advisory risk policy primitives.
//!
//! Policy decisions only have security effect when a trusted caller derives the
//! required capabilities and enforces the result at an OS or operation boundary.

pub mod allow;
pub mod capability;
pub mod policy;
pub mod risk;

pub use allow::AllowPolicy;
pub use capability::{Capability, CapabilityError, MAX_CAPABILITY_BYTES};
pub use policy::{evaluate_policy, PolicyDecision, PolicyMode, Principal, PrincipalError};
pub use risk::{analyze_graph, RiskFinding, RiskLevel};
