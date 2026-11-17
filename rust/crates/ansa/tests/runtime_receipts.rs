#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}
use ansa::{frontend::jsonl, provider::native, runtime::*, session::journal_v1::Point};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::runtime_fixture as f;

async fn lookup(handle: &Handle, query: &str, command: &str) -> RuntimeEvent {
    f::post(
        handle,
        query,
        CommandPayload::GetMessageReceipt(command.into()),
        "read",
        PostStatus::Accepted,
    )
    .await;
    loop {
        let event = f::event(handle).await;
        if matches!(&event.payload, EventPayload::MessageReceipt { query: id, .. } if id.0 == query)
        {
            return event;
        }
    }
}
fn receipt(event: RuntimeEvent) -> MessageReceiptInfo {
    let EventPayload::MessageReceipt {
        receipt: Some(receipt),
        ready,
        ..
    } = event.payload
    else {
        panic!("missing message receipt")
    };
    assert!(ready);
    *receipt
}
fn parts(root: &f::Root) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(root.journal())
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("part-")
                .then(|| (name, std::fs::read(entry.path()).unwrap()))
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn message_lookup_survives_restart_without_generation_execution_or_journal_writes() {
    let root = f::Root::new();
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = f::setup(
        count.clone(),
        None,
        f::generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = f::submit(&handle, "editor_message", "SUBMITTED_TEXT_CANARY", "turn").await;
    f::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let before = parts(&root);
    let event = lookup(&handle, "lookup_complete", "editor_message").await;
    let wire = jsonl::encode(&event).dump();
    assert!(!wire.contains("SUBMITTED_TEXT_CANARY") && !wire.contains("OMH_FIXED_VALUE_42"));
    let found = receipt(event);
    assert_eq!(found.kind, MessageKind::Submit);
    assert_eq!(found.request, Some(target.clone()));
    assert_eq!(found.state, "completed");
    assert!(found.admission_durable);
    assert!(found.state_durable);
    assert_eq!(found.mode, Some(AgentMode::Basic));
    assert_eq!(
        found.text_sha256,
        ansa::protocol::legacy_json_v1::sha256(b"SUBMITTED_TEXT_CANARY")
    );
    let absent = lookup(&handle, "lookup_absent", "never_submitted").await;
    assert!(matches!(
        absent.payload,
        EventPayload::MessageReceipt {
            receipt: None,
            ready: true,
            storage_failed: false,
            ..
        }
    ));
    assert_eq!(before, parts(&root));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let old = handle.incarnation().clone();
    f::finish(&mut runtime).await;
    let generations = Arc::new(AtomicUsize::new(0));
    let observed = generations.clone();
    let mut restored = f::setup(
        count.clone(),
        None,
        Arc::new(move |r, _, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            f::model(r, 1)
        }),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = restored.handle();
    assert_ne!(*handle.incarnation(), old);
    let found = receipt(lookup(&handle, "lookup_reopened", "editor_message").await);
    assert_eq!(found.request, Some(target));
    assert!(found.admission_durable);
    assert_eq!(found.state, "completed");
    f::post(
        &handle,
        "editor_message",
        CommandPayload::Submit(Submit {
            text: "SUBMITTED_TEXT_CANARY".into(),
            mode: AgentMode::Basic,
        }),
        "turn",
        PostStatus::Duplicate,
    )
    .await;
    assert_eq!(generations.load(Ordering::SeqCst), 0);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(before, parts(&root));
    f::finish(&mut restored).await;
}

struct Release(Arc<f::Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn message_lookup_distinguishes_volatile_submit_from_durable_queued_steering() {
    let root = f::Root::new();
    let gate = Arc::new(f::Gate::default());
    let _release = Release(gate.clone());
    let held = gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        Arc::new(move |r, stop, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            held.hold(stop);
            native::load(&f::saved_response(r, Some("done"), 0))
        }),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let first = f::submit(&handle, "first", "A", "turn").await;
    gate.wait();
    f::post(
        &handle,
        "queued_submit",
        CommandPayload::Submit(Submit {
            text: "later".into(),
            mode: AgentMode::Plan,
        }),
        "later",
        PostStatus::Accepted,
    )
    .await;
    let before = parts(&root);
    let queued = receipt(lookup(&handle, "lookup_queued", "queued_submit").await);
    assert_eq!(queued.state, "queued");
    assert!(!queued.admission_durable);
    assert!(queued.request.is_none());
    assert_eq!(queued.mode, Some(AgentMode::Plan));
    assert_eq!(before, parts(&root));
    f::post(
        &handle,
        "followup",
        CommandPayload::Steer(Steer {
            target: first.clone(),
            text: "guidance".into(),
        }),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let before = parts(&root);
    let queued = receipt(lookup(&handle, "lookup_followup", "followup").await);
    assert_eq!(queued.kind, MessageKind::Steer);
    assert_eq!(queued.request, Some(first));
    assert_eq!(queued.state, "queued");
    assert!(queued.admission_durable);
    assert_eq!(queued.mode, Some(AgentMode::Basic));
    assert_eq!(before, parts(&root));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.release();
    f::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn lost_acceptance_ack_is_uncertain_until_canonical_restart_lookup() {
    let root = f::Root::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        Arc::new(move |r, _, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            f::model(r, 0)
        }),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(|point, kind| {
                if point == Point::AfterSync && kind == "accepted" {
                    Err("fixture_ack_loss")
                } else {
                    Ok(())
                }
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    f::post(
        &handle,
        "uncertain",
        CommandPayload::Submit(Submit {
            text: "question".into(),
            mode: AgentMode::Basic,
        }),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    loop {
        if matches!(f::event(&handle).await.payload,EventPayload::FrontendSnapshot(v) if v.storage_failed)
        {
            break;
        }
    }
    let event = lookup(&handle, "lookup_failed", "uncertain").await;
    assert!(matches!(
        &event.payload,
        EventPayload::MessageReceipt {
            storage_failed: true,
            ..
        }
    ));
    assert!(!receipt(event).admission_durable);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    f::finish(&mut runtime).await;
    let mut restored = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        f::generator(0),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let found = receipt(lookup(&restored.handle(), "lookup_known", "uncertain").await);
    assert!(found.admission_durable);
    assert!(found.request.is_some());
    assert_eq!(found.state, "paused");
    f::finish(&mut restored).await;
}

#[tokio::test(flavor = "current_thread")]
async fn memory_only_messages_never_claim_journal_durability() {
    let gate = Arc::new(f::Gate::default());
    let _release = Release(gate.clone());
    let held = gate.clone();
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        Arc::new(move |r, stop, _| {
            held.hold(stop);
            native::load(&f::saved_response(r, Some("done"), 0))
        }),
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let target = f::submit(&handle, "memory_submit", "A", "turn").await;
    gate.wait();
    let found = receipt(lookup(&handle, "memory_lookup", "memory_submit").await);
    assert!(!found.admission_durable && !found.state_durable);
    f::post(
        &handle,
        "memory_steer",
        CommandPayload::Steer(Steer {
            target,
            text: "B".into(),
        }),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let found = receipt(lookup(&handle, "memory_followup_lookup", "memory_steer").await);
    assert!(!found.admission_durable && !found.state_durable);
    gate.release();
    f::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_sync_does_not_claim_a_durable_cancelled_state() {
    let root = f::Root::new();
    let gate = Arc::new(f::Gate::default());
    let _release = Release(gate.clone());
    let held = gate.clone();
    let executed = Arc::new(AtomicUsize::new(0));
    let mut runtime = f::setup(
        executed.clone(),
        None,
        f::generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "intent" {
                    held.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = f::submit(&handle, "cancelled_message", "A", "turn").await;
    gate.wait();
    f::post(
        &handle,
        "stop",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let found = receipt(lookup(&handle, "cancel_before_sync", "cancelled_message").await);
    assert_eq!(found.state, "cancelled");
    assert!(found.admission_durable);
    assert!(!found.state_durable);
    gate.release();
    f::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    let found = receipt(lookup(&handle, "cancel_after_sync", "cancelled_message").await);
    assert_eq!(found.state, "cancelled");
    assert!(found.admission_durable && found.state_durable);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    f::finish(&mut runtime).await;
}
