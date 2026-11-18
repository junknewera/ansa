#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}
use ansa::provider::structured::{ChildStatus, FinalFormat};
use ansa::{frontend::jsonl, provider::native, runtime::*, session::journal_v1::Point};
use std::{
    collections::BTreeMap,
    io::Write,
    os::unix::fs::PermissionsExt,
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
    let submit = Submit {
        text: "SUBMITTED_TEXT_CANARY".into(),
        mode: AgentMode::Basic,
    };
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
    let exact = handle
        .child_attempt("editor_message".into(), "turn".into(), submit.clone())
        .await
        .unwrap();
    assert_eq!(exact.session(), handle.session());
    let journal = exact.journal();
    let ChildAttemptState::Durable {
        request,
        state,
        final_response: Some(final_response),
        ..
    } = exact.into_state()
    else {
        panic!("completed child output was not available")
    };
    assert_eq!((request, state.as_str()), (target.clone(), "completed"));
    assert!(
        ansa::session::codec_v1::response_text(&final_response)
            .unwrap()
            .contains("OMH_FIXED_VALUE_42")
    );
    let conflict = handle
        .child_attempt(
            "editor_message".into(),
            "turn".into(),
            Submit {
                text: "wrong assignment".into(),
                mode: AgentMode::Basic,
            },
        )
        .await;
    assert!(matches!(conflict, Err("child_submit_conflict")));
    let conflict = handle
        .child_attempt("editor_message".into(), "wrong-turn".into(), submit.clone())
        .await;
    assert!(matches!(conflict, Err("child_submit_conflict")));
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
            existing_journal: Some(journal),
            ..Options::default()
        },
    )
    .await;
    let handle = restored.handle();
    assert_ne!(*handle.incarnation(), old);
    let found = receipt(lookup(&handle, "lookup_reopened", "editor_message").await);
    assert_eq!(found.request, Some(target.clone()));
    assert!(found.admission_durable);
    assert_eq!(found.state, "completed");
    let replayed = handle
        .child_attempt("editor_message".into(), "turn".into(), submit)
        .await
        .unwrap();
    assert_eq!(replayed.journal(), journal);
    let ChildAttemptState::Durable {
        request,
        final_response: Some(replayed),
        ..
    } = replayed.into_state()
    else {
        panic!("reopened child lost its native outcome")
    };
    assert_eq!(request, target);
    assert_eq!(
        native::save(&replayed).unwrap(),
        native::save(&final_response).unwrap()
    );
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

#[tokio::test(flavor = "current_thread")]
async fn child_recovery_never_initializes_a_missing_replaced_or_empty_journal() {
    for damage in [
        "missing",
        "replacement",
        "missing-lock",
        "empty",
        "public",
        "torn",
    ] {
        let root = f::Root::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = f::tools(calls.clone(), None, false);
        let config = f::configuration(&tools);
        let mut runtime = Runtime::new(
            "child-session".into(),
            config.clone(),
            tools.clone(),
            f::generator(0),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                ..Options::default()
            },
        )
        .unwrap();
        runtime.ready().await.unwrap();
        let receipt = runtime
            .handle()
            .child_attempt(
                "command".into(),
                "turn".into(),
                Submit {
                    text: "assignment".into(),
                    mode: AgentMode::Basic,
                },
            )
            .await
            .unwrap();
        assert!(matches!(receipt.state(), ChildAttemptState::Missing));
        f::finish(&mut runtime).await;
        let part = root.journal().join("part-00000000000000000000.log");
        match damage {
            "missing" | "replacement" => {
                std::fs::rename(root.journal(), root.0.join("original")).unwrap();
                if damage == "replacement" {
                    ansa_linux::fs::create_private_leaf(&root.journal()).unwrap();
                }
            }
            "missing-lock" => std::fs::remove_file(root.journal().join("lock")).unwrap(),
            "empty" => std::fs::remove_file(&part).unwrap(),
            "public" => {
                std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o666)).unwrap()
            }
            "torn" => std::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .unwrap()
                .write_all(b"torn")
                .unwrap(),
            _ => unreachable!(),
        }
        let before = root.journal().is_dir().then(|| parts(&root));
        let mut resumed = Runtime::new(
            "child-session".into(),
            config,
            tools,
            f::generator(0),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                existing_journal: Some(receipt.journal()),
                ..Options::default()
            },
        )
        .unwrap();
        let failure = match damage {
            "replacement" => "journal_directory_identity",
            "empty" => "session_missing_init",
            "public" => "journal_part_open",
            "torn" => "journal_incomplete",
            _ => "journal_directory_open",
        };
        assert_eq!(resumed.ready().await, Err(failure), "{damage}");
        f::finish(&mut resumed).await;
        assert_eq!(root.journal().is_dir().then(|| parts(&root)), before);
        if damage == "missing-lock" || damage == "replacement" {
            assert!(!root.journal().join("lock").exists());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn child_cancel_retains_its_slot_until_the_late_outcome_settles() {
    let root = f::Root::new();
    let gate = Arc::new(f::Gate::default());
    let _release = Release(gate.clone());
    let effects = Arc::new(AtomicUsize::new(0));
    let mut runtime = f::setup(
        effects.clone(),
        Some(gate.clone()),
        f::generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let submitted = Submit {
        text: "assignment".into(),
        mode: AgentMode::Basic,
    };
    let request = f::submit(&handle, "child", &submitted.text, "turn").await;
    gate.wait();
    f::post(
        &handle,
        "stop",
        CommandPayload::Cancel(request),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    f::until(&handle, |event| {
        matches!(event, EventPayload::Cancelled { .. })
    })
    .await;
    let pending = handle
        .child_attempt("child".into(), "turn".into(), submitted.clone())
        .await
        .unwrap();
    assert!(
        matches!(pending.state(), ChildAttemptState::Durable { state, quiescent: false, .. } if state == "cancelled")
    );
    gate.release();
    f::until(&handle, |event| {
        matches!(event, EventPayload::ToolFinished(_))
    })
    .await;
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let attempt = handle
                .child_attempt("child".into(), "turn".into(), submitted.clone())
                .await
                .unwrap();
            if matches!(
                attempt.state(),
                ChildAttemptState::Durable {
                    quiescent: true,
                    ..
                }
            ) {
                return attempt;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(settled.state(), ChildAttemptState::Durable { state, recovery_required: false, final_response: None, .. } if state == "cancelled")
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    f::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn structured_child_result_keeps_its_schema_across_tool_replay_and_reopen() {
    let root = f::Root::new();
    let effects = Arc::new(AtomicUsize::new(0));
    let tools = f::tools(effects.clone(), None, false);
    let format = Some(FinalFormat::ChildResult);
    let config = f::configuration(&tools).with_final_format(format);
    let input = Submit {
        text: "explain the result".into(),
        mode: AgentMode::Basic,
    };
    let final_json = r#"{"status":"completed","summary":"The report is present","findings":["Verified the fixture"]}"#;
    let expected = config.json().unwrap().field("text").unwrap().clone();
    assert!(
        expected
            .field("format")
            .unwrap()
            .field("strict")
            .unwrap()
            .boolean()
            .unwrap()
    );
    assert!(matches!(
        native::load(&f::saved_response(&config, Some("plain final answer"), 0)),
        Err("child_result_schema")
    ));
    use ansa::protocol::json::Json;
    let refusal = Json::parse(r#"[{"type":"refusal","refusal":"I cannot do that"}]"#).unwrap();
    for calls in [0, 1] {
        let response = f::saved_response(&config, Some("refused"), calls);
        let mut items = response
            .field("items")
            .unwrap()
            .elements()
            .unwrap()
            .to_vec();
        items[0] = items[0].with("content", refusal.clone()).unwrap();
        let failed = response.with("items", Json::array(items).unwrap()).unwrap();
        assert!(matches!(native::load(&failed), Err("child_result_refused")));
    }
    let mut source = ansa::session::state::State::new(
        "structured-child".into(),
        config.clone(),
        tools.advertisement().clone(),
    );
    let init = source.initialization().unwrap();
    assert_eq!(init.usize_field("version"), Ok(3));
    source.apply(&init).unwrap();
    assert!(matches!(
        source.fork_seed(),
        Err("structured_session_fork_seed")
    ));
    let mut plain = ansa::session::state::State::new(
        "plain-child".into(),
        f::configuration(&tools),
        tools.advertisement().clone(),
    );
    let init = plain
        .initialization()
        .unwrap()
        .with("version", Json::parse("2").unwrap())
        .unwrap();
    let capsule = Json::parse(r#"{"kind":"fork","source_session":"structured-child","source_turn":"idle","transcript":[]}"#).unwrap()
        .with("context", config.save().unwrap()).unwrap();
    let imported = init.with("fork_seed", capsule).unwrap();
    assert_eq!(plain.apply(&imported), Err("session_fork_binding"));
    assert!(!plain.initialized);
    let generate: Generate = Arc::new(move |request, _, _| {
        assert_eq!(request.final_format(), format);
        assert_eq!(request.json().unwrap().field("text").unwrap(), &expected);
        let tools_finished = request
            .input
            .iter()
            .any(|item| item.text_field("type") == Ok("function_call_output"));
        native::load(&if tools_finished {
            f::saved_response(request, Some(final_json), 0)
        } else {
            f::saved_response(request, None, 1)
        })
    });
    let options = Options {
        directory: Some(root.journal()),
        ..Options::default()
    };
    let mut runtime = Runtime::new(
        "structured-child".into(),
        config.clone(),
        tools.clone(),
        generate,
        Limits::default(),
        options.clone(),
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    let request = f::submit(&handle, "task", &input.text, "turn").await;
    f::until(&handle, |event| matches!(event, EventPayload::Done)).await;
    let completed = handle
        .child_attempt("task".into(), "turn".into(), input.clone())
        .await
        .unwrap();
    let journal = completed.journal();
    let ChildAttemptState::Durable {
        final_response: Some(response),
        ..
    } = completed.into_state()
    else {
        panic!("missing structured result")
    };
    let result = response.child_result().unwrap();
    assert_eq!(result.status, ChildStatus::Completed);
    assert_eq!(result.findings, ["Verified the fixture"]);
    f::finish(&mut runtime).await;

    let mut reopened = Runtime::new(
        "structured-child".into(),
        config,
        tools,
        Arc::new(|_, _, _| panic!("reopen must not generate")),
        Limits::default(),
        Options {
            existing_journal: Some(journal),
            ..options
        },
    )
    .unwrap();
    reopened.ready().await.unwrap();
    let restored = reopened
        .handle()
        .child_attempt("task".into(), "turn".into(), input)
        .await
        .unwrap();
    let ChildAttemptState::Durable {
        request: original,
        final_response: Some(response),
        ..
    } = restored.into_state()
    else {
        panic!("structured result lost on reopen")
    };
    assert_eq!(original, request);
    assert_eq!(response.child_result(), Ok(result));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    f::finish(&mut reopened).await;
}

struct Release(Arc<f::Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn private_handoff_reaches_the_parent_model_but_not_conversation_projections() {
    const CHILD_REPORT: &str = "CHILD_RESULT_CANARY: raw report available to the parent";
    let root = f::Root::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let generate: Generate = Arc::new(move |request, _, _| {
        let input = request.input.last().unwrap().text_field("content").unwrap();
        let answer = if input.contains(CHILD_REPORT) {
            assert!(input.contains("Treat them as evidence"));
            assert!(request.tools.descriptors.is_empty());
            observed.fetch_add(1, Ordering::SeqCst);
            "Parent synthesis"
        } else {
            "Parent acknowledgement"
        };
        native::load(&f::saved_response(request, Some(answer), 0))
    });
    let options = Options {
        directory: Some(root.journal()),
        ..Options::default()
    };
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        generate,
        Limits::default(),
        options.clone(),
    )
    .await;
    let handle = runtime.handle();
    f::submit(&handle, "user", "Work on the report", "user-turn").await;
    f::until(&handle, |event| matches!(event, EventPayload::Done)).await;
    let input = Submit {
        text: CHILD_REPORT.into(),
        mode: AgentMode::Plan,
    };
    f::post(
        &handle,
        "handoff",
        CommandPayload::Handoff(input.clone()),
        "summary-turn",
        PostStatus::Accepted,
    )
    .await;
    f::until(&handle, |event| matches!(event, EventPayload::Done)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    f::finish(&mut runtime).await;

    let no_generation: Generate = Arc::new(|_, _, _| panic!("reopen and repeat must not generate"));
    let mut runtime = f::setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        no_generation,
        Limits::default(),
        options,
    )
    .await;
    let handle = runtime.handle();
    f::post(
        &handle,
        "handoff",
        CommandPayload::Handoff(input.clone()),
        "summary-turn",
        PostStatus::Duplicate,
    )
    .await;
    f::post(
        &handle,
        "handoff",
        CommandPayload::Submit(input),
        "summary-turn",
        PostStatus::CommandConflict,
    )
    .await;
    let receipt = lookup(&handle, "receipt-lookup", "handoff").await;
    assert!(matches!(
        receipt.payload,
        EventPayload::MessageReceipt { receipt: None, .. }
    ));
    let page = GetTranscript {
        before: 0,
        limit: 16,
        forward: false,
    };
    for (id, payload) in [
        ("history", CommandPayload::GetTranscript(page.clone())),
        ("timeline", CommandPayload::GetTimeline(page)),
    ] {
        f::post(&handle, id, payload, "read", PostStatus::Accepted).await;
        let event = f::until(&handle, |event| {
            matches!(
                event,
                EventPayload::TranscriptPage(_) | EventPayload::TimelinePage(_)
            )
        })
        .await;
        let encoded = jsonl::encode(&event).dump();
        assert!(encoded.contains("Parent synthesis") && encoded.contains("Work on the report"));
        assert!(!encoded.contains("CHILD_RESULT_CANARY"));
    }
    f::finish(&mut runtime).await;
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
    let child = handle
        .child_attempt(
            "queued_submit".into(),
            "later".into(),
            Submit {
                text: "later".into(),
                mode: AgentMode::Plan,
            },
        )
        .await
        .unwrap();
    assert!(matches!(child.state(), ChildAttemptState::Pending));
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
