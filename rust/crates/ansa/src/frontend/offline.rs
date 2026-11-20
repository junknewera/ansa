//! Deterministic C++-compatible mock, admitted through the production decoder.
use super::Result;
use crate::{
    auth::Cancel,
    context::legacy_compact::{CompactedContext, summary_request_at},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{native, request::Request, responses::AdmittedResponse, structured::FinalFormat},
};
fn s(v: &str) -> Json {
    Json::string(v).expect("UTF-8")
}
fn obj(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).expect("static fields")
}
fn call(suffix: &str) -> Json {
    obj(vec![
        ("type", s("function_call")),
        ("id", s(&format!("fixture_item{suffix}"))),
        ("call_id", s(&format!("fixture_call{suffix}"))),
        ("namespace", s("omh")),
        ("name", s("get_test_value")),
        ("arguments", s("{}")),
        ("status", s("completed")),
    ])
}
fn reasoning(suffix: &str) -> Json {
    obj(vec![
        ("type", s("reasoning")),
        ("id", s(&format!("fixture_reasoning{suffix}"))),
        ("summary", Json::parse("[]").expect("array")),
        (
            "encrypted_content",
            s(&format!("synthetic_opaque_value{suffix}")),
        ),
    ])
}
pub fn generate(request: &Request, stop: &Cancel) -> Result<AdmittedResponse> {
    if stop.cancelled() {
        return Err("offline_cancelled");
    }
    if let Some(response) = orchestration_fixture(request, stop)? {
        return Ok(response);
    }
    if let Some(response) = terminal_fixture(request)? {
        return Ok(response);
    }
    let last = request.input.last();
    let (id, items) = if !last
        .is_some_and(|v| v.has("type") && v.text_field("type") == Ok("function_call_output"))
    {
        let suffix = if request.input.len() <= 1 {
            String::new()
        } else {
            format!(
                "_{}",
                &sha256(Json::array(request.input.clone())?.dump().as_bytes())[..16]
            )
        };
        (
            format!("fixture_response1{suffix}"),
            vec![reasoning(&suffix), call(&suffix)],
        )
    } else {
        let size = request.input.len();
        if size < 4 {
            return Err("fixture_replay_conflict");
        }
        let previous = &request.input[size - 2];
        let call_id = previous.text_field("call_id")?;
        let suffix = call_id
            .strip_prefix("fixture_call")
            .ok_or("fixture_replay_conflict")?;
        if request.input[size - 3] != reasoning(suffix)
            || *previous != call(suffix)
            || last.expect("present").text_field("call_id")? != call_id
        {
            return Err("fixture_replay_conflict");
        }
        let output = Json::parse_with_limits(
            last.expect("present").text_field("output")?,
            128 * 1024,
            64,
            131072,
        )?;
        let part = obj(vec![
            ("type", s("output_text")),
            (
                "text",
                s(&format!("OMH_TOOL_OK:{}", output.text_field("value")?)),
            ),
        ]);
        (
            format!("fixture_response2{suffix}"),
            vec![obj(vec![
                ("type", s("message")),
                ("id", s(&format!("fixture_message{suffix}"))),
                ("role", s("assistant")),
                ("status", s("completed")),
                ("content", Json::array(vec![part])?),
            ])],
        )
    };
    native::load(&obj(vec![
        ("request", request.save()?),
        ("id", s(&id)),
        ("model", s(&request.binding.returned_model)),
        ("items", Json::array(items)?),
    ]))
}

fn orchestration_fixture(request: &Request, stop: &Cancel) -> Result<Option<AdmittedResponse>> {
    if request.final_format().is_none()
        && request.epoch.exact_state.text_field("purpose") != Ok("orchestration_summary")
    {
        return Ok(None);
    }
    let last = request.input.last();
    let user = last
        .and_then(|item| {
            (item.text_field("role") == Ok("user"))
                .then(|| item.text_field("content").ok())
                .flatten()
        })
        .unwrap_or_default();
    let id = sha256(request.save()?.dump().as_bytes());
    if request.final_format() == Some(FinalFormat::ChildResult)
        && user.starts_with("Review first fixture file")
    {
        stop.wait(std::time::Duration::from_millis(950));
        stop.check()?;
    }
    if request.final_format() == Some(FinalFormat::ChildResult) {
        let operation = if user.starts_with("Синтетическая правка отчёта")
        {
            Some(("edit_file", "report.txt", "orch_edit_file_"))
        } else if user.starts_with("Read the review-resume fixture") {
            Some(("read_file", "report.txt", "orch_read_report_"))
        } else if user.starts_with("Независимо проверьте замороженный коммит")
        {
            Some(("read_file", "ansa-candidate.diff", "orch_read_file_"))
        } else {
            review_resume_probe(last)?.map(|(path, call_prefix)| ("read_file", path, call_prefix))
        };
        if let Some((name, path, call_prefix)) = operation {
            let base = if request.epoch.exact_state.has("base_state") {
                request.epoch.exact_state.field("base_state")?
            } else {
                &request.epoch.exact_state
            };
            let token = sha256(base.field("instructions")?.dump().as_bytes());
            let args = if name == "edit_file" {
                serde_json::json!({"path": path, "old_text": "before fixture", "new_text": "after fixture", "instruction_token": token})
            } else {
                serde_json::json!({"path": path, "offset": 0, "max_bytes": 8192, "instruction_token": token})
            };
            let item = obj(vec![
                ("type", s("function_call")),
                ("id", s(&format!("item_{}", &id[..24]))),
                ("call_id", s(&format!("{call_prefix}{}", &id[..24]))),
                ("namespace", s("omh")),
                ("name", s(name)),
                ("arguments", s(&args.to_string())),
                ("status", s("completed")),
            ]);
            return orchestration_response(request, &id, item).map(Some);
        }
    }
    let text = match request.final_format() {
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands | FinalFormat::OrchestrationPlanWithReadFolder) if user == "ORCHESTRATION_CLARIFICATION_FIXTURE" => serde_json::json!({
            "schema_version": 1, "summary": "Read and compare two files", "checks":[], "manual_checks":[], "steps": [
                {"id":"first", "task":"Review first fixture file", "access":"read", "paths":["first.txt"], "depends_on":[], "acceptance":"Keep the first file intact"},
                {"id":"second", "task":"Review second fixture file", "access":"read", "paths":["second.txt"], "depends_on":["first"], "acceptance":"Compare the findings"}
            ]
        }).to_string(),
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands | FinalFormat::OrchestrationPlanWithReadFolder) if user == "ORCHESTRATION_LONG_PLAN_FIXTURE" => serde_json::json!({
            "schema_version": 1, "summary": "Проверить перенос длинного плана", "checks":[], "manual_checks":[], "steps": [
                {"id":"long", "task":format!("{}VISIBLE_END_OF_STEP", "длинная строка ".repeat(130)), "access":"read", "paths":[], "depends_on":[], "acceptance":"Прочитать строку целиком"}
            ]
        }).to_string(),
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands | FinalFormat::OrchestrationPlanWithReadFolder) if user == "ORCHESTRATION_REVIEW_RESUME_FIXTURE" => serde_json::json!({
            "schema_version":1, "summary":"Verify review-resume fixture", "checks":[{"path":"report.txt","contains":"after fixture review-resume"}], "manual_checks":[], "steps":[
                {"id":"read","task":"Read the review-resume fixture","access":"read","paths":["report.txt"],"depends_on":[],"acceptance":"Report contains before fixture"},
                {"id":"edit","task":"Синтетическая правка отчёта","access":"write","paths":["report.txt"],"depends_on":["read"],"acceptance":"Report contains after fixture"}
            ]
        }).to_string(),
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands | FinalFormat::OrchestrationPlanWithReadFolder) if matches!(user, "ORCHESTRATION_WRITE_FIXTURE" | "ORCHESTRATION_COMMAND_FIXTURE" | "ORCHESTRATION_BLOCKED_WRITE_FIXTURE") => serde_json::json!({
            "schema_version": 1, "summary": "Обновить тестовый отчёт", "checks":[{"path":"report.txt","contains":"after fixture"}], "manual_checks": if user == "ORCHESTRATION_BLOCKED_WRITE_FIXTURE" { vec!["Подтвердить исходные данные"] } else { vec![] }, "steps": [
                {"id":"edit", "task":"Синтетическая правка отчёта", "access":"write", "paths":["report.txt"], "depends_on":[], "acceptance":"Тестовый отчёт содержит after fixture"}
            ]
        }).to_string(),
        Some(FinalFormat::OrchestrationPlan | FinalFormat::OrchestrationPlanWithCommands | FinalFormat::OrchestrationPlanWithReadFolder) => serde_json::json!({
            "schema_version": 1,
            "summary": "Разобрать запрос и собрать подтверждения",
            "checks": [],
            "manual_checks": [],
            "steps": [
                {"id":"files","task":"Найти файлы, связанные с запросом","access":"read","paths":[],"depends_on":[],"acceptance":"Указать источники"},
                {"id":"check","task":"Проверить, достаточно ли данных для ответа","access":"read","paths":[],"depends_on":[],"acceptance":"Указать оставшиеся вопросы"}
            ]
        }).to_string(),
        Some(FinalFormat::ChildResult) => {
            let mut success = true;
            if let Some(last) = last.filter(|item| item.text_field("type") == Ok("function_call_output")) {
                let output = Json::parse(last.text_field("output")?)?;
                let call = last.text_field("call_id")?;
                success = if call.starts_with("orch_edit_file_") {
                    output.field("applied").and_then(Json::boolean) == Ok(true)
                } else if call.starts_with("orch_read_report_") {
                    output.text_field("content").is_ok_and(|text| text == "before fixture review-resume\n")
                } else if call.starts_with("orch_review_checkout_") {
                    output.text_field("content").is_ok_and(|text| text == "after fixture review-resume\n")
                } else {
                    call.starts_with("orch_read_file_") && output.text_field("content").is_ok_and(|diff| diff.contains("-before fixture") && diff.contains("+after fixture"))
                };
            }
            serde_json::json!({"status": if success { "completed" } else { "blocked" }, "summary":"Синтетический результат проверки", "findings":[]}).to_string()
        },
        None if request.epoch.exact_state.text_field("purpose") == Ok("orchestration_summary") => {
            if user.contains("verified_candidate") {
                "Рабочая копия проверена. Ветка ansa-result готова.".to_owned()
            } else {
                "Сводка готова. Оба помощника завершили проверку.".to_owned()
            }
        }
        None => return Ok(None),
    };
    let text = if request
        .final_format()
        .is_some_and(FinalFormat::has_command_checks)
    {
        let checks = if matches!(
            user,
            "ORCHESTRATION_COMMAND_FIXTURE" | "ORCHESTRATION_REVIEW_RESUME_FIXTURE"
        ) {
            serde_json::json!([{"argv":["./check.sh"]}])
        } else {
            serde_json::json!([])
        };
        let mut plan =
            Json::parse(&text)?.with("command_checks", Json::parse(&checks.to_string())?)?;
        if request.final_format() == Some(FinalFormat::OrchestrationPlanWithReadFolder) {
            plan = plan.with("read_folder", Json::parse("null")?)?;
        }
        plan.dump()
    } else {
        text
    };
    let item = obj(vec![
        ("type", s("message")),
        ("id", s(&format!("msg_{}", &id[..24]))),
        ("role", s("assistant")),
        ("status", s("completed")),
        (
            "content",
            Json::array(vec![obj(vec![
                ("type", s("output_text")),
                ("text", s(&text)),
            ])])?,
        ),
    ]);
    orchestration_response(request, &id, item).map(Some)
}

fn review_resume_probe(last: Option<&Json>) -> Result<Option<(&'static str, &'static str)>> {
    let Some(last) = last.filter(|item| item.text_field("type") == Ok("function_call_output"))
    else {
        return Ok(None);
    };
    let call = last.text_field("call_id")?;
    if call.starts_with("orch_read_file_") {
        let output = Json::parse(last.text_field("output")?)?;
        if output
            .text_field("content")
            .is_ok_and(|diff| diff.contains("+after fixture review-resume"))
        {
            return Ok(Some(("approved-plan.json", "orch_review_plan_")));
        }
    }
    if call.starts_with("orch_review_plan_") {
        let output = Json::parse(last.text_field("output")?)?;
        if output
            .text_field("content")
            .is_ok_and(|plan| plan.contains("Verify review-resume fixture"))
        {
            return Ok(Some(("checkout/report.txt", "orch_review_checkout_")));
        }
        return Err("fixture_review_plan");
    }
    Ok(None)
}

fn orchestration_response(request: &Request, id: &str, item: Json) -> Result<AdmittedResponse> {
    native::load(&obj(vec![
        ("request", request.save()?),
        ("id", s(&format!("response_{}", &id[..24]))),
        ("model", s(&request.binding.returned_model)),
        ("items", Json::array(vec![item])?),
    ]))
}

fn terminal_fixture(request: &Request) -> Result<Option<AdmittedResponse>> {
    if !request
        .tools
        .descriptors
        .iter()
        .any(|tool| tool.name == "terminal_create")
    {
        return Ok(None);
    }
    let Some(last) = request.input.last() else {
        return Ok(None);
    };
    let (id, item) = if last.text_field("role") == Ok("user")
        && last.text_field("content") == Ok("TERMINAL_DEMO")
    {
        let hash = sha256(Json::array(request.input.clone())?.dump().as_bytes());
        let suffix = &hash[..16];
        let arguments = serde_json::json!({
            "label": "Demo terminal",
            "cwd": ".",
            "argv": ["sh", "-c", "printf 'ANSA_%s\\n' TERMINAL_READY; exec sh -i"],
            "rows": 24,
            "columns": 80,
            "lifetime_seconds": 0,
            "idle_seconds": 0,
        });
        (
            format!("fixture_terminal_open_{suffix}"),
            obj(vec![
                ("type", s("function_call")),
                ("id", s(&format!("fixture_terminal_item_{suffix}"))),
                ("call_id", s(&format!("fixture_terminal_call_{suffix}"))),
                ("namespace", s("omh")),
                ("name", s("terminal_create")),
                ("arguments", s(&arguments.to_string())),
                ("status", s("completed")),
            ]),
        )
    } else if last.text_field("type") == Ok("function_call_output")
        && let Some(suffix) = last
            .text_field("call_id")?
            .strip_prefix("fixture_terminal_call_")
    {
        let output = Json::parse(last.text_field("output")?)?;
        if output
            .field("receipt")?
            .field("effect")?
            .text_field("state")?
            != "applied"
        {
            return Err("fixture_terminal_create");
        }
        let part = obj(vec![
            ("type", s("output_text")),
            ("text", s("Demo terminal opened. View it with /terminals.")),
        ]);
        (
            format!("fixture_terminal_done_{suffix}"),
            obj(vec![
                ("type", s("message")),
                ("id", s(&format!("fixture_terminal_message_{suffix}"))),
                ("role", s("assistant")),
                ("status", s("completed")),
                ("content", Json::array(vec![part])?),
            ]),
        )
    } else {
        return Ok(None);
    };
    native::load(&obj(vec![
        ("request", request.save()?),
        ("id", s(&id)),
        ("model", s(&request.binding.returned_model)),
        ("items", Json::array(vec![item])?),
    ]))
    .map(Some)
}
pub fn compact_retained(
    request: &Request,
    prefix: usize,
    stop: &Cancel,
) -> Result<CompactedContext> {
    if stop.cancelled() {
        return Err("offline_cancelled");
    }
    let summary = summary_request_at(request, prefix)?;
    let output = obj(vec![
        ("type", s("message")),
        ("id", s("fixture_summary_message")),
        ("role", s("assistant")),
        ("status", s("completed")),
        (
            "content",
            Json::array(vec![obj(vec![
                ("type", s("output_text")),
                ("text", s("Earlier fixture turns were summarized.")),
            ])])?,
        ),
    ]);
    let response = native::load(&obj(vec![
        ("request", summary.save()?),
        ("id", s("fixture_summary_response")),
        ("model", s(&summary.binding.returned_model)),
        ("items", Json::array(vec![output])?),
    ]))?;
    CompactedContext::from_summary_at(request.clone(), &response, prefix)
}

pub fn compact(request: &Request, stop: &Cancel) -> Result<CompactedContext> {
    if stop.cancelled() {
        return Err("offline_cancelled");
    }
    let digest = sha256(Json::array(request.input.clone())?.dump().as_bytes());
    let mut output = vec![];
    if let Some(user) = request
        .input
        .iter()
        .rev()
        .find(|v| v.has("role") && v.text_field("role") == Ok("user"))
    {
        output.push(user.clone());
    }
    output.push(obj(vec![
        ("type", s("compaction")),
        ("id", s(&format!("mock_compact_{}", &digest[..16]))),
        (
            "encrypted_content",
            s(&format!("synthetic_compacted_state_{digest}")),
        ),
    ]));
    CompactedContext::admit(
        request.clone(),
        obj(vec![
            ("object", s("response.compaction")),
            ("id", s(&format!("mock_compaction_{}", &digest[..16]))),
            ("output", Json::array(output)?),
        ]),
    )
}
