//! Read-only descriptors shared by runtime sessions and journal inspection.
use super::{
    files::error,
    registry::{Definition, Outcome, Policy, Registry},
};
use crate::{protocol::json::Json, provider::request::Descriptor};
use std::sync::Arc;

const LIST_SCHEMA: &str =
    r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#;
const READ_SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#;
const PAGE_SCHEMA: &str = r#"{"type":"object","properties":{"name":{"type":"string"},"offset":{"type":"integer"}},"required":["name","offset"],"additionalProperties":false}"#;

pub(crate) trait Catalog: Send + Sync {
    fn list(&self) -> serde_json::Value;
    fn read(&self, name: &str) -> Result<serde_json::Value, &'static str>;
    fn read_page(&self, name: &str, offset: usize) -> Result<serde_json::Value, &'static str>;
}

pub(crate) fn register(
    registry: &mut Registry,
    profile: Option<Arc<dyn Catalog>>,
) -> Result<(), &'static str> {
    for (name, description, schema) in [
        (
            "list_skills",
            "List names, triggers and hashes for procedures pinned to this project conversation.",
            LIST_SCHEMA,
        ),
        (
            "read_skill",
            "Read the exact pinned procedure by name, including after context compaction.",
            READ_SCHEMA,
        ),
        (
            "read_skill_page",
            "Continue reading a long pinned procedure using its next_offset, until next_offset is null.",
            PAGE_SCHEMA,
        ),
    ] {
        let profile = profile.clone();
        let definition = Definition {
            descriptor: Descriptor {
                name: name.into(),
                description: description.into(),
                revision: "harness_skill_v1".into(),
                schema: Json::parse(schema)?,
            },
            validator: Arc::new(move |args| {
                let Ok(keys) = args.keys() else {
                    return false;
                };
                let valid_name = || {
                    args.text_field("name")
                        .is_ok_and(|s| !s.is_empty() && s.len() <= 64)
                };
                match name {
                    "read_skill" => keys.len() == 1 && valid_name(),
                    "read_skill_page" => {
                        keys.len() == 2 && valid_name() && args.usize_field("offset").is_ok()
                    }
                    _ => keys.is_empty(),
                }
            }),
            executor: Arc::new(move |args| {
                let Some(profile) = &profile else {
                    return error("harness_profile_missing");
                };
                let result = match name {
                    "read_skill" => args.text_field("name").and_then(|key| profile.read(key)),
                    "read_skill_page" => args
                        .text_field("name")
                        .and_then(|key| profile.read_page(key, args.usize_field("offset")?)),
                    _ => Ok(profile.list()),
                };
                match result {
                    Ok(value) => Outcome::succeeded(value.to_string()),
                    Err(code) => error(code),
                }
            }),
        };
        registry.register_with_policy(
            definition,
            Policy {
                read_only: true,
                ..Policy::default()
            },
        )?;
    }
    Ok(())
}
