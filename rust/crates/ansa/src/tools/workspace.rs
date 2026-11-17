use super::{
    files::{Host, b, error, result, schema},
    registry::{Definition, Outcome, Policy, Registry},
};
use crate::{
    auth::Cancel,
    config::{Result, path_text, s},
    protocol::json::Json,
    provider::request::Descriptor,
};
use std::{io::Read, os::unix::fs::MetadataExt, sync::Arc};

fn valid_context(args: &Json) -> bool {
    (|| -> Result<bool> {
        Ok(
            crate::provider::contracts::valid_id(args.text_field("client")?)
                && crate::provider::contracts::valid_id(args.text_field("editor_incarnation")?)
                && args.usize_field("buffer")? > 0
                && args.usize_field("sequence")? > 0,
        )
    })()
    .unwrap_or(false)
}
fn readable(text: &str) -> bool {
    text.len() <= 16 * 1024
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}
fn presentation(
    host: &Host,
    invocation: &super::invocation::Invocation,
    stop: &Cancel,
) -> Result<Outcome> {
    let args = &invocation.arguments;
    let hub = host.editor.as_ref().ok_or("editor_unavailable")?;
    let context = hub.get(args)?;
    if stop.cancelled() {
        return Ok(error("cancelled"));
    }
    let (kind, path, filetype, line, before, after) = if invocation.name == "editor_open_file" {
        let path = host.path(args.text_field("path")?)?;
        if !path.starts_with(&host.settings.cwd) {
            return Err("editor_path");
        }
        if let Some(value) = host.instructions(path.parent().ok_or("editor_path")?, args)? {
            return Ok(value);
        }
        let mut file = super::files::regular(&path).map_err(|_| "file_not_regular")?;
        let metadata = file.metadata().map_err(|_| "file_access_failed")?;
        if metadata.len() > 16 * 1024 {
            return Err("editor_document_limit");
        }
        let mut bytes = vec![];
        file.by_ref()
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "file_access_failed")?;
        let now = file.metadata().map_err(|_| "file_access_failed")?;
        let current = std::fs::symlink_metadata(&path).map_err(|_| "file_changed")?;
        let identity = |m: &std::fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
                m.nlink(),
            )
        };
        if identity(&metadata) != identity(&now)
            || identity(&metadata) != identity(&current)
            || host.path(args.text_field("path")?)? != path
        {
            return Err("file_changed");
        }
        let text = String::from_utf8(bytes).map_err(|_| "file_utf8")?;
        if !readable(&text) {
            return Err("editor_document_limit");
        }
        let line = args.usize_field("line")?;
        if line == 0 || line > text.lines().count().max(1) {
            return Err("editor_line");
        }
        let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("text");
        let filetype = match extension {
            "rs" => "rust",
            "md" => "markdown",
            "py" => "python",
            "js" => "javascript",
            "ts" => "typescript",
            "yml" => "yaml",
            other => other,
        }
        .to_owned();
        ("source", Some(path), filetype, line, text, String::new())
    } else {
        let directory = if let Some(path) = &context.path {
            let resolved = if path.exists() {
                host.path_os(path)?
            } else {
                host.path_os(path.parent().ok_or("editor_path")?)?
                    .join(path.file_name().ok_or("editor_path")?)
            };
            if !resolved.starts_with(&host.settings.cwd) || host.protected_path(&resolved) {
                return Err("protected_editor_path");
            }
            resolved.parent().ok_or("editor_path")?.to_owned()
        } else {
            host.settings.cwd.clone()
        };
        if let Some(value) = host.instructions(&directory, args)? {
            return Ok(value);
        }
        let replacement = args.text_field("replacement")?;
        if !readable(&context.text) || !readable(replacement) {
            return Err("editor_document_limit");
        }
        (
            "diff",
            context.path.clone(),
            context.filetype.clone(),
            context.first_line,
            context.text.clone(),
            replacement.into(),
        )
    };
    hub.get(args)?;
    if stop.cancelled() {
        return Ok(error("cancelled"));
    }
    let filetype = if filetype.len() <= 32
        && filetype
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        filetype
    } else {
        "text".into()
    };
    let session = host
        .session
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("editor_session")?;
    let document = Json::object(vec![
        (
            "document_id",
            s(&format!(
                "d_{}",
                crate::protocol::legacy_json_v1::sha256(invocation.operation.as_bytes())
            ))?,
        ),
        ("session_id", s(session)?),
        ("producer_operation_id", s(&invocation.operation)?),
        ("kind", s(kind)?),
        (
            "path",
            path.map_or_else(|| Json::parse("null"), |p| s(path_text(&p)?))?,
        ),
        ("filetype", s(&filetype)?),
        ("first_line", crate::config::n(line)),
        ("before", s(&before)?),
        ("after", s(&after)?),
        ("client", s(&context.client)?),
        ("editor_incarnation", s(&context.incarnation)?),
        ("buffer", crate::config::n(context.buffer)),
        ("changedtick", crate::config::n(context.changedtick)),
        ("sequence", crate::config::n(context.sequence)),
    ])?;
    Ok(result(Json::object(vec![
        ("status", s("presentation_requested")?),
        ("document", document),
    ])?))
}

fn invoke(work: &Host, name: &str, args: &Json, stop: &Cancel) -> Outcome {
    (|| -> Result<_> {
        if stop.cancelled() {
            return Ok(error("cancelled"));
        }
        if name == "workspace_describe" {
            if let Some(instructions) = work.instructions(&work.settings.cwd, args)? {
                return Ok(instructions);
            }
            return Ok(result(Json::object(vec![
                ("cwd", s(path_text(&work.settings.cwd)?)?),
                ("editor_available", b(work.editor.is_some())),
                (
                    "editors",
                    work.editor
                        .as_ref()
                        .map_or_else(|| Json::array(vec![]), |hub| hub.list())?,
                ),
            ])?));
        }
        let hub = work.editor.as_ref().ok_or("editor_unavailable")?;
        let context = hub.get(args)?;
        let directory = if let Some(path) = &context.path {
            let resolved = if path.exists() {
                work.path_os(path)?
            } else {
                work.path_os(path.parent().ok_or("editor_path")?)?
                    .join(path.file_name().ok_or("editor_path")?)
            };
            if !resolved.starts_with(&work.settings.cwd) || work.protected_path(&resolved) {
                return Err("protected_editor_path");
            }
            resolved.parent().ok_or("editor_path")?.to_owned()
        } else {
            work.settings.cwd.clone()
        };
        if let Some(instructions) = work.instructions(&directory, args)? {
            return Ok(instructions);
        }
        // Instructions may require slow filesystem I/O. Revalidate the exact
        // shared version before publishing its text after that wait.
        if stop.cancelled() {
            return Ok(error("cancelled"));
        }
        Ok(result(hub.get(args)?.json()?))
    })()
    .unwrap_or_else(error)
}

pub(super) fn register(registry: &mut Registry, host: Arc<Host>) -> Result<()> {
    for (name, description, properties) in [
        (
            "workspace_describe",
            "Describe the current workspace and explicitly shared editor contexts. No native model context is exposed.",
            vec![],
        ),
        (
            "editor_get_context",
            "Read an explicitly shared unsaved editor selection at the exact client, editor incarnation, buffer, changedtick and sequence. A changed or disconnected editor returns stale/unavailable.",
            vec![
                ("client", "string"),
                ("editor_incarnation", "string"),
                ("buffer", "integer"),
                ("changedtick", "integer"),
                ("sequence", "integer"),
            ],
        ),
    ] {
        let work = host.clone();
        let legacy = host.clone();
        registry.register_with_policy(
            Definition {
                descriptor: Descriptor {
                    name: name.into(),
                    description: description.into(),
                    revision: "ansa_workspace_v1".into(),
                    schema: schema(&properties)?,
                },
                validator: Arc::new(move |args| {
                    name == "workspace_describe"
                        || (|| -> Result<bool> {
                            Ok(
                                crate::provider::contracts::valid_id(args.text_field("client")?)
                                    && crate::provider::contracts::valid_id(
                                        args.text_field("editor_incarnation")?,
                                    )
                                    && args.usize_field("buffer")? > 0
                                    && args.usize_field("sequence")? > 0,
                            )
                        })()
                        .unwrap_or(false)
                }),
                executor: Arc::new(move |args| invoke(&legacy, name, args, &Cancel::default())),
            },
            Policy {
                read_only: true,
                invoke: Some(Arc::new(move |invocation, stop| {
                    invoke(&work, name, &invocation.arguments, stop)
                })),
                ..Policy::default()
            },
        )?;
    }
    for (name, description, extra) in [
        (
            "editor_open_file",
            "Prepare a read-only source view (up to 16 KiB) for the connected editor at an exact shared context version. The user opens it through AnsaDocuments; original files and buffers are preserved.",
            vec![("path", "string"), ("line", "integer")],
        ),
        (
            "editor_propose_diff",
            "Prepare a read-only before/after diff against the exact explicitly shared editor selection. replacement is at most 16 KiB. The user opens the proposal through AnsaDocuments.",
            vec![("replacement", "string")],
        ),
    ] {
        let mut properties = vec![
            ("client", "string"),
            ("editor_incarnation", "string"),
            ("buffer", "integer"),
            ("changedtick", "integer"),
            ("sequence", "integer"),
        ];
        properties.extend(extra);
        let work = host.clone();
        registry.register_with_policy(
            Definition {
                descriptor: Descriptor {
                    name: name.into(),
                    description: description.into(),
                    revision: "ansa_editor_view_v1".into(),
                    schema: schema(&properties)?,
                },
                validator: Arc::new(move |args| {
                    valid_context(args)
                        && if name == "editor_open_file" {
                            super::files::path_argument(args, "path").unwrap_or(false)
                                && args
                                    .usize_field("line")
                                    .is_ok_and(|n| n > 0 && n <= 1000000)
                        } else {
                            args.text_field("replacement").is_ok_and(readable)
                        }
                }),
                executor: Arc::new(|_| error("workspace_invocation_required")),
            },
            Policy {
                read_only: true,
                invoke: Some(Arc::new(move |invocation, stop| {
                    presentation(&work, invocation, stop).unwrap_or_else(error)
                })),
                ..Policy::default()
            },
        )?;
    }
    Ok(())
}
