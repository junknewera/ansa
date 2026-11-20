//! One current-thread control owner. Only acknowledged storage state is adopted.
use super::{
    dispatch::Gate,
    outbox::Outbox,
    shutdown::{ChildLookup, Closing, Envelope, Mailboxes},
    work::*,
    *,
};
use crate::{
    protocol::json::Json,
    provider::replay::Effects,
    session::{
        codec_v1::{Invocation, SavedResult, response_text, save_response},
        state::{Receipt as SubmitReceipt, State},
    },
    tools::registry::Snapshot as Tools,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, atomic::Ordering},
};
use tokio::sync::{mpsc, oneshot, watch};
struct PendingStore {
    identity: Identity,
    kind: Kind,
    known: Option<SavedResult>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolTask {
    Prepare,
    Execute,
    Recover,
}
pub(crate) struct Owner {
    state: Arc<State>,
    journal_identity: Option<DirectoryIdentity>,
    tools: Arc<Tools>,
    plan_tools: Arc<Tools>,
    limits: Limits,
    options: Options,
    incarnation: RuntimeIncarnation,
    closing: Arc<Closing>,
    outbox: Arc<Outbox>,
    gate: Arc<Gate>,
    mail: Mailboxes,
    done: mpsc::Receiver<Completion>,
    starts: mpsc::Receiver<(Identity, Started)>,
    previews: watch::Receiver<Option<Arc<PreviewUpdate>>>,
    initialized: bool,
    starting: bool,
    recovery_attempted: bool,
    phase: String,
    diagnostic: String,
    storage_failed: bool,
    recovered_pause: bool,
    recovery: Option<OperationId>,
    receipts: BTreeMap<String, SubmitReceipt>,
    queued: VecDeque<RuntimeCommand>,
    new_followups: VecDeque<RuntimeCommand>,
    control_replies: BTreeMap<String, oneshot::Sender<Result<PostStatus>>>,
    control_commands: BTreeMap<String, RuntimeCommand>,
    selections: VecDeque<RuntimeCommand>,
    selecting: Option<RuntimeCommand>,
    selection_stop: Option<Cancel>,
    deferred_completions: VecDeque<Completion>,
    pending_receipt: Option<SubmitReceipt>,
    cancelling: Option<CommandId>,
    resuming: Option<CommandId>,
    plan_admission_pending: bool,
    plan_workspace: Option<Box<dyn Send>>,
    generation: u64,
    resume_epoch: u64,
    preview_sequence: u64,
    preview: Option<Preview>,
    activity: Vec<Activity>,
    turn_stop: Cancel,
    storing: Option<PendingStore>,
    provider: Option<Identity>,
    tool: Option<(Identity, ToolTask)>,
    auth: Option<(Identity, AuthTask, bool)>,
    release_sent: bool,
    lease_stop: Option<Cancel>,
    plan: Option<(Vec<Invocation>, Arc<Tools>)>,
    permit: Option<DurableIntent>,
    owned_intent: bool,
    pending_outcome: Option<SavedResult>,
    pending_pause: Option<&'static str>,
    shutting_down: bool,
}
fn number(n: usize) -> Result<Json> {
    Json::parse(&n.to_string())
}
fn transition(turn: &TurnId, kind: &str, mut fields: Vec<(&str, Json)>) -> Result<Json> {
    fields.push(("kind", Json::string(kind)?));
    fields.push(("turn", Json::string(&turn.0)?));
    Json::object_with_limits(fields, crate::session::journal_v1::MAX_RECORD)
}
impl Owner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: State,
        tools: Arc<Tools>,
        plan_tools: Arc<Tools>,
        limits: Limits,
        options: Options,
        incarnation: RuntimeIncarnation,
        closing: Arc<Closing>,
        outbox: Arc<Outbox>,
        gate: Arc<Gate>,
        mail: Mailboxes,
        done: mpsc::Receiver<Completion>,
        starts: mpsc::Receiver<(Identity, Started)>,
        previews: watch::Receiver<Option<Arc<PreviewUpdate>>>,
    ) -> Self {
        Self {
            state: Arc::new(state),
            tools,
            plan_tools,
            limits,
            options,
            incarnation,
            closing,
            outbox,
            gate,
            mail,
            done,
            starts,
            previews,
            initialized: false,
            starting: true,
            recovery_attempted: false,
            phase: "loading".into(),
            diagnostic: String::new(),
            storage_failed: false,
            recovered_pause: false,
            recovery: None,
            journal_identity: None,
            receipts: BTreeMap::new(),
            queued: VecDeque::new(),
            new_followups: VecDeque::new(),
            control_replies: BTreeMap::new(),
            control_commands: BTreeMap::new(),
            selections: VecDeque::new(),
            selecting: None,
            selection_stop: None,
            deferred_completions: VecDeque::new(),
            pending_receipt: None,
            cancelling: None,
            resuming: None,
            plan_admission_pending: false,
            plan_workspace: None,
            generation: 0,
            resume_epoch: 0,
            preview_sequence: 0,
            preview: None,
            activity: vec![],
            turn_stop: Cancel::default(),
            storing: None,
            provider: None,
            tool: None,
            auth: None,
            release_sent: false,
            lease_stop: None,
            plan: None,
            permit: None,
            owned_intent: false,
            pending_outcome: None,
            pending_pause: None,
            shutting_down: false,
        }
    }
    fn turn(&self) -> TurnId {
        self.pending_receipt
            .as_ref()
            .map(|r| r.turn.clone())
            .or_else(|| self.state.turn.as_ref().map(|t| t.receipt.turn.clone()))
            .unwrap_or_else(|| "idle".into())
    }
    fn request(&self) -> Option<RequestId> {
        self.pending_receipt
            .as_ref()
            .map(|r| r.request.clone())
            .or_else(|| self.state.turn.as_ref().map(|t| t.receipt.request.clone()))
    }
    fn identity(&mut self) -> Result<Identity> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or("runtime_generation_limit")?;
        Ok(Identity {
            incarnation: self.incarnation.clone(),
            turn: self.turn(),
            request: self.request().unwrap_or_else(|| "control".into()),
            resume_epoch: self.resume_epoch,
            generation: self.generation,
        })
    }
    fn emit(&self, payload: EventPayload) {
        self.emit_at(payload, self.turn(), self.request());
    }
    fn emit_at(&self, payload: EventPayload, turn: TurnId, request: Option<RequestId>) {
        self.outbox.publish(RuntimeEvent {
            schema_version: 1,
            stream_sequence: 0,
            session: self.state.session.clone(),
            turn,
            incarnation: self.incarnation.clone(),
            request,
            payload,
        });
    }
    fn busy(&self) -> bool {
        self.storing.is_some()
            || self.provider.is_some()
            || self.tool.is_some()
            || self.auth.is_some()
            || self.plan_admission_pending
    }
    fn can_cancel(&self) -> bool {
        self.request().is_some()
            && !matches!(self.phase.as_str(), "completed" | "cancelled" | "finishing")
    }
    fn can_act(&self) -> bool {
        !self.shutting_down
            && self.closing.mode.load(Ordering::Acquire) != 2
            && self.cancelling.is_none()
            && !self.turn_stop.cancelled()
            && !self.storage_failed
            && !self.recovered_pause
            && self.recovery.is_none()
            && self.state.turn.as_ref().is_some_and(|t| !t.paused)
            && !matches!(self.phase.as_str(), "cancelled" | "paused")
    }
    fn available(&self) -> bool {
        !self.busy()
            && self.pending_outcome.is_none()
            && !self.recovered_pause
            && self
                .state
                .turn
                .as_ref()
                .is_none_or(|t| t.stage == "completed" || t.stage == "cancelled")
    }
    fn snapshot(&self, command: Option<CommandId>) {
        let text = if self.phase == "completed" && !self.storage_failed {
            self.state
                .turn
                .as_ref()
                .and_then(|t| t.response.as_ref())
                .and_then(|r| response_text(r).ok())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let current = self
            .pending_receipt
            .as_ref()
            .or_else(|| self.state.turn.as_ref().map(|t| &t.receipt));
        self.emit(EventPayload::FrontendSnapshot(Box::new(Snapshot {
            timeline_size: self.state.timeline.len(),
            generation: self.state.generation.clone(),
            followups: self
                .state
                .followups
                .iter()
                .rev()
                .take(32)
                .map(|message| message.as_ref().clone())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            through_sequence: 0,
            state: self.phase.clone(),
            request: self.request(),
            worker_busy: self.busy(),
            completed_tools: self
                .activity
                .iter()
                .filter(|v| matches!(v,Activity::Finished(f) if f.effects==Effects::KnownResult))
                .count(),
            text,
            diagnostic: self.diagnostic.clone(),
            recovery_operation: self.recovery.clone(),
            submit: current.map(|r| r.command.clone()),
            command,
            tools: self.activity.clone(),
            history_items: self.state.history.len(),
            compactions: self.state.compactions,
            storage_failed: self.storage_failed,
            durable: self.storing.is_none() && !self.storage_failed,
            resume_available: self.phase == "paused"
                && self.recovery.is_none()
                && !self.storage_failed
                && self.state.turn.is_some(),
            mode: current.map_or(AgentMode::Basic, |r| r.mode),
            transcript_size: self.state.transcript().len(),
            preview: self.preview.clone(),
        })));
    }
    fn message_receipt(&self, command: &CommandId) -> Option<MessageReceiptInfo> {
        if let Some(receipt) = self.receipts.get(&command.0) {
            if receipt.internal {
                return None;
            }
            let state = if self.request().as_ref() == Some(&receipt.request) {
                self.phase.clone()
            } else {
                receipt.state.clone()
            };
            let admission_durable = self.options.directory.is_some()
                && !self.storage_failed
                && self.state.submits.contains_key(&command.0);
            return Some(MessageReceiptInfo {
                kind: MessageKind::Submit,
                turn: receipt.turn.clone(),
                request: (!receipt.request.0.is_empty()).then(|| receipt.request.clone()),
                state_durable: admission_durable
                    && self
                        .state
                        .submits
                        .get(&command.0)
                        .is_some_and(|r| r.state == state),
                state,
                mode: Some(receipt.mode),
                text_sha256: crate::protocol::legacy_json_v1::sha256(receipt.text.as_bytes()),
                admission_durable,
            });
        }
        if let Some(message) = self.state.followups.iter().find(|m| m.command == *command) {
            let source = self
                .state
                .submits
                .values()
                .find(|r| r.request == message.target);
            return Some(MessageReceiptInfo {
                kind: MessageKind::Steer,
                turn: source
                    .map(|r| r.turn.clone())
                    .unwrap_or_else(|| self.turn()),
                request: Some(message.target.clone()),
                state: message.state.clone(),
                mode: source.map(|r| r.mode),
                text_sha256: crate::protocol::legacy_json_v1::sha256(message.text.as_bytes()),
                admission_durable: self.options.directory.is_some() && !self.storage_failed,
                state_durable: self.options.directory.is_some() && !self.storage_failed,
            });
        }
        if let Some(pending) = self.control_commands.get(&command.0)
            && let CommandPayload::Steer(message) = &pending.payload
        {
            return Some(MessageReceiptInfo {
                kind: MessageKind::Steer,
                turn: pending.turn.clone(),
                request: Some(message.target.clone()),
                state: "queued".into(),
                mode: self
                    .state
                    .submits
                    .values()
                    .find(|r| r.request == message.target)
                    .map(|r| r.mode),
                admission_durable: false,
                text_sha256: crate::protocol::legacy_json_v1::sha256(message.text.as_bytes()),
                state_durable: false,
            });
        }
        None
    }

    fn child_attempt(&self, lookup: &ChildLookup) -> Result<ChildAttempt> {
        if !self.initialized || self.starting {
            return Err("runtime_not_ready");
        }
        if self.storage_failed {
            return Err("child_journal_read_only");
        }
        let journal = self.journal_identity.ok_or("child_requires_journal")?;
        let state = match self.state.submits.get(&lookup.command.0) {
            Some(receipt) => {
                Self::match_child_submit(receipt, lookup)?;
                let turn = self
                    .state
                    .turn
                    .as_ref()
                    .filter(|turn| {
                        turn.receipt.command == lookup.command
                            && turn.receipt.request == receipt.request
                    })
                    .ok_or("child_attempt_not_current")?;
                let quiescent = !self.busy()
                    && self.pending_outcome.is_none()
                    && self.cancelling.is_none()
                    && self.resuming.is_none()
                    && self.plan_workspace.is_none()
                    && !self.owned_intent;
                let final_response = if receipt.state == "completed" {
                    if turn.stage != "completed" {
                        return Err("child_completion_not_current");
                    }
                    Some(turn.response.clone().ok_or("child_completion_missing")?)
                } else {
                    None
                };
                ChildAttemptState::Durable {
                    request: receipt.request.clone(),
                    state: receipt.state.clone(),
                    quiescent,
                    resume_required: self.recovered_pause || turn.paused,
                    recovery_required: self.recovery.is_some()
                        || quiescent && self.state.unknown_operation().is_some(),
                    final_response,
                }
            }
            None => match self.receipts.get(&lookup.command.0) {
                Some(receipt) => {
                    Self::match_child_submit(receipt, lookup)?;
                    ChildAttemptState::Pending
                }
                None if self
                    .state
                    .generation_commands
                    .contains_key(&lookup.command.0)
                    || self
                        .state
                        .followups
                        .iter()
                        .any(|item| item.command == lookup.command) =>
                {
                    return Err("child_submit_conflict");
                }
                None => ChildAttemptState::Missing,
            },
        };
        Ok(ChildAttempt {
            session: self.state.session.clone(),
            incarnation: self.incarnation.clone(),
            journal,
            command: lookup.command.clone(),
            turn: lookup.turn.clone(),
            mode: lookup.submit.mode,
            text_sha256: crate::protocol::legacy_json_v1::sha256(lookup.submit.text.as_bytes()),
            latest_tool: self
                .activity
                .iter()
                .rev()
                .find_map(|activity| match activity {
                    Activity::Started(tool) => {
                        let target = self
                            .state
                            .turn
                            .as_ref()
                            .and_then(|turn| turn.intent.as_ref())
                            .filter(|intent| intent.operation == tool.operation.0)
                            .and_then(|intent| intent.arguments.text_field("path").ok())
                            .map(|path| path.chars().take(120).collect());
                        Some((tool.name.clone(), target))
                    }
                    Activity::Finished(_) => None,
                }),
            state,
        })
    }

    fn match_child_submit(receipt: &SubmitReceipt, lookup: &ChildLookup) -> Result<()> {
        if receipt.internal
            || receipt.turn != lookup.turn
            || receipt.mode != lookup.submit.mode
            || *receipt.text != lookup.submit.text
        {
            return Err("child_submit_conflict");
        }
        Ok(())
    }
    fn finish_preview(&mut self, state: &str, final_text: bool) {
        if let Some(preview) = &mut self.preview
            && preview.state == "preview"
        {
            if final_text
                && let Some(response) = self.state.turn.as_ref().and_then(|t| t.response.as_ref())
            {
                preview.text = response_text(response).unwrap_or_default();
            }
            preview.state = state.into();
            let value = preview.clone();
            self.emit(EventPayload::Preview(value));
        }
    }
    fn show_outcome(&mut self, result: &SavedResult) {
        if !result.dispatched {
            return;
        }
        let finished = Finished {
            operation: OperationId(result.result.operation_id().into()),
            call_id: result.result.call_id().into(),
            execution: result.result.execution(),
            effects: result.result.effects(),
            output: Arc::new(result.result.output().into()),
        };
        if let Some(index) = self
            .activity
            .iter()
            .position(|a| a.operation() == &finished.operation)
        {
            self.activity[index] = Activity::Finished(finished.clone());
        } else {
            self.activity.push(Activity::Finished(finished.clone()));
        }
        self.emit(EventPayload::ToolFinished(finished.clone()));
        if finished.effects == Effects::Unknown {
            self.recovery = Some(finished.operation.clone());
            self.emit(EventPayload::RecoveryRequired {
                operation: finished.operation,
                effects: Effects::Unknown,
            });
        }
    }
    fn reject(&self, command: &RuntimeCommand, code: &str) {
        self.emit_at(
            EventPayload::RuntimeError {
                command: Some(command.command.clone()),
                diagnostic: code.into(),
            },
            command.turn.clone(),
            None,
        );
    }

    fn reject_plan(&mut self, command: &RuntimeCommand, code: &str) {
        // This Submit was queued but never reached the durable admission.
        self.receipts.remove(&command.command.0);
        self.reject(command, code);
    }
    fn fail(&mut self, code: &'static str) {
        if let Some(stop) = &self.selection_stop {
            stop.cancel();
        }
        for (_, reply) in std::mem::take(&mut self.control_replies) {
            let _ = reply.send(Err(code));
        }
        self.control_commands.clear();
        // A provider/auth completion can be parked behind the failing journal
        // write. It has already returned, so do not retain a fictitious busy
        // worker that would prevent bounded shutdown.
        while let Some(done) = self.deferred_completions.pop_front() {
            match done {
                Completion::Provider { identity, .. }
                    if self.provider.as_ref() == Some(&identity) =>
                {
                    self.provider = None
                }
                Completion::Auth { identity, task, .. }
                    if self
                        .auth
                        .as_ref()
                        .is_some_and(|(active, kind, _)| *active == identity && *kind == task) =>
                {
                    self.auth = None;
                    self.selecting = None;
                }
                _ => {}
            }
        }
        self.storage_failed = true;
        self.diagnostic = if crate::provider::contracts::valid_id(code) {
            code.into()
        } else {
            "runtime_failure".into()
        };
        self.gate.cancel();
        self.turn_stop.cancel();
        self.tools.signal_cancel();
        self.finish_preview("discarded", false);
        if self.phase != "cancelled" {
            self.phase = "paused".into();
        }
        if let Some(id) = self.cancelling.take() {
            self.phase = "cancelled".into();
            self.emit(EventPayload::Cancelled { command: id });
        }
        self.emit(EventPayload::RuntimeError {
            command: self
                .pending_receipt
                .as_ref()
                .map(|r| r.command.clone())
                .or_else(|| self.state.turn.as_ref().map(|t| t.receipt.command.clone())),
            diagnostic: self.diagnostic.clone(),
        });
        self.snapshot(None);
    }
    fn persist(
        &mut self,
        workers: &Workers,
        kind: Kind,
        build: Build,
        known: Option<SavedResult>,
    ) -> Result<()> {
        if self.storing.is_some() {
            return Err("concurrent_journal_transition");
        }
        let identity = self.identity()?;
        workers
            .store
            .as_ref()
            .ok_or("journal_worker_closed")?
            .try_send(StoreJob {
                identity: identity.clone(),
                kind,
                build,
            })
            .map_err(|_| "journal_queue_full")?;
        self.storing = Some(PendingStore {
            identity,
            kind,
            known,
        });
        Ok(())
    }
    fn record(
        &mut self,
        workers: &Workers,
        kind: Kind,
        name: &'static str,
        fields: Vec<(&'static str, Json)>,
    ) -> Result<()> {
        let turn = self.turn();
        self.persist(
            workers,
            kind,
            Box::new(move |_| transition(&turn, name, fields)),
            None,
        )
    }
    fn pause(&mut self, code: &'static str) {
        self.finish_preview("discarded", false);
        self.pending_pause = Some(if crate::provider::contracts::valid_id(code) {
            code
        } else {
            "worker_failure"
        });
    }
    fn release_auth(&mut self, workers: &Workers) -> Result<()> {
        if let Some((identity, _, holding)) = &self.auth
            && *holding
            && !self.release_sent
        {
            workers
                .release
                .as_ref()
                .ok_or("auth_worker_closed")?
                .try_send(identity.clone())
                .map_err(|_| "auth_release_queue")?;
            self.release_sent = true;
        }
        Ok(())
    }
    fn authorize(&mut self, workers: &Workers, task: AuthTask) -> Result<()> {
        let identity = self.identity()?;
        self.gate.allow(&identity)?;
        let request = if matches!(task, AuthTask::Generate | AuthTask::Compact) {
            let current = self.state.turn.as_ref().ok_or("runtime_turn")?;
            Some(if task == AuthTask::Generate {
                if let Some(response) = &current.tools_response {
                    match crate::session::codec_v1::next_request(response, &current.results) {
                        Ok(request) => request,
                        Err("json_build_limit" | "json_structure_limit" | "request_context") => {
                            self.pause("request_frame_limit");
                            return self.advance(workers);
                        }
                        Err(code) => return Err(code),
                    }
                } else {
                    current.next.clone()
                }
            } else {
                current.next.clone()
            })
        } else {
            None
        };
        workers
            .auth
            .as_ref()
            .ok_or("auth_worker_closed")?
            .try_send(AuthJob {
                selection: None,
                request,
                identity: identity.clone(),
                stop: self.turn_stop.clone(),
                task,
            })
            .map_err(|_| "auth_queue_full")?;
        self.auth = Some((identity, task, false));
        self.snapshot(None);
        Ok(())
    }
    fn provider(&mut self, workers: &Workers, task: ProviderTask) -> Result<()> {
        let identity = self.identity()?;
        self.gate.allow(&identity)?;
        let preview_generation = if self.options.streaming && matches!(task, ProviderTask::Generate)
        {
            self.preview_sequence += 1;
            self.preview = Some(Preview {
                generation: self.preview_sequence,
                text: String::new(),
                state: "preview".into(),
            });
            self.preview_sequence
        } else {
            0
        };
        workers
            .provider
            .as_ref()
            .ok_or("provider_worker_closed")?
            .try_send(ProviderJob {
                identity: identity.clone(),
                state: self.state.clone(),
                stop: self.turn_stop.clone(),
                task,
                preview_generation,
            })
            .map_err(|_| "provider_queue_full")?;
        self.provider = Some(identity);
        self.snapshot(None);
        Ok(())
    }
    fn prepare(&mut self, workers: &Workers) -> Result<()> {
        let current = self.state.turn.as_ref().ok_or("runtime_turn")?;
        let selected = if current.next.tools.revision == self.plan_tools.advertisement().revision {
            self.plan_tools.clone()
        } else {
            self.tools.clone()
        };
        let identity = self.identity()?;
        workers
            .tool
            .as_ref()
            .ok_or("tool_worker_closed")?
            .try_send(ToolJob::Prepare {
                identity: identity.clone(),
                state: self.state.clone(),
                tools: selected.clone(),
            })
            .map_err(|_| "tool_queue_full")?;
        self.tool = Some((identity, ToolTask::Prepare));
        self.plan = Some((vec![], selected));
        self.phase = "executing".into();
        self.snapshot(None);
        Ok(())
    }
    fn execute(&mut self, workers: &Workers, lease_stop: Cancel) -> Result<()> {
        let tools = self
            .plan
            .as_ref()
            .ok_or("missing_prepared_batch")?
            .1
            .clone();
        let identity = self.identity()?;
        self.gate.allow(&identity)?;
        let permit = self.permit.take().ok_or("missing_durable_intent")?;
        workers
            .tool
            .as_ref()
            .ok_or("tool_worker_closed")?
            .try_send(ToolJob::Execute {
                identity: identity.clone(),
                permit,
                tools,
                stop: self.turn_stop.clone(),
                lease_stop: lease_stop.clone(),
            })
            .map_err(|_| "tool_queue_full")?;
        self.tool = Some((identity, ToolTask::Execute));
        self.lease_stop = Some(lease_stop);
        self.snapshot(None);
        Ok(())
    }
    fn cancel(&mut self, command: CommandId) {
        self.gate.cancel();
        self.drain_started();
        self.turn_stop.cancel();
        self.tools.signal_cancel();
        self.finish_preview("discarded", false);
        self.cancelling = Some(command);
        self.phase = "cancelled".into();
        self.pending_pause = None;
        if self.state.pending_followups().next().is_some() && self.recovery.is_none() {
            self.recovered_pause = false;
        }
        if self.storage_failed {
            let command = self.cancelling.take().expect("owned cancel");
            self.emit(EventPayload::Cancelled { command });
            self.snapshot(None);
        }
    }
    fn admit(&mut self, envelope: Envelope) -> Result<()> {
        let command = envelope.command;
        let status = self.admit_inner(&command);
        if matches!(status, Ok(PostStatus::Accepted))
            && matches!(
                command.payload,
                CommandPayload::Steer(_) | CommandPayload::SetGeneration(_)
            )
        {
            // Admission is acknowledged only after the durable queued/selection
            // record. Cancel and snapshot use independent mailboxes meanwhile.
            self.control_commands
                .insert(command.command.0.clone(), command.clone());
            self.control_replies
                .insert(command.command.0, envelope.reply);
        } else {
            let _ = envelope.reply.send(status);
        }
        Ok(())
    }
    fn admit_inner(&mut self, command: &RuntimeCommand) -> Result<PostStatus> {
        if self.closing.mode.load(Ordering::Acquire) != 0 {
            return Ok(PostStatus::Closed);
        }
        if self.options.inspect_only
            && !matches!(
                command.payload,
                CommandPayload::GetSnapshot(_)
                    | CommandPayload::GetTranscript(_)
                    | CommandPayload::GetTimeline(_)
                    | CommandPayload::GetMessageReceipt(_)
            )
        {
            return Err("archive_only");
        }
        if let Some(pending) = self.control_commands.get(&command.command.0) {
            return Ok(
                if pending.payload == command.payload && pending.turn == command.turn {
                    PostStatus::QueueFull
                } else {
                    PostStatus::CommandConflict
                },
            );
        }
        match &command.payload {
            CommandPayload::GetMessageReceipt(target) => {
                self.emit(EventPayload::MessageReceipt {
                    query: command.command.clone(),
                    command: target.clone(),
                    receipt: self.message_receipt(target).map(Box::new),
                    ready: self.initialized && !self.starting && self.phase != "loading",
                    storage_failed: self.storage_failed,
                    recovery_required: self.recovery.is_some(),
                });
            }
            CommandPayload::Steer(message) => {
                if let Some(previous) = self
                    .state
                    .followups
                    .iter()
                    .find(|m| m.command == command.command)
                {
                    let turn_matches = self
                        .state
                        .submits
                        .values()
                        .any(|r| r.request == previous.target && r.turn == command.turn);
                    return Ok(
                        if previous.target == message.target
                            && previous.text == message.text
                            && turn_matches
                        {
                            PostStatus::Duplicate
                        } else {
                            PostStatus::CommandConflict
                        },
                    );
                }
                if self
                    .new_followups
                    .iter()
                    .any(|c| c.command == command.command)
                    || self.control_replies.contains_key(&command.command.0)
                {
                    return Ok(PostStatus::QueueFull);
                }
                if self.receipts.contains_key(&command.command.0)
                    || self
                        .state
                        .generation_commands
                        .contains_key(&command.command.0)
                {
                    return Ok(PostStatus::CommandConflict);
                }
                if !self.state.turn.as_ref().is_some_and(|turn| {
                    turn.receipt.request == message.target && turn.receipt.turn == command.turn
                }) || self.pending_receipt.is_some()
                {
                    return Ok(PostStatus::StaleTarget);
                }
                if self.storage_failed {
                    return Err("journal_read_only");
                }
                // An admitted command remains reserved while it is waiting,
                // selected by advance(), or in the journal worker. Popping it
                // from the deque does not release its durable queue slot.
                let reserved = self
                    .control_commands
                    .values()
                    .filter(|c| {
                        matches!(c.payload, CommandPayload::Steer(_))
                            && !self.state.followups.iter().any(|m| m.command == c.command)
                    })
                    .count();
                if self.state.followups.len() + reserved >= 4096 {
                    return Ok(PostStatus::CommandIdLimit);
                }
                if self.state.pending_followups().count() + reserved >= self.limits.commands.min(16)
                {
                    return Ok(PostStatus::QueueFull);
                }
                self.new_followups.push_back(command.clone());
            }
            CommandPayload::SetGeneration(selection) => {
                if let Some(previous) = self.state.generation_commands.get(&command.command.0) {
                    return Ok(if previous == selection {
                        PostStatus::Duplicate
                    } else {
                        PostStatus::CommandConflict
                    });
                }
                if self.control_replies.contains_key(&command.command.0) {
                    return Ok(PostStatus::QueueFull);
                }
                if self.receipts.contains_key(&command.command.0)
                    || self
                        .state
                        .followups
                        .iter()
                        .any(|m| m.command == command.command)
                {
                    return Ok(PostStatus::CommandConflict);
                }
                if self.storage_failed {
                    return Err("journal_read_only");
                }
                let reserved = self
                    .control_commands
                    .values()
                    .filter(|c| {
                        matches!(c.payload, CommandPayload::SetGeneration(_))
                            && !self.state.generation_commands.contains_key(&c.command.0)
                    })
                    .count();
                if self.state.generation_commands.len() + reserved >= 4096 {
                    return Ok(PostStatus::CommandIdLimit);
                }
                if reserved >= 4 {
                    return Ok(PostStatus::QueueFull);
                }
                self.selections.push_back(command.clone());
            }
            CommandPayload::GetContext => {
                self.emit(EventPayload::ContextSnapshot {
                    command: command.command.clone(),
                    seed: self.state.fork_seed()?,
                });
            }
            CommandPayload::GetTimeline(query) => {
                let page = self.state.timeline.page(query.clone());
                self.emit(EventPayload::TimelinePage(TranscriptPage {
                    begin: page.begin,
                    end: page.end,
                    total: page.total,
                    entries: page.entries,
                    query: page.query,
                }));
            }
            CommandPayload::Submit(submitted) | CommandPayload::Handoff(submitted) => {
                let internal = matches!(command.payload, CommandPayload::Handoff(_));
                if self
                    .state
                    .generation_commands
                    .contains_key(&command.command.0)
                    || self
                        .state
                        .followups
                        .iter()
                        .any(|m| m.command == command.command)
                    || self.control_replies.contains_key(&command.command.0)
                {
                    return Ok(PostStatus::CommandConflict);
                }
                if let Some(receipt) = self.receipts.get(&command.command.0) {
                    if *receipt.text != submitted.text
                        || receipt.turn != command.turn
                        || receipt.mode != submitted.mode
                        || receipt.internal != internal
                    {
                        return Ok(PostStatus::CommandConflict);
                    }
                    if internal
                        && (!self.state.submits.contains_key(&command.command.0)
                            || self.storage_failed)
                    {
                        return Ok(PostStatus::QueueFull);
                    }
                    let state = if self.request().as_ref() == Some(&receipt.request) {
                        self.phase.clone()
                    } else {
                        receipt.state.clone()
                    };
                    self.emit_at(
                        EventPayload::CommandReplayed {
                            command: command.command.clone(),
                            state,
                        },
                        receipt.turn.clone(),
                        if receipt.request.0.is_empty() {
                            None
                        } else {
                            Some(receipt.request.clone())
                        },
                    );
                    return Ok(PostStatus::Duplicate);
                }
                if self.receipts.len() >= self.limits.submit_ids {
                    return Ok(PostStatus::CommandIdLimit);
                }
                if self.queued.len() == self.limits.commands {
                    return Ok(PostStatus::QueueFull);
                }
                self.receipts.insert(
                    command.command.0.clone(),
                    SubmitReceipt {
                        command: command.command.clone(),
                        turn: command.turn.clone(),
                        request: RequestId(String::new()),
                        text: Arc::new(submitted.text.clone()),
                        state: "queued".into(),
                        mode: submitted.mode,
                        internal,
                    },
                );
                self.queued.push_back(command.clone());
            }
            CommandPayload::Cancel(target) => {
                if !self.can_cancel()
                    || self.request().as_ref() != Some(target)
                    || self.turn() != command.turn
                {
                    return Ok(PostStatus::StaleTarget);
                }
                if self.cancelling.is_some() {
                    return Ok(PostStatus::Duplicate);
                }
                self.cancel(command.command.clone());
            }
            CommandPayload::GetSnapshot(after) => {
                if *after > self.outbox.sequence() {
                    self.reject(command, "resync_cursor_ahead");
                } else {
                    self.snapshot(Some(command.command.clone()));
                }
            }
            CommandPayload::GetTranscript(query) => {
                let page = self.state.transcript().page(query.clone());
                self.emit(EventPayload::TranscriptPage(TranscriptPage {
                    begin: page.begin,
                    end: page.end,
                    total: page.total,
                    entries: page.entries,
                    query: page.query,
                }));
            }
            CommandPayload::Resume(target) => {
                if self.phase != "paused"
                    || self.request().as_ref() != Some(target)
                    || self.turn() != command.turn
                {
                    return Ok(PostStatus::StaleTarget);
                }
                if self.resuming.is_some() || self.plan_admission_pending {
                    return Ok(PostStatus::QueueFull);
                }
                if self.storage_failed || self.recovery.is_some() {
                    self.reject(
                        command,
                        if self.storage_failed {
                            "journal_read_only"
                        } else {
                            "recovery_required"
                        },
                    );
                } else if self
                    .state
                    .turn
                    .as_ref()
                    .is_some_and(|t| t.resumes.contains(&command.command.0))
                {
                    self.emit(EventPayload::CommandReplayed {
                        command: command.command.clone(),
                        state: self.phase.clone(),
                    });
                } else if self
                    .state
                    .turn
                    .as_ref()
                    .is_some_and(|t| t.resumes.len() >= 4096)
                {
                    self.reject(command, "resume_id_limit");
                } else {
                    self.resuming = Some(command.command.clone());
                }
            }
        }
        Ok(PostStatus::Accepted)
    }
    fn submit(&mut self, workers: &Workers, command: RuntimeCommand) -> Result<()> {
        if self.recovery.is_some() || self.storage_failed {
            self.reject(
                &command,
                if self.recovery.is_some() {
                    "recovery_required"
                } else {
                    "journal_read_only"
                },
            );
            return Ok(());
        }
        if matches!(&command.payload, CommandPayload::Submit(submitted) | CommandPayload::Handoff(submitted) if submitted.mode == AgentMode::Plan)
            && !self.tools.plan_ready()
        {
            self.reject_plan(&command, "terminal_workspace_busy");
            return Ok(());
        }
        let (submitted, internal) = match command.payload {
            CommandPayload::Submit(submitted) => (submitted, false),
            CommandPayload::Handoff(submitted) => (submitted, true),
            _ => return Err("submit_contract"),
        };
        self.tools.begin_turn();
        let request_number = self.state.next_request + 1;
        let request = RequestId(format!("request_{request_number}"));
        let run = format!("{}_{}", self.incarnation.0, request.0);
        let receipt = self
            .receipts
            .get_mut(&command.command.0)
            .ok_or("submit_receipt")?;
        receipt.request = request.clone();
        self.pending_receipt = Some(receipt.clone());
        let receipt = receipt.clone();
        self.activity.clear();
        self.preview = None;
        self.diagnostic.clear();
        self.recovered_pause = false;
        self.recovery = None;
        self.turn_stop = Cancel::default();
        self.gate.reset();
        self.phase = "persisting".into();
        self.plan = None;
        self.permit = None;
        self.owned_intent = false;
        self.pending_pause = None;
        let limits = self.limits;
        let keep = self.options.keep_recent_turns;
        let model_window = self.options.model_window.clone();
        self.persist(
            workers,
            Kind::Accepted,
            Box::new(move |state| {
                let next = state.request_for_input(&submitted.text, submitted.mode, internal)?;
                let inherited_incomplete_turn = next.input.len() != state.history.len() + 1;
                let compact = !inherited_incomplete_turn
                    && state.completed_context_turns() > keep
                    && state.compaction_prefix().is_some()
                    && state
                        .context_estimate(&next, || model_window(&next))
                        .is_some_and(|(tokens, window)| tokens >= window.div_ceil(2));
                let mut fields = vec![
                    ("kind", Json::string("accepted")?),
                    ("command", Json::string(&receipt.command.0)?),
                    ("turn", Json::string(&receipt.turn.0)?),
                    ("request", Json::string(&request.0)?),
                    ("request_number", number(request_number)?),
                    ("text", Json::string(&receipt.text)?),
                    (
                        "mode",
                        Json::string(if receipt.mode == AgentMode::Plan {
                            "plan"
                        } else {
                            "basic"
                        })?,
                    ),
                    ("operation_run", Json::string(&run)?),
                    ("next", next.save()?),
                    (
                        "compact",
                        Json::parse(if compact { "true" } else { "false" })?,
                    ),
                    ("round_limit", number(limits.tool_rounds)?),
                    ("call_limit", number(limits.tool_calls)?),
                ];
                if internal {
                    fields.push(("handoff", Json::parse("true")?));
                }
                Json::object_with_limits(fields, crate::session::journal_v1::MAX_RECORD)
            }),
            None,
        )?;
        self.snapshot(None);
        Ok(())
    }
    fn advance(&mut self, workers: &Workers) -> Result<()> {
        if !self.initialized || self.storing.is_some() {
            return Ok(());
        }
        if self.storage_failed {
            if let Some(value) = self.pending_outcome.take() {
                self.show_outcome(&value);
            }
            if self.tool.is_none() {
                self.release_auth(workers)?;
            }
            return Ok(());
        }
        if self.cancelling.is_some() && self.state.turn.is_some() {
            return self.record(workers, Kind::Cancel, "cancel", vec![]);
        }
        if let Some(outcome) = self.pending_outcome.take() {
            let known = outcome.clone();
            let turn = self.turn();
            return self.persist(
                workers,
                Kind::Outcome,
                Box::new(move |_| transition(&turn, "outcome", vec![("result", outcome.save()?)])),
                Some(known),
            );
        }
        if self.phase == "cancelled"
            && self.owned_intent
            && self.tool.is_none()
            && let Some(intent) = self.state.turn.as_ref().and_then(|t| t.intent.as_ref())
        {
            self.pending_outcome = Some(not_started(intent));
            self.permit = None;
            return self.advance(workers);
        }
        if self.tool.is_none() && self.phase == "cancelled" {
            self.release_auth(workers)?;
        }
        if let Some(done) = self.deferred_completions.pop_front() {
            self.completed(workers, done)?;
            // A consumed completion can remove the last worker without
            // producing another wake (e.g. late provider success after Cancel).
            // Continue the bounded owner transition instead of parking B until
            // an unrelated frontend command happens to arrive.
            return self.advance(workers);
        }
        if !self.starting
            && let Some(command) = self.new_followups.pop_front()
        {
            let CommandPayload::Steer(message) = command.payload else {
                return Err("steer_contract");
            };
            return self.record(
                workers,
                Kind::Queued,
                "queued",
                vec![
                    ("command", Json::string(&command.command.0)?),
                    ("target", Json::string(&message.target.0)?),
                    ("text", Json::string(&message.text)?),
                ],
            );
        }
        if self.starting {
            if !self.recovery_attempted {
                self.recovery_attempted = true;
                if !self.options.inspect_only
                    && let Some(invocation) =
                        self.state.turn.as_ref().and_then(|t| t.intent.clone())
                    && self
                        .tools
                        .policy(&invocation.name)
                        .is_some_and(|p| p.recover.is_some())
                {
                    let identity = self.identity()?;
                    workers
                        .tool
                        .as_ref()
                        .ok_or("tool_worker_closed")?
                        .try_send(ToolJob::Recover {
                            identity: identity.clone(),
                            invocation,
                            tools: self.tools.clone(),
                        })
                        .map_err(|_| "tool_queue_full")?;
                    self.tool = Some((identity, ToolTask::Recover));
                }
            }
            return Ok(());
        }
        if !self.shutting_down
            && self.closing.mode.load(Ordering::Acquire) != 2
            && self.auth.is_none()
            && let Some(command) = self.selections.pop_front()
        {
            let CommandPayload::SetGeneration(selection) = &command.payload else {
                return Err("generation_selection");
            };
            let identity = self.identity()?;
            let stop = Cancel::default();
            workers
                .auth
                .as_ref()
                .ok_or("auth_worker_closed")?
                .try_send(AuthJob {
                    selection: Some(selection.clone()),
                    request: None,
                    identity: identity.clone(),
                    stop: stop.clone(),
                    task: AuthTask::SelectGeneration,
                })
                .map_err(|_| "auth_queue_full")?;
            self.selection_stop = Some(stop);
            self.auth = Some((identity, AuthTask::SelectGeneration, false));
            self.selecting = Some(command);
            return Ok(());
        }
        if self.busy() {
            return Ok(());
        }
        if self.state.pending_followups().next().is_some()
            && self.owned_intent
            && let Some(intent) = self.state.turn.as_ref().and_then(|t| t.intent.as_ref())
        {
            self.pending_outcome = Some(not_started(intent));
            self.permit = None;
            return self.advance(workers);
        }
        if let Some(command) = self.resuming.take() {
            if self.phase != "paused" || self.recovery.is_some() {
                return Ok(());
            }
            let plan_mode = self
                .state
                .turn
                .as_ref()
                .is_some_and(|turn| turn.receipt.mode == AgentMode::Plan);
            if plan_mode && !self.tools.plan_ready() {
                self.emit(EventPayload::RuntimeError {
                    command: Some(command),
                    diagnostic: "terminal_workspace_busy".into(),
                });
                return Ok(());
            }
            if plan_mode
                && self.plan_workspace.is_none()
                && let Some(plan) = &workers.plan
            {
                plan.try_send(PlanTarget::Resume(command))
                    .map_err(|_| "plan_worker_queue")?;
                self.plan_admission_pending = true;
                return Ok(());
            }
            self.resuming = Some(command.clone());
            return self.record(
                workers,
                Kind::Resume,
                "resume",
                vec![("command", Json::string(&command.0)?)],
            );
        }
        if let Some(code) = self.pending_pause.take()
            && self.phase != "cancelled"
        {
            return self.record(
                workers,
                Kind::Pause,
                "pause",
                vec![("diagnostic", Json::string(code)?)],
            );
        }
        if !self.shutting_down
            && !self.recovered_pause
            && self.recovery.is_none()
            && self.state.pending_followups().next().is_some()
        {
            let messages = self.state.pending_followups().cloned().collect::<Vec<_>>();
            let turn = self.turn();
            self.finish_preview("discarded", false);
            return self.persist(
                workers,
                Kind::Steered,
                Box::new(move |state| {
                    let commands = Json::array(
                        messages
                            .iter()
                            .map(|m| Json::string(&m.command.0))
                            .collect::<Result<Vec<_>>>()?,
                    )?;
                    match state
                        .followup_request(&messages)
                        .and_then(|request| request.save())
                    {
                        Ok(next) => transition(
                            &turn,
                            "steered",
                            vec![("commands", commands), ("next", next)],
                        ),
                        Err("json_build_limit" | "json_structure_limit" | "request_context") => {
                            transition(&turn, "steer_rejected", vec![("commands", commands)])
                        }
                        Err(code) => Err(code),
                    }
                }),
                None,
            );
        }
        if self.available()
            && self.closing.mode.load(Ordering::Acquire) != 2
            && let Some(command) = self.queued.pop_front()
        {
            if matches!(&command.payload, CommandPayload::Submit(value) | CommandPayload::Handoff(value) if value.mode == AgentMode::Plan)
                && let Some(plan) = &workers.plan
            {
                plan.try_send(PlanTarget::Submit(command))
                    .map_err(|_| "plan_worker_queue")?;
                self.plan_admission_pending = true;
                return Ok(());
            }
            return self.submit(workers, command);
        }
        if !self.can_act() {
            return Ok(());
        }
        let current = self.state.turn.as_ref().ok_or("runtime_turn")?;
        match current.stage.as_str() {
            "generating" => {
                self.phase = "generating".into();
                self.authorize(workers, AuthTask::Generate)
            }
            "compacting" => {
                self.phase = "compacting".into();
                self.authorize(workers, AuthTask::Compact)
            }
            "tools" => {
                if current.intent.is_some() {
                    if self.permit.is_some() {
                        return self.authorize(workers, AuthTask::ToolLease);
                    }
                    return Ok(());
                }
                if current
                    .results
                    .iter()
                    .any(|r| r.result.effects() != Effects::KnownResult || !r.dispatched)
                {
                    self.pause("tool_failed");
                    return self.advance(workers);
                }
                if current
                    .tools_response
                    .as_ref()
                    .is_some_and(|r| current.results.len() == r.calls().len())
                {
                    match crate::session::codec_v1::next_request(
                        current
                            .tools_response
                            .as_ref()
                            .expect("matched tool response"),
                        &current.results,
                    ) {
                        Ok(_) => {}
                        Err("json_build_limit" | "json_structure_limit" | "request_context") => {
                            self.pause("request_frame_limit");
                            return self.advance(workers);
                        }
                        Err(code) => return Err(code),
                    }
                    self.plan = None;
                    return self.record(workers, Kind::Continued, "continued", vec![]);
                }
                if self
                    .plan
                    .as_ref()
                    .is_none_or(|(values, _)| values.is_empty())
                {
                    self.prepare(workers)
                } else {
                    self.authorize(workers, AuthTask::BeforeIntent)
                }
            }
            "final" => {
                self.phase = "finishing".into();
                self.record(workers, Kind::Done, "done", vec![])
            }
            _ => Ok(()),
        }
    }
    fn stored(&mut self, done: Stored) -> Result<()> {
        let Some(pending) = self.storing.as_ref() else {
            return Ok(());
        };
        if pending.identity != done.identity || pending.kind != done.kind {
            return Ok(());
        }
        let pending = self.storing.take().expect("matched store job");
        if done.rejected_frame {
            if done.kind != Kind::Accepted {
                if self.cancelling.is_none() {
                    self.pause("request_frame_limit");
                }
                return Ok(());
            }
            let submitted = self.pending_receipt.take().ok_or("submit_receipt")?;
            self.receipts.remove(&submitted.command.0);
            self.plan_workspace = None;
            self.phase = self
                .state
                .turn
                .as_ref()
                .map_or_else(|| "idle".into(), |turn| turn.stage.clone());
            self.emit_at(
                EventPayload::RuntimeError {
                    command: Some(submitted.command),
                    diagnostic: "request_frame_limit".into(),
                },
                submitted.turn.clone(),
                None,
            );
            if let Some(command) = self.cancelling.take() {
                self.emit_at(EventPayload::Cancelled { command }, submitted.turn, None);
            }
            self.snapshot(None);
            return Ok(());
        }
        let receipt = match done.result {
            Ok(receipt) => receipt,
            Err(code) => {
                if let Some(outcome) = done.known.or(pending.known) {
                    self.show_outcome(&outcome);
                }
                if done.kind != Kind::Accepted {
                    self.pending_receipt = None;
                }
                self.fail(code);
                return Ok(());
            }
        };
        self.state = done.state;
        if let Some(turn) = &self.state.turn {
            self.receipts
                .insert(turn.receipt.command.0.clone(), turn.receipt.clone());
        }
        self.pending_receipt = None;
        match done.kind {
            Kind::Queued => {
                let command: CommandId = receipt.event().text_field("command")?.into();
                self.control_commands.remove(&command.0);
                if let Some(reply) = self.control_replies.remove(&command.0) {
                    let _ = reply.send(Ok(PostStatus::Accepted));
                }
                self.emit(EventPayload::FollowupQueued { command });
                self.snapshot(None);
            }
            Kind::Steered => {
                if receipt.event().text_field("kind")? == "steer_rejected" {
                    self.emit(EventPayload::RuntimeError {
                        command: None,
                        diagnostic: "followup_frame_limit".into(),
                    });
                    self.snapshot(None);
                    return Ok(());
                }
                self.turn_stop = Cancel::default();
                self.gate.reset();
                self.resume_epoch += 1;
                self.plan = None;
                self.permit = None;
                self.pending_pause = None;
                self.diagnostic.clear();
                self.phase = "generating".into();
                let commands = receipt
                    .event()
                    .field("commands")?
                    .elements()?
                    .iter()
                    .map(|id| Ok(CommandId(id.text()?.into())))
                    .collect::<Result<Vec<_>>>()?;
                self.emit(EventPayload::FollowupDelivered { commands });
                self.snapshot(None);
            }
            Kind::GenerationSelected => {
                let command: CommandId = receipt.event().text_field("command")?.into();
                self.control_commands.remove(&command.0);
                if let Some(reply) = self.control_replies.remove(&command.0) {
                    let _ = reply.send(Ok(PostStatus::Accepted));
                }
                self.emit(EventPayload::GenerationSelected {
                    command,
                    selection: self.state.generation.clone(),
                });
                self.snapshot(None);
            }
            Kind::Accepted => {
                if self.phase != "cancelled" {
                    self.phase = self
                        .state
                        .turn
                        .as_ref()
                        .ok_or("runtime_turn")?
                        .stage
                        .clone();
                }
                self.emit(EventPayload::CommandAccepted {
                    command: self
                        .state
                        .turn
                        .as_ref()
                        .ok_or("runtime_turn")?
                        .receipt
                        .command
                        .clone(),
                    request: self.request(),
                });
            }
            Kind::Intent => {
                self.owned_intent = true;
                self.permit = Some(receipt.intent()?);
            }
            Kind::Outcome => {
                self.owned_intent = false;
                self.permit = None;
                if !self.starting {
                    let result = self
                        .state
                        .turn
                        .as_ref()
                        .and_then(|t| t.results.last())
                        .ok_or("runtime_outcome")?
                        .clone();
                    self.show_outcome(&result);
                }
                self.recovery = self
                    .state
                    .unknown_operation()
                    .map(|id| OperationId(id.into()));
                if self.recovery.is_some() && self.phase != "cancelled" {
                    self.phase = "paused".into();
                }
            }
            Kind::Done => {
                self.plan_workspace = None;
                self.phase = "completed".into();
                self.finish_preview("committed", true);
                if let Some(response) = self.state.turn.as_ref().and_then(|t| t.response.as_ref()) {
                    let text = response_text(response)?;
                    if !text.is_empty() {
                        self.emit(EventPayload::TextDelta { text });
                    }
                }
                self.emit(EventPayload::Done);
                self.snapshot(None);
            }
            Kind::Pause => {
                self.phase = "paused".into();
                self.diagnostic = self
                    .state
                    .turn
                    .as_ref()
                    .ok_or("runtime_turn")?
                    .diagnostic
                    .clone();
                self.emit(EventPayload::RuntimeError {
                    command: self
                        .pending_receipt
                        .as_ref()
                        .map(|r| r.command.clone())
                        .or_else(|| self.state.turn.as_ref().map(|t| t.receipt.command.clone())),
                    diagnostic: self.diagnostic.clone(),
                });
                self.snapshot(None);
            }
            Kind::Cancel => {
                self.plan_workspace = None;
                self.phase = "cancelled".into();
                if let Some(command) = self.cancelling.take() {
                    self.emit(EventPayload::Cancelled { command });
                }
                self.snapshot(None);
            }
            Kind::Resume => {
                self.recovered_pause = false;
                self.resume_epoch += 1;
                self.turn_stop = Cancel::default();
                self.gate.reset();
                self.plan = None;
                self.pending_pause = None;
                self.diagnostic.clear();
                self.phase = self
                    .state
                    .turn
                    .as_ref()
                    .ok_or("runtime_turn")?
                    .stage
                    .clone();
                if let Some(command) = self.resuming.take() {
                    self.emit(EventPayload::CommandAccepted {
                        command,
                        request: self.request(),
                    });
                }
            }
            Kind::Response | Kind::Continued | Kind::Compacted => {}
        }
        Ok(())
    }
    fn completed(&mut self, workers: &Workers, done: Completion) -> Result<()> {
        if self.storing.is_some()
            && matches!(
                &done,
                Completion::Provider { .. }
                    | Completion::Plan { .. }
                    | Completion::Auth {
                        task: AuthTask::BeforeIntent | AuthTask::SelectGeneration,
                        ..
                    }
            )
        {
            if self.deferred_completions.len() >= 3 {
                return Err("runtime_completion_limit");
            }
            self.deferred_completions.push_back(done);
            return Ok(());
        }
        // A biased select is only priority at poll time. Started can be sent
        // after that poll but before Completion becomes ready. Every executor
        // sends Started before its completion, so consume it while the exact
        // active tool identity is still owned here.
        if matches!(done, Completion::Tool { .. }) {
            self.drain_started();
        }
        match done {
            Completion::Plan { target, result } => {
                self.plan_admission_pending = false;
                if self.shutting_down
                    || self.storage_failed
                    || self.closing.mode.load(Ordering::Acquire) == 2
                {
                    return Ok(());
                }
                match (target, result) {
                    (PlanTarget::Submit(command), Ok(lease)) => {
                        if self.tools.plan_ready() {
                            self.plan_workspace = Some(lease);
                            self.submit(workers, command)?;
                        } else {
                            self.reject_plan(&command, "terminal_workspace_busy");
                        }
                    }
                    (PlanTarget::Submit(command), Err(code)) => self.reject_plan(&command, code),
                    (PlanTarget::Resume(command), Ok(lease))
                        if self.phase == "paused"
                            && self.recovery.is_none()
                            && self.tools.plan_ready() =>
                    {
                        self.plan_workspace = Some(lease);
                        self.resuming = Some(command);
                    }
                    (PlanTarget::Resume(command), Err(code)) => {
                        self.emit(EventPayload::RuntimeError {
                            command: Some(command),
                            diagnostic: code.into(),
                        })
                    }
                    (PlanTarget::Resume(command), Ok(_))
                        if self.phase == "paused" && self.recovery.is_none() =>
                    {
                        self.emit(EventPayload::RuntimeError {
                            command: Some(command),
                            diagnostic: "terminal_workspace_busy".into(),
                        });
                    }
                    _ => {}
                }
            }
            Completion::Stored(value) => self.stored(value)?,
            Completion::Provider { identity, result } => {
                if self.provider.as_ref() != Some(&identity) {
                    return Ok(());
                }
                self.provider = None;
                if !self.can_act() {
                    self.snapshot(None);
                    return Ok(());
                }
                match result {
                    Err(code) => self.pause(code),
                    Ok(ProviderValue::Response(response)) => {
                        let current = self.state.turn.as_ref().ok_or("runtime_turn")?;
                        let completed = current.operation_offset + current.results.len();
                        if !response.calls().is_empty()
                            && (current.rounds >= 64
                                || current.rounds - current.resume_round >= current.round_limit
                                || completed > 256
                                || response.calls().len() > 256 - completed
                                || completed - current.resume_operation > current.call_limit
                                || response.calls().len()
                                    > current.call_limit - (completed - current.resume_operation))
                        {
                            self.pause("tool_round_limit");
                            return Ok(());
                        }
                        let turn = self.turn();
                        self.persist(
                            workers,
                            Kind::Response,
                            Box::new(move |_| {
                                transition(
                                    &turn,
                                    "response",
                                    vec![("response", save_response(&response)?)],
                                )
                            }),
                            None,
                        )?;
                    }
                    Ok(ProviderValue::Compacted(value)) => {
                        let turn = self.turn();
                        self.persist(
                            workers,
                            Kind::Compacted,
                            Box::new(move |_| {
                                transition(
                                    &turn,
                                    "compacted",
                                    vec![(
                                        "compaction",
                                        Json::object_with_limits(
                                            vec![
                                                ("request", value.request().save()?),
                                                ("native", value.native().clone()),
                                            ],
                                            crate::session::journal_v1::MAX_RECORD,
                                        )?,
                                    )],
                                )
                            }),
                            None,
                        )?;
                    }
                }
            }
            Completion::Tool { identity, result } => {
                let Some((active, task)) = &self.tool else {
                    return Ok(());
                };
                if active != &identity {
                    return Ok(());
                }
                let task = *task;
                self.tool = None;
                match result {
                    Ok(ToolValue::Outcome(value)) => {
                        if self.lease_stop.as_ref().is_some_and(Cancel::cancelled)
                            && !self.turn_stop.cancelled()
                        {
                            self.pending_pause = Some("auth_required");
                        }
                        self.pending_outcome = Some(value);
                        self.release_auth(workers)?;
                    }
                    Ok(ToolValue::Recovered(value)) => {
                        if let Some(value) = value {
                            self.pending_outcome = Some(value);
                        }
                    }
                    Ok(ToolValue::Plan(values)) => {
                        if self.can_act() {
                            self.plan.as_mut().ok_or("runtime_plan")?.0 = values;
                            self.finish_preview("committed", true);
                        } else {
                            self.plan = None;
                        }
                    }
                    Err(code) => {
                        if task == ToolTask::Recover {
                            self.diagnostic = "operation_recovery_failed".into();
                        } else if self.can_act() {
                            self.pause(code);
                        }
                    }
                }
                self.snapshot(None);
            }
            Completion::Auth {
                identity,
                task,
                result,
            } => {
                if !self
                    .auth
                    .as_ref()
                    .is_some_and(|(active, kind, _)| active == &identity && *kind == task)
                {
                    return Ok(());
                }
                let holding = task == AuthTask::ToolLease && result.is_ok();
                if holding {
                    self.auth.as_mut().expect("matched auth").2 = true;
                    self.release_sent = false;
                } else {
                    self.auth = None;
                }
                if task == AuthTask::SelectGeneration {
                    self.selection_stop = None;
                    let command = self.selecting.take().ok_or("generation_selection")?;
                    if self.shutting_down || self.storage_failed {
                        self.control_commands.remove(&command.command.0);
                        if let Some(reply) = self.control_replies.remove(&command.command.0) {
                            let _ = reply.send(if self.storage_failed {
                                Err("journal_read_only")
                            } else {
                                Ok(PostStatus::Closed)
                            });
                        }
                        return Ok(());
                    }
                    match result {
                        Err(code) => {
                            self.control_commands.remove(&command.command.0);
                            if let Some(reply) = self.control_replies.remove(&command.command.0) {
                                let _ = reply.send(Err(code));
                            }
                            self.reject(&command, code);
                        }
                        Ok(_) => {
                            let CommandPayload::SetGeneration(selection) = command.payload else {
                                return Err("generation_selection");
                            };
                            self.record(
                                workers,
                                Kind::GenerationSelected,
                                "generation_selected",
                                vec![
                                    ("command", Json::string(&command.command.0)?),
                                    ("model", Json::string(&selection.model)?),
                                    (
                                        "effort",
                                        selection.effort.as_ref().map_or_else(
                                            || Json::parse("null"),
                                            |value| Json::string(value),
                                        )?,
                                    ),
                                ],
                            )?;
                        }
                    }
                    return Ok(());
                }
                if !self.can_act() {
                    if holding {
                        self.release_auth(workers)?;
                    }
                    self.snapshot(None);
                    return Ok(());
                }
                match result {
                    Err(code) => {
                        if task == AuthTask::ToolLease
                            && let Some(intent) =
                                self.state.turn.as_ref().and_then(|t| t.intent.as_ref())
                        {
                            self.pending_outcome = Some(not_started(intent));
                        }
                        self.pause(code);
                    }
                    Ok(stop) => match task {
                        AuthTask::SelectGeneration => unreachable!("selection handled above"),
                        AuthTask::Generate => {
                            if self.state.pending_followups().next().is_none() {
                                self.provider(workers, ProviderTask::Generate)?;
                            }
                        }
                        AuthTask::Compact => self.provider(workers, ProviderTask::Compact)?,
                        AuthTask::BeforeIntent => {
                            if self.state.pending_followups().next().is_some() {
                                return Ok(());
                            }
                            let current = self.state.turn.as_ref().ok_or("runtime_turn")?;
                            let invocation = self
                                .plan
                                .as_ref()
                                .and_then(|(p, _)| p.get(current.results.len()))
                                .ok_or("runtime_plan")?
                                .clone();
                            self.record(
                                workers,
                                Kind::Intent,
                                "intent",
                                vec![("invocation", invocation.save()?)],
                            )?;
                        }
                        AuthTask::ToolLease => {
                            if stop.cancelled() || self.state.pending_followups().next().is_some() {
                                let intent = self
                                    .state
                                    .turn
                                    .as_ref()
                                    .and_then(|t| t.intent.as_ref())
                                    .ok_or("runtime_intent")?;
                                self.pending_outcome = Some(not_started(intent));
                                if stop.cancelled() {
                                    self.pause("auth_required");
                                }
                                self.release_auth(workers)?;
                            } else {
                                self.execute(workers, stop)?;
                            }
                        }
                    },
                }
            }
            Completion::AuthReleased { identity, result } => {
                if self
                    .auth
                    .as_ref()
                    .is_some_and(|(active, _, holding)| active == &identity && *holding)
                {
                    self.auth = None;
                    self.release_sent = false;
                    self.lease_stop = None;
                    if let Err(code) = result {
                        self.fail(code);
                    }
                    self.snapshot(None);
                }
            }
        }
        Ok(())
    }
    fn finish_start(&mut self, status: &watch::Sender<Option<Result<()>>>) {
        if self.initialized
            && self.starting
            && (self.recovery_attempted || self.storage_failed)
            && self.tool.is_none()
            && self.storing.is_none()
            && self.pending_outcome.is_none()
        {
            self.starting = false;
            self.activity.clear();
            if let Some(turn) = &self.state.turn {
                for result in turn.previous_results.iter().chain(&turn.results) {
                    if result.dispatched {
                        self.activity.push(Activity::Finished(Finished {
                            operation: OperationId(result.result.operation_id().into()),
                            call_id: result.result.call_id().into(),
                            execution: result.result.execution(),
                            effects: result.result.effects(),
                            output: Arc::new(result.result.output().into()),
                        }));
                    }
                }
            }
            self.recovery = self
                .state
                .unknown_operation()
                .map(|id| OperationId(id.into()));
            status.send_replace(Some(Ok(())));
            self.snapshot(None);
        }
    }
    fn started(&mut self, identity: Identity, value: Started) {
        if self
            .tool
            .as_ref()
            .is_some_and(|(active, task)| active == &identity && *task == ToolTask::Execute)
        {
            self.activity.push(Activity::Started(value.clone()));
            self.emit(EventPayload::ToolStarted(value));
        }
    }
    fn drain_started(&mut self) {
        while let Ok((identity, value)) = self.starts.try_recv() {
            self.started(identity, value);
        }
    }
    pub async fn run(
        mut self,
        mut boot: oneshot::Receiver<Result<Ready>>,
        workers: &Workers,
        status: &watch::Sender<Option<Result<()>>>,
    ) -> Result<()> {
        loop {
            if self.closing.mode.load(Ordering::Acquire) == 2 && !self.shutting_down {
                self.shutting_down = true;
                if let Some(stop) = &self.selection_stop {
                    stop.cancel();
                }
                // Commands not yet assigned to a journal write have no durable
                // admission. Release their callers; never start a new catalog
                // validation while shutting down.
                for command in self
                    .new_followups
                    .drain(..)
                    .chain(self.selections.drain(..))
                {
                    self.control_commands.remove(&command.command.0);
                    if let Some(reply) = self.control_replies.remove(&command.command.0) {
                        let _ = reply.send(Ok(PostStatus::Closed));
                    }
                }
                self.queued.clear();
                if self.can_cancel() {
                    self.cancel("shutdown".into());
                } else {
                    self.gate.cancel();
                    self.turn_stop.cancel();
                }
            }
            if let Err(code) = self.advance(workers)
                && (code != "dispatch_cancelled" || self.closing.mode.load(Ordering::Acquire) != 2)
            {
                self.fail(code);
            }
            self.finish_start(status);
            // A shutdown can race the last dispatch check in advance(). The
            // owner must close any unexecuted durable intent before exiting.
            let closing_mode = self.closing.mode.load(Ordering::Acquire);
            if closing_mode == 2 && !self.shutting_down {
                continue;
            }
            if self.initialized
                && closing_mode > 0
                && !self.busy()
                && self.pending_outcome.is_none()
                && !(self.available() && !self.queued.is_empty() && !self.shutting_down)
            {
                break;
            }
            enum Action {
                Boot(std::result::Result<Result<Ready>, oneshot::error::RecvError>),
                Command(Option<Envelope>),
                Child(Option<ChildLookup>),
                Start(Option<(Identity, Started)>),
                Complete(Option<Completion>),
                Preview(std::result::Result<(), watch::error::RecvError>),
                Wake,
            }
            let action = tokio::select! {biased;
                value=self.starts.recv()=>Action::Start(value),
                value=self.mail.cancel.recv()=>Action::Command(value),
                _=self.closing.wake.notified()=>Action::Wake,
                value=&mut boot,if !self.initialized=>Action::Boot(value),
                value=self.done.recv()=>Action::Complete(value),
                value=self.mail.snapshot.recv()=>Action::Command(value),
                value=self.mail.child.recv()=>Action::Child(value),
                value=self.mail.transcript.recv()=>Action::Command(value),
                value=self.mail.resume.recv()=>Action::Command(value),
                value=self.mail.submits.recv()=>Action::Command(value),
                value=self.previews.changed()=>Action::Preview(value),
            };
            let result = match action {
                Action::Boot(value) => match value.unwrap_or(Err("journal_worker_closed")) {
                    Ok(ready) => {
                        if ready.state.submits.len() > self.limits.submit_ids {
                            status.send_replace(Some(Err("session_command_limit")));
                            return Err("session_command_limit");
                        }
                        self.state = ready.state;
                        self.journal_identity = ready.journal_identity;
                        self.diagnostic = ready.diagnostic.into();
                        self.storage_failed = !ready.diagnostic.is_empty();
                        self.recovered_pause = ready.requires_resume;
                        if self.diagnostic.is_empty() && ready.requires_resume {
                            self.diagnostic = self
                                .state
                                .turn
                                .as_ref()
                                .map(|turn| turn.diagnostic.as_str())
                                .filter(|code| crate::provider::contracts::valid_id(code))
                                .unwrap_or("session_interrupted")
                                .to_owned();
                        }
                        self.receipts = self.state.submits.clone();
                        self.recovery = self
                            .state
                            .unknown_operation()
                            .map(|id| OperationId(id.into()));
                        self.phase = if self.storage_failed || ready.requires_resume {
                            "paused".into()
                        } else {
                            self.state
                                .turn
                                .as_ref()
                                .map_or_else(|| "idle".into(), |t| t.stage.clone())
                        };
                        if self.state.turn.is_some() {
                            self.gate.reset();
                        }
                        self.initialized = true;
                        Ok(())
                    }
                    Err(code) => {
                        status.send_replace(Some(Err(code)));
                        return Err(code);
                    }
                },
                Action::Command(Some(value)) => self.admit(value),
                Action::Command(None) => Err("runtime_mailbox_closed"),
                Action::Child(Some(lookup)) => {
                    let result = self.child_attempt(&lookup);
                    let _ = lookup.reply.send(result);
                    Ok(())
                }
                Action::Child(None) => Err("runtime_mailbox_closed"),
                Action::Start(Some((identity, value))) => {
                    self.started(identity, value);
                    Ok(())
                }
                Action::Start(None) => Err("tool_worker_closed"),
                Action::Complete(Some(value)) => self.completed(workers, value),
                Action::Complete(None) => Err("runtime_workers_closed"),
                Action::Preview(Ok(())) => {
                    let value = self.previews.borrow_and_update().clone();
                    if let Some(update) = value
                        && self.provider.as_ref() == Some(&update.identity)
                        && self.can_act()
                        && let Some(preview) = &mut self.preview
                        && preview.generation == update.generation
                        && preview.state == "preview"
                    {
                        if update.text.len() > 64 * 1024 || !update.text.starts_with(&preview.text)
                        {
                            self.pause("preview_prefix_conflict");
                        } else if update.text != preview.text {
                            preview.text = update.text.clone();
                            let value = preview.clone();
                            self.emit(EventPayload::Preview(value));
                        }
                    }
                    Ok(())
                }
                Action::Preview(Err(_)) => Err("provider_worker_closed"),
                Action::Wake => Ok(()),
            };
            if let Err(code) = result
                && (code != "dispatch_cancelled" || self.closing.mode.load(Ordering::Acquire) != 2)
            {
                self.fail(code);
            }
        }
        Ok(())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(stop) = &self.selection_stop {
            stop.cancel();
        }
        self.gate.cancel();
        self.turn_stop.cancel();
    }
}
