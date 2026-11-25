//! Admission receipt and exact serialized continuation construction.
use crate::{
    protocol::json::Json,
    provider::{request::Request, responses::AdmittedResponse},
};
#[derive(Debug)]
pub struct AdmissionReceipt {
    request_fingerprint: String,
}
pub(crate) fn receipt(request: &Request) -> Result<AdmissionReceipt, &'static str> {
    Ok(AdmissionReceipt {
        request_fingerprint: request.fingerprint()?,
    })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    Succeeded,
    Failed,
    Cancelled,
    Denied,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effects {
    NotStarted,
    KnownResult,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    operation_id: String,
    call_id: String,
    output: String,
    execution: Execution,
    effects: Effects,
}
impl ToolResult {
    pub(crate) fn new(
        operation_id: String,
        call_id: String,
        output: String,
        execution: Execution,
        effects: Effects,
    ) -> Self {
        Self {
            operation_id,
            call_id,
            output,
            execution,
            effects,
        }
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn call_id(&self) -> &str {
        &self.call_id
    }
    pub fn output(&self) -> &str {
        &self.output
    }
    pub fn execution(&self) -> Execution {
        self.execution
    }
    pub fn effects(&self) -> Effects {
        self.effects
    }
}
#[derive(Debug)]
/// Raw peer data cannot create a proof of completed tool results.
/// ```compile_fail,E0277
/// use ansa::provider::replay::KnownBatchResults;
/// fn needs_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// needs_deserialize::<KnownBatchResults>();
/// ```
pub struct KnownBatchResults {
    response_binding: String,
    results: Vec<ToolResult>,
}
impl KnownBatchResults {
    pub(crate) fn new(response_binding: String, results: Vec<ToolResult>) -> Self {
        Self {
            response_binding,
            results,
        }
    }
    pub fn results(&self) -> &[ToolResult] {
        &self.results
    }
}
pub(crate) fn response_binding(response: &AdmittedResponse) -> Result<String, &'static str> {
    let items = response
        .items
        .iter()
        .map(|item| item.value().clone())
        .collect::<Vec<_>>();
    let calls = response
        .calls
        .iter()
        .map(|call| {
            Json::object(vec![
                ("item_id", Json::string(&call.item_id)?),
                ("call_id", Json::string(&call.call_id)?),
                ("name", Json::string(&call.name)?),
                ("arguments", Json::string(&call.arguments)?),
                ("output_index", Json::parse(&call.output_index.to_string())?),
            ])
        })
        .collect::<Result<Vec<_>, _>>()?;
    let value = Json::object(vec![
        ("request", Json::string(&response.request.fingerprint()?)?),
        ("id", Json::string(&response.id)?),
        ("returned_model", Json::string(&response.returned_model)?),
        ("items", Json::array(items)?),
        ("calls", Json::array(calls)?),
    ])?;
    Ok(crate::protocol::legacy_json_v1::sha256(
        value.dump().as_bytes(),
    ))
}
pub fn continuation(
    response: &AdmittedResponse,
    results: &KnownBatchResults,
) -> Result<Request, &'static str> {
    if response.receipt.request_fingerprint != response.request.fingerprint()?
        || results.response_binding != response_binding(response)?
        || results.results.len() != response.calls.len()
    {
        return Err("replay_admission");
    }
    append_results(response, &results.results, None)
}

pub(crate) fn recovery_continuation(
    response: &AdmittedResponse,
    results: &[ToolResult],
    unknown: &str,
) -> Result<Request, &'static str> {
    if response.receipt.request_fingerprint != response.request.fingerprint()?
        || results.len() != response.calls.len()
        || results
            .iter()
            .filter(|r| r.effects == Effects::Unknown)
            .count()
            != 1
    {
        return Err("replay_admission");
    }
    append_results(response, results, Some(unknown))
}

pub(crate) const RECOVERY_NOTICE: &str = "Continue the original task. The tool marked unknown may have had partial effects. Inspect the current state first. Do not repeat that invocation. The remaining calls from that tool batch were skipped.";

fn append_results(
    response: &AdmittedResponse,
    results: &[ToolResult],
    unknown: Option<&str>,
) -> Result<Request, &'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut input = response.request.input.clone();
    input.extend(response.items.iter().map(|i| i.value().clone()));
    for (call, result) in response.calls.iter().zip(results) {
        if result.call_id != call.call_id
            || result.operation_id.is_empty()
            || !seen.insert(&result.operation_id)
            || (result.effects == Effects::Unknown && Some(result.operation_id.as_str()) != unknown)
        {
            return Err("replay_result_mismatch");
        };
        input.push(Json::object(vec![
            ("type", Json::string("function_call_output")?),
            ("call_id", Json::string(&result.call_id)?),
            ("output", Json::string(&result.output)?),
        ])?);
    }
    if unknown.is_some() {
        input.push(Json::object(vec![
            ("role", Json::string("user")?),
            ("content", Json::string(RECOVERY_NOTICE)?),
        ])?);
    }
    response.request.clone().with_input(input)
}
