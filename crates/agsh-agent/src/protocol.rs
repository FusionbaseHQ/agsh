#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub id: String,
    pub op: String,
    pub session: Option<String>,
    pub params_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResponse {
    pub id: String,
    pub ok: bool,
    pub body_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    CommandStarted {
        cmd_id: String,
        argv: Vec<String>,
        cwd: String,
    },
    Observation {
        cmd_id: String,
        body_json: String,
    },
    Exit {
        cmd_id: String,
        code: i32,
        duration_ms: u128,
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
}
