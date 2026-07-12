//! Bounded wire types and workspace-scoped session metadata for the draft agsh
//! agent protocol.
//!
//! This crate does not implement a server, authentication, authorization,
//! command execution, or filesystem handlers. See `specs/agent-protocol-v0.md`.

pub mod protocol;
pub mod session;

pub use protocol::{
    AgentEvent, AgentOperation, AgentRequest, AgentResponse, ProtocolError, MAX_JSONL_FRAME_BYTES,
};
pub use session::{AgentSession, SessionError, SessionId, MAX_TOKEN_BUDGET};
