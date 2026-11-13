use ansa::{
    context::{
        epoch::compact_epoch,
        legacy_compact::{CompactedContext, summary_request},
    },
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        contracts::{GetTranscript, SessionId},
        request::{Advertisement, Request},
    },
    session::{
        codec_v1::SavedResult,
        journal_v1::{Journal, Observer, Point},
        state::State,
        store::{Store, Worker},
    },
};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "ansa-m4-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn journal(&self) -> PathBuf {
        self.0.join("journal")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixtures() -> PathBuf {
    std::env::var_os("ANSA_SESSION_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/sessions")
        })
}
fn data() -> Json {
    Json::parse(&fs::read_to_string(fixtures().join("golden.json")).unwrap()).unwrap()
}
fn state() -> State {
    State::new(
        SessionId("fixture".into()),
        Request::load(data().field("configuration").unwrap()).unwrap(),
        Advertisement {
            revision: String::new(),
            descriptors: vec![],
        },
    )
}
fn events(name: &str) -> Vec<Json> {
    data()
        .field("traces")
        .unwrap()
        .field(name)
        .unwrap()
        .elements()
        .unwrap()
        .to_vec()
}
fn record(value: usize) -> Json {
    Json::parse(&format!("{{\"kind\":\"test\",\"value\":{value}}}")).unwrap()
}
fn frame(event: &Json, sequence: usize, previous: &str) -> Vec<u8> {
    let body = format!(
        "{{\"schema_version\":1,\"sequence\":{sequence},\"previous\":\"{previous}\",\"event\":{}}}",
        event.dump()
    );
    format!(
        "{:08x} {:08x} {}\n{}",
        body.len(),
        !(body.len() as u32),
        sha256(body.as_bytes()),
        body
    )
    .into_bytes()
}
#[test]
fn cpp_golden_journals() {
    let d = data();
    for name in d.field("traces").unwrap().keys().unwrap() {
        let root = Root::new();
        fs::create_dir(root.journal()).unwrap();
        let file = root.journal().join("part-00000000000000000000.log");
        fs::copy(fixtures().join(format!("{name}.log")), &file).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(&file).unwrap();
        let config = Request::load(d.field("configuration").unwrap()).unwrap();
        let mut restored = State::new("fixture".into(), config.clone(), config.tools.clone());
        let journal = Journal::open(&root.journal(), |e| restored.apply(e), None).unwrap();
        assert_eq!(journal.diagnostic(), "", "{name}");
        assert!(restored.initialized);
        assert_eq!(fs::read(&file).unwrap(), before);
        if name == "completed" {
            assert_eq!(restored.turn.as_ref().unwrap().stage, "completed");
            assert_eq!(restored.transcript().len(), 2);
        }
        if name == "outstanding-intent" {
            assert_eq!(restored.unknown_operation(), Some("run1:op:1"));
        }
    }
}
#[test]
fn exact_cpp_frame_and_chain() {
    let root = Root::new();
    let mut journal = Journal::open(&root.journal(), |_| Ok(()), None).unwrap();
    journal.append(&record(1)).unwrap();
    assert_eq!(
        fs::read(root.journal().join("part-00000000000000000000.log")).unwrap(),
        fs::read(fixtures().join("minimal.log")).unwrap()
    );
    assert!(matches!(
        Journal::open(&root.journal(), |_| Ok(()), None),
        Err("session_busy")
    ));
}
#[test]
fn torn_tail_is_retained() {
    let bytes = fs::read(fixtures().join("minimal.log")).unwrap();
    for cut in 0..bytes.len() {
        let root = Root::new();
        fs::create_dir(root.journal()).unwrap();
        let file = root.journal().join("part-00000000000000000000.log");
        fs::write(&file, &bytes[..cut]).unwrap();
        let mut journal = Journal::open(&root.journal(), |_| Ok(()), None).unwrap();
        assert_eq!(journal.diagnostic(), "");
        journal.append(&record(2)).unwrap();
        let retained = fs::read(&file).unwrap();
        if cut == 92 {
            assert!(retained.starts_with(&bytes[..cut]));
        } else {
            assert_eq!(retained, bytes[..cut]);
        }
    }
}
#[test]
fn read_only_corruption_and_transactional_reducer() {
    let root = Root::new();
    fs::create_dir(root.journal()).unwrap();
    let mut bytes = fs::read(fixtures().join("minimal.log")).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(root.journal().join("part-00000000000000000000.log"), &bytes).unwrap();
    let mut journal = Journal::open(&root.journal(), |_| Ok(()), None).unwrap();
    assert_eq!(journal.diagnostic(), "journal_checksum");
    assert_eq!(journal.append(&record(2)), Err("journal_read_only"));
    let mut s = state();
    s.apply(&events("completed")[0]).unwrap();
    let before = s.transcript();
    let mut invalid = events("completed")[1].clone();
    invalid = invalid
        .with("next", data().field("configuration").unwrap().clone())
        .unwrap();
    assert_eq!(s.apply(&invalid), Err("session_request_conflict"));
    assert_eq!(s.next_request, 0);
    assert_eq!(before.len(), s.transcript().len());
}
#[test]
fn scan_conflict_cannot_adopt_unreplayed_size() {
    let root = Root::new();
    let file = root.journal().join("part-00000000000000000000.log");
    {
        let mut journal = Journal::open(&root.journal(), |_| Ok(()), None).unwrap();
        journal.append(&record(1)).unwrap();
    }
    let body = Json::parse(std::str::from_utf8(&fs::read(&file).unwrap()[175..]).unwrap()).unwrap();
    let previous = sha256(body.dump().as_bytes());
    let mut injected = false;
    let mut journal = Journal::open(
        &root.journal(),
        |_| {
            if !injected {
                injected = true;
                OpenOptions::new()
                    .append(true)
                    .open(&file)
                    .unwrap()
                    .write_all(&frame(&record(2), 2, &previous))
                    .unwrap();
            }
            Ok(())
        },
        None,
    )
    .unwrap();
    assert_eq!(journal.diagnostic(), "journal_write_conflict");
    assert_eq!(journal.append(&record(3)), Err("journal_read_only"));
}
#[test]
fn append_size_and_replacement_conflicts() {
    for replace in [false, true] {
        let root = Root::new();
        let mut journal = Journal::open(&root.journal(), |_| Ok(()), None).unwrap();
        journal.append(&record(1)).unwrap();
        let file = root.journal().join("part-00000000000000000000.log");
        if replace {
            fs::rename(&file, root.0.join("old")).unwrap();
            fs::write(&file, b"replacement").unwrap();
        } else {
            OpenOptions::new()
                .append(true)
                .open(&file)
                .unwrap()
                .write_all(b"tail")
                .unwrap();
        }
        assert_eq!(journal.append(&record(2)), Err("journal_write_conflict"));
        assert_eq!(journal.append(&record(3)), Err("journal_read_only"));
    }
}
#[test]
fn durable_done_and_known_outcome_on_io_failure() {
    for target in ["outcome", "done"] {
        let root = Root::new();
        let observer: Observer = Arc::new(move |point, kind| {
            if point == Point::BeforeSync && kind == target {
                Err("journal_injected_io")
            } else {
                Ok(())
            }
        });
        let mut store = Store::open(&root.journal(), state(), Some(observer)).unwrap();
        for event in events("completed").into_iter().skip(1) {
            let target_event = event.text_field("kind").unwrap() == target;
            let result = store.commit(&event);
            if target_event {
                assert_eq!(result, Err("journal_injected_io"));
                assert_eq!(store.state().transcript().len(), 1);
                assert_ne!(store.state().turn.as_ref().unwrap().stage, "completed");
                assert_eq!(store.known_outcome().is_some(), target == "outcome");
                assert_eq!(store.commit(&event), Err("journal_read_only"));
                break;
            }
            result.unwrap();
        }
    }
}
#[test]
fn restart_requires_resume_and_known_recovery_is_once() {
    let root = Root::new();
    {
        let mut store = Store::open(&root.journal(), state(), None).unwrap();
        for e in events("outstanding-intent").into_iter().skip(1) {
            store.commit(&e).unwrap();
        }
    }
    let mut store = Store::open(&root.journal(), state(), None).unwrap();
    assert!(store.requires_resume());
    assert_eq!(store.state().unknown_operation(), Some("run1:op:1"));
    let calls = AtomicUsize::new(0);
    let result = SavedResult::load(events("completed")[4].field("result").unwrap()).unwrap();
    store
        .recover(|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(Some(result))
        })
        .unwrap();
    store
        .recover(|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            panic!("recovered twice")
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(store.requires_resume());
    assert_eq!(store.state().unknown_operation(), None);
}
#[test]
fn recovery_append_failure_preserves_known_result() {
    let root = Root::new();
    {
        let mut store = Store::open(&root.journal(), state(), None).unwrap();
        for e in events("outstanding-intent").into_iter().skip(1) {
            store.commit(&e).unwrap();
        }
    }
    let observer: Observer = Arc::new(|p, k| {
        if p == Point::BeforeSync && k == "outcome" {
            Err("journal_injected_io")
        } else {
            Ok(())
        }
    });
    let mut store = Store::open(&root.journal(), state(), Some(observer)).unwrap();
    let result = SavedResult::load(events("completed")[4].field("result").unwrap()).unwrap();
    assert_eq!(
        store.recover(|_| Ok(Some(result))),
        Err("journal_injected_io")
    );
    assert_eq!(
        store.known_outcome().unwrap().result.operation_id(),
        "run1:op:1"
    );
    assert_eq!(store.state().unknown_operation(), Some("run1:op:1"));
    assert_eq!(store.state().transcript().len(), 1);
}
#[test]
fn worker_backpressure_and_drop_join() {
    let root = Root::new();
    let barrier = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let cloned = barrier.clone();
    let observer: Observer = Arc::new(move |p, k| {
        if p == Point::BeforeSync && k == "accepted" {
            let (mutex, changed) = &*cloned;
            let mut state = mutex.lock().unwrap();
            state.0 = true;
            changed.notify_all();
            while !state.1 {
                state = changed
                    .wait_timeout(state, Duration::from_secs(10))
                    .unwrap()
                    .0;
            }
        }
        Ok(())
    });
    let store = Store::open(&root.journal(), state(), Some(observer)).unwrap();
    let worker = Worker::start(store).unwrap();
    let event = events("completed")[1].clone();
    let first = worker.submit(&event).unwrap();
    let (mutex, changed) = &*barrier;
    let mut flags = mutex.lock().unwrap();
    while !flags.0 {
        flags = changed
            .wait_timeout(flags, Duration::from_secs(10))
            .unwrap()
            .0;
    }
    let pause =
        Json::parse(r#"{"kind":"pause","turn":"t1","diagnostic":"fixture_pause"}"#).unwrap();
    let second = worker.submit(&pause).unwrap();
    assert!(matches!(worker.submit(&pause), Err("journal_queue_full")));
    assert!(first.try_recv().is_err());
    flags.1 = true;
    changed.notify_all();
    drop(flags);
    drop(worker);
    assert!(first.recv().unwrap().result.is_ok());
    assert!(second.recv().unwrap().result.is_ok());
}
#[test]
fn transcript_snapshot_pages_and_epoch_hashes() {
    let mut state = state();
    state.apply(&state.initialization().unwrap()).unwrap();
    let base = state.transcript();
    for e in events("completed").into_iter().skip(1) {
        state.apply(&e).unwrap();
    }
    assert!(base.is_empty());
    let page = state.transcript().page(GetTranscript {
        before: 0,
        limit: 1,
        forward: false,
    });
    assert_eq!(page.begin, 1);
    assert_eq!(page.entries[0].role, "assistant");
    let d = data();
    let source = Request::load(d.field("source").unwrap()).unwrap();
    let summary =
        CompactedContext::admit(source.clone(), d.field("summary").unwrap().clone()).unwrap();
    assert_eq!(summary.output().unwrap().len(), 5);
    assert_eq!(summary_request(&source).unwrap().input.len(), 2);
    let epoch = compact_epoch(&source.epoch, 1, &summary).unwrap();
    assert_eq!(
        epoch.id,
        format!(
            "compact_1_{}",
            &sha256(summary.native().dump().as_bytes())[..16]
        )
    );
}
#[test]
fn catalog_identity_and_latest() {
    use ansa::session::catalog::Catalog;
    let root = Root::new();
    let cwd = root.0.join("workspace");
    let catalog = Catalog::open(&root.0.join("sessions"), &cwd, "offline").unwrap();
    assert!(matches!(catalog.latest(), Err("no_previous_session")));
    let first = catalog.create_named("first").unwrap();
    let second = catalog.create_named("second").unwrap();
    for (selected, seconds) in [(&first, 1), (&second, 2)] {
        let file = std::fs::File::create(selected.directory.join("part-00000000000000000000.log"))
            .unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .unwrap();
    }
    assert_eq!(catalog.latest().unwrap().id, "second");
    assert!(catalog.create_named("first").is_err());
    assert_eq!(catalog.explicit("first").unwrap().id, "first");
    let other = Catalog::open(&root.0.join("sessions"), &cwd, "other").unwrap();
    assert!(matches!(
        other.explicit("first"),
        Err("session_workspace_mismatch")
    ));
}

#[test]
fn transcript_block_boundaries_and_byte_cap() {
    use ansa::provider::contracts::AgentMode;
    fn accept(state: &State, index: usize, text: &str) -> Json {
        Json::object(vec![
            ("kind", Json::string("accepted").unwrap()),
            ("command", Json::string(&format!("c{index}")).unwrap()),
            ("turn", Json::string(&format!("t{index}")).unwrap()),
            ("request", Json::string(&format!("r{index}")).unwrap()),
            ("request_number", Json::parse(&index.to_string()).unwrap()),
            ("text", Json::string(text).unwrap()),
            (
                "operation_run",
                Json::string(&format!("run{index}")).unwrap(),
            ),
            (
                "next",
                state
                    .request_for(text, AgentMode::Basic)
                    .unwrap()
                    .save()
                    .unwrap(),
            ),
            ("compact", Json::parse("false").unwrap()),
        ])
        .unwrap()
    }
    let mut long = state();
    long.apply(&long.initialization().unwrap()).unwrap();
    let mut early = None;
    for index in 1..=300 {
        let event = accept(&long, index, &format!("item {index}"));
        long.apply(&event).unwrap();
        if index == 64 {
            early = Some(long.transcript());
        }
        long.apply(
            &Json::parse(&format!("{{\"kind\":\"cancel\",\"turn\":\"t{index}\"}}")).unwrap(),
        )
        .unwrap();
    }
    let early = early.unwrap();
    assert_eq!(early.len(), 64);
    assert_eq!(
        early
            .page(GetTranscript {
                before: 0,
                limit: 64,
                forward: false
            })
            .entries
            .last()
            .unwrap()
            .state,
        "generating"
    );
    let page = long.transcript().page(GetTranscript {
        before: 130,
        limit: 64,
        forward: false,
    });
    assert_eq!((page.begin, page.end, page.total), (66, 130, 300));
    assert_eq!(page.entries[0].text, "item 67");
    assert_eq!(page.entries.last().unwrap().text, "item 130");
    let page = long.transcript().page(GetTranscript {
        before: 61,
        limit: 8,
        forward: true,
    });
    assert_eq!((page.begin, page.end), (61, 69));
    assert_eq!(page.entries[0].text, "item 62");
    assert_eq!(page.entries.last().unwrap().text, "item 69");
    let page = long.transcript().page(GetTranscript {
        before: 2,
        limit: 16,
        forward: false,
    });
    assert_eq!(page.begin, 0);
    assert_eq!(page.entries[0].text, "item 1");
    let mut sized = state();
    sized.apply(&sized.initialization().unwrap()).unwrap();
    for index in 1..=24 {
        let text = char::from(b'a' + index as u8 - 1)
            .to_string()
            .repeat(if index <= 4 { 1 } else { 16 * 1024 });
        sized.apply(&accept(&sized, index, &text)).unwrap();
        sized
            .apply(&Json::parse(&format!("{{\"kind\":\"cancel\",\"turn\":\"t{index}\"}}")).unwrap())
            .unwrap();
    }
    let page = sized.transcript().page(GetTranscript {
        before: 4,
        limit: 64,
        forward: true,
    });
    assert_eq!((page.begin, page.end, page.entries.len()), (4, 20, 16));
    assert!(page.entries[0].text.starts_with('e'));
    assert!(page.entries.last().unwrap().text.starts_with('t'));
    let next = sized.transcript().page(GetTranscript {
        before: page.end,
        limit: 64,
        forward: true,
    });
    assert_eq!((next.begin, next.end), (20, 24));
    assert!(next.entries[0].text.starts_with('u'));
    let page = sized.transcript().page(GetTranscript {
        before: 20,
        limit: 64,
        forward: false,
    });
    assert_eq!((page.begin, page.end, page.entries.len()), (4, 20, 16));
}
