//! Owning JSON with exact numeric tokens and decoded duplicate-key rejection.
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Json(Arc<Node>);
#[derive(Debug)]
struct Node {
    value: Value,
    bytes: usize,
    nodes: usize,
    depth: usize,
}
#[derive(Debug)]
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
    pub const FRAME_BYTES: usize = 16 * 1024 * 1024;
    pub(crate) const JOURNAL_RECORD_BYTES: usize = 32 * 1024 * 1024;
    pub const FRAME_NODES: usize = 131072;

    pub(crate) fn text_fits_frame(text: &str) -> bool {
        quoted_bytes(text) <= Self::FRAME_BYTES
    }
    pub(crate) fn quoted_size(text: &str) -> usize {
        quoted_bytes(text)
    }
    pub(crate) fn encoded_bytes(&self) -> usize {
        self.0.bytes
    }

    fn owned(value: Value) -> Self {
        let (bytes, nodes, depth) = match &value {
            Value::Null => (4, 1, 0),
            Value::Bool(value) => (if *value { 4 } else { 5 }, 1, 0),
            Value::Number(value) => (value.len(), 1, 0),
            Value::String(value) => (quoted_bytes(value), 1, 0),
            Value::Array(values) => values.iter().enumerate().fold(
                (2usize, 1usize, 1usize),
                |(bytes, nodes, depth), (index, value)| {
                    (
                        bytes
                            .saturating_add(usize::from(index > 0))
                            .saturating_add(value.0.bytes),
                        nodes.saturating_add(value.0.nodes),
                        depth.max(value.0.depth.saturating_add(1)),
                    )
                },
            ),
            Value::Object(values) => values.iter().enumerate().fold(
                (2usize, 1usize, 1usize),
                |(bytes, nodes, depth), (index, (key, value))| {
                    (
                        bytes
                            .saturating_add(usize::from(index > 0))
                            .saturating_add(quoted_bytes(key))
                            .saturating_add(1)
                            .saturating_add(value.0.bytes),
                        nodes.saturating_add(value.0.nodes),
                        depth.max(value.0.depth.saturating_add(1)),
                    )
                },
            ),
        };
        Self(Arc::new(Node {
            value,
            bytes,
            nodes,
            depth,
        }))
    }
    fn value(&self) -> &Value {
        &self.0.value
    }
    fn constructed(self, max_bytes: usize) -> Result<Self, &'static str> {
        if self.0.bytes > max_bytes {
            return Err("json_build_limit");
        }
        if self.0.depth >= 64 {
            return Err("json_depth_limit");
        }
        if self.0.nodes > Self::FRAME_NODES {
            return Err("json_structure_limit");
        }
        Ok(self)
    }
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        Self::parse_with_limits(input, Self::FRAME_BYTES, 64, Self::FRAME_NODES)
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
            b'n' => Ok(Self::owned(Value::Null)),
            b't' => Ok(Self::owned(Value::Bool(true))),
            b'f' => Ok(Self::owned(Value::Bool(false))),
            b'"' => serde_json::from_str(text)
                .map(|v| Self::owned(Value::String(v)))
                .map_err(|_| "json_invalid"),
            b'[' => {
                let values: Vec<&RawValue> =
                    serde_json::from_str(text).map_err(|_| "json_invalid")?;
                values
                    .into_iter()
                    .map(|v| Self::from_raw(v, nodes, max_nodes))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|v| Self::owned(Value::Array(v)))
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
                    .map(|v| Self::owned(Value::Object(v)))
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
                Ok(Self::owned(Value::Number(text.to_owned())))
            }
        }
    }

    pub fn dump(&self) -> String {
        let mut out = String::with_capacity(self.0.bytes);
        self.write_dump(&mut out);
        out
    }
    fn write_dump(&self, out: &mut String) {
        match self.value() {
            Value::Null => out.push_str("null"),
            Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => out.push_str(value),
            Value::String(value) => write_quote(value, out),
            Value::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',')
                    }
                    value.write_dump(out);
                }
                out.push(']');
            }
            Value::Object(entries) => {
                out.push('{');
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',')
                    }
                    write_quote(key, out);
                    out.push(':');
                    value.write_dump(out);
                }
                out.push('}');
            }
        }
    }

    pub fn string(text: &str) -> Result<Self, &'static str> {
        Self::string_with_limits(text, Self::FRAME_BYTES)
    }
    pub fn string_with_limits(text: &str, max_bytes: usize) -> Result<Self, &'static str> {
        if quoted_bytes(text) > max_bytes {
            return Err("json_build_limit");
        }
        Ok(Self::owned(Value::String(text.to_owned())))
    }
    pub fn object(entries: Vec<(&str, Json)>) -> Result<Self, &'static str> {
        Self::object_with_limits(entries, Self::FRAME_BYTES)
    }
    pub fn object_with_limits(
        entries: Vec<(&str, Json)>,
        max_bytes: usize,
    ) -> Result<Self, &'static str> {
        let bytes = entries
            .iter()
            .enumerate()
            .fold(2usize, |size, (index, (key, value))| {
                size.saturating_add(usize::from(index > 0))
                    .saturating_add(quoted_bytes(key))
                    .saturating_add(1)
                    .saturating_add(value.0.bytes)
            });
        if bytes > max_bytes {
            return Err("json_build_limit");
        }
        if entries.iter().any(|(_, value)| value.0.depth >= 63) {
            return Err("json_depth_limit");
        }
        let mut keys = HashSet::new();
        for (key, _) in &entries {
            if !keys.insert(*key) {
                return Err("json_duplicate_key");
            }
        }
        Self::owned(Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        ))
        .constructed(max_bytes)
    }
    pub fn array(values: Vec<Json>) -> Result<Self, &'static str> {
        Self::owned(Value::Array(values)).constructed(Self::FRAME_BYTES)
    }
    pub fn field(&self, key: &str) -> Result<&Json, &'static str> {
        if let Value::Object(entries) = self.value() {
            return entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .ok_or("json_missing_field");
        }
        Err("json_expected_object")
    }
    pub fn has(&self, key: &str) -> bool {
        matches!(self.value(), Value::Object(entries) if entries.iter().any(|(k,_)|k==key))
    }
    pub fn text_field(&self, key: &str) -> Result<&str, &'static str> {
        self.field(key)?.text()
    }
    pub fn usize_field(&self, key: &str) -> Result<usize, &'static str> {
        self.field(key)?.index()
    }
    pub fn text(&self) -> Result<&str, &'static str> {
        if let Value::String(v) = self.value() {
            Ok(v)
        } else {
            Err("json_expected_string")
        }
    }
    pub fn elements(&self) -> Result<&[Json], &'static str> {
        if let Value::Array(v) = self.value() {
            Ok(v)
        } else {
            Err("json_expected_array")
        }
    }
    pub fn boolean(&self) -> Result<bool, &'static str> {
        if let Value::Bool(v) = self.value() {
            Ok(*v)
        } else {
            Err("json_expected_bool")
        }
    }
    pub fn is_object(&self) -> bool {
        matches!(self.value(), Value::Object(_))
    }
    pub fn is_null(&self) -> bool {
        matches!(self.value(), Value::Null)
    }
    pub fn is_array(&self) -> bool {
        matches!(self.value(), Value::Array(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self.value(), Value::String(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self.value(), Value::Number(_))
    }
    pub fn is_integer(&self) -> bool {
        matches!(self.value(), Value::Number(n) if !n.contains(['.', 'e', 'E']))
    }
    pub fn keys(&self) -> Result<Vec<&str>, &'static str> {
        if let Value::Object(entries) = self.value() {
            Ok(entries.iter().map(|(k, _)| k.as_str()).collect())
        } else {
            Err("json_expected_object")
        }
    }
    pub fn index(&self) -> Result<usize, &'static str> {
        if let Value::Number(n) = self.value()
            && !n.starts_with('-')
            && !n.contains(['.', 'e', 'E'])
        {
            return n.parse().map_err(|_| "json_integer_overflow");
        }
        Err("json_expected_unsigned_integer")
    }
    pub fn with(&self, key: &str, replacement: Json) -> Result<Self, &'static str> {
        let Value::Object(entries) = self.value() else {
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
        let Value::Object(entries) = self.value() else {
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

fn quoted_bytes(text: &str) -> usize {
    text.bytes().fold(2usize, |size, byte| {
        size.saturating_add(match byte {
            b'"' | b'\\' | 8 | 9 | 10 | 12 | 13 => 2,
            0..=31 => 6,
            _ => 1,
        })
    })
}
fn write_quote(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < '\u{20}' => {
                use std::fmt::Write;
                write!(out, "\\u{:04X}", c as u32).expect("String write");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        match (self.value(), other.value()) {
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
