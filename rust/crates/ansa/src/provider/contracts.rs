//! Typed frontend wire contract. This module contains validation only; it owns no runtime.
//!
//! Session and turn identities cannot be substituted for each other.
//! ```compile_fail,E0308
//! use ansa::provider::contracts::{SessionId, TurnId};
//! fn wrong_identity(session: SessionId) -> TurnId { session }
//! ```
use crate::{orchestration::profile::RoleProfile, protocol::json::Json, provider::replay::Effects};

pub(crate) mod image;
pub use image::{ImageInput, MAX_IMAGE_BYTES};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        #[cfg_attr(test, derive(serde::Serialize, serde::Deserialize), serde(transparent))]
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
    OrchestrationChat,
    Lead,
    LeadPlus,
}
impl AgentMode {
    pub(crate) fn read_only(self) -> bool {
        matches!(self, Self::Plan | Self::OrchestrationChat)
    }
    pub(crate) fn leads(self) -> bool {
        matches!(self, Self::Lead | Self::LeadPlus)
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submit {
    pub text: String,
    pub mode: AgentMode,
}
/// A user message for the active task, admitted independently of Submit FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Steer {
    pub target: RequestId,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationSelection {
    pub model: String,
    pub effort: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followup {
    pub command: CommandId,
    pub target: RequestId,
    pub text: String,
    pub state: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetTranscript {
    pub before: usize,
    pub limit: usize,
    pub forward: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Submit,
    Steer,
}
/// Status only: no submitted text, native items, tool payloads or auth binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageReceiptInfo {
    pub kind: MessageKind,
    pub turn: TurnId,
    pub request: Option<RequestId>,
    pub state: String,
    pub mode: Option<AgentMode>,
    /// Binds a reconnect lookup to the exact user message without exporting it.
    pub text_sha256: String,
    /// Ordered attachment hashes, omitted from legacy text-only receipt frames.
    pub image_sha256: Vec<String>,
    /// The message admission is confirmed by the canonical journal.
    pub admission_durable: bool,
    /// The reported state also matches an acknowledged journal state.
    pub state_durable: bool,
}
impl MessageReceiptInfo {
    pub(crate) fn add_image_hashes(&self, fields: &mut Vec<(&'static str, Json)>) {
        if !self.image_sha256.is_empty() {
            let hashes = self
                .image_sha256
                .iter()
                .map(|value| Json::string(value).expect("sha256 hex"))
                .collect();
            fields.push((
                "image_sha256",
                Json::array(hashes).expect("bounded image count"),
            ));
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDraft {
    pub(crate) instructions: String,
    pub(crate) format: crate::provider::structured::FinalFormat,
    pub(crate) previous_prompt: Option<String>,
    pub(crate) skill: Option<String>,
    pub(crate) profile: Option<String>,
}

impl PlanDraft {
    pub(crate) fn save(&self) -> Result<Json, &'static str> {
        let mut fields = vec![
            ("instructions", Json::string(&self.instructions)?),
            ("format", Json::string(self.format.name())?),
        ];
        if let Some(prompt) = &self.previous_prompt {
            fields.push(("previous_prompt", Json::string(prompt)?));
        }
        if let Some(skill) = &self.skill {
            fields.push(("skill", Json::string(skill)?));
        }
        if let Some(profile) = &self.profile {
            fields.push(("profile", Json::string(profile)?));
        }
        Json::object(fields)
    }

    pub(crate) fn load(value: &Json) -> Result<Self, &'static str> {
        use crate::provider::structured::FinalFormat;
        let format = FinalFormat::parse(value.text_field("format")?)?;
        if format == FinalFormat::ChildResult {
            return Err("session_draft_plan");
        }
        let optional_text = |name| {
            value
                .has(name)
                .then(|| value.text_field(name).map(str::to_owned))
                .transpose()
        };
        Ok(Self {
            instructions: value.text_field("instructions")?.to_owned(),
            format,
            previous_prompt: optional_text("previous_prompt")?,
            skill: optional_text("skill")?,
            profile: optional_text("profile")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPayload {
    SetAutocompact(u8),
    Submit(Submit),
    SubmitImages(Submit, Vec<ImageInput>),
    SubmitWithRoles(Submit, Vec<ImageInput>, Box<RoleProfile>),
    /// Native-only synthesis input from the coordinator. Frontend wire
    /// decoders accept ordinary Submit and never construct this variant.
    Handoff(Submit),
    DraftPlan(Submit, Box<PlanDraft>),
    Steer(Steer),
    SetGeneration(GenerationSelection),
    GetContext,
    GetTimeline(GetTranscript),
    GetConversation(GetTranscript),
    Cancel(RequestId),
    GetSnapshot(u64),
    Resume(RequestId),
    GetTranscript(GetTranscript),
    GetMessageReceipt(CommandId),
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
    MessageReceipt {
        query: CommandId,
        command: CommandId,
        receipt: Option<Box<MessageReceiptInfo>>,
        ready: bool,
        storage_failed: bool,
        recovery_required: bool,
    },
    FollowupQueued {
        command: CommandId,
    },
    FollowupDelivered {
        commands: Vec<CommandId>,
    },
    AutocompactSelected {
        command: CommandId,
        percent: u8,
    },
    GenerationSelected {
        command: CommandId,
        selection: GenerationSelection,
    },
    ContextSnapshot {
        command: CommandId,
        seed: crate::session::state::ForkSeed,
    },
    TimelinePage(crate::frontend::protocol_v1::TranscriptPage),
    ConversationPage(crate::frontend::protocol_v1::TranscriptPage),
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
        CommandPayload::GetMessageReceipt(id) if !valid_id(&id.0) => Err("command_id"),
        CommandPayload::Submit(submit)
        | CommandPayload::SubmitImages(submit, _)
        | CommandPayload::SubmitWithRoles(submit, _, _)
        | CommandPayload::Handoff(submit)
        | CommandPayload::DraftPlan(submit, _) => {
            if submit.text.is_empty() || !Json::text_fits_frame(&submit.text) {
                return Err("submit_size");
            }
            if let CommandPayload::SubmitImages(_, images) = &command.payload {
                image::validate_images(images)?;
            }
            if let CommandPayload::SubmitWithRoles(_, images, profile) = &command.payload {
                if submit.mode != AgentMode::LeadPlus {
                    return Err("submit_mode");
                }
                if !images.is_empty() {
                    image::validate_images(images)?;
                }
                profile.validate()?;
            }
            if submit.mode == AgentMode::OrchestrationChat
                && !matches!(command.payload, CommandPayload::Submit(_))
            {
                return Err("submit_mode");
            }
            if submit.mode.leads()
                && !matches!(
                    command.payload,
                    CommandPayload::Submit(_)
                        | CommandPayload::SubmitImages(..)
                        | CommandPayload::SubmitWithRoles(..)
                )
            {
                return Err("submit_mode");
            }
            if matches!(command.payload, CommandPayload::Handoff(_))
                && submit.mode != AgentMode::Plan
            {
                return Err("handoff_read_only");
            }
            Ok(())
        }
        CommandPayload::Steer(message) => {
            if !valid_id(&message.target.0)
                || message.text.is_empty()
                || !Json::text_fits_frame(&message.text)
            {
                return Err("steer_contract");
            }
            Ok(())
        }
        CommandPayload::SetAutocompact(percent) if !(1..=100).contains(percent) => {
            Err("autocompact_range")
        }
        CommandPayload::SetGeneration(selection) => {
            if selection.model.is_empty()
                || selection.model.len() > 256
                || selection.model.chars().any(char::is_control)
                || selection.effort.as_ref().is_some_and(|v| !valid_id(v))
            {
                return Err("generation_selection");
            }
            Ok(())
        }
        CommandPayload::GetTranscript(query)
        | CommandPayload::GetTimeline(query)
        | CommandPayload::GetConversation(query)
            if query.limit == 0 || query.limit > 64 =>
        {
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
