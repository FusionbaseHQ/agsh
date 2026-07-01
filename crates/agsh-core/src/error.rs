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
}

impl ShellError {
    pub fn new(kind: ShellErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            span: None,
        }
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
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
        let kind = if value.kind() == std::io::ErrorKind::BrokenPipe {
            ShellErrorKind::BrokenPipe
        } else {
            ShellErrorKind::Io
        };
        Self::new(kind, value.to_string())
    }
}
