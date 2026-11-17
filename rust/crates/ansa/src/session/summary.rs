//! Untrusted presentation derived from read-only canonical journal inspection.
//! These observations never authorize Resume, provider calls or tool dispatch.
use super::{
    Result,
    catalog::CatalogEntry,
    codec_v1::{Invocation, SavedResult, load_response, next_request, response_text},
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
            Self::Empty => "Новая",
            Self::Completed => "Завершена",
            Self::Cancelled => "Остановлена",
            Self::Paused => "Приостановлена",
            Self::Interrupted => "Прервана",
            Self::Unknown => "Неизвестно",
            Self::Unavailable => "Недоступна",
        }
    }
}
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub entry: CatalogEntry,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub status: SummaryStatus,
}
impl SessionSummary {
    fn unavailable(entry: CatalogEntry) -> Self {
        Self {
            entry,
            title: None,
            preview: None,
            status: SummaryStatus::Unavailable,
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
) -> Result<SessionSummary> {
    let mut summary = SessionSummary {
        entry: entry.clone(),
        title: None,
        preview: None,
        status: SummaryStatus::Empty,
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
    let mut accepted_text = String::new();
    let mut plan = false;
    let mut compactions = 0;
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
                let version = event.usize_field("version")?;
                need(
                    version == 1 && !event.has("fork_seed")
                        || version == 2 && event.has("fork_seed"),
                    "session_fork_seed",
                )?;
                let saved = Request::load(event.field("configuration")?)?;
                // Replay through the production reducer before presenting a state.
                // Inspection has no compiled read-only tool policy, so plan turns
                // remain unavailable instead of trusting their saved advertisement.
                let mut state = State::new(
                    entry.id.as_str().into(),
                    saved.clone(),
                    Advertisement {
                        revision: String::new(),
                        descriptors: vec![],
                    },
                );
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
                need(
                    !text.is_empty()
                        && text.len() <= 16 * 1024
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
                let next = Request::load(event.field("next")?)?;
                let saved = configuration.as_ref().ok_or("session_missing_init")?;
                need(
                    next.binding.provider == saved.binding.provider
                        && next.binding.account == saved.binding.account,
                    "session_request_conflict",
                )?;
                need(
                    next.input.last().is_some_and(|item| {
                        item.text_field("role") == Ok("user")
                            && item.text_field("content") == Ok(text)
                    }),
                    "session_request_conflict",
                )?;
                stage = if event.field("compact")?.boolean()? {
                    Stage::Compacting
                } else {
                    Stage::Generating
                };
                expected = Some(next);
                response = None;
                results.clear();
                request_id = event.text_field("request")?.into();
                accepted_text = text.into();
                plan = event.has("mode") && event.text_field("mode")? == "plan";
                paused = false;
                if summary.title.is_none() {
                    summary.title = Some(excerpt(text, 96));
                }
                summary.preview = Some(excerpt(text, 192));
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
                        Some(response_text(&admitted)?)
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
                            && event.text_field("text")?.len() <= 16 * 1024
                            && queued.len() < 16,
                        "session_followup",
                    )?;
                    queued.push((
                        event.text_field("command")?.into(),
                        event.text_field("text")?.into(),
                    ));
                }
                "steered" => {
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
                    let start = next.input.len() - queued.len();
                    need(
                        next.input[start..]
                            .iter()
                            .zip(&queued)
                            .all(|(item, (_, text))| {
                                item.text_field("role") == Ok("user")
                                    && item.text_field("content") == Ok(text.as_str())
                            }),
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
                    let value = event.field("compaction")?;
                    let compacted = crate::context::legacy_compact::CompactedContext::admit(
                        Request::load(value.field("request")?)?,
                        value.field("native")?.clone(),
                    )?;
                    let previous = expected.as_ref().ok_or("session_compaction_request")?;
                    let saved = configuration.as_ref().ok_or("session_missing_init")?;
                    let original = Request::new(
                        previous.binding.clone(),
                        previous.epoch.clone(),
                        saved.tools.clone(),
                        previous.input[..previous.input.len() - 1].to_vec(),
                    )?
                    .with_effort(previous.effort.clone())?;
                    need(
                        compacted.request() == &original,
                        "session_compaction_request",
                    )?;
                    compactions += 1;
                    let mut epoch = crate::context::epoch::compact_epoch(
                        &saved.epoch,
                        compactions,
                        &compacted,
                    )?;
                    if plan {
                        let provenance = Json::object(vec![
                            ("base_epoch", Json::string(&epoch.id)?),
                            ("base_state", epoch.exact_state.clone()),
                            ("base_instructions", Json::string(&epoch.instructions)?),
                            ("mode", Json::string("plan")?),
                            ("tools_revision", Json::string(&previous.tools.revision)?),
                        ])?;
                        epoch = crate::provider::request::Epoch {
                            id: crate::protocol::legacy_json_v1::sha256(
                                provenance.dump().as_bytes(),
                            ),
                            instructions: format!(
                                "{}\nPlan mode: inspect and reason using read-only tools. Do not modify files or execute commands. Provide a concrete plan.",
                                epoch.instructions
                            ),
                            exact_state: provenance,
                        };
                    }
                    let mut input = compacted.output()?.to_vec();
                    input.push(Json::object(vec![
                        ("role", Json::string("user")?),
                        ("content", Json::string(&accepted_text)?),
                    ])?);
                    expected = Some(
                        Request::new(
                            previous.binding.clone(),
                            epoch,
                            previous.tools.clone(),
                            input,
                        )?
                        .with_effort(previous.effort.clone())?,
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
    Ok(summary)
}
