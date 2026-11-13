//! Exact v1 result/invocation codec. Responses use the M2 production admission.
use super::{Result, need, num};
pub use crate::provider::native::{load as load_response, save as save_response};
pub use crate::tools::invocation::Invocation;
use crate::{
    protocol::json::Json,
    provider::{
        replay::{
            Effects, Execution, KnownBatchResults, ToolResult, continuation, response_binding,
        },
        request::Request,
        responses::AdmittedResponse,
    },
};
#[derive(Clone, Debug)]
pub struct SavedResult {
    pub result: ToolResult,
    pub dispatched: bool,
}
impl SavedResult {
    pub fn load(value: &Json) -> Result<Self> {
        let execution = match value.usize_field("execution")? {
            0 => Execution::Succeeded,
            1 => Execution::Failed,
            2 => Execution::Cancelled,
            3 => Execution::Denied,
            _ => return Err("session_outcome"),
        };
        let effects = match value.usize_field("effects")? {
            0 => Effects::NotStarted,
            1 => Effects::KnownResult,
            2 => Effects::Unknown,
            _ => return Err("session_outcome"),
        };
        let output = value.text_field("output")?;
        need(output.len() <= 128 * 1024, "session_outcome_limit")?;
        let dispatched = value.field("dispatched")?.boolean()?;
        need(
            dispatched || effects == Effects::NotStarted && execution != Execution::Succeeded,
            "session_outcome_dispatch",
        )?;
        Ok(Self {
            result: ToolResult::new(
                value.text_field("operation")?.into(),
                value.text_field("call")?.into(),
                output.into(),
                execution,
                effects,
            ),
            dispatched,
        })
    }
    pub fn save(&self) -> Result<Json> {
        Json::object(vec![
            ("operation", Json::string(self.result.operation_id())?),
            ("call", Json::string(self.result.call_id())?),
            ("output", Json::string(self.result.output())?),
            ("execution", num(self.result.execution() as usize)?),
            ("effects", num(self.result.effects() as usize)?),
            (
                "dispatched",
                Json::parse(if self.dispatched { "true" } else { "false" })?,
            ),
        ])
    }
}
pub fn response_text(response: &AdmittedResponse) -> Result<String> {
    let mut text = String::new();
    for item in response.items() {
        let item = item.value();
        if item.text_field("type")? != "message" {
            continue;
        }
        for part in item.field("content")?.elements()? {
            let value = part.text_field(if part.text_field("type")? == "refusal" {
                "refusal"
            } else {
                "text"
            })?;
            need(value.len() <= 64 * 1024 - text.len(), "runtime_text_limit")?;
            text.push_str(value);
        }
    }
    Ok(text)
}
pub(crate) fn next_request(
    response: &AdmittedResponse,
    results: &[SavedResult],
) -> Result<Request> {
    let known = KnownBatchResults::new(
        response_binding(response)?,
        results.iter().map(|r| r.result.clone()).collect(),
    );
    continuation(response, &known)
}
