//! Inspect a helper without acquiring its journal writer or reopening its workspace.
use super::{Factory, Result};
use crate::{
    auth::Cancel,
    frontend::markdown::Document,
    provider::{
        contracts::GetTranscript,
        request::{Advertisement, Request},
        structured::FinalFormat,
    },
    session::{journal_v1::inspect_read_only, state::State},
};
use std::sync::Arc;

#[derive(Clone)]
pub(in crate::frontend) struct AgentArchive {
    timeline: Arc<crate::session::timeline::Timeline>,
    structured: bool,
}

pub(in crate::frontend) struct AgentTranscript {
    pub archive: AgentArchive,
    pub document: Document,
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
        stop: &Cancel,
    ) -> Result<AgentTranscript> {
        let selected = self.search_catalog()?.explicit(id)?;
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
        Ok(AgentArchive {
            timeline: state.timeline,
            structured,
        }
        .page(query))
    }
}

impl AgentArchive {
    pub(in crate::frontend) fn page(&self, query: GetTranscript) -> AgentTranscript {
        let page = self.timeline.page(query);
        let mut sections = Vec::new();
        for entry in &page.entries {
            if let Some(tool) = &entry.tool {
                if !tool.continued {
                    sections.push(format!(
                        "**{}** · {} · {}",
                        tool.name, tool.target, entry.state
                    ));
                }
                // Canonical timeline stores public arguments/output after the host prefix.
                let data = entry.text.get(tool.content_start..).unwrap_or_default();
                if !data.is_empty() {
                    sections.push(
                        data.lines()
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
                sections.push(format!("### {label}"));
                sections.push(entry.text.clone());
            }
        }
        if sections.is_empty() {
            sections.push("Waiting for the first transcript entry.".into());
        }
        AgentTranscript {
            archive: self.clone(),
            document: Document::sections(&sections),
            begin: page.begin,
            end: page.end,
            total: page.total,
        }
    }
}
