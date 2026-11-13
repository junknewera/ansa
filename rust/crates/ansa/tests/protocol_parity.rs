#[path = "../src/protocol/error.rs"]
pub mod error;
#[path = "../src/protocol/json.rs"]
pub mod json;
#[path = "../src/protocol/legacy_json_v1.rs"]
pub mod legacy_json_v1;
#[path = "../src/protocol/sse.rs"]
pub mod sse;
mod protocol {
    pub use crate::{json, legacy_json_v1, sse};
}
#[path = "../src/transport/http.rs"]
pub mod http_impl;
#[path = "../src/provider/mod.rs"]
pub mod provider;
#[path = "../src/tools/mod.rs"]
#[allow(dead_code)] // Source copy intentionally exercises M2 without the M5 owner.
pub mod tools;
pub use ansa::auth;
#[path = "../src/frontend/mod.rs"]
#[allow(dead_code)]
pub mod frontend;
#[path = "../src/session/transcript.rs"]
#[allow(dead_code)]
pub mod transcript;
mod session {
    pub use crate::transcript;
}
mod transport {
    pub use crate::http_impl as http;
}
use json::Json;
use provider::{
    replay::{Effects, Execution, continuation},
    request::{Advertisement, Binding, Descriptor, Epoch, Request},
};
use sse::{Limits, Parser};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tools::admission::OperationIds;
use tools::{
    admission::PreparedBatch,
    registry::{Definition, Executor, Outcome, Registry, Validator},
};
use transport::http::{Head, RESPONSES_ENDPOINT, admit_request};

fn fixture(name: &str) -> Vec<u8> {
    let root = std::env::var_os("ANSA_PROTOCOL_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures")
        });
    fs::read(root.join(name)).expect("protocol fixture")
}
fn request() -> Request {
    let schema = Json::parse(
        r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#,
    )
    .unwrap();
    let descriptor = Descriptor {
        name: "get_test_value".into(),
        description: "pure fixture".into(),
        revision: "1".into(),
        schema,
    };
    let record = Json::object(vec![
        ("name", Json::string(&descriptor.name).unwrap()),
        (
            "description",
            Json::string(&descriptor.description).unwrap(),
        ),
        (
            "implementation_revision",
            Json::string(&descriptor.revision).unwrap(),
        ),
        ("schema", descriptor.schema.clone()),
    ])
    .unwrap();
    let revision = legacy_json_v1::sha256(Json::array(vec![record]).unwrap().dump().as_bytes());
    Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        Epoch {
            id: "epoch_1".into(),
            instructions: "Exact fixture instructions".into(),
            exact_state: Json::parse(
                r#"{"constraints":["offline","pure only"],"root":"fixture","revision":1}"#,
            )
            .unwrap(),
        },
        Advertisement {
            revision,
            descriptors: vec![descriptor],
        },
        vec![
            Json::parse(r#"{"role":"developer","content":"fixture"}"#).unwrap(),
            Json::parse(r#"{"role":"user","content":"Call omh.get_test_value"}"#).unwrap(),
        ],
    )
    .unwrap()
}
fn with(value: &Json, key: &str, replacement: Json) -> Json {
    value.with(key, replacement).unwrap()
}
fn calls_with_second(arguments: &str) -> Vec<u8> {
    let source = String::from_utf8(fixture("round1.sse")).unwrap();
    let mut events: Vec<Json> = source
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|s| Json::parse(s).unwrap())
        .collect();
    let mut added = events[3].clone();
    added = with(&added, "sequence_number", Json::parse("8").unwrap());
    added = with(&added, "output_index", Json::parse("2").unwrap());
    let mut added_item = added.field("item").unwrap().clone();
    added_item = with(&added_item, "id", Json::string("fc_b").unwrap());
    added_item = with(&added_item, "call_id", Json::string("call_b").unwrap());
    added = with(&added, "item", added_item.clone());
    let mut done = events[7].clone();
    done = with(&done, "sequence_number", Json::parse("9").unwrap());
    done = with(&done, "output_index", Json::parse("2").unwrap());
    let mut done_item = added_item;
    done_item = with(&done_item, "arguments", Json::string(arguments).unwrap());
    done_item = with(&done_item, "status", Json::string("completed").unwrap());
    done = with(&done, "item", done_item.clone());
    events[8] = with(&events[8], "sequence_number", Json::parse("10").unwrap());
    let mut response = events[8].field("response").unwrap().clone();
    let mut output = response
        .field("output")
        .unwrap()
        .elements()
        .unwrap()
        .to_vec();
    output.push(done_item);
    response = with(&response, "output", Json::array(output).unwrap());
    events[8] = with(&events[8], "response", response);
    events.insert(8, added);
    events.insert(9, done);
    events
        .iter()
        .map(|e| format!("data: {}\n\n", e.dump()))
        .collect::<String>()
        .into_bytes()
}
fn fixture_events(name: &str) -> Vec<Json> {
    String::from_utf8(fixture(name))
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| Json::parse(data).unwrap())
        .collect()
}
fn wire(events: &[Json]) -> Vec<u8> {
    events
        .iter()
        .enumerate()
        .map(|(i, event)| {
            format!(
                "data: {}\n\n",
                with(
                    event,
                    "sequence_number",
                    Json::parse(&i.to_string()).unwrap()
                )
                .dump()
            )
        })
        .collect::<String>()
        .into_bytes()
}
fn decode(name: &str) -> Result<provider::responses::AdmittedResponse, &'static str> {
    let body = fixture(name);
    decode_synthetic(
        request(),
        Head {
            status: 200,
            content_types: vec!["text/event-stream".into()],
        },
        &[&body],
    )
}
fn decode_synthetic(
    request: Request,
    head: Head,
    chunks: &[&[u8]],
) -> Result<provider::responses::AdmittedResponse, &'static str> {
    http_impl::validate_head(&head)?;
    provider::responses::decode_synthetic(
        request,
        head.status,
        &head.content_types[0],
        chunks,
        Limits::default(),
    )
}

#[test]
fn dependencies_json_ownership_and_structural_equality() {
    for bad in [
        "",
        "{}junk",
        "{broken}",
        "[NaN]",
        "[Infinity]",
        "[1e9999]",
        r#"{"x":1,"x":2}"#,
        r#"{"a":1,"\u0061":2}"#,
        r#"{"x":"\ud800"}"#,
        "[1,]",
        "/*comment*/{}",
    ] {
        assert!(Json::parse(bad).is_err(), "{bad}")
    }
    let exact =
        r#"{"integer":123456789012345678901234567890,"decimal":0.123456789012345678901234567890}"#;
    assert_eq!(Json::parse(exact).unwrap().dump(), exact);
    assert_eq!(
        Json::parse(r#"{"b":2,"a":1}"#).unwrap(),
        Json::parse(r#"{"a":1,"b":2}"#).unwrap()
    );
    assert_ne!(Json::parse("1").unwrap(), Json::parse("1.0").unwrap());
    assert!(Json::parse_with_limits("{}", 1, 64, 10).is_err());
    assert!(Json::parse_with_limits("[[[]]]", 64, 3, 10).is_err());
    assert!(Json::parse_with_limits("[1,2,3]", 64, 64, 2).is_err());
    let empty = Json::parse("{}").unwrap();
    let mut deep = Json::parse("0").unwrap();
    for _ in 0..63 {
        deep = Json::array(vec![deep]).unwrap();
    }
    assert!(
        empty.with("x", deep).is_err(),
        "with must recheck aggregate depth"
    );
    let mut many = Vec::with_capacity(131_071);
    for _ in 0..131_071 {
        many.push(Json::parse("0").unwrap());
    }
    let many = Json::array(many).unwrap();
    assert!(
        empty.with("x", many).is_err(),
        "with must recheck aggregate node count"
    );
    assert!(
        empty
            .with(&"k".repeat(16 * 1024 * 1024), Json::parse("0").unwrap())
            .is_err(),
        "oversized key must be rejected without a dump panic"
    );
    assert!(
        empty
            .with(
                &"a".repeat(8 * 1024 * 1024),
                Json::string(&"b".repeat(8 * 1024 * 1024)).unwrap()
            )
            .is_err(),
        "with must recheck aggregate serialized byte size"
    );
    let large_source = format!("\"{}\"", "x".repeat(16 * 1024 * 1024));
    let large = Json::parse_with_limits(&large_source, 32 * 1024 * 1024, 64, 1).unwrap();
    assert_eq!(
        large.dump(),
        large_source,
        "dump must support accepted custom byte limits"
    );
    for token in [
        "true",
        "false",
        "-1",
        "1.0",
        "1e0",
        "18446744073709551616",
        "null",
        r#""0""#,
    ] {
        assert!(Json::parse(token).unwrap().index().is_err(), "{token}")
    }
    assert_eq!(Json::parse("0").unwrap().index().unwrap(), 0);
    let diagnostic = error::ProtocolError::new(
        error::ErrorCode::Protocol,
        error::Stage::Admission,
        "fixed_code",
    )
    .json()
    .unwrap();
    assert_eq!(
        diagnostic.field("schema_version").unwrap().index().unwrap(),
        1
    );
    assert_eq!(
        diagnostic.field("diagnostic").unwrap().text().unwrap(),
        "fixed_code"
    );
}

#[test]
fn request_roles_registration_bounds_and_verified_mime() {
    let base = request();
    for role in ["1", "null", "{}"] {
        let input = vec![Json::parse(&format!(r#"{{"role":{role}}}"#)).unwrap()];
        assert!(
            Request::new(
                base.binding.clone(),
                base.epoch.clone(),
                base.tools.clone(),
                input
            )
            .is_err()
        );
    }

    let descriptor = base.tools.descriptors[0].clone();
    let validator: Validator = Arc::new(|_| true);
    let executor: Executor = Arc::new(|_| Outcome::succeeded("ok".into()));
    for (name, revision, accepted) in [
        ("a".repeat(64), "r".repeat(128), true),
        ("a".repeat(65), "1".into(), false),
        ("bad-name".into(), "1".into(), false),
        ("naïve".into(), "1".into(), false),
        ("a".into(), "r".repeat(129), false),
    ] {
        let mut candidate = descriptor.clone();
        candidate.name = name;
        candidate.revision = revision;
        let mut registry = Registry::new();
        let registered = registry.register(Definition {
            descriptor: candidate,
            validator: Arc::clone(&validator),
            executor: Arc::clone(&executor),
        });
        assert_eq!(registered.is_ok(), accepted);
    }

    for mime in [
        "TEXT/EVENT-STREAM",
        "text/event-stream; charset=utf-8",
        "text/event-stream; charset=\"utf-8\"",
    ] {
        let body = fixture("round1.sse");
        let response = decode_synthetic(
            request(),
            Head {
                status: 200,
                content_types: vec![mime.into()],
            },
            &[&body],
        );
        assert!(response.is_ok(), "valid media type {mime}: {response:?}");
    }
    assert!(OperationIds::new("a".repeat(64), 0).is_ok());
    for run in [
        "a".repeat(65),
        "bad-run".into(),
        "naïve".into(),
        String::new(),
    ] {
        assert!(OperationIds::new(run, 0).is_err());
    }
}

#[test]
fn frontend_contract_ids_versions_and_cursors() {
    use provider::contracts::*;
    assert_ne!(
        std::any::TypeId::of::<SessionId>(),
        std::any::TypeId::of::<TurnId>()
    );
    assert_ne!(
        std::any::TypeId::of::<RequestId>(),
        std::any::TypeId::of::<OperationId>()
    );
    let mut command = RuntimeCommand {
        schema_version: 1,
        session: "session".into(),
        turn: "turn".into(),
        incarnation: "incarnation".into(),
        command: "command".into(),
        payload: CommandPayload::Submit(Submit {
            text: "fixture".into(),
            mode: AgentMode::Basic,
        }),
    };
    assert!(validate_command(&command).is_ok());
    command.schema_version = 2;
    assert!(validate_command(&command).is_err());
    command.schema_version = 1;
    command.payload = CommandPayload::Cancel("".into());
    assert!(validate_command(&command).is_err());
    command.payload = CommandPayload::Cancel("request".into());
    assert!(validate_command(&command).is_ok());
    command.payload = CommandPayload::GetSnapshot(17);
    assert!(validate_command(&command).is_ok());
    let event = |sequence, payload| RuntimeEvent {
        schema_version: 1,
        stream_sequence: sequence,
        session: "session".into(),
        turn: "turn".into(),
        incarnation: "incarnation".into(),
        request: None,
        payload,
    };
    assert!(
        validate_event(&event(
            18,
            EventPayload::Snapshot {
                through_sequence: 17,
                state: "idle".into()
            }
        ))
        .is_ok()
    );
    assert!(
        validate_event(&event(
            18,
            EventPayload::Snapshot {
                through_sequence: 19,
                state: "idle".into()
            }
        ))
        .is_err()
    );
    assert!(
        validate_event(&event(
            20,
            EventPayload::ResyncRequired {
                oldest_available: 20
            }
        ))
        .is_ok()
    );
    assert!(
        validate_event(&event(
            21,
            EventPayload::RecoveryRequired {
                operation: "op".into(),
                effects: Effects::Unknown
            }
        ))
        .is_ok()
    );
    assert!(
        validate_event(&event(
            0,
            EventPayload::ResyncRequired {
                oldest_available: 1
            }
        ))
        .is_err()
    );
}

#[test]
fn transport_head_grammar_endpoint_and_profile() {
    let req = request().json().unwrap();
    for value in [
        "text/event-stream",
        "Text/Event-Stream; charset=UTF-8",
        "text/event-stream; charset=\"utf-8\"",
    ] {
        assert!(
            admit_request(
                &Head {
                    status: 200,
                    content_types: vec![value.into()]
                },
                RESPONSES_ENDPOINT,
                "POST",
                &req,
                false
            )
            .is_ok()
        )
    }
    for value in [
        "text/event-stream; charset=utf-8; charset=utf-8",
        "text/event-stream; charset=\"utf-8",
        "text/event-stream, text/html",
        "text/html",
    ] {
        assert!(
            admit_request(
                &Head {
                    status: 200,
                    content_types: vec![value.into()]
                },
                RESPONSES_ENDPOINT,
                "POST",
                &req,
                false
            )
            .is_err(),
            "{value}"
        )
    }
    assert!(
        admit_request(
            &Head {
                status: 200,
                content_types: vec![]
            },
            RESPONSES_ENDPOINT,
            "POST",
            &req,
            false
        )
        .is_err()
    );
    assert!(
        admit_request(
            &Head {
                status: 200,
                content_types: vec![]
            },
            RESPONSES_ENDPOINT,
            "POST",
            &req,
            true
        )
        .is_ok()
    );
    for url in [
        "https://other.example/v1/responses",
        "http://api.openai.com/v1/responses",
        "https://api.openai.com/v1/responses?x=1",
        "https://api.openai.com/v1/models",
    ] {
        assert!(
            admit_request(
                &Head {
                    status: 200,
                    content_types: vec![]
                },
                url,
                "POST",
                &req,
                true
            )
            .is_err()
        )
    }
    assert!(
        admit_request(
            &Head {
                status: 200,
                content_types: vec![]
            },
            RESPONSES_ENDPOINT,
            "GET",
            &req,
            true
        )
        .is_err()
    );
    let bad = req.with("stream", Json::parse("false").unwrap()).unwrap();
    assert!(
        admit_request(
            &Head {
                status: 200,
                content_types: vec![]
            },
            RESPONSES_ENDPOINT,
            "POST",
            &bad,
            true
        )
        .is_err()
    );
}

#[test]
fn sse_all_splits_bytewise_utf8_crlf_eof_and_poison() {
    for newline in ["\n", "\r", "\r\n"] {
        let bytes=format!("\u{feff}: comment{newline}event: update{newline}data: α😀{newline}data: second{newline}{newline}data:{newline}{newline}event: discarded{newline}{newline}data: after reset{newline}{newline}").into_bytes();
        for split in 0..=bytes.len() {
            let mut p = Parser::new(Limits::default());
            p.feed(&bytes[..split]).unwrap();
            p.feed(&bytes[split..]).unwrap();
            let events = p.finish().unwrap();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0].name, "update");
            assert_eq!(events[0].data, "α😀\nsecond");
            assert_eq!(events[1].name, "message");
            assert_eq!(events[1].data, "");
            assert_eq!(events[2].name, "message");
        }
        let mut p = Parser::new(Limits::default());
        for b in &bytes {
            p.feed(&[*b]).unwrap()
        }
        assert_eq!(p.finish().unwrap().len(), 3);
    }
    let fields = Parser::new(Limits::default());
    let mut fields = fields;
    fields
        .feed(b"unknown: ignored\nid: 1\nretry: 10\ndata:  two spaces\n\n")
        .unwrap();
    assert_eq!(fields.finish().unwrap()[0].data, " two spaces");
    let mut bare = Parser::new(Limits::default());
    bare.feed(b"id\nretry\nevent\ndata: value\n\n").unwrap();
    let bare_event = bare.finish().unwrap();
    assert_eq!(bare_event[0].name, "message");
    assert_eq!(bare_event[0].data, "value");
    for bad in [
        b"data: unclosed".as_slice(),
        b"data: no boundary\n",
        b"event: unfinished\n",
        b"data: x\r",
        b"\xef\xbb",
        b"\xf0\x9f",
        b"data: \xc0\xaf\n\n",
    ] {
        let mut p = Parser::new(Limits::default());
        if p.feed(bad).is_ok() {
            assert!(p.finish().is_err())
        }
    }
    let mut p = Parser::new(Limits {
        line: 4,
        ..Limits::default()
    });
    assert!(p.feed(b"data: x\n\n").is_err());
    let long_line = format!("data: {}\n\n", "x".repeat(70 * 1024));
    let mut p = Parser::new(Limits::default());
    p.feed(long_line.as_bytes()).unwrap();
    assert_eq!(
        p.finish().unwrap()[0].data.len(),
        70 * 1024,
        "default SSE line limit matches C++ 1MiB"
    );
    let mut p = Parser::new(Limits::default());
    assert!(p.feed(b"\xc0").is_err());
    assert!(p.feed(b"x").is_err());
    for bad in [
        b"\xc0\xaf".as_slice(),
        b"\xed\xa0\x80",
        b"\xf4\x90\x80\x80",
        b"\x80",
        b"\xe2\x28\xa1",
        b"\xf5\x80\x80\x80",
    ] {
        let mut p = Parser::new(Limits::default());
        let mut malformed = b": ".to_vec();
        malformed.extend_from_slice(bad);
        malformed.extend_from_slice(b"\n\n");
        assert!(p.feed(&malformed).is_err(), "invalid UTF-8 sequence");
    }
    let mut limits = Limits {
        event: 12,
        ..Limits::default()
    };
    let mut p = Parser::new(limits);
    assert!(p.feed(b":123456\n:abcdef\n\n").is_err());
    limits = Limits {
        body: 8,
        ..Limits::default()
    };
    let mut p = Parser::new(limits);
    assert!(p.feed(b"data:\n\n:x\n").is_err());
    limits = Limits {
        events: 1,
        ..Limits::default()
    };
    let mut p = Parser::new(limits);
    assert!(p.feed(b"data:\n\ndata:\n\n").is_err());
    let mut p = Parser::new(Limits::default());
    p.finish().unwrap();
    assert!(p.feed(b"").is_err());
    assert!(p.finish().is_err());
}

#[test]
fn reducer_fixtures_models_order_and_rejected_streams() {
    let first = decode("round1.sse").unwrap();
    assert_eq!(first.items().len(), 2);
    assert_eq!(first.calls().len(), 1);
    assert_eq!(first.calls()[0].call_id, "call_a");
    assert_eq!(
        first.items()[0]
            .value()
            .field("encrypted_content")
            .unwrap()
            .text()
            .unwrap()
            .as_bytes(),
        "opaque:v1+/=\0λ".as_bytes()
    );
    assert_eq!(decode("round2.sse").unwrap().calls().len(), 0);
    let body = fixture("round1.sse");
    for split in 0..=body.len() {
        assert!(
            decode_synthetic(
                request(),
                Head {
                    status: 200,
                    content_types: vec!["text/event-stream".into()]
                },
                &[&body[..split], &body[split..]]
            )
            .is_ok(),
            "split {split}"
        )
    }
    let mut seed = 0x0a11ce_u64;
    for _ in 0..32 {
        let mut chunks = Vec::new();
        let mut pos = 0;
        while pos < body.len() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (1 + (seed as usize % 47)).min(body.len() - pos);
            chunks.push(&body[pos..pos + n]);
            pos += n;
        }
        assert!(
            decode_synthetic(
                request(),
                Head {
                    status: 200,
                    content_types: vec!["text/event-stream".into()]
                },
                &chunks
            )
            .is_ok()
        )
    }
    let mut body = fixture("round1.sse");
    let accepted = decode("round1.sse").is_ok();
    assert!(accepted);
    let original = String::from_utf8(body.clone()).unwrap();
    for (needle, replacement) in [
        ("\"sequence_number\":4", "\"sequence_number\":3"),
        (
            "\"type\":\"response.completed\"",
            "\"type\":\"response.failed\"",
        ),
        ("\"model\":\"gpt-6.1-sol\"", "\"model\":\"gpt-other\""),
    ] {
        let mutated = original.replacen(needle, replacement, 1);
        assert!(
            decode_synthetic(
                request(),
                Head {
                    status: 200,
                    content_types: vec!["text/event-stream".into()]
                },
                &[mutated.as_bytes()]
            )
            .is_err(),
            "{needle}"
        );
    }
    for bad in [
        "duplicate id",
        "wrong terminal output order",
        "missing native reasoning payload",
        "unknown executable item",
    ] {
        let mut doc = String::from_utf8(body.clone()).unwrap();
        match bad {
            "duplicate id" => doc = doc.replacen("\"id\":\"fc_a\"", "\"id\":\"rs_a\"", 1),
            "wrong terminal output order" => {
                doc = doc.replacen("\"output_index\":1", "\"output_index\":0", 1)
            }
            "missing native reasoning payload" => {
                doc = doc.replacen(
                    "\"encrypted_content\":\"opaque:v1+/=\\u0000\\u03bb\",",
                    "",
                    1,
                )
            }
            _ => {
                doc = doc.replacen(
                    "\"type\":\"function_call\"",
                    "\"type\":\"future_execute\"",
                    1,
                )
            }
        }
        assert!(
            decode_synthetic(
                request(),
                Head {
                    status: 200,
                    content_types: vec!["text/event-stream".into()]
                },
                &[doc.as_bytes()]
            )
            .is_err(),
            "{bad}"
        );
    }
    body.extend_from_slice(b"data: {unfinished");
    assert!(
        decode_synthetic(
            request(),
            Head {
                status: 200,
                content_types: vec!["text/event-stream".into()]
            },
            &[&body]
        )
        .is_err()
    );
    let bad_head = Head {
        status: 200,
        content_types: vec!["text/event-stream".into(), "text/event-stream".into()],
    };
    assert!(decode_synthetic(request(), bad_head, &[&fixture("round1.sse")]).is_err());
}

#[test]
fn streaming_preview_emits_ordered_utf8_and_cancels_before_next_event() {
    let mut events = fixture_events("round2.sse");
    events[3] = with(&events[3], "delta", Json::string("Привет").unwrap());
    events[4] = with(&events[4], "text", Json::string("Привет").unwrap());
    let first = with(
        events[5].field("part").unwrap(),
        "text",
        Json::string("Привет").unwrap(),
    );
    events[5] = with(&events[5], "part", first.clone());
    let second = Json::parse(r#"{"type":"refusal","refusal":"!"}"#).unwrap();
    let added = with(&second, "refusal", Json::string("").unwrap());
    let more = vec![
        Json::object(vec![
            ("type", Json::string("response.content_part.added").unwrap()),
            ("item_id", Json::string("msg_a").unwrap()),
            ("output_index", Json::parse("0").unwrap()),
            ("content_index", Json::parse("1").unwrap()),
            ("part", added.clone()),
        ])
        .unwrap(),
        Json::object(vec![
            ("type", Json::string("response.refusal.delta").unwrap()),
            ("item_id", Json::string("msg_a").unwrap()),
            ("output_index", Json::parse("0").unwrap()),
            ("content_index", Json::parse("1").unwrap()),
            ("delta", Json::string("!").unwrap()),
        ])
        .unwrap(),
        Json::object(vec![
            ("type", Json::string("response.refusal.done").unwrap()),
            ("item_id", Json::string("msg_a").unwrap()),
            ("output_index", Json::parse("0").unwrap()),
            ("content_index", Json::parse("1").unwrap()),
            ("refusal", Json::string("!").unwrap()),
        ])
        .unwrap(),
        Json::object(vec![
            ("type", Json::string("response.content_part.done").unwrap()),
            ("item_id", Json::string("msg_a").unwrap()),
            ("output_index", Json::parse("0").unwrap()),
            ("content_index", Json::parse("1").unwrap()),
            ("part", second.clone()),
        ])
        .unwrap(),
    ];
    events.splice(6..6, more);
    let mut item = events[10].field("item").unwrap().clone();
    item = with(&item, "content", Json::array(vec![first, second]).unwrap());
    events[10] = with(&events[10], "item", item.clone());
    let response = events[11].field("response").unwrap().clone();
    let mut output = response
        .field("output")
        .unwrap()
        .elements()
        .unwrap()
        .to_vec();
    output[0] = item;
    events[11] = with(
        &events[11],
        "response",
        with(&response, "output", Json::array(output).unwrap()),
    );
    let prefix = wire(&events[..11]);
    let complete_wire = wire(&events);
    let tail = &complete_wire[prefix.len()..];
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&seen);
    let mut stream = provider::responses::ResponseStream::new_with_callbacks(
        request(),
        Limits::default(),
        Some(Box::new(move |text| {
            sink.lock().unwrap().push(text.to_owned())
        })),
        None,
    );
    stream.feed(&prefix).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["Привет", "Привет!"]);
    stream.feed(tail).unwrap();
    let result = stream.finish();
    assert!(result.is_ok(), "{:?}", result.err());
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let set = Arc::clone(&cancel);
    let check = Arc::clone(&cancel);
    let mut stream = provider::responses::ResponseStream::new_with_callbacks(
        request(),
        Limits::default(),
        Some(Box::new(move |_| set.store(true, Ordering::SeqCst))),
        Some(Box::new(move || check.load(Ordering::SeqCst))),
    );
    assert!(stream.feed(&wire(&events)).is_err());
    let mut failed = events.clone();
    failed[11] = with(
        &failed[11],
        "type",
        Json::string("response.failed").unwrap(),
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&seen);
    let mut stream = provider::responses::ResponseStream::new_with_callbacks(
        request(),
        Limits::default(),
        Some(Box::new(move |text| {
            sink.lock().unwrap().push(text.to_owned())
        })),
        None,
    );
    assert!(stream.feed(&wire(&failed)).is_err());
    assert!(!seen.lock().unwrap().is_empty());
    assert!(stream.finish().is_err());

    let mut wrong_model = fixture_events("round2.sse");
    let created = wrong_model[0].field("response").unwrap().clone();
    wrong_model[0] = with(
        &wrong_model[0],
        "response",
        with(&created, "model", Json::string("wrong-model").unwrap()),
    );
    for invalid in [wrong_model, fixture_events("round2.sse")[1..].to_vec()] {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let mut stream = provider::responses::ResponseStream::new_with_callbacks(
            request(),
            Limits::default(),
            Some(Box::new(move |text| {
                sink.lock().unwrap().push(text.to_owned())
            })),
            None,
        );
        assert!(stream.feed(&wire(&invalid)).is_err());
        assert!(seen.lock().unwrap().is_empty());
        assert!(stream.finish().is_err());
    }
}

#[test]
fn replay_serializes_prefix_native_values_and_matching_results() {
    let response = decode("round1.sse").unwrap();
    let mut registry = Registry::new();
    let descriptor = response.request().tools.descriptors[0].clone();
    let validator: Validator = Arc::new(|_| true);
    let executor: Executor = Arc::new(|_| Outcome::succeeded(r#"{"ok":true}"#.into()));
    registry
        .register(Definition {
            descriptor,
            validator,
            executor,
        })
        .unwrap();
    let snapshot = registry.snapshot();
    let mut ids = OperationIds::new("runtime", 0).unwrap();
    let batch = PreparedBatch::prepare(&response, snapshot, &mut ids).unwrap();
    let result = batch.execute(false);
    let next = continuation(&response, &result).unwrap();
    let serialized = next.json().unwrap();
    let input = serialized.field("input").unwrap().elements().unwrap();
    assert_eq!(input.len(), 5, "replay oracle: native item count");
    assert_eq!(
        &input[2..4],
        &[
            response.items()[0].value().clone(),
            response.items()[1].value().clone(),
        ],
        "replay oracle: native item order and values"
    );
    assert_eq!(input[0], response.request().input[0]);
    assert_eq!(input[1], response.request().input[1]);
    assert_eq!(input[2], *response.items()[0].value());
    assert_eq!(input[3], *response.items()[1].value());
    assert_eq!(input[4].text_field("call_id").unwrap(), "call_a");
    assert_eq!(input[4].text_field("output").unwrap(), r#"{"ok":true}"#);
}

#[test]
fn replay_results_are_bound_to_the_exact_admitted_response() {
    let response = decode("round1.sse").unwrap();
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor: response.request().tools.descriptors[0].clone(),
            validator: Arc::new(|_| true),
            executor: Arc::new(|_| Outcome::succeeded("ok".into())),
        })
        .unwrap();
    let mut ids = OperationIds::new("runtime", 0).unwrap();
    let results = PreparedBatch::prepare(&response, registry.snapshot(), &mut ids)
        .unwrap()
        .execute(false);

    let mut saved = provider::native::save(&response).unwrap();
    let mut items = saved.field("items").unwrap().elements().unwrap().to_vec();
    items[0] = with(
        &items[0],
        "encrypted_content",
        Json::string("opaque:changed").unwrap(),
    );
    saved = with(&saved, "items", Json::array(items).unwrap());
    let different_response = provider::native::load(&saved).unwrap();
    assert_eq!(
        different_response.calls()[0].call_id,
        response.calls()[0].call_id
    );
    assert_eq!(
        different_response.calls()[0].arguments,
        response.calls()[0].arguments
    );
    assert!(continuation(&different_response, &results).is_err());
}

#[test]
fn saved_response_revalidates_through_synthetic_sse_decoder() {
    let response = decode("round1.sse").unwrap();
    let saved = provider::native::save(&response).unwrap();
    let restored = provider::native::load(&saved).unwrap();
    assert_eq!(restored.id(), response.id());
    assert_eq!(restored.returned_model(), response.returned_model());
    assert_eq!(
        restored
            .items()
            .iter()
            .map(|x| x.value().clone())
            .collect::<Vec<_>>(),
        response
            .items()
            .iter()
            .map(|x| x.value().clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(restored.calls(), response.calls());
    let mut invalid = saved.clone();
    invalid = with(
        &invalid,
        "model",
        Json::string("gpt-6.1-sol-spoof").unwrap(),
    );
    assert!(provider::native::load(&invalid).is_err());
    let mut invalid = saved;
    let mut items = invalid.field("items").unwrap().elements().unwrap().to_vec();
    items[0] = with(
        &items[0],
        "type",
        Json::string("unknown_native_type").unwrap(),
    );
    invalid = with(&invalid, "items", Json::array(items).unwrap());
    assert!(provider::native::load(&invalid).is_err());
}

#[test]
fn batch_validates_all_before_dispatch_and_consumes_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let executor: Executor = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Outcome::succeeded("ok".into())
    });
    let validator: Validator = Arc::new(|_| true);
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor: request().tools.descriptors[0].clone(),
            validator,
            executor,
        })
        .unwrap();
    let snapshot = registry.snapshot();
    let response = decode("round1.sse").unwrap();
    let mut ids = OperationIds::new("test_run", 0).unwrap();
    let batch = PreparedBatch::prepare(&response, Arc::clone(&snapshot), &mut ids).unwrap();
    let results = batch.execute(false);
    assert_eq!(results.results().len(), 1);
    assert_eq!(results.results()[0].operation_id(), "test_run:op:1");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let cancelled = PreparedBatch::prepare(&response, Arc::clone(&snapshot), &mut ids)
        .unwrap()
        .execute(true);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        continuation(&response, &cancelled)
            .unwrap()
            .json()
            .unwrap()
            .field("input")
            .unwrap()
            .elements()
            .unwrap()
            .last()
            .unwrap()
            .text_field("output")
            .unwrap(),
        r#"{"error":"cancelled_before_dispatch"}"#
    );
    let invalid = calls_with_second(r#"{"unexpected":true}"#);
    let rejected = decode_synthetic(
        request(),
        Head {
            status: 200,
            content_types: vec!["text/event-stream".into()],
        },
        &[&invalid],
    )
    .unwrap();
    assert!(PreparedBatch::prepare(&rejected, Arc::clone(&snapshot), &mut ids).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let good_two = decode_synthetic(
        request(),
        Head {
            status: 200,
            content_types: vec!["text/event-stream".into()],
        },
        &[&calls_with_second("{}")],
    )
    .unwrap();
    let two_results = PreparedBatch::prepare(&good_two, snapshot, &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(two_results.results()[0].operation_id(), "test_run:op:3");
    assert_eq!(two_results.results().len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn validator_panic_rejects_before_ids_or_executors() {
    let response = decode("round1.sse").unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&executions);
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor: response.request().tools.descriptors[0].clone(),
            validator: Arc::new(|_| panic!("synthetic validator panic")),
            executor: Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Outcome::succeeded("unexpected".into())
            }),
        })
        .unwrap();
    let mut ids = OperationIds::new("validator", 0).unwrap();
    assert_eq!(
        PreparedBatch::prepare(&response, registry.snapshot(), &mut ids).err(),
        Some("tool_validator_panicked")
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);

    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor: response.request().tools.descriptors[0].clone(),
            validator: Arc::new(|_| true),
            executor: Arc::new(|_| Outcome::succeeded("ok".into())),
        })
        .unwrap();
    let results = PreparedBatch::prepare(&response, registry.snapshot(), &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(results.results()[0].operation_id(), "validator:op:1");
}

#[test]
fn batch_keeps_known_failure_and_stops_after_unknown_effect() {
    let response = decode_synthetic(
        request(),
        Head {
            status: 200,
            content_types: vec!["text/event-stream".into()],
        },
        &[&calls_with_second("{}")],
    )
    .unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let n = Arc::clone(&counter);
    let descriptor = response.request().tools.descriptors[0].clone();
    let validator: Validator = Arc::new(|_| true);
    let executor: Executor = Arc::new(move |_| {
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            Outcome::failed_known("known failure".into())
        } else {
            Outcome::succeeded("later result".into())
        }
    });
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor: descriptor.clone(),
            validator: Arc::clone(&validator),
            executor,
        })
        .unwrap();
    let snapshot = registry.snapshot();
    let mut ids = OperationIds::new("known", 0).unwrap();
    let results = PreparedBatch::prepare(&response, Arc::clone(&snapshot), &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(results.results()[0].execution(), Execution::Failed);
    assert_eq!(results.results()[0].effects(), Effects::KnownResult);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert!(continuation(&response, &results).is_ok());
    let n = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&n);
    let executor: Executor = Arc::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        Outcome::failed_unknown()
    });
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor,
            validator,
            executor,
        })
        .unwrap();
    let snapshot = registry.snapshot();
    let mut ids = OperationIds::new("unknown", 0).unwrap();
    let results = PreparedBatch::prepare(&response, snapshot, &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert_eq!(results.results()[1].execution(), Execution::Cancelled);
    assert_eq!(results.results()[1].effects(), Effects::NotStarted);
    assert!(continuation(&response, &results).is_err());
}

#[test]
fn batch_cancel_after_first_executor_preserves_known_result_and_stops_tail() {
    let response = decode_synthetic(
        request(),
        Head {
            status: 200,
            content_types: vec!["text/event-stream".into()],
        },
        &[&calls_with_second("{}")],
    )
    .unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    let descriptor = response.request().tools.descriptors[0].clone();
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor,
            validator: Arc::new(|_| true),
            executor: Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                signal.store(true, Ordering::SeqCst);
                Outcome::succeeded("known".into())
            }),
        })
        .unwrap();
    let snapshot = registry.snapshot();
    let mut ids = OperationIds::new("cancel", 0).unwrap();
    let results = PreparedBatch::prepare(&response, snapshot, &mut ids)
        .unwrap()
        .execute_with_cancel(|| cancelled.load(Ordering::SeqCst));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(results.results()[0].output(), "known");
    assert_eq!(results.results()[1].effects(), Effects::NotStarted);
    assert!(continuation(&response, &results).is_ok());
}

#[test]
fn batch_rejects_tool_provenance_before_validation() {
    let response = decode("round1.sse").unwrap();
    let validations = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let v = Arc::clone(&validations);
    let c = Arc::clone(&calls);
    let mut descriptor = response.request().tools.descriptors[0].clone();
    descriptor.description = "different implementation snapshot".into();
    let validator: Validator = Arc::new(move |_| {
        v.fetch_add(1, Ordering::SeqCst);
        true
    });
    let executor: Executor = Arc::new(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        Outcome::succeeded("unexpected".into())
    });
    let mut registry = Registry::new();
    registry
        .register(Definition {
            descriptor,
            validator,
            executor,
        })
        .unwrap();
    let mut ids = OperationIds::new("provenance", 0).unwrap();
    assert!(PreparedBatch::prepare(&response, registry.snapshot(), &mut ids).is_err());
    assert_eq!(validations.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
