use std::fmt;
use std::path::{Component, Path, PathBuf};

use agsh_output::OutputMode;
use agsh_policy::{Capability, PolicyMode, Principal};

use crate::protocol::validate_identifier;

pub const MAX_TOKEN_BUDGET: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        validate_identifier(&value).map_err(|()| SessionError::InvalidSessionId)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct AgentSession {
    id: SessionId,
    principal: Principal,
    workspace: PathBuf,
    policy: PolicyMode,
    capabilities: Vec<Capability>,
    output_mode: OutputMode,
    token_budget: usize,
}

impl AgentSession {
    /// Create a session rooted at an existing directory.
    ///
    /// Canonicalizing once establishes the comparison root used by
    /// [`Self::resolve_existing_path`]. Filesystem handlers must still open and
    /// validate their targets at use time; this type does not eliminate TOCTOU
    /// races on a hostile, concurrently-mutated filesystem.
    pub fn new(id: impl Into<String>, workspace: PathBuf) -> Result<Self, SessionError> {
        let id = SessionId::parse(id)?;
        let workspace = std::fs::canonicalize(workspace)
            .map_err(|error| SessionError::InvalidWorkspace(error.to_string()))?;
        if !workspace.is_dir() {
            return Err(SessionError::WorkspaceNotDirectory);
        }
        Ok(Self {
            id,
            principal: Principal::agent_unknown(),
            workspace,
            policy: PolicyMode::AgentWorkspace,
            capabilities: Vec::new(),
            output_mode: OutputMode::Semantic,
            token_budget: 2000,
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn policy(&self) -> PolicyMode {
        self.policy
    }

    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    pub fn output_mode(&self) -> OutputMode {
        self.output_mode
    }

    pub fn token_budget(&self) -> usize {
        self.token_budget
    }

    pub fn set_token_budget(&mut self, token_budget: usize) -> Result<(), SessionError> {
        if token_budget == 0 || token_budget > MAX_TOKEN_BUDGET {
            return Err(SessionError::InvalidTokenBudget);
        }
        self.token_budget = token_budget;
        Ok(())
    }

    /// Resolve an existing workspace-relative path and reject lexical or symlink
    /// escapes. Absolute paths and `..` are never accepted by file operations.
    pub fn resolve_existing_path(&self, requested: &Path) -> Result<PathBuf, SessionError> {
        if requested.as_os_str().is_empty()
            || requested.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(SessionError::InvalidRelativePath);
        }
        let resolved = std::fs::canonicalize(self.workspace.join(requested))
            .map_err(|error| SessionError::PathUnavailable(error.to_string()))?;
        if !resolved.starts_with(&self.workspace) {
            return Err(SessionError::PathEscapesWorkspace);
        }
        Ok(resolved)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    InvalidSessionId,
    InvalidWorkspace(String),
    WorkspaceNotDirectory,
    InvalidTokenBudget,
    InvalidRelativePath,
    PathUnavailable(String),
    PathEscapesWorkspace,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId => formatter.write_str("invalid session id"),
            Self::InvalidWorkspace(error) => write!(formatter, "invalid workspace: {error}"),
            Self::WorkspaceNotDirectory => formatter.write_str("workspace is not a directory"),
            Self::InvalidTokenBudget => formatter.write_str("invalid token budget"),
            Self::InvalidRelativePath => formatter.write_str("path must be workspace-relative"),
            Self::PathUnavailable(error) => write!(formatter, "path is unavailable: {error}"),
            Self::PathEscapesWorkspace => formatter.write_str("path escapes the workspace"),
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agsh-agent-session-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("workspace/sub")).unwrap();
        std::fs::write(root.join("workspace/sub/file"), b"ok").unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret"), b"secret").unwrap();
        root
    }

    #[test]
    fn canonicalizes_workspace_and_bounds_budget() {
        let root = fixture("basic");
        let mut session = AgentSession::new("sess_1", root.join("workspace/../workspace")).unwrap();
        assert!(session.workspace().is_absolute());
        assert_eq!(session.token_budget(), 2000);
        assert!(session.set_token_budget(MAX_TOKEN_BUDGET).is_ok());
        assert_eq!(
            session.set_token_budget(MAX_TOKEN_BUDGET + 1),
            Err(SessionError::InvalidTokenBudget)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_absolute_parent_and_symlink_workspace_escapes() {
        let root = fixture("escape");
        let session = AgentSession::new("sess_1", root.join("workspace")).unwrap();
        assert_eq!(
            session.resolve_existing_path(Path::new("../outside/secret")),
            Err(SessionError::InvalidRelativePath)
        );
        assert_eq!(
            session.resolve_existing_path(&root.join("outside/secret")),
            Err(SessionError::InvalidRelativePath)
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside"), root.join("workspace/link")).unwrap();
            assert_eq!(
                session.resolve_existing_path(Path::new("link/secret")),
                Err(SessionError::PathEscapesWorkspace)
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
