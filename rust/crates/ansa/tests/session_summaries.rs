#![forbid(unsafe_code)]
use ansa::{
    auth::Cancel,
    config::load_agent_settings,
    frontend::{
        protocol_v1::*,
        sessions::{Factory, Kind},
    },
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::{Advertisement, Request},
    runtime::{PostStatus, Runtime},
    session::{
        catalog::{Catalog, SummaryStatus},
        journal_v1::{Journal, inspect_read_only},
        state::State,
    },
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ansa-summary-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["", "home", "work", "sessions"] {
            ansa_linux::fs::create_private(&path.join(name)).unwrap();
        }
        Self(path)
    }
    fn catalog(&self) -> Catalog {
        Catalog::open(&self.0.join("sessions"), &self.0.join("work"), "offline").unwrap()
    }
    fn factory(&self) -> Factory {
        Factory::new(
            load_agent_settings(&self.0.join("work"), &self.0.join("home")).unwrap(),
            self.0.join("auth"),
            true,
            "offline-fixture".into(),
            None,
        )
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
fn golden() -> Json {
    Json::parse(&fs::read_to_string(fixtures().join("golden.json")).unwrap()).unwrap()
}
fn copy_fixture(name: &str, destination: &Path) {
    let source = fixtures().join(name);
    let original = sha256(&fs::read(&source).unwrap());
    fs::copy(&source, destination).unwrap();
    fs::set_permissions(destination, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(sha256(&fs::read(destination).unwrap()), original);
    assert_eq!(sha256(&fs::read(&source).unwrap()), original);
    assert_eq!(fs::metadata(destination).unwrap().mode() & 0o777, 0o600);
}
fn fingerprints(path: &Path) -> BTreeMap<String, (String, u64, u64, u32)> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            if !name.starts_with("part-") {
                return None;
            }
            let meta = fs::metadata(&path).unwrap();
            Some((
                name,
                (
                    sha256(&fs::read(&path).unwrap()),
                    meta.ino(),
                    meta.nlink(),
                    meta.mode(),
                ),
            ))
        })
        .collect()
}
fn command(runtime: &Runtime, id: &str, payload: CommandPayload) -> RuntimeCommand {
    let handle = runtime.handle();
    RuntimeCommand {
        schema_version: 1,
        session: handle.session().clone(),
        turn: "summary_turn".into(),
        incarnation: handle.incarnation().clone(),
        command: id.into(),
        payload,
    }
}
async fn context(runtime: &Runtime, id: &str) -> ansa::runtime::ForkSeed {
    tokio::time::timeout(Duration::from_secs(5), runtime.ready())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime
            .post(command(runtime, id, CommandPayload::GetContext))
            .await
            .unwrap(),
        PostStatus::Accepted
    );
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
async fn complete(runtime: &Runtime, id: &str, text: &str, mode: AgentMode) {
    tokio::time::timeout(Duration::from_secs(5), runtime.ready())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime
            .post(command(
                runtime,
                id,
                CommandPayload::Submit(Submit {
                    text: text.into(),
                    mode
                })
            ))
            .await
            .unwrap(),
        PostStatus::Accepted
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), runtime.next_event())
            .await
            .unwrap_or_else(|_| panic!("completion timeout: {id}"))
            .unwrap();
        match event.payload {
            EventPayload::FrontendSnapshot(view) => {
                assert!(!view.storage_failed, "{}", view.diagnostic);
                assert_ne!(view.state, "paused", "{}", view.diagnostic);
                if view.submit.as_ref().is_some_and(|command| command.0 == id)
                    && view.state == "completed"
                    && !view.worker_busy
                {
                    return;
                }
            }
            EventPayload::RuntimeError { diagnostic, .. } => panic!("{id}: {diagnostic}"),
            EventPayload::ResyncRequired { .. } => {
                panic!("{id}: unexpected summary fixture event overflow")
            }
            _ => {}
        }
    }
}
#[tokio::test(flavor = "current_thread")]
async fn child_title_is_first_own_prompt_and_completed_tree_survives_restart() {
    let root = Root::new();
    let factory = root.factory();
    let mut main = factory.create(Kind::Main, None).unwrap();
    complete(
        &main.runtime,
        "parent_command",
        "Parent original title",
        AgentMode::Plan,
    )
    .await;
    let seed = context(&main.runtime, "parent_context").await;
    let mut child = factory.create(Kind::Agent, Some(seed)).unwrap();
    context(&child.runtime, "child_ready").await;
    let before = fingerprints(&child.selected.directory);
    let summaries = factory.summaries(&Cancel::default()).unwrap();
    let summary = summaries
        .iter()
        .find(|value| value.entry.id == child.selected.id)
        .unwrap();
    assert_eq!(
        summary.entry.parent.as_deref(),
        Some(main.selected.id.as_str())
    );
    assert_eq!(summary.title, None);
    assert_eq!(summary.status, SummaryStatus::Empty);
    assert_eq!(fingerprints(&child.selected.directory), before);
    complete(
        &child.runtime,
        "child_command",
        "Own child task title",
        AgentMode::Basic,
    )
    .await;
    complete(
        &child.runtime,
        "child_second",
        "Later user prompt is a preview, not the title",
        AgentMode::Basic,
    )
    .await;
    let main_id = main.selected.id.clone();
    let child_id = child.selected.id.clone();
    let before_main = fingerprints(&main.selected.directory);
    let before_child = fingerprints(&child.selected.directory);
    let live = factory.summaries(&Cancel::default()).unwrap(); // Coexists with exclusive runtime journals.
    assert_eq!(
        live.iter()
            .find(|value| value.entry.id == child_id)
            .unwrap()
            .title
            .as_deref(),
        Some("Own child task title")
    );
    assert_eq!(fingerprints(&main.selected.directory), before_main);
    assert_eq!(fingerprints(&child.selected.directory), before_child);
    child.runtime.shutdown().await.unwrap();
    main.runtime.shutdown().await.unwrap();
    let restarted = root.factory().summaries(&Cancel::default()).unwrap();
    assert_eq!(restarted.len(), 2);
    let main_summary = restarted
        .iter()
        .find(|value| value.entry.id == main_id)
        .unwrap();
    let child_summary = restarted
        .iter()
        .find(|value| value.entry.id == child_id)
        .unwrap();
    assert_eq!(main_summary.title.as_deref(), Some("Parent original title"));
    assert_eq!(main_summary.status, SummaryStatus::Completed);
    assert_eq!(child_summary.title.as_deref(), Some("Own child task title"));
    assert_eq!(
        child_summary.entry.parent.as_deref(),
        Some(main_id.as_str())
    );
    assert_eq!(child_summary.entry.kind, "agent");
    assert_eq!(child_summary.status, SummaryStatus::Completed);
    assert!(
        child_summary
            .preview
            .as_ref()
            .is_some_and(|value| value.starts_with("OMH_TOOL_OK:"))
    );
}
#[test]
fn legacy_cpp_codec_summary_is_read_only_and_never_creates_a_lock() {
    let root = Root::new();
    let catalog = root.catalog();
    let selected = catalog.create_named("fixture").unwrap();
    let part = selected.directory.join("part-00000000000000000000.log");
    copy_fixture("completed.log", &part);
    let before = fingerprints(&selected.directory);
    let summary = catalog.summaries(&Cancel::default()).unwrap().remove(0);
    let data = golden();
    let first = &data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap()[1];
    assert_eq!(
        summary.title.as_deref(),
        Some(first.text_field("text").unwrap())
    );
    assert_eq!(summary.status, SummaryStatus::Completed);
    assert_eq!(fingerprints(&selected.directory), before);
    assert!(!selected.directory.join("lock").exists());
}
#[test]
fn malformed_missing_partial_and_linked_journals_are_unavailable_without_recovery() {
    let root = Root::new();
    let catalog = root.catalog();
    let selected = catalog.create_named("fixture").unwrap();
    assert_eq!(
        catalog.summaries(&Cancel::default()).unwrap()[0].status,
        SummaryStatus::Unavailable
    );
    let part = selected.directory.join("part-00000000000000000000.log");
    copy_fixture("completed.log", &part);
    OpenOptions::new()
        .append(true)
        .open(&part)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let before = fingerprints(&selected.directory);
    let summary = catalog.summaries(&Cancel::default()).unwrap().remove(0);
    assert_eq!(summary.status, SummaryStatus::Unavailable);
    assert_eq!(summary.title, None);
    assert_eq!(fingerprints(&selected.directory), before);
    assert!(!selected.directory.join("lock").exists());
    copy_fixture("completed.log", &part);
    let mut corrupt = fs::read(&part).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&part, corrupt).unwrap();
    let before = fingerprints(&selected.directory);
    assert_eq!(
        catalog.summaries(&Cancel::default()).unwrap()[0].status,
        SummaryStatus::Unavailable
    );
    assert_eq!(fingerprints(&selected.directory), before);
    copy_fixture("completed.log", &part);
    fs::hard_link(&part, root.0.join("part-alias")).unwrap();
    assert_eq!(
        catalog.summaries(&Cancel::default()).unwrap()[0].status,
        SummaryStatus::Unavailable
    );
    assert_eq!(fs::metadata(&part).unwrap().nlink(), 2);
}
#[test]
fn inspection_is_cancelled_bounded_and_detects_append_during_a_read() {
    use std::cell::{Cell, RefCell};
    let root = Root::new();
    let selected = root.catalog().create_named("fixture").unwrap();
    let data = golden();
    let events = data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap();
    let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
    writer.append(&events[0]).unwrap();
    writer.append(&events[1]).unwrap();
    let before = fingerprints(&selected.directory);
    let mut budget = 1;
    assert_eq!(
        inspect_read_only(&selected.directory, &mut budget, || Ok(()), |_| Ok(())).unwrap_err(),
        "journal_summary_limit"
    );
    assert_eq!(fingerprints(&selected.directory), before);
    let stop = Cancel::default();
    stop.cancel();
    assert!(root.catalog().summaries(&stop).is_err());
    assert_eq!(fingerprints(&selected.directory), before);
    let writer = RefCell::new(writer);
    let calls = Cell::new(0);
    let mut budget = 64 * 1024 * 1024;
    let result = inspect_read_only(
        &selected.directory,
        &mut budget,
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                writer.borrow_mut().append(&events[2]).unwrap();
            }
            Ok(())
        },
        |_| Ok(()),
    );
    assert_eq!(result.unwrap_err(), "journal_write_conflict");
    let mut budget = 64 * 1024 * 1024;
    inspect_read_only(&selected.directory, &mut budget, || Ok(()), |_| Ok(())).unwrap();
}
#[test]
fn valid_frames_with_unrelated_response_or_terminal_stage_violation_are_unavailable() {
    let data = golden();
    let events = data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap();
    let response_index = events
        .iter()
        .rposition(|event| event.text_field("kind") == Ok("response"))
        .unwrap();
    let done_index = events
        .iter()
        .position(|event| event.text_field("kind") == Ok("done"))
        .unwrap();
    let mut unrelated = events.to_vec();
    let native = unrelated[response_index].field("response").unwrap();
    let request = native.field("request").unwrap();
    let binding = request
        .field("binding")
        .unwrap()
        .with(
            "account",
            Json::string("unrelated-synthetic-account").unwrap(),
        )
        .unwrap();
    let request = request.with("binding", binding).unwrap();
    let native = native.with("request", request).unwrap();
    // The production native codec admits this response for its own request;
    // it must still fail the summary's link to the accepted conversation.
    ansa::session::codec_v1::load_response(&native).unwrap();
    unrelated[response_index] = unrelated[response_index].with("response", native).unwrap();
    let mut cancelled = events.to_vec();
    cancelled.insert(
        done_index,
        events[done_index]
            .with("kind", Json::string("cancel").unwrap())
            .unwrap(),
    );
    let mut missing_response = events.to_vec();
    missing_response.remove(response_index);
    let mut after_completed = events.to_vec();
    after_completed.push(events[response_index].clone());
    for (name, forged) in [
        ("unrelated-request", unrelated),
        ("cancel-then-done", cancelled),
        ("done-before-response", missing_response),
        ("response-after-done", after_completed),
    ] {
        let root = Root::new();
        let catalog = root.catalog();
        let selected = catalog.create_named("fixture").unwrap();
        let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
        for event in &forged {
            writer.append(event).unwrap();
        }
        let before = fingerprints(&selected.directory);
        let mut budget = 64 * 1024 * 1024;
        inspect_read_only(&selected.directory, &mut budget, || Ok(()), |_| Ok(())).unwrap();
        let summary = catalog.summaries(&Cancel::default()).unwrap().remove(0);
        assert_eq!(summary.status, SummaryStatus::Unavailable, "{name}");
        assert_eq!(summary.title, None, "{name}");
        assert_eq!(fingerprints(&selected.directory), before, "{name}");
    }
}
#[test]
fn jointly_forged_accepted_and_response_requests_match_production_replay_rejection() {
    let data = golden();
    let events = data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap();
    let configuration = Request::load(events[0].field("configuration").unwrap()).unwrap();
    let replay = || {
        State::new(
            "fixture".into(),
            configuration.clone(),
            Advertisement {
                revision: String::new(),
                descriptors: vec![],
            },
        )
    };
    let mut valid = replay();
    for event in events {
        valid.apply(event).unwrap();
    }
    assert_eq!(valid.turn.as_ref().unwrap().stage, "completed");
    for name in ["model", "epoch", "tools", "history", "effort", "all"] {
        let forge = |value: &Json| {
            let mut request = Request::load(value).unwrap();
            if matches!(name, "model" | "all") {
                request.binding.requested_model = "forged-synthetic-model".into();
                request.binding.returned_model = "forged-synthetic-model".into();
            }
            if matches!(name, "epoch" | "all") {
                request.epoch.id = "forged-synthetic-epoch".into();
                request.epoch.instructions = "Forged synthetic instructions".into();
                request.epoch.exact_state = Json::parse(r#"{"forged":true}"#).unwrap();
            }
            if matches!(name, "tools" | "all") {
                request.tools.revision = "forged-synthetic-tools".into();
            }
            if matches!(name, "history" | "all") {
                request.input.insert(
                    0,
                    Json::parse(r#"{"role":"user","content":"Forged earlier history"}"#).unwrap(),
                );
            }
            if matches!(name, "effort" | "all") {
                request.effort = Some("high".into());
            }
            let original = Request::load(value).unwrap();
            assert_eq!(request.binding.provider, original.binding.provider);
            assert_eq!(request.binding.account, original.binding.account);
            assert_eq!(request.input.last(), original.input.last());
            request.save().unwrap()
        };
        let mut forged = events.to_vec();
        for event in &mut forged {
            match event.text_field("kind").unwrap() {
                "accepted" => {
                    *event = event
                        .with("next", forge(event.field("next").unwrap()))
                        .unwrap()
                }
                "response" => {
                    let native = event.field("response").unwrap();
                    let request = forge(native.field("request").unwrap());
                    let model = request
                        .field("binding")
                        .unwrap()
                        .field("returned_model")
                        .unwrap()
                        .clone();
                    let native = native
                        .with("request", request)
                        .unwrap()
                        .with("model", model)
                        .unwrap();
                    // Each forged request/response pair is independently valid.
                    ansa::session::codec_v1::load_response(&native).unwrap();
                    *event = event.with("response", native).unwrap();
                }
                _ => {}
            }
        }
        let mut restored = replay();
        restored.apply(&forged[0]).unwrap();
        assert_eq!(
            restored.apply(&forged[1]),
            Err("session_request_conflict"),
            "{name}"
        );
        let root = Root::new();
        let catalog = root.catalog();
        let selected = catalog.create_named("fixture").unwrap();
        let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
        for event in &forged {
            writer.append(event).unwrap();
        }
        let before = fingerprints(&selected.directory);
        let mut budget = 64 * 1024 * 1024;
        inspect_read_only(&selected.directory, &mut budget, || Ok(()), |_| Ok(())).unwrap();
        let summary = catalog.summaries(&Cancel::default()).unwrap().remove(0);
        assert_eq!(summary.status, SummaryStatus::Unavailable, "{name}");
        assert_eq!(summary.title, None, "{name}");
        assert_eq!(fingerprints(&selected.directory), before, "{name}");
    }
}
#[test]
fn plan_summary_without_a_verified_compiled_catalogue_is_unavailable() {
    let data = golden();
    let events = data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap();
    let configuration = Request::load(events[0].field("configuration").unwrap()).unwrap();
    // This committed fixture tool is read-only; the production state validates
    // its plan requests only when the caller supplies that trusted catalogue.
    let mut trusted = State::new(
        "fixture".into(),
        configuration.clone(),
        configuration.tools.clone(),
    );
    trusted.apply(&events[0]).unwrap();
    let accepted = &events[1];
    let plan_request = trusted
        .request_for(accepted.text_field("text").unwrap(), AgentMode::Plan)
        .unwrap()
        .save()
        .unwrap();
    let mut plan = events.to_vec();
    plan[1] = accepted
        .with("mode", Json::string("plan").unwrap())
        .unwrap()
        .with("next", plan_request.clone())
        .unwrap();
    for event in &mut plan[2..] {
        if event.text_field("kind") == Ok("response") {
            let native = event.field("response").unwrap();
            let request = native
                .field("request")
                .unwrap()
                .with("epoch", plan_request.field("epoch").unwrap().clone())
                .unwrap();
            *event = event
                .with("response", native.with("request", request).unwrap())
                .unwrap();
        }
    }
    for event in &plan[1..] {
        trusted.apply(event).unwrap();
    }
    assert_eq!(trusted.turn.as_ref().unwrap().stage, "completed");
    let mut unavailable = State::new(
        "fixture".into(),
        configuration,
        Advertisement {
            revision: String::new(),
            descriptors: vec![],
        },
    );
    unavailable.apply(&plan[0]).unwrap();
    assert_eq!(unavailable.apply(&plan[1]), Err("session_plan_tools"));
    let root = Root::new();
    let catalog = root.catalog();
    let selected = catalog.create_named("fixture").unwrap();
    let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
    for event in &plan {
        writer.append(event).unwrap();
    }
    let before = fingerprints(&selected.directory);
    let summary = catalog.summaries(&Cancel::default()).unwrap().remove(0);
    assert_eq!(summary.status, SummaryStatus::Unavailable);
    assert_eq!(fingerprints(&selected.directory), before);
}
#[test]
fn final_inspection_recheck_detects_same_length_rewrite_of_an_earlier_part() {
    use std::{cell::Cell, fs::FileTimes, time::UNIX_EPOCH};
    let root = Root::new();
    let selected = root.catalog().create_named("fixture").unwrap();
    let data = golden();
    let events = data
        .field("traces")
        .unwrap()
        .field("completed")
        .unwrap()
        .elements()
        .unwrap();
    let first = selected.directory.join("part-00000000000000000000.log");
    {
        let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
        writer.append(&events[0]).unwrap();
        writer.append(&events[1]).unwrap();
    }
    OpenOptions::new()
        .append(true)
        .open(&first)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    {
        let mut writer = Journal::open(&selected.directory, |_| Ok(()), None).unwrap();
        writer.append(&events[2]).unwrap();
    }
    let before = fs::metadata(&first).unwrap();
    let mut rewritten = fs::read(&first).unwrap();
    *rewritten.last_mut().unwrap() ^= 1;
    let changed = Cell::new(false);
    let mut budget = 64 * 1024 * 1024;
    let result = inspect_read_only(
        &selected.directory,
        &mut budget,
        || Ok(()),
        |event| {
            if event.text_field("kind") == Ok("response") {
                // Part zero was read and checked before part one's replay starts.
                // Keep its inode and length, and choose a distinct mtime without a
                // timing sleep so even coarse fixture filesystems are deterministic.
                let mut file = OpenOptions::new().write(true).open(&first).unwrap();
                file.write_all(&rewritten).unwrap();
                file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
                    .unwrap();
                file.sync_all().unwrap();
                changed.set(true);
            }
            Ok(())
        },
    );
    assert!(changed.get());
    assert_eq!(result.unwrap_err(), "journal_write_conflict");
    let after = fs::metadata(&first).unwrap();
    assert_eq!(
        (after.dev(), after.ino(), after.len(), after.nlink()),
        (before.dev(), before.ino(), before.len(), before.nlink())
    );
}
