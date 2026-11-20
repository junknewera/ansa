//! JSONL adapter used only by the offline C++/Rust protocol corpus.
use ansa::{
    protocol::{json::Json, sse},
    provider::{
        replay::{Effects, Execution, continuation},
        request::{Advertisement, Binding, Descriptor, Epoch, Request},
        responses::ResponseStream,
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        registry::{Definition, Executor, Outcome, Registry, Validator},
    },
    transport::frame_probe::{Probe, ProtocolProbe, fetch_protocol},
    transport::http::{Head, admit_request, decode_fixture_with_limits},
};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::object_with_limits(fields, 20 * 1024 * 1024).expect("bounded adapter object")
}
fn hex_bytes(hex: &str) -> Result<Vec<u8>, &'static str> {
    if !hex.len().is_multiple_of(2) {
        return Err("invalid_hex");
    }
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |c: u8| match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err("invalid_hex"),
            };
            Ok((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}
fn row_limits(row: &Json) -> sse::Limits {
    let mut limits = sse::Limits::default();
    if let Ok(value) = row.field("limits") {
        let set = |name: &str, current: usize| value.usize_field(name).unwrap_or(current);
        limits.body = set("body", limits.body);
        limits.event = set("event", limits.event);
        limits.line = set("line", limits.line);
        limits.events = set("events", limits.events);
        limits.arguments = set("arguments", limits.arguments);
        limits.items = set("items", limits.items);
        limits.calls = set("calls", limits.calls);
        limits.json_depth = set("json_depth", limits.json_depth);
        limits.json_nodes = set("json_nodes", limits.json_nodes);
    }
    limits
}
fn row_mime(row: &Json) -> Vec<String> {
    match row.field("mime") {
        Ok(value) if value.is_array() => value
            .elements()
            .unwrap_or(&[])
            .iter()
            .filter_map(|v| v.text().ok().map(str::to_owned))
            .collect(),
        Ok(value) => value.text().map(str::to_owned).into_iter().collect(),
        _ => vec![],
    }
}
fn json_probe(row: &Json) -> Json {
    let parsed = (|| -> Result<Json, &'static str> {
        let bytes = if row.has("payload_hex") {
            let hex = row.text_field("payload_hex")?;
            if hex.len() % 2 != 0 {
                return Err("json_invalid");
            }
            let mut bytes = Vec::with_capacity(hex.len() / 2);
            for pair in hex.as_bytes().as_chunks::<2>().0 {
                let digit = |c: u8| -> Result<u8, &'static str> {
                    match c {
                        b'0'..=b'9' => Ok(c - b'0'),
                        b'a'..=b'f' => Ok(c - b'a' + 10),
                        b'A'..=b'F' => Ok(c - b'A' + 10),
                        _ => Err("json_invalid"),
                    }
                };
                bytes.push((digit(pair[0])? << 4) | digit(pair[1])?);
            }
            bytes
        } else {
            row.text_field("payload")?.as_bytes().to_vec()
        };
        let source = std::str::from_utf8(&bytes).map_err(|_| "json_invalid")?;
        let (max_bytes, depth, nodes) = if row.has("limits") {
            let limits = row.field("limits")?;
            (
                limits.usize_field("bytes").unwrap_or(16 * 1024 * 1024),
                limits.usize_field("depth").unwrap_or(64),
                limits.usize_field("nodes").unwrap_or(131072),
            )
        } else {
            (16 * 1024 * 1024, 64, 131072)
        };
        Json::parse_with_limits(source, max_bytes, depth, nodes)
    })();
    let Ok(value) = parsed else {
        return obj(vec![
            ("accepted", Json::parse("false").unwrap()),
            (
                "error",
                obj(vec![
                    ("schema_version", Json::parse("1").unwrap()),
                    ("code", Json::parse("0").unwrap()),
                    ("stage", Json::string("json").unwrap()),
                    ("diagnostic", Json::string("json_invalid").unwrap()),
                ]),
            ),
        ]);
    };
    let mut out = vec![
        ("accepted", Json::parse("true").unwrap()),
        (
            "dump",
            Json::string_with_limits(&value.dump(), 20 * 1024 * 1024).unwrap(),
        ),
        (
            "sha256",
            Json::string(&ansa::protocol::legacy_json_v1::sha256(
                value.dump().as_bytes(),
            ))
            .unwrap(),
        ),
    ];
    if row.has("other") {
        let equal = row
            .text_field("other")
            .ok()
            .and_then(|s| Json::parse(s).ok())
            .is_some_and(|other| other == value);
        out.push((
            "equal",
            Json::parse(if equal { "true" } else { "false" }).unwrap(),
        ));
    }
    if row.has("index_key") {
        let key = row.text_field("index_key").unwrap_or("");
        if let Ok(index) = value.field(key).and_then(Json::index) {
            out.push(("index", Json::parse(&index.to_string()).unwrap()));
        } else {
            return obj(vec![
                ("accepted", Json::parse("false").unwrap()),
                (
                    "error",
                    obj(vec![
                        ("schema_version", Json::parse("1").unwrap()),
                        ("code", Json::parse("0").unwrap()),
                        ("stage", Json::string("json").unwrap()),
                        ("diagnostic", Json::string("json_index").unwrap()),
                    ]),
                ),
            ]);
        }
    }
    if row.has("lifetime_probe") {
        let child = (|| {
            let source = value.dump();
            let parsed = Json::parse(&source)?;
            Ok::<Json, &'static str>(parsed.field("a")?.clone())
        })();
        if let Ok(child) = child
            && let Ok(text) = child
                .elements()
                .and_then(|v| v.first().ok_or("json_missing_field"))
                .and_then(|v| v.text_field("text"))
        {
            out.push(("lifetime_text", Json::string(text).unwrap()));
        }
    }
    obj(out)
}
fn process(row: Json, counter: Arc<AtomicUsize>) -> Json {
    if row.text_field("op").ok() == Some("json") {
        return json_probe(&row);
    }
    let schema = Json::parse(
        r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#,
    )
    .unwrap();
    let descriptor = Descriptor {
        name: "get_test_value".into(),
        description: "Return a fixed test JSON value without side effects".into(),
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
    let revision = ansa::protocol::legacy_json_v1::sha256(
        Json::array(vec![record]).unwrap().dump().as_bytes(),
    );
    let request = Request::new(
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
            descriptors: vec![descriptor.clone()],
        },
        vec![
            Json::parse(r#"{"role":"developer","content":"fixture"}"#).unwrap(),
            Json::parse(r#"{"role":"user","content":"Call omh.get_test_value"}"#).unwrap(),
        ],
    )
    .unwrap();
    if row.text_field("op").ok() == Some("admit_request") {
        let mut profile = request.json().unwrap();
        if let Ok(stream) = row.field("stream").and_then(Json::boolean) {
            profile = profile
                .with(
                    "stream",
                    Json::parse(if stream { "true" } else { "false" }).unwrap(),
                )
                .unwrap();
        }
        if let Ok(store) = row.field("store").and_then(Json::boolean) {
            profile = profile
                .with(
                    "store",
                    Json::parse(if store { "true" } else { "false" }).unwrap(),
                )
                .unwrap();
        }
        let status = row.field("status").and_then(Json::index).unwrap_or(0) as u16;
        let head = Head {
            status,
            content_types: row_mime(&row),
        };
        let url = row.text_field("url").unwrap_or("");
        let method = row.text_field("method").unwrap_or("POST");
        return obj(vec![(
            "accepted",
            Json::parse(
                if admit_request(&head, url, method, &profile, false).is_ok() {
                    "true"
                } else {
                    "false"
                },
            )
            .unwrap(),
        )]);
    }
    let batch_config = row.field("batch").ok();
    let behaviors = batch_config
        .and_then(|b| b.field("executor_behaviors").ok())
        .and_then(|v| v.elements().ok())
        .map(|v| {
            v.iter()
                .filter_map(|x| x.text().ok().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let executor_index = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&counter);
    let executor_index_for_call = Arc::clone(&executor_index);
    let executor: Executor = Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        let index = executor_index_for_call.fetch_add(1, Ordering::SeqCst);
        match behaviors.get(index).map(String::as_str).unwrap_or("ok") {
            "throw" => panic!("synthetic executor failure"),
            "oversized" => Outcome::succeeded("x".repeat(128 * 1024 + 1)),
            "cancelled_unknown" => Outcome::failed_unknown(),
            "invalid_utf8" => {
                Outcome::from_bytes(Execution::Succeeded, Effects::KnownResult, vec![0xff, 0xfe])
            }
            "inconsistent" => {
                Outcome::from_bytes(Execution::Succeeded, Effects::Unknown, b"{}".to_vec())
            }
            _ => Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into()),
        }
    });
    let validation_index = Arc::new(AtomicUsize::new(0));
    let invalid_call = batch_config.and_then(|b| b.usize_field("invalid_call_index").ok());
    let validation_index_for_call = Arc::clone(&validation_index);
    let validator: Validator = Arc::new(move |_| {
        let index = validation_index_for_call.fetch_add(1, Ordering::SeqCst);
        invalid_call != Some(index)
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
    if row.has("network_url") {
        let url = row.field("network_url").unwrap().text().unwrap().to_owned();
        let spec = ProtocolProbe {
            network: Probe {
                url,
                ca_der: None,
                proxy_env: BTreeMap::new(),
                body_limit: 16 * 1024 * 1024,
                h2_only: false,
            },
            request,
            snapshot,
            invocations: counter,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("single-threaded local adapter runtime");
        let output = runtime.block_on(fetch_protocol(spec));
        let mut output = Json::parse(&output.to_string()).expect("bounded transport output");
        for (raw, field) in [
            ("items_raw", "items"),
            ("continuation_raw", "continuation"),
            ("context_raw", "context_provenance"),
        ] {
            if !output.has(raw) {
                continue;
            }
            let value = if raw == "items_raw" {
                Json::array(
                    output
                        .field(raw)
                        .unwrap()
                        .elements()
                        .unwrap()
                        .iter()
                        .map(|item| Json::parse(item.text().unwrap()).unwrap())
                        .collect(),
                )
                .unwrap()
            } else {
                Json::parse(output.text_field(raw).unwrap()).unwrap()
            };
            output = output.without(raw).unwrap().with(field, value).unwrap();
        }
        // Frame observations stay in peer evidence. Both language adapters
        // return the same protocol-level response envelope.
        return output
            .without("interim")
            .unwrap()
            .without("trailers")
            .unwrap();
    }
    let body = if let Ok(hex) = row.text_field("body_hex") {
        hex_bytes(hex).unwrap_or_default()
    } else {
        row.field("body")
            .and_then(Json::text)
            .unwrap_or("")
            .as_bytes()
            .to_vec()
    };
    let chunks: Vec<Vec<u8>> = if let Ok(hexes) = row.field("chunks_hex").and_then(Json::elements) {
        hexes
            .iter()
            .filter_map(|hex| hex.text().ok())
            .map(|hex| hex_bytes(hex).unwrap_or_default())
            .collect()
    } else if let Ok(parts) = row.field("chunks").and_then(Json::elements) {
        parts
            .iter()
            .filter_map(|part| part.text().ok().map(|x| x.as_bytes().to_vec()))
            .collect()
    } else {
        vec![body]
    };
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    let status = row.field("status").unwrap().index().unwrap_or(0) as u16;
    let head = Head {
        status,
        content_types: row_mime(&row),
    };
    let preview_texts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let cancel_preview = row
        .field("cancel_preview_after_first")
        .and_then(Json::boolean)
        .unwrap_or(false);
    let preview_enabled = row
        .field("preview")
        .and_then(Json::boolean)
        .unwrap_or(false);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let response = if preview_enabled {
        let seen = Arc::clone(&preview_texts);
        let cancel = Arc::clone(&cancelled);
        let callback: Box<dyn FnMut(&str)> = Box::new(move |text| {
            seen.lock().expect("preview lock").push(text.to_owned());
            if cancel_preview {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        let check = Arc::clone(&cancelled);
        let mut stream = ResponseStream::new_for_adapter(
            request,
            row_limits(&row),
            Some(callback),
            Some(Box::new(move || check.load(Ordering::SeqCst))),
        );
        let mut result = Ok(());
        if head.status != 200
            || head.content_types.len() != 1
            || !head.content_types[0].eq_ignore_ascii_case("text/event-stream")
        {
            result = Err("http_head_rejected");
        }
        if result.is_ok() {
            for chunk in &chunk_refs {
                if let Err(error) = stream.feed_for_adapter(chunk) {
                    result = Err(error);
                    break;
                }
            }
        }
        if let Err(error) = result {
            Err(error)
        } else {
            stream.finish_for_adapter()
        }
    } else {
        decode_fixture_with_limits(request, head, &chunk_refs, row_limits(&row))
    };
    let Ok(response) = response else {
        let mut fields = vec![
            ("accepted", Json::parse("false").unwrap()),
            ("executors", Json::parse("0").unwrap()),
        ];
        if preview_enabled {
            fields.push((
                "preview_texts",
                Json::array(
                    preview_texts
                        .lock()
                        .expect("preview lock")
                        .iter()
                        .map(|s| Json::string(s).unwrap())
                        .collect(),
                )
                .unwrap(),
            ));
        }
        return obj(fields);
    };
    let items = response
        .items()
        .iter()
        .map(|item| item.value().clone())
        .collect();
    let calls = response
        .calls()
        .iter()
        .map(|call| {
            obj(vec![
                ("id", Json::string(&call.item_id).unwrap()),
                ("call_id", Json::string(&call.call_id).unwrap()),
                ("name", Json::string(&call.name).unwrap()),
                ("arguments", Json::string(&call.arguments).unwrap()),
            ])
        })
        .collect();
    let mut ids = OperationIds::new("parity", 0).unwrap();
    let batch = PreparedBatch::prepare(&response, snapshot, &mut ids);
    let Ok(batch) = batch else {
        return obj(vec![
            ("accepted", Json::parse("true").unwrap()),
            ("batch_accepted", Json::parse("false").unwrap()),
            ("items", Json::array(items).unwrap()),
            ("calls", Json::array(calls).unwrap()),
            ("executors", Json::parse("0").unwrap()),
        ]);
    };
    let cancel_before = batch_config
        .and_then(|b| b.field("cancel_before_dispatch").ok())
        .and_then(|v| v.boolean().ok())
        .unwrap_or(false);
    let results = batch.execute(cancel_before);
    let unknown = results
        .results()
        .iter()
        .any(|result| result.effects() == Effects::Unknown);
    let bound_request = response.request();
    let provenance = obj(vec![
        (
            "provider",
            Json::string(&bound_request.binding.provider).unwrap(),
        ),
        (
            "account",
            Json::string(&bound_request.binding.account).unwrap(),
        ),
        (
            "requested_model",
            Json::string(&bound_request.binding.requested_model).unwrap(),
        ),
        (
            "returned_model",
            Json::string(&bound_request.binding.returned_model).unwrap(),
        ),
        (
            "epoch",
            obj(vec![
                ("id", Json::string(&bound_request.epoch.id).unwrap()),
                (
                    "instructions",
                    Json::string(&bound_request.epoch.instructions).unwrap(),
                ),
                ("exact_state", bound_request.epoch.exact_state.clone()),
            ]),
        ),
        (
            "tools",
            obj(vec![
                (
                    "revision",
                    Json::string(&bound_request.tools.revision).unwrap(),
                ),
                (
                    "descriptors",
                    Json::array(
                        bound_request
                            .tools
                            .descriptors
                            .iter()
                            .map(|d| {
                                Json::object(vec![
                                    ("name", Json::string(&d.name).unwrap()),
                                    ("description", Json::string(&d.description).unwrap()),
                                    ("revision", Json::string(&d.revision).unwrap()),
                                    ("schema", d.schema.clone()),
                                ])
                                .unwrap()
                            })
                            .collect(),
                    )
                    .unwrap(),
                ),
            ]),
        ),
    ]);
    let mut output = vec![
        ("accepted", Json::parse("true").unwrap()),
        ("batch_accepted", Json::parse("true").unwrap()),
        ("items", Json::array(items).unwrap()),
        ("calls", Json::array(calls).unwrap()),
        ("context_provenance", provenance),
        (
            "execution_failed",
            Json::parse(if unknown { "true" } else { "false" }).unwrap(),
        ),
        (
            "executors",
            Json::parse(&counter.load(Ordering::SeqCst).to_string()).unwrap(),
        ),
    ];
    if !unknown {
        output.push((
            "continuation",
            continuation(&response, &results)
                .expect("known synthetic outcomes")
                .json()
                .unwrap(),
        ));
    }
    if preview_enabled {
        output.push((
            "preview_texts",
            Json::array(
                preview_texts
                    .lock()
                    .expect("preview lock")
                    .iter()
                    .map(|s| Json::string(s).unwrap())
                    .collect(),
            )
            .unwrap(),
        ));
    }
    obj(output)
}
fn bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(false)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated JSONL record",
                ))
            };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let take = end.unwrap_or(available.len());
        if line.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSONL record exceeds limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(true);
        }
    }
}
fn main() {
    const MAX_LINE: usize = 20 * 1024 * 1024;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut bytes = Vec::new();
    loop {
        match bounded_line(&mut input, &mut bytes, MAX_LINE) {
            Ok(true) => {
                let counter = Arc::new(AtomicUsize::new(0));
                let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    process(
                        Json::parse_with_limits(
                            std::str::from_utf8(&bytes).unwrap(),
                            MAX_LINE,
                            64,
                            131072,
                        )
                        .unwrap(),
                        counter,
                    )
                }));
                match output {
                    Ok(value) => println!("{}", value.dump()),
                    Err(_) => println!("{{\"adapter_error\":true}}"),
                }
                bytes.clear();
            }
            Ok(false) => break,
            Err(_) => {
                eprintln!("protocol_adapter: invalid or oversized JSONL input");
                std::process::exit(2);
            }
        }
    }
}
