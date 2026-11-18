//! Explicit compiled tool set for an attached terminal scope. The host's
//! coordinator remains the authority for leases and evidence.
use crate::{
    auth::Cancel,
    protocol::json::Json,
    provider::{
        replay::{Effects, Execution},
        request::Descriptor,
    },
    terminal_host::{
        FileJournal, Host, Lifecycle, OperationStatus,
        native::{MAX_READ, NativeBackend},
        protocol::{Action, Binding, Effect, Id, Intent, Launch, MAX_INPUT, Size, Writer, sha256},
    },
    tools::{
        invocation::Invocation,
        registry::{Definition, Outcome, Policy, Recovery, Registry},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

type Result<T> = std::result::Result<T, &'static str>;
pub(crate) type Owner = Arc<Mutex<Host<FileJournal, NativeBackend>>>;

pub(crate) fn open_for(
    settings: &crate::config::AgentSettings,
    session: &str,
    directory: &Path,
    restoring: bool,
) -> Result<Owner> {
    let session = Id::try_from(session.to_owned())?;
    let binding = Binding {
        host: Id::try_from(format!("attached_{}", sha256(session.as_str().as_bytes())))?,
        workspace: crate::config::path_text(&settings.cwd)?.to_owned(),
        scope: session.clone(),
        session,
    };
    let backend = NativeBackend::new(
        std::env::current_exe().map_err(|_| "process_executable")?,
        settings.directory.join("terminal-workspaces"),
    );
    let path = directory.join("terminal-host-v1");
    let host = if restoring {
        Host::restore_file(&path, binding, backend)?
    } else {
        Host::create_file(&path, binding, backend)?
    };
    Ok(Arc::new(Mutex::new(host)))
}

pub(crate) fn close(owner: &Owner) -> bool {
    owner.lock().is_ok_and(|mut host| {
        if let Some(recovered) = host.recovery() {
            return recovered.closure_known;
        }
        super::daemon::close_owned(&mut host) && host.seal_clean().is_ok()
    })
}

#[derive(Clone, Copy)]
enum Tool {
    Create,
    Input,
    Resize,
    Interrupt,
    Close,
    List,
    Operation,
    Capture,
    Read,
}

impl Tool {
    fn descriptor(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    ) {
        match self {
            Self::Create => (
                "terminal_create",
                "Create an attached PTY in the session workspace. The same shell stays alive between terminal calls.",
                &[
                    ("label", "string"),
                    ("cwd", "string"),
                    ("argv", "array"),
                    ("rows", "integer"),
                    ("columns", "integer"),
                    ("lifetime_seconds", "integer"),
                    ("idle_seconds", "integer"),
                ],
            ),
            Self::Input => (
                "terminal_input",
                "Deliver bytes to an owned terminal. Encoding is utf8 or base64. Include an explicit CR for Enter. Receipt counts bytes, then read or capture to observe output.",
                &[
                    ("terminal", "string"),
                    ("epoch", "integer"),
                    ("encoding", "string"),
                    ("data", "string"),
                ],
            ),
            Self::Resize => (
                "terminal_resize",
                "Change terminal dimensions and notify its foreground program.",
                &[
                    ("terminal", "string"),
                    ("epoch", "integer"),
                    ("rows", "integer"),
                    ("columns", "integer"),
                ],
            ),
            Self::Interrupt => (
                "terminal_interrupt",
                "Request SIGINT for the foreground group attached to this terminal.",
                &[("terminal", "string"), ("epoch", "integer")],
            ),
            Self::Close => (
                "terminal_close",
                "Close an owned terminal and verify cleanup of its descendants.",
                &[("terminal", "string"), ("epoch", "integer")],
            ),
            Self::List => (
                "terminal_list",
                "List terminals in the current scope with owner, lifecycle and epoch.",
                &[],
            ),
            Self::Operation => (
                "terminal_operation",
                "Look up the receipt for an existing runtime operation ID.",
                &[("operation", "string")],
            ),
            Self::Capture => (
                "terminal_capture",
                "Capture the currently visible terminal grid. Use the epoch and visible_after from terminal_list.",
                &[
                    ("terminal", "string"),
                    ("epoch", "integer"),
                    ("after", "integer"),
                ],
            ),
            Self::Read => (
                "terminal_read",
                "Read up to 8192 retained output bytes after a cursor. Offsets track raw bytes; returned text replaces unsafe control characters and marks any retention gap.",
                &[
                    ("terminal", "string"),
                    ("epoch", "integer"),
                    ("after", "integer"),
                    ("limit", "integer"),
                ],
            ),
        }
    }

    fn mutating(self) -> bool {
        matches!(
            self,
            Self::Create | Self::Input | Self::Resize | Self::Interrupt | Self::Close
        )
    }
}

pub fn register(registry: &mut Registry, owner: Owner, conversation: &Path) -> Result<()> {
    {
        let mut host = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
        let stop = host.stop_handle();
        let quiescence = stop.clone();
        let fresh_turn = stop.clone();
        registry.bind_owner(
            host.binding().session.as_str(),
            conversation,
            Arc::new(move || stop.request()),
            Arc::new(move || quiescence.quiescent()),
            Arc::new(move || fresh_turn.begin_turn()),
        )?;
        host.bind_runtime()?;
    }
    for tool in [
        Tool::Create,
        Tool::Input,
        Tool::Resize,
        Tool::Interrupt,
        Tool::Close,
        Tool::List,
        Tool::Operation,
        Tool::Capture,
        Tool::Read,
    ] {
        let (name, description, fields) = tool.descriptor();
        let executor = owner.clone();
        let resolver = owner.clone();
        registry.register_with_policy(
            Definition {
                descriptor: Descriptor {
                    name: name.into(),
                    description: description.into(),
                    revision: "ansa_terminal_attached_v1".into(),
                    schema: schema(fields)?,
                },
                validator: Arc::new(move |args| Arguments::parse(tool, args).is_ok()),
                executor: Arc::new(|_| Outcome::failed_unknown()),
            },
            Policy {
                requires_journal: true,
                invoke: Some(Arc::new(move |invocation, stop| {
                    invoke(&executor, tool, invocation, stop).unwrap_or_else(rejected)
                })),
                recover: tool.mutating().then(|| -> Recovery {
                    Arc::new(move |invocation| recover(&resolver, tool, invocation).map(Some))
                }),
                // The fixture registry supplies no independently granted
                // terminal evidence scope to Plan.
                read_only: false,
            },
        )?;
    }
    Ok(())
}

fn schema(fields: &[(&str, &str)]) -> Result<Json> {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, kind) in fields {
        required.push(*name);
        let value = if *kind == "array" {
            json!({"type":"array","items":{"type":"string"}})
        } else {
            json!({"type":kind})
        };
        properties.insert((*name).into(), value);
    }
    Json::parse(&json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).to_string())
}

struct Mutation {
    terminal: Option<Id>,
    epoch: u64,
    action: Action,
    bytes: Vec<u8>,
}

impl Mutation {
    fn intent(
        mut self,
        owner: &Host<FileJournal, NativeBackend>,
        invocation: &Invocation,
    ) -> Result<(Intent, Vec<u8>)> {
        let operation = Id::try_from(invocation.operation.clone())?;
        // The admission layer owns every operation identity. A caller does
        // not supply an idempotency key or impersonate a human writer.
        let terminal = self
            .terminal
            .take()
            .unwrap_or_else(|| owner.terminal_id(&operation));
        if let Action::Create { launch } = &mut self.action {
            let raw = Path::new(&launch.cwd);
            let path = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                Path::new(&owner.binding().workspace).join(raw)
            };
            launch.cwd = ansa_linux::fs::normalize(&path)
                .into_os_string()
                .into_string()
                .map_err(|_| "terminal_cwd")?;
        }
        Ok((
            Intent {
                operation,
                terminal,
                binding: owner.binding().clone(),
                epoch: self.epoch,
                writer: Writer::Agent,
                invocation_digest: Some(crate::protocol::legacy_json_v1::sha256(
                    invocation.save()?.dump().as_bytes(),
                )),
                action: self.action,
            },
            self.bytes,
        ))
    }
}

enum Arguments {
    Mutate(Mutation),
    List,
    Operation(Id),
    Observe {
        terminal: Id,
        epoch: u64,
        after: u64,
        limit: Option<usize>,
    },
}

impl Arguments {
    fn parse(tool: Tool, args: &Json) -> Result<Self> {
        let id = |field| Id::try_from(args.text_field(field)?.to_owned());
        let size = || -> Result<Size> {
            let rows = u16::try_from(args.usize_field("rows")?).map_err(|_| "terminal_size")?;
            let columns =
                u16::try_from(args.usize_field("columns")?).map_err(|_| "terminal_size")?;
            let size = Size { rows, columns };
            if !size.validate() {
                return Err("terminal_size");
            }
            Ok(size)
        };
        if matches!(tool, Tool::Create) {
            let argv: Vec<_> = args
                .field("argv")?
                .elements()?
                .iter()
                .map(|argument| argument.text().map(str::to_owned))
                .collect::<Result<_>>()?;
            let cwd = args.text_field("cwd")?;
            if cwd.is_empty() || cwd.len() > 1024 || cwd.chars().any(char::is_control) {
                return Err("terminal_cwd");
            }
            let launch = Launch {
                label: args.text_field("label")?.to_owned(),
                cwd: "/".into(),
                argv,
                size: size()?,
                lifetime_seconds: u32::try_from(args.usize_field("lifetime_seconds")?)
                    .map_err(|_| "terminal_deadline")?,
                idle_seconds: u32::try_from(args.usize_field("idle_seconds")?)
                    .map_err(|_| "terminal_deadline")?,
            };
            if !launch.validate() {
                return Err("terminal_launch");
            }
            return Ok(Self::Mutate(Mutation {
                terminal: None,
                epoch: 0,
                action: Action::Create {
                    launch: Launch {
                        cwd: cwd.to_owned(),
                        ..launch
                    },
                },
                bytes: Vec::new(),
            }));
        }
        if matches!(tool, Tool::List) {
            return Ok(Self::List);
        }
        if matches!(tool, Tool::Operation) {
            return Ok(Self::Operation(id("operation")?));
        }
        let terminal = id("terminal")?;
        let epoch = args.usize_field("epoch")? as u64;
        if matches!(tool, Tool::Capture | Tool::Read) {
            let limit = if matches!(tool, Tool::Read) {
                let value = args.usize_field("limit")?;
                if !(1..=MAX_READ).contains(&value) {
                    return Err("terminal_read_limit");
                }
                Some(value)
            } else {
                None
            };
            return Ok(Self::Observe {
                terminal,
                epoch,
                after: args.usize_field("after")? as u64,
                limit,
            });
        }
        let mut bytes = Vec::new();
        let action = match tool {
            Tool::Input => {
                let data = args.text_field("data")?;
                bytes = match args.text_field("encoding")? {
                    "utf8" if data.len() <= MAX_INPUT => data.as_bytes().to_vec(),
                    "base64" if data.len() <= MAX_INPUT.div_ceil(3) * 4 => STANDARD
                        .decode(data)
                        .map_err(|_| "terminal_input_encoding")?,
                    _ => return Err("terminal_input_encoding"),
                };
                Action::input(Writer::Agent, &bytes).ok_or("terminal_input_limit")?
            }
            Tool::Resize => Action::Resize { size: size()? },
            Tool::Interrupt => Action::Interrupt,
            Tool::Close => Action::Close,
            _ => return Err("terminal_tool"),
        };
        Ok(Self::Mutate(Mutation {
            terminal: Some(terminal),
            epoch,
            action,
            bytes,
        }))
    }
}

fn invoke(owner: &Owner, tool: Tool, invocation: &Invocation, stop: &Cancel) -> Result<Outcome> {
    let args = Arguments::parse(tool, &invocation.arguments)?;
    let mut owner = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
    if stop.cancelled() {
        return Ok(Outcome::not_started());
    }
    let output = match args {
        Arguments::Mutate(mutation) => {
            let (intent, bytes) = mutation.intent(&owner, invocation)?;
            let status =
                owner.execute_if(Writer::Agent, intent.clone(), &bytes, || !stop.cancelled())?;
            return Ok(receipt(&intent, status));
        }
        Arguments::Operation(operation) => {
            json!({"operation":operation,"receipt":owner.operation(&operation).map(status_value),"quarantined":owner.quarantined()})
        }
        Arguments::List => {
            let terminals: Vec<_> = owner.terminals().clone().into_iter().map(|(id, terminal)| {
                let (state, exit) = match terminal.lifecycle {
                    Lifecycle::Reserved => ("reserved", None),
                    Lifecycle::Closed => ("closed", None),
                    Lifecycle::Lost => ("lost", None),
                    Lifecycle::Live => match owner.worker_status(&id) {
                        Ok(None) => ("live", None),
                        Ok(Some(exit)) if exit.cleanup_known => ("exited", Some(exit)),
                        Ok(Some(exit)) => ("cleanup_unknown", Some(exit)),
                        Err(_) => ("lost", None),
                    },
                };
                json!({"terminal":id,"label":terminal.launch.label,"state":state,"exit":exit,"owner":terminal.writer,"epoch":terminal.epoch,"visible_after":terminal.visible_after})
            }).collect();
            json!({"terminals":terminals,"quarantined":owner.quarantined()})
        }
        Arguments::Observe {
            terminal,
            epoch,
            after,
            limit,
        } => {
            if let Some(limit) = limit {
                json!({"terminal":terminal,"output":owner.read_output(Writer::Agent,&terminal,epoch,after,limit)?})
            } else {
                json!({"terminal":terminal,"screen":owner.capture(Writer::Agent,&terminal,epoch,after)?})
            }
        }
    };
    Ok(Outcome::succeeded(output.to_string()))
}

fn recover(owner: &Owner, tool: Tool, invocation: &Invocation) -> Result<Outcome> {
    let Arguments::Mutate(mutation) = Arguments::parse(tool, &invocation.arguments)? else {
        return Err("terminal_recovery_tool");
    };
    let owner = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
    let (intent, bytes) = mutation.intent(&owner, invocation)?;
    let status = owner
        .matching_receipt(&intent, &bytes)?
        .unwrap_or(OperationStatus::Unresolved);
    Ok(receipt(&intent, status))
}

fn status_value(status: OperationStatus) -> Value {
    match status {
        OperationStatus::Durable(effect) => json!({"durable":true,"effect":effect}),
        OperationStatus::KnownWithoutDurableReceipt(effect) => {
            json!({"durable":false,"effect":effect})
        }
        OperationStatus::Unresolved => json!({"durable":false,"effect":Effect::Unknown}),
    }
}

fn receipt(intent: &Intent, status: OperationStatus) -> Outcome {
    let (execution, effects) = match status {
        OperationStatus::Durable(Effect::Applied | Effect::Delivered { .. }) => {
            (Execution::Succeeded, Effects::KnownResult)
        }
        OperationStatus::Durable(Effect::NotStarted) => (Execution::Failed, Effects::NotStarted),
        _ => (Execution::Failed, Effects::Unknown),
    };
    let epoch = if matches!(intent.action, Action::Create { .. }) {
        1
    } else {
        intent.epoch
    };
    Outcome::from_bytes(execution, effects, json!({"operation":intent.operation,"terminal":intent.terminal,"epoch":epoch,"receipt":status_value(status)}).to_string().into_bytes())
}

fn rejected(code: &'static str) -> Outcome {
    let effect = if matches!(
        code,
        "terminal_intent_not_durable" | "terminal_owner_unavailable"
    ) {
        Effects::Unknown
    } else {
        Effects::NotStarted
    };
    Outcome::from_bytes(
        Execution::Failed,
        effect,
        json!({"error":code}).to_string().into_bytes(),
    )
}
