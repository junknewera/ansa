//! Synthetic owned runtime; production entry point remains C++ until cutover.
mod dispatch;
mod outbox;
mod owner;
mod shutdown;
mod work;
pub use crate::auth::Cancel;
pub use crate::frontend::protocol_v1::*;
pub use crate::session::journal_v1::DirectoryIdentity;
pub use crate::session::state::ForkSeed;
use crate::{
    context::legacy_compact::CompactedContext,
    provider::{request::Request, responses::AdmittedResponse},
    session::journal_v1::Observer,
    tools::registry::Snapshot as ToolSnapshot,
};
use std::{path::PathBuf, sync::Arc};
pub type Result<T> = std::result::Result<T, &'static str>;
pub type PreviewSink = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;
pub type Generate =
    Arc<dyn Fn(&Request, &Cancel, PreviewSink) -> Result<AdmittedResponse> + Send + Sync>;
pub type Compact = Arc<dyn Fn(&Request, usize, &Cancel) -> Result<CompactedContext> + Send + Sync>;
pub type CheckDispatch = Arc<dyn Fn(u64, &Cancel) -> Result<()> + Send + Sync>;
pub type CheckRequest = Arc<dyn Fn(&Request, u64, &Cancel) -> Result<()> + Send + Sync>;
pub type SelectGeneration = Arc<dyn Fn(&GenerationSelection, &Cancel) -> Result<()> + Send + Sync>;
pub type ToolGuard = Arc<dyn Fn(&Cancel) -> Result<Lease> + Send + Sync>;
pub type PlanGuard = Arc<dyn Fn() -> Result<Box<dyn Send>> + Send + Sync>;
pub type ModelWindow = Arc<dyn Fn(&Request) -> Option<usize> + Send + Sync>;
/// Ownership is retained and dropped on the auth worker, never on the reactor.
pub struct Lease {
    pub ownership: Box<dyn Send>,
    pub stop: Cancel,
}
impl Default for Lease {
    fn default() -> Self {
        Self {
            ownership: Box::new(()),
            stop: Cancel::default(),
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub commands: usize,
    pub events: usize,
    pub submit_ids: usize,
    pub tool_rounds: usize,
    pub tool_calls: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            commands: 16,
            events: 64,
            submit_ids: 4096,
            tool_rounds: 1,
            tool_calls: 16,
        }
    }
}
#[derive(Clone)]
pub struct Options {
    pub fork_seed: Option<crate::session::state::ForkSeed>,
    pub check_request: CheckRequest,
    pub select_generation: SelectGeneration,
    pub directory: Option<PathBuf>,
    /// Require the original initialized journal when resuming a child.
    pub existing_journal: Option<DirectoryIdentity>,
    pub inspect_only: bool,
    pub compact: Option<Compact>,
    pub model_window: ModelWindow,
    pub journal: Option<Observer>,
    pub keep_recent_turns: usize,
    pub check_dispatch: CheckDispatch,
    pub tool_guard: ToolGuard,
    pub plan_guard: Option<PlanGuard>,
    pub streaming: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            fork_seed: None,
            check_request: Arc::new(|_, _, _| Ok(())),
            select_generation: Arc::new(|_, _| Err("generation_selection_unavailable")),
            directory: None,
            existing_journal: None,
            inspect_only: false,
            compact: None,
            model_window: Arc::new(|_| None),
            journal: None,
            keep_recent_turns: 0,
            check_dispatch: Arc::new(|_, _| Ok(())),
            tool_guard: Arc::new(|_| Ok(Lease::default())),
            plan_guard: None,
            streaming: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostStatus {
    Accepted,
    QueueFull,
    Closed,
    StaleTarget,
    StaleIncarnation,
    Duplicate,
    CommandIdLimit,
    CommandConflict,
}

/// Result of inspecting an exact child Submit through the native owner.
/// It bypasses the lossy frontend event stream and is never a JSONL event.
#[derive(Debug)]
pub struct ChildAttempt {
    session: SessionId,
    incarnation: RuntimeIncarnation,
    journal: DirectoryIdentity,
    command: CommandId,
    turn: TurnId,
    mode: AgentMode,
    text_sha256: String,
    state: ChildAttemptState,
    latest_tool: Option<(String, Option<String>)>,
}

impl ChildAttempt {
    pub fn session(&self) -> &SessionId {
        &self.session
    }
    pub(crate) fn incarnation(&self) -> &RuntimeIncarnation {
        &self.incarnation
    }
    pub fn journal(&self) -> DirectoryIdentity {
        self.journal
    }
    pub fn command(&self) -> &CommandId {
        &self.command
    }
    pub fn turn(&self) -> &TurnId {
        &self.turn
    }
    pub fn mode(&self) -> AgentMode {
        self.mode
    }
    pub fn text_sha256(&self) -> &str {
        &self.text_sha256
    }
    pub fn state(&self) -> &ChildAttemptState {
        &self.state
    }
    pub(crate) fn latest_tool(&self) -> Option<(&str, Option<&str>)> {
        self.latest_tool
            .as_ref()
            .map(|(name, target)| (name.as_str(), target.as_deref()))
    }
    pub fn into_state(self) -> ChildAttemptState {
        self.state
    }
}

#[derive(Debug)]
pub enum ChildAttemptState {
    Missing,
    Pending,
    Durable {
        request: RequestId,
        state: String,
        /// The exact turn and all of its workers have settled. A cancelled
        /// journal transition by itself cannot release a writer's slot.
        quiescent: bool,
        resume_required: bool,
        recovery_required: bool,
        final_response: Option<Arc<AdmittedResponse>>,
    },
}
pub use dispatch::DurableIntent;
pub use shutdown::{Handle, Runtime};
pub(crate) fn valid_configuration(
    session: &SessionId,
    config: &Request,
    tools: &ToolSnapshot,
    limits: Limits,
    options: &Options,
) -> Result<()> {
    if !crate::provider::contracts::valid_id(&session.0)
        || config.tools != *tools.advertisement()
        || !config.input.is_empty()
        || limits.commands == 0
        || limits.commands > 256
        || limits.events < 2
        || limits.events > 256
        || limits.submit_ids == 0
        || limits.submit_ids > 65536
        || limits.tool_rounds == 0
        || limits.tool_rounds > 64
        || limits.tool_calls == 0
        || limits.tool_calls > 256
    {
        return Err("runtime_configuration");
    }
    if tools.requires_journal() && options.directory.is_none() {
        return Err("durable_tools_require_session");
    }
    if options.existing_journal.is_some() && options.directory.is_none() {
        return Err("missing_journal_directory");
    }
    if options.inspect_only && options.existing_journal.is_none() {
        return Err("inspection_requires_existing_journal");
    }
    if !tools.owner_matches(&session.0, options.directory.as_deref()) {
        return Err("tool_owner_binding");
    }
    Ok(())
}
