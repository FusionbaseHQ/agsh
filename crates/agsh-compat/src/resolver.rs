use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub const REQUIRED_BUILTINS: &[&str] = &[
    "cd",
    "pwd",
    "exit",
    "export",
    "unset",
    "set",
    "alias",
    "unalias",
    "abbr",
    "unabbr",
    "source",
    ".",
    "jobs",
    "fg",
    "bg",
    "wait",
    "kill",
    "history",
    "type",
    "which",
    "command",
    "external",
    "builtin",
    "eval",
    "exec",
    "read",
    "test",
    "[",
    "true",
    "false",
    "printf",
    "echo",
    "ulimit",
    "umask",
    "break",
    "continue",
    // agsh tools: ag-prefixed where a common CLI shares the name (the bare name is
    // left for the real tool), bare where there is no conflict.
    "agview",
    "agpatch",
    "agmath",
    "agz",
    "agjump",
    "agtrust",
    "agcontext",
    "agtrace",
    "confine",
    "agconfine",
    "peek",
    "agpeek",
    "risk",
    "agrisk",
    "snapshot",
    "agsnapshot",
    "pty",
    "agpty",
];

#[derive(Debug, Clone)]
pub struct ResolverConfig {
    pub external_coreutils_by_default: bool,
    pub accelerated_coreutils: bool,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            external_coreutils_by_default: true,
            accelerated_coreutils: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResolution {
    Builtin(String),
    External(PathBuf),
    Function(String),
    Alias(String),
    Abbreviation(String),
    Plugin(String),
    NotFound(String),
}

#[derive(Debug, Default)]
pub struct Resolver {
    pub config: ResolverConfig,
}

impl Resolver {
    pub fn resolve(&self, name: &str, path: Option<&str>) -> CommandResolution {
        if let Some(resolution) = self.resolve_builtin_only(name) {
            return resolution;
        }
        self.resolve_external_only(name, path)
            .unwrap_or_else(|| CommandResolution::NotFound(name.to_string()))
    }

    pub fn resolve_builtin_only(&self, name: &str) -> Option<CommandResolution> {
        if REQUIRED_BUILTINS.contains(&name) {
            Some(CommandResolution::Builtin(name.to_string()))
        } else {
            None
        }
    }

    pub fn resolve_external_only(
        &self,
        name: &str,
        path: Option<&str>,
    ) -> Option<CommandResolution> {
        if name.contains('/') {
            let candidate = PathBuf::from(name);
            return is_executable_file(&candidate)
                .then_some(CommandResolution::External(candidate));
        }

        for dir in path.unwrap_or_default().split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let candidate = PathBuf::from(dir).join(name);
            if is_executable_file(&candidate) {
                return Some(CommandResolution::External(candidate));
            }
        }

        None
    }
}

fn is_executable_file(path: &PathBuf) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_required_builtin() {
        let resolver = Resolver::default();
        assert_eq!(
            resolver.resolve("cd", None),
            CommandResolution::Builtin("cd".to_string())
        );
    }

    #[test]
    fn external_only_skips_builtins() {
        let resolver = Resolver::default();
        assert_eq!(resolver.resolve_external_only("cd", None), None);
    }
}
