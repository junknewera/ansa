//! Deterministic synthetic callbacks shared by native tests and the test adapter.
use ansa::{
    protocol::json::Json,
    provider::{
        native,
        request::{Binding, Descriptor, Epoch, Request},
    },
    runtime::*,
    session::{
        journal_v1::{Journal, Point},
        state::State,
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        registry::{Definition, Outcome, Policy, Registry},
    },
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
pub struct Root(pub PathBuf);
impl Root {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-m5-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub fn journal(&self) -> PathBuf {
        self.0.join("session")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
pub struct Gate {
    state: Mutex<(bool, bool, Option<Cancel>)>,
    wake: Condvar,
}
impl Gate {
    pub fn hold(&self, stop: &Cancel) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        state.2 = Some(stop.clone());
        self.wake.notify_all();
        while !state.1 {
            let (next, timeout) = self
                .wake
                .wait_timeout(state, Duration::from_secs(10))
                .unwrap();
            state = next;
            assert!(!timeout.timed_out(), "barrier release timeout");
        }
    }
    pub fn wait(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.0 {
            let (next, timeout) = self
                .wake
                .wait_timeout(state, Duration::from_secs(10))
                .unwrap();
            state = next;
            assert!(!timeout.timed_out(), "barrier entry timeout");
        }
    }
    pub fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.wake.notify_all();
    }
    pub fn stopped(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .2
            .as_ref()
            .is_some_and(Cancel::cancelled)
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        self.wake.notify_all();
    }
}
fn j(text: &str) -> Json {
    Json::parse(text).unwrap()
}
pub fn configuration(tools: &ansa::tools::registry::Snapshot) -> Request {
    Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        Epoch {
            id: "fixture_epoch".into(),
            instructions: "Use the pure fixture tool".into(),
            exact_state: j(r#"{"mode":"offline","revision":1}"#),
        },
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap()
}
pub fn tools(
    counter: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    durable: bool,
) -> Arc<ansa::tools::registry::Snapshot> {
    let mut registry = Registry::new();
    let descriptor = Descriptor {
        name: "get_test_value".into(),
        description: "Return a fixed test JSON value without side effects".into(),
        revision: "1".into(),
        schema: j(
            r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#,
        ),
    };
    let legacy = counter.clone();
    registry
        .register_with_policy(
            Definition {
                descriptor,
                validator: Arc::new(|_| true),
                executor: Arc::new(move |_| {
                    legacy.fetch_add(1, Ordering::SeqCst);
                    Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into())
                }),
            },
            Policy {
                invoke: Some(Arc::new(move |_, stop| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if let Some(gate) = &gate {
                        gate.hold(stop);
                    }
                    Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into())
                })),
                read_only: true,
                requires_journal: durable,
                ..Policy::default()
            },
        )
        .unwrap();
    registry.snapshot()
}
fn string(value: &str) -> Json {
    Json::string(value).unwrap()
}
pub fn saved_response(request: &Request, text: Option<&str>, calls: usize) -> Json {
    response_with_offset(request, text, calls, 0)
}
fn response_with_offset(
    request: &Request,
    text: Option<&str>,
    calls: usize,
    offset: usize,
) -> Json {
    let mut items = vec![];
    if let Some(text) = text {
        items.push(
            Json::object(vec![
                ("type", string("message")),
                ("id", string("message")),
                ("role", string("assistant")),
                ("status", string("completed")),
                (
                    "content",
                    Json::array(vec![
                        Json::object(vec![
                            ("type", string("output_text")),
                            ("text", string(text)),
                        ])
                        .unwrap(),
                    ])
                    .unwrap(),
                ),
            ])
            .unwrap(),
        );
    }
    for index in offset..offset + calls {
        items.push(
            Json::object(vec![
                ("type", string("function_call")),
                ("id", string(&format!("item{index}"))),
                ("call_id", string(&format!("call{index}"))),
                ("namespace", string("omh")),
                ("name", string("get_test_value")),
                ("arguments", string("{}")),
                ("status", string("completed")),
            ])
            .unwrap(),
        );
    }
    Json::object(vec![
        ("request", request.save().unwrap()),
        ("id", string("response")),
        ("model", string(&request.binding.returned_model)),
        ("items", Json::array(items).unwrap()),
    ])
    .unwrap()
}
pub fn model(
    request: &Request,
    calls: usize,
) -> Result<ansa::provider::responses::AdmittedResponse> {
    let final_turn = request.input.last().is_some_and(|v| {
        v.has("type")
            && v.text_field("type")
                .is_ok_and(|t| t == "function_call_output")
    });
    native::load(&saved_response(
        request,
        if final_turn {
            Some("OMH_TOOL_OK:OMH_FIXED_VALUE_42")
        } else {
            None
        },
        if final_turn { 0 } else { calls },
    ))
}
pub fn generator(calls: usize) -> Generate {
    Arc::new(move |request, _, _| model(request, calls))
}
pub fn command(handle: &Handle, id: &str, payload: CommandPayload, turn: &str) -> RuntimeCommand {
    RuntimeCommand {
        schema_version: 1,
        session: handle.session().clone(),
        turn: turn.into(),
        incarnation: handle.incarnation().clone(),
        command: id.into(),
        payload,
    }
}
pub async fn post(
    handle: &Handle,
    id: &str,
    payload: CommandPayload,
    turn: &str,
    expected: PostStatus,
) {
    assert_eq!(
        handle
            .post(command(handle, id, payload, turn))
            .await
            .unwrap(),
        expected
    );
}
pub async fn event(handle: &Handle) -> RuntimeEvent {
    let event = tokio::time::timeout(Duration::from_secs(10), handle.next_event())
        .await
        .expect("event timeout")
        .expect("unexpected EOF");
    validate_event(&event).unwrap();
    assert_eq!(&event.incarnation, handle.incarnation());
    event
}
pub async fn until(handle: &Handle, predicate: impl Fn(&EventPayload) -> bool) -> RuntimeEvent {
    loop {
        let event = event(handle).await;
        if predicate(&event.payload) {
            return event;
        }
        assert!(
            !matches!(event.payload, EventPayload::RuntimeError { .. }),
            "unexpected runtime error: {event:?}"
        );
    }
}
pub async fn submit(handle: &Handle, id: &str, text: &str, turn: &str) -> RequestId {
    post(
        handle,
        id,
        CommandPayload::Submit(Submit {
            text: text.into(),
            mode: AgentMode::Basic,
        }),
        turn,
        PostStatus::Accepted,
    )
    .await;
    let value = until(
        handle,
        |p| matches!(p,EventPayload::CommandAccepted{command,..} if command.0==id),
    )
    .await;
    match value.payload {
        EventPayload::CommandAccepted {
            request: Some(id), ..
        } => id,
        _ => panic!("missing request"),
    }
}
pub async fn snapshot(handle: &Handle, id: &str) -> Snapshot {
    post(
        handle,
        id,
        CommandPayload::GetSnapshot(0),
        "query",
        PostStatus::Accepted,
    )
    .await;
    let value=until(handle,|p|matches!(p,EventPayload::FrontendSnapshot(s) if s.command.as_ref().is_some_and(|c|c.0==id))).await;
    match value.payload {
        EventPayload::FrontendSnapshot(s) => {
            assert_eq!(s.through_sequence, value.stream_sequence);
            *s
        }
        _ => unreachable!(),
    }
}
pub async fn finish(runtime: &mut Runtime) {
    runtime.close_input();
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.next_event().await.is_some() {}
        runtime.join().await.unwrap();
    })
    .await
    .expect("lost completion during drain/join");
}
pub async fn setup(
    counter: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    provider: Generate,
    limits: Limits,
    options: Options,
) -> Runtime {
    let tools = tools(counter, gate, options.directory.is_some());
    let runtime = Runtime::new(
        "offline".into(),
        configuration(&tools),
        tools,
        provider,
        limits,
        options,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), runtime.ready())
        .await
        .expect("ready timeout")
        .unwrap();
    runtime
}
pub async fn roundtrip() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let request = submit(&handle, "submit", "fixture", "turn").await;
    runtime.close_input();
    let start = until(&handle, |p| matches!(p, EventPayload::ToolStarted(_))).await;
    let finish = until(&handle, |p| matches!(p, EventPayload::ToolFinished(_))).await;
    assert_eq!(start.request, Some(request.clone()));
    assert_eq!(finish.request, Some(request));
    match (&start.payload, &finish.payload) {
        (EventPayload::ToolStarted(a), EventPayload::ToolFinished(b)) => {
            assert_eq!(a.operation, b.operation);
            assert_eq!(a.call_id, b.call_id);
            assert_eq!(b.effects, ansa::provider::replay::Effects::KnownResult);
        }
        _ => unreachable!(),
    };
    let text = until(&handle, |p| matches!(p, EventPayload::TextDelta { .. })).await;
    if let EventPayload::TextDelta { text } = text.payload {
        assert_eq!(text, "OMH_TOOL_OK:OMH_FIXED_VALUE_42");
    }
    until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = until(
        &handle,
        |p| matches!(p,EventPayload::FrontendSnapshot(s) if s.state=="completed"),
    )
    .await;
    if let EventPayload::FrontendSnapshot(s) = view.payload {
        assert!(!s.worker_busy);
        assert_eq!(s.completed_tools, 1);
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(runtime.next_event().await.is_none());
    runtime.join().await.unwrap();
}
pub async fn cancel_at_completion() {
    let count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Gate::default());
    let held = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        let value = model(request, 1);
        held.hold(stop);
        value
    });
    let mut runtime = setup(
        count.clone(),
        None,
        provider,
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    gate.wait();
    post(
        &handle,
        "cancel",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    let view = snapshot(&handle, "blocked").await;
    assert_eq!(view.state, "cancelled");
    assert!(view.worker_busy);
    assert!(gate.stopped());
    gate.release();
    runtime.close_input();
    while let Some(value) = runtime.next_event().await {
        assert!(!matches!(
            value.payload,
            EventPayload::ToolStarted(_) | EventPayload::Done
        ));
    }
    runtime.join().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
pub async fn tool_cancel() {
    let count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Gate::default());
    let mut runtime = setup(
        count.clone(),
        Some(gate.clone()),
        generator(2),
        Limits::default(),
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    let first = until(&handle, |p| matches!(p, EventPayload::ToolStarted(_))).await;
    gate.wait();
    post(
        &handle,
        "cancel",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    gate.release();
    let result = until(&handle, |p| matches!(p, EventPayload::ToolFinished(_))).await;
    match (first.payload, result.payload) {
        (EventPayload::ToolStarted(a), EventPayload::ToolFinished(b)) => {
            assert_eq!(a.operation, b.operation);
            assert_eq!(b.execution, ansa::provider::replay::Execution::Succeeded);
            assert!(b.output.contains("OMH_FIXED_VALUE_42"));
        }
        _ => unreachable!(),
    };
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
pub async fn durable() {
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let hold = gate.clone();
    let observer: ansa::session::journal_v1::Observer = Arc::new(move |point, kind| {
        if point == Point::BeforeSync && kind == "intent" {
            hold.hold(&Cancel::default());
        }
        Ok(())
    });
    let count = Arc::new(AtomicUsize::new(0));
    let options = Options {
        directory: Some(root.journal()),
        journal: Some(observer),
        ..Options::default()
    };
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        options,
    )
    .await;
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    gate.wait();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    post(
        &handle,
        "cancel",
        CommandPayload::Cancel(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let view = snapshot(&handle, "during-sync").await;
    assert_eq!(view.state, "cancelled");
    assert!(view.worker_busy);
    gate.release();
    until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let tools = tools(Arc::new(AtomicUsize::new(0)), None, true);
    let mut state = State::new(
        "offline".into(),
        configuration(&tools),
        tools.read_only().advertisement().clone(),
    );
    let journal = Journal::open(&root.journal(), |e| state.apply(e), None).unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert!(state.unknown_operation().is_none());
}
pub fn durable_bypass() {
    let count = Arc::new(AtomicUsize::new(0));
    let tools = tools(count.clone(), None, true);
    let mut ids = OperationIds::new("run", 0).unwrap();
    let response = model(&configuration(&tools), 1).unwrap();
    let result = PreparedBatch::prepare(&response, tools, &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        result.results()[0].effects(),
        ansa::provider::replay::Effects::NotStarted
    );
    assert!(
        result.results()[0]
            .output()
            .contains("durable_checkpoint_required")
    );
}

pub fn mixed_registry() {
    let count = Arc::new(AtomicUsize::new(0));
    let pure = tools(count.clone(), None, false);
    let mut registry = Registry::new();
    let definition = pure.definition("get_test_value").unwrap().clone();
    registry.register(definition.clone()).unwrap();
    let mut writer = definition;
    writer.descriptor.name = "writer".into();
    registry
        .register_with_policy(
            writer,
            Policy {
                requires_journal: true,
                ..Policy::default()
            },
        )
        .unwrap();
    let snapshot = registry.snapshot();
    for calls in [0, 1] {
        let response = native::load(&saved_response(
            &configuration(&snapshot),
            Some("answer"),
            calls,
        ))
        .unwrap();
        let mut ids = OperationIds::new("mixed", 0).unwrap();
        let result = PreparedBatch::prepare(&response, snapshot.clone(), &mut ids)
            .unwrap()
            .execute(false);
        assert_eq!(result.results().len(), calls);
        if calls == 1 {
            assert_eq!(
                result.results()[0].effects(),
                ansa::provider::replay::Effects::KnownResult
            );
        }
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

pub async fn admission() {
    let count = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Gate::default());
    let hold = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        hold.hold(stop);
        model(request, 1)
    });
    let mut runtime = setup(
        count.clone(),
        None,
        provider,
        Limits {
            commands: 1,
            submit_ids: 3,
            ..Limits::default()
        },
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    let request = submit(&handle, "one", "fixture", "turn").await;
    gate.wait();
    let payload = CommandPayload::Submit(Submit {
        text: "fixture".into(),
        mode: AgentMode::Basic,
    });
    post(
        &handle,
        "one",
        payload.clone(),
        "turn",
        PostStatus::Duplicate,
    )
    .await;
    post(
        &handle,
        "one",
        CommandPayload::Submit(Submit {
            text: "changed".into(),
            mode: AgentMode::Basic,
        }),
        "turn",
        PostStatus::CommandConflict,
    )
    .await;
    let mut stale = command(&handle, "stale", payload.clone(), "turn");
    stale.incarnation = "old".into();
    assert_eq!(
        handle.post(stale).await.unwrap(),
        PostStatus::StaleIncarnation
    );
    post(
        &handle,
        "two",
        payload.clone(),
        "second",
        PostStatus::Accepted,
    )
    .await;
    post(&handle, "three", payload, "third", PostStatus::QueueFull).await;
    post(
        &handle,
        "stale-cancel",
        CommandPayload::Cancel("wrong".into()),
        "turn",
        PostStatus::StaleTarget,
    )
    .await;
    post(
        &handle,
        "cancel",
        CommandPayload::Cancel(request),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    assert_eq!(snapshot(&handle, "responsive").await.state, "cancelled");
    gate.release();
    until(&handle, |p| matches!(p, EventPayload::Done)).await;
    submit(&handle, "third", "fixture", "third").await;
    until(&handle, |p| matches!(p, EventPayload::Done)).await;
    post(
        &handle,
        "four",
        CommandPayload::Submit(Submit {
            text: "fixture".into(),
            mode: AgentMode::Basic,
        }),
        "fourth",
        PostStatus::CommandIdLimit,
    )
    .await;
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

pub async fn resync() {
    let gate = Arc::new(Gate::default());
    let hold = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, _| {
        hold.hold(stop);
        model(request, 1)
    });
    let mut runtime = setup(
        Arc::new(AtomicUsize::new(0)),
        None,
        provider,
        Limits {
            events: 2,
            ..Limits::default()
        },
        Options::default(),
    )
    .await;
    let handle = runtime.handle();
    // With capacity two, acceptance can itself be evicted before the consumer
    // reads it. Synchronize on the worker barrier and recover via Snapshot.
    post(
        &handle,
        "submit",
        CommandPayload::Submit(Submit {
            text: "fixture".into(),
            mode: AgentMode::Basic,
        }),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    gate.wait();
    for index in 0..8 {
        post(
            &handle,
            &format!("snapshot{index}"),
            CommandPayload::GetSnapshot(0),
            "query",
            PostStatus::Accepted,
        )
        .await;
    }
    let marker = event(&handle).await;
    if let EventPayload::ResyncRequired { oldest_available } = marker.payload {
        assert_eq!(oldest_available, marker.stream_sequence + 1);
    } else {
        panic!("missing resync");
    }
    let view = event(&handle).await;
    assert_eq!(view.stream_sequence, marker.stream_sequence + 1);
    if let EventPayload::FrontendSnapshot(s) = view.payload {
        assert_eq!(s.through_sequence, view.stream_sequence);
        assert_eq!(s.state, "generating");
    } else {
        panic!("missing snapshot");
    }
    post(
        &handle,
        "future",
        CommandPayload::GetSnapshot(u64::MAX),
        "query",
        PostStatus::Accepted,
    )
    .await;
    let error = until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    if let EventPayload::RuntimeError { diagnostic, .. } = error.payload {
        assert_eq!(diagnostic, "resync_cursor_ahead");
    }
    handle.request_shutdown();
    gate.release();
    finish(&mut runtime).await;
}

pub async fn invalid_response() {
    for case in 0..3 {
        let count = Arc::new(AtomicUsize::new(0));
        let provider: Generate = Arc::new(move |request, _, _| {
            let mut saved = saved_response(request, None, 2).dump();
            saved = match case {
                0 => saved.replacen("\"name\":\"get_test_value\"", "\"name\":\"unknown\"", 1),
                1 => saved.replacen("\"arguments\":\"{}\"", "\"arguments\":\"[]\"", 1),
                _ => saved.replacen("\"model\":\"gpt-6.1-sol\"", "\"model\":\"unexpected\"", 1),
            };
            native::load(&Json::parse(&saved)?)
        });
        let mut runtime = setup(
            count.clone(),
            None,
            provider,
            Limits::default(),
            Options::default(),
        )
        .await;
        let handle = runtime.handle();
        submit(&handle, "submit", "fixture", "turn").await;
        until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
        assert_eq!(snapshot(&handle, "paused").await.state, "paused");
        finish(&mut runtime).await;
        assert_eq!(count.load(Ordering::SeqCst), 0, "batch case {case}");
    }
}

pub async fn auth_cancel() {
    for tool_guard in [false, true] {
        let root = Root::new();
        let gate = Arc::new(Gate::default());
        let hold = gate.clone();
        let mut options = Options {
            directory: Some(root.journal()),
            ..Options::default()
        };
        if tool_guard {
            options.tool_guard = Arc::new(move |stop| {
                hold.hold(stop);
                Ok(Lease::default())
            });
        } else {
            options.check_dispatch = Arc::new(move |_, stop| {
                hold.hold(stop);
                Ok(())
            });
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut runtime = setup(
            count.clone(),
            None,
            generator(1),
            Limits::default(),
            options,
        )
        .await;
        let handle = runtime.handle();
        let request = submit(&handle, "submit", "fixture", "turn").await;
        gate.wait();
        post(
            &handle,
            "cancel",
            CommandPayload::Cancel(request),
            "turn",
            PostStatus::Accepted,
        )
        .await;
        let view = snapshot(&handle, "auth-blocked").await;
        assert_eq!(view.state, "cancelled");
        assert!(view.worker_busy);
        assert!(gate.stopped());
        gate.release();
        finish(&mut runtime).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let tools = tools(count, None, true);
        let mut state = State::new(
            "offline".into(),
            configuration(&tools),
            tools.read_only().advertisement().clone(),
        );
        let journal = Journal::open(&root.journal(), |e| state.apply(e), None).unwrap();
        assert_eq!(journal.diagnostic(), "");
        assert!(state.unknown_operation().is_none());
    }
}

pub async fn auth_drop() {
    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("synthetic lease cleanup");
        }
    }
    let root = Root::new();
    let count = Arc::new(AtomicUsize::new(0));
    let options = Options {
        directory: Some(root.journal()),
        tool_guard: Arc::new(|_| {
            Ok(Lease {
                ownership: Box::new(Panics),
                stop: Cancel::default(),
            })
        }),
        ..Options::default()
    };
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        options,
    )
    .await;
    let handle = runtime.handle();
    submit(&handle, "submit", "fixture", "turn").await;
    until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    let mut view = snapshot(&handle, "cleanup").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while view.worker_busy {
        assert!(
            std::time::Instant::now() < deadline,
            "lost auth cleanup completion"
        );
        view = snapshot(&handle, "cleanup").await;
    }
    assert!(view.storage_failed);
    assert!(view.tools.iter().any(|v| matches!(v, Activity::Finished(f) if f.effects==ansa::provider::replay::Effects::KnownResult)));
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

pub async fn preview_flood() {
    let held_sink: Arc<Mutex<Option<PreviewSink>>> = Arc::new(Mutex::new(None));
    let saved = held_sink.clone();
    let gate = Arc::new(Gate::default());
    let hold = gate.clone();
    let provider: Generate = Arc::new(move |request, stop, preview| {
        *saved.lock().unwrap() = Some(preview.clone());
        preview("preview")?;
        // The separate producer keeps the watch ready while owner handles control.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !stop.cancelled() && std::time::Instant::now() < deadline {
                    if preview("preview").is_err() {
                        break;
                    }
                    std::thread::yield_now();
                }
            });
            hold.hold(stop);
        });
        model(request, 1)
    });
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = setup(
        count.clone(),
        None,
        provider,
        Limits::default(),
        Options {
            streaming: true,
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let request = submit(&handle, "submit", "fixture", "turn").await;
    gate.wait();
    for index in 0..16 {
        tokio::time::timeout(
            Duration::from_secs(2),
            snapshot(&handle, &format!("flood{index}")),
        )
        .await
        .expect("preview starved control");
    }
    post(
        &handle,
        "cancel",
        CommandPayload::Cancel(request),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let before = snapshot(&handle, "cancelled").await;
    assert_eq!(before.preview.unwrap().state, "discarded");
    gate.release();
    assert_eq!(
        held_sink.lock().unwrap().as_ref().unwrap()("preview late"),
        Ok(())
    );
    assert_eq!(
        snapshot(&handle, "late-preview")
            .await
            .preview
            .unwrap()
            .state,
        "discarded"
    );
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

pub async fn shutdown() {
    for _ in 0..32 {
        let root = Root::new();
        let count = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Gate::default());
        let hold = gate.clone();
        let options = Options {
            directory: Some(root.journal()),
            tool_guard: Arc::new(move |stop| {
                hold.hold(stop);
                Ok(Lease::default())
            }),
            ..Options::default()
        };
        let mut runtime = setup(
            count.clone(),
            None,
            generator(1),
            Limits::default(),
            options,
        )
        .await;
        submit(&runtime.handle(), "submit", "fixture", "turn").await;
        gate.wait();
        runtime.handle().request_shutdown();
        let mut join = Box::pin(runtime.join());
        assert!(
            tokio::time::timeout(Duration::from_millis(2), &mut join)
                .await
                .is_err(),
            "joined while lease worker blocked"
        );
        gate.release();
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("shutdown join timeout")
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let tools = tools(count, None, true);
        let mut state = State::new(
            "offline".into(),
            configuration(&tools),
            tools.read_only().advertisement().clone(),
        );
        let journal = Journal::open(&root.journal(), |e| state.apply(e), None).unwrap();
        assert_eq!(journal.diagnostic(), "");
        assert!(
            state.unknown_operation().is_none(),
            "graceful shutdown left unresolved intent"
        );
    }
}

pub async fn restart_limit() {
    let root = Root::new();
    let options = Options {
        directory: Some(root.journal()),
        ..Options::default()
    };
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        options.clone(),
    )
    .await;
    let handle = runtime.handle();
    for index in 0..3 {
        submit(
            &handle,
            &format!("turn{index}"),
            "fixture",
            &format!("turn{index}"),
        )
        .await;
        until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    finish(&mut runtime).await;
    let snapshot = tools(count, None, true);
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration(&snapshot),
        snapshot,
        generator(1),
        Limits {
            submit_ids: 2,
            ..Limits::default()
        },
        options,
    )
    .unwrap();
    assert_eq!(runtime.ready().await, Err("session_command_limit"));
    runtime.join().await.unwrap();
}

pub async fn storage_failure() {
    for kind in ["outcome", "done"] {
        let root = Root::new();
        let count = Arc::new(AtomicUsize::new(0));
        let observer: ansa::session::journal_v1::Observer = Arc::new(move |point, name| {
            if point == Point::BeforeWrite && name == kind {
                return Err("synthetic_write_failure");
            }
            Ok(())
        });
        let mut runtime = setup(
            count.clone(),
            None,
            generator(1),
            Limits::default(),
            Options {
                directory: Some(root.journal()),
                journal: Some(observer),
                ..Options::default()
            },
        )
        .await;
        let handle = runtime.handle();
        let request = submit(&handle, "submit", "fixture", "turn").await;
        let mut published_text = false;
        loop {
            let e = event(&handle).await;
            match e.payload {
                EventPayload::TextDelta { .. } | EventPayload::Done => published_text = true,
                EventPayload::RuntimeError { .. } => break,
                _ => {}
            }
        }
        assert!(!published_text, "undurable assistant was published");
        let view = snapshot(&handle, "failed").await;
        assert!(view.storage_failed && !view.durable);
        assert_eq!(view.text, "");
        assert!(view.tools.iter().any(|v| matches!(v, Activity::Finished(f) if f.output.contains("OMH_FIXED_VALUE_42") && f.effects==ansa::provider::replay::Effects::KnownResult)));
        assert_eq!(
            view.transcript_size, 1,
            "undurable assistant leaked into transcript"
        );
        post(
            &handle,
            "cancel",
            CommandPayload::Cancel(request),
            "turn",
            PostStatus::Accepted,
        )
        .await;
        until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
        finish(&mut runtime).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

pub async fn summary(cancelled: bool) {
    use ansa::context::legacy_compact::{CompactedContext, summary_prefix_size, summary_request};
    let root = Root::new();
    let gate = Arc::new(Gate::default());
    let hold = gate.clone();
    let compactions = Arc::new(AtomicUsize::new(0));
    let called = compactions.clone();
    let compact: Compact = Arc::new(move |source, stop| {
        called.fetch_add(1, Ordering::SeqCst);
        let request = summary_request(source)?;
        assert!(request.tools.descriptors.is_empty());
        assert_eq!(request.binding, source.binding);
        let answer = native::load(&saved_response(&request, Some("Goal ORCHID retained"), 0))?;
        let result = CompactedContext::from_summary(source.clone(), &answer)?;
        let prefix = summary_prefix_size(source)?;
        assert_eq!(&result.output()?[1..], &source.input[prefix..]);
        if cancelled {
            hold.hold(stop);
        }
        Ok(result)
    });
    let count = Arc::new(AtomicUsize::new(0));
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits {
            compact_bytes: 300,
            ..Limits::default()
        },
        Options {
            directory: Some(root.journal()),
            compact: Some(compact),
            keep_recent_turns: 2,
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    for index in 0..3 {
        submit(
            &handle,
            &format!("turn{index}"),
            "Goal ORCHID",
            &format!("turn{index}"),
        )
        .await;
        until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    let before = snapshot(&handle, "before-summary").await;
    let request = submit(&handle, "summary", "next", "summary").await;
    if cancelled {
        gate.wait();
        post(
            &handle,
            "cancel",
            CommandPayload::Cancel(request),
            "summary",
            PostStatus::Accepted,
        )
        .await;
        let view = snapshot(&handle, "summary-blocked").await;
        assert_eq!(view.compactions, 0);
        assert!(view.worker_busy);
        gate.release();
    } else {
        until(&handle, |p| matches!(p, EventPayload::Done)).await;
    }
    finish(&mut runtime).await;
    let tools = tools(count, None, true);
    let mut state = State::new(
        "offline".into(),
        configuration(&tools),
        tools.read_only().advertisement().clone(),
    );
    let journal = Journal::open(&root.journal(), |e| state.apply(e), None).unwrap();
    assert_eq!(journal.diagnostic(), "");
    assert_eq!(compactions.load(Ordering::SeqCst), 1);
    assert_eq!(state.compactions, usize::from(!cancelled));
    if cancelled {
        assert_eq!(state.history.len(), before.history_items);
        assert_eq!(state.epoch, state.configuration.epoch);
    } else {
        assert!(
            state.history[0]
                .text_field("content")
                .unwrap()
                .contains("ORCHID")
        );
        assert_ne!(state.epoch.id, state.configuration.epoch.id);
    }
}

pub async fn checkpoint_cancellation() {
    for kind in ["accepted", "response", "intent", "outcome", "continued"] {
        for point in [Point::BeforeWrite, Point::AfterSync] {
            let root = Root::new();
            let gate = Arc::new(Gate::default());
            let hold = gate.clone();
            let observer: ansa::session::journal_v1::Observer = Arc::new(move |p, k| {
                if k == kind && p == point {
                    hold.hold(&Cancel::default());
                }
                Ok(())
            });
            let count = Arc::new(AtomicUsize::new(0));
            let mut runtime = setup(
                count.clone(),
                None,
                generator(2),
                Limits::default(),
                Options {
                    directory: Some(root.journal()),
                    journal: Some(observer),
                    ..Options::default()
                },
            )
            .await;
            let handle = runtime.handle();
            post(
                &handle,
                "submit",
                CommandPayload::Submit(Submit {
                    text: "fixture".into(),
                    mode: AgentMode::Basic,
                }),
                "turn",
                PostStatus::Accepted,
            )
            .await;
            gate.wait();
            let view = snapshot(&handle, "checkpoint").await;
            assert!(view.worker_busy && !view.durable);
            let request = view.request.unwrap();
            post(
                &handle,
                "cancel",
                CommandPayload::Cancel(request),
                "turn",
                PostStatus::Accepted,
            )
            .await;
            assert_eq!(snapshot(&handle, "cancelled").await.state, "cancelled");
            let expected = if kind == "continued" {
                2
            } else {
                usize::from(kind == "outcome")
            };
            assert_eq!(count.load(Ordering::SeqCst), expected);
            gate.release();
            until(&handle, |p| matches!(p, EventPayload::Cancelled { .. })).await;
            finish(&mut runtime).await;
            assert_eq!(count.load(Ordering::SeqCst), expected);
            let tools = tools(count, None, true);
            let mut state = State::new(
                "offline".into(),
                configuration(&tools),
                tools.read_only().advertisement().clone(),
            );
            let journal = Journal::open(&root.journal(), |e| state.apply(e), None).unwrap();
            assert_eq!(journal.diagnostic(), "");
            assert!(state.unknown_operation().is_none());
            assert_eq!(state.turn.as_ref().unwrap().stage, "cancelled");
        }
    }
}

pub async fn restart_resume() {
    use std::sync::atomic::AtomicBool;
    let root = Root::new();
    let count = Arc::new(AtomicUsize::new(0));
    let permitted = Arc::new(AtomicBool::new(true));
    let revoke = permitted.clone();
    let intents = Arc::new(AtomicUsize::new(0));
    let seen = intents.clone();
    let observer: ansa::session::journal_v1::Observer = Arc::new(move |point, kind| {
        if point == Point::AfterSync && kind == "intent" && seen.fetch_add(1, Ordering::SeqCst) == 1
        {
            revoke.store(false, Ordering::SeqCst);
        }
        Ok(())
    });
    let options = Options {
        directory: Some(root.journal()),
        journal: Some(observer),
        check_dispatch: Arc::new(move |_, _| {
            if permitted.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("auth_required")
            }
        }),
        ..Options::default()
    };
    let mut runtime = setup(
        count.clone(),
        None,
        generator(2),
        Limits::default(),
        options,
    )
    .await;
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(snapshot(&handle, "pause").await.resume_available);
    finish(&mut runtime).await;
    let mut runtime = setup(
        count.clone(),
        None,
        generator(2),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let view = snapshot(&handle, "restart").await;
    assert!(view.resume_available);
    assert_eq!(view.state, "paused");
    assert!(!view.worker_busy);
    assert_eq!(count.load(Ordering::SeqCst), 1, "restart executed tool");
    post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    until(&handle, |p| matches!(p, EventPayload::Done)).await;
    let view = snapshot(&handle, "complete").await;
    assert_eq!(view.completed_tools, 2);
    assert_eq!(view.state, "completed");
    finish(&mut runtime).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "known prefix was executed twice"
    );
}

pub async fn plan_admission() {
    let count = Arc::new(AtomicUsize::new(0));
    let safe = tools(count.clone(), None, false);
    let definition = safe.definition("get_test_value").unwrap().clone();
    let mut registry = Registry::new();
    registry
        .register_with_policy(
            definition.clone(),
            Policy {
                read_only: true,
                ..Policy::default()
            },
        )
        .unwrap();
    let mut writer = definition;
    writer.descriptor.name = "writer".into();
    registry.register(writer).unwrap();
    let tools = registry.snapshot();
    let provider: Generate = Arc::new(|request, _, _| {
        assert_eq!(request.tools.descriptors.len(), 1);
        assert!(request.tools.descriptors.iter().all(|d| d.name != "writer"));
        let mut value = saved_response(request, None, 2).dump();
        let index = value.rfind("\"name\":\"get_test_value\"").unwrap();
        value.replace_range(
            index..index + "\"name\":\"get_test_value\"".len(),
            "\"name\":\"writer\"",
        );
        native::load(&Json::parse(&value)?)
    });
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration(&tools),
        tools,
        provider,
        Limits::default(),
        Options::default(),
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    post(
        &handle,
        "plan",
        CommandPayload::Submit(Submit {
            text: "fixture".into(),
            mode: AgentMode::Plan,
        }),
        "plan",
        PostStatus::Accepted,
    )
    .await;
    until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    assert_eq!(
        snapshot(&handle, "plan-rejected").await.mode,
        AgentMode::Plan
    );
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

pub async fn unknown_restart() {
    let root = Root::new();
    let count = Arc::new(AtomicUsize::new(0));
    let make_tools = || {
        let mut registry = Registry::new();
        let base = tools(count.clone(), None, false);
        let mut definition = base.definition("get_test_value").unwrap().clone();
        let counter = count.clone();
        definition.executor = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Outcome::from_bytes(
                ansa::provider::replay::Execution::Failed,
                ansa::provider::replay::Effects::Unknown,
                b"unknown effect".to_vec(),
            )
        });
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
        registry.snapshot()
    };
    let options = Options {
        directory: Some(root.journal()),
        ..Options::default()
    };
    let tools = make_tools();
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration(&tools),
        tools,
        generator(2),
        Limits::default(),
        options.clone(),
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    until(&handle, |p| {
        matches!(p, EventPayload::RecoveryRequired { .. })
    })
    .await;
    finish(&mut runtime).await;
    let tools = make_tools();
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration(&tools),
        tools,
        generator(2),
        Limits::default(),
        options,
    )
    .unwrap();
    runtime.ready().await.unwrap();
    let handle = runtime.handle();
    let view = snapshot(&handle, "unknown-restart").await;
    assert!(view.recovery_operation.is_some() && !view.resume_available && !view.worker_busy);
    post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    let error = until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    if let EventPayload::RuntimeError { diagnostic, .. } = error.payload {
        assert_eq!(diagnostic, "recovery_required");
    }
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

pub async fn unresolved_restart() {
    let root = Root::new();
    let count = Arc::new(AtomicUsize::new(0));
    let observer: ansa::session::journal_v1::Observer = Arc::new(|point, kind| {
        if point == Point::AfterSync && kind == "intent" {
            return Err("synthetic_ack_loss");
        }
        Ok(())
    });
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            journal: Some(observer),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let target = submit(&handle, "submit", "fixture", "turn").await;
    until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
    finish(&mut runtime).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let mut runtime = setup(
        count.clone(),
        None,
        generator(1),
        Limits::default(),
        Options {
            directory: Some(root.journal()),
            ..Options::default()
        },
    )
    .await;
    let handle = runtime.handle();
    let view = snapshot(&handle, "unresolved").await;
    assert!(view.recovery_operation.is_some() && !view.resume_available && !view.worker_busy);
    post(
        &handle,
        "resume",
        CommandPayload::Resume(target),
        "turn",
        PostStatus::Accepted,
    )
    .await;
    until(&handle, |p| matches!(p, EventPayload::RuntimeError { diagnostic,.. } if diagnostic=="recovery_required")).await;
    finish(&mut runtime).await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "unresolved intent replayed"
    );
}

pub async fn multi_round() {
    for limit in [1, 3] {
        let count = Arc::new(AtomicUsize::new(0));
        let provider: Generate = Arc::new(|request, _, _| {
            let round = request
                .input
                .iter()
                .filter(|v| {
                    v.has("type")
                        && v.text_field("type")
                            .is_ok_and(|t| t == "function_call_output")
                })
                .count();
            if round == 2 {
                return native::load(&saved_response(request, Some("TWO_ROUNDS"), 0));
            }
            native::load(&response_with_offset(request, None, 1, round))
        });
        let mut runtime = setup(
            count.clone(),
            None,
            provider,
            Limits {
                tool_rounds: limit,
                ..Limits::default()
            },
            Options::default(),
        )
        .await;
        let handle = runtime.handle();
        submit(&handle, "submit", "fixture", "turn").await;
        if limit == 1 {
            until(&handle, |p| matches!(p, EventPayload::RuntimeError { .. })).await;
            let view = snapshot(&handle, "limit").await;
            assert_eq!(view.state, "paused");
            assert!(!view.storage_failed);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        } else {
            until(&handle, |p| matches!(p, EventPayload::Done)).await;
            assert_eq!(snapshot(&handle, "multi").await.text, "TWO_ROUNDS");
            assert_eq!(count.load(Ordering::SeqCst), 2);
        }
        finish(&mut runtime).await;
    }
}
