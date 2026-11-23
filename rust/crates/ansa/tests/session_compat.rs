use ansa::{
    context::{
        epoch::compact_epoch,
        legacy_compact::{CompactedContext, summary_request},
        prompt_policy::PromptPolicy,
    },
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        contracts::{AgentMode, GetTranscript, SessionId},
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
fn legacy_replay_preserves_the_original_per_response_call_bound() {
    let events = events("completed");
    let accepted = events[1]
        .with("call_limit", Json::parse("64").unwrap())
        .unwrap();
    let response = events[2].field("response").unwrap();
    let call = &response.field("items").unwrap().elements().unwrap()[1];
    let calls = (0..17)
        .map(|n| {
            call.with("id", Json::string(&format!("item{n}")).unwrap())
                .unwrap()
                .with("call_id", Json::string(&format!("call{n}")).unwrap())
                .unwrap()
        })
        .collect();
    let large = events[2]
        .with(
            "response",
            response.with("items", Json::array(calls).unwrap()).unwrap(),
        )
        .unwrap();
    for legacy in [true, false] {
        let mut state = state();
        state.apply(&events[0]).unwrap();
        let turn = if legacy {
            accepted.clone()
        } else {
            accepted
                .with("tool_budget_version", Json::parse("2").unwrap())
                .unwrap()
        };
        state.apply(&turn).unwrap();
        if legacy {
            assert_eq!(state.apply(&large), Err("session_turn_limits"));
        } else {
            state.apply(&large).unwrap();
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

    // A late rejection after a candidate changed its transcript/history must
    // preserve both the live index and all previously published snapshots.
    let trace = events("completed");
    let mut s = state();
    for event in &trace[..trace.len() - 1] {
        s.apply(event).unwrap();
    }
    let key = s.turn.as_ref().unwrap().receipt.command.0.clone();
    let receipt = s.submits[&key].clone();
    for index in 0..128 {
        let mut past = receipt.clone();
        past.command.0 = format!("past_{index}");
        past.state = "completed".into();
        s.submits.insert(past.command.0.clone(), past);
    }
    let published = s.clone();
    s.submits.remove(&key);
    let before = format!("{s:?}");
    assert_eq!(s.apply(trace.last().unwrap()), Err("session_submit"));
    assert_eq!(format!("{s:?}"), before);
    s.submits.insert(key.clone(), receipt);
    s.apply(trace.last().unwrap()).unwrap();
    assert_eq!(s.submits[&key].state, "completed");
    for index in 0..128 {
        let key = format!("past_{index}");
        assert_eq!(s.submits[&key].state, published.submits[&key].state);
        assert!(Arc::ptr_eq(
            &s.submits[&key].text,
            &published.submits[&key].text
        ));
    }
    assert_eq!(published.transcript().len(), 1);
    assert_eq!(s.transcript().len(), 2);
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
fn prompt_policy_replays_legacy_and_opt_in_plan_from_saved_epoch() {
    let legacy = Request::load(data().field("configuration").unwrap()).unwrap();
    let accepted = events("plan")[1].clone();
    let text = accepted.text_field("text").unwrap();
    let plan_tools = legacy.tools.clone();
    let old = State::new("fixture".into(), legacy.clone(), plan_tools.clone());
    assert_eq!(
        old.request_for(text, AgentMode::Plan)
            .unwrap()
            .save()
            .unwrap(),
        *accepted.field("next").unwrap()
    );

    let mut opted_in = legacy;
    PromptPolicy::HandoffV1.bind(&mut opted_in.epoch).unwrap();
    let mut state = State::new("fixture".into(), opted_in, plan_tools.clone());
    state.apply(&state.initialization().unwrap()).unwrap();
    let next = state.request_for(text, AgentMode::Plan).unwrap();
    assert_eq!(next.tools, plan_tools);
    assert_eq!(
        PromptPolicy::resolve(&next.epoch.exact_state).unwrap(),
        PromptPolicy::HandoffV1
    );
    assert_eq!(state.apply(&accepted), Err("session_request_conflict"));
    state
        .apply(&accepted.with("next", next.save().unwrap()).unwrap())
        .unwrap();
    let seed = state.fork_seed().unwrap();
    let fork = seed
        .restricted_tools(&Advertisement {
            revision: sha256(b"[]"),
            descriptors: vec![],
        })
        .unwrap()
        .request()
        .unwrap();
    assert!(fork.tools.descriptors.is_empty());
    assert_eq!(
        PromptPolicy::resolve(&fork.epoch.exact_state).unwrap(),
        PromptPolicy::HandoffV1
    );
}

#[test]
fn handoff_reconstruction_keeps_tail_and_rejects_legacy_summary_response() {
    let fixture = data();
    let mut original = Request::load(fixture.field("source").unwrap()).unwrap();
    let legacy = fixture.field("summary").unwrap();
    let saved = legacy.field("summary_response").unwrap();
    let mut legacy_epoch = original.epoch.clone();
    PromptPolicy::LegacyV1.bind(&mut legacy_epoch).unwrap();
    assert_eq!(legacy_epoch, original.epoch);
    assert_eq!(
        summary_request(&original).unwrap().save().unwrap(),
        *saved.field("request").unwrap()
    );
    PromptPolicy::HandoffV1.bind(&mut original.epoch).unwrap();
    assert_eq!(
        CompactedContext::admit(original.clone(), legacy.clone()).unwrap_err(),
        "summary_response_request"
    );
    for generation in 1..=3 {
        let request = summary_request(&original).unwrap();
        assert!(request.tools.descriptors.is_empty());
        assert!(request.epoch.instructions.contains("stale checks"));
        assert_eq!(request.binding, original.binding);
        let response =
            ansa::provider::native::load(&saved.with("request", request.save().unwrap()).unwrap())
                .unwrap();
        let compacted = CompactedContext::from_summary(original.clone(), &response).unwrap();
        let restored =
            CompactedContext::admit(original.clone(), compacted.native().clone()).unwrap();
        assert_eq!(
            &restored.output().unwrap()[1..],
            &original.input[request.input.len()..]
        );
        original.epoch = compact_epoch(&original.epoch, generation, &restored).unwrap();
        let mut continued = restored.output().unwrap().to_vec();
        continued.extend([
            Json::parse(r#"{"role":"user","content":"Continue with these exact constraints."}"#)
                .unwrap(),
            Json::parse(r#"{"role":"assistant","content":"Continuing this turn."}"#).unwrap(),
        ]);
        original = original.with_input(continued).unwrap();
    }
}

#[test]
fn prompt_policy_rejects_unknown_ambiguous_and_overridden_provenance() {
    for (state, diagnostic) in [
        (
            r#"{"prompt_policy":"unknown"}"#,
            "prompt_policy_unsupported",
        ),
        (
            r#"{"compaction":"unknown"}"#,
            "compaction_policy_unsupported",
        ),
        (
            r#"{"base_epoch":"id","base_state":{},"base":{}}"#,
            "prompt_policy_conflict",
        ),
        (
            r#"{"base_epoch":"id","base_state":{"prompt_policy":"handoff-v1"},"prompt_policy":"legacy-v1"}"#,
            "prompt_policy_conflict",
        ),
    ] {
        let policy = Json::parse(state).unwrap();
        assert_eq!(PromptPolicy::resolve(&policy), Err(diagnostic));
        let mut configuration = Request::load(data().field("configuration").unwrap()).unwrap();
        configuration.epoch.exact_state = policy;
        let state = State::new("fixture".into(), configuration.clone(), configuration.tools);
        assert_eq!(state.initialization(), Err(diagnostic));
    }
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

#[test]
fn fork_and_native_compaction_preserve_original_image_parts() {
    let original = Request::load(data().field("configuration").unwrap()).unwrap();
    let valid = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4//8/AAX+Av4N70a4AAAAAElFTkSuQmCC";
    let item = |url| {
        Json::parse(
            &serde_json::json!({"role":"user","content":[
        {"type":"input_text","text":"describe"},{"type":"input_image","image_url":url}]})
            .to_string(),
        )
        .unwrap()
    };
    let retained = item(valid);
    let altered = item("data:image/png;base64,bm90IHBuZw==");
    let history = original.clone().with_input(vec![retained.clone()]).unwrap();
    let capsule = |inputs: Request| {
        Json::parse(r#"{"kind":"fork","source_session":"parent","source_turn":"one","transcript":[],"completed_starts":[0]}"#)
            .unwrap().with("context", inputs.save().unwrap()).unwrap()
            .with("timeline", ansa::session::timeline::Timeline::default().save().unwrap()).unwrap()
    };
    let mut child = state();
    child.apply(&child.initialization().unwrap()).unwrap();
    assert!(
        child
            .apply(&capsule(
                original.with_input(vec![altered.clone()]).unwrap()
            ))
            .is_err()
    );
    assert!(child.history.is_empty());
    child.apply(&capsule(history.clone())).unwrap();
    assert_eq!(child.history.as_slice(), std::slice::from_ref(&retained));
    let compact = |user| {
        Json::parse(r#"{"object":"response.compaction","id":"cp"}"#)
            .unwrap()
            .with(
                "output",
                Json::array(vec![
                    Json::parse(
                        r#"{"type":"compaction","id":"opaque","encrypted_content":"cipher"}"#,
                    )
                    .unwrap(),
                    user,
                ])
                .unwrap(),
            )
            .unwrap()
    };
    assert!(CompactedContext::admit(history.clone(), compact(retained)).is_ok());
    assert!(CompactedContext::admit(history, compact(altered)).is_err());
}
