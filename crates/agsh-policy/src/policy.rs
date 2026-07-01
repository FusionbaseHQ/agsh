use crate::RiskFinding;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
}

impl Principal {
    pub fn human_local() -> Self {
        Self {
            name: "human.local".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    HumanNormal,
    AgentReadOnly,
    AgentWorkspace,
    AgentNetworked,
    AgentFull,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    RequireApproval {
        findings: Vec<RiskFinding>,
    },
    Deny {
        reason: String,
        findings: Vec<RiskFinding>,
    },
}
