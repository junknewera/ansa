//! Transactional canonical event reducer. An invalid event never changes state.
use super::{
    Result,
    codec_v1::{Invocation, SavedResult, load_response, next_request, response_text},
    need, num,
    transcript::{Entry, Transcript},
};
use crate::{
    context::{epoch::compact_epoch, legacy_compact::CompactedContext},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        contracts::{AgentMode, CommandId, RequestId, SessionId, TurnId, valid_id},
        replay::{Effects, Execution},
        request::{Advertisement, Epoch, Request},
        responses::AdmittedResponse,
    },
    tools::admission::OperationIds,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
pub const COMPACT_ITEM_THRESHOLD: usize = 4096 - 2 * 64 - 16;
#[derive(Clone, Debug)]
pub struct Receipt {
    pub command: CommandId,
    pub turn: TurnId,
    pub request: RequestId,
    pub text: Arc<String>,
    pub state: String,
    pub mode: AgentMode,
}
#[derive(Clone, Debug)]
pub struct SavedTurn {
    pub receipt: Receipt,
    pub operation_run: String,
    pub next: Request,
    pub response: Option<Arc<AdmittedResponse>>,
    pub tools_response: Option<Arc<AdmittedResponse>>,
    pub results: Vec<SavedResult>,
    pub previous_results: Vec<SavedResult>,
    pub intent: Option<Invocation>,
    pub stage: String,
    pub diagnostic: String,
    pub paused: bool,
    pub resumes: BTreeSet<String>,
    pub rounds: usize,
    pub operation_offset: usize,
    pub round_limit: usize,
    pub call_limit: usize,
    pub resume_round: usize,
    pub resume_operation: usize,
}
#[derive(Clone, Debug)]
pub struct State {
    pub session: SessionId,
    pub configuration: Request,
    pub epoch: Epoch,
    pub history: Vec<Json>,
    pub history_bytes: usize,
    pub submits: BTreeMap<String, Receipt>,
    pub turn: Option<SavedTurn>,
    pub next_request: usize,
    pub compactions: usize,
    pub initialized: bool,
    plan_tools: Advertisement,
    transcript: Arc<Transcript>,
}
impl State {
    pub fn new(session: SessionId, configuration: Request, plan_tools: Advertisement) -> Self {
        Self {
            session,
            epoch: configuration.epoch.clone(),
            configuration,
            history: vec![],
            history_bytes: 2,
            submits: BTreeMap::new(),
            turn: None,
            next_request: 0,
            compactions: 0,
            initialized: false,
            plan_tools,
            transcript: Arc::default(),
        }
    }
    pub fn transcript(&self) -> Arc<Transcript> {
        self.transcript.clone()
    }
    pub fn initialization(&self) -> Result<Json> {
        Json::object(vec![
            ("kind", Json::string("init")?),
            ("version", num(1)?),
            ("session", Json::string(&self.session.0)?),
            ("configuration", self.configuration.save()?),
        ])
    }
    pub fn request_for(&self, text: &str, mode: AgentMode) -> Result<Request> {
        let (epoch, tools) = if mode == AgentMode::Basic {
            (self.epoch.clone(), self.configuration.tools.clone())
        } else {
            need(!self.plan_tools.revision.is_empty(), "session_plan_tools")?;
            let provenance = Json::object(vec![
                ("base_epoch", Json::string(&self.epoch.id)?),
                ("base_state", self.epoch.exact_state.clone()),
                ("base_instructions", Json::string(&self.epoch.instructions)?),
                ("mode", Json::string("plan")?),
                ("tools_revision", Json::string(&self.plan_tools.revision)?),
            ])?;
            (
                Epoch {
                    id: sha256(provenance.dump().as_bytes()),
                    instructions: format!(
                        "{}\nPlan mode: inspect and reason using read-only tools. Do not modify files or execute commands. Provide a concrete plan.",
                        self.epoch.instructions
                    ),
                    exact_state: provenance,
                },
                self.plan_tools.clone(),
            )
        };
        let mut input = self.history.clone();
        input.push(Json::object(vec![
            ("role", Json::string("user")?),
            ("content", Json::string(text)?),
        ])?);
        Request::new(self.configuration.binding.clone(), epoch, tools, input)
    }
    pub fn unknown_operation(&self) -> Option<&str> {
        let turn = self.turn.as_ref()?;
        turn.intent
            .as_ref()
            .map(|i| i.operation.as_str())
            .or_else(|| {
                turn.results
                    .iter()
                    .find(|r| r.result.effects() == Effects::Unknown)
                    .map(|r| r.result.operation_id())
            })
    }
    pub fn apply(&mut self, event: &Json) -> Result<()> {
        let mut next = self.clone();
        next.reduce(event)?;
        *self = next;
        Ok(())
    }
    fn reduce(&mut self, event: &Json) -> Result<()> {
        let kind = event.text_field("kind")?;
        if kind == "init" {
            need(
                !self.initialized
                    && event.usize_field("version")? == 1
                    && event.text_field("session")? == self.session.0
                    && Request::load(event.field("configuration")?)?.save()?
                        == self.configuration.save()?,
                "session_configuration_mismatch",
            )?;
            self.initialized = true;
            return Ok(());
        }
        need(self.initialized, "session_missing_init")?;
        if kind == "accepted" {
            need(
                self.turn
                    .as_ref()
                    .is_none_or(|t| t.stage == "completed" || t.stage == "cancelled"),
                "session_active_turn",
            )?;
            let id = event.text_field("command")?;
            let turn = event.text_field("turn")?;
            let request = event.text_field("request")?;
            let text = event.text_field("text")?;
            need(
                valid_id(id)
                    && valid_id(turn)
                    && valid_id(request)
                    && !text.is_empty()
                    && text.len() <= 16 * 1024
                    && !self.submits.contains_key(id)
                    && self.submits.len() < 65536
                    && event.usize_field("request_number")?
                        == self.next_request.checked_add(1).ok_or("session_submit")?,
                "session_submit",
            )?;
            let mode = match if event.has("mode") {
                event.text_field("mode")?
            } else {
                "basic"
            } {
                "basic" => AgentMode::Basic,
                "plan" => AgentMode::Plan,
                _ => return Err("session_mode"),
            };
            let next = Request::load(event.field("next")?)?;
            need(
                next.save()? == self.request_for(text, mode)?.save()?,
                "session_request_conflict",
            )?;
            let stage = if *event.field("compact")? == Json::parse("true")? {
                "compacting"
            } else {
                "generating"
            };
            let receipt = Receipt {
                command: CommandId(id.into()),
                turn: TurnId(turn.into()),
                request: RequestId(request.into()),
                text: Arc::new(text.into()),
                state: stage.into(),
                mode,
            };
            let run = event.text_field("operation_run")?;
            OperationIds::new(run, 0)?;
            let round_limit = if event.has("round_limit") {
                event.usize_field("round_limit")?
            } else {
                1
            };
            let call_limit = if event.has("call_limit") {
                event.usize_field("call_limit")?
            } else {
                16
            };
            need(
                round_limit > 0 && round_limit <= 64 && call_limit > 0 && call_limit <= 256,
                "session_turn_limits",
            )?;
            Arc::make_mut(&mut self.transcript).append(Entry {
                turn: receipt.turn.clone(),
                request: receipt.request.clone(),
                role: "user".into(),
                text: text.into(),
                state: stage.into(),
                mode,
            });
            self.turn = Some(SavedTurn {
                receipt: receipt.clone(),
                operation_run: run.into(),
                next,
                response: None,
                tools_response: None,
                results: vec![],
                previous_results: vec![],
                intent: None,
                stage: stage.into(),
                diagnostic: String::new(),
                paused: false,
                resumes: BTreeSet::new(),
                rounds: 0,
                operation_offset: 0,
                round_limit,
                call_limit,
                resume_round: 0,
                resume_operation: 0,
            });
            self.submits.insert(id.into(), receipt);
            self.next_request += 1;
            return Ok(());
        }
        need(
            self.turn.as_ref().is_some_and(|t| {
                event
                    .text_field("turn")
                    .is_ok_and(|id| id == t.receipt.turn.0)
            }),
            "session_turn_identity",
        )?;
        let unknown = self.unknown_operation().is_some();
        if kind == "compacted" {
            need(
                self.turn.as_ref().is_some_and(|t| t.stage == "compacting"),
                "session_compaction_stage",
            )?;
            let value = event.field("compaction")?;
            let compacted = CompactedContext::admit(
                Request::load(value.field("request")?)?,
                value.field("native")?.clone(),
            )?;
            let expected = Request::new(
                self.configuration.binding.clone(),
                self.epoch.clone(),
                self.configuration.tools.clone(),
                self.history.clone(),
            )?;
            need(
                compacted.request().save()? == expected.save()?,
                "session_compaction_request",
            )?;
            need(
                compacted.output()?.len() + 1 < COMPACT_ITEM_THRESHOLD,
                "compaction_item_limit",
            )?;
            self.compactions += 1;
            self.epoch = compact_epoch(&self.configuration.epoch, self.compactions, &compacted)?;
            self.history = compacted.output()?.to_vec();
            self.history_bytes = Json::array(self.history.clone())?.dump().len();
            let current = self.turn.as_ref().ok_or("session_turn_identity")?;
            let next = self.request_for(&current.receipt.text, current.receipt.mode)?;
            let current = self.turn.as_mut().ok_or("session_turn_identity")?;
            current.next = next;
            current.stage = "generating".into();
        } else {
            let current = self.turn.as_mut().ok_or("session_turn_identity")?;
            match kind {
                "response" => {
                    need(current.stage == "generating", "session_response_stage")?;
                    let response = load_response(event.field("response")?)?;
                    let expected = if let Some(previous) = &current.tools_response {
                        if !previous.calls().is_empty() {
                            need(
                                current.results.len() == previous.calls().len()
                                    && current.intent.is_none(),
                                "session_results_incomplete",
                            )?;
                            next_request(previous, &current.results)?
                        } else {
                            current.next.clone()
                        }
                    } else {
                        current.next.clone()
                    };
                    need(
                        response.request().save()? == expected.save()?,
                        "session_response_request",
                    )?;
                    let completed = current.operation_offset + current.results.len();
                    if !response.calls().is_empty() {
                        need(
                            current.rounds < 64
                                && current.rounds.saturating_sub(current.resume_round)
                                    < current.round_limit
                                && completed <= 256
                                && response.calls().len() <= 256 - completed
                                && completed.saturating_sub(current.resume_operation)
                                    <= current.call_limit
                                && response.calls().len()
                                    <= current.call_limit
                                        - completed.saturating_sub(current.resume_operation),
                            "session_turn_limits",
                        )?;
                    }
                    if current.tools_response.is_some() {
                        need(
                            current.results.iter().all(|r| {
                                r.dispatched && r.result.effects() == Effects::KnownResult
                            }),
                            "session_round_incomplete",
                        )?;
                        current.previous_results.append(&mut current.results);
                        current.operation_offset = completed;
                        current.tools_response = None;
                    }
                    let response = Arc::new(response);
                    if response.calls().is_empty() {
                        current.stage = "final".into();
                    } else {
                        current.rounds += 1;
                        current.tools_response = Some(response.clone());
                        current.stage = "tools".into();
                    }
                    current.next = response.request().clone();
                    current.response = Some(response);
                }
                "intent" => {
                    need(
                        current.stage == "tools"
                            && current.tools_response.is_some()
                            && current.intent.is_none(),
                        "session_intent_stage",
                    )?;
                    let response = current
                        .tools_response
                        .as_ref()
                        .ok_or("session_intent_stage")?;
                    let index = current.results.len();
                    let call = response.calls().get(index).ok_or("session_intent_stage")?;
                    let invocation = Invocation::load(event.field("invocation")?)?;
                    need(
                        invocation.operation
                            == format!(
                                "{}:op:{}",
                                current.operation_run,
                                current.operation_offset + index + 1
                            )
                            && invocation.call == call.call_id
                            && invocation.name == call.name
                            && invocation.arguments == Json::parse(&call.arguments)?
                            && current.next.tools.descriptors.iter().any(|d| {
                                d.name == invocation.name && d.revision == invocation.revision
                            }),
                        "session_intent_conflict",
                    )?;
                    current.intent = Some(invocation);
                }
                "outcome" => {
                    let intent = current
                        .intent
                        .as_ref()
                        .ok_or("session_outcome_without_intent")?;
                    let result = SavedResult::load(event.field("result")?)?;
                    need(
                        result.result.operation_id() == intent.operation
                            && result.result.call_id() == intent.call
                            && (result.result.execution() != Execution::Succeeded
                                || result.result.effects() == Effects::KnownResult),
                        "session_outcome_identity",
                    )?;
                    current.results.push(result);
                    current.intent = None;
                }
                "continued" => {
                    need(
                        current.stage == "tools"
                            && current
                                .tools_response
                                .as_ref()
                                .is_some_and(|r| current.results.len() == r.calls().len())
                            && current.intent.is_none(),
                        "session_continuation",
                    )?;
                    need(
                        current
                            .results
                            .iter()
                            .all(|r| r.dispatched && r.result.effects() == Effects::KnownResult),
                        "session_tool_failed",
                    )?;
                    current.stage = "generating".into();
                }
                "done" => {
                    need(
                        current.stage == "final"
                            && current
                                .response
                                .as_ref()
                                .is_some_and(|r| r.calls().is_empty()),
                        "session_done_stage",
                    )?;
                    let response = current.response.as_ref().ok_or("session_done_stage")?;
                    let text = response_text(response)?;
                    let transcript = Arc::make_mut(&mut self.transcript);
                    transcript.set_last("completed");
                    transcript.append(Entry {
                        turn: current.receipt.turn.clone(),
                        request: current.receipt.request.clone(),
                        role: "assistant".into(),
                        text,
                        state: "completed".into(),
                        mode: current.receipt.mode,
                    });
                    self.history = response.request().input.clone();
                    self.history
                        .extend(response.items().iter().map(|i| i.value().clone()));
                    self.history_bytes = Json::array(self.history.clone())?.dump().len();
                    current.stage = "completed".into();
                }
                "cancel" => {
                    need(current.stage != "completed", "session_cancel_completed")?;
                    current.stage = "cancelled".into();
                    current.paused = false;
                }
                "pause" => {
                    need(
                        current.stage != "completed" && current.stage != "cancelled",
                        "session_pause_terminal",
                    )?;
                    let code = event.text_field("diagnostic")?;
                    need(valid_id(code), "session_diagnostic")?;
                    current.diagnostic = code.into();
                    current.paused = true;
                }
                "resume" => {
                    let id = event.text_field("command")?;
                    need(
                        current.stage != "completed"
                            && current.stage != "cancelled"
                            && !unknown
                            && valid_id(id)
                            && !current.resumes.contains(id)
                            && current.resumes.len() < 4096,
                        "session_resume",
                    )?;
                    current.resumes.insert(id.into());
                    current.resume_round = current.rounds;
                    while current
                        .results
                        .last()
                        .is_some_and(|r| !r.dispatched && r.result.effects() == Effects::NotStarted)
                    {
                        current.results.pop();
                    }
                    current.resume_operation = current.operation_offset + current.results.len();
                    current.paused = false;
                    current.diagnostic.clear();
                }
                _ => return Err("session_record_kind"),
            }
        }
        let current = self.turn.as_mut().ok_or("session_turn_identity")?;
        current.receipt.state = if current.paused {
            "paused".into()
        } else {
            current.stage.clone()
        };
        if kind != "done" {
            Arc::make_mut(&mut self.transcript).set_last(&current.receipt.state);
        }
        self.submits
            .get_mut(&current.receipt.command.0)
            .ok_or("session_submit")?
            .state = current.receipt.state.clone();
        Ok(())
    }
}
