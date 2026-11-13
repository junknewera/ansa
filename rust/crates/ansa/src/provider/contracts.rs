//! Typed frontend wire contract. This module contains validation only; it owns no runtime.
//!
//! Session and turn identities cannot be substituted for each other.
//! ```compile_fail,E0308
//! use ansa::provider::contracts::{SessionId, TurnId};
//! fn wrong_identity(session: SessionId) -> TurnId { session }
//! ```
use crate::provider::replay::Effects;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name(pub String);
        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}
id_type!(SessionId);
id_type!(TurnId);
id_type!(RequestId);
id_type!(OperationId);
id_type!(CommandId);
id_type!(RuntimeIncarnation);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    Basic,
    Plan,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub text: String,
    pub mode: AgentMode,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetTranscript {
    pub before: usize,
    pub limit: usize,
    pub forward: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPayload {
    Submit(Submit),
    Cancel(RequestId),
    GetSnapshot(u64),
    Resume(RequestId),
    GetTranscript(GetTranscript),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCommand {
    pub schema_version: u32,
    pub session: SessionId,
    pub turn: TurnId,
    pub incarnation: RuntimeIncarnation,
    pub command: CommandId,
    pub payload: CommandPayload,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPayload {
    CommandAccepted {
        command: CommandId,
        request: Option<RequestId>,
    },
    CommandReplayed {
        command: CommandId,
        state: String,
    },
    TextDelta {
        text: String,
    },
    ToolStarted(crate::frontend::protocol_v1::Started),
    ToolFinished(crate::frontend::protocol_v1::Finished),
    Cancelled {
        command: CommandId,
    },
    Done,
    RuntimeError {
        command: Option<CommandId>,
        diagnostic: String,
    },
    FrontendSnapshot(Box<crate::frontend::protocol_v1::Snapshot>),
    TranscriptPage(crate::frontend::protocol_v1::TranscriptPage),
    Preview(crate::frontend::protocol_v1::Preview),
    Snapshot {
        through_sequence: u64,
        state: String,
    },
    ResyncRequired {
        oldest_available: u64,
    },
    RecoveryRequired {
        operation: OperationId,
        effects: Effects,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEvent {
    pub schema_version: u32,
    pub stream_sequence: u64,
    pub session: SessionId,
    pub turn: TurnId,
    pub incarnation: RuntimeIncarnation,
    pub request: Option<RequestId>,
    pub payload: EventPayload,
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn validate_command(command: &RuntimeCommand) -> Result<(), &'static str> {
    if command.schema_version != 1
        || !valid_id(&command.session.0)
        || !valid_id(&command.turn.0)
        || !valid_id(&command.incarnation.0)
        || !valid_id(&command.command.0)
    {
        return Err("command_contract");
    }
    match &command.payload {
        CommandPayload::Cancel(id) | CommandPayload::Resume(id) if !valid_id(&id.0) => {
            Err("request_id")
        }
        CommandPayload::Submit(submit) => {
            if submit.text.is_empty() || submit.text.len() > 16 * 1024 {
                return Err("submit_size");
            }
            if submit.mode != AgentMode::Basic && submit.mode != AgentMode::Plan {
                return Err("submit_mode");
            }
            Ok(())
        }
        CommandPayload::GetTranscript(query) if query.limit == 0 || query.limit > 64 => {
            Err("transcript_page_limit")
        }
        _ => Ok(()),
    }
}
pub fn validate_event(event: &RuntimeEvent) -> Result<(), &'static str> {
    if event.schema_version != 1
        || event.stream_sequence == 0
        || !valid_id(&event.session.0)
        || !valid_id(&event.turn.0)
        || !valid_id(&event.incarnation.0)
        || event.request.as_ref().is_some_and(|id| !valid_id(&id.0))
    {
        return Err("event_contract");
    }
    if let EventPayload::Snapshot {
        through_sequence, ..
    } = event.payload
        && through_sequence > event.stream_sequence
    {
        return Err("snapshot_sequence");
    }
    if let EventPayload::FrontendSnapshot(snapshot) = &event.payload
        && snapshot.through_sequence > event.stream_sequence
    {
        return Err("snapshot_sequence");
    }
    Ok(())
}
