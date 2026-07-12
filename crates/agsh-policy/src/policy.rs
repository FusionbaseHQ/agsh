use std::fmt;

use crate::{Capability, RiskFinding, RiskLevel};

const MAX_PRINCIPAL_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    name: String,
}

impl Principal {
    pub fn parse(name: impl Into<String>) -> Result<Self, PrincipalError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > MAX_PRINCIPAL_BYTES
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            })
        {
            return Err(PrincipalError);
        }
        Ok(Self { name })
    }

    pub fn human_local() -> Self {
        Self {
            name: "human.local".to_string(),
        }
    }

    pub fn agent_unknown() -> Self {
        Self {
            name: "agent.unknown".to_string(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrincipalError;

impl fmt::Display for PrincipalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid principal name")
    }
}

impl std::error::Error for PrincipalError {}

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

/// Evaluate capabilities derived by a trusted operation handler plus static risk
/// findings. Callers must never use capability names supplied by the requester:
/// this function validates a decision, but does not discover a command's effects
/// or enforce the resulting capabilities at the OS boundary.
pub fn evaluate_policy(
    mode: PolicyMode,
    required: &[Capability],
    findings: &[RiskFinding],
) -> PolicyDecision {
    if mode == PolicyMode::HumanNormal {
        return PolicyDecision::Allow;
    }

    let denied: std::collections::BTreeSet<&str> = required
        .iter()
        .map(Capability::as_str)
        .filter(|capability| !baseline_allows(mode, capability))
        .collect();
    if !denied.is_empty() {
        return PolicyDecision::Deny {
            reason: format!(
                "policy does not grant capabilities: {}",
                denied.into_iter().collect::<Vec<_>>().join(", ")
            ),
            findings: findings.to_vec(),
        };
    }

    if findings
        .iter()
        .any(|finding| finding.level == RiskLevel::Critical)
    {
        return PolicyDecision::Deny {
            reason: "critical-risk command is denied for agent sessions".to_string(),
            findings: findings.to_vec(),
        };
    }

    let approval_threshold = match mode {
        PolicyMode::AgentReadOnly | PolicyMode::AgentWorkspace => RiskLevel::Medium,
        PolicyMode::AgentNetworked | PolicyMode::AgentFull => RiskLevel::High,
        PolicyMode::HumanNormal => unreachable!("handled above"),
    };
    if findings
        .iter()
        .any(|finding| finding.level >= approval_threshold)
    {
        PolicyDecision::RequireApproval {
            findings: findings.to_vec(),
        }
    } else {
        PolicyDecision::Allow
    }
}

fn baseline_allows(mode: PolicyMode, capability: &str) -> bool {
    match mode {
        PolicyMode::HumanNormal | PolicyMode::AgentFull => true,
        PolicyMode::AgentReadOnly => matches!(capability, "read:workspace" | "exec:project"),
        PolicyMode::AgentWorkspace => matches!(
            capability,
            "read:workspace" | "write:workspace" | "exec:project"
        ),
        PolicyMode::AgentNetworked => matches!(
            capability,
            "read:workspace" | "write:workspace" | "exec:project" | "network:outbound"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(name: &str) -> Capability {
        Capability::parse(name).unwrap()
    }

    #[test]
    fn denies_capabilities_outside_the_mode_baseline() {
        let decision = evaluate_policy(
            PolicyMode::AgentReadOnly,
            &[
                capability("write:workspace"),
                capability("network:outbound"),
                capability("write:workspace"),
            ],
            &[],
        );
        assert!(matches!(
            decision,
            PolicyDecision::Deny { reason, .. }
                if reason == "policy does not grant capabilities: network:outbound, write:workspace"
        ));
    }

    #[test]
    fn risk_thresholds_are_deterministic() {
        let medium = RiskFinding::new(RiskLevel::Medium, "network.access", "network");
        assert!(matches!(
            evaluate_policy(
                PolicyMode::AgentWorkspace,
                &[capability("exec:project")],
                std::slice::from_ref(&medium)
            ),
            PolicyDecision::RequireApproval { .. }
        ));
        assert_eq!(
            evaluate_policy(
                PolicyMode::AgentNetworked,
                &[capability("exec:project")],
                &[medium]
            ),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn critical_findings_are_denied_for_every_agent_mode() {
        let finding = RiskFinding::new(RiskLevel::Critical, "fs.root", "root deletion");
        for mode in [
            PolicyMode::AgentReadOnly,
            PolicyMode::AgentWorkspace,
            PolicyMode::AgentNetworked,
            PolicyMode::AgentFull,
        ] {
            assert!(matches!(
                evaluate_policy(mode, &[], std::slice::from_ref(&finding)),
                PolicyDecision::Deny { .. }
            ));
        }
    }

    #[test]
    fn validates_principals_before_they_reach_logs_or_policy_keys() {
        assert_eq!(
            Principal::parse("agent.codex").unwrap().as_str(),
            "agent.codex"
        );
        for invalid in ["", "agent codex", "agent\ncodex", "agent/codex"] {
            assert!(Principal::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
