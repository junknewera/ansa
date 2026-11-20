//! The owner of an approved plan and its durable child dispatch decisions.
use super::{
    journal::{Journal, Result},
    plan::{Access, Plan, Progress, Step},
};
use crate::{
    protocol::legacy_json_v1::sha256,
    provider::{
        contracts::valid_id,
        native,
        structured::{ChildResult, ChildStatus},
    },
    runtime::{
        AgentMode, Cancel, ChildAttempt, ChildAttemptState, CommandId, CommandPayload,
        DirectoryIdentity, Handle, PostStatus, RuntimeCommand,
    },
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunId {
    pub parent_session: String,
    pub run: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchBudget {
    pub started_at: u64,
    pub deadline: u64,
    pub submissions: u8,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    identity: RunId,
    plan: String,
    budget: DispatchBudget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<RunContext>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunContext {
    pub prompt: String,
    pub configuration: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    session: String,
    device: u64,
    inode: u64,
    command: String,
    turn: String,
    text_sha256: String,
    plan_mode: bool,
}

impl Binding {
    fn from_receipt(receipt: &ChildAttempt) -> Self {
        let journal = receipt.journal();
        Self {
            session: receipt.session().0.clone(),
            device: journal.device,
            inode: journal.inode,
            command: receipt.command().0.clone(),
            turn: receipt.turn().0.clone(),
            text_sha256: receipt.text_sha256().to_owned(),
            plan_mode: receipt.mode() == AgentMode::Plan,
        }
    }

    fn valid(&self, parent: &str, step: &Step) -> bool {
        valid_id(&self.session)
            && self.session != parent
            && self.inode != 0
            && valid_id(&self.command)
            && valid_id(&self.turn)
            && self.text_sha256 == sha256(step.assignment().text.as_bytes())
            && self.plan_mode == (step.access == Access::Read)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum Outcome {
    Result { digest: String, value: ChildResult },
    Cancelled,
    Unresolved,
}

#[derive(Clone)]
struct Child {
    binding: Binding,
    posts: u8,
    request: Option<String>,
    outcome: Option<Outcome>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Approved {
        digest: String,
    },
    Bound {
        step: String,
        binding: Binding,
    },
    Post {
        step: String,
        at: u64,
    },
    ResumePost {
        step: String,
        request: String,
        at: u64,
    },
    Expired {
        at: u64,
    },
    Admitted {
        step: String,
        request: String,
    },
    Finished {
        step: String,
        request: String,
        outcome: Outcome,
    },
    Cancelled,
    Continued,
}

#[derive(Clone)]
struct State {
    plan: Plan,
    digest: String,
    budget: DispatchBudget,
    last_post: u64,
    posts: u8,
    approved: bool,
    cancelled: bool,
    expired: bool,
    unresolved: bool,
    children: BTreeMap<String, Child>,
}

impl State {
    fn new(header: &Header) -> Result<Self> {
        let budget = header.budget;
        if header.version != 1
            || !valid_id(&header.identity.parent_session)
            || !valid_id(&header.identity.run)
            || !(1..=64).contains(&budget.submissions)
            || budget.deadline <= budget.started_at
            || header
                .context
                .as_ref()
                .is_some_and(|context| context.prompt.is_empty())
        {
            return Err("run_configuration");
        }
        let plan = Plan::parse(&header.plan)?;
        let digest = sha256(plan.encode()?.as_bytes());
        Ok(Self {
            plan,
            digest,
            budget,
            last_post: budget.started_at,
            posts: 0,
            approved: false,
            cancelled: false,
            expired: false,
            unresolved: false,
            children: BTreeMap::new(),
        })
    }

    fn can_dispatch(&self) -> Result<()> {
        if !self.approved || self.cancelled || self.expired || self.unresolved {
            return Err("run_dispatch_unavailable");
        }
        Ok(())
    }

    fn progress(&self) -> BTreeMap<String, Progress> {
        self.children
            .iter()
            .map(|(id, child)| {
                let progress = match &child.outcome {
                    None => Progress::Running,
                    Some(Outcome::Result { value, .. })
                        if value.status == ChildStatus::Completed =>
                    {
                        Progress::Succeeded
                    }
                    Some(_) => Progress::Failed,
                };
                (id.clone(), progress)
            })
            .collect()
    }

    fn apply(&mut self, parent: &str, event: &Event) -> Result<()> {
        match event {
            Event::Approved { digest }
                if !self.approved && !self.cancelled && digest == &self.digest =>
            {
                self.approved = true;
            }
            Event::Bound { step, binding } => {
                self.can_dispatch()?;
                let ready = self.plan.ready(&self.progress())?;
                let task = ready
                    .into_iter()
                    .find(|task| task.id == *step)
                    .ok_or("run_step_unavailable")?;
                if !binding.valid(parent, task)
                    || self.children.values().any(|child| {
                        child.binding.session == binding.session
                            || (child.binding.device, child.binding.inode)
                                == (binding.device, binding.inode)
                    })
                {
                    return Err("run_child_binding");
                }
                self.children.insert(
                    step.clone(),
                    Child {
                        binding: binding.clone(),
                        posts: 0,
                        request: None,
                        outcome: None,
                    },
                );
            }
            Event::Post { step, at } => {
                self.can_dispatch()?;
                if *at < self.last_post
                    || *at >= self.budget.deadline
                    || self.posts >= self.budget.submissions
                {
                    return Err("run_budget");
                }
                let child = self.children.get_mut(step).ok_or("run_child_missing")?;
                if child.request.is_some() || child.outcome.is_some() {
                    return Err("run_child_already_admitted");
                }
                child.posts += 1;
                self.posts += 1;
                self.last_post = *at;
            }
            Event::ResumePost { step, request, at } => {
                self.can_dispatch()?;
                if *at < self.last_post
                    || *at >= self.budget.deadline
                    || self.posts >= self.budget.submissions
                {
                    return Err("run_budget");
                }
                let child = self.children.get_mut(step).ok_or("run_child_missing")?;
                if child.request.as_ref() != Some(request) || child.outcome.is_some() {
                    return Err("run_child_not_resumable");
                }
                child.posts += 1;
                self.posts += 1;
                self.last_post = *at;
            }
            Event::Admitted { step, request } => {
                let child = self.children.get_mut(step).ok_or("run_child_missing")?;
                if child.posts == 0 || child.request.is_some() || !valid_id(request) {
                    return Err("run_child_admission");
                }
                child.request = Some(request.clone());
            }
            Event::Finished {
                step,
                request,
                outcome,
            } => {
                let child = self.children.get_mut(step).ok_or("run_child_missing")?;
                if child.request.as_ref() != Some(request) || child.outcome.is_some() {
                    return Err("run_child_outcome");
                }
                if let Outcome::Result { digest, value } = outcome {
                    if !valid_digest(digest) {
                        return Err("run_child_outcome_digest");
                    }
                    value.validate()?;
                }
                self.unresolved |= *outcome == Outcome::Unresolved;
                child.outcome = Some(outcome.clone());
            }
            Event::Expired { at } if !self.expired && *at >= self.budget.deadline => {
                self.expired = true
            }
            Event::Cancelled if !self.cancelled => self.cancelled = true,
            Event::Continued => self.can_dispatch()?,
            _ => return Err("run_transition"),
        }
        Ok(())
    }
}

/// A live owner has an exclusive journal lease. Reopen does not authorize
/// dispatch until the original child stores are reconciled from native receipts.
pub struct Run {
    header: Header,
    state: State,
    journal: Journal,
    needs_reconciliation: bool,
    storage_failed: bool,
}

impl Run {
    pub fn create(
        path: &Path,
        identity: RunId,
        plan: Plan,
        budget: DispatchBudget,
    ) -> Result<Self> {
        Self::create_with_context(path, identity, plan, budget, None)
    }

    pub(crate) fn create_with_context(
        path: &Path,
        identity: RunId,
        plan: Plan,
        budget: DispatchBudget,
        context: Option<RunContext>,
    ) -> Result<Self> {
        let header = Header {
            version: 1,
            identity,
            plan: plan.encode()?,
            budget,
            context,
        };
        let state = State::new(&header)?;
        let (journal, header) = Journal::create(path, |_| Ok(header))?;
        Ok(Self {
            header,
            state,
            journal,
            needs_reconciliation: false,
            storage_failed: false,
        })
    }

    pub fn open(path: &Path, expected: &RunId) -> Result<Self> {
        let (journal, header, events): (_, Header, Vec<Event>) = Journal::open(path)?;
        if &header.identity != expected {
            return Err("run_identity");
        }
        let mut state = State::new(&header)?;
        for event in events {
            state.apply(&expected.parent_session, &event)?;
        }
        Ok(Self {
            header,
            state,
            journal,
            needs_reconciliation: true,
            storage_failed: false,
        })
    }

    pub fn plan(&self) -> &Plan {
        &self.state.plan
    }

    pub(crate) fn identity(&self) -> &RunId {
        &self.header.identity
    }

    pub(crate) fn context(&self) -> Option<&RunContext> {
        self.header.context.as_ref()
    }

    pub(crate) fn deadline(&self) -> u64 {
        self.state.budget.deadline
    }

    pub(crate) fn may_continue(&self, now: u64) -> bool {
        self.state.can_dispatch().is_ok() && now < self.state.budget.deadline
    }

    pub(crate) fn original_children(
        &self,
    ) -> impl Iterator<Item = (&str, &str, DirectoryIdentity)> {
        self.state.children.iter().map(|(step, child)| {
            let binding = &child.binding;
            (
                step.as_str(),
                binding.session.as_str(),
                DirectoryIdentity {
                    device: binding.device,
                    inode: binding.inode,
                },
            )
        })
    }

    pub fn plan_digest(&self) -> &str {
        &self.state.digest
    }

    pub fn approve(&mut self, displayed_digest: &str) -> Result<()> {
        if self.needs_reconciliation {
            return Err("run_reconciliation_required");
        }
        self.commit(Event::Approved {
            digest: displayed_digest.to_owned(),
        })
    }

    pub fn ready(&self) -> Result<Vec<&Step>> {
        self.can_dispatch()?;
        self.state.plan.ready(&self.state.progress())
    }

    pub fn bind(&mut self, step: &str, child: &ChildAttempt) -> Result<()> {
        self.can_dispatch()?;
        if !matches!(child.state(), ChildAttemptState::Missing) {
            return Err("run_child_not_fresh");
        }
        self.commit(Event::Bound {
            step: step.to_owned(),
            binding: Binding::from_receipt(child),
        })
    }

    /// The owner retains this run across the durable reservation and native
    /// enqueue. Cancellation cannot acquire the run between those decisions.
    pub async fn dispatch(
        &mut self,
        step: &str,
        child: &ChildAttempt,
        handle: &Handle,
        now: u64,
        stop: &Cancel,
    ) -> Result<PostStatus> {
        self.can_dispatch()?;
        self.check_binding(step, child)?;
        if handle.session() != child.session() || handle.incarnation() != child.incarnation() {
            return Err("run_child_binding");
        }
        stop.check()?;
        if now >= self.state.budget.deadline {
            self.commit(Event::Expired { at: now })?;
            return Err("run_budget");
        }
        if !matches!(child.state(), ChildAttemptState::Missing) {
            return Err("run_child_already_admitted");
        }
        self.commit(Event::Post {
            step: step.to_owned(),
            at: now,
        })?;
        if stop.cancelled() {
            self.cancel()?;
            return Err("run_cancelled");
        }
        let assignment = self
            .state
            .plan
            .steps()
            .iter()
            .find(|item| item.id == step)
            .ok_or("run_child_missing")?
            .assignment();
        handle
            .post(RuntimeCommand {
                schema_version: 1,
                session: child.session().clone(),
                incarnation: handle.incarnation().clone(),
                command: child.command().clone(),
                turn: child.turn().clone(),
                payload: CommandPayload::Submit(assignment),
            })
            .await
    }

    pub fn continue_run(&mut self, children: &[(&str, &ChildAttempt)]) -> Result<()> {
        if !self.needs_reconciliation {
            return Err("run_already_open");
        }
        let original = &self.state.children;
        if original.len() != children.len()
            || original
                .keys()
                .any(|id| children.iter().filter(|(step, _)| step == id).count() != 1)
        {
            return Err("run_child_reconciliation");
        }
        for (step, child) in children {
            if !matches!(
                child.state(),
                ChildAttemptState::Missing
                    | ChildAttemptState::Durable {
                        quiescent: true,
                        ..
                    }
            ) {
                return Err("run_child_reconciliation");
            }
            self.observe(step, child)?;
        }
        // An unapproved plan can be shown and approved after reopen.
        if self.state.can_dispatch().is_ok() {
            self.commit(Event::Continued)?;
        }
        self.needs_reconciliation = false;
        Ok(())
    }

    pub(crate) async fn resume_child(
        &mut self,
        step: &str,
        child: &ChildAttempt,
        handle: &Handle,
        now: u64,
        stop: &Cancel,
    ) -> Result<PostStatus> {
        self.can_dispatch()?;
        self.check_binding(step, child)?;
        if handle.session() != child.session() || handle.incarnation() != child.incarnation() {
            return Err("run_child_binding");
        }
        let ChildAttemptState::Durable {
            request,
            resume_required: true,
            recovery_required: false,
            quiescent: true,
            ..
        } = child.state()
        else {
            return Err("run_child_not_resumable");
        };
        stop.check()?;
        self.commit(Event::ResumePost {
            step: step.to_owned(),
            request: request.0.clone(),
            at: now,
        })?;
        if stop.cancelled() {
            self.cancel()?;
            return Err("run_cancelled");
        }
        handle
            .post(RuntimeCommand {
                schema_version: 1,
                session: handle.session().clone(),
                incarnation: handle.incarnation().clone(),
                command: CommandId(format!("resume-assignment-{}", self.state.posts)),
                turn: child.turn().clone(),
                payload: CommandPayload::Resume(request.clone()),
            })
            .await
    }

    pub fn active(&self) -> impl Iterator<Item = &str> {
        self.state
            .children
            .iter()
            .filter(|(_, child)| child.outcome.is_none())
            .map(|(id, _)| id.as_str())
    }

    /// A settled native completion supplies the result to the parent runtime.
    pub fn result(&self, step: &str) -> Option<&ChildResult> {
        if self.needs_reconciliation || self.storage_failed {
            return None;
        }
        self.recorded_result(step)
    }

    /// Archived presentation of the result sealed by this ledger. Dispatch
    /// and continuation must still use `result` after child reconciliation.
    pub(crate) fn recorded_result(&self, step: &str) -> Option<&ChildResult> {
        match self.state.children.get(step)?.outcome.as_ref()? {
            Outcome::Result { value, .. } => Some(value),
            _ => None,
        }
    }

    pub fn observe(&mut self, step: &str, receipt: &ChildAttempt) -> Result<()> {
        let known = self.check_binding(step, receipt)?;
        let ChildAttemptState::Durable {
            request,
            state,
            quiescent,
            recovery_required,
            final_response,
            ..
        } = receipt.state()
        else {
            if known.request.is_some() {
                return Err("run_child_receipt_missing");
            }
            return Ok(());
        };
        let step = step.to_owned();
        let outcome = if *quiescent && *recovery_required {
            Some(Outcome::Unresolved)
        } else if *quiescent && state == "cancelled" {
            Some(Outcome::Cancelled)
        } else if *quiescent && state == "completed" {
            let response = final_response.as_ref().ok_or("run_child_result_missing")?;
            Some(Outcome::Result {
                digest: sha256(native::save(response)?.dump().as_bytes()),
                value: response.child_result()?,
            })
        } else {
            None
        };
        let admitted = known.request.as_deref();
        if let Some(previous) = admitted
            && previous != request.0
        {
            return Err("run_child_request_conflict");
        }
        if let Some(previous) = &known.outcome {
            return if Some(previous) == outcome.as_ref() {
                Ok(())
            } else {
                Err("run_child_outcome_conflict")
            };
        }
        if admitted.is_none() {
            self.commit(Event::Admitted {
                step: step.clone(),
                request: request.0.clone(),
            })?;
        }
        if let Some(outcome) = outcome {
            self.commit(Event::Finished {
                step,
                request: request.0.clone(),
                outcome,
            })?;
        }
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<()> {
        if !self.state.cancelled {
            self.commit(Event::Cancelled)?;
        }
        Ok(())
    }

    fn can_dispatch(&self) -> Result<()> {
        if self.needs_reconciliation || self.storage_failed {
            return Err("run_reconciliation_required");
        }
        self.state.can_dispatch()
    }

    fn check_binding(&self, step: &str, receipt: &ChildAttempt) -> Result<&Child> {
        let child = self.state.children.get(step).ok_or("run_child_missing")?;
        if child.binding != Binding::from_receipt(receipt) {
            return Err("run_child_binding");
        }
        Ok(child)
    }

    fn commit(&mut self, event: Event) -> Result<()> {
        if self.storage_failed {
            return Err("run_store_failed");
        }
        let mut next = self.state.clone();
        next.apply(&self.header.identity.parent_session, &event)?;
        if let Err(error) = self.journal.append(&event) {
            self.storage_failed = true;
            return Err(error);
        }
        self.state = next;
        Ok(())
    }
}

fn valid_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
