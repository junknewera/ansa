pub mod epoch;
pub mod legacy_compact;
pub mod summary;

use crate::{protocol::json::Json, provider::request::Request};

pub(crate) fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
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
            .input
            .iter()
            .map(estimate_item_tokens)
            .sum::<usize>()
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
