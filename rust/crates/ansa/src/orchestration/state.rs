use serde::{Deserialize, Serialize};
use std::{fs::File, os::unix::fs::MetadataExt};

use super::journal::{Result, encode};
use crate::{
    protocol::legacy_json_v1::sha256,
    provider::contracts::{
        AgentMode, CommandId, OperationId, RequestId, SessionId, Submit, TurnId, valid_id,
    },
};

pub(super) const SCHEMA_VERSION: u8 = 1;

macro_rules! id {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(super) struct $name(pub(super) $inner);
    };
}

id!(RunId, String);
id!(TaskId, String);
id!(AttemptId, u32);
id!(OwnerEpoch, u32);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskKey {
    pub run: RunId,
    pub task: TaskId,
    pub attempt: AttemptId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Address {
    pub task: TaskKey,
    pub epoch: OwnerEpoch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoreId(pub u64, pub u64);

impl StoreId {
    pub(super) fn of(directory: &File) -> Result<Self> {
        let metadata = directory.metadata().map_err(|_| "store_identity")?;
        Ok(Self(metadata.dev(), metadata.ino()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubmitIds {
    pub session: SessionId,
    pub command: CommandId,
    pub turn: TurnId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskSpec {
    pub key: TaskKey,
    pub parent: SessionId,
    pub base_sha: String,
    pub policy_sha256: String,
    pub assignment: String,
    pub native: SubmitIds,
    pub created_at: u64,
    pub deadline: u64,
    pub post_limit: u8,
}

impl TaskSpec {
    pub(super) fn validate(&self) -> Result<()> {
        let ids = [
            &self.key.run.0,
            &self.key.task.0,
            &self.parent.0,
            &self.native.session.0,
            &self.native.command.0,
            &self.native.turn.0,
        ];
        if ids.into_iter().any(|id| !valid_id(id))
            || self.key.attempt.0 != 1
            || self.parent == self.native.session
            || !is_sha(&self.base_sha)
            || !is_digest(&self.policy_sha256)
            || self.assignment.trim().is_empty()
            || self.assignment.len() > 1024
            || self.deadline <= self.created_at
            || !(1..=8).contains(&self.post_limit)
        {
            return Err("task_specification");
        }
        Ok(())
    }

    pub(super) fn digest(&self) -> Result<String> {
        Ok(sha256(&encode(self)?))
    }

    pub(super) fn payload_digest(&self) -> Result<String> {
        submit_digest(&self.submit())
    }

    pub(super) fn submit(&self) -> Submit {
        // O1 reserves one Builder, with a fixed native mode.
        Submit {
            text: self.assignment.clone(),
            mode: AgentMode::Basic,
        }
    }
}

pub(super) fn submit_digest(submit: &Submit) -> Result<String> {
    let mode = match submit.mode {
        AgentMode::Basic => "basic",
        AgentMode::Plan => "plan",
        AgentMode::OrchestrationChat => return Err("orchestration_child_submit_mode"),
    };
    Ok(sha256(&encode(&(mode, &submit.text))?))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChildSessionBinding {
    pub origin: Address,
    pub native: SubmitIds,
    pub spec_sha256: String,
    pub payload_sha256: String,
    pub store: StoreId,
    pub init_nonce: String,
}

impl ChildSessionBinding {
    pub(super) fn matches_spec(&self, spec: &TaskSpec) -> Result<bool> {
        Ok(self.origin.task == spec.key
            && self.native == spec.native
            && self.spec_sha256 == spec.digest()?
            && self.payload_sha256 == spec.payload_digest()?
            && is_digest(&self.init_nonce))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Admission {
    pub binding: ChildSessionBinding,
    pub request: RequestId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Effect {
    Candidate { sha: String },
    KnownFailure,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Outcome {
    pub admission: Admission,
    pub operation: OperationId,
    pub sequence: u64,
    pub effect: Effect,
}

impl Outcome {
    pub(super) fn validate(&self) -> Result<()> {
        if self.sequence != 1 || !valid_id(&self.operation.0) {
            return Err("child_operation_identity");
        }
        if let Effect::Candidate { sha } = &self.effect
            && !is_sha(sha)
        {
            return Err("candidate_sha");
        }
        Ok(())
    }

    pub(super) fn digest(&self) -> Result<String> {
        Ok(sha256(&encode(self)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Gate {
    Mechanical,
    Review,
    Integration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Evidence {
    pub gate: Gate,
    pub candidate_sha: String,
    pub outcome_sha256: String,
    pub artifact_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "event",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum Event {
    Owner(OwnerEpoch),
    Orphan,
    Bind(ChildSessionBinding),
    SubmitIntent,
    Post { at: u64 },
    Expire { at: u64 },
    Admit(Admission),
    Outcome(Outcome),
    Evidence(Evidence),
    Cancel(Address),
}

#[derive(Clone, Debug)]
pub(super) struct State {
    pub spec: TaskSpec,
    pub epoch: OwnerEpoch,
    pub orphan: bool,
    pub binding: Option<ChildSessionBinding>,
    pub intent: bool,
    pub posts: u8,
    pub last_post_at: u64,
    pub expired: bool,
    pub admission: Option<Admission>,
    pub outcome: Option<Outcome>,
    pub evidence: Vec<Evidence>,
    pub cancelled: bool,
}

impl State {
    pub(super) fn new(spec: TaskSpec) -> Result<Self> {
        spec.validate()?;
        Ok(Self {
            last_post_at: spec.created_at,
            spec,
            epoch: OwnerEpoch(1),
            orphan: false,
            binding: None,
            intent: false,
            posts: 0,
            expired: false,
            admission: None,
            outcome: None,
            evidence: Vec::new(),
            cancelled: false,
        })
    }

    pub(super) fn address(&self) -> Address {
        Address {
            task: self.spec.key.clone(),
            epoch: self.epoch.clone(),
        }
    }

    pub(super) fn can_start(&self) -> Result<()> {
        if self.cancelled || self.orphan {
            return Err("task_stopped");
        }
        Ok(())
    }

    pub(super) fn check_budget(&self, at: u64) -> Result<()> {
        if self.expired
            || at < self.last_post_at
            || at >= self.spec.deadline
            || self.posts >= self.spec.post_limit
        {
            return Err("task_budget");
        }
        Ok(())
    }

    pub(super) fn accepted(&self) -> bool {
        !self.cancelled && self.evidence.len() == 3
    }

    /// The same transition checks govern replay and live mutations. Returning
    /// false identifies an exact duplicate that needs no additional record.
    pub(super) fn apply(&mut self, event: &Event) -> Result<bool> {
        match event {
            Event::Owner(epoch) if self.epoch.0.checked_add(1) == Some(epoch.0) => {
                self.epoch = epoch.clone();
            }
            Event::Orphan if self.binding.is_none() && !self.orphan => self.orphan = true,
            Event::Bind(binding) if self.binding.is_none() => {
                self.can_start()?;
                if binding.origin != self.address() || !binding.matches_spec(&self.spec)? {
                    return Err("child_binding");
                }
                self.binding = Some(binding.clone());
            }
            Event::SubmitIntent if self.binding.is_some() && !self.intent => {
                self.can_start()?;
                self.intent = true;
            }
            Event::Post { at } if self.intent && self.admission.is_none() => {
                self.can_start()?;
                self.check_budget(*at)?;
                self.last_post_at = *at;
                self.posts += 1;
            }
            Event::Expire { at } if !self.expired && *at >= self.spec.deadline => {
                self.expired = true;
            }
            Event::Admit(receipt) if self.posts != 0 => {
                if self.binding.as_ref() != Some(&receipt.binding) || !valid_id(&receipt.request.0)
                {
                    return Err("child_admission_identity");
                }
                match &self.admission {
                    Some(previous) if previous == receipt => return Ok(false),
                    Some(_) => return Err("child_admission_conflict"),
                    None => self.admission = Some(receipt.clone()),
                }
            }
            Event::Outcome(outcome) if self.admission.as_ref() == Some(&outcome.admission) => {
                outcome.validate()?;
                match &self.outcome {
                    Some(previous) if previous == outcome => return Ok(false),
                    Some(_) => return Err("child_outcome_conflict"),
                    None => self.outcome = Some(outcome.clone()),
                }
            }
            Event::Evidence(receipt) => {
                self.can_start()?;
                let outcome = self.outcome.as_ref().ok_or("candidate_missing")?;
                if let Effect::Candidate { sha } = &outcome.effect
                    && &receipt.candidate_sha == sha
                    && receipt.outcome_sha256 == outcome.digest()?
                    && is_digest(&receipt.artifact_sha256)
                {
                    if self.evidence.contains(receipt) {
                        return Ok(false);
                    }
                    let order = [Gate::Mechanical, Gate::Review, Gate::Integration];
                    if order.get(self.evidence.len()) == Some(&receipt.gate) {
                        self.evidence.push(receipt.clone());
                        return Ok(true);
                    }
                }
                return Err("candidate_evidence");
            }
            Event::Cancel(address) if address == &self.address() && !self.accepted() => {
                if self.cancelled {
                    return Ok(false);
                }
                self.cancelled = true;
            }
            _ => return Err("task_transition"),
        }
        Ok(true)
    }
}

fn hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && hex(value)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && hex(value)
}
