//! HTTP response head validation and body-to-Responses admission.
use crate::{
    protocol::sse::Limits,
    provider::{request::Request, responses::AdmittedResponse},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub content_types: Vec<String>,
}
pub const RESPONSES_ENDPOINT: &str = "https://api.openai.com/v1/responses";
pub fn admit_request(
    head: &Head,
    url: &str,
    method: &str,
    request: &crate::protocol::json::Json,
    allow_missing_siwc: bool,
) -> Result<(), &'static str> {
    if url != RESPONSES_ENDPOINT || method != "POST" {
        return Err("http_endpoint");
    }
    if !request.field("stream")?.boolean()? || request.field("store")?.boolean()? {
        return Err("http_request_profile");
    }
    if head.status != 200 {
        return Err("http_status");
    }
    if head.content_types.is_empty() {
        return if allow_missing_siwc {
            Ok(())
        } else {
            Err("http_content_type_missing")
        };
    }
    validate_head(head)
}
pub fn validate_head(head: &Head) -> Result<(), &'static str> {
    if head.status != 200 {
        return Err("http_status");
    };
    if head.content_types.len() != 1 {
        return Err("http_content_type_count");
    };
    let mime = head.content_types[0]
        .trim_matches([' ', '\t'])
        .to_ascii_lowercase();
    let mut parts = mime.splitn(2, ';');
    let media = parts.next().unwrap_or("").trim_matches([' ', '\t']);
    if media != "text/event-stream" {
        return Err("http_mime");
    };
    if let Some(parameter) = parts.next() {
        let parameter = parameter.trim_matches([' ', '\t']);
        if parameter != "charset=utf-8" && parameter != "charset=\"utf-8\"" {
            return Err("http_mime_parameter");
        }
    }
    Ok(())
}
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
pub(crate) fn decode_synthetic_with_limits(
    request: Request,
    head: Head,
    chunks: &[&[u8]],
    limits: Limits,
) -> Result<AdmittedResponse, &'static str> {
    validate_head(&head)?;
    crate::provider::responses::decode_synthetic(
        request,
        head.status,
        &head.content_types[0],
        chunks,
        limits,
    )
}

/// Feature-gated JSONL adapter. Not present in the default production API.
#[cfg(feature = "protocol-test-adapter")]
pub fn decode_fixture(
    request: Request,
    head: Head,
    chunks: &[&[u8]],
) -> Result<AdmittedResponse, &'static str> {
    decode_fixture_with_limits(request, head, chunks, Limits::default())
}
#[cfg(feature = "protocol-test-adapter")]
pub fn decode_fixture_with_limits(
    request: Request,
    head: Head,
    chunks: &[&[u8]],
    limits: Limits,
) -> Result<AdmittedResponse, &'static str> {
    decode_synthetic_with_limits(request, head, chunks, limits)
}
