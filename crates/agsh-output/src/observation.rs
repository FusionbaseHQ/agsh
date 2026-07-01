#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamRef {
    pub stdout: String,
    pub stderr: String,
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
