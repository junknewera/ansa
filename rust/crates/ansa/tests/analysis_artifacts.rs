#[allow(dead_code)]
#[path = "support/tools_fixture.rs"]
mod local;
#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}
use ansa::{
    config::agent_epoch,
    protocol::json::Json,
    provider::{native, replay::Effects, request::Request},
    runtime::*,
    session::journal_v1::{Observer, Point},
    tools::{artifacts::Store, files::local_tools, registry::Snapshot},
};
use local::{Root, s};
use std::{
    collections::BTreeMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use support::runtime_fixture as f;

fn tools(root: &Root) -> Arc<Snapshot> {
    let settings = root.settings();
    local_tools(
        settings.clone(),
        agent_epoch(&settings).unwrap(),
        &root.0.join("session"),
        &root.0.join("auth"),
        &local::worker(),
    )
    .unwrap()
}
fn args(root: &Root, path: &str, key: &str) -> Json {
    Json::object(vec![
        ("path", s(path)),
        ("date_column", s("date")),
        ("value_column", s("value")),
        ("report_key", s(key)),
        ("instruction_token", s(&local::token(root))),
    ])
    .unwrap()
}
async fn runtime(
    root: &Root,
    args: Json,
    observer: Option<Observer>,
    calls: Arc<AtomicUsize>,
) -> Runtime {
    let tools = tools(root);
    let configuration = Request::new(
        ansa::frontend::app::binding(),
        agent_epoch(&root.settings()).unwrap(),
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let generator: Generate = Arc::new(move |request, _, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        if request
            .input
            .last()
            .is_some_and(|item| item.text_field("type").ok() == Some("function_call_output"))
        {
            return native::load(&f::saved_response(request, Some("ANALYSIS_DONE"), 0));
        }
        native::load(&Json::object(vec![
            ("request", request.save()?),
            ("id", s("response")),
            ("model", s("gpt-6.1-sol")),
            (
                "items",
                Json::array(vec![Json::object(vec![
                    ("type", s("function_call")),
                    ("id", s("item")),
                    ("call_id", s("call")),
                    ("name", s("python_execute")),
                    ("namespace", s("omh")),
                    ("arguments", s(&args.dump())),
                    ("status", s("completed")),
                ])?])?,
            ),
        ])?)
    });
    let runtime = Runtime::new(
        "session".into(),
        configuration,
        tools,
        generator,
        Limits {
            tool_rounds: 4,
            ..Limits::default()
        },
        Options {
            directory: Some(root.0.join("session")),
            journal: observer,
            ..Options::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    runtime
}
async fn outcome(handle: &Handle) -> (Json, OperationId) {
    let event = f::until(handle, |p| matches!(p, EventPayload::ToolFinished(_))).await;
    let EventPayload::ToolFinished(result) = event.payload else {
        unreachable!()
    };
    assert_eq!(result.effects, Effects::KnownResult, "{}", result.output);
    (Json::parse(&result.output).unwrap(), result.operation)
}
fn files(path: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                return None;
            }
            Some((
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            ))
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn monthly_analysis_publishes_exact_table_report_png_and_reopens_without_execution() {
    let root = Root::new();
    local::write(
        &root.cwd().join("data.csv"),
        "date,value,note\n2026-01-02,0.1,\"Ж,界\"\n2026-01-03,0.2,x\n2026-02-01,-0.05,y\n2026-02-02,,missing\n",
    );
    let catalog = tools(&root);
    assert!(catalog.read_only().definition("dataset_inspect").is_some());
    assert!(catalog.read_only().definition("python_execute").is_none());
    let inspect = local::invoke(
        &root,
        "dataset_inspect",
        Json::object(vec![
            ("path", s("data.csv")),
            ("instruction_token", s(&local::token(&root))),
        ])
        .unwrap(),
    );
    assert_eq!(inspect.usize_field("rows").unwrap(), 4);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut run = runtime(
        &root,
        args(&root, "data.csv", "monthly"),
        None,
        calls.clone(),
    )
    .await;
    f::submit(&run.handle(), "analysis", "aggregate", "turn").await;
    let (result, operation) = outcome(&run.handle()).await;
    assert_eq!(
        result.text_field("status").unwrap(),
        "ready",
        "{}",
        result.dump()
    );
    assert_eq!(
        result
            .field("summary")
            .unwrap()
            .text_field("total")
            .unwrap(),
        "0.25"
    );
    assert_eq!(
        result
            .field("summary")
            .unwrap()
            .usize_field("missing")
            .unwrap(),
        1
    );
    f::until(&run.handle(), |p| matches!(p, EventPayload::Done)).await;
    let store = Store::new(&root.0.join("session")).unwrap();
    let descriptors = result.field("artifacts").unwrap().elements().unwrap();
    assert_eq!(descriptors.len(), 3);
    for descriptor in descriptors {
        assert_eq!(
            descriptor.text_field("producer_operation_id").unwrap(),
            operation.0
        );
        store.resolve(descriptor).unwrap();
    }
    let table = store.resolve(&descriptors[1]).unwrap();
    assert_eq!(
        fs::read_to_string(&table).unwrap(),
        "month,total\n2026-01,0.3\n2026-02,-0.05\n".replace('\n', "\r\n")
    );
    let job = table.parent().unwrap();
    let before = files(job);
    local::write(&root.cwd().join("data.csv"), "date,value\n2026-01-01,2\n");
    f::submit(
        &run.handle(),
        "refine",
        "refine monthly report",
        "turn_refine",
    )
    .await;
    let (refined, _) = outcome(&run.handle()).await;
    assert_eq!(
        refined
            .field("summary")
            .unwrap()
            .text_field("total")
            .unwrap(),
        "2"
    );
    let revisions = refined.field("artifacts").unwrap().elements().unwrap();
    for (old, new) in descriptors.iter().zip(revisions) {
        assert_eq!(
            old.text_field("artifact_id").unwrap(),
            new.text_field("artifact_id").unwrap()
        );
        assert_ne!(
            old.text_field("revision").unwrap(),
            new.text_field("revision").unwrap()
        );
        store.resolve(old).unwrap();
        store.resolve(new).unwrap();
    }
    f::until(&run.handle(), |p| matches!(p, EventPayload::Done)).await;
    f::finish(&mut run).await;
    let resumed_calls = Arc::new(AtomicUsize::new(0));
    let mut reopened = runtime(
        &root,
        args(&root, "data.csv", "monthly"),
        None,
        resumed_calls.clone(),
    )
    .await;
    for descriptor in descriptors {
        store.resolve(descriptor).unwrap();
    }
    assert_eq!(files(job), before);
    assert_eq!(resumed_calls.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    for key in ["unfinished_one", "unfinished_two"] {
        let revision = ansa::protocol::legacy_json_v1::sha256(key.as_bytes());
        ansa_linux::fs::create_private(&root.0.join("session/artifacts").join(revision)).unwrap();
    }
    assert!(matches!(store.reserve(), Err("artifact_quota")));
    assert_eq!(files(job), before, "quota deleted referenced originals");
    let alias = root.cwd().join("linked-table.csv");
    fs::hard_link(&table, &alias).unwrap();
    assert!(matches!(
        store.resolve(&descriptors[1]),
        Err("artifact_file_permissions")
    ));
    fs::remove_file(alias).unwrap();
    let forged = descriptors[1]
        .with("storage_ref", s("../outside.csv"))
        .unwrap();
    assert!(matches!(store.resolve(&forged), Err("artifact_descriptor")));
    let foreign = Store::new(&root.0.join("different_session")).unwrap();
    assert!(matches!(
        foreign.resolve(&descriptors[1]),
        Err("artifact_descriptor")
    ));
    fs::write(&table, "CORRUPTED").unwrap();
    assert!(matches!(
        store.resolve(&descriptors[1]),
        Err("artifact_corrupt")
    ));
    fs::remove_file(store.resolve(&descriptors[2]).unwrap()).unwrap();
    assert!(matches!(
        store.resolve(&descriptors[2]),
        Err("artifact_missing")
    ));
    assert_eq!(resumed_calls.load(Ordering::SeqCst), 0);
    f::finish(&mut reopened).await;
}

#[tokio::test(flavor = "current_thread")]
async fn completed_worker_with_lost_outcome_is_reconciled_after_restart_without_rerun() {
    let root = Root::new();
    local::write(
        &root.cwd().join("data.csv"),
        "date,value\n2026-01-01,12.3\n",
    );
    let fail = Arc::new(AtomicBool::new(true));
    let inject = fail.clone();
    let observer = Arc::new(move |point, kind: &str| {
        if point == Point::BeforeWrite && kind == "outcome" && inject.swap(false, Ordering::SeqCst)
        {
            Err("injected_outcome_failure")
        } else {
            Ok(())
        }
    });
    let mut run = runtime(
        &root,
        args(&root, "data.csv", "recover"),
        Some(observer),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    f::submit(&run.handle(), "analysis", "aggregate", "turn").await;
    let (result, _) = outcome(&run.handle()).await;
    loop {
        match f::event(&run.handle()).await.payload {
            EventPayload::FrontendSnapshot(s) if s.storage_failed => break,
            EventPayload::RuntimeError { diagnostic, .. } => {
                assert_eq!(diagnostic, "injected_outcome_failure")
            }
            _ => {}
        }
    }
    let store = Store::new(&root.0.join("session")).unwrap();
    let descriptors = result.field("artifacts").unwrap().elements().unwrap();
    let report = store.resolve(&descriptors[0]).unwrap();
    let before = files(report.parent().unwrap());
    f::finish(&mut run).await;
    fs::remove_file(report.parent().unwrap().join("receipt.json")).unwrap();
    local::write(&root.cwd().join("data.csv"), "date,value\n2026-01-01,999\n");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut reopened = runtime(
        &root,
        args(&root, "data.csv", "recover"),
        None,
        calls.clone(),
    )
    .await;
    let snapshot = f::snapshot(&reopened.handle(), "recovered").await;
    assert!(!snapshot.storage_failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(files(report.parent().unwrap()), before);
    for descriptor in descriptors {
        store.resolve(descriptor).unwrap();
    }
    let journal = fs::read_dir(root.0.join("session"))
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            p.file_name()?
                .to_str()?
                .starts_with("part-")
                .then(|| fs::read_to_string(p).unwrap())
        })
        .collect::<String>();
    assert!(journal.contains("12.3") && journal.contains("outcome"));
    f::finish(&mut reopened).await;
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_or_missing_dataset_is_known_and_does_not_start_a_worker() {
    let root = Root::new();
    for (index, path) in ["missing.csv", "invalid.csv", "nested/data.csv"]
        .into_iter()
        .enumerate()
    {
        if index == 1 {
            local::write(&root.cwd().join(path), "date,value\n\"unterminated");
        }
        if index == 2 {
            fs::create_dir(root.cwd().join("nested")).unwrap();
            local::write(&root.cwd().join("nested/AGENTS.md"), &"x".repeat(8193));
            local::write(&root.cwd().join(path), "date,value\n2026-01-01,1\n");
        }
        let mut run = runtime(
            &root,
            args(&root, path, "invalid"),
            None,
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        f::submit(
            &run.handle(),
            &format!("analysis_{index}"),
            "aggregate",
            &format!("turn_{index}"),
        )
        .await;
        let (result, _) = outcome(&run.handle()).await;
        assert!(result.has("error"));
        f::until(&run.handle(), |p| matches!(p, EventPayload::Done)).await;
        assert!(!root.0.join("session/artifacts").exists());
        assert!(
            fs::read_dir(root.0.join("session/operations"))
                .unwrap()
                .next()
                .is_none()
        );
        f::finish(&mut run).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn worker_nonzero_and_cancellation_have_distinct_known_execution_status() {
    let root = Root::new();
    local::write(
        &root.cwd().join("bad-date.csv"),
        "date,value\n2026-13-01,1\n",
    );
    let mut run = runtime(
        &root,
        args(&root, "bad-date.csv", "invalid_date"),
        None,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    f::submit(&run.handle(), "bad", "aggregate", "turn_bad").await;
    let event = f::until(&run.handle(), |p| {
        matches!(p, EventPayload::ToolFinished(_))
    })
    .await;
    let EventPayload::ToolFinished(failed) = event.payload else {
        unreachable!()
    };
    assert_eq!(failed.effects, Effects::KnownResult);
    assert_eq!(failed.execution, ansa::provider::replay::Execution::Failed);
    assert_eq!(
        Json::parse(&failed.output)
            .unwrap()
            .text_field("error")
            .unwrap(),
        "analysis_failed"
    );
    f::until(&run.handle(), |p| matches!(p, EventPayload::Done)).await;
    f::finish(&mut run).await;

    let root = Root::new();
    local::write(
        &root.cwd().join("data.csv"),
        &format!("date,value\n{}", "2026-01-01,1\n".repeat(50000)),
    );
    let mut run = runtime(
        &root,
        args(&root, "data.csv", "cancelled"),
        None,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let target = f::submit(&run.handle(), "cancelled", "aggregate", "turn_cancelled").await;
    let event = f::until(&run.handle(), |p| matches!(p, EventPayload::ToolStarted(_))).await;
    let EventPayload::ToolStarted(started) = event.payload else {
        unreachable!()
    };
    let revision = ansa::protocol::legacy_json_v1::sha256(started.operation.0.as_bytes());
    let started_path = root
        .0
        .join("session/operations")
        .join(revision)
        .join("started.json");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !started_path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("analysis did not start");
    f::post(
        &run.handle(),
        "cancel",
        CommandPayload::Cancel(target),
        "turn_cancelled",
        PostStatus::Accepted,
    )
    .await;
    let event = f::until(&run.handle(), |p| {
        matches!(p, EventPayload::ToolFinished(_))
    })
    .await;
    let EventPayload::ToolFinished(cancelled) = event.payload else {
        unreachable!()
    };
    assert_eq!(cancelled.effects, Effects::KnownResult);
    assert_eq!(
        cancelled.execution,
        ansa::provider::replay::Execution::Cancelled
    );
    assert_eq!(
        Json::parse(&cancelled.output)
            .unwrap()
            .text_field("error")
            .unwrap(),
        "analysis_cancelled"
    );
    f::finish(&mut run).await;
}

#[tokio::test(flavor = "current_thread")]
async fn corrupt_recovery_inputs_and_outputs_are_unavailable_and_never_reexecuted() {
    for filename in [
        "parameters.json",
        "worker.py",
        "input.csv",
        "table.csv",
        "chart.png",
    ] {
        let root = Root::new();
        local::write(&root.cwd().join("data.csv"), "date,value\n2026-01-01,1\n");
        let observer = Arc::new(|point, kind: &str| {
            if point == Point::BeforeWrite && kind == "outcome" {
                Err("injected_outcome_failure")
            } else {
                Ok(())
            }
        });
        let mut run = runtime(
            &root,
            args(&root, "data.csv", "corrupt"),
            Some(observer),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        f::submit(&run.handle(), "analysis", "aggregate", "turn").await;
        let (result, _) = outcome(&run.handle()).await;
        loop {
            if matches!(f::event(&run.handle()).await.payload,EventPayload::FrontendSnapshot(s) if s.storage_failed)
            {
                break;
            }
        }
        f::finish(&mut run).await;
        let store = Store::new(&root.0.join("session")).unwrap();
        let descriptors = result.field("artifacts").unwrap().elements().unwrap();
        let report = store.resolve(&descriptors[0]).unwrap();
        let job = report.parent().unwrap();
        fs::remove_file(job.join("receipt.json")).unwrap();
        let damaged = if filename == "parameters.json" {
            br#"{"date_column":"date","value_column":"other"}"#.as_slice()
        } else {
            b"CORRUPTED"
        };
        fs::write(job.join(filename), damaged).unwrap();
        let before = files(job);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut reopened = runtime(
            &root,
            args(&root, "data.csv", "corrupt"),
            None,
            calls.clone(),
        )
        .await;
        let view = f::snapshot(&reopened.handle(), "failed_recovery").await;
        assert_eq!(view.state, "paused", "{filename}: {view:?}");
        assert!(view.recovery_operation.is_some(), "{filename}: {view:?}");
        assert_eq!(
            view.diagnostic, "operation_recovery_failed",
            "{filename}: {view:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            files(job),
            before,
            "{filename}: recovery changed a consumed job"
        );
        assert!(store.resolve(&descriptors[0]).is_err());
        f::finish(&mut reopened).await;
    }
}
