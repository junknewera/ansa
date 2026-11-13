use ansa::protocol::json::Json;
#[path = "../examples/support/frame_server.rs"]
mod frame_server;

#[test]
fn numeric_tokens_and_decoded_keys_are_not_normalized() {
    let text = r#"[1,1.0,1e0,-0,99999999999999999999999999999999999999999999]"#;
    assert_eq!(Json::parse(text).unwrap().dump(), text);
    for text in [
        r#"{"a":1,"\u0061":2}"#,
        "1e999",
        "1e-999",
        "[1,]",
        "\"\\ud800\"",
    ] {
        assert!(Json::parse(text).is_err(), "{text}");
    }
    assert!(Json::parse(&format!("{}0{}", "[".repeat(64), "]".repeat(64))).is_err());
    assert_eq!(
        Json::parse(r#"{"\u000b":"\u001f"}"#).unwrap().dump(),
        r#"{"\u000B":"\u001F"}"#
    );
    assert_eq!(
        Json::parse(r#"{"a":1,"b":2}"#).unwrap(),
        Json::parse(r#"{"b":2,"a":1}"#).unwrap()
    );
    assert_ne!(Json::parse("1").unwrap(), Json::parse("1.0").unwrap());
    assert!(
        Json::object(vec![
            ("a", Json::parse("1").unwrap()),
            ("a", Json::parse("2").unwrap())
        ])
        .is_err()
    );
}

#[test]
fn isolated_supervisor_risks() {
    let root = std::env::var_os("ANSA_PARITY_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/parity")
        });
    let output = std::process::Command::new("python3")
        .arg(root.join("risk_process.py"))
        .arg(env!("CARGO_BIN_EXE_ansa-rs"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["cases"].as_array().unwrap().len(), 11);
}

#[test]
fn tls_h2_frames_and_failures() {
    let root = std::env::var_os("ANSA_PARITY_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/parity")
        });
    let fixtures = root.join("fixtures");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (case, accepted) in [
            ("h2-valid", true),
            ("h2-trailer", true),
            ("h2-mime-trailer", false),
            ("h2-reset", false),
            ("h2-duplicate-mime", false),
            ("h2-large-headers", false),
            ("h2-invalid-ca", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let f = fixtures.clone();
            let server = tokio::spawn(async move {
                let _ = frame_server::serve(listener, &f, case).await;
            });
            let response =
                ansa::transport::frame_probe::fetch(ansa::transport::frame_probe::Probe {
                    url: format!("https://localhost:{port}/"),
                    ca_der: if case == "h2-invalid-ca" {
                        None
                    } else {
                        Some(std::fs::read(fixtures.join("transport/tls/ca.der")).unwrap())
                    },
                    proxy_env: Default::default(),
                    body_limit: 16 * 1024 * 1024,
                    h2_only: false,
                })
                .await;
            assert_eq!(response["accepted"], accepted, "{case}: {response}");
            assert_eq!(response["executors"], 0);
            server.abort();
            let _ = server.await;
        }
    });
}
