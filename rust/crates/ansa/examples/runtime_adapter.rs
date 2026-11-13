//! Opt-in synthetic runtime evidence adapter; no live provider or credential IO.
#[allow(dead_code)]
#[path = "../tests/support/runtime_fixture.rs"]
mod fixture;
use ansa::{
    protocol::json::Json,
    provider::replay::{Effects, Execution},
    runtime::*,
    session::{journal_v1::Journal, state::State},
};
use std::{
    io::{self, BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
fn s(v: &str) -> Json {
    Json::string(v).unwrap()
}
fn n(v: usize) -> Json {
    Json::parse(&v.to_string()).unwrap()
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).unwrap()
}
fn null() -> Json {
    Json::parse("null").unwrap()
}
fn object(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).unwrap()
}
fn mode(v: AgentMode) -> &'static str {
    if v == AgentMode::Plan {
        "plan"
    } else {
        "basic"
    }
}
fn event(e: &RuntimeEvent) -> Option<Json> {
    validate_event(e).unwrap();
    let mut fields = vec![
        ("sequence", n(e.stream_sequence as usize)),
        ("session", s(&e.session.0)),
        ("turn", s(&e.turn.0)),
        ("incarnation", s(&e.incarnation.0)),
        ("request", e.request.as_ref().map_or_else(null, |v| s(&v.0))),
    ];
    match &e.payload {
        EventPayload::CommandAccepted { command, .. } => {
            fields.extend([("type", s("accepted")), ("command", s(&command.0))])
        }
        EventPayload::ToolStarted(v) => fields.extend([
            ("type", s("started")),
            ("operation", s(&v.operation.0)),
            ("call", s(&v.call_id)),
            ("name", s(&v.name)),
        ]),
        EventPayload::ToolFinished(v) => fields.extend([
            ("type", s("finished")),
            ("operation", s(&v.operation.0)),
            ("call", s(&v.call_id)),
            (
                "execution",
                s(match v.execution {
                    Execution::Succeeded => "succeeded",
                    Execution::Failed => "failed",
                    Execution::Cancelled => "cancelled",
                    Execution::Denied => "denied",
                }),
            ),
            (
                "effects",
                s(match v.effects {
                    Effects::KnownResult => "known_result",
                    Effects::NotStarted => "not_started",
                    Effects::Unknown => "unknown",
                }),
            ),
            ("output", s(&v.output)),
        ]),
        EventPayload::TextDelta { text } => fields.extend([("type", s("text")), ("text", s(text))]),
        EventPayload::Done => fields.push(("type", s("done"))),
        EventPayload::RuntimeError { diagnostic, .. } => {
            fields.extend([("type", s("error")), ("diagnostic", s(diagnostic))])
        }
        EventPayload::FrontendSnapshot(v) => {
            assert_eq!(v.through_sequence, e.stream_sequence);
            return None;
        }
        EventPayload::TranscriptPage(_) => return None,
        _ => panic!("unexpected runtime trace event: {e:?}"),
    }
    Some(object(fields))
}
async fn trace(row: &Json) -> Result<Json> {
    let calls = row.usize_field("calls")?;
    let turns = row.usize_field("turns")?;
    let start = if row.has("start") {
        row.usize_field("start")?
    } else {
        0
    };
    if calls > 3 || !(1..=4).contains(&turns) || start > 16 {
        return Err("runtime_fixture_limits");
    }
    let selected = match row.text_field("mode")? {
        "basic" => AgentMode::Basic,
        "plan" => AgentMode::Plan,
        _ => return Err("runtime_fixture_mode"),
    };
    let directory = if row.has("root") {
        let root = std::path::Path::new(row.text_field("root")?);
        ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
        Some(root.join("journal"))
    } else {
        None
    };
    let count = Arc::new(AtomicUsize::new(0));
    let tools = fixture::tools(count.clone(), None, directory.is_some());
    let requests = Arc::new(Mutex::new(vec![]));
    let saved = requests.clone();
    let generate: Generate = Arc::new(move |request, _, _| {
        saved.lock().unwrap().push(request.save()?);
        if calls == 0 {
            ansa::provider::native::load(&fixture::saved_response(request, Some("ANSWER"), 0))
        } else {
            fixture::model(request, calls)
        }
    });
    let journal = if row.has("fault") {
        use ansa::session::journal_v1::{Observer, Point};
        let fault = row.field("fault")?;
        let kind = fault.text_field("kind")?.to_owned();
        let point = fault.text_field("point")?.to_owned();
        let action = fault.text_field("action")?.to_owned();
        Some(Arc::new(move |p, k: &str| {
            let name = match p {
                Point::BeforeWrite => "before_write",
                Point::AfterWrite => "after_write",
                Point::BeforeSync => "before_sync",
                Point::AfterSync => "after_sync",
            };
            if k == kind && name == point {
                if action == "exit" {
                    std::process::exit(71);
                }
                return Err("journal_injected_io");
            }
            Ok(())
        }) as Observer)
    } else {
        None
    };
    let mut runtime = Runtime::new(
        "offline".into(),
        fixture::configuration(&tools),
        tools.clone(),
        generate,
        Limits::default(),
        Options {
            directory: directory.clone(),
            journal,
            ..Options::default()
        },
    )?;
    runtime.ready().await?;
    let handle = runtime.handle();
    let mut events = vec![];
    let mut final_snapshot = None;
    let mut consume = |e: RuntimeEvent| {
        if let Some(value) = event(&e) {
            events.push(value);
        }
        if let EventPayload::FrontendSnapshot(v) = e.payload {
            final_snapshot = Some(*v);
        }
    };
    for index in start..start + turns {
        let id = format!("turn{index}");
        fixture::post(
            &handle,
            &id,
            CommandPayload::Submit(Submit {
                text: "fixture".into(),
                mode: selected,
            }),
            &id,
            PostStatus::Accepted,
        )
        .await;
        loop {
            let e = fixture::event(&handle).await;
            let done = matches!(e.payload, EventPayload::Done);
            assert!(
                !matches!(e.payload, EventPayload::RuntimeError { .. }),
                "runtime trace failed: {e:?}"
            );
            consume(e);
            if done {
                break;
            }
        }
    }
    fixture::post(
        &handle,
        "query",
        CommandPayload::GetTranscript(GetTranscript {
            before: 0,
            limit: 64,
            forward: false,
        }),
        "query",
        PostStatus::Accepted,
    )
    .await;
    let page = loop {
        let e = fixture::event(&handle).await;
        if let EventPayload::TranscriptPage(page) = &e.payload {
            let page = page.clone();
            consume(e);
            break page;
        }
        consume(e);
    };
    runtime.close_input();
    while let Some(e) = runtime.next_event().await {
        consume(e);
    }
    runtime.join().await?;
    let f = final_snapshot.ok_or("runtime_fixture_snapshot")?;
    let transcript = page
        .entries
        .iter()
        .map(|v| {
            object(vec![
                ("turn", s(&v.turn.0)),
                ("request", s(&v.request.0)),
                ("role", s(&v.role)),
                ("text", s(&v.text)),
                ("state", s(&v.state)),
                ("mode", s(mode(v.mode))),
            ])
        })
        .collect();
    let context = if let Some(path) = directory {
        let mut state = State::new(
            "offline".into(),
            fixture::configuration(&tools),
            tools.read_only().advertisement().clone(),
        );
        let journal = Journal::open(&path, |e| state.apply(e), None)?;
        if !journal.diagnostic().is_empty() {
            return Err("runtime_fixture_reopen");
        }
        Json::array(state.history)?
    } else {
        null()
    };
    Ok(object(vec![
        ("incarnation", s(&handle.incarnation().0)),
        ("invocations", n(count.load(Ordering::SeqCst))),
        ("requests", Json::array(requests.lock().unwrap().clone())?),
        ("events", Json::array(events)?),
        ("transcript", Json::array(transcript)?),
        ("context", context),
        (
            "snapshot",
            object(vec![
                ("state", s(&f.state)),
                ("busy", b(f.worker_busy)),
                ("text", s(&f.text)),
                ("diagnostic", s(&f.diagnostic)),
                ("completed", n(f.completed_tools)),
                ("history", n(f.history_items)),
                ("transcript", n(f.transcript_size)),
                ("compactions", n(f.compactions)),
                ("durable", b(f.durable)),
                ("storage_failed", b(f.storage_failed)),
                ("mode", s(mode(f.mode))),
                ("through", n(f.through_sequence as usize)),
                ("request", f.request.as_ref().map_or_else(null, |v| s(&v.0))),
                ("submit", f.submit.as_ref().map_or_else(null, |v| s(&v.0))),
                (
                    "recovery",
                    f.recovery_operation.as_ref().map_or_else(null, |v| s(&v.0)),
                ),
                (
                    "preview",
                    f.preview.as_ref().map_or_else(null, |v| s(&v.text)),
                ),
                ("resume_available", b(f.resume_available)),
            ]),
        ),
    ]))
}
async fn run(row: &Json) -> Result<Json> {
    if row.text_field("op")? == "trace" {
        return trace(row).await;
    }
    if row.text_field("op")? != "case" {
        return Err("runtime_adapter_operation");
    }
    let suite = row.text_field("suite")?;
    match suite {
        "roundtrip" => fixture::roundtrip().await,
        "cancel" | "priority" => fixture::cancel_at_completion().await,
        "admission" => fixture::admission().await,
        "resync" => fixture::resync().await,
        "invalid-response" => fixture::invalid_response().await,
        "tool-cancel" => fixture::tool_cancel().await,
        "shutdown" => fixture::shutdown().await,
        "durable" => fixture::durable().await,
        "auth-cancel" => fixture::auth_cancel().await,
        "auth-drop" => fixture::auth_drop().await,
        "preview-flood" => fixture::preview_flood().await,
        "durable-bypass" => fixture::durable_bypass(),
        "mixed-registry" => fixture::mixed_registry(),
        "restart-limit" => fixture::restart_limit().await,
        "storage-failure" => fixture::storage_failure().await,
        "summary" => fixture::summary(false).await,
        "summary-cancel" => fixture::summary(true).await,
        "checkpoints" => fixture::checkpoint_cancellation().await,
        "restart-resume" => fixture::restart_resume().await,
        "plan-admission" => fixture::plan_admission().await,
        "unknown-restart" => fixture::unknown_restart().await,
        "unresolved-restart" => fixture::unresolved_restart().await,
        "multi-round" => fixture::multi_round().await,
        _ => return Err("runtime_fixture_suite"),
    }
    Ok(object(vec![
        ("suite", s(suite)),
        ("assertions_completed", b(true)),
    ]))
}
fn main() {
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        if line.len() > 4096 {
            std::process::exit(2);
        }
        let result = Json::parse(&line).and_then(|row| reactor.block_on(run(&row)));
        let output = match result {
            Ok(v) => object(vec![("accepted", b(true)), ("value", v)]),
            Err(e) => object(vec![("accepted", b(false)), ("diagnostic", s(e))]),
        };
        println!("{}", output.dump());
        io::stdout().flush().unwrap();
    }
}
