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

#[derive(Debug)]
pub(crate) enum PrepareError {
    Fatal(&'static str),
    Invalid { index: usize, message: String },
}
impl From<&'static str> for PrepareError {
    fn from(code: &'static str) -> Self {
        Self::Fatal(code)
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
    /// Consumes the validated batch without exposing an executor capability.
    /// Runtime dispatch still requires an independently durable intent receipt.
    pub(crate) fn into_plan(mut self) -> Result<Vec<super::invocation::Invocation>, &'static str> {
        self.pending
            .take()
            .ok_or("batch_already_consumed")?
            .into_iter()
            .map(|(operation, call, name, arguments)| {
                let definition = self.snapshot.definition(&name).ok_or("unknown_tool")?;
                Ok(super::invocation::Invocation {
                    operation,
                    call,
                    name,
                    revision: definition.descriptor.revision.clone(),
                    arguments,
                })
            })
            .collect()
    }
    pub fn prepare(
        response: &AdmittedResponse,
        snapshot: Arc<Snapshot>,
        ids: &mut OperationIds,
    ) -> Result<Self, &'static str> {
        Self::prepare_checked(response, snapshot, ids).map_err(|error| match error {
            PrepareError::Invalid { .. } => "tool_arguments_rejected",
            PrepareError::Fatal(code) => code,
        })
    }
    pub(crate) fn prepare_checked(
        response: &AdmittedResponse,
        snapshot: Arc<Snapshot>,
        ids: &mut OperationIds,
    ) -> Result<Self, PrepareError> {
        if !snapshot.definitions_match_advertisement()
            || response.request().tools != *snapshot.advertisement()
        {
            return Err("snapshot_provenance_conflict".into());
        }
        let mut pending = Vec::with_capacity(response.calls().len());
        // Validate the entire batch before allocating IDs or making any executor reachable.
        let mut invalid = None;
        for (index, call) in response.calls().iter().enumerate() {
            let definition = snapshot.definition(&call.name).ok_or("unknown_tool")?;
            let args = Json::parse_with_limits(&call.arguments, 256 * 1024, 64, 131072)?;
            let issue = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let schema = &definition.descriptor.schema;
                if let Err(message) = super::schema::check_arguments(schema, &args) {
                    return Some(message);
                }
                if (definition.validator)(&args) {
                    return None;
                }
                let detail = snapshot
                    .policy(&call.name)
                    .and_then(|policy| policy.argument_hint.as_ref())
                    .and_then(|hint| hint(&args))
                    .unwrap_or(&definition.descriptor.description);
                Some(format!("Check the argument values. {detail}"))
            }))
            .map_err(|_| "tool_validator_panicked")?;
            if let Some(issue) = issue
                && invalid.is_none()
            {
                let mut message = format!("{}: {issue}", call.name);
                message.truncate(message.floor_char_boundary(512));
                invalid = Some(PrepareError::Invalid { index, message });
            }
            pending.push((String::new(), call.call_id.clone(), call.name.clone(), args));
        }
        let fingerprint = super::super::provider::replay::response_binding(response)?;
        if let Some(error) = invalid {
            return Err(error);
        }
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
    pub fn execute_with_cancel(self, mut cancelled: impl FnMut() -> bool) -> KnownBatchResults {
        self.execute_checked(&mut cancelled, &crate::auth::Cancel::default())
    }
    pub fn execute_with_token(self, token: &crate::auth::Cancel) -> KnownBatchResults {
        self.execute_checked(&mut || token.cancelled(), token)
    }
    fn execute_checked(
        mut self,
        cancelled: &mut dyn FnMut() -> bool,
        token: &crate::auth::Cancel,
    ) -> KnownBatchResults {
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
        if pending.iter().any(|(_, _, name, _)| {
            self.snapshot
                .policy(name)
                .is_some_and(|p| p.requires_journal)
        }) {
            for result in &mut results {
                *result = ToolResult::new(
                    result.operation_id().into(),
                    result.call_id().into(),
                    r#"{"error":"durable_checkpoint_required"}"#.into(),
                    Execution::Denied,
                    Effects::NotStarted,
                );
            }
            return KnownBatchResults::new(self.request_fingerprint, results);
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
                if let Some(invoke) = self.snapshot.policy(&name).and_then(|p| p.invoke.as_ref()) {
                    invoke(
                        &super::invocation::Invocation {
                            operation: operation_id.clone(),
                            call: call_id.clone(),
                            name: name.clone(),
                            revision: definition.descriptor.revision.clone(),
                            arguments: args.clone(),
                        },
                        token,
                    )
                } else {
                    (definition.executor)(&args)
                }
            }));
            let result = match outcome {
                Ok(mut value) => {
                    if value.execution == Execution::Succeeded
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
                            Ok(output) if Json::text_fits_frame(&output) => ToolResult::new(
                                operation_id.clone(),
                                call_id.clone(),
                                output,
                                value.execution,
                                value.effects,
                            ),
                            _ => ToolResult::new(
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
