use std::path::{Path, PathBuf};

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
    /// A command candidate exists, but the effective process identity cannot
    /// execute it (or it is not a regular file). PATH search remembers the first
    /// such candidate while continuing to look for a later executable.
    NotExecutable(PathBuf),
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
            return match classify_candidate(&candidate) {
                CandidateStatus::Executable => Some(CommandResolution::External(candidate)),
                CandidateStatus::NotExecutable => Some(CommandResolution::NotExecutable(candidate)),
                CandidateStatus::Missing => None,
            };
        }

        let mut first_not_executable = None;
        for dir in path.unwrap_or_default().split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let candidate = PathBuf::from(dir).join(name);
            match classify_candidate(&candidate) {
                CandidateStatus::Executable => {
                    return Some(CommandResolution::External(candidate));
                }
                CandidateStatus::NotExecutable => {
                    first_not_executable.get_or_insert(candidate);
                }
                CandidateStatus::Missing => {}
            }
        }

        first_not_executable.map(CommandResolution::NotExecutable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateStatus {
    Executable,
    NotExecutable,
    Missing,
}

fn classify_candidate(path: &Path) -> CandidateStatus {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return CandidateStatus::Missing;
        }
        // Permission failures while traversing a candidate, symlink loops, and
        // other existing-but-unusable entries must yield 126 if no later PATH
        // entry succeeds, rather than being mislabeled as command-not-found.
        Err(_) => return CandidateStatus::NotExecutable,
    };
    if !metadata.is_file() {
        return CandidateStatus::NotExecutable;
    }

    #[cfg(unix)]
    {
        use rustix::fs::{accessat, Access, AtFlags, CWD};

        if accessat(CWD, path, Access::EXEC_OK, AtFlags::EACCESS).is_ok() {
            CandidateStatus::Executable
        } else {
            CandidateStatus::NotExecutable
        }
    }

    #[cfg(not(unix))]
    {
        CandidateStatus::Executable
    }
}

/// Whether `path` is a regular file executable by the process's effective
/// identity. Unlike a raw `mode & 0o111` check, this honors ownership, group
/// membership, ACLs, and root's platform-specific execute rules.
pub fn is_executable_file(path: &Path) -> bool {
    classify_candidate(path) == CandidateStatus::Executable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::io;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::{Command, Stdio};

    #[cfg(unix)]
    fn test_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agsh-compat-resolver-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create resolver test directory");
        path
    }

    #[cfg(unix)]
    fn write_script(path: &std::path::Path, output: &str, mode: u32) {
        std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"))
            .expect("write resolver test executable");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("set resolver test permissions");
    }

    #[cfg(unix)]
    fn cannot_execute_directly(path: &std::path::Path) -> bool {
        matches!(
            Command::new(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied
        )
    }

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

    #[cfg(unix)]
    #[test]
    fn external_only_skips_inaccessible_candidate_before_executable_candidate() {
        let base = test_dir("skip-inaccessible");
        let first_dir = base.join("first");
        let second_dir = base.join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first = first_dir.join("agsh-resolver-probe");
        let second = second_dir.join("agsh-resolver-probe");
        // An owner-created 0001 file has an execute bit, but is inaccessible to
        // its non-root owner. The old mode-bit-only check selected it anyway.
        write_script(&first, "wrong", 0o001);
        write_script(&second, "right", 0o700);
        if !cannot_execute_directly(&first) {
            // Root can execute any regular file with at least one execute bit;
            // the regression is specifically about ordinary effective IDs.
            let _ = std::fs::remove_dir_all(base);
            return;
        }

        let path = format!("{}:{}", first_dir.display(), second_dir.display());
        assert_eq!(
            Resolver::default().resolve_external_only("agsh-resolver-probe", Some(&path)),
            Some(CommandResolution::External(second))
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn external_only_reports_an_explicit_non_executable_file() {
        let base = test_dir("explicit-non-executable");
        let candidate = base.join("agsh-resolver-probe");
        write_script(&candidate, "unused", 0o600);

        assert_eq!(
            Resolver::default()
                .resolve_external_only(candidate.to_str().expect("UTF-8 temp path"), None),
            Some(CommandResolution::NotExecutable(candidate.clone()))
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
