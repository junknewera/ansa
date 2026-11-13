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
        Self::parse_with_limits(input, 16 * 1024 * 1024, 64, 131072)
    }
    pub fn parse_with_limits(
        input: &str,
        max_bytes: usize,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<Self, &'static str> {
        if input.is_empty() {
            return Err("json_empty");
        }
        if input.len() > max_bytes {
            return Err("json_byte_limit");
        }
        if max_depth == 0 || max_nodes == 0 {
            return Err("json_structure_limit");
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
                        if level >= max_depth {
                            return Err("json_depth_limit");
                        }
                    }
                    b']' | b'}' => level = level.saturating_sub(1),
                    _ => {}
                }
            }
        }
        let raw: &RawValue = serde_json::from_str(input).map_err(|_| "json_invalid")?;
        Self::from_raw(raw, &mut 0, max_nodes)
    }

    fn from_raw(raw: &RawValue, nodes: &mut usize, max_nodes: usize) -> Result<Self, &'static str> {
        *nodes += 1;
        if *nodes > max_nodes {
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
                    .map(|v| Self::from_raw(v, nodes, max_nodes))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| Self(Value::Array(v)))
            }
            b'{' => {
                let values: Entries = serde_json::from_str(text).map_err(|error| {
                    if error.to_string().contains("json_duplicate_key") {
                        "json_duplicate_key"
                    } else {
                        "json_invalid"
                    }
                })?;
                values
                    .0
                    .into_iter()
                    .map(|(k, v)| Self::from_raw(v, nodes, max_nodes).map(|v| (k, v)))
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
            Value::String(value) => quote_unbounded(value),
            Value::Array(values) => format!(
                "[{}]",
                values.iter().map(Self::dump).collect::<Vec<_>>().join(",")
            ),
            Value::Object(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{}:{}", quote_unbounded(k), v.dump()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    pub fn string(text: &str) -> Result<Self, &'static str> {
        Self::string_with_limits(text, 16 * 1024 * 1024)
    }
    pub fn string_with_limits(text: &str, max_bytes: usize) -> Result<Self, &'static str> {
        let encoded = quote_with_limit(text, max_bytes)?;
        Self::parse_with_limits(&encoded, max_bytes, 64, 1)
    }
    pub fn object(entries: Vec<(&str, Json)>) -> Result<Self, &'static str> {
        Self::object_with_limits(entries, 16 * 1024 * 1024)
    }
    pub fn object_with_limits(
        entries: Vec<(&str, Json)>,
        max_bytes: usize,
    ) -> Result<Self, &'static str> {
        let mut out = String::from("{");
        for (index, (key, value)) in entries.into_iter().enumerate() {
            if index != 0 {
                append_limit(&mut out, ",", max_bytes)?;
            }
            append_limit(&mut out, &quote_with_limit(key, max_bytes)?, max_bytes)?;
            append_limit(&mut out, ":", max_bytes)?;
            append_limit(&mut out, &value.dump(), max_bytes)?;
        }
        append_limit(&mut out, "}", max_bytes)?;
        Self::parse_with_limits(&out, max_bytes, 64, 131072)
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
    pub fn has(&self, key: &str) -> bool {
        matches!(&self.0, Value::Object(entries) if entries.iter().any(|(k,_)|k==key))
    }
    pub fn text_field(&self, key: &str) -> Result<&str, &'static str> {
        self.field(key)?.text()
    }
    pub fn usize_field(&self, key: &str) -> Result<usize, &'static str> {
        self.field(key)?.index()
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
    pub fn is_object(&self) -> bool {
        matches!(self.0, Value::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self.0, Value::Array(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self.0, Value::String(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self.0, Value::Number(_))
    }
    pub fn is_integer(&self) -> bool {
        matches!(&self.0, Value::Number(n) if !n.contains(['.', 'e', 'E']))
    }
    pub fn keys(&self) -> Result<Vec<&str>, &'static str> {
        if let Value::Object(entries) = &self.0 {
            Ok(entries.iter().map(|(k, _)| k.as_str()).collect())
        } else {
            Err("json_expected_object")
        }
    }
    pub fn index(&self) -> Result<usize, &'static str> {
        if let Value::Number(n) = &self.0
            && !n.starts_with('-')
            && !n.contains(['.', 'e', 'E'])
        {
            return n.parse().map_err(|_| "json_integer_overflow");
        }
        Err("json_expected_unsigned_integer")
    }
    pub fn with(&self, key: &str, replacement: Json) -> Result<Self, &'static str> {
        let Value::Object(entries) = &self.0 else {
            return Err("json_expected_object");
        };
        let mut out =
            Vec::with_capacity(entries.len() + usize::from(!entries.iter().any(|(k, _)| k == key)));
        let mut found = false;
        for (k, v) in entries {
            if k == key {
                if !found {
                    out.push((k.clone(), replacement.clone()));
                    found = true;
                }
            } else {
                out.push((k.clone(), v.clone()));
            }
        }
        if !found {
            out.push((key.to_owned(), replacement));
        }
        Self::object(out.iter().map(|(k, v)| (k.as_str(), v.clone())).collect())
    }
    pub fn without(&self, key: &str) -> Result<Self, &'static str> {
        let Value::Object(entries) = &self.0 else {
            return Err("json_expected_object");
        };
        Self::object(
            entries
                .iter()
                .filter(|(k, _)| k != key)
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect(),
        )
    }
}

fn append(out: &mut String, text: &str) -> Result<(), &'static str> {
    append_limit(out, text, 16 * 1024 * 1024)
}
fn append_limit(out: &mut String, text: &str, max_bytes: usize) -> Result<(), &'static str> {
    if text.len() > max_bytes.saturating_sub(out.len()) {
        return Err("json_build_limit");
    }
    out.push_str(text);
    Ok(())
}
fn quote_with_limit(text: &str, max_bytes: usize) -> Result<String, &'static str> {
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
        append_limit(&mut out, &escaped, max_bytes)?;
    }
    append_limit(&mut out, "\"", max_bytes)?;
    Ok(out)
}
fn quote_unbounded(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
