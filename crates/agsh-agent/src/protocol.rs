use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum size of one JSONL frame, including its optional trailing newline.
///
/// Transports must enforce this limit before buffering or parsing a frame. The
/// codec enforces it again so direct callers cannot pass unbounded input.
pub const MAX_JSONL_FRAME_BYTES: usize = 1024 * 1024;

const MAX_IDENTIFIER_BYTES: usize = 128;

/// Operations named by the v0 draft. This is a wire vocabulary, not a claim
/// that a server or handler for an operation is currently implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOperation {
    SessionOpen,
    CommandRun,
    CommandInput,
    CommandCancel,
    TraceRead,
    FileReadRange,
    FilePatch,
    GitDiff,
    GitSnapshot,
}

impl AgentOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionOpen => "session.open",
            Self::CommandRun => "command.run",
            Self::CommandInput => "command.input",
            Self::CommandCancel => "command.cancel",
            Self::TraceRead => "trace.read",
            Self::FileReadRange => "file.read_range",
            Self::FilePatch => "file.patch",
            Self::GitDiff => "git.diff",
            Self::GitSnapshot => "git.snapshot",
        }
    }

    fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "session.open" => Ok(Self::SessionOpen),
            "command.run" => Ok(Self::CommandRun),
            "command.input" => Ok(Self::CommandInput),
            "command.cancel" => Ok(Self::CommandCancel),
            "trace.read" => Ok(Self::TraceRead),
            "file.read_range" => Ok(Self::FileReadRange),
            "file.patch" => Ok(Self::FilePatch),
            "git.diff" => Ok(Self::GitDiff),
            "git.snapshot" => Ok(Self::GitSnapshot),
            _ => Err(ProtocolError::UnsupportedOperation),
        }
    }

    fn requires_session(self) -> bool {
        !matches!(self, Self::SessionOpen)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRequest {
    id: String,
    op: AgentOperation,
    session: Option<String>,
    params: Value,
}

impl AgentRequest {
    /// Decode and validate exactly one JSONL request frame.
    ///
    /// An optional final newline is removed. Any other literal line break is
    /// rejected, preventing request smuggling between readers that disagree
    /// about framing. Duplicate and unknown envelope fields are also rejected.
    pub fn decode_jsonl(frame: &[u8]) -> Result<Self, ProtocolError> {
        let content = frame_content(frame)?;
        let wire: WireRequest = serde_json::from_slice(content)
            .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
        validate_identifier(&wire.id).map_err(|()| ProtocolError::InvalidRequestId)?;
        let op = AgentOperation::parse(&wire.op)?;
        if !wire.params.is_object() {
            return Err(ProtocolError::ParamsMustBeObject);
        }
        match (&wire.session, op.requires_session()) {
            (Some(session), _) => {
                validate_identifier(session).map_err(|()| ProtocolError::InvalidSessionId)?;
            }
            (None, true) => return Err(ProtocolError::MissingSession),
            (None, false) => {}
        }
        if matches!(op, AgentOperation::SessionOpen) && wire.session.is_some() {
            return Err(ProtocolError::UnexpectedSession);
        }
        Ok(Self {
            id: wire.id,
            op,
            session: wire.session,
            params: wire.params,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn operation(&self) -> AgentOperation {
        self.op
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub fn params(&self) -> &Value {
        &self.params
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    id: String,
    op: String,
    #[serde(default)]
    session: Option<String>,
    params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentResponse {
    id: String,
    body: ResponseBody,
}

#[derive(Debug, Clone, PartialEq)]
enum ResponseBody {
    Result(Value),
    Error(Value),
}

impl AgentResponse {
    pub fn success(id: impl Into<String>, result: Value) -> Result<Self, ProtocolError> {
        let id = id.into();
        validate_identifier(&id).map_err(|()| ProtocolError::InvalidRequestId)?;
        Ok(Self {
            id,
            body: ResponseBody::Result(result),
        })
    }

    pub fn error(id: impl Into<String>, error: Value) -> Result<Self, ProtocolError> {
        let id = id.into();
        validate_identifier(&id).map_err(|()| ProtocolError::InvalidRequestId)?;
        Ok(Self {
            id,
            body: ResponseBody::Error(error),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_ok(&self) -> bool {
        matches!(self.body, ResponseBody::Result(_))
    }

    pub fn encode_jsonl(&self) -> Result<Vec<u8>, ProtocolError> {
        let wire = match &self.body {
            ResponseBody::Result(result) => WireResponse {
                id: &self.id,
                ok: true,
                result: Some(result),
                error: None,
            },
            ResponseBody::Error(error) => WireResponse {
                id: &self.id,
                ok: false,
                result: None,
                error: Some(error),
            },
        };
        encode_jsonl(&wire)
    }
}

#[derive(Serialize)]
struct WireResponse<'a> {
    id: &'a str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    CommandStarted {
        cmd_id: String,
        argv: Vec<String>,
        cwd: String,
    },
    Observation {
        cmd_id: String,
        body: Value,
    },
    Exit {
        cmd_id: String,
        code: i32,
        duration_ms: u64,
    },
}

impl AgentEvent {
    pub fn event_name(&self) -> &'static str {
        match self {
            AgentEvent::CommandStarted { .. } => "command_started",
            AgentEvent::Observation { .. } => "observation",
            AgentEvent::Exit { .. } => "exit",
        }
    }

    pub fn encode_jsonl(&self) -> Result<Vec<u8>, ProtocolError> {
        let value = match self {
            AgentEvent::CommandStarted { cmd_id, argv, cwd } => {
                validate_identifier(cmd_id).map_err(|()| ProtocolError::InvalidCommandId)?;
                serde_json::json!({
                    "event": self.event_name(),
                    "cmd_id": cmd_id,
                    "argv": argv,
                    "cwd": cwd,
                })
            }
            AgentEvent::Observation { cmd_id, body } => {
                validate_identifier(cmd_id).map_err(|()| ProtocolError::InvalidCommandId)?;
                serde_json::json!({
                    "event": self.event_name(),
                    "cmd_id": cmd_id,
                    "body": body,
                })
            }
            AgentEvent::Exit {
                cmd_id,
                code,
                duration_ms,
            } => {
                validate_identifier(cmd_id).map_err(|()| ProtocolError::InvalidCommandId)?;
                serde_json::json!({
                    "event": self.event_name(),
                    "cmd_id": cmd_id,
                    "code": code,
                    "duration_ms": duration_ms,
                })
            }
        };
        encode_jsonl(&value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    EmptyFrame,
    FrameTooLarge,
    MultipleFrames,
    InvalidJson(String),
    InvalidRequestId,
    InvalidSessionId,
    InvalidCommandId,
    UnsupportedOperation,
    MissingSession,
    UnexpectedSession,
    ParamsMustBeObject,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFrame => formatter.write_str("empty JSONL frame"),
            Self::FrameTooLarge => formatter.write_str("JSONL frame exceeds the protocol limit"),
            Self::MultipleFrames => formatter.write_str("expected exactly one JSONL frame"),
            Self::InvalidJson(error) => write!(formatter, "invalid JSON: {error}"),
            Self::InvalidRequestId => formatter.write_str("invalid request id"),
            Self::InvalidSessionId => formatter.write_str("invalid session id"),
            Self::InvalidCommandId => formatter.write_str("invalid command id"),
            Self::UnsupportedOperation => formatter.write_str("unsupported operation"),
            Self::MissingSession => formatter.write_str("operation requires a session"),
            Self::UnexpectedSession => formatter.write_str("session.open must not name a session"),
            Self::ParamsMustBeObject => formatter.write_str("params must be a JSON object"),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn frame_content(frame: &[u8]) -> Result<&[u8], ProtocolError> {
    if frame.len() > MAX_JSONL_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    let content = frame
        .strip_suffix(b"\r\n")
        .or_else(|| frame.strip_suffix(b"\n"))
        .unwrap_or(frame);
    if content.is_empty() {
        return Err(ProtocolError::EmptyFrame);
    }
    if content.iter().any(|byte| matches!(byte, b'\n' | b'\r')) {
        return Err(ProtocolError::MultipleFrames);
    }
    Ok(content)
}

pub(crate) fn validate_identifier(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(());
    }
    Ok(())
}

fn encode_jsonl(value: &impl Serialize) -> Result<Vec<u8>, ProtocolError> {
    let mut encoded =
        serde_json::to_vec(value).map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    if encoded.len().saturating_add(1) > MAX_JSONL_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge);
    }
    encoded.push(b'\n');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_strict_single_request_frame() {
        let request = AgentRequest::decode_jsonl(
            br#"{"id":"req_1","op":"command.run","session":"sess_1","params":{"cmd":"true"}}
"#,
        )
        .unwrap();
        assert_eq!(request.id(), "req_1");
        assert_eq!(request.operation(), AgentOperation::CommandRun);
        assert_eq!(request.session(), Some("sess_1"));
        assert_eq!(request.params()["cmd"], "true");
    }

    #[test]
    fn rejects_oversize_before_json_parsing() {
        let frame = vec![b' '; MAX_JSONL_FRAME_BYTES + 1];
        assert_eq!(
            AgentRequest::decode_jsonl(&frame),
            Err(ProtocolError::FrameTooLarge)
        );
    }

    #[test]
    fn rejects_multiple_lines_duplicate_fields_and_unknown_fields() {
        for frame in [
            br#"{"id":"a","op":"session.open","params":{}}
{"id":"b","op":"session.open","params":{}}"#
                .as_slice(),
            br#"{"id":"a","id":"b","op":"session.open","params":{}}"#.as_slice(),
            br#"{"id":"a","op":"session.open","params":{},"admin":true}"#.as_slice(),
        ] {
            assert!(
                AgentRequest::decode_jsonl(frame).is_err(),
                "accepted {frame:?}"
            );
        }
    }

    #[test]
    fn enforces_operation_session_and_param_shape() {
        let missing = br#"{"id":"a","op":"command.run","params":{}}"#;
        assert_eq!(
            AgentRequest::decode_jsonl(missing),
            Err(ProtocolError::MissingSession)
        );
        let unexpected = br#"{"id":"a","op":"session.open","session":"s","params":{}}"#;
        assert_eq!(
            AgentRequest::decode_jsonl(unexpected),
            Err(ProtocolError::UnexpectedSession)
        );
        let array = br#"{"id":"a","op":"session.open","params":[]}"#;
        assert_eq!(
            AgentRequest::decode_jsonl(array),
            Err(ProtocolError::ParamsMustBeObject)
        );
    }

    #[test]
    fn rejects_control_characters_and_unbounded_identifiers() {
        let whitespace_id = br#"{"id":"request one","op":"session.open","params":{}}"#;
        assert_eq!(
            AgentRequest::decode_jsonl(whitespace_id),
            Err(ProtocolError::InvalidRequestId)
        );
        let long_id = "x".repeat(MAX_IDENTIFIER_BYTES + 1);
        let frame = format!(r#"{{"id":"{long_id}","op":"session.open","params":{{}}}}"#);
        assert_eq!(
            AgentRequest::decode_jsonl(frame.as_bytes()),
            Err(ProtocolError::InvalidRequestId)
        );
    }

    #[test]
    fn response_and_event_encoding_are_bounded_jsonl() {
        let response = AgentResponse::success("req_1", serde_json::json!({"done": true}))
            .unwrap()
            .encode_jsonl()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&response).unwrap(),
            serde_json::json!({"id": "req_1", "ok": true, "result": {"done": true}})
        );

        let event = AgentEvent::Exit {
            cmd_id: "cmd_1".to_string(),
            code: 0,
            duration_ms: 12,
        }
        .encode_jsonl()
        .unwrap();
        assert!(event.ends_with(b"\n"));

        let huge = "x".repeat(MAX_JSONL_FRAME_BYTES);
        let response = AgentResponse::success("req_1", Value::String(huge)).unwrap();
        assert_eq!(response.encode_jsonl(), Err(ProtocolError::FrameTooLarge));
    }
}
