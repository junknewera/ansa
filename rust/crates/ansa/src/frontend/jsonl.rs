//! Owning legacy v1 wire codec, shared by the actual CLI and parity oracle.
use super::{Result, protocol_v1::*};
use crate::{
    protocol::json::Json,
    provider::replay::{Effects, Execution},
};
fn s(v: &str) -> Json {
    Json::string(v).expect("validated UTF-8")
}
fn excerpt(value: &str, bytes: usize) -> Json {
    let end = value.floor_char_boundary(bytes.min(value.len()));
    if end == value.len() {
        s(value)
    } else {
        s(&format!("{}…", &value[..end]))
    }
}
fn n(v: u64) -> Json {
    Json::parse(&v.to_string()).expect("u64")
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).expect("boolean")
}
fn null() -> Json {
    Json::parse("null").expect("null")
}
fn id(v: Option<&str>) -> Json {
    v.map_or_else(null, s)
}
fn object(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).expect("unique static fields")
}
pub fn mode(v: AgentMode) -> &'static str {
    if v.read_only() { "plan" } else { "basic" }
}
pub fn execution(v: Execution) -> &'static str {
    match v {
        Execution::Succeeded => "succeeded",
        Execution::Failed => "failed",
        Execution::Cancelled => "cancelled",
        Execution::Denied => "denied",
    }
}
pub fn effects(v: Effects) -> &'static str {
    match v {
        Effects::NotStarted => "not_started",
        Effects::KnownResult => "known_result",
        Effects::Unknown => "effect_unknown",
    }
}
pub fn decode(line: &str) -> Result<RuntimeCommand> {
    let json = Json::parse_with_limits(line, 128 * 1024, 16, 64)?;
    if json.usize_field("schema_version")? != 1 {
        return Err("command_version");
    }
    let kind = json.text_field("type")?;
    let fields = if matches!(kind, "transcript" | "timeline") {
        if json.has("forward") { 9 } else { 8 }
    } else if kind == "context" {
        6
    } else if matches!(kind, "steer" | "generation") || kind == "submit" && json.has("mode") {
        8
    } else {
        7
    };
    if json.keys()?.len() != fields {
        return Err("command_fields");
    }
    let payload = match kind {
        "submit" => CommandPayload::Submit(Submit {
            text: json.text_field("text")?.into(),
            mode: match if json.has("mode") {
                json.text_field("mode")?
            } else {
                "basic"
            } {
                "basic" => AgentMode::Basic,
                "plan" => AgentMode::Plan,
                _ => return Err("submit_mode"),
            },
        }),
        "cancel" => CommandPayload::Cancel(json.text_field("target")?.into()),
        "steer" => CommandPayload::Steer(Steer {
            target: json.text_field("target")?.into(),
            text: json.text_field("text")?.into(),
        }),
        "generation" => CommandPayload::SetGeneration(GenerationSelection {
            model: json.text_field("model")?.into(),
            effort: if json.field("effort")?.is_null() {
                None
            } else {
                Some(json.text_field("effort")?.into())
            },
        }),
        "autocompact" => CommandPayload::SetAutocompact(
            u8::try_from(json.usize_field("percent")?).map_err(|_| "autocompact_range")?,
        ),
        "context" => CommandPayload::GetContext,
        "resume" => CommandPayload::Resume(json.text_field("target")?.into()),
        "snapshot" => CommandPayload::GetSnapshot(json.field("after_sequence")?.index()? as u64),
        "transcript" | "timeline" => {
            let query = GetTranscript {
                before: json.usize_field("before")?,
                limit: json.usize_field("limit")?,
                forward: if json.has("forward") {
                    json.field("forward")?
                        .boolean()
                        .map_err(|_| "transcript_direction")?
                } else {
                    false
                },
            };
            if kind == "timeline" {
                CommandPayload::GetTimeline(query)
            } else {
                CommandPayload::GetTranscript(query)
            }
        }
        _ => return Err("command_type"),
    };
    Ok(RuntimeCommand {
        schema_version: 1,
        session: json.text_field("session")?.into(),
        turn: json.text_field("turn")?.into(),
        incarnation: json.text_field("incarnation")?.into(),
        command: json.text_field("command_id")?.into(),
        payload,
    })
}
pub fn diagnostic(code: &str) -> Json {
    let json = code.starts_with("json_");
    let error = match code {
        "json_byte_limit"
        | "json_depth_limit"
        | "json_structure_limit"
        | "json_build_limit"
        | "json_string_limit"
        | "command_line_limit"
        | "command_queue_full"
        | "submit_size"
        | "transcript_page_limit"
        | "submit_id_limit" => 1,
        "command_version"
        | "command_fields"
        | "command_type"
        | "command_contract"
        | "cancel_target"
        | "resume_target"
        | "unknown_cli_option"
        | "runtime_closed"
        | "stale_cancel_target"
        | "stale_incarnation"
        | "command_id_conflict"
        | "unknown_session" => 7,
        "auth_required" | "auth_home_directory" | "auth_account_mismatch" => 4,
        v if v.starts_with("auth_") && !matches!(v, "auth_browser_hint" | "auth_browser_spawn") => {
            4
        }
        _ => 0,
    };
    object(vec![
        ("schema_version", n(1)),
        ("code", n(error)),
        ("stage", n(if json { 0 } else { 7 })),
        ("diagnostic", s(code)),
    ])
}
pub fn error(code: &str, command: Option<&str>) -> Json {
    let diag = diagnostic(code);
    let mut fields = vec![("type", s("error")), ("command_id", id(command))];
    for key in ["schema_version", "code", "stage", "diagnostic"] {
        fields.push((key, diag.field(key).expect("diagnostic field").clone()));
    }
    object(fields)
}
fn preview(v: &Preview) -> Json {
    object(vec![
        ("generation", n(v.generation)),
        ("text", s(&v.text)),
        ("state", s(&v.state)),
    ])
}
fn activity(v: &Activity, maximum: usize) -> Json {
    let mut fields = vec![("operation_id", s(&v.operation().0))];
    match v {
        Activity::Started(v) => fields.push(("call_id", s(&v.call_id))),
        Activity::Finished(v) => fields.extend([
            ("call_id", s(&v.call_id)),
            ("execution", s(execution(v.execution))),
            ("effects", s(effects(v.effects))),
            ("output", excerpt(&v.output, maximum)),
        ]),
    }
    object(fields)
}
pub fn encode(event: &RuntimeEvent) -> Json {
    let mut fields = vec![
        ("schema_version", n(event.schema_version.into())),
        ("stream_sequence", n(event.stream_sequence)),
        ("session", s(&event.session.0)),
        ("turn", s(&event.turn.0)),
        ("incarnation", s(&event.incarnation.0)),
        (
            "request_id",
            id(event.request.as_ref().map(|v| v.0.as_str())),
        ),
    ];
    let (kind, extra) = match &event.payload {
        EventPayload::MessageReceipt {
            query,
            command,
            receipt,
            ready,
            storage_failed,
            recovery_required,
        } => (
            "message_receipt",
            vec![
                ("query_id", s(&query.0)),
                ("command_id", s(&command.0)),
                ("ready", b(*ready)),
                ("storage_failed", b(*storage_failed)),
                ("recovery_required", b(*recovery_required)),
                (
                    "receipt",
                    receipt.as_ref().map_or_else(null, |receipt| {
                        let mut fields = vec![
                            (
                                "kind",
                                s(match receipt.kind {
                                    MessageKind::Submit => "submit",
                                    MessageKind::Steer => "steer",
                                }),
                            ),
                            ("turn", s(&receipt.turn.0)),
                            (
                                "request_id",
                                id(receipt.request.as_ref().map(|r| r.0.as_str())),
                            ),
                            ("state", s(&receipt.state)),
                            ("mode", receipt.mode.map_or_else(null, |m| s(mode(m)))),
                            ("admission_durable", b(receipt.admission_durable)),
                            ("text_sha256", s(&receipt.text_sha256)),
                            ("state_durable", b(receipt.state_durable)),
                        ];
                        receipt.add_image_hashes(&mut fields);
                        object(fields)
                    }),
                ),
            ],
        ),
        EventPayload::FollowupQueued { command } => {
            ("followup_queued", vec![("command_id", s(&command.0))])
        }
        EventPayload::FollowupDelivered { commands } => (
            "followup_delivered",
            vec![(
                "command_ids",
                Json::array(commands.iter().map(|c| s(&c.0)).collect()).expect("bounded commands"),
            )],
        ),
        EventPayload::AutocompactSelected { command, percent } => (
            "autocompact_selected",
            vec![
                ("command_id", s(&command.0)),
                ("percent", n(u64::from(*percent))),
            ],
        ),
        EventPayload::GenerationSelected { command, selection } => (
            "generation_selected",
            vec![
                ("command_id", s(&command.0)),
                ("model", s(&selection.model)),
                ("effort", selection.effort.as_deref().map_or_else(null, s)),
            ],
        ),
        EventPayload::ContextSnapshot { command, seed } => (
            "context",
            vec![
                ("command_id", s(&command.0)),
                ("source_session", s(&seed.source_session().0)),
                ("source_turn", s(&seed.source_turn().0)),
            ],
        ),
        EventPayload::FrontendSnapshot(v) => {
            // Reserve room for the envelope, response preview and JSON escapes.
            // Ordinary snapshots retain all their content.
            let group = Json::FRAME_BYTES / 4;
            let complete_tools = v
                .tools
                .iter()
                .map(|entry| match entry {
                    Activity::Started(_) => 0,
                    Activity::Finished(tool) => Json::quoted_size(&tool.output),
                })
                .sum::<usize>()
                <= group;
            let complete_followups = v
                .followups
                .iter()
                .map(|entry| Json::quoted_size(&entry.text))
                .sum::<usize>()
                <= group;
            let tool_output_limit = if complete_tools {
                usize::MAX
            } else {
                group / 6 / v.tools.len().max(1)
            };
            let followup_limit = if complete_followups {
                usize::MAX
            } else {
                group / 6 / v.followups.len().max(1)
            };
            (
                "snapshot",
                vec![
                    ("through_sequence", n(v.through_sequence)),
                    ("state", s(&v.state)),
                    ("worker_busy", b(v.worker_busy)),
                    ("completed_tools", n(v.completed_tools as u64)),
                    ("text", excerpt(&v.text, 128 * 1024)),
                    ("diagnostic", s(&v.diagnostic)),
                    (
                        "recovery_operation",
                        id(v.recovery_operation.as_ref().map(|v| v.0.as_str())),
                    ),
                    ("submit_id", id(v.submit.as_ref().map(|v| v.0.as_str()))),
                    ("command_id", id(v.command.as_ref().map(|v| v.0.as_str()))),
                    (
                        "tools",
                        Json::array(
                            v.tools
                                .iter()
                                .map(|entry| activity(entry, tool_output_limit))
                                .collect(),
                        )
                        .expect("activities"),
                    ),
                    ("history_items", n(v.history_items as u64)),
                    ("compactions", n(v.compactions as u64)),
                    ("autocompact_percent", n(u64::from(v.autocompact_percent))),
                    ("storage_failed", b(v.storage_failed)),
                    ("durable", b(v.durable)),
                    ("resume_available", b(v.resume_available)),
                    ("mode", s(mode(v.mode))),
                    ("transcript_size", n(v.transcript_size as u64)),
                    ("preview", v.preview.as_ref().map_or_else(null, preview)),
                    ("timeline_size", n(v.timeline_size as u64)),
                    (
                        "generation",
                        object(vec![
                            ("model", s(&v.generation.model)),
                            (
                                "effort",
                                v.generation.effort.as_deref().map_or_else(null, s),
                            ),
                        ]),
                    ),
                    (
                        "followups",
                        Json::array(
                            v.followups
                                .iter()
                                .map(|f| {
                                    object(vec![
                                        ("command_id", s(&f.command.0)),
                                        ("target", s(&f.target.0)),
                                        ("text", excerpt(&f.text, followup_limit)),
                                        ("state", s(&f.state)),
                                    ])
                                })
                                .collect(),
                        )
                        .expect("bounded followups"),
                    ),
                ],
            )
        }
        EventPayload::Preview(v) => (
            "text_preview",
            vec![
                ("generation", n(v.generation)),
                ("text", s(&v.text)),
                ("state", s(&v.state)),
            ],
        ),
        EventPayload::TranscriptPage(v)
        | EventPayload::TimelinePage(v)
        | EventPayload::ConversationPage(v) => (
            match event.payload {
                EventPayload::TimelinePage(_) => "timeline",
                EventPayload::ConversationPage(_) => "conversation",
                _ => "transcript",
            },
            vec![
                ("begin", n(v.begin as u64)),
                ("end", n(v.end as u64)),
                ("total", n(v.total as u64)),
                (
                    "query",
                    object(vec![
                        ("before", n(v.query.before as u64)),
                        ("limit", n(v.query.limit as u64)),
                        ("forward", b(v.query.forward)),
                    ]),
                ),
                (
                    "entries",
                    Json::array(
                        v.entries
                            .iter()
                            .map(|e| {
                                object(vec![
                                    ("turn", s(&e.turn.0)),
                                    ("request_id", s(&e.request.0)),
                                    ("role", s(&e.role)),
                                    ("text", s(&e.text)),
                                    ("state", s(&e.state)),
                                    ("mode", s(mode(e.mode))),
                                ])
                            })
                            .collect(),
                    )
                    .expect("entries"),
                ),
            ],
        ),
        EventPayload::ResyncRequired { oldest_available } => (
            "resync_required",
            vec![("oldest_available", n(*oldest_available))],
        ),
        EventPayload::CommandAccepted { command, .. } => {
            ("command_accepted", vec![("command_id", s(&command.0))])
        }
        EventPayload::CommandReplayed { command, state } => (
            "command_replayed",
            vec![("command_id", s(&command.0)), ("state", s(state))],
        ),
        EventPayload::RuntimeError {
            command,
            diagnostic,
        } => (
            "error",
            vec![
                ("command_id", id(command.as_ref().map(|v| v.0.as_str()))),
                ("diagnostic", s(diagnostic)),
            ],
        ),
        EventPayload::RecoveryRequired {
            operation,
            effects: v,
        } => (
            "recovery_required",
            vec![
                ("operation_id", s(&operation.0)),
                ("effects", s(effects(*v))),
            ],
        ),
        EventPayload::TextDelta { text } => ("text_delta", vec![("text", s(text))]),
        EventPayload::ToolStarted(v) => {
            let mut f = vec![
                ("operation_id", s(&v.operation.0)),
                ("call_id", s(&v.call_id)),
            ];
            if !v.name.is_empty() {
                f.push(("name", s(&v.name)));
            }
            ("tool_started", f)
        }
        EventPayload::ToolFinished(v) => (
            "tool_finished",
            vec![
                ("operation_id", s(&v.operation.0)),
                ("call_id", s(&v.call_id)),
                ("execution", s(execution(v.execution))),
                ("effects", s(effects(v.effects))),
                ("output", excerpt(&v.output, 1024 * 1024)),
            ],
        ),
        EventPayload::Cancelled { command } => ("cancelled", vec![("command_id", s(&command.0))]),
        EventPayload::Done => ("done", vec![]),
        EventPayload::Snapshot {
            through_sequence,
            state,
        } => (
            "snapshot",
            vec![
                ("through_sequence", n(*through_sequence)),
                ("state", s(state)),
            ],
        ),
    };
    fields.push(("type", s(kind)));
    fields.extend(extra);
    object(fields)
}
