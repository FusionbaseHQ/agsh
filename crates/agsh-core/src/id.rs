use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(String);

impl CommandId {
    pub fn new() -> Self {
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Self(format!("cmd_{n:016x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for CommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for CommandId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
