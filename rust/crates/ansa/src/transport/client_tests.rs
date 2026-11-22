use super::*;
use crate::{
    frontend::app,
    provider::{
        request::{Epoch, Request as ModelRequest},
        responses::ResponseStream,
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        registry::{Policy, Registry},
    },
};
use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

#[allow(dead_code)] // The shared fixture also exposes entry points for M1/M7.
#[path = "../../examples/support/frame_server.rs"]
mod frame_server;

fn protocol_fixture(name: &str) -> std::path::PathBuf {
    std::env::var_os("ANSA_PROTOCOL_FIXTURES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures"))
        .join(name)
}
fn transport_fixtures() -> std::path::PathBuf {
    std::env::var_os("ANSA_PARITY_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/parity"))
        .join("fixtures")
}
fn registry() -> (Arc<crate::tools::registry::Snapshot>, Arc<AtomicUsize>) {
    let mut definition = app::pure_tools()
        .unwrap()
        .definition("get_test_value")
        .unwrap()
        .clone();
    let count = Arc::new(AtomicUsize::new(0));
    let executions = count.clone();
    definition.executor = Arc::new(move |_| {
        executions.fetch_add(1, Ordering::SeqCst);
        crate::tools::registry::Outcome::succeeded("{}".into())
    });
    let mut registry = Registry::new();
    registry
        .register_with_policy(
            definition,
            Policy {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    (registry.snapshot(), count)
}
fn h1_peer(listener: TcpListener, case: &str, body: &[u8]) {
    let (mut socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let end = loop {
        let n = socket.read(&mut buffer).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(i) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break i + 4;
        }
        assert!(bytes.len() < 65536);
    };
    let head = std::str::from_utf8(&bytes[..end]).unwrap();
    let size = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + size {
        let n = socket.read(&mut buffer).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
    }
    let mime = match case {
        "missing" | "missing-stall" => "",
        "duplicate" => "Content-Type: text/event-stream\r\nContent-Type: text/event-stream\r\n",
        "wrong" => "Content-Type: application/json\r\n",
        _ => "Content-Type: text/event-stream\r\n",
    };
    let status = if case == "unauthorized" { 401 } else { 200 };
    if case == "missing-stall" {
        socket
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(600));
        return;
    }
    if case.starts_with("delayed-complete") {
        thread::sleep(Duration::from_millis(300));
    }
    let wire = if case == "trailer" {
        let mut v = format!(
            "HTTP/1.1 200 OK\r\n{mime}Transfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v.extend_from_slice(b"\r\n0\r\nContent-Type: text/event-stream\r\n\r\n");
        v
    } else {
        let mut v = format!(
            "HTTP/1.1 {status} OK\r\n{mime}Content-Length: {}\r\n\r\n",
            body.len()
                + if matches!(case, "truncated" | "unbounded-cancel") {
                    20
                } else {
                    0
                }
        )
        .into_bytes();
        v.extend_from_slice(body);
        v
    };
    let _ = socket.write_all(&wire);
    if case == "unbounded-cancel" {
        // Withhold HTTP EOF after the complete SSE response. Only cancellation
        // should close the exchange without dispatching its tool call.
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
    }
    let _ = socket.shutdown(Shutdown::Both);
}
fn exercise(case: &str, h2: bool, siwc: bool) -> (Result<()>, usize, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = std::fs::read(protocol_fixture("round1.sse")).unwrap();
    let body_length = body.len();
    let server = if h2 {
        listener.set_nonblocking(true).unwrap();
        let fixture = transport_fixtures();
        let case = case.to_owned();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = tokio::time::timeout(
                    Duration::from_secs(4),
                    frame_server::serve_body(listener, &fixture, &case, Bytes::from(body)),
                )
                .await;
            });
        })
    } else {
        let case = case.to_owned();
        thread::spawn(move || h1_peer(listener, &case, &body))
    };
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            std::fs::read(transport_fixtures().join("transport/tls/ca.der")).unwrap(),
        ))
        .unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let (tools, count) = registry();
    let model = ModelRequest::new(
        app::binding(),
        Epoch {
            id: "test".into(),
            instructions: "test".into(),
            exact_state: Json::parse("{}").unwrap(),
        },
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let value = model.json().unwrap();
    let mut request = responses_request(&value, &Secret::new("synthetic-only".into()));
    match case {
        "delayed-complete" | "unbounded-cancel" => assert!(request.timeout_ms.is_none()),
        "delayed-complete-bounded" | "missing-stall" | "h2-missing-timeout" => {
            request.timeout_ms = Some(200);
        }
        _ => request.timeout_ms = Some(2000),
    }
    let mut decoder = ResponseStream::new_with_callbacks(
        model,
        crate::protocol::sse::Limits::default(),
        None,
        None,
    );
    let mut received = 0;
    let stop = Cancel::default();
    let result = perform_route(
        &request,
        &stop,
        Some(&mut |bytes| {
            received += bytes.len();
            decoder.feed(bytes)?;
            if case == "unbounded-cancel" && received == body_length {
                stop.cancel();
            }
            Ok(())
        }),
        format!("{}://localhost:{port}/", if h2 { "https" } else { "http" })
            .parse()
            .unwrap(),
        tls,
        None,
        RoutePolicy {
            local_only: true,
            admission: if siwc {
                StreamAdmission::SiwcResponses(&value)
            } else {
                StreamAdmission::Strict
            },
            mcp: None,
        },
    )
    .and_then(|_| decoder.finish())
    .and_then(|response| {
        let batch = PreparedBatch::prepare(
            &response,
            tools,
            &mut OperationIds::new("probe", 0).unwrap(),
        )?;
        let _ = batch.execute(false);
        Ok(())
    });
    server.join().unwrap();
    (result, received, count.load(Ordering::SeqCst))
}
#[test]
fn siwc_missing_mime_admits_complete_http1_response() {
    let (result, received, executors) = exercise("missing", false, true);
    assert_eq!(result, Ok(()));
    assert!(received > 0);
    assert_eq!(executors, 1);
}
#[test]
fn siwc_missing_mime_admits_complete_http2_response() {
    let (result, received, executors) = exercise("h2-missing-mime", true, true);
    assert_eq!(result, Ok(()));
    assert!(received > 0);
    assert_eq!(executors, 1);
}
#[test]
fn unbounded_siwc_waits_for_eof_and_still_cancels() {
    let (result, _, executors) = exercise("delayed-complete-bounded", false, true);
    assert_eq!(result, Err("http_timeout"));
    assert_eq!(executors, 0);

    let (result, received, executors) = exercise("delayed-complete", false, true);
    assert_eq!(result, Ok(()));
    assert!(received > 0);
    assert_eq!(executors, 1);

    let (result, received, executors) = exercise("unbounded-cancel", false, true);
    assert_eq!(result, Err("auth_cancelled"));
    assert!(received > 0);
    assert_eq!(executors, 0);
}
#[test]
fn strict_headers_and_failed_eof_never_dispatch() {
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(rustls::RootCertStore::empty())
    .with_no_client_auth();
    for (url, method, value, expected) in [
        (
            super::super::http::RESPONSES_ENDPOINT,
            "POST",
            r#"{"stream":true,"store":true}"#,
            "http_request_profile",
        ),
        (
            super::super::http::RESPONSES_ENDPOINT,
            "POST",
            r#"{"stream":false,"store":false}"#,
            "http_request_profile",
        ),
        (
            super::super::http::RESPONSES_ENDPOINT,
            "GET",
            r#"{"stream":true,"store":false}"#,
            "http_endpoint",
        ),
        (
            "https://api.openai.com/v1/other",
            "POST",
            r#"{"stream":true,"store":false}"#,
            "http_endpoint",
        ),
    ] {
        let value = Json::parse(value).unwrap();
        let request = HttpRequest {
            method,
            url,
            content_type: "application/json",
            body: Secret::new(value.dump()),
            bearer: None,
            body_limit: 1024,
            timeout_ms: Some(1),
        };
        let mut called = false;
        let result = perform_route(
            &request,
            &Cancel::default(),
            Some(&mut |_| {
                called = true;
                Ok(())
            }),
            "http://127.0.0.1:1/".parse().unwrap(),
            tls.clone(),
            None,
            RoutePolicy {
                local_only: true,
                admission: StreamAdmission::SiwcResponses(&value),
                mcp: None,
            },
        );
        assert_eq!(result.err(), Some(expected));
        assert!(!called, "invalid profile reached body delivery");
    }
    for (h2, cases) in [
        (
            false,
            vec![
                "missing",
                "duplicate",
                "wrong",
                "trailer",
                "truncated",
                "unauthorized",
                "missing-stall",
            ],
        ),
        (
            true,
            vec![
                "h2-missing-mime",
                "h2-duplicate-mime",
                "h2-wrong-mime",
                "h2-missing-mime-trailer",
                "h2-reset",
                "h2-missing-timeout",
            ],
        ),
    ] {
        for case in cases {
            for siwc in [false, true] {
                if siwc && matches!(case, "missing" | "h2-missing-mime") {
                    continue;
                }
                let (result, received, executors) = exercise(case, h2, siwc);
                assert!(result.is_err(), "{case} siwc={siwc}");
                if siwc && matches!(case, "missing-stall" | "h2-missing-timeout") {
                    assert_eq!(result, Err("http_timeout"));
                    if h2 {
                        assert_eq!(
                            received,
                            std::fs::read(protocol_fixture("round1.sse")).unwrap().len(),
                            "timeout must follow the complete terminal body"
                        );
                    }
                }
                assert_eq!(executors, 0, "{case}");
            }
        }
    }
    for (h2, case) in [(false, "valid"), (true, "h2-valid")] {
        let (result, _, executors) = exercise(case, h2, false);
        assert_eq!(result, Ok(()));
        assert_eq!(executors, 1);
    }
}
