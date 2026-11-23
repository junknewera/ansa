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
    collections::BTreeMap,
    sync::{Mutex, TryLockError},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;

pub(crate) use crate::provider::request::OPENROUTER_MODEL as MODEL;
pub(crate) const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const ROUTE_CHECK_TIMEOUT_MS: u64 = 60_000;
pub(crate) const ENDPOINT: &str = "fireworks/us";
pub(crate) const PROVIDER: &str = "Fireworks";
pub(crate) const ROLE_CATALOG: &str = "https://openrouter.ai/api/v1/models?providers=fireworks&supported_parameters=structured_outputs,tools,tool_choice";
fn endpoint_tag(model: &str) -> &'static str {
    if model == MODEL {
        ENDPOINT
    } else {
        "fireworks"
    }
}
pub(crate) const API: &str = "https://openrouter.ai/api/v1/chat/completions";
#[cfg(any(
    test,
    feature = "orchestration-pilot",
    feature = "frontend-test-adapter"
))]
pub(crate) const CATALOG: &str =
    "https://openrouter.ai/api/v1/models/deepseek/deepseek-v4.1-flash-1017/endpoints";
#[cfg(feature = "orchestration-pilot")]
pub(crate) const KEY: &str = "https://openrouter.ai/api/v1/key";

pub(crate) struct OpenRouter {
    key: Secret,
    binding: Binding,
    exchange: Exchange,
    route: Mutex<BTreeMap<String, Route>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RouteCheck {
    BestEffortCatalog,
    RequireCatalog,
    RequireStrict,
}

#[derive(Default)]
struct Route {
    catalog_retry_at: Option<Instant>,
    catalog_verified: bool,
    window: Option<usize>,
    strict_verified: bool,
    accepts_effort: bool,
    accepts_images: bool,
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
            route: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }

    pub(crate) fn accepts_binding(&self, binding: &Binding) -> bool {
        binding.provider == self.binding.provider
            && binding.account == self.binding.account
            && crate::provider::request::openrouter_model_id(&binding.requested_model)
            && binding.requested_model == binding.returned_model
    }

    pub(crate) fn role_models(&self, stop: &Cancel) -> Result<Vec<(String, String, Vec<String>)>> {
        let response = self.send("GET", ROLE_CATALOG, None, Some(10_000), stop)?;
        let rows = response["data"].as_array().ok_or("openrouter_catalog")?;
        let mut models = BTreeMap::new();
        for row in rows {
            let Some(id) = row["id"]
                .as_str()
                .filter(|id| crate::provider::request::openrouter_model_id(id))
            else {
                continue;
            };
            let Some(parameters) = row["supported_parameters"].as_array() else {
                continue;
            };
            if !["structured_outputs", "tools", "tool_choice"]
                .iter()
                .all(|p| parameters.iter().any(|v| v == *p))
            {
                continue;
            }
            let label = row["name"].as_str().unwrap_or(id);
            if label.len() > 256 || label.chars().any(char::is_control) {
                return Err("openrouter_catalog");
            }
            let efforts = if parameters.iter().any(|v| v == "reasoning_effort") {
                ["low", "medium", "high"].map(str::to_owned).to_vec()
            } else {
                Vec::new()
            };
            if models
                .insert(id.to_owned(), (id.to_owned(), label.to_owned(), efforts))
                .is_some()
                || models.len() > 256
            {
                return Err("openrouter_catalog");
            }
        }
        if models.is_empty() {
            return Err("openrouter_catalog_empty");
        }
        Ok(models.into_values().collect())
    }

    pub(crate) fn cached_window(&self, request: &Request) -> Option<usize> {
        if !self.accepts_binding(&request.binding) {
            return None;
        }
        self.route
            .try_lock()
            .ok()?
            .get(&request.binding.requested_model)?
            .window
    }

    fn send(
        &self,
        method: &'static str,
        url: &str,
        body: Option<Value>,
        timeout_ms: Option<u64>,
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
                timeout_ms,
            },
            stop,
        )?;
        read_reply(reply)
    }

    pub(crate) fn require_strict(&self, request: &Request, stop: &Cancel) -> Result<()> {
        self.prepare_route(
            &request.binding.requested_model,
            request.effort.as_deref(),
            RouteCheck::RequireStrict,
            request.has_image_input(),
            stop,
        )
        .map(|_| ())
    }

    pub(crate) fn validate_role(
        &self,
        selection: &crate::provider::contracts::GenerationSelection,
        stop: &Cancel,
    ) -> Result<()> {
        if !crate::provider::request::openrouter_model_id(&selection.model) {
            return Err("openrouter_model");
        }
        self.prepare_route(
            &selection.model,
            selection.effort.as_deref(),
            RouteCheck::RequireCatalog,
            false,
            stop,
        )
        .map(|_| ())
    }

    fn prepare_route(
        &self,
        model: &str,
        effort: Option<&str>,
        check: RouteCheck,
        needs_images: bool,
        stop: &Cancel,
    ) -> Result<Option<usize>> {
        let require_catalog = needs_images || check != RouteCheck::BestEffortCatalog;
        let mut routes = loop {
            stop.check()?;
            match self.route.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::WouldBlock) => stop.wait(Duration::from_millis(20)),
                Err(TryLockError::Poisoned(_)) => return Err("openrouter_route_state"),
            }
        };
        if routes.len() >= 256 && !routes.contains_key(model) {
            return Err("openrouter_catalog_limit");
        }
        let route = routes.entry(model.to_owned()).or_default();
        if !route.catalog_verified
            && (require_catalog
                || route
                    .catalog_retry_at
                    .is_none_or(|retry| Instant::now() >= retry))
        {
            let timeout = if require_catalog {
                ROUTE_CHECK_TIMEOUT_MS
            } else {
                10_000
            };
            let url = format!("https://openrouter.ai/api/v1/models/{model}/endpoints");
            let catalog = self
                .send("GET", &url, None, Some(timeout), stop)
                .and_then(|response| {
                    let endpoint = catalog_endpoint(&response, model)?;
                    route.accepts_images = response["data"]["architecture"]["input_modalities"]
                        .as_array()
                        .is_some_and(|modes| modes.iter().any(|mode| mode == "image"));
                    route.accepts_effort = endpoint["supported_parameters"]
                        .as_array()
                        .is_some_and(|v| v.iter().any(|p| p == "reasoning_effort"));
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
                    if require_catalog {
                        return Err(error);
                    }
                }
            }
        }
        if needs_images && !route.accepts_images {
            return Err("openrouter_image_unsupported");
        }
        if effort.is_some_and(|v| !route.accepts_effort || !matches!(v, "low" | "medium" | "high"))
        {
            return Err("openrouter_effort");
        }
        if check == RouteCheck::RequireStrict && !route.strict_verified {
            let mut probe = probe_body();
            probe["model"] = json!(model);
            probe["provider"] = route_for(model);
            verify_probe_for(
                &self.send("POST", API, Some(probe), Some(ROUTE_CHECK_TIMEOUT_MS), stop)?,
                model,
            )?;
            route.strict_verified = true;
        }
        Ok(route.window)
    }

    pub(crate) fn generate(&self, request: &Request, stop: &Cancel) -> Result<AdmittedResponse> {
        if !self.accepts_binding(&request.binding) {
            return Err("openrouter_binding");
        }
        let model = &request.binding.requested_model;
        let check = if request.final_format().is_some() {
            RouteCheck::RequireStrict
        } else {
            RouteCheck::BestEffortCatalog
        };
        let window = self.prepare_route(
            model,
            request.effort.as_deref(),
            check,
            request.has_image_input(),
            stop,
        )?;
        let tokens = request
            .estimated_tokens()
            .unwrap_or_else(|| crate::context::estimate_request_tokens(request));
        if let Some(window) = window {
            crate::context::require_response_room(tokens, window)?;
        }
        let attach_context = |response: AdmittedResponse| {
            response
                .with_context_window(window)
                .with_context_estimate(tokens)
        };
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
            let response = self.send("POST", API, Some(body.clone()), None, stop)?;
            if choice(&response, model)?["finish_reason"] == "tool_calls" {
                return decode(request, &response).map(attach_context);
            }
            final_choice_for(&response, model)?;
        }
        if request.final_format().is_some() {
            apply_final_format(request, &mut body)?;
            if !request.tools.descriptors.is_empty() {
                body["tool_choice"] = json!("none");
            }
        }
        let response = self.send("POST", API, Some(body), None, stop)?;
        let admitted = decode(request, &response)?;
        if request.final_format().is_some() && !admitted.calls().is_empty() {
            return Err("openrouter_final_has_tools");
        }
        Ok(attach_context(admitted))
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
    catalog_endpoint(response, MODEL).map(|_| ())
}

fn catalog_endpoint<'a>(response: &'a Value, model: &str) -> Result<&'a Value> {
    let data = response.get("data").ok_or("openrouter_catalog")?;
    if data.get("id").and_then(Value::as_str) != Some(model) {
        return Err("openrouter_catalog");
    }
    let endpoints = data
        .get("endpoints")
        .and_then(Value::as_array)
        .ok_or("openrouter_catalog")?;
    let mut matches = endpoints
        .iter()
        .filter(|endpoint| endpoint["tag"] == endpoint_tag(model));
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
    route_for(MODEL)
}
fn route_for(model: &str) -> Value {
    let endpoint = endpoint_tag(model);
    json!({"only":[endpoint],"order":[endpoint],"allow_fallbacks":false,"require_parameters":true})
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

#[cfg(any(test, feature = "orchestration-pilot"))]
pub(crate) fn verify_probe(value: &Value) -> Result<()> {
    verify_probe_for(value, MODEL)
}

fn verify_probe_for(value: &Value, model: &str) -> Result<()> {
    let message = final_choice_for(value, model)?;
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
        } else if value["role"] == "user" && value["content"].is_array() {
            let parts = value["content"].as_array().ok_or("openrouter_input")?;
            let (text, images) = parts.split_first().ok_or("openrouter_input")?;
            if text["type"] != "input_text"
                || images.is_empty()
                || images.len() > crate::provider::contracts::MAX_IMAGES
            {
                return Err("openrouter_input");
            }
            let mut content = vec![
                json!({"type":"text","text":text["text"].as_str().ok_or("openrouter_input")?}),
            ];
            for image in images {
                let url = image["image_url"].as_str().ok_or("openrouter_input")?;
                if image["type"] != "input_image"
                    || !(url.starts_with("data:image/png;base64,")
                        || url.starts_with("data:image/jpeg;base64,"))
                {
                    return Err("openrouter_input");
                }
                content.push(json!({"type":"image_url","image_url":{"url":url}}));
            }
            messages.push(json!({"role":"user","content":content}));
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
        json!({"model":request.binding.requested_model, "provider":route_for(&request.binding.requested_model), "messages":messages, "max_tokens":4096, "reasoning":request.effort.as_ref().map_or_else(|| json!({"enabled":false}), |effort| json!({"effort":effort,"exclude":true}))}),
    )
}

fn choice<'a>(response: &'a Value, expected: &str) -> Result<&'a Value> {
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .ok_or("openrouter_model")?;
    if model != expected {
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

fn final_choice_for<'a>(response: &'a Value, expected: &str) -> Result<&'a Value> {
    let choice = choice(response, expected)?;
    let calls = &choice["message"]["tool_calls"];
    if choice["finish_reason"] != "stop"
        || !(calls.is_null() || calls.as_array().is_some_and(Vec::is_empty))
    {
        return Err("openrouter_incomplete");
    }
    Ok(&choice["message"])
}

pub(crate) fn decode(request: &Request, response: &Value) -> Result<AdmittedResponse> {
    let choice = choice(response, &request.binding.returned_model)?;
    let id = response["id"]
        .as_str()
        .filter(|id| crate::provider::contracts::valid_id(id))
        .ok_or("openrouter_identity")?;
    let mut items = Vec::new();
    if choice["finish_reason"] == "stop" {
        let text = final_choice_for(response, &request.binding.returned_model)?["content"]
            .as_str()
            .ok_or("openrouter_content")?;
        items.push(json!({"type":"message","id":format!("msg_{id}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}));
    } else if choice["finish_reason"] == "tool_calls" {
        let message = &choice["message"];
        // The tool phase of a structured child can include explanatory text.
        // Only the validated tool calls advance the turn. The final phase
        // still requires the provider-enforced child result schema.
        if !message["content"].is_null()
            && !message["content"].as_str().is_some_and(|text| {
                text.trim().is_empty() || request.final_format() == Some(FinalFormat::ChildResult)
            })
        {
            return Err("openrouter_mixed_output");
        }
        let calls = message["tool_calls"]
            .as_array()
            .ok_or("openrouter_tool_calls")?;
        if calls.is_empty() || calls.len() > crate::protocol::sse::Limits::default().items {
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
        ("model", Json::string(&request.binding.returned_model)?),
        ("items", Json::parse(&Value::Array(items).to_string())?),
    ])?;
    let usage = &response["usage"];
    let count = |value: &Value| usize::try_from(value.as_u64()?).ok();
    let reported = count(&usage["prompt_tokens"]).and_then(|input| {
        native::ReportedUsage::new(
            input,
            count(&usage["prompt_tokens_details"]["cached_tokens"]),
            count(&usage["completion_tokens"])?,
        )
    });
    native::load(&envelope).map(|response| response.with_reported_usage(reported))
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
    fn accepts_twenty_calls_in_one_response_and_replays_them() {
        let router = OpenRouter::new(
            Secret::new("synthetic-fixture-key-0123456789".into()),
            Arc::new(|_, _| unreachable!()),
        )
        .unwrap();
        let calls: Vec<_> = (0..20).map(|n| json!({"id":format!("call{n}"),"type":"function","function":{"name":"get_test_value","arguments":"{}"}})).collect();
        let response = json!({"id":"batch","model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":calls}}]});
        let batch = decode(&request(&router), &response).unwrap();
        assert_eq!(batch.calls().len(), 20);
        assert_eq!(
            native::load(&native::save(&batch).unwrap())
                .unwrap()
                .calls(),
            batch.calls()
        );
    }
    #[test]
    fn route_without_catalog_window_keeps_admitted_estimate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exchange = {
            let calls = calls.clone();
            Arc::new(move |request: &HttpRequest, _: &Cancel| {
                calls.fetch_add(1, Ordering::Relaxed);
                if request.url == CATALOG {
                    assert_eq!(request.timeout_ms, Some(10_000));
                    return Ok(reply(json!({"error":"catalog fixture unavailable"})));
                }
                assert_eq!(request.url, API);
                assert_eq!(request.timeout_ms, None);
                assert_eq!(request.body_limit, 512 * 1024);
                Ok(reply(response("done")))
            })
        };
        let route = OpenRouter::new(
            Secret::new("synthetic-fixture-key-0123456789".into()),
            exchange,
        )
        .unwrap();
        let request = request(&route);
        let estimate = crate::context::estimate_request_tokens(&request);
        let response = route.generate(&request, &Cancel::default()).unwrap();
        let response = native::load(&native::save(&response).unwrap()).unwrap();
        assert_eq!(response.request().estimated_tokens(), Some(estimate));
        assert!(response.context_budget().is_none());
        assert!(response.reported_usage().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn selected_role_keeps_model_account_provider_and_effort_bound() {
        let model = "author/worker";
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let router = OpenRouter::new(Secret::new("synthetic-fixture-key-0123456789".into()), Arc::new(move |req, _| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let data = match index {
                0 => {
                    assert_eq!(req.url, ROLE_CATALOG);
                    assert_eq!(req.timeout_ms, Some(10_000));
                    assert!(req.bearer.is_none());
                    json!({"data":[{"id":model, "name":"Worker", "supported_parameters":["structured_outputs","response_format","tools","tool_choice","reasoning_effort"]}]})
                }
                1 => {
                    assert!(req.url.ends_with("/author/worker/endpoints") && req.bearer.is_none());
                    json!({"data":{"id":model,"endpoints":[{"tag":"fireworks","provider_name":PROVIDER,"supported_parameters":["structured_outputs","response_format","tools","tool_choice","reasoning_effort"]}]}})
                }
                2..=4 => {
                    let body: Value = serde_json::from_str(req.body.expose()).unwrap();
                    assert_eq!(body["model"], model);
                    assert_eq!(body["provider"], route_for(model));
                    let mut value = if index == 2 {
                        response(r#"{"value":"schema-enforced"}"#)
                    } else {
                        assert_eq!(body["reasoning"], json!({"effort":"medium","exclude":true}));
                        response(r#"{"status":"completed","summary":"ok","findings":[]}"#)
                    };
                    if index != 4 { value["model"] = json!(model); }
                    value
                }
                _ => panic!("unexpected call"),
            };
            Ok(reply(data))
        })).unwrap();
        let stop = Cancel::default();
        assert_eq!(router.role_models(&stop).unwrap()[0].0, model);
        let mut req = request(&router)
            .with_effort(Some("medium".into()))
            .unwrap()
            .with_final_format(Some(FinalFormat::ChildResult));
        req.tools.descriptors.clear();
        req.binding.requested_model = model.into();
        req.binding.returned_model = model.into();
        assert_eq!(
            router
                .generate(&req, &stop)
                .unwrap()
                .child_result()
                .unwrap()
                .status,
            crate::provider::structured::ChildStatus::Completed
        );
        assert!(matches!(
            router.generate(&req, &stop),
            Err("openrouter_model")
        ));
        req.binding.account.push('x');
        assert!(matches!(
            router.generate(&req, &stop),
            Err("openrouter_binding")
        ));
        assert_eq!(count.load(Ordering::SeqCst), 5);
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
                    assert_eq!(request.timeout_ms, Some(ROUTE_CHECK_TIMEOUT_MS));
                    assert!(request.bearer.is_none());
                    return Ok(reply(
                        json!({"data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"context_length":1000000,"supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}}),
                    ));
                }
                let body: Value = serde_json::from_str(request.body.expose()).unwrap();
                assert_eq!(body["provider"], route());
                assert_eq!(
                    request.timeout_ms,
                    (call == 1).then_some(ROUTE_CHECK_TIMEOUT_MS)
                );
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
                        json!({"id":"review_tool","model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"I will read the candidate.","tool_calls":[{"type":"function","id":"one","function":{"name":"get_test_value","arguments":"{}"}}]}}]})
                    }
                    3 => {
                        assert!(body.get("response_format").is_none());
                        json!({"id":"failed_tool","model":MODEL,"provider":PROVIDER,"choices":[{"error":{"message":"fixture"},"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"type":"function","id":"one","function":{"name":"get_test_value","arguments":"{}"}}]}}]})
                    }
                    4 => {
                        assert!(body.get("response_format").is_none());
                        response("drop this unstructured response")
                    }
                    5 => {
                        assert_eq!(body["tool_choice"], "none");
                        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
                        assert!(!body.to_string().contains("drop this unstructured response"));
                        let mut value =
                            response(r#"{"status":"completed","summary":"Checked","findings":[]}"#);
                        value["usage"] = json!({"prompt_tokens":499990,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":31000}});
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
        let tool = route.generate(&request, &stop).unwrap();
        assert_eq!(tool.calls()[0].name, "get_test_value");
        assert_eq!(tool.items().len(), 1);
        assert_eq!(
            tool.items()[0].value().text_field("type"),
            Ok("function_call")
        );
        assert!(matches!(
            route.generate(&request, &stop),
            Err("openrouter_choice")
        ));
        let answer = route.generate(&request, &stop).unwrap();
        assert_eq!(answer.child_result().unwrap().summary, "Checked");
        let usage = native::ReportedUsage::new(499990, Some(31000), 10);
        assert_eq!(answer.reported_usage(), usage);
        assert_eq!(
            native::load(&native::save(&answer).unwrap())
                .unwrap()
                .reported_usage(),
            usage
        );
        assert!(tool.reported_usage().is_none());
        let legacy = native::save(&answer)
            .unwrap()
            .without("reported_usage_v1")
            .unwrap();
        let legacy = native::load(&legacy).unwrap();
        assert_eq!(legacy.token_usage(), Some(500_000));
        assert!(legacy.reported_usage().is_none());
        for output in [0, 5] {
            let mut raw = response("known output");
            raw["usage"] = json!({
                "prompt_tokens": 0, "completion_tokens": output,
                "prompt_tokens_details": {"cached_tokens": 0},
            });
            let req = self::request(&route);
            let zero_response = decode(&req, &raw)
                .unwrap()
                .with_context_window(Some(1_000_000));
            let zero_response = native::load(&native::save(&zero_response).unwrap()).unwrap();
            assert_eq!(
                zero_response.reported_usage(),
                native::ReportedUsage::new(0, Some(0), output)
            );
            assert_eq!(zero_response.context_budget().unwrap().tokens, None);
        }
        assert_eq!(
            answer.context_budget(),
            Some(native::ContextBudget {
                window: 1_000_000,
                tokens: Some(500_000)
            })
        );
        stop.cancel();
        assert!(matches!(
            route.generate(&request, &stop),
            Err("auth_cancelled")
        ));
        assert_eq!(calls.load(Ordering::Acquire), 6);
        assert_eq!(
            verify_probe(&response(r#"{"value":"wrong","value":"schema-enforced"}"#)),
            Err("openrouter_probe")
        );
    }

    #[test]
    fn native_image_bytes_survive_both_wires_and_following_turn() {
        const WHITE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC";
        const BLACK: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let http = Arc::new(AtomicUsize::new(0));
        let observed = http.clone();
        let exchange = Arc::new(move |req: &HttpRequest, _: &Cancel| {
            if req.url == CATALOG {
                return Ok(reply(
                    json!({"data":{"id":MODEL, "architecture":{"input_modalities":["text","image"]},
                    "endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,
                    "supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}}),
                ));
            }
            let wire: Value = serde_json::from_str(req.body.expose()).unwrap();
            let messages = wire["messages"].as_array().unwrap();
            assert_eq!(
                messages[1],
                json!({"role":"user","content":[
                    {"type":"text","text":"describe"}, {"type":"image_url","image_url":{"url":WHITE}},
                    {"type":"image_url","image_url":{"url":BLACK}}
                ]})
            );
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                assert_eq!(messages.len(), 2);
            } else {
                assert_eq!(messages.len(), 4);
                assert_eq!(
                    messages[2],
                    json!({"role":"assistant","content":"first answer"})
                );
                assert_eq!(messages[3], json!({"role":"user","content":"follow up"}));
            }
            Ok(reply(response("first answer")))
        });
        let route = OpenRouter::new(
            Secret::new("synthetic-fixture-key-0123456789".into()),
            exchange,
        )
        .unwrap();
        let image = json!({"role":"user","content":[
            {"type":"input_text","text":"describe"}, {"type":"input_image","image_url":WHITE},
            {"type":"input_image","image_url":BLACK}
        ]});
        let image_inputs = [WHITE, BLACK]
            .map(|url| crate::provider::contracts::ImageInput::from_data_url(url).unwrap());
        let user = Json::object(vec![
            ("role", Json::string("user").unwrap()),
            (
                "content",
                crate::provider::contracts::image::content("describe", &image_inputs).unwrap(),
            ),
        ])
        .unwrap();
        let mut req = request(&route).with_input(vec![user]).unwrap();
        for followup in [false, true] {
            let mut siwc = req.clone();
            siwc.binding.provider = "openai-siwc".into();
            let wire: Value = serde_json::from_str(&siwc.json().unwrap().dump()).unwrap();
            assert_eq!(wire["input"][0], image);
            assert_eq!(
                wire["input"].as_array().unwrap().len(),
                if followup { 3 } else { 1 }
            );
            if followup {
                assert_eq!(wire["input"][1]["content"][0]["text"], "first answer");
                assert_eq!(wire["input"][2]["content"], "follow up");
            }
            let admitted = route.generate(&req, &Cancel::default()).unwrap();
            if !followup {
                let mut next = req.input.clone();
                next.push(admitted.items()[0].value().clone());
                next.push(Json::parse(r#"{"role":"user","content":"follow up"}"#).unwrap());
                req = Request::load(&req.with_input(next).unwrap().save().unwrap()).unwrap();
            }
        }
        assert_eq!(http.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn image_request_stops_before_post_on_text_only_route() {
        let route = OpenRouter::new(Secret::new("synthetic-fixture-key-0123456789".into()), Arc::new(|req, _| {
            assert_eq!(req.url, CATALOG);
            Ok(reply(json!({"data":{"id":MODEL,"architecture":{"input_modalities":["text"]},
                "endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,
                "supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}})))
        })).unwrap();
        let req = request(&route).with_input(vec![Json::parse(r#"{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}]}"#).unwrap()]).unwrap();
        assert!(matches!(
            route.generate(&req, &Cancel::default()),
            Err("openrouter_image_unsupported")
        ));
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
