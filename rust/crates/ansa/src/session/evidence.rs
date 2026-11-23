//! Redacted evidence derived from the canonical journal and production reducer.
use super::{
    Result,
    codec_v1::{Invocation, SavedResult},
    journal_v1::inspect_records_read_only,
    need, num,
    state::State,
};
use crate::{
    context::estimate_request_tokens,
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        contracts::valid_id,
        replay::{Effects, Execution},
        request::{Advertisement, Request},
        responses::AdmittedResponse,
    },
    tools::registry::Snapshot,
};
use std::path::Path;

const JOURNAL_BYTES: usize = 64 * 1024 * 1024;
const RECORD_LIMIT: u64 = 8192;

fn flag(value: bool) -> Result<Json> {
    Json::parse(if value { "true" } else { "false" })
}

fn digest(value: &Json) -> Result<Json> {
    Json::string(&sha256(value.dump().as_bytes()))
}

fn identity(request: &Request) -> Result<Json> {
    use crate::context::prompt_policy::PromptPolicy;
    let saved = request.save()?;
    let root = crate::harness::root_epoch_state(&request.epoch.exact_state)?;
    let base = root
        .without("harness_profile")?
        .without("prompt_policy")?
        .without("harness_trial")?;
    let prompt_policy = match PromptPolicy::resolve(&request.epoch.exact_state)? {
        PromptPolicy::LegacyV1 => "legacy-v1",
        PromptPolicy::HandoffV1 => "handoff-v1",
    };
    let mut fields = vec![
        ("provider", Json::string(&request.binding.provider)?),
        (
            "requested_model",
            Json::string(&request.binding.requested_model)?,
        ),
        (
            "returned_model",
            Json::string(&request.binding.returned_model)?,
        ),
        ("context_sha256", digest(saved.field("epoch")?)?),
        ("base_context_sha256", digest(&base)?),
        ("prompt_policy", Json::string(prompt_policy)?),
        ("tool_snapshot_sha256", digest(saved.field("tools")?)?),
        ("request_sha256", digest(&saved)?),
        (
            "account_binding_sha256",
            Json::string(&sha256(request.binding.account.as_bytes()))?,
        ),
        (
            "effort",
            request
                .effort
                .as_deref()
                .map(Json::string)
                .transpose()?
                .unwrap_or(Json::parse("null")?),
        ),
    ];
    let pin = harness_identity(root)?;
    if let Some(trial) = harness_trial(root, pin.is_some())? {
        fields.push(("harness_trial", trial));
    }
    if let Some((profile, settings)) = pin {
        fields.push(("harness_profile", profile));
        fields.push(("harness_settings_sha256", digest(settings)?));
        fields.push((
            "harness_cwd_sha256",
            Json::string(&sha256(settings.text_field("cwd")?.as_bytes()))?,
        ));
    }
    Json::object(fields)
}

fn harness_identity(state: &Json) -> Result<Option<(Json, &Json)>> {
    crate::harness::saved_identity(state)
        .map(|entry| entry.map(|(id, settings)| (id.clone(), settings)))
        .map_err(|_| "evidence_harness_profile")
}

fn harness_trial(root: &Json, pinned: bool) -> Result<Option<Json>> {
    if !root.has("harness_trial") {
        return Ok(None);
    }
    let trial = root.field("harness_trial")?;
    let proposal = trial.text_field("proposal")?;
    let arm = trial.text_field("arm")?;
    need(
        pinned
            && trial.keys()?.len() == 2
            && crate::harness::digest_ok(proposal)
            && matches!(arm, "baseline" | "candidate"),
        "evidence_harness_trial",
    )?;
    Ok(Some(Json::object(vec![
        ("proposal", Json::string(proposal)?),
        ("arm", Json::string(arm)?),
    ])?))
}

fn add_usage(
    fields: &mut Vec<(&'static str, Json)>,
    response: Option<&AdmittedResponse>,
) -> Result<()> {
    let Some(response) = response else {
        fields.push(("estimate_source", Json::string("unknown")?));
        fields.push(("context_window_status", Json::string("unknown")?));
        return Ok(());
    };
    let estimate = response.request().estimated_tokens();
    fields.push((
        "estimated_input_tokens",
        num(estimate.unwrap_or_else(|| estimate_request_tokens(response.request())))?,
    ));
    fields.push((
        "estimate_source",
        Json::string(if estimate.is_some() {
            "dispatch"
        } else {
            "recomputed"
        })?,
    ));
    if let Some(usage) = response.reported_usage() {
        fields.push(("reported_usage", usage.save()?));
    } else if let Some(tokens) = response.token_usage() {
        fields.push(("legacy_reported_total_tokens", num(tokens)?));
    }
    let window = response.context_budget().map(|budget| budget.window);
    fields.push((
        "context_window_status",
        Json::string(if window.is_some() { "known" } else { "unknown" })?,
    ));
    if let Some(window) = window {
        fields.push(("context_window", num(window)?));
    }
    Ok(())
}

pub(crate) fn export(path: &Path, compiled: &Snapshot, fixture: &Snapshot) -> Result<Json> {
    let mut budget = JOURNAL_BYTES;
    let mut state = None::<State>;
    let mut records = Vec::new();
    let head = inspect_records_read_only(
        path,
        &mut budget,
        || Ok(()),
        |sequence, hash, event| {
            need(sequence <= RECORD_LIMIT, "evidence_record_limit")?;
            let kind = event.text_field("kind")?;
            if state.is_none() {
                need(kind == "init", "session_missing_init")?;
                let session = event.text_field("session")?;
                need(valid_id(session), "invalid_session_id")?;
                let saved = Request::load(event.field("configuration")?)?;
                let plan_catalog = if saved.tools == *fixture.advertisement() {
                    fixture
                } else {
                    compiled
                };
                let plan =
                    plan_catalog
                        .verified_plan_tools(&saved.tools)
                        .unwrap_or(Advertisement {
                            revision: String::new(),
                            descriptors: vec![],
                        });
                state = Some(State::new(session.into(), saved, plan));
            }
            let current = state.as_mut().ok_or("session_missing_init")?;
            let pending = current.turn.as_ref().and_then(|turn| turn.intent.as_ref());
            let artifacts = if kind == "outcome"
                && pending.is_some_and(|intent| intent.name == "python_execute")
            {
                analysis_artifacts(event, &current.session.0)?
            } else {
                None
            };
            current.apply(event)?;
            let mut fields = vec![
                ("sequence", num(sequence as usize)?),
                ("record_sha256", Json::string(hash)?),
                ("kind", Json::string(kind)?),
            ];
            if let Some(turn) = &current.turn {
                fields.push(("turn", Json::string(&turn.receipt.turn.0)?));
                fields.push(("request", Json::string(&turn.receipt.request.0)?));
            }
            match kind {
                "accepted" => {
                    let turn = current.turn.as_ref().ok_or("session_turn_identity")?;
                    fields.push(("request_manifest", identity(&turn.next)?));
                    fields.push(("internal", flag(turn.receipt.internal)?));
                    fields.push((
                        "prompt_sha256",
                        Json::string(&sha256(turn.receipt.text.as_bytes()))?,
                    ));
                }
                "response" | "side_reply" => {
                    let side;
                    let response = if kind == "side_reply" {
                        side = super::codec_v1::load_response(event.field("response")?)?;
                        &side
                    } else {
                        current
                            .turn
                            .as_ref()
                            .and_then(|turn| turn.response.as_deref())
                            .ok_or("session_response_stage")?
                    };
                    fields.push(("request_manifest", identity(response.request())?));
                    fields.push(("response_id", Json::string(response.id())?));
                    fields.push(("requested_tool_calls", num(response.calls().len())?));
                    add_usage(&mut fields, Some(response))?;
                }
                "compacted" => {
                    let compaction = event.field("compaction")?;
                    let request = Request::load(compaction.field("request")?)?;
                    fields.push(("source_manifest", identity(&request)?));
                    let native = compaction.field("native")?;
                    let summary = match native.text_field("object")? {
                        "omh.summary.v1" | "omh.summary.v2" => Some(
                            super::codec_v1::load_response(native.field("summary_response")?)?,
                        ),
                        _ => None,
                    };
                    add_usage(&mut fields, summary.as_ref())?;
                    let turn = current.turn.as_ref().ok_or("session_turn_identity")?;
                    fields.push(("request_manifest", identity(&turn.next)?));
                }
                "intent" => {
                    let intent = Invocation::load(event.field("invocation")?)?;
                    fields.push(("operation", Json::string(&intent.operation)?));
                    fields.push(("call", Json::string(&intent.call)?));
                    fields.push(("tool", Json::string(&intent.name)?));
                }
                "outcome" => {
                    let saved = SavedResult::load(event.field("result")?)?;
                    fields.push(("operation", Json::string(saved.result.operation_id())?));
                    fields.push(("call", Json::string(saved.result.call_id())?));
                    fields.push(("dispatched", flag(saved.dispatched)?));
                    fields.push((
                        "execution",
                        Json::string(match saved.result.execution() {
                            Execution::Succeeded => "succeeded",
                            Execution::Failed => "failed",
                            Execution::Cancelled => "cancelled",
                            Execution::Denied => "denied",
                        })?,
                    ));
                    fields.push((
                        "effects",
                        Json::string(match saved.result.effects() {
                            Effects::NotStarted => "not_started",
                            Effects::KnownResult => "known_result",
                            Effects::Unknown => "effect_unknown",
                        })?,
                    ));
                    if let Some(artifacts) = artifacts {
                        fields.push(("artifacts", artifacts));
                    }
                }
                "pause" => fields.push(("diagnostic", event.field("diagnostic")?.clone())),
                _ => {}
            }
            records.push(Json::object(fields)?);
            Ok(())
        },
    )?;
    let state = state.ok_or("session_missing_init")?;
    let seeded = state.fork_source.is_some();
    let mut fields = vec![
        ("schema_version", num(1)?),
        ("seeded", flag(seeded)?),
        (
            "fresh_initial_context",
            flag(!seeded && state.configuration.input.is_empty())?,
        ),
        ("session", Json::string(&state.session.0)?),
        ("journal_head_sha256", Json::string(&head)?),
        ("initial_manifest", identity(&state.configuration)?),
        (
            "current_context_sha256",
            digest(&Json::object(vec![
                ("id", Json::string(&state.epoch.id)?),
                ("instructions", Json::string(&state.epoch.instructions)?),
                ("exact_state", state.epoch.exact_state.clone()),
            ])?)?,
        ),
        (
            "autocompact_percent",
            num(state.autocompact_percent.into())?,
        ),
        ("input_estimator_version", num(1)?),
        ("records", Json::array(records)?),
    ];
    let unknown = state.unknown_operation().is_some();
    if let Some(turn) = state.turn {
        let status = if turn.intent.is_some() {
            "pending_intent"
        } else if unknown {
            "effect_unknown"
        } else if turn.receipt.state == "paused"
            || matches!(turn.stage.as_str(), "completed" | "cancelled")
        {
            &turn.receipt.state
        } else {
            "interrupted"
        };
        fields.push(("status", Json::string(status)?));
    } else {
        fields.push(("status", Json::string("empty")?));
    }
    Json::object(fields)
}

fn analysis_artifacts(event: &Json, session: &str) -> Result<Option<Json>> {
    let saved = SavedResult::load(event.field("result")?)?;
    if saved.result.execution() != Execution::Succeeded
        || saved.result.effects() != Effects::KnownResult
    {
        return Ok(None);
    }
    let output = Json::parse(saved.result.output())?;
    let descriptors = output.field("artifacts")?.elements()?;
    let mut artifacts = Vec::new();
    for descriptor in descriptors {
        crate::tools::artifacts::validate_descriptor(
            descriptor,
            session,
            Some(saved.result.operation_id()),
        )?;
        artifacts.push(Json::object(vec![
            ("artifact_id", descriptor.field("artifact_id")?.clone()),
            ("revision", descriptor.field("revision")?.clone()),
            ("sha256", descriptor.field("sha256")?.clone()),
            ("size_bytes", descriptor.field("size_bytes")?.clone()),
        ])?);
    }
    Ok(Some(Json::array(artifacts)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_export_rejects_invalid_trial_markers() {
        let trial = Json::object(vec![
            ("proposal", Json::string(&"c".repeat(64)).unwrap()),
            ("arm", Json::string("candidate").unwrap()),
        ])
        .unwrap();
        let root = Json::object(vec![("harness_trial", trial.clone())]).unwrap();
        assert_eq!(harness_trial(&root, true), Ok(Some(trial.clone())));
        assert_eq!(harness_trial(&root, false), Err("evidence_harness_trial"));
        let injected = trial
            .with("body", Json::string("private").unwrap())
            .unwrap();
        let root = root.with("harness_trial", injected).unwrap();
        assert_eq!(harness_trial(&root, true), Err("evidence_harness_trial"));
    }

    #[test]
    fn harness_export_validates_nested_profile_identity() {
        let profile = Json::object(vec![
            ("version", num(1).unwrap()),
            ("scope", Json::string(&sha256(b"/private/path")).unwrap()),
            ("revision", Json::string(&"b".repeat(64)).unwrap()),
        ])
        .unwrap();
        let state = |profile| {
            Json::object(vec![
                ("base_id", Json::string("original").unwrap()),
                ("compaction_generation", num(1).unwrap()),
                ("compaction_digest", Json::string(&"c".repeat(64)).unwrap()),
                (
                    "base",
                    Json::object(vec![
                        ("harness_profile", profile),
                        (
                            "base_instructions_sha256",
                            Json::string(&sha256(b"base instructions")).unwrap(),
                        ),
                        (
                            "settings",
                            Json::parse(r#"{"cwd":"/private/path"}"#).unwrap(),
                        ),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap()
        };
        assert_eq!(
            harness_identity(&state(profile.clone()))
                .unwrap()
                .unwrap()
                .0,
            profile
        );
        let wrapper = state(profile.clone())
            .with("harness_profile", profile.clone())
            .unwrap();
        assert_eq!(harness_identity(&wrapper), Err("evidence_harness_profile"));
        let malformed = profile
            .with("secret", Json::string("private").unwrap())
            .unwrap();
        assert_eq!(
            harness_identity(&state(malformed)),
            Err("evidence_harness_profile")
        );
        let malformed = profile
            .with("scope", Json::string(&"a".repeat(64)).unwrap())
            .unwrap();
        assert_eq!(
            harness_identity(&state(malformed)),
            Err("evidence_harness_profile")
        );
    }
}
