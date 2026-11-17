//! Read-only projection of acknowledged records, including public tool detail.
//! It never supplies provider context and never exposes encrypted reasoning.
use super::{
    Result, codec_v1,
    transcript::{Entry, Page, ToolDetail, ToolPhase, Transcript},
};
use crate::{
    protocol::json::Json,
    provider::contracts::{AgentMode, GetTranscript, RequestId, TurnId},
};
use std::collections::BTreeMap;

impl ToolDetail {
    fn save(&self) -> Result<Json> {
        let (execution, effects) = match self.phase {
            ToolPhase::Prepared => (4, 0),
            ToolPhase::Finished { execution, effects } => (execution as usize, effects as usize),
        };
        Json::object(vec![
            ("operation", Json::string(&self.operation)?),
            ("name", Json::string(&self.name)?),
            ("call", Json::string(&self.call)?),
            ("purpose", Json::string(&self.purpose)?),
            ("target", Json::string(&self.target)?),
            ("execution", super::num(execution)?),
            ("effects", super::num(effects)?),
            ("content_start", super::num(self.content_start)?),
            (
                "continued",
                Json::parse(if self.continued { "true" } else { "false" })?,
            ),
            (
                "last_chunk",
                Json::parse(if self.last_chunk { "true" } else { "false" })?,
            ),
        ])
    }
    fn load(value: &Json, text: &str) -> Result<Self> {
        use crate::provider::replay::{Effects, Execution};
        let operation = value.text_field("operation")?;
        let name = value.text_field("name")?;
        let call = value.text_field("call")?;
        let purpose = value.text_field("purpose")?;
        let target = value.text_field("target")?;
        let content_start = value.usize_field("content_start")?;
        if !text.is_char_boundary(content_start) {
            return Err("fork_timeline_tool");
        }
        let effects = match value.usize_field("effects")? {
            0 => Effects::NotStarted,
            1 => Effects::KnownResult,
            2 => Effects::Unknown,
            _ => return Err("fork_timeline_tool"),
        };
        let phase = match value.usize_field("execution")? {
            4 if effects == Effects::NotStarted => ToolPhase::Prepared,
            n => ToolPhase::Finished {
                execution: match n {
                    0 => Execution::Succeeded,
                    1 => Execution::Failed,
                    2 => Execution::Cancelled,
                    3 => Execution::Denied,
                    _ => return Err("fork_timeline_tool"),
                },
                effects,
            },
        };
        let result = Self {
            operation: operation.into(),
            name: name.into(),
            call: call.into(),
            purpose: purpose.into(),
            target: target.into(),
            phase,
            content_start,
            continued: value.field("continued")?.boolean()?,
            last_chunk: value.field("last_chunk")?.boolean()?,
        };
        if !result.display_valid() || result.display_bytes() + text.len() > 256 * 1024 {
            return Err("fork_timeline_tool_limit");
        }
        Ok(result)
    }
}

#[derive(Clone, Debug)]
pub struct Timeline {
    entries: Transcript,
    turn: TurnId,
    request: RequestId,
    mode: AgentMode,
    final_text: String,
    names: BTreeMap<String, ToolDetail>,
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
                let mut fields = vec![
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
                ];
                if let Some(tool) = &entry.tool {
                    fields.push(("tool", tool.save()?));
                    bytes += tool.display_bytes();
                    if bytes > 8 * 1024 * 1024 {
                        return Err("fork_timeline_limit");
                    }
                }
                entries.push(Json::object(fields)?);
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
            let tool = if entry.has("tool") {
                let tool = ToolDetail::load(entry.field("tool")?, text)?;
                if !matches!(role, "tool" | "tool_result")
                    || (role == "tool") != matches!(tool.phase, ToolPhase::Prepared)
                {
                    return Err("fork_timeline_tool");
                }
                bytes += tool.display_bytes();
                if bytes > 8 * 1024 * 1024 {
                    return Err("fork_timeline_limit");
                }
                Some(std::sync::Arc::new(tool))
            } else {
                None
            };
            result.entries.append(Entry {
                turn: turn.into(),
                request: request.into(),
                role: role.into(),
                text: text.into(),
                state: state.into(),
                mode,
                tool,
            });
        }
        Ok(result)
    }
    fn append(&mut self, role: &str, text: String, state: &str) {
        self.append_tool(role, text, state, None);
    }
    fn append_tool(&mut self, role: &str, text: String, state: &str, tool: Option<ToolDetail>) {
        // An oversized custom descriptor keeps its lossless plain projection.
        // Typed display metadata must fit alongside at least one UTF-8 scalar.
        let tool = tool.filter(ToolDetail::display_valid);
        let chunk_limit =
            (128 * 1024).min(256 * 1024 - tool.as_ref().map_or(0, ToolDetail::display_bytes));
        // Arguments and summaries may exceed a page entry, and tool output
        // may fill its whole native limit before the display prefix is added.
        // Split the projection losslessly; provider-native history is untouched.
        let mut remaining = text.as_str();
        let mut offset = 0;
        loop {
            let mut end = remaining.len().min(chunk_limit);
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
                tool: tool.as_ref().map(|tool| {
                    let mut detail = tool.clone();
                    detail.content_start = detail.content_start.saturating_sub(offset).min(end);
                    detail.continued = offset > 0;
                    detail.last_chunk = offset + end == text.len();
                    std::sync::Arc::new(detail)
                }),
            });
            remaining = &remaining[end..];
            offset += end;
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
                let purpose = self
                    .purposes
                    .get(&invocation.name)
                    .cloned()
                    .unwrap_or_default();
                let prefix = format!(
                    "{} (prepared)\n{}\n{}\n",
                    invocation.name, purpose, invocation.operation
                );
                let mut target = ["command", "path", "query"]
                    .iter()
                    .filter_map(|field| invocation.arguments.text_field(field).ok())
                    .map(|value| value.chars().take(160).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" · ");
                if target.is_empty()
                    && let Ok(argv) = invocation.arguments.field("argv").and_then(Json::elements)
                {
                    target = argv
                        .iter()
                        .take(16)
                        .filter_map(|arg| arg.text().ok())
                        .map(|arg| {
                            let arg: String = arg.chars().take(160).collect();
                            if arg.chars().any(char::is_whitespace) {
                                Json::string(&arg).map_or(arg.clone(), |value| value.dump())
                            } else {
                                arg
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if let Ok(cwd) = invocation.arguments.text_field("cwd") {
                        target.push_str(" · ");
                        target.extend(cwd.chars().take(80));
                    }
                }
                let target: String = target.chars().take(480).collect();
                let detail = ToolDetail {
                    operation: invocation.operation.clone(),
                    name: invocation.name,
                    call: invocation.call,
                    purpose,
                    target,
                    phase: ToolPhase::Prepared,
                    content_start: prefix.len(),
                    continued: false,
                    last_chunk: true,
                };
                self.names.insert(invocation.operation, detail.clone());
                self.append_tool(
                    "tool",
                    format!("{prefix}{}", invocation.arguments.dump()),
                    "intent",
                    Some(detail),
                );
            }
            "outcome" => {
                let result = codec_v1::SavedResult::load(event.field("result")?)?;
                let mut detail = self
                    .names
                    .get(result.result.operation_id())
                    .cloned()
                    .unwrap_or_else(|| ToolDetail {
                        operation: result.result.operation_id().into(),
                        name: "tool".into(),
                        call: result.result.call_id().into(),
                        purpose: "".into(),
                        target: String::new(),
                        phase: ToolPhase::Prepared,
                        content_start: 0,
                        continued: false,
                        last_chunk: true,
                    });
                let prefix = format!(
                    "{} {:?} {:?}\n",
                    detail.name,
                    result.result.execution(),
                    result.result.effects()
                );
                detail.phase = ToolPhase::Finished {
                    execution: result.result.execution(),
                    effects: result.result.effects(),
                };
                detail.content_start = prefix.len();
                self.append_tool(
                    "tool_result",
                    format!("{prefix}{}", result.result.output()),
                    "finished",
                    Some(detail),
                );
            }
            "done" => {
                let text = std::mem::take(&mut self.final_text);
                self.append("assistant", text, "completed");
            }
            "cancel" => self.append("notice", "Текущий шаг остановлен".into(), "cancelled"),
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
    fn command_target_uses_the_production_argv_and_cwd_contract() {
        let mut timeline = Timeline::default();
        timeline.observe(&Json::parse(r#"{"kind":"intent","invocation":{"operation":"run:op:1","call":"call","name":"run_command","revision":"1","arguments":{"argv":["sh","-c","printf hello"],"cwd":"src"}}}"#).unwrap()).unwrap();
        let page = timeline.page(GetTranscript {
            before: 0,
            limit: 1,
            forward: true,
        });
        let target = &page.entries[0].tool.as_ref().unwrap().target;
        assert!(
            target.contains("sh -c \"printf hello\"") && target.contains("src"),
            "{target}"
        );
    }
    #[test]
    fn tool_metadata_roundtrips_chunked_unicode_and_accepts_legacy_fork_rows() {
        let mut timeline = Timeline::default();
        let payload = "界".repeat(100_000);
        let detail = ToolDetail {
            operation: "run:op:1".into(),
            name: "read_file".into(),
            call: "call".into(),
            purpose: "Read file purpose".into(),
            target: "src/界.rs".into(),
            phase: ToolPhase::Prepared,
            content_start: 4,
            continued: false,
            last_chunk: true,
        };
        timeline.append_tool("tool", format!("raw\n{payload}"), "intent", Some(detail));
        let restored = Timeline::load(&timeline.save().unwrap()).unwrap();
        let mut offset = 0;
        let mut seen = String::new();
        while offset < restored.len() {
            let page = restored.page(GetTranscript {
                before: offset,
                limit: 64,
                forward: true,
            });
            assert!(
                page.entries
                    .iter()
                    .map(|entry| entry.text.len() + entry.tool.as_ref().unwrap().display_bytes())
                    .sum::<usize>()
                    <= 256 * 1024
            );
            for (index, entry) in page.entries.iter().enumerate() {
                let tool = entry.tool.as_ref().unwrap();
                assert_eq!(tool.continued, offset + index > 0);
                assert_eq!(tool.operation, "run:op:1");
                seen.push_str(&entry.text[tool.content_start..]);
            }
            offset = page.end;
        }
        assert_eq!(seen, payload);
        let old = Json::parse(r#"[{"turn":"idle","request":"idle","role":"tool","text":"legacy args","state":"intent","mode":"basic"}]"#).unwrap();
        let old = Timeline::load(&old).unwrap();
        assert!(
            old.page(GetTranscript {
                before: 0,
                limit: 1,
                forward: true
            })
            .entries[0]
                .tool
                .is_none()
        );
        let detail = timeline
            .page(GetTranscript {
                before: 0,
                limit: 1,
                forward: true,
            })
            .entries[0]
            .tool
            .clone()
            .unwrap();
        let bad = Json::parse(
            &detail
                .save()
                .unwrap()
                .dump()
                .replace("\"content_start\":4", "\"content_start\":999999"),
        )
        .unwrap();
        assert!(ToolDetail::load(&bad, "raw\n{}").is_err());
        for long_purpose in [false, true] {
            let mut metadata = (*detail).clone();
            if long_purpose {
                metadata.purpose = "p".repeat(160 * 1024).into();
            } else {
                metadata.call = "c".repeat(5000);
            }
            let mut plain = Timeline::default();
            plain.append_tool(
                "tool",
                "tool (prepared)\nraw args".into(),
                "intent",
                Some(metadata),
            );
            let restored = Timeline::load(&plain.save().unwrap()).unwrap();
            let page = restored.page(GetTranscript {
                before: 0,
                limit: 64,
                forward: true,
            });
            assert!(page.entries.iter().all(|entry| entry.tool.is_none()));
            assert_eq!(
                page.entries
                    .iter()
                    .map(|entry| entry.text.as_str())
                    .collect::<String>(),
                "tool (prepared)\nraw args"
            );
        }
    }
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
