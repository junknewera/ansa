//! Read-only projection of acknowledged records, including public tool detail.
//! It never supplies provider context and never exposes encrypted reasoning.
use super::{
    Result, codec_v1,
    transcript::{Entry, Page, Transcript},
};
use crate::{
    protocol::json::Json,
    provider::contracts::{AgentMode, GetTranscript, RequestId, TurnId},
};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Timeline {
    entries: Transcript,
    turn: TurnId,
    request: RequestId,
    mode: AgentMode,
    final_text: String,
    names: BTreeMap<String, String>,
    purposes: BTreeMap<String, std::sync::Arc<str>>,
    pending: BTreeMap<String, String>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            entries: Transcript::default(),
            turn: "idle".into(),
            request: "idle".into(),
            mode: AgentMode::Basic,
            final_text: String::new(),
            names: BTreeMap::new(),
            purposes: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}
impl Timeline {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn page(&self, query: GetTranscript) -> Page {
        self.entries.page(query)
    }
    pub fn save(&self) -> Result<Json> {
        let mut entries = Vec::new();
        let mut offset = 0;
        let mut bytes = 0;
        while offset < self.len() {
            let page = self.page(GetTranscript {
                before: offset,
                limit: 64,
                forward: true,
            });
            for entry in page.entries {
                bytes += entry.text.len();
                if bytes > 8 * 1024 * 1024 || entries.len() >= 65536 {
                    return Err("fork_timeline_limit");
                }
                entries.push(Json::object(vec![
                    ("turn", Json::string(&entry.turn.0)?),
                    ("request", Json::string(&entry.request.0)?),
                    ("role", Json::string(&entry.role)?),
                    ("text", Json::string(&entry.text)?),
                    ("state", Json::string(&entry.state)?),
                    (
                        "mode",
                        Json::string(if entry.mode == AgentMode::Plan {
                            "plan"
                        } else {
                            "basic"
                        })?,
                    ),
                ])?);
            }
            if page.end <= offset {
                return Err("fork_timeline_page");
            }
            offset = page.end;
        }
        Json::array(entries)
    }
    pub fn load(value: &Json) -> Result<Self> {
        use crate::provider::contracts::valid_id;
        let entries = value.elements()?;
        if entries.len() > 65536 {
            return Err("fork_timeline_limit");
        }
        let mut result = Self::default();
        let mut bytes = 0;
        for entry in entries {
            let turn = entry.text_field("turn")?;
            let request = entry.text_field("request")?;
            let role = entry.text_field("role")?;
            let text = entry.text_field("text")?;
            let state = entry.text_field("state")?;
            bytes += text.len();
            if !valid_id(turn)
                || !valid_id(request)
                || !matches!(
                    role,
                    "user" | "assistant" | "tool" | "tool_result" | "reasoning" | "notice"
                )
                || text.len() > 128 * 1024
                || bytes > 8 * 1024 * 1024
                || state.len() > 64
            {
                return Err("fork_timeline_entry");
            }
            let mode = match entry.text_field("mode")? {
                "basic" => AgentMode::Basic,
                "plan" => AgentMode::Plan,
                _ => return Err("session_mode"),
            };
            result.entries.append(Entry {
                turn: turn.into(),
                request: request.into(),
                role: role.into(),
                text: text.into(),
                state: state.into(),
                mode,
            });
        }
        Ok(result)
    }
    fn append(&mut self, role: &str, text: String, state: &str) {
        // Arguments and summaries may exceed a page entry, and tool output
        // may fill its whole native limit before the display prefix is added.
        // Split the projection losslessly; provider-native history is untouched.
        let mut remaining = text.as_str();
        loop {
            let mut end = remaining.len().min(128 * 1024);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            self.entries.append(Entry {
                turn: self.turn.clone(),
                request: self.request.clone(),
                role: role.into(),
                text: remaining[..end].into(),
                state: state.into(),
                mode: self.mode,
            });
            remaining = &remaining[end..];
            if remaining.is_empty() {
                break;
            }
        }
    }
    /// Called only after the canonical reducer accepts this record.
    pub fn observe(&mut self, event: &Json) -> Result<()> {
        match event.text_field("kind")? {
            "queued" => {
                self.pending.insert(
                    event.text_field("command")?.into(),
                    event.text_field("text")?.into(),
                );
            }
            "steered" => {
                for command in event.field("commands")?.elements()? {
                    if let Some(text) = self.pending.remove(command.text()?) {
                        self.append("user", text, "delivered");
                    }
                }
                self.final_text.clear();
            }
            "accepted" => {
                self.turn = event.text_field("turn")?.into();
                self.request = event.text_field("request")?.into();
                self.mode = if event.has("mode") && event.text_field("mode")? == "plan" {
                    AgentMode::Plan
                } else {
                    AgentMode::Basic
                };
                self.final_text.clear();
                self.names.clear();
                self.purposes.clear();
                self.append("user", event.text_field("text")?.into(), "accepted");
            }
            "response" => {
                let response = codec_v1::load_response(event.field("response")?)?;
                self.purposes = response
                    .request()
                    .tools
                    .descriptors
                    .iter()
                    .map(|tool| {
                        (
                            tool.name.clone(),
                            std::sync::Arc::from(tool.description.as_str()),
                        )
                    })
                    .collect();
                self.final_text = codec_v1::response_text(&response)?;
                for item in response.items() {
                    let item = item.value();
                    if item.text_field("type")? == "reasoning" && item.has("summary") {
                        let mut text = String::new();
                        for part in item.field("summary")?.elements()? {
                            if part.text_field("type")? == "summary_text" {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(part.text_field("text")?);
                            }
                        }
                        if !text.is_empty() {
                            self.append("reasoning", text, "confirmed");
                        }
                    }
                }
            }
            "intent" => {
                let invocation = codec_v1::Invocation::load(event.field("invocation")?)?;
                self.names
                    .insert(invocation.operation.clone(), invocation.name.clone());
                self.append(
                    "tool",
                    format!(
                        "{} (prepared)\n{}\n{}\n{}",
                        invocation.name,
                        self.purposes
                            .get(&invocation.name)
                            .map_or("", |text| text.as_ref()),
                        invocation.operation,
                        invocation.arguments.dump()
                    ),
                    "intent",
                );
            }
            "outcome" => {
                let result = codec_v1::SavedResult::load(event.field("result")?)?;
                let name = self
                    .names
                    .get(result.result.operation_id())
                    .map(String::as_str)
                    .unwrap_or("tool");
                self.append(
                    "tool_result",
                    format!(
                        "{name} {:?} {:?}\n{}",
                        result.result.execution(),
                        result.result.effects(),
                        result.result.output()
                    ),
                    "finished",
                );
            }
            "done" => {
                let text = std::mem::take(&mut self.final_text);
                self.append("assistant", text, "completed");
            }
            "cancel" => self.append("notice", "Current step interrupted".into(), "cancelled"),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        frontend::app,
        provider::request::{Epoch, Request},
    };
    #[test]
    fn prepared_tool_purpose_comes_from_admitted_descriptor() {
        let tools = app::pure_tools().unwrap();
        let request = Request::new(
            app::binding(),
            Epoch {
                id: "epoch".into(),
                instructions: "fixture".into(),
                exact_state: Json::parse("{}").unwrap(),
            },
            tools.advertisement().clone(),
            vec![],
        )
        .unwrap();
        let response =
            crate::frontend::offline::generate(&request, &crate::auth::Cancel::default()).unwrap();
        let call = &response.calls()[0];
        let invocation = codec_v1::Invocation {
            operation: "run:op:1".into(),
            call: call.call_id.clone(),
            name: call.name.clone(),
            revision: "1".into(),
            arguments: Json::parse(&call.arguments).unwrap(),
        };
        let mut timeline = Timeline::default();
        timeline
            .observe(
                &Json::object(vec![
                    ("kind", Json::string("response").unwrap()),
                    ("response", codec_v1::save_response(&response).unwrap()),
                ])
                .unwrap(),
            )
            .unwrap();
        timeline
            .observe(
                &Json::object(vec![
                    ("kind", Json::string("intent").unwrap()),
                    ("invocation", invocation.save().unwrap()),
                ])
                .unwrap(),
            )
            .unwrap();
        let page = timeline.page(GetTranscript {
            before: 0,
            limit: 64,
            forward: false,
        });
        let tool = page.entries.iter().find(|e| e.role == "tool").unwrap();
        assert_eq!(tool.state, "intent");
        assert!(
            tool.text
                .contains(&tools.advertisement().descriptors[0].description)
        );
        assert!(tool.text.ends_with("{}"));
    }
    #[test]
    fn large_tool_and_summary_projection_roundtrips_without_oversized_pages() {
        let mut timeline = Timeline::default();
        let text = format!(
            "run_command succeeded known_result\n{}",
            "界".repeat(100_000)
        );
        timeline.append("tool_result", text.clone(), "finished");
        let restored = Timeline::load(&timeline.save().unwrap()).unwrap();
        let mut seen = String::new();
        let mut offset = 0;
        while offset < restored.len() {
            let page = restored.page(GetTranscript {
                before: offset,
                limit: 64,
                forward: true,
            });
            assert!(page.end > offset);
            assert!(page.entries.iter().map(|e| e.text.len()).sum::<usize>() <= 256 * 1024);
            for entry in page.entries {
                seen.push_str(&entry.text);
            }
            offset = page.end;
        }
        assert_eq!(seen, text);
    }
    #[test]
    fn projection_keeps_public_summary_and_never_serializes_opaque_reasoning() {
        let configuration = Request::new(
            app::binding(),
            Epoch {
                id: "epoch".into(),
                instructions: "fixture".into(),
                exact_state: Json::parse("{}").unwrap(),
            },
            app::pure_tools().unwrap().advertisement().clone(),
            vec![],
        )
        .unwrap();
        let saved = Json::object(vec![
            ("request", configuration.save().unwrap()),
            ("id", Json::string("response").unwrap()),
            ("model", Json::string(&configuration.binding.returned_model).unwrap()),
            ("items", Json::parse(r#"[{"type":"reasoning","id":"reasoning","summary":[{"type":"summary_text","text":"Check cancellation before dispatch"}],"encrypted_content":"OPAQUE_MUST_NEVER_APPEAR_IN_TIMELINE"}]"#).unwrap()),
        ]).unwrap();
        let mut timeline = Timeline::default();
        timeline.observe(&Json::parse(r#"{"kind":"accepted","turn":"turn","request":"request","text":"hello","mode":"basic"}"#).unwrap()).unwrap();
        timeline
            .observe(
                &Json::object(vec![
                    ("kind", Json::string("response").unwrap()),
                    ("response", saved),
                ])
                .unwrap(),
            )
            .unwrap();
        let encoded = timeline.save().unwrap();
        assert!(
            encoded
                .dump()
                .contains("Check cancellation before dispatch")
        );
        assert!(
            !encoded
                .dump()
                .contains("OPAQUE_MUST_NEVER_APPEAR_IN_TIMELINE")
        );
        let restored = Timeline::load(&encoded).unwrap();
        let page = restored.page(GetTranscript {
            before: 0,
            limit: 64,
            forward: false,
        });
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[1].role, "reasoning");
    }
}
