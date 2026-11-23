//! Untrusted presentation derived from read-only canonical journal inspection.
//! These observations never authorize Resume, provider calls or tool dispatch.
use super::{
    Result,
    catalog::CatalogEntry,
    codec_v1::{Invocation, SavedResult, load_response, next_request},
    journal_v1::inspect_read_only,
    need,
    state::State,
};
use crate::{
    auth::Cancel,
    protocol::json::Json,
    provider::{
        contracts::valid_id,
        replay::Effects,
        request::{Advertisement, Request},
        responses::AdmittedResponse,
    },
};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummaryStatus {
    Empty,
    Completed,
    Cancelled,
    Paused,
    Interrupted,
    Unknown,
    Unavailable,
}
impl SummaryStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "New",
            Self::Completed => "Completed",
            Self::Cancelled => "Stopped",
            Self::Paused => "Paused",
            Self::Interrupted => "Interrupted",
            Self::Unknown => "Unknown",
            Self::Unavailable => "Unavailable",
        }
    }
}
#[derive(Clone, Debug)]
pub struct AgentSummary {
    pub model: String,
    pub writable: bool,
    pub outcome: Option<crate::provider::structured::ChildStatus>,
}
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub agent: Option<AgentSummary>,
    pub entry: CatalogEntry,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub status: SummaryStatus,
    pub pause_diagnostic: Option<String>,
}
impl SessionSummary {
    fn unavailable(entry: CatalogEntry) -> Self {
        Self {
            agent: None,
            entry,
            title: None,
            preview: None,
            status: SummaryStatus::Unavailable,
            pause_diagnostic: None,
        }
    }
}
fn excerpt(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars().filter(|v| !v.is_control());
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push_str("...");
    }
    result
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Empty,
    Generating,
    Compacting,
    Tools,
    Final,
    Completed,
    Cancelled,
}
pub(crate) fn read(
    path: &Path,
    entry: CatalogEntry,
    budget: &mut usize,
    stop: &Cancel,
    compiled: Option<&crate::tools::registry::Snapshot>,
) -> Result<SessionSummary> {
    let mut summary = SessionSummary {
        agent: None,
        entry: entry.clone(),
        title: None,
        preview: None,
        status: SummaryStatus::Empty,
        pause_diagnostic: None,
    };
    let mut initialized = false;
    let mut turn: Option<String> = None;
    let mut request_number = 0usize;
    let mut final_text: Option<String> = None;
    let mut unknown_effect = false;
    let mut outstanding: Option<(String, String)> = None;
    let mut stage = Stage::Empty;
    let mut expected: Option<Request> = None;
    let mut response: Option<AdmittedResponse> = None;
    let mut results: Vec<SavedResult> = Vec::new();
    let mut request_id = String::new();
    let mut queued: Vec<(String, String)> = Vec::new();
    let mut paused = false;
    let mut configuration: Option<Request> = None;
    let mut canonical: Option<State> = None;
    let mut plan = false;
    let result = inspect_read_only(
        path,
        budget,
        || stop.check(),
        |event| {
            let kind = event.text_field("kind")?;
            if !initialized {
                need(
                    kind == "init" && event.text_field("session")? == entry.id,
                    "session_missing_init",
                )?;
                let saved = Request::load(event.field("configuration")?)?;
                let structured_child = saved.final_format()
                    == Some(crate::provider::structured::FinalFormat::ChildResult);
                let plan_tools = compiled
                    .and_then(|catalog| catalog.verified_plan_tools(&saved.tools))
                    .unwrap_or_else(|| Advertisement {
                        revision: String::new(),
                        descriptors: vec![],
                    });
                let mut state = State::new(entry.id.as_str().into(), saved.clone(), plan_tools);
                state.apply(event)?;
                canonical = Some(state);
                summary.entry.parent = None;
                if event.has("fork_seed") {
                    let fork = event.field("fork_seed")?;
                    need(fork.text_field("kind")? == "fork", "session_fork_seed")?;
                    let source = fork.text_field("source_session")?;
                    need(
                        valid_id(source)
                            && source != entry.id
                            && valid_id(fork.text_field("source_turn")?),
                        "session_fork_source",
                    )?;
                    Request::load(fork.field("context")?)?;
                    summary.entry.parent = Some(source.into());
                    // Deliberately do not use the inherited transcript for titles.
                } else if saved.epoch.exact_state.has("local_console_parent") {
                    let source = saved.epoch.exact_state.text_field("local_console_parent")?;
                    need(valid_id(source), "session_fork_source")?;
                    summary.entry.parent = Some(source.into());
                } else if structured_child {
                    let source = saved.epoch.exact_state.text_field("orchestration_parent")?;
                    need(
                        valid_id(source) && source != entry.id,
                        "session_fork_source",
                    )?;
                    summary.entry.parent = Some(source.into());
                    summary.agent = Some(AgentSummary {
                        model: saved.binding.requested_model.clone(),
                        writable: saved
                            .epoch
                            .exact_state
                            .field("orchestration_writes")
                            .and_then(Json::elements)
                            .is_ok_and(|paths| !paths.is_empty()),
                        outcome: None,
                    });
                }
                configuration = Some(saved);
                initialized = true;
                return Ok(());
            }
            canonical
                .as_mut()
                .ok_or("session_missing_init")?
                .apply(event)?;
            if kind == "fork" {
                let source = event.text_field("source_session")?;
                need(
                    turn.is_none() && valid_id(source) && source != entry.id,
                    "session_fork_source",
                )?;
                Request::load(event.field("context")?)?;
                summary.entry.parent = Some(source.into());
                return Ok(());
            }
            if kind == "autocompact_selected" {
                return Ok(());
            }
            if kind == "generation_selected" {
                need(
                    valid_id(event.text_field("command")?)
                        && !event.text_field("model")?.is_empty(),
                    "generation_selection",
                )?;
                if !event.field("effort")?.is_null() {
                    need(valid_id(event.text_field("effort")?), "reasoning_effort")?;
                }
                return Ok(());
            }
            if kind == "accepted" {
                need(
                    matches!(stage, Stage::Empty | Stage::Completed | Stage::Cancelled)
                        && outstanding.is_none(),
                    "session_active_turn",
                )?;
                let text = event.text_field("text")?;
                let internal = event.has("handoff");
                need(
                    !text.is_empty()
                        && valid_id(event.text_field("command")?)
                        && valid_id(event.text_field("request")?)
                        && valid_id(event.text_field("turn")?),
                    "session_submit",
                )?;
                request_number = request_number.checked_add(1).ok_or("session_submit")?;
                need(
                    event.usize_field("request_number")? == request_number,
                    "session_submit",
                )?;
                if event.has("mode") {
                    need(
                        matches!(event.text_field("mode")?, "basic" | "plan"),
                        "session_mode",
                    )?;
                }
                // State::apply already checked the exact turn, image hashes and request.
                let next = canonical
                    .as_ref()
                    .and_then(|state| state.turn.as_ref())
                    .ok_or("session_request_conflict")?
                    .next
                    .clone();
                let saved = configuration.as_ref().ok_or("session_missing_init")?;
                stage = if event.field("compact")?.boolean()? {
                    Stage::Compacting
                } else {
                    Stage::Generating
                };
                expected = Some(next);
                response = None;
                results.clear();
                request_id = event.text_field("request")?.into();
                plan = event.has("mode") && event.text_field("mode")? == "plan";
                paused = false;
                let question = if internal {
                    super::state::displayed_handoff_question(text)
                } else {
                    Some(text.to_owned())
                };
                if let Some(question) = &question {
                    let text = question.as_str();
                    if saved.final_format()
                        == Some(crate::provider::structured::FinalFormat::ChildResult)
                    {
                        summary.title.get_or_insert_with(|| {
                            excerpt(&crate::orchestration::plan::assignment_title(text), 96)
                        });
                        summary.preview = Some(excerpt(&saved.binding.requested_model, 192));
                    } else {
                        summary.title.get_or_insert_with(|| excerpt(text, 96));
                        summary.preview = Some(excerpt(text, 192));
                    }
                }
                summary.status = SummaryStatus::Interrupted;
                turn = Some(event.text_field("turn")?.into());
                final_text = None;
                unknown_effect = false;
                return Ok(());
            }
            need(
                turn.as_deref()
                    .is_some_and(|turn| event.text_field("turn").is_ok_and(|v| v == turn)),
                "session_turn_identity",
            )?;
            match kind {
                "side_reply" => {} // The canonical reducer checked this answer above.
                "response" => {
                    need(
                        stage == Stage::Generating && !paused && outstanding.is_none(),
                        "session_response_stage",
                    )?;
                    let admitted = load_response(event.field("response")?)?;
                    need(
                        admitted.request().save()?
                            == expected
                                .as_ref()
                                .ok_or("session_response_request")?
                                .save()?,
                        "session_response_request",
                    )?;
                    final_text = if admitted.calls().is_empty() {
                        Some(super::state::display_response(&admitted)?)
                    } else {
                        None
                    };
                    stage = if admitted.calls().is_empty() {
                        Stage::Final
                    } else {
                        Stage::Tools
                    };
                    response = Some(admitted);
                    results.clear();
                }
                "done" => {
                    need(
                        stage == Stage::Final
                            && !paused
                            && outstanding.is_none()
                            && !unknown_effect,
                        "session_done_stage",
                    )?;
                    let text = final_text.take().ok_or("session_done_stage")?;
                    if let Some(agent) = &mut summary.agent {
                        agent.outcome = response
                            .as_ref()
                            .and_then(|r| r.child_result().ok())
                            .map(|report| report.status);
                    }
                    summary.preview = Some(excerpt(&text, 192));
                    summary.status = if unknown_effect {
                        SummaryStatus::Unknown
                    } else {
                        SummaryStatus::Completed
                    };
                    stage = Stage::Completed;
                }
                "cancel" => {
                    need(stage != Stage::Completed, "session_cancel_completed")?;
                    if event.has("dismissed") {
                        queued.clear();
                    }
                    stage = Stage::Cancelled;
                    paused = false;
                    final_text = None;
                    summary.status = SummaryStatus::Cancelled;
                }
                "pause" => {
                    need(
                        !matches!(stage, Stage::Completed | Stage::Cancelled),
                        "session_pause_terminal",
                    )?;
                    need(
                        valid_id(event.text_field("diagnostic")?),
                        "session_diagnostic",
                    )?;
                    paused = true;
                    summary.status = if unknown_effect {
                        SummaryStatus::Unknown
                    } else {
                        SummaryStatus::Paused
                    };
                }
                "resume" => {
                    need(
                        stage != Stage::Completed
                            && (stage != Stage::Cancelled || !queued.is_empty())
                            && !unknown_effect
                            && outstanding.is_none(),
                        "session_resume",
                    )?;
                    need(valid_id(event.text_field("command")?), "session_resume")?;
                    while results.last().is_some_and(|result| {
                        !result.dispatched && result.result.effects() == Effects::NotStarted
                    }) {
                        results.pop();
                    }
                    paused = false;
                    summary.status = SummaryStatus::Interrupted;
                }
                "intent" => {
                    let invocation = Invocation::load(event.field("invocation")?)?;
                    need(
                        stage == Stage::Tools && !paused && outstanding.is_none(),
                        "session_intent_stage",
                    )?;
                    let call = response
                        .as_ref()
                        .and_then(|value| value.calls().get(results.len()))
                        .ok_or("session_intent_stage")?;
                    need(
                        invocation.call == call.call_id
                            && invocation.name == call.name
                            && invocation.arguments == Json::parse(&call.arguments)?
                            && expected.as_ref().is_some_and(|next| {
                                next.tools.descriptors.iter().any(|descriptor| {
                                    descriptor.name == invocation.name
                                        && descriptor.revision == invocation.revision
                                })
                            }),
                        "session_intent_conflict",
                    )?;
                    outstanding = Some((invocation.operation, invocation.call));
                }
                "outcome" => {
                    let saved = SavedResult::load(event.field("result")?)?;
                    let result = &saved.result;
                    need(
                        outstanding.as_ref().is_some_and(|(operation, call)| {
                            operation == result.operation_id() && call == result.call_id()
                        }),
                        "session_outcome_identity",
                    )?;
                    outstanding = None;
                    if result.effects() == Effects::Unknown {
                        unknown_effect = true;
                        summary.status = SummaryStatus::Unknown;
                    }
                    results.push(saved);
                }
                "queued" => {
                    need(
                        valid_id(event.text_field("command")?)
                            && event.text_field("target")? == request_id
                            && !event.text_field("text")?.is_empty()
                            && queued.len() < 16,
                        "session_followup",
                    )?;
                    queued.push((
                        event.text_field("command")?.into(),
                        event.text_field("text")?.into(),
                    ));
                }
                "steered" | "steer_rejected" => {
                    need(
                        !queued.is_empty()
                            && !matches!(stage, Stage::Completed | Stage::Empty)
                            && outstanding.is_none()
                            && !unknown_effect,
                        "session_followup_empty",
                    )?;
                    let commands = event.field("commands")?.elements()?;
                    need(
                        commands.len() == queued.len()
                            && commands
                                .iter()
                                .zip(&queued)
                                .all(|(id, (command, _))| id.text() == Ok(command.as_str())),
                        "session_followup_order",
                    )?;
                    if kind == "steer_rejected" {
                        queued.clear();
                        return Ok(());
                    }
                    let next = Request::load(event.field("next")?)?;
                    let previous = expected.as_ref().ok_or("session_followup_context")?;
                    need(
                        next.binding == previous.binding
                            && next.epoch == previous.epoch
                            && next.tools == previous.tools
                            && next.effort == previous.effort
                            && next.input.len() >= queued.len(),
                        "session_followup_context",
                    )?;
                    need(
                        canonical
                            .as_ref()
                            .and_then(|state| state.turn.as_ref())
                            .is_some_and(|turn| turn.next.input == next.input),
                        "session_followup_context",
                    )?;
                    expected = Some(next);
                    queued.clear();
                    response = None;
                    results.clear();
                    final_text = None;
                    stage = Stage::Generating;
                    paused = false;
                    summary.status = SummaryStatus::Interrupted;
                }
                "continued" => {
                    let admitted = response.as_ref().ok_or("session_continuation")?;
                    need(
                        stage == Stage::Tools
                            && !paused
                            && !unknown_effect
                            && outstanding.is_none()
                            && results.len() == admitted.calls().len()
                            && results.iter().all(|result| {
                                result.dispatched && result.result.effects() == Effects::KnownResult
                            }),
                        "session_continuation",
                    )?;
                    expected = Some(next_request(admitted, &results)?);
                    stage = Stage::Generating;
                    final_text = None;
                }
                "compacted" => {
                    need(
                        stage == Stage::Compacting && !paused,
                        "session_compaction_stage",
                    )?;
                    expected = Some(
                        canonical
                            .as_ref()
                            .and_then(|state| state.turn.as_ref())
                            .ok_or("session_compaction_request")?
                            .next
                            .clone(),
                    );
                    stage = Stage::Generating;
                }
                _ => return Err("session_record_kind"),
            }
            Ok(())
        },
    );
    stop.check()?;
    if result.is_err() || !initialized {
        return Ok(SessionSummary::unavailable(entry));
    }
    if outstanding.is_some() || unknown_effect {
        summary.status = SummaryStatus::Unknown;
    }
    if summary.status == SummaryStatus::Paused {
        summary.pause_diagnostic = canonical
            .as_ref()
            .and_then(|state| state.turn.as_ref())
            .filter(|turn| !turn.diagnostic.is_empty())
            .map(|turn| turn.diagnostic.clone());
    }
    Ok(summary)
}
