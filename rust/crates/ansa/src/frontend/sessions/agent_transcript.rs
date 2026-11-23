//! Inspect a helper without acquiring its journal writer or reopening its workspace.
use super::{Factory, Result};
use crate::{
    auth::Cancel,
    frontend::markdown::Document,
    provider::{
        contracts::GetTranscript,
        replay::{Effects, Execution},
        request::{Advertisement, Request},
        structured::FinalFormat,
    },
    session::{journal_v1::inspect_read_only, state::State, transcript::ToolPhase},
};
use std::sync::Arc;

#[derive(Clone)]
pub(in crate::frontend) struct AgentArchive {
    tail: (String, ansa_linux::journal::ReadIdentity),
    timeline: Arc<crate::session::timeline::Timeline>,
    structured: bool,
}

pub(in crate::frontend) struct AgentTranscript {
    pub archive: AgentArchive,
    pub document: Document,
    pub details: Document,
    pub begin: usize,
    pub end: usize,
    pub total: usize,
}

impl Factory {
    pub(in crate::frontend) fn agent_transcript(
        &self,
        id: &str,
        parent: &str,
        query: GetTranscript,
        previous: Option<&AgentArchive>,
        stop: &Cancel,
    ) -> Result<Option<AgentTranscript>> {
        use ansa_linux::journal::{ReadIdentity, ReadOnly};
        let selected = self.search_catalog()?.explicit(id)?;
        let journal = ReadOnly::open(&selected.directory).map_err(|_| "journal_directory_open")?;
        let parts = journal.names().map_err(|_| "journal_parts_limit")?;
        let name = parts
            .last()
            .and_then(|name| name.to_str())
            .ok_or("session_missing_init")?;
        let part = journal.part(name).map_err(|_| "journal_part_open")?;
        let tail = (
            name.to_owned(),
            ReadIdentity::capture(&part).map_err(|_| "journal_part_metadata")?,
        );
        if previous.is_some_and(|archive| archive.tail == tail) {
            return Ok(None);
        }
        let compiled = crate::tools::files::summary_catalog(&self.settings)?;
        let mut state = None;
        let mut structured = false;
        inspect_read_only(
            &selected.directory,
            &mut (64 * 1024 * 1024),
            || stop.check(),
            |event| {
                if state.is_none() {
                    if event.text_field("kind")? != "init" || event.text_field("session")? != id {
                        return Err("session_missing_init");
                    }
                    let request = Request::load(event.field("configuration")?)?;
                    self.check_orchestration_context(&request)?;
                    structured = request.final_format() == Some(FinalFormat::ChildResult);
                    let source = if structured {
                        request
                            .epoch
                            .exact_state
                            .text_field("orchestration_parent")?
                    } else {
                        event.field("fork_seed")?.text_field("source_session")?
                    };
                    if source != parent || id == parent {
                        return Err("session_workspace_mismatch");
                    }
                    let tools =
                        compiled
                            .verified_plan_tools(&request.tools)
                            .unwrap_or(Advertisement {
                                revision: String::new(),
                                descriptors: vec![],
                            });
                    state = Some(State::new(id.into(), request, tools));
                }
                state.as_mut().ok_or("session_missing_init")?.apply(event)
            },
        )?;
        let state = state.ok_or("session_missing_init")?;
        Ok(Some(
            AgentArchive {
                timeline: state.timeline,
                structured,
                tail,
            }
            .page(query),
        ))
    }
}

impl AgentArchive {
    pub(in crate::frontend) fn page(&self, query: GetTranscript) -> AgentTranscript {
        let page = self.timeline.page(query);
        let mut sections = Vec::new();
        let mut details = Vec::new();
        let settled: std::collections::BTreeSet<_> = page
            .entries
            .iter()
            .filter_map(|entry| entry.tool.as_ref())
            .filter(|tool| matches!(tool.phase, ToolPhase::Finished { .. }))
            .map(|tool| tool.operation.as_str())
            .collect();
        let mut rows = page.entries.iter().peekable();
        while let Some(entry) = rows.next() {
            if let Some(tool) = &entry.tool {
                let state = tool_state(tool.phase);
                let target = tool_target(&tool.target);
                if !(tool.phase == ToolPhase::Prepared && settled.contains(tool.operation.as_str()))
                {
                    let (action, show_target) = match tool.name.as_str() {
                        "read_file" => ("Read", true),
                        "list_directory" => ("List", true),
                        "search_files" => ("Search", true),
                        "edit_file" => ("Edit", true),
                        "git_snapshot" => ("Git snapshot", false),
                        "run_command" => ("Command", false),
                        name => (name, false),
                    };
                    let shown = if show_target { target.as_str() } else { "" };
                    sections.push(format!("**{}**{shown} · {state}", literal(action)));
                }
                details.push(format!("**{}**{target} · {state}", literal(&tool.name)));
                let mut data = entry
                    .text
                    .get(tool.content_start..)
                    .unwrap_or_default()
                    .to_owned();
                let mut complete = tool.last_chunk;
                while !complete
                    && let Some(row) = rows.next_if(|row| {
                        row.request == entry.request
                            && row.tool.as_ref().is_some_and(|chunk| {
                                chunk.continued
                                    && chunk.operation == tool.operation
                                    && chunk.phase == tool.phase
                            })
                    })
                {
                    let chunk = row.tool.as_ref().expect("matching tool chunk");
                    data.push_str(row.text.get(chunk.content_start..).unwrap_or_default());
                    complete = chunk.last_chunk;
                }
                let decoded = if tool.continued || !complete {
                    format!(
                        "Partial tool output on this page\n{}",
                        crate::frontend::workspace::display_text(&data)
                    )
                } else {
                    super::super::tool_ui::transcript_payload(&data)
                };
                if !decoded.is_empty() {
                    details.push(
                        decoded
                            .lines()
                            .map(|line| format!("    {line}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            } else {
                let label = match entry.role.as_str() {
                    "assistant" => "Agent",
                    "user" if self.structured => "Assignment",
                    "user" => "Request",
                    "reasoning" => "Reasoning summary",
                    "notice" => "Status",
                    _ => continue,
                };
                details.push(format!("### {label}"));
                details.push(entry.text.clone());
                sections.push(format!("### {label}"));
                if self.structured
                    && entry.role == "user"
                    && let Some((prompt, _)) = entry.text.rsplit_once("\n\nШаг ")
                {
                    let task = if let Some(assignment) = prompt.strip_prefix("Assigned task ") {
                        assignment
                            .lines()
                            .skip(1)
                            .take_while(|line| !line.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else if prompt.starts_with("Evaluate the worker") {
                        "Verify the worker's result against the approved task.".into()
                    } else {
                        crate::orchestration::plan::assignment_title(&entry.text)
                    };
                    sections.push(task);
                } else {
                    sections.push(entry.text.clone());
                }
            }
        }
        if sections.is_empty() {
            sections.push("Waiting for the first transcript entry.".into());
        }
        if details.is_empty() {
            details.clone_from(&sections);
        }
        AgentTranscript {
            archive: self.clone(),
            document: Document::sections(&sections),
            details: Document::sections(&details),
            begin: page.begin,
            end: page.end,
            total: page.total,
        }
    }
}

fn tool_state(phase: ToolPhase) -> &'static str {
    match phase {
        ToolPhase::Prepared => "Prepared",
        ToolPhase::Finished {
            effects: Effects::Unknown,
            ..
        } => "Unknown effect",
        ToolPhase::Finished {
            execution: Execution::Succeeded,
            ..
        } => "Done",
        ToolPhase::Finished {
            execution: Execution::Cancelled,
            ..
        } => "Cancelled",
        ToolPhase::Finished {
            execution: Execution::Denied,
            ..
        } => "Denied",
        ToolPhase::Finished {
            execution: Execution::Failed,
            ..
        } => "Failed",
    }
}

fn tool_target(target: &str) -> String {
    if target.is_empty() {
        String::new()
    } else {
        format!(" · {}", literal(target))
    }
}

fn literal(text: &str) -> String {
    let mut literal = String::new();
    for ch in crate::frontend::workspace::display_text(text).chars() {
        if ch.is_ascii_punctuation() {
            literal.push('\\');
        }
        literal.push(if ch.is_whitespace() && ch != ' ' {
            ' '
        } else {
            ch
        });
    }
    literal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::json::Json, session::timeline::Timeline};

    #[test]
    fn detail_pages_decode_complete_payloads_and_only_include_their_own_chunks() {
        let rows = [
            (r#"{"content":"first\n"#, false, false),
            (r#"second"}"#, true, true),
        ]
        .map(|(text, continued, last_chunk)| {
            serde_json::json!({
                "turn": "t", "request": "r", "mode": "basic", "state": "finished",
                "role": "tool_result", "text": text, "tool": {
                    "operation": "op", "call": "c", "name": "read_file", "purpose": "",
                    "target": "file", "execution": 0, "effects": 1, "content_start": 0,
                    "continued": continued, "last_chunk": last_chunk
                }
            })
        });
        let timeline =
            Timeline::load(&Json::parse(&serde_json::to_string(&rows).unwrap()).unwrap()).unwrap();
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let archive = AgentArchive {
            tail: (
                "test".into(),
                ansa_linux::journal::ReadIdentity::capture(&file).unwrap(),
            ),
            timeline: Arc::new(timeline),
            structured: false,
        };
        let details = |before, limit| {
            archive
                .page(GetTranscript {
                    before,
                    limit,
                    forward: true,
                })
                .details
                .rows(100)
                .into_iter()
                .map(|row| row.plain())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let full = details(0, 2);
        assert!(full.contains("first\n") && full.contains("second") && !full.contains(r"\n"));
        let first = details(0, 1);
        let second = details(1, 1);
        assert!(
            first.contains("Partial tool output")
                && first.contains("first")
                && !first.contains("second")
        );
        assert!(
            second.contains("Partial tool output")
                && second.contains("second")
                && !second.contains("first")
        );
    }
}
