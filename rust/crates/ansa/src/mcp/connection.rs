//! Bounded, negotiated JSON-RPC over explicitly configured stdio or HTTP.
use super::{FRAME, Features, Json, Result, TIMEOUT, Transport, VERSION, valid_name};
use crate::{
    auth::{Cancel, Secret, manager::HttpRequest},
    transport::client::{McpExchange, mcp_exchange},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{ErrorKind, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};
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
        launch
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ansa_linux::managed::spawn(launch, None, Some(std::process::id()))
            .map_err(|_| "mcp_spawn")?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("mcp_pipes");
        };
        let process = Self {
            child,
            input: Some(input),
            output,
            buffer: Vec::new(),
        };
        ansa_linux::managed::nonblocking(process.input.as_ref().expect("owned stdin"))
            .map_err(|_| "mcp_pipes")?;
        ansa_linux::managed::nonblocking(&process.output).map_err(|_| "mcp_pipes")?;
        Ok(process)
    }
    fn send(
        &mut self,
        message: &Json,
        stop: &Cancel,
        deadline: Instant,
        may_have_sent: &mut bool,
    ) -> Result<()> {
        if message.encoded_bytes() >= FRAME {
            return Err("mcp_frame");
        }
        let mut frame = message.dump().into_bytes();
        frame.push(b'\n');
        let mut left = frame.as_slice();
        while !left.is_empty() {
            stop.check()?;
            if Instant::now() >= deadline {
                return Err("mcp_timeout");
            }
            match self.input.as_mut().ok_or("mcp_stream_closed")?.write(left) {
                Ok(0) => return Err("mcp_write"),
                Ok(count) => {
                    *may_have_sent = true;
                    left = &left[count..];
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
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
            if let Some(relative) = self.buffer[scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = scanned + relative;
                let message =
                    Json::parse(std::str::from_utf8(&self.buffer[..end]).map_err(|_| "mcp_utf8")?)?;
                self.buffer.drain(..=end);
                scanned = 0;
                if message.usize_field("id").ok() == Some(id) && !message.has("method") {
                    return Ok(message);
                }
                if message.text_field("jsonrpc") == Ok("2.0")
                    && !message.has("result")
                    && !message.has("error")
                    && let (Ok(method), Ok(server_id)) =
                        (message.text_field("method"), message.field("id"))
                    && (server_id.is_string() || server_id.is_integer())
                {
                    let (key, value) = if method == "ping" {
                        ("result", Json::parse("{}")?)
                    } else {
                        (
                            "error",
                            Json::parse(r#"{"code":-32601,"message":"Method not found"}"#)?,
                        )
                    };
                    self.send(
                        &Json::object(vec![
                            ("jsonrpc", Json::string("2.0")?),
                            ("id", server_id.clone()),
                            (key, value),
                        ])?,
                        stop,
                        deadline,
                        &mut false,
                    )?;
                    continue;
                }
                if message.text_field("jsonrpc") != Ok("2.0")
                    || message.text_field("method").is_err()
                    || message.has("id")
                    || message.has("result")
                    || message.has("error")
                    || message.text_field("method").ok() == Some("notifications/tools/list_changed")
                {
                    return Err("mcp_catalog_changed_or_unexpected_message");
                }
                continue;
            }
            scanned = self.buffer.len();
            if scanned == FRAME {
                return Err("mcp_frame");
            }
            match self.output.read(&mut chunk[..(FRAME - scanned).min(8192)]) {
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
    fn exchange(
        &mut self,
        message: &Json,
        expected: Option<usize>,
        stop: &Cancel,
        timeout: Duration,
    ) -> Result<Option<Json>> {
        let mut fields = McpExchange {
            version: self.version.clone(),
            session: self.session.clone(),
            response_session: None,
            response_id: expected,
        };
        let reply = mcp_exchange(
            &HttpRequest {
                url: &self.url,
                method: "POST",
                content_type: "application/json",
                body: Secret::new(message.dump()),
                bearer: self.bearer.clone(),
                body_limit: FRAME,
                timeout_ms: Some(timeout.as_millis().max(1) as u64),
            },
            stop,
            &mut fields,
        )?;
        if self.version.is_none() {
            self.session = fields.response_session;
        } else if fields.response_session.is_some() && self.session != fields.response_session {
            return Err("mcp_session_changed");
        }
        match (expected, reply.head.status) {
            (None, 202) if reply.body.is_empty() => Ok(None),
            (Some(_), 200)
                if reply.head.content_types.len() == 1
                    && matches!(
                        reply.head.content_types[0].split(';').next(),
                        Some("application/json" | "text/event-stream")
                    ) =>
            {
                Json::parse(&reply.body).map(Some)
            }
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
            version: self.version.take(),
            session: self.session.take(),
            response_session: None,
            response_id: None,
        };
        let request = HttpRequest {
            url: &self.url,
            method: "DELETE",
            content_type: "",
            body: Secret::new(String::new()),
            bearer: self.bearer.clone(),
            body_limit: 4096,
            timeout_ms: Some(500),
        };
        let _ = mcp_exchange(&request, &Cancel::default(), &mut fields);
    }
}

enum Link {
    Stdio(Process),
    Http(Http),
}
pub(super) struct Connection {
    next: usize,
    link: Link,
    pub(super) setup_deadline: Option<Instant>,
}
impl Connection {
    pub(super) fn open(
        transport: &Transport,
        features: Features,
        cwd: &Path,
        stop: &Cancel,
        deadline: Instant,
    ) -> Result<Self> {
        stop.check()?;
        if Instant::now() >= deadline {
            return Err("mcp_timeout");
        }
        let link = match transport {
            Transport::Stdio { command, args } => Link::Stdio(Process::open(command, args, cwd)?),
            Transport::Http { url, bearer_env } => {
                let bearer = bearer_env
                    .as_ref()
                    .map(|name| {
                        if name.is_empty()
                            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        {
                            return Err("mcp_bearer_env");
                        }
                        std::env::var(name)
                            .ok()
                            .filter(|value| {
                                !value.is_empty()
                                    && value.len() <= 8192
                                    && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
                            })
                            .map(Secret::new)
                            .ok_or("mcp_auth_required")
                    })
                    .transpose()?;
                Link::Http(Http {
                    url: url.clone(),
                    bearer,
                    session: None,
                    version: None,
                })
            }
        };
        let mut connection = Self {
            link,
            next: 0,
            setup_deadline: Some(deadline),
        };
        let result = connection.call(
            "initialize",
            Json::parse(
                &serde_json::json!({
                    "protocolVersion": VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "ansa", "version": env!("CARGO_PKG_VERSION") },
                })
                .to_string(),
            )?,
            stop,
            &mut false,
        )?;
        let version = result
            .text_field("protocolVersion")
            .map_err(|_| "mcp_protocol")?;
        if !matches!(version, VERSION | "2025-06-18" | "2025-03-26") {
            return Err("mcp_protocol");
        }
        if let Link::Http(http) = &mut connection.link {
            http.version = Some(version.to_owned());
        }
        features.require(result.field("capabilities").map_err(|_| "mcp_protocol")?)?;
        let notification =
            Json::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)?;
        connection.exchange(&notification, None, stop, &mut false)?;
        Ok(connection)
    }
    fn exchange(
        &mut self,
        message: &Json,
        id: Option<usize>,
        stop: &Cancel,
        may_have_sent: &mut bool,
    ) -> Result<Option<Json>> {
        stop.check()?;
        let now = Instant::now();
        let deadline = self.setup_deadline.unwrap_or(now + TIMEOUT);
        let timeout = deadline.checked_duration_since(now).ok_or("mcp_timeout")?;
        match &mut self.link {
            Link::Stdio(process) => {
                process.send(message, stop, deadline, may_have_sent)?;
                id.map(|id| process.receive(id, stop, deadline)).transpose()
            }
            Link::Http(http) => {
                if message.encoded_bytes() > FRAME {
                    return Err("mcp_frame");
                }
                // The bridge doesn't expose the POST's dispatch phase. Once
                // handed over, a failure may have happened after dispatch.
                *may_have_sent = true;
                http.exchange(message, id, stop, timeout)
            }
        }
    }
    pub(super) fn call(
        &mut self,
        method: &str,
        params: Json,
        stop: &Cancel,
        may_have_sent: &mut bool,
    ) -> Result<Json> {
        self.next = self.next.checked_add(1).ok_or("mcp_id")?;
        let message = Json::object(vec![
            ("jsonrpc", Json::string("2.0")?),
            ("id", Json::parse(&self.next.to_string())?),
            ("method", Json::string(method)?),
            ("params", params),
        ])?;
        let reply = self
            .exchange(&message, Some(self.next), stop, may_have_sent)?
            .ok_or("mcp_missing_response")?;
        if reply.text_field("jsonrpc") != Ok("2.0")
            || reply.usize_field("id") != Ok(self.next)
            || reply.has("method")
        {
            return Err("mcp_response_identity");
        }
        match (reply.field("result"), reply.has("error")) {
            (Ok(result), false) if result.is_object() => Ok(result.clone()),
            (Err(_), true)
                if reply.field("error").is_ok_and(|error| {
                    error.field("code").is_ok_and(Json::is_integer)
                        && error.text_field("message").is_ok()
                }) =>
            {
                Err("mcp_rpc_error")
            }
            _ => Err("mcp_invalid_result"),
        }
    }
    pub(super) fn tools(&mut self, stop: &Cancel) -> Result<BTreeMap<String, Json>> {
        let mut cursor = None;
        let mut tools = BTreeMap::new();
        let mut pages = BTreeSet::new();
        for _ in 0..16 {
            let params = Json::object(
                cursor
                    .take()
                    .map(|cursor| ("cursor", cursor))
                    .into_iter()
                    .collect(),
            )?;
            let list = self.call("tools/list", params, stop, &mut false)?;
            for tool in list.field("tools")?.elements()? {
                let name = tool.text_field("name")?;
                if tools.len() >= 256
                    || !valid_name(name)
                    || tools.insert(name.to_owned(), tool.clone()).is_some()
                {
                    return Err("mcp_catalog");
                }
            }
            let Ok(next) = list.field("nextCursor") else {
                return Ok(tools);
            };
            if !pages.insert(next.text()?.to_owned()) {
                return Err("mcp_catalog_cursor");
            }
            cursor = Some(next.clone());
        }
        Err("mcp_catalog_pages")
    }
}
