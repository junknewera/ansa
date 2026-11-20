use crate::{
    auth::Cancel,
    config::{AgentSettings, Result, agents::saved_instructions, n, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        replay::{Effects, Execution},
        request::{Descriptor, Epoch},
    },
    tools::{
        command::{ProcessOptions, ProcessRunner},
        invocation::Invocation,
        registry::{Definition, Outcome, Policy, Registry, Snapshot},
    },
};
use std::{
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
};
pub(crate) fn result(value: Json) -> Outcome {
    Outcome::succeeded(value.dump())
}
pub(crate) fn error(code: &str) -> Outcome {
    result(Json::object(vec![("error", s(code).expect("diagnostic"))]).expect("diagnostic object"))
}
pub(crate) fn b(value: bool) -> Json {
    Json::parse(if value { "true" } else { "false" }).expect("bool")
}
pub(crate) struct Host {
    pub settings: AgentSettings,
    pub epoch: Epoch,
    pub processes: ProcessRunner,
    pub auth: PathBuf,
    pub session: PathBuf,
    pub editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    pub(super) shared_writer: Option<WorkspaceWriter>,
    pub(super) scope: Option<TaskScope>,
}

pub(crate) type WorkspaceWriter =
    Arc<dyn Fn(&Path) -> Result<Option<ansa_linux::local::WorkspaceWrite>> + Send + Sync>;

pub(crate) struct LocalExtensions {
    pub editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    pub shared_writer: Option<WorkspaceWriter>,
}

pub struct TaskScope {
    root: PathBuf,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
}

impl TaskScope {
    pub fn new(root: &Path, writes: &[String]) -> Result<Self> {
        let directory = ansa_linux::fs::open_nofollow(root, ansa_linux::local::read_flags())
            .map_err(|_| "task_workspace")?;
        if !directory.metadata().map_err(|_| "task_workspace")?.is_dir() || writes.len() > 32 {
            return Err("task_workspace");
        }
        let root = ansa_linux::fs::normalize(root);
        let mut paths = Vec::with_capacity(writes.len());
        for path in writes {
            let path = Path::new(path);
            if path.as_os_str().is_empty() || path.components().any(|component| {
                !matches!(component, std::path::Component::Normal(value) if value != ".git")
            }) || ansa_linux::fs::protected_path(path) {
                return Err("task_write_scope");
            }
            paths.push(root.join(path));
        }
        Ok(Self {
            root,
            reads: Vec::new(),
            writes: paths,
        })
    }
    pub(crate) fn restrict_reads(mut self, reads: &[String]) -> Result<Self> {
        self.reads = Self::new(&self.root, reads)?.writes;
        Ok(self)
    }
}

impl Host {
    pub(super) fn workspace_writer(
        &self,
        target: &Path,
    ) -> Result<ansa_linux::local::WorkspaceWrite> {
        if let Some(writer) = &self.shared_writer
            && let Some(workspace) = writer(target)?
        {
            return Ok(workspace);
        }
        ansa_linux::local::WorkspaceWrite::acquire(
            &self.settings.directory.join("terminal-workspaces"),
            &self.settings.cwd,
            target,
        )
    }

    pub(crate) fn protected_path(&self, path: &Path) -> bool {
        ansa_linux::fs::protected_path(path)
            || path.starts_with(&self.auth)
            || path.starts_with(&self.session)
            || path.starts_with(self.settings.directory.join("auth"))
            || path.starts_with(self.settings.directory.join("openrouter"))
            || path.starts_with(self.settings.directory.join("sessions"))
            || path.starts_with(self.settings.directory.join("terminal-workspaces"))
            || self.scope.is_some() && path.components().any(|part| part.as_os_str() == ".git")
    }
    pub(super) fn check_task_path(&self, path: &Path, writing: bool) -> Result<()> {
        if let Some(scope) = &self.scope
            && (!path.starts_with(&scope.root)
                || writing && !scope.writes.iter().any(|allowed| path.starts_with(allowed))
                || !writing
                    && !scope.reads.is_empty()
                    && !scope.reads.iter().any(|allowed| path.starts_with(allowed)))
        {
            return Err("task_path_scope");
        }
        Ok(())
    }
    pub fn path(&self, text: &str) -> Result<PathBuf> {
        self.path_os(Path::new(text))
    }
    pub(super) fn task_directory_path(&self, text: &str) -> Result<PathBuf> {
        self.resolve_path(Path::new(text), true)
    }
    pub fn path_os(&self, path: &Path) -> Result<PathBuf> {
        self.resolve_path(path, false)
    }
    pub(super) fn task_directory_allowed(&self, path: &Path) -> bool {
        self.scope.as_ref().is_some_and(|scope| {
            path.starts_with(&scope.root)
                && scope.reads.iter().any(|allowed| allowed.starts_with(path))
        })
    }
    pub(super) fn resolve_path(&self, path: &Path, traverse: bool) -> Result<PathBuf> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) {
            return Err("file_path");
        }
        let input = if path.is_absolute() {
            path.into()
        } else {
            self.settings.cwd.join(path)
        };
        if self.protected_path(&input) {
            return Err("protected_file");
        }
        let normalized = ansa_linux::fs::normalize(&input);
        if !traverse || !self.task_directory_allowed(&normalized) {
            self.check_task_path(&normalized, false)?;
        }
        let resolved = input.canonicalize().map_err(|_| "file_access_failed")?;
        if self.protected_path(&resolved) {
            return Err("protected_file");
        }
        if !traverse || !self.task_directory_allowed(&resolved) {
            self.check_task_path(&resolved, false)?;
        }
        Ok(resolved)
    }
    pub fn instructions(&self, directory: &Path, args: &Json) -> Result<Option<Outcome>> {
        let complete = Json::array(saved_instructions(directory, &self.settings.home)?)?;
        let token = sha256(complete.dump().as_bytes());
        if args.text_field("instruction_token")? == token {
            return Ok(None);
        }
        let base = self.epoch.exact_state.field("instructions")?.elements()?;
        let added = complete
            .elements()?
            .iter()
            .filter(|file| !base.contains(file))
            .cloned()
            .collect();
        let output = Json::object(vec![
            ("status", s("instructions_required")?),
            ("executed", b(false)),
            ("instruction_token", s(&token)?),
            ("instructions", Json::array(added)?),
        ])?
        .dump();
        Ok(Some(Outcome::succeeded(output)))
    }
    pub fn list(&self, args: &Json, stop: &Cancel) -> Result<Outcome> {
        let path = self.task_directory_path(args.text_field("path")?)?;
        if let Some(value) = self.instructions(&path, args)? {
            return Ok(value);
        }
        let mut entries = vec![];
        let mut bytes = 0;
        let mut truncated = false;
        for entry in ansa_linux::fs::read_directory(&path, self.scope.is_none())
            .map_err(|_| "file_access_failed")?
        {
            if stop.cancelled() {
                return Ok(error("cancelled"));
            }
            if entries.len() == 128 {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|_| "file_access_failed")?;
            let entry_path = path.join(&entry.name);
            if self.protected_path(&entry_path)
                || (self.check_task_path(&entry_path, false).is_err()
                    && !(entry.directory && self.task_directory_allowed(&entry_path)))
            {
                continue;
            }
            let value = Json::object(vec![
                (
                    "name",
                    s(&ansa_linux::fs::utf8_text(entry.name.as_encoded_bytes()))?,
                ),
                ("directory", b(entry.directory)),
                ("symlink", b(entry.symlink)),
            ])?;
            if !append(&mut entries, value, &mut bytes) {
                truncated = true;
                break;
            }
        }
        Ok(result(Json::object(vec![
            ("path", s(path_text(&path)?)?),
            ("entries", Json::array(entries)?),
            ("truncated", b(truncated)),
        ])?))
    }
    pub fn read(&self, args: &Json, stop: &Cancel) -> Result<Outcome> {
        let path = self.path(args.text_field("path")?)?;
        if let Some(value) = self.instructions(path.parent().ok_or("file_path")?, args)? {
            return Ok(value);
        }
        let offset = args.usize_field("offset")?;
        let limit = args.usize_field("max_bytes")?;
        if offset > 64 * 1024 * 1024 || !(1..=8192).contains(&limit) {
            return Err("file_read_limit");
        }
        let file = regular(&path).map_err(|_| "file_not_regular")?;
        if stop.cancelled() {
            return Ok(error("cancelled"));
        }
        let mut content = vec![0; limit + 1];
        let count = loop {
            match file.read_at(&mut content, offset as u64) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err("file_read_failed"),
            }
        };
        let truncated = count > limit;
        content.truncate(count.min(limit));
        if content.contains(&0) {
            return Ok(error("binary_file"));
        }
        Ok(result(Json::object(vec![
            ("path", s(path_text(&path)?)?),
            ("offset", n(offset)),
            ("next_offset", n(offset + content.len())),
            ("content", s(&ansa_linux::fs::utf8_text(&content))?),
            ("truncated", b(truncated)),
        ])?))
    }
    fn run(&self, invocation: &Invocation, stop: &Cancel) -> Result<Outcome> {
        let args = &invocation.arguments;
        let cwd = self.path(args.text_field("cwd")?).map_err(|code| {
            if code == "file_access_failed" {
                "process_cwd_unavailable"
            } else {
                code
            }
        })?;
        if let Some(value) = self.instructions(&cwd, args)? {
            return Ok(value);
        }
        let timeout_ms = args.usize_field("timeout_ms")?;
        if timeout_ms == 0 || timeout_ms > self.settings.process_timeout_ms {
            return Err("process_timeout_limit");
        }
        let options = ProcessOptions {
            cwd,
            argv: args
                .field("argv")?
                .elements()?
                .iter()
                .map(|value| value.text().map(String::from))
                .collect::<Result<_>>()?,
            timeout_ms,
            output_bytes: self.settings.process_output_bytes,
        };
        let workspace = ansa_linux::local::WorkspaceWrite::acquire(
            &self.settings.directory.join("terminal-workspaces"),
            &self.settings.cwd,
            &options.cwd,
        )?;
        self.processes
            .run_in(invocation, &options, stop, Some(workspace))
    }
}
pub(crate) fn regular(path: &Path) -> std::io::Result<File> {
    let file = ansa_linux::fs::open_nofollow(path, ansa_linux::local::read_flags())?;
    if !ansa_linux::fs::regular_single(&file)? {
        return Err(std::io::Error::other("file_not_regular"));
    }
    Ok(file)
}
pub(crate) fn append(values: &mut Vec<Json>, value: Json, bytes: &mut usize) -> bool {
    let size = value.dump().len();
    if size > 64 * 1024 - *bytes {
        false
    } else {
        *bytes += size;
        values.push(value);
        true
    }
}
pub(super) fn schema(properties: &[(&str, &str)]) -> Result<Json> {
    let mut fields = vec![];
    let mut required = vec![];
    for (name, ty) in properties {
        fields.push((
            *name,
            if *ty == "array" {
                Json::parse(r#"{"type":"array","items":{"type":"string"}}"#)?
            } else {
                Json::object(vec![("type", s(ty)?)])?
            },
        ));
        required.push(s(name)?);
    }
    fields.push(("instruction_token", Json::parse(r#"{"type":"string"}"#)?));
    required.push(s("instruction_token")?);
    Json::object(vec![
        ("type", s("object")?),
        ("properties", Json::object(fields)?),
        ("required", Json::array(required)?),
        ("additionalProperties", b(false)),
    ])
}
pub(super) fn path_argument(args: &Json, key: &str) -> Result<bool> {
    let text = args.text_field(key)?;
    Ok(!text.is_empty()
        && text.len() <= 4096
        && !text.contains('\0')
        && !ansa_linux::fs::protected_path(Path::new(text))
        && args.text_field("instruction_token")?.len() <= 64)
}
pub fn local_tools(
    settings: AgentSettings,
    epoch: Epoch,
    session: &Path,
    auth: &Path,
    worker: &Path,
) -> Result<Arc<Snapshot>> {
    local_tools_with_editor(settings, epoch, session, auth, worker, None)
}
pub fn local_tools_with_editor(
    settings: AgentSettings,
    epoch: Epoch,
    session: &Path,
    auth: &Path,
    worker: &Path,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
) -> Result<Arc<Snapshot>> {
    local_tools_extended(
        settings,
        epoch,
        session,
        auth,
        worker,
        LocalExtensions {
            editor,
            shared_writer: None,
        },
        |_| Ok(()),
    )
}

pub(crate) fn local_tools_extended(
    settings: AgentSettings,
    epoch: Epoch,
    session: &Path,
    auth: &Path,
    worker: &Path,
    extensions: LocalExtensions,
    extend: impl FnOnce(&mut Registry) -> Result<()>,
) -> Result<Arc<Snapshot>> {
    let mut host = local_host(settings, epoch, session, auth, worker, extensions.editor)?;
    host.shared_writer = extensions.shared_writer;
    register_local_tools(Arc::new(host), extend)
}

pub(crate) fn summary_catalog(settings: &AgentSettings) -> Result<Arc<Snapshot>> {
    let worker = std::env::current_exe().map_err(|_| "process_executable")?;
    let host = Host {
        settings: settings.clone(),
        epoch: Epoch {
            id: "summary_catalog".into(),
            instructions: String::new(),
            exact_state: Json::object(vec![])?,
        },
        processes: ProcessRunner::configured(&settings.directory.join("operations"), &worker)?,
        auth: settings.directory.join("auth"),
        session: settings.directory.clone(),
        editor: None,
        shared_writer: None,
        scope: None,
    };
    Ok(register_local_tools(Arc::new(host), |_| Ok(()))?.read_only())
}

pub fn local_task_tools(
    settings: AgentSettings,
    epoch: Epoch,
    session: &Path,
    auth: &Path,
    worker: &Path,
    scope: TaskScope,
) -> Result<Arc<Snapshot>> {
    if !settings.cwd.starts_with(&scope.root) {
        return Err("task_workspace");
    }
    let mut names = vec!["list_directory", "read_file", "search_files"];
    if !scope.writes.is_empty() {
        names.push("edit_file");
    }
    let mut host = local_host(settings, epoch, session, auth, worker, None)?;
    host.scope = Some(scope);
    Ok(register_local_tools(Arc::new(host), |_| Ok(()))?.named_tools(&names))
}

fn local_host(
    settings: AgentSettings,
    epoch: Epoch,
    session: &Path,
    auth: &Path,
    worker: &Path,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
) -> Result<Host> {
    Ok(Host {
        processes: ProcessRunner::new(&session.join("operations"), worker)?,
        auth: ansa_linux::fs::weakly_canonical(auth).map_err(|_| "file_access_failed")?,
        session: ansa_linux::fs::weakly_canonical(session).map_err(|_| "file_access_failed")?,
        settings,
        epoch,
        editor,
        shared_writer: None,
        scope: None,
    })
}

fn register_local_tools(
    host: Arc<Host>,
    extend: impl FnOnce(&mut Registry) -> Result<()>,
) -> Result<Arc<Snapshot>> {
    let mut registry = Registry::new();
    type Action = fn(&Host, &Json, &Cancel) -> Result<Outcome>;
    for (name, description, properties, action) in [
        (
            "list_directory",
            "List up to 128 directory entries. instruction_token identifies applicable AGENTS.md instructions.",
            vec![("path", "string")],
            Host::list as Action,
        ),
        (
            "read_file",
            "Read a regular UTF-8 file by byte offset, max_bytes 1..8192. Dotenv and credential stores are excluded.",
            vec![
                ("path", "string"),
                ("offset", "integer"),
                ("max_bytes", "integer"),
            ],
            Host::read as Action,
        ),
        (
            "search_files",
            "Bounded literal search: mode=name or content, max_results 1..50. Nested instruction scopes are returned separately.",
            vec![
                ("path", "string"),
                ("query", "string"),
                ("mode", "string"),
                ("max_results", "integer"),
            ],
            Host::search as Action,
        ),
    ] {
        let first = host.clone();
        let second = host.clone();
        let invoke = Arc::new(move |args: &Json, stop: &Cancel| {
            action(&second, args, stop).unwrap_or_else(error)
        });
        let legacy = invoke.clone();
        registry.register_with_policy(
            Definition {
                descriptor: Descriptor {
                    name: name.into(),
                    description: description.into(),
                    revision: "ansa_local_v1".into(),
                    schema: schema(&properties)?,
                },
                validator: Arc::new(move |args| {
                    (|| -> Result<bool> {
                        if !path_argument(args, "path")? {
                            return Ok(false);
                        }
                        Ok(match name {
                            "read_file" => {
                                args.usize_field("offset")? <= 64 * 1024 * 1024
                                    && (1..=8192).contains(&args.usize_field("max_bytes")?)
                            }
                            "search_files" => {
                                !args.text_field("query")?.is_empty()
                                    && args.text_field("query")?.len() <= 256
                                    && matches!(args.text_field("mode")?, "name" | "content")
                                    && (1..=50).contains(&args.usize_field("max_results")?)
                            }
                            _ => true,
                        })
                    })()
                    .unwrap_or(false)
                }),
                executor: Arc::new(move |args| {
                    let _ = &first;
                    legacy(args, &Cancel::default())
                }),
            },
            Policy {
                read_only: true,
                invoke: Some(Arc::new(move |invocation, stop| {
                    invoke(&invocation.arguments, stop)
                })),
                ..Policy::default()
            },
        )?;
    }
    super::edit::register(&mut registry, host.clone())?;
    super::workspace::register(&mut registry, host.clone())?;
    super::analysis::register(&mut registry, host.clone())?;
    let preview = host.clone();
    registry.register_with_policy(
        Definition {
            descriptor: Descriptor {
                name: "show_png".into(),
                description: "Open a local PNG in the Carbon graph panel. Use after creating a PNG when the user asks to see it. A compatible terminal can display PNGs of up to 2 MiB and 8 million pixels.".into(),
                revision: "ansa_show_png_v1".into(),
                schema: schema(&[("path", "string")])?,
            },
            validator: Arc::new(|args| path_argument(args, "path").unwrap_or(false)),
            executor: Arc::new(|_| Outcome::failed_unknown()),
        },
        Policy {
            read_only: true,
            invoke: Some(Arc::new(move |invocation, stop| {
                if stop.cancelled() {
                    return Outcome::not_started();
                }
                let requested = (|| -> Result<Outcome> {
                    let path = preview.path(invocation.arguments.text_field("path")?)?;
                    if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("png")) {
                        return Err("preview_png_extension");
                    }
                    if let Some(required) = preview.instructions(path.parent().ok_or("file_path")?, &invocation.arguments)? {
                        return Ok(required);
                    }
                    let bytes = super::artifacts::read_preview_png(&path)?;
                    Ok(result(Json::object(vec![
                        ("status", s("png_requested")?),
                        ("path", s(path.to_str().ok_or("file_path")?)?),
                        ("sha256", s(&sha256(&bytes))?),
                    ])?))
                })();
                if stop.cancelled() {
                    Outcome::not_started()
                } else {
                    requested.unwrap_or_else(|code| {
                        Outcome::from_bytes(
                            Execution::Failed,
                            Effects::KnownResult,
                            error(code).output().to_vec(),
                        )
                    })
                }
            })),
            ..Policy::default()
        },
    )?;
    let maximum = host.settings.process_timeout_ms;
    let run = host.clone();
    let recovery = host.clone();
    registry.register_with_policy(Definition {descriptor:Descriptor {name:"run_command".into(),description:"Run argv without a shell unless explicitly requested. Noninteractive, no sudo, no background services. timeout_ms must be positive and not exceed the configured limit. Exit codes and truncated stdout/stderr are returned as data.".into(),revision:"ansa_process_v1".into(),schema:schema(&[("argv","array"),("cwd","string"),("timeout_ms","integer")])?},validator:Arc::new(move |args|{
        (||->Result<bool>{if !path_argument(args,"cwd")?||args.usize_field("timeout_ms")?==0||args.usize_field("timeout_ms")?>maximum{return Ok(false);}
            let argv=args.field("argv")?.elements()?;if argv.is_empty()||argv.len()>64||argv[0].text()?.is_empty(){return Ok(false);}let mut bytes=0;for arg in argv {let arg=arg.text()?;if arg.contains('\0')||arg.len()>16384-bytes{return Ok(false);}bytes+=arg.len();}Ok(true)})().unwrap_or(false)
    }),executor:Arc::new(|_|Outcome::failed_unknown())},Policy {requires_journal:true,invoke:Some(Arc::new(move |invocation,stop|match run.run(invocation,stop) {
        Ok(value)=>value,Err(code) if matches!(code,"process_timeout_limit"|"protected_file"|"file_path"|"instruction_size_limit"|"process_cwd_unavailable"|"terminal_workspace_busy"|"terminal_workspace_quarantined"|"terminal_workspace_registry"|"terminal_workspace_identity"|"terminal_workspace_evidence")=>error(code),Err(_)=>Outcome::failed_unknown()
    })),recover:Some(Arc::new(move |invocation|recovery.processes.recover_in(invocation, Some(&recovery.settings.directory.join("terminal-workspaces"))))),..Policy::default()})?;
    registry.register_with_policy(Definition {descriptor:Descriptor{name:"get_test_value".into(),description:"Return a fixed test JSON value without side effects".into(),revision:"1".into(),schema:Json::parse(r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#)?},validator:Arc::new(|_|true),executor:Arc::new(|_|Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into()))},Policy {read_only:true,..Policy::default()})?;
    extend(&mut registry)?;
    Ok(registry.snapshot())
}
