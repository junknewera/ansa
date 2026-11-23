use super::registry::Outcome;
use crate::{
    auth::Cancel,
    orchestration::profile::RoleProfile,
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::{Advertisement, Descriptor, Request},
};
use std::sync::Arc;

pub(crate) const NAME: &str = "delegate_task";
pub(crate) type Delegate =
    Arc<dyn Fn(&Request, Option<&RoleProfile>, &Json, &Cancel) -> Outcome + Send + Sync>;

fn descriptor() -> Descriptor {
    Descriptor {
        name: NAME.into(),
        revision: "ansa_delegate_v1".into(),
        description: "Give one independent task to a helper and receive its final status, summary and findings. Include the context the helper needs. Empty paths allow workspace research. For file changes specify the relative files or directories the helper may read and edit. The lead can run tests itself.".into(),
        schema: Json::parse(r#"{"type":"object","properties":{"task":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}},"required":["task","paths"],"additionalProperties":false}"#).expect("delegate tool schema"),
    }
}

pub(crate) fn assignment<'a>(
    args: &'a Json,
    parent: &Advertisement,
) -> Result<(&'a str, Vec<String>), &'static str> {
    let task = args.text_field("task")?;
    if task.trim().is_empty() || !Json::text_fits_frame(task) {
        return Err("delegate_task_text");
    }
    let paths = args
        .field("paths")?
        .elements()?
        .iter()
        .map(|p| p.text().map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()?;
    let has = |name: &str| parent.descriptors.iter().any(|tool| tool.name == name);
    if !["list_directory", "read_file", "search_files"]
        .into_iter()
        .all(has)
        || !paths.is_empty() && !has("edit_file")
    {
        return Err("delegate_parent_access");
    }
    super::files::TaskScope::validate_paths(&paths)?;
    Ok((task, paths))
}

/// The journal's base catalogue stays immutable. Only an orchestration turn
/// advertises this extra compiled tool, including in pre-existing sessions.
pub(crate) fn advertise(base: &Advertisement) -> Result<Advertisement, &'static str> {
    let tool = descriptor();
    if base.descriptors.len() >= 64 || base.descriptors.iter().any(|d| d.name == NAME) {
        return Err("delegate_catalog_conflict");
    }
    let mut result = base.clone();
    result.revision = sha256(format!("{}/{}", base.revision, tool.revision).as_bytes());
    result.descriptors.push(tool);
    Ok(result)
}
