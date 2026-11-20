use ansa::{
    auth::Cancel,
    config::{AgentSettings, agent_epoch, load_agent_settings},
    frontend::workspace::{DiffStatus, EditorCommand, FileHost, console_generate},
    protocol::json::Json,
    provider::request::{Binding, Request},
    runtime::{
        AgentMode, CommandPayload, EventPayload, Limits, Options, Runtime, RuntimeCommand, Submit,
    },
    tools::files::local_tools,
};
use std::{
    os::unix::fs::{DirBuilderExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-workspace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::create_dir_all(path.join("home/project")).unwrap();
        Self(path)
    }
    fn cwd(&self) -> PathBuf {
        self.0.join("home/project")
    }
    fn settings(&self) -> AgentSettings {
        load_agent_settings(&self.cwd(), &self.0.join("home")).unwrap()
    }
    fn host(&self) -> FileHost {
        FileHost::new(&self.settings(), &self.0.join("auth")).unwrap()
    }
    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=ANSA Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .current_dir(self.cwd())
            .output()
            .unwrap();
        assert!(status.status.success(), "fixture git command failed");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn s(text: &str) -> Json {
    Json::string(text).unwrap()
}

#[test]
fn file_edit_waits_for_durable_intent_and_ambiguous_restart_never_replays_it() {
    use ansa::{
        provider::{
            native,
            replay::{Effects, Execution},
        },
        session::journal_v1::{Journal, Point},
    };
    use std::sync::{Arc, atomic::AtomicBool};
    for lose_ack in [false, true] {
        let fixture = Fixture::new();
        let settings = fixture.settings();
        let epoch = agent_epoch(&settings).unwrap();
        let token = ansa::protocol::legacy_json_v1::sha256(
            epoch
                .exact_state
                .field("instructions")
                .unwrap()
                .dump()
                .as_bytes(),
        );
        let file = fixture.cwd().join("code.rs");
        std::fs::write(&file, "let value = 1;\n").unwrap();
        let directory = fixture.0.join("edit-session");
        let tools = local_tools(
            settings,
            epoch.clone(),
            &directory,
            &fixture.0.join("auth"),
            Path::new(env!("CARGO_BIN_EXE_ansa")),
        )
        .unwrap();
        let configuration = Request::new(
            Binding {
                provider: "openai-siwc".into(),
                account: "fixture".into(),
                requested_model: "fixture".into(),
                returned_model: "fixture".into(),
            },
            epoch,
            tools.advertisement().clone(),
            vec![],
        )
        .unwrap();
        let target = file.clone();
        let intent_seen = Arc::new(AtomicBool::new(false));
        let seen = intent_seen.clone();
        let observer = Arc::new(move |point: Point, kind: &str| {
            if point == Point::AfterSync && kind == "intent" {
                assert_eq!(
                    std::fs::read_to_string(&target).unwrap(),
                    "let value = 1;\n"
                );
                seen.store(true, Ordering::SeqCst);
                if lose_ack {
                    return Err("fixture_edit_intent_ack_loss");
                }
            }
            Ok(())
        }) as ansa::session::journal_v1::Observer;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let generate = Arc::new(
            move |request: &Request, _: &Cancel, _: ansa::runtime::PreviewSink| {
                count.fetch_add(1, Ordering::SeqCst);
                let done = request
                    .input
                    .last()
                    .is_some_and(|item| item.text_field("type") == Ok("function_call_output"));
                let item = if done {
                    Json::object(vec![
                        ("type", s("message")),
                        ("id", s("edit_message")),
                        ("role", s("assistant")),
                        ("status", s("completed")),
                        (
                            "content",
                            Json::array(vec![
                                Json::object(vec![
                                    ("type", s("output_text")),
                                    ("text", s("EDIT_DONE")),
                                ])
                                .unwrap(),
                            ])
                            .unwrap(),
                        ),
                    ])
                    .unwrap()
                } else {
                    let arguments = Json::object(vec![
                        ("path", s("code.rs")),
                        ("old_text", s("let value = 1;")),
                        ("new_text", s("let value = 2;")),
                        ("instruction_token", s(&token)),
                    ])
                    .unwrap();
                    Json::object(vec![
                        ("type", s("function_call")),
                        ("id", s("edit_item")),
                        ("call_id", s("edit_call")),
                        ("namespace", s("omh")),
                        ("name", s("edit_file")),
                        ("arguments", s(&arguments.dump())),
                        ("status", s("completed")),
                    ])
                    .unwrap()
                };
                native::load(
                    &Json::object(vec![
                        ("request", request.save()?),
                        (
                            "id",
                            s(if done {
                                "edit_response2"
                            } else {
                                "edit_response1"
                            }),
                        ),
                        ("model", s(&request.binding.returned_model)),
                        ("items", Json::array(vec![item])?),
                    ])
                    .unwrap(),
                )
            },
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut runtime = Runtime::new(
                    "edit".into(),
                    configuration.clone(),
                    tools.clone(),
                    generate.clone(),
                    Limits::default(),
                    Options {
                        directory: Some(directory.clone()),
                        journal: Some(observer),
                        ..Options::default()
                    },
                )
                .unwrap();
                let handle = runtime.handle();
                handle.ready().await.unwrap();
                let command = RuntimeCommand {
                    schema_version: 1,
                    session: handle.session().clone(),
                    incarnation: handle.incarnation().clone(),
                    turn: "turn".into(),
                    command: "submit".into(),
                    payload: CommandPayload::Submit(Submit {
                        text: "edit".into(),
                        mode: AgentMode::Basic,
                    }),
                };
                handle.post(command).await.unwrap();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while let Some(event) = handle.next_event().await {
                        match event.payload {
                            EventPayload::ToolFinished(value) if !lose_ack => {
                                assert_eq!(value.execution, Execution::Succeeded);
                                assert_eq!(value.effects, Effects::KnownResult);
                            }
                            EventPayload::Done if !lose_ack => break,
                            EventPayload::FrontendSnapshot(snapshot)
                                if lose_ack && snapshot.state == "paused" =>
                            {
                                break;
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .unwrap();
                assert!(intent_seen.load(Ordering::SeqCst));
                runtime.shutdown().await.unwrap();
                if lose_ack {
                    let previous_calls = calls.load(Ordering::SeqCst);
                    let mut restarted = Runtime::new(
                        "edit".into(),
                        configuration.clone(),
                        tools.clone(),
                        generate.clone(),
                        Limits::default(),
                        Options {
                            directory: Some(directory.clone()),
                            ..Options::default()
                        },
                    )
                    .unwrap();
                    let handle = restarted.handle();
                    handle.ready().await.unwrap();
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(event) = handle.next_event().await {
                            if let EventPayload::FrontendSnapshot(snapshot) = event.payload
                                && snapshot.state == "paused"
                            {
                                assert!(snapshot.recovery_operation.is_some());
                                break;
                            }
                        }
                    })
                    .await
                    .unwrap();
                    assert_eq!(calls.load(Ordering::SeqCst), previous_calls);
                    restarted.shutdown().await.unwrap();
                }
            });
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            if lose_ack {
                "let value = 1;\n"
            } else {
                "let value = 2;\n"
            }
        );
        let mut kinds = Vec::new();
        let journal = Journal::open(
            &directory,
            |event| {
                kinds.push(event.text_field("kind")?.to_owned());
                Ok(())
            },
            None,
        )
        .unwrap();
        assert!(journal.diagnostic().is_empty());
        assert!(kinds.iter().any(|kind| kind == "intent"));
        if !lose_ack {
            assert!(kinds.iter().any(|kind| kind == "outcome"));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn edit_instruction_handshake_retries_the_returned_token_and_completes_without_resume() {
    use ansa::{
        provider::{
            native,
            replay::{Effects, Execution},
        },
        session::journal_v1::Journal,
    };
    use std::sync::Arc;
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let epoch = agent_epoch(&settings).unwrap();
    let nested = fixture.cwd().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(
        nested.join("AGENTS.md"),
        "Keep the surrounding code intact.",
    )
    .unwrap();
    let file = nested.join("code.rs");
    std::fs::write(&file, "let value = 1;\n").unwrap();
    let directory = fixture.0.join("instruction-edit-session");
    let tools = local_tools(
        settings,
        epoch.clone(),
        &directory,
        &fixture.0.join("auth"),
        Path::new(env!("CARGO_BIN_EXE_ansa")),
    )
    .unwrap();
    let configuration = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "fixture".into(),
            requested_model: "fixture".into(),
            returned_model: "fixture".into(),
        },
        epoch,
        tools.advertisement().clone(),
        Vec::new(),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    let target = file.clone();
    let generate = Arc::new(
        move |request: &Request, _: &Cancel, _: ansa::runtime::PreviewSink| {
            let round = observed_calls.fetch_add(1, Ordering::SeqCst);
            let token = match round {
                0 => {
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "let value = 1;\n"
                    );
                    "wrong".to_owned()
                }
                1 => {
                    let last = request.input.last().unwrap();
                    assert_eq!(last.text_field("type")?, "function_call_output");
                    let instructions = Json::parse(last.text_field("output")?)?;
                    assert_eq!(instructions.text_field("status")?, "instructions_required");
                    assert_eq!(*instructions.field("executed")?, Json::parse("false")?);
                    assert!(!instructions.field("instructions")?.elements()?.is_empty());
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "let value = 1;\n",
                        "the instruction handshake changed the target"
                    );
                    let token = instructions.text_field("instruction_token")?;
                    assert_eq!(token.len(), 64);
                    assert_ne!(token, "wrong");
                    token.to_owned()
                }
                2 => {
                    let last = request.input.last().unwrap();
                    let output = Json::parse(last.text_field("output")?)?;
                    assert_eq!(*output.field("applied")?, Json::parse("true")?);
                    assert_eq!(output.text_field("before")?, "let value = 1;\n");
                    assert_eq!(output.text_field("after")?, "let value = 2;\n");
                    assert_eq!(
                        std::fs::read_to_string(&target).unwrap(),
                        "let value = 2;\n"
                    );
                    String::new()
                }
                _ => panic!("unexpected generation after the edit completed"),
            };
            let item = if round < 2 {
                let arguments = Json::object(vec![
                    ("path", s("nested/code.rs")),
                    ("old_text", s("let value = 1;")),
                    ("new_text", s("let value = 2;")),
                    ("instruction_token", s(&token)),
                ])?;
                Json::object(vec![
                    ("type", s("function_call")),
                    ("id", s(&format!("instruction_edit_item{round}"))),
                    ("call_id", s(&format!("instruction_edit_call{round}"))),
                    ("namespace", s("omh")),
                    ("name", s("edit_file")),
                    ("arguments", s(&arguments.dump())),
                    ("status", s("completed")),
                ])?
            } else {
                Json::object(vec![
                    ("type", s("message")),
                    ("id", s("instruction_edit_message")),
                    ("role", s("assistant")),
                    ("status", s("completed")),
                    (
                        "content",
                        Json::array(vec![Json::object(vec![
                            ("type", s("output_text")),
                            ("text", s("EDIT_DONE")),
                        ])?])?,
                    ),
                ])?
            };
            native::load(&Json::object(vec![
                ("request", request.save()?),
                ("id", s(&format!("instruction_edit_response{round}"))),
                ("model", s(&request.binding.returned_model)),
                ("items", Json::array(vec![item])?),
            ])?)
        },
    );
    let mut runtime = Runtime::new(
        "edit_instructions".into(),
        configuration,
        tools,
        generate,
        Limits {
            tool_rounds: 3,
            ..Limits::default()
        },
        Options {
            directory: Some(directory.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let handle = runtime.handle();
    handle.ready().await.unwrap();
    handle
        .post(RuntimeCommand {
            schema_version: 1,
            session: handle.session().clone(),
            incarnation: handle.incarnation().clone(),
            turn: "turn".into(),
            command: "only_submit".into(),
            payload: CommandPayload::Submit(Submit {
                text: "edit the nested file".into(),
                mode: AgentMode::Basic,
            }),
        })
        .await
        .unwrap();
    let mut finished = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = handle
                .next_event()
                .await
                .expect("runtime ended before Done");
            match event.payload {
                EventPayload::ToolFinished(value) => finished.push(value),
                EventPayload::FrontendSnapshot(value) => {
                    assert_ne!(value.state, "paused", "{}", value.diagnostic);
                    assert!(!value.storage_failed, "{}", value.diagnostic);
                }
                EventPayload::Done => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(finished.len(), 2);
    assert_ne!(finished[0].operation, finished[1].operation);
    assert_eq!(
        (finished[0].execution, finished[0].effects),
        (Execution::Failed, Effects::KnownResult)
    );
    assert_eq!(
        (finished[1].execution, finished[1].effects),
        (Execution::Succeeded, Effects::KnownResult)
    );
    let outputs: Vec<_> = finished
        .iter()
        .map(|value| Json::parse(value.output.as_str()).unwrap())
        .collect();
    assert_eq!(
        outputs
            .iter()
            .filter(|value| value.has("applied")
                && *value.field("applied").unwrap() == Json::parse("true").unwrap())
            .count(),
        1
    );
    assert_eq!(std::fs::read_to_string(file).unwrap(), "let value = 2;\n");
    let mut kinds = Vec::new();
    let journal = Journal::open(
        &directory,
        |event| {
            kinds.push(event.text_field("kind")?.to_owned());
            Ok(())
        },
        None,
    )
    .unwrap();
    assert!(journal.diagnostic().is_empty());
    assert_eq!(kinds.iter().filter(|kind| *kind == "intent").count(), 2);
    assert_eq!(kinds.iter().filter(|kind| *kind == "outcome").count(), 2);
    assert!(!kinds.iter().any(|kind| kind == "resume"));
}

#[test]
fn file_guards_selection_and_markdown_are_plain_terminal_text() {
    let fixture = Fixture::new();
    let cwd = fixture.cwd();
    let host = fixture.host();
    let stop = Cancel::default();
    std::fs::write(
        cwd.join("notes.md"),
        "# Заголовок\n\n**text**\n\x1b]52;c;evil\x07\n",
    )
    .unwrap();
    std::fs::write(cwd.join("source.rs"), "fn main() {}\n").unwrap();
    std::fs::create_dir(cwd.join("directory")).unwrap();
    symlink(cwd.join("source.rs"), cwd.join("alias.rs")).unwrap();
    std::fs::hard_link(cwd.join("source.rs"), cwd.join("hard.rs")).unwrap();
    let document = host.open(Path::new("notes.md"), &stop).unwrap();
    assert!(!document.source.contains('\x1b'));
    assert!(document.source.contains("\\u{1b}]52"));
    let preview = document.preview.unwrap().plain();
    assert!(preview.starts_with("Заголовок\n"));
    assert!(!preview.contains("**"));
    assert!(!preview.contains('\x1b'));
    assert_eq!(document.diff.status, DiffStatus::NoRepository);
    assert!(host.open(Path::new("alias.rs"), &stop).is_err());
    assert_eq!(
        host.open(Path::new("hard.rs"), &stop),
        Err("workspace_file_type")
    );
    // Protection is checked before any attempt to open this nonexistent path.
    assert_eq!(host.open(Path::new(".env"), &stop), Err("protected_file"));
    assert_eq!(
        host.open(Path::new(".env/../notes.md"), &stop),
        Err("protected_file")
    );
    let listing = host.list(Path::new("."), &stop).unwrap();
    assert!(listing.entries[0].directory);
    assert!(
        !listing
            .entries
            .iter()
            .any(|entry| matches!(entry.name.as_str(), "alias.rs" | "hard.rs" | "source.rs"))
    );
    stop.cancel();
    assert_eq!(
        host.open(Path::new("notes.md"), &stop),
        Err("workspace_cancelled")
    );
}

#[test]
fn diff_is_head_against_the_opened_current_snapshot() {
    let fixture = Fixture::new();
    fixture.git(&["init", "-q"]);
    std::fs::write(fixture.cwd().join("retry.rs"), "a\nb\nc\n").unwrap();
    fixture.git(&["add", "--", "retry.rs"]);
    fixture.git(&["commit", "-q", "-m", "fixture"]);
    let host = fixture.host();
    let stop = Cancel::default();
    assert_eq!(
        host.open(Path::new("retry.rs"), &stop).unwrap().diff.status,
        DiffStatus::Unchanged
    );
    std::fs::write(fixture.cwd().join("retry.rs"), "a\nchanged\nc\n").unwrap();
    let diff = host.open(Path::new("retry.rs"), &stop).unwrap().diff;
    assert_eq!(diff.status, DiffStatus::Modified);
    assert!(diff.text.contains("--- HEAD/retry.rs"));
    assert!(diff.text.contains("-b\n+changed\n"));
    std::fs::write(fixture.cwd().join("new.md"), "new\n").unwrap();
    let diff = host.open(Path::new("new.md"), &stop).unwrap().diff;
    assert_eq!(diff.status, DiffStatus::Untracked);
    assert!(diff.text.contains("+new\n"));
}

#[test]
fn readonly_diff_never_executes_promisor_fetch_helpers() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.git(&["init", "-q"]);
    std::fs::write(fixture.cwd().join("source.txt"), "committed content\n").unwrap();
    fixture.git(&["add", "source.txt"]);
    fixture.git(&["commit", "-q", "-m", "fixture"]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD:source.txt"])
        .current_dir(fixture.cwd())
        .output()
        .unwrap();
    assert!(output.status.success());
    let object = String::from_utf8(output.stdout).unwrap();
    let object = object.trim();
    let helper = fixture.0.join("ssh-probe");
    let marker = fixture.0.join("helper-executed");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.git(&[
        "config",
        "remote.origin.url",
        "ssh://example.invalid/fixture",
    ]);
    fixture.git(&["config", "remote.origin.promisor", "true"]);
    fixture.git(&["config", "remote.origin.partialclonefilter", "blob:none"]);
    fixture.git(&["config", "core.sshCommand", helper.to_str().unwrap()]);
    std::fs::remove_file(
        fixture
            .cwd()
            .join(".git/objects")
            .join(&object[..2])
            .join(&object[2..]),
    )
    .unwrap();
    let document = fixture
        .host()
        .open(Path::new("source.txt"), &Cancel::default())
        .unwrap();
    assert_eq!(document.diff.status, DiffStatus::Unavailable);
    assert!(
        !marker.exists(),
        "readonly inspection executed a repository helper"
    );
    assert_eq!(document.source, "committed content\n");
}

#[test]
fn file_size_and_binary_limits_are_explicit() {
    let fixture = Fixture::new();
    let host = fixture.host();
    std::fs::write(fixture.cwd().join("large.rs"), vec![b'a'; 300 * 1024]).unwrap();
    let document = host
        .open(Path::new("large.rs"), &Cancel::default())
        .unwrap();
    assert!(document.truncated);
    assert_eq!(document.diff.status, DiffStatus::Unavailable);
    std::fs::write(fixture.cwd().join("binary"), [b'a', 0, b'b']).unwrap();
    assert_eq!(
        host.open(Path::new("binary"), &Cancel::default()),
        Err("workspace_binary_file")
    );
}

#[test]
fn editor_parsing_never_expands_shell_fragments_or_file_names() {
    let command = EditorCommand::parse(
        "'editor path' --wait \"literal $(echo x)\"",
        Path::new("/tmp/work space/notes.md"),
    )
    .unwrap();
    assert_eq!(
        command.argv,
        [
            "editor path",
            "--wait",
            "literal $(echo x)",
            "--",
            "/tmp/work space/notes.md"
        ]
    );
    assert!(EditorCommand::parse("vim 'unfinished", Path::new("/tmp/file")).is_err());
    assert!(EditorCommand::parse("vim", Path::new("/tmp/.env.test")).is_err());
}

#[test]
fn console_uses_normal_durable_tool_admission_and_keeps_nonzero_exit() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let epoch = agent_epoch(&settings).unwrap();
    let directory = fixture.0.join("console");
    let tools = local_tools(
        settings.clone(),
        epoch.clone(),
        &directory,
        &fixture.0.join("auth"),
        Path::new(env!("CARGO_BIN_EXE_ansa")),
    )
    .unwrap();
    let request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "console-fixture".into(),
            requested_model: "console".into(),
            returned_model: "console".into(),
        },
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let generate = console_generate(&settings, PathBuf::from("/bin/sh")).unwrap();
    let mut runtime = Runtime::new(
        "console-fixture".into(),
        request,
        tools,
        generate,
        Limits {
            tool_rounds: 4,
            ..Limits::default()
        },
        Options {
            directory: Some(directory),
            ..Options::default()
        },
    )
    .unwrap();
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    reactor.block_on(async {
        runtime.ready().await.unwrap();
        let command = RuntimeCommand {
            schema_version: 1,
            session: "console-fixture".into(),
            turn: "console-turn".into(),
            incarnation: runtime.handle().incarnation().clone(),
            command: "console-submit".into(),
            payload: CommandPayload::Submit(Submit {
                text: "printf console-output; exit 7".into(),
                mode: AgentMode::Basic,
            }),
        };
        runtime.post(command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let event = runtime.next_event().await.unwrap();
                if let EventPayload::FrontendSnapshot(snapshot) = event.payload {
                    assert!(!snapshot.storage_failed);
                    if snapshot.state == "completed" {
                        assert!(
                            snapshot.text.contains("console-output"),
                            "{}",
                            snapshot.text
                        );
                        assert!(snapshot.text.contains("exit: 7"), "{}", snapshot.text);
                        assert_eq!(snapshot.completed_tools, 1);
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
    });
}

#[test]
fn console_rejects_invalid_user_command_before_dispatch() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let mut request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "fixture".into(),
            requested_model: "console".into(),
            returned_model: "console".into(),
        },
        agent_epoch(&settings).unwrap(),
        ansa::frontend::app::pure_tools()
            .unwrap()
            .advertisement()
            .clone(),
        vec![Json::object(vec![("role", s("user")), ("content", s("\0"))]).unwrap()],
    )
    .unwrap();
    let generate = console_generate(&settings, PathBuf::from("/bin/sh")).unwrap();
    let preview = std::sync::Arc::new(|_: &str| Ok(()));
    assert_eq!(
        generate(&request, &Cancel::default(), preview.clone()).unwrap_err(),
        "console_command"
    );
    request.input =
        vec![Json::object(vec![("role", s("user")), ("content", s("echo x"))]).unwrap()];
    let stop = Cancel::default();
    stop.cancel();
    assert_eq!(
        generate(&request, &stop, preview).unwrap_err(),
        "workspace_cancelled"
    );
}
