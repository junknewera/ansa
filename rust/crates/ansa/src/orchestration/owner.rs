use ansa_linux::local;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
};

use super::{
    child::Child,
    journal::{Journal, Result},
    state::{
        Address, ChildSessionBinding, Event, Evidence, OwnerEpoch, SCHEMA_VERSION, State, StoreId,
        TaskSpec,
    },
};
use crate::{provider::contracts::Submit, runtime::PostStatus};

const ROOT_STORE: &str = "task-v1";
const CHILD_STORE: &str = "child-v1";
pub(super) const MAILBOX_CAPACITY: usize = 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    repository: StoreId,
    spec: TaskSpec,
}

struct Repository {
    // Keep the original common directory descriptor and the shared lease.
    _common_dir: File,
    _lease: File,
    path: PathBuf,
    id: StoreId,
}

impl Repository {
    fn acquire(registry: &Path, common_dir: &Path) -> Result<Self> {
        let (directory, entry, (device, inode)) =
            local::workspace_records(registry, common_dir).map_err(|_| "repository_identity")?;
        let lease = local::lock(&entry).map_err(|_| "repository_busy")?;
        Ok(Self {
            _common_dir: directory,
            _lease: lease,
            path: registry.join(format!("{device:x}-{inode:x}")),
            id: StoreId(device, inode),
        })
    }

    fn has_child(&self) -> Result<bool> {
        match fs::symlink_metadata(self.path.join(CHILD_STORE)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("child_store_probe"),
        }
    }
}

#[derive(Default)]
struct Control {
    cancel: bool,
    expired: bool,
    closed: bool,
}

/// A bounded evidence queue and separate single, idempotent cancellation slot.
pub(super) struct Handle {
    target: Address,
    normal: SyncSender<Evidence>,
    control: Arc<Mutex<Control>>,
}

impl Handle {
    pub(super) fn cancel(&self, target: &Address) -> PostStatus {
        if target != &self.target {
            return PostStatus::StaleTarget;
        }
        let mut control = self.control.lock().expect("control lock");
        if control.closed {
            return PostStatus::Closed;
        }
        control.cancel = true;
        PostStatus::Accepted
    }

    pub(super) fn evidence(&self, target: &Address, receipt: Evidence) -> PostStatus {
        if target != &self.target {
            return PostStatus::StaleTarget;
        }
        try_send(&self.normal, receipt)
    }
}

pub(super) struct Post {
    pub binding: ChildSessionBinding,
    pub payload: Submit,
    control: Arc<Mutex<Control>>,
}

impl Post {
    /// Only the queue handoff and the synthetic effect use this guard.
    /// Journal I/O stays outside it so a slow sync does not block Cancel.
    pub(super) fn before_cancel<T>(&self, operation: impl FnOnce() -> T) -> Result<T> {
        let control = self.control.lock().expect("control lock");
        if control.cancel || control.expired || control.closed {
            return Err("task_stopped");
        }
        Ok(operation())
    }
}

pub(super) struct Owner {
    repository: Repository,
    pub journal: Journal,
    state: State,
    control: Arc<Mutex<Control>>,
    normal: Receiver<Evidence>,
    child_slot: SyncSender<Post>,
    failed: bool,
    recovery_required: bool,
}

impl Owner {
    pub(super) fn create(
        registry: &Path,
        common_dir: &Path,
        spec: TaskSpec,
    ) -> Result<(Self, Handle, Receiver<Post>)> {
        let state = State::new(spec)?;
        let repository = Repository::acquire(registry, common_dir)?;
        if repository.has_child()? {
            return Err("child_store_already_exists");
        }
        let (journal, _) = Journal::create(&repository.path.join(ROOT_STORE), |_| {
            Ok(Header {
                version: SCHEMA_VERSION,
                repository: repository.id.clone(),
                spec: state.spec.clone(),
            })
        })?;
        Ok(Self::new(repository, journal, state))
    }

    pub(super) fn open(
        registry: &Path,
        common_dir: &Path,
    ) -> Result<(Self, Handle, Receiver<Post>)> {
        let repository = Repository::acquire(registry, common_dir)?;
        let (journal, header, records): (_, Header, Vec<Event>) =
            Journal::open(&repository.path.join(ROOT_STORE))?;
        if header.version != SCHEMA_VERSION || header.repository != repository.id {
            return Err("repository_binding");
        }
        let mut state = State::new(header.spec)?;
        for record in &records {
            if !state.apply(record)? {
                return Err("unexpected_duplicate_record");
            }
        }
        let epoch = OwnerEpoch(state.epoch.0.checked_add(1).ok_or("owner_epoch_limit")?);
        let (mut owner, mut handle, child) = Self::new(repository, journal, state);
        owner.commit(Event::Owner(epoch))?;
        handle.target = owner.address();
        // A crash before Bind can leave an initialized child. A store at that
        // path does not authorize either a fresh initialization or a Submit.
        if owner.state.binding.is_none() && !owner.state.orphan && owner.repository.has_child()? {
            owner.commit(Event::Orphan)?;
        }
        Ok((owner, handle, child))
    }

    fn new(
        repository: Repository,
        journal: Journal,
        state: State,
    ) -> (Self, Handle, Receiver<Post>) {
        let control = Arc::new(Mutex::new(Control {
            expired: state.expired,
            ..Control::default()
        }));
        let recovery_required = state.binding.is_some();
        let (normal_sender, normal) = sync_channel(MAILBOX_CAPACITY);
        let (child_slot, child) = sync_channel(1);
        let handle = Handle {
            target: state.address(),
            normal: normal_sender,
            control: control.clone(),
        };
        (
            Self {
                repository,
                journal,
                state,
                control,
                normal,
                child_slot,
                failed: false,
                recovery_required,
            },
            handle,
            child,
        )
    }

    pub(super) fn address(&self) -> Address {
        self.state.address()
    }

    pub(super) fn state(&self) -> &State {
        &self.state
    }

    pub(super) fn child_path(&self) -> PathBuf {
        self.repository.path.join(CHILD_STORE)
    }

    pub(super) fn root_path(&self) -> PathBuf {
        self.repository.path.join(ROOT_STORE)
    }

    pub(super) fn prepare(&mut self, now: u64) -> Result<Child> {
        self.drain_cancel()?;
        self.state.can_start()?;
        self.check_budget(now)?;
        if self.state.binding.is_some() {
            return Err("child_already_bound");
        }
        let child = Child::create(&self.child_path(), &self.state.spec, self.address())?;
        self.commit(Event::Bind(child.binding.clone()))?;
        Ok(child)
    }

    pub(super) fn open_child(&mut self) -> Result<Child> {
        self.drain_cancel()?;
        let binding = self.state.binding.as_ref().ok_or("child_binding_missing")?;
        let child = Child::open(&self.child_path(), binding)?;
        self.import_receipts(&child)?;
        self.recovery_required = false;
        Ok(child)
    }

    pub(super) fn submit_intent(&mut self, child: &Child, now: u64) -> Result<()> {
        self.drain_cancel()?;
        self.state.can_start()?;
        self.require_reconciled()?;
        self.check_budget(now)?;
        self.verify_child_store()?;
        if self.state.binding.as_ref() != Some(&child.binding) || child.receipts()?.0.is_some() {
            return Err("child_binding");
        }
        self.commit(Event::SubmitIntent)
    }

    pub(super) fn post(&mut self, child: &Child, now: u64) -> Result<PostStatus> {
        self.drain_cancel()?;
        self.state.can_start()?;
        self.reconcile(child)?;
        self.expire_if_due(now)?;
        if self.state.admission.is_some() {
            return Ok(PostStatus::Duplicate);
        }
        self.state.check_budget(now)?;
        self.commit(Event::Post { at: now })?;
        let post = Post {
            binding: child.binding.clone(),
            payload: self.state.spec.submit(),
            control: self.control.clone(),
        };
        // The mutex linearizes the bounded handoff with an incoming Cancel.
        {
            let control = self.control.lock().expect("control lock");
            if !control.cancel && !control.expired && !control.closed {
                return Ok(try_send(&self.child_slot, post));
            }
        }
        self.drain_cancel()?;
        Err("task_stopped")
    }

    pub(super) fn reconcile(&mut self, child: &Child) -> Result<()> {
        self.drain_cancel()?;
        self.require_reconciled()?;
        self.import_receipts(child)
    }

    fn import_receipts(&mut self, child: &Child) -> Result<()> {
        if self.state.binding.as_ref() != Some(&child.binding) {
            return Err("child_binding");
        }
        self.verify_child_store()?;
        let (admission, outcome) = child.receipts()?;
        if self.state.admission.is_some() && admission.is_none()
            || self.state.outcome.is_some() && outcome.is_none()
        {
            return Err("child_receipt_missing");
        }
        if let Some(admission) = admission {
            self.commit(Event::Admit(admission.clone()))?;
        }
        if let Some(outcome) = outcome {
            self.commit(Event::Outcome(outcome.clone()))?;
        }
        Ok(())
    }

    pub(super) fn pump(&mut self) -> Result<()> {
        self.drain_cancel()?;
        if let Ok(receipt) = self.normal.try_recv() {
            self.state.can_start()?;
            self.require_reconciled()?;
            self.verify_child_store()?;
            self.commit(Event::Evidence(receipt))?;
        }
        Ok(())
    }

    fn require_reconciled(&self) -> Result<()> {
        if self.recovery_required {
            return Err("child_recovery_required");
        }
        Ok(())
    }

    fn verify_child_store(&self) -> Result<()> {
        let binding = self.state.binding.as_ref().ok_or("child_binding_missing")?;
        let directory = ansa_linux::fs::private_directory(&self.child_path())
            .map_err(|_| "child_store_missing")?;
        if StoreId::of(&directory)? != binding.store {
            return Err("child_store_identity");
        }
        Ok(())
    }

    fn check_budget(&mut self, now: u64) -> Result<()> {
        self.expire_if_due(now)?;
        self.state.check_budget(now)
    }

    fn expire_if_due(&mut self, now: u64) -> Result<()> {
        if now >= self.state.spec.deadline && !self.state.expired {
            self.control.lock().expect("control lock").expired = true;
            self.commit(Event::Expire { at: now })?;
        }
        Ok(())
    }

    fn drain_cancel(&mut self) -> Result<()> {
        if self.failed {
            return Err("owner_store_failed");
        }
        let cancel = self.control.lock().expect("control lock").cancel;
        if cancel && !self.state.cancelled && !self.state.accepted() {
            self.commit(Event::Cancel(self.address()))?;
        }
        Ok(())
    }

    fn commit(&mut self, event: Event) -> Result<()> {
        if self.failed {
            return Err("owner_store_failed");
        }
        let mut next = self.state.clone();
        if next.apply(&event)? {
            if let Err(error) = self.journal.append(&event) {
                self.failed = true;
                self.control.lock().expect("control lock").closed = true;
                return Err(error);
            }
            self.state = next;
        }
        Ok(())
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.control.lock().expect("control lock").closed = true;
    }
}

fn try_send<T>(sender: &SyncSender<T>, value: T) -> PostStatus {
    match sender.try_send(value) {
        Ok(()) => PostStatus::Accepted,
        Err(TrySendError::Full(_)) => PostStatus::QueueFull,
        Err(TrySendError::Disconnected(_)) => PostStatus::Closed,
    }
}
