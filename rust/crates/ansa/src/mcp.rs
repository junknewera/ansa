//! Explicitly configured MCP tools use the same durable admission as local tools.
use crate::{
    auth::{Cancel, Secret, manager::HttpRequest},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::{Advertisement, Descriptor},
    tools::registry::{Definition, Outcome, Policy, Registry},
    transport::client::{McpExchange, mcp_exchange},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;
const VERSION: &str = "2025-11-25";
const FRAME: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const WRAPPER: &str = r#"{"type":"object","properties":{"arguments_json":{"type":"string","description":"JSON object matching the MCP input schema"}},"required":["arguments_json"],"additionalProperties":false}"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    servers: BTreeMap<String, Server>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
    connection: Transport,
    tools: BTreeSet<String>,
    #[serde(default)]
    read_only: BTreeSet<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
enum Transport {
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        bearer_env: Option<String>,
    },
}

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    buffer: Vec<u8>,
}
impl Process {
    fn open(command: &Path, args: &[String], cwd: &Path) -> Result<Self> {
        if !command.is_absolute() || args.len() > 64 {
            return Err("mcp_command");
        }
        let mut launch = Command::new(command);
        launch.args(args).current_dir(cwd).env_clear();
        for name in ["PATH", "HOME", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(name) {
                launch.env(name, value);
            }
        }
        launch.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = ansa_linux::managed::spawn(launch, None, Some(std::process::id()))
            .map_err(|_| "mcp_spawn")?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("mcp_pipes");
        };
        let process = Self { child, input: Some(input), output, buffer: Vec::new() };
        ansa_linux::managed::nonblocking(process.input.as_ref().expect("owned stdin")).map_err(|_| "mcp_pipes")?;
        ansa_linux::managed::nonblocking(&process.output).map_err(|_| "mcp_pipes")?;
        Ok(process)
    }
    fn send(&mut self, message: &Json, stop: &Cancel, deadline: Instant) -> Result<()> {
        let mut frame = message.dump().into_bytes();
        frame.push(b'\n');
        if frame.len() > FRAME {
            return Err("mcp_frame");
        }
        let mut left = frame.as_slice();
        while !left.is_empty() {
            stop.check()?;
            if Instant::now() >= deadline {
                return Err("mcp_timeout");
            }
            match self.input.as_mut().ok_or("mcp_stream_closed")?.write(left) {
                Ok(0) => return Err("mcp_write"),
                Ok(count) => left = &left[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Err("mcp_write"),
            }
        }
        Ok(())
    }
    fn receive(&mut self, id: usize, stop: &Cancel, deadline: Instant) -> Result<Json> {
        let mut chunk = [0; 8192];
        let mut scanned = 0;
        loop {
            stop.check()?;
            if Instant::now() >= deadline {
                return Err("mcp_timeout");
            }
            if let Some(relative) = self.buffer[scanned..].iter().position(|byte| *byte == b'\n') {
                let end = scanned + relative;
                let message = Json::parse(std::str::from_utf8(&self.buffer[..end]).map_err(|_| "mcp_utf8")?)?;
                self.buffer.drain(..=end);
                scanned = 0;
                if message.usize_field("id").ok() == Some(id) && !message.has("method") {
                    return Ok(message);
                }
                if message.text_field("jsonrpc") == Ok("2.0")
                    && let (Ok(method), Ok(server_id)) = (message.text_field("method"), message.field("id"))
                    && (server_id.is_string() || server_id.is_integer())
                {
                    let (key, value) = if method == "ping" {
                        ("result", Json::parse("{}")?)
                    } else {
                        ("error", Json::parse(r#"{"code":-32601,"message":"Method not found"}"#)?)
                    };
                    self.send(&Json::object(vec![
                        ("jsonrpc", Json::string("2.0")?),
                        ("id", server_id.clone()),
                        (key, value),
                    ])?, stop, deadline)?;
                    continue;
                }
                if message.has("id") || message.text_field("method").ok() == Some("notifications/tools/list_changed") {
                    return Err("mcp_catalog_changed_or_unexpected_message");
                }
                continue;
            }
            scanned = self.buffer.len();
            match self.output.read(&mut chunk[..FRAME.saturating_sub(scanned).min(8192)]) {
                Ok(0) => return Err("mcp_stream_closed"),
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return Err("mcp_read"),
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        drop(self.input.take());
        // Keep the child unreaped until the group is signalled.
        std::thread::sleep(Duration::from_millis(50));
        let _ = ansa_linux::process::kill_group(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Http {
    url: String,
    bearer: Option<Secret>,
    session: Option<String>,
    version: Option<String>,
}
impl Http {
    fn exchange(&mut self, message: &Json, expected: Option<usize>, stop: &Cancel, timeout: Duration) -> Result<Option<Json>> {
        let mut fields = McpExchange {
            version: self.version.clone(),
            session: self.session.clone(),
            response_session: None,
            response_id: expected,
        };
        let reply = mcp_exchange(&HttpRequest {
            url: &self.url, method: "POST", content_type: "application/json",
            body: Secret::new(message.dump()), bearer: self.bearer.clone(), body_limit: FRAME,
            timeout_ms: timeout.as_millis().max(1) as u64,
        }, stop, &mut fields)?;
        if self.version.is_none() {
            self.session = fields.response_session;
        } else if fields.response_session.is_some() && self.session != fields.response_session {
            return Err("mcp_session_changed");
        }
        match (expected, reply.head.status) {
            (None, 202) if reply.body.is_empty() => Ok(None),
            (Some(_), 200) if reply.head.content_types.len() == 1
                && matches!(reply.head.content_types[0].split(';').next(), Some("application/json" | "text/event-stream")) =>
                    Json::parse(&reply.body).map(Some),
            (_, 401 | 403) => Err("mcp_auth_required"),
            (_, 404) => Err("mcp_session_expired"),
            _ => Err("mcp_http_response"),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        if self.session.is_none() || self.version.is_none() {
            return;
        }
        let mut fields = McpExchange {
            version: self.version.take(), session: self.session.take(),
            response_session: None, response_id: None,
        };
        let request = HttpRequest {
            url: &self.url, method: "DELETE", content_type: "", body: Secret::new(String::new()),
            bearer: self.bearer.clone(), body_limit: 4096, timeout_ms: 500,
        };
        let _ = mcp_exchange(&request, &Cancel::default(), &mut fields);
    }
}

enum Link {
    Stdio(Process),
    Http(Http),
}
struct Connection {
    next: usize,
    link: Link,
    setup_deadline: Option<Instant>,
}
impl Connection {
    fn open(transport: &Transport, cwd: &Path, stop: &Cancel, deadline: Instant) -> Result<Self> {
        let link = match transport {
            Transport::Stdio { command, args } => Link::Stdio(Process::open(command, args, cwd)?),
            Transport::Http { url, bearer_env } => {
                let bearer = bearer_env.as_ref().map(|name| {
                    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                        return Err("mcp_bearer_env");
                    }
                    std::env::var(name).ok()
                        .filter(|value| !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                        .map(Secret::new).ok_or("mcp_auth_required")
                }).transpose()?;
                Link::Http(Http { url: url.clone(), bearer, session: None, version: None })
            }
        };
        let mut connection = Self { link, next: 0, setup_deadline: Some(deadline) };
        let result = connection.call("initialize", Json::parse(&serde_json::json!({
            "protocolVersion": VERSION,
            "capabilities": {},
            "clientInfo": { "name": "ansa", "version": env!("CARGO_PKG_VERSION") },
        }).to_string())?, stop)?;
        let version = result.text_field("protocolVersion")?;
        if !matches!(version, VERSION | "2025-06-18" | "2025-03-26")
            || !result.field("capabilities")?.field("tools")?.is_object()
        {
            return Err("mcp_protocol");
        }
        if let Link::Http(http) = &mut connection.link {
            http.version = Some(version.to_owned());
        }
        let notification = Json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
        connection.exchange(&notification, None, stop)?;
        Ok(connection)
    }
    fn exchange(&mut self, message: &Json, id: Option<usize>, stop: &Cancel) -> Result<Option<Json>> {
        stop.check()?;
        let now = Instant::now();
        let deadline = self.setup_deadline.unwrap_or(now + TIMEOUT);
        let timeout = deadline.checked_duration_since(now).ok_or("mcp_timeout")?;
        match &mut self.link {
            Link::Stdio(process) => {
                process.send(message, stop, deadline)?;
                id.map(|id| process.receive(id, stop, deadline)).transpose()
            }
            Link::Http(http) => http.exchange(message, id, stop, timeout),
        }
    }
    fn call(&mut self, method: &str, params: Json, stop: &Cancel) -> Result<Json> {
        self.next = self.next.checked_add(1).ok_or("mcp_id")?;
        let message = Json::object(vec![
            ("jsonrpc", Json::string("2.0")?),
            ("id", Json::parse(&self.next.to_string())?),
            ("method", Json::string(method)?),
            ("params", params),
        ])?;
        let reply = self.exchange(&message, Some(self.next), stop)?.ok_or("mcp_missing_response")?;
        if reply.text_field("jsonrpc")? != "2.0" || reply.usize_field("id")? != self.next || reply.has("method") {
            return Err("mcp_response_identity");
        }
        match (reply.field("result"), reply.has("error")) {
            (Ok(result), false) if result.is_object() => Ok(result.clone()),
            (Err(_), true) if reply.field("error").is_ok_and(|error| {
                error.field("code").is_ok_and(Json::is_integer)
                    && error.text_field("message").is_ok()
            }) => Err("mcp_rpc_error"),
            _ => Err("mcp_invalid_result"),
        }
    }
    fn tools(&mut self, stop: &Cancel) -> Result<BTreeMap<String, Json>> {
        let mut cursor = None;
        let mut tools = BTreeMap::new();
        let mut pages = BTreeSet::new();
        for _ in 0..16 {
            let params = Json::object(cursor.take().map(|cursor| ("cursor", cursor)).into_iter().collect())?;
            let list = self.call("tools/list", params, stop)?;
            for tool in list.field("tools")?.elements()? {
                let name = tool.text_field("name")?;
                if tools.len() >= 256 || !valid_name(name) || tools.insert(name.to_owned(), tool.clone()).is_some() {
                    return Err("mcp_catalog");
                }
            }
            let Ok(next) = list.field("nextCursor") else { return Ok(tools); };
            if !pages.insert(next.text()?.to_owned()) {
                return Err("mcp_catalog_cursor");
            }
            cursor = Some(next.clone());
        }
        Err("mcp_catalog_pages")
    }
}
struct Call {
    params: Json,
    catalog_entry: Arc<Json>,
    stop: Cancel,
    reply: mpsc::SyncSender<Outcome>,
}
struct Worker {
    caller: Option<mpsc::SyncSender<Call>>,
    thread: Option<JoinHandle<()>>,
    stop: Cancel,
}
impl Worker {
    fn start(transport: Transport, cwd: PathBuf, setup: Cancel, deadline: Instant) -> Result<(Self, BTreeMap<String, Json>)> {
        let (ready, initial) = mpsc::sync_channel(1);
        let (caller, calls) = mpsc::sync_channel::<Call>(1);
        let stop = Cancel::default();
        let lifetime = stop.clone();
        let thread = std::thread::Builder::new().name("ansa-mcp".into()).spawn(move || {
            let initialized = Connection::open(&transport, &cwd, &setup, deadline).and_then(|mut connection| {
                let catalog = connection.tools(&setup)?;
                connection.setup_deadline = None;
                Ok((connection, catalog))
            });
            let mut connection = match initialized {
                Ok((connection, catalog)) if ready.send(Ok(catalog.clone())).is_ok() => connection,
                Ok(_) => return,
                Err(code) => { let _ = ready.send(Err(code)); return; }
            };
            while let Ok(call) = calls.recv() {
                let stop = Cancel::combined(&lifetime, &call.stop);
                if stop.cancelled() {
                    let _ = call.reply.send(Outcome::not_started());
                    continue;
                }
                connection.setup_deadline = Some(Instant::now() + TIMEOUT);
                let name = call.params.text_field("name").unwrap_or_default();
                let verified = connection.tools(&stop).is_ok_and(|tools| {
                    tools.get(name) == Some(call.catalog_entry.as_ref())
                });
                if !verified {
                    let _ = call.reply.send(Outcome::failed_known(r#"{"error":"mcp_tool_catalog_changed"}"#.into()));
                    break;
                }
                if stop.cancelled() {
                    let _ = call.reply.send(Outcome::not_started());
                    connection.setup_deadline = None;
                    continue;
                }
                match connection.call("tools/call", call.params, &stop) {
                    Ok(result) => {
                        let _ = call.reply.send(tool_result(&result));
                    }
                    Err("mcp_rpc_error") => {
                        let _ = call.reply.send(Outcome::failed_known(r#"{"error":"mcp_rpc_error"}"#.into()));
                    }
                    Err(_) => { let _ = call.reply.send(Outcome::failed_unknown()); break; }
                }
                connection.setup_deadline = None;
            }
        }).map_err(|_| "mcp_worker")?;
        let worker = Self { caller: Some(caller), thread: Some(thread), stop };
        let catalog = initial.recv().map_err(|_| "mcp_worker")??;
        Ok((worker, catalog))
    }
    fn invoke(&self, params: Json, catalog_entry: Arc<Json>, stop: &Cancel) -> Outcome {
        let (reply, result) = mpsc::sync_channel(1);
        if self.caller.as_ref().is_none_or(|caller| caller.try_send(Call { params, catalog_entry, stop: stop.clone(), reply }).is_err()) {
            return Outcome::failed_known(r#"{"error":"mcp_disconnected"}"#.into());
        }
        result.recv().unwrap_or_else(|_| Outcome::failed_unknown())
    }
}
fn tool_result(result: &Json) -> Outcome {
    if !result.field("content").is_ok_and(|content| content.elements().is_ok())
        && !result.field("structuredContent").is_ok_and(Json::is_object)
    {
        return Outcome::failed_unknown();
    }
    match result.field("isError").map_or(Ok(false), Json::boolean) {
        Ok(true) => Outcome::failed_known(result.dump()),
        Ok(false) => Outcome::succeeded(result.dump()),
        Err(_) => Outcome::failed_unknown(),
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.cancel();
        drop(self.caller.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn arguments(value: &Json) -> Result<Json> {
    let decoded = Json::parse(value.text_field("arguments_json")?)?;
    if !decoded.is_object() {
        return Err("mcp_arguments");
    }
    Ok(decoded)
}
fn local_name(server: &str, tool: &str) -> String {
    let readable = tool.bytes().map(|b| if b.is_ascii_alphanumeric() { b as char } else { '_' })
        .take(36).collect::<String>();
    let digest = sha256(format!("{server}/{tool}").as_bytes());
    format!("mcp_{readable}_{}", &digest[..16])
}
fn invoke(
    worker: &Worker,
    name: &str,
    catalog_entry: Arc<Json>,
    args: &Json,
    stop: &Cancel,
) -> Outcome {
    if stop.cancelled() {
        return Outcome::not_started();
    }
    let params = arguments(args).and_then(|args| Json::object(vec![
        ("name", Json::string(name)?), ("arguments", args),
    ]));
    let Ok(params) = params else { return Outcome::not_started(); };
    worker.invoke(params, catalog_entry, stop)
}

pub(crate) fn register(
    registry: &mut Registry,
    directory: &Path,
    cwd: &Path,
    saved: Option<&Advertisement>,
    stop: &Cancel,
) -> Result<()> {
    if saved.is_some_and(|tools| !tools.descriptors.iter().any(|tool| tool.name.starts_with("mcp_"))) {
        return Ok(());
    }
    match directory.join("mcp.json").symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("mcp_config_file"),
        Ok(_) => {}
    }
    let private = match ansa_linux::fs::private_directory(directory) {
        Ok(private) => private,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("mcp_config_directory"),
    };
    let Some(bytes) = ansa_linux::local::read_regular(&private, "mcp.json", FRAME).map_err(|_| "mcp_config_file")? else {
        return Ok(());
    };
    let json = Json::parse(std::str::from_utf8(&bytes).map_err(|_| "mcp_config_utf8")?)?;
    let settings: Settings = serde_json::from_str(&json.dump()).map_err(|_| "mcp_config_schema")?;
    if settings.servers.len() > 8 {
        return Err("mcp_config_servers");
    }
    let deadline = Instant::now() + TIMEOUT;
    for (server, settings) in settings.servers {
        stop.check()?;
        if !valid_name(&server) || settings.tools.is_empty() || settings.tools.len() > 32
            || settings.tools.iter().any(|tool| !valid_name(tool))
            || !settings.read_only.is_subset(&settings.tools)
        {
            return Err("mcp_config_tools");
        }
        let selected: Vec<_> = settings.tools.iter().filter(|name| {
            saved.is_none_or(|tools| tools.descriptors.iter().any(|tool| tool.name == local_name(&server, name)))
        }).collect();
        if selected.is_empty() {
            continue;
        }
        let settings_digest = sha256(serde_json::to_string(&settings.connection).map_err(|_| "mcp_config_schema")?.as_bytes());
        let (worker, catalog) = Worker::start(settings.connection, cwd.to_owned(), stop.clone(), deadline)?;
        let worker = Arc::new(worker);
        for name in selected {
            let tool = catalog.get(name).ok_or("mcp_tool_unavailable")?;
            let schema = tool.field("inputSchema")?;
            if schema.text_field("type")? != "object" || schema.dump().len() > 8192 {
                return Err("mcp_tool_schema");
            }
            let description = tool.text_field("description").unwrap_or_default();
            let read_only = settings.read_only.contains(name);
            let revision = sha256(format!("{settings_digest}/{read_only}/{}", tool.dump()).as_bytes());
            let descriptor = Descriptor {
                name: local_name(&server, name),
                description: format!(
                    "{server}/{name}: {}. Pass arguments as a JSON object matching {}",
                    description.chars().take(512).collect::<String>(), schema.dump(),
                ),
                revision,
                schema: Json::parse(WRAPPER)?,
            };
            let catalog_entry = Arc::new(tool.clone());
            let worker = worker.clone();
            let name = name.clone();
            registry.register_with_policy(Definition {
                descriptor, validator: Arc::new(|args| arguments(args).is_ok()),
                executor: Arc::new(|_| Outcome::not_started()),
            }, Policy {
                invoke: Some(Arc::new(move |invocation, stop| invoke(&worker, &name, catalog_entry.clone(), &invocation.arguments, stop))),
                requires_journal: true,
                read_only,
                ..Policy::default()
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::replay::{Effects, Execution};

    const TOOL: &str = r#"{"name":"echo","inputSchema":{"type":"object"}}"#;

    fn params() -> Json {
        Json::parse(r#"{"name":"echo","arguments":{"value":"fixture"}}"#).unwrap()
    }

    #[test]
    fn stdio_discovers_then_executes_verified_tool_and_handles_ping() {
        let shell = option_env!("ANSA_TEST_BASH").unwrap_or("/bin/bash");
        let args = [
            "-c",
            r#"while IFS= read -r message; do
                case "$message" in
                *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}' ;;
                *'"id":2,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
                *'"id":3,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
                *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":"probe","method":"ping"}'
                    IFS= read -r ping
                    [[ "$ping" == '{"jsonrpc":"2.0","id":"probe","result":{}}' ]] || exit 2
                    printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"fixture"}]}}' ;;
                esac
            done"#,
        ].map(str::to_owned).to_vec();
        let stop = Cancel::default();
        let (worker, catalog) = Worker::start(
            Transport::Stdio { command: shell.into(), args },
            std::env::temp_dir(), stop.clone(), Instant::now() + TIMEOUT,
        ).unwrap();
        assert_eq!(catalog.get("echo"), Some(&Json::parse(TOOL).unwrap()));
        let outcome = worker.invoke(params(), Arc::new(catalog["echo"].clone()), &stop);
        assert_eq!((outcome.execution(), outcome.effects()), (Execution::Succeeded, Effects::KnownResult));
        assert!(std::str::from_utf8(outcome.output()).unwrap().contains("fixture"));
    }

    #[test]
    fn streamable_http_negotiates_session_and_reads_sse_result() {
        use std::{io::{BufRead, BufReader}, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for index in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" { break; }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let length = headers.lines().find_map(|line| line.strip_prefix("content-length: "))
                    .map(|value| value.parse().unwrap()).unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                assert!(headers.contains("accept: application/json, text/event-stream"));
                if index > 0 {
                    assert!(headers.contains("mcp-session-id: fixture-session"));
                    assert!(headers.contains("mcp-protocol-version: 2025-11-25"));
                }
                let (status, mime, extra, body) = match index {
                    0 => ("200 OK", "application/json", "Mcp-Session-Id: fixture-session\r\n",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}"#.into()),
                    1 => ("202 Accepted", "application/json", "", String::new()),
                    2 | 3 => ("200 OK", "application/json", "", format!("{{\"jsonrpc\":\"2.0\",\"id\":{index},\"result\":{{\"tools\":[{TOOL}]}}}}")),
                    4 => ("200 OK", "text/event-stream", "", "data: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fixture\"}]}}\n\n".into()),
                    _ => { assert!(headers.starts_with("delete /mcp ")); ("200 OK", "application/json", "", String::new()) }
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let stop = Cancel::default();
        let launched = Worker::start(
            Transport::Http { url, bearer_env: None },
            std::env::temp_dir(), stop.clone(), Instant::now() + TIMEOUT,
        );
        let (server, catalog) = launched.unwrap();
        let outcome = server.invoke(params(), Arc::new(catalog["echo"].clone()), &stop);
        assert_eq!((outcome.execution(), outcome.effects()), (Execution::Succeeded, Effects::KnownResult));
        drop(server);
        worker.join().unwrap();
    }
}
