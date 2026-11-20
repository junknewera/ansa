//! Scripted native lifecycle for O1. The effect counter lives outside both
//! journals; reopening a fixture must not reset the test oracle.
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::{
    journal::{Journal, Result},
    owner::Post,
    state::{
        Address, Admission, ChildSessionBinding, Effect, Outcome, SCHEMA_VERSION, StoreId,
        TaskSpec, submit_digest,
    },
};
use crate::{
    protocol::legacy_json_v1::sha256,
    provider::contracts::{OperationId, RequestId, valid_id},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    child: ChildSessionBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "event",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Record {
    Admitted(Admission),
    EffectIntent(OperationId),
    Finished(Outcome),
}

pub(super) struct Child {
    pub journal: Journal,
    pub binding: ChildSessionBinding,
    admission: Option<Admission>,
    operation: Option<OperationId>,
    outcome: Option<Outcome>,
    applied: bool,
    failed: bool,
}

impl Child {
    pub(super) fn create(path: &Path, spec: &TaskSpec, origin: Address) -> Result<Self> {
        let (journal, header) = Journal::create(path, |directory| {
            let mut nonce = [0; 32];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| "child_nonce")?;
            Ok(Header {
                version: SCHEMA_VERSION,
                child: ChildSessionBinding {
                    origin,
                    native: spec.native.clone(),
                    spec_sha256: spec.digest()?,
                    payload_sha256: spec.payload_digest()?,
                    store: StoreId::of(directory)?,
                    init_nonce: sha256(&nonce),
                },
            })
        })?;
        Ok(Self::new(journal, header.child))
    }

    pub(super) fn open(path: &Path, expected: &ChildSessionBinding) -> Result<Self> {
        let (journal, header, records): (_, Header, Vec<Record>) = Journal::open(path)?;
        if header.version != SCHEMA_VERSION
            || &header.child != expected
            || StoreId::of(journal.directory())? != expected.store
        {
            return Err("child_store_identity");
        }
        let mut child = Self::new(journal, header.child);
        for record in records {
            child.apply(record)?;
        }
        if child.operation.is_some() && child.outcome.is_none() {
            // Reacquiring the synthetic store lease proves the previous fixture
            // is gone. Its side-effect boundary is still uncertain.
            child.finish(Effect::Unknown)?;
        }
        Ok(child)
    }

    fn new(journal: Journal, binding: ChildSessionBinding) -> Self {
        Self {
            journal,
            binding,
            admission: None,
            operation: None,
            outcome: None,
            applied: false,
            failed: false,
        }
    }

    pub(super) fn receipts(&self) -> Result<(Option<&Admission>, Option<&Outcome>)> {
        if self.failed {
            return Err("child_store_failed");
        }
        Ok((self.admission.as_ref(), self.outcome.as_ref()))
    }

    /// Queue acceptance is deliberately distinct from durable admission.
    pub(super) fn admit(&mut self, post: &Post) -> Result<Admission> {
        self.receipts()?;
        self.check_post(post)?;
        if let Some(admission) = &self.admission {
            return Ok(admission.clone());
        }
        post.before_cancel(|| ())?;
        let admission = Admission {
            binding: self.binding.clone(),
            // Fixture intentionally reuses this request ID in each session.
            request: RequestId::from("r_1"),
        };
        self.append(Record::Admitted(admission.clone()))?;
        Ok(admission)
    }

    /// A scripted barrier separates intent, effect and durable outcome.
    pub(super) fn effect(&mut self, post: &Post, counter: &AtomicUsize) -> Result<()> {
        self.check_post(post)?;
        self.receipts()?;
        if self.admission.is_none() || self.operation.is_some() {
            return Err("child_effect_already_reserved_or_not_admitted");
        }
        post.before_cancel(|| ())?;
        self.append(Record::EffectIntent(OperationId::from("o_1")))?;
        post.before_cancel(|| counter.fetch_add(1, Ordering::SeqCst))?;
        self.applied = true;
        Ok(())
    }

    pub(super) fn finish(&mut self, effect: Effect) -> Result<Outcome> {
        if !self.applied && effect != Effect::Unknown {
            return Err("child_effect_unobserved");
        }
        let outcome = Outcome {
            admission: self.admission.clone().ok_or("child_admission_missing")?,
            operation: self.operation.clone().ok_or("child_operation_missing")?,
            sequence: 1,
            effect,
        };
        outcome.validate()?;
        if self.outcome.is_some() {
            return Err("child_outcome_conflict");
        }
        self.append(Record::Finished(outcome.clone()))?;
        Ok(outcome)
    }

    fn check_post(&self, post: &Post) -> Result<()> {
        if post.binding != self.binding
            || submit_digest(&post.payload)? != self.binding.payload_sha256
        {
            return Err("child_submit_conflict");
        }
        Ok(())
    }

    fn append(&mut self, record: Record) -> Result<()> {
        self.receipts()?;
        self.failed = true;
        self.journal.append(&record)?;
        self.apply(record)?;
        self.failed = false;
        Ok(())
    }

    fn apply(&mut self, record: Record) -> Result<()> {
        match record {
            Record::Admitted(receipt)
                if self.admission.is_none()
                    && receipt.binding == self.binding
                    && valid_id(&receipt.request.0) =>
            {
                self.admission = Some(receipt);
            }
            Record::EffectIntent(operation)
                if self.admission.is_some()
                    && self.operation.is_none()
                    && valid_id(&operation.0) =>
            {
                self.operation = Some(operation);
            }
            Record::Finished(outcome)
                if self.outcome.is_none()
                    && self.admission.as_ref() == Some(&outcome.admission)
                    && self.operation.as_ref() == Some(&outcome.operation) =>
            {
                outcome.validate()?;
                self.outcome = Some(outcome);
            }
            _ => return Err("child_record_transition"),
        }
        Ok(())
    }
}
