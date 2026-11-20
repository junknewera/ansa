//! Feature-gated, metered acceptance runner for the pinned OpenRouter route.
mod trace;

use super::Result;
#[cfg(test)]
use crate::openrouter::{PROVIDER, route};
use crate::{
    auth::{
        Cancel, Secret,
        manager::{Exchange, HttpRequest},
    },
    openrouter::{self, API, CATALOG, ENDPOINT, KEY, MODEL, decode, verify_catalog},
    protocol::json::Json,
    provider::{
        request::{Binding, Request},
        responses::AdmittedResponse,
        structured::FinalFormat,
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

const CALLS: u8 = 12;
const DURATION: Duration = Duration::from_secs(600);
const REPORT: &str = "report.txt";
const FULL_READ_BYTES: usize = 8192;
const REVIEW_FILES: [&str; 3] = [
    "approved-plan.json",
    "ansa-candidate.diff",
    "checkout/report.txt",
];

/// This ledger belongs to a single approved pilot, including its preflight.
/// A reservation survives transport errors and an interrupted process.
struct Budget {
    directory: File,
    _lease: BudgetLease,
    used: u8,
    model_started: bool,
    failed: bool,
}

struct BudgetLease(File);

impl Drop for BudgetLease {
    fn drop(&mut self) {
        // A concurrent fork can briefly hold a duplicate CLOEXEC descriptor.
        // Unlock explicitly after the last ledger action instead of waiting for exec.
        let _ = self.0.unlock();
    }
}

impl Budget {
    fn open(path: &Path, prior: Option<u8>) -> Result<Self> {
        use ansa_linux::{fs, local};
        let exists = path.try_exists().map_err(|_| "pilot_budget_open")?;
        let (directory, lease) = if exists {
            let directory = fs::private_directory(path).map_err(|_| "pilot_budget_open")?;
            let lease =
                BudgetLease(local::lock_existing(&directory).map_err(|_| "pilot_budget_lock")?);
            (directory, lease)
        } else {
            let used = prior
                .filter(|used| *used <= CALLS)
                .ok_or("pilot_budget_init")?;
            let directory = fs::create_private_leaf(path).map_err(|_| "pilot_budget_create")?;
            let lease = BudgetLease(local::lock_new(&directory).map_err(|_| "pilot_budget_lock")?);
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
    trace: trace::Trace,
}

impl Pilot {
    fn new(key: Secret, exchange: Exchange, budget: Budget) -> Self {
        Self {
            key,
            exchange,
            budget: Mutex::new(budget),
            deadline: Instant::now() + DURATION,
            trace: trace::Trace::default(),
        }
    }

    fn preflight(&self, allow_unlimited_key: bool, stop: &Cancel) -> Result<()> {
        let catalog = self.exchange("catalog", "GET", CATALOG, None, stop)?;
        verify_catalog(&catalog)?;
        let credits = self.exchange("key", "GET", KEY, None, stop)?;
        verify_credit_limit(&credits, allow_unlimited_key)?;
        // Probe, plan, writer read/edit/final, three reviewer reads and final,
        // and the parent summary.
        if CALLS - self.requests()? < 10 {
            return Err("pilot_insufficient_scenario_budget");
        }
        let value = self.exchange(
            "schema_probe",
            "POST",
            API,
            Some(openrouter::probe_body()),
            stop,
        )?;
        openrouter::verify_probe(&value)
    }

    fn exchange(
        &self,
        phase: &'static str,
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
        let request = {
            let mut budget = self.budget.lock().map_err(|_| "pilot_budget_lock")?;
            budget.reserve(method, url)?;
            budget.used
        };
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
        );
        // The exchange exposes headers only together with a complete body.
        let status = reply.as_ref().ok().map(|reply| reply.head.status);
        let value = reply.and_then(openrouter::read_reply);
        let finish = (method == "POST")
            .then(|| value.as_ref().ok().map(trace::finish_reason))
            .flatten();
        self.trace.add(json!({"kind":"http","phase":phase,"request":request,"status":status,"finish_reason":finish,"code":value.as_ref().err()}))?;
        value
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
        fixture_settings(&request.epoch.exact_state)?;
        let tool = next_tool(request)?;
        let body = encode(request, tool)?;
        let phase = trace::phase(request);
        let response = self.exchange(phase, "POST", API, Some(body), stop)?;
        let decoded = decode(request, &response);
        let blocked_reason = decoded.as_ref().ok().and_then(trace::blocked_reason);
        let admitted = decoded.and_then(|response| match (tool, response.calls()) {
            (Some(tool), [call]) if call.name == tool.name => {
                validate_tool_arguments(request, tool, &call.arguments)?;
                Ok(response)
            }
            (None, []) => Ok(response),
            (Some(_), _) => Err("pilot_required_tool"),
            (None, _) => Err("pilot_final_has_tools"),
        });
        self.trace.add(json!({"kind":"response_admission","phase":phase,"required_tool":tool.map(|tool| tool.name),"accepted":admitted.is_ok(),"code":admitted.as_ref().err(),"blocked_reason":blocked_reason}))?;
        admitted
    }
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

#[derive(Clone, Copy)]
struct ToolStep {
    name: &'static str,
    path: &'static str,
}

fn fixture_settings(mut state: &Json) -> Result<&Json> {
    for _ in 0..8 {
        if state.has("instructions") && !state.field("instructions")?.elements()?.is_empty() {
            return Err("pilot_external_instructions");
        }
        if !state.has("base_state") {
            state.field("instructions")?;
            return state.field("settings");
        }
        state = state.field("base_state")?;
    }
    Err("pilot_external_instructions")
}

fn validate_tool_arguments(request: &Request, tool: ToolStep, arguments: &str) -> Result<()> {
    let args = Json::parse(arguments).map_err(|_| "pilot_tool_arguments")?;
    let path = Path::new(
        args.text_field("path")
            .map_err(|_| "pilot_tool_arguments")?,
    );
    let cwd = Path::new(fixture_settings(&request.epoch.exact_state)?.text_field("cwd")?);
    if path != Path::new(tool.path) && path != cwd.join(tool.path) {
        return Err("pilot_tool_arguments");
    }
    if tool.name == "read_file"
        && (args.usize_field("offset") != Ok(0)
            || args.usize_field("max_bytes") != Ok(FULL_READ_BYTES))
    {
        return Err("pilot_tool_arguments");
    }
    Ok(())
}

fn next_tool(request: &Request) -> Result<Option<ToolStep>> {
    if request.final_format() != Some(FinalFormat::ChildResult) {
        return Ok(None);
    }
    let cwd = Path::new(fixture_settings(&request.epoch.exact_state)?.text_field("cwd")?);
    let mut calls = std::collections::BTreeMap::new();
    let mut results = Vec::new();
    for item in &request.input {
        match item.text_field("type") {
            Ok("function_call") => {
                calls.insert(item.text_field("call_id")?, item);
            }
            Ok("function_call_output") => {
                let call = calls
                    .remove(item.text_field("call_id")?)
                    .ok_or("pilot_tool_result")?;
                let arguments = Json::parse(call.text_field("arguments")?)?;
                let output =
                    Json::parse(item.text_field("output")?).map_err(|_| "pilot_tool_result")?;
                if output.has("instructions")
                    && !output.field("instructions")?.elements()?.is_empty()
                {
                    return Err("pilot_external_instructions");
                }
                let path = cwd.join(arguments.text_field("path")?);
                if output
                    .text_field("path")
                    .is_ok_and(|actual| Path::new(actual) == path)
                {
                    results.push((call.text_field("name")?, path, output));
                }
            }
            _ => (),
        }
    }
    let has_read = |path| {
        results.iter().any(|(name, actual, result)| {
            *name == "read_file"
                && *actual == cwd.join(path)
                && result.usize_field("offset") == Ok(0)
                && result.field("truncated").and_then(Json::boolean) == Ok(false)
                && result.text_field("content").is_ok()
        })
    };
    if trace::phase(request) == "writer" {
        if results.iter().any(|(name, path, result)| {
            *name == "edit_file"
                && *path == cwd.join(REPORT)
                && result.field("applied").and_then(Json::boolean) == Ok(true)
        }) {
            return Ok(None);
        }
        let name = if has_read(REPORT) {
            "edit_file"
        } else {
            "read_file"
        };
        return Ok(Some(ToolStep { name, path: REPORT }));
    }
    Ok(REVIEW_FILES
        .into_iter()
        .find(|path| !has_read(path))
        .map(|path| ToolStep {
            name: "read_file",
            path,
        }))
}

fn encode(request: &Request, tool: Option<ToolStep>) -> Result<Value> {
    let mut instructions = request.epoch.instructions.clone();
    if let Some(tool) = tool {
        use std::fmt::Write as _;
        write!(instructions, "\nFor this synthetic step, call {} once with path {}. Supply the instruction_token from these instructions, or an updated token supplied by the tool. ", tool.name, tool.path)
            .map_err(|_| "pilot_tool_instruction")?;
        if tool.name == "read_file" {
            instructions.push_str("Read the complete file with offset 0 and max_bytes 8192.");
        } else {
            instructions.push_str(
                "Use the exact old and new text from the assigned task and the preceding read.",
            );
        }
    } else if request.final_format() == Some(FinalFormat::ChildResult) {
        instructions.push_str("\nReturn your final assessment using the tool results. If blocked, state the concrete reason in summary.");
    }
    let mut body = openrouter::base_body(request, &instructions)?;
    if request.final_format() == Some(FinalFormat::ChildResult) {
        // OpenRouter requires the definitions on continuations with tool history.
        let names: &[&str] = if trace::phase(request) == "writer" {
            &["read_file", "edit_file"]
        } else {
            &["read_file"]
        };
        body["tools"] = Value::Array(
            names
                .iter()
                .map(|name| {
                    let descriptor = request
                        .tools
                        .descriptors
                        .iter()
                        .find(|descriptor| descriptor.name == *name)
                        .ok_or("pilot_tool_not_advertised")?;
                    openrouter::tool_value(descriptor)
                })
                .collect::<Result<Vec<_>>>()?,
        );
        body["tool_choice"] = tool.map_or_else(
            || json!("none"),
            |tool| json!({"type":"function","function":{"name":tool.name}}),
        );
    }
    if tool.is_none() && request.final_format().is_some() {
        openrouter::apply_final_format(request, &mut body)?;
    }
    Ok(body)
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
    let pilot = Arc::new(Pilot::new(
        Secret::new(key.into()),
        Arc::new(crate::transport::client::openrouter_exchange),
        Budget::open(&ledger, prior)?,
    ));
    drop(token);
    let result = pilot.preflight(allow_unlimited_key, &stop);
    let mut report = std::io::stdout();
    if result.is_err() {
        pilot.trace.write(&mut report)?;
        return result;
    }
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
    let result = scenario(&pilot, &stop, &mut report);
    let _ = finish.send(());
    watchdog.join().map_err(|_| "pilot_deadline_watchdog")?;
    pilot.trace.write(&mut report)?;
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
        _ if plan.encode()?.len() > FULL_READ_BYTES => "plan_size",
        [step] if step.access != Access::Write => "step_access",
        [step] if step.paths != [REPORT] => "step_paths",
        [step]
            if !step.task.contains("Status: unverified")
                || !step.task.contains("Status: verified") =>
        {
            "step_task"
        }
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
    let storage = Sandbox::create()?;
    verify_instruction_ancestors(&storage.0)?;
    let home = storage.0.join("home");
    let cwd = storage.0.join("repo");
    ansa_linux::fs::create_private_leaf(&home).map_err(|_| "pilot_storage")?;
    ansa_linux::fs::create_private_leaf(&cwd).map_err(|_| "pilot_storage")?;
    let settings = crate::config::load_agent_settings(&cwd, &home)?;
    let factory =
        super::sessions::Factory::new(settings, home.join("auth"), true, String::new(), None)
            .with_pilot(pilot.clone());
    let result = run_synthetic(pilot, stop, report, &factory);
    // The scenario and its workers have stopped. Give a timed-out run a
    // separate, short window to collect its last durable outcomes.
    let evidence = pilot.trace.collect_journals(&factory.settings);
    if let Err(code) = evidence {
        pilot
            .trace
            .add(json!({"kind":"journal_capture_error","code":code}))?;
    }
    result.and(evidence)
}

fn verify_instruction_ancestors(root: &Path) -> Result<()> {
    let root = root.canonicalize().map_err(|_| "pilot_storage")?;
    for directory in root.ancestors() {
        match std::fs::symlink_metadata(directory.join("AGENTS.md")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => return Err("pilot_external_instructions"),
        }
    }
    Ok(())
}

fn run_synthetic(
    pilot: &Pilot,
    stop: &Cancel,
    report: &mut impl Write,
    factory: &super::sessions::Factory,
) -> Result<()> {
    use super::{
        orchestration::{self, Proposal},
        sessions::Kind,
    };
    use crate::runtime::{
        AgentMode, CommandPayload, EventPayload, PostStatus, RuntimeCommand, Submit,
    };
    let cwd = factory.settings.cwd.as_path();
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
    let git = |args: &[&str]| git_at(cwd, args);
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
    let proposal = Proposal::generate(factory, seed, prompt.into(), false, None, stop)?;
    check_scenario_scope(&proposal.plan, report)?;
    let (progress, _) = tokio::sync::watch::channel(None);
    let (_, inbox) = tokio::sync::mpsc::channel(1);
    let completed = orchestration::execute(factory, proposal, stop, progress, inbox)?;
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
                        text: completed.evidence.ok_or("pilot_parent_handoff")?,
                        mode: AgentMode::Plan,
                    }),
                ))
                .await?
                != PostStatus::Accepted
            {
                return Err("pilot_handoff");
            }
            let mut delivered = false;
            while let Some(event) = handle.next_event().await {
                match event.payload {
                    EventPayload::TextDelta { text } => delivered |= !text.trim().is_empty(),
                    EventPayload::Done if delivered => return root.shutdown().await,
                    EventPayload::Done => return Err("pilot_missing_summary"),
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
        let mut catalog = json!({"data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","tools","response_format","tool_choice"]}]}});
        assert_eq!(verify_catalog(&catalog), Ok(()));
        let mut missing_choice = catalog.clone();
        missing_choice["data"]["endpoints"][0]["supported_parameters"] =
            json!(["structured_outputs", "tools", "response_format"]);
        assert_eq!(
            verify_catalog(&missing_choice),
            Err("openrouter_catalog_parameters")
        );
        catalog["data"]["endpoints"][0]["tag"] = json!("fireworks");
        assert_eq!(verify_catalog(&catalog), Err("openrouter_catalog_endpoint"));
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
        let meter = Pilot::new(
            Secret::new("dummy".into()),
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Ok(reply(json!({})))
            }),
            Budget::open(&ledger, None).unwrap(),
        );
        for _ in 4..CALLS {
            meter
                .exchange("key", "GET", KEY, None, &Cancel::default())
                .unwrap();
        }
        assert_eq!(
            meter.exchange("key", "GET", KEY, None, &Cancel::default()),
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
        assert_eq!(
            Budget::open(&ledger, None).err(),
            Some("pilot_budget_incomplete")
        );
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
            ("step_task", json!([edit])),
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
        let oversized = Plan::parse(&json!({
            "schema_version":1,"summary":"x".repeat(4096),
            "steps":[{"id":"edit","task":format!("Replace Status: unverified with Status: verified {}", "x".repeat(3200)),
            "access":"write","paths":[REPORT],"depends_on":[],"acceptance":"x".repeat(1600)}]
        }).to_string()).unwrap();
        let mut report = Vec::new();
        assert_eq!(
            check_scenario_scope(&oversized, &mut report),
            Err("pilot_plan_scope")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&report).unwrap()["status"],
            "plan_size"
        );
    }

    #[test]
    fn synthetic_pilot_rejects_inherited_instruction_sources() {
        let root = Sandbox::create().unwrap();
        let nested = root.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert_eq!(verify_instruction_ancestors(&nested), Ok(()));
        std::fs::write(root.0.join("AGENTS.md"), "private-fixture-fragment").unwrap();
        assert_eq!(
            verify_instruction_ancestors(&nested),
            Err("pilot_external_instructions")
        );
        let epoch = Json::parse(r#"{"base_state":{"settings":{"cwd":"/tmp/fixture"},"instructions":[{"path":"/tmp/AGENTS.md","text":"private-fixture-fragment"}]}}"#).unwrap();
        assert_eq!(
            fixture_settings(&epoch).err(),
            Some("pilot_external_instructions")
        );
        let registry = crate::tools::registry::Registry::new().snapshot();
        let request = Request::new(
            Binding {
                provider: "openrouter-test".into(),
                account: "synthetic-openrouter-pilot".into(),
                requested_model: MODEL.into(),
                returned_model: MODEL.into(),
            },
            crate::provider::request::Epoch {
                id: "test".into(),
                instructions: String::new(),
                exact_state: Json::parse(r#"{"settings":{"cwd":"/tmp/fixture"},"instructions":[]}"#).unwrap(),
            },
            registry.advertisement().clone(),
            vec![
                Json::parse(r#"{"type":"function_call","call_id":"read","name":"read_file","arguments":"{\"path\":\"approved-plan.json\"}"}"#).unwrap(),
                Json::parse(r#"{"type":"function_call_output","call_id":"read","output":"{\"instructions\":[{\"text\":\"private-fixture-fragment\"}]}"}"#).unwrap(),
            ],
        ).unwrap().with_final_format(Some(FinalFormat::ChildResult));
        assert!(matches!(
            next_tool(&request),
            Err("pilot_external_instructions")
        ));
        let read = ToolStep {
            name: "read_file",
            path: REPORT,
        };
        assert_eq!(
            validate_tool_arguments(
                &request,
                read,
                r#"{"path":"report.txt","offset":0,"max_bytes":1}"#
            ),
            Err("pilot_tool_arguments")
        );
    }

    #[test]
    fn full_synthetic_scenario_uses_twelve_requests_and_strict_finals() {
        let (result, trace) = synthetic_scenario(FixtureWriter::Complete);
        assert_eq!(result, Ok(()));
        for phase in ["writer", "reviewer", "summary"] {
            assert!(
                trace
                    .iter()
                    .any(|event| event["phase"] == phase && event["kind"] == "done")
            );
        }
        assert!(trace.iter().any(|event| event["phase"] == "writer"
            && event["kind"] == "tool_admitted"
            && event["tool"] == "edit_file"));
        assert!(trace.iter().any(|event| event["phase"] == "writer"
            && event["kind"] == "tool_outcome"
            && event["execution"] == "succeeded"
            && event["effects"] == "known_result"));
    }

    #[test]
    fn rejected_and_incomplete_writer_responses_have_distinct_traces() {
        let (result, trace) = synthetic_scenario(FixtureWriter::OutsideScope);
        assert!(result.is_err());
        assert!(
            trace.iter().any(|event| event["phase"] == "writer"
                && event["kind"] == "response_admission"
                && event["required_tool"] == "edit_file"
                && event["code"] == "pilot_tool_arguments"),
            "{trace:?}"
        );
        assert!(
            !trace
                .iter()
                .any(|event| event["kind"] == "tool_admitted" && event["tool"] == "edit_file")
        );

        let (result, trace) = synthetic_scenario(FixtureWriter::Incomplete);
        assert!(result.is_err());
        assert!(trace.iter().any(|event| event["phase"] == "writer"
            && event["kind"] == "http"
            && event["status"] == 200
            && event["finish_reason"] == "length"));
        assert!(trace.iter().any(|event| event["phase"] == "writer"
            && event["kind"] == "response_admission"
            && event["accepted"] == false
            && event["code"] == "openrouter_incomplete"));
        assert!(trace.iter().any(|event| event["phase"] == "writer"
            && event["kind"] == "paused"
            && event["code"] == "openrouter_incomplete"));
        assert!(
            !trace
                .iter()
                .any(|event| event["kind"] == "tool_admitted" || event["kind"] == "tool_outcome")
        );

        let (result, trace) = synthetic_scenario(FixtureWriter::EarlyFinal);
        assert!(result.is_err());
        assert!(
            trace
                .iter()
                .any(|event| event["kind"] == "response_admission"
                    && event["phase"] == "writer"
                    && event["required_tool"] == "read_file"
                    && event["code"] == "pilot_required_tool"
                    && event["blocked_reason"] == "instructions")
        );
        assert!(!trace.iter().any(|event| event["phase"] == "writer"
            && matches!(
                event["kind"].as_str(),
                Some("tool_admitted" | "response_committed")
            )));
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FixtureWriter {
        Complete,
        OutsideScope,
        Incomplete,
        EarlyFinal,
    }

    fn synthetic_scenario(writer: FixtureWriter) -> (Result<()>, Vec<Value>) {
        let root = Sandbox::create().unwrap();
        let ledger = root.0.join("budget");
        let stop = Cancel::default();
        let sequence = AtomicU8::new(0);
        let exchange: Exchange = Arc::new(move |request, _| {
            let step = sequence.fetch_add(1, Ordering::AcqRel);
            match (step, request.method, request.url) {
                (0, "GET", CATALOG) if request.bearer.is_none() => {
                    return Ok(reply(json!({
                        "data":{"id":MODEL,"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}
                    })));
                }
                (1, "GET", KEY) if request.bearer.is_some() => {
                    return Ok(reply(json!({"data":{"limit":null,"limit_remaining":null}})));
                }
                (2..=11, "POST", API) if request.bearer.is_some() => (),
                _ => return Err("unexpected_pilot_request"),
            }
            let body: Value =
                serde_json::from_str(request.body.expose()).map_err(|_| "fixture_body")?;
            if body["model"] != MODEL || body["provider"] != route() {
                return Err("fixture_binding");
            }
            let expected_format = match step {
                2 => Some("ansa_native_schema_probe"),
                3 => Some("ansa_orchestration_plan_v1"),
                6 | 10 => Some("ansa_child_result_v1"),
                _ => None,
            };
            if let Some(name) = expected_format {
                let format = &body["response_format"]["json_schema"];
                if format["name"] != name || format["strict"] != true {
                    return Err("fixture_strict_schema");
                }
            } else if body.get("response_format").is_some() {
                return Err("fixture_unexpected_schema");
            }
            let messages = body["messages"].as_array().ok_or("fixture_messages")?;
            let tool = match step {
                4 => Some(("writer_read", "read_file", REPORT)),
                5 => Some(("edit", "edit_file", REPORT)),
                7..=9 => Some((
                    ["plan", "diff", "report"][(step - 7) as usize],
                    "read_file",
                    REVIEW_FILES[(step - 7) as usize],
                )),
                _ => None,
            };
            let system = messages[0]["content"]
                .as_str()
                .ok_or("fixture_instructions")?;
            if (4..=6).contains(&step) {
                let assignment = messages
                    .iter()
                    .find(|message| message["role"] == "user")
                    .and_then(|message| message["content"].as_str())
                    .ok_or("fixture_assignment")?;
                if !assignment.contains("Status: unverified")
                    || !assignment.contains("Status: verified")
                {
                    return Err("fixture_assignment");
                }
            }
            if let Some((_, name, path)) = tool {
                if body["tool_choice"] != json!({"type":"function","function":{"name":name}})
                    || !body["tools"]
                        .as_array()
                        .ok_or("fixture_tools")?
                        .iter()
                        .any(|tool| tool["function"]["name"] == name)
                    || !system.contains(&format!("call {name} once with path {path}."))
                {
                    return Err("fixture_tool_stage");
                }
            } else if matches!(step, 6 | 10)
                && (body["tool_choice"] != "none" || !body["tools"].is_array())
            {
                return Err("fixture_final_stage");
            }
            if step == 4 && writer == FixtureWriter::Incomplete {
                return Ok(reply(
                    json!({"id":"fixture4","model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"private-fixture-fragment"}}]}),
                ));
            }
            let output = match step {
                2 => json!({"value":"schema-enforced"}).to_string(),
                3 => json!({
                    "schema_version":1,"summary":"Update the report",
                    "steps":[{"id":"edit","task":"Replace Status: unverified with Status: verified in report.txt","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"Status: verified"}],
                    "checks":[{"path":"report.txt","contains":"Status: verified"}],"manual_checks":[]
                }).to_string(),
                4 if writer == FixtureWriter::EarlyFinal => {
                    json!({"status":"blocked","summary":"Missing instruction_token private-fixture-fragment","findings":[]}).to_string()
                }
                4 | 5 | 7..=9 => {
                    let (id, name, path) = tool.ok_or("fixture_tool_stage")?;
                    let token = system.split_once("instruction_token is ").and_then(|(_, tail)| tail.get(..64)).ok_or("fixture_instruction_token")?;
                    let args = if name == "edit_file" {
                        let path = if writer == FixtureWriter::OutsideScope { "other.txt" } else { path };
                        json!({"path":path,"old_text":"Status: unverified","new_text":"Status: verified","instruction_token":token})
                    } else {
                        json!({"path":path,"max_bytes":8192,"offset":0,"instruction_token":token})
                    };
                    let call = json!({"type":"function","id":id,"function":{"name":name,"arguments":args.to_string()}});
                    return Ok(reply(json!({"id":format!("fixture{step}"),"model":MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[call]}}]})));
                }
                6 | 10 => {
                    let tools: Vec<_> = messages.iter().filter(|message| message["role"] == "tool").collect();
                    let expected: &[&str] = if step == 6 { &["writer_read", "edit"] } else { &["plan", "diff", "report"] };
                    let evidence: &[&str] = if step == 6 { &["Status: unverified", "\"applied\":true"] } else { &["Update the report", "+Status: verified", "Report title: demo"] };
                    if tools.len() != expected.len() || tools.iter().zip(expected.iter().zip(evidence)).any(|(message, (id, fragment))| {
                        message["tool_call_id"].as_str() != Some(*id) || !message["content"].as_str().is_some_and(|value| value.contains(fragment))
                    }) { return Err("fixture_tool_results"); }
                    json!({"status":"completed","summary":"The exact report edit has been verified","findings":[]}).to_string()
                }
                11 => {
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
        let pilot = Arc::new(Pilot::new(
            Secret::new("dummy".into()),
            exchange,
            Budget::open(&ledger, Some(0)).unwrap(),
        ));
        pilot.preflight(true, &stop).unwrap();
        let mut report = Vec::new();
        let result = scenario(&pilot, &stop, &mut report);
        assert_eq!(
            serde_json::from_slice::<Value>(&report).unwrap(),
            json!({"event":"pilot_plan_scope","status":"accepted","step_count":1,"steps":[{"access":"write","paths":["report.txt"]}]}),
        );
        assert_eq!(
            pilot.requests(),
            Ok(match writer {
                FixtureWriter::Complete => 12,
                FixtureWriter::OutsideScope => 6,
                FixtureWriter::Incomplete | FixtureWriter::EarlyFinal => 5,
            })
        );
        let mut report = Vec::new();
        pilot.trace.write(&mut report).unwrap();
        let report = String::from_utf8(report).unwrap();
        for private in [
            "instruction_token",
            "private-fixture-fragment",
            "Status: unverified",
            "Status: verified",
            "dummy",
        ] {
            assert!(!report.contains(private));
        }
        let report: Value = serde_json::from_str(&report).unwrap();
        let trace = report["events"].as_array().unwrap().to_vec();
        assert!(
            !trace.iter().any(|event| {
                event["kind"] == "journal_error" || event["kind"] == "journal_capture_error"
            }),
            "{trace:?}"
        );
        assert_eq!(Arc::strong_count(&pilot), 1);
        drop(pilot);
        assert_eq!(
            Budget::open(&ledger, None).err(),
            Some("pilot_already_started")
        );
        (result, trace)
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
                Err("openrouter_mixed_output")
            ));
        }
        response["choices"][0]["message"]["refusal"] = json!("refused");
        assert!(matches!(
            decode(&config, &response),
            Err("openrouter_choice")
        ));
    }
}
