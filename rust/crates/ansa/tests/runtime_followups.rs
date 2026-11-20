#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}
use ansa::{
    protocol::json::Json,
    provider::{
        native,
        replay::{Effects, Execution},
        request::Request,
    },
    runtime::*,
    session::{
        journal_v1::{Journal, Point},
        state::State,
    },
    tools::registry::{Outcome, Policy, Registry},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use support::runtime_fixture as fixture;

fn has_user(request: &Request, text: &str) -> bool {
    request.input.iter().any(|item| {
        item.has("role")
            && item.text_field("role").is_ok_and(|r| r == "user")
            && item.text_field("content").is_ok_and(|v| v == text)
    })
}
async fn steer(handle: &Handle, id: &str, target: &RequestId, text: &str) {
    fixture::post(
        handle,
        id,
        CommandPayload::Steer(Steer {
            target: target.clone(),
            text: text.into(),
        }),
        "turn",
        PostStatus::Accepted,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_followup_batch_is_kept_without_poisoning_the_journal() {
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let hold = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        if has_user(request, "first") {
            assert_eq!(request.input.len(), 1);
            hold.hold(stop);
        }
        native::load(&fixture::saved_response(request, Some("done"), 0))
    });
    let options = Options {
        directory: Some(root.journal()),
        ..Options::default()
    };
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider.clone(),
        Limits::default(),
        options.clone(),
    )
    .await;
    let handle = runtime.handle();
    let request = fixture::submit(&handle, "first", "first", "turn").await;
    gate.wait();
    let large = "a".repeat(Json::FRAME_BYTES / 2);
    steer(&handle, "large_a", &request, &large).await;
    steer(&handle, "large_b", &request, &large).await;
    gate.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { diagnostic, .. } if diagnostic == "followup_frame_limit")).await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::finish(&mut runtime).await;
    let mut reopened = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        options,
    )
    .await;
    let snapshot = fixture::snapshot(&reopened.handle(), "check").await;
    assert!(!snapshot.storage_failed);
    assert_eq!(snapshot.followups.len(), 2);
    assert!(
        snapshot
            .followups
            .iter()
            .all(|message| message.state == "rejected")
    );
    fixture::finish(&mut reopened).await;
}

#[tokio::test(flavor = "current_thread")]
async fn new_turn_preserves_cancelled_context_over_compaction_threshold() {
    let root = fixture::Root::new();
    let first = Arc::new(fixture::Gate::default());
    let second = Arc::new(fixture::Gate::default());
    let initial = first.clone();
    let delivered = second.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let seen = observed.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        let last = request
            .input
            .last()
            .and_then(|item| item.text_field("content").ok());
        match last {
            Some("D") => initial.hold(stop),
            Some("D_UPDATE") => delivered.hold(stop),
            Some(text) if text.len() == 14 * 1024 => {
                assert!(has_user(request, "D") && has_user(request, "D_UPDATE"));
                seen.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
        native::load(
            &fixture::saved_response(request, Some("done"), 0)
                .with("context_tokens", Json::parse("49998")?)?
                .with("context_window", Json::parse("100000")?)?,
        )
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            keep_recent_turns: 2,
            compact: Some(Arc::new(ansa::frontend::offline::compact_retained)),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    for id in ["A", "B", "C"] {
        fixture::submit(&handle, id, id, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    let request = fixture::submit(&handle, "startD", "D", "turn").await;
    first.wait();
    steer(&handle, "updateD", &request, "D_UPDATE").await;
    first.release();
    second.wait();
    fixture::post(
        &handle,
        "stopD",
        CommandPayload::Cancel(request),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    second.release();
    fixture::until(
        &handle,
        |p| matches!(p, EventPayload::FrontendSnapshot(s) if !s.worker_busy),
    )
    .await;
    fixture::submit(&handle, "nextE", &"e".repeat(14 * 1024), "E").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(
        fixture::snapshot(&handle, "verify_no_compact")
            .await
            .compactions,
        0
    );
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn autocompact_reaches_half_window_even_after_reopen() {
    let root = fixture::Root::new();
    let large = "a".repeat(14 * 1024);
    let expected = large.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        let last = request.input.last().unwrap().text_field("content")?;
        match last {
            "D" => assert!(has_user(request, &expected)),
            "E" => {
                assert!(!has_user(request, &expected));
                assert!(has_user(request, "C") && has_user(request, "D"));
            }
            _ => {}
        }
        observed.fetch_add(1, Ordering::SeqCst);
        let saved = fixture::saved_response(request, Some("done"), 0);
        let saved = if last == "D" {
            saved.with("measured_context_tokens", Json::parse("499999")?)?
        } else {
            saved
                .with("context_tokens", Json::parse("499998")?)?
                .with("context_window", Json::parse("1000000")?)?
        };
        native::load(&saved)
    });
    let start = || {
        fixture::setup(
            Arc::new(AtomicUsize::new(0)),
            None,
            provider.clone(),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                keep_recent_turns: 2,
                compact: Some(Arc::new(ansa::frontend::offline::compact_retained)),
                ..Options::default()
            },
        )
    };
    let mut runtime = start().await;
    let handle = runtime.handle();
    for (id, text) in [("A", large.as_str()), ("B", "B"), ("C", "C"), ("D", "D")] {
        fixture::submit(&handle, id, text, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    assert_eq!(
        fixture::snapshot(&handle, "below_half").await.compactions,
        0
    );
    fixture::finish(&mut runtime).await;

    let mut runtime = start().await;
    let handle = runtime.handle();
    fixture::submit(&handle, "E", "E", "E").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(fixture::snapshot(&handle, "at_half").await.compactions, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn coordinator_without_summarizer_keeps_its_history_at_the_threshold() {
    let large = "a".repeat(14 * 1024);
    let expected = large.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        assert!(has_user(request, &expected));
        native::load(
            &fixture::saved_response(request, Some("done"), 0)
                .with("context_tokens", Json::parse("600000")?)?
                .with("context_window", Json::parse("1000000")?)?,
        )
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    for (id, text) in [("A", large.as_str()), ("B", "B"), ("C", "C"), ("D", "D")] {
        fixture::submit(&handle, id, text, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    assert_eq!(
        fixture::snapshot(&handle, "coordinator").await.compactions,
        0
    );
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn autocompact_selection_survives_reopen_and_controls_next_submit() {
    let root = fixture::Root::new();
    let large = "a".repeat(14 * 1024);
    let expected = large.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        let last = request.input.last().unwrap().text_field("content")?;
        if last == "D" {
            assert!(has_user(request, &expected));
        } else if last == "E" {
            assert!(!has_user(request, &expected));
            assert!(has_user(request, "C") && has_user(request, "D"));
        }
        native::load(
            &fixture::saved_response(request, Some("done"), 0)
                .with("context_tokens", Json::parse("600000")?)?
                .with("context_window", Json::parse("1000000")?)?,
        )
    });
    let start = |default| {
        fixture::setup(
            Arc::new(AtomicUsize::new(0)),
            None,
            provider.clone(),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                keep_recent_turns: 2,
                compact: Some(Arc::new(ansa::frontend::offline::compact_retained)),
                autocompact_percent: Some(default),
                ..Options::default()
            },
        )
    };
    let mut runtime = start(85).await;
    let handle = runtime.handle();
    assert_eq!(
        fixture::snapshot(&handle, "initial")
            .await
            .autocompact_percent,
        85
    );
    let select = |handle: &Handle, id: &'static str, percent, status| {
        let handle = handle.clone();
        async move {
            fixture::post(
                &handle,
                id,
                CommandPayload::SetAutocompact(percent),
                "idle",
                status,
            )
            .await;
        }
    };
    select(&handle, "high", 90, PostStatus::Accepted).await;
    for (id, text) in [("A", large.as_str()), ("B", "B"), ("C", "C"), ("D", "D")] {
        fixture::submit(&handle, id, text, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    assert_eq!(
        fixture::snapshot(&handle, "below_high").await.compactions,
        0
    );
    select(&handle, "low", 60, PostStatus::Accepted).await;
    fixture::finish(&mut runtime).await;

    let mut runtime = start(100).await;
    let handle = runtime.handle();
    assert_eq!(
        fixture::snapshot(&handle, "restored")
            .await
            .autocompact_percent,
        60
    );
    select(&handle, "low", 60, PostStatus::Duplicate).await;
    select(&handle, "low", 61, PostStatus::CommandConflict).await;
    fixture::submit(&handle, "E", "E", "E").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(fixture::snapshot(&handle, "at_low").await.compactions, 1);
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn autocompact_estimates_the_known_window_without_usage_after_reopen() {
    let root = fixture::Root::new();
    let older = "a".repeat(14 * 1024);
    let crosses_half = "e".repeat(6 * 1024);
    let expected = older.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        let last = request.input.last().unwrap().text_field("content")?;
        let mut saved = fixture::saved_response(request, Some("done"), 0);
        if last == expected {
            saved = saved.with("model_context_window", Json::parse("10000")?)?;
        } else if last == "D" {
            assert!(has_user(request, &expected));
        } else if last.starts_with('e') {
            assert!(!has_user(request, &expected));
            assert!(has_user(request, "C") && has_user(request, "D"));
        }
        native::load(&saved)
    });
    let start = || {
        fixture::setup(
            Arc::new(AtomicUsize::new(0)),
            None,
            provider.clone(),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                keep_recent_turns: 2,
                compact: Some(Arc::new(ansa::frontend::offline::compact_retained)),
                ..Options::default()
            },
        )
    };
    let mut runtime = start().await;
    let handle = runtime.handle();
    for (id, text) in [("A", older.as_str()), ("B", "B"), ("C", "C"), ("D", "D")] {
        fixture::submit(&handle, id, text, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    assert_eq!(
        fixture::snapshot(&handle, "estimate_below_half")
            .await
            .compactions,
        0
    );
    fixture::finish(&mut runtime).await;

    let mut runtime = start().await;
    let handle = runtime.handle();
    fixture::submit(&handle, "E", &crosses_half, "E").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(
        fixture::snapshot(&handle, "estimate_at_half")
            .await
            .compactions,
        1
    );
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_window_does_not_compact_a_large_serialized_request() {
    let root = fixture::Root::new();
    let text = "Ж".repeat(150_000);
    let expected = text.clone();
    let reached = Arc::new(AtomicBool::new(false));
    let observed = reached.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        if request.input.last().unwrap().text_field("content")? == expected {
            assert!(has_user(request, "earliest"));
            observed.store(true, Ordering::SeqCst);
        }
        native::load(&fixture::saved_response(request, Some("done"), 0))
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            keep_recent_turns: 2,
            compact: Some(Arc::new(|_, _, _| {
                panic!("unknown window triggered compaction")
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    for (id, input) in [
        ("A", "earliest"),
        ("B", "second"),
        ("C", "third"),
        ("D", &text),
    ] {
        fixture::submit(&handle, id, input, id).await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    assert!(reached.load(Ordering::SeqCst));
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_boundary_delivers_fifo_before_any_requested_executor() {
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let held = gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            held.hold(stop);
            return native::load(&fixture::saved_response(request, None, 2));
        }
        assert!(has_user(request, "A") && has_user(request, "B") && has_user(request, "C"));
        let outputs = request
            .input
            .iter()
            .filter(|v| {
                v.has("type")
                    && v.text_field("type")
                        .is_ok_and(|t| t == "function_call_output")
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert!(outputs.iter().all(|item| {
            item.text_field("output")
                .unwrap()
                .contains("not_started_user_followup")
        }));
        let b = request
            .input
            .iter()
            .position(|v| v.has("content") && v.text_field("content").is_ok_and(|s| s == "B"))
            .unwrap();
        let c = request
            .input
            .iter()
            .position(|v| v.has("content") && v.text_field("content").is_ok_and(|s| s == "C"))
            .unwrap();
        assert!(b < c);
        native::load(&fixture::saved_response(request, Some("updated"), 0))
    });
    let executed = Arc::new(AtomicUsize::new(0));
    let mut runtime = fixture::setup(
        executed.clone(),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    gate.wait();
    steer(&handle, "message_b", &target, "B").await;
    steer(&handle, "message_c", &target, "C").await;
    fixture::post(
        &handle,
        "message_b",
        CommandPayload::Steer(Steer {
            target: target.clone(),
            text: "B".into(),
        }),
        "turn",
        PostStatus::Duplicate,
    )
    .await;
    fixture::post(
        &handle,
        "message_b",
        CommandPayload::Steer(Steer {
            target,
            text: "changed".into(),
        }),
        "turn",
        PostStatus::CommandConflict,
    )
    .await;
    let view = fixture::snapshot(&handle, "queued_view").await;
    assert_eq!(view.followups.len(), 2);
    assert!(view.followups.iter().all(|m| m.state == "queued"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = fixture::snapshot(&handle, "completed_view").await;
    assert!(view.followups.iter().all(|m| m.state == "delivered"));
    assert_eq!(view.transcript_size, 4);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    fixture::finish(&mut runtime).await;
    let tools = fixture::tools(executed, None, true);
    let mut state = State::new(
        "offline".into(),
        fixture::configuration(&tools),
        tools.read_only().advertisement().clone(),
    );
    let journal = Journal::open(&root.journal(), |event| state.apply(event), None).unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert_eq!(state.followups.len(), 2);
    assert_eq!(state.pending_followups().count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_boundary_preserves_known_result_and_skips_rest_of_batch() {
    let gate = Arc::new(fixture::Gate::default());
    let count = Arc::new(AtomicUsize::new(0));
    let provider: Generate = Arc::new(|request, _, _| {
        if !has_user(request, "B") {
            return native::load(&fixture::saved_response(request, None, 2));
        }
        let outputs = request
            .input
            .iter()
            .filter(|v| {
                v.has("type")
                    && v.text_field("type")
                        .is_ok_and(|t| t == "function_call_output")
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert!(
            outputs[0]
                .text_field("output")
                .unwrap()
                .contains("OMH_FIXED_VALUE_42")
        );
        assert!(
            outputs[1]
                .text_field("output")
                .unwrap()
                .contains("not_started_user_followup")
        );
        native::load(&fixture::saved_response(request, Some("after tool"), 0))
    });
    let mut runtime = fixture::setup(
        count.clone(),
        Some(gate.clone()),
        provider,
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    gate.wait();
    steer(&handle, "message_b", &target, "B").await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    gate.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(
        fixture::snapshot(&handle, "final_view")
            .await
            .completed_tools,
        1
    );
    fixture::finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn interrupt_during_unacknowledged_queue_fsync_preserves_message() {
    let root = fixture::Root::new();
    let provider_gate = Arc::new(fixture::Gate::default());
    let held_provider = provider_gate.clone();
    let journal_gate = Arc::new(fixture::Gate::default());
    let held_journal = journal_gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let numbered = calls.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        if numbered.fetch_add(1, Ordering::SeqCst) == 0 {
            held_provider.hold(stop);
            return native::load(&fixture::saved_response(request, None, 1));
        }
        assert!(has_user(request, "A") && has_user(request, "B"));
        native::load(&fixture::saved_response(request, Some("read pending B"), 0))
    });
    let executed = Arc::new(AtomicUsize::new(0));
    let mut runtime = fixture::setup(
        executed.clone(),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "queued" {
                    held_journal.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    provider_gate.wait();
    let command = fixture::command(
        &handle,
        "message_b",
        CommandPayload::Steer(Steer {
            target: target.clone(),
            text: "B".into(),
        }),
        "turn",
    );
    let sender = handle.clone();
    let queued = tokio::spawn(async move { sender.post(command).await });
    tokio::task::yield_now().await;
    journal_gate.wait();
    assert!(!queued.is_finished(), "message acknowledged before fsync");
    fixture::post(
        &handle,
        "interrupt",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    assert!(provider_gate.stopped(), "Cancel waited for queued fsync");
    provider_gate.release();
    assert_eq!(
        fixture::snapshot(&handle, "blocked_queue_view").await.state,
        "cancelled"
    );
    journal_gate.release();
    assert_eq!(queued.await.unwrap().unwrap(), PostStatus::Accepted);
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = fixture::snapshot(&handle, "delivered_view").await;
    assert_eq!(view.followups[0].state, "delivered");
    fixture::finish(&mut runtime).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn queue_capacity_reserves_inflight_fsync_without_poisoning_journal() {
    let root = fixture::Root::new();
    let provider_gate = Arc::new(fixture::Gate::default());
    let held_provider = provider_gate.clone();
    let journal_gate = Arc::new(fixture::Gate::default());
    let held_journal = journal_gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let numbered = calls.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        if numbered.fetch_add(1, Ordering::SeqCst) == 0 {
            held_provider.hold(stop);
        }
        native::load(&fixture::saved_response(request, Some("answer"), 0))
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "queued" {
                    held_journal.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    provider_gate.wait();
    let mut pending = Vec::new();
    for index in 0..16 {
        let id = format!("message_{index}");
        let command = fixture::command(
            &handle,
            &id,
            CommandPayload::Steer(Steer {
                target: target.clone(),
                text: format!("B{index}"),
            }),
            "turn",
        );
        let sender = handle.clone();
        pending.push(tokio::spawn(async move { sender.post(command).await }));
        tokio::task::yield_now().await;
        if index == 0 {
            journal_gate.wait();
        }
        // This reaches the owner after the original on the same FIFO. A
        // conflicting retry proves the original owns a reservation even while
        // its durable admission reply is blocked.
        fixture::post(
            &handle,
            &id,
            CommandPayload::Steer(Steer {
                target: target.clone(),
                text: "different".into(),
            }),
            "turn",
            PostStatus::CommandConflict,
        )
        .await;
    }
    let extra = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.post(fixture::command(
            &handle,
            "over_capacity",
            CommandPayload::Steer(Steer {
                target,
                text: "excess".into(),
            }),
            "turn",
        )),
    )
    .await;
    journal_gate.release();
    provider_gate.release();
    assert_eq!(
        extra.expect("in-flight reservation was omitted").unwrap(),
        PostStatus::QueueFull
    );
    for reply in pending {
        assert_eq!(reply.await.unwrap().unwrap(), PostStatus::Accepted);
    }
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = fixture::snapshot(&handle, "capacity_view").await;
    assert!(!view.storage_failed);
    assert_eq!(view.followups.len(), 16);
    assert!(view.followups.iter().all(|m| m.state == "delivered"));
    fixture::finish(&mut runtime).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn generation_capacity_includes_current_validation_and_write() {
    let gate = Arc::new(fixture::Gate::default());
    let held = gate.clone();
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        fixture::generator(0),
        Limits::default(),
        Options {
            select_generation: Arc::new(move |_, stop| {
                held.hold(stop);
                Ok(())
            }),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let mut pending = Vec::new();
    for index in 0..4 {
        let id = format!("selection_{index}");
        let command = fixture::command(
            &handle,
            &id,
            CommandPayload::SetGeneration(GenerationSelection {
                model: format!("model_{index}"),
                effort: None,
            }),
            "idle",
        );
        let sender = handle.clone();
        pending.push(tokio::spawn(async move { sender.post(command).await }));
        tokio::task::yield_now().await;
        if index == 0 {
            gate.wait();
        }
        fixture::post(
            &handle,
            &id,
            CommandPayload::SetGeneration(GenerationSelection {
                model: "different".into(),
                effort: None,
            }),
            "idle",
            PostStatus::CommandConflict,
        )
        .await;
    }
    let extra = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle.post(fixture::command(
            &handle,
            "over_capacity",
            CommandPayload::SetGeneration(GenerationSelection {
                model: "excess".into(),
                effort: None,
            }),
            "idle",
        )),
    )
    .await;
    gate.release();
    assert_eq!(
        extra.expect("active selection was omitted").unwrap(),
        PostStatus::QueueFull
    );
    for reply in pending {
        assert_eq!(reply.await.unwrap().unwrap(), PostStatus::Accepted);
    }
    let view = fixture::snapshot(&handle, "generation_capacity_view").await;
    assert!(!view.storage_failed);
    assert_eq!(view.generation.model, "model_3");
    fixture::finish(&mut runtime).await;
}

async fn wait_flag(flag: &AtomicBool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("callback did not start");
}
fn wait_cancel(stop: &Cancel) {
    while !stop.cancelled() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cancels_running_selection_and_does_not_start_queued_validation() {
    let started = Arc::new(AtomicBool::new(false));
    let entered = started.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let checked = count.clone();
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        fixture::generator(0),
        Limits::default(),
        Options {
            select_generation: Arc::new(move |_, stop| {
                checked.fetch_add(1, Ordering::SeqCst);
                entered.store(true, Ordering::SeqCst);
                wait_cancel(stop);
                Err("selection_cancelled")
            }),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let mut pending = Vec::new();
    for index in 0..2 {
        let command = fixture::command(
            &handle,
            &format!("selection_{index}"),
            CommandPayload::SetGeneration(GenerationSelection {
                model: "model".into(),
                effort: None,
            }),
            "idle",
        );
        let sender = handle.clone();
        pending.push(tokio::spawn(async move { sender.post(command).await }));
        tokio::task::yield_now().await;
        if index == 0 {
            wait_flag(&started).await;
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), runtime.shutdown())
        .await
        .expect("selection was detached from shutdown cancellation")
        .unwrap();
    for reply in pending {
        assert_eq!(reply.await.unwrap().unwrap(), PostStatus::Closed);
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "shutdown launched another catalog validation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn storage_failure_cancels_selection_without_committing_generation() {
    let root = fixture::Root::new();
    let provider_started = Arc::new(AtomicBool::new(false));
    let entered_provider = provider_started.clone();
    let provider: Generate = Arc::new(move |_, stop, _| {
        entered_provider.store(true, Ordering::SeqCst);
        wait_cancel(stop);
        Err("generation_cancelled")
    });
    let selection_started = Arc::new(AtomicBool::new(false));
    let entered_selection = selection_started.clone();
    let selection_stopped = Arc::new(AtomicBool::new(false));
    let ended_selection = selection_stopped.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = fixture::setup(
        count.clone(),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(|point, kind| {
                if point == Point::BeforeWrite && kind == "queued" {
                    Err("synthetic_queue_failure")
                } else {
                    Ok(())
                }
            })),
            select_generation: Arc::new(move |_, stop| {
                entered_selection.store(true, Ordering::SeqCst);
                wait_cancel(stop);
                ended_selection.store(true, Ordering::SeqCst);
                Ok(())
            }),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    wait_flag(&provider_started).await;
    let command = fixture::command(
        &handle,
        "selection",
        CommandPayload::SetGeneration(GenerationSelection {
            model: "future_model".into(),
            effort: None,
        }),
        "turn",
    );
    let sender = handle.clone();
    let selecting = tokio::spawn(async move { sender.post(command).await });
    wait_flag(&selection_started).await;
    assert_eq!(
        handle
            .post(fixture::command(
                &handle,
                "message_b",
                CommandPayload::Steer(Steer {
                    target,
                    text: "B".into()
                }),
                "turn"
            ))
            .await,
        Err("synthetic_queue_failure")
    );
    assert_eq!(selecting.await.unwrap(), Err("synthetic_queue_failure"));
    // close_input is explicit; the cancelled callback cannot retain the actor.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture::finish(&mut runtime),
    )
    .await
    .expect("failed storage retained selection callback");
    assert!(selection_stopped.load(Ordering::SeqCst));
    let tools = fixture::tools(count, None, true);
    let mut state = State::new(
        "offline".into(),
        fixture::configuration(&tools),
        tools.read_only().advertisement().clone(),
    );
    let journal = Journal::open(&root.journal(), |event| state.apply(event), None).unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert!(state.generation_commands.is_empty());
    assert_eq!(state.generation.model, "gpt-6.1-sol");
}

#[tokio::test(flavor = "current_thread")]
async fn failed_queue_write_releases_parked_completion_and_preserves_known_tool_result() {
    for tool in [false, true] {
        let root = fixture::Root::new();
        let step_gate = Arc::new(fixture::Gate::default());
        let held_step = step_gate.clone();
        let journal_gate = Arc::new(fixture::Gate::default());
        let held_journal = journal_gate.clone();
        let returned = Arc::new(AtomicBool::new(false));
        let ended = returned.clone();
        let provider: Generate = Arc::new(move |request, stop, _| {
            if !tool {
                held_step.hold(stop);
                ended.store(true, Ordering::SeqCst);
            }
            native::load(&fixture::saved_response(request, None, 1))
        });
        let count = Arc::new(AtomicUsize::new(0));
        let mut runtime = fixture::setup(
            count.clone(),
            if tool { Some(step_gate.clone()) } else { None },
            provider,
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                journal: Some(Arc::new(move |point, kind| {
                    if point == Point::BeforeWrite && kind == "queued" {
                        held_journal.hold(&Cancel::default());
                        return Err("synthetic_queue_failure");
                    }
                    Ok(())
                })),
                ..Options::default()
            },
        )
        .await;
        let handle = runtime.handle();
        let target = fixture::submit(&handle, "submit", "A", "turn").await;
        step_gate.wait();
        let command = fixture::command(
            &handle,
            "message_b",
            CommandPayload::Steer(Steer {
                target,
                text: "B".into(),
            }),
            "turn",
        );
        let sender = handle.clone();
        let queued = tokio::spawn(async move { sender.post(command).await });
        tokio::task::yield_now().await;
        journal_gate.wait();
        step_gate.release();
        if !tool {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !returned.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            fixture::until(&handle, |p| matches!(p, EventPayload::ToolStarted(_))).await;
        }
        // The higher-priority completion is consumed before this snapshot,
        // even though the unrelated queue write still owns the journal slot.
        fixture::snapshot(&handle, "before_failure").await;
        journal_gate.release();
        assert_eq!(queued.await.unwrap(), Err("synthetic_queue_failure"));
        fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
        if tool {
            fixture::until(&handle, |p| matches!(p, EventPayload::ToolFinished(_))).await;
        }
        let view = fixture::snapshot(&handle, "failed_view").await;
        assert!(view.storage_failed && !view.durable);
        if tool {
            assert_eq!(view.completed_tools, 1);
        }
        fixture::finish(&mut runtime).await;
        assert_eq!(count.load(Ordering::SeqCst), usize::from(tool));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn targeted_interrupt_reads_pending_message_but_without_one_stays_stopped() {
    for queued in [false, true] {
        let gate = Arc::new(fixture::Gate::default());
        let held = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let provider: Generate = Arc::new(move |request, stop, _| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                held.hold(stop);
                // A late admitted success must not dispatch its requested tool.
                return native::load(&fixture::saved_response(request, None, 1));
            }
            assert!(has_user(request, "A") && has_user(request, "B"));
            native::load(&fixture::saved_response(request, Some("replanned"), 0))
        });
        let count = Arc::new(AtomicUsize::new(0));
        let mut runtime = fixture::setup(
            count.clone(),
            None,
            provider,
            Limits::default(),
            Options::default(),
        )
        .await;
        let handle = runtime.handle();
        let target = fixture::submit(&handle, "submit", "A", "turn").await;
        gate.wait();
        if queued {
            steer(&handle, "message_b", &target, "B").await;
        }
        fixture::post(
            &handle,
            "interrupt",
            CommandPayload::Cancel(target),
            "turn",
            PostStatus::Accepted,
        )
        .await;
        fixture::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
        assert!(gate.stopped());
        gate.release();
        if queued {
            fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        } else {
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        fixture::finish(&mut runtime).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_message_survives_restart_without_automatic_dispatch() {
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let held = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        held.hold(stop);
        native::load(&fixture::saved_response(request, None, 0))
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(|point, kind| {
                if point == Point::AfterSync && kind == "cancel" {
                    Err("synthetic_ack_loss")
                } else {
                    Ok(())
                }
            })),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    gate.wait();
    steer(&handle, "message_b", &target, "B").await;
    fixture::post(
        &handle,
        "interrupt",
        CommandPayload::Cancel(target.clone()),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    gate.release();
    fixture::finish(&mut runtime).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(has_user(request, "A") && has_user(request, "B"));
        native::load(&fixture::saved_response(request, Some("resumed"), 0))
    });
    let mut runtime = fixture::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let view = fixture::snapshot(&handle, "restart_view").await;
    assert_eq!(view.state, "paused");
    assert_eq!(view.followups[0].state, "queued");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture::post(
        &handle,
        "message_b",
        CommandPayload::Steer(Steer {
            target: target.clone(),
            text: "B".into(),
        }),
        "turn",
        PostStatus::Duplicate,
    )
    .await;
    fixture::post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::finish(&mut runtime).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_tool_effect_keeps_followup_pending_without_replay() {
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let held = gate.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let base = fixture::tools(count.clone(), None, false);
    let mut definition = base.definition("get_test_value").unwrap().clone();
    let observed = count.clone();
    definition.executor = Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        held.hold(&Cancel::default());
        Outcome::from_bytes(Execution::Failed, Effects::Unknown, b"unknown".to_vec())
    });
    let mut registry = Registry::new();
    registry
        .register_with_policy(
            definition,
            Policy {
                requires_journal: true,
                read_only: true,
                ..Policy::default()
            },
        )
        .unwrap();
    let tools = registry.snapshot();
    let mut runtime = Runtime::new(
        "offline".into(),
        fixture::configuration(&tools),
        tools,
        fixture::generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "A", "turn").await;
    gate.wait();
    steer(&handle, "message_b", &target, "B").await;
    gate.release();
    fixture::until(&handle, |p| {
        matches!(p, EventPayload::RecoveryRequired { .. })
    })
    .await;
    let view = fixture::snapshot(&handle, "unknown_view").await;
    assert!(view.recovery_operation.is_some());
    assert_eq!(view.followups[0].state, "queued");
    fixture::finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn generation_selection_is_durable_and_does_not_rebind_active_turn() {
    let root = fixture::Root::new();
    let gate = Arc::new(fixture::Gate::default());
    let held = gate.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let numbered = calls.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        observed
            .lock()
            .unwrap()
            .push((request.binding.clone(), request.effort.clone()));
        if numbered.fetch_add(1, Ordering::SeqCst) == 0 {
            held.hold(stop);
        }
        native::load(&fixture::saved_response(request, Some("answer"), 0))
    });
    let tools = fixture::tools(Arc::new(AtomicUsize::new(0)), None, true);
    let mut configuration = fixture::configuration(&tools);
    configuration.binding.returned_model = "returned_alias".into();
    let initial = configuration.clone();
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration,
        tools.clone(),
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            select_generation: Arc::new(|selection, _| {
                if selection.model == "other_model"
                    && selection.effort.as_deref() == Some("fixture_level")
                {
                    Ok(())
                } else {
                    Err("model_not_available")
                }
            }),
            ..Options::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    fixture::submit(&handle, "submit", "A", "turn").await;
    gate.wait();
    let selection = GenerationSelection {
        model: "other_model".into(),
        effort: Some("fixture_level".into()),
    };
    fixture::post(
        &handle,
        "select",
        CommandPayload::SetGeneration(selection.clone()),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    assert_eq!(
        fixture::snapshot(&handle, "settings_view").await.generation,
        selection
    );
    gate.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::submit(&handle, "submit_second", "second", "second_turn").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::finish(&mut runtime).await;
    let requests = seen.lock().unwrap();
    assert_eq!(requests[0].0, initial.binding);
    assert!(requests[0].1.is_none());
    assert_eq!(requests[1].0.requested_model, "other_model");
    assert_eq!(requests[1].0.returned_model, "other_model");
    assert_eq!(requests[1].1.as_deref(), Some("fixture_level"));
    drop(requests);
    let mut state = State::new(
        "offline".into(),
        initial,
        tools.read_only().advertisement().clone(),
    );
    let journal = Journal::open(&root.journal(), |event| state.apply(event), None).unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert_eq!(state.generation, selection);
    assert_ne!(state.epoch.id, state.configuration.epoch.id);
}

#[tokio::test(flavor = "current_thread")]
async fn fork_has_verified_native_context_but_no_source_execution_or_receipts() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut source = fixture::setup(
        count.clone(),
        None,
        fixture::generator(1),
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = source.handle();
    fixture::submit(&handle, "submit", "A", "turn").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::post(
        &handle,
        "context",
        CommandPayload::GetContext,
        "query",
        PostStatus::Accepted,
    )
    .await;
    let context = fixture::until(&handle, |p| {
        matches!(p, EventPayload::ContextSnapshot { .. })
    })
    .await;
    let EventPayload::ContextSnapshot { seed, .. } = context.payload else {
        unreachable!()
    };
    assert_eq!(seed.source_session(), handle.session());
    let prefix = seed.request().unwrap();
    assert!(prefix.input.iter().any(|v| {
        v.has("type")
            && v.text_field("type")
                .is_ok_and(|t| t == "function_call_output")
    }));
    let tools = fixture::tools(count.clone(), None, false);
    let initial = prefix.clone().with_input(vec![]).unwrap();
    let mut child = Runtime::new(
        "fork_child".into(),
        initial,
        tools.clone(),
        Arc::new(|request, _, _| native::load(&fixture::saved_response(request, Some("child"), 0))),
        Limits::default(),
        Options {
            fork_seed: Some(seed.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    child.ready().await.unwrap();
    assert_eq!(
        fixture::snapshot(&child.handle(), "child_view")
            .await
            .transcript_size,
        2
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let request = fixture::submit(&child.handle(), "submit", "B", "child_turn").await;
    assert_eq!(request.0, "request_1");
    fixture::until(&child.handle(), |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(
        fixture::snapshot(&handle, "source_view")
            .await
            .transcript_size,
        2
    );
    fixture::finish(&mut child).await;
    fixture::finish(&mut source).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let mut wider = tools.advertisement().clone();
    wider.descriptors[0].description.push_str("new descriptor");
    assert!(seed.restricted_tools(&wider).is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn seeded_init_is_atomic_at_fsync_ack_loss_and_rejects_incomplete_v2() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut source = fixture::setup(
        count.clone(),
        None,
        fixture::generator(1),
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = source.handle();
    fixture::submit(&handle, "submit", "A", "turn").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    fixture::post(
        &handle,
        "context",
        CommandPayload::GetContext,
        "query",
        PostStatus::Accepted,
    )
    .await;
    let event = fixture::until(&handle, |p| {
        matches!(p, EventPayload::ContextSnapshot { .. })
    })
    .await;
    let EventPayload::ContextSnapshot { seed, .. } = event.payload else {
        unreachable!()
    };
    let tools = fixture::tools(count.clone(), None, false);
    let configuration = seed.request().unwrap().with_input(vec![]).unwrap();
    let root = fixture::Root::new();
    let mut interrupted = Runtime::new(
        "child".into(),
        configuration.clone(),
        tools.clone(),
        fixture::generator(0),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            fork_seed: Some(seed),
            journal: Some(Arc::new(|point, kind| {
                if point == Point::AfterSync && kind == "init" {
                    Err("synthetic_init_ack_loss")
                } else {
                    Ok(())
                }
            })),
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(interrupted.ready().await, Err("synthetic_init_ack_loss"));
    interrupted.shutdown().await.unwrap();
    let mut state = State::new(
        "child".into(),
        configuration.clone(),
        tools.read_only().advertisement().clone(),
    );
    let mut records = Vec::new();
    let journal = Journal::open(
        &root.journal(),
        |record| {
            state.apply(record)?;
            records.push(record.clone());
            Ok(())
        },
        None,
    )
    .unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert_eq!(records.len(), 1, "fork requires one durable first record");
    assert_eq!(records[0].usize_field("version").unwrap(), 2);
    assert!(records[0].has("fork_seed"));
    assert_eq!(state.fork_source.as_ref(), Some(handle.session()));
    assert_eq!(state.transcript().len(), 2);
    assert!(state.history.iter().any(|item| {
        item.has("type")
            && item
                .text_field("type")
                .is_ok_and(|t| t == "function_call_output")
    }));
    drop(journal);
    let mut invalid = State::new(
        "child".into(),
        configuration.clone(),
        tools.read_only().advertisement().clone(),
    );
    let incomplete = records[0].without("fork_seed").unwrap();
    assert!(invalid.apply(&incomplete).is_err());
    assert!(!invalid.initialized && invalid.history.is_empty());
    let misleading = records[0]
        .with("version", Json::parse("1").unwrap())
        .unwrap();
    assert!(invalid.apply(&misleading).is_err());
    assert!(!invalid.initialized && invalid.history.is_empty());
    let called = Arc::new(AtomicUsize::new(0));
    let observed = called.clone();
    let provider: Generate = Arc::new(move |request, _, _| {
        observed.fetch_add(1, Ordering::SeqCst);
        assert!(has_user(request, "A"));
        native::load(&fixture::saved_response(
            request,
            Some("continued child"),
            0,
        ))
    });
    let mut recovered = Runtime::new(
        "child".into(),
        configuration,
        tools,
        provider,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .unwrap();
    recovered.ready().await.unwrap();
    assert_eq!(
        fixture::snapshot(&recovered.handle(), "recovered_child")
            .await
            .transcript_size,
        2
    );
    assert_eq!(called.load(Ordering::SeqCst), 0);
    fixture::submit(&recovered.handle(), "submit", "B", "child_turn").await;
    fixture::until(&recovered.handle(), |p| matches!(p, EventPayload::Done)).await;
    fixture::finish(&mut recovered).await;
    fixture::finish(&mut source).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn capabilities_require_explicit_catalog_metadata_and_legacy_effort_is_absent() {
    let catalog = Json::parse(r#"{"models":[{"slug":"gpt-6.1-sol","visibility":"list","display_name":"One","context_window":1000000,"max_context_window":2000000},{"slug":"second","visibility":"list","context_window":null,"max_context_window":120000,"supported_reasoning_levels":[{"effort":"proven"},"also_proven"]},{"slug":"hidden","visibility":"hide"}]}"#).unwrap();
    let values = ansa::auth::dispatch::model_capabilities(&catalog).unwrap();
    assert_eq!(values.len(), 2);
    assert!(values[0].efforts.is_empty());
    assert_eq!(values[0].context_window, Some(1000000));
    assert_eq!(values[1].efforts, vec!["proven", "also_proven"]);
    assert_eq!(values[1].context_window, Some(120000));
    let tools = fixture::tools(Arc::new(AtomicUsize::new(0)), None, false);
    let request = fixture::configuration(&tools);
    assert!(!request.save().unwrap().has("effort"));
    assert!(!request.json().unwrap().has("reasoning"));
    let with = request.with_effort(Some("proven".into())).unwrap();
    assert_eq!(Request::load(&with.save().unwrap()).unwrap(), with);
    assert_eq!(
        with.json()
            .unwrap()
            .field("reasoning")
            .unwrap()
            .text_field("effort")
            .unwrap(),
        "proven"
    );
}
