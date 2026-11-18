#[allow(dead_code)]
mod support {
    pub mod runtime_fixture;
}

use ansa::{
    orchestration::{
        plan::{Plan, Step},
        run::{DispatchBudget, Run, RunId},
    },
    provider::{native, structured::FinalFormat},
    runtime::*,
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::runtime_fixture as fixture;

async fn child(
    path: &Path,
    id: &str,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<fixture::Gate>>,
    existing: Option<DirectoryIdentity>,
) -> Runtime {
    let tools = fixture::tools(Arc::new(AtomicUsize::new(0)), None, false);
    let configuration =
        fixture::configuration(&tools).with_final_format(Some(FinalFormat::ChildResult));
    let generate: Generate = Arc::new(move |request, stop, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &gate {
            gate.hold(stop);
        }
        native::load(&fixture::saved_response(
            request,
            Some(
                r#"{"status":"completed","summary":"Inspected fixture","findings":["Evidence found"]}"#,
            ),
            0,
        ))
    });
    let runtime = Runtime::new(
        id.into(),
        configuration,
        tools,
        generate,
        Limits {
            submit_ids: 1,
            ..Limits::default()
        },
        Options {
            directory: Some(path.to_owned()),
            existing_journal: existing,
            ..Options::default()
        },
    )
    .unwrap();
    runtime.ready().await.unwrap();
    runtime
}

async fn receipt(runtime: &Runtime, step: &Step) -> ChildAttempt {
    runtime
        .handle()
        .child_attempt("task".into(), "turn".into(), step.assignment())
        .await
        .unwrap()
}

struct Release([Arc<fixture::Gate>; 2]);
impl Drop for Release {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lost_parent_receipt_is_reconciled_from_two_original_native_children() {
    let root = fixture::Root::new();
    let journal = root.0.join("run");
    let paths = [root.0.join("child-input"), root.0.join("child-format")];
    let identity = RunId {
        parent_session: "parent".into(),
        run: "report-run".into(),
    };
    let plan = Plan::parse(r#"{
        "schema_version":1,
        "summary":"Find out why the report is empty",
        "steps":[
            {"id":"input","task":"Inspect input handling","access":"read","paths":["input"],"depends_on":[],"acceptance":"Identify input causes"},
            {"id":"format","task":"Inspect report formatting","access":"read","paths":["report"],"depends_on":[],"acceptance":"Identify formatting causes"},
            {"id":"fix","task":"Fix the smallest confirmed cause","access":"write","paths":["report"],"depends_on":["input","format"],"acceptance":"Report no longer comes back empty"}
        ]
    }"#).unwrap();
    let mut run = Run::create(
        &journal,
        identity.clone(),
        plan.clone(),
        DispatchBudget {
            started_at: 1,
            deadline: 100,
            submissions: 4,
        },
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let gates = [
        Arc::new(fixture::Gate::default()),
        Arc::new(fixture::Gate::default()),
    ];
    let mut one = child(
        &paths[0],
        "input-agent",
        calls.clone(),
        Some(gates[0].clone()),
        None,
    )
    .await;
    let mut two = child(
        &paths[1],
        "format-agent",
        calls.clone(),
        Some(gates[1].clone()),
        None,
    )
    .await;
    let _release = Release(gates.clone());
    let first = receipt(&one, &plan.steps()[0]).await;
    let second = receipt(&two, &plan.steps()[1]).await;
    assert_eq!(run.bind("input", &first), Err("run_dispatch_unavailable"));
    assert_eq!(run.approve("stale-preview"), Err("run_transition"));
    let approved = run.plan_digest().to_owned();
    run.approve(&approved).unwrap();
    let altered = one
        .handle()
        .child_attempt(
            "task".into(),
            "turn".into(),
            Submit {
                text: "A task never shown for approval".into(),
                mode: AgentMode::Plan,
            },
        )
        .await
        .unwrap();
    assert_eq!(run.bind("input", &altered), Err("run_child_binding"));
    let steps = ["input", "format"];
    for (index, (runtime, proof)) in [(&one, &first), (&two, &second)].into_iter().enumerate() {
        run.bind(steps[index], proof).unwrap();
        let stop = Cancel::default();
        assert_eq!(
            run.dispatch(
                steps[index],
                proof,
                &runtime.handle(),
                2 + index as u64,
                &stop
            )
            .await
            .unwrap(),
            PostStatus::Accepted
        );
    }
    assert!(run.ready().unwrap().is_empty());
    for gate in &gates {
        gate.wait();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for gate in &gates {
        gate.release();
    }
    for runtime in [&one, &two] {
        fixture::until(&runtime.handle(), |p| matches!(p, EventPayload::Done)).await;
    }
    assert!(run.result("input").is_none());
    drop(run);
    fixture::finish(&mut one).await;
    fixture::finish(&mut two).await;

    let mut run = Run::open(&journal, &identity).unwrap();
    assert_eq!(run.ready(), Err("run_reconciliation_required"));
    assert_eq!(run.continue_run(&[]), Err("run_child_reconciliation"));
    let mut one = child(
        &paths[0],
        "input-agent",
        calls.clone(),
        None,
        Some(first.journal()),
    )
    .await;
    let mut two = child(
        &paths[1],
        "format-agent",
        calls.clone(),
        None,
        Some(second.journal()),
    )
    .await;
    let first = receipt(&one, &plan.steps()[0]).await;
    let second = receipt(&two, &plan.steps()[1]).await;
    let requests: Vec<_> = [&first, &second]
        .into_iter()
        .map(|proof| match proof.state() {
            ChildAttemptState::Durable {
                request,
                quiescent: true,
                ..
            } => request,
            other => panic!("unexpected native receipt {other:?}"),
        })
        .collect();
    assert_eq!(
        requests[0], requests[1],
        "request ID alone cannot identify the child"
    );
    assert_eq!(run.observe("input", &second), Err("run_child_binding"));
    run.continue_run(&[("input", &first), ("format", &second)])
        .unwrap();
    assert_eq!(run.ready().unwrap()[0].id, "fix");
    assert_eq!(run.result("input").unwrap().summary, "Inspected fixture");
    assert_eq!(run.result("format").unwrap().findings, ["Evidence found"]);
    run.observe("input", &first).unwrap();
    assert_eq!(
        run.dispatch("input", &first, &one.handle(), 4, &Cancel::default())
            .await,
        Err("run_child_already_admitted")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(run);
    let mut run = Run::open(&journal, &identity).unwrap();
    assert!(run.result("input").is_none());
    assert_eq!(run.continue_run(&[]), Err("run_child_reconciliation"));
    run.continue_run(&[("input", &first), ("format", &second)])
        .unwrap();
    assert!(run.result("input").is_some());
    fixture::finish(&mut one).await;
    fixture::finish(&mut two).await;
}
