//! Durable ownership and operation admission for a single terminal scope.
//! A host serializes calls to this coordinator. PTY workers do not own leases
//! and evidence never recreates a shell after a host restart.

pub mod daemon;
mod evidence;
pub mod native;
pub mod protocol;
pub mod tools;
mod wire;
pub(crate) mod workspace;

pub use evidence::{FileJournal, Journal, Record, Tail};
use evidence::{MAX_OPERATIONS, NORMAL_OPERATIONS};
use protocol::{Action, Binding, Effect, Id, Intent, Launch, Writer};
use std::{collections::BTreeMap, path::Path};

pub const MAX_LIVE_TERMINALS: usize = 2;
pub const MAX_TERMINAL_IDS: usize = 8;
pub const MAX_TERMINAL_INPUT: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Reserved,
    Live,
    Closed,
    Lost,
}

#[derive(Clone, Debug)]
pub struct Terminal {
    pub launch: Launch,
    pub lifecycle: Lifecycle,
    pub writer: Writer,
    pub epoch: u64,
    pub visible_after: u64,
    input_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "effect", rename_all = "snake_case")]
pub enum OperationStatus {
    Durable(Effect),
    KnownWithoutDurableReceipt(Effect),
    Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recovery {
    pub verified_records: usize,
    pub tail: Tail,
    pub closure_known: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadPermit {
    binding: Binding,
    terminal: Id,
    epoch: u64,
    writer: Writer,
    after: u64,
}

pub trait Backend {
    /// Invoked once after the host publishes the intent and checks its lease.
    /// Return Unknown for an effect that cannot be proved. Applied on Close
    /// certifies cleanup of all owned processes, not reconciliation of effects.
    fn execute(&mut self, intent: &Intent, input: &[u8]) -> Effect;

    /// Best-effort stop on a failed evidence store or host shutdown. The host
    /// cannot publish a durable cleanup claim from this path.
    fn stop(&mut self);

    fn quarantine(&mut self) {}

    fn receipt_committed(&mut self) {}
}

struct Operation {
    intent: Intent,
    status: OperationStatus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostMode {
    Bounded,
    Conversation,
}

pub struct Host<J: Journal, B: Backend> {
    binding: Binding,
    mode: HostMode,
    journal: J,
    backend: B,
    terminals: BTreeMap<Id, Terminal>,
    operations: BTreeMap<Id, Operation>,
    archived_operations: BTreeMap<Id, Operation>,
    quarantined: bool,
    storage_failed: bool,
    recovery: Option<Recovery>,
    runtime_bound: bool,
}

impl<B: Backend> Host<FileJournal, B> {
    pub fn create_file(path: &Path, binding: Binding, backend: B) -> Result<Self, &'static str> {
        Self::create_in_mode(path, binding, backend, HostMode::Bounded)
    }

    fn create_in_mode(
        path: &Path,
        binding: Binding,
        backend: B,
        mode: HostMode,
    ) -> Result<Self, &'static str> {
        let journal = FileJournal::create(path, &binding, mode)?;
        Self::new_in_mode(binding, journal, backend, mode)
    }

    pub fn restore_file(path: &Path, binding: Binding, backend: B) -> Result<Self, &'static str> {
        Self::restore_in_mode(path, binding, backend, HostMode::Bounded)
    }

    fn restore_in_mode(
        path: &Path,
        binding: Binding,
        backend: B,
        mode: HostMode,
    ) -> Result<Self, &'static str> {
        let restored = FileJournal::open(path, &binding, mode)?;
        Self::restore(
            binding,
            restored.journal,
            backend,
            restored.records,
            restored.tail,
            restored.sealed,
            mode,
        )
    }

    pub(crate) fn seal_clean(&mut self) -> Result<(), &'static str> {
        if self.storage_failed
            || self.quarantined
            || self
                .terminals
                .values()
                .any(|terminal| terminal.lifecycle != Lifecycle::Closed)
        {
            return Err("terminal_cleanup_unknown");
        }
        self.journal.seal()
    }
}

impl<J: Journal, B: Backend> Host<J, B> {
    pub fn new(binding: Binding, journal: J, backend: B) -> Result<Self, &'static str> {
        Self::new_in_mode(binding, journal, backend, HostMode::Bounded)
    }

    fn new_in_mode(
        binding: Binding,
        journal: J,
        backend: B,
        mode: HostMode,
    ) -> Result<Self, &'static str> {
        if !binding.validate() {
            return Err("terminal_binding");
        }
        Ok(Self {
            binding,
            mode,
            journal,
            backend,
            terminals: BTreeMap::new(),
            operations: BTreeMap::new(),
            archived_operations: BTreeMap::new(),
            quarantined: false,
            storage_failed: false,
            recovery: None,
            runtime_bound: false,
        })
    }

    fn restore(
        binding: Binding,
        journal: J,
        backend: B,
        records: Vec<Record>,
        tail: Tail,
        sealed: bool,
        mode: HostMode,
    ) -> Result<Self, &'static str> {
        if mode == HostMode::Bounded && records.len() > MAX_OPERATIONS * 2 {
            return Err("terminal_store_limit");
        }
        let mut host = Self::new_in_mode(binding, journal, backend, mode)?;
        let mut pending = None;
        let mut recovery = Recovery {
            verified_records: 0,
            tail,
            closure_known: false,
        };
        for record in records {
            let valid = match record {
                Record::Intent { value } => {
                    if pending.is_some()
                        || host.operations.contains_key(&value.operation)
                        || host.admit(&value).is_err()
                    {
                        false
                    } else {
                        pending = Some(value.operation.clone());
                        host.accept(*value);
                        true
                    }
                }
                Record::Receipt { operation, effect } => {
                    if pending.as_ref() == Some(&operation)
                        && host.apply_receipt(&operation, effect).is_ok()
                    {
                        pending = None;
                        true
                    } else {
                        false
                    }
                }
            };
            if !valid {
                recovery.tail = Tail::Damaged;
                break;
            }
            recovery.verified_records += 1;
        }
        recovery.closure_known = sealed
            && recovery.tail == Tail::End
            && pending.is_none()
            && !host.quarantined
            && host
                .terminals
                .values()
                .all(|terminal| terminal.lifecycle == Lifecycle::Closed);
        for terminal in host
            .terminals
            .values_mut()
            .filter(|terminal| terminal.lifecycle != Lifecycle::Closed)
        {
            terminal.lifecycle = Lifecycle::Lost;
        }
        // An unpaired final intent is evidence, never a suspended call to replay.
        // Undurable shutdown is still available via Backend::stop.
        host.storage_failed = pending.is_some();
        // A truncated tail can be a valid prefix, so replay cannot prove
        // quiescence. Only the original live host may mutate this incarnation.
        host.quarantined = !recovery.closure_known;
        if host.quarantined {
            host.backend.quarantine();
        }
        host.recovery = Some(recovery);
        Ok(host)
    }

    pub fn terminal_id(&self, operation: &Id) -> Id {
        self.binding.terminal_id(operation)
    }

    pub fn terminals(&self) -> &BTreeMap<Id, Terminal> {
        &self.terminals
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    pub(crate) fn bind_runtime(&mut self) -> Result<(), &'static str> {
        if self.terminals.values().any(|terminal| {
            terminal.writer == Writer::Human && terminal.lifecycle == Lifecycle::Live
        }) {
            return Err("terminal_human_lease");
        }
        self.runtime_bound = true;
        Ok(())
    }

    pub fn quarantined(&self) -> bool {
        self.quarantined
    }

    pub fn recovery(&self) -> Option<Recovery> {
        self.recovery
    }

    pub fn operation(&self, id: &Id) -> Option<OperationStatus> {
        self.operation_record(id).map(|operation| operation.status)
    }

    fn operation_record(&self, id: &Id) -> Option<&Operation> {
        self.operations
            .get(id)
            .or_else(|| self.archived_operations.get(id))
    }

    pub fn matching_receipt(
        &self,
        intent: &Intent,
        input: &[u8],
    ) -> Result<Option<OperationStatus>, &'static str> {
        if intent.binding != self.binding || !intent.binds_payload(input) {
            return Err("terminal_operation_conflict");
        }
        self.operation_record(&intent.operation)
            .map(|prior| {
                (prior.intent == *intent)
                    .then_some(prior.status)
                    .ok_or("terminal_operation_conflict")
            })
            .transpose()
    }

    pub fn execute(
        &mut self,
        writer: Writer,
        intent: Intent,
        input: &[u8],
    ) -> Result<OperationStatus, &'static str> {
        self.execute_if(writer, intent, input, || true)
    }

    pub(crate) fn execute_if(
        &mut self,
        writer: Writer,
        intent: Intent,
        input: &[u8],
        admitted: impl FnOnce() -> bool,
    ) -> Result<OperationStatus, &'static str> {
        if self.runtime_bound && writer == Writer::Human && !matches!(intent.action, Action::Close)
        {
            return Err("terminal_handoff_unavailable");
        }
        if writer != intent.writer || !intent.binds_payload(input) {
            return Err("terminal_payload");
        }
        if intent.binding != self.binding {
            return Err("terminal_operation_invalid");
        }
        if let Some(prior) = self.matching_receipt(&intent, input)? {
            if writer == Writer::Human {
                return Err("terminal_operation_conflict");
            }
            return Ok(prior);
        }
        if self.recovery.is_some() {
            return Err("terminal_recovered_read_only");
        }
        self.admit(&intent)?;
        if self
            .journal
            .publish(&Record::Intent {
                value: Box::new(intent.clone()),
            })
            .is_err()
        {
            self.storage_failure();
            return Err("terminal_intent_not_durable");
        }
        self.accept(intent.clone());
        let outcome = if admitted() {
            self.backend.execute(&intent, input)
        } else {
            Effect::NotStarted
        };
        let effect = if outcome.validates(&intent.action) {
            outcome
        } else {
            Effect::Unknown
        };
        if self
            .journal
            .publish(&Record::Receipt {
                operation: intent.operation.clone(),
                effect,
            })
            .is_err()
        {
            self.operations
                .get_mut(&intent.operation)
                .expect("accepted operation")
                .status = OperationStatus::KnownWithoutDurableReceipt(effect);
            self.storage_failure();
            return Ok(OperationStatus::KnownWithoutDurableReceipt(effect));
        }
        self.apply_receipt(&intent.operation, effect)?;
        self.backend.receipt_committed();
        if self.mode == HostMode::Bounded && self.operations.len() == MAX_OPERATIONS {
            self.emergency_stop();
        }
        Ok(OperationStatus::Durable(effect))
    }

    fn storage_failure(&mut self) {
        self.storage_failed = true;
        self.emergency_stop();
    }

    /// Request immediate best-effort shutdown when normal durable control is
    /// unavailable. Previous receipts and uncertainty retain their meaning.
    pub fn emergency_stop(&mut self) {
        self.quarantined = true;
        self.backend.stop();
        for terminal in self
            .terminals
            .values_mut()
            .filter(|terminal| terminal.lifecycle != Lifecycle::Closed)
        {
            terminal.lifecycle = Lifecycle::Lost;
        }
    }

    pub fn read_permit(
        &self,
        writer: Writer,
        terminal: &Id,
        epoch: u64,
        after: u64,
    ) -> Result<ReadPermit, &'static str> {
        let state = self.terminals.get(terminal).ok_or("terminal_unknown")?;
        if state.lifecycle != Lifecycle::Live
            || state.epoch != epoch
            || (writer == Writer::Agent
                && (state.writer != Writer::Agent || after < state.visible_after))
        {
            return Err("terminal_read_lease");
        }
        Ok(ReadPermit {
            binding: self.binding.clone(),
            terminal: terminal.clone(),
            epoch,
            writer,
            after,
        })
    }

    pub fn recheck_read(&self, permit: &ReadPermit) -> bool {
        permit.binding == self.binding
            && self
                .read_permit(permit.writer, &permit.terminal, permit.epoch, permit.after)
                .is_ok()
    }

    fn admit(&self, intent: &Intent) -> Result<(), &'static str> {
        if intent.binding != self.binding || !intent.validates() {
            return Err("terminal_operation_invalid");
        }
        let emergency_close =
            intent.writer == Writer::Human && matches!(intent.action, Action::Close);
        if self.storage_failed || (self.quarantined && !emergency_close) {
            return Err("terminal_quarantined");
        }
        let limit = if emergency_close {
            MAX_OPERATIONS
        } else {
            NORMAL_OPERATIONS
        };
        let attached = self.mode == HostMode::Conversation;
        if !attached && self.operations.len() >= limit {
            return Err("terminal_operation_budget");
        }
        if let Action::Create { launch } = &intent.action {
            let live_limit = if self.runtime_bound {
                1
            } else {
                MAX_LIVE_TERMINALS
            };
            return if intent.epoch == 0
                && intent.terminal == self.terminal_id(&intent.operation)
                && Path::new(&launch.cwd).starts_with(&self.binding.workspace)
                && !ansa_linux::fs::protected_path(Path::new(&launch.cwd))
                && (!launch.session_lifetime() || intent.writer == Writer::Agent && attached)
                && (attached || self.terminals.len() < MAX_TERMINAL_IDS)
                && self
                    .terminals
                    .values()
                    .filter(|terminal| terminal.lifecycle != Lifecycle::Closed)
                    .count()
                    < live_limit
                && !self.terminals.contains_key(&intent.terminal)
            {
                Ok(())
            } else {
                Err("terminal_create_admission")
            };
        }
        let terminal = self
            .terminals
            .get(&intent.terminal)
            .ok_or("terminal_unknown")?;
        if terminal.epoch != intent.epoch
            || (terminal.lifecycle != Lifecycle::Live
                && !(emergency_close && terminal.lifecycle == Lifecycle::Lost))
        {
            return Err("terminal_lease");
        }
        let allowed = match &intent.action {
            Action::Takeover => intent.writer == Writer::Human && terminal.writer == Writer::Agent,
            Action::Handback { screen } => {
                intent.writer == Writer::Human
                    && terminal.writer == Writer::Human
                    && screen.through >= terminal.visible_after
            }
            Action::Input { count, .. } => {
                intent.writer == terminal.writer
                    && (terminal.launch.session_lifetime()
                        || terminal.input_bytes + count <= MAX_TERMINAL_INPUT)
            }
            _ => intent.writer == terminal.writer || emergency_close,
        };
        if !allowed {
            return Err("terminal_lease");
        }
        Ok(())
    }

    fn accept(&mut self, intent: Intent) {
        match &intent.action {
            Action::Create { launch } => {
                self.terminals.insert(
                    intent.terminal.clone(),
                    Terminal {
                        launch: launch.clone(),
                        lifecycle: Lifecycle::Reserved,
                        writer: intent.writer,
                        epoch: 1,
                        visible_after: 0,
                        input_bytes: 0,
                    },
                );
            }
            Action::Input { count, .. } => {
                self.terminals
                    .get_mut(&intent.terminal)
                    .expect("admitted lease")
                    .input_bytes += count
            }
            _ => {}
        }
        self.operations.insert(
            intent.operation.clone(),
            Operation {
                intent,
                status: OperationStatus::Unresolved,
            },
        );
    }

    fn apply_receipt(&mut self, operation: &Id, effect: Effect) -> Result<(), &'static str> {
        let record = self
            .operations
            .get_mut(operation)
            .ok_or("terminal_store_sequence")?;
        if record.status != OperationStatus::Unresolved || !effect.validates(&record.intent.action)
        {
            return Err("terminal_receipt_invalid");
        }
        let terminal = self
            .terminals
            .get_mut(&record.intent.terminal)
            .ok_or("terminal_store_sequence")?;
        match (&record.intent.action, effect) {
            (Action::Create { .. }, Effect::Applied) => terminal.lifecycle = Lifecycle::Live,
            (Action::Create { .. }, Effect::NotStarted) | (Action::Close, Effect::Applied) => {
                terminal.lifecycle = Lifecycle::Closed
            }
            (
                Action::Create { .. } | Action::Close | Action::Takeover | Action::Handback { .. },
                Effect::Unknown,
            ) => terminal.lifecycle = Lifecycle::Lost,
            (Action::Takeover, Effect::Applied) => {
                terminal.writer = Writer::Human;
                terminal.epoch += 1;
            }
            (Action::Renew, Effect::Applied) => terminal.epoch += 1,
            (Action::Handback { screen }, Effect::Applied) => {
                terminal.writer = Writer::Agent;
                terminal.epoch += 1;
                terminal.visible_after = screen.through;
            }
            _ => {}
        }
        self.quarantined |= effect.uncertain();
        if effect.uncertain() {
            self.backend.quarantine();
        }
        record.status = OperationStatus::Durable(effect);
        if self.mode == HostMode::Conversation && terminal.lifecycle == Lifecycle::Closed {
            self.terminals.remove(&record.intent.terminal);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
