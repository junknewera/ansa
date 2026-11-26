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
        FileJournal, Host, HostMode, Lifecycle, OperationStatus,
        native::{MAX_READ, NativeBackend},
        protocol::{Action, Binding, Effect, Id, Intent, Launch, MAX_INPUT, Size, Writer},
        shell,
    },
    tools::{
        invocation::Invocation,
        registry::{Definition, Outcome, Policy, Recovery, Registry},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, &'static str>;
pub(crate) type Owner = Arc<Mutex<Host<FileJournal, NativeBackend>>>;
pub(crate) const EXEC: &str = "terminal_exec";

pub(crate) fn add_agent_instructions(epoch: &mut crate::provider::request::Epoch) {
    epoch.instructions = epoch.instructions.replacen(
        "Tools are non-interactive; ",
        "Use the conversation terminal for shell commands. ",
        1,
    );
    epoch.instructions.push_str(concat!(
        "\n\nFor files and code, prefer search_files, list_directory and read_file. ",
        "Use terminal_exec for a foreground shell command. It opens a Bash shell when needed, ",
        "keeps its working directory and environment, and returns fresh output and exit_code. ",
        "Call it directly without listing or reading the terminal first. ",
        "For interactive programs or connections, Call terminal_list first. If no terminal exists, ",
        "open an interactive shell by name from PATH (e.g. bash) in the working directory. Reuse its current terminal ID and writer epoch ",
        "across commands and user messages. Check the active program and working directory before entering commands. ",
        "A terminal_input receipt confirms byte delivery. Check the output to tell when the command finishes. ",
        "Set lifetime_seconds=0 and idle_seconds=0 to keep the terminal for the conversation. ",
        "Keep the terminal open across replies to preserve the shell environment. Close it when the ",
        "user asks. Close an exited terminal before opening its ",
        "replacement. Handle terminal operations through tools while the user stays in the conversation."
    ));
}

pub(crate) fn open_for(
    settings: &crate::config::AgentSettings,
    session: &str,
    directory: &Path,
    restoring: bool,
) -> Result<Owner> {
    let session = Id::try_from(session.to_owned())?;
    let binding = Binding::attached(session, crate::config::path_text(&settings.cwd)?.to_owned());
    let executable = std::env::current_exe().map_err(|_| "process_executable")?;
    let registry = settings.directory.join("terminal-workspaces");
    let backend =
        |path: &Path| NativeBackend::new(executable.clone(), registry.clone()).with_evidence(path);
    let mut archived = BTreeMap::new();
    let mut next_generation = 1;
    if restoring {
        let last = terminal_store_generation(directory)?;
        for generation in 1..=last {
            let path = terminal_store_path(directory, generation);
            let mut previous = Host::restore_in_mode(
                &path,
                binding.clone(),
                backend(&path),
                HostMode::Conversation,
            )?;
            if previous
                .operations
                .keys()
                .any(|id| archived.contains_key(id))
            {
                return Err("terminal_store_chain");
            }
            // An unsealed generation can also prove that no terminal ever ran.
            // Keep its failed create receipts for recovery under their original IDs.
            let reusable = previous.recovery().is_some_and(|value| {
                value.closure_known
                    || value.tail == super::Tail::End
                        && previous.operations.values().all(|op| {
                            matches!(op.intent.action, Action::Create { .. })
                                && op.status == OperationStatus::Durable(Effect::NotStarted)
                        })
            });
            if !reusable {
                if generation != last {
                    return Err("terminal_store_chain");
                }
                previous.archived_operations = archived;
                return Ok(Arc::new(Mutex::new(previous)));
            }
            archived.append(&mut previous.operations);
        }
        next_generation = last + 1;
    }
    let path = terminal_store_path(directory, next_generation);
    let mut host = Host::create_in_mode(&path, binding, backend(&path), HostMode::Conversation)?;
    host.archived_operations = archived;
    Ok(Arc::new(Mutex::new(host)))
}

const TERMINAL_STORE: &str = "terminal-host-v1";

pub(super) fn terminal_store_path(directory: &Path, generation: usize) -> PathBuf {
    if generation == 1 {
        directory.join(TERMINAL_STORE)
    } else {
        directory.join(format!("{TERMINAL_STORE}-{generation}"))
    }
}

pub(super) fn terminal_store_generation(directory: &Path) -> Result<usize> {
    let directory =
        ansa_linux::fs::private_directory(directory).map_err(|_| "terminal_store_chain")?;
    let (names, more) =
        ansa_linux::local::names(&directory, 8192).map_err(|_| "terminal_store_chain")?;
    if more {
        return Err("terminal_store_chain");
    }
    let mut generations = Vec::new();
    for name in names.iter().filter_map(|name| name.to_str()) {
        if name == TERMINAL_STORE {
            generations.push(1);
        } else if let Some(suffix) = name.strip_prefix("terminal-host-v1-") {
            let number: usize = suffix.parse().map_err(|_| "terminal_store_chain")?;
            if number < 2 || number.to_string() != suffix {
                return Err("terminal_store_chain");
            }
            generations.push(number);
        }
    }
    generations.sort_unstable();
    let last = generations.len();
    if last == 0 || generations.into_iter().ne(1..=last) {
        return Err("terminal_store_chain");
    }
    Ok(last)
}

pub(crate) fn close(owner: &Owner) -> bool {
    owner.lock().is_ok_and(|mut host| {
        if let Some(recovered) = host.recovery() {
            return recovered.closure_known;
        }
        super::daemon::close_owned(&mut host) && host.seal_clean().is_ok()
    })
}

pub(crate) fn workspace_writer(owner: &Owner) -> crate::tools::files::WorkspaceWriter {
    let owner = owner.clone();
    Arc::new(move |target| {
        let host = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
        let root = Path::new(&host.binding().workspace);
        let Ok(relative) = target.strip_prefix(root) else {
            return Ok(None);
        };
        if host.quarantined() {
            return Err("terminal_workspace_quarantined");
        }
        host.backend.write_descendant(root, relative)
    })
}

#[derive(Clone, Copy)]
enum Tool {
    Create,
    Input,
    Exec,
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
            Self::Exec => (
                EXEC,
                "Run one foreground command in the session's Bash shell and return new output and exit_code. Working directory and shell variables persist.",
                &[("command", "string")],
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
            Self::Create | Self::Input | Self::Exec | Self::Resize | Self::Interrupt | Self::Close
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
        Tool::Exec,
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
                argument_hint: Some(Arc::new(move |args| {
                    Arguments::parse(tool, args).err().map(|reason| match reason {
                        "terminal_cwd" => "cwd must be a nonempty path up to 1024 bytes, without control characters.",
                        "terminal_launch" => "Check label, argv and the two timeouts. For an interactive shell use a short label, argv [\"bash\"], lifetime_seconds 0 and idle_seconds 0.",
                        "terminal_input_encoding" | "terminal_input_limit" => "Use utf8 or base64 encoding and send 1..4096 bytes of data per call.",
                        _ => description,
                    })
                })),
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
    Exec {
        bytes: Vec<u8>,
        action: Action,
    },
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
        if matches!(tool, Tool::Exec) {
            let bytes = args.text_field("command")?.as_bytes();
            let action = Action::exec(bytes).ok_or("terminal_exec_command")?;
            return Ok(Self::Exec {
                bytes: bytes.to_vec(),
                action,
            });
        }
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
    if let Arguments::Exec { bytes, action } = args {
        return execute_command(owner, invocation, bytes, action, stop);
    }
    let mut owner = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
    if stop.cancelled() {
        return Ok(Outcome::not_started());
    }
    let output = match args {
        Arguments::Exec { .. } => unreachable!("handled shell command"),
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
    if matches!(tool, Tool::Exec) {
        let owner = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
        let operation = Id::try_from(invocation.operation.clone())?;
        let digest = crate::protocol::legacy_json_v1::sha256(invocation.save()?.dump().as_bytes());
        let prior = owner.operation_record(&operation);
        if let Some(prior) = prior
            && (prior.intent.invocation_digest.as_deref() != Some(&digest)
                || !matches!(prior.intent.action, Action::Exec { .. }))
        {
            return Err("terminal_operation_conflict");
        }
        return Ok(
            if prior
                .is_some_and(|prior| prior.status == OperationStatus::Durable(Effect::NotStarted))
            {
                Outcome::not_started()
            } else {
                unknown_command("terminal_exec_interrupted")
            },
        );
    }
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

fn execute_command(
    owner: &Owner,
    invocation: &Invocation,
    bytes: Vec<u8>,
    action: Action,
    stop: &Cancel,
) -> Result<Outcome> {
    let operation = Id::try_from(invocation.operation.clone())?;
    let (terminal, epoch, created) = {
        let mut host = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
        if stop.cancelled() {
            return Ok(Outcome::not_started());
        }
        if host.operation(&operation).is_some() {
            return Err("terminal_operation_conflict");
        }
        if host.quarantined() {
            return Ok(Outcome::failed_known(
                json!({
                    "error": "terminal_workspace_quarantined",
                    "applied": false,
                    "message": "The session terminal is unavailable. Use read_file, list_directory or search_files to inspect the workspace."
                }).to_string(),
            ));
        }
        if let Some((id, current)) = host
            .terminals()
            .iter()
            .find(|(_, terminal)| terminal.lifecycle != Lifecycle::Closed)
        {
            (id.clone(), current.epoch, false)
        } else {
            let create_operation = Id::try_from(format!(
                "exec_shell:{}",
                super::protocol::sha256(operation.as_str().as_bytes())
            ))?;
            let terminal = host.terminal_id(&create_operation);
            let launch = Launch {
                label: "Shell".into(),
                cwd: host.binding().workspace.clone(),
                argv: shell::ARGV.into_iter().map(str::to_owned).collect(),
                size: Size {
                    rows: 24,
                    columns: 80,
                },
                lifetime_seconds: 0,
                idle_seconds: 0,
            };
            let create = exec_intent(
                &host,
                invocation,
                create_operation,
                terminal.clone(),
                0,
                Action::Create { launch },
            )?;
            let status =
                host.execute_if(Writer::Agent, create.clone(), &[], || !stop.cancelled())?;
            if status != OperationStatus::Durable(Effect::Applied) {
                return Ok(receipt(&create, status));
            }
            (terminal, 1, true)
        }
    };
    let poll = Duration::from_millis(20);
    let startup = Instant::now() + Duration::from_secs(3);
    loop {
        if stop.cancelled() {
            return Ok(Outcome::not_started());
        }
        let state = owner
            .lock()
            .map_err(|_| "terminal_owner_unavailable")?
            .shell_status(&terminal, epoch, None)?;
        match state {
            shell::State::Ready => break,
            shell::State::Busy if created && Instant::now() < startup => stop.wait(poll),
            shell::State::Busy => return Err("terminal_exec_shell_busy"),
            _ => return Err("terminal_exec_shell_unavailable"),
        }
    }
    let status = {
        let mut host = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
        let intent = exec_intent(
            &host,
            invocation,
            operation.clone(),
            terminal.clone(),
            epoch,
            action,
        )?;
        host.execute_if(Writer::Agent, intent, &bytes, || !stop.cancelled())?
    };
    match status {
        OperationStatus::Durable(Effect::Delivered { .. }) => {}
        OperationStatus::Durable(Effect::NotStarted) => return Err("terminal_exec_shell_busy"),
        _ => return Ok(unknown_command("terminal_exec_delivery_unknown")),
    }
    let deadline =
        Instant::now() + Duration::from_secs(u64::from(super::protocol::MAX_LIFETIME_SECONDS));
    let start_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if Instant::now() >= deadline {
            if let Ok(mut host) = owner.lock() {
                host.emergency_stop();
            }
            return Ok(unknown_command("terminal_exec_deadline"));
        }
        let polled = owner
            .lock()
            .map_or(Err("terminal_owner_unavailable"), |mut host| {
                host.shell_status(&terminal, epoch, Some(operation.clone()))
            });
        match polled {
            Ok(shell::State::Finished {
                exit_code,
                output,
                truncated,
            }) => {
                return Ok(Outcome::succeeded(
                    json!({
                        "terminal": terminal, "exit_code": exit_code,
                        "output": output, "truncated": truncated
                    })
                    .to_string(),
                ));
            }
            Ok(shell::State::Running) if !stop.cancelled() => stop.wait(poll),
            Ok(shell::State::Starting) if !stop.cancelled() => {
                if Instant::now() >= start_deadline {
                    if let Ok(mut host) = owner.lock() {
                        host.emergency_stop();
                    }
                    return Ok(unknown_command("terminal_exec_start_unconfirmed"));
                }
                stop.wait(poll);
            }
            _ => return Ok(unknown_command("terminal_exec_interrupted")),
        }
    }
}

fn exec_intent(
    owner: &Host<FileJournal, NativeBackend>,
    invocation: &Invocation,
    operation: Id,
    terminal: Id,
    epoch: u64,
    action: Action,
) -> Result<Intent> {
    Ok(Intent {
        operation,
        terminal,
        epoch,
        writer: Writer::Agent,
        binding: owner.binding().clone(),
        invocation_digest: Some(super::protocol::sha256(
            invocation.save()?.dump().as_bytes(),
        )),
        action,
    })
}

fn unknown_command(code: &'static str) -> Outcome {
    Outcome::from_bytes(
        Execution::Failed,
        Effects::Unknown,
        json!({"error":code}).to_string().into_bytes(),
    )
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
    let create = matches!(intent.action, Action::Create { .. });
    let (execution, effects) = match status {
        OperationStatus::Durable(Effect::Applied | Effect::Delivered { .. }) => {
            (Execution::Succeeded, Effects::KnownResult)
        }
        // The host completed the tool call and proved the requested process never ran.
        OperationStatus::Durable(Effect::NotStarted) if create => {
            (Execution::Failed, Effects::KnownResult)
        }
        OperationStatus::Durable(Effect::NotStarted) => (Execution::Failed, Effects::NotStarted),
        _ => (Execution::Failed, Effects::Unknown),
    };
    let epoch = if create { 1 } else { intent.epoch };
    let mut output = json!({"operation":intent.operation,"terminal":intent.terminal,"epoch":epoch,"receipt":status_value(status)});
    if create && status == OperationStatus::Durable(Effect::NotStarted) {
        output["hint"] = json!(
            "No terminal was opened. Check the executable, working directory and workspace availability. A shell name such as bash is resolved through PATH."
        );
    }
    Outcome::from_bytes(execution, effects, output.to_string().into_bytes())
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

#[cfg(test)]
#[test]
fn quarantined_terminal_exec_returns_a_known_rejection_without_creating_a_shell() {
    let sandbox =
        std::env::temp_dir().join(format!("ansa-terminal-quarantined-{}", std::process::id()));
    std::fs::create_dir(&sandbox).unwrap();
    let sandbox = sandbox.canonicalize().unwrap();
    let workspace = sandbox.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let binding = Binding::attached(
        Id::try_from("quarantined-session".to_owned()).unwrap(),
        workspace.to_str().unwrap().into(),
    );
    let journal = sandbox.join("journal");
    let backend = NativeBackend::new(std::env::current_exe().unwrap(), sandbox.join("workspaces"));
    let owner = Arc::new(Mutex::new(
        Host::create_in_mode(&journal, binding, backend, HostMode::Conversation).unwrap(),
    ));
    owner.lock().unwrap().emergency_stop();
    let invocation = Invocation {
        operation: "quarantined:op:1".into(),
        call: "call".into(),
        name: EXEC.into(),
        revision: "ansa_terminal_attached_v1".into(),
        arguments: Json::parse(r#"{"command":"touch should-not-run"}"#).unwrap(),
    };
    let result = invoke(&owner, Tool::Exec, &invocation, &Cancel::default());
    let empty = owner.lock().unwrap().terminals().is_empty();
    drop(owner);
    let marker = workspace.join("should-not-run").exists();
    std::fs::remove_dir_all(&sandbox).unwrap();
    let result = result.unwrap();
    assert_eq!(result.execution, Execution::Failed);
    assert_eq!(result.effects, Effects::KnownResult);
    let output = Json::parse(std::str::from_utf8(&result.output).unwrap()).unwrap();
    assert_eq!(
        output.text_field("error"),
        Ok("terminal_workspace_quarantined")
    );
    assert_eq!(output.field("applied").unwrap().boolean(), Ok(false));
    assert!(empty && !marker);
}
