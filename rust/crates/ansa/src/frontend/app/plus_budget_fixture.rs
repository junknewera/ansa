//! Offline SIWC responses for the Carbon tool budget regression.
use crate::frontend::Result;
use serde_json::{Value, json};

pub(super) fn items(request: &Value, input: &[Value], index: usize) -> Result<Vec<Value>> {
    let task = input
        .iter()
        .rev()
        .find(|v| v["role"] == "user")
        .and_then(|v| v["content"].as_str())
        .ok_or("fixture_input")?;
    let format = request["text"]["format"]["name"].as_str();
    let result = |summary| {
        json!({"status":"completed","summary":summary,"findings":["report.txt verified"]})
            .to_string()
    };
    let text = match format {
        Some("ansa_orchestration_plan_v4") => json!({
            "schema_version":1, "requested_outcome":"implementation","summary":"Budget regression",
            "steps":[{"id":"edit","task":"Read and update report.txt","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"report.txt contains UPDATED"}],
            "checks":[{"path":"report.txt","contains":"UPDATED"}],"command_checks":[{"argv":["git","diff","--check"]}],"manual_checks":[],"read_folder":null
        }).to_string(),
        Some("ansa_child_result_v1") if task.starts_with("Assigned task") && request["model"] == "gpt-6.1-luna" => {
            let done = input.iter().filter(|v| v["type"] == "function_call_output").count();
            if done == 66 {
                result("Updated report.txt")
            } else {
                let batch = match done { 0 | 16 | 32 => 16, 48 => 15, 63 => 2, 65 => 1, _ => return Err("fixture_budget_history") };
                let token = request["instructions"].as_str().and_then(|s| s.split_once("instruction_token is "))
                    .and_then(|(_, s)| s.split_once('.')).map(|(t, _)| t).ok_or("fixture_instruction_token")?;
                return Ok((done..done+batch).map(|n| {
                    let (name, args) = if n == 65 {
                        ("edit_file", json!({"path":"report.txt","old_text":"ORIGINAL","new_text":"UPDATED","instruction_token":token}))
                    } else {
                        ("read_file", json!({"path":"report.txt","offset":n,"max_bytes":1,"instruction_token":token}))
                    };
                    json!({"type":"function_call","id":format!("budget-item-{n}"),"call_id":format!("budget-call-{n}"),
                        "namespace":"omh","name":name,"arguments":args.to_string(),"status":"completed"})
                }).collect());
            }
        }
        Some("ansa_child_result_v1") if task.starts_with("Evaluate the worker") && request["model"] == "gpt-6.1-sol" => result("Worker result verified"),
        Some("ansa_child_result_v1") if task.starts_with("Perform the whole candidate's") && task.contains(r#""failed_checks":[]"#) && request["model"] == "gpt-6.2-sol" => result("Candidate accepted"),
        None => "BUDGET_WORK_COMPLETE".into(),
        _ => return Err("fixture_budget_stage"),
    };
    Ok(vec![
        json!({"type":"message","id":format!("budget-message-{index}"),"role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":text}]}),
    ])
}
