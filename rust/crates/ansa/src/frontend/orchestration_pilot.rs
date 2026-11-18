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
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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

/// This ledger belongs to a single approved pilot, including its preflight.
/// A reservation survives transport errors and an interrupted process.
struct Budget {
    directory: File,
    _lease: File,
    used: u8,
    model_started: bool,
    failed: bool,
}

impl Budget {
    fn open(path: &Path, prior: Option<u8>) -> Result<Self> {
        use ansa_linux::{fs, local};
        let exists = path.try_exists().map_err(|_| "pilot_budget_open")?;
        let (directory, lease) = if exists {
            let directory = fs::private_directory(path).map_err(|_| "pilot_budget_open")?;
            let lease = local::lock_existing(&directory).map_err(|_| "pilot_budget_lock")?;
            (directory, lease)
        } else {
            let used = prior
                .filter(|used| *used <= CALLS)
                .ok_or("pilot_budget_init")?;
            let directory = fs::create_private_leaf(path).map_err(|_| "pilot_budget_create")?;
            let lease = local::lock_new(&directory).map_err(|_| "pilot_budget_lock")?;
            let header =
                json!({"version":1,"model":MODEL,"endpoint":ENDPOINT,"previous_requests":used});
            local::create(&directory, "pilot.json", header.to_string().as_bytes())
                .map_err(|_| "pilot_budget_init")?;
            (directory, lease)
        };
        let header = local::read(&directory, "pilot.json", 512)
            .map_err(|_| "pilot_budget_read")?
            .ok_or("pilot_budget_init")?;
        let header = std::str::from_utf8(&header).map_err(|_| "pilot_budget_init")?;
        let header = Json::parse(header)?;
        let start = header.usize_field("previous_requests")?;
        if header.usize_field("version")? != 1
            || header.text_field("model")? != MODEL
            || header.text_field("endpoint")? != ENDPOINT
            || start > usize::from(CALLS)
            || prior.is_some_and(|value| usize::from(value) != start)
        {
            return Err("pilot_budget_identity");
        }
        let (names, extra) =
            local::names(&directory, usize::from(CALLS) + 2).map_err(|_| "pilot_budget_read")?;
        let mut entries = Vec::new();
        for name in names {
            match name.to_str() {
                Some("pilot.json" | "worker.lock") => (),
                Some("model-started") => return Err("pilot_already_started"),
                Some(name) => {
                    let index = name
                        .strip_prefix("request-")
                        .and_then(|value| value.parse::<u8>().ok())
                        .ok_or("pilot_budget_incomplete")?;
                    if name != format!("request-{index:02}") {
                        return Err("pilot_budget_incomplete");
                    }
                    let entry = local::read(&directory, name, 32)
                        .map_err(|_| "pilot_budget_read")?
                        .ok_or("pilot_budget_incomplete")?;
                    if entry == b"model" {
                        return Err("pilot_already_started");
                    }
                    if !matches!(entry.as_slice(), b"catalog" | b"key") {
                        return Err("pilot_budget_incomplete");
                    }
                    entries.push(index);
                }
                None => return Err("pilot_budget_incomplete"),
            }
        }
        let used = start + entries.len();
        entries.sort_unstable();
        if extra
            || used > usize::from(CALLS)
            || entries
                .iter()
                .map(|index| usize::from(*index))
                .ne(start..used)
        {
            return Err("pilot_budget_incomplete");
        }
        directory.sync_all().map_err(|_| "pilot_budget_sync")?;
        Ok(Self {
            directory,
            _lease: lease,
            used: used as u8,
            model_started: false,
            failed: false,
        })
    }

    fn reserve(&mut self, method: &str, url: &str) -> Result<()> {
        let label: &[u8] = match (method, url) {
            ("GET", CATALOG) => b"catalog",
            ("GET", KEY) => b"key",
            ("POST", API) => b"model",
            _ => return Err("pilot_endpoint"),
        };
        if self.used >= CALLS || self.failed {
            return Err("pilot_request_budget");
        }
        let name = format!("request-{:02}", self.used);
        self.failed = true;
        if label == b"model" && !self.model_started {
            // Preserve the one-attempt boundary even if the last request record is lost.
            ansa_linux::local::create(&self.directory, "model-started", b"reserved")
                .map_err(|_| "pilot_budget_publish")?;
            self.model_started = true;
        }
        ansa_linux::local::create(&self.directory, &name, label)
            .map_err(|_| "pilot_budget_publish")?;
        self.used += 1;
        self.failed = false;
        Ok(())
    }
}

pub(super) struct Pilot {
    key: Secret,
    exchange: Exchange,
    budget: Mutex<Budget>,
    deadline: Instant,
}

impl Pilot {
    fn authorize(
        key: Secret,
        exchange: Exchange,
        budget: Budget,
        allow_unlimited_key: bool,
        stop: &Cancel,
    ) -> Result<Arc<Self>> {
        let meter = Self {
            key,
            exchange,
            budget: Mutex::new(budget),
            deadline: Instant::now() + DURATION,
        };
        let catalog = meter.exchange("GET", CATALOG, None, stop)?;
        verify_catalog(&catalog)?;
        let credits = meter.exchange("GET", KEY, None, stop)?;
        verify_credit_limit(&credits, allow_unlimited_key)?;
        // The successful scenario needs a schema probe, plan, tool+final for
        // each child, and a parent summary.
        if CALLS - meter.requests()? < 7 {
            return Err("pilot_insufficient_scenario_budget");
        }
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
        stop.check()?;
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        if body.len() > 256 * 1024 {
            return Err("pilot_request_size");
        }
        self.budget
            .lock()
            .map_err(|_| "pilot_budget_lock")?
            .reserve(method, url)?;
        let timeout = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or("pilot_deadline")?;
        stop.check()?;
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

    fn requests(&self) -> Result<u8> {
        self.budget
            .lock()
            .map(|budget| budget.used)
            .map_err(|_| "pilot_budget_lock")
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

fn verify_credit_limit(response: &Value, allow_unlimited_key: bool) -> Result<()> {
    let data = response.get("data").ok_or("pilot_key_limit")?;
    let limit = data.get("limit").ok_or("pilot_key_limit")?;
    if allow_unlimited_key && limit.is_null() {
        return Ok(());
    }
    let cap = limit.as_f64().ok_or("pilot_key_limit")?;
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
/// launcher supplies the test key on a private pipe. An unlimited key requires
/// explicit opt-in for this run.
pub fn run() -> Result<()> {
    use std::io::{IsTerminal, Read};
    let mut arguments = std::env::args_os().skip(1).peekable();
    let allow_unlimited_key = arguments
        .next_if(|argument| argument == "--allow-unlimited-key")
        .is_some();
    let ledger = PathBuf::from(arguments.next().ok_or("pilot_budget_path")?);
    let prior = arguments
        .next()
        .map(|value| {
            value
                .to_str()
                .and_then(|value| value.parse::<u8>().ok())
                .ok_or("pilot_budget_init")
        })
        .transpose()?;
    if !ledger.is_absolute() || arguments.next().is_some() {
        return Err("pilot_budget_path");
    }
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
        Budget::open(&ledger, prior)?,
        allow_unlimited_key,
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
    let result = scenario(&pilot, &stop, &mut std::io::stdout());
    let _ = finish.send(());
    watchdog.join().map_err(|_| "pilot_deadline_watchdog")?;
    println!(
        "model={MODEL} endpoint={ENDPOINT} requests={} result={}",
        pilot.requests()?,
        result.err().unwrap_or("pass")
    );
    result
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn create() -> Result<Self> {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut bytes = [0; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "pilot_random")?;
        let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = PathBuf::from("/tmp").join(format!("ansa-openrouter-pilot-{nonce}"));
        ansa_linux::fs::create_private_leaf(&path).map_err(|_| "pilot_storage")?;
        Ok(Self(path))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check_scenario_scope(
    plan: &crate::orchestration::plan::Plan,
    report: &mut impl Write,
) -> Result<()> {
    use crate::orchestration::plan::Access;
    let steps = plan.steps();
    let status = match steps {
        [step] if step.access != Access::Write => "step_access",
        [step] if step.paths != ["report.txt"] => "step_paths",
        [_] => "accepted",
        _ => "step_count",
    };
    let scopes: Vec<_> = steps
        .iter()
        .map(|step| json!({"access":step.access,"paths":step.paths}))
        .collect();
    writeln!(
        report,
        "{}",
        json!({"event":"pilot_plan_scope","status":status,"step_count":steps.len(),"steps":scopes})
    )
    .map_err(|_| "pilot_plan_evidence")?;
    if status != "accepted" {
        return Err("pilot_plan_scope");
    }
    Ok(())
}

fn scenario(pilot: &Arc<Pilot>, stop: &Cancel, report: &mut impl Write) -> Result<()> {
    use super::{
        orchestration::{self, Proposal},
        sessions::{Factory, Kind},
    };
    use crate::runtime::{
        AgentMode, CommandPayload, EventPayload, PostStatus, RuntimeCommand, Submit,
    };
    let storage = Sandbox::create()?;
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
    let report_path = cwd.join("report.txt");
    std::fs::write(&report_path, "Report title: demo\nStatus: unverified\n")
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
    let prompt = concat!(
        "In this synthetic repository, replace the line Status: unverified with Status: verified in report.txt. ",
        "Return exactly one entry in steps, with access write and paths [\"report.txt\"]. Give the helper both exact lines. ",
        "Put the assertion that report.txt contains Status: verified in checks. The coordinator runs the check and ",
        "independent review automatically. Leave all other lines and files unchanged."
    );
    let proposal = Proposal::generate(&factory, seed, prompt.into(), stop)?;
    check_scenario_scope(&proposal.plan, report)?;
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
        || std::fs::read_to_string(&report_path).map_err(|_| "pilot_fixture")?
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
    use crate::auth::manager::HttpReply;
    use crate::transport::http::Head;
    use std::sync::atomic::{AtomicU8, Ordering};

    fn reply(value: Value) -> HttpReply {
        HttpReply {
            head: Head {
                status: 200,
                content_types: vec!["application/json".into()],
            },
            body: value.to_string(),
        }
    }

    #[test]
    fn preflight_requires_the_exact_provider_variant_and_credit_ceiling() {
        let mut catalog = json!({"data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","tools","response_format"]}]}});
        assert_eq!(verify_catalog(&catalog), Ok(()));
        catalog["data"]["endpoints"][0]["tag"] = json!("fireworks");
        assert_eq!(verify_catalog(&catalog), Err("pilot_catalog_endpoint"));
        let unlimited = json!({"data":{"limit":null,"limit_remaining":null}});
        assert_eq!(
            verify_credit_limit(&unlimited, false),
            Err("pilot_key_limit")
        );
        assert_eq!(verify_credit_limit(&unlimited, true), Ok(()));
        for invalid in [json!(1.01), json!(0), json!("unlimited")] {
            let credits = json!({"data":{"limit":invalid,"limit_remaining":1}});
            assert_eq!(verify_credit_limit(&credits, false), Err("pilot_key_limit"));
            assert_eq!(verify_credit_limit(&credits, true), Err("pilot_key_limit"));
        }
        assert_eq!(
            verify_credit_limit(&json!({"data":{}}), true),
            Err("pilot_key_limit")
        );
        assert_eq!(
            verify_credit_limit(
                &json!({"data":{"limit":1,"limit_remaining":0.75,"limit_reset":null}}),
                false,
            ),
            Ok(())
        );
        assert!(
            verify_credit_limit(
                &json!({"data":{"limit":1,"limit_remaining":0.75,"limit_reset":"daily"}}),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn meter_never_sends_a_thirteenth_request() {
        let root = Sandbox::create().unwrap();
        let ledger = root.0.join("budget");
        let mut budget = Budget::open(&ledger, Some(3)).unwrap();
        budget.reserve("GET", KEY).unwrap();
        assert!(matches!(
            Budget::open(&ledger, None),
            Err("pilot_budget_lock")
        ));
        drop(budget);
        assert!(matches!(
            Budget::open(&ledger, Some(0)),
            Err("pilot_budget_identity")
        ));
        let native_calls = Arc::new(AtomicU8::new(0));
        let calls = native_calls.clone();
        let meter = Pilot {
            key: Secret::new("dummy".into()),
            budget: Mutex::new(Budget::open(&ledger, None).unwrap()),
            deadline: Instant::now() + DURATION,
            exchange: Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Ok(reply(json!({})))
            }),
        };
        for _ in 4..CALLS {
            meter
                .exchange("GET", KEY, None, &Cancel::default())
                .unwrap();
        }
        assert_eq!(
            meter.exchange("GET", KEY, None, &Cancel::default()),
            Err("pilot_request_budget")
        );
        assert_eq!(native_calls.load(Ordering::Acquire), CALLS - 4);
    }

    #[test]
    fn interrupted_or_paid_attempt_cannot_reopen_for_another_model_call() {
        let root = Sandbox::create().unwrap();
        let ledger = root.0.join("budget");
        let mut budget = Budget::open(&ledger, Some(3)).unwrap();
        budget.reserve("POST", API).unwrap();
        drop(budget);
        assert!(matches!(
            Budget::open(&ledger, None),
            Err("pilot_already_started")
        ));
        std::fs::remove_file(ledger.join("request-03")).unwrap();
        assert!(matches!(
            Budget::open(&ledger, None),
            Err("pilot_already_started")
        ));

        let ledger = root.0.join("incomplete");
        let budget = Budget::open(&ledger, Some(0)).unwrap();
        ansa_linux::local::create(&budget.directory, "request-00.pending", b"model").unwrap();
        drop(budget);
        assert!(matches!(
            Budget::open(&ledger, None),
            Err("pilot_budget_incomplete")
        ));
    }

    #[test]
    fn rejected_synthetic_plan_reports_the_scope_and_reason() {
        use crate::orchestration::plan::Plan;
        let edit = json!({"id":"edit","task":"Update the report","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"Report verified"});
        let inspect = json!({"id":"inspect","task":"Read the report","access":"read","paths":["report.txt"],"depends_on":[],"acceptance":"Report read"});
        let mut expanded = edit.clone();
        expanded["paths"] = json!(["report.txt", "other.txt"]);
        for (status, steps) in [
            ("step_count", json!([edit, inspect])),
            ("step_access", json!([inspect])),
            ("step_paths", json!([expanded])),
        ] {
            let plan = Plan::parse(
                &json!({"schema_version":1,"summary":"Check the report","steps":steps}).to_string(),
            )
            .unwrap();
            let mut report = Vec::new();
            assert_eq!(
                check_scenario_scope(&plan, &mut report),
                Err("pilot_plan_scope")
            );
            let actual: Value = serde_json::from_slice(&report).unwrap();
            let steps: Vec<_> = steps
                .as_array()
                .unwrap()
                .iter()
                .map(|step| json!({"access":step["access"],"paths":step["paths"]}))
                .collect();
            assert_eq!(
                actual,
                json!({"event":"pilot_plan_scope","status":status,"step_count":steps.len(),"steps":steps})
            );
        }
    }

    #[test]
    fn full_synthetic_scenario_fits_remaining_requests_after_preflight() {
        let root = Sandbox::create().unwrap();
        let ledger = root.0.join("budget");
        let stop = Cancel::default();
        let sequence = AtomicU8::new(0);
        let exchange: Exchange = Arc::new(move |request, _| {
            let step = sequence.fetch_add(1, Ordering::AcqRel);
            match (step, request.method, request.url) {
                (0, "GET", CATALOG) if request.bearer.is_none() => {
                    return Ok(reply(json!({
                        "data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","response_format","tools"]}]}
                    })));
                }
                (1, "GET", KEY) if request.bearer.is_some() => {
                    return Ok(reply(json!({"data":{"limit":null,"limit_remaining":null}})));
                }
                (2..=8, "POST", API) if request.bearer.is_some() => (),
                _ => return Err("unexpected_pilot_request"),
            }
            let body: Value =
                serde_json::from_str(request.body.expose()).map_err(|_| "fixture_body")?;
            if body["model"] != MODEL || body["provider"] != route() {
                return Err("fixture_binding");
            }
            let format = &body["response_format"]["json_schema"];
            let expected_format = match step {
                2 => "ansa_native_schema_probe",
                3 => "ansa_orchestration_plan_v1",
                4..=7 => "ansa_child_result_v1",
                _ => "",
            };
            if step < 8 && (format["name"] != expected_format || format["strict"] != true) {
                return Err("fixture_strict_schema");
            }
            let messages = body["messages"].as_array().ok_or("fixture_messages")?;
            let output = match step {
                2 => json!({"value":"schema-enforced"}).to_string(),
                3 => json!({
                    "schema_version":1,"summary":"Update the report",
                    "steps":[{"id":"edit","task":"Replace Status: unverified with Status: verified in report.txt","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"Status: verified"}],
                    "checks":[{"path":"report.txt","contains":"Status: verified"}],"manual_checks":[]
                }).to_string(),
                4 | 6 => {
                    let token = messages[0]["content"].as_str().and_then(|text| text.split_once("instruction_token is ")).and_then(|(_, tail)| tail.get(..64)).ok_or("fixture_instruction_token")?;
                    let calls = if step == 4 {
                        vec![("edit", "edit_file", json!({"path":"report.txt","old_text":"Status: unverified","new_text":"Status: verified","instruction_token":token}))]
                    } else {
                        ["ansa-candidate.diff", "approved-plan.json", "checkout/report.txt"].into_iter().zip(["diff", "plan", "report"]).map(|(path, id)| {
                            (id, "read_file", json!({"path":path,"max_bytes":8192,"offset":0,"instruction_token":token}))
                        }).collect()
                    };
                    let calls: Vec<_> = calls.into_iter().map(|(id, name, args)| json!({"type":"function","id":id,"function":{"name":name,"arguments":args.to_string()}})).collect();
                    return Ok(reply(json!({"id":format!("fixture{step}"),"model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":calls}}]})));
                }
                5 | 7 => {
                    let tools: Vec<_> = messages.iter().filter(|message| message["role"] == "tool").collect();
                    let expected: &[&str] = if step == 5 { &["edit"] } else { &["diff", "plan", "report"] };
                    let evidence: &[&str] = if step == 5 { &["\"applied\":true"] } else { &["+Status: verified", "Update the report", "Report title: demo"] };
                    if tools.len() != expected.len() || tools.iter().zip(expected.iter().zip(evidence)).any(|(message, (id, fragment))| {
                        message["tool_call_id"].as_str() != Some(*id) || !message["content"].as_str().is_some_and(|value| value.contains(fragment))
                    }) { return Err("fixture_tool_results"); }
                    json!({"status":"completed","summary":"The exact report edit has been verified","findings":[]}).to_string()
                }
                8 => {
                    if body.get("response_format").is_some() || !messages.last().and_then(|message| message["content"].as_str()).is_some_and(|text| text.contains("ansa-result")) {
                        return Err("fixture_parent_handoff");
                    }
                    "The report update has been reviewed.".into()
                }
                _ => return Err("unexpected_pilot_request"),
            };
            Ok(reply(
                json!({"id":format!("fixture{step}"),"model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":output}}]}),
            ))
        });
        let pilot = Pilot::authorize(
            Secret::new("dummy".into()),
            exchange,
            Budget::open(&ledger, Some(3)).unwrap(),
            true,
            &stop,
        )
        .unwrap();
        let mut report = Vec::new();
        scenario(&pilot, &stop, &mut report).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&report).unwrap(),
            json!({"event":"pilot_plan_scope","status":"accepted","step_count":1,"steps":[{"access":"write","paths":["report.txt"]}]}),
        );
        assert_eq!(pilot.requests(), Ok(12));
        drop(pilot);
        assert!(matches!(
            Budget::open(&ledger, None),
            Err("pilot_already_started")
        ));
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
