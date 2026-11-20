use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::Receiver,
    },
};

use ansa_linux::fs as local_fs;

use super::{
    child::Child,
    journal::Fault,
    owner::{Handle, MAILBOX_CAPACITY, Owner, Post},
    state::{
        AttemptId, Effect, Event, Evidence, Gate, OwnerEpoch, RunId, State, SubmitIds, TaskId,
        TaskKey, TaskSpec,
    },
};
use crate::{
    protocol::legacy_json_v1::sha256,
    provider::contracts::{CommandId, RequestId, SessionId, TurnId},
    runtime::PostStatus,
};

struct Fixture {
    root: PathBuf,
    effects: AtomicUsize,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "ansa-o1-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        local_fs::create_private_leaf(&root).unwrap();
        local_fs::create_private_leaf(&root.join("common")).unwrap();
        Self {
            root,
            effects: AtomicUsize::new(0),
        }
    }

    fn repository(&self) -> PathBuf {
        self.root.join("common")
    }

    fn registry(&self) -> PathBuf {
        self.root.join("registry")
    }

    fn create(&self) -> (Owner, Handle, Receiver<Post>) {
        Owner::create(&self.registry(), &self.repository(), spec()).unwrap()
    }

    fn open(&self) -> (Owner, Handle, Receiver<Post>) {
        Owner::open(&self.registry(), &self.repository()).unwrap()
    }

    fn effect_count(&self) -> usize {
        self.effects.load(Ordering::SeqCst)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn spec() -> TaskSpec {
    TaskSpec {
        key: TaskKey {
            run: RunId("run-one".into()),
            task: TaskId("task-one".into()),
            attempt: AttemptId(1),
        },
        parent: SessionId::from("s_parent"),
        base_sha: "a".repeat(40),
        policy_sha256: sha256(b"fixture-only policy"),
        assignment: "Add a regression for cancelling a pending retry.".into(),
        native: SubmitIds {
            session: SessionId::from("s_builder"),
            command: CommandId::from("c_1"),
            turn: TurnId::from("t_1"),
        },
        created_at: 10,
        deadline: 30,
        post_limit: 2,
    }
}

fn candidate() -> Effect {
    Effect::Candidate {
        sha: "b".repeat(40),
    }
}

fn submit(owner: &mut Owner, queue: &Receiver<Post>) -> (Child, Post) {
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    assert_eq!(owner.post(&child, 12), Ok(PostStatus::Accepted));
    (child, queue.try_recv().unwrap())
}

fn evidence(state: &State, gate: Gate) -> Evidence {
    Evidence {
        gate,
        candidate_sha: "b".repeat(40),
        outcome_sha256: state.outcome.as_ref().unwrap().digest().unwrap(),
        artifact_sha256: sha256(format!("{gate:?} fixture").as_bytes()),
    }
}

fn record_count(path: &Path) -> usize {
    fs::read_dir(path)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b"event-")
        })
        .count()
}

#[test]
fn admission_and_each_acceptance_gate_survive_reopen() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    owner.reconcile(&child).unwrap();
    assert!(
        owner.state().admission.is_none(),
        "queue acceptance is not a receipt"
    );
    let receipt = child.admit(&post).unwrap();
    owner.reconcile(&child).unwrap();
    let count = record_count(&owner.root_path());
    owner.reconcile(&child).unwrap();
    assert_eq!(record_count(&owner.root_path()), count);
    assert_eq!(owner.state().admission.as_ref(), Some(&receipt));
    child.effect(&post, &f.effects).unwrap();
    let outcome = child.finish(candidate()).unwrap();
    owner.reconcile(&child).unwrap();
    assert!(!owner.state().accepted());
    drop((owner, child));

    for (index, gate) in [Gate::Mechanical, Gate::Review, Gate::Integration]
        .into_iter()
        .enumerate()
    {
        let (mut owner, handle, queue) = f.open();
        let child = owner.open_child().unwrap();
        owner.reconcile(&child).unwrap();
        assert_eq!(owner.state().outcome.as_ref(), Some(&outcome));
        assert_eq!(owner.state().evidence.len(), index);
        assert!(!owner.state().accepted());
        assert!(queue.try_recv().is_err());
        if index == 0 {
            assert_eq!(
                handle.evidence(&owner.address(), evidence(owner.state(), Gate::Review)),
                PostStatus::Accepted
            );
            assert_eq!(owner.pump(), Err("candidate_evidence"));
            let mut wrong_candidate = evidence(owner.state(), gate);
            wrong_candidate.candidate_sha = "c".repeat(40);
            assert_eq!(
                handle.evidence(&owner.address(), wrong_candidate),
                PostStatus::Accepted
            );
            assert_eq!(owner.pump(), Err("candidate_evidence"));
            let mut wrong_outcome = evidence(owner.state(), gate);
            wrong_outcome.outcome_sha256 = sha256(b"different receipt");
            assert_eq!(
                handle.evidence(&owner.address(), wrong_outcome),
                PostStatus::Accepted
            );
            assert_eq!(owner.pump(), Err("candidate_evidence"));
        }
        assert_eq!(
            handle.evidence(&owner.address(), evidence(owner.state(), gate)),
            PostStatus::Accepted
        );
        owner.pump().unwrap();
    }
    let (owner, _, _) = f.open();
    assert!(owner.state().accepted());
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn lost_ack_recovers_original_receipts_without_reposting() {
    let f = Fixture::new();
    let (mut owner, handle, queue) = f.create();
    let previous = owner.address();
    let (mut child, post) = submit(&mut owner, &queue);
    let receipt = child.admit(&post).unwrap();
    child.effect(&post, &f.effects).unwrap();
    let outcome = child.finish(candidate()).unwrap();
    drop((owner, child));

    let (mut owner, fresh_handle, queue) = f.open();
    let child = owner.open_child().unwrap();
    assert_eq!(handle.cancel(&previous), PostStatus::Closed);
    assert_eq!(fresh_handle.cancel(&previous), PostStatus::StaleTarget);
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Duplicate));
    assert_eq!(owner.state().posts, 1);
    assert_eq!(owner.state().admission.as_ref(), Some(&receipt));
    assert_eq!(owner.state().outcome.as_ref(), Some(&outcome));
    assert_eq!(owner.address().epoch, OwnerEpoch(2));
    assert_eq!(outcome.admission.binding.origin, previous);
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn duplicate_submit_binds_exact_payload_and_runs_one_effect() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, first) = submit(&mut owner, &queue);
    // A drained queue without a durable receipt can be redelivered to the
    // same store. The fake deduplicates even if both deliveries arrive.
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Accepted));
    let mut second = queue.try_recv().unwrap();
    let exact_payload = second.payload.clone();
    second.payload.text.push_str(" Check a second file.");
    assert_eq!(child.admit(&second), Err("child_submit_conflict"));
    let admission = child.admit(&first).unwrap();
    assert_eq!(child.admit(&second), Err("child_submit_conflict"));
    second.payload = exact_payload;
    assert_eq!(child.admit(&second), Ok(admission));
    child.effect(&first, &f.effects).unwrap();
    assert_eq!(
        child.effect(&second, &f.effects),
        Err("child_effect_already_reserved_or_not_admitted")
    );
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn reducer_rejects_stale_and_cross_session_identities_even_with_same_request_id() {
    let a = Fixture::new();
    let b = Fixture::new();
    let (mut owner, _, queue) = a.create();
    let (mut child, post) = submit(&mut owner, &queue);
    let mut other_spec = spec();
    other_spec.native.session = SessionId::from("s_other");
    let (mut other_owner, _, other_queue) =
        Owner::create(&b.registry(), &b.repository(), other_spec).unwrap();
    let (mut other_child, other_post) = submit(&mut other_owner, &other_queue);
    let receipt = child.admit(&post).unwrap();
    let other_receipt = other_child.admit(&other_post).unwrap();
    assert_eq!(receipt.request, other_receipt.request);
    let mut state = owner.state().clone();
    assert_eq!(
        state.apply(&Event::Admit(other_receipt)),
        Err("child_admission_identity")
    );

    for field in 0..8 {
        let mut wrong = receipt.clone();
        match field {
            0 => wrong.binding.origin.task.run = RunId("other-run".into()),
            1 => wrong.binding.origin.task.task = TaskId("other-task".into()),
            2 => wrong.binding.origin.task.attempt = AttemptId(2),
            3 => wrong.binding.native.session = SessionId::from("s_wrong"),
            4 => wrong.binding.native.turn = TurnId::from("t_wrong"),
            5 => wrong.binding.native.command = CommandId::from("c_wrong"),
            6 => wrong.binding.origin.epoch = OwnerEpoch(2),
            _ => wrong.binding.payload_sha256 = sha256(b"different payload"),
        }
        assert_eq!(
            state.apply(&Event::Admit(wrong)),
            Err("child_admission_identity"),
            "field {field}"
        );
    }
    assert_eq!(state.apply(&Event::Admit(receipt.clone())), Ok(true));
    assert_eq!(state.apply(&Event::Admit(receipt.clone())), Ok(false));
    let mut conflicting = receipt;
    conflicting.request = RequestId::from("r_wrong");
    assert_eq!(
        state.apply(&Event::Admit(conflicting.clone())),
        Err("child_admission_conflict")
    );

    child.effect(&post, &a.effects).unwrap();
    let mut outcome = child.finish(candidate()).unwrap();
    outcome.admission = conflicting;
    assert_eq!(
        state.apply(&Event::Outcome(outcome)),
        Err("task_transition")
    );
    assert_eq!(
        state.apply(&Event::Owner(OwnerEpoch(1))),
        Err("task_transition")
    );
    assert_eq!(
        state.apply(&Event::Owner(OwnerEpoch(3))),
        Err("task_transition")
    );
}

#[test]
fn cancellation_precedes_normal_queue_even_when_full() {
    let f = Fixture::new();
    let (mut owner, handle, queue) = f.create();
    let child = owner.prepare(10).unwrap();
    let address = owner.address();
    let receipt = Evidence {
        gate: Gate::Mechanical,
        candidate_sha: "b".repeat(40),
        outcome_sha256: sha256(b"no candidate"),
        artifact_sha256: sha256(b"fixture"),
    };
    for _ in 0..MAILBOX_CAPACITY {
        assert_eq!(
            handle.evidence(&address, receipt.clone()),
            PostStatus::Accepted
        );
    }
    assert_eq!(handle.evidence(&address, receipt), PostStatus::QueueFull);
    let mut wrong = address.clone();
    wrong.task.attempt = AttemptId(2);
    assert_eq!(handle.cancel(&wrong), PostStatus::StaleTarget);
    assert_eq!(handle.cancel(&address), PostStatus::Accepted);
    let count = record_count(&owner.root_path());
    assert_eq!(owner.pump(), Err("task_stopped"));
    assert_eq!(handle.cancel(&address), PostStatus::Accepted);
    assert_eq!(owner.submit_intent(&child, 11), Err("task_stopped"));
    assert_eq!(record_count(&owner.root_path()), count + 1);
    assert!(queue.try_recv().is_err());
    drop((owner, child));
    let (owner, _, _) = f.open();
    assert!(owner.state().cancelled);
    assert!(!owner.state().intent);
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn cancellation_after_intent_stops_already_queued_admission() {
    let f = Fixture::new();
    let (mut owner, handle, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    assert_eq!(handle.cancel(&owner.address()), PostStatus::Accepted);
    assert_eq!(child.admit(&post), Err("task_stopped"));
    assert_eq!(owner.post(&child, 13), Err("task_stopped"));
    assert_eq!(f.effect_count(), 0);
    drop((owner, child));
    let (mut owner, _, _) = f.open();
    assert!(owner.state().cancelled && owner.state().intent);
    assert!(owner.open_child().unwrap().receipts().unwrap().0.is_none());
}

#[test]
fn known_late_outcome_stays_durable_after_cancel_without_acceptance() {
    let f = Fixture::new();
    let (mut owner, handle, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    child.admit(&post).unwrap();
    child.effect(&post, &f.effects).unwrap();
    assert_eq!(handle.cancel(&owner.address()), PostStatus::Accepted);
    owner.pump().unwrap();
    let outcome = child.finish(candidate()).unwrap();
    owner.reconcile(&child).unwrap();
    assert_eq!(owner.state().outcome.as_ref(), Some(&outcome));
    drop((owner, child));

    let (mut owner, handle, queue) = f.open();
    let child = owner.open_child().unwrap();
    owner.reconcile(&child).unwrap();
    assert_eq!(owner.state().outcome.as_ref(), Some(&outcome));
    assert_eq!(owner.post(&child, 14), Err("task_stopped"));
    assert_eq!(
        handle.evidence(&owner.address(), evidence(owner.state(), Gate::Mechanical)),
        PostStatus::Accepted
    );
    assert_eq!(owner.pump(), Err("task_stopped"));
    assert!(!owner.state().accepted());
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn crash_after_child_init_quarantines_orphan() {
    let f = Fixture::new();
    let (owner, _, _) = f.create();
    let child = Child::create(&owner.child_path(), &owner.state().spec, owner.address()).unwrap();
    let before = fs::read(owner.child_path().join("init.json")).unwrap();
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    assert!(owner.state().orphan);
    assert!(owner.prepare(15).is_err());
    assert!(owner.open_child().is_err());
    assert!(queue.try_recv().is_err());
    assert_eq!(
        before,
        fs::read(owner.child_path().join("init.json")).unwrap()
    );
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn replay_before_and_after_bind_and_intent_preserves_dispatch_budget() {
    let f = Fixture::new();
    drop(f.create());
    let (mut owner, _, _) = f.open();
    let child = owner.prepare(10).unwrap();
    let original = child.binding.clone();
    drop((owner, child));
    let (mut owner, _, _) = f.open();
    let child = owner.open_child().unwrap();
    assert_eq!(child.binding, original);
    assert!(!owner.state().intent);
    owner.submit_intent(&child, 11).unwrap();
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    assert!(owner.state().intent);
    let child = owner.open_child().unwrap();
    assert_eq!(owner.post(&child, 12), Ok(PostStatus::Accepted));
    let lost_before_admission = queue.try_recv().unwrap();
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let mut child = owner.open_child().unwrap();
    assert_eq!(child.admit(&lost_before_admission), Err("task_stopped"));
    assert_eq!(owner.state().posts, 1);
    assert_eq!(owner.post(&child, 11), Err("task_budget"));
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Accepted));
    let post = queue.try_recv().unwrap();
    assert_eq!(post.binding, original);
    assert_eq!(post.payload, lost_before_admission.payload);
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let child = owner.open_child().unwrap();
    assert_eq!(owner.state().posts, 2);
    assert_eq!(owner.post(&child, 14), Err("task_budget"));
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn missing_empty_replaced_and_corrupt_child_stores_are_never_reinitialized() {
    for damage in ["missing", "empty", "corrupt", "replaced"] {
        let f = Fixture::new();
        let (mut owner, _, queue) = f.create();
        let (mut child, post) = submit(&mut owner, &queue);
        child.admit(&post).unwrap();
        child.effect(&post, &f.effects).unwrap();
        let path = owner.child_path();
        let header = fs::read(path.join("init.json")).unwrap();
        drop((owner, child));
        fs::rename(&path, f.root.join("saved")).unwrap();
        if damage != "missing" {
            local_fs::create_private_leaf(&path).unwrap();
        }
        if matches!(damage, "corrupt" | "replaced") {
            let dir = local_fs::private_directory(&path).unwrap();
            ansa_linux::local::lock_new(&dir).unwrap();
            let bytes = if damage == "corrupt" {
                b"{".as_slice()
            } else {
                &header
            };
            ansa_linux::local::create(&dir, "init.json", bytes).unwrap();
        }

        let (mut owner, _, queue) = f.open();
        assert!(owner.open_child().is_err(), "{damage}");
        assert!(owner.prepare(15).is_err(), "{damage}");
        assert!(queue.try_recv().is_err());
        if damage == "missing" {
            assert!(!path.exists());
        }
        assert_eq!(f.effect_count(), 1);
    }
}

#[test]
fn missing_native_store_blocks_acceptance_of_an_imported_candidate() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    child.admit(&post).unwrap();
    child.effect(&post, &f.effects).unwrap();
    child.finish(candidate()).unwrap();
    owner.reconcile(&child).unwrap();
    let path = owner.child_path();
    drop((owner, child));
    let saved = f.root.join("saved");
    fs::rename(&path, &saved).unwrap();

    let (mut owner, handle, _) = f.open();
    assert!(owner.state().outcome.is_some());
    assert!(owner.open_child().is_err());
    for gate in [Gate::Mechanical, Gate::Review, Gate::Integration] {
        assert_eq!(
            handle.evidence(&owner.address(), evidence(owner.state(), gate)),
            PostStatus::Accepted
        );
        assert_eq!(owner.pump(), Err("child_recovery_required"));
    }
    assert!(!owner.state().accepted());
    assert_eq!(f.effect_count(), 1);

    fs::rename(saved, path).unwrap();
    let _child = owner.open_child().unwrap();
    let gate = evidence(owner.state(), Gate::Mechanical);
    assert_eq!(
        handle.evidence(&owner.address(), gate),
        PostStatus::Accepted
    );
    owner.pump().unwrap();
    assert_eq!(owner.state().evidence.len(), 1);
}

#[test]
fn observed_expiry_survives_restart_and_stops_admitted_child() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    child.admit(&post).unwrap();
    assert_eq!(owner.post(&child, 30), Ok(PostStatus::Duplicate));
    assert!(owner.state().expired);
    assert_eq!(child.effect(&post, &f.effects), Err("task_stopped"));
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let child = owner.open_child().unwrap();
    assert!(owner.state().expired);
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Duplicate));
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 0);

    let empty = Fixture::new();
    let (mut owner, _, queue) = empty.create();
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    assert_eq!(owner.post(&child, 30), Err("task_budget"));
    assert!(queue.try_recv().is_err());
    drop((owner, child));
    let (mut owner, _, queue) = empty.open();
    let child = owner.open_child().unwrap();
    assert_eq!(owner.post(&child, 13), Err("task_budget"));
    assert!(queue.try_recv().is_err());
}

#[test]
fn cancel_seen_between_post_publication_and_handoff_is_durable() {
    let f = Fixture::new();
    let (mut owner, handle, queue) = f.create();
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    let address = owner.address();
    owner.journal.after_publish = Some(Box::new(move || {
        assert_eq!(handle.cancel(&address), PostStatus::Accepted);
    }));
    assert_eq!(owner.post(&child, 12), Err("task_stopped"));
    assert_eq!(owner.state().posts, 1);
    assert!(owner.state().cancelled);
    assert!(queue.try_recv().is_err());
    drop((owner, child));
    let (mut owner, _, queue) = f.open();
    let child = owner.open_child().unwrap();
    assert!(owner.state().cancelled);
    assert_eq!(owner.post(&child, 13), Err("task_stopped"));
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn unresolved_effect_blocks_second_execution_and_acceptance() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    child.admit(&post).unwrap();
    child.effect(&post, &f.effects).unwrap();
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let mut child = owner.open_child().unwrap();
    assert_eq!(owner.post(&child, 14), Ok(PostStatus::Duplicate));
    assert_eq!(
        owner.state().outcome.as_ref().unwrap().effect,
        Effect::Unknown
    );
    assert_eq!(
        child.effect(&post, &f.effects),
        Err("child_effect_already_reserved_or_not_admitted")
    );
    assert!(child.finish(Effect::KnownFailure).is_err());
    assert!(!owner.state().accepted());
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn repository_lease_is_exclusive_and_resolves_common_directory_identity() {
    let f = Fixture::new();
    let (owner, _, _) = f.create();
    let alias = f.root.join("alias");
    symlink(f.repository(), &alias).unwrap();
    assert!(matches!(
        Owner::open(&f.registry(), &f.repository()),
        Err("repository_busy")
    ));
    assert!(matches!(
        Owner::create(&f.registry(), &f.repository(), spec()),
        Err("repository_busy")
    ));
    assert!(Owner::open(&f.registry(), &alias).is_err());
    drop(owner);
    assert!(Owner::create(&f.registry(), &f.repository(), spec()).is_err());
    assert_eq!(f.open().0.address().epoch, OwnerEpoch(2));
}

#[test]
fn append_failure_and_torn_tail_do_not_dispatch() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    owner.journal.fault = Some(Fault::PartialAppend);
    assert_eq!(owner.post(&child, 12), Err("store_append"));
    assert_eq!(owner.state().posts, 0);
    assert_eq!(owner.post(&child, 13), Err("owner_store_failed"));
    assert!(queue.try_recv().is_err());
    drop((owner, child));
    assert!(matches!(
        Owner::open(&f.registry(), &f.repository()),
        Err("store_incomplete_or_foreign_record")
    ));
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn root_sync_ack_failure_does_not_publish_and_replay_charges_reservation() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    owner.journal.fault = Some(Fault::LostSyncAck);
    assert_eq!(owner.post(&child, 12), Err("store_sync"));
    assert_eq!(owner.state().posts, 0);
    assert!(queue.try_recv().is_err());
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let mut child = owner.open_child().unwrap();
    assert_eq!(owner.state().posts, 1);
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Accepted));
    let post = queue.try_recv().unwrap();
    child.admit(&post).unwrap();
    child.effect(&post, &f.effects).unwrap();
    child.finish(candidate()).unwrap();
    assert_eq!(f.effect_count(), 1);
}

#[test]
fn child_sync_ack_failure_never_becomes_a_receipt_without_reopen() {
    let f = Fixture::new();
    let (mut owner, _, queue) = f.create();
    let (mut child, post) = submit(&mut owner, &queue);
    child.journal.fault = Some(Fault::LostSyncAck);
    assert_eq!(child.admit(&post), Err("store_sync"));
    assert_eq!(owner.reconcile(&child), Err("child_store_failed"));
    assert!(owner.state().admission.is_none());
    drop((owner, child));

    let (mut owner, _, queue) = f.open();
    let child = owner.open_child().unwrap();
    owner.reconcile(&child).unwrap();
    assert_eq!(
        owner.state().admission.as_ref().unwrap().request,
        RequestId::from("r_1")
    );
    assert_eq!(owner.post(&child, 13), Ok(PostStatus::Duplicate));
    assert!(queue.try_recv().is_err());
    assert_eq!(f.effect_count(), 0);
}

#[test]
fn checksum_detects_a_changed_last_event_without_following_frame() {
    let f = Fixture::new();
    let (mut owner, _, _) = f.create();
    let child = owner.prepare(10).unwrap();
    owner.submit_intent(&child, 11).unwrap();
    owner.journal.fault = Some(Fault::LostSyncAck);
    assert_eq!(owner.post(&child, 12), Err("store_sync"));
    let path = owner.root_path().join("event-002.json");
    drop((owner, child));
    let raw = fs::read_to_string(&path).unwrap();
    let tampered = raw.replace("\"at\":12", "\"at\":13");
    assert_ne!(raw, tampered);
    fs::write(&path, tampered).unwrap();
    assert!(matches!(
        Owner::open(&f.registry(), &f.repository()),
        Err("store_record_chain")
    ));
}
