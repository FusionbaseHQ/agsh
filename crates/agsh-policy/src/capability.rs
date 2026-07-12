use std::fmt;

pub const MAX_CAPABILITY_BYTES: usize = 128;

/// A validated, serializable capability name such as `read:workspace`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Capability(String);

impl Capability {
    pub fn parse(value: impl Into<String>) -> Result<Self, CapabilityError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CAPABILITY_BYTES
            || !value.is_ascii()
            || value.split(':').any(|segment| {
                segment.is_empty()
                    || !segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-' | b'.')
                    })
            })
        {
            return Err(CapabilityError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityError;

impl fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid capability name")
    }
}

impl std::error::Error for CapabilityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_stable_names_and_rejects_delimiters() {
        assert_eq!(
            Capability::parse("read:workspace").unwrap().as_str(),
            "read:workspace"
        );
        for invalid in [
            "",
            "read::workspace",
            "Read:workspace",
            "read:work space",
            "read:workspace,network:outbound",
            "read:\nworkspace",
        ] {
            assert!(Capability::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
