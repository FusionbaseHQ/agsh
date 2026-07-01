pub mod error;
pub mod id;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod value;

pub use error::{ShellError, ShellErrorKind, SourceSpan};
pub use id::CommandId;
pub use ir::{
    Assignment, CommandGraph, CommandInvocation, CommandList, CommandListItem, ListOperator,
    Pipeline, Redirection, RedirectionMode, RedirectionTarget,
};
pub use lexer::{QuoteKind, WordSegment};
pub use parser::{is_incomplete, parse_line};
pub use value::Value;
