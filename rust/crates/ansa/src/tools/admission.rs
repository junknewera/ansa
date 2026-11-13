//! Whole-response validation followed by an owned, consume-once batch.
use crate::{
    protocol::json::Json,
    provider::{
        replay::{Effects, Execution, KnownBatchResults, ToolResult},
        responses::AdmittedResponse,
    },
    tools::registry::Snapshot,
};
use std::sync::Arc;

/// Runtime-owned operation ID allocator; a prepared batch never invents IDs.
pub struct OperationIds {
    run: String,
    next: u64,
}
impl OperationIds {
    pub fn new(run: impl Into<String>, offset: u64) -> Result<Self, &'static str> {
        let run = run.into();
        if run.is_empty()
            || run.len() > 64
            || !run.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("invalid_run_id");
        };
        Ok(Self { run, next: offset })
    }
    fn allocate(&mut self) -> Result<String, &'static str> {
        self.next = self.next.checked_add(1).ok_or("operation_id_exhausted")?;
        Ok(format!("{}:op:{}", self.run, self.next))
    }
}

/// A prepared batch cannot be cloned or dispatched twice.
/// ```compile_fail,E0277
/// use ansa::tools::admission::PreparedBatch;
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<PreparedBatch>();
/// ```
/// ```compile_fail,E0382
/// use ansa::tools::admission::PreparedBatch;
/// fn dispatch_twice(mut batch: PreparedBatch) {
///     batch.execute(false);
///     batch.execute(false);
/// }
/// ```
pub struct PreparedBatch {
    snapshot: Arc<Snapshot>,
    request_fingerprint: String,
    pending: Option<Vec<(String, String, String, Json)>>,
}
impl PreparedBatch {
    pub fn prepare(
        response: &AdmittedResponse,
        snapshot: Arc<Snapshot>,
        ids: &mut OperationIds,
    ) -> Result<Self, &'static str> {
        if !snapshot.definitions_match_advertisement()
            || response.request().tools != *snapshot.advertisement()
        {
            return Err("snapshot_provenance_conflict");
        }
        let mut pending = Vec::with_capacity(response.calls().len());
        // Validate the entire batch before allocating IDs or making any executor reachable.
        for call in response.calls() {
            let definition = snapshot.definition(&call.name).ok_or("unknown_tool")?;
            let args = Json::parse_with_limits(&call.arguments, 256 * 1024, 64, 131072)?;
            let valid = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                args.is_object()
                    && super::schema::matches(&definition.descriptor.schema, &args)
                    && (definition.validator)(&args)
            }))
            .map_err(|_| "tool_validator_panicked")?;
            if !valid {
                return Err("tool_arguments_rejected");
            }
            pending.push((String::new(), call.call_id.clone(), call.name.clone(), args));
        }
        let fingerprint = super::super::provider::replay::response_binding(response)?;
        for entry in &mut pending {
            entry.0 = ids.allocate()?
        }
        Ok(Self {
            snapshot,
            request_fingerprint: fingerprint,
            pending: Some(pending),
        })
    }
    pub fn execute(self, cancelled: bool) -> KnownBatchResults {
        self.execute_with_cancel(|| cancelled)
    }
    pub fn execute_with_cancel(mut self, mut cancelled: impl FnMut() -> bool) -> KnownBatchResults {
        let pending = self.pending.take().expect("consume-once batch");
        let mut results = Vec::with_capacity(pending.len());
        for (operation_id, call_id, _, _) in &pending {
            results.push(ToolResult::new(
                operation_id.clone(),
                call_id.clone(),
                r#"{"error":"cancelled_before_dispatch"}"#.to_owned(),
                Execution::Cancelled,
                Effects::NotStarted,
            ))
        }
        for (index, (operation_id, call_id, name, args)) in pending.into_iter().enumerate() {
            if cancelled() {
                break;
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let definition = self
                    .snapshot
                    .definition(&name)
                    .expect("immutable validated snapshot");
                (definition.executor)(&args)
            }));
            let result = match outcome {
                Ok(mut value) => {
                    if value.output.len() > 128 * 1024
                        || value.execution == Execution::Succeeded
                            && value.effects != Effects::KnownResult
                    {
                        ToolResult::new(
                            operation_id.clone(),
                            call_id.clone(),
                            r#"{"error":"executor_failure","effects":"unknown"}"#.to_owned(),
                            Execution::Failed,
                            Effects::Unknown,
                        )
                    } else {
                        match String::from_utf8(std::mem::take(&mut value.output)) {
                            Ok(output) => ToolResult::new(
                                operation_id.clone(),
                                call_id.clone(),
                                output,
                                value.execution,
                                value.effects,
                            ),
                            Err(_) => ToolResult::new(
                                operation_id.clone(),
                                call_id.clone(),
                                r#"{"error":"executor_failure","effects":"unknown"}"#.to_owned(),
                                Execution::Failed,
                                Effects::Unknown,
                            ),
                        }
                    }
                }
                Err(_) => ToolResult::new(
                    operation_id.clone(),
                    call_id.clone(),
                    r#"{"error":"executor_failure","effects":"unknown"}"#.to_owned(),
                    Execution::Failed,
                    Effects::Unknown,
                ),
            };
            let stop = result.effects() == Effects::Unknown;
            results[index] = result;
            if stop {
                break;
            }
        }
        KnownBatchResults::new(self.request_fingerprint, results)
    }
}
