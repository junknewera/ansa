//! Synthetic owned runtime; production entry point remains C++ until cutover.
mod dispatch;
mod outbox;
mod owner;
mod shutdown;
mod work;
pub use crate::auth::Cancel;
pub use crate::frontend::protocol_v1::*;
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
pub type Compact = Arc<dyn Fn(&Request, &Cancel) -> Result<CompactedContext> + Send + Sync>;
pub type CheckDispatch = Arc<dyn Fn(u64, &Cancel) -> Result<()> + Send + Sync>;
pub type ToolGuard = Arc<dyn Fn(&Cancel) -> Result<Lease> + Send + Sync>;
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
    pub compact_bytes: usize,
    pub tool_rounds: usize,
    pub tool_calls: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            commands: 16,
            events: 64,
            submit_ids: 4096,
            compact_bytes: 256 * 1024,
            tool_rounds: 1,
            tool_calls: 16,
        }
    }
}
#[derive(Clone)]
pub struct Options {
    pub directory: Option<PathBuf>,
    pub compact: Option<Compact>,
    pub journal: Option<Observer>,
    pub keep_recent_turns: usize,
    pub check_dispatch: CheckDispatch,
    pub tool_guard: ToolGuard,
    pub streaming: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            directory: None,
            compact: None,
            journal: None,
            keep_recent_turns: 0,
            check_dispatch: Arc::new(|_, _| Ok(())),
            tool_guard: Arc::new(|_| Ok(Lease::default())),
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
        || limits.compact_bytes < 256
        || limits.compact_bytes > 4 * 1024 * 1024
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
    Ok(())
}
