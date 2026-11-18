use ansa::terminal_host::{
    FileJournal, Host, OperationStatus,
    native::{NativeBackend, Screen},
    protocol::{Action, Binding, Effect, Id, Intent, Launch, Size, Writer},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-terminal-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        ansa_linux::fs::create_private_leaf(&path).expect("fresh fixture root");
        fs::create_dir(path.join("workspace")).unwrap();
        Self(path)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(value: &str) -> Id {
    Id::try_from(value.to_owned()).unwrap()
}

type AttachedHost = Host<FileJournal, NativeBackend>;

struct Fixture {
    host: AttachedHost,
    root: Sandbox,
    binding: Binding,
}

impl Fixture {
    fn new() -> Self {
        let root = Sandbox::new();
        let binding = Binding {
            session: id("fixture_session"),
            scope: id("fixture_scope"),
            host: id("fixture_host"),
            workspace: root.0.join("workspace").to_str().unwrap().into(),
        };
        let backend = NativeBackend::new(
            PathBuf::from(env!("CARGO_BIN_EXE_ansa")),
            root.0.join("workspace_registry"),
        );
        let host = Host::create_file(&root.0.join("store"), binding.clone(), backend).unwrap();
        Self {
            host,
            root,
            binding,
        }
    }

    fn intent(
        &self,
        name: &str,
        terminal: &Id,
        writer: Writer,
        epoch: u64,
        action: Action,
    ) -> Intent {
        Intent {
            operation: id(name),
            terminal: terminal.clone(),
            binding: self.binding.clone(),
            writer,
            epoch,
            invocation_digest: (writer == Writer::Agent)
                .then(|| ansa::protocol::legacy_json_v1::sha256(name.as_bytes())),
            action,
        }
    }

    fn shell(&mut self, name: &str, cwd: String, expected: Effect) -> Id {
        let terminal = self.host.terminal_id(&id(name));
        let intent = self.intent(
            name,
            &terminal,
            Writer::Agent,
            0,
            Action::Create {
                launch: Launch {
                    label: "Shell".into(),
                    cwd,
                    argv: ["bash", "--noprofile", "--norc", "-i"]
                        .map(str::to_owned)
                        .to_vec(),
                    size: Size {
                        rows: 24,
                        columns: 80,
                    },
                    lifetime_seconds: 30,
                    idle_seconds: 20,
                },
            },
        );
        assert_eq!(
            self.host.execute(Writer::Agent, intent, &[]),
            Ok(OperationStatus::Durable(expected))
        );
        terminal
    }

    fn input(&mut self, name: &str, terminal: &Id, writer: Writer, epoch: u64, input: &[u8]) {
        let intent = self.intent(
            name,
            terminal,
            writer,
            epoch,
            Action::input(writer, input).unwrap(),
        );
        assert_eq!(
            self.host.execute(writer, intent, input),
            Ok(OperationStatus::Durable(Effect::Delivered {
                count: input.len()
            }))
        );
    }

    fn screen(
        &mut self,
        terminal: &Id,
        writer: Writer,
        epoch: u64,
        after: u64,
        matches: impl Fn(&Screen) -> bool,
    ) -> Screen {
        let until = Instant::now() + Duration::from_secs(4);
        loop {
            let screen = self.host.capture(writer, terminal, epoch, after).unwrap();
            if matches(&screen) {
                return screen;
            }
            assert!(Instant::now() < until, "latest screen {screen:?}");
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn action(
        &mut self,
        name: &str,
        terminal: &Id,
        writer: Writer,
        epoch: u64,
        action: Action,
    ) -> OperationStatus {
        let intent = self.intent(name, terminal, writer, epoch, action);
        self.host.execute(writer, intent, &[]).unwrap()
    }
}

#[test]
fn workspace_claim_releases_only_after_known_durable_close() {
    let mut fx = Fixture::new();
    let peer_binding = Binding {
        scope: id("peer_scope"),
        host: id("peer_host"),
        ..fx.binding.clone()
    };
    let mut peer = Host::create_file(
        &fx.root.0.join("peer"),
        peer_binding.clone(),
        NativeBackend::new(
            PathBuf::from(env!("CARGO_BIN_EXE_ansa")),
            fx.root.0.join("workspace_registry"),
        ),
    )
    .unwrap();
    let registry = fx.root.0.join("workspace_registry");
    let workspace = fx.root.0.join("workspace");
    let other_workspace = fx.root.0.join("other");
    let nested = workspace.join("nested");
    fs::create_dir(&other_workspace).unwrap();
    fs::create_dir(&nested).unwrap();
    let admit = || ansa_linux::local::WorkspaceWrite::acquire(&registry, &other_workspace, &nested);

    let mut known_writer = admit().unwrap();
    known_writer.begin(&"a".repeat(64)).unwrap();
    fx.shell(
        "while_writer",
        fx.binding.workspace.clone(),
        Effect::NotStarted,
    );
    known_writer.finish().unwrap();
    let terminal = fx.shell("first", fx.binding.workspace.clone(), Effect::Applied);
    assert!(matches!(admit(), Err("terminal_workspace_busy")));
    assert!(matches!(
        ansa_linux::local::workspace_participant(&registry, &nested),
        Err("terminal_workspace_busy"),
    ));
    assert!(matches!(
        ansa_linux::local::workspace_participant(&registry, &fx.root.0),
        Err("terminal_workspace_busy"),
    ));
    let operation = |host: &AttachedHost, name: &str| {
        let operation = id(name);
        Intent {
            terminal: host.terminal_id(&operation),
            operation,
            binding: peer_binding.clone(),
            writer: Writer::Agent,
            epoch: 0,
            invocation_digest: Some(ansa::protocol::legacy_json_v1::sha256(name.as_bytes())),
            action: Action::Create {
                launch: Launch {
                    label: "peer".into(),
                    cwd: peer_binding.workspace.clone(),
                    argv: vec!["sleep".into(), "30".into()],
                    size: Size {
                        rows: 24,
                        columns: 80,
                    },
                    lifetime_seconds: 30,
                    idle_seconds: 20,
                },
            },
        }
    };
    assert_eq!(
        peer.execute(Writer::Agent, operation(&peer, "overlap"), &[]),
        Ok(OperationStatus::Durable(Effect::NotStarted))
    );
    assert_eq!(
        fx.action("close", &terminal, Writer::Agent, 1, Action::Close),
        OperationStatus::Durable(Effect::Applied)
    );
    let mut interrupted_writer = admit().unwrap();
    interrupted_writer.begin(&"b".repeat(64)).unwrap();
    let records = interrupted_writer.records().collect::<Vec<_>>();
    drop(interrupted_writer);
    assert_eq!(
        peer.execute(Writer::Agent, operation(&peer, "after_writer_loss"), &[]),
        Ok(OperationStatus::Durable(Effect::NotStarted))
    );
    assert!(matches!(
        ansa_linux::local::workspace_participant(&registry, &workspace),
        Err("terminal_workspace_quarantined"),
    ));
    // Reconcile the synthetic known result before exercising terminal-host loss.
    for (identity, root) in records {
        ansa_linux::local::reconcile_writer(&registry, &"b".repeat(64), identity, root).unwrap();
    }
    let nested_plan = ansa_linux::local::workspace_participant(&registry, &nested).unwrap();
    let second_plan = ansa_linux::local::workspace_participant(&registry, &nested).unwrap();
    assert_eq!(
        peer.execute(Writer::Agent, operation(&peer, "while_nested_plan"), &[]),
        Ok(OperationStatus::Durable(Effect::NotStarted))
    );
    drop(second_plan);
    drop(nested_plan);
    assert_eq!(
        peer.execute(Writer::Agent, operation(&peer, "after_close"), &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    drop(peer);
    // Losing the original process owner releases the flock only. The active
    // marker still blocks a replacement from assuming files are reconciled.
    fx.shell(
        "after_loss",
        fx.binding.workspace.clone(),
        Effect::NotStarted,
    );
}

#[test]
fn plan_admission_holds_the_workspace_across_generation() {
    use ansa::{
        frontend::{
            app::{binding, pure_tools},
            offline,
        },
        protocol::json::Json,
        provider::request::{Epoch, Request},
        runtime::{
            AgentMode, CommandPayload, EventPayload, Limits, Options, Runtime, RuntimeCommand,
            Submit,
        },
    };
    use std::sync::{Arc, atomic::AtomicBool};
    let mut fixture = Fixture::new();
    let terminal = fixture.shell(
        "initial",
        fixture.binding.workspace.clone(),
        Effect::Applied,
    );
    let registry = fixture.root.0.join("workspace_registry");
    let workspace = fixture.root.0.join("workspace");
    let tools = pure_tools().unwrap();
    let configuration = Request::new(
        binding(),
        Epoch {
            id: "fixture".into(),
            instructions: "test".into(),
            exact_state: Json::parse("{}").unwrap(),
        },
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let options = Options {
        plan_guard: Some(Arc::new(move || {
            Ok(Box::new(ansa_linux::local::workspace_participant(
                &registry, &workspace,
            )?))
        })),
        ..Options::default()
    };
    let enabled = Arc::new(AtomicBool::new(false));
    let generating = Arc::new(AtomicBool::new(false));
    let finish_probe = Arc::new(AtomicBool::new(false));
    let allowed = enabled.clone();
    let started = generating.clone();
    let continue_model = finish_probe.clone();
    let generate = Arc::new(
        move |request: &Request, stop: &ansa::runtime::Cancel, _: ansa::runtime::PreviewSink| {
            assert!(
                allowed.load(Ordering::SeqCst),
                "Plan reached the provider before workspace admission"
            );
            started.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(3);
            while !continue_model.load(Ordering::SeqCst) {
                if Instant::now() >= until {
                    return Err("fixture_plan_timeout");
                }
                thread::sleep(Duration::from_millis(5));
            }
            offline::generate(request, stop)
        },
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut runtime = Runtime::new(
                "plan_fixture".into(),
                configuration,
                tools,
                generate,
                Limits::default(),
                options,
            )
            .unwrap();
            let handle = runtime.handle();
            handle.ready().await.unwrap();
            let submit = |id: &str| RuntimeCommand {
                schema_version: 1,
                session: handle.session().clone(),
                incarnation: handle.incarnation().clone(),
                command: id.into(),
                turn: id.into(),
                payload: CommandPayload::Submit(Submit {
                    text: "Inspect scope".into(),
                    mode: AgentMode::Plan,
                }),
            };
            handle.post(submit("blocked")).await.unwrap();
            tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    match handle.next_event().await.unwrap().payload {
                        EventPayload::RuntimeError { diagnostic, .. }
                            if diagnostic == "terminal_workspace_busy" =>
                        {
                            break;
                        }
                        EventPayload::CommandAccepted { .. } => {
                            panic!("Plan was accepted while a terminal held its workspace")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert!(!generating.load(Ordering::SeqCst));
            assert_eq!(
                fixture.action("close", &terminal, Writer::Agent, 1, Action::Close),
                OperationStatus::Durable(Effect::Applied)
            );
            enabled.store(true, Ordering::SeqCst);
            handle.post(submit("blocked")).await.unwrap();
            tokio::time::timeout(Duration::from_secs(4), async {
                while !generating.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            fixture.shell(
                "overlap_plan",
                fixture.binding.workspace.clone(),
                Effect::NotStarted,
            );
            finish_probe.store(true, Ordering::SeqCst);
            tokio::time::timeout(Duration::from_secs(4), async {
                while !matches!(
                    handle.next_event().await.unwrap().payload,
                    EventPayload::Done
                ) {}
            })
            .await
            .unwrap();
            runtime.shutdown().await.unwrap();
        });
    let terminal = fixture.shell(
        "after_plan",
        fixture.binding.workspace.clone(),
        Effect::Applied,
    );
    assert_eq!(
        fixture.action("finish", &terminal, Writer::Agent, 1, Action::Close),
        OperationStatus::Durable(Effect::Applied)
    );
}

#[test]
fn worker_preserves_tty_state_and_transfers_only_an_approved_screen() {
    let mut fx = Fixture::new();
    let terminal = fx.shell("launch", fx.binding.workspace.clone(), Effect::Applied);
    fx.input(
        "setup",
        &terminal,
        Writer::Agent,
        1,
        b"PS1='ansa-fixture> '; mkdir -p sub; cd sub; export ANSA_PTY_TEST=kept; test -t 0 && printf '%s%s\n' TTY_ OK\r",
    );
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| {
        s.text.contains("TTY_OK")
    });
    fx.input(
        "verify",
        &terminal,
        Writer::Agent,
        1,
        b"printf '%s:%s\n' \"$ANSA_PTY_TEST\" \"${PWD##*/}\"\r",
    );
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| {
        s.text.contains("kept:sub")
    });
    let output = fx
        .host
        .read_output(Writer::Agent, &terminal, 1, 0, 8192)
        .unwrap();
    assert!(output.text.contains("kept:sub") && !output.gap && output.through > output.from);
    let size = Size {
        rows: 26,
        columns: 100,
    };
    assert_eq!(
        fx.action(
            "resize",
            &terminal,
            Writer::Agent,
            1,
            Action::Resize { size }
        ),
        OperationStatus::Durable(Effect::Applied)
    );
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| s.size == size);
    assert_eq!(
        fx.action("takeover", &terminal, Writer::Human, 1, Action::Takeover),
        OperationStatus::Durable(Effect::Applied)
    );
    fx.input(
        "private",
        &terminal,
        Writer::Human,
        2,
        b"printf '%s%s\n' HUMAN_ INPUT\r",
    );
    let private = fx.screen(&terminal, Writer::Human, 2, 0, |s| {
        s.text.contains("HUMAN_INPUT")
    });
    assert!(fx.host.capture(Writer::Agent, &terminal, 2, 0).is_err());
    fx.input(
        "changed",
        &terminal,
        Writer::Human,
        2,
        b"printf '%s%s\n' SCREEN_ CHANGED\r",
    );
    let approved = fx
        .screen(&terminal, Writer::Human, 2, 0, |s| {
            s.text.contains("SCREEN_CHANGED") && s.text.trim_end().ends_with("ansa-fixture>")
        })
        .approval()
        .unwrap();
    assert_eq!(
        fx.action(
            "old_handback",
            &terminal,
            Writer::Human,
            2,
            Action::Handback {
                screen: private.approval().unwrap()
            }
        ),
        OperationStatus::Durable(Effect::NotStarted)
    );
    assert_eq!(
        fx.action(
            "handback",
            &terminal,
            Writer::Human,
            2,
            Action::Handback {
                screen: approved.clone()
            }
        ),
        OperationStatus::Durable(Effect::Applied)
    );
    assert!(
        fx.host
            .capture(Writer::Agent, &terminal, 3, approved.through - 1)
            .is_err()
    );
    assert!(
        fx.host
            .read_output(Writer::Agent, &terminal, 3, approved.through - 1, 8192)
            .is_err()
    );
    fx.screen(&terminal, Writer::Agent, 3, approved.through, |s| {
        s.text.contains("SCREEN_CHANGED")
    });
    assert_eq!(
        fx.action("close", &terminal, Writer::Agent, 3, Action::Close),
        OperationStatus::Durable(Effect::Applied)
    );
    assert!(fx.root.0.join("workspace/sub").is_dir());
}

#[test]
fn worker_distinguishes_raw_ctrl_c_from_foreground_interrupt() {
    let mut fx = Fixture::new();
    let terminal = fx.shell("launch", fx.binding.workspace.clone(), Effect::Applied);
    fx.input("raw", &terminal, Writer::Agent, 1, b"stty -icanon -isig -echo min 1 time 0; printf '%s%s\n' RAW_ READY; dd bs=1 count=1 of=raw_byte 2>/dev/null; stty sane; printf '%s%s\n' RAW_ DONE\r");
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| {
        s.text.contains("RAW_READY")
    });
    fx.input("key", &terminal, Writer::Agent, 1, &[3]);
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| {
        s.text.contains("RAW_DONE")
    });
    assert_eq!(fs::read(fx.root.0.join("workspace/raw_byte")).unwrap(), [3]);
    fx.input(
        "foreground",
        &terminal,
        Writer::Agent,
        1,
        b"sh -c 'printf ready > foreground_ready; exec sleep 30'\r",
    );
    let until = Instant::now() + Duration::from_secs(3);
    while !fx.root.0.join("workspace/foreground_ready").exists() {
        assert!(Instant::now() < until, "foreground program started");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fx.action("interrupt", &terminal, Writer::Agent, 1, Action::Interrupt),
        OperationStatus::Durable(Effect::Applied)
    );
    fx.input(
        "check",
        &terminal,
        Writer::Agent,
        1,
        b"printf '%s%s\n' FOREGROUND_ INTERRUPTED\r",
    );
    fx.screen(&terminal, Writer::Agent, 1, 0, |s| {
        s.text.contains("FOREGROUND_INTERRUPTED")
    });
    assert_eq!(
        fx.action("close", &terminal, Writer::Agent, 1, Action::Close),
        OperationStatus::Durable(Effect::Applied)
    );
}

#[test]
fn worker_retains_output_after_program_exit_and_deadline() {
    use ansa::terminal_host::native::EndReason;
    let mut fx = Fixture::new();
    for (index, reason, script, exit_code) in [
        (
            0,
            EndReason::ProgramExited,
            "printf 'FINAL_LINE\\n'; exit 7",
            Some(7),
        ),
        (
            1,
            EndReason::Deadline,
            "printf 'FINAL_LINE\\n'; exec sleep 30",
            None,
        ),
    ] {
        let operation = format!("open_{index}");
        let terminal = fx.host.terminal_id(&id(&operation));
        let action = Action::Create {
            launch: Launch {
                label: "short command".into(),
                cwd: fx.binding.workspace.clone(),
                argv: vec!["sh".into(), "-c".into(), script.into()],
                size: Size {
                    rows: 24,
                    columns: 80,
                },
                lifetime_seconds: 1,
                idle_seconds: 1,
            },
        };
        assert_eq!(
            fx.action(&operation, &terminal, Writer::Agent, 0, action),
            OperationStatus::Durable(Effect::Applied)
        );
        let final_screen = fx.screen(&terminal, Writer::Agent, 1, 0, |s| s.exit.is_some());
        let exit = final_screen.exit.unwrap();
        assert_eq!(exit.reason, reason);
        assert_eq!(exit.status, exit_code);
        assert!(exit.cleanup_known && final_screen.text.contains("FINAL_LINE"));
        assert_eq!(fx.host.worker_status(&terminal), Ok(Some(exit.clone())));
        let output = fx
            .host
            .read_output(Writer::Agent, &terminal, 1, 0, 1024)
            .unwrap();
        assert_eq!(output.exit, Some(exit));
        assert!(output.text.contains("FINAL_LINE"));
        assert_eq!(
            fx.action(
                &format!("close_{index}"),
                &terminal,
                Writer::Agent,
                1,
                Action::Close
            ),
            OperationStatus::Durable(Effect::Applied)
        );
    }
}

#[test]
fn detached_daemon_reconnects_to_the_same_shell_and_fences_old_writers() {
    use ansa::terminal_host::daemon::{Client, Daemon, Endpoint};
    let fx = Fixture::new();
    let directory = fx.root.0.join("daemon");
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_ansa"));
    let mut daemon = Daemon::launch(
        &executable,
        &directory,
        &fx.root.0.join("workspace_registry"),
        fx.binding.clone(),
        10,
    )
    .unwrap();
    let mut agent = daemon.attach(Writer::Agent, true).unwrap();
    let old_connection = agent.connection();
    daemon.endpoint().remember(&fx.root.0).unwrap();
    assert!(daemon.attach(Writer::Agent, true).is_err());
    let mut reader = daemon.attach(Writer::Agent, false).unwrap();
    let terminal = fx.binding.terminal_id(&id("open"));
    let open = fx.intent(
        "open",
        &terminal,
        Writer::Agent,
        0,
        Action::Create {
            launch: Launch {
                label: "Shell".into(),
                cwd: fx.binding.workspace.clone(),
                argv: ["bash", "--noprofile", "--norc", "-i"]
                    .map(str::to_owned)
                    .to_vec(),
                size: Size {
                    rows: 24,
                    columns: 80,
                },
                lifetime_seconds: 30,
                idle_seconds: 20,
            },
        },
    );
    assert!(reader.execute(open.clone(), Vec::new()).is_err());
    assert_eq!(
        agent.execute(open, Vec::new()).unwrap(),
        OperationStatus::Durable(Effect::Applied)
    );
    let input = b"export KEEP=original; printf '%s' \"$$\" > same.pid\r";
    let write = fx.intent(
        "write",
        &terminal,
        Writer::Agent,
        1,
        Action::input(Writer::Agent, input).unwrap(),
    );
    assert_eq!(
        agent.execute(write.clone(), input.to_vec()).unwrap(),
        OperationStatus::Durable(Effect::Delivered { count: input.len() })
    );
    drop(agent);
    drop(reader);
    let endpoint = Endpoint::recall(&fx.root.0, &fx.binding).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let mut agent = loop {
        if let Ok(client) = Client::attach(&endpoint, Writer::Agent, true) {
            break client;
        }
        assert!(
            Instant::now() < until,
            "writer lease released on disconnect"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_ne!(agent.connection(), old_connection);
    let epoch = agent.attached_epoch(&terminal).unwrap();
    assert_eq!(epoch, 2);
    let terminals = agent.terminals().unwrap();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].id, terminal);
    assert_eq!(terminals[0].epoch, epoch);
    assert_eq!(terminals[0].label, "Shell");
    assert_eq!(
        agent.operation(id("write")).unwrap(),
        Some(OperationStatus::Durable(Effect::Delivered {
            count: input.len()
        }))
    );
    let stale = fx.intent(
        "queued_before_disconnect",
        &terminal,
        Writer::Agent,
        1,
        Action::input(Writer::Agent, b"false\r").unwrap(),
    );
    assert!(agent.execute(stale, b"false\r".to_vec()).is_err());
    let input = b"printf '%s %s' \"$KEEP\" \"$$\" > reattached.txt\r";
    let verify = fx.intent(
        "verify",
        &terminal,
        Writer::Agent,
        epoch,
        Action::input(Writer::Agent, input).unwrap(),
    );
    agent.execute(verify, input.to_vec()).unwrap();
    let proof = fx.root.0.join("workspace/reattached.txt");
    let content = loop {
        if let Ok(text) = fs::read_to_string(&proof)
            && text.split_whitespace().count() == 2
        {
            break text;
        }
        assert!(Instant::now() < until, "reattached command ran");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        content,
        format!(
            "original {}",
            fs::read_to_string(fx.root.0.join("workspace/same.pid")).unwrap()
        )
    );
    let mut human = daemon.attach(Writer::Human, true).unwrap();
    let take = fx.intent("take", &terminal, Writer::Human, epoch, Action::Takeover);
    assert!(agent.execute(take.clone(), vec![]).is_err());
    assert_eq!(
        human.execute(take, vec![]).unwrap(),
        OperationStatus::Durable(Effect::Applied)
    );
    assert!(agent.capture(terminal.clone(), epoch + 1, 0).is_err());
    let keys = b"PS1='handoff> '; printf '%s%s\\n' PRIVATE_ DAEMON; printf '\\033[2J\\033[H'; printf '%s%s\\n' PUBLIC_ DAEMON\r";
    let input = fx.intent(
        "human_keys",
        &terminal,
        Writer::Human,
        epoch + 1,
        Action::input(Writer::Human, keys).unwrap(),
    );
    assert_eq!(
        human.execute(input, keys.to_vec()).unwrap(),
        OperationStatus::Durable(Effect::Delivered { count: keys.len() })
    );
    let until = Instant::now() + Duration::from_secs(3);
    let screen = loop {
        let screen = human.capture(terminal.clone(), epoch + 1, 0).unwrap();
        if screen.text.contains("PUBLIC_DAEMON") && screen.text.trim_end().ends_with("handoff>") {
            break screen;
        }
        assert!(Instant::now() < until, "{screen:?}");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!screen.text.contains("PRIVATE_DAEMON"));
    let approved = screen.approval().unwrap();
    let handback = fx.intent(
        "handback",
        &terminal,
        Writer::Human,
        epoch + 1,
        Action::Handback {
            screen: approved.clone(),
        },
    );
    assert_eq!(
        human.execute(handback, Vec::new()).unwrap(),
        OperationStatus::Durable(Effect::Applied)
    );
    assert!(
        agent
            .read(terminal.clone(), epoch + 2, approved.through - 1, 8192)
            .is_err()
    );
    let public = agent
        .capture(terminal, epoch + 2, approved.through)
        .unwrap();
    assert!(public.text.contains("PUBLIC_DAEMON"));
    assert!(!public.text.contains("PRIVATE_DAEMON"));
    assert!(human.shutdown().unwrap());
    assert!(daemon.wait(Duration::from_secs(3)).unwrap());
    let restored = Host::restore_file(
        &directory.join("evidence"),
        fx.binding.clone(),
        NativeBackend::new(executable.clone(), fx.root.0.join("workspace_registry")),
    )
    .unwrap();
    assert!(restored.recovery().unwrap().closure_known);
    drop(restored);
    assert!(Client::attach(&endpoint, Writer::Agent, true).is_err());
    assert!(
        Daemon::launch(
            &executable,
            &directory,
            &fx.root.0.join("workspace_registry"),
            fx.binding.clone(),
            10
        )
        .is_err()
    );
    fs::rename(&directory, fx.root.0.join("original_daemon")).unwrap();
    let mut replacement = Daemon::launch(
        &executable,
        &directory,
        &fx.root.0.join("workspace_registry"),
        fx.binding.clone(),
        1,
    )
    .unwrap();
    assert!(replacement.endpoint().remember(&fx.root.0).is_err());
    assert!(Client::attach(&endpoint, Writer::Agent, true).is_err());
    assert!(replacement.wait(Duration::from_secs(3)).unwrap());

    let blocked = fx.root.0.join("blocked_seal");
    let mut bad_seal = Daemon::launch(
        &executable,
        &blocked,
        &fx.root.0.join("workspace_registry"),
        fx.binding.clone(),
        10,
    )
    .unwrap();
    fs::write(blocked.join("evidence/closed-v1.json"), b"occupied").unwrap();
    let mut reader = bad_seal.attach(Writer::Human, true).unwrap();
    assert!(!reader.shutdown().unwrap());
    assert!(!bad_seal.wait(Duration::from_secs(3)).unwrap());
}

#[test]
fn worker_cleans_background_processes_that_left_the_shell_session() {
    use std::os::unix::fs::symlink;
    let mut fx = Fixture::new();
    let outside = fx.root.0.join("outside");
    fs::create_dir(&outside).unwrap();
    let link = fx.root.0.join("workspace/link");
    symlink(outside, &link).unwrap();
    fx.shell(
        "deny_link",
        link.to_str().unwrap().into(),
        Effect::NotStarted,
    );
    let terminal = fx.shell("launch", fx.binding.workspace.clone(), Effect::Applied);
    fx.input(
        "background",
        &terminal,
        Writer::Agent,
        1,
        b"setsid sh -c 'echo $$ > detached.pid; exec sleep 60' &\r",
    );
    let marker = fx.root.0.join("workspace/detached.pid");
    let until = Instant::now() + Duration::from_secs(4);
    let pid = loop {
        if let Ok(text) = fs::read_to_string(&marker)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < until,
            "test requires setsid and sleep on PATH"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let proc = PathBuf::from(format!("/proc/{pid}/stat"));
    assert!(proc.is_file(), "detached process alive before close");
    assert_eq!(
        fx.action("close", &terminal, Writer::Agent, 1, Action::Close),
        OperationStatus::Durable(Effect::Applied)
    );
    assert!(!proc.exists(), "detached child was reaped by its worker");
}

#[test]
fn runtime_publishes_session_intent_before_compiled_terminal_calls() {
    use ansa::{
        protocol::json::Json,
        provider::{
            native,
            request::{Binding as ProviderBinding, Epoch, Request},
        },
        runtime::{
            AgentMode, CommandPayload, EventPayload, Limits, Options, Runtime, RuntimeCommand,
            Submit,
        },
        session::journal_v1::Point,
        terminal_host::tools as terminal,
        tools::registry::Registry,
    };
    use serde_json::{Value, json};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    };

    for invalid_batch in [false, true] {
        let fx = Fixture::new();
        let directory = fx.root.0.join("conversation");
        let evidence = fx.root.0.join("store");
        let workspace = fx.root.0.join("workspace");
        let owner = Arc::new(Mutex::new(fx.host));
        let mut registry = Registry::new();
        terminal::register(&mut registry, owner.clone(), &directory).unwrap();
        let tools = registry.snapshot();
        assert!(tools.read_only().advertisement().descriptors.is_empty());
        let configuration = Request::new(
            ProviderBinding {
                provider: "openai-siwc".into(),
                account: "offline".into(),
                requested_model: "fixture".into(),
                returned_model: "fixture".into(),
            },
            Epoch {
                id: "fixture".into(),
                instructions: "Fixture".into(),
                exact_state: Json::parse("{}").unwrap(),
            },
            tools.advertisement().clone(),
            vec![],
        )
        .unwrap();
        let observed_intent = Arc::new(AtomicBool::new(false));
        let observed = observed_intent.clone();
        let first_intent = Arc::new(move |point, kind: &str| {
            if point == Point::AfterSync
                && kind == "intent"
                && !observed.swap(true, Ordering::SeqCst)
            {
                assert!(
                    !evidence.join("0000.json").exists(),
                    "PTY intent preceded session intent"
                );
            }
            Ok(())
        });
        let step = AtomicUsize::new(0);
        let generate = Arc::new(
            move |request: &Request, _: &ansa::runtime::Cancel, _: ansa::runtime::PreviewSink| {
                let index = step.fetch_add(1, Ordering::SeqCst);
                let call = |index: usize, name: &str, arguments: Value| json!({"type":"function_call","id":format!("item_{index}"),"call_id":format!("call_{index}"),"namespace":"omh","name":name,"arguments":arguments.to_string(),"status":"completed"});
                let message = |id: &str, text: &str| json!({"type":"message","id":id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]});
                let receipt = || -> Result<Value, &'static str> {
                    serde_json::from_str(request.input.last().unwrap().text_field("output")?)
                        .map_err(|_| "terminal_fixture_receipt")
                };
                let wait_for = |name: &str| {
                    let until = Instant::now() + Duration::from_secs(2);
                    loop {
                        if let Ok(text) = fs::read_to_string(workspace.join(name))
                            && !text.is_empty()
                        {
                            break text;
                        }
                        assert!(Instant::now() < until, "missing PTY result {name}");
                        thread::sleep(Duration::from_millis(5));
                    }
                };
                let items = match index {
                    0 => {
                        let launch = call(
                            0,
                            "terminal_create",
                            json!({"cwd":".","label":"Shell","argv":["bash","--noprofile","--norc","-i"],"rows":24,"columns":80,"lifetime_seconds":30,"idle_seconds":20}),
                        );
                        if invalid_batch {
                            vec![
                                launch,
                                call(
                                    1,
                                    "terminal_input",
                                    json!({"terminal":"nope","epoch":1,"encoding":"base64","data":"invalid!"}),
                                ),
                            ]
                        } else {
                            vec![launch]
                        }
                    }
                    1 => vec![call(
                        1,
                        "terminal_input",
                        json!({"terminal":receipt()?["terminal"],"epoch":1,"encoding":"utf8","data":"mkdir -p saved; cd saved; export ANSA_FIXTURE=retained; printf '%s' \"$$\" > ../first_pid\r"}),
                    )],
                    2 => {
                        wait_for("first_pid");
                        vec![message("environment_ready", "Окружение готово")]
                    }
                    3 => {
                        assert_eq!(
                            request.input.last().unwrap().text_field("content")?,
                            "Используй это окружение"
                        );
                        vec![call(3, "terminal_list", json!({}))]
                    }
                    4 => {
                        let listing = receipt()?;
                        let entries = listing["terminals"].as_array().unwrap();
                        assert_eq!(entries.len(), 1);
                        let terminal = &entries[0];
                        assert_eq!(terminal["state"], "live");
                        vec![call(
                            4,
                            "terminal_input",
                            json!({"terminal":terminal["terminal"],"epoch":terminal["epoch"],"encoding":"utf8","data":"printf '%s:%s:%s' \"$ANSA_FIXTURE\" \"$PWD\" \"$$\" > ../continued\r"}),
                        )]
                    }
                    5 => {
                        assert_eq!(
                            wait_for("continued"),
                            format!(
                                "retained:{}:{}",
                                workspace.join("saved").display(),
                                wait_for("first_pid")
                            )
                        );
                        let receipt = receipt()?;
                        vec![call(
                            5,
                            "terminal_close",
                            json!({"terminal":receipt["terminal"],"epoch":receipt["epoch"]}),
                        )]
                    }
                    6 => vec![message("finished", "PTY_FINISHED")],
                    _ => return Err("terminal_fixture_unexpected_generation"),
                };
                native::load(&Json::object(vec![
                    ("request", request.save()?),
                    ("id", Json::string(&format!("response_{index}"))?),
                    ("model", Json::string(&request.binding.returned_model)?),
                    ("items", Json::parse(&json!(items).to_string())?),
                ])?)
            },
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                assert!(matches!(
                    Runtime::new(
                        "different_session".into(),
                        configuration.clone(),
                        tools.clone(),
                        generate.clone(),
                        Limits::default(),
                        Options {
                            directory: Some(directory.clone()),
                            ..Options::default()
                        },
                    ),
                    Err("tool_owner_binding")
                ));
                assert!(matches!(
                    Runtime::new(
                        "fixture_session".into(),
                        configuration.clone(),
                        tools.clone(),
                        generate.clone(),
                        Limits::default(),
                        Options {
                            directory: Some(directory.join("incorrect")),
                            ..Options::default()
                        },
                    ),
                    Err("tool_owner_binding")
                ));
                let mut runtime = Runtime::new(
                    "fixture_session".into(),
                    configuration,
                    tools,
                    generate,
                    Limits {
                        tool_rounds: 4,
                        ..Limits::default()
                    },
                    Options {
                        directory: Some(directory),
                        journal: Some(first_intent),
                        ..Options::default()
                    },
                )
                .unwrap();
                let handle = runtime.handle();
                handle.ready().await.unwrap();
                for (id, text) in [
                    ("prepare", "Подготовь окружение"),
                    ("reuse", "Используй это окружение"),
                ]
                .into_iter()
                .take(if invalid_batch { 1 } else { 2 })
                {
                    handle
                        .post(RuntimeCommand {
                            schema_version: 1,
                            session: handle.session().clone(),
                            incarnation: handle.incarnation().clone(),
                            command: id.into(),
                            turn: id.into(),
                            payload: CommandPayload::Submit(Submit {
                                text: text.into(),
                                mode: AgentMode::Basic,
                            }),
                        })
                        .await
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(6), async {
                        loop {
                            match handle.next_event().await.expect("runtime event").payload {
                                EventPayload::Done if !invalid_batch => break,
                                EventPayload::FrontendSnapshot(s) if s.state == "paused" => {
                                    assert!(invalid_batch, "unexpected pause {}", s.diagnostic);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    })
                    .await
                    .unwrap();
                }
                runtime.shutdown().await.unwrap();
            });
        assert_eq!(observed_intent.load(Ordering::SeqCst), !invalid_batch);
        let owner = owner.lock().unwrap();
        assert_eq!(owner.terminals().len(), usize::from(!invalid_batch));
        assert!(
            owner
                .terminals()
                .values()
                .all(|terminal| terminal.lifecycle == ansa::terminal_host::Lifecycle::Closed)
        );
    }
}
