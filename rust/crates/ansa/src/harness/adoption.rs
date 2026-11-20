//! Manual profile adoption. Immutable decisions are the active-profile authority.
mod grading;

use super::{Result, digest_ok, load_manifest, store_manifest};
use crate::protocol::legacy_json_v1::sha256;
use ansa_linux::local;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fs::File;

const RECORD_BYTES: usize = 256 * 1024;
const DECISIONS: usize = 128;
const STORE_ENTRIES: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    version: u8,
    scope: String,
    id: String,
    previous: Option<String>,
    revision: String,
    reason: String,
    target: String,
    evidence: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    version: u8,
    scope: String,
    preimage: String,
    activation: String,
    evaluator: String,
    plan: grading::Plan,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    version: u8,
    scope: String,
    freeze: String,
    revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationRef {
    version: u8,
    scope: String,
    proposal: String,
    receipt: String,
    archive: String,
}

pub(super) fn active(dir: &File, scope: &str) -> Result<Option<String>> {
    let history = committed_history(dir, scope)?;
    if let Some(latest) = history.last() {
        load_manifest(dir, scope, &latest.revision)?;
        Ok(Some(latest.revision.clone()))
    } else {
        Ok(None)
    }
}

pub(super) fn trial_revision(dir: &File, scope: &str, id: &str, arm: &str) -> Result<String> {
    let _lock = local::lock_shared(dir).map_err(|_| "harness_adoption_busy")?;
    let history = history(dir, scope)?;
    let (proposal, frozen) = proposal(dir, scope, id)?;
    require_current(history.last(), &frozen)?;
    require_evaluator(&frozen)?;
    match arm {
        "baseline" => Ok(frozen.preimage),
        "candidate" => Ok(proposal.revision),
        _ => Err("harness_trial_arm"),
    }
}

pub(super) fn command(dir: &File, scope: &str, args: &[String], input: &[u8]) -> Result<Value> {
    let Some((verb, parameters)) = args.split_first() else {
        return Err("harness_adoption_arguments");
    };
    bounded(input.len() <= RECORD_BYTES, "harness_adoption_size")?;
    if verb == "status" && parameters.is_empty() && input.is_empty() {
        let history = committed_history(dir, scope)?;
        return Ok(json!({ "active": history.last(), "history": &history }));
    }
    let _lock = local::lock(dir).map_err(|_| "harness_adoption_busy")?;
    let (entries, more) = local::names(dir, STORE_ENTRIES).map_err(|_| "harness_adoption_store")?;
    let reservation = match verb.as_str() {
        "baseline" | "stage" => 34,
        "evaluate" => 2,
        _ => 1,
    };
    bounded(
        !more && entries.len() + reservation <= STORE_ENTRIES,
        "harness_adoption_quota",
    )?;
    let history = history(dir, scope)?;
    sync_history(dir, &history)?;
    let current = history.last();

    match (verb.as_str(), parameters) {
        ("baseline", []) => {
            let revision = store_manifest(dir, scope, input)?;
            let id = transition_id(scope, "baseline", &revision, None);
            if let Some(existing) = history.iter().find(|record| record.id == id) {
                return Ok(json!({"activation": existing}));
            }
            bounded(current.is_none(), "harness_baseline_already_set")?;
            let record = activate(scope, None, revision.clone(), "baseline", revision, None);
            append(dir, &history, record)
        }
        ("freeze", [preimage]) => {
            let current = current.ok_or("harness_baseline_required")?;
            bounded(current.revision == *preimage, "harness_stale_preimage")?;
            crate::protocol::json::Json::parse_with_limits(
                std::str::from_utf8(input).map_err(|_| "harness_adoption_json")?,
                RECORD_BYTES,
                16,
                8192,
            )?;
            let plan: grading::Plan = decode(input)?;
            plan.validate()?;
            let frozen = Frozen {
                version: 1,
                scope: scope.into(),
                preimage: preimage.clone(),
                activation: current.id.clone(),
                evaluator: grading::revision(),
                plan,
            };
            let id = store_addressed(dir, "freeze", &frozen)?;
            Ok(json!({"freeze": id, "preimage": preimage}))
        }
        ("stage", [freeze]) => {
            let frozen = frozen(dir, scope, freeze)?;
            require_current(current, &frozen)?;
            let revision = store_manifest(dir, scope, input)?;
            bounded(revision != frozen.preimage, "harness_no_profile_change")?;
            let staged = Proposal {
                version: 1,
                scope: scope.into(),
                freeze: freeze.clone(),
                revision: revision.clone(),
            };
            save_once(dir, &record_name("proposal", freeze)?, &encode(&staged)?)?;
            Ok(json!({"proposal": freeze, "candidate": revision}))
        }
        ("evaluate", [id, path]) if input.is_empty() => {
            let (proposal, frozen) = proposal(dir, scope, id)?;
            let name = record_name("evaluation", id)?;
            if let Some((reference, receipt)) = evaluation(dir, scope, id, &proposal, &frozen)? {
                local::sync_regular(dir, &record_name("graded", &reference.receipt)?)
                    .and_then(|()| local::sync_regular(dir, &name))
                    .and_then(|()| dir.sync_all())
                    .map_err(|_| "harness_adoption_publish")?;
                return Ok(evaluation_result(receipt));
            }
            require_current(current, &frozen)?;
            require_evaluator(&frozen)?;
            let receipt = grading::evaluate(dir, scope, id, &proposal, &frozen, path.as_ref())?;
            receipt.validate_identity(dir, scope, id, &proposal, &frozen)?;
            let digest = store_addressed(dir, "graded", &receipt)?;
            save_once(
                dir,
                &name,
                &encode(&EvaluationRef {
                    version: 1,
                    scope: scope.into(),
                    proposal: id.clone(),
                    receipt: digest,
                    archive: path.clone(),
                })?,
            )?;
            Ok(evaluation_result(receipt))
        }
        ("adopt", [id]) if input.is_empty() => {
            let (proposal, frozen) = proposal(dir, scope, id)?;
            let transition = transition_id(scope, "adopt", id, Some(&frozen.activation));
            if let Some(existing) = history.iter().find(|record| record.id == transition) {
                return Ok(json!({"activation": existing}));
            }
            require_current(current, &frozen)?;
            require_evaluator(&frozen)?;
            let (reference, receipt) = evaluation(dir, scope, id, &proposal, &frozen)?
                .ok_or("harness_evaluation_required")?;
            if !receipt.promotable() {
                return Ok(
                    json!({"status": "NO_ENTRY", "proposal": id, "confounded": receipt.confounded()}),
                );
            }
            let replay = grading::evaluate(
                dir,
                scope,
                id,
                &proposal,
                &frozen,
                reference.archive.as_ref(),
            )?;
            bounded(
                sha256(&encode(&replay)?) == reference.receipt,
                "harness_evaluation_changed",
            )?;
            append(
                dir,
                &history,
                activate(
                    scope,
                    current,
                    proposal.revision,
                    "adopt",
                    id.clone(),
                    Some(reference.receipt),
                ),
            )
        }
        ("rollback", [expected, target]) if input.is_empty() => {
            let target = history
                .iter()
                .find(|record| record.id == *target)
                .ok_or("harness_rollback_target")?;
            let id = transition_id(scope, "rollback", &target.id, Some(expected));
            if let Some(existing) = history.iter().find(|record| record.id == id) {
                return Ok(json!({"activation": existing}));
            }
            let current = current.ok_or("harness_baseline_required")?;
            bounded(
                current.id == *expected && current.id != target.id,
                "harness_stale_preimage",
            )?;
            load_manifest(dir, scope, &target.revision)?;
            append(
                dir,
                &history,
                activate(
                    scope,
                    Some(current),
                    target.revision.clone(),
                    "rollback",
                    target.id.clone(),
                    Some(target.id.clone()),
                ),
            )
        }
        _ => Err("harness_adoption_arguments"),
    }
}

fn evaluation_result(receipt: grading::Evaluation) -> Value {
    let status = if receipt.promotable() {
        "eligible"
    } else {
        "NO_ENTRY"
    };
    json!({"status": status, "evaluation": receipt})
}

fn committed_history(dir: &File, scope: &str) -> Result<Vec<Activation>> {
    let _lock = local::lock_shared(dir).map_err(|_| "harness_adoption_busy")?;
    let history = history(dir, scope)?;
    sync_history(dir, &history)?;
    Ok(history)
}

fn sync_history(dir: &File, history: &[Activation]) -> Result<()> {
    if let Some(index) = history.len().checked_sub(1) {
        local::sync_regular(dir, &format!("activation-{index:04}.json"))
            .and_then(|()| dir.sync_all())
            .map_err(|_| "harness_adoption_publish")?;
    }
    Ok(())
}

fn history(dir: &File, scope: &str) -> Result<Vec<Activation>> {
    let (names, more) = local::names(dir, STORE_ENTRIES).map_err(|_| "harness_adoption_store")?;
    bounded(!more, "harness_adoption_quota")?;
    let decisions = names
        .iter()
        .filter_map(|name| {
            name.to_str()
                .and_then(|name| name.strip_prefix("activation-"))
                .and_then(|name| name.strip_suffix(".json"))
        })
        .count();
    bounded(decisions <= DECISIONS, "harness_activation_quota")?;
    let mut history = Vec::<Activation>::new();
    for sequence in 0..decisions {
        let record = get::<Activation>(dir, &format!("activation-{sequence:04}.json"))?
            .ok_or("harness_activation_identity")?;
        bounded(
            record.version == 1
                && record.scope == scope
                && digest_ok(&record.revision)
                && digest_ok(&record.target)
                && record.id
                    == transition_id(
                        scope,
                        &record.reason,
                        &record.target,
                        record.previous.as_deref(),
                    )
                && record.previous.as_ref() == history.last().map(|previous| &previous.id)
                && ((sequence == 0
                    && record.reason == "baseline"
                    && record.evidence.is_none()
                    && record.target == record.revision)
                    || (sequence != 0
                        && matches!(record.reason.as_str(), "adopt" | "rollback")
                        && record.evidence.as_deref().is_some_and(digest_ok)))
                && history.iter().all(|prior| prior.id != record.id),
            "harness_activation_identity",
        )?;
        match record.reason.as_str() {
            "adopt" => {
                let current = history.last();
                let (proposal, frozen) = proposal(dir, scope, &record.target)?;
                require_current(current, &frozen)?;
                let (reference, receipt) =
                    evaluation(dir, scope, &record.target, &proposal, &frozen)?
                        .ok_or("harness_evaluation_required")?;
                bounded(
                    record.revision == proposal.revision
                        && record.evidence.as_ref() == Some(&reference.receipt)
                        && receipt.promotable(),
                    "harness_activation_identity",
                )?;
            }
            "rollback" => {
                let target = history
                    .iter()
                    .find(|prior| prior.id == record.target)
                    .ok_or("harness_rollback_target")?;
                bounded(
                    record.previous.as_ref() != Some(&target.id)
                        && record.revision == target.revision
                        && record.evidence.as_ref() == Some(&target.id),
                    "harness_activation_identity",
                )?;
            }
            _ => {}
        }
        history.push(record);
    }
    Ok(history)
}

fn transition_id(scope: &str, reason: &str, target: &str, previous: Option<&str>) -> String {
    sha256(
        format!(
            "ansa-harness-activation-v1/{scope}/{reason}/{}/{target}",
            previous.unwrap_or_default()
        )
        .as_bytes(),
    )
}

fn activate(
    scope: &str,
    current: Option<&Activation>,
    revision: String,
    reason: &str,
    target: String,
    evidence: Option<String>,
) -> Activation {
    let previous = current.map(|current| current.id.clone());
    Activation {
        version: 1,
        scope: scope.into(),
        id: transition_id(scope, reason, &target, previous.as_deref()),
        previous,
        revision,
        reason: reason.into(),
        target,
        evidence,
    }
}

fn append(dir: &File, history: &[Activation], record: Activation) -> Result<Value> {
    bounded(history.len() < DECISIONS, "harness_activation_quota")?;
    save_once(
        dir,
        &format!("activation-{:04}.json", history.len()),
        &encode(&record)?,
    )?;
    Ok(json!({"activation": record}))
}

fn require_current(current: Option<&Activation>, frozen: &Frozen) -> Result<()> {
    bounded(
        current.is_some_and(|current| {
            current.id == frozen.activation && current.revision == frozen.preimage
        }),
        "harness_stale_preimage",
    )
}

fn require_evaluator(frozen: &Frozen) -> Result<()> {
    bounded(
        frozen.evaluator == grading::revision(),
        "harness_evaluator_changed",
    )
}

fn frozen(dir: &File, scope: &str, id: &str) -> Result<Frozen> {
    let record: Frozen = get_addressed(dir, "freeze", id)?;
    bounded(
        record.version == 1
            && record.scope == scope
            && digest_ok(&record.preimage)
            && digest_ok(&record.activation)
            && digest_ok(&record.evaluator),
        "harness_freeze_identity",
    )?;
    record.plan.validate()?;
    Ok(record)
}

fn proposal(dir: &File, scope: &str, id: &str) -> Result<(Proposal, Frozen)> {
    let record: Proposal =
        get(dir, &record_name("proposal", id)?)?.ok_or("harness_adoption_missing")?;
    bounded(
        record.version == 1
            && record.scope == scope
            && record.freeze == id
            && digest_ok(&record.revision),
        "harness_proposal_identity",
    )?;
    let frozen = frozen(dir, scope, &record.freeze)?;
    load_manifest(dir, scope, &record.revision)?;
    Ok((record, frozen))
}

fn evaluation(
    dir: &File,
    scope: &str,
    id: &str,
    proposal: &Proposal,
    frozen: &Frozen,
) -> Result<Option<(EvaluationRef, grading::Evaluation)>> {
    let Some(reference) = get::<EvaluationRef>(dir, &record_name("evaluation", id)?)? else {
        return Ok(None);
    };
    bounded(
        reference.version == 1
            && reference.scope == scope
            && reference.proposal == id
            && std::path::Path::new(&reference.archive).is_absolute(),
        "harness_evaluation_identity",
    )?;
    let receipt: grading::Evaluation = get_addressed(dir, "graded", &reference.receipt)?;
    receipt.validate_identity(dir, scope, id, proposal, frozen)?;
    Ok(Some((reference, receipt)))
}

fn record_name(kind: &str, id: &str) -> Result<String> {
    bounded(digest_ok(id), "harness_adoption_id")?;
    Ok(format!("{kind}-{id}.json"))
}

fn store_addressed(dir: &File, kind: &str, value: &impl Serialize) -> Result<String> {
    let bytes = encode(value)?;
    let id = sha256(&bytes);
    save_once(dir, &record_name(kind, &id)?, &bytes)?;
    Ok(id)
}

fn get_addressed<T: DeserializeOwned>(dir: &File, kind: &str, id: &str) -> Result<T> {
    let name = record_name(kind, id)?;
    let bytes = local::read(dir, &name, RECORD_BYTES)
        .map_err(|_| "harness_adoption_store")?
        .ok_or("harness_adoption_missing")?;
    bounded(sha256(&bytes) == id, "harness_adoption_changed")?;
    decode(&bytes)
}

fn get<T: DeserializeOwned>(dir: &File, name: &str) -> Result<Option<T>> {
    local::read(dir, name, RECORD_BYTES)
        .map_err(|_| "harness_adoption_store")?
        .map(|bytes| decode(&bytes))
        .transpose()
}

fn save_once(dir: &File, name: &str, bytes: &[u8]) -> Result<()> {
    bounded(
        !bytes.is_empty() && bytes.len() <= RECORD_BYTES,
        "harness_adoption_size",
    )?;
    local::publish_recovered(dir, name, bytes).map_err(|_| "harness_adoption_publish")
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| "harness_adoption_json")
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| "harness_adoption_json")
}

fn bounded(valid: bool, error: &'static str) -> Result<()> {
    if valid { Ok(()) } else { Err(error) }
}

#[cfg(test)]
mod tests;
