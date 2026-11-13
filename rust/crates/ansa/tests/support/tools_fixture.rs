use ansa::{
    auth::Cancel,
    config::{self, AgentSettings, agent_epoch, load_agent_settings},
    protocol::json::Json,
    provider::{
        native,
        replay::Effects,
        request::{Binding, Request},
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        command::{ProcessOptions, ProcessRunner},
        files::local_tools,
        invocation::Invocation,
    },
};
use std::{
    os::unix::fs::{DirBuilderExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
pub struct Root(pub PathBuf);
impl Root {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-m6-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::create_dir_all(path.join("home/workspace")).unwrap();
        Self(path)
    }
    pub fn cwd(&self) -> PathBuf {
        self.0.join("home/workspace")
    }
    pub fn home(&self) -> PathBuf {
        self.0.join("home")
    }
    pub fn settings(&self) -> AgentSettings {
        load_agent_settings(&self.cwd(), &self.home()).unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub fn j(value: &str) -> Json {
    Json::parse(value).unwrap()
}
pub fn s(value: &str) -> Json {
    Json::string(value).unwrap()
}
pub fn n(value: usize) -> Json {
    Json::parse(&value.to_string()).unwrap()
}
pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
pub fn worker() -> PathBuf {
    option_env!("CARGO_BIN_EXE_ansa-rs")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap())
}
pub fn invocation(name: &str) -> Invocation {
    Invocation {
        operation: name.into(),
        call: "call".into(),
        name: "execute".into(),
        revision: "1".into(),
        arguments: j("{}"),
    }
}
pub fn options(root: &Root, command: &str) -> ProcessOptions {
    ProcessOptions {
        cwd: root.0.clone(),
        argv: vec!["sh".into(), "-c".into(), command.into()],
        timeout_ms: 3000,
        output_bytes: 16384,
    }
}
pub fn fixture() -> PathBuf {
    std::env::var_os("ANSA_PARITY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/parity"))
        .join("process_fixture.py")
}
pub fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "process never started: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub fn invoke(root: &Root, name: &str, args: Json) -> Json {
    let settings = root.settings();
    let epoch = agent_epoch(&settings).unwrap();
    let tools = local_tools(
        settings,
        epoch.clone(),
        &root.0.join("session"),
        &root.0.join("auth"),
        &worker(),
    )
    .unwrap();
    let request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let response = native::load(
        &Json::object(vec![
            ("request", request.save().unwrap()),
            ("id", s("response")),
            ("model", s(&request.binding.returned_model)),
            (
                "items",
                Json::array(vec![
                    Json::object(vec![
                        ("type", s("function_call")),
                        ("id", s("item")),
                        ("call_id", s("call")),
                        ("name", s(name)),
                        ("namespace", s("omh")),
                        ("arguments", s(&args.dump())),
                        ("status", s("completed")),
                    ])
                    .unwrap(),
                ])
                .unwrap(),
            ),
        ])
        .unwrap(),
    )
    .unwrap();
    let mut ids = OperationIds::new("test", 0).unwrap();
    let result = PreparedBatch::prepare(&response, tools, &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(result.results().len(), 1);
    assert_eq!(result.results()[0].effects(), Effects::KnownResult);
    Json::parse(result.results()[0].output()).unwrap()
}
pub fn token(root: &Root) -> String {
    ansa::protocol::legacy_json_v1::sha256(
        agent_epoch(&root.settings())
            .unwrap()
            .exact_state
            .field("instructions")
            .unwrap()
            .dump()
            .as_bytes(),
    )
}
pub fn read_args(root: &Root, path: &str) -> Json {
    Json::object(vec![
        ("path", s(path)),
        ("offset", n(0)),
        ("max_bytes", n(8192)),
        ("instruction_token", s(&token(root))),
    ])
    .unwrap()
}
pub fn configuration() {
    let root = Root::new();
    write(
        &root.home().join(".ansa/config.json"),
        r#"{"schema_version":1,"model":"global","tool_rounds":12,"allow_sudo":false}"#,
    );
    write(
        &root.cwd().join(".ansa/config.json"),
        r#"{"schema_version":1,"model":"project","tool_calls":24}"#,
    );
    let settings = root.settings();
    assert_eq!(settings.model, "project");
    assert_eq!(settings.returned_model, "project");
    assert_eq!(settings.tool_rounds, 12);
    assert_eq!(settings.tool_calls, 24);
    for invalid in [
        r#"{"schema_version":1,"allow_sudo":false}"#,
        r#"{"schema_version":1,"unknown":true}"#,
        r#"{"schema_version":1,"process_timeout_ms":0}"#,
    ] {
        write(&root.cwd().join(".ansa/config.json"), invalid);
        assert!(load_agent_settings(&root.cwd(), &root.home()).is_err());
    }
}
pub fn instruction_links() {
    let root = Root::new();
    let private = root.home().join(".ansa/auth/fixture");
    write(&private, "synthetic fixture");
    let target = root.cwd().join("AGENTS.md");
    symlink(&private, &target).unwrap();
    assert!(config::load_instructions(&root.cwd(), &root.home(), 65536).is_err());
    std::fs::remove_file(&target).unwrap();
    std::fs::hard_link(&private, &target).unwrap();
    assert!(config::load_instructions(&root.cwd(), &root.home(), 65536).is_err());
    std::fs::remove_file(target).unwrap();
    symlink(private.parent().unwrap(), root.cwd().join(".ansa")).unwrap();
    assert!(load_agent_settings(&root.cwd(), &root.home()).is_err());
}
pub fn files() {
    let root = Root::new();
    write(&root.home().join(".ansa/AGENTS.md"), "Global MARKER_GLOBAL");
    write(&root.cwd().join("AGENTS.md"), "Project MARKER_PROJECT");
    write(&root.cwd().join("value.txt"), "line one\nneedle line\n");
    write(&root.cwd().join("nested/AGENTS.md"), "MARKER_NESTED");
    write(&root.cwd().join("nested/hidden.txt"), "nested needle");
    let epoch = agent_epoch(&root.settings()).unwrap();
    assert!(
        epoch.instructions.contains("MARKER_GLOBAL")
            && epoch.instructions.contains("MARKER_PROJECT")
            && !epoch.instructions.contains("MARKER_NESTED")
    );
    assert_eq!(
        invoke(&root, "read_file", read_args(&root, "value.txt"))
            .text_field("content")
            .unwrap(),
        "line one\nneedle line\n"
    );
    let mut args = read_args(&root, "nested/hidden.txt");
    let first = invoke(&root, "read_file", args.clone());
    assert_eq!(first.text_field("status").unwrap(), "instructions_required");
    args = Json::object(vec![
        ("path", s("nested/hidden.txt")),
        ("offset", n(0)),
        ("max_bytes", n(8192)),
        (
            "instruction_token",
            first.field("instruction_token").unwrap().clone(),
        ),
    ])
    .unwrap();
    assert_eq!(
        invoke(&root, "read_file", args)
            .text_field("content")
            .unwrap(),
        "nested needle"
    );
    let searched = invoke(
        &root,
        "search_files",
        Json::object(vec![
            ("path", s(".")),
            ("query", s("needle")),
            ("mode", s("content")),
            ("max_results", n(10)),
            ("instruction_token", s(&token(&root))),
        ])
        .unwrap(),
    );
    assert_eq!(
        searched.field("matches").unwrap().elements().unwrap().len(),
        1
    );
    assert_eq!(
        searched
            .field("instruction_scopes")
            .unwrap()
            .elements()
            .unwrap()
            .len(),
        1
    );
    let private = root.0.join("auth/private");
    write(&private, "synthetic secret");
    symlink(&private, root.cwd().join("linked")).unwrap();
    assert_eq!(
        invoke(&root, "read_file", read_args(&root, "linked"))
            .text_field("error")
            .unwrap(),
        "protected_file"
    );
    std::fs::hard_link(private, root.cwd().join("hard")).unwrap();
    assert_eq!(
        invoke(&root, "read_file", read_args(&root, "hard"))
            .text_field("error")
            .unwrap(),
        "file_not_regular"
    );
}
pub fn basic() {
    let root = Root::new();
    let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
    let options = options(
        &root,
        "printf hello; printf diagnostic >&2; printf x >> marker; exit 7",
    );
    let id = invocation("operation");
    let result = runner.run(&id, &options, &Cancel::default()).unwrap();
    assert_eq!(result.effects(), Effects::KnownResult);
    let text = String::from_utf8(result.output().to_vec()).unwrap();
    let output = j(&text);
    assert_eq!(output.usize_field("exit_code").unwrap(), 7);
    assert_eq!(output.text_field("stdout").unwrap(), "hello");
    assert_eq!(output.text_field("stderr").unwrap(), "diagnostic");
    assert_eq!(
        runner.run(&id, &options, &Cancel::default()).unwrap(),
        result
    );
    assert_eq!(std::fs::metadata(root.0.join("marker")).unwrap().len(), 1);
    let mut changed = options.clone();
    changed.argv[2] = "printf bad >> marker".into();
    assert!(runner.run(&id, &changed, &Cancel::default()).is_err());
}

pub fn raw_names() {
    use std::os::unix::ffi::OsStringExt;
    let root = Root::new();
    let raw = std::ffi::OsString::from_vec(b"match\xff.txt".to_vec());
    std::fs::write(root.cwd().join(raw), "needle").unwrap();
    for mode in ["name", "content"] {
        let output = invoke(
            &root,
            "search_files",
            Json::object(vec![
                ("path", s(".")),
                ("query", s(if mode == "name" { "match" } else { "needle" })),
                ("mode", s(mode)),
                ("max_results", n(10)),
                ("instruction_token", s(&token(&root))),
            ])
            .unwrap(),
        );
        let values = output.field("matches").unwrap().elements().unwrap();
        assert_eq!(values.len(), 1);
        assert!(
            values[0]
                .text_field("path")
                .unwrap()
                .ends_with("match\u{fffd}.txt")
        );
    }
    let dir = root
        .cwd()
        .join(std::ffi::OsString::from_vec(b"scope\xff".to_vec()));
    write(&dir.join("AGENTS.md"), "fixture rule");
    let output = invoke(
        &root,
        "search_files",
        Json::object(vec![
            ("path", s(".")),
            ("query", s("needle")),
            ("mode", s("content")),
            ("max_results", n(10)),
            ("instruction_token", s(&token(&root))),
        ])
        .unwrap(),
    );
    assert_eq!(output.text_field("error").unwrap(), "json_string_invalid");
}
pub fn future_auth_root() {
    for alias in [false, true] {
        let root = Root::new();
        let settings = root.settings();
        let epoch = agent_epoch(&settings).unwrap();
        std::fs::create_dir(root.0.join("alias_parent")).unwrap();
        if alias {
            symlink(root.0.join("alias_parent"), root.0.join("link")).unwrap();
        }
        let auth = root.0.join(if alias {
            "link/../custom-auth"
        } else {
            "alias_parent/../custom-auth"
        });
        let tools =
            local_tools(settings, epoch, &root.0.join("session"), &auth, &worker()).unwrap();
        let private = root.0.join("custom-auth/private");
        write(&private, "synthetic must not escape");
        let result = (tools.definition("read_file").unwrap().executor)(&read_args(
            &root,
            private.to_str().unwrap(),
        ));
        let output = j(std::str::from_utf8(result.output()).unwrap());
        assert_eq!(output.text_field("error").unwrap(), "protected_file");
        let args = Json::object(vec![
            ("path", s(root.0.to_str().unwrap())),
            ("query", s("synthetic must not escape")),
            ("mode", s("content")),
            ("max_results", n(10)),
            ("instruction_token", s(&token(&root))),
        ])
        .unwrap();
        let output = (tools.definition("search_files").unwrap().executor)(&args);
        let output = j(std::str::from_utf8(output.output()).unwrap());
        assert!(
            output
                .field("matches")
                .unwrap()
                .elements()
                .unwrap()
                .is_empty()
        );
    }
}
fn parsed(value: &ansa::tools::registry::Outcome) -> Json {
    j(std::str::from_utf8(value.output()).unwrap())
}
fn fixture_options(root: &Root, mode: &str, timeout_ms: usize) -> ProcessOptions {
    ProcessOptions {
        cwd: root.0.clone(),
        argv: vec![
            "python3".into(),
            fixture().to_str().unwrap().into(),
            mode.into(),
            root.0.to_str().unwrap().into(),
        ],
        timeout_ms,
        output_bytes: 16384,
    }
}
pub fn cancellation(timeout: bool) {
    let root = Root::new();
    let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
    let options = fixture_options(&root, "tree", if timeout { 200 } else { 30000 });
    let stop = Cancel::default();
    let linked = stop.clone();
    let running = std::thread::spawn(move || runner.run(&invocation("cancel"), &options, &linked));
    wait_file(&root.0.join("descendant"));
    let start = Instant::now();
    if !timeout {
        stop.cancel();
    }
    let result = running.join().unwrap().unwrap();
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(result.effects(), Effects::KnownResult);
    let output = parsed(&result);
    assert!(
        output
            .field(if timeout { "timed_out" } else { "cancelled" })
            .unwrap()
            .boolean()
            .unwrap()
    );
    assert!(output.field("cleanup_complete").unwrap().boolean().unwrap());
    for name in ["primary", "descendant"] {
        let pid: u32 = std::fs::read_to_string(root.0.join(name))
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 1);
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "owned {name} survived"
        );
    }
}
pub fn detached() {
    for index in 0..32 {
        let root = Root::new();
        let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
        let result = runner
            .run(
                &invocation("detach"),
                &fixture_options(&root, "detach", 3000),
                &Cancel::default(),
            )
            .unwrap();
        assert_eq!(
            result.effects(),
            Effects::KnownResult,
            "schedule {index}: {result:?}"
        );
        let output = parsed(&result);
        assert!(output.field("cleanup_complete").unwrap().boolean().unwrap());
        let pid: u32 = std::fs::read_to_string(root.0.join("descendant"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 1);
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "detached child survived adoption schedule {index}"
        );
    }
}
pub fn process_output() {
    let root = Root::new();
    let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
    let mut options = fixture_options(&root, "output", 5000);
    options.output_bytes = 1024;
    let result = runner
        .run(&invocation("flood"), &options, &Cancel::default())
        .unwrap();
    assert_eq!(result.effects(), Effects::KnownResult);
    let output = parsed(&result);
    assert!(output.field("truncated").unwrap().boolean().unwrap());
    assert_eq!(output.usize_field("stdout_bytes").unwrap(), 64 * 4096);
    assert_eq!(output.usize_field("stderr_bytes").unwrap(), 64 * 4096);
    assert!(
        output.text_field("stdout").unwrap().len() + output.text_field("stderr").unwrap().len()
            <= 3072
    );
}
pub fn privileges() {
    let root = Root::new();
    let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
    let output = parsed(
        &runner
            .run(
                &invocation("security"),
                &fixture_options(&root, "security", 3000),
                &Cancel::default(),
            )
            .unwrap(),
    );
    let status = j(output.text_field("stdout").unwrap());
    assert_eq!(status.usize_field("no_new_privs").unwrap(), 1);
    assert_ne!(status.usize_field("uid").unwrap(), 0);
    assert!(!status.field("tty").unwrap().boolean().unwrap());
    let mut options = options(&root, "unused");
    options.argv = vec!["sudo".into(), "unreachable".into()];
    let output = parsed(
        &runner
            .run(&invocation("sudo"), &options, &Cancel::default())
            .unwrap(),
    );
    assert_eq!(output.text_field("error").unwrap(), "sudo_disabled");
}
pub fn evidence() {
    let root = Root::new();
    let runner = ProcessRunner::new(&root.0.join("jobs"), &worker()).unwrap();
    let id = invocation("evidence");
    let options = options(&root, "printf x >> marker; printf ok");
    runner.run(&id, &options, &Cancel::default()).unwrap();
    let job = root
        .0
        .join("jobs")
        .join(ansa::protocol::legacy_json_v1::sha256(
            id.operation.as_bytes(),
        ));
    std::fs::rename(job.join("result.json"), job.join("saved-result.json")).unwrap();
    let status = std::process::Command::new(worker())
        .arg("--process-worker")
        .arg(&job)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(125));
    assert_eq!(std::fs::metadata(root.0.join("marker")).unwrap().len(), 1);
    std::fs::rename(job.join("saved-result.json"), job.join("result.json")).unwrap();
    std::fs::rename(job.join("started.json"), job.join("saved-started.json")).unwrap();
    assert!(runner.recover(&id).is_err());
    let directory = ansa_linux::fs::private_directory(&job).unwrap();
    ansa_linux::local::create(
        &directory,
        "started.json",
        br#"{"version":1,"request_hash":"wrong"}"#,
    )
    .unwrap();
    assert!(runner.recover(&id).is_err());
}

pub fn runtime_loop(cancel: bool) {
    use ansa::runtime::*;
    use std::sync::Arc;
    let root = Root::new();
    let settings = root.settings();
    let epoch = agent_epoch(&settings).unwrap();
    let token = token(&root);
    let tools = local_tools(
        settings,
        epoch.clone(),
        &root.0.join("session"),
        &root.0.join("auth"),
        &worker(),
    )
    .unwrap();
    let request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let cwd = root.0.to_str().unwrap().to_owned();
    let helper = fixture().to_str().unwrap().to_owned();
    let generate: Generate = Arc::new(move |request, _, _| {
        let completed: Vec<_> = request
            .input
            .iter()
            .filter(|v| {
                v.has("type")
                    && v.text_field("type")
                        .is_ok_and(|v| v == "function_call_output")
            })
            .collect();
        let index = completed.len();
        if index == 1 && !cancel {
            let previous = j(completed[0].text_field("output")?);
            assert_eq!(previous.usize_field("exit_code")?, 7);
        }
        let item = if index >= 2 {
            Json::object(vec![
                ("type", s("message")),
                ("id", s("message")),
                ("role", s("assistant")),
                ("status", s("completed")),
                (
                    "content",
                    Json::array(vec![Json::object(vec![
                        ("type", s("output_text")),
                        ("text", s("NONZERO_RECOVERED")),
                    ])?])?,
                ),
            ])?
        } else {
            let argv = if cancel {
                Json::array(vec![s("python3"), s(&helper), s("tree"), s(&cwd)])?
            } else {
                Json::array(vec![
                    s("sh"),
                    s("-c"),
                    s(if index == 0 {
                        "printf x >> marker; exit 7"
                    } else {
                        "printf x >> marker; printf corrected"
                    }),
                ])?
            };
            let args = Json::object(vec![
                ("argv", argv),
                ("cwd", s(&cwd)),
                ("timeout_ms", n(30000)),
                ("instruction_token", s(&token)),
            ])?;
            Json::object(vec![
                ("type", s("function_call")),
                ("id", s(&format!("item{index}"))),
                ("call_id", s(&format!("call{index}"))),
                ("name", s("run_command")),
                ("namespace", s("omh")),
                ("arguments", s(&args.dump())),
                ("status", s("completed")),
            ])?
        };
        native::load(&Json::object(vec![
            ("request", request.save()?),
            ("id", s("response")),
            ("model", s(&request.binding.returned_model)),
            ("items", Json::array(vec![item])?),
        ])?)
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let mut runtime = Runtime::new(
                "local".into(),
                request,
                tools,
                generate,
                Limits {
                    tool_rounds: 3,
                    ..Limits::default()
                },
                Options {
                    directory: Some(root.0.join("session")),
                    ..Options::default()
                },
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), runtime.ready())
                .await
                .unwrap()
                .unwrap();
            let handle = runtime.handle();
            let cmd = |id: &str, payload| RuntimeCommand {
                schema_version: 1,
                session: "local".into(),
                turn: "turn".into(),
                incarnation: handle.incarnation().clone(),
                command: id.into(),
                payload,
            };
            assert_eq!(
                handle
                    .post(cmd(
                        "submit",
                        CommandPayload::Submit(Submit {
                            text: "fixture".into(),
                            mode: AgentMode::Basic
                        })
                    ))
                    .await
                    .unwrap(),
                PostStatus::Accepted
            );
            let request = loop {
                let e = tokio::time::timeout(Duration::from_secs(10), runtime.next_event())
                    .await
                    .unwrap()
                    .unwrap();
                if let EventPayload::CommandAccepted {
                    request: Some(id), ..
                } = e.payload
                {
                    break id;
                }
            };
            if cancel {
                wait_file(&root.0.join("descendant"));
                assert_eq!(
                    handle
                        .post(cmd("cancel", CommandPayload::Cancel(request)))
                        .await
                        .unwrap(),
                    PostStatus::Accepted
                );
            }
            let mut finished = 0;
            let mut cancelled = false;
            loop {
                let e = tokio::time::timeout(Duration::from_secs(10), runtime.next_event())
                    .await
                    .unwrap()
                    .unwrap();
                match e.payload {
                    EventPayload::ToolFinished(v) => {
                        assert_eq!(v.effects, Effects::KnownResult);
                        finished += 1;
                    }
                    EventPayload::Cancelled { .. } => cancelled = true,
                    EventPayload::Done => {
                        assert!(!cancel);
                        assert_eq!(finished, 2);
                        break;
                    }
                    EventPayload::RuntimeError { diagnostic, .. } => {
                        panic!("runtime local command: {diagnostic}")
                    }
                    _ => {}
                }
                if cancel && cancelled && finished == 1 {
                    break;
                }
            }
            runtime.close_input();
            tokio::time::timeout(Duration::from_secs(10), async {
                while runtime.next_event().await.is_some() {}
                runtime.join().await.unwrap();
            })
            .await
            .unwrap();
            if !cancel {
                assert_eq!(std::fs::metadata(root.0.join("marker")).unwrap().len(), 2);
            } else {
                let pid = std::fs::read_to_string(root.0.join("descendant")).unwrap();
                assert!(!Path::new("/proc").join(pid).exists());
            }
        });
}
