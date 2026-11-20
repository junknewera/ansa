//! JSON Schema keywords accepted for an MCP tool's local batch preflight.
use crate::protocol::json::Json;
use std::collections::{BTreeSet, HashSet};

type Result<T> = std::result::Result<T, &'static str>;

fn kind(value: &str, input: &Json) -> bool {
    match value {
        "object" => input.is_object(),
        "array" => input.is_array(),
        "string" => input.is_string(),
        "integer" => {
            input.is_number() && decimal(input).is_some_and(|(_, _, exponent)| exponent >= 0)
        }
        "number" => input.is_number(),
        "boolean" => input.boolean().is_ok(),
        "null" => input.is_null(),
        _ => false,
    }
}

fn valid_kind(value: &str) -> bool {
    matches!(
        value,
        "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
    )
}

pub(super) fn check(schema: &Json, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err("mcp_tool_schema");
    }
    if schema.boolean().is_ok() {
        return Ok(());
    }
    for key in schema.keys()? {
        let value = schema.field(key)?;
        match key {
            "type" if value.text().is_ok_and(valid_kind) => {}
            "type"
                if value.elements().is_ok_and(|types| {
                    !types.is_empty()
                        && types.iter().all(|value| value.text().is_ok_and(valid_kind))
                }) => {}
            "properties" => {
                for key in value.keys()? {
                    check(value.field(key)?, depth + 1)?;
                }
            }
            "required" => {
                let mut seen = BTreeSet::new();
                for name in value.elements()? {
                    if !seen.insert(name.text()?) {
                        return Err("mcp_tool_schema");
                    }
                }
            }
            "items" | "additionalProperties" | "not" => check(value, depth + 1)?,
            "allOf" | "anyOf" | "oneOf" => {
                let children = value.elements()?;
                if children.is_empty() {
                    return Err("mcp_tool_schema");
                }
                for child in children {
                    check(child, depth + 1)?;
                }
            }
            "enum"
                if value.elements().is_ok_and(|values| !values.is_empty())
                    && supported_numbers(value) => {}
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties"
                if value.index().is_ok() => {}
            "uniqueItems" if value.boolean().is_ok() => {}
            "const" if supported_numbers(value) => {}
            "$comment" | "title" | "description" | "default" | "examples" | "deprecated"
            | "readOnly" | "writeOnly" => {}
            "$schema" if value.text() == Ok("https://json-schema.org/draft/2020-12/schema") => {}
            _ => return Err("mcp_tool_schema_keyword"),
        }
    }
    Ok(())
}

pub(super) fn matches(schema: &Json, input: &Json) -> bool {
    if let Ok(value) = schema.boolean() {
        return value;
    }
    schema
        .keys()
        .expect("checked schema")
        .into_iter()
        .all(|key| {
            let value = schema.field(key).expect("schema key");
            match key {
                "type" => value.text().map_or_else(
                    |_| {
                        value
                            .elements()
                            .expect("checked types")
                            .iter()
                            .any(|value| kind(value.text().expect("checked type"), input))
                    },
                    |value| kind(value, input),
                ),
                "required" if input.is_object() => value
                    .elements()
                    .expect("checked required")
                    .iter()
                    .all(|name| input.has(name.text().expect("checked name"))),
                "properties" if input.is_object() => value
                    .keys()
                    .expect("checked properties")
                    .into_iter()
                    .all(|name| {
                        input.field(name).map_or(true, |input| {
                            matches(value.field(name).expect("checked property"), input)
                        })
                    }),
                "additionalProperties" if input.is_object() => input
                    .keys()
                    .expect("object")
                    .into_iter()
                    .filter(|name| {
                        !schema
                            .field("properties")
                            .is_ok_and(|props| props.has(name))
                    })
                    .all(|name| matches(value, input.field(name).expect("object key"))),
                "items" if input.is_array() => input
                    .elements()
                    .expect("array")
                    .iter()
                    .all(|input| matches(value, input)),
                "allOf" => value
                    .elements()
                    .expect("checked allOf")
                    .iter()
                    .all(|schema| matches(schema, input)),
                "anyOf" => value
                    .elements()
                    .expect("checked anyOf")
                    .iter()
                    .any(|schema| matches(schema, input)),
                "oneOf" => {
                    value
                        .elements()
                        .expect("checked oneOf")
                        .iter()
                        .filter(|schema| matches(schema, input))
                        .take(2)
                        .count()
                        == 1
                }
                "not" => !matches(value, input),
                "const" => equivalent(value, input),
                "enum" => value
                    .elements()
                    .expect("checked enum")
                    .iter()
                    .any(|item| equivalent(item, input)),
                "minLength" | "maxLength" => input
                    .text()
                    .map_or(true, |text| count(key, value, text.chars().count())),
                "minItems" | "maxItems" => input
                    .elements()
                    .map_or(true, |items| count(key, value, items.len())),
                "minProperties" | "maxProperties" => input
                    .keys()
                    .map_or(true, |keys| count(key, value, keys.len())),
                "uniqueItems" if value.boolean() == Ok(true) => {
                    input.elements().map_or(true, |items| {
                        let mut seen = HashSet::with_capacity(items.len());
                        items.iter().all(|item| {
                            normalized(item, true).is_ok_and(|item| seen.insert(item.dump()))
                        })
                    })
                }
                _ => true,
            }
        })
}

fn count(key: &str, limit: &Json, actual: usize) -> bool {
    let limit = limit.index().expect("checked count");
    if key.starts_with("min") {
        actual >= limit
    } else {
        actual <= limit
    }
}

pub(super) fn canonical(value: &Json) -> Result<Json> {
    normalized(value, false)
}

fn normalized(value: &Json, numbers: bool) -> Result<Json> {
    if let Ok(mut keys) = value.keys() {
        keys.sort_unstable();
        Json::object(
            keys.into_iter()
                .map(|key| Ok((key, normalized(value.field(key)?, numbers)?)))
                .collect::<Result<_>>()?,
        )
    } else if let Ok(items) = value.elements() {
        Json::array(
            items
                .iter()
                .map(|item| normalized(item, numbers))
                .collect::<Result<_>>()?,
        )
    } else if numbers && value.is_number() {
        let (sign, digits, exponent) = decimal(value).ok_or("mcp_schema_number")?;
        Json::parse(&format!("{sign}{digits}e{exponent}"))
    } else {
        Ok(value.clone())
    }
}

fn equivalent(left: &Json, right: &Json) -> bool {
    match (normalized(left, true), normalized(right, true)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub(super) fn supported_numbers(value: &Json) -> bool {
    if value.is_number() {
        decimal(value).is_some()
    } else if let Ok(items) = value.elements() {
        items.iter().all(supported_numbers)
    } else if let Ok(keys) = value.keys() {
        keys.into_iter()
            .all(|name| value.field(name).is_ok_and(supported_numbers))
    } else {
        true
    }
}

fn decimal(value: &Json) -> Option<(&'static str, String, i64)> {
    let token = value.dump();
    let (sign, token) = token
        .strip_prefix('-')
        .map_or(("", token.as_str()), |token| ("-", token));
    let (mantissa, exponent) = token.split_once(['e', 'E']).unwrap_or((token, "0"));
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{integer}{fraction}");
    let significant = digits.trim_start_matches('0').trim_end_matches('0');
    if significant.is_empty() {
        return Some(("", "0".into(), 0));
    }
    let trailing = digits.len() - digits.trim_end_matches('0').len();
    let exponent = exponent
        .parse::<i64>()
        .ok()?
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing).ok()?)?;
    Some((sign, significant.into(), exponent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_required_optional_and_nested_arguments_and_canonicalizes_key_order() {
        let schema = Json::parse(r#"{"properties":{"value":{"type":"integer"},"tags":{"type":"array","items":{"type":"string"}}},"required":["value"],"additionalProperties":false,"type":"object"}"#).unwrap();
        check(&schema, 0).unwrap();
        for (input, valid) in [
            (r#"{"value":5}"#, true),
            (r#"{"value":5.0}"#, true),
            (r#"{"value":5,"tags":["test"]}"#, true),
            (r#"{"tags":["test"]}"#, false),
            (r#"{"value":"5"}"#, false),
            (r#"{"value":5,"tags":[1]}"#, false),
            (r#"{"value":5,"other":true}"#, false),
        ] {
            assert_eq!(matches(&schema, &Json::parse(input).unwrap()), valid);
        }
        assert!(equivalent(
            &Json::parse("[1,-0.0,20]").unwrap(),
            &Json::parse("[1.0,0,2e1]").unwrap()
        ));
        let unique = Json::parse(r#"{"uniqueItems":true}"#).unwrap();
        assert!(!matches(&unique, &Json::parse("[1,1.0]").unwrap()));
        let left = Json::parse(r#"{"type":"object","properties":{"b":true,"a":false}}"#).unwrap();
        let right = Json::parse(r#"{"properties":{"a":false,"b":true},"type":"object"}"#).unwrap();
        assert_eq!(
            canonical(&left).unwrap().dump(),
            canonical(&right).unwrap().dump()
        );
        assert!(
            check(
                &Json::parse(r#"{"$ref":"https://example.com/schema"}"#).unwrap(),
                0
            )
            .is_err()
        );
    }
}
