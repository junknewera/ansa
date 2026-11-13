//! Shared owning CLI/TUI events. This is not a second source of session truth.
pub use crate::provider::contracts::{
    AgentMode, CommandId, CommandPayload, EventPayload, GetTranscript, OperationId, RequestId,
    RuntimeCommand, RuntimeEvent, RuntimeIncarnation, SessionId, Submit, TurnId, validate_command,
    validate_event,
};
use crate::provider::replay::{Effects, Execution};
use std::sync::Arc;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub operation: OperationId,
    pub call_id: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub operation: OperationId,
    pub call_id: String,
    pub execution: Execution,
    pub effects: Effects,
    pub output: Arc<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Started(Started),
    Finished(Finished),
}
impl Activity {
    pub(crate) fn operation(&self) -> &OperationId {
        match self {
            Self::Started(v) => &v.operation,
            Self::Finished(v) => &v.operation,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub generation: u64,
    pub text: String,
    pub state: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPage {
    pub begin: usize,
    pub end: usize,
    pub total: usize,
    pub entries: Vec<crate::session::transcript::Entry>,
    pub query: GetTranscript,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub through_sequence: u64,
    pub state: String,
    pub request: Option<RequestId>,
    pub worker_busy: bool,
    pub completed_tools: usize,
    pub text: String,
    pub diagnostic: String,
    pub recovery_operation: Option<OperationId>,
    pub submit: Option<CommandId>,
    pub command: Option<CommandId>,
    pub tools: Vec<Activity>,
    pub history_items: usize,
    pub compactions: usize,
    pub storage_failed: bool,
    pub durable: bool,
    pub resume_available: bool,
    pub mode: AgentMode,
    pub transcript_size: usize,
    pub preview: Option<Preview>,
}
