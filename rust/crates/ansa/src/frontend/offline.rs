//! Deterministic C++-compatible mock, admitted through the production decoder.
use super::Result;
use crate::{
    auth::Cancel,
    context::legacy_compact::CompactedContext,
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{native, request::Request, responses::AdmittedResponse},
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
