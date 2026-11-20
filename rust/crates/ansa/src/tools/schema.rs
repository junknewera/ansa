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
    match schema.field("type").and_then(Json::text) {
        Ok("string") => value.is_string(),
        Ok("integer") => value.is_integer(),
        Ok("number") => value.is_number(),
        Ok("boolean") => value.boolean().is_ok(),
        Ok("null") => value.dump() == "null",
        Ok("array") => value.elements().is_ok_and(|vs| {
            vs.iter()
                .all(|v| matches(schema.field("items").expect("validated schema"), v))
        }),
        Ok("object") => schema
            .field("properties")
            .and_then(|props| {
                let keys = props.keys()?;
                if value.keys()?.len() != keys.len() {
                    return Ok(false);
                };
                Ok(keys.into_iter().all(|k| {
                    value
                        .field(k)
                        .is_ok_and(|v| matches(props.field(k).expect("key from props"), v))
                }))
            })
            .unwrap_or(false),
        _ => false,
    }
}
