#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Capability(pub String);

impl Capability {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}
