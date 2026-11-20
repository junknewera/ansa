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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudget {
    pub window: usize,
    pub tokens: Option<usize>,
}
impl ContextBudget {
    pub(crate) fn save(self, fields: &mut Vec<(&str, Json)>) -> Result<(), &'static str> {
        if let Some(tokens) = self.tokens {
            fields.push(("context_tokens", Json::parse(&tokens.to_string())?));
            fields.push(("context_window", Json::parse(&self.window.to_string())?));
        } else {
            // Older readers expect both fields in the measured representation.
            fields.push((
                "model_context_window",
                Json::parse(&self.window.to_string())?,
            ));
        }
        Ok(())
    }

    pub(crate) fn load(value: &Json) -> Result<Option<Self>, &'static str> {
        let measured = value.has("context_tokens") || value.has("context_window");
        let window_only = value.has("model_context_window");
        let (window, tokens) = match (measured, window_only) {
            (false, false) => return Ok(None),
            (true, false) => (
                value.usize_field("context_window")?,
                Some(value.usize_field("context_tokens")?),
            ),
            (false, true) => (value.usize_field("model_context_window")?, None),
            (true, true) => return Err("saved_response_context_usage"),
        };
        if window < 2 || tokens == Some(0) {
            return Err("saved_response_context_usage");
        }
        Ok(Some(Self { window, tokens }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReportedUsage {
    input: usize,
    cached_input: Option<usize>,
    output: usize,
}

impl ReportedUsage {
    pub(crate) fn new(input: usize, cached_input: Option<usize>, output: usize) -> Option<Self> {
        input.checked_add(output)?;
        if cached_input.is_some_and(|cached| cached > input) {
            return None;
        }
        Some(Self {
            input,
            cached_input,
            output,
        })
    }

    /// Zero input cannot calibrate the size of the preceding context.
    pub(crate) fn calibration_tokens(self) -> Option<usize> {
        (self.input > 0).then_some(self.input + self.output)
    }

    pub(crate) fn save(self) -> Result<Json, &'static str> {
        let mut fields = vec![
            ("input_tokens", Json::parse(&self.input.to_string())?),
            ("output_tokens", Json::parse(&self.output.to_string())?),
        ];
        if let Some(cached) = self.cached_input {
            fields.push(("cached_input_tokens", Json::parse(&cached.to_string())?));
        }
        Json::object(fields)
    }

    fn load(value: &Json) -> Result<Self, &'static str> {
        Self::new(
            value.usize_field("input_tokens")?,
            if value.has("cached_input_tokens") {
                Some(value.usize_field("cached_input_tokens")?)
            } else {
                None
            },
            value.usize_field("output_tokens")?,
        )
        .ok_or("saved_response_reported_usage")
    }
}

/// Persisted response envelope. Loading always reconstructs the SSE lifecycle
/// and routes it through the same production reducer as a network response.
pub fn save(response: &AdmittedResponse) -> Result<Json, &'static str> {
    let mut fields = vec![
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
    ];
    if let Some(budget) = response.context_budget() {
        budget.save(&mut fields)?;
    } else if let Some(tokens) = response.token_usage() {
        fields.push(("measured_context_tokens", Json::parse(&tokens.to_string())?));
    }
    if let Some(tokens) = response.request().estimated_tokens() {
        fields.push((
            "estimated_input_tokens_v1",
            Json::parse(&tokens.to_string())?,
        ));
    }
    if let Some(usage) = response.reported_usage() {
        fields.push(("reported_usage_v1", usage.save()?));
    }
    Json::object_with_limits(fields, Json::JOURNAL_RECORD_BYTES)
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
    let response = decode_synthetic(
        request,
        200,
        "text/event-stream",
        &[body.as_bytes()],
        limits,
    )?;
    let budget = ContextBudget::load(value)?;
    let response = if value.has("measured_context_tokens") {
        let tokens = value.usize_field("measured_context_tokens")?;
        if budget.is_some() || tokens == 0 {
            return Err("saved_response_context_usage");
        }
        response.with_token_usage(Some(tokens))
    } else if let Some(budget) = budget {
        response
            .with_token_usage(budget.tokens)
            .with_context_window(Some(budget.window))
    } else {
        response
    };
    let response = if value.has("estimated_input_tokens_v1") {
        response.with_context_estimate(value.usize_field("estimated_input_tokens_v1")?)
    } else {
        response
    };
    if !value.has("reported_usage_v1") {
        return Ok(response);
    }
    let usage = ReportedUsage::load(value.field("reported_usage_v1")?)?;
    if response.token_usage() != usage.calibration_tokens() {
        return Err("saved_response_reported_usage");
    }
    Ok(response.with_reported_usage(Some(usage)))
}
