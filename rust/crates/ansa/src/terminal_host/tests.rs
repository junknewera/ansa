use super::{protocol::*, *};
use std::{
    cell::RefCell,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Probe {
    records: Vec<Record>,
    writes: Vec<Vec<u8>>,
    calls: usize,
    stops: usize,
    fail_at: Option<usize>,
    partial: bool,
    close_not_started: bool,
}

#[derive(Clone, Default)]
struct Fixture(Rc<RefCell<Probe>>);

impl Journal for Fixture {
    fn publish(&mut self, record: &Record) -> Result<(), &'static str> {
        let mut probe = self.0.borrow_mut();
        if probe.fail_at == Some(probe.records.len()) {
            return Err("injected_publish_fault");
        }
        probe.records.push(record.clone());
        Ok(())
    }
}

impl Backend for Fixture {
    fn execute(&mut self, intent: &Intent, input: &[u8]) -> Effect {
        let mut probe = self.0.borrow_mut();
        if !probe.records.is_empty() {
            assert_eq!(
                probe.records.last(),
                Some(&Record::Intent {
                    value: Box::new(intent.clone())
                })
            );
        }
        probe.calls += 1;
        if matches!(intent.action, Action::Close) && probe.close_not_started {
            return Effect::NotStarted;
        }
        if input.is_empty() {
            return Effect::Applied;
        }
        if probe.partial {
            probe.writes.push(input[..1].to_vec());
            Effect::Partial { count: 1 }
        } else {
            probe.writes.push(input.to_vec());
            Effect::Delivered { count: input.len() }
        }
    }

    fn stop(&mut self) {
        self.0.borrow_mut().stops += 1;
    }
}

fn id(value: &str) -> Id {
    Id::try_from(value.to_owned()).unwrap()
}

fn binding() -> Binding {
    Binding {
        session: id("session"),
        scope: id("scope"),
        host: id("host"),
        workspace: "/work".into(),
    }
}

fn launch() -> Action {
    Action::Create {
        launch: Launch {
            label: "Shell".into(),
            cwd: "/work".into(),
            argv: vec!["/bin/sh".into()],
            size: Size {
                rows: 24,
                columns: 80,
            },
            lifetime_seconds: 600,
            idle_seconds: 60,
        },
    }
}

fn op(name: &str, terminal: &Id, writer: Writer, epoch: u64, action: Action) -> Intent {
    Intent {
        operation: id(name),
        terminal: terminal.clone(),
        binding: binding(),
        epoch,
        writer,
        invocation_digest: (writer == Writer::Agent).then(|| sha256(name.as_bytes())),
        action,
    }
}

fn open<J: Journal>(host: &mut Host<J, Fixture>, name: &str) -> Id {
    let terminal = host.terminal_id(&id(name));
    let intent = op(name, &terminal, Writer::Agent, 0, launch());
    assert_eq!(
        host.execute(Writer::Agent, intent, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    terminal
}

fn input(name: &str, terminal: &Id, writer: Writer, epoch: u64, bytes: &[u8]) -> Intent {
    op(
        name,
        terminal,
        writer,
        epoch,
        Action::input(writer, bytes).unwrap(),
    )
}

#[test]
fn receipts_deduplicate_every_model_side_effect() {
    let fixture = Fixture::default();
    let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut host, "open1");
    let submitted = b"echo hello\r";
    for (name, epoch, action, bytes, expected) in [
        ("open1", 0, launch(), &[][..], Effect::Applied),
        (
            "input",
            1,
            Action::input(Writer::Agent, submitted).unwrap(),
            &submitted[..],
            Effect::Delivered {
                count: submitted.len(),
            },
        ),
        (
            "resize",
            1,
            Action::Resize {
                size: Size {
                    rows: 25,
                    columns: 81,
                },
            },
            &[][..],
            Effect::Applied,
        ),
        ("interrupt", 1, Action::Interrupt, &[][..], Effect::Applied),
        ("close", 1, Action::Close, &[][..], Effect::Applied),
    ] {
        let intent = op(name, &terminal, Writer::Agent, epoch, action);
        assert_eq!(
            host.execute(Writer::Agent, intent.clone(), bytes),
            Ok(OperationStatus::Durable(expected))
        );
        assert_eq!(
            host.execute(Writer::Agent, intent, bytes),
            Ok(OperationStatus::Durable(expected))
        );
    }
    let different = b"echo other\r";
    assert_eq!(
        host.execute(
            Writer::Agent,
            input("input", &terminal, Writer::Agent, 1, different),
            different
        ),
        Err("terminal_operation_conflict")
    );
    assert_eq!(fixture.0.borrow().calls, 5);
    assert_eq!(fixture.0.borrow().writes, [submitted.to_vec()]);
    assert_eq!(host.terminals()[&terminal].lifecycle, Lifecycle::Closed);
}

#[test]
fn takeover_fences_stale_input_and_private_output() {
    let fixture = Fixture::default();
    let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut host, "open1");
    let before = host.read_permit(Writer::Agent, &terminal, 1, 0).unwrap();
    let takeover = op("take", &terminal, Writer::Human, 1, Action::Takeover);
    assert_eq!(
        host.execute(Writer::Agent, takeover.clone(), &[]),
        Err("terminal_payload")
    );
    assert_eq!(
        host.execute(Writer::Human, takeover, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    assert!(!host.recheck_read(&before));
    let bytes = b"private-keyboard-input";
    assert!(host.read_permit(Writer::Agent, &terminal, 2, 0).is_err());
    assert!(
        host.execute(
            Writer::Agent,
            input("stale", &terminal, Writer::Agent, 1, bytes),
            bytes
        )
        .is_err()
    );
    let human_input = input("human", &terminal, Writer::Human, 2, bytes);
    assert_eq!(
        host.execute(Writer::Human, human_input.clone(), bytes),
        Ok(OperationStatus::Durable(Effect::Delivered {
            count: bytes.len()
        }))
    );
    assert_eq!(
        host.execute(Writer::Human, human_input, bytes),
        Err("terminal_operation_conflict")
    );
    let handback = op(
        "release",
        &terminal,
        Writer::Human,
        2,
        Action::Handback {
            screen: ApprovedScreen {
                through: 100,
                digest: sha256(b"whole approved screen"),
            },
        },
    );
    assert_eq!(
        host.execute(Writer::Human, handback, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    assert!(host.read_permit(Writer::Agent, &terminal, 3, 99).is_err());
    assert!(host.read_permit(Writer::Agent, &terminal, 3, 100).is_ok());
    let encoded = serde_json::to_string(&fixture.0.borrow().records).unwrap();
    assert!(!encoded.contains(std::str::from_utf8(bytes).unwrap()));
    assert!(!encoded.contains(&sha256(bytes)));
    assert_eq!(fixture.0.borrow().writes, [bytes.to_vec()]);
}

#[test]
fn publication_faults_and_restart_preserve_known_facts_without_resending() {
    for fail_after_write in [false, true] {
        let fixture = Fixture::default();
        let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
        let terminal = open(&mut host, "open1");
        fixture.0.borrow_mut().fail_at = Some(if fail_after_write { 3 } else { 2 });
        let bytes = b"once\r";
        let intent = input("enter", &terminal, Writer::Agent, 1, bytes);
        let outcome = host.execute(Writer::Agent, intent.clone(), bytes);
        if fail_after_write {
            assert_eq!(
                outcome,
                Ok(OperationStatus::KnownWithoutDurableReceipt(
                    Effect::Delivered { count: bytes.len() }
                ))
            );
            assert_eq!(host.execute(Writer::Agent, intent.clone(), bytes), outcome);
        } else {
            assert_eq!(outcome, Err("terminal_intent_not_durable"));
        }
        assert!(host.quarantined());
        assert_eq!(fixture.0.borrow().stops, 1);
        assert_eq!(
            fixture.0.borrow().writes.len(),
            usize::from(fail_after_write)
        );
        let records = fixture.0.borrow().records.clone();
        let fresh = Fixture::default();
        let mut recovered = Host::restore(
            binding(),
            fresh.clone(),
            fresh.clone(),
            records,
            Tail::End,
            false,
            HostMode::Bounded,
        )
        .unwrap();
        assert_eq!(recovered.terminals()[&terminal].lifecycle, Lifecycle::Lost);
        assert_eq!(
            recovered.operation(&id("open1")),
            Some(OperationStatus::Durable(Effect::Applied))
        );
        if fail_after_write {
            assert_eq!(
                recovered.execute(Writer::Agent, intent, bytes),
                Ok(OperationStatus::Unresolved)
            );
            recovered.emergency_stop();
            assert_eq!(
                recovered.operation(&id("enter")),
                Some(OperationStatus::Unresolved)
            );
            assert_eq!(fresh.0.borrow().stops, 1);
        } else {
            assert_eq!(
                recovered.execute(Writer::Agent, intent, bytes),
                Err("terminal_recovered_read_only")
            );
        }
        assert!(fresh.0.borrow().writes.is_empty());
    }
}

#[test]
fn cancellation_after_terminal_intent_fsync_dispatches_no_bytes() {
    let fixture = Fixture::default();
    let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut host, "open1");
    let bytes = b"do_not_send\r";
    let intent = input("cancelled_input", &terminal, Writer::Agent, 1, bytes);
    let expected = intent.clone();
    assert_eq!(
        host.execute_if(Writer::Agent, intent.clone(), bytes, || {
            assert_eq!(
                fixture.0.borrow().records.last(),
                Some(&Record::Intent {
                    value: Box::new(expected)
                })
            );
            false
        }),
        Ok(OperationStatus::Durable(Effect::NotStarted))
    );
    assert_eq!(
        host.execute(Writer::Agent, intent, bytes),
        Ok(OperationStatus::Durable(Effect::NotStarted))
    );
    assert!(fixture.0.borrow().writes.is_empty());
}

#[test]
fn partial_input_fences_both_writers_and_close_retains_quarantine() {
    let fixture = Fixture::default();
    let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut host, "open1");
    fixture.0.borrow_mut().partial = true;
    let bytes = b"CR\r";
    assert_eq!(
        host.execute(
            Writer::Agent,
            input("partial", &terminal, Writer::Agent, 1, bytes),
            bytes
        ),
        Ok(OperationStatus::Durable(Effect::Partial { count: 1 }))
    );
    assert!(host.read_permit(Writer::Agent, &terminal, 1, 0).is_ok());
    for writer in [Writer::Agent, Writer::Human] {
        assert_eq!(
            host.execute(writer, input("later", &terminal, writer, 1, bytes), bytes),
            Err("terminal_quarantined")
        );
    }
    assert_eq!(
        host.execute(
            Writer::Human,
            op("close", &terminal, Writer::Human, 1, Action::Close),
            &[]
        ),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    assert!(host.quarantined());
    let new = host.terminal_id(&id("new"));
    assert_eq!(
        host.execute(
            Writer::Agent,
            op("new", &new, Writer::Agent, 0, launch()),
            &[]
        ),
        Err("terminal_quarantined")
    );
    assert_eq!(fixture.0.borrow().writes, [bytes[..1].to_vec()]);
}

#[test]
fn bound_file_evidence_rejects_concurrent_or_corrupt_open() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let temp = Temp(std::env::temp_dir().join(format!(
        "ansa-terminal-host-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let mut host = Host::create_file(&temp.0, binding(), Fixture::default()).unwrap();
    assert!(Host::restore_file(&temp.0, binding(), Fixture::default()).is_err());
    let terminal = open(&mut host, "open1");
    let input = input("enter", &terminal, Writer::Agent, 1, b"once\r");
    assert_eq!(
        host.execute(Writer::Agent, input, b"once\r"),
        Ok(OperationStatus::Durable(Effect::Delivered { count: 5 }))
    );
    // The shell waits before its next exec while holding the same open file
    // description. The pipe keeps this window open across both lock attempts.
    let mut child = Command::new("sh")
        .args(["-c", "IFS= read -r gate; exec true"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(host.journal.duplicate_lock().unwrap()))
        .spawn()
        .unwrap();
    let competing_writer = Host::restore_file(&temp.0, binding(), Fixture::default());
    drop(host);
    let restored = Host::restore_file(&temp.0, binding(), Fixture::default());
    let child_was_waiting = child.try_wait().unwrap().is_none();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert!(child_was_waiting);
    assert!(matches!(competing_writer, Err("terminal_store_locked")));
    assert_eq!(
        restored.unwrap().terminals()[&terminal].lifecycle,
        Lifecycle::Lost
    );
    assert!(Host::create_file(&temp.0, binding(), Fixture::default()).is_err());
    let mut other = binding();
    other.host = id("another_host");
    assert!(Host::restore_file(&temp.0, other, Fixture::default()).is_err());
    let restored = Host::restore_file(&temp.0, binding(), Fixture::default()).unwrap();
    assert_eq!(restored.terminals()[&terminal].lifecycle, Lifecycle::Lost);
    drop(restored);
    std::fs::write(temp.0.join("0004.json.pending"), b"incomplete").unwrap();
    let restored = Host::restore_file(&temp.0, binding(), Fixture::default()).unwrap();
    assert_eq!(
        restored.operation(&id("open1")),
        Some(OperationStatus::Durable(Effect::Applied))
    );
    assert_eq!(
        restored.recovery(),
        Some(Recovery {
            verified_records: 4,
            tail: Tail::Unpublished,
            closure_known: false,
        })
    );
    assert!(restored.quarantined());
    drop(restored);
    std::fs::remove_file(temp.0.join("0004.json.pending")).unwrap();
    let last = temp.0.join("0003.json");
    let receipt = std::fs::read_to_string(&last).unwrap();
    std::fs::write(&last, receipt.replace("\"count\":5", "\"count\":4")).unwrap();
    let restored = Host::restore_file(&temp.0, binding(), Fixture::default()).unwrap();
    assert_eq!(
        restored.recovery(),
        Some(Recovery {
            verified_records: 3,
            tail: Tail::Damaged,
            closure_known: false,
        })
    );
    assert_eq!(
        restored.operation(&id("open1")),
        Some(OperationStatus::Durable(Effect::Applied))
    );
    assert_eq!(
        restored.operation(&id("enter")),
        Some(OperationStatus::Unresolved)
    );
    drop(restored);
    std::fs::write(&last, receipt).unwrap();
    std::fs::remove_file(temp.0.join("0000.json")).unwrap();
    let gap = Host::restore_file(&temp.0, binding(), Fixture::default()).unwrap();
    assert_eq!(
        gap.recovery(),
        Some(Recovery {
            verified_records: 0,
            tail: Tail::Damaged,
            closure_known: false,
        })
    );
    drop(gap);
    for sequence in 1..=3 {
        std::fs::remove_file(temp.0.join(format!("{sequence:04}.json"))).unwrap();
    }
    let fixture = Fixture::default();
    let mut truncated = Host::restore_file(&temp.0, binding(), fixture.clone()).unwrap();
    let lost_create = op("open1", &terminal, Writer::Agent, 0, launch());
    assert_eq!(truncated.operation(&id("open1")), None);
    assert!(truncated.quarantined());
    assert_eq!(
        truncated.execute(Writer::Agent, lost_create, &[]),
        Err("terminal_recovered_read_only")
    );
    assert_eq!(fixture.0.borrow().calls, 0);
    drop(truncated);
    std::fs::remove_file(temp.0.join("terminal-v1.json")).unwrap();
    assert!(temp.0.join("worker.lock").exists());
    assert!(Host::create_file(&temp.0, binding(), fixture.clone()).is_err());
    assert_eq!(fixture.0.borrow().calls, 0);

    let escape_store = Temp(temp.0.with_extension("escaped"));
    let mut escaped = binding();
    escaped.workspace = format!("/{}", "\"".repeat(1023));
    let mut host = Host::create_file(&escape_store.0, escaped.clone(), Fixture::default()).unwrap();
    let Action::Create { mut launch } = launch() else {
        unreachable!()
    };
    launch.cwd = escaped.workspace.clone();
    launch.argv.push("\\".repeat(2041));
    let terminal = host.terminal_id(&id("open"));
    let mut intent = op(
        "open",
        &terminal,
        Writer::Agent,
        0,
        Action::Create { launch },
    );
    intent.binding = escaped;
    assert_eq!(
        host.execute(Writer::Agent, intent, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );

    let conversation_store = Temp(temp.0.with_extension("conversation"));
    let conversation = Binding::attached(id("session"), "/work".into());
    let fixture = Fixture::default();
    let mut host = Host::create_in_mode(
        &conversation_store.0,
        conversation.clone(),
        fixture.clone(),
        HostMode::Conversation,
    )
    .unwrap();
    host.bind_runtime().unwrap();
    let Action::Create { mut launch } = self::launch() else {
        unreachable!()
    };
    launch.lifetime_seconds = 0;
    launch.idle_seconds = 0;
    let terminal = conversation.terminal_id(&id("open"));
    let intent = |name: &str, terminal: &Id, epoch, action| Intent {
        binding: conversation.clone(),
        ..op(name, terminal, Writer::Agent, epoch, action)
    };
    let start = intent(
        "open",
        &terminal,
        0,
        Action::Create {
            launch: launch.clone(),
        },
    );
    let mut bounded = Host::new(conversation.clone(), fixture.clone(), fixture.clone()).unwrap();
    assert_eq!(
        bounded.execute(Writer::Agent, start.clone(), &[]),
        Err("terminal_create_admission")
    );
    assert_eq!(
        host.execute(Writer::Agent, start, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    let second = intent(
        "second",
        &conversation.terminal_id(&id("second")),
        0,
        Action::Create { launch },
    );
    assert_eq!(
        host.execute(Writer::Agent, second, &[]),
        Err("terminal_create_admission")
    );
    for index in 0..MAX_OPERATIONS {
        let resize = Action::Resize {
            size: Size {
                rows: 25,
                columns: 80,
            },
        };
        let request = intent(&format!("resize_{index}"), &terminal, 1, resize);
        assert_eq!(
            host.execute(Writer::Agent, request, &[]),
            Ok(OperationStatus::Durable(Effect::Applied))
        );
    }
    let close = intent("close", &terminal, 1, Action::Close);
    assert_eq!(
        host.execute(Writer::Agent, close.clone(), &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    assert!(host.terminals().is_empty());
    host.seal_clean().unwrap();
    drop(host);
    let restored = Host::restore_in_mode(
        &conversation_store.0,
        conversation,
        fixture,
        HostMode::Conversation,
    )
    .unwrap();
    assert!(restored.recovery().unwrap().closure_known);
    assert_eq!(
        restored.matching_receipt(&close, &[]),
        Ok(Some(OperationStatus::Durable(Effect::Applied)))
    );
}

#[test]
fn scopes_do_not_exchange_operations_or_read_permits() {
    let fixture = Fixture::default();
    let mut source = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut source, "open1");
    let permit = source.read_permit(Writer::Agent, &terminal, 1, 0).unwrap();
    let mut other = binding();
    other.scope = id("other_scope");
    let fixture = Fixture::default();
    let mut destination = Host::new(other.clone(), fixture.clone(), fixture.clone()).unwrap();
    let other_terminal = destination.terminal_id(&id("open1"));
    assert_ne!(terminal, other_terminal);
    let mut create = op("open1", &other_terminal, Writer::Agent, 0, launch());
    assert_eq!(
        destination.execute(Writer::Agent, create.clone(), &[]),
        Err("terminal_operation_invalid")
    );
    create.binding = other.clone();
    assert_eq!(
        destination.execute(Writer::Agent, create, &[]),
        Ok(OperationStatus::Durable(Effect::Applied))
    );
    let mut forged = permit.clone();
    forged.terminal = other_terminal;
    assert!(!destination.recheck_read(&forged));
    forged.binding = other;
    assert!(destination.recheck_read(&forged));
    assert_eq!(fixture.0.borrow().calls, 1);
}

#[test]
fn exhausted_operation_budget_preserves_close_reserve_and_stops_at_hard_cap() {
    let fixture = Fixture::default();
    let mut host = Host::new(binding(), fixture.clone(), fixture.clone()).unwrap();
    let terminal = open(&mut host, "open1");
    for index in 1..NORMAL_OPERATIONS {
        let resize = op(
            &format!("resize_{index}"),
            &terminal,
            Writer::Agent,
            1,
            Action::Resize {
                size: Size {
                    rows: 25,
                    columns: 80,
                },
            },
        );
        assert_eq!(
            host.execute(Writer::Agent, resize, &[]),
            Ok(OperationStatus::Durable(Effect::Applied))
        );
    }
    assert_eq!(
        host.execute(
            Writer::Agent,
            op("interrupt", &terminal, Writer::Agent, 1, Action::Interrupt),
            &[]
        ),
        Err("terminal_operation_budget")
    );
    fixture.0.borrow_mut().close_not_started = true;
    for index in NORMAL_OPERATIONS..MAX_OPERATIONS {
        let close = op(
            &format!("close_{index}"),
            &terminal,
            Writer::Human,
            1,
            Action::Close,
        );
        assert_eq!(
            host.execute(Writer::Human, close, &[]),
            Ok(OperationStatus::Durable(Effect::NotStarted))
        );
    }
    assert_eq!(fixture.0.borrow().stops, 1);
    assert!(host.quarantined());
    assert_eq!(
        host.operation(&id("open1")),
        Some(OperationStatus::Durable(Effect::Applied))
    );
}
