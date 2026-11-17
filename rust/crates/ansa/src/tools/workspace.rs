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
use std::sync::Arc;

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
    Ok(())
}
