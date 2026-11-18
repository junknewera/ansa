//! Pinned OpenRouter route shared by Carbon and the bounded acceptance runner.
use crate::{
    auth::{
        Cancel, Secret,
        manager::{Exchange, HttpReply, HttpRequest},
    },
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        native,
        request::{Binding, Descriptor, Request},
        responses::AdmittedResponse,
        structured::FinalFormat,
    },
};
use serde_json::{Value, json};
use std::{
    sync::{Mutex, TryLockError},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;

pub(crate) use crate::provider::request::OPENROUTER_MODEL as MODEL;
pub(crate) const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const ENDPOINT: &str = "fireworks/us";
pub(crate) const PROVIDER: &str = "Fireworks";
pub(crate) const API: &str = "https://openrouter.ai/api/v1/chat/completions";
pub(crate) const CATALOG: &str =
    "https://openrouter.ai/api/v1/models/deepseek/deepseek-v4.1-flash-1017/endpoints";
#[cfg(feature = "orchestration-pilot")]
pub(crate) const KEY: &str = "https://openrouter.ai/api/v1/key";

pub(crate) struct OpenRouter {
    key: Secret,
    binding: Binding,
    exchange: Exchange,
    route: Mutex<Route>,
}

#[derive(Default)]
struct Route {
    catalog_retry_at: Option<Instant>,
    catalog_verified: bool,
    window: Option<usize>,
    strict_verified: bool,
}

impl OpenRouter {
    pub(crate) fn new(key: Secret, exchange: Exchange) -> Result<Self> {
        let token = key.expose();
        if token.len() < 24
            || token.len() > 512
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("openrouter_key_invalid");
        }
        let account = format!(
            "openrouter-key-{}",
            sha256(format!("ansa/openrouter/v1:{token}").as_bytes())
        );
        Ok(Self {
            key,
            binding: Binding {
                provider: "openrouter".into(),
                account,
                requested_model: MODEL.into(),
                returned_model: MODEL.into(),
            },
            exchange,
            route: Mutex::new(Route::default()),
        })
    }

    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }

    fn send(
        &self,
        method: &'static str,
        url: &'static str,
        body: Option<Value>,
        stop: &Cancel,
    ) -> Result<Value> {
        stop.check()?;
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        if body.len() > MAX_REQUEST_BYTES {
            return Err("openrouter_request_size");
        }
        let reply = (self.exchange)(
            &HttpRequest {
                method,
                url,
                content_type: "application/json",
                body: Secret::new(body),
                bearer: (url == API).then(|| self.key.clone()),
                body_limit: 512 * 1024,
                timeout_ms: if url == CATALOG { 10_000 } else { 60_000 },
            },
            stop,
        )?;
        read_reply(reply)
    }

    pub(crate) fn require_strict(&self, stop: &Cancel) -> Result<()> {
        self.prepare_route(true, stop).map(|_| ())
    }

    fn prepare_route(&self, strict: bool, stop: &Cancel) -> Result<Option<usize>> {
        let mut route = loop {
            stop.check()?;
            match self.route.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::WouldBlock) => stop.wait(Duration::from_millis(20)),
                Err(TryLockError::Poisoned(_)) => return Err("openrouter_route_state"),
            }
        };
        if !route.catalog_verified
            && (strict
                || route
                    .catalog_retry_at
                    .is_none_or(|retry| Instant::now() >= retry))
        {
            let catalog = self.send("GET", CATALOG, None, stop).and_then(|response| {
                let endpoint = catalog_endpoint(&response)?;
                Ok(endpoint["context_length"]
                    .as_u64()
                    .and_then(|window| usize::try_from(window).ok())
                    .filter(|window| *window >= 2))
            });
            stop.check()?;
            match catalog {
                Ok(window) => {
                    route.window = window;
                    route.catalog_verified = true;
                }
                Err(error) => {
                    route.catalog_retry_at = Some(Instant::now() + Duration::from_secs(30));
                    if strict {
                        return Err(error);
                    }
                }
            }
        }
        if strict && !route.strict_verified {
            verify_probe(&self.send("POST", API, Some(probe_body()), stop)?)?;
            route.strict_verified = true;
        }
        Ok(route.window)
    }

    pub(crate) fn generate(&self, request: &Request, stop: &Cancel) -> Result<AdmittedResponse> {
        if request.binding != self.binding || request.effort.is_some() {
            return Err("openrouter_binding");
        }
        let window = self.prepare_route(request.final_format().is_some(), stop)?;
        let mut body = base_body(request, &request.epoch.instructions)?;
        if !request.tools.descriptors.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .descriptors
                    .iter()
                    .map(tool_value)
                    .collect::<Result<_>>()?,
            );
        }
        // The tool turn is complete on its own. A plain stop here only selects
        // the strict-final request; that unconstrained text is never admitted.
        if request.final_format() == Some(FinalFormat::ChildResult)
            && !request.tools.descriptors.is_empty()
        {
            let response = self.send("POST", API, Some(body.clone()), stop)?;
            if choice(&response)?["finish_reason"] == "tool_calls" {
                return decode(request, &response)
                    .map(|response| response.with_context_window(window));
            }
            final_choice(&response)?;
        }
        if request.final_format().is_some() {
            apply_final_format(request, &mut body)?;
            if !request.tools.descriptors.is_empty() {
                body["tool_choice"] = json!("none");
            }
        }
        let response = self.send("POST", API, Some(body), stop)?;
        let admitted = decode(request, &response)?;
        if request.final_format().is_some() && !admitted.calls().is_empty() {
            return Err("openrouter_final_has_tools");
        }
        Ok(admitted.with_context_window(window))
    }
}

pub(crate) fn read_reply(reply: HttpReply) -> Result<Value> {
    if matches!(reply.head.status, 401 | 403) {
        return Err("openrouter_auth");
    }
    if reply.head.status != 200
        || reply.head.content_types.len() != 1
        || reply.head.content_types[0].split(';').next() != Some("application/json")
    {
        return Err("openrouter_http");
    }
    let strict = Json::parse_with_limits(&reply.body, 512 * 1024, 64, 131072)?;
    serde_json::from_str(&strict.dump()).map_err(|_| "openrouter_json")
}

pub(crate) fn tool_value(descriptor: &Descriptor) -> Result<Value> {
    let schema: Value =
        serde_json::from_str(&descriptor.schema.dump()).map_err(|_| "openrouter_tool_schema")?;
    Ok(
        json!({"type":"function","function":{"name":descriptor.name,"description":descriptor.description,"parameters":schema}}),
    )
}

pub(crate) fn apply_final_format(request: &Request, body: &mut Value) -> Result<()> {
    let wire: Value =
        serde_json::from_str(&request.json()?.dump()).map_err(|_| "openrouter_wire")?;
    let format = &wire["text"]["format"];
    body["response_format"] = json!({"type":"json_schema","json_schema":{"name":format["name"],"strict":true,"schema":format["schema"]}});
    Ok(())
}

#[cfg(feature = "orchestration-pilot")]
pub(crate) fn verify_catalog(response: &Value) -> Result<()> {
    catalog_endpoint(response).map(|_| ())
}

fn catalog_endpoint(response: &Value) -> Result<&Value> {
    let data = response.get("data").ok_or("openrouter_catalog")?;
    if data.get("id").and_then(Value::as_str) != Some(MODEL) {
        return Err("openrouter_catalog");
    }
    let endpoints = data
        .get("endpoints")
        .and_then(Value::as_array)
        .ok_or("openrouter_catalog")?;
    let mut matches = endpoints
        .iter()
        .filter(|endpoint| endpoint["tag"] == ENDPOINT);
    let endpoint = matches.next().ok_or("openrouter_catalog_endpoint")?;
    let parameters = endpoint["supported_parameters"]
        .as_array()
        .ok_or("openrouter_catalog_parameters")?;
    if matches.next().is_some()
        || endpoint["provider_name"] != PROVIDER
        || ![
            "structured_outputs",
            "response_format",
            "tools",
            "tool_choice",
        ]
        .iter()
        .all(|name| parameters.iter().any(|value| value == *name))
    {
        return Err("openrouter_catalog_parameters");
    }
    Ok(endpoint)
}

pub(crate) fn route() -> Value {
    json!({"only":[ENDPOINT],"order":[ENDPOINT],"allow_fallbacks":false,"require_parameters":true})
}

pub(crate) fn probe_body() -> Value {
    let challenge = json!({"type":"json_schema","json_schema":{"name":"ansa_native_schema_probe","strict":true,"schema":{
        "type":"object","properties":{"value":{"type":"string","enum":["schema-enforced"]}},
        "required":["value"],"additionalProperties":false
    }}});
    json!({
        "model": MODEL, "provider": route(), "max_tokens": 256, "reasoning":{"enabled":false},
        "messages": [{"role":"user","content":"Answer with the JSON object {\"value\":\"replace-this\"}. Never use the phrase schema-enforced."}],
        "response_format": challenge,
    })
}

pub(crate) fn verify_probe(value: &Value) -> Result<()> {
    let message = final_choice(value)?;
    let answer = Json::parse(message["content"].as_str().ok_or("openrouter_probe")?)
        .map_err(|_| "openrouter_probe")?;
    if answer != Json::parse(r#"{"value":"schema-enforced"}"#)? {
        return Err("openrouter_schema_enforcement");
    }
    Ok(())
}

pub(crate) fn base_body(request: &Request, instructions: &str) -> Result<Value> {
    let mut messages = vec![json!({"role":"system","content":instructions})];
    let mut calls = Vec::new();
    for item in &request.input {
        let value: Value = serde_json::from_str(&item.dump()).map_err(|_| "openrouter_input")?;
        if value["type"] == "function_call" {
            calls.push(json!({"type":"function","id":value["call_id"],"function":{"name":value["name"],"arguments":value["arguments"]}}));
            continue;
        }
        if !calls.is_empty() {
            messages.push(json!({"role":"assistant","tool_calls":std::mem::take(&mut calls)}));
        }
        if value["type"] == "function_call_output" {
            messages.push(
                json!({"role":"tool","tool_call_id":value["call_id"],"content":value["output"]}),
            );
        } else if (value["role"] == "user"
            || value["role"] == "assistant" && value["type"].is_null())
            && value["content"].is_string()
        {
            messages.push(json!({"role":value["role"],"content":value["content"]}));
        } else if value["type"] == "message" && value["role"] == "assistant" {
            let parts = value["content"].as_array().ok_or("openrouter_input")?;
            if parts.len() != 1 || parts[0]["type"] != "output_text" {
                return Err("openrouter_input");
            }
            messages.push(json!({"role":"assistant","content":parts[0]["text"]}));
        } else {
            return Err("openrouter_input");
        }
    }
    if !calls.is_empty() {
        return Err("openrouter_input");
    }
    Ok(
        json!({"model":MODEL, "provider":route(), "messages":messages, "max_tokens":4096, "reasoning":{"enabled":false}}),
    )
}

fn choice(response: &Value) -> Result<&Value> {
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .ok_or("openrouter_model")?;
    if model != MODEL {
        return Err("openrouter_model");
    }
    if response["provider"] != PROVIDER || !response["error"].is_null() {
        return Err("openrouter_provider");
    }
    let choices = response["choices"].as_array().ok_or("openrouter_choice")?;
    if choices.len() != 1
        || !choices[0]["error"].is_null()
        || choices[0]["message"]["role"] != "assistant"
        || !choices[0]["message"]["refusal"].is_null()
        || !choices[0]["message"]["reasoning"].is_null() && choices[0]["message"]["reasoning"] != ""
        || !choices[0]["message"]["reasoning_details"].is_null()
            && choices[0]["message"]["reasoning_details"] != json!([])
    {
        return Err("openrouter_choice");
    }
    Ok(&choices[0])
}

pub(crate) fn final_choice(response: &Value) -> Result<&Value> {
    let choice = choice(response)?;
    let calls = &choice["message"]["tool_calls"];
    if choice["finish_reason"] != "stop"
        || !(calls.is_null() || calls.as_array().is_some_and(Vec::is_empty))
    {
        return Err("openrouter_incomplete");
    }
    Ok(&choice["message"])
}

pub(crate) fn decode(request: &Request, response: &Value) -> Result<AdmittedResponse> {
    let choice = choice(response)?;
    let id = response["id"]
        .as_str()
        .filter(|id| crate::provider::contracts::valid_id(id))
        .ok_or("openrouter_identity")?;
    let mut items = Vec::new();
    if choice["finish_reason"] == "stop" {
        let text = final_choice(response)?["content"]
            .as_str()
            .ok_or("openrouter_content")?;
        items.push(json!({"type":"message","id":format!("msg_{id}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}));
    } else if choice["finish_reason"] == "tool_calls" {
        let message = &choice["message"];
        if !message["content"].is_null()
            && !message["content"]
                .as_str()
                .is_some_and(|text| text.trim().is_empty())
        {
            return Err("openrouter_mixed_output");
        }
        let calls = message["tool_calls"]
            .as_array()
            .ok_or("openrouter_tool_calls")?;
        if calls.is_empty() || calls.len() > 16 {
            return Err("openrouter_tool_calls");
        }
        for (index, call) in calls.iter().enumerate() {
            if call["type"] != "function" {
                return Err("openrouter_tool_calls");
            }
            items.push(json!({"type":"function_call","id":format!("item_{index}_{id}"),"call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"],"namespace":"omh","status":"completed"}));
        }
    } else {
        return Err("openrouter_incomplete");
    }
    let envelope = Json::object(vec![
        ("request", request.save()?),
        ("id", Json::string(id)?),
        ("model", Json::string(MODEL)?),
        ("items", Json::parse(&Value::Array(items).to_string())?),
    ])?;
    let usage = &response["usage"];
    let tokens = usage["prompt_tokens"].as_u64().and_then(|input| {
        input
            .checked_add(usage["completion_tokens"].as_u64()?)
            .filter(|_| input > 0)
            .and_then(|total| usize::try_from(total).ok())
    });
    native::load(&envelope).map(|response| response.with_token_usage(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::legacy_compact::{CompactedContext, summary_request_at},
        provider::request::Epoch,
        transport::http::Head,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn response(content: &str) -> Value {
        json!({"id":"fixture_response","model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":content}}]})
    }

    fn reply(body: Value) -> HttpReply {
        HttpReply {
            head: Head {
                status: 200,
                content_types: vec!["application/json".into()],
            },
            body: body.to_string(),
        }
    }

    fn request(route: &OpenRouter) -> Request {
        Request::new(
            route.binding().clone(),
            Epoch {
                id: "fixture".into(),
                instructions: "fixture instructions".into(),
                exact_state: Json::parse("{}").unwrap(),
            },
            crate::frontend::app::pure_tools()
                .unwrap()
                .advertisement()
                .clone(),
            vec![],
        )
        .unwrap()
    }

    #[test]
    fn strict_child_rejects_provider_errors_and_discards_unconstrained_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exchange = {
            let calls = calls.clone();
            Arc::new(move |request: &HttpRequest, _: &Cancel| {
                let call = calls.fetch_add(1, Ordering::AcqRel);
                if call == 0 {
                    assert_eq!(request.url, CATALOG);
                    assert!(request.bearer.is_none());
                    return Ok(reply(
                        json!({"data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"context_length":1000000,"supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}}),
                    ));
                }
                let body: Value = serde_json::from_str(request.body.expose()).unwrap();
                assert_eq!(body["provider"], route());
                let value = match call {
                    1 => {
                        assert_eq!(
                            body["response_format"]["json_schema"]["name"],
                            "ansa_native_schema_probe"
                        );
                        response(r#"{"value":"schema-enforced"}"#)
                    }
                    2 => {
                        assert!(body.get("response_format").is_none());
                        json!({"id":"failed_tool","model":MODEL,"provider":PROVIDER,"choices":[{"error":{"message":"fixture"},"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"type":"function","id":"one","function":{"name":"get_test_value","arguments":"{}"}}]}}]})
                    }
                    3 => {
                        assert!(body.get("response_format").is_none());
                        response("drop this unstructured response")
                    }
                    4 => {
                        assert_eq!(body["tool_choice"], "none");
                        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
                        assert!(!body.to_string().contains("drop this unstructured response"));
                        let mut value =
                            response(r#"{"status":"completed","summary":"Checked","findings":[]}"#);
                        value["usage"] = json!({"prompt_tokens":499990,"completion_tokens":10});
                        value
                    }
                    _ => panic!("unexpected request"),
                };
                Ok(reply(value))
            })
        };
        let route = OpenRouter::new(
            Secret::new("synthetic-fixture-key-0123456789".into()),
            exchange,
        )
        .unwrap();
        let request = request(&route).with_final_format(Some(FinalFormat::ChildResult));
        let stop = Cancel::default();
        assert!(matches!(
            route.generate(&request, &stop),
            Err("openrouter_choice")
        ));
        let answer = route.generate(&request, &stop).unwrap();
        assert_eq!(answer.child_result().unwrap().summary, "Checked");
        assert_eq!(answer.context_usage(), Some((500000, 1000000)));
        assert_eq!(calls.load(Ordering::Acquire), 5);
        assert_eq!(
            verify_probe(&response(r#"{"value":"wrong","value":"schema-enforced"}"#)),
            Err("openrouter_probe")
        );
    }

    #[test]
    fn native_compaction_remains_valid_on_the_openrouter_wire() {
        let route = OpenRouter::new(
            Secret::new("synthetic-fixture-key-0123456789".into()),
            Arc::new(|_, _| Err("unexpected_request")),
        )
        .unwrap();
        let original = request(&route)
            .with_input(vec![
                Json::parse(r#"{"role":"user","content":"earlier input"}"#).unwrap(),
                Json::parse(r#"{"role":"user","content":"new input"}"#).unwrap(),
            ])
            .unwrap();
        let summary = summary_request_at(&original, 1).unwrap();
        let admitted = decode(&summary, &response("Earlier task context.")).unwrap();
        let compacted = CompactedContext::from_summary_at(original.clone(), &admitted, 1).unwrap();
        let replay = original
            .with_input(
                compacted
                    .native()
                    .field("output")
                    .unwrap()
                    .elements()
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
        let body = base_body(&replay, &replay.epoch.instructions).unwrap();
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .ends_with("Earlier task context.")
        );
        assert_eq!(body["messages"][2]["content"], "new input");
    }
}
