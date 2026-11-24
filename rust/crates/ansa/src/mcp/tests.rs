use super::{management_tests::Fixture, *};
use crate::tools::{invocation::Invocation, registry::Snapshot};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
};

const TOOL: &str = r#"{"name":"echo","inputSchema":{"type":"object"}}"#;
const SHELL: &str = match option_env!("ANSA_TEST_BASH") {
    Some(value) => value,
    None => "/bin/bash",
};

fn params() -> Json {
    Json::parse(r#"{"name":"echo","arguments":{"value":"fixture"}}"#).unwrap()
}

#[test]
fn stdio_discovers_then_executes_verified_tool_and_handles_ping() {
    let args = [
        "-c",
        r#"while IFS= read -r message; do
            case "$message" in
            *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}' ;;
            *'"id":2,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
            *'"id":3,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
            *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":"probe","method":"ping"}'
                IFS= read -r ping
                [[ "$ping" == '{"jsonrpc":"2.0","id":"probe","result":{}}' ]] || exit 2
                printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"fixture"}]}}' ;;
            esac
        done"#,
    ]
    .map(str::to_owned)
    .to_vec();
    let stop = Cancel::default();
    let (worker, catalog) = Worker::start(
        Transport::Stdio {
            command: SHELL.into(),
            args,
        },
        Features {
            tools: true,
            ..Features::default()
        },
        std::env::temp_dir(),
        stop.clone(),
        Instant::now() + TIMEOUT,
    )
    .unwrap();
    assert_eq!(catalog.get("echo"), Some(&Json::parse(TOOL).unwrap()));
    let outcome = worker.invoke(params(), Arc::new(catalog["echo"].clone()), &stop);
    assert_eq!(
        (outcome.execution(), outcome.effects()),
        (Execution::Succeeded, Effects::KnownResult)
    );
    assert!(
        std::str::from_utf8(outcome.output())
            .unwrap()
            .contains("fixture")
    );
}

fn http_request(stream: &mut TcpStream) -> (String, String) {
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line.to_ascii_lowercase());
    }
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .map(|value| value.parse().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (headers, String::from_utf8(body).unwrap())
}

fn http_reply(stream: &mut TcpStream, status: &str, mime: &str, extra: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

#[test]
fn streamable_http_negotiates_session_and_reads_sse_result() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        for index in 0..7 {
            let (mut stream, _) = listener.accept().unwrap();
            let (headers, body) = http_request(&mut stream);
            assert!(headers.contains("accept: application/json, text/event-stream"));
            if index > 0 {
                assert!(headers.contains("mcp-session-id: fixture-session"));
                assert!(headers.contains("mcp-protocol-version: 2025-11-25"));
            }
            let (status, mime, extra, body) = match index {
                0 => ("200 OK", "application/json", "Mcp-Session-Id: fixture-session\r\n",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}}"#.into()),
                1 => ("202 Accepted", "application/json", "", String::new()),
                2 | 3 => ("200 OK", "application/json", "", format!("{{\"jsonrpc\":\"2.0\",\"id\":{index},\"result\":{{\"tools\":[{TOOL}]}}}}")),
                4 => ("200 OK", "text/event-stream", "", concat!(
                    "data: {\"jsonrpc\":\"2.0\",\"id\":\"probe\",\"method\":\"ping\"}\n\n",
                    "data: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"fixture\"}]}}\n\n"
                ).into()),
                5 => {
                    assert_eq!(body, r#"{"jsonrpc":"2.0","id":"probe","result":{}}"#);
                    ("202 Accepted", "application/json", "", String::new())
                }
                _ => { assert!(headers.starts_with("delete /mcp ")); ("200 OK", "application/json", "", String::new()) }
            };
            http_reply(&mut stream, status, mime, extra, &body);
        }
    });
    let stop = Cancel::default();
    let (server, catalog) = Worker::start(
        Transport::Http {
            url,
            bearer_env: None,
        },
        Features {
            tools: true,
            ..Features::default()
        },
        std::env::temp_dir(),
        stop.clone(),
        Instant::now() + TIMEOUT,
    )
    .unwrap();
    let outcome = server.invoke(params(), Arc::new(catalog["echo"].clone()), &stop);
    assert_eq!(
        (outcome.execution(), outcome.effects()),
        (Execution::Succeeded, Effects::KnownResult)
    );
    drop(server);
    worker.join().unwrap();
}

fn stdio_connection(script: &str) -> serde_json::Value {
    json!({"transport":"stdio", "command":SHELL, "args":["-c", script]})
}

fn fixture_snapshot(fixture: &Fixture, saved: Option<&Advertisement>) -> Result<Arc<Snapshot>> {
    let mut registry = Registry::new();
    register(
        &mut registry,
        &fixture.0,
        &fixture.0,
        saved,
        &Cancel::default(),
    )?;
    Ok(registry.snapshot())
}

fn wrapper(params: &str) -> Json {
    Json::object(vec![("arguments_json", Json::string(params).unwrap())]).unwrap()
}

fn registered_call(snapshot: &Snapshot, method: Method, params: &str, stop: &Cancel) -> Outcome {
    let name = method.local_name("fixture");
    assert!(snapshot.policy(&name).unwrap().read_only);
    registered_tool(snapshot, &name, params, stop)
}

fn registered_tool(snapshot: &Snapshot, name: &str, params: &str, stop: &Cancel) -> Outcome {
    let definition = snapshot.definition(name).unwrap();
    let arguments = wrapper(params);
    assert!((definition.validator)(&arguments));
    let invocation = Invocation {
        operation: "fixture-operation".into(),
        call: "fixture-call".into(),
        name: name.into(),
        revision: definition.descriptor.revision.clone(),
        arguments,
    };
    let policy = snapshot.policy(name).unwrap();
    assert!(policy.requires_journal);
    policy.invoke.as_ref().unwrap()(&invocation, stop)
}

// Common stdio handshake for focused faults. Capabilities and body are only
// static synthetic fixture literals, never user input.
fn fault_script(capabilities: &str, cases: &str) -> String {
    format!(
        r#"while IFS= read -r message; do
            case "$message" in
            *'"id":1,'*'"method":"initialize"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-11-25","capabilities":{capabilities}}}}}' ;;
            *'"method":"notifications/initialized"'*) : > initialized ;;
            {cases}
            *) exit 2 ;;
            esac
        done"#
    )
}

#[test]
fn mixed_catalog_keeps_legacy_hash_and_saved_subset() {
    let fixture = Fixture::new();
    let cases = r#"
        *'"id":2,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
    "#;
    let script = fault_script(r#"{"tools":{},"resources":{}}"#, cases);
    let connection = stdio_connection(&script);
    fixture.config(json!({"fixture": {"connection": &connection, "tools": ["echo"]}}));
    let legacy = fixture_snapshot(&fixture, None).unwrap();
    let saved = legacy.advertisement();
    let tool = &saved.descriptors[0];
    assert_eq!(
        tool.name,
        format!("mcp_echo_{}", &sha256(b"fixture/echo")[..16])
    );
    // Pin the legacy digest inputs independently of Transport serialization,
    // catalog canonicalization, and Registry record construction.
    let transport_bytes = format!(
        r#"{{"transport":"stdio","command":{},"args":["-c",{}]}}"#,
        serde_json::to_string(SHELL).unwrap(),
        serde_json::to_string(&script).unwrap(),
    );
    let transport = sha256(transport_bytes.as_bytes());
    let expected_revision = sha256(
        format!(r#"{transport}/false/{{"inputSchema":{{"type":"object"}},"name":"echo"}}"#)
            .as_bytes(),
    );
    assert_eq!(tool.revision, expected_revision);
    let wrapper_bytes = r#"{"type":"object","properties":{"arguments_json":{"type":"string","description":"JSON object matching the MCP input schema"}},"required":["arguments_json"],"additionalProperties":false}"#;
    let advertisement_bytes = format!(
        r#"[{{"descriptor":{{"name":"{}","description":"fixture/echo: . Pass arguments as a JSON object matching {{\"type\":\"object\"}}","implementation_revision":"{expected_revision}","schema":{wrapper_bytes}}},"requires_journal":true}}]"#,
        tool.name,
    );
    assert_eq!(saved.revision, sha256(advertisement_bytes.as_bytes()));

    fixture.config(json!({"fixture": {
        "connection": &connection, "tools": ["echo"], "resources": true,
    }}));
    let expanded = fixture_snapshot(&fixture, None).unwrap();
    assert_eq!(expanded.advertisement().descriptors.len(), 3);
    let plan = expanded.read_only();
    assert_eq!(plan.advertisement().descriptors.len(), 2);
    assert!(plan.definition(&tool.name).is_none());
    assert!(
        plan.definition(&Method::ResourcesRead.local_name("fixture"))
            .is_some()
    );
    assert_eq!(expanded.compatible(saved).unwrap().advertisement(), saved);

    // The existing server does not support prompts. Adding that config must
    // still permit opening the pinned legacy catalog, but not a fresh catalog.
    fixture.config(json!({"fixture": {
        "connection": &connection, "tools": ["echo"], "resources": true, "prompts": true,
    }}));
    let reopened = fixture_snapshot(&fixture, Some(saved)).unwrap();
    assert_eq!(reopened.advertisement(), saved);
    assert_eq!(
        fixture_snapshot(&fixture, None).err(),
        Some("mcp_capability_prompts")
    );
}

#[test]
fn configured_discovery_rejects_missing_and_malformed_capabilities() {
    for (resources, prompts, caps, expected) in [
        (true, false, r#"{"prompts":{}}"#, "mcp_capability_resources"),
        (false, true, r#"{"resources":{}}"#, "mcp_capability_prompts"),
        (
            true,
            false,
            r#"{"resources":{"subscribe":"yes"}}"#,
            "mcp_capability_resources",
        ),
        (
            false,
            true,
            r#"{"prompts":false}"#,
            "mcp_capability_prompts",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.config(json!({"fixture": {
            "connection": stdio_connection(&fault_script(caps, "")),
            "resources": resources, "prompts": prompts,
        }}));
        let mut registry = Registry::new();
        assert_eq!(
            register(
                &mut registry,
                &fixture.0,
                &fixture.0,
                None,
                &Cancel::default()
            ),
            Err(expected)
        );
        assert!(registry.snapshot().advertisement().descriptors.is_empty());
        assert!(!fixture.0.join("initialized").exists());
    }
}

#[test]
fn discovery_retires_stream_after_invalid_envelope_eof_or_oversized_frame() {
    for (response, code) in [
        (
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{},"error":{"code":-32602,"message":"canary"}}'"#,
            "mcp_invalid_result",
        ),
        ("exit 0", "mcp_stream_closed"),
        ("printf '%1048576s' ''", "mcp_frame"),
    ] {
        let fixture = Fixture::new();
        let cases = format!(r#"*'"id":2,'*'"method":"prompts/list"'*) {response} ;;"#);
        fixture.config(json!({"fixture": {
            "connection": stdio_connection(&fault_script(r#"{"prompts":{}}"#, &cases)),
            "prompts": true,
        }}));
        let snapshot = fixture_snapshot(&fixture, None).unwrap();
        let stop = Cancel::default();
        let outcome = registered_call(&snapshot, Method::PromptsList, "{}", &stop);
        assert_eq!(outcome.execution(), Execution::Failed);
        assert_eq!(outcome.effects(), Effects::Unknown);
        assert_eq!(
            outcome.output(),
            format!(r#"{{"error":"{code}"}}"#).as_bytes()
        );
        assert_eq!(
            registered_call(&snapshot, Method::PromptsList, "{}", &stop),
            Outcome::failed_known(r#"{"error":"mcp_disconnected"}"#.into())
        );
    }
}

#[test]
fn cancel_during_catalog_recheck_differs_from_cancel_after_discovery_dispatch() {
    for during_tool_catalog in [true, false] {
        let fixture = Fixture::new();
        let (caps, cases, tools): (&str, &str, &[&str]) = if during_tool_catalog {
            (
                r#"{"tools":{}}"#,
                r#"
                *'"id":2,'*'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}' ;;
                *'"id":3,'*'"method":"tools/list"'*) : > waiting ;;
                *'"method":"tools/call"'*) : > dispatched ;;
            "#,
                &["echo"],
            )
        } else {
            (
                r#"{"prompts":{}}"#,
                r#"*'"id":2,'*'"method":"prompts/list"'*) : > waiting ;;"#,
                &[],
            )
        };
        fixture.config(json!({"fixture": {
            "connection": stdio_connection(&fault_script(caps, cases)),
            "tools": tools, "prompts": !during_tool_catalog,
        }}));
        let snapshot = fixture_snapshot(&fixture, None).unwrap();
        let stop = Cancel::default();
        let outcome = std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                if during_tool_catalog {
                    registered_tool(&snapshot, &local_name("fixture", "echo"), "{}", &stop)
                } else {
                    registered_call(&snapshot, Method::PromptsList, "{}", &stop)
                }
            });
            let wait = fixture.0.join("waiting");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !wait.exists() && !running.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.cancel();
            assert!(wait.exists(), "fixture did not receive the expected RPC");
            running.join().unwrap()
        });
        if during_tool_catalog {
            assert_eq!(outcome, Outcome::not_started());
            drop(snapshot);
            assert!(!fixture.0.join("dispatched").exists());
        } else {
            assert_eq!(outcome.execution(), Execution::Failed);
            assert_eq!(outcome.effects(), Effects::Unknown);
            assert_eq!(outcome.output(), br#"{"error":"auth_cancelled"}"#);
        }
    }
}

#[test]
fn managed_stdio_uses_only_selected_methods_and_returns_one_page_at_a_time() {
    let script = r#"while IFS= read -r message; do
        case "$message" in
        *'"id":1,'*'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"resources":{},"prompts":{}}}}' ;;
        *'"method":"notifications/initialized"'*) initialized=yes ;;
        *'"id":2,'*'"method":"resources/list","params":{}'*)
            [[ "$initialized" == yes ]] || exit 3
            printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}'
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resources":[{"uri":"mem://fixture/guide","name":"guide"}],"nextCursor":"+page/2="}}' ;;
        *'"id":3,'*'"method":"resources/list","params":{"cursor":"+page/2="}'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resources":[]}}' ;;
        *'"id":4,'*'"method":"resources/read","params":{"uri":"mem://fixture/guide"}'*) printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"contents":[{"uri":"mem://fixture/guide","text":"fixture content"}]}}' ;;
        *'"id":5,'*'"method":"prompts/list","params":{}'*) printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"prompts":[{"name":"brief","arguments":[{"name":"topic","required":true}]}]}}' ;;
        *'"id":6,'*'"method":"prompts/get","params":{"name":"brief","arguments":{"topic":"Rust"}}'*) printf '%s\n' '{"jsonrpc":"2.0","id":6,"result":{"messages":[{"role":"user","content":{"type":"text","text":"fixture prompt"}}]}}' ;;
        *'"id":7,'*'"method":"prompts/get","params":{"name":"missing"}'*) printf '%s\n' '{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"synthetic-private-message"}}' ;;
        *'"id":8,'*'"method":"resources/list","params":{}'*) printf '%s\n' '{"jsonrpc":"2.0","id":8,"result":{"resources":42}}' ;;
        *'"id":9,'*'"method":"prompts/list","params":{}'*) printf '%s\n' '{"jsonrpc":"2.0","id":9,"result":{"prompts":[]}}' ;;
        *) exit 2 ;;
        esac
    done"#;
    let fixture = Fixture::new();
    fixture.config(json!({"fixture": {
        "connection": stdio_connection(script), "resources": true, "prompts": true,
    }}));
    let stop = Cancel::default();
    let work = fixture_snapshot(&fixture, None).unwrap();
    let snapshot = work.read_only();
    assert_eq!(snapshot.advertisement(), work.advertisement());
    assert_eq!(snapshot.advertisement().descriptors.len(), 4);
    assert!(snapshot.requires_journal());
    for (method, args) in [
        (Method::ResourcesList, r#"{"cursor":7}"#),
        (Method::ResourcesRead, r#"{"uri":"relative/path"}"#),
        (Method::PromptsList, r#"{"extra":true}"#),
        (
            Method::PromptsGet,
            r#"{"name":"brief","arguments":{"topic":null}}"#,
        ),
    ] {
        let definition = snapshot.definition(&method.local_name("fixture")).unwrap();
        assert!(!(definition.validator)(&wrapper(args)));
    }
    let huge = format!(
        r#"{{"name":"brief","arguments":{{"topic":"{}"}}}}"#,
        "x".repeat(64 * 1024)
    );
    let prompt = snapshot
        .definition(&Method::PromptsGet.local_name("fixture"))
        .unwrap();
    assert!(!(prompt.validator)(&wrapper(&huge)));
    let cancelled = Cancel::default();
    cancelled.cancel();
    assert_eq!(
        registered_call(&snapshot, Method::ResourcesList, "{}", &cancelled),
        Outcome::not_started()
    );
    for (method, params, expected) in [
        (
            Method::ResourcesList,
            "{}",
            r#"{"resources":[{"uri":"mem://fixture/guide","name":"guide"}],"nextCursor":"+page/2="}"#,
        ),
        (
            Method::ResourcesList,
            r#"{"cursor":"+page/2="}"#,
            r#"{"resources":[]}"#,
        ),
        (
            Method::ResourcesRead,
            r#"{"uri":"mem://fixture/guide"}"#,
            r#"{"contents":[{"uri":"mem://fixture/guide","text":"fixture content"}]}"#,
        ),
        (
            Method::PromptsList,
            "{}",
            r#"{"prompts":[{"name":"brief","arguments":[{"name":"topic","required":true}]}]}"#,
        ),
        (
            Method::PromptsGet,
            r#"{"name":"brief","arguments":{"topic":"Rust"}}"#,
            r#"{"messages":[{"role":"user","content":{"type":"text","text":"fixture prompt"}}]}"#,
        ),
    ] {
        assert_eq!(
            registered_call(&snapshot, method, params, &stop),
            Outcome::succeeded(expected.into())
        );
    }
    assert_eq!(
        registered_call(
            &snapshot,
            Method::PromptsGet,
            r#"{"name":"missing"}"#,
            &stop
        ),
        Outcome::failed_known(r#"{"error":"mcp_rpc_error"}"#.into())
    );
    assert_eq!(
        registered_call(&snapshot, Method::ResourcesList, "{}", &stop),
        Outcome::failed_known(r#"{"error":"mcp_discovery_result"}"#.into())
    );
    assert_eq!(
        registered_call(&snapshot, Method::PromptsList, "{}", &stop),
        Outcome::succeeded(r#"{"prompts":[]}"#.into())
    );
}

#[test]
fn managed_http_sse_uses_negotiated_session_without_tools_capability() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let fixture = std::thread::spawn(move || {
        for index in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let (headers, body) = http_request(&mut stream);
            if index > 0 {
                assert!(headers.contains("mcp-session-id: fixture-session"));
                assert!(headers.contains("mcp-protocol-version: 2025-03-26"));
            }
            match index {
                0 => {
                    let request = Json::parse(&body).unwrap();
                    assert_eq!(request.text_field("method"), Ok("initialize"));
                    assert_eq!(
                        request.field("params").unwrap().field("capabilities"),
                        Ok(&Json::parse("{}").unwrap())
                    );
                    http_reply(
                        &mut stream,
                        "200 OK",
                        "application/json",
                        "Mcp-Session-Id: fixture-session\r\n",
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"prompts":{}}}}"#,
                    );
                }
                1 => {
                    assert_eq!(
                        body,
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
                    );
                    http_reply(&mut stream, "202 Accepted", "application/json", "", "");
                }
                2 => {
                    assert_eq!(
                        body,
                        r#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"fixture"}}"#
                    );
                    http_reply(
                        &mut stream,
                        "200 OK",
                        "text/event-stream",
                        "",
                        concat!(
                            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/prompts/list_changed\"}\n\n",
                            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"messages\":[]}}\n\n",
                        ),
                    );
                }
                _ => {
                    assert!(headers.starts_with("delete /mcp "));
                    http_reply(&mut stream, "200 OK", "application/json", "", "");
                }
            }
        }
    });
    let stop = Cancel::default();
    let (worker, tools) = Worker::start(
        Transport::Http {
            url,
            bearer_env: None,
        },
        Features {
            prompts: true,
            ..Features::default()
        },
        std::env::temp_dir(),
        stop.clone(),
        Instant::now() + TIMEOUT,
    )
    .unwrap();
    assert!(tools.is_empty());
    let outcome = worker.invoke_discovery(
        Method::PromptsGet,
        Json::parse(r#"{"name":"fixture"}"#).unwrap(),
        &stop,
    );
    assert_eq!(outcome, Outcome::succeeded(r#"{"messages":[]}"#.into()));
    drop(worker);
    fixture.join().unwrap();
}
