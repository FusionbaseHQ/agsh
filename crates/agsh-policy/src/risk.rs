use agsh_core::{CommandGraph, CommandInvocation};

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

/// Statically identify a deliberately small set of high-signal command risks.
///
/// This analyzer is deterministic but intentionally not an authorization
/// boundary. Expansion, aliases, functions, interpreters, and arbitrary program
/// behavior mean an empty result never proves that a command is safe.
pub fn analyze_graph(graph: &CommandGraph) -> Vec<RiskFinding> {
    let mut findings = Vec::new();
    analyze_graph_at_depth(graph, &mut findings, 0);
    findings
}

fn analyze_graph_at_depth(graph: &CommandGraph, findings: &mut Vec<RiskFinding>, depth: usize) {
    for item in &graph.list.items {
        for command in &item.pipeline.commands {
            analyze_command(command, findings, depth);
        }
    }
}

fn analyze_command(command: &CommandInvocation, findings: &mut Vec<RiskFinding>, depth: usize) {
    analyze_argv(&command.argv, findings, depth);
}

fn analyze_argv(argv: &[String], findings: &mut Vec<RiskFinding>, depth: usize) {
    if depth >= 16 {
        return;
    }
    let Some(name) = argv.first().map(|name| command_basename(name)) else {
        return;
    };
    let args = &argv[1..];
    match name {
        "sudo" | "doas" => findings.push(RiskFinding::new(
            RiskLevel::High,
            "exec.privilege_escalation",
            "command invokes privilege escalation",
        )),
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "rsync" | "nc" | "ncat" | "netcat" => findings
            .push(RiskFinding::new(
                RiskLevel::Medium,
                "network.access",
                "command may access the network",
            )),
        "rm" if has_recursive_flag(args) => {
            let targets_root = args.iter().any(|arg| matches!(arg.as_str(), "/" | "/*"));
            findings.push(RiskFinding::new(
                if targets_root {
                    RiskLevel::Critical
                } else {
                    RiskLevel::High
                },
                "fs.recursive_delete",
                if targets_root {
                    "recursive deletion targets the filesystem root"
                } else {
                    "recursive deletion requires review for agent sessions"
                },
            ));
        }
        "chmod" | "chown" | "chgrp" => findings.push(RiskFinding::new(
            RiskLevel::Medium,
            "fs.permission_change",
            "command changes filesystem permissions or ownership",
        )),
        "mkfs" | "mkfs.ext4" | "mkfs.xfs" => destructive_device_finding(findings),
        "diskutil" if diskutil_is_destructive(args) => destructive_device_finding(findings),
        "fdisk" if fdisk_is_destructive(args) => destructive_device_finding(findings),
        "parted" if parted_is_destructive(args) => destructive_device_finding(findings),
        "find" if args.iter().any(|arg| arg == "-delete") => findings.push(RiskFinding::new(
            RiskLevel::High,
            "fs.recursive_delete",
            "find -delete recursively removes matching paths",
        )),
        "git" => analyze_git(args, findings),
        "sh" | "bash" | "dash" | "zsh" | "ksh" => {
            analyze_inline_program(args, findings, depth, &["-c"], true);
        }
        "python" | "python3" => {
            analyze_inline_program(args, findings, depth, &["-c"], false);
        }
        "ruby" => analyze_inline_program(args, findings, depth, &["-e"], false),
        "perl" => analyze_inline_program(args, findings, depth, &["-e", "-E"], false),
        "node" => {
            analyze_inline_program(args, findings, depth, &["-e", "--eval"], false);
        }
        "command" | "external" | "builtin" => {
            if let Some(nested) = command_wrapper_payload(args) {
                analyze_argv(nested, findings, depth + 1);
            }
        }
        "env" => {
            if let Some(nested) = env_payload(args) {
                analyze_argv(nested, findings, depth + 1);
            }
        }
        "nohup" | "setsid" => {
            if let Some(index) = args.iter().position(|arg| !arg.starts_with('-')) {
                analyze_argv(&args[index..], findings, depth + 1);
            }
        }
        _ => {}
    }
}

fn command_wrapper_payload(args: &[String]) -> Option<&[String]> {
    if args.iter().any(|arg| matches!(arg.as_str(), "-v" | "-V")) {
        return None;
    }
    let index = args
        .iter()
        .position(|arg| arg == "--" || !arg.starts_with('-'))?;
    let index = index + usize::from(args[index] == "--");
    (index < args.len()).then_some(&args[index..])
}

fn env_payload(args: &[String]) -> Option<&[String]> {
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--" => {
                index += 1;
                break;
            }
            "-u" | "--unset" | "-C" | "--chdir" => index += 2,
            _ if arg.starts_with('-') => index += 1,
            _ if is_assignment(arg) => index += 1,
            _ => break,
        }
    }
    (index < args.len()).then_some(&args[index..])
}

fn is_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
            && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn analyze_git(args: &[String], findings: &mut Vec<RiskFinding>) {
    if args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "clone" | "fetch" | "pull" | "push" | "ls-remote"
        )
    }) {
        findings.push(RiskFinding::new(
            RiskLevel::Medium,
            "network.access",
            "git command may access a remote repository",
        ));
    }
    if args.first().is_some_and(|arg| arg == "clean")
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-n" | "--dry-run"))
        && args.iter().skip(1).any(|arg| {
            arg == "--force"
                || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
        })
    {
        findings.push(RiskFinding::new(
            RiskLevel::High,
            "fs.git_clean",
            "git clean --force deletes untracked files",
        ));
    }
    if args.first().is_some_and(|arg| arg == "reset") && args.iter().any(|arg| arg == "--hard") {
        findings.push(RiskFinding::new(
            RiskLevel::High,
            "fs.git_hard_reset",
            "git reset --hard discards working-tree changes",
        ));
    }
}

fn destructive_device_finding(findings: &mut Vec<RiskFinding>) {
    findings.push(RiskFinding::new(
        RiskLevel::Critical,
        "device.destructive_write",
        "command may overwrite a filesystem or partition table",
    ));
}

fn diskutil_is_destructive(args: &[String]) -> bool {
    let Some(command) = args.first().map(String::as_str) else {
        return false;
    };
    matches!(
        command,
        "eraseDisk" | "eraseVolume" | "partitionDisk" | "zeroDisk" | "randomDisk" | "secureErase"
    ) || (command == "apfs"
        && args.get(1).is_some_and(|subcommand| {
            matches!(
                subcommand.as_str(),
                "deleteVolume" | "deleteContainer" | "eraseVolume"
            )
        }))
}

fn fdisk_is_destructive(args: &[String]) -> bool {
    !args.is_empty()
        && !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-l" | "--list"))
}

fn parted_is_destructive(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "mklabel" | "mktable" | "mkpart" | "rm" | "resizepart" | "rescue"
        )
    })
}

fn analyze_inline_program(
    args: &[String],
    findings: &mut Vec<RiskFinding>,
    depth: usize,
    flags: &[&str],
    shell_syntax: bool,
) {
    let Some(index) = args.iter().position(|arg| {
        flags.contains(&arg.as_str())
            || (shell_syntax
                && arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].contains('c'))
    }) else {
        return;
    };
    findings.push(RiskFinding::new(
        RiskLevel::Medium,
        "exec.dynamic_code",
        "interpreter executes inline code that requires separate review",
    ));
    if !shell_syntax || depth >= 4 {
        return;
    }
    if let Some(source) = args.get(index + 1) {
        if let Ok(nested) = agsh_core::parse_line(source) {
            analyze_graph_at_depth(&nested, findings, depth + 1);
        }
    }
}

fn has_recursive_flag(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(arg.as_str(), "--recursive" | "-r" | "-R")
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].chars().any(|flag| matches!(flag, 'r' | 'R')))
    })
}

fn command_basename(command: &str) -> &str {
    command.rsplit(['/', '\\']).next().unwrap_or(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_core::parse_line;

    fn codes(source: &str) -> Vec<String> {
        analyze_graph(&parse_line(source).unwrap())
            .into_iter()
            .map(|finding| finding.code)
            .collect()
    }

    #[test]
    fn analyzes_all_commands_in_command_lists() {
        let findings = codes("echo ok; rm -rf tmp && sudo true");
        assert!(findings.iter().any(|code| code == "fs.recursive_delete"));
        assert!(findings
            .iter()
            .any(|code| code == "exec.privilege_escalation"));
    }

    #[test]
    fn recognizes_absolute_paths_and_recursive_flag_spellings() {
        for source in [
            "/bin/rm --recursive tmp",
            "rm -R tmp",
            "rm -vfr tmp",
            "rm -r -- tmp",
        ] {
            assert!(
                codes(source)
                    .iter()
                    .any(|code| code == "fs.recursive_delete"),
                "missed {source}"
            );
        }
        let root = analyze_graph(&parse_line("rm -rf /").unwrap());
        assert!(root.iter().any(|finding| {
            finding.code == "fs.recursive_delete" && finding.level == RiskLevel::Critical
        }));
    }

    #[test]
    fn inspects_literal_shell_c_but_marks_all_inline_code() {
        let shell = codes("sh -c 'rm --recursive tmp'");
        assert!(shell.iter().any(|code| code == "exec.dynamic_code"));
        assert!(shell.iter().any(|code| code == "fs.recursive_delete"));

        let python = codes("python3 -c 'import shutil; shutil.rmtree(\"tmp\")'");
        assert_eq!(python, vec!["exec.dynamic_code"]);
        assert_eq!(
            codes("node -e 'process.exit(0)'"),
            vec!["exec.dynamic_code"]
        );
        assert!(codes("perl -c script.pl").is_empty());

        let login_shell = codes("bash -lc 'rm -rf tmp'");
        assert!(login_shell.iter().any(|code| code == "exec.dynamic_code"));
        assert!(login_shell.iter().any(|code| code == "fs.recursive_delete"));
    }

    #[test]
    fn recognizes_destructive_git_and_find_forms() {
        assert_eq!(codes("git clean -fdx"), vec!["fs.git_clean"]);
        assert_eq!(codes("git reset --hard HEAD"), vec!["fs.git_hard_reset"]);
        assert_eq!(codes("find . -delete"), vec!["fs.recursive_delete"]);
        for source in ["git push origin main", "git fetch origin", "git clone repo"] {
            assert_eq!(codes(source), vec!["network.access"]);
        }
        assert!(codes("git clean -n -fdx").is_empty());
    }

    #[test]
    fn distinguishes_read_only_disk_inspection_from_destructive_operations() {
        for source in ["diskutil list", "fdisk --list", "parted /dev/disk print"] {
            assert!(codes(source).is_empty(), "false positive for {source}");
        }
        for source in [
            "mkfs.ext4 /dev/disk",
            "diskutil eraseDisk APFS x /dev/disk1",
            "fdisk /dev/disk",
            "parted /dev/disk mklabel gpt",
        ] {
            assert_eq!(
                codes(source),
                vec!["device.destructive_write"],
                "missed {source}"
            );
        }
    }

    #[test]
    fn sees_through_resolution_and_environment_wrappers() {
        for source in [
            "command /bin/rm --recursive tmp",
            "external rm -rf tmp",
            "env FOO=bar rm -R tmp",
            "env -u HOME -- /bin/rm -r tmp",
            "nohup rm -rf tmp",
        ] {
            assert!(
                codes(source)
                    .iter()
                    .any(|code| code == "fs.recursive_delete"),
                "missed {source}"
            );
        }
        assert!(codes("command -v rm").is_empty());
    }

    #[test]
    fn does_not_treat_non_recursive_rm_as_recursive() {
        assert!(codes("rm -f file").is_empty());
        assert!(codes("echo --recursive").is_empty());
    }
}
