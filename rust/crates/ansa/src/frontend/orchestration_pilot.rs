//! Feature-gated, metered OpenRouter pilot. The normal CLI has no such route.
use super::Result;
use crate::{
    auth::{
        Cancel, Secret,
        manager::{Exchange, HttpRequest},
    },
    protocol::json::Json,
    provider::{
        native,
        request::{Binding, Request},
        responses::AdmittedResponse,
    },
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

pub const MODEL: &str = "deepseek/deepseek-v4.1-flash-1017";
const ENDPOINT: &str = "fireworks/us";
const PROVIDER: &str = "Fireworks";
const API: &str = "https://openrouter.ai/api/v1/chat/completions";
const KEY: &str = "https://openrouter.ai/api/v1/key";
const CATALOG: &str =
    "https://openrouter.ai/api/v1/models/deepseek/deepseek-v4.1-flash-1017/endpoints";
const CALLS: u8 = 12;
const DURATION: Duration = Duration::from_secs(600);

pub(super) struct Pilot {
    key: Secret,
    exchange: Exchange,
    calls: AtomicU8,
    deadline: Instant,
}

impl Pilot {
    fn authorize(key: Secret, exchange: Exchange, stop: &Cancel) -> Result<Arc<Self>> {
        let meter = Self {
            key,
            exchange,
            calls: AtomicU8::new(0),
            deadline: Instant::now() + DURATION,
        };
        let catalog = meter.exchange("GET", CATALOG, None, stop)?;
        verify_catalog(&catalog)?;
        let credits = meter.exchange("GET", KEY, None, stop)?;
        verify_credit_limit(&credits)?;
        let challenge = json!({"type":"json_schema","json_schema":{"name":"ansa_native_schema_probe","strict":true,"schema":{
            "type":"object","properties":{"value":{"type":"string","enum":["schema-enforced"]}},
            "required":["value"],"additionalProperties":false
        }}});
        let value = meter.exchange("POST", API, Some(json!({
            "model": MODEL, "provider": route(), "max_tokens": 256, "reasoning":{"enabled":false},
            "messages": [{"role":"user","content":"Answer with the JSON object {\"value\":\"replace-this\"}. Never use the phrase schema-enforced."}],
            "response_format": challenge,
        })), stop)?;
        let message = final_choice(&value)?;
        let answer: Value = serde_json::from_str(
            message
                .get("content")
                .and_then(Value::as_str)
                .ok_or("pilot_probe")?,
        )
        .map_err(|_| "pilot_probe")?;
        if answer != json!({"value":"schema-enforced"}) {
            return Err("pilot_schema_enforcement");
        }
        Ok(Arc::new(meter))
    }

    fn exchange(
        &self,
        method: &'static str,
        url: &'static str,
        body: Option<Value>,
        stop: &Cancel,
    ) -> Result<Value> {
        let timeout = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or("pilot_deadline")?;
        stop.check()?;
        self.calls
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |calls| {
                (calls < CALLS).then_some(calls + 1)
            })
            .map_err(|_| "pilot_request_budget")?;
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        if body.len() > 256 * 1024 {
            return Err("pilot_request_size");
        }
        let reply = (self.exchange)(
            &HttpRequest {
                method,
                url,
                content_type: "application/json",
                body: Secret::new(body),
                bearer: (url != CATALOG).then(|| self.key.clone()),
                body_limit: 512 * 1024,
                timeout_ms: (timeout.as_millis() as u64).clamp(1, 75_000),
            },
            stop,
        )?;
        if reply.head.status != 200
            || reply.head.content_types.len() != 1
            || reply.head.content_types[0].split(';').next() != Some("application/json")
        {
            return Err("pilot_http");
        }
        let strict = Json::parse_with_limits(&reply.body, 512 * 1024, 64, 131072)?;
        serde_json::from_str(&strict.dump()).map_err(|_| "pilot_json")
    }

    pub(super) fn binding(&self) -> Binding {
        Binding {
            provider: "openrouter-test".into(),
            account: "synthetic-openrouter-pilot".into(),
            requested_model: MODEL.into(),
            returned_model: MODEL.into(),
        }
    }

    pub(super) fn generate(&self, request: &Request, stop: &Cancel) -> Result<AdmittedResponse> {
        if request.binding != self.binding() {
            return Err("pilot_binding");
        }
        let body = encode(request)?;
        let response = self.exchange("POST", API, Some(body), stop)?;
        decode(request, &response)
    }
}

fn verify_catalog(response: &Value) -> Result<()> {
    let data = response.get("data").ok_or("pilot_catalog")?;
    if data.get("id").and_then(Value::as_str) != Some(MODEL) {
        return Err("pilot_catalog");
    }
    let endpoints = data
        .get("endpoints")
        .and_then(Value::as_array)
        .ok_or("pilot_catalog")?;
    let mut matches = endpoints
        .iter()
        .filter(|endpoint| endpoint["tag"] == ENDPOINT);
    let endpoint = matches.next().ok_or("pilot_catalog_endpoint")?;
    let parameters = endpoint["supported_parameters"]
        .as_array()
        .ok_or("pilot_catalog_parameters")?;
    if matches.next().is_some()
        || endpoint["provider_name"] != PROVIDER
        || !["structured_outputs", "response_format", "tools"]
            .iter()
            .all(|name| parameters.iter().any(|value| value == *name))
    {
        return Err("pilot_catalog_parameters");
    }
    Ok(())
}

fn verify_credit_limit(response: &Value) -> Result<()> {
    let data = response.get("data").ok_or("pilot_key_limit")?;
    let cap = data
        .get("limit")
        .and_then(Value::as_f64)
        .ok_or("pilot_key_limit")?;
    let balance = data
        .get("limit_remaining")
        .and_then(Value::as_f64)
        .ok_or("pilot_key_limit")?;
    if !(0.0 < balance && balance <= cap && cap <= 1.0) || !data["limit_reset"].is_null() {
        return Err("pilot_key_limit");
    }
    Ok(())
}

fn route() -> Value {
    json!({"only":[ENDPOINT],"order":[ENDPOINT],"allow_fallbacks":false,"require_parameters":true})
}

fn encode(request: &Request) -> Result<Value> {
    let mut messages = vec![json!({"role":"system","content":request.epoch.instructions})];
    let mut calls = Vec::new();
    for item in &request.input {
        let value: Value = serde_json::from_str(&item.dump()).map_err(|_| "pilot_input")?;
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
        } else if value["role"] == "user" && value["content"].is_string() {
            messages.push(json!({"role":"user","content":value["content"]}));
        } else if value["type"] == "message" && value["role"] == "assistant" {
            let parts = value["content"].as_array().ok_or("pilot_input")?;
            if parts.len() != 1 || parts[0]["type"] != "output_text" {
                return Err("pilot_input");
            }
            messages.push(json!({"role":"assistant","content":parts[0]["text"]}));
        } else {
            return Err("pilot_input");
        }
    }
    if !calls.is_empty() {
        return Err("pilot_input");
    }
    let mut body = json!({"model":MODEL, "provider":route(), "messages":messages, "max_tokens":4096, "reasoning":{"enabled":false}});
    if !request.tools.descriptors.is_empty() {
        body["tools"] = Value::Array(request.tools.descriptors.iter().map(|descriptor| {
            let schema: Value = serde_json::from_str(&descriptor.schema.dump()).map_err(|_| "pilot_tool_schema")?;
            Ok(json!({"type":"function","function":{"name":descriptor.name,"description":descriptor.description,"parameters":schema}}))
        }).collect::<Result<Vec<_>>>()?);
    }
    if request.final_format().is_some() {
        let wire: Value =
            serde_json::from_str(&request.json()?.dump()).map_err(|_| "pilot_wire")?;
        let format = &wire["text"]["format"];
        body["response_format"] = json!({"type":"json_schema","json_schema":{"name":format["name"],"strict":true,"schema":format["schema"]}});
    }
    Ok(body)
}

fn choice(response: &Value) -> Result<&Value> {
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .ok_or("pilot_model")?;
    if model != MODEL {
        return Err("pilot_model");
    }
    if response["provider"] != PROVIDER || !response["error"].is_null() {
        return Err("pilot_provider");
    }
    let choices = response["choices"].as_array().ok_or("pilot_choice")?;
    if choices.len() != 1
        || choices[0]["message"]["role"] != "assistant"
        || !choices[0]["message"]["refusal"].is_null()
        || !choices[0]["message"]["reasoning"].is_null() && choices[0]["message"]["reasoning"] != ""
        || !choices[0]["message"]["reasoning_details"].is_null()
            && choices[0]["message"]["reasoning_details"] != json!([])
    {
        return Err("pilot_choice");
    }
    Ok(&choices[0])
}

fn final_choice(response: &Value) -> Result<&Value> {
    let choice = choice(response)?;
    if choice["finish_reason"] != "stop" || !choice["message"]["tool_calls"].is_null() {
        return Err("pilot_incomplete");
    }
    Ok(&choice["message"])
}

fn decode(request: &Request, response: &Value) -> Result<AdmittedResponse> {
    let choice = choice(response)?;
    let id = response["id"]
        .as_str()
        .filter(|id| crate::provider::contracts::valid_id(id))
        .ok_or("pilot_identity")?;
    let mut items = Vec::new();
    if choice["finish_reason"] == "stop" {
        let text = final_choice(response)?["content"]
            .as_str()
            .ok_or("pilot_content")?;
        items.push(json!({"type":"message","id":format!("msg_{id}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}));
    } else if choice["finish_reason"] == "tool_calls" {
        let message = &choice["message"];
        if !message["content"].is_null() && message["content"].as_str() != Some("") {
            return Err("pilot_mixed_output");
        }
        let calls = message["tool_calls"].as_array().ok_or("pilot_tool_calls")?;
        if calls.is_empty() || calls.len() > 16 {
            return Err("pilot_tool_calls");
        }
        for (index, call) in calls.iter().enumerate() {
            if call["type"] != "function" {
                return Err("pilot_tool_calls");
            }
            items.push(json!({"type":"function_call","id":format!("item_{index}_{id}"),"call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"],"namespace":"omh","status":"completed"}));
        }
    } else {
        return Err("pilot_incomplete");
    }
    let envelope = Json::object(vec![
        ("request", request.save()?),
        ("id", Json::string(id)?),
        ("model", Json::string(MODEL)?),
        ("items", Json::parse(&Value::Array(items).to_string())?),
    ])?;
    native::load(&envelope)
}

/// Run the approved scenario on a newly created synthetic repository. The
/// launcher supplies the capped test key on a private pipe.
pub fn run() -> Result<()> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return Err("pilot_requires_key_pipe");
    }
    let mut token = String::new();
    std::io::stdin()
        .take(513)
        .read_to_string(&mut token)
        .map_err(|_| "pilot_key_pipe")?;
    let key = token.trim_end_matches(['\r', '\n']);
    if key.is_empty() || token.len() > 512 || key.bytes().any(|byte| !byte.is_ascii_graphic()) {
        return Err("pilot_key_pipe");
    }
    let stop = Cancel::default();
    let pilot = Pilot::authorize(
        Secret::new(key.into()),
        Arc::new(crate::transport::client::pilot_exchange),
        &stop,
    )?;
    drop(token);
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let watchdog_stop = stop.clone();
    let remaining = pilot.deadline.saturating_duration_since(Instant::now());
    let watchdog = std::thread::Builder::new()
        .name("ansa-pilot-deadline".into())
        .spawn(move || {
            if finished.recv_timeout(remaining) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                watchdog_stop.cancel();
            }
        })
        .map_err(|_| "pilot_deadline_watchdog")?;
    let result = scenario(&pilot, &stop);
    let _ = finish.send(());
    watchdog.join().map_err(|_| "pilot_deadline_watchdog")?;
    println!(
        "model={MODEL} endpoint={ENDPOINT} requests={} result={}",
        pilot.calls.load(Ordering::Acquire),
        result.err().unwrap_or("pass")
    );
    result
}

fn scenario(pilot: &Arc<Pilot>, stop: &Cancel) -> Result<()> {
    use super::{
        orchestration::{self, Proposal},
        sessions::{Factory, Kind},
    };
    use crate::{
        orchestration::plan::Access,
        runtime::{AgentMode, CommandPayload, EventPayload, PostStatus, RuntimeCommand, Submit},
    };
    use ring::rand::{SecureRandom, SystemRandom};
    use std::path::PathBuf;

    struct Sandbox(PathBuf);
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "pilot_random")?;
    let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let storage = PathBuf::from("/tmp").join(format!("ansa-openrouter-pilot-{nonce}"));
    ansa_linux::fs::create_private_leaf(&storage).map_err(|_| "pilot_storage")?;
    let storage = Sandbox(storage);
    let home = storage.0.join("home");
    let cwd = storage.0.join("repo");
    ansa_linux::fs::create_private_leaf(&home).map_err(|_| "pilot_storage")?;
    ansa_linux::fs::create_private_leaf(&cwd).map_err(|_| "pilot_storage")?;
    let git_at = |directory: &std::path::Path, args: &[&str]| match crate::process::git::run(
        directory,
        args,
        None,
        8192,
        Duration::from_secs(5),
        stop,
    )? {
        crate::process::git::GitOutput::Success(output) => Ok(output),
        _ => Err("pilot_git"),
    };
    let git = |args: &[&str]| git_at(&cwd, args);
    git(&["init", "--quiet", "--template="])?;
    let report = cwd.join("report.txt");
    std::fs::write(&report, "Report title: demo\nStatus: unverified\n")
        .map_err(|_| "pilot_fixture")?;
    git(&["add", "--", "report.txt"])?;
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@localhost",
        "commit",
        "--quiet",
        "-m",
        "Fixture",
    ])?;
    let before = git(&["status", "--short"])?;
    let head = git(&["rev-parse", "HEAD"])?;
    let index = git(&["ls-files", "--stage", "-z"])?;
    let settings = crate::config::load_agent_settings(&cwd, &home)?;
    let factory = Factory::new(settings, home.join("auth"), true, String::new(), None)
        .with_pilot(pilot.clone());
    let mut root = factory.create(Kind::Main, None)?.runtime;
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "pilot_reactor")?;
    let handle = root.handle();
    let command = |id: &str, payload| RuntimeCommand {
        schema_version: 1,
        session: handle.session().clone(),
        incarnation: handle.incarnation().clone(),
        command: id.into(),
        turn: id.into(),
        payload,
    };
    let seed = reactor.block_on(async {
        root.ready().await?;
        if handle
            .post(command("seed", CommandPayload::GetContext))
            .await?
            != PostStatus::Accepted
        {
            return Err("pilot_parent_context");
        }
        while let Some(event) = handle.next_event().await {
            if let EventPayload::ContextSnapshot { seed, .. } = event.payload {
                return Ok(seed);
            }
        }
        Err("pilot_parent_context")
    })?;
    let prompt = "In this synthetic repository update report.txt: replace Status: unverified with Status: verified. Use a single edit helper scoped exactly to report.txt. After the edit the coordinator will arrange an independent review. Keep the plan minimal. Do not change anything else.";
    let proposal = Proposal::generate(&factory, seed, prompt.into(), stop)?;
    let [step] = proposal.plan.steps() else {
        return Err("pilot_plan_scope");
    };
    if step.access != Access::Write || step.paths != ["report.txt"] {
        return Err("pilot_plan_scope");
    }
    let completed = orchestration::execute(&factory, proposal, stop)?;
    let candidate = completed
        .candidate
        .as_ref()
        .ok_or("pilot_missing_candidate")?;
    let commit = format!("{}\n", candidate.commit);
    if git_at(&candidate.workspace, &["rev-parse", "ansa-result"])? != commit.as_bytes()
        || git_at(&candidate.workspace, &["show", "ansa-result:report.txt"])?
            != b"Report title: demo\nStatus: verified\n"
        || git_at(
            &candidate.workspace,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-z",
                &candidate.commit,
            ],
        )? != b"report.txt\0"
        || std::fs::read_to_string(&report).map_err(|_| "pilot_fixture")?
            != "Report title: demo\nStatus: unverified\n"
        || git(&["status", "--short"])? != before
        || git(&["rev-parse", "HEAD"])? != head
        || git(&["ls-files", "--stage", "-z"])? != index
    {
        return Err("pilot_candidate");
    }
    reactor.block_on(async {
        let timelimit = pilot.deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(timelimit, async {
            if handle
                .post(command(
                    "summary",
                    CommandPayload::Handoff(Submit {
                        text: completed.evidence,
                        mode: AgentMode::Plan,
                    }),
                ))
                .await?
                != PostStatus::Accepted
            {
                return Err("pilot_handoff");
            }
            while let Some(event) = handle.next_event().await {
                match event.payload {
                    EventPayload::Done => return root.shutdown().await,
                    EventPayload::RuntimeError { .. } => return Err("pilot_handoff"),
                    _ => (),
                }
            }
            Err("pilot_handoff")
        })
        .await
        .map_err(|_| "pilot_deadline")?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_requires_the_exact_provider_variant_and_credit_ceiling() {
        let mut catalog = json!({"data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","tools","response_format"]}]}});
        assert_eq!(verify_catalog(&catalog), Ok(()));
        catalog["data"]["endpoints"][0]["tag"] = json!("fireworks");
        assert_eq!(verify_catalog(&catalog), Err("pilot_catalog_endpoint"));
        for credit in [json!(null), json!(1.01), json!(0)] {
            assert!(
                verify_credit_limit(&json!({"data":{"limit":credit,"limit_remaining":1}})).is_err()
            );
        }
        assert_eq!(
            verify_credit_limit(
                &json!({"data":{"limit":1,"limit_remaining":0.75,"limit_reset":null}})
            ),
            Ok(())
        );
        assert!(
            verify_credit_limit(
                &json!({"data":{"limit":1,"limit_remaining":0.75,"limit_reset":"daily"}})
            )
            .is_err()
        );
    }

    #[test]
    fn meter_never_sends_a_thirteenth_request() {
        let native_calls = Arc::new(AtomicU8::new(0));
        let calls = native_calls.clone();
        let meter = Pilot {
            key: Secret::new("dummy".into()),
            calls: AtomicU8::new(0),
            deadline: Instant::now() + DURATION,
            exchange: Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Ok(crate::auth::manager::HttpReply {
                    head: crate::transport::http::Head {
                        status: 200,
                        content_types: vec!["application/json".into()],
                    },
                    body: "{}".into(),
                })
            }),
        };
        for _ in 0..CALLS {
            meter
                .exchange("GET", KEY, None, &Cancel::default())
                .unwrap();
        }
        assert_eq!(
            meter.exchange("GET", KEY, None, &Cancel::default()),
            Err("pilot_request_budget")
        );
        assert_eq!(native_calls.load(Ordering::Acquire), CALLS);
    }

    #[test]
    fn malformed_or_refused_tool_output_cannot_become_a_native_batch() {
        let registry = crate::tools::registry::Registry::new().snapshot();
        let config = Request::new(
            Binding {
                provider: "openrouter-test".into(),
                account: "synthetic-openrouter-pilot".into(),
                requested_model: MODEL.into(),
                returned_model: MODEL.into(),
            },
            crate::provider::request::Epoch {
                id: "test".into(),
                instructions: String::new(),
                exact_state: Json::parse("{}").unwrap(),
            },
            registry.advertisement().clone(),
            Vec::new(),
        )
        .unwrap();
        let mut response = json!({"id":"test","model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"type":"function","id":"call1","function":{"name":"edit_file","arguments":"{}"}}]}}]});
        for content in [
            json!([{"type":"refusal","refusal":"refused"}]),
            json!(42),
            json!({"type":"output_text","text":"text"}),
            json!("also returned text"),
        ] {
            response["choices"][0]["message"]["content"] = content;
            assert!(matches!(
                decode(&config, &response),
                Err("pilot_mixed_output")
            ));
        }
        response["choices"][0]["message"]["refusal"] = json!("refused");
        assert!(matches!(decode(&config, &response), Err("pilot_choice")));
    }
}
