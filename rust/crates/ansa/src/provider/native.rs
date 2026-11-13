//! Provider native items are retained as owned lossless JSON values.
use crate::protocol::json::Json;
use crate::provider::{
    request::Request,
    responses::{AdmittedResponse, decode_synthetic},
};
#[derive(Debug, Clone, PartialEq)]
pub struct NativeItem {
    value: Json,
}
impl NativeItem {
    pub fn new(value: Json) -> Result<Self, &'static str> {
        if !value.is_object() {
            return Err("native_item_object");
        }
        Ok(Self { value })
    }
    pub fn value(&self) -> &Json {
        &self.value
    }
}

/// Persisted response envelope. Loading always reconstructs the SSE lifecycle
/// and routes it through the same production reducer as a network response.
pub fn save(response: &AdmittedResponse) -> Result<Json, &'static str> {
    Json::object(vec![
        ("request", response.request().save()?),
        ("id", Json::string(response.id())?),
        ("model", Json::string(response.returned_model())?),
        (
            "items",
            Json::array(
                response
                    .items()
                    .iter()
                    .map(|item| item.value().clone())
                    .collect(),
            )?,
        ),
    ])
}

pub fn load(value: &Json) -> Result<AdmittedResponse, &'static str> {
    let request = Request::load(value.field("request")?)?;
    let id = value.text_field("id")?;
    let model = value.text_field("model")?;
    let items = value.field("items")?.elements()?;
    if id.is_empty() || items.len() > crate::protocol::sse::Limits::default().items {
        return Err("saved_response_identity_or_limit");
    }
    let mut events = Vec::with_capacity(items.len().saturating_mul(3).saturating_add(2));
    let mut sequence = 0usize;
    let mut emit = |kind: &str, fields: Vec<(&str, Json)>| -> Result<(), &'static str> {
        let mut all = vec![
            ("type", Json::string(kind)?),
            ("sequence_number", Json::parse(&sequence.to_string())?),
        ];
        all.extend(fields);
        sequence = sequence.checked_add(1).ok_or("saved_response_sequence")?;
        events.push(Json::object(all)?);
        Ok(())
    };
    let empty = Json::array(vec![])?;
    emit(
        "response.created",
        vec![(
            "response",
            Json::object(vec![
                ("id", Json::string(id)?),
                ("model", Json::string(model)?),
                ("status", Json::string("in_progress")?),
                ("output", empty.clone()),
            ])?,
        )],
    )?;
    for (index, item) in items.iter().enumerate() {
        if !item.is_object() {
            return Err("saved_native_item_object");
        }
        let mut added = item.with("status", Json::string("in_progress")?)?;
        if item.text_field("type")? == "function_call" {
            added = added.with("arguments", Json::string("")?)?;
        }
        emit(
            "response.output_item.added",
            vec![
                ("output_index", Json::parse(&index.to_string())?),
                ("item", added),
            ],
        )?;
        if item.text_field("type")? == "function_call" {
            emit(
                "response.function_call_arguments.done",
                vec![
                    ("output_index", Json::parse(&index.to_string())?),
                    ("item_id", item.field("id")?.clone()),
                    ("call_id", item.field("call_id")?.clone()),
                    ("name", item.field("name")?.clone()),
                    ("arguments", item.field("arguments")?.clone()),
                ],
            )?;
        }
        emit(
            "response.output_item.done",
            vec![
                ("output_index", Json::parse(&index.to_string())?),
                ("item", item.clone()),
            ],
        )?;
    }
    emit(
        "response.completed",
        vec![(
            "response",
            Json::object(vec![
                ("id", Json::string(id)?),
                ("model", Json::string(model)?),
                ("status", Json::string("completed")?),
                ("output", Json::array(items.to_vec())?),
                ("error", Json::parse("null")?),
                ("incomplete_details", Json::parse("null")?),
            ])?,
        )],
    )?;
    let body = events
        .iter()
        .map(|event| format!("data: {}\n\n", event.dump()))
        .collect::<String>();
    let limits = crate::protocol::sse::Limits {
        body: 128 * 1024 * 1024,
        event: 32 * 1024 * 1024,
        line: 32 * 1024 * 1024,
        ..Default::default()
    };
    decode_synthetic(
        request,
        200,
        "text/event-stream",
        &[body.as_bytes()],
        limits,
    )
}
