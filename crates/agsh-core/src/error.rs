use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellErrorKind {
    Parse,
    Execution,
    Policy,
    Io,
    /// A write to a closed downstream pipe (SIGPIPE). Treated as a clean
    /// early-exit by the executor, matching a POSIX shell — not a real error.
    BrokenPipe,
    NotFound,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError {
    pub kind: ShellErrorKind,
    pub message: String,
    pub span: Option<SourceSpan>,
    /// A stable, machine-branchable code (e.g. `agsh::parse::unclosed_quote`) that an
    /// agent can dispatch on instead of regexing the English message. `None` falls
    /// back to a kind-derived default via [`ShellError::code`]. Exposed for agent /
    /// semantic rendering; the human `Display` is deliberately unchanged.
    code: Option<&'static str>,
}

impl ShellError {
    pub fn new(kind: ShellErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            span: None,
            code: None,
        }
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    /// Attach a specific stable code, refining the kind-derived default.
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    /// The stable code: the explicit one if set, else a kind-derived default. Always
    /// returns a value so an agent can branch on `err.code()` unconditionally.
    pub fn code(&self) -> &'static str {
        if let Some(code) = self.code {
            return code;
        }
        match self.kind {
            ShellErrorKind::Parse => "agsh::parse",
            ShellErrorKind::Execution => "agsh::execution",
            ShellErrorKind::Policy => "agsh::policy::denied",
            ShellErrorKind::Io => "agsh::io",
            ShellErrorKind::BrokenPipe => "agsh::broken_pipe",
            ShellErrorKind::NotFound => "agsh::not_found",
            ShellErrorKind::Unsupported => "agsh::unsupported",
        }
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(ShellErrorKind::Parse, message)
    }

    pub fn execution(message: impl Into<String>) -> Self {
        Self::new(ShellErrorKind::Execution, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ShellErrorKind::Unsupported, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ShellErrorKind::NotFound, message)
    }

    /// A command refused by policy (e.g. a `confine` allowlist). Maps to exit 126.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ShellErrorKind::Policy, message)
    }
}

impl Display for ShellError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.span {
            Some(span) => write!(
                f,
                "{:?}: {} at {}..{}",
                self.kind, self.message, span.start, span.end
            ),
            None => write!(f, "{:?}: {}", self.kind, self.message),
        }
    }
}

impl std::error::Error for ShellError {}

impl From<std::io::Error> for ShellError {
    fn from(value: std::io::Error) -> Self {
        use std::io::ErrorKind;
        let (kind, code) = match value.kind() {
            ErrorKind::BrokenPipe => (ShellErrorKind::BrokenPipe, "agsh::broken_pipe"),
            ErrorKind::PermissionDenied => (ShellErrorKind::Io, "agsh::io::permission_denied"),
            ErrorKind::NotFound => (ShellErrorKind::Io, "agsh::io::not_found"),
            _ => (ShellErrorKind::Io, "agsh::io"),
        };
        Self::new(kind, value.to_string()).with_code(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_has_a_kind_default_and_is_overridable() {
        // Every error carries a stable, branchable code even without an explicit one.
        assert_eq!(ShellError::parse("bad").code(), "agsh::parse");
        assert_eq!(ShellError::denied("nope").code(), "agsh::policy::denied");
        assert_eq!(ShellError::not_found("x").code(), "agsh::not_found");
        // A specific code refines the default.
        assert_eq!(
            ShellError::parse("unterminated")
                .with_code("agsh::parse::unclosed_quote")
                .code(),
            "agsh::parse::unclosed_quote"
        );
        // io::Error refines by ErrorKind, keeping the path-bearing message.
        let perm: ShellError =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into();
        assert_eq!(perm.code(), "agsh::io::permission_denied");
        // The human Display is unchanged (byte-exact stderr contract).
        assert_eq!(ShellError::parse("boom").to_string(), "Parse: boom");
    }
}
