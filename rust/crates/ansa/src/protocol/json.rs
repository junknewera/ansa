//! Owning JSON with exact numeric tokens and decoded duplicate-key rejection.
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone)]
pub struct Json(Value);
#[derive(Debug, Clone)]
enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

struct Entries<'a>(Vec<(String, &'a RawValue)>);
impl<'de> Deserialize<'de> for Entries<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor;
        impl<'de> Visitor<'de> for MapVisitor {
            type Value = Entries<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                let mut keys = HashSet::new();
                while let Some((key, value)) = map.next_entry::<String, &'de RawValue>()? {
                    if !keys.insert(key.clone()) {
                        return Err(de::Error::custom("json_duplicate_key"));
                    }
                    entries.push((key, value));
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(MapVisitor)
    }
}

impl Json {
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        if input.is_empty() || input.len() > 16 * 1024 * 1024 {
            return Err("json_byte_limit");
        }
        let (mut level, mut quoted, mut escape) = (0usize, false, false);
        for byte in input.bytes() {
            if quoted {
                if escape {
                    escape = false;
                } else if byte == b'\\' {
                    escape = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'[' | b'{' => {
                        level += 1;
                        if level >= 64 {
                            return Err("json_depth_limit");
                        }
                    }
                    b']' | b'}' => level = level.saturating_sub(1),
                    _ => {}
                }
            }
        }
        let raw: &RawValue = serde_json::from_str(input).map_err(|_| "json_invalid")?;
        Self::from_raw(raw, &mut 0)
    }

    fn from_raw(raw: &RawValue, nodes: &mut usize) -> Result<Self, &'static str> {
        *nodes += 1;
        if *nodes > 131072 {
            return Err("json_structure_limit");
        }
        let text = raw.get();
        match text.as_bytes()[0] {
            b'n' => Ok(Self(Value::Null)),
            b't' => Ok(Self(Value::Bool(true))),
            b'f' => Ok(Self(Value::Bool(false))),
            b'"' => serde_json::from_str(text)
                .map(|v| Self(Value::String(v)))
                .map_err(|_| "json_invalid"),
            b'[' => {
                let values: Vec<&RawValue> =
                    serde_json::from_str(text).map_err(|_| "json_invalid")?;
                values
                    .into_iter()
                    .map(|v| Self::from_raw(v, nodes))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| Self(Value::Array(v)))
            }
            b'{' => {
                let values: Entries =
                    serde_json::from_str(text).map_err(|_| "json_duplicate_or_invalid")?;
                values
                    .0
                    .into_iter()
                    .map(|(k, v)| Self::from_raw(v, nodes).map(|v| (k, v)))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| Self(Value::Object(v)))
            }
            _ => {
                if text.contains(['.', 'e', 'E']) {
                    let value: f64 = text.parse().map_err(|_| "json_nonfinite")?;
                    let mantissa = text.split(['e', 'E']).next().unwrap_or("");
                    if !value.is_finite()
                        || (value == 0.0 && mantissa.bytes().any(|b| (b'1'..=b'9').contains(&b)))
                    {
                        return Err("json_nonfinite");
                    }
                }
                Ok(Self(Value::Number(text.to_owned())))
            }
        }
    }

    pub fn dump(&self) -> String {
        match &self.0 {
            Value::Null => "null".into(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.clone(),
            Value::String(value) => quote(value).expect("validated string"),
            Value::Array(values) => format!(
                "[{}]",
                values.iter().map(Self::dump).collect::<Vec<_>>().join(",")
            ),
            Value::Object(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{}:{}", quote(k).expect("validated key"), v.dump()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    pub fn string(text: &str) -> Result<Self, &'static str> {
        quote(text)?;
        Ok(Self(Value::String(text.to_owned())))
    }
    pub fn object(entries: Vec<(&str, Json)>) -> Result<Self, &'static str> {
        let mut out = String::from("{");
        for (index, (key, value)) in entries.into_iter().enumerate() {
            if index != 0 {
                append(&mut out, ",")?;
            }
            append(&mut out, &quote(key)?)?;
            append(&mut out, ":")?;
            append(&mut out, &value.dump())?;
        }
        append(&mut out, "}")?;
        Self::parse(&out)
    }
    pub fn array(values: Vec<Json>) -> Result<Self, &'static str> {
        let mut out = String::from("[");
        for (index, value) in values.into_iter().enumerate() {
            if index != 0 {
                append(&mut out, ",")?;
            }
            append(&mut out, &value.dump())?;
        }
        append(&mut out, "]")?;
        Self::parse(&out)
    }
    pub fn field(&self, key: &str) -> Result<&Json, &'static str> {
        if let Value::Object(entries) = &self.0 {
            return entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .ok_or("json_missing_field");
        }
        Err("json_expected_object")
    }
    pub fn text(&self) -> Result<&str, &'static str> {
        if let Value::String(v) = &self.0 {
            Ok(v)
        } else {
            Err("json_expected_string")
        }
    }
    pub fn elements(&self) -> Result<&[Json], &'static str> {
        if let Value::Array(v) = &self.0 {
            Ok(v)
        } else {
            Err("json_expected_array")
        }
    }
    pub fn boolean(&self) -> Result<bool, &'static str> {
        if let Value::Bool(v) = &self.0 {
            Ok(*v)
        } else {
            Err("json_expected_bool")
        }
    }
}

fn append(out: &mut String, text: &str) -> Result<(), &'static str> {
    if text.len() > 16 * 1024 * 1024 - out.len() {
        return Err("json_build_limit");
    }
    out.push_str(text);
    Ok(())
}
fn quote(text: &str) -> Result<String, &'static str> {
    let mut out = String::from("\"");
    for c in text.chars() {
        let escaped = match c {
            '"' => "\\\"".into(),
            '\\' => "\\\\".into(),
            '\u{08}' => "\\b".into(),
            '\u{0c}' => "\\f".into(),
            '\n' => "\\n".into(),
            '\r' => "\\r".into(),
            '\t' => "\\t".into(),
            c if c < '\u{20}' => format!("\\u{:04X}", c as u32),
            c => c.to_string(),
        };
        append(&mut out, &escaped)?;
    }
    append(&mut out, "\"")?;
    Ok(out)
}
impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) | (Value::String(a), Value::String(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.iter().any(|(bk, bv)| k == bk && v == bv))
            }
            _ => false,
        }
    }
}
