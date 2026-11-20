use super::*;
use crate::{
    config::{AgentSettings, agent_epoch},
    protocol::json::Json,
    provider::{
        native,
        request::{Binding, Request},
    },
    runtime::{
        AgentMode, CommandPayload, EventPayload, Generate, Limits, Options, PostStatus, Runtime,
        RuntimeCommand, SessionId, Submit,
    },
    tools::registry::Registry,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ansa-profile-{}-{nonce}", std::process::id()));
        ansa_linux::fs::create_private_leaf(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn profile(guidance: &str, policy: &str) -> Vec<u8> {
    encode(&json!({
        "version": 1,
        "prompt_policy": policy,
        "guidance": guidance,
        "skills": [],
        "memory": []
    }))
    .unwrap()
}

fn plan() -> Vec<u8> {
    let task = |prompt: &str| {
        json!({
            "prompt_sha256": sha256(prompt.as_bytes()),
            "inputs": {"input": sha256(b"fixture")},
            "checks": {"result": sha256(b"expected")}
        })
    };
    let budget = |calls| {
        json!({
            "calls": calls,
            "tools": 0,
            "compactions": 0,
            "estimated_input_tokens": 1_000_000
        })
    };
    encode(&json!({
        "version": 1,
        "repetitions": 2,
        "development": {"development": task("development")},
        "holdout": {"holdout": task("holdout")},
        "per_trial": budget(1),
        "total": budget(8)
    }))
    .unwrap()
}

enum Mutation {
    None,
    WrongPolicy,
    WarmContext,
}

async fn capture(
    root: &Path,
    settings: &AgentSettings,
    trial: &str,
    session: String,
    prompt: &str,
    artifact: &[u8],
    mutation: Mutation,
) {
    ansa_linux::fs::create_private(root).unwrap();
    fs::create_dir_all(root.join("inputs")).unwrap();
    fs::create_dir_all(root.join("artifacts")).unwrap();
    fs::write(root.join("inputs/input"), b"fixture").unwrap();
    fs::write(root.join("artifacts/result"), artifact).unwrap();
    let tools = Registry::new().snapshot();
    let mut epoch = agent_epoch(settings).unwrap();
    crate::harness::select(settings, None, Some(trial))
        .unwrap()
        .unwrap()
        .attach(&mut epoch)
        .unwrap();
    if matches!(mutation, Mutation::WrongPolicy) {
        epoch.exact_state = epoch.exact_state.without("prompt_policy").unwrap();
    }
    let request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-host-fixture".into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let fork_seed = if matches!(mutation, Mutation::WarmContext) {
        // Carbon creates a fork through the source state even before its first turn.
        let mut source = crate::session::state::State::new(
            "fixture-source".into(),
            request.clone(),
            tools.advertisement().clone(),
        );
        source.apply(&source.initialization().unwrap()).unwrap();
        Some(source.fork_seed().unwrap())
    } else {
        None
    };
    let generate: Generate = Arc::new(|request, _, _| {
        let items = Json::parse(
            r#"[{"type":"message","id":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"fixture result"}]}]"#,
        )?;
        native::load(&Json::object(vec![
            ("request", request.save()?),
            ("model", Json::string(&request.binding.returned_model)?),
            ("id", Json::string("response")?),
            ("items", items),
        ])?)
    });
    let mut runtime = Runtime::new(
        SessionId(session.clone()),
        request,
        tools,
        generate,
        Limits::default(),
        Options {
            directory: Some(root.join("session")),
            fork_seed,
            ..Options::default()
        },
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.ready())
        .await
        .unwrap()
        .unwrap();
    let handle = runtime.handle();
    let submitted = handle
        .post(RuntimeCommand {
            schema_version: 1,
            session: SessionId(session),
            turn: "turn".into(),
            incarnation: handle.incarnation().clone(),
            command: "submit".into(),
            payload: CommandPayload::Submit(Submit {
                text: prompt.into(),
                mode: AgentMode::Basic,
            }),
        })
        .await
        .unwrap();
    assert_eq!(submitted, PostStatus::Accepted);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match runtime.next_event().await.unwrap().payload {
                EventPayload::Done => break,
                EventPayload::RuntimeError { diagnostic, .. } => panic!("{diagnostic}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn host_evaluation_recovers_adopts_once_and_rolls_back() {
    let temp = Temp::new();
    let path = temp.0.join("project");
    ansa_linux::fs::create_private_leaf(&path).unwrap();
    let scope = crate::harness::scope(&path);
    let settings = AgentSettings::defaults(&path, &temp.0);
    let store = settings.directory.join("harness-profiles").join(&scope);
    ansa_linux::fs::create_private(&store).unwrap();
    let directory = ansa_linux::fs::private_directory(&store).unwrap();
    let run = |args: &[&str], stdin: &[u8]| {
        command(
            &directory,
            &scope,
            &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            stdin,
        )
    };

    local::create(&directory, "activation-0000.json.pending", b"incomplete").unwrap();
    assert_eq!(active(&directory, &scope).unwrap(), None);
    let baseline = run(&["baseline"], &profile("Original", "legacy-v1")).unwrap();
    let baseline_id = baseline["activation"]["id"].as_str().unwrap();
    let baseline_rev = baseline["activation"]["revision"].as_str().unwrap();
    let plan = plan();
    assert_eq!(
        run(&["freeze", &sha256(b"wrong")], &plan),
        Err("harness_stale_preimage")
    );
    let frozen = run(&["freeze", baseline_rev], &plan).unwrap();
    let freeze = frozen["freeze"].as_str().unwrap();
    let candidate_profile = profile("Original", "handoff-v1");
    let stage = run(&["stage", freeze], &candidate_profile).unwrap();
    let id = stage["proposal"].as_str().unwrap();
    let candidate = stage["candidate"].as_str().unwrap();
    assert_eq!(run(&["stage", freeze], &candidate_profile).unwrap(), stage);
    assert!(
        run(
            &["stage", freeze],
            &profile("Different candidate", "handoff-v1")
        )
        .is_err()
    );
    assert_eq!(run(&["adopt", id], b""), Err("harness_evaluation_required"));
    assert_eq!(
        active(&directory, &scope).unwrap().as_deref(),
        Some(baseline_rev)
    );

    let captures = temp.0.join("captures");
    ansa_linux::fs::create_private(&captures).unwrap();
    fs::write(captures.join("report.json"), br#"{"eligible":true}"#).unwrap();
    let captures_str = captures.to_str().unwrap();
    assert!(run(&["evaluate", id, captures_str], b"").is_err());
    for task in ["development", "holdout"] {
        for repetition in 1..=2 {
            for arm in ["baseline", "candidate"] {
                let root = captures.join(task).join(repetition.to_string()).join(arm);
                let session = format!("{task}-{repetition}-{arm}");
                let artifact = if task == "holdout" && arm == "baseline" {
                    b"failed".as_slice()
                } else {
                    b"expected"
                };
                let mutation = match (task, repetition, arm) {
                    ("development", 1, "candidate") => Mutation::WarmContext,
                    ("holdout", 2, "candidate") => Mutation::WrongPolicy,
                    _ => Mutation::None,
                };
                capture(
                    &root,
                    &settings,
                    &format!("{id}:{arm}"),
                    session,
                    task,
                    artifact,
                    mutation,
                )
                .await;
            }
        }
    }
    assert_eq!(
        run(&["evaluate", id, captures_str], b""),
        Err("harness_trial_context")
    );
    let warm = captures.join("development/1/candidate");
    fs::rename(warm.join("session"), warm.join("warm-session")).unwrap();
    capture(
        &warm,
        &settings,
        &format!("{id}:candidate"),
        "development-1-candidate-fixed".into(),
        "development",
        b"expected",
        Mutation::None,
    )
    .await;
    assert_eq!(
        run(&["evaluate", id, captures_str], b""),
        Err("harness_trial_policy")
    );
    let wrong = captures.join("holdout/2/candidate");
    fs::rename(wrong.join("session"), wrong.join("wrong-session")).unwrap();
    capture(
        &wrong,
        &settings,
        &format!("{id}:candidate"),
        "holdout-2-candidate-fixed".into(),
        "holdout",
        b"expected",
        Mutation::None,
    )
    .await;
    let graded = run(&["evaluate", id, captures_str], b"").unwrap();
    assert_eq!(graded["status"], "eligible");
    assert_eq!(graded["evaluation"]["trials"].as_array().unwrap().len(), 4);
    assert_eq!(run(&["evaluate", id, "/unknown"], b"").unwrap(), graded);
    assert_eq!(
        active(&directory, &scope).unwrap().as_deref(),
        Some(baseline_rev)
    );

    let reference_name = record_name("evaluation", id).unwrap();
    let reference_path = store.join(&reference_name);
    let reference: EvaluationRef = get(&directory, &reference_name).unwrap().unwrap();
    let mut forged = graded["evaluation"].clone();
    forged["trials"][0]["holdout"] = json!(true);
    let counterfeit = store_addressed(&directory, "graded", &forged).unwrap();
    let original_receipt = reference.receipt.clone();
    let mut reference = reference;
    reference.receipt = counterfeit;
    fs::write(&reference_path, encode(&reference).unwrap()).unwrap();
    assert_eq!(run(&["adopt", id], b""), Err("harness_evaluation_schedule"));
    forged["trials"][0]["holdout"] = json!(false);
    forged["trials"][0]["candidate"]["session"] =
        forged["trials"][0]["baseline"]["session"].clone();
    reference.receipt = store_addressed(&directory, "graded", &forged).unwrap();
    fs::write(&reference_path, encode(&reference).unwrap()).unwrap();
    assert_eq!(run(&["adopt", id], b""), Err("harness_evaluation_trial"));
    reference.receipt = original_receipt;
    fs::write(&reference_path, encode(&reference).unwrap()).unwrap();

    let result = captures.join("holdout/1/candidate/artifacts/result");
    fs::write(&result, b"post-evaluation change").unwrap();
    assert_eq!(run(&["adopt", id], b""), Err("harness_evaluation_changed"));
    assert_eq!(
        active(&directory, &scope).unwrap().as_deref(),
        Some(baseline_rev)
    );
    fs::write(&result, b"expected").unwrap();

    let decision = run(&["adopt", id], b"").unwrap();
    assert_eq!(run(&["adopt", id], b"").unwrap(), decision);
    assert_eq!(
        active(&directory, &scope).unwrap().as_deref(),
        Some(candidate)
    );
    let decision_id = decision["activation"]["id"].as_str().unwrap();
    let rollback = run(&["rollback", decision_id, baseline_id], b"").unwrap();
    assert_ne!(rollback["activation"]["id"], baseline["activation"]["id"]);
    assert_eq!(
        run(&["rollback", decision_id, baseline_id], b"").unwrap(),
        rollback
    );
    assert_eq!(
        active(&directory, &scope).unwrap().as_deref(),
        Some(baseline_rev)
    );
    assert_eq!(
        run(&["stage", freeze], &candidate_profile),
        Err("harness_stale_preimage")
    );
    let path = store.join("activation-0002.json");
    let mut changed = rollback["activation"].clone();
    changed["revision"] = json!(candidate);
    fs::write(path, encode(&changed).unwrap()).unwrap();
    assert_eq!(
        active(&directory, &scope),
        Err("harness_activation_identity")
    );
}
