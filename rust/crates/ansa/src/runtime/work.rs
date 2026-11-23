//! Fixed owned blocking workers. No blocking callback executes on the owner.
use super::{
    dispatch::{DurableIntent, Gate},
    *,
};
use crate::{
    protocol::json::Json,
    provider::replay::{Effects, Execution, ToolResult},
    session::{
        codec_v1::{Invocation, SavedResult, next_request, response_text},
        state::State,
        store::Store,
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        registry::Snapshot as Tools,
    },
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, mpsc as blocking},
    thread::{self, JoinHandle},
};
use tokio::sync::{mpsc, oneshot, watch};
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    pub incarnation: RuntimeIncarnation,
    pub turn: TurnId,
    pub request: RequestId,
    pub resume_epoch: u64,
    pub generation: u64,
}
impl Identity {
    pub fn same_turn(&self, other: &Self) -> bool {
        self.incarnation == other.incarnation
            && self.turn == other.turn
            && self.request == other.request
            && self.resume_epoch == other.resume_epoch
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Queued,
    SideReply,
    Steered,
    GenerationSelected,
    AutocompactSelected,
    Accepted,
    Response,
    Intent,
    Outcome,
    Continued,
    Compacted,
    Done,
    Pause,
    Cancel,
    Resume,
}
pub(crate) type Build = Box<dyn FnOnce(&State) -> Result<Json> + Send>;
pub(crate) struct StoreJob {
    pub identity: Identity,
    pub kind: Kind,
    pub build: Build,
}
/// Created only after the append/fsync boundary succeeds.
pub(crate) struct Receipt {
    identity: Identity,
    kind: Kind,
    event: Json,
    durable: bool,
}
impl Receipt {
    pub fn event(&self) -> &Json {
        &self.event
    }
    pub fn intent(self) -> Result<DurableIntent> {
        if self.kind != Kind::Intent {
            return Err("intent_receipt_kind");
        }
        Ok(DurableIntent {
            identity: self.identity,
            invocation: Invocation::load(self.event.field("invocation")?)?,
            durable: self.durable,
        })
    }
}
pub(crate) struct Stored {
    pub identity: Identity,
    pub kind: Kind,
    pub state: Arc<State>,
    pub result: Result<Receipt>,
    pub known: Option<SavedResult>,
    pub rejected_frame: bool,
}
pub(crate) struct Ready {
    pub state: Arc<State>,
    pub journal_identity: Option<DirectoryIdentity>,
    pub diagnostic: &'static str,
    pub requires_resume: bool,
}
pub(crate) enum ProviderTask {
    Generate,
    SideReply(CommandId),
    Compact,
}
pub(crate) struct ProviderJob {
    pub identity: Identity,
    pub state: Arc<State>,
    pub stop: Cancel,
    pub task: ProviderTask,
    pub preview_generation: u64,
}
pub(crate) struct PreviewUpdate {
    pub identity: Identity,
    pub generation: u64,
    pub text: String,
}
pub(crate) enum ProviderValue {
    Response(Arc<AdmittedResponse>),
    Compacted(Box<CompactedContext>),
}
pub(crate) enum ToolJob {
    Prepare {
        identity: Identity,
        state: Arc<State>,
        tools: Arc<Tools>,
    },
    Execute {
        identity: Identity,
        permit: DurableIntent,
        tools: Arc<Tools>,
        stop: Cancel,
        lease_stop: Cancel,
    },
    Recover {
        identity: Identity,
        invocation: Invocation,
        tools: Arc<Tools>,
    },
}
pub(crate) enum ToolValue {
    Plan(Vec<Invocation>),
    Outcome(SavedResult),
    Recovered(Option<SavedResult>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthTask {
    SelectGeneration,
    Generate,
    Compact,
    BeforeIntent,
    ToolLease,
    ChildDispatch,
}
pub(crate) struct AuthJob {
    pub selection: Option<GenerationSelection>,
    pub request: Option<Request>,
    pub identity: Identity,
    pub stop: Cancel,
    pub task: AuthTask,
}
pub(crate) enum PlanTarget {
    Submit(RuntimeCommand),
    Resume(CommandId),
}
pub(crate) enum Completion {
    Plan {
        target: PlanTarget,
        result: Result<Box<dyn Send>>,
    },
    Stored(Stored),
    Provider {
        identity: Identity,
        result: Result<ProviderValue>,
    },
    Tool {
        identity: Identity,
        result: Result<ToolValue>,
    },
    Auth {
        identity: Identity,
        task: AuthTask,
        result: Result<Cancel>,
    },
    AuthReleased {
        identity: Identity,
        result: Result<()>,
    },
}
pub(crate) struct Workers {
    pub store: Option<blocking::SyncSender<StoreJob>>,
    pub provider: Option<blocking::SyncSender<ProviderJob>>,
    pub tool: Option<blocking::SyncSender<ToolJob>>,
    pub auth: Option<blocking::SyncSender<AuthJob>>,
    pub release: Option<blocking::SyncSender<Identity>>,
    pub plan: Option<blocking::SyncSender<PlanTarget>>,
    joins: Vec<JoinHandle<()>>,
}
fn guarded<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or(Err("worker_failure"))
}
pub(crate) fn not_started(invocation: &Invocation) -> SavedResult {
    SavedResult {
        result: ToolResult::new(
            invocation.operation.clone(),
            invocation.call.clone(),
            r#"{"error":"cancelled_before_dispatch"}"#.into(),
            Execution::Cancelled,
            Effects::NotStarted,
        ),
        dispatched: false,
    }
}
fn outcome(invocation: &Invocation, value: crate::tools::registry::Outcome) -> SavedResult {
    let fallback = || SavedResult {
        result: ToolResult::new(
            invocation.operation.clone(),
            invocation.call.clone(),
            r#"{"error":"executor_failure","effects":"unknown"}"#.into(),
            Execution::Failed,
            Effects::Unknown,
        ),
        dispatched: true,
    };
    if value.execution == Execution::Succeeded && value.effects != Effects::KnownResult {
        return fallback();
    }
    match String::from_utf8(value.output) {
        Ok(output) if Json::text_fits_frame(&output) => SavedResult {
            result: ToolResult::new(
                invocation.operation.clone(),
                invocation.call.clone(),
                output,
                value.execution,
                value.effects,
            ),
            dispatched: true,
        },
        _ => fallback(),
    }
}
impl Workers {
    #[allow(clippy::too_many_arguments)] // Explicit ownership transfer to fixed worker roles.
    pub fn start(
        initial: State,
        options: Options,
        generate: Generate,
        gate: Arc<Gate>,
        done: mpsc::Sender<Completion>,
        started: mpsc::Sender<(Identity, Started)>,
        preview: watch::Sender<Option<Arc<PreviewUpdate>>>,
        ready: oneshot::Sender<Result<Ready>>,
    ) -> Result<Self> {
        let mut workers = Self {
            store: None,
            provider: None,
            tool: None,
            auth: None,
            release: None,
            plan: None,
            joins: vec![],
        };
        if let Some(check) = options.plan_guard.clone() {
            let (sender, receiver) = blocking::sync_channel::<PlanTarget>(1);
            workers.plan = Some(sender);
            let output = done.clone();
            workers.joins.push(
                thread::Builder::new()
                    .name("ansa-plan-workspace".into())
                    .spawn(move || {
                        while let Ok(target) = receiver.recv() {
                            let result = guarded(|| check());
                            if output
                                .blocking_send(Completion::Plan { target, result })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .map_err(|_| "plan_worker")?,
            );
        }
        let (sender, receiver) = blocking::sync_channel::<StoreJob>(1);
        workers.store = Some(sender);
        let channel = done.clone();
        let init_options = options.clone();
        workers.joins.push(
            thread::Builder::new()
                .name("ansa-journal".into())
                .spawn(move || {
                    let initialized = guarded(|| {
                        initial.configuration.json()?;
                        if let Some(path) = &init_options.directory {
                            let store = Store::open_with_seed(
                                path,
                                initial,
                                init_options.journal.clone(),
                                init_options.fork_seed.as_ref(),
                                init_options.existing_journal,
                                init_options.autocompact_percent,
                            )?;
                            let ready = Ready {
                                state: store.state().clone(),
                                journal_identity: Some(store.identity()?),
                                diagnostic: store.diagnostic(),
                                requires_resume: store.requires_resume(),
                            };
                            Ok((Some(store), ready))
                        } else {
                            let mut state = initial;
                            state.apply(&state.initialization_with_seed(
                                init_options.fork_seed.as_ref(),
                                init_options.autocompact_percent,
                            )?)?;
                            Ok((
                                None,
                                Ready {
                                    state: Arc::new(state),
                                    journal_identity: None,
                                    diagnostic: "",
                                    requires_resume: false,
                                },
                            ))
                        }
                    });
                    let Ok((mut store, ready_value)) = initialized else {
                        let _ = ready.send(Err(initialized.err().unwrap_or("journal_failure")));
                        return;
                    };
                    let mut state = ready_value.state.clone();
                    let mut poisoned = !ready_value.diagnostic.is_empty();
                    if ready.send(Ok(ready_value)).is_err() {
                        return;
                    }
                    while let Ok(job) = receiver.recv() {
                        let mut known = None;
                        let mut built = false;
                        let result = if poisoned {
                            Err("journal_read_only")
                        } else {
                            guarded(|| {
                                let event = (job.build)(&state)?;
                                if matches!(
                                    job.kind,
                                    Kind::Accepted
                                        | Kind::Response
                                        | Kind::Compacted
                                        | Kind::SideReply
                                ) {
                                    crate::session::journal_v1::check_record_frame(&event)?;
                                }
                                built = true;
                                if let Some(backend) = &mut store {
                                    let result = backend.commit(&event);
                                    known = backend.known_outcome().cloned();
                                    result?;
                                    state = backend.state().clone();
                                } else {
                                    let mut next = (*state).clone();
                                    next.apply(&event)?;
                                    state = Arc::new(next);
                                }
                                Ok(Receipt {
                                    identity: job.identity.clone(),
                                    kind: job.kind,
                                    event,
                                    durable: store.is_some(),
                                })
                            })
                        };
                        let rejected_frame = !poisoned
                            && matches!(
                                job.kind,
                                Kind::Accepted | Kind::Response | Kind::Compacted | Kind::SideReply
                            )
                            && !built
                            && matches!(
                                result,
                                Err("json_build_limit"
                                    | "json_structure_limit"
                                    | "request_context")
                            );
                        if result.is_err() && !rejected_frame {
                            poisoned = true;
                        }
                        let value = Stored {
                            identity: job.identity,
                            kind: job.kind,
                            state: state.clone(),
                            result,
                            known,
                            rejected_frame,
                        };
                        if channel.blocking_send(Completion::Stored(value)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|_| "journal_worker")?,
        );
        let (sender, receiver) = blocking::sync_channel::<ProviderJob>(1);
        workers.provider = Some(sender);
        let channel = done.clone();
        let provider_options = options.clone();
        let provider_gate = gate.clone();
        workers.joins.push(
            thread::Builder::new()
                .name("ansa-provider".into())
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let identity = job.identity.clone();
                        let result = guarded(|| {
                            if job.stop.cancelled() || !provider_gate.begin(&job.identity, None) {
                                return Err("generation_cancelled");
                            }
                            let current = job.state.turn.as_ref().ok_or("runtime_turn")?;
                            match &job.task {
                                ProviderTask::Generate | ProviderTask::SideReply(_) => {
                                    let mut request =
                                        if let ProviderTask::SideReply(command) = &job.task {
                                            let request = job.state.side_request(command)?;
                                            (provider_options.check_dispatch)(
                                                job.identity.resume_epoch,
                                                &job.stop,
                                            )?;
                                            (provider_options.check_request)(
                                                &request,
                                                job.identity.resume_epoch,
                                                &job.stop,
                                            )?;
                                            job.stop.check()?;
                                            if !provider_gate.active(&job.identity) {
                                                return Err("generation_cancelled");
                                            }
                                            request
                                        } else if let Some(response) = &current.tools_response {
                                            next_request(response, &current.results)?
                                        } else {
                                            current.next.clone()
                                        };
                                    let estimate = job.state.context_estimate(&request, || {
                                        (provider_options.model_window)(&request)
                                    });
                                    let tokens = if let Some((tokens, window)) = estimate {
                                        crate::context::require_response_room(tokens, window)?;
                                        tokens
                                    } else {
                                        crate::context::estimate_request_tokens(&request)
                                    };
                                    request.set_context_estimate(tokens);
                                    let (
                                        sink_gate,
                                        sink_preview,
                                        sink_stop,
                                        sink_id,
                                        sink_generation,
                                    ) = (
                                        provider_gate.clone(),
                                        preview.clone(),
                                        job.stop.clone(),
                                        job.identity.clone(),
                                        job.preview_generation,
                                    );
                                    let inline = matches!(&job.task, ProviderTask::SideReply(_));
                                    let update: PreviewSink = Arc::new(move |text: &str| {
                                        if text.len() > 64 * 1024 {
                                            return Err("preview_text_limit");
                                        }
                                        if !inline
                                            && !sink_stop.cancelled()
                                            && sink_gate.active(&sink_id)
                                        {
                                            sink_preview.send_replace(Some(Arc::new(
                                                PreviewUpdate {
                                                    identity: sink_id.clone(),
                                                    generation: sink_generation,
                                                    text: text.into(),
                                                },
                                            )));
                                        }
                                        Ok(())
                                    });
                                    let value = crate::benchmark_trace::generation(
                                        &job.state.session.0,
                                        "generate",
                                        || generate(&request, &job.stop, update),
                                    )?;
                                    let value = if value.context_budget().is_none() {
                                        value
                                            .with_context_window(estimate.map(|(_, window)| window))
                                    } else {
                                        value
                                    };
                                    if value.request().save()? != request.save()?
                                        || value.returned_model() != request.binding.returned_model
                                    {
                                        return Err("provider_request_mismatch");
                                    }
                                    if value.items().len()
                                        > crate::protocol::sse::Limits::default().items
                                    {
                                        return Err("runtime_response_limit");
                                    }
                                    if matches!(&job.task, ProviderTask::SideReply(_))
                                        && (!value.calls().is_empty()
                                            || response_text(&value)?.is_empty())
                                    {
                                        return Err("session_side_reply");
                                    }
                                    if value.calls().is_empty() || provider_options.streaming {
                                        response_text(&value)?;
                                    }
                                    Ok(ProviderValue::Response(Arc::new(
                                        value.with_context_estimate(tokens),
                                    )))
                                }
                                ProviderTask::Compact => {
                                    let request = job.state.compaction_request()?;
                                    let prefix = match job.state.compaction_prefix() {
                                        Some(prefix) => prefix,
                                        None => {
                                            crate::context::legacy_compact::summary_prefix_size(
                                                &request,
                                            )?
                                        }
                                    };
                                    let compact = provider_options
                                        .compact
                                        .as_ref()
                                        .ok_or("compaction_unavailable")?;
                                    let value = crate::benchmark_trace::generation(
                                        &job.state.session.0,
                                        "compact",
                                        || compact(&request, prefix, &job.stop),
                                    )?;
                                    if value.request().save()? != request.save()? {
                                        return Err("compaction_request_mismatch");
                                    }
                                    if value
                                        .summary_prefix()?
                                        .is_some_and(|retained| retained != prefix)
                                    {
                                        return Err("compaction_boundary_mismatch");
                                    }
                                    if value.output()?.len() + 1
                                        >= crate::session::state::COMPACT_ITEM_THRESHOLD
                                    {
                                        return Err("compaction_item_limit");
                                    }
                                    if value.output()?.len() >= request.input.len()
                                        && crate::context::estimate_input_tokens(value.output()?)
                                            >= crate::context::estimate_input_tokens(&request.input)
                                    {
                                        return Err("compaction_no_progress");
                                    }
                                    Ok(ProviderValue::Compacted(Box::new(value)))
                                }
                            }
                        });
                        if channel
                            .blocking_send(Completion::Provider { identity, result })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .map_err(|_| "provider_worker")?,
        );
        let (sender, receiver) = blocking::sync_channel::<ToolJob>(1);
        workers.tool = Some(sender);
        let channel = done.clone();
        let tool_gate = gate.clone();
        workers.joins.push(
            thread::Builder::new()
                .name("ansa-tool".into())
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let (identity, result) = match job {
                            ToolJob::Prepare {
                                identity,
                                state,
                                tools,
                            } => {
                                let result = guarded(|| {
                                    let current = state.turn.as_ref().ok_or("runtime_turn")?;
                                    let response = current
                                        .tools_response
                                        .as_ref()
                                        .ok_or("runtime_response")?;
                                    let mut ids = OperationIds::new(
                                        &current.operation_run,
                                        current.operation_offset as u64,
                                    )?;
                                    let plan = PreparedBatch::prepare(response, tools, &mut ids)?
                                        .into_plan()?;
                                    if current.results.len() > plan.len() {
                                        return Err("completed_prefix_size");
                                    }
                                    for (invocation, result) in plan.iter().zip(&current.results) {
                                        if invocation.operation != result.result.operation_id()
                                            || invocation.call != result.result.call_id()
                                            || !result.dispatched
                                            || result.result.effects() != Effects::KnownResult
                                        {
                                            return Err("completed_prefix_conflict");
                                        }
                                    }
                                    Ok(ToolValue::Plan(plan))
                                });
                                (identity, result)
                            }
                            ToolJob::Execute {
                                identity,
                                permit,
                                tools,
                                stop,
                                lease_stop,
                            } => {
                                let invocation = permit.invocation;
                                let mut saved = not_started(&invocation);
                                if (!tools
                                    .policy(&invocation.name)
                                    .is_some_and(|p| p.requires_journal)
                                    || permit.durable)
                                    && permit.identity.same_turn(&identity)
                                    && !stop.cancelled()
                                    && !lease_stop.cancelled()
                                {
                                    let value = Started {
                                        operation: OperationId(invocation.operation.clone()),
                                        call_id: invocation.call.clone(),
                                        name: invocation.name.clone(),
                                    };
                                    if tool_gate.begin(&identity, Some((&started, value))) {
                                        let combined = Cancel::combined(&stop, &lease_stop);
                                        let returned = crate::benchmark_trace::tool(
                                            &invocation.operation,
                                            || {
                                                catch_unwind(AssertUnwindSafe(|| {
                                                    let definition = tools
                                                        .definition(&invocation.name)
                                                        .ok_or("unknown_tool")?;
                                                    let policy = tools
                                                        .policy(&invocation.name)
                                                        .ok_or("unknown_tool")?;
                                                    Ok::<_, &'static str>(
                                                        if let Some(invoke) = &policy.invoke {
                                                            invoke(&invocation, &combined)
                                                        } else {
                                                            (definition.executor)(
                                                                &invocation.arguments,
                                                            )
                                                        },
                                                    )
                                                }))
                                            },
                                        );
                                        saved = match returned {
                                            Ok(Ok(value)) => outcome(&invocation, value),
                                            _ => outcome(
                                                &invocation,
                                                crate::tools::registry::Outcome::failed_unknown(),
                                            ),
                                        };
                                    }
                                }
                                (identity, Ok(ToolValue::Outcome(saved)))
                            }
                            ToolJob::Recover {
                                identity,
                                invocation,
                                tools,
                            } => {
                                let result = guarded(|| {
                                    let value = if let Some(reader) = tools
                                        .policy(&invocation.name)
                                        .and_then(|p| p.recover.as_ref())
                                    {
                                        reader(&invocation)?.map(|v| outcome(&invocation, v))
                                    } else {
                                        None
                                    };
                                    Ok(ToolValue::Recovered(value))
                                });
                                (identity, result)
                            }
                        };
                        if channel
                            .blocking_send(Completion::Tool { identity, result })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .map_err(|_| "tool_worker")?,
        );
        let (sender, receiver) = blocking::sync_channel::<AuthJob>(1);
        let (release, release_receiver) = blocking::sync_channel::<Identity>(1);
        workers.auth = Some(sender);
        workers.release = Some(release);
        let channel = done;
        workers.joins.push(
            thread::Builder::new()
                .name("ansa-auth-io".into())
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let identity = job.identity.clone();
                        let mut lease = None;
                        let result = guarded(|| {
                            if job.task == AuthTask::SelectGeneration {
                                if job.stop.cancelled() {
                                    return Err("generation_cancelled");
                                }
                                (options.select_generation)(
                                    job.selection.as_ref().ok_or("generation_selection")?,
                                    &job.stop,
                                )?;
                                return Ok(Cancel::default());
                            }
                            if job.stop.cancelled() || !gate.begin(&job.identity, None) {
                                return Err("generation_cancelled");
                            }
                            (options.check_dispatch)(job.identity.resume_epoch, &job.stop)?;
                            if let Some(request) = &job.request {
                                (options.check_request)(
                                    request,
                                    job.identity.resume_epoch,
                                    &job.stop,
                                )?;
                            }
                            if job.stop.cancelled() || !gate.active(&job.identity) {
                                return Err("generation_cancelled");
                            }
                            if job.task == AuthTask::ToolLease {
                                lease = Some((options.tool_guard)(&job.stop)?);
                                Ok(lease.as_ref().expect("owned lease").stop.clone())
                            } else {
                                Ok(Cancel::default())
                            }
                        });
                        let holding = lease.is_some();
                        if channel
                            .blocking_send(Completion::Auth {
                                identity: identity.clone(),
                                task: job.task,
                                result,
                            })
                            .is_err()
                        {
                            break;
                        }
                        if holding {
                            let released = release_receiver.recv();
                            let cleanup = guarded(|| {
                                drop(lease);
                                Ok(())
                            })
                            .map_err(|_| "auth_cleanup_failed");
                            let result = if released.as_ref().ok() != Some(&identity) {
                                Err("auth_release_identity")
                            } else {
                                cleanup
                            };
                            if channel
                                .blocking_send(Completion::AuthReleased { identity, result })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                })
                .map_err(|_| "auth_worker")?,
        );
        Ok(workers)
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        self.store.take();
        self.provider.take();
        self.tool.take();
        self.auth.take();
        self.release.take();
        self.plan.take();
        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}
