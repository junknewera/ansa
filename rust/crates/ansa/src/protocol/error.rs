//! Stable, payload-free diagnostics for protocol and boundary failures.
use super::json::Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    Protocol = 0,
    Limit = 1,
    Transport = 2,
    Mime = 3,
    Validation = 4,
    Cancelled = 5,
    Tool = 6,
    Contract = 7,
    ResponseFailed = 8,
    ResponseIncomplete = 9,
    PrematureEof = 10,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Stage {
    Json = 0,
    Framing = 1,
    Admission = 2,
    Request = 3,
    Preparation = 4,
    Execution = 5,
    Http = 6,
    Frontend = 7,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub stage: Stage,
    pub diagnostic: &'static str,
}
impl ProtocolError {
    pub fn new(code: ErrorCode, stage: Stage, diagnostic: &'static str) -> Self {
        Self {
            code,
            stage,
            diagnostic,
        }
    }
    pub fn from_json_error(diagnostic: &'static str) -> Self {
        let code = if matches!(
            diagnostic,
            "json_byte_limit" | "json_depth_limit" | "json_structure_limit"
        ) {
            ErrorCode::Limit
        } else {
            ErrorCode::Protocol
        };
        Self::new(code, Stage::Json, diagnostic)
    }
    pub fn json(&self) -> Result<Json, &'static str> {
        Json::object(vec![
            ("schema_version", Json::parse("1")?),
            ("code", Json::parse(&(self.code as u32).to_string())?),
            ("stage", Json::parse(&(self.stage as u32).to_string())?),
            ("diagnostic", Json::string(self.diagnostic)?),
        ])
    }
}
