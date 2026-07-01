use std::path::PathBuf;

use agsh_output::OutputMode;
use agsh_policy::{Capability, PolicyMode, Principal};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub String);

#[derive(Debug, Clone)]
pub struct AgentSession {
    pub id: SessionId,
    pub principal: Principal,
    pub workspace: PathBuf,
    pub policy: PolicyMode,
    pub capabilities: Vec<Capability>,
    pub output_mode: OutputMode,
    pub token_budget: usize,
}

impl AgentSession {
    pub fn new(id: impl Into<String>, workspace: PathBuf) -> Self {
        Self {
            id: SessionId(id.into()),
            principal: Principal {
                name: "agent.unknown".to_string(),
            },
            workspace,
            policy: PolicyMode::AgentWorkspace,
            capabilities: Vec::new(),
            output_mode: OutputMode::Semantic,
            token_budget: 2000,
        }
    }
}
