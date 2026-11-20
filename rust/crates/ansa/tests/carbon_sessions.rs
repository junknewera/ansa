use ansa::{
    config::load_agent_settings,
    frontend::{
        protocol_v1::*,
        sessions::{Factory, Kind},
    },
    runtime::{PostStatus, Runtime},
};
use std::{
    fs::{self, DirBuilder},
    io::ErrorKind,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self::with_clock(
            &std::env::temp_dir(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            &NEXT,
        )
    }
    fn with_clock(parent: &Path, clock: u128, next: &AtomicU64) -> Self {
        let fixture = loop {
            let suffix = next.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "ansa-carbon-sessions-{}-{clock}-{suffix}",
                std::process::id()
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => break Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("fixture directory: {error}"),
            }
        };
        for name in ["home", "work"] {
            ansa_linux::fs::create_private(&fixture.0.join(name)).unwrap();
        }
        fixture
    }
    fn factory(&self) -> Factory {
        Self::factory_at(&self.0)
    }
    fn factory_at(root: &Path) -> Factory {
        let settings = load_agent_settings(&root.join("work"), &root.join("home")).unwrap();
        Factory::new(
            settings,
            root.join("auth"),
            true,
            "offline-fixture".into(),
            None,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[tokio::test(flavor = "current_thread")]
async fn fixture_clock_collisions_and_cleanup_preserve_another_live_journal() {
    use std::os::unix::fs::PermissionsExt;
    struct LegacyFixture(PathBuf);
    impl LegacyFixture {
        fn with_fixed_clock(parent: &Path, clock: u128) -> Self {
            let path = parent.join(format!(
                "ansa-carbon-sessions-{}-{clock}",
                std::process::id()
            ));
            // Reproduce the old reuse of an existing root only inside the
            // regression's exclusively owned evidence directory.
            for name in ["", "home", "work"] {
                ansa_linux::fs::create_private(&path.join(name)).unwrap();
            }
            Self(path)
        }
    }
    impl Drop for LegacyFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    async fn submit(
        runtime: &Runtime,
        id: &str,
        mode: AgentMode,
    ) -> ansa::frontend::protocol_v1::Snapshot {
        assert_eq!(
            runtime
                .post(command(
                    runtime,
                    id,
                    CommandPayload::Submit(Submit {
                        text: "inspect only".into(),
                        mode,
                    })
                ))
                .await
                .unwrap(),
            PostStatus::Accepted,
        );
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), runtime.next_event())
                .await
                .unwrap()
                .unwrap();
            match event.payload {
                EventPayload::FrontendSnapshot(view)
                    if view.storage_failed
                        || view.state == "paused"
                        || view.submit.as_ref().is_some_and(|submit| submit.0 == id)
                            && view.state == "completed"
                            && !view.worker_busy =>
                {
                    return *view;
                }
                EventPayload::ResyncRequired { .. } => panic!("fixture regression event overflow"),
                _ => {}
            }
        }
    }
    const CLOCK: u128 = 42;
    let evidence = Fixture::new();
    {
        let first = LegacyFixture::with_fixed_clock(&evidence.0, CLOCK);
        let second = LegacyFixture::with_fixed_clock(&evidence.0, CLOCK);
        assert_eq!(first.0, second.0);
        let mut main = Fixture::factory_at(&second.0)
            .create(Kind::Main, None)
            .unwrap();
        main.runtime.ready().await.unwrap();
        let part = main
            .selected
            .directory
            .join("part-00000000000000000000.log");
        assert!(part.is_file());
        drop(first);
        assert!(!part.exists());
        // Reach journal append even though the colliding fixture deleted
        // the workspace before a Plan would acquire its directory guard.
        let view = submit(&main.runtime, "old_fixture_plan", AgentMode::Basic).await;
        assert_eq!(view.state, "paused");
        assert!(view.storage_failed);
        assert_eq!(view.diagnostic, "journal_write_conflict");
        main.runtime.shutdown().await.unwrap();
        drop(second);
    }
    assert!(evidence.0.is_dir());
    let stale = evidence.0.join(format!(
        "ansa-carbon-sessions-{}-{CLOCK}-0",
        std::process::id()
    ));
    DirBuilder::new().mode(0o700).create(&stale).unwrap();
    fs::write(stale.join("marker"), b"stale owned evidence").unwrap();
    let next = AtomicU64::new(0);
    let first = Fixture::with_clock(&evidence.0, CLOCK, &next);
    let second = Fixture::with_clock(&evidence.0, CLOCK, &next);
    assert_ne!(first.0, second.0);
    assert_ne!(first.0, stale);
    assert_ne!(second.0, stale);
    assert_eq!(
        fs::metadata(&first.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&second.0).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut main = second.factory().create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    let part = main
        .selected
        .directory
        .join("part-00000000000000000000.log");
    let before = fs::read(&part).unwrap();
    assert!(!before.is_empty());
    drop(first);
    assert!(second.0.is_dir());
    assert_eq!(fs::read(&part).unwrap(), before);
    let view = submit(&main.runtime, "owned_fixture_plan", AgentMode::Plan).await;
    assert_eq!(view.state, "completed", "{}", view.diagnostic);
    assert!(!view.storage_failed, "{}", view.diagnostic);
    assert!(part.is_file());
    assert!(fs::read(&part).unwrap().len() > before.len());
    assert_eq!(
        fs::read(stale.join("marker")).unwrap(),
        b"stale owned evidence"
    );
    main.runtime.shutdown().await.unwrap();
}
fn command(runtime: &Runtime, id: &str, payload: CommandPayload) -> RuntimeCommand {
    let h = runtime.handle();
    RuntimeCommand {
        schema_version: 1,
        session: h.session().clone(),
        turn: "test_turn".into(),
        incarnation: h.incarnation().clone(),
        command: id.into(),
        payload,
    }
}
async fn context(runtime: &Runtime, id: &str) -> ansa::runtime::ForkSeed {
    runtime
        .post(command(runtime, id, CommandPayload::GetContext))
        .await
        .unwrap();
    loop {
        if let EventPayload::ContextSnapshot { command, seed } =
            tokio::time::timeout(Duration::from_secs(5), runtime.next_event())
                .await
                .unwrap()
                .unwrap()
                .payload
            && command.0 == id
        {
            return seed;
        }
    }
}
async fn complete(runtime: &Runtime, id: &str, text: &str) {
    runtime
        .post(command(
            runtime,
            id,
            CommandPayload::Submit(Submit {
                text: text.into(),
                mode: AgentMode::Basic,
            }),
        ))
        .await
        .unwrap();
    loop {
        if let EventPayload::FrontendSnapshot(v) =
            tokio::time::timeout(Duration::from_secs(5), runtime.next_event())
                .await
                .unwrap()
                .unwrap()
                .payload
        {
            assert!(!v.storage_failed, "{}", v.diagnostic);
            if v.state == "paused" {
                panic!("paused: {}", v.diagnostic);
            }
            if v.state == "completed" && !v.worker_busy {
                return;
            }
        }
    }
}
#[tokio::test(flavor = "current_thread")]
async fn pre_edit_catalog_resumes_and_forks_without_upgrading_saved_tools_or_context() {
    use ansa::{
        config::agent_epoch,
        frontend::offline,
        provider::request::{Binding, Request},
        runtime::{Limits, Options},
        tools::{
            files::local_tools,
            registry::{Policy, Registry},
        },
    };
    use std::{collections::BTreeMap, path::Path, sync::Arc};
    fn parts(directory: &Path) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter_map(|path| {
                let name = path.file_name()?.to_str()?;
                (name.starts_with("part-") && name.ends_with(".log"))
                    .then(|| (name.to_owned(), std::fs::read(&path).unwrap()))
            })
            .collect()
    }
    let fixture = Fixture::new();
    let factory = fixture.factory();
    let selected = factory
        .settings
        .catalog("offline")
        .unwrap()
        .create_kind("main", None)
        .unwrap();
    let epoch = agent_epoch(&factory.settings).unwrap();
    let current = local_tools(
        factory.settings.clone(),
        epoch.clone(),
        &selected.directory,
        &factory.auth_path,
        &std::env::current_exe().unwrap(),
    )
    .unwrap();
    // Reconstruct the compiled pre-edit catalogue using the unchanged existing
    // descriptors and the same durable/read-only policies as before edit_file.
    let mut legacy = Registry::new();
    for descriptor in &current.advertisement().descriptors {
        if ![
            "list_directory",
            "read_file",
            "search_files",
            "run_command",
            "get_test_value",
        ]
        .contains(&descriptor.name.as_str())
        {
            continue;
        }
        legacy
            .register_with_policy(
                current.definition(&descriptor.name).unwrap().clone(),
                Policy {
                    requires_journal: descriptor.name == "run_command",
                    read_only: descriptor.name != "run_command",
                    ..Policy::default()
                },
            )
            .unwrap();
    }
    let legacy = legacy.snapshot();
    let configuration = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: factory.settings.model.clone(),
            returned_model: factory.settings.returned_model.clone(),
        },
        epoch,
        legacy.advertisement().clone(),
        Vec::new(),
    )
    .unwrap();
    let mut original = Runtime::new(
        selected.id.as_str().into(),
        configuration,
        legacy,
        Arc::new(|request, stop, _| offline::generate(request, stop)),
        Limits {
            tool_rounds: factory.settings.tool_rounds,
            ..Limits::default()
        },
        Options {
            directory: Some(selected.directory.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    original.ready().await.unwrap();
    complete(&original, "old_turn", "old catalogue message").await;
    let expected = context(&original, "old_context").await.request().unwrap();
    assert!(!expected.tools.descriptors.iter().any(|d| {
        [
            "edit_file",
            "workspace_describe",
            "editor_get_context",
            "dataset_inspect",
            "python_execute",
        ]
        .contains(&d.name.as_str())
    }));
    assert!(
        expected
            .input
            .iter()
            .any(|item| item.has("encrypted_content"))
    );
    original.shutdown().await.unwrap();
    let journal_before = parts(&selected.directory);
    assert!(!journal_before.is_empty());

    let mut resumed = factory.resume(&selected.id).unwrap();
    resumed.runtime.ready().await.unwrap();
    let seed = context(&resumed.runtime, "resumed_old").await;
    let actual = seed.request().unwrap();
    assert_eq!(actual.input, expected.input);
    assert_eq!(actual.tools, expected.tools);
    assert_eq!(actual.epoch, expected.epoch);
    assert_eq!(actual.binding, expected.binding);
    assert_eq!(
        parts(&selected.directory),
        journal_before,
        "reopen rewrote the canonical journal"
    );

    let mut fork = factory.create(Kind::Fork, Some(seed)).unwrap();
    fork.runtime.ready().await.unwrap();
    let forked = context(&fork.runtime, "forked_old")
        .await
        .request()
        .unwrap();
    assert_eq!(forked.tools, expected.tools);
    assert_eq!(forked.input, expected.input);
    assert_eq!(forked.epoch, expected.epoch);
    complete(
        &fork.runtime,
        "fork_turn",
        "continue with the saved catalogue",
    )
    .await;
    assert_eq!(
        context(&fork.runtime, "fork_after_turn")
            .await
            .request()
            .unwrap()
            .tools,
        expected.tools
    );

    let mut fresh = factory.create(Kind::Main, None).unwrap();
    fresh.runtime.ready().await.unwrap();
    assert!(
        context(&fresh.runtime, "fresh_tools")
            .await
            .request()
            .unwrap()
            .tools
            .descriptors
            .iter()
            .any(|descriptor| descriptor.name == "edit_file")
    );
    fresh.runtime.shutdown().await.unwrap();
    fork.runtime.shutdown().await.unwrap();
    resumed.runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn fork_keeps_native_context_and_independent_history_across_restart() {
    let fixture = Fixture::new();
    let factory = fixture.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    complete(&main.runtime, "first", "parent message").await;
    let original = context(&main.runtime, "source").await.request().unwrap();
    let mut fork = factory
        .create(Kind::Fork, Some(context(&main.runtime, "fork").await))
        .unwrap();
    fork.runtime.ready().await.unwrap();
    let inherited = context(&fork.runtime, "child_context")
        .await
        .request()
        .unwrap();
    assert_eq!(original.input, inherited.input);
    complete(&fork.runtime, "second", "child-only message").await;
    assert_eq!(
        original.input,
        context(&main.runtime, "parent_again")
            .await
            .request()
            .unwrap()
            .input
    );
    let child = context(&fork.runtime, "before_restart")
        .await
        .request()
        .unwrap();
    let id = fork.selected.id.clone();
    fork.runtime.shutdown().await.unwrap();
    let mut reopened = factory.resume(&id).unwrap();
    reopened.runtime.ready().await.unwrap();
    assert_eq!(
        child.input,
        context(&reopened.runtime, "after_restart")
            .await
            .request()
            .unwrap()
            .input
    );
    reopened.runtime.shutdown().await.unwrap();
    main.runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn side_conversation_enforces_readonly_and_does_not_replace_continue_target() {
    let fixture = Fixture::new();
    let factory = fixture.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    let mut side = factory
        .create(Kind::Btw, Some(context(&main.runtime, "side").await))
        .unwrap();
    side.runtime.ready().await.unwrap();
    let seed = context(&side.runtime, "readonly").await;
    assert!(
        !seed
            .request()
            .unwrap()
            .tools
            .descriptors
            .iter()
            .any(|d| d.name == "run_command")
    );
    assert!(
        seed.request()
            .unwrap()
            .tools
            .descriptors
            .iter()
            .any(|d| d.name == "read_file")
    );
    let catalog = factory.settings.catalog("offline").unwrap();
    assert_eq!(catalog.latest().unwrap().id, main.selected.id);
    let expected = seed.request().unwrap();
    let mut fork = factory.create(Kind::Fork, Some(seed)).unwrap();
    fork.runtime.ready().await.unwrap();
    let forked = context(&fork.runtime, "fork_readonly")
        .await
        .request()
        .unwrap();
    assert_eq!(forked.input, expected.input);
    assert_eq!(forked.tools, expected.tools);
    assert_eq!(forked.epoch, expected.epoch);
    fork.runtime.shutdown().await.unwrap();
    assert_eq!(catalog.latest().unwrap().id, fork.selected.id);
    assert_eq!(
        factory
            .entries()
            .unwrap()
            .iter()
            .find(|e| e.id == side.selected.id)
            .unwrap()
            .parent
            .as_deref(),
        Some(main.selected.id.as_str())
    );
    side.runtime.shutdown().await.unwrap();
    main.runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fork_of_plan_turn_keeps_its_actual_tool_snapshot() {
    let fixture = Fixture::new();
    let factory = fixture.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    main.runtime
        .post(command(
            &main.runtime,
            "plan",
            CommandPayload::Submit(Submit {
                text: "inspect only".into(),
                mode: AgentMode::Plan,
            }),
        ))
        .await
        .unwrap();
    loop {
        if let EventPayload::FrontendSnapshot(v) =
            tokio::time::timeout(Duration::from_secs(5), main.runtime.next_event())
                .await
                .unwrap()
                .unwrap()
                .payload
        {
            assert_ne!(v.state, "paused", "{}", v.diagnostic);
            if v.state == "completed" {
                break;
            }
        }
    }
    let source = context(&main.runtime, "plan_seed").await;
    let expected = source.request().unwrap();
    let mut fork = factory.create(Kind::Fork, Some(source)).unwrap();
    fork.runtime.ready().await.unwrap();
    let actual = context(&fork.runtime, "fork_seed").await.request().unwrap();
    assert_eq!(actual.tools, expected.tools);
    assert_eq!(actual.input, expected.input);
    assert!(
        !actual
            .tools
            .descriptors
            .iter()
            .any(|d| d.name == "run_command")
    );
    fork.runtime.shutdown().await.unwrap();
    main.runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn local_console_context_is_not_attributed_or_forked_to_the_model_account() {
    let fixture = Fixture::new();
    let factory = fixture.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    let mut console = factory.create_console(&main.selected.id).unwrap();
    console.runtime.ready().await.unwrap();
    let seed = context(&console.runtime, "console_context").await;
    assert_eq!(
        seed.request().unwrap().binding.account,
        "ansa-local-console"
    );
    assert_eq!(
        seed.request().unwrap().binding.requested_model,
        "local-console"
    );
    assert!(matches!(
        factory.create(Kind::Fork, Some(seed)),
        Err("session_provider_context")
    ));
    let id = console.selected.id.clone();
    console.runtime.shutdown().await.unwrap();
    let index = console.selected.directory.join("info.json");
    let derived =
        ansa::protocol::json::Json::parse(&std::fs::read_to_string(&index).unwrap()).unwrap();
    let derived = derived
        .with(
            "parent",
            ansa::protocol::json::Json::string("stale_derived_parent").unwrap(),
        )
        .unwrap();
    std::fs::write(&index, derived.dump()).unwrap();
    let mut reopened = factory.resume(&id).unwrap();
    reopened.runtime.ready().await.unwrap();
    assert_eq!(reopened.parent.as_deref(), Some(main.selected.id.as_str()));
    assert_eq!(
        context(&reopened.runtime, "reopened_console")
            .await
            .request()
            .unwrap()
            .binding
            .account,
        "ansa-local-console"
    );
    reopened.runtime.shutdown().await.unwrap();
    main.runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fork_does_not_silently_rebind_workspace_or_initial_instructions() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let factory = fixture.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    main.runtime.ready().await.unwrap();
    let seed = context(&main.runtime, "original_context").await;
    assert!(matches!(
        other.factory().create(Kind::Fork, Some(seed.clone())),
        Err("session_configuration_mismatch")
    ));
    std::fs::write(
        factory.settings.cwd.join("AGENTS.md"),
        "Changed initial instructions\n",
    )
    .unwrap();
    assert!(matches!(
        factory.create(Kind::Fork, Some(seed)),
        Err("session_configuration_mismatch")
    ));
    main.runtime.shutdown().await.unwrap();
}
