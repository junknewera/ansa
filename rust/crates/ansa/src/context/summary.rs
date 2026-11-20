//! Prompt summary through the same admitted provider callback as ordinary turns.
use super::legacy_compact::{CompactedContext, summary_request_at};
use crate::runtime::{Compact, Generate};
use std::sync::Arc;
pub fn summarizer(generate: Generate) -> Compact {
    Arc::new(move |original, prefix, stop| {
        let request = summary_request_at(original, prefix)?;
        let response = generate(&request, stop, Arc::new(|_| Ok(())))?;
        CompactedContext::from_summary_at(original.clone(), &response, prefix)
    })
}
