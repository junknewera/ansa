//! Presentation of acknowledged tool records and transient execution activity.
use super::{
    carbon::Entry,
    protocol_v1::{Activity, Snapshot},
};
use crate::protocol::json::Json;
use crate::{
    provider::replay::{Effects, Execution},
    session::transcript::{self, ToolDetail, ToolPhase},
};
use std::collections::{BTreeMap, BTreeSet};

struct Tool {
    key: String,
    request: String,
    detail: ToolDetail,
    arguments: String,
    output: String,
    partial_arguments: bool,
    partial_output: bool,
    arguments_suffix_missing: bool,
    output_suffix_missing: bool,
    running: bool,
    uncertain: bool,
    arguments_json: Option<Json>,
    output_json: Option<Json>,
    volatile_outcome: bool,
}
enum Item {
    Text(Entry),
    Tool(usize),
}
use super::diff::EditView;
fn parsed_edit(tool: &Tool) -> Option<EditView> {
    use crate::protocol::legacy_json_v1::sha256;
    if tool.detail.name != "edit_file"
        || tool.partial_output
        || tool.uncertain
        || !matches!(
            tool.detail.phase,
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult
            }
        )
    {
        return None;
    }
    let value = tool.output_json.as_ref()?;
    if !value.field("applied").ok()?.boolean().ok()? {
        return None;
    }
    let before = value.text_field("before").ok()?;
    let after = value.text_field("after").ok()?;
    if value.text_field("before_hash").ok()? != sha256(before.as_bytes())
        || value.text_field("after_hash").ok()? != sha256(after.as_bytes())
    {
        return None;
    }
    Some(EditView {
        path: value.text_field("path").ok()?.into(),
        diff: super::diff::between(before, after)?,
    })
}
#[cfg(test)]
fn edit_view(entries: &[transcript::Entry], key: &str) -> Option<EditView> {
    let mut output = String::new();
    let mut first = None;
    let mut last = false;
    let mut detail = None;
    for row in entries {
        let Some(tool) = &row.tool else {
            continue;
        };
        if format!("{}:{}", row.request.0, tool.operation) != key
            || tool.name != "edit_file"
            || !matches!(tool.phase, ToolPhase::Finished { .. })
        {
            continue;
        }
        first.get_or_insert(tool.continued);
        last = tool.last_chunk;
        output.push_str(row.text.get(tool.content_start..)?);
        if output.len() > 120 * 1024 {
            return None;
        }
        detail = Some((**tool).clone());
    }
    parsed_edit(&Tool {
        key: key.into(),
        request: String::new(),
        detail: detail?,
        arguments: String::new(),
        output: output.clone(),
        partial_arguments: false,
        partial_output: first.unwrap_or(true) || !last,
        arguments_suffix_missing: false,
        output_suffix_missing: false,
        running: false,
        uncertain: false,
        arguments_json: None,
        output_json: Json::parse(&output).ok(),
        volatile_outcome: false,
    })
}

fn label(name: &str) -> &str {
    match name {
        "list_directory" => "List",
        "read_file" => "Read",
        "search_files" => "Search",
        "run_command" => "Run",
        "edit_file" => "Edit",
        "workspace_describe" => "Workspace",
        "editor_get_context" => "Selection",
        "editor_open_file" => "Open in editor",
        "editor_propose_diff" => "Propose edit",
        "dataset_inspect" => "Inspect CSV",
        "python_execute" => "Analyze",
        "show_png" => "Show PNG",
        _ => name,
    }
}
fn read(name: &str) -> bool {
    matches!(name, "list_directory" | "read_file" | "search_files")
}
fn flag(value: &Json, name: &str) -> bool {
    value.field(name).and_then(Json::boolean) == Ok(true)
}
fn output_notice(tool: &Tool) -> Option<&'static str> {
    if tool.volatile_outcome {
        return Some("result not saved");
    }
    if tool.partial_output {
        return Some("partial output");
    }
    let Some(value) = tool.output_json.as_ref() else {
        return (tool.detail.name == "run_command"
            && matches!(tool.detail.phase, ToolPhase::Finished { .. }))
        .then_some("unknown exit code");
    };
    if flag(value, "cleanup_unknown")
        || value.field("cleanup_complete").and_then(Json::boolean) == Ok(false)
    {
        Some("process cleanup unconfirmed")
    } else if flag(value, "timed_out") {
        Some("timed out")
    } else if flag(value, "cancelled") || value.text_field("error") == Ok("cancelled") {
        Some("stopped")
    } else if value.text_field("status") == Ok("instructions_required") {
        Some("not run · instructions required")
    } else if value.has("error")
        || (tool.detail.name == "run_command"
            && value
                .field("exit_code")
                .is_ok_and(|code| code.dump() != "0"))
    {
        Some("error")
    } else if tool.detail.name == "run_command" && !value.has("exit_code") {
        Some("unknown exit code")
    } else if flag(value, "truncated") {
        Some("output truncated")
    } else {
        None
    }
}
fn status(tool: &Tool) -> &'static str {
    if tool.uncertain {
        return "unknown result (effect_unknown)";
    }
    match tool.detail.phase {
        ToolPhase::Prepared if tool.running => "running",
        ToolPhase::Prepared => "prepared",
        ToolPhase::Finished {
            effects: Effects::Unknown,
            ..
        } => "unknown result (effect_unknown)",
        ToolPhase::Finished {
            execution: Execution::Denied,
            effects: Effects::NotStarted,
        } => "denied · not run",
        ToolPhase::Finished {
            execution: Execution::Cancelled,
            effects: Effects::NotStarted,
        } => "stopped · not run",
        ToolPhase::Finished {
            effects: Effects::NotStarted,
            ..
        } => "failed · not run",
        ToolPhase::Finished { execution, .. } => match execution {
            Execution::Succeeded => output_notice(tool).unwrap_or("done"),
            Execution::Failed => "error",
            Execution::Cancelled => "stopped",
            Execution::Denied => "denied",
        },
    }
}
fn failed(tool: &Tool) -> bool {
    tool.uncertain
        || output_notice(tool).is_some()
        || matches!(tool.detail.phase, ToolPhase::Finished { execution, effects }
        if execution != Execution::Succeeded || effects != Effects::KnownResult)
}
fn preview(text: &str, limit: usize) -> String {
    let text = super::workspace::display_text(text);
    let mut result = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if result.chars().count() > limit {
        result = result.chars().take(limit).collect();
        result.push_str("...");
    }
    result
}

fn presentation_role(tool: &Tool) -> &'static str {
    if tool.uncertain
        || matches!(
            tool.detail.phase,
            ToolPhase::Finished {
                effects: Effects::Unknown,
                ..
            }
        )
        || output_notice(tool).is_some_and(|notice| {
            matches!(
                notice,
                "result not saved"
                    | "partial output"
                    | "output truncated"
                    | "process cleanup unconfirmed"
                    | "not run · instructions required"
                    | "unknown exit code"
            )
        })
    {
        "tool_warning"
    } else if failed(tool) {
        "tool_error"
    } else if matches!(tool.detail.phase, ToolPhase::Finished { .. }) {
        "tool_success"
    } else if tool.running {
        "tool_running"
    } else {
        "tool_pending"
    }
}
fn calls(count: usize) -> &'static str {
    if count == 1 { "call" } else { "calls" }
}
fn entry(text: String, action: Option<String>, role: &str) -> Entry {
    Entry {
        role: role.into(),
        text,
        action,
        edit: None,
        output: None,
        agent: None,
        status: None,
    }
}
fn hidden(key: &str) -> bool {
    matches!(
        key,
        "instruction_token"
            | "instructions"
            | "instruction_scopes"
            | "operation"
            | "operation_id"
            | "call"
            | "call_id"
            | "execution"
            | "effects"
            | "before_hash"
            | "after_hash"
    )
}
fn readable(value: &Json, depth: usize, lines: &mut Vec<String>) {
    if depth > 6 || lines.len() > 2048 {
        return;
    }
    if let Ok(text) = value.text() {
        lines.extend(text.lines().map(String::from));
    } else if let Ok(items) = value.elements() {
        for item in items {
            readable(item, depth + 1, lines);
        }
    } else if let Ok(keys) = value.keys() {
        for key in keys.into_iter().filter(|key| !hidden(key)) {
            if let Ok(item) = value.field(key) {
                if let Ok(text) = item.text() {
                    lines.push(format!("{key}: {text}"));
                } else if item.is_object() || item.is_array() {
                    readable(item, depth + 1, lines);
                } else {
                    lines.push(format!("{key}: {}", item.dump()));
                }
            }
        }
    } else {
        lines.push(value.dump());
    }
}
fn fallback(raw: &str, parsed: Option<&Json>, lines: &mut Vec<String>) {
    if let Some(value) = parsed {
        readable(value, 0, lines);
    } else if raw.trim_start().starts_with(['{', '[']) {
        lines.push("Incomplete structured output".into());
    } else {
        lines.extend(raw.lines().map(String::from));
    }
}
fn details(tool: &Tool, include_output: bool) -> super::carbon::OutputView {
    let mut lines = Vec::new();
    if !tool.arguments.is_empty() {
        lines.push(
            if tool.partial_arguments {
                "Arguments (excerpt):"
            } else {
                "Arguments:"
            }
            .into(),
        );
        if let Some(args) = &tool.arguments_json {
            if let Ok(argv) = args.field("argv").and_then(Json::elements) {
                let command = argv
                    .iter()
                    .filter_map(|arg| arg.text().ok())
                    .map(|arg| {
                        if !arg.is_empty()
                            && arg
                                .chars()
                                .all(|ch| ch.is_alphanumeric() || "_./=-".contains(ch))
                        {
                            arg.into()
                        } else {
                            format!("'{}'", arg.replace('\'', "'\\''"))
                        }
                    })
                    .collect::<Vec<String>>()
                    .join(" ");
                lines.push(format!("$ {command}"));
            }
            for (key, label) in [
                ("command", "$"),
                ("path", "Path:"),
                ("cwd", "Directory:"),
                ("query", "Query:"),
                ("offset", "Offset:"),
                ("max_bytes", "Bytes:"),
                ("start_line", "First line:"),
                ("end_line", "Last line:"),
            ] {
                if let Ok(value) = args.field(key) {
                    lines.push(format!(
                        "{label} {}",
                        value
                            .text()
                            .map(String::from)
                            .unwrap_or_else(|_| value.dump())
                    ));
                }
            }
            if !matches!(
                tool.detail.name.as_str(),
                "run_command" | "read_file" | "list_directory" | "search_files" | "edit_file"
            ) {
                readable(args, 0, &mut lines);
            }
        } else {
            fallback(&tool.arguments, None, &mut lines);
        }
    }
    if include_output && !tool.output.is_empty() {
        lines.push(
            if tool.partial_output {
                "Output (excerpt):"
            } else {
                "Output:"
            }
            .into(),
        );
        let begin = lines.len();
        if let Some(value) = &tool.output_json {
            if let Ok(error) = value.text_field("error") {
                lines.push(format!("Error: {error}"));
            }
            if value.text_field("status") == Ok("instructions_required") {
                lines.push("Directory instructions required. The action has not run".into());
            }
            match tool.detail.name.as_str() {
                "read_file" => {
                    if let Ok(path) = value.text_field("path") {
                        let range = value
                            .field("offset")
                            .ok()
                            .zip(value.field("next_offset").ok())
                            .map(|(start, end)| format!(" · bytes {}-{}", start.dump(), end.dump()))
                            .unwrap_or_default();
                        lines.push(format!("{path}{range}"));
                    }
                    if let Ok(text) = value.text_field("content") {
                        lines.extend(text.split('\n').map(String::from));
                    }
                }
                "list_directory" => {
                    if let Ok(path) = value.text_field("path") {
                        lines.push(path.into());
                    }
                    if let Ok(entries) = value.field("entries").and_then(Json::elements) {
                        for item in entries {
                            if let Ok(name) = item.text_field("name") {
                                lines.push(format!(
                                    "{name}{}{}",
                                    if flag(item, "directory") { "/" } else { "" },
                                    if flag(item, "symlink") {
                                        " (symlink)"
                                    } else {
                                        ""
                                    }
                                ));
                            }
                        }
                        if entries.is_empty() {
                            lines.push("Directory is empty".into());
                        }
                    }
                }
                "search_files" => {
                    if let Ok(matches) = value.field("matches").and_then(Json::elements) {
                        for item in matches {
                            if let Ok(path) = item.text_field("path") {
                                let line = item
                                    .field("line")
                                    .ok()
                                    .map(|line| format!(":{}", line.dump()))
                                    .unwrap_or_default();
                                let text = item
                                    .text_field("text")
                                    .ok()
                                    .map(|text| format!(": {text}"))
                                    .unwrap_or_default();
                                lines.push(format!("{path}{line}{text}"));
                            }
                        }
                        if matches.is_empty() {
                            lines.push("No matches".into());
                        }
                    }
                }
                "run_command" => {
                    for (key, label) in [("stdout", "stdout:"), ("stderr", "stderr:")] {
                        if let Ok(text) = value.text_field(key)
                            && !text.is_empty()
                        {
                            lines.push(label.into());
                            lines.extend(text.split('\n').map(String::from));
                        }
                    }
                    if let Ok(exit) = value.field("exit_code") {
                        lines.push(format!("Exit code: {}", exit.dump()));
                    }
                    if flag(value, "timed_out") {
                        lines.push("Timed out".into());
                    }
                    if flag(value, "cancelled") {
                        lines.push("Command stopped".into());
                    }
                    if flag(value, "cleanup_unknown")
                        || value.field("cleanup_complete").and_then(Json::boolean) == Ok(false)
                    {
                        lines.push("Process cleanup unconfirmed".into());
                    }
                }
                _ => readable(value, 0, &mut lines),
            }
            if flag(value, "truncated") {
                lines.push("Tool truncated output".into());
            }
        }
        if lines.len() == begin {
            fallback(&tool.output, tool.output_json.as_ref(), &mut lines);
        }
    }
    if tool.partial_output && tool.output.is_empty() {
        lines.push("Output excerpt unavailable on this page".into());
    }
    let text = super::workspace::display_text(&lines.join("\n"));
    let mut text = text.chars().take(128 * 1024).collect::<String>();
    if text.len() < lines.iter().map(String::len).sum() {
        text.push_str("\nOutput truncated for display");
    }
    super::carbon::OutputView {
        title: format!(
            "{} {}",
            label(&tool.detail.name),
            preview(&tool.detail.target, 140)
        ),
        text,
    }
}
fn show(tool: &Tool, expanded: bool, elapsed: &impl Fn(&str) -> String, out: &mut Vec<Entry>) {
    let arrow = if expanded { "v" } else { ">" };
    let target = preview(&tool.detail.target, 140);
    let edit = parsed_edit(tool).map(std::sync::Arc::new);
    let mut title = format!(
        "{arrow} {}{}{} · ",
        if edit.is_some() {
            "Edited"
        } else {
            label(&tool.detail.name)
        },
        if target.is_empty() { "" } else { " " },
        target,
    );
    if let Some(edit) = &edit {
        title.push_str(&format!("(+{} -{}) · ", edit.diff.added, edit.diff.removed));
    }
    let start = title.len();
    title.push_str(status(tool));
    title.push_str(if status(tool).contains("unknown") {
        " ?"
    } else if status(tool).starts_with("stopped") {
        " ■"
    } else if status(tool).starts_with("denied") {
        " ⊘"
    } else {
        match presentation_role(tool) {
            "tool_success" => " ✓",
            "tool_error" => " ✕",
            "tool_warning" => " !",
            "tool_running" => " ◷",
            _ => "",
        }
    });
    let end = start + status(tool).len();
    title.push_str(&elapsed(&tool.detail.operation));
    let action = if edit.is_some() {
        format!("diff:{}", tool.key)
    } else {
        tool.key.clone()
    };
    let mut header = entry(title, Some(action.clone()), presentation_role(tool));
    header.edit = edit.clone();
    header.status = Some(start..end);
    out.push(header);
    if !tool.uncertain
        && !tool.volatile_outcome
        && matches!(
            tool.detail.phase,
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult
            }
        )
        && let Some(output) = tool.output_json.as_ref()
        && !output.has("error")
        && output
            .field("executed")
            .and_then(crate::protocol::json::Json::boolean)
            != Ok(false)
        && let Some(path) = match tool.detail.name.as_str() {
            "read_file" if output.has("content") => output.text_field("path").ok(),
            "dataset_inspect" if output.has("rows") => tool
                .arguments_json
                .as_ref()
                .and_then(|value| value.text_field("path").ok()),
            "python_execute" if output.has("artifacts") => tool
                .arguments_json
                .as_ref()
                .and_then(|value| value.text_field("path").ok()),
            _ => None,
        }
    {
        out.push(entry(
            format!("Source · {path}"),
            Some(format!("file:0:{path}")),
            "tool_child_success",
        ));
    }
    if tool.detail.name == "edit_file"
        && edit.is_none()
        && !tool.uncertain
        && matches!(
            tool.detail.phase,
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult
            }
        )
    {
        out.push(entry(
            "  Diff unavailable. Press Ctrl+O for tool details".into(),
            None,
            "tool_diff_notice",
        ));
    }
    if let Some(edit) = &edit {
        for row in edit.diff.rows.iter().take(10) {
            let mut entry = entry(row.display(), Some(action.clone()), row.role());
            entry.edit = Some(edit.clone());
            out.push(entry);
        }
        if edit.diff.rows.len() > 10 {
            let mut notice = entry(
                format!(
                    "  {} more lines · open full diff",
                    edit.diff.rows.len() - 10
                ),
                Some(action),
                "tool_diff_notice",
            );
            notice.edit = Some(edit.clone());
            out.push(notice);
        }
    }
    if expanded {
        let full = std::sync::Arc::new(details(tool, edit.is_none()));
        let visible = full
            .text
            .lines()
            .take(12)
            .map(|line| {
                let shown = line.chars().take(180).collect::<String>();
                format!(
                    "    {shown}{}",
                    if shown.len() < line.len() { "…" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !visible.is_empty() {
            out.push(entry(visible, None, "tool_detail"));
        }
        if !full.text.is_empty() {
            let mut link = entry(
                "  Full output".into(),
                Some(format!("output:{}", tool.key)),
                "tool_output",
            );
            link.output = Some(full);
            out.push(link);
        }
    } else if failed(tool) && !tool.output.is_empty() {
        let mut summary = Vec::new();
        if let Some(value) = &tool.output_json {
            for key in ["error", "stderr"] {
                if let Ok(text) = value.text_field(key)
                    && !text.is_empty()
                {
                    summary.push(text.into());
                }
            }
            if let Ok(exit) = value.field("exit_code") {
                summary.push(format!("Exit code: {}", exit.dump()));
            }
        }
        if summary.is_empty() {
            fallback(&tool.output, tool.output_json.as_ref(), &mut summary);
        }
        out.push(entry(
            format!("  {}", preview(&summary.join("\n"), 240)),
            None,
            presentation_role(tool),
        ));
    }
}

use crate::session::timeline::{conversation_row, draft_material};

pub(super) fn project(
    entries: &[transcript::Entry],
    view: &Snapshot,
    expanded: &BTreeSet<String>,
    all: bool,
    current: bool,
    elapsed: impl Fn(&str) -> String,
) -> Vec<Entry> {
    let mut tools: Vec<Tool> = Vec::new();
    let mut indices = BTreeMap::new();
    let mut items = Vec::new();
    let mut drafts = BTreeSet::new();
    for row in entries {
        let Some(detail) = &row.tool else {
            let is_tool = matches!(row.role.as_str(), "tool" | "tool_result");
            if draft_material(row) {
                if drafts.insert(row.turn.0.as_str()) {
                    let proposal = entry(
                        "Proposed plan".into(),
                        Some(format!("plan:draft:{}", row.turn.0)),
                        "tool_success",
                    );
                    if row.turn.0.starts_with("draft-revise-")
                        && let Some(Item::Text(previous)) = items.last_mut()
                        && previous
                            .action
                            .as_ref()
                            .is_some_and(|key| key.starts_with("plan:draft:"))
                    {
                        *previous = proposal;
                    } else {
                        items.push(Item::Text(proposal));
                    }
                }
            } else if row.role != "user" && !conversation_row(row) {
                // Old fork seeds contain plain tool rows without operation IDs.
                let text = if !all && is_tool {
                    row.text.lines().next().unwrap_or_default().into()
                } else {
                    row.text.clone()
                };
                items.push(Item::Text(entry(text, None, &row.role)));
            }
            if row.role == "assistant"
                && let Some(run) = row.turn.0.strip_prefix("summary-")
                && crate::provider::contracts::valid_id(run)
            {
                items.push(Item::Text(entry(
                    "Approved plan".into(),
                    Some(format!("plan:{run}")),
                    "tool_success",
                )));
            }
            if row.role == "assistant" && row.text.contains("#L") {
                let mut links = BTreeSet::new();
                for event in pulldown_cmark::Parser::new(&row.text) {
                    let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
                        dest_url, ..
                    }) = event
                    else {
                        continue;
                    };
                    let Some((path, line)) = dest_url.rsplit_once("#L") else {
                        continue;
                    };
                    let Ok(line) = line.parse::<usize>() else {
                        continue;
                    };
                    if line == 0
                        || path.contains(':')
                        || path.chars().any(char::is_control)
                        || !std::path::Path::new(path)
                            .components()
                            .all(|part| matches!(part, std::path::Component::Normal(_)))
                        || path.is_empty()
                        || !links.insert((path.to_owned(), line))
                    {
                        continue;
                    }
                    items.push(Item::Text(entry(
                        format!("Response link · {path}:{line}"),
                        Some(format!("file:{line}:{path}")),
                        "tool_detail",
                    )));
                    if links.len() == 8 {
                        break;
                    }
                }
            }
            continue;
        };
        let key = format!("{}:{}", row.request.0, detail.operation);
        let index = *indices.entry(key.clone()).or_insert_with(|| {
            let index = tools.len();
            tools.push(Tool {
                key,
                request: row.request.0.clone(),
                detail: (**detail).clone(),
                arguments: String::new(),
                output: String::new(),
                partial_arguments: false,
                partial_output: false,
                arguments_suffix_missing: false,
                output_suffix_missing: false,
                running: false,
                uncertain: false,
                arguments_json: None,
                output_json: None,
                volatile_outcome: false,
            });
            items.push(Item::Tool(index));
            index
        });
        let tool = &mut tools[index];
        let payload = row.text.get(detail.content_start..).unwrap_or_default();
        match detail.phase {
            ToolPhase::Prepared => {
                if tool.arguments.is_empty() {
                    tool.partial_arguments = detail.continued;
                }
                tool.arguments.push_str(payload);
                tool.arguments_suffix_missing = !detail.last_chunk;
            }
            ToolPhase::Finished { .. } => {
                if tool.output.is_empty() {
                    tool.partial_output = detail.continued;
                }
                tool.output.push_str(payload);
                tool.output_suffix_missing = !detail.last_chunk;
                tool.detail = (**detail).clone();
            }
        }
    }
    for tool in &mut tools {
        tool.partial_arguments |= tool.arguments_suffix_missing;
        tool.partial_output |= tool.output_suffix_missing;
    }
    // Snapshot repairs a lost notification. It never demotes a durable outcome.
    for activity in view.tools.iter().filter(|_| current) {
        let Some(request) = &view.request else {
            break;
        };
        let operation = activity.operation();
        let key = format!("{}:{}", request.0, operation.0);
        if !indices.contains_key(&key) && matches!(activity, Activity::Finished(_)) {
            // Older completed calls may lie before this page. The canonical
            // outcome page supplies their detail; snapshot is no second log.
            continue;
        }
        let index = *indices.entry(key.clone()).or_insert_with(|| {
            let index = tools.len();
            let (name, call) = match activity {
                Activity::Started(start) => (start.name.clone(), start.call_id.clone()),
                Activity::Finished(finish) => ("Tool".into(), finish.call_id.clone()),
            };
            tools.push(Tool {
                key,
                request: request.0.clone(),
                detail: ToolDetail {
                    operation: operation.0.clone(),
                    name,
                    call,
                    purpose: "".into(),
                    target: String::new(),
                    phase: ToolPhase::Prepared,
                    content_start: 0,
                    continued: false,
                    last_chunk: true,
                },
                arguments: String::new(),
                output: String::new(),
                partial_arguments: false,
                partial_output: false,
                arguments_suffix_missing: false,
                output_suffix_missing: false,
                running: false,
                uncertain: false,
                arguments_json: None,
                output_json: None,
                volatile_outcome: false,
            });
            items.push(Item::Tool(index));
            index
        });
        let tool = &mut tools[index];
        if !matches!(tool.detail.phase, ToolPhase::Prepared) {
            continue;
        }
        match activity {
            Activity::Started(_) => tool.running = view.worker_busy,
            Activity::Finished(finish) => {
                tool.detail.phase = ToolPhase::Finished {
                    execution: finish.execution,
                    effects: finish.effects,
                };
                tool.output = finish.output.chars().take(120 * 1024).collect();
                tool.partial_output = tool.output.len() < finish.output.len();
                tool.volatile_outcome = view.storage_failed || !view.durable;
            }
        }
    }
    if current && let (Some(request), Some(operation)) = (&view.request, &view.recovery_operation) {
        let key = format!("{}:{}", request.0, operation.0);
        if let Some(index) = indices.get(&key) {
            tools[*index].uncertain = matches!(tools[*index].detail.phase, ToolPhase::Prepared);
        } else {
            let index = tools.len();
            tools.push(Tool {
                key,
                request: request.0.clone(),
                detail: ToolDetail {
                    operation: operation.0.clone(),
                    name: "Tool".into(),
                    call: String::new(),
                    purpose: "".into(),
                    target: String::new(),
                    phase: ToolPhase::Prepared,
                    content_start: 0,
                    continued: false,
                    last_chunk: true,
                },
                arguments: String::new(),
                output: String::new(),
                partial_arguments: false,
                partial_output: false,
                arguments_suffix_missing: false,
                output_suffix_missing: false,
                running: false,
                uncertain: true,
                arguments_json: None,
                output_json: None,
                volatile_outcome: false,
            });
            items.push(Item::Tool(index));
        }
    }
    for tool in &mut tools {
        tool.arguments_json = (!tool.partial_arguments)
            .then(|| Json::parse_with_limits(&tool.arguments, 128 * 1024, 64, 131072).ok())
            .flatten();
        tool.output_json = (!tool.partial_output)
            .then(|| Json::parse_with_limits(&tool.output, 128 * 1024, 64, 131072).ok())
            .flatten();
    }
    let mut result = Vec::new();
    let mut at = 0;
    while at < items.len() {
        let Item::Tool(index) = &items[at] else {
            if let Item::Text(row) = &items[at] {
                result.push(row.clone());
            }
            at += 1;
            continue;
        };
        let first = &tools[*index];
        let mut end = at + 1;
        if read(&first.detail.name) {
            while let Some(Item::Tool(index)) = items.get(end) {
                if !read(&tools[*index].detail.name) || tools[*index].request != first.request {
                    break;
                }
                end += 1;
            }
        }
        if end > at + 1 {
            let key = format!("group:{}", first.key);
            let open = all || expanded.contains(&key);
            let group: Vec<&Tool> = items[at..end]
                .iter()
                .filter_map(|item| match item {
                    Item::Tool(i) => Some(&tools[*i]),
                    _ => None,
                })
                .collect();
            let active = group
                .iter()
                .any(|tool| tool.running && matches!(tool.detail.phase, ToolPhase::Prepared));
            let problems = group.iter().filter(|tool| failed(tool)).count();
            let state = if problems > 0 {
                format!(" · issues: {problems}")
            } else if active {
                " · running".into()
            } else if group
                .iter()
                .any(|tool| matches!(tool.detail.phase, ToolPhase::Prepared))
            {
                " · prepared".into()
            } else {
                " · done ✓".into()
            };
            let mut seen = BTreeSet::new();
            let targets = group
                .iter()
                .map(|tool| preview(&tool.detail.target, 45))
                .filter(|t| !t.is_empty() && seen.insert(t.clone()))
                .take(3)
                .collect::<Vec<_>>()
                .join(", ");
            let mut title = format!(
                "{} Read and search · {} {}",
                if open { "v" } else { ">" },
                group.len(),
                calls(group.len())
            );
            let start = title.len() + " · ".len();
            title.push_str(&state);
            let end = title.len();
            if !targets.is_empty() {
                title.push_str(" · ");
                title.push_str(&targets);
            }
            let mut header = entry(
                title,
                Some(key),
                if group
                    .iter()
                    .any(|tool| presentation_role(tool) == "tool_error")
                {
                    "tool_error"
                } else if problems > 0 {
                    "tool_warning"
                } else if active {
                    "tool_running"
                } else if group
                    .iter()
                    .any(|tool| matches!(tool.detail.phase, ToolPhase::Prepared))
                {
                    "tool_pending"
                } else {
                    "tool_success"
                },
            );
            header.status = Some(start..end);
            result.push(header);
            for tool in group {
                if open || failed(tool) {
                    let begin = result.len();
                    show(
                        tool,
                        all || expanded.contains(&tool.key),
                        &elapsed,
                        &mut result,
                    );
                    for row in &mut result[begin..] {
                        if row.action.is_some() {
                            row.role = format!(
                                "tool_child_{}",
                                row.role.strip_prefix("tool_").unwrap_or(&row.role)
                            );
                        }
                    }
                }
            }
        } else {
            show(
                first,
                all || expanded.contains(&first.key),
                &elapsed,
                &mut result,
            );
        }
        at = end;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::protocol_v1::{AgentMode, Finished, GenerationSelection, Started};
    use super::*;
    use std::sync::Arc;
    fn view() -> Snapshot {
        Snapshot {
            timeline_size: 1,
            conversation_size: 1,
            generation: GenerationSelection {
                model: "fixture".into(),
                effort: None,
            },
            followups: vec![],
            through_sequence: 1,
            state: "executing".into(),
            request: Some("request".into()),
            worker_busy: true,
            completed_tools: 0,
            text: String::new(),
            diagnostic: String::new(),
            recovery_operation: None,
            submit: None,
            command: None,
            tools: vec![],
            history_items: 0,
            compactions: 0,
            autocompact_percent: 50,
            storage_failed: false,
            durable: true,
            resume_available: false,
            mode: AgentMode::Basic,
            transcript_size: 1,
            preview: None,
        }
    }
    fn row(op: &str, name: &str, phase: ToolPhase, payload: &str) -> transcript::Entry {
        transcript::Entry {
            turn: "turn".into(),
            request: "request".into(),
            role: if matches!(phase, ToolPhase::Prepared) {
                "tool"
            } else {
                "tool_result"
            }
            .into(),
            text: format!("raw\n{payload}"),
            state: "intent".into(),
            mode: AgentMode::Basic,
            tool: Some(Arc::new(ToolDetail {
                operation: op.into(),
                name: name.into(),
                call: format!("call-{op}"),
                purpose: "Purpose from descriptor".into(),
                target: "docs/README.md".into(),
                phase,
                content_start: 4,
                continued: false,
                last_chunk: true,
            })),
        }
    }
    fn text(entries: &[Entry]) -> String {
        entries
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn command_exit_timeout_cancel_and_cleanup_never_claim_success() {
        for (output, expected) in [
            (
                r#"{"exit_code":7,"stdout":"","stderr":"command failed\nnext line"}"#,
                "error",
            ),
            (r#"{"exit_code":0,"timed_out":true}"#, "timed out"),
            (r#"{"exit_code":0,"cancelled":true}"#, "stopped"),
            (
                r#"{"exit_code":0,"cleanup_complete":false}"#,
                "process cleanup unconfirmed",
            ),
            (r#"{"exit_code":0,"truncated":true}"#, "output truncated"),
            (r#"{"error":"protected_file"}"#, "error"),
            ("legacy output", "unknown exit code"),
        ] {
            let result = row(
                "op",
                "run_command",
                ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                },
                output,
            );
            let shown = project(&[result], &view(), &BTreeSet::new(), false, false, |_| {
                String::new()
            });
            assert!(shown[0].text.contains(expected), "{}", shown[0].text);
            assert_ne!(shown[0].role, "tool_success");
            assert!(!shown[0].text.contains('✓'));
        }
        let success = row(
            "ok",
            "run_command",
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            r#"{"exit_code":0,"stdout":"ok\n","stderr":"","cleanup_complete":true}"#,
        );
        let shown = project(&[success], &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        assert_eq!(shown[0].role, "tool_success");
        assert!(shown[0].text.contains('✓'));
    }
    #[test]
    fn known_outputs_decode_newlines_and_hide_internal_fields() {
        for (name, output, expected) in [
            (
                "read_file",
                r#"{"path":"file.rs","offset":0,"next_offset":12,"content":"one\ntwo\n","truncated":false}"#,
                "one\n",
            ),
            (
                "list_directory",
                r#"{"path":"src","entries":[{"name":"child","directory":true,"symlink":false}],"truncated":false}"#,
                "child/",
            ),
            (
                "search_files",
                r#"{"matches":[{"path":"src/file.rs","line":12,"text":"needle"}],"instruction_scopes":["hidden"],"truncated":false}"#,
                "src/file.rs:12: needle",
            ),
            (
                "run_command",
                r#"{"exit_code":0,"stdout":"one\ntwo\n","stderr":"","truncated":false}"#,
                "one\n",
            ),
        ] {
            let rows = [
                row(
                    "op",
                    name,
                    ToolPhase::Prepared,
                    r#"{"path":"src","argv":["printf","one\ntwo"],"instruction_token":"HIDDEN_TOKEN"}"#,
                ),
                row(
                    "op",
                    name,
                    ToolPhase::Finished {
                        execution: Execution::Succeeded,
                        effects: Effects::KnownResult,
                    },
                    output,
                ),
            ];
            let shown = project(&rows, &view(), &BTreeSet::new(), true, false, |_| {
                String::new()
            });
            let full = shown
                .iter()
                .find_map(|entry| entry.output.as_ref())
                .unwrap();
            assert!(full.text.contains(expected), "{}", full.text);
            for hidden in [
                "HIDDEN_TOKEN",
                "instruction_token",
                "instruction_scopes",
                "execution=",
                "effects=",
                "call-op",
                "Purpose from descriptor",
                r#"\n"#,
            ] {
                assert!(!full.text.contains(hidden), "{hidden}: {}", full.text);
            }
        }
    }
    #[test]
    fn full_output_is_captured_from_the_displayed_page_and_preview_is_bounded() {
        use super::super::carbon::{self, Workspace};
        let stdout = (0..40)
            .map(|line| format!("line_{line} {}界😀", "x".repeat(300)))
            .collect::<Vec<_>>()
            .join("\n");
        let output = Json::object(vec![
            (
                "stdout",
                Json::string(&format!("{stdout}\n\x1b]52;blocked")).unwrap(),
            ),
            ("exit_code", Json::parse("0").unwrap()),
        ])
        .unwrap()
        .dump();
        let result = row(
            "long",
            "run_command",
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            &output,
        );
        let shown = project(&[result], &view(), &BTreeSet::new(), true, false, |_| {
            String::new()
        });
        assert!(text(&shown).len() < 4096);
        assert!(!text(&shown).contains("line_39"));
        let frame = carbon::layout_workspace(
            &shown,
            b"draft",
            0,
            "",
            0,
            "project",
            (80, 32),
            false,
            &Workspace::default(),
        )
        .unwrap();
        let full = frame.output_view("output:request:long").unwrap();
        drop(shown);
        let _other_page = project(&[], &view(), &BTreeSet::new(), true, false, |_| {
            String::new()
        });
        assert!(full.text.contains("line_39") && full.text.contains("界😀"));
        assert!(!full.text.contains('\x1b'));
        assert_eq!(frame.output_view("output:request:long").unwrap(), full);
    }
    #[test]
    fn status_range_ignores_separators_in_command_arguments() {
        let mut result = row(
            "op",
            "run_command",
            ToolPhase::Finished {
                execution: Execution::Failed,
                effects: Effects::KnownResult,
            },
            "failure",
        );
        let mut metadata = (**result.tool.as_ref().unwrap()).clone();
        metadata.target = "printf 'a · b' · project".into();
        result.tool = Some(Arc::new(metadata));
        let shown = project(&[result], &view(), &BTreeSet::new(), false, false, |_| {
            " · 12 ms".into()
        });
        let header = &shown[0];
        assert_eq!(&header.text[header.status.clone().unwrap()], "error");
        assert!(header.text.contains("printf 'a · b'"));
    }
    #[test]
    fn acknowledged_edit_has_numbered_inline_diff_and_exact_full_view() {
        use crate::protocol::{json::Json, legacy_json_v1::sha256};
        let before = "fn main() {\n    old();\n}\n";
        let after = "fn main() {\n    new();\n}\n";
        let value = Json::object(vec![
            ("path", Json::string("code.rs").unwrap()),
            ("before", Json::string(before).unwrap()),
            ("after", Json::string(after).unwrap()),
            (
                "before_hash",
                Json::string(&sha256(before.as_bytes())).unwrap(),
            ),
            (
                "after_hash",
                Json::string(&sha256(after.as_bytes())).unwrap(),
            ),
            ("applied", Json::parse("true").unwrap()),
        ])
        .unwrap();
        let result = row(
            "edit",
            "edit_file",
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            &value.dump(),
        );
        let shown = project(
            std::slice::from_ref(&result),
            &view(),
            &BTreeSet::new(),
            false,
            false,
            |_| String::new(),
        );
        assert!(shown[0].text.contains("(+1 -1)"));
        assert_eq!(shown[0].action.as_deref(), Some("diff:request:edit"));
        assert!(
            shown
                .iter()
                .any(|row| row.role == "tool_diff_remove" && row.text.contains("old();"))
        );
        assert!(
            shown
                .iter()
                .any(|row| row.role == "tool_diff_add" && row.text.contains("new();"))
        );
        let full = edit_view(std::slice::from_ref(&result), "request:edit").unwrap();
        assert_eq!(full.path, "code.rs");
        assert_eq!((full.diff.added, full.diff.removed), (1, 1));
        for phase in [
            ToolPhase::Prepared,
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::Unknown,
            },
        ] {
            let unknown = row("edit", "edit_file", phase, &value.dump());
            assert!(
                project(&[unknown], &view(), &BTreeSet::new(), false, false, |_| {
                    String::new()
                })
                .iter()
                .all(|row| !row.role.starts_with("tool_diff_"))
            );
        }
    }
    #[test]
    fn displayed_edit_survives_snapshot_only_outcome_and_page_replacement() {
        use super::super::carbon::{self, Workspace};
        use crate::protocol::{json::Json, legacy_json_v1::sha256};
        let before = "old\n";
        let after = "new\n";
        let output = Json::object(vec![
            ("path", Json::string("saved.rs").unwrap()),
            ("before", Json::string(before).unwrap()),
            ("after", Json::string(after).unwrap()),
            (
                "before_hash",
                Json::string(&sha256(before.as_bytes())).unwrap(),
            ),
            (
                "after_hash",
                Json::string(&sha256(after.as_bytes())).unwrap(),
            ),
            ("applied", Json::parse("true").unwrap()),
        ])
        .unwrap()
        .dump();
        let prepared = row("edit", "edit_file", ToolPhase::Prepared, "{}");
        let mut snapshot = view();
        snapshot.storage_failed = true;
        snapshot.tools = vec![Activity::Finished(Finished {
            operation: "edit".into(),
            call_id: "call-edit".into(),
            execution: Execution::Succeeded,
            effects: Effects::KnownResult,
            output: Arc::new(output),
        })];
        let shown = project(
            &[prepared],
            &snapshot,
            &BTreeSet::new(),
            false,
            true,
            |_| String::new(),
        );
        let frame = carbon::layout_workspace(
            &shown,
            b"draft",
            0,
            "storage failed",
            0,
            "project",
            (80, 24),
            true,
            &Workspace::default(),
        )
        .unwrap();
        assert!(
            frame
                .tool_actions
                .iter()
                .any(|(_, key)| key == "diff:request:edit")
        );
        let captured = frame.edit_view("diff:request:edit").unwrap();
        drop(snapshot);
        drop(shown);
        let _different_page = project(&[], &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        assert_eq!(captured.path, "saved.rs");
        assert!(captured.diff.rows.iter().any(|row| row.text == "old"));
        assert!(captured.diff.rows.iter().any(|row| row.text == "new"));
        assert_eq!(frame.edit_view("diff:request:edit").unwrap(), captured);
    }
    #[test]
    fn group_targets_are_unique_and_counts_use_english_plural_forms() {
        for (count, expected) in [
            (1, "call"),
            (2, "calls"),
            (3, "calls"),
            (5, "calls"),
            (11, "calls"),
            (21, "calls"),
            (24, "calls"),
        ] {
            assert_eq!(calls(count), expected);
        }
        let rows = ["one", "two", "three"].map(|op| {
            row(
                op,
                "read_file",
                ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                },
                "file body",
            )
        });
        let shown = project(&rows, &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].role, "tool_success");
        assert!(shown[0].text.contains("3 calls · done"));
        assert_eq!(shown[0].text.matches("docs/README.md").count(), 1);
    }
    #[test]
    fn one_operation_updates_and_expands_without_repeating_the_intent() {
        let mut view = view();
        let intent = row(
            "op",
            "run_command",
            ToolPhase::Prepared,
            "{\"command\":\"printf ok\"}",
        );
        view.tools.push(Activity::Started(Started {
            operation: "op".into(),
            call_id: "call-op".into(),
            name: "run_command".into(),
        }));
        let shown = project(
            std::slice::from_ref(&intent),
            &view,
            &BTreeSet::new(),
            false,
            true,
            |_| " · 12 ms".into(),
        );
        assert_eq!(shown.len(), 1);
        assert!(shown[0].text.contains("running") && shown[0].text.contains("12 ms"));
        assert!(!shown[0].text.contains("raw") && !shown[0].text.contains("printf"));
        let outcome = row(
            "op",
            "run_command",
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            "OUTPUT_CANARY",
        );
        view.tools.push(Activity::Finished(Finished {
            operation: "op".into(),
            call_id: "call-op".into(),
            execution: Execution::Succeeded,
            effects: Effects::KnownResult,
            output: Arc::new("OUTPUT_CANARY".into()),
        }));
        let rows = [intent, outcome];
        let compact = project(&rows, &view, &BTreeSet::new(), false, true, |_| {
            String::new()
        });
        assert_eq!(compact.len(), 2);
        assert!(compact[0].text.contains("unknown exit code"));
        assert!(!text(&compact).contains("KnownResult"));
        let open = project(
            &rows,
            &view,
            &BTreeSet::from(["request:op".into()]),
            false,
            true,
            |_| String::new(),
        );
        assert_eq!(open.iter().filter(|e| e.action.is_some()).count(), 2);
        assert!(text(&open).contains("printf ok") && text(&open).contains("OUTPUT_CANARY"));
        assert!(!text(&open).contains("Purpose from descriptor"));
    }
    #[test]
    fn read_groups_keep_denials_and_unknown_effects_visible_when_collapsed() {
        let rows = [
            row(
                "one",
                "read_file",
                ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                },
                "read ok",
            ),
            row(
                "two",
                "search_files",
                ToolPhase::Finished {
                    execution: Execution::Denied,
                    effects: Effects::NotStarted,
                },
                "ACCESS_CANARY",
            ),
            row(
                "three",
                "list_directory",
                ToolPhase::Finished {
                    execution: Execution::Cancelled,
                    effects: Effects::Unknown,
                },
                "UNKNOWN_CANARY",
            ),
        ];
        let compact = project(&rows, &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        let shown = text(&compact);
        assert!(
            shown.contains("3 calls")
                && shown.contains("ACCESS_CANARY")
                && shown.contains("UNKNOWN_CANARY")
        );
        assert!(shown.contains("not run") && shown.contains("effect_unknown"));
        assert!(!shown.contains("read ok"));
        let open = project(
            &rows,
            &view(),
            &BTreeSet::from(["group:request:one".into()]),
            false,
            false,
            |_| String::new(),
        );
        assert_eq!(
            open.iter().filter(|entry| entry.action.is_some()).count(),
            4
        );
    }
    #[test]
    fn outcome_page_keeps_identity_and_marks_partial_payload_without_injecting_live_tools() {
        let mut outcome = row(
            "old",
            "read_file",
            ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            "PAGE_FRAGMENT",
        );
        Arc::make_mut(outcome.tool.as_mut().unwrap()).continued = true;
        let mut live = view();
        live.tools.push(Activity::Started(Started {
            operation: "unrelated".into(),
            call_id: "call".into(),
            name: "run_command".into(),
        }));
        let shown = project(&[outcome], &live, &BTreeSet::new(), true, false, |_| {
            String::new()
        });
        let text = text(&shown);
        assert!(text.contains("PAGE_FRAGMENT") && text.contains("excerpt"));
        assert!(!text.contains("unrelated") && !text.contains("Run"));
        let mut first = row("head", "read_file", ToolPhase::Prepared, "FIRST_FRAGMENT");
        Arc::make_mut(first.tool.as_mut().unwrap()).last_chunk = false;
        let first = project(&[first], &live, &BTreeSet::new(), true, false, |_| {
            String::new()
        });
        assert!(super::tests::text(&first).contains("Arguments (excerpt)"));
    }
    #[test]
    fn terminal_activity_keeps_plan_links_and_draft_material_without_parent_chat() {
        let plain = |turn: &str, role: &str, state: &str, text: &str| transcript::Entry {
            turn: turn.into(),
            request: "request".into(),
            role: role.into(),
            text: text.into(),
            state: state.into(),
            mode: AgentMode::Basic,
            tool: None,
        };
        let rows = [
            plain("draft-legacy", "user", "accepted", "USER_INPUT"),
            plain("draft-legacy", "assistant", "completed", "LEGACY_DRAFT"),
            plain("planned", "assistant", "plan_draft", "PART_ONE"),
            plain("planned", "assistant", "plan_draft", "PART_TWO"),
            plain("planned", "reasoning", "confirmed", "PLAN_REASONING"),
            plain("draft-revise-auto", "user", "accepted", "REPAIR_PROMPT"),
            plain(
                "draft-revise-auto",
                "reasoning",
                "confirmed",
                "REPAIR_REASONING",
            ),
            plain(
                "draft-revise-auto",
                "assistant",
                "plan_draft",
                "CORRECTED_PLAN",
            ),
            plain("summary-test", "reasoning", "confirmed", "PARENT_REASONING"),
            plain("summary-test", "assistant", "completed", "PARENT_MESSAGE"),
        ];
        let chat: Vec<_> = rows
            .iter()
            .filter(|row| conversation_row(row))
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(
            chat,
            [
                "USER_INPUT",
                "PLAN_REASONING",
                "REPAIR_REASONING",
                "PARENT_REASONING",
                "PARENT_MESSAGE"
            ]
        );

        let activity = project(&rows, &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        let actions: Vec<_> = activity
            .iter()
            .filter_map(|entry| entry.action.as_deref())
            .collect();
        assert_eq!(
            actions,
            [
                "plan:draft:draft-legacy",
                "plan:draft:draft-revise-auto",
                "plan:test"
            ]
        );
        let shown = text(&activity);
        assert!(!shown.contains("REPAIR_PROMPT"));
        assert!(
            !shown.contains("REASONING")
                && !shown.contains("PARENT_")
                && !shown.contains("USER_INPUT")
        );
    }
    #[test]
    fn recovery_and_legacy_unknown_results_remain_visible_in_closed_groups() {
        let mut snapshot = view();
        snapshot.tools.clear();
        snapshot.recovery_operation = Some("op".into());
        let rows = [
            row("op", "read_file", ToolPhase::Prepared, "{}"),
            row("next", "read_file", ToolPhase::Prepared, "{}"),
        ];
        let projected = project(&rows, &snapshot, &BTreeSet::new(), false, true, |_| {
            String::new()
        });
        assert!(text(&projected).contains("effect_unknown"));
        assert!(
            projected
                .iter()
                .any(|entry| entry.action.as_deref() == Some("request:op"))
        );
        let mut legacy = row("old", "run_command", ToolPhase::Prepared, "{}");
        legacy.tool = None;
        legacy.text = "run_command Cancelled Unknown\nunknown output".into();
        let projected = project(&[legacy], &view(), &BTreeSet::new(), false, false, |_| {
            String::new()
        });
        assert!(text(&projected).contains("Cancelled Unknown"));
        let recovered = project(&[], &snapshot, &BTreeSet::new(), false, true, |_| {
            String::new()
        });
        assert!(text(&recovered).contains("effect_unknown"));
    }
    #[test]
    fn snapshot_overlay_cannot_expand_old_outputs_past_the_timeline_page() {
        let mut snapshot = view();
        let huge = Arc::new("x".repeat(128 * 1024));
        snapshot.tools = (0..256)
            .map(|index| {
                Activity::Finished(Finished {
                    operation: super::super::protocol_v1::OperationId(format!("op{index}")),
                    call_id: "call".into(),
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                    output: huge.clone(),
                })
            })
            .collect();
        let rows = [row("op255", "run_command", ToolPhase::Prepared, "{}")];
        let projected = project(&rows, &snapshot, &BTreeSet::new(), true, true, |_| {
            String::new()
        });
        let shown = text(&projected);
        assert!(shown.len() < 4096, "{}", shown.len());
        assert!(shown.contains("Output (excerpt)"));
        assert_eq!(
            projected.iter().filter(|row| row.action.is_some()).count(),
            2
        );
    }
}
