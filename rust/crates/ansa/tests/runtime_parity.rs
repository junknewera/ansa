mod support {
    pub mod runtime_fixture;
}
use support::runtime_fixture as fixture;
#[tokio::test(flavor = "current_thread")]
async fn roundtrip() {
    fixture::roundtrip().await;
}
#[tokio::test(flavor = "current_thread")]
async fn queued_success_cancel() {
    fixture::cancel_at_completion().await;
}
#[tokio::test(flavor = "current_thread")]
async fn late_known_tool_outcome() {
    fixture::tool_cancel().await;
}
#[tokio::test(flavor = "current_thread")]
async fn intent_fsync_cancel() {
    fixture::durable().await;
}
#[test]
fn public_batch_cannot_bypass_durable_policy() {
    fixture::durable_bypass();
}
#[test]
fn mixed_registry_safe_batch() {
    fixture::mixed_registry();
}
#[tokio::test(flavor = "current_thread")]
async fn admission() {
    fixture::admission().await;
}
#[tokio::test(flavor = "current_thread")]
async fn resync() {
    fixture::resync().await;
}
#[tokio::test(flavor = "current_thread")]
async fn invalid_response() {
    fixture::invalid_response().await;
}
#[tokio::test(flavor = "current_thread")]
async fn auth_cancel() {
    fixture::auth_cancel().await;
}
#[tokio::test(flavor = "current_thread")]
async fn auth_drop() {
    fixture::auth_drop().await;
}
#[tokio::test(flavor = "current_thread")]
async fn preview_flood() {
    fixture::preview_flood().await;
}
#[tokio::test(flavor = "current_thread")]
async fn shutdown() {
    fixture::shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn restart_limit() {
    fixture::restart_limit().await;
}
#[tokio::test(flavor = "current_thread")]
async fn known_outcome_storage_failure() {
    fixture::storage_failure().await;
}
#[tokio::test(flavor = "current_thread")]
async fn summary_commit() {
    fixture::summary(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn late_summary_cancel() {
    fixture::summary(true).await;
}
#[tokio::test(flavor = "current_thread")]
async fn summary_budget_uses_serialized_request() {
    fixture::summary_budget().await;
}
#[tokio::test(flavor = "current_thread")]
async fn checkpoint_cancellation() {
    fixture::checkpoint_cancellation().await;
}
#[tokio::test(flavor = "current_thread")]
async fn restart_resume() {
    fixture::restart_resume().await;
}
#[tokio::test(flavor = "current_thread")]
async fn plan_admission() {
    fixture::plan_admission().await;
}
#[tokio::test(flavor = "current_thread")]
async fn cancelled_during_response_commit_keeps_only_the_confirmed_note() {
    use ansa::{provider::native, runtime::*, session::journal_v1::Point};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let hold = gate.clone();
    let calls = Arc::new(AtomicUsize::default());
    let mut runtime = fixture::setup(
        calls.clone(),
        None,
        Arc::new(|request, _, preview| {
            preview("Checking")?;
            native::load(&fixture::saved_response(request, Some("Checking"), 1))
        }),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            streaming: true,
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "response" {
                    hold.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Default::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let request = fixture::submit(&handle, "submit", "fixture", "turn").await;
    gate.wait();
    fixture::post(
        &handle,
        "cancel",
        CommandPayload::Cancel(request),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    assert_eq!(
        fixture::snapshot(&handle, "before-sync").await.state,
        "cancelled"
    );
    gate.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    assert_eq!(
        fixture::snapshot(&handle, "after-sync")
            .await
            .preview
            .unwrap()
            .state,
        "committed"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_commentary_stays_committed_when_tool_preparation_fails() {
    use ansa::{provider::native, runtime::*, tools::registry::Registry};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::default());
    let base = fixture::tools(calls.clone(), None, false);
    let mut tool = base.definition("get_test_value").unwrap().clone();
    tool.validator = Arc::new(|_| false);
    let mut registry = Registry::new();
    registry.register(tool).unwrap();
    let tools = registry.snapshot();
    let mut runtime = Runtime::new(
        "offline".into(),
        fixture::configuration(&tools),
        tools,
        Arc::new(|request, _, preview| {
            preview("Checking the report")?;
            native::load(&fixture::saved_response(
                request,
                Some("Checking the report"),
                1,
            ))
        }),
        Limits::default(),
        Options {
            streaming: true,
            ..Default::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    fixture::submit(&handle, "submit", "fixture", "turn").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    let view = fixture::snapshot(&handle, "check-note").await;
    assert_eq!(view.state, "paused");
    assert_eq!(view.preview.unwrap().state, "committed");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn tool_pause_shows_workspace_reason_before_and_after_restart() {
    use ansa::{
        provider::replay::{Effects, Execution},
        runtime::*,
        tools::registry::{Outcome, Registry},
    };
    use std::sync::{Arc, atomic::AtomicUsize};
    let root = fixture::Root::new();
    let base = fixture::tools(Arc::new(AtomicUsize::default()), None, false);
    let mut tool = base.definition("get_test_value").unwrap().clone();
    tool.executor = Arc::new(|_| {
        Outcome::from_bytes(
            Execution::Failed,
            Effects::NotStarted,
            br#"{"error":"terminal_workspace_quarantined","applied":false}"#.to_vec(),
        )
    });
    let mut registry = Registry::new();
    registry.register(tool).unwrap();
    let tools = registry.snapshot();
    for first in [true, false] {
        let mut runtime = Runtime::new(
            "offline".into(),
            fixture::configuration(&tools),
            tools.clone(),
            fixture::generator(1),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                ..Default::default()
            },
        )
        .unwrap();
        runtime.ready().await.unwrap();
        let handle = runtime.handle();
        if first {
            fixture::submit(&handle, "submit", "fixture", "turn").await;
            fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
        }
        let view = fixture::snapshot(&handle, "check-reason").await;
        assert_eq!(view.state, "paused");
        assert_eq!(view.diagnostic, "terminal_workspace_quarantined");
        fixture::finish(&mut runtime).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_restart() {
    fixture::unknown_restart().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unresolved_restart() {
    fixture::unresolved_restart().await;
}
#[tokio::test(flavor = "current_thread")]
async fn multi_round() {
    fixture::multi_round().await;
}
#[tokio::test(flavor = "current_thread")]
async fn fast_events() {
    fixture::fast_events().await;
}
