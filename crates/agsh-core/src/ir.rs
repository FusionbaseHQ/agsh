use crate::lexer::{QuoteKind, WordSegment};
use crate::{CommandId, SourceSpan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandGraph {
    pub id: CommandId,
    pub source: String,
    pub pipeline: Pipeline,
    pub list: CommandList,
}

impl CommandGraph {
    pub fn new(source: impl Into<String>, pipeline: Pipeline) -> Self {
        let list = CommandList {
            items: vec![CommandListItem {
                operator: ListOperator::Always,
                pipeline: pipeline.clone(),
                background: false,
            }],
        };
        Self {
            id: CommandId::new(),
            source: source.into(),
            pipeline,
            list,
        }
    }

    pub fn with_list(source: impl Into<String>, list: CommandList) -> Self {
        let pipeline = list
            .items
            .first()
            .map(|item| item.pipeline.clone())
            .unwrap_or_default();
        Self {
            id: CommandId::new(),
            source: source.into(),
            pipeline,
            list,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.items.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandList {
    pub items: Vec<CommandListItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandListItem {
    pub operator: ListOperator,
    pub pipeline: Pipeline,
    /// True when this item is terminated by `&` and should run asynchronously.
    pub background: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOperator {
    Always,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pipeline {
    pub negated: bool,
    pub commands: Vec<CommandInvocation>,
}

impl Pipeline {
    pub fn new(commands: Vec<CommandInvocation>, negated: bool) -> Self {
        Self { negated, commands }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub assignments: Vec<Assignment>,
    pub argv: Vec<String>,
    pub argv_quote: Vec<QuoteKind>,
    pub argv_segments: Vec<Vec<WordSegment>>,
    pub redirections: Vec<Redirection>,
    pub span: Option<SourceSpan>,
}

impl CommandInvocation {
    pub fn new(
        assignments: Vec<Assignment>,
        argv: Vec<String>,
        argv_quote: Vec<QuoteKind>,
        argv_segments: Vec<Vec<WordSegment>>,
        redirections: Vec<Redirection>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self {
            assignments,
            argv,
            argv_quote,
            argv_segments,
            redirections,
            span,
        }
    }

    pub fn command_name(&self) -> Option<&str> {
        self.argv.first().map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    pub value: String,
    pub value_segments: Vec<WordSegment>,
}

impl Assignment {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            name: name.into(),
            value_segments: vec![WordSegment::new(value.clone(), QuoteKind::None)],
            value,
        }
    }

    pub fn with_segments(
        name: impl Into<String>,
        value: impl Into<String>,
        value_segments: Vec<WordSegment>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            value_segments,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirection {
    pub fd: u8,
    pub mode: RedirectionMode,
    pub target: RedirectionTarget,
}

impl Redirection {
    pub fn new(fd: u8, mode: RedirectionMode, target: RedirectionTarget) -> Self {
        Self { fd, mode, target }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectionMode {
    Read,
    Write,
    WriteClobber,
    Append,
    WriteBoth,
    DupFd,
    /// `<<` / `<<-`: the target word carries the heredoc body. Whether the body
    /// is expanded is encoded by the target segment's quote (Double = expand,
    /// Single = literal).
    HereDoc,
    /// `<<<`: the target word is expanded and fed to stdin with a trailing
    /// newline.
    HereString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectionTarget {
    Word {
        text: String,
        quote: QuoteKind,
        segments: Vec<WordSegment>,
    },
    Fd(u8),
    Close,
}
