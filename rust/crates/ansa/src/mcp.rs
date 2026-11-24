//! Explicitly configured MCP tools use the same durable admission as local tools.
mod config;
mod connection;
use connection::Connection;
mod discovery;
mod schema;
pub(crate) use self::config::cli;
use self::{
    config::{Config, Transport},
    discovery::{Features, Method},
};
use crate::{
    auth::Cancel,
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        replay::{Effects, Execution},
        request::{Advertisement, Descriptor},
    },
    tools::registry::{Definition, Outcome, Policy, Registry},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;
const VERSION: &str = "2025-11-25";
const FRAME: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const WRAPPER: &str = r#"{"type":"object","properties":{"arguments_json":{"type":"string","description":"JSON object matching the MCP input schema"}},"required":["arguments_json"],"additionalProperties":false}"#;

enum Operation {
    Tool(Arc<Json>),
    Discovery(Method),
}

struct Call {
    params: Json,
    operation: Operation,
    stop: Cancel,
    reply: mpsc::SyncSender<Outcome>,
}
struct Worker {
    caller: Option<mpsc::SyncSender<Call>>,
    thread: Option<JoinHandle<()>>,
    stop: Cancel,
}
impl Worker {
    fn start(
        transport: Transport,
        features: Features,
        cwd: PathBuf,
        setup: Cancel,
        deadline: Instant,
    ) -> Result<(Self, BTreeMap<String, Json>)> {
        let (ready, initial) = mpsc::sync_channel(1);
        let (caller, calls) = mpsc::sync_channel::<Call>(1);
        let stop = Cancel::default();
        let lifetime = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ansa-mcp".into())
            .spawn(move || {
                let initialized = Connection::open(&transport, features, &cwd, &setup, deadline)
                    .and_then(|mut connection| {
                        let catalog = if features.tools {
                            connection.tools(&setup)?
                        } else {
                            BTreeMap::new()
                        };
                        connection.setup_deadline = None;
                        Ok((connection, catalog))
                    });
                let (mut connection, catalog) = match initialized {
                    Ok(ready) => ready,
                    Err(code) => {
                        let _ = ready.send(Err(code));
                        return;
                    }
                };
                if ready.send(Ok(catalog)).is_err() {
                    return;
                }
                while let Ok(call) = calls.recv() {
                    let stop = Cancel::combined(&lifetime, &call.stop);
                    if stop.cancelled() {
                        let _ = call.reply.send(Outcome::not_started());
                        continue;
                    }
                    connection.setup_deadline = Some(Instant::now() + TIMEOUT);
                    let discovery = match call.operation {
                        Operation::Tool(catalog_entry) => {
                            let name = call.params.text_field("name").unwrap_or_default();
                            let verification = connection.tools(&stop).and_then(|tools| {
                                (tools.get(name) == Some(catalog_entry.as_ref()))
                                    .then_some(())
                                    .ok_or("mcp_tool_catalog_changed")
                            });
                            if let Err(code) = verification {
                                // Only tools/list was attempted. A late reply to it
                                // may still be outstanding, so retire this worker.
                                let outcome = if code == "auth_cancelled" {
                                    Outcome::not_started()
                                } else {
                                    discovery::failure(code)
                                };
                                drop(calls);
                                let _ = call.reply.send(outcome);
                                break;
                            }
                            None
                        }
                        Operation::Discovery(method) => Some(method),
                    };
                    let rpc = discovery.map_or("tools/call", Method::rpc);
                    let mut may_have_sent = false;
                    let outcome = match connection.call(rpc, call.params, &stop, &mut may_have_sent)
                    {
                        Ok(result) => match discovery {
                            Some(method) => method
                                .result(&result)
                                .map_or_else(discovery::failure, Outcome::succeeded),
                            None => tool_result(&result),
                        },
                        Err("mcp_rpc_error") => discovery::failure("mcp_rpc_error"),
                        Err("auth_cancelled") if !may_have_sent => Outcome::not_started(),
                        Err("mcp_frame") if !may_have_sent => discovery::failure("mcp_frame"),
                        Err(code) => {
                            let outcome = if !may_have_sent {
                                discovery::failure(code)
                            } else if discovery.is_some() {
                                // Even a nominally read-only remote method may
                                // have run when transport lost the reply.
                                Outcome::from_bytes(
                                    Execution::Failed,
                                    Effects::Unknown,
                                    format!(r#"{{"error":"{code}"}}"#).into_bytes(),
                                )
                            } else {
                                Outcome::failed_unknown()
                            };
                            drop(calls);
                            let _ = call.reply.send(outcome);
                            break;
                        }
                    };
                    let _ = call.reply.send(outcome);
                    connection.setup_deadline = None;
                }
            })
            .map_err(|_| "mcp_worker")?;
        let worker = Self {
            caller: Some(caller),
            thread: Some(thread),
            stop,
        };
        let catalog = initial.recv().map_err(|_| "mcp_worker")??;
        Ok((worker, catalog))
    }
    fn invoke(&self, params: Json, catalog_entry: Arc<Json>, stop: &Cancel) -> Outcome {
        self.submit(Operation::Tool(catalog_entry), params, stop)
    }
    fn invoke_discovery(&self, method: Method, params: Json, stop: &Cancel) -> Outcome {
        self.submit(Operation::Discovery(method), params, stop)
    }
    fn submit(&self, operation: Operation, params: Json, stop: &Cancel) -> Outcome {
        let (reply, result) = mpsc::sync_channel(1);
        if self.caller.as_ref().is_none_or(|caller| {
            caller
                .try_send(Call {
                    params,
                    operation,
                    stop: stop.clone(),
                    reply,
                })
                .is_err()
        }) {
            return discovery::failure("mcp_disconnected");
        }
        result.recv().unwrap_or_else(|_| Outcome::failed_unknown())
    }
}
fn tool_result(result: &Json) -> Outcome {
    let content = result.field("content").ok();
    let structured = result.field("structuredContent").ok();
    let content_valid = content.is_none_or(|content| {
        content.elements().is_ok_and(|items| {
            items.iter().all(|item| match item.text_field("type") {
                Ok("text") => item.text_field("text").is_ok(),
                Ok("image" | "audio") => {
                    item.text_field("data").is_ok() && item.text_field("mimeType").is_ok()
                }
                Ok("resource") => item.field("resource").is_ok_and(|resource| {
                    resource.text_field("uri").is_ok()
                        && (resource.text_field("text").is_ok()
                            || resource.text_field("blob").is_ok())
                }),
                Ok("resource_link") => {
                    item.text_field("uri").is_ok() && item.text_field("name").is_ok()
                }
                _ => false,
            })
        })
    });
    if (content.is_none() && structured.is_none())
        || !content_valid
        || !structured.is_none_or(Json::is_object)
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
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn arguments(value: &Json) -> Result<Json> {
    let decoded = Json::parse(value.text_field("arguments_json")?)?;
    if !decoded.is_object() || !schema::supported_numbers(&decoded) {
        return Err("mcp_arguments");
    }
    Ok(decoded)
}
fn local_name(server: &str, tool: &str) -> String {
    let readable = tool
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '_'
            }
        })
        .take(36)
        .collect::<String>();
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
    let params = arguments(args)
        .and_then(|args| Json::object(vec![("name", Json::string(name)?), ("arguments", args)]));
    let Ok(params) = params else {
        return Outcome::not_started();
    };
    worker.invoke(params, catalog_entry, stop)
}

pub(crate) fn register(
    registry: &mut Registry,
    directory: &Path,
    cwd: &Path,
    saved: Option<&Advertisement>,
    stop: &Cancel,
) -> Result<()> {
    let pinned = saved.map(|tools| {
        tools
            .descriptors
            .iter()
            .map(|tool| tool.name.as_str())
            .filter(|name| name.starts_with("mcp_"))
            .collect::<BTreeSet<_>>()
    });
    if pinned.as_ref().is_some_and(BTreeSet::is_empty) {
        return Ok(());
    }
    let Some(config) = Config::load(directory)? else {
        return if pinned.is_some() {
            Err("session_fork_tools")
        } else {
            Ok(())
        };
    };
    // Check the entire static subset before starting any transport. A removed
    // or disabled pinned server must not cause a partial reopen or activate
    // another server. The caller still checks exact descriptor compatibility.
    let mut missing = pinned.clone().unwrap_or_default();
    let mut plan = Vec::new();
    let wanted = |name: String| {
        pinned
            .as_ref()
            .is_none_or(|names| names.contains(name.as_str()))
    };
    for (server, settings) in &config.servers {
        stop.check()?;
        let selected: Vec<_> = settings
            .tools
            .iter()
            .filter(|name| wanted(local_name(server, name)))
            .collect();
        let discovery: Vec<_> = Method::ALL
            .into_iter()
            .filter(|method| {
                method.enabled(settings.resources, settings.prompts)
                    && wanted(method.local_name(server))
            })
            .collect();
        if (selected.is_empty() && discovery.is_empty()) || !config.enabled(server)? {
            continue;
        }
        for name in &selected {
            missing.remove(local_name(server, name).as_str());
        }
        for method in &discovery {
            missing.remove(method.local_name(server).as_str());
        }
        plan.push((server, settings, selected, discovery));
    }
    if !missing.is_empty() {
        return Err("session_fork_tools");
    }
    let deadline = Instant::now() + TIMEOUT;
    for (server, settings, selected, discovery) in plan {
        stop.check()?;
        // Keep the transport serialization used by legacy tool revisions. In
        // particular, enabling discovery cannot change a saved tools/call tool.
        let settings_digest = sha256(
            serde_json::to_string(&settings.connection)
                .map_err(|_| "mcp_config_schema")?
                .as_bytes(),
        );
        let features = Features {
            tools: !selected.is_empty(),
            resources: discovery.iter().any(|method| method.enabled(true, false)),
            prompts: discovery.iter().any(|method| method.enabled(false, true)),
        };
        let (worker, catalog) = Worker::start(
            settings.connection.clone(),
            features,
            cwd.to_owned(),
            stop.clone(),
            deadline,
        )?;
        let worker = Arc::new(worker);
        for name in selected {
            let tool = schema::canonical(catalog.get(name).ok_or("mcp_tool_unavailable")?)?;
            let schema = tool.field("inputSchema")?;
            if schema.text_field("type")? != "object" {
                return Err("mcp_tool_schema");
            }
            schema::check(schema, 0)?;
            let description = tool.text_field("description").unwrap_or_default();
            let read_only = settings.read_only.contains(name);
            let revision =
                sha256(format!("{settings_digest}/{read_only}/{}", tool.dump()).as_bytes());
            let descriptor = Descriptor {
                name: local_name(server, name),
                description: format!(
                    "{server}/{name}: {}. Pass arguments as a JSON object matching {}",
                    description,
                    schema.dump(),
                ),
                revision,
                schema: Json::parse(WRAPPER)?,
            };
            let validator_schema = schema.clone();
            let catalog_entry = Arc::new(tool);
            let worker = worker.clone();
            let name = name.clone();
            registry.register_with_policy(
                Definition {
                    descriptor,
                    validator: Arc::new(move |args| {
                        arguments(args)
                            .is_ok_and(|input| schema::matches(&validator_schema, &input))
                    }),
                    executor: Arc::new(|_| Outcome::not_started()),
                },
                Policy {
                    invoke: Some(Arc::new(move |invocation, stop| {
                        invoke(
                            &worker,
                            &name,
                            catalog_entry.clone(),
                            &invocation.arguments,
                            stop,
                        )
                    })),
                    requires_journal: true,
                    read_only,
                    ..Policy::default()
                },
            )?;
        }
        for method in discovery {
            method.register(registry, server, &settings_digest, worker.clone())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod management_tests;
