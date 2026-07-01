use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    #[default]
    Raw,
    Clean,
    Compact,
    Semantic,
    LosslessRef,
    Silent,
    /// Human display: render recognized output by type (markdown, JSON, CSV,
    /// diff, binary). Raw bytes still flow to pipes/redirects/files.
    Rich,
}

impl OutputMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            OutputMode::Raw => "raw",
            OutputMode::Clean => "clean",
            OutputMode::Compact => "compact",
            OutputMode::Semantic => "semantic",
            OutputMode::LosslessRef => "lossless-ref",
            OutputMode::Silent => "silent",
            OutputMode::Rich => "rich",
        }
    }

    pub const fn should_capture(self) -> bool {
        !matches!(self, OutputMode::Raw)
    }
}

impl FromStr for OutputMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "raw" => Ok(OutputMode::Raw),
            "clean" => Ok(OutputMode::Clean),
            "compact" => Ok(OutputMode::Compact),
            "semantic" => Ok(OutputMode::Semantic),
            "lossless-ref" | "lossless_ref" => Ok(OutputMode::LosslessRef),
            "silent" => Ok(OutputMode::Silent),
            "rich" => Ok(OutputMode::Rich),
            other => Err(format!("unknown output mode: {other}")),
        }
    }
}
