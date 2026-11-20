//! Host-side exact-artifact grader for independently captured native trials.
use super::{Frozen, Proposal, Result, bounded, digest_ok, load_manifest};
use crate::{
    config::AgentSettings,
    context::prompt_policy::PromptPolicy,
    harness::Manifest,
    protocol::{json::Json, legacy_json_v1::sha256},
    session::evidence,
    tools::{files::summary_catalog, registry::Snapshot},
};
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{ErrorKind, Read},
    path::Path,
};

const ARTIFACT_BYTES: usize = 1024 * 1024;
const TRIALS: usize = 32;

pub(super) fn revision() -> String {
    sha256(include_bytes!("grading.rs"))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    version: u8,
    repetitions: usize,
    development: BTreeMap<String, Task>,
    holdout: BTreeMap<String, Task>,
    per_trial: Budget,
    total: Budget,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    prompt_sha256: String,
    inputs: BTreeMap<String, String>,
    checks: BTreeMap<String, String>,
}

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    calls: usize,
    tools: usize,
    compactions: usize,
    estimated_input_tokens: usize,
}

impl Budget {
    fn checked_add(self, other: Self) -> Result<Self> {
        let overflow = "harness_trial_usage_overflow";
        Ok(Self {
            calls: self.calls.checked_add(other.calls).ok_or(overflow)?,
            tools: self.tools.checked_add(other.tools).ok_or(overflow)?,
            compactions: self
                .compactions
                .checked_add(other.compactions)
                .ok_or(overflow)?,
            estimated_input_tokens: self
                .estimated_input_tokens
                .checked_add(other.estimated_input_tokens)
                .ok_or(overflow)?,
        })
    }

    fn fits(self, limit: Self) -> bool {
        self.calls <= limit.calls
            && self.tools <= limit.tools
            && self.compactions <= limit.compactions
            && self.estimated_input_tokens <= limit.estimated_input_tokens
    }

    fn valid(self) -> bool {
        self.calls > 0
            && self.estimated_input_tokens > 0
            && self.fits(Self {
                calls: 1_024,
                tools: 8_192,
                compactions: 128,
                estimated_input_tokens: 256_000_000,
            })
    }
}

impl Plan {
    pub(super) fn validate(&self) -> Result<()> {
        bounded(
            self.version == 1
                && (2..=4).contains(&self.repetitions)
                && !self.development.is_empty()
                && !self.holdout.is_empty()
                && self.development.len() + self.holdout.len() <= TRIALS / (self.repetitions * 2)
                && self.per_trial.valid()
                && self.total.valid()
                && self.per_trial.fits(self.total),
            "harness_grader_plan",
        )?;
        let mut prompts = BTreeSet::new();
        for (id, task, _) in self.tasks() {
            if !component(id)
                || !digest_ok(&task.prompt_sha256)
                || !prompts.insert(&task.prompt_sha256)
                || !inventory(&task.inputs)
                || !inventory(&task.checks)
                || self.development.contains_key(id) && self.holdout.contains_key(id)
            {
                return Err("harness_grader_task");
            }
        }
        Ok(())
    }

    fn tasks(&self) -> impl Iterator<Item = (&String, &Task, bool)> {
        self.development
            .iter()
            .map(|(id, task)| (id, task, false))
            .chain(self.holdout.iter().map(|(id, task)| (id, task, true)))
    }
}

fn component(name: &str) -> bool {
    name.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && !ansa_linux::fs::protected_path(Path::new(name))
}

fn inventory(files: &BTreeMap<String, String>) -> bool {
    (1..=8).contains(&files.len())
        && files
            .iter()
            .all(|(path, value)| component(path) && digest_ok(value))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evaluation {
    version: u8,
    scope: String,
    proposal: String,
    baseline: String,
    candidate: String,
    confounded: bool,
    trials: Vec<Pair>,
    met_budget: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    task: String,
    repetition: usize,
    holdout: bool,
    baseline: Trial,
    candidate: Trial,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trial {
    session: String,
    journal_sha256: String,
    route_sha256: String,
    operation_ids: Vec<String>,
    usage: Budget,
    estimates_complete: bool,
    reported_tokens: Option<usize>,
    eligible: bool,
    artifacts: BTreeMap<String, Option<String>>,
    passed: bool,
}

impl Evaluation {
    pub(super) fn validate_identity(
        &self,
        dir: &File,
        scope: &str,
        id: &str,
        proposal: &Proposal,
        frozen: &Frozen,
    ) -> Result<()> {
        bounded(
            self.version == 1
                && self.scope == scope
                && self.proposal == id
                && self.baseline == frozen.preimage
                && self.candidate == proposal.revision
                && self.trials.len()
                    == (frozen.plan.development.len() + frozen.plan.holdout.len())
                        * frozen.plan.repetitions,
            "harness_evaluation_identity",
        )?;
        let baseline = load_manifest(dir, scope, &frozen.preimage)?;
        let candidate = load_manifest(dir, scope, &proposal.revision)?;
        bounded(
            self.confounded == confounded(&baseline, &candidate)?,
            "harness_evaluation_identity",
        )?;
        let expected = frozen.plan.tasks().flat_map(|(id, task, holdout)| {
            (1..=frozen.plan.repetitions).map(move |repetition| (id, task, holdout, repetition))
        });
        let mut sessions = BTreeSet::new();
        let mut heads = BTreeSet::new();
        let mut routes = BTreeSet::new();
        let mut total = Budget::default();
        let mut met_budget = true;
        for (pair, (id, task, holdout, repetition)) in self.trials.iter().zip(expected) {
            bounded(
                pair.task == *id && pair.holdout == holdout && pair.repetition == repetition,
                "harness_evaluation_schedule",
            )?;
            for trial in [&pair.baseline, &pair.candidate] {
                let operations: BTreeSet<_> = trial.operation_ids.iter().collect();
                bounded(
                    crate::provider::contracts::valid_id(&trial.session)
                        && digest_ok(&trial.journal_sha256)
                        && digest_ok(&trial.route_sha256)
                        && sessions.insert(&trial.session)
                        && heads.insert(&trial.journal_sha256)
                        && trial.usage.calls > 0
                        && trial.usage.tools == operations.len()
                        && operations.len() == trial.operation_ids.len()
                        && trial.artifacts.keys().eq(task.checks.keys())
                        && trial
                            .artifacts
                            .values()
                            .all(|digest| digest.as_deref().is_none_or(digest_ok))
                        && trial.passed == checks_passed(&trial.artifacts, task)
                        && (!trial.eligible || trial.estimates_complete),
                    "harness_evaluation_trial",
                )?;
                routes.insert(&trial.route_sha256);
                total = total.checked_add(trial.usage)?;
                met_budget &= trial.estimates_complete && trial.usage.fits(frozen.plan.per_trial);
            }
        }
        bounded(
            routes.len() == 1 && self.met_budget == (met_budget && total.fits(frozen.plan.total)),
            "harness_evaluation_budget",
        )
    }

    pub(super) fn promotable(&self) -> bool {
        !self.confounded
            && self.met_budget
            && self.trials.iter().all(|trial| {
                trial.baseline.eligible
                    && trial.candidate.eligible
                    && (!trial.baseline.passed || trial.candidate.passed)
            })
            && self
                .trials
                .iter()
                .filter(|trial| trial.holdout)
                .any(|first| {
                    self.trials
                        .iter()
                        .filter(|trial| trial.task == first.task)
                        .all(|pair| !pair.baseline.passed && pair.candidate.passed)
                })
    }

    pub(super) fn confounded(&self) -> bool {
        self.confounded
    }
}

pub(super) fn evaluate(
    dir: &File,
    scope: &str,
    id: &str,
    proposal: &Proposal,
    frozen: &Frozen,
    root: &Path,
) -> Result<Evaluation> {
    fs::private_directory(root).map_err(|_| "harness_host_evidence_directory")?;
    let catalog = summary_catalog(&AgentSettings::defaults(root, root))?;
    let baseline = load_manifest(dir, scope, &frozen.preimage)?;
    let candidate = load_manifest(dir, scope, &proposal.revision)?;
    let mut trials = Vec::new();
    let mut seen = BTreeSet::new();
    let mut observed_route = None::<String>;
    let mut total = Budget::default();
    let mut met_budget = true;
    for (task_id, task, holdout) in frozen.plan.tasks() {
        for repetition in 1..=frozen.plan.repetitions {
            let path = root.join(task_id).join(repetition.to_string());
            let mut grade = |arm, revision, policy| {
                let resolved = TrialIdentity {
                    scope,
                    proposal: id,
                    arm,
                    revision,
                    policy,
                };
                let trial = trial(&path.join(arm), resolved, task, &catalog)?;
                bounded(
                    observed_route
                        .as_ref()
                        .is_none_or(|expected| &trial.route_sha256 == expected)
                        && seen.insert(trial.session.clone()),
                    "harness_trial_provenance",
                )?;
                observed_route = Some(trial.route_sha256.clone());
                total = total.checked_add(trial.usage)?;
                met_budget &= trial.estimates_complete && trial.usage.fits(frozen.plan.per_trial);
                Ok::<_, &'static str>(trial)
            };
            trials.push(Pair {
                task: task_id.clone(),
                repetition,
                holdout,
                baseline: grade("baseline", frozen.preimage.as_str(), baseline.prompt_policy)?,
                candidate: grade(
                    "candidate",
                    proposal.revision.as_str(),
                    candidate.prompt_policy,
                )?,
            });
        }
    }
    Ok(Evaluation {
        version: 1,
        scope: scope.into(),
        proposal: id.into(),
        baseline: frozen.preimage.clone(),
        candidate: proposal.revision.clone(),
        confounded: confounded(&baseline, &candidate)?,
        trials,
        met_budget: met_budget && total.fits(frozen.plan.total),
    })
}

fn confounded(baseline: &Manifest, candidate: &Manifest) -> Result<bool> {
    let baseline = serde_json::to_value(baseline).map_err(|_| "harness_manifest")?;
    let candidate = serde_json::to_value(candidate).map_err(|_| "harness_manifest")?;
    let changed = ["guidance", "skills", "memory", "prompt_policy"]
        .into_iter()
        .filter(|axis| baseline.get(axis) != candidate.get(axis))
        .count();
    Ok(changed > 1)
}

struct TrialIdentity<'a> {
    scope: &'a str,
    proposal: &'a str,
    arm: &'a str,
    revision: &'a str,
    policy: PromptPolicy,
}

fn trial(
    root: &Path,
    identity: TrialIdentity<'_>,
    task: &Task,
    catalog: &Snapshot,
) -> Result<Trial> {
    fs::private_directory(root).map_err(|_| "harness_trial_directory")?;
    let export = evidence::export(&root.join("session"), catalog, catalog)?;
    bounded(
        export.field("fresh_initial_context")?.boolean()?,
        "harness_trial_context",
    )?;
    let manifest = export.field("initial_manifest")?;
    let profile = manifest.field("harness_profile")?;
    bounded(
        profile.usize_field("version")? == 1
            && profile.text_field("scope")? == identity.scope
            && manifest.text_field("harness_cwd_sha256")? == identity.scope
            && profile.text_field("revision")? == identity.revision,
        "harness_trial_profile",
    )?;
    let marker = manifest.field("harness_trial")?;
    bounded(
        marker.text_field("proposal")? == identity.proposal
            && marker.text_field("arm")? == identity.arm,
        "harness_trial_binding",
    )?;
    let policy = match identity.policy {
        PromptPolicy::LegacyV1 => "legacy-v1",
        PromptPolicy::HandoffV1 => "handoff-v1",
    };
    bounded(
        manifest.text_field("prompt_policy")? == policy,
        "harness_trial_policy",
    )?;
    let route = [
        "provider",
        "requested_model",
        "returned_model",
        "account_binding_sha256",
        "tool_snapshot_sha256",
        "effort",
        "harness_settings_sha256",
        "base_context_sha256",
    ];
    let route = route
        .into_iter()
        .map(|field| Ok(manifest.field(field)?.clone()))
        .collect::<Result<Vec<_>>>()?;

    let mut eligible = export.text_field("status")? == "completed";
    let mut calls = 0;
    let mut tools = 0;
    let mut compactions = 0;
    let mut estimated = 0usize;
    let mut estimates_complete = true;
    let mut reported = Some(0usize);
    let mut prompt = false;
    let mut operations = Vec::new();
    for record in export.field("records")?.elements()? {
        match record.text_field("kind")? {
            "accepted" => {
                bounded(
                    !prompt
                        && !record.field("internal")?.boolean()?
                        && record.text_field("prompt_sha256")? == task.prompt_sha256,
                    "harness_trial_prompt",
                )?;
                prompt = true;
            }
            kind @ ("response" | "compacted") => {
                calls += 1;
                compactions += usize::from(kind == "compacted");
                if let Ok(tokens) = record.usize_field("estimated_input_tokens") {
                    estimated = estimated
                        .checked_add(tokens)
                        .ok_or("harness_trial_usage_overflow")?;
                } else {
                    estimates_complete = false;
                    eligible = false;
                }
                reported = reported
                    .zip(reported_tokens(record))
                    .and_then(|(a, b)| a.checked_add(b));
                let request = record.field("request_manifest")?;
                for field in [
                    "provider",
                    "requested_model",
                    "returned_model",
                    "account_binding_sha256",
                    "effort",
                    "base_context_sha256",
                    "prompt_policy",
                    "harness_profile",
                    "harness_trial",
                    "harness_settings_sha256",
                    "harness_cwd_sha256",
                ] {
                    eligible &= request.field(field)? == manifest.field(field)?;
                }
            }
            "intent" => {
                tools += 1;
                operations.push(record.text_field("operation")?.to_owned());
            }
            "outcome" => eligible &= record.text_field("effects")? == "known_result",
            "pause" => eligible = false,
            "fork" | "queued" | "steered" | "steer_rejected" => {
                return Err("harness_trial_context");
            }
            _ => {}
        }
    }
    bounded(prompt && calls > 0, "harness_trial_incomplete")?;

    for (name, expected) in &task.inputs {
        bounded(
            file_digest(&root.join("inputs").join(name))?.as_ref() == Some(expected),
            "harness_trial_fixture_changed",
        )?;
    }
    let mut artifacts = BTreeMap::new();
    for name in task.checks.keys() {
        artifacts.insert(
            name.clone(),
            file_digest(&root.join("artifacts").join(name))?,
        );
    }
    let passed = checks_passed(&artifacts, task);
    Ok(Trial {
        session: export.text_field("session")?.into(),
        journal_sha256: export.text_field("journal_head_sha256")?.into(),
        route_sha256: sha256(Json::array(route)?.dump().as_bytes()),
        operation_ids: operations,
        usage: Budget {
            calls,
            tools,
            compactions,
            estimated_input_tokens: estimated,
        },
        estimates_complete,
        reported_tokens: reported,
        eligible,
        artifacts,
        passed,
    })
}

fn reported_tokens(record: &Json) -> Option<usize> {
    record
        .field("reported_usage")
        .ok()
        .and_then(|usage| {
            usage
                .usize_field("input_tokens")
                .ok()?
                .checked_add(usage.usize_field("output_tokens").ok()?)
        })
        .or_else(|| record.usize_field("legacy_reported_total_tokens").ok())
}

fn checks_passed(artifacts: &BTreeMap<String, Option<String>>, task: &Task) -> bool {
    task.checks
        .iter()
        .all(|(name, expected)| artifacts.get(name).and_then(Option::as_ref) == Some(expected))
}

fn file_digest(path: &Path) -> Result<Option<String>> {
    let file = match fs::open_nofollow(path, local::read_flags()) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("harness_trial_file"),
    };
    bounded(
        fs::regular_single(&file).map_err(|_| "harness_trial_file")?,
        "harness_trial_file",
    )?;
    let mut bytes = Vec::new();
    file.take(ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "harness_trial_file")?;
    bounded(bytes.len() <= ARTIFACT_BYTES, "harness_trial_file_limit")?;
    Ok(Some(sha256(&bytes)))
}
