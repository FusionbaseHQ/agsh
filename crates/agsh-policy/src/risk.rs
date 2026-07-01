use agsh_core::CommandGraph;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskFinding {
    pub level: RiskLevel,
    pub code: String,
    pub message: String,
}

impl RiskFinding {
    pub fn new(level: RiskLevel, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level,
            code: code.into(),
            message: message.into(),
        }
    }
}

pub fn analyze_graph(graph: &CommandGraph) -> Vec<RiskFinding> {
    let mut findings = Vec::new();
    for item in &graph.list.items {
        for command in &item.pipeline.commands {
            let Some(name) = command.command_name() else {
                continue;
            };
            match name {
                "sudo" | "doas" => findings.push(RiskFinding::new(
                    RiskLevel::High,
                    "exec.privilege_escalation",
                    "command invokes privilege escalation",
                )),
                "curl" | "wget" | "ssh" | "scp" | "rsync" => findings.push(RiskFinding::new(
                    RiskLevel::Medium,
                    "network.access",
                    "command may access the network",
                )),
                "rm" => {
                    if command
                        .argv
                        .iter()
                        .any(|arg| arg.contains("-rf") || arg == "-fr")
                    {
                        findings.push(RiskFinding::new(
                            RiskLevel::High,
                            "fs.recursive_delete",
                            "recursive deletion requires review for agent sessions",
                        ));
                    }
                }
                "chmod" | "chown" => findings.push(RiskFinding::new(
                    RiskLevel::Medium,
                    "fs.permission_change",
                    "command changes filesystem permissions or ownership",
                )),
                _ => {}
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_core::parse_line;

    #[test]
    fn analyzes_all_commands_in_command_lists() {
        let graph = parse_line("echo ok; rm -rf tmp && sudo true").unwrap();
        let findings = analyze_graph(&graph);

        assert!(findings
            .iter()
            .any(|finding| finding.code == "fs.recursive_delete"));
        assert!(findings
            .iter()
            .any(|finding| finding.code == "exec.privilege_escalation"));
    }
}
