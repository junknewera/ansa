//! Prompt summary through the same admitted provider callback as ordinary turns.
use super::legacy_compact::{CompactedContext, summary_request};
use crate::runtime::{Compact, Generate};
use std::sync::Arc;
pub fn summarizer(generate: Generate) -> Compact {
    Arc::new(move |original, stop| {
        let request = summary_request(original)?;
        let response = generate(&request, stop, Arc::new(|_| Ok(())))?;
        CompactedContext::from_summary(original.clone(), &response)
    })
}
