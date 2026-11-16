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

fn label(name: &str) -> &str {
    match name {
        "list_directory" => "Просмотр",
        "read_file" => "Чтение",
        "search_files" => "Поиск",
        "run_command" => "Запуск",
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

fn entry(text: String, action: Option<String>) -> Entry {
    Entry {
        role: "tool".into(),
        text,
        action,
    }
}
fn show(tool: &Tool, expanded: bool, elapsed: &impl Fn(&str) -> String, out: &mut Vec<Entry>) {
    let arrow = if expanded { "v" } else { ">" };
    let target = preview(&tool.detail.target, 140);
    let title = format!(
        "{arrow} {}{}{} · {}{}",
        label(&tool.detail.name),
        if target.is_empty() { "" } else { " " },
        target,
        status(tool),
        elapsed(&tool.detail.operation)
    );
    out.push(entry(title, Some(tool.key.clone())));
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
            if !value.is_empty() {
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
        });
    } else if failed(tool) && !tool.output.is_empty() {
        out.push(entry(format!("  {}", preview(&tool.output, 240)), None));
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
                format!(" · с ошибками: {problems}")
            } else if active {
                " · выполняется".into()
            } else {
                String::new()
            };
            let targets = group
                .iter()
                .take(3)
                .map(|tool| preview(&tool.detail.target, 45))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            result.push(entry(
                format!(
                    "{} Чтение и поиск · {} вызовов{state}{}{}",
                    if open { "v" } else { ">" },
                    group.len(),
                    if targets.is_empty() { "" } else { " · " },
                    targets
                ),
                Some(key),
            ));
            for tool in group {
                if open || failed(tool) {
                    show(
                        tool,
                        all || expanded.contains(&tool.key),
                        &elapsed,
                        &mut result,
                    );
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
            shown.contains("3 вызовов")
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
