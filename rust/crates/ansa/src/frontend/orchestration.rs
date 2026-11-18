//! Connect an approved Carbon proposal to owned native child runtimes.
use super::{Result, sessions::Factory};
use crate::{
    orchestration::{
        plan::{Access, Plan, Step},
        run::{DispatchBudget, Run, RunContext, RunId},
        worktree::Worktree,
    },
    protocol::json::Json,
    provider::request::Request,
    provider::structured::{ChildResult, ChildStatus},
    runtime::{
        Cancel, ChildAttempt, ChildAttemptState, CommandPayload, ForkSeed, PostStatus, Runtime,
        RuntimeCommand,
    },
};
use ansa_linux::{fs, local};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct Proposal {
    pub prompt: String,
    pub seed: ForkSeed,
    pub plan: Plan,
    pub digest: String,
}

impl Proposal {
    pub(super) fn generate(
        factory: &Factory,
        seed: ForkSeed,
        prompt: String,
        stop: &Cancel,
    ) -> Result<Self> {
        let plan = factory.plan_orchestration(seed.clone(), &prompt, stop)?;
        let digest = crate::protocol::legacy_json_v1::sha256(plan.encode()?.as_bytes());
        Ok(Self {
            prompt,
            seed,
            plan,
            digest,
        })
    }
}

struct Child {
    task: Step,
    runtime: Runtime,
}

impl Child {
    async fn receipt(&self) -> Result<ChildAttempt> {
        self.runtime
            .handle()
            .child_attempt("assignment".into(), "task".into(), self.task.assignment())
            .await
    }

    async fn stop(&mut self, run: &mut Run) -> Result<()> {
        if let Ok(receipt) = self.receipt().await
            && let ChildAttemptState::Durable {
                request,
                quiescent: false,
                ..
            } = receipt.state()
        {
            let handle = self.runtime.handle();
            let _ = handle
                .post(RuntimeCommand {
                    schema_version: 1,
                    session: handle.session().clone(),
                    incarnation: handle.incarnation().clone(),
                    command: "stop-assignment".into(),
                    turn: receipt.turn().clone(),
                    payload: CommandPayload::Cancel(request.clone()),
                })
                .await;
        }
        // Retain ownership until the runtime has stopped and joined.
        for _ in 0..100 {
            if let Ok(proof) = self.receipt().await
                && matches!(
                    proof.state(),
                    ChildAttemptState::Durable {
                        quiescent: true,
                        ..
                    }
                )
            {
                run.observe(&self.task.id, &proof)?;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.runtime.shutdown().await
    }
}

pub(super) struct Completed {
    pub id: String,
    pub evidence: String,
    pub candidate: Option<Candidate>,
}

pub(super) struct Candidate {
    pub commit: String,
    pub workspace: PathBuf,
    pub diff: String,
}

pub(super) fn execute(factory: &Factory, proposal: Proposal, stop: &Cancel) -> Result<Completed> {
    factory.orchestration_route()?;
    proposal.plan.execution_ready()?;
    stop.check()?;
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "orchestration_reactor")?;
    reactor.block_on(execute_owned(factory, proposal, stop))
}

async fn execute_owned(factory: &Factory, proposal: Proposal, stop: &Cancel) -> Result<Completed> {
    let started = now()?;
    let deadline = started + 15 * 60;
    let parent = &proposal.seed.source_session().0;
    let source = proposal.seed.request()?;
    let (storage, identity) = run_directory(factory, parent)?;
    let submissions = (proposal.plan.steps().len() * 2).max(1) as u8;
    let mut run = Run::create_with_context(
        &storage.join("ledger"),
        identity,
        proposal.plan,
        DispatchBudget {
            started_at: started,
            deadline,
            submissions,
        },
        Some(RunContext {
            prompt: proposal.prompt,
            configuration: source.with_input(Vec::new())?.save()?.dump(),
        }),
    )?;
    run.approve(&proposal.digest)?;
    let candidate = match writer_workspace(factory, &run, &storage, false, stop) {
        Ok(candidate) => candidate,
        Err(code) => {
            run.cancel()?;
            return Err(code);
        }
    };
    schedule(factory, run, candidate, Vec::new(), stop).await
}

fn writer_workspace(
    factory: &Factory,
    run: &Run,
    storage: &Path,
    restore: bool,
    stop: &Cancel,
) -> Result<Option<(Worktree, File)>> {
    if !run
        .plan()
        .steps()
        .iter()
        .any(|step| step.access == Access::Write)
    {
        return Ok(None);
    }
    let directory = &factory.settings.directory;
    let root = Worktree::repository_root(&factory.settings.cwd, stop)?;
    let common = Worktree::common_directory(&root, stop)?;
    let (_, lease, _) = local::workspace_records(&directory.join("orchestration-writers"), &common)
        .map_err(|_| "run_workspace_lease")?;
    let lease = local::lock(&lease).map_err(|_| "run_workspace_busy")?;
    let worktree = if restore && storage.join("snapshot.json").exists() {
        Worktree::reopen(&root, storage, stop)?
    } else {
        if restore && run.original_children().next().is_some() {
            return Err("worktree_snapshot_missing");
        }
        let partial = storage.join("checkout");
        if restore && partial.exists() {
            let retained = storage.join("interrupted-snapshot");
            if retained.exists() {
                return Err("worktree_snapshot_incomplete");
            }
            std::fs::rename(&partial, retained).map_err(|_| "worktree_snapshot_incomplete")?;
        }
        Worktree::capture(
            &root,
            storage,
            &[factory.auth_path.clone(), directory.clone()],
            stop,
        )?
    };
    Ok(Some((worktree, lease)))
}

async fn schedule(
    factory: &Factory,
    mut run: Run,
    candidate: Option<(Worktree, File)>,
    mut active: Vec<Child>,
    stop: &Cancel,
) -> Result<Completed> {
    let identity = run.identity().clone();
    let deadline = run.deadline();
    let context = run.context().ok_or("orchestration_context")?;
    let prompt = context.prompt.clone();
    let source = Request::load(&Json::parse(&context.configuration)?)?;
    let result = async {
        loop {
            stop.check()?;
            if now()? >= deadline {
                return Err("orchestration_deadline");
            }
            let mut paused = 0;
            for index in (0..active.len()).rev() {
                let receipt = active[index].receipt().await?;
                let id = &active[index].task.id;
                run.observe(id, &receipt)?;
                if !run.active().any(|active_id| active_id == id) {
                    active.swap_remove(index).runtime.shutdown().await?;
                } else if matches!(receipt.state(), ChildAttemptState::Durable { quiescent: true, resume_required: true, recovery_required: false, .. }) {
                    paused += 1;
                }
            }
            if paused > 0 {
                if paused == active.len() {
                    return Err("orchestration_paused");
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
                continue;
            }
            let ready: Vec<_> = run.ready()?.into_iter().cloned().collect();
            if ready.is_empty() && active.is_empty() {
                break;
            }
            for task in ready {
                stop.check()?;
                let workspace = if run.plan().candidate_access(&task) {
                    let worktree = &candidate.as_ref().ok_or("worktree_missing")?.0;
                    worktree.path()
                } else {
                    factory.settings.cwd.as_path()
                };
                active.push(start_child(factory, &mut run, task, workspace, &source, stop).await?);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let release = if let Some((worktree, _lease)) = &candidate {
            if run.plan().steps().iter().any(|step| !run.result(&step.id).is_some_and(|result| result.status == ChildStatus::Completed)) {
                return Err("orchestration_candidate_incomplete");
            }
            Some(review_candidate(factory, &run, worktree, &source, stop).await?)
        } else { None };
        let mut summary = evidence(&run, &prompt)?;
        if let Some(candidate) = &release {
            let mut object: serde_json::Value = serde_json::from_str(&summary).map_err(|_| "orchestration_summary")?;
            object["verified_candidate"] = serde_json::json!({ "commit": candidate.commit, "workspace": candidate.workspace, "branch": "ansa-result", "verification": "git diff --check, approved assertions and independent review" });
            summary = object.to_string();
        }
        Ok(Completed {
            id: identity.run,
            evidence: summary,
            candidate: release,
        })
    }
    .await;
    if result
        .as_ref()
        .is_err_and(|code| *code == "orchestration_paused")
    {
        for child in &mut active {
            child.runtime.shutdown().await?;
        }
    } else if result.is_err() {
        stop_children(&mut run, &mut active).await?;
    }
    result
}

async fn stop_children(run: &mut Run, children: &mut [Child]) -> Result<()> {
    let mut failure = run.cancel().err();
    for child in children {
        if let Err(code) = child.stop(run).await {
            failure.get_or_insert(code);
        }
    }
    failure.map_or(Ok(()), Err)
}

fn dependency_evidence(run: &Run, task: &Step) -> Result<Option<Json>> {
    if task.depends_on.is_empty() {
        return Ok(None);
    }
    let values = task.depends_on.iter().map(|id| {
        let result = run.result(id).ok_or("orchestration_dependency_missing")?;
        Ok(serde_json::json!({ "step": id, "summary": result.summary.chars().take(256).collect::<String>(), "findings": result.findings.iter().take(2).map(|text| text.chars().take(160).collect::<String>()).collect::<Vec<_>>() }))
    }).collect::<Result<Vec<_>>>()?;
    Ok(Some(Json::parse(
        &serde_json::Value::Array(values).to_string(),
    )?))
}

async fn review_candidate(
    factory: &Factory,
    run: &Run,
    worktree: &Worktree,
    source: &Request,
    stop: &Cancel,
) -> Result<Candidate> {
    let scope: Vec<_> = run
        .plan()
        .steps()
        .iter()
        .filter(|step| step.access == Access::Write)
        .flat_map(|step| step.paths.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let storage = worktree.path().parent().ok_or("worktree_storage")?;
    let parent = &run.identity().parent_session;
    for attempt in 0..=2 {
        stop.check()?;
        let candidate = worktree.candidate(attempt, &scope, stop)?;
        let review_path = worktree.frozen_view(&candidate, attempt, &run.plan().encode()?, stop)?;
        let failed = Worktree::failed_checks(&review_path, run.plan().checks(), stop)?;
        let reviewer = Step {
            id: format!("review-{attempt}"),
            task: format!("Независимо проверьте замороженный коммит {}. Прочитайте approved-plan.json, ansa-candidate.diff и нужные файлы из checkout/. Оцените корректность, полноту и риски по критериям каждого шага.", candidate.commit()),
            access: Access::Read,
            paths: Vec::new(),
            depends_on: Vec::new(),
            acceptance: "Верните completed, если ветку можно выдать. Для необходимых исправлений верните changes_requested и конкретные замечания. Для расширения области верните blocked.".into(),
        };
        let verdict = run_candidate_phase(
            factory,
            storage,
            parent,
            run.deadline(),
            &review_path,
            source,
            reviewer,
            stop,
        )
        .await?;
        if verdict.status == ChildStatus::Completed && failed.is_empty() {
            worktree.local_branch(&candidate, stop)?;
            return Ok(Candidate {
                commit: candidate.commit().into(),
                workspace: worktree.path().to_owned(),
                diff: candidate.diff().into(),
            });
        }
        if verdict.status == ChildStatus::Blocked || attempt == 2 {
            return Err("orchestration_review_unresolved");
        }
        let repair = Step {
            id: format!("repair-{attempt}"),
            task: format!("Исправьте в согласованных путях замечания ревью и проверки к {}. Замечания задают предмет проверки, а разрешённую область задаёт список путей.\n{}", candidate.commit(), failed.iter().chain(verdict.findings.iter()).cloned().collect::<Vec<_>>().join("\n").chars().take(730).collect::<String>()),
            access: Access::Write,
            paths: scope.clone(),
            depends_on: Vec::new(),
            acceptance: "Исправьте только подтверждённые замечания. Верните blocked, если требуется расширение области.".into(),
        };
        if run_candidate_phase(
            factory,
            storage,
            parent,
            run.deadline(),
            worktree.path(),
            source,
            repair,
            stop,
        )
        .await?
        .status
            != ChildStatus::Completed
        {
            return Err("orchestration_repair_unresolved");
        }
    }
    Err("orchestration_review_unresolved")
}

#[allow(clippy::too_many_arguments)]
async fn run_candidate_phase(
    factory: &Factory,
    storage: &Path,
    parent: &str,
    deadline: u64,
    path: &Path,
    source: &Request,
    task: Step,
    stop: &Cancel,
) -> Result<ChildResult> {
    let started_at = now()?;
    stop.check()?;
    if started_at >= deadline {
        return Err("orchestration_deadline");
    }
    let encoded = serde_json::json!({"schema_version": 1, "summary": "Проверка согласованного кандидата", "steps": [&task]}).to_string();
    let ledger = storage.join(&task.id);
    let identity = RunId {
        parent_session: parent.into(),
        run: task.id.clone(),
    };
    let plan = Plan::parse(&encoded)?;
    let (mut phase, mut original) = if ledger.exists() {
        let mut phase = Run::open(&ledger, &identity)?;
        if phase.plan() != &plan || phase.deadline() != deadline {
            return Err("candidate_review_identity");
        }
        let original = restore_active(factory, &mut phase, stop).await?;
        (phase, original)
    } else {
        let mut phase = Run::create(
            &ledger,
            identity,
            plan,
            DispatchBudget {
                started_at,
                deadline,
                submissions: 2,
            },
        )?;
        let digest = phase.plan_digest().to_owned();
        phase.approve(&digest)?;
        (phase, Vec::new())
    };
    if let Some(result) = phase.result(&task.id) {
        return Ok(result.clone());
    }
    let mut child = match original.pop() {
        Some(child) => child,
        None => start_child(factory, &mut phase, task, path, source, stop).await?,
    };
    let result = async {
        loop {
            stop.check()?;
            if now()? >= deadline {
                return Err("orchestration_deadline");
            }
            let proof = child.receipt().await?;
            phase.observe(&child.task.id, &proof)?;
            if !phase.active().any(|id| id == child.task.id) {
                return phase
                    .result(&child.task.id)
                    .cloned()
                    .ok_or("orchestration_candidate_incomplete");
            }
            if matches!(
                proof.state(),
                ChildAttemptState::Durable {
                    quiescent: true,
                    resume_required: true,
                    recovery_required: false,
                    ..
                }
            ) {
                return Err("orchestration_paused");
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    .await;
    if result.is_ok()
        || result
            .as_ref()
            .is_err_and(|code| *code == "orchestration_paused")
    {
        child.runtime.shutdown().await?;
    } else {
        let cancelled = phase.cancel();
        let stopped = child.stop(&mut phase).await;
        cancelled?;
        stopped?;
    }
    result
}

async fn start_child(
    factory: &Factory,
    run: &mut Run,
    task: Step,
    workspace: &Path,
    source: &Request,
    stop: &Cancel,
) -> Result<Child> {
    let writes = if task.access == Access::Write {
        task.paths.as_slice()
    } else {
        &[]
    };
    let dependencies = dependency_evidence(run, &task)?;
    let created = factory.orchestration_child(
        &run.identity().parent_session,
        workspace,
        writes,
        source,
        dependencies.as_ref(),
        stop,
    )?;
    created.runtime.ready().await?;
    let mut child = Child {
        task,
        runtime: created.runtime,
    };
    let proof = child.receipt().await?;
    run.bind(&child.task.id, &proof)?;
    let result = run
        .dispatch(
            &child.task.id,
            &proof,
            &child.runtime.handle(),
            now()?,
            stop,
        )
        .await;
    if !matches!(result, Ok(PostStatus::Accepted)) {
        stop_children(run, std::slice::from_mut(&mut child)).await?;
        return Err(result.err().unwrap_or("orchestration_admission"));
    }
    Ok(child)
}

fn evidence(run: &Run, prompt: &str) -> Result<String> {
    // Limit contribution from each child so the final parent input stays
    // within its native cap even when every step returns the maximum.
    let results: Vec<_> = run.plan().steps().iter().map(|step| {
        let outcome = run.result(&step.id).map(|result| serde_json::json!({
            "status": result.status,
            "summary": result.summary.chars().take(96).collect::<String>(),
            "findings": result.findings.iter().take(2).map(|finding| finding.chars().take(40).collect::<String>()).collect::<Vec<_>>(),
        }));
        serde_json::json!({ "step": step.id, "outcome": outcome })
    }).collect();
    let input = serde_json::json!({
        "user_request": prompt.chars().take(512).collect::<String>(),
        "results": results,
    })
    .to_string();
    if input.len() > 16 * 1024 {
        return Err("orchestration_summary_limit");
    }
    Ok(input)
}

fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| "orchestration_clock")
}

fn run_directory(factory: &Factory, parent: &str) -> Result<(PathBuf, RunId)> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "orchestration_random")?;
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let root = factory
        .settings
        .directory
        .join("orchestration")
        .join(parent);
    fs::create_private(&root).map_err(|_| "orchestration_storage")?;
    let path = root.join(&id);
    fs::create_private_leaf(&path).map_err(|_| "orchestration_storage")?;
    Ok((
        path,
        RunId {
            parent_session: parent.into(),
            run: id,
        },
    ))
}

pub(super) fn latest_continuation(
    factory: &Factory,
    parent: &str,
) -> Result<Option<(RunId, String)>> {
    if factory.orchestration_route().is_err() {
        return Ok(None);
    }
    let root = run_root(factory, parent);
    if !root.exists() {
        return Ok(None);
    }
    let directory = fs::private_directory(&root).map_err(|_| "orchestration_storage")?;
    let (names, more) = local::names(&directory, 128).map_err(|_| "orchestration_inventory")?;
    if more {
        return Err("orchestration_inventory_limit");
    }
    let time = now()?;
    let mut latest = None;
    for name in names {
        let Some(id) = name
            .to_str()
            .filter(|id| crate::provider::contracts::valid_id(id))
        else {
            continue;
        };
        let identity = RunId {
            parent_session: parent.into(),
            run: id.into(),
        };
        let storage = root.join(id);
        let Ok(directory) = fs::private_directory(&storage) else {
            continue;
        };
        if local::read(&directory, "summary-admitted", 64)
            .ok()
            .flatten()
            .is_some_and(|value| value == id.as_bytes())
        {
            continue;
        }
        let Ok(run) = Run::open(&storage.join("ledger"), &identity) else {
            continue;
        };
        if !continuable(&run, time) {
            continue;
        }
        let modified = std::fs::metadata(storage.join("ledger"))
            .and_then(|metadata| metadata.modified())
            .map_err(|_| "orchestration_storage")?;
        if latest.as_ref().is_none_or(|(old, _, _)| *old < modified) {
            latest = Some((modified, identity, run.plan().summary().to_owned()));
        }
    }
    Ok(latest.map(|(_, id, title)| (id, title)))
}

fn continuable(run: &Run, time: u64) -> bool {
    run.may_continue(time) && run.context().is_some() && run.plan().execution_ready().is_ok()
}

pub(super) fn resume(factory: &Factory, identity: RunId, stop: &Cancel) -> Result<Completed> {
    factory.orchestration_route()?;
    stop.check()?;
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "orchestration_reactor")?;
    reactor.block_on(resume_owned(factory, identity, stop))
}

async fn resume_owned(factory: &Factory, identity: RunId, stop: &Cancel) -> Result<Completed> {
    let storage = run_root(factory, &identity.parent_session).join(&identity.run);
    let mut run = Run::open(&storage.join("ledger"), &identity)?;
    if !continuable(&run, now()?) {
        return Err("orchestration_continuation_unavailable");
    }
    let candidate = writer_workspace(factory, &run, &storage, true, stop)?;
    let active = restore_active(factory, &mut run, stop).await?;
    schedule(factory, run, candidate, active, stop).await
}

async fn restore_active(factory: &Factory, run: &mut Run, stop: &Cancel) -> Result<Vec<Child>> {
    let mut children = Vec::<Child>::new();
    let mut receipts = Vec::new();
    let original: Vec<_> = run
        .original_children()
        .map(|(step, session, store)| (step.to_owned(), session.to_owned(), store))
        .collect();
    for (step, session, store) in original {
        if stop.cancelled() {
            for child in &mut children {
                child.runtime.shutdown().await?;
            }
            return run.cancel().and(Err("run_cancelled"));
        }
        let task = run
            .plan()
            .steps()
            .iter()
            .find(|task| task.id == step)
            .ok_or("run_child_missing")?
            .clone();
        let child = Child {
            task,
            runtime: factory
                .restore_orchestration_child(&session, store, false, stop)?
                .runtime,
        };
        child.runtime.ready().await?;
        receipts.push((step, child.receipt().await?));
        children.push(child);
    }
    run.continue_run(
        &receipts
            .iter()
            .map(|(step, receipt)| (step.as_str(), receipt))
            .collect::<Vec<_>>(),
    )?;
    if !run.may_continue(now()?) {
        return Err("orchestration_continuation_unavailable");
    }
    let mut active = Vec::new();
    let startup = async {
        for (mut child, (_, proof)) in children.into_iter().zip(receipts) {
            if run.active().any(|id| id == child.task.id) {
                active.push(child);
                let child = active.last().ok_or("run_child_missing")?;
                let status = match proof.state() {
                    ChildAttemptState::Missing => {
                        run.dispatch(
                            &child.task.id,
                            &proof,
                            &child.runtime.handle(),
                            now()?,
                            stop,
                        )
                        .await?
                    }
                    ChildAttemptState::Durable {
                        resume_required: true,
                        recovery_required: false,
                        ..
                    } => {
                        run.resume_child(
                            &child.task.id,
                            &proof,
                            &child.runtime.handle(),
                            now()?,
                            stop,
                        )
                        .await?
                    }
                    _ => return Err("orchestration_continuation_unavailable"),
                };
                if !matches!(status, PostStatus::Accepted | PostStatus::Duplicate) {
                    return Err("orchestration_admission");
                }
            } else {
                child.runtime.shutdown().await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(code) = startup {
        stop_children(run, &mut active).await?;
        return Err(code);
    }
    Ok(active)
}

fn run_root(factory: &Factory, parent: &str) -> PathBuf {
    factory
        .settings
        .directory
        .join("orchestration")
        .join(parent)
}

pub(super) fn mark_handoff(factory: &Factory, parent: &str, id: &str) -> Result<()> {
    let path = run_root(factory, parent).join(id);
    let directory = fs::private_directory(&path).map_err(|_| "orchestration_storage")?;
    let _lock = local::lock(&directory).map_err(|_| "orchestration_receipt_busy")?;
    local::publish_recovered(&directory, "summary-admitted", id.as_bytes())
        .map_err(|_| "orchestration_receipt")
}
