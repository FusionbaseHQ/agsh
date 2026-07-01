//! Detect a command's family from its argv so the right compactor is chosen.

use crate::util::{command_basename, subcommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandFamily {
    Git,
    Tests,
    Compilers,
    Search,
    Package,
    Container,
    Generic,
}

impl CommandFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandFamily::Git => "git",
            CommandFamily::Tests => "tests",
            CommandFamily::Compilers => "compilers",
            CommandFamily::Search => "search",
            CommandFamily::Package => "package",
            CommandFamily::Container => "container",
            CommandFamily::Generic => "generic",
        }
    }
}

/// Classify a command from its argv (program basename + subcommand).
pub fn classify(argv: &[String]) -> CommandFamily {
    let prog = command_basename(argv);
    let sub = subcommand(argv).unwrap_or("");
    match prog {
        "git" => CommandFamily::Git,
        "pytest" | "py.test" => CommandFamily::Tests,
        "go" if sub == "test" => CommandFamily::Tests,
        "cargo" => match sub {
            "test" | "nextest" => CommandFamily::Tests,
            "build" | "check" | "clippy" | "rustc" | "b" | "c" => CommandFamily::Compilers,
            _ => CommandFamily::Generic,
        },
        "jest" | "vitest" | "mocha" | "ava" => CommandFamily::Tests,
        "npx" if matches!(sub, "jest" | "vitest" | "mocha") => CommandFamily::Tests,
        "gcc" | "g++" | "clang" | "clang++" | "cc" | "c++" => CommandFamily::Compilers,
        "tsc" | "mypy" | "ruff" | "eslint" | "flake8" | "pyright" | "pylint" => {
            CommandFamily::Compilers
        }
        "grep" | "egrep" | "fgrep" => CommandFamily::Search,
        "rg" | "ripgrep" | "ag" | "ack" => CommandFamily::Search,
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            "test" | "t" => CommandFamily::Tests,
            _ => CommandFamily::Package,
        },
        "pip" | "pip3" => CommandFamily::Package,
        "make" | "cmake" | "ninja" | "bazel" | "gradle" | "mvn" => CommandFamily::Package,
        "docker" | "docker-compose" | "podman" | "nerdctl" => CommandFamily::Container,
        "kubectl" | "helm" | "k9s" | "kustomize" => CommandFamily::Container,
        "python" | "python3" => {
            if argv.iter().any(|a| a == "pytest") {
                CommandFamily::Tests
            } else {
                CommandFamily::Generic
            }
        }
        _ => CommandFamily::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classifies_families() {
        assert_eq!(classify(&argv(&["git", "status"])), CommandFamily::Git);
        assert_eq!(classify(&argv(&["pytest", "-q"])), CommandFamily::Tests);
        assert_eq!(classify(&argv(&["cargo", "test"])), CommandFamily::Tests);
        assert_eq!(
            classify(&argv(&["cargo", "build"])),
            CommandFamily::Compilers
        );
        assert_eq!(
            classify(&argv(&["/usr/bin/clang", "-c", "a.c"])),
            CommandFamily::Compilers
        );
        assert_eq!(classify(&argv(&["rg", "needle"])), CommandFamily::Search);
        assert_eq!(classify(&argv(&["npm", "install"])), CommandFamily::Package);
        assert_eq!(classify(&argv(&["npm", "test"])), CommandFamily::Tests);
        assert_eq!(
            classify(&argv(&["docker", "build", "."])),
            CommandFamily::Container
        );
        assert_eq!(
            classify(&argv(&["kubectl", "get", "pods"])),
            CommandFamily::Container
        );
        assert_eq!(classify(&argv(&["ls", "-la"])), CommandFamily::Generic);
    }
}
