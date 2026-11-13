use ansa::{
    frontend::{
        app,
        args::Options,
        carbon::{self, Entry},
        controller::{history_page_current, preview_visible},
        input::{Action, Editor},
        jsonl,
        protocol_v1::*,
    },
    protocol::json::Json,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-m7-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn args(v: &[&str]) -> ansa::frontend::Result<Options> {
    Options::parse(
        &v.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
        "/fixture/cwd".into(),
        "/fixture/auth".into(),
    )
}
fn command(kind: &str, extra: &str) -> String {
    format!(
        r#"{{"schema_version":1,"session":"offline","turn":"turn","incarnation":"incarnation","command_id":"cmd","type":"{kind}",{extra}}}"#
    )
}
#[test]
fn argument_grammar_keeps_legacy_option_and_prompt_rules() {
    let value = args(&["--offline", "--model", "model", "--cwd", "relative", "task"]).unwrap();
    assert!(value.offline);
    assert_eq!(value.cwd, PathBuf::from("relative"));
    assert_eq!(value.prompt.as_deref(), Some("task"));
    for (input, expected) in [
        (vec!["--offline", "--offline"], "duplicate_cli_option"),
        (vec!["--model"], "cli_option_value"),
        (vec!["a", "b"], "cli_prompt_arguments"),
        (vec!["--unknown", "x"], "unknown_cli_option"),
        (vec!["/cancel"], "slash_command_requires_stdin"),
    ] {
        assert_eq!(args(&input).unwrap_err(), expected);
    }
    assert_eq!(
        args(&["--model", "--offline"]).unwrap().model.as_deref(),
        Some("--offline")
    );
}
#[test]
fn jsonl_decodes_all_payloads_without_rounding_cursors() {
    let value = jsonl::decode(&command(
        "snapshot",
        r#""after_sequence":18446744073709551615"#,
    ))
    .unwrap();
    assert_eq!(value.payload, CommandPayload::GetSnapshot(u64::MAX));
    let value = jsonl::decode(&command("submit", r#""text":"hello""#)).unwrap();
    assert_eq!(
        value.payload,
        CommandPayload::Submit(Submit {
            text: "hello".into(),
            mode: AgentMode::Basic
        })
    );
    for (kind, extra) in [
        ("cancel", r#""target":"request""#),
        ("resume", r#""target":"request""#),
        ("transcript", r#""before":5,"limit":4,"forward":true"#),
    ] {
        validate_command(&jsonl::decode(&command(kind, extra)).unwrap()).unwrap();
    }
}
#[test]
fn jsonl_rejects_invalid_types_fields_and_versions() {
    for input in [
        command("snapshot", r#""after_sequence":true"#),
        command("snapshot", r#""after_sequence":-1"#),
        command("submit", r#""text":"x","mode":"bad""#),
        command("transcript", r#""before":0,"limit":1,"forward":0"#),
        command("snapshot", r#""after_sequence":0,"extra":true"#),
        command("snapshot", r#""after_sequence":0"#)
            .replace("schema_version\":1", "schema_version\":2"),
    ] {
        assert!(jsonl::decode(&input).is_err(), "{input}");
    }
    assert_eq!(
        jsonl::diagnostic("submit_mode")
            .field("code")
            .unwrap()
            .index()
            .unwrap(),
        0
    );
    assert_eq!(
        jsonl::diagnostic("command_fields")
            .field("code")
            .unwrap()
            .index()
            .unwrap(),
        7
    );
}
#[test]
fn editor_preserves_utf8_boundaries_and_escape_following_text() {
    let mut editor = Editor::default();
    for &ch in "привет".as_bytes() {
        editor.feed(ch, true);
    }
    editor.feed(127, true);
    assert_eq!(editor.text(), Ok("приве"));
    for ch in b"\x1b[D!" {
        editor.feed(*ch, true);
    }
    assert_eq!(editor.text(), Ok("прив!е"));
    editor.clear();
    for &ch in b"\x1babc" {
        editor.feed(ch, true);
    }
    assert_eq!(editor.text(), Ok("abc"));
    assert_eq!(editor.feed(9, true), Some(Action::ToggleMode));
    assert_eq!(editor.feed(6, true), Some(Action::ToggleHistory));
}
#[test]
fn editor_overflow_cannot_be_healed_by_backspaces() {
    let mut editor = Editor::default();
    for _ in 0..16 * 1024 + 1 {
        editor.feed(b'x', true);
    }
    for _ in 0..16 * 1024 {
        editor.feed(127, true);
    }
    assert!(editor.overflow);
    assert_eq!(editor.feed(b'\r', true), Some(Action::Overflow));
    editor.clear();
    assert_eq!(editor.feed(b'\r', true), Some(Action::Submit));
}
fn entry(role: &str, text: &str) -> Entry {
    Entry {
        role: role.into(),
        text: text.into(),
    }
}
#[test]
fn carbon_sanitizes_controls_and_invalid_bytes_without_locale_dependency() {
    let frame = carbon::layout(
        &[
            entry("user", "**literal**\x1b[31m\u{9b}danger"),
            entry("assistant", "```cpp\ncode\n```\nassistant manager"),
        ],
        b"e\xcc\x81\xf0\x9f\x98\x80\xc0\xaf",
        7,
        "basic",
        false,
        0,
        "fixture",
        (80, 24),
        false,
    )
    .unwrap();
    let body = frame.lines.join("\n");
    assert!(!body.contains('\x1b'));
    assert!(body.contains("**literal**?"));
    assert!(!body.contains("```"));
    assert!(body.contains("assistant manager"));
    assert!(body.contains("??"));
    assert_eq!(frame.cursor, 5);
}
#[test]
fn carbon_keeps_role_markdown_and_clears_shrunk_rows() {
    let history = [
        entry("user", "**bold** `literal`"),
        entry(
            "assistant",
            "**alpha bravo charlie delta echo foxtrot** `code`\n# Heading",
        ),
    ];
    let frame = carbon::layout(
        &history,
        b"draft",
        5,
        "plan",
        true,
        0,
        "context",
        (25, 24),
        true,
    )
    .unwrap();
    assert!(frame.lines.iter().any(|v| v.contains("**bold**")));
    assert!(
        frame
            .lines
            .iter()
            .any(|v| v.contains("echo") && v.contains("\x1b[1m"))
    );
    let small = carbon::layout(
        &[],
        b"draft",
        5,
        "plan",
        false,
        0,
        "context",
        (100, 32),
        false,
    )
    .unwrap();
    assert!(small.paint(frame.lines.len()).contains("\x1b7\x1b[1B"));
    assert_eq!(small.cursor, 7);
    assert!(carbon::layout(&history, b"", 0, "basic", false, 0, "", (4, 3), false).is_none());
}
#[test]
fn history_queries_and_preview_cannot_publish_stale_or_tool_round_text() {
    let query = GetTranscript {
        before: 0,
        limit: 16,
        forward: false,
    };
    let entry = ansa::session::transcript::Entry {
        turn: "turn".into(),
        request: "request".into(),
        role: "user".into(),
        text: "question".into(),
        state: "pending".into(),
        mode: AgentMode::Basic,
    };
    let page = TranscriptPage {
        begin: 0,
        end: 1,
        total: 1,
        entries: vec![entry],
        query: query.clone(),
    };
    assert!(history_page_current(&page, &query, 1));
    assert!(!history_page_current(
        &page,
        &GetTranscript { before: 1, ..query },
        1
    ));
    assert!(!history_page_current(&page, &page.query, 2));
    let request = RequestId("request".into());
    let preview = Preview {
        generation: 1,
        text: "draft".into(),
        state: "committed".into(),
    };
    assert!(!preview_visible(
        &page.entries,
        1,
        1,
        Some(&request),
        Some(&preview),
        false
    ));
    assert!(preview_visible(
        &page.entries,
        1,
        1,
        Some(&request),
        Some(&preview),
        true
    ));
    assert!(!preview_visible(
        &page.entries,
        0,
        1,
        Some(&request),
        Some(&preview),
        true
    ));
}
#[test]
fn default_auth_path_preserves_owned_legacy_directories() {
    let root = Root::new();
    let home = root.0.join("home");
    std::fs::create_dir(&home).unwrap();
    assert_eq!(
        app::auth_path_with(Some(&home), None).unwrap(),
        home.join(".ansa/auth")
    );
    let old = home.join(".local/state/oh-my-harness");
    std::fs::create_dir_all(&old).unwrap();
    assert_eq!(app::auth_path_with(Some(&home), None).unwrap(), old);
    assert_eq!(
        app::auth_path_with(Some(&home), Some(std::ffi::OsStr::new(""))).unwrap(),
        old
    );
    let xdg = root.0.join("xdg");
    std::fs::create_dir_all(xdg.join("oh-my-harness")).unwrap();
    assert_eq!(
        app::auth_path_with(Some(&home), Some(xdg.as_os_str())).unwrap(),
        xdg.join("oh-my-harness")
    );
    assert_eq!(
        app::auth_path_with(None, Some(xdg.as_os_str())).unwrap_err(),
        "auth_home_directory"
    );
}
#[test]
fn ca_loader_checks_regular_type_size_and_public_symlink() {
    let root = Root::new();
    let path = root.0.join("ca.pem");
    std::fs::write(&path, b"certificate fixture").unwrap();
    assert_eq!(
        ansa::transport::client::read_ca(&path).unwrap(),
        "certificate fixture"
    );
    let link = root.0.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert_eq!(
        ansa::transport::client::read_ca(&link).unwrap(),
        "certificate fixture"
    );
    assert!(ansa::transport::client::read_ca(&root.0).is_err());
    std::fs::write(&path, vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    assert!(ansa::transport::client::read_ca(&path).is_err());
}
fn cli(root: &Root, args: &[&str]) -> std::process::Output {
    let home = root.0.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_ansa-rs"))
        .args(args)
        .arg("--cwd")
        .arg(&home)
        .env("HOME", &home)
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap()
}
#[test]
fn executable_runs_real_local_catalog_and_resume() {
    let root = Root::new();
    let first = cli(&root, &["--offline", "hello"]);
    assert!(first.status.success(), "{:?}", first);
    assert_eq!(first.stdout, b"OMH_TOOL_OK:OMH_FIXED_VALUE_42\n");
    let second = cli(&root, &["--offline", "--continue", "again"]);
    assert!(second.status.success(), "{:?}", second);
    assert_eq!(second.stdout, first.stdout);
    let sessions = root.0.join("home/.ansa/sessions");
    assert_eq!(std::fs::read_dir(sessions).unwrap().count(), 1);
}
#[test]
fn executable_rejects_non_submitting_one_shot_prompts() {
    let root = Root::new();
    for prompt in ["", "/plan", "/basic"] {
        let output = cli(&root, &["--offline", prompt]);
        assert_eq!(output.status.code(), Some(1), "{:?}", output);
        assert!(output.stdout.is_empty());
    }
}
#[test]
fn executable_invalid_utf8_argv_is_a_controlled_diagnostic() {
    use std::os::unix::ffi::OsStrExt;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ansa-rs"))
        .args([
            std::ffi::OsStr::new("--offline"),
            std::ffi::OsStr::from_bytes(b"\xff"),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value = Json::parse(std::str::from_utf8(&output.stderr).unwrap().trim()).unwrap();
    assert_eq!(value.text_field("diagnostic"), Ok("json_string_invalid"));
}
#[test]
fn large_valid_snapshot_is_encoded_with_complete_tool_results() {
    let output = "x".repeat(100 * 1024);
    let activities = (0..12)
        .map(|i| {
            Activity::Finished(Finished {
                operation: OperationId(format!("op{i}")),
                call_id: format!("call{i}"),
                execution: ansa::provider::replay::Execution::Succeeded,
                effects: ansa::provider::replay::Effects::KnownResult,
                output: std::sync::Arc::new(output.clone()),
            })
        })
        .collect();
    let snapshot = Snapshot {
        through_sequence: 1,
        state: "completed".into(),
        request: Some("request".into()),
        worker_busy: false,
        completed_tools: 12,
        text: "done".into(),
        diagnostic: String::new(),
        recovery_operation: None,
        submit: None,
        command: None,
        tools: activities,
        history_items: 0,
        compactions: 0,
        storage_failed: false,
        durable: true,
        resume_available: false,
        mode: AgentMode::Basic,
        transcript_size: 0,
        preview: None,
    };
    let event = RuntimeEvent {
        schema_version: 1,
        stream_sequence: 1,
        session: "session".into(),
        turn: "turn".into(),
        incarnation: "incarnation".into(),
        request: Some("request".into()),
        payload: EventPayload::FrontendSnapshot(Box::new(snapshot)),
    };
    let encoded = jsonl::encode(&event);
    assert!(encoded.dump().len() > 1024 * 1024);
    let tools = encoded.field("tools").unwrap().elements().unwrap();
    assert_eq!(tools.len(), 12);
    assert!(
        tools
            .iter()
            .all(|tool| tool.text_field("output") == Ok(output.as_str()))
    );
}
