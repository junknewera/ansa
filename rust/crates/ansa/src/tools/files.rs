use crate::{
    auth::Cancel,
    config::{AgentSettings, Result, agents::saved_instructions, n, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::{Descriptor, Epoch},
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
    let text = value.dump();
    Outcome::succeeded(if text.len() > 120 * 1024 {
        r#"{"error":"tool_output_limit"}"#.into()
    } else {
        text
    })
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
}
impl Host {
    pub fn path(&self, text: &str) -> Result<PathBuf> {
        self.path_os(Path::new(text))
    }
    pub fn path_os(&self, path: &Path) -> Result<PathBuf> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) {
            return Err("file_path");
        }
        let input = if path.is_absolute() {
            path.into()
        } else {
            self.settings.cwd.join(path)
        };
        if ansa_linux::fs::protected_path(&input) {
            return Err("protected_file");
        }
        let resolved = input.canonicalize().map_err(|_| "file_access_failed")?;
        if ansa_linux::fs::protected_path(&resolved)
            || resolved.starts_with(&self.auth)
            || resolved.starts_with(self.settings.directory.join("auth"))
            || resolved.starts_with(self.settings.directory.join("sessions"))
        {
            return Err("protected_file");
        }
        Ok(resolved)
    }
    pub fn instructions(&self, directory: &Path, args: &Json) -> Result<Option<Outcome>> {
        let complete = saved_instructions(directory, &self.settings.home)?;
        let base = self.epoch.exact_state.field("instructions")?.elements()?;
        let mut added = vec![];
        let mut bytes = 0;
        for file in &complete {
            if !base.contains(file) {
                let len = file.text_field("text")?.len();
                if len > 8192 - bytes {
                    return Err("instruction_size_limit");
                }
                bytes += len;
                added.push(file.clone());
            }
        }
        let token = sha256(Json::array(complete)?.dump().as_bytes());
        if args.text_field("instruction_token")? == token {
            return Ok(None);
        }
        Ok(Some(result(Json::object(vec![
            ("status", s("instructions_required")?),
            ("executed", b(false)),
            ("instruction_token", s(&token)?),
            ("instructions", Json::array(added)?),
        ])?)))
    }
    pub fn list(&self, args: &Json, stop: &Cancel) -> Result<Outcome> {
        let path = self.path(args.text_field("path")?)?;
        if let Some(value) = self.instructions(&path, args)? {
            return Ok(value);
        }
        let mut entries = vec![];
        let mut bytes = 0;
        let mut truncated = false;
        for entry in std::fs::read_dir(&path).map_err(|_| "file_access_failed")? {
            if stop.cancelled() {
                return Ok(error("cancelled"));
            }
            if entries.len() == 128 {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|_| "file_access_failed")?;
            if ansa_linux::fs::protected_path(&entry.path()) {
                continue;
            }
            let value = Json::object(vec![
                (
                    "name",
                    s(&ansa_linux::fs::utf8_text(
                        entry.file_name().as_encoded_bytes(),
                    ))?,
                ),
                ("directory", b(entry.path().is_dir())),
                (
                    "symlink",
                    b(entry
                        .file_type()
                        .map_err(|_| "file_access_failed")?
                        .is_symlink()),
                ),
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
        self.processes.run(
            invocation,
            &ProcessOptions {
                cwd,
                argv: args
                    .field("argv")?
                    .elements()?
                    .iter()
                    .map(|v| v.text().map(String::from))
                    .collect::<Result<_>>()?,
                timeout_ms,
                output_bytes: self.settings.process_output_bytes,
            },
            stop,
        )
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
fn schema(properties: &[(&str, &str)]) -> Result<Json> {
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
fn path_argument(args: &Json, key: &str) -> Result<bool> {
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
    let host = Arc::new(Host {
        processes: ProcessRunner::new(&session.join("operations"), worker)?,
        auth: ansa_linux::fs::weakly_canonical(auth).map_err(|_| "file_access_failed")?,
        settings,
        epoch,
    });
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
    let maximum = host.settings.process_timeout_ms;
    let run = host.clone();
    let recovery = host.clone();
    registry.register_with_policy(Definition {descriptor:Descriptor {name:"run_command".into(),description:"Run argv without a shell unless explicitly requested. Noninteractive, no sudo, no background services. timeout_ms must be positive and not exceed the configured limit. Exit codes and truncated stdout/stderr are returned as data.".into(),revision:"ansa_process_v1".into(),schema:schema(&[("argv","array"),("cwd","string"),("timeout_ms","integer")])?},validator:Arc::new(move |args|{
        (||->Result<bool>{if !path_argument(args,"cwd")?||args.usize_field("timeout_ms")?==0||args.usize_field("timeout_ms")?>maximum{return Ok(false);}
            let argv=args.field("argv")?.elements()?;if argv.is_empty()||argv.len()>64||argv[0].text()?.is_empty(){return Ok(false);}let mut bytes=0;for arg in argv {let arg=arg.text()?;if arg.contains('\0')||arg.len()>16384-bytes{return Ok(false);}bytes+=arg.len();}Ok(true)})().unwrap_or(false)
    }),executor:Arc::new(|_|Outcome::failed_unknown())},Policy {requires_journal:true,invoke:Some(Arc::new(move |invocation,stop|match run.run(invocation,stop) {
        Ok(value)=>value,Err(code) if matches!(code,"process_timeout_limit"|"protected_file"|"file_path"|"instruction_size_limit"|"process_cwd_unavailable")=>error(code),Err(_)=>Outcome::failed_unknown()
    })),recover:Some(Arc::new(move |invocation|recovery.processes.recover(invocation))),..Policy::default()})?;
    registry.register_with_policy(Definition {descriptor:Descriptor{name:"get_test_value".into(),description:"Return a fixed test JSON value without side effects".into(),revision:"1".into(),schema:Json::parse(r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#)?},validator:Arc::new(|_|true),executor:Arc::new(|_|Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into()))},Policy {read_only:true,..Policy::default()})?;
    Ok(registry.snapshot())
}
