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

pub(crate) trait Catalog: Send + Sync {
    fn list(&self) -> serde_json::Value;
    fn read(&self, name: &str) -> Result<serde_json::Value, &'static str>;
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
    ] {
        let profile = profile.clone();
        let reads_body = name == "read_skill";
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
                if reads_body {
                    keys.len() == 1
                        && args
                            .text_field("name")
                            .is_ok_and(|s| !s.is_empty() && s.len() <= 64)
                } else {
                    keys.is_empty()
                }
            }),
            executor: Arc::new(move |args| {
                let Some(profile) = &profile else {
                    return error("harness_profile_missing");
                };
                let result = if reads_body {
                    args.text_field("name").and_then(|key| profile.read(key))
                } else {
                    Ok(profile.list())
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
