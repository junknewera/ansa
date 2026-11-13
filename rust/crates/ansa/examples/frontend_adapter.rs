//! Synthetic-only adapter over the actual frontend and loopback transport bridge.
use ansa::{
    auth::Cancel,
    frontend::{self, app, carbon, controller, input::Editor, jsonl, protocol_v1::*, terminal::Io},
    protocol::json::Json,
    provider::{
        native,
        replay::{Effects, Execution},
        request::{Epoch, Request},
        responses::ResponseStream,
    },
    runtime::{Limits, Options, Runtime},
};
use std::{
    io::{self, BufRead, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
#[allow(dead_code)]
#[path = "support/frame_server.rs"]
mod frame_server;
type Result<T> = frontend::Result<T>;
fn s(v: &str) -> Json {
    Json::string(v).unwrap()
}
fn n(v: usize) -> Json {
    Json::parse(&v.to_string()).unwrap()
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).unwrap()
}
fn obj(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).unwrap()
}
fn null() -> Json {
    Json::parse("null").unwrap()
}
fn hex(raw: &str) -> Result<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return Err("fixture_hex");
    }
    (0..raw.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(raw.get(at..at + 2).ok_or("fixture_hex")?, 16)
                .map_err(|_| "fixture_hex")
        })
        .collect()
}
fn command(c: &RuntimeCommand) -> Json {
    let mut f = vec![
        ("schema_version", n(1)),
        ("session", s(&c.session.0)),
        ("turn", s(&c.turn.0)),
        ("incarnation", s(&c.incarnation.0)),
        ("command_id", s(&c.command.0)),
    ];
    match &c.payload {
        CommandPayload::Submit(v) => f.extend([
            ("type", s("submit")),
            ("text", s(&v.text)),
            ("mode", s(jsonl::mode(v.mode))),
        ]),
        CommandPayload::Cancel(v) => f.extend([("type", s("cancel")), ("target", s(&v.0))]),
        CommandPayload::Resume(v) => f.extend([("type", s("resume")), ("target", s(&v.0))]),
        CommandPayload::GetSnapshot(v) => {
            f.extend([("type", s("snapshot")), ("after_sequence", n(*v as usize))])
        }
        CommandPayload::GetTranscript(v) => f.extend([
            ("type", s("transcript")),
            ("before", n(v.before)),
            ("limit", n(v.limit)),
            ("forward", b(v.forward)),
        ]),
    }
    obj(f)
}
fn run(row: &Json) -> Result<Json> {
    let root = Path::new(row.text_field("root")?);
    ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
    match row.text_field("op")? {
        "decode" => {
            let command = jsonl::decode(row.text_field("payload")?)?;
            validate_command(&command).map_err(|code| {
                if code == "request_id" {
                    if matches!(command.payload, CommandPayload::Cancel(_)) {
                        "cancel_target"
                    } else {
                        "resume_target"
                    }
                } else {
                    code
                }
            })?;
            Ok(self::command(&command))
        }
        "args" => {
            let home = root.join("home");
            std::fs::create_dir_all(home.join("workspace")).map_err(|_| "fixture_path")?;
            let value = frontend::args::Options::parse(
                &row.field("args")?
                    .elements()?
                    .iter()
                    .map(|v| v.text().map(String::from))
                    .collect::<Result<Vec<_>>>()?,
                home.join("workspace"),
                app::auth_path_with(Some(&home), Some(root.join("xdg").as_os_str()))?,
            )?;
            Ok(obj(vec![
                ("offline", b(value.offline)),
                ("resume", b(value.resume)),
                ("print_config", b(value.print_config)),
                ("cwd", s(value.cwd.to_str().ok_or("fixture_path")?)),
                ("auth", s(value.auth.to_str().ok_or("fixture_path")?)),
                ("session", s(&value.session)),
                ("model", value.model.as_deref().map_or_else(null, s)),
                (
                    "returned_model",
                    value.returned_model.as_deref().map_or_else(null, s),
                ),
                ("prompt", value.prompt.as_deref().map_or_else(null, s)),
            ]))
        }
        "render" => {
            let mut previous = 0;
            let mut frames = vec![];
            let color = row.field("color")?.boolean()?;
            for step in row.field("steps")?.elements()? {
                let entries = step
                    .field("entries")?
                    .elements()?
                    .iter()
                    .map(|v| {
                        Ok(carbon::Entry {
                            role: v.text_field("role")?.into(),
                            text: v.text_field("text")?.into(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let input = hex(step.text_field("input_hex")?)?;
                if let Some(frame) = carbon::layout(
                    &entries,
                    &input,
                    step.usize_field("cursor")?,
                    step.text_field("status")?,
                    step.field("expanded")?.boolean()?,
                    step.usize_field("scroll")?,
                    step.text_field("context")?,
                    (step.usize_field("cols")?, step.usize_field("rows")?),
                    color,
                ) {
                    frames.push(obj(vec![
                        ("frame", s(&frame.paint(previous))),
                        ("scroll_limit", n(frame.scroll_limit)),
                    ]));
                    previous = frame.lines.len();
                } else {
                    frames.push(obj(vec![("frame", s("")), ("scroll_limit", n(0))]));
                }
            }
            Json::array(frames)
        }
        "events" => events(),
        "editor" => {
            let mut editor = Editor::default();
            let mut actions = vec![];
            for byte in hex(row.text_field("bytes_hex")?)? {
                if let Some(action) = editor.feed(byte, true) {
                    actions.push(s(&format!("{action:?}")));
                }
            }
            Ok(obj(vec![
                (
                    "bytes_hex",
                    s(&editor
                        .bytes
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<String>()),
                ),
                ("cursor", n(editor.cursor)),
                ("overflow", b(editor.overflow)),
                ("actions", Json::array(actions)?),
            ]))
        }
        "ca" => {
            let _ = ansa::transport::client::read_ca(&root.join("ca.pem"))?;
            Ok(b(true))
        }
        "bridge" => {
            let request = Request::load(row.field("request")?)?;
            let value = request.json()?;
            let mut stream = ResponseStream::new_for_adapter(
                request,
                ansa::protocol::sse::Limits::default(),
                None,
                None,
            );
            let ca = hex(row.text_field("ca_hex")?)?;
            let mut received = 0;
            let mut received_raw = Vec::new();
            let result = ansa::transport::client::fixture_stream(
                row.text_field("url")?,
                &ca,
                &value,
                &mut |bytes| {
                    received += bytes.len();
                    received_raw.extend_from_slice(bytes);
                    stream.feed_for_adapter(bytes)
                },
                &Cancel::default(),
            );
            let response = result.and_then(|_| stream.finish_for_adapter());
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut batch_accepted = false;
            if let Ok(response) = &response {
                let tools = app::pure_tools()?;
                let mut definition = tools
                    .definition("get_test_value")
                    .expect("fixture tool")
                    .clone();
                let counter = count.clone();
                definition.executor = Arc::new(move |_| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ansa::tools::registry::Outcome::succeeded(
                        r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into(),
                    )
                });
                let mut registry = ansa::tools::registry::Registry::new();
                registry.register_with_policy(
                    definition,
                    ansa::tools::registry::Policy {
                        read_only: true,
                        ..Default::default()
                    },
                )?;
                let mut ids = ansa::tools::admission::OperationIds::new("bridge", 0)?;
                if let Ok(batch) = ansa::tools::admission::PreparedBatch::prepare(
                    response,
                    registry.snapshot(),
                    &mut ids,
                ) {
                    batch_accepted = true;
                    let _ = batch.execute(false);
                }
            }
            Ok(obj(vec![
                ("accepted", b(response.is_ok())),
                ("batch_accepted", b(batch_accepted)),
                (
                    "executors",
                    n(count.load(std::sync::atomic::Ordering::SeqCst)),
                ),
                ("received", n(received)),
                (
                    "received_hex",
                    s(&received_raw
                        .iter()
                        .map(|v| format!("{v:02x}"))
                        .collect::<String>()),
                ),
                (
                    "diagnostic",
                    s(response.as_ref().err().copied().unwrap_or("")),
                ),
            ]))
        }
        _ => Err("frontend_adapter_operation"),
    }
}
fn events() -> Result<Json> {
    let preview = Preview {
        generation: 3,
        text: "draft".into(),
        state: "discarded".into(),
    };
    let started = Started {
        operation: "operation".into(),
        call_id: "call".into(),
        name: "safe".into(),
    };
    let finished = Finished {
        operation: "operation".into(),
        call_id: "call".into(),
        execution: Execution::Failed,
        effects: Effects::KnownResult,
        output: Arc::new("result".into()),
    };
    let snapshot = Snapshot {
        through_sequence: 7,
        state: "paused".into(),
        request: Some("request".into()),
        worker_busy: false,
        completed_tools: 1,
        text: "final".into(),
        diagnostic: "failure".into(),
        recovery_operation: Some("operation".into()),
        submit: Some("submit".into()),
        command: Some("command".into()),
        tools: vec![
            Activity::Started(started.clone()),
            Activity::Finished(finished.clone()),
        ],
        history_items: 5,
        compactions: 2,
        storage_failed: false,
        durable: true,
        resume_available: true,
        mode: AgentMode::Plan,
        transcript_size: 1,
        preview: Some(preview.clone()),
    };
    let payloads = vec![
        EventPayload::FrontendSnapshot(Box::new(snapshot)),
        EventPayload::Preview(preview),
        EventPayload::TranscriptPage(TranscriptPage {
            begin: 0,
            end: 1,
            total: 1,
            query: GetTranscript {
                before: 0,
                limit: 16,
                forward: false,
            },
            entries: vec![ansa::session::transcript::Entry {
                turn: "turn".into(),
                request: "request".into(),
                role: "user".into(),
                text: "question".into(),
                state: "pending".into(),
                mode: AgentMode::Plan,
            }],
        }),
        EventPayload::CommandAccepted {
            command: "command".into(),
            request: Some("request".into()),
        },
        EventPayload::CommandReplayed {
            command: "command".into(),
            state: "completed".into(),
        },
        EventPayload::TextDelta {
            text: "final".into(),
        },
        EventPayload::ToolStarted(started),
        EventPayload::ToolFinished(finished),
        EventPayload::Cancelled {
            command: "command".into(),
        },
        EventPayload::Done,
        EventPayload::RuntimeError {
            command: None,
            diagnostic: "failure".into(),
        },
        EventPayload::RecoveryRequired {
            operation: "operation".into(),
            effects: Effects::Unknown,
        },
        EventPayload::ResyncRequired {
            oldest_available: 3,
        },
    ];
    Json::array(
        payloads
            .into_iter()
            .map(|payload| {
                jsonl::encode(&RuntimeEvent {
                    schema_version: 1,
                    stream_sequence: 7,
                    session: "session".into(),
                    turn: "turn".into(),
                    incarnation: "incarnation".into(),
                    request: Some("request".into()),
                    payload,
                })
            })
            .collect(),
    )
}
fn final_response(
    request: &Request,
    text: &str,
) -> Result<ansa::provider::responses::AdmittedResponse> {
    let part = obj(vec![("type", s("output_text")), ("text", s(text))]);
    let item = obj(vec![
        ("type", s("message")),
        ("id", s("message")),
        ("role", s("assistant")),
        ("status", s("completed")),
        ("content", Json::array(vec![part])?),
    ]);
    native::load(&obj(vec![
        ("request", request.save()?),
        ("id", s("reply")),
        ("model", s(&request.binding.returned_model)),
        ("items", Json::array(vec![item])?),
    ]))
}
fn fixture(root: &Path, mode: &str) -> Result<i32> {
    ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
    let mut io = Io::open(mode != "backpressure" && mode != "large-jsonl")?;
    let tools = app::pure_tools()?;
    let tools = if mode == "large-jsonl" {
        let mut registry = ansa::tools::registry::Registry::new();
        let mut definition = tools
            .definition("get_test_value")
            .expect("fixture tool")
            .clone();
        definition.executor =
            Arc::new(|_| ansa::tools::registry::Outcome::succeeded("x".repeat(100 * 1024)));
        registry.register_with_policy(
            definition,
            ansa::tools::registry::Policy {
                read_only: true,
                ..Default::default()
            },
        )?;
        registry.snapshot()
    } else {
        tools
    };
    let configuration = Request::new(
        app::binding(),
        Epoch {
            id: "frontend_fixture".into(),
            instructions: "fixture".into(),
            exact_state: Json::parse("{}")?,
        },
        tools.advertisement().clone(),
        vec![],
    )?;
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "fixture_runtime")?;
    let directory = root.join("journal");
    if mode == "terminal" {
        let mut seed = Runtime::new(
            "cli".into(),
            configuration.clone(),
            tools.clone(),
            Arc::new(|r, _, _| final_response(r, "done")),
            Limits::default(),
            Options {
                directory: Some(directory.clone()),
                ..Options::default()
            },
        )?;
        reactor.block_on(async {
            seed.ready().await?;
            seed.post(RuntimeCommand {
                schema_version: 1,
                session: "cli".into(),
                turn: "previous".into(),
                incarnation: seed.handle().incarnation().clone(),
                command: "previous".into(),
                payload: CommandPayload::Submit(Submit {
                    text: "EARLIER_CONTEXT **bold** `code` alpha bravo charlie delta \x1b[31m"
                        .into(),
                    mode: AgentMode::Basic,
                }),
            })
            .await?;
            seed.close_input();
            while seed.next_event().await.is_some() {}
            seed.join().await
        })?;
    }
    let root_path = root.to_owned();
    let mode_owned = mode.to_owned();
    let generate = Arc::new(
        move |request: &Request, stop: &Cancel, preview: ansa::runtime::PreviewSink| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root_path.join("generated"))
                .and_then(|mut f| f.write_all(b"x"))
                .map_err(|_| "fixture_marker")?;
            if mode_owned == "terminal" || mode_owned == "preview-invalid" {
                preview(
                    "```cpp\ndraft\n```\nassistant manager\n**bold alpha bravo charlie delta echo foxtrot** `code` ",
                )?;
            }
            let until = Instant::now()
                + Duration::from_millis(if mode_owned == "limit" { 10 } else { 500 });
            while Instant::now() < until && !stop.cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            if stop.cancelled() {
                return Err("offline_cancelled");
            }
            if mode_owned == "preview-invalid" {
                return Err("fixture_invalid_response");
            }
            if mode_owned == "large-jsonl"
                && request.input.last().is_none_or(|v| {
                    !v.has("type") || v.text_field("type") != Ok("function_call_output")
                })
            {
                let items = (0..12)
                    .map(|i| {
                        obj(vec![
                            ("type", s("function_call")),
                            ("id", s(&format!("item{i}"))),
                            ("call_id", s(&format!("call{i}"))),
                            ("namespace", s("omh")),
                            ("name", s("get_test_value")),
                            ("arguments", s("{}")),
                            ("status", s("completed")),
                        ])
                    })
                    .collect();
                return native::load(&obj(vec![
                    ("request", request.save()?),
                    ("id", s("calls")),
                    ("model", s(&request.binding.returned_model)),
                    ("items", Json::array(items)?),
                ]));
            }
            let text = if mode_owned == "backpressure" {
                "x".repeat(64 * 1024)
            } else {
                "done".into()
            };
            final_response(request, &text)
        },
    );
    let journal = if mode == "pending" {
        let path = root.to_owned();
        Some(Arc::new(move |point, kind: &str| {
            if point == ansa::session::journal_v1::Point::BeforeSync && kind == "accepted" {
                std::fs::write(path.join("accepted-blocked"), b"x")
                    .map_err(|_| "fixture_marker")?;
                let until = Instant::now() + Duration::from_secs(3);
                while !path.join("release").exists() {
                    if Instant::now() >= until {
                        return Err("fixture_barrier_timeout");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }) as ansa::session::journal_v1::Observer)
    } else {
        None
    };
    let options = Options {
        directory: Some(directory),
        streaming: true,
        journal,
        ..Options::default()
    };
    let mut runtime = Runtime::new(
        "cli".into(),
        configuration,
        tools,
        generate,
        Limits {
            submit_ids: if mode == "limit" { 1 } else { 4096 },
            events: 2,
            ..Limits::default()
        },
        options,
    )?;
    reactor.block_on(controller::run(
        &mut runtime,
        &mut io,
        if mode == "backpressure" {
            Some("task")
        } else {
            None
        },
        mode == "large-jsonl",
        "fixture workspace  fixture-model",
    ))
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|v| v == "--serve-h2") {
        if args.len() != 5 {
            std::process::exit(2);
        }
        let body =
            std::fs::read(Path::new(&args[2]).join("round1.sse")).expect("public SSE fixture");
        let reactor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture reactor");
        let observation = Arc::new(std::sync::Mutex::new(None));
        let result = reactor.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            println!("{}", listener.local_addr()?.port());
            io::stdout().flush()?;
            tokio::time::timeout(
                Duration::from_secs(5),
                frame_server::serve_recorded(
                    listener,
                    Path::new(&args[3]),
                    &args[4],
                    hyper::body::Bytes::from(body),
                    Some(observation.clone()),
                ),
            )
            .await??;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        if let Some(value) = observation.lock().unwrap().as_ref() {
            println!("{value}");
            io::stdout().flush().unwrap();
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if let Some(result) = ansa::process::runner::private_entry(&args) {
        std::process::exit(if result.is_ok() { 0 } else { 125 });
    }
    if let Some(result) = ansa::process::supervisor::private_entry(&args) {
        std::process::exit(if result.is_ok() { 0 } else { 125 });
    }
    if args.get(1).is_some_and(|v| v == "--frontend-fixture") {
        let result = if args.len() == 4 {
            fixture(Path::new(&args[2]), &args[3])
        } else {
            Err("fixture_arguments")
        };
        std::process::exit(result.unwrap_or(1));
    }
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        if line.len() > 128 * 1024 {
            std::process::exit(2);
        }
        let result = Json::parse(&line).and_then(|row| run(&row));
        let value = match result {
            Ok(value) => obj(vec![("accepted", b(true)), ("value", value)]),
            Err(code) => obj(vec![
                ("accepted", b(false)),
                ("error", jsonl::diagnostic(code)),
            ]),
        };
        println!("{}", value.dump());
    }
}
