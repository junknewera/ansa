//! Synthetic provider for a direct delegation followed by a test in the lead's shell.
use crate::frontend::Result;
use serde_json::{Value, json};

pub(super) fn items(request: &Value, input: &[Value], index: usize) -> Result<Vec<Value>> {
    let call = |name: &str, args: Value| {
        vec![json!({"type":"function_call","id":format!("item-{index}"),
            "call_id":format!("call-{index}"),"namespace":"omh","name":name,
            "arguments":args.to_string(),"status":"completed"})]
    };
    let message = |text: &str| {
        vec![
            json!({"type":"message","id":format!("message-{index}"),"role":"assistant",
            "status":"completed","content":[{"type":"output_text","text":text}]}),
        ]
    };
    let outcomes = input
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| {
            serde_json::from_str(item["output"].as_str().ok_or("fixture_output")?)
                .map_err(|_| "fixture_output")
        })
        .collect::<Result<Vec<Value>>>()?;
    if request["text"]["format"]["name"] == "ansa_child_result_v1" {
        let token = request["instructions"]
            .as_str()
            .and_then(|s| s.split_once("instruction_token is "))
            .and_then(|(_, s)| s.split_once('.'))
            .map(|(token, _)| token)
            .ok_or("fixture_instruction_token")?;
        let read = |path| {
            call(
                "read_file",
                json!({"path":path,"offset":0,"max_bytes":256,"instruction_token":token}),
            )
        };
        let edit = |old| {
            call(
                "edit_file",
                json!({"path":"report.txt","old_text":old,"new_text":"after fixture","instruction_token":token}),
            )
        };
        return Ok(match outcomes.as_slice() {
            [] => read("check.sh"),
            [test]
                if test["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("report.txt")) =>
            {
                edit("stale fixture")
            }
            [_, rejected]
                if rejected["error"] == "file_preimage_missing" && rejected["applied"] == false =>
            {
                read("report.txt")
            }
            [_, _, page]
                if page["content"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("before fixture\n")) =>
            {
                edit("before fixture")
            }
            [_, _, _, applied]
                if applied["applied"] == true && applied["change"]["after"] == "after fixture" =>
            {
                message(
                    &json!({"status":"completed","summary":"Updated report.txt","findings":[]})
                        .to_string(),
                )
            }
            _ => return Err("fixture_child_edit_sequence"),
        });
    }
    let actions = match outcomes.as_slice() {
        [] => call(
            "terminal_create",
            json!({"label":"Test shell","cwd":".","argv":["sh","-i"],"rows":24,"columns":100,"lifetime_seconds":0,"idle_seconds":0}),
        ),
        [shell] if shell["receipt"]["effect"]["state"] == "applied" => call(
            "delegate_task",
            json!({"task":"Update report.txt to satisfy check.sh","paths":["report.txt"],"read_paths":["check.sh"]}),
        ),
        [shell, report] if report["status"] == "completed" => call(
            "terminal_input",
            json!({
                "terminal":shell["terminal"],"epoch":shell["epoch"],"encoding":"utf8",
                "data":"git --no-pager diff --no-color -- report.txt && sh check.sh\r"
            }),
        ),
        [shell, _, delivery, reads @ ..]
            if delivery["receipt"]["effect"]["state"] == "delivered" =>
        {
            let text = reads
                .last()
                .and_then(|read| read["output"]["text"].as_str())
                .unwrap_or("");
            if text.contains("+after fixture") && text.contains("DELEGATED_EDIT_PASS") {
                message("DELEGATED_EDIT_VERIFIED")
            } else if reads.len() < 20 {
                call(
                    "terminal_read",
                    json!({"terminal":shell["terminal"],"epoch":shell["epoch"],"after":0,"limit":8192}),
                )
            } else {
                return Err("fixture_shell_result");
            }
        }
        _ => return Err("fixture_lead_edit_sequence"),
    };
    Ok(actions)
}
