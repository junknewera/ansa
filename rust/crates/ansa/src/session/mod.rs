//! Canonical append-only session v1. No executor or runtime bootstrap.
pub mod catalog;
pub mod codec_v1;
pub mod journal_v1;
pub mod state;
pub mod store;
pub mod transcript;
pub type Result<T> = std::result::Result<T, &'static str>;
pub(crate) fn need(value: bool, code: &'static str) -> Result<()> {
    if value { Ok(()) } else { Err(code) }
}
pub(crate) fn num(value: usize) -> Result<crate::protocol::json::Json> {
    crate::protocol::json::Json::parse(&value.to_string())
}
