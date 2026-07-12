#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawTraceStatus {
    Complete,
    Truncated,
    /// Storage was enabled, but no usable reference could be retained.
    Unavailable,
    /// Raw storage was disabled by configuration before capture began.
    Disabled,
}

impl RawTraceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Truncated => "truncated",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamRef {
    pub stdout: String,
    pub stderr: String,
    pub stdout_status: RawTraceStatus,
    pub stderr_status: RawTraceStatus,
    /// Combined stdout/stderr storage ceiling for this command. Synthetic
    /// in-memory references have no configured ceiling.
    pub storage_limit_bytes: Option<u64>,
}

impl RawStreamRef {
    pub fn exact(stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            stdout_status: RawTraceStatus::Complete,
            stderr_status: RawTraceStatus::Complete,
            storage_limit_bytes: None,
        }
    }

    pub fn persisted(
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        stdout_status: RawTraceStatus,
        stderr_status: RawTraceStatus,
        storage_limit_bytes: u64,
    ) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: stderr.into(),
            stdout_status,
            stderr_status,
            storage_limit_bytes: Some(storage_limit_bytes),
        }
    }

    pub fn disabled() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            stdout_status: RawTraceStatus::Disabled,
            stderr_status: RawTraceStatus::Disabled,
            storage_limit_bytes: Some(0),
        }
    }

    pub fn unavailable() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            stdout_status: RawTraceStatus::Unavailable,
            stderr_status: RawTraceStatus::Unavailable,
            storage_limit_bytes: None,
        }
    }

    pub const fn is_complete(&self) -> bool {
        matches!(self.stdout_status, RawTraceStatus::Complete)
            && matches!(self.stderr_status, RawTraceStatus::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::{RawStreamRef, RawTraceStatus};

    #[test]
    fn unavailable_reference_has_no_usable_paths() {
        let raw = RawStreamRef::unavailable();

        assert!(raw.stdout.is_empty());
        assert!(raw.stderr.is_empty());
        assert_eq!(raw.stdout_status, RawTraceStatus::Unavailable);
        assert_eq!(raw.stderr_status, RawTraceStatus::Unavailable);
        assert_eq!(raw.storage_limit_bytes, None);
        assert!(!raw.is_complete());
    }

    #[test]
    fn disabled_and_unavailable_references_are_distinct() {
        let disabled = RawStreamRef::disabled();
        let unavailable = RawStreamRef::unavailable();

        assert_eq!(disabled.stdout_status, RawTraceStatus::Disabled);
        assert_eq!(disabled.storage_limit_bytes, Some(0));
        assert_ne!(disabled, unavailable);
    }
}

/// Bounded bytes used to render an observation plus durable references to the
/// corresponding exact streams.
#[derive(Debug, Clone, Copy)]
pub struct ObservationStreams<'a> {
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub raw: &'a RawStreamRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputObservation {
    pub display: String,
    pub token_estimate: usize,
    pub raw: Option<RawStreamRef>,
}

impl OutputObservation {
    pub fn empty() -> Self {
        Self {
            display: String::new(),
            token_estimate: 0,
            raw: None,
        }
    }
}
