//! Synthetic provider for exercising search continuation through Carbon Plan.
use serde_json::{Value, json};

pub(super) fn item(request: &Value, input: &[Value], index: usize) -> Result<Value, &'static str> {
    let user = input
        .iter()
        .rev()
        .find(|entry| entry["role"] == "user")
        .and_then(|entry| entry["content"].as_str())
        .ok_or("fixture_search_user")?;
    let (query, read_file) = if user.contains("side_chat_parent") {
        ("fn side_chat_parent", "workbench.rs")
    } else if user.contains("@build.log") {
        ("LINKER_FAILURE_SENTINEL", "build.log")
    } else {
        return Err("fixture_search_prompt");
    };
    let last = input.last().ok_or("fixture_search_user")?;
    let token = request["instructions"]
        .as_str()
        .and_then(|text| text.split_once("instruction_token is "))
        .and_then(|(_, suffix)| suffix.split_once('.'))
        .map(|(token, _)| token)
        .ok_or("fixture_search_instructions")?;
    let mut cursor = "";
    let result: Value;
    if last["type"] == "function_call_output" {
        result = serde_json::from_str(last["output"].as_str().ok_or("fixture_search_output")?)
            .map_err(|_| "fixture_search_output")?;
        let id = last["call_id"].as_str().ok_or("fixture_search_output")?;
        if id.starts_with("fixture-read-") {
            let text = result["content"].as_str().ok_or("fixture_search_read")?;
            if !text.starts_with(query) {
                return Err("fixture_search_read");
            }
            return Ok(json!({
                "type":"message","id":format!("fixture-search-done-{index}"),
                "role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":format!("FOUND: {text}")}]
            }));
        }
        let matches = result["matches"]
            .as_array()
            .ok_or("fixture_search_result")?;
        if let Some(hit) = matches.first() {
            let path = hit["path"].as_str().ok_or("fixture_search_path")?;
            if !path.ends_with(read_file) {
                return Err("fixture_search_path");
            }
            let offset = hit["offset"].as_u64().ok_or("fixture_search_offset")?;
            return Ok(call(
                "read_file",
                format!("fixture-read-{index}"),
                json!({
                    "path": path, "offset": offset, "max_bytes": 128, "instruction_token": token,
                }),
            ));
        }
        cursor = result["next_cursor"]
            .as_str()
            .ok_or("fixture_search_end_without_match")?;
    }
    Ok(call(
        "search_files",
        format!("fixture-search-{index}"),
        json!({
            "path":".", "query":query, "mode":"content", "max_results":1,
            "instruction_token":token, "cursor":cursor,
        }),
    ))
}

fn call(name: &str, id: String, args: Value) -> Value {
    json!({
        "type":"function_call","id":format!("item-{id}"),"call_id":id,
        "namespace":"omh","name":name,"arguments":args.to_string(),"status":"completed"
    })
}
