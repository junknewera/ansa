pub mod epoch;
pub mod legacy_compact;
pub mod summary;

use crate::{protocol::json::Json, provider::request::Request};

pub(crate) fn estimate_text_tokens(text: &str) -> usize {
    let units: usize = text
        .chars()
        .map(|character| match character {
            c if c.is_ascii_alphanumeric() || c == ' ' => 1,
            c if c.is_ascii() => 3,
            c if u32::from(c) >= 0x1f000 => 8,
            _ => 4,
        })
        .sum();
    units.div_ceil(4)
}

pub(crate) fn estimate_request_tokens(request: &Request) -> usize {
    estimate_text_tokens(&request.epoch.instructions)
        + request
            .tools
            .descriptors
            .iter()
            .map(|tool| {
                estimate_text_tokens(&tool.name)
                    + estimate_text_tokens(&tool.description)
                    + estimate_text_tokens(&tool.schema.dump())
            })
            .sum::<usize>()
        + request
            .final_format()
            .and_then(|format| format.format().ok())
            .map_or(0, |format| estimate_text_tokens(&format.dump()))
        + estimate_input_tokens(&request.input)
}

pub(crate) fn estimate_input_tokens(items: &[Json]) -> usize {
    items.iter().map(estimate_item_tokens).sum()
}

pub(crate) fn require_response_room(tokens: usize, window: usize) -> Result<(), &'static str> {
    let reserve = (window / 4).clamp(1, 4096);
    if tokens > window.saturating_sub(reserve) {
        return Err("model_context_limit");
    }
    Ok(())
}

fn estimate_item_tokens(item: &Json) -> usize {
    let field = |key| item.text_field(key).map_or(0, estimate_text_tokens);
    let value = |value: &Json| {
        value.text().map_or_else(
            |_| estimate_text_tokens(&value.dump()),
            estimate_text_tokens,
        )
    };
    let parts = |parts: &[Json]| {
        parts
            .iter()
            .map(|part| {
                part.text_field("text")
                    .or_else(|_| part.text_field("refusal"))
                    .map_or_else(|_| value(part), estimate_text_tokens)
            })
            .sum()
    };
    match item.text_field("type") {
        Ok("message") | Err(_) => item.field("content").map_or_else(
            |_| value(item),
            |content| content.elements().map_or_else(|_| value(content), parts),
        ),
        Ok("function_call") => field("name") + field("arguments"),
        Ok("function_call_output") => item.field("output").map_or_else(|_| value(item), value),
        // Ciphertext is transport data. Only the readable summary is measurable.
        Ok("reasoning") => item
            .field("summary")
            .and_then(Json::elements)
            .map_or(0, parts),
        Ok("compaction") => field("text"),
        _ => value(item),
    }
}
