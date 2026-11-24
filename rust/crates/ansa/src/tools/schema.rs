//! Deliberately closed JSON-schema subset matching compiled local tools.
use crate::protocol::json::Json;
use std::collections::BTreeSet;
pub fn validate_schema(schema: &Json, depth: usize) -> Result<(), &'static str> {
    if depth >= 16 || !schema.is_object() {
        return Err("tool_schema_shape");
    }
    let ty = schema.field("type")?.text()?;
    let allowed: BTreeSet<&str> = match ty {
        "object" => [
            "type",
            "description",
            "properties",
            "required",
            "additionalProperties",
        ]
        .into(),
        "array" => ["type", "description", "items"].into(),
        "string" | "integer" | "number" | "boolean" | "null" => ["type", "description"].into(),
        _ => return Err("tool_schema_type"),
    };
    for key in schema.keys()? {
        if !allowed.contains(key) {
            return Err("unsupported_schema_keyword");
        }
    }
    if ty == "object" {
        let properties = schema.field("properties")?;
        if !properties.is_object() || schema.field("additionalProperties")?.boolean()? {
            return Err("tool_schema_strict");
        };
        let names = properties.keys()?;
        let mut required = BTreeSet::new();
        for r in schema.field("required")?.elements()? {
            if !required.insert(r.text()?) {
                return Err("tool_schema_required_duplicate");
            }
        }
        if names.len() > 64 || required.len() != names.len() {
            return Err("tool_schema_required");
        };
        for name in names {
            if name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !required.contains(name)
            {
                return Err("tool_schema_property");
            };
            validate_schema(properties.field(name)?, depth + 1)?
        }
    } else if ty == "array" {
        validate_schema(schema.field("items")?, depth + 1)?
    }
    if schema.field("description").is_ok() {
        schema.field("description")?.text()?;
    }
    Ok(())
}
pub fn matches(schema: &Json, value: &Json) -> bool {
    check_arguments(schema, value).is_ok()
}

pub(super) fn check_arguments(schema: &Json, value: &Json) -> Result<(), String> {
    let ty = schema.text_field("type")?;
    let valid = match ty {
        "string" => value.is_string(),
        "integer" => value.is_integer(),
        "number" => value.is_number(),
        "boolean" => value.boolean().is_ok(),
        "null" => value.dump() == "null",
        "array" if let Ok(items) = value.elements() => {
            let item_schema = schema.field("items")?;
            for (index, item) in items.iter().enumerate() {
                check_arguments(item_schema, item)
                    .map_err(|error| format!("[{index}]: {error}"))?;
            }
            true
        }
        "object" if value.is_object() => {
            let properties = schema.field("properties")?;
            let names = properties.keys()?;
            for name in &names {
                let field = value
                    .field(name)
                    .map_err(|_| format!("Missing required field {name}"))?;
                check_arguments(properties.field(name)?, field)
                    .map_err(|error| format!("{name}: {error}"))?;
            }
            if value.keys()?.len() != names.len() {
                if names.is_empty() {
                    return Err("This tool takes no arguments. Pass an empty object {}".into());
                }
                return Err(format!(
                    "Unexpected field. Allowed fields: {}",
                    names.join(", ")
                ));
            }
            true
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("Expected {ty}"))
    }
}
