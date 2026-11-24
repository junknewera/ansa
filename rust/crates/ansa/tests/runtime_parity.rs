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
    tool.validator = Arc::new(|_| panic!("fixture validator failed"));
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
    fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { diagnostic, .. } if diagnostic == "tool_validator_panicked")).await;
    let view = fixture::snapshot(&handle, "check-note").await;
    assert_eq!(view.state, "paused");
    assert_eq!(view.preview.unwrap().state, "committed");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    fixture::finish(&mut runtime).await;
}

fn invalid_second_call(request: &ansa::provider::request::Request) -> ansa::protocol::json::Json {
    use ansa::protocol::json::Json;
    let response = fixture::saved_response(request, Some("Checking both calls"), 2);
    let mut items = response
        .field("items")
        .unwrap()
        .elements()
        .unwrap()
        .to_vec();
    items[2] = items[2]
        .with("arguments", Json::string(r#"{"extra":true}"#).unwrap())
        .unwrap();
    response.with("items", Json::array(items).unwrap()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_batch_replays_both_outputs_before_executing_a_corrected_call() {
    use ansa::{protocol::json::Json, provider::native, runtime::*, session::journal_v1::Journal};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let root = fixture::Root::new();
    let writes = Arc::new(AtomicUsize::default());
    let corrected_request = Arc::new(Mutex::new(None));
    let saved = corrected_request.clone();
    let generate: Generate = Arc::new(move |request, _, _| {
        let outputs: Vec<_> = request
            .input
            .iter()
            .filter(|v| v.text_field("type") == Ok("function_call_output"))
            .collect();
        match outputs.len() {
            0 => native::load(&invalid_second_call(request)),
            2 => {
                for (index, expected) in ["tool_batch_not_executed", "tool_arguments_rejected"]
                    .into_iter()
                    .enumerate()
                {
                    assert_eq!(
                        outputs[index].text_field("call_id")?,
                        format!("call{index}")
                    );
                    let error = Json::parse(outputs[index].text_field("output")?)?;
                    assert_eq!(error.text_field("error")?, expected);
                    assert!(!error.field("applied")?.boolean()?);
                    if index == 1 {
                        assert!(!error.text_field("message")?.is_empty());
                    }
                }
                let mut before = saved.lock().unwrap();
                if let Some(expected) = before.as_ref() {
                    assert_eq!(expected, &request.save()?);
                } else {
                    *before = Some(request.save()?);
                    return Err("fixture_provider_interrupted");
                }
                native::load(&fixture::response_with_offset(request, None, 1, 2))
            }
            3 => native::load(&fixture::saved_response(request, Some("Corrected"), 0)),
            _ => panic!("unexpected continuation"),
        }
    });
    let options = || Options {
        directory: Some(root.journal()),
        ..Default::default()
    };
    let mut runtime = fixture::setup(
        writes.clone(),
        None,
        generate.clone(),
        Limits::default(),
        options(),
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "fixture", "turn").await;
    fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { diagnostic, .. } if diagnostic == "fixture_provider_interrupted")).await;
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    fixture::finish(&mut runtime).await;

    let mut runtime =
        fixture::setup(writes.clone(), None, generate, Limits::default(), options()).await;
    let handle = runtime.handle();
    assert_eq!(fixture::snapshot(&handle, "restored").await.state, "paused");
    fixture::post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = fixture::snapshot(&handle, "result").await;
    assert_eq!(view.text, "Corrected");
    assert_eq!(view.completed_tools, 1);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    fixture::finish(&mut runtime).await;
    let mut rejections = 0;
    let mut intents = 0;
    let journal = Journal::open(
        &root.journal(),
        |event| {
            match event.text_field("kind")? {
                "tool_batch_rejected" => {
                    rejections += 1;
                    assert_eq!(event.text_field("call_id")?, "call1");
                }
                "intent" => {
                    intents += 1;
                    let intent = event.field("invocation")?;
                    assert_eq!(intent.text_field("call")?, "call2");
                    assert!(intent.text_field("operation")?.ends_with(":op:3"));
                }
                _ => {}
            }
            Ok(())
        },
        None,
    )
    .unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert_eq!((rejections, intents), (1, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn work_rereads_and_corrects_a_rejected_edit_in_the_same_turn() {
    use ansa::{
        config::{agent_epoch, load_agent_settings},
        protocol::{json::Json, legacy_json_v1::sha256},
        provider::{
            native,
            replay::{Effects, Execution},
            request::Request,
        },
        runtime::*,
        tools::files::local_tools,
    };
    use std::{fs, sync::Arc};

    let root = fixture::Root::new();
    let home = root.0.join("home");
    let cwd = home.join("work");
    fs::create_dir_all(&cwd).unwrap();
    let file = cwd.join("report.txt");
    fs::write(&file, "current\n").unwrap();
    let settings = load_agent_settings(&cwd, &home).unwrap();
    let epoch = agent_epoch(&settings).unwrap();
    let token = sha256(
        epoch
            .exact_state
            .field("instructions")
            .unwrap()
            .dump()
            .as_bytes(),
    );
    let tools = local_tools(
        settings,
        epoch.clone(),
        &root.journal(),
        &home.join("auth"),
        &std::env::current_exe().unwrap(),
    )
    .unwrap();
    let configuration = Request::new(
        fixture::configuration(&tools).binding,
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let edited = file.clone();
    let generate: Generate = Arc::new(move |request, _, _| {
        let outputs = request
            .input
            .iter()
            .filter(|item| item.text_field("type") == Ok("function_call_output"))
            .collect::<Vec<_>>();
        let edit = |old: &str| {
            Json::object(vec![
                ("path", Json::string("report.txt")?),
                ("old_text", Json::string(old)?),
                ("new_text", Json::string("corrected")?),
                ("instruction_token", Json::string(&token)?),
            ])
        };
        let (name, args) = match outputs.as_slice() {
            [] => ("edit_file", edit("stale")?),
            [rejected] => {
                let outcome = Json::parse(rejected.text_field("output")?)?;
                assert_eq!(outcome.text_field("error")?, "file_preimage_missing");
                assert!(!outcome.field("applied")?.boolean()?);
                assert_eq!(fs::read_to_string(&edited).unwrap(), "current\n");
                (
                    "read_file",
                    Json::object(vec![
                        ("path", Json::string("report.txt")?),
                        ("offset", Json::parse("0")?),
                        ("max_bytes", Json::parse("128")?),
                        ("instruction_token", Json::string(&token)?),
                    ])?,
                )
            }
            [_, read] => {
                let outcome = Json::parse(read.text_field("output")?)?;
                assert_eq!(outcome.text_field("content")?, "current\n");
                ("edit_file", edit("current")?)
            }
            [_, _, applied] => {
                let outcome = Json::parse(applied.text_field("output")?)?;
                assert!(outcome.field("applied")?.boolean()?);
                return native::load(&fixture::saved_response(request, Some("Corrected"), 0));
            }
            _ => panic!("unexpected continuation"),
        };
        let response = fixture::response_with_offset(request, None, 1, outputs.len());
        let call = response.field("items")?.elements()?[0]
            .with("name", Json::string(name)?)?
            .with("arguments", Json::string(&args.dump())?)?;
        native::load(&response.with("items", Json::array(vec![call])?)?)
    });
    let mut runtime = Runtime::new(
        "edit-recovery".into(),
        configuration,
        tools,
        generate,
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    let request = fixture::submit(&handle, "submit", "Correct the report", "turn").await;
    let event = fixture::until(&handle, |p| matches!(p, EventPayload::ToolFinished(_))).await;
    assert_eq!(event.request.as_ref(), Some(&request));
    let EventPayload::ToolFinished(rejected) = event.payload else {
        unreachable!()
    };
    assert_eq!(rejected.call_id, "call0");
    assert_eq!(rejected.execution, Execution::Failed);
    assert_eq!(rejected.effects, Effects::KnownResult);
    let event = fixture::until(&handle, |p| matches!(p, EventPayload::Done)).await;
    assert_eq!(event.request.as_ref(), Some(&request));
    let completed = fixture::snapshot(&handle, "result").await;
    assert_eq!(completed.text, "Corrected");
    assert_eq!(completed.completed_tools, 3);
    assert_eq!(fs::read_to_string(file).unwrap(), "corrected\n");
    fixture::finish(&mut runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_batch_reserves_response_room_using_the_measured_context_after_reopen() {
    use ansa::{protocol::json::Json, provider::native, runtime::*};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = fixture::Root::new();
    let calls = Arc::new(AtomicUsize::default());
    let generations = Arc::new(AtomicUsize::default());
    let count = generations.clone();
    let generate: Generate = Arc::new(move |request, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        native::load(
            &invalid_second_call(request)
                .with("context_tokens", Json::parse("7500")?)?
                .with("context_window", Json::parse("10000")?)?,
        )
    });
    let start = || {
        fixture::setup(
            calls.clone(),
            None,
            generate.clone(),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                ..Options::default()
            },
        )
    };
    let mut runtime = start().await;
    let target = fixture::submit(&runtime.handle(), "submit", "fixture", "turn").await;
    fixture::until(&runtime.handle(), |p| matches!(p, EventPayload::RuntimeError { diagnostic, .. } if diagnostic == "model_context_limit")).await;
    fixture::finish(&mut runtime).await;
    let mut runtime = start().await;
    let handle = runtime.handle();
    fixture::post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    fixture::until(&handle, |p| matches!(p, EventPayload::RuntimeError { diagnostic, .. } if diagnostic == "model_context_limit")).await;
    fixture::finish(&mut runtime).await;
    assert_eq!(generations.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_during_batch_rejection_prevents_the_next_provider_request() {
    use ansa::{auth::Cancel, provider::native, runtime::*, session::journal_v1::Point};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let root = fixture::Root::new();
    let sync = Arc::new(fixture::Gate::default());
    let hold = sync.clone();
    let writes = Arc::new(AtomicUsize::default());
    let generations = Arc::new(AtomicUsize::default());
    let count = generations.clone();
    let mut runtime = fixture::setup(
        writes.clone(),
        None,
        Arc::new(move |request, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            native::load(&invalid_second_call(request))
        }),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(Arc::new(move |point, kind| {
                if point == Point::BeforeSync && kind == "tool_batch_rejected" {
                    hold.hold(&Cancel::default());
                }
                Ok(())
            })),
            ..Default::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = fixture::submit(&handle, "submit", "fixture", "turn").await;
    sync.wait();
    fixture::post(
        &handle,
        "cancel",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    sync.release();
    fixture::until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    fixture::finish(&mut runtime).await;
    assert_eq!(generations.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
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
