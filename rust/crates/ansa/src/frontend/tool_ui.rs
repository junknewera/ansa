//! Presentation of acknowledged tool records and transient execution activity.
use super::{
    carbon::Entry,
    protocol_v1::{Activity, Snapshot},
};
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
}
enum Item {
    Text(Entry),
    Tool(usize),
}
use super::diff::EditView;
fn parsed_edit(tool: &Tool) -> Option<EditView> {
    use crate::protocol::{json::Json, legacy_json_v1::sha256};
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
    let value = Json::parse(&tool.output).ok()?;
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
        output,
        partial_arguments: false,
        partial_output: first.unwrap_or(true) || !last,
        arguments_suffix_missing: false,
        output_suffix_missing: false,
        running: false,
        uncertain: false,
    })
}

fn label(name: &str) -> &str {
    match name {
        "list_directory" => "Просмотр",
        "read_file" => "Чтение",
        "search_files" => "Поиск",
        "run_command" => "Запуск",
        "edit_file" => "Правка",
        _ => name,
    }
}
fn read(name: &str) -> bool {
    matches!(name, "list_directory" | "read_file" | "search_files")
}
fn status(tool: &Tool) -> &'static str {
    if tool.uncertain {
        return "результат неизвестен (effect_unknown)";
    }
    match tool.detail.phase {
        ToolPhase::Prepared if tool.running => "выполняется",
        ToolPhase::Prepared => "подготовлен",
        ToolPhase::Finished {
            effects: Effects::Unknown,
            ..
        } => "результат неизвестен (effect_unknown)",
        ToolPhase::Finished {
            execution: Execution::Denied,
            effects: Effects::NotStarted,
        } => "отклонён · не запускался",
        ToolPhase::Finished {
            execution: Execution::Cancelled,
            effects: Effects::NotStarted,
        } => "остановлен · не запускался",
        ToolPhase::Finished {
            effects: Effects::NotStarted,
            ..
        } => "ошибка · не запускался",
        ToolPhase::Finished { execution, .. } => match execution {
            Execution::Succeeded => "готово",
            Execution::Failed => "ошибка",
            Execution::Cancelled => "остановлен",
            Execution::Denied => "отклонён",
        },
    }
}
fn failed(tool: &Tool) -> bool {
    tool.uncertain
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
    match (count % 100, count % 10) {
        (11..=14, _) => "вызовов",
        (_, 1) => "вызов",
        (_, 2..=4) => "вызова",
        _ => "вызовов",
    }
}
fn entry(text: String, action: Option<String>, role: &str) -> Entry {
    Entry {
        role: role.into(),
        text,
        action,
        edit: None,
        status: None,
    }
}
fn show(tool: &Tool, expanded: bool, elapsed: &impl Fn(&str) -> String, out: &mut Vec<Entry>) {
    let arrow = if expanded { "v" } else { ">" };
    let target = preview(&tool.detail.target, 140);
    let edit = parsed_edit(tool).map(std::sync::Arc::new);
    let mut title = format!(
        "{arrow} {}{}{} · ",
        if edit.is_some() {
            "Изменён"
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
    let end = title.len();
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
            "  Diff недоступен; результат инструмента можно раскрыть через Ctrl+T".into(),
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
                    "  Ещё {} строк · нажмите, чтобы открыть diff",
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
        let mut detail = String::new();
        if !tool.detail.purpose.is_empty() {
            detail.push_str(&format!("  {}\n", tool.detail.purpose));
        }
        detail.push_str(&format!(
            "  {} · {}\n",
            tool.detail.operation, tool.detail.call
        ));
        for (label, value, partial) in [
            ("Аргументы", &tool.arguments, tool.partial_arguments),
            ("Вывод", &tool.output, tool.partial_output),
        ] {
            if !value.is_empty() && !(edit.is_some() && label == "Вывод") {
                detail.push_str(&format!(
                    "  {label}{}:\n",
                    if partial { " (фрагмент)" } else { "" }
                ));
                for line in value.lines() {
                    detail.push_str("    ");
                    detail.push_str(line);
                    detail.push('\n');
                }
            }
        }
        if let ToolPhase::Finished { execution, effects } = tool.detail.phase {
            detail.push_str(&format!("  execution={execution:?}, effects={effects:?}"));
        }
        out.push(Entry {
            role: "tool_detail".into(),
            text: detail.trim_end().into(),
            action: None,
            edit: None,
            status: None,
        });
    } else if failed(tool) && !tool.output.is_empty() {
        out.push(entry(
            format!("  {}", preview(&tool.output, 240)),
            None,
            presentation_role(tool),
        ));
    }
}

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
    for row in entries {
        let Some(detail) = &row.tool else {
            let is_tool = matches!(row.role.as_str(), "tool" | "tool_result");
            // Old fork seeds have plain timeline rows. Their full text remains
            // available through Ctrl+T without guessing identity from labels.
            let text = if !all && (is_tool || row.role == "reasoning") {
                row.text.lines().next().unwrap_or_default().into()
            } else {
                row.text.clone()
            };
            items.push(Item::Text(Entry {
                role: row.role.clone(),
                text,
                action: None,
                edit: None,
                status: None,
            }));
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
                Activity::Finished(finish) => ("Инструмент".into(), finish.call_id.clone()),
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
                tool.output = finish.output.chars().take(2048).collect();
                tool.partial_output = tool.output.len() < finish.output.len();
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
                    name: "Инструмент".into(),
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
            });
            items.push(Item::Tool(index));
        }
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
                format!(" · проблем: {problems}")
            } else if active {
                " · выполняется".into()
            } else if group
                .iter()
                .any(|tool| matches!(tool.detail.phase, ToolPhase::Prepared))
            {
                " · подготовлено".into()
            } else {
                " · готово".into()
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
                "{} Чтение и поиск · {} {}",
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
            " · 12 мс".into()
        });
        let header = &shown[0];
        assert_eq!(&header.text[header.status.clone().unwrap()], "ошибка");
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
    fn group_targets_are_unique_and_counts_use_russian_plural_forms() {
        for (count, expected) in [
            (1, "вызов"),
            (2, "вызова"),
            (3, "вызова"),
            (5, "вызовов"),
            (11, "вызовов"),
            (21, "вызов"),
            (24, "вызова"),
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
        assert!(shown[0].text.contains("3 вызова · готово"));
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
            |_| " · 12 мс".into(),
        );
        assert_eq!(shown.len(), 1);
        assert!(shown[0].text.contains("выполняется") && shown[0].text.contains("12 мс"));
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
        assert_eq!(compact.len(), 1);
        assert!(compact[0].text.contains("готово"));
        assert!(!text(&compact).contains("KnownResult"));
        let open = project(
            &rows,
            &view,
            &BTreeSet::from(["request:op".into()]),
            false,
            true,
            |_| String::new(),
        );
        assert_eq!(open.iter().filter(|e| e.action.is_some()).count(), 1);
        assert!(
            text(&open).contains("printf ok")
                && text(&open).contains("OUTPUT_CANARY")
                && text(&open).contains("Purpose from descriptor")
        );
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
            shown.contains("3 вызова")
                && shown.contains("ACCESS_CANARY")
                && shown.contains("UNKNOWN_CANARY")
        );
        assert!(shown.contains("не запускался") && shown.contains("effect_unknown"));
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
        assert!(text.contains("PAGE_FRAGMENT") && text.contains("фрагмент"));
        assert!(!text.contains("unrelated") && !text.contains("Запуск"));
        let mut first = row("head", "read_file", ToolPhase::Prepared, "FIRST_FRAGMENT");
        Arc::make_mut(first.tool.as_mut().unwrap()).last_chunk = false;
        let first = project(&[first], &live, &BTreeSet::new(), true, false, |_| {
            String::new()
        });
        assert!(super::tests::text(&first).contains("Аргументы (фрагмент)"));
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
        assert!(shown.contains("Вывод (фрагмент)"));
        assert_eq!(
            projected.iter().filter(|row| row.action.is_some()).count(),
            1
        );
    }
}
