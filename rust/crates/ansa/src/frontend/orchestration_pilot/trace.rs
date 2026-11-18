//! Bounded diagnostic projection for the synthetic O4 runner.
use super::Result;
use crate::{
    config::AgentSettings,
    protocol::json::Json,
    provider::{
        contracts::valid_id,
        replay::{Effects, Execution},
        request::Request,
        responses::AdmittedResponse,
        structured::{ChildStatus, FinalFormat},
    },
    session::{
        codec_v1::{SavedResult, load_response},
        journal_v1::inspect_read_only,
    },
};
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_EVENTS: usize = 512;

#[derive(Default)]
pub(super) struct Trace(Mutex<Vec<Value>>);

impl Trace {
    pub(super) fn add(&self, event: Value) -> Result<()> {
        self.extend(std::iter::once(event))
    }

    pub(super) fn write(&self, report: &mut impl Write) -> Result<()> {
        let events = self.0.lock().map_err(|_| "pilot_trace_lock")?;
        writeln!(
            report,
            "{}",
            json!({"event":"pilot_trace","events":*events})
        )
        .map_err(|_| "pilot_trace_write")
    }

    fn extend(&self, incoming: impl ExactSizeIterator<Item = Value>) -> Result<()> {
        let mut events = self.0.lock().map_err(|_| "pilot_trace_lock")?;
        if incoming.len() > MAX_EVENTS - events.len() {
            return Err("pilot_trace_limit");
        }
        events.extend(incoming);
        Ok(())
    }

    pub(super) fn collect_journals(&self, settings: &AgentSettings) -> Result<()> {
        let catalog = settings.catalog("offline")?;
        let entries = catalog.entries()?;
        if entries.len() > 16 {
            return Err("pilot_trace_sessions");
        }
        let mut bytes = 32 * 1024 * 1024;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = Ok(());
        for entry in entries {
            let selected = catalog.explicit(&entry.id)?;
            let mut role = "unknown";
            let mut checked = Vec::new();
            let inspected = inspect_read_only(
                &selected.directory,
                &mut bytes,
                || {
                    if Instant::now() >= deadline {
                        return Err("pilot_trace_deadline");
                    }
                    Ok(())
                },
                |event| {
                    if event.text_field("kind")? == "init" {
                        role = phase(&Request::load(event.field("configuration")?)?);
                    }
                    if let Some(mut projection) = journal_event(event)? {
                        if checked.len() >= MAX_EVENTS {
                            return Err("pilot_trace_limit");
                        }
                        projection["phase"] = json!(role);
                        projection["session"] = json!(selected.id);
                        checked.push(projection);
                    }
                    Ok(())
                },
            );
            if let Err(code) = inspected {
                self.add(json!({"kind":"journal_error","session":selected.id,"code":code}))?;
                outcome = outcome.and(inspected);
            } else {
                self.extend(checked.into_iter())?;
            }
        }
        outcome
    }
}

pub(super) fn phase(request: &Request) -> &'static str {
    match request.final_format() {
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands) => "plan",
        Some(FinalFormat::ChildResult) => {
            if request
                .epoch
                .exact_state
                .field("orchestration_writes")
                .and_then(Json::elements)
                .is_ok_and(|paths| !paths.is_empty())
            {
                "writer"
            } else {
                "reviewer"
            }
        }
        None => "summary",
    }
}

pub(super) fn finish_reason(value: &Value) -> &'static str {
    match value["choices"][0]["finish_reason"].as_str() {
        Some("stop") => "stop",
        Some("tool_calls") => "tool_calls",
        Some("length") => "length",
        Some("content_filter") => "content_filter",
        Some("error") => "error",
        Some(_) => "other",
        None => "missing",
    }
}

pub(super) fn blocked_reason(response: &AdmittedResponse) -> Option<&'static str> {
    let result = response.child_result().ok()?;
    if result.status != ChildStatus::Blocked {
        return None;
    }
    // Only categories leave the synthetic workspace, even if the provider
    // includes a tool argument or instruction token in its explanation.
    let summary = result.summary.to_ascii_lowercase();
    Some(
        if summary.contains("instruction_token") || summary.contains("instructions_required") {
            "instructions"
        } else if summary.contains("permission")
            || summary.contains("scope")
            || summary.contains("read-only")
        {
            "permission_or_scope"
        } else if summary.contains("tool")
            || summary.contains("edit_file")
            || summary.contains("read_file")
        {
            "tool_unavailable"
        } else if summary.contains("evidence")
            || summary.contains("missing")
            || summary.contains("verify")
        {
            "missing_evidence"
        } else {
            "unspecified"
        },
    )
}

fn journal_event(event: &Json) -> Result<Option<Value>> {
    let projection = match event.text_field("kind")? {
        "response" => {
            let response = load_response(event.field("response")?)?;
            let child_status = if response.calls().is_empty()
                && response.request().final_format() == Some(FinalFormat::ChildResult)
            {
                Some(response.child_result()?.status)
            } else {
                None
            };
            json!({"kind":"response_committed","calls":response.calls().len(),"child_status":child_status})
        }
        "intent" => {
            let invocation = event.field("invocation")?;
            json!({"kind":"tool_admitted","tool":invocation.text_field("name")?,"operation":invocation.text_field("operation")?})
        }
        "outcome" => {
            let saved = SavedResult::load(event.field("result")?)?;
            let tool = &saved.result;
            let execution = match tool.execution() {
                Execution::Succeeded => "succeeded",
                Execution::Failed => "failed",
                Execution::Cancelled => "cancelled",
                Execution::Denied => "denied",
            };
            let effects = match tool.effects() {
                Effects::NotStarted => "not_started",
                Effects::KnownResult => "known_result",
                Effects::Unknown => "unknown",
            };
            let code = if tool.execution() == Execution::Succeeded {
                None
            } else {
                Json::parse_with_limits(tool.output(), 16 * 1024, 16, 1024)
                    .ok()
                    .and_then(|value| {
                        value
                            .text_field("error")
                            .ok()
                            .filter(|code| valid_id(code))
                            .map(str::to_owned)
                    })
            };
            json!({"kind":"tool_outcome","operation":tool.operation_id(),"execution":execution,"effects":effects,"dispatched":saved.dispatched,"code":code})
        }
        "pause" => json!({"kind":"paused","code":event.text_field("diagnostic")?}),
        "done" => json!({"kind":"done"}),
        "cancel" => json!({"kind":"cancelled"}),
        _ => return Ok(None),
    };
    Ok(Some(projection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal_v1::Journal;

    #[test]
    fn torn_journal_does_not_publish_its_prefix_as_verified() {
        let root = super::super::Sandbox::create().unwrap();
        let settings = crate::config::load_agent_settings(&root.0, &root.0).unwrap();
        let catalog = settings.catalog("offline").unwrap();
        let selected = catalog.create().unwrap();
        let mut journal = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
        journal
            .append(&Json::parse(r#"{"kind":"done"}"#).unwrap())
            .unwrap();
        drop(journal);
        let mut part = std::fs::OpenOptions::new()
            .append(true)
            .open(selected.directory.join("part-00000000000000000000.log"))
            .unwrap();
        part.write_all(b"torn").unwrap();
        drop(part);

        let trace = Trace::default();
        assert_eq!(trace.collect_journals(&settings), Err("journal_incomplete"));
        let events = trace.0.into_inner().unwrap();
        assert_eq!(
            events,
            [json!({"kind":"journal_error","session":selected.id,"code":"journal_incomplete"})]
        );
    }
}
