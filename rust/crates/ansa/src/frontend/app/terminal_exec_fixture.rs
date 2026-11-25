//! Synthetic provider used by the attached-shell PTY acceptance.
use crate::frontend::Result;
use serde_json::{Value, json};

pub(super) fn items(input: &[Value], index: usize) -> Result<Vec<Value>> {
    let start = input
        .iter()
        .rposition(|row| row["role"] == "user")
        .ok_or("fixture_exec_input")?;
    let prompt = input[start]["content"]
        .as_str()
        .ok_or("fixture_exec_input")?;
    let call = |name: &str, arguments: Value| {
        vec![json!({"type":"function_call","id":format!("item-{index}"),
            "call_id":format!("call-{index}"),"namespace":"omh","name":name,
            "arguments":arguments.to_string(),"status":"completed"})]
    };
    let done = || {
        vec![json!({"type":"message","id":format!("message-{index}"),
            "role":"assistant","status":"completed","content":[{"type":"output_text","text":"Fixture complete."}]})]
    };
    let results: Vec<Value> = input[start + 1..]
        .iter()
        .filter(|row| row["type"] == "function_call_output")
        .map(|row| {
            serde_json::from_str(row["output"].as_str().ok_or("fixture_exec_output")?)
                .map_err(|_| "fixture_exec_output")
        })
        .collect::<Result<_>>()?;
    if prompt == "TEST_EXEC_LEGACY_DIFF" {
        return Ok(match results.as_slice() {
            [] => call("terminal_list", json!({})),
            [list] => {
                let terminal = &list["terminals"][0];
                call(
                    "terminal_capture",
                    json!({
                        "terminal":terminal["terminal"],"epoch":terminal["epoch"],
                        "after":terminal["visible_after"]
                    }),
                )
            }
            [list, _] => {
                let terminal = &list["terminals"][0];
                call(
                    "terminal_input",
                    json!({
                        "terminal":terminal["terminal"],"epoch":terminal["epoch"],
                        "encoding":"utf8","data":"git diff --stat\r"
                    }),
                )
            }
            [list, capture, _, reads @ ..]
                if reads.len() < 3
                    && !reads.iter().any(|read| {
                        read["output"]["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("1 file changed"))
                    }) =>
            {
                let terminal = &list["terminals"][0];
                let cursor =
                    reads.last().map_or(
                        &capture["screen"]["through"],
                        |read| &read["output"]["through"],
                    );
                call(
                    "terminal_read",
                    json!({
                        "terminal":terminal["terminal"],"epoch":terminal["epoch"],"after":cursor,"limit":4096
                    }),
                )
            }
            _ => done(),
        });
    }
    if prompt == "TEST_EXEC_SPLIT" {
        let lines = ["if true; then\r", "cd ke", "pt\r", "fi\r"];
        let Some(data) = lines.get(results.len()) else {
            return Ok(done());
        };
        let terminal = input[..start]
            .iter()
            .rev()
            .filter(|row| row["type"] == "function_call_output")
            .filter_map(|row| serde_json::from_str::<Value>(row["output"].as_str()?).ok())
            .find_map(|row| row["terminal"].as_str().map(str::to_owned))
            .ok_or("fixture_exec_terminal")?;
        return Ok(call(
            "terminal_input",
            json!({
                "terminal":terminal,"epoch":1,"encoding":"utf8","data":data
            }),
        ));
    }
    if !results.is_empty() {
        return Ok(done());
    }
    if prompt.starts_with("TEST_EXEC_CLOSE_") {
        let terminal = input[..start]
            .iter()
            .rev()
            .filter(|row| row["type"] == "function_call_output")
            .filter_map(|row| serde_json::from_str::<Value>(row["output"].as_str()?).ok())
            .find_map(|row| row["terminal"].as_str().map(str::to_owned))
            .ok_or("fixture_exec_terminal")?;
        return Ok(call(
            "terminal_close",
            json!({"terminal":terminal,"epoch":1}),
        ));
    }
    if prompt == "TEST_EXEC_CREATE_BASH" {
        return Ok(call(
            "terminal_create",
            json!({
                "label":"Shell","cwd":".","argv":["bash"],"rows":24,"columns":80,
                "lifetime_seconds":0,"idle_seconds":0
            }),
        ));
    }
    let command = match prompt {
        "TEST_EXEC_DIFF"
        | "TEST_EXEC_DIFF_WARM"
        | "TEST_EXEC_DIFF_REOPEN"
        | "TEST_EXEC_DIFF_MANUAL" => "git diff --stat",
        "TEST_EXEC_SET" => "trap - URG; declare -x ANSA_EXEC_FIXTURE=remembered",
        "TEST_EXEC_STATE" => r#"printf '%s:%s\n' "${PWD##*/}" "$ANSA_EXEC_FIXTURE""#,
        "TEST_EXEC_LONG" => r#"printf 'START\n'; sleep 0.8; printf 'END\n'"#,
        "TEST_EXEC_FAIL" => "sh check-fixture.sh",
        "TEST_EXEC_BACKGROUND" => {
            r#"(sleep 0.05; printf '%064000d\n' 0; printf 'alive\n' > child-marker) & printf 'now\n'"#
        }
        _ => return Err("fixture_exec_prompt"),
    };
    Ok(call(
        crate::terminal_host::tools::EXEC,
        json!({"command":command}),
    ))
}
