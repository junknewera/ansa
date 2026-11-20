//! Connect an approved Carbon proposal to owned native child runtimes.
use super::{Result, sessions::Factory};
use crate::{
    orchestration::{
        plan::{Access, Plan, Step},
        profile::RoleProfile,
        run::{DispatchBudget, Run, RunContext, RunId},
        worktree::{Frozen, Worktree},
    },
    protocol::json::Json,
    provider::request::Request,
    provider::structured::{ChildResult, ChildStatus},
    runtime::{
        Cancel, ChildAttempt, ChildAttemptState, CommandId, CommandPayload, ForkSeed, PostStatus,
        Runtime, RuntimeCommand, TurnId,
    },
    tools::{
        command::{ProcessOptions, ProcessRunner},
        invocation::Invocation,
    },
};
use ansa_linux::{fs, local};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, watch};

mod plus;
mod plus_review;
pub(super) use plus_review::PLUS_REVIEW_SKILL;

pub(super) struct Proposal {
    pub prompt: String,
    pub seed: ForkSeed,
    pub plan: Plan,
    pub digest: String,
    pub read_only: bool,
    pub skill: Option<String>,
    pub profile: Option<RoleProfile>,
}

impl Proposal {
    pub(super) fn generate(
        factory: &Factory,
        seed: ForkSeed,
        prompt: String,
        read_only: bool,
        skill: Option<&str>,
        stop: &Cancel,
    ) -> Result<Self> {
        Self::generate_with_profile(factory, seed, prompt, read_only, skill, stop, None)
    }

    pub(super) fn generate_plus(
        factory: &Factory,
        seed: ForkSeed,
        prompt: String,
        read_only: bool,
        skill: Option<&str>,
        stop: &Cancel,
        profile: RoleProfile,
    ) -> Result<Self> {
        Self::generate_with_profile(factory, seed, prompt, read_only, skill, stop, Some(profile))
    }

    fn generate_with_profile(
        factory: &Factory,
        seed: ForkSeed,
        prompt: String,
        read_only: bool,
        skill: Option<&str>,
        stop: &Cancel,
        profile: Option<RoleProfile>,
    ) -> Result<Self> {
        let instructions = skill
            .map(|name| {
                factory
                    .delegation_skill(name)
                    .ok_or("orchestration_skill_grant")
            })
            .transpose()?;
        let mut plan = if let Some(profile) = &profile {
            factory.plan_orchestration_plus(
                seed.clone(),
                &prompt,
                read_only,
                instructions.as_deref(),
                profile,
                stop,
            )?
        } else {
            factory.plan_orchestration(
                seed.clone(),
                &prompt,
                read_only,
                instructions.as_deref(),
                stop,
            )?
        };
        if let Some(folder) = plan.read_folder() {
            if !named_folder(&prompt, folder) {
                return Err("orchestration_read_folder_not_named");
            }
            let resolved = factory.orchestration_read_folder(folder)?;
            plan.resolve_read_folder(crate::config::path_text(&resolved)?.to_owned())?;
        }
        let digest = crate::protocol::legacy_json_v1::sha256(plan.encode()?.as_bytes());
        Ok(Self {
            prompt,
            seed,
            plan,
            digest,
            read_only,
            skill: skill.map(str::to_owned),
            profile,
        })
    }
}

fn named_folder(mut prompt: &str, folder: &str) -> bool {
    let separator = |ch: char| ch.is_whitespace() || "()[]<>,!?".contains(ch);
    loop {
        prompt = prompt.trim_start_matches(separator);
        let Some(first) = prompt.chars().next() else {
            return false;
        };
        if matches!(first, '`' | '"' | '\'') {
            let Some((path, rest)) = prompt[1..].split_once(first) else {
                return false;
            };
            let after = rest.strip_prefix('.').unwrap_or(rest);
            if path == folder && after.chars().next().is_none_or(separator) {
                return true;
            }
            prompt = if rest.chars().next().is_none_or(separator) {
                rest
            } else {
                rest.split_once(separator).map_or("", |(_, rest)| rest)
            };
        } else {
            let (path, rest) = prompt.split_once(separator).unwrap_or((prompt, ""));
            if path == folder || path.strip_suffix('.') == Some(folder) {
                return true;
            }
            prompt = rest;
        }
    }
}

#[cfg(test)]
#[test]
fn read_folder_consent_matches_complete_paths() {
    assert!(named_folder("Inspect Workspace/pet.", "Workspace/pet"));
    assert!(named_folder("Read `/tmp/pet archive`.", "/tmp/pet archive"));
    assert!(!named_folder("Read \"/tmp/pet archive\"", "/tmp/pet"));
    assert!(!named_folder("Read \"/tmp/pet:archive\"", "/tmp/pet"));
    assert!(!named_folder("Read /tmp/pet/ansa", "/tmp/pet"));
}

struct Child {
    task: Step,
    runtime: Runtime,
    distinct_turns: bool,
}

impl Child {
    async fn receipt(&self) -> Result<ChildAttempt> {
        let (command, turn) = if self.distinct_turns {
            (self.task.id.clone(), self.task.id.clone())
        } else {
            ("assignment".into(), "task".into())
        };
        self.runtime
            .handle()
            .child_attempt(CommandId(command), TurnId(turn), self.task.assignment())
            .await
    }

    async fn park(&mut self) -> Result<()> {
        // The scheduler has observed a durable, quiescent pause. An aborting
        // shutdown would append Cancel and make the same turn unrecoverable.
        self.runtime.close_input();
        self.runtime.join().await
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
                    command: if self.distinct_turns {
                        CommandId(format!("stop-{}", self.task.id))
                    } else {
                        "stop-assignment".into()
                    },
                    turn: receipt.turn().clone(),
                    payload: CommandPayload::Cancel(request.clone()),
                })
                .await;
        }
        // Retain ownership until the runtime has stopped and joined.
        let mut observed = Ok(());
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
                observed = run.observe(&self.task.id, &proof);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stopped = self.runtime.shutdown().await;
        observed.and(stopped)
    }
}

pub(super) struct Completed {
    pub id: String,
    pub evidence: Option<String>,
    pub candidate: Option<Candidate>,
}

pub(super) struct Candidate {
    pub commit: String,
    pub workspace: PathBuf,
    pub diff: String,
    pub paths: Vec<String>,
    pub checks: Vec<super::carbon::OutputView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RunView {
    pub id: String,
    pub plan: Plan,
    pub title: String,
    pub done: usize,
    pub reviewing: bool,
    pub clarification_pending: bool,
    pub agents: Vec<(String, super::carbon::AgentLine)>,
}

pub(super) struct Clarification {
    pub text: String,
    pub acknowledgement: oneshot::Sender<Result<()>>,
}
type Clarifications = mpsc::Receiver<Clarification>;

fn accept_clarifications(run: &mut Run, inbox: &mut Clarifications) {
    while let Ok(message) = inbox.try_recv() {
        let _ = message.acknowledgement.send(run.clarify(message.text));
    }
}

fn publish_progress(
    progress: &watch::Sender<Option<RunView>>,
    run: &Run,
    prompt: &str,
    details: &BTreeMap<String, String>,
    reviewing: bool,
) {
    let mut done = 0;
    let agents = run
        .original_children()
        .map(|(id, session, _)| (id, session))
        .chain(run.plus_results())
        .filter_map(|(id, session)| {
            let task = run.plan().steps().iter().find(|step| step.id == id)?;
            let result = run.result(id);
            done += usize::from(result.is_some());
            let (state, detail) = if let Some(result) = result {
                (
                    if result.status == ChildStatus::Completed {
                        "Completed"
                    } else {
                        "Stopped"
                    },
                    result.findings.first().unwrap_or(&result.summary).clone(),
                )
            } else {
                ("Running", details.get(id).cloned().unwrap_or_default())
            };
            Some((
                session.to_owned(),
                super::carbon::AgentLine {
                    title: task.task.lines().next().unwrap_or(&task.id).to_owned(),
                    state: state.into(),
                    detail,
                },
            ))
        })
        .collect();
    let view = RunView {
        id: run.identity().run.clone(),
        plan: run.plan().clone(),
        title: prompt
            .split_whitespace()
            .take(10)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(72)
            .collect(),
        done,
        reviewing,
        clarification_pending: !run.pending_clarifications().is_empty(),
        agents,
    };
    progress.send_if_modified(|previous| {
        if previous.as_ref() == Some(&view) {
            return false;
        }
        *previous = Some(view);
        true
    });
}

pub(super) fn execute(
    factory: &Factory,
    proposal: Proposal,
    stop: &Cancel,
    progress: watch::Sender<Option<RunView>>,
    inbox: Clarifications,
) -> Result<Completed> {
    factory.check_orchestration_context(&proposal.seed.request()?)?;
    proposal.plan.execution_ready()?;
    if proposal.profile.is_none() {
        factory.require_orchestration_route(stop)?;
    }
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "orchestration_reactor")?;
    reactor.block_on(execute_owned(factory, proposal, stop, progress, inbox))
}

async fn execute_owned(
    factory: &Factory,
    proposal: Proposal,
    stop: &Cancel,
    progress: watch::Sender<Option<RunView>>,
    inbox: Clarifications,
) -> Result<Completed> {
    let started = now()?;
    let deadline = started + 15 * 60;
    let parent = &proposal.seed.source_session().0;
    let source = proposal.seed.request()?;
    let model_window = proposal.seed.model_window();
    let handoff_tokens = model_window.map(|window| {
        let mut parent = source.clone();
        parent.tools.descriptors.clear();
        crate::context::input_capacity(window)
            .saturating_sub(crate::context::estimate_request_tokens(&parent))
    });
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
            model_window,
            handoff_tokens,
            plus_profile: proposal
                .profile
                .as_ref()
                .map(RoleProfile::encode)
                .transpose()?
                .map(|value| value.dump()),
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
    if let Some(profile) = proposal.profile {
        return plus::schedule(
            factory, run, &storage, candidate, &profile, stop, progress, inbox,
        )
        .await;
    }
    schedule(factory, run, candidate, Vec::new(), stop, progress, inbox).await
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
        if restore && (run.original_children().next().is_some() || storage.join("plus").exists()) {
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
    progress: watch::Sender<Option<RunView>>,
    mut inbox: Clarifications,
) -> Result<Completed> {
    let identity = run.identity().clone();
    let deadline = run.deadline();
    let context = run.context().ok_or("orchestration_context")?;
    let prompt = context.prompt.clone();
    let source = Request::load(&Json::parse(&context.configuration)?)?;
    let read_folder = run
        .plan()
        .read_folder()
        .map_or_else(|| factory.settings.cwd.clone(), PathBuf::from);
    let mut details = BTreeMap::new();
    let result = async {
        loop {
            stop.check()?;
            if now()? >= deadline {
                return Err("orchestration_deadline");
            }
            let mut paused = 0;
            let previously_active = active.len();
            for index in (0..active.len()).rev() {
                let receipt = active[index].receipt().await?;
                let id = &active[index].task.id;
                if let Some((name, target)) = receipt.latest_tool() {
                    let label = match name {
                        "list_directory" => "List",
                        "read_file" => "Read",
                        "search_files" => "Search",
                        "edit_file" => "Edit",
                        _ => "Tool",
                    };
                    details.insert(
                        id.clone(),
                        target.map_or_else(|| label.into(), |path| format!("{label} {path}")),
                    );
                }
                run.observe(id, &receipt)?;
                if matches!(receipt.state(), ChildAttemptState::Durable {
                    quiescent: true, recovery_required: true, ..
                }) {
                    return Err("orchestration_effect_unknown");
                }
                if !run.active().any(|active_id| active_id == id) {
                    active.swap_remove(index).runtime.shutdown().await?;
                } else if matches!(
                    receipt.state(),
                    ChildAttemptState::Durable {
                        quiescent: true,
                        resume_required: true,
                        recovery_required: false,
                        ..
                    }
                ) {
                    paused += 1;
                }
            }
            accept_clarifications(&mut run, &mut inbox);
            publish_progress(&progress, &run, &prompt, &details, false);
            if paused > 0 {
                if paused == active.len() {
                    return Err("orchestration_paused");
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
                continue;
            }
            if run.decisions().last().is_some_and(|note| note.status != ChildStatus::Completed) {
                return Err("orchestration_scope_confirmation");
            }
            if !run.pending_clarifications().is_empty() {
                if !active.is_empty() && previously_active == active.len() {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    continue;
                }
                {
                    let input = Json::parse(&serde_json::json!({
                        "request": prompt,
                        "approved_plan": run.plan().encode()?,
                        "completed": run.plan().steps().iter()
                            .filter_map(|step| run.result(&step.id).map(|result| (&step.id, result)))
                            .collect::<BTreeMap<_, _>>(),
                        "prior_decisions": run.decisions(),
                        "clarifications": run.pending_clarifications(),
                    }).to_string())?;
                    let decision = factory.coordinate_clarification(&source, &input, stop)?;
                    let approved = decision.status == ChildStatus::Completed;
                    run.record_decision(decision)?;
                    if !approved {
                        return Err("orchestration_scope_confirmation");
                    }
                    publish_progress(&progress, &run, &prompt, &details, false);
                }
            }
            let slots = factory.orchestration_slots().saturating_sub(active.len());
            let ready: Vec<_> = run.ready()?.into_iter().take(slots).cloned().collect();
            if ready.is_empty() && active.is_empty() {
                inbox.close();
                accept_clarifications(&mut run, &mut inbox);
                if !run.pending_clarifications().is_empty() {
                    continue;
                }
                break;
            }
            for task in ready {
                stop.check()?;
                accept_clarifications(&mut run, &mut inbox);
                if !run.pending_clarifications().is_empty() {
                    break;
                }
                let workspace = if run.plan().candidate_access(&task) {
                    let worktree = &candidate.as_ref().ok_or("worktree_missing")?.0;
                    worktree.path()
                } else {
                    read_folder.as_path()
                };
                active.push(start_child(factory, &mut run, task, workspace, &source, stop).await?);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let release = if let Some((worktree, _lease)) = &candidate {
            publish_progress(&progress, &run, &prompt, &details, true);
            if run.plan().steps().iter().any(|step| {
                !run.result(&step.id)
                    .is_some_and(|result| result.status == ChildStatus::Completed)
            }) {
                return Err("orchestration_candidate_incomplete");
            }
            Some(review_candidate(factory, &run, worktree, &source, stop).await?)
        } else {
            None
        };
        Ok(Completed {
            id: identity.run,
            evidence: evidence(&run, &prompt, release.as_ref(), None),
            candidate: release,
        })
    }
    .await;
    if result
        .as_ref()
        .is_err_and(|code| *code == "orchestration_paused")
    {
        for child in &mut active {
            child.park().await?;
        }
        if stop.cancelled() {
            return run.cancel().and(Err("run_cancelled"));
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
        Ok(serde_json::json!({ "step": id, "summary": result.summary, "findings": result.findings }))
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
        let (mut failed, mut checks) =
            run_command_checks(factory, run, worktree, &candidate, attempt, stop)?;
        let plan = run.plan().encode()?;
        let review_path = worktree.frozen_view(&candidate, attempt, &plan, stop)?;
        failed.extend(Worktree::failed_checks(
            &review_path,
            run.plan().checks(),
            stop,
        )?);
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
            worktree.frozen_view(&candidate, attempt, &plan, stop)?;
            worktree.local_branch(&candidate, stop)?;
            super::publish::record_release(storage, worktree, &candidate)?;
            append_review_checks(&mut checks, run.plan(), &verdict);
            return Ok(Candidate {
                commit: candidate.commit().into(),
                workspace: worktree.path().to_owned(),
                diff: candidate.diff().into(),
                paths: candidate.paths().to_vec(),
                checks,
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

fn run_command_checks(
    factory: &Factory,
    run: &Run,
    worktree: &Worktree,
    candidate: &Frozen,
    attempt: u8,
    stop: &Cancel,
) -> Result<(Vec<String>, Vec<super::carbon::OutputView>)> {
    let checks = run.plan().command_checks();
    if checks.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let storage = worktree.path().parent().ok_or("worktree_storage")?;
    let runner = ProcessRunner::new(
        &storage.join("check-jobs"),
        &std::env::current_exe().map_err(|_| "process_executable")?,
    )?;
    let plan = run.plan().encode()?;
    let timeout_ms = factory.settings.process_timeout_ms.clamp(1, 300_000);
    let mut failures = Vec::new();
    let mut passed = Vec::new();
    for (index, check) in checks.iter().enumerate() {
        stop.check()?;
        let invocation = check_invocation(run, candidate, attempt, index, &check.argv)?;
        let name = &invocation.call;
        let path = storage.join(name);
        let existing = runner.has_job(&invocation)?;
        if !existing {
            if now()?.saturating_add((timeout_ms as u64).div_ceil(1000)) > run.deadline() {
                return Err("orchestration_deadline");
            }
            worktree.frozen_view_named(candidate, name, &plan, false, stop)?;
            if now()?.saturating_add((timeout_ms as u64).div_ceil(1000)) > run.deadline() {
                return Err("orchestration_deadline");
            }
        }
        let outcome = runner.run(
            &invocation,
            &ProcessOptions {
                cwd: path.join("checkout"),
                argv: check.argv.clone(),
                timeout_ms,
                output_bytes: 8192,
            },
            stop,
        )?;
        if outcome.effects() == crate::provider::replay::Effects::Unknown {
            return Err("orchestration_check_effect_unknown");
        }
        stop.check()?;
        if !existing {
            std::fs::remove_dir_all(&path).map_err(|_| "orchestration_check_cleanup")?;
        }
        let output =
            std::str::from_utf8(outcome.output()).map_err(|_| "orchestration_check_result")?;
        let output = Json::parse(output)?;
        if output.field("cancelled").and_then(Json::boolean) == Ok(true) {
            return Err("orchestration_check_cancelled");
        }
        if let Some(view) = passed_check(&check.argv, &outcome, &output) {
            passed.push(view);
        } else {
            failures.push(format!(
                "Check {} failed ({})",
                check.argv[0],
                output
                    .field("exit_code")
                    .map_or_else(|_| "startup".into(), Json::dump)
            ));
        }
    }
    Ok((failures, passed))
}

fn check_invocation(
    run: &Run,
    candidate: &Frozen,
    attempt: u8,
    index: usize,
    argv: &[String],
) -> Result<Invocation> {
    let call = format!("check-{attempt}-{index}");
    let arguments = Json::parse(
        &serde_json::json!({"candidate": candidate.commit(), "argv": argv}).to_string(),
    )?;
    Ok(Invocation {
        operation: crate::protocol::legacy_json_v1::sha256(
            format!("{}\n{call}\n{}", run.identity().run, arguments.dump()).as_bytes(),
        ),
        call,
        name: "candidate_check".into(),
        revision: "ansa_candidate_check_v1".into(),
        arguments,
    })
}

fn passed_check(
    argv: &[String],
    outcome: &crate::tools::registry::Outcome,
    output: &Json,
) -> Option<super::carbon::OutputView> {
    use crate::provider::replay::{Effects, Execution};
    if outcome.effects() != Effects::KnownResult
        || outcome.execution() != Execution::Succeeded
        || output.usize_field("exit_code") != Ok(0)
        || output.field("cancelled").and_then(Json::boolean) == Ok(true)
        || output.field("timed_out").and_then(Json::boolean) != Ok(false)
        || output.field("cleanup_complete").and_then(Json::boolean) != Ok(true)
    {
        return None;
    }
    let mut text = String::from("Exit code 0\n");
    for key in ["stdout", "stderr"] {
        if let Ok(value) = output.text_field(key)
            && !value.is_empty()
        {
            text.push_str(&format!("\n{key}\n{value}\n"));
        }
    }
    if output.field("truncated").and_then(Json::boolean) == Ok(true) {
        text.push_str("\nOutput truncated");
    }
    Some(super::carbon::OutputView {
        title: argv.join(" "),
        text,
    })
}

fn append_review_checks(
    checks: &mut Vec<super::carbon::OutputView>,
    plan: &Plan,
    verdict: &ChildResult,
) {
    checks.extend(plan.checks().iter().map(|check| super::carbon::OutputView {
        title: format!("{} · content check", check.path),
        text: format!("Matched expected text\n\n{}", check.contains),
    }));
    checks.push(super::carbon::OutputView {
        title: "Independent review".into(),
        text: std::iter::once(&verdict.summary)
            .chain(&verdict.findings)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    });
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
    if phase.child_cancelled(&task.id) {
        return Err("orchestration_candidate_child_cancelled");
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
            phase.check_continuation(now()?)?;
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
    if result == Err("orchestration_paused") {
        child.park().await?;
        if stop.cancelled() {
            return phase.cancel().and(Err("run_cancelled"));
        }
        if phase.submissions_exhausted() {
            return Err("run_budget");
        }
    } else if result.is_ok() {
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
    let dependencies = dependency_evidence(run, &task)?;
    let created = factory.orchestration_child(
        &run.identity().parent_session,
        workspace,
        &task,
        source,
        dependencies.as_ref(),
        run.decisions(),
        stop,
    )?;
    created.runtime.ready().await?;
    let mut child = Child {
        task,
        runtime: created.runtime,
        distinct_turns: false,
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

fn evidence(
    run: &Run,
    prompt: &str,
    candidate: Option<&Candidate>,
    report_review: Option<&serde_json::Value>,
) -> Option<String> {
    let results: Vec<_> = run
        .plan()
        .steps()
        .iter()
        .map(|step| serde_json::json!({ "step": step.id, "outcome": run.result(&step.id) }))
        .collect();
    let mut evidence = serde_json::json!({
        "show_approved_plan": true,
        "user_request": prompt,
        "results": results,
    });
    if !run.clarifications().is_empty() {
        evidence["user_clarifications"] = serde_json::json!(run.clarifications());
        evidence["coordinator_decisions"] = serde_json::json!(run.decisions());
    }
    if let Some(candidate) = candidate {
        evidence["verified_candidate"] = serde_json::json!({
            "commit": candidate.commit,
            "workspace": candidate.workspace,
            "branch": "ansa-result",
            "verification": "git diff --check, approved checks and independent review",
            "command_checks_passed": run.plan().command_checks().len(),
        });
    }
    if let Some(review) = report_review {
        evidence["report_review"] = review.clone();
    }
    let serialized = evidence.to_string();
    let budget = run.context().and_then(|context| context.handoff_tokens);
    let fits = |value: &str| {
        Json::text_fits_frame(value)
            && budget
                .is_none_or(|remaining| crate::context::estimate_text_tokens(value) <= remaining)
    };
    if fits(&serialized) {
        return Some(serialized);
    }
    // Results and clarifications remain in the run and child journals. Give
    // each report a share of the available input and keep its archive identity.
    let tokens = budget.unwrap_or_else(|| crate::context::estimate_text_tokens(&serialized) / 2);
    let mut share = tokens / (run.plan().steps().len() + 2);
    evidence
        .as_object_mut()
        .expect("orchestration evidence")
        .remove("coordinator_decisions");
    evidence["reports_are_excerpts"] = serde_json::json!(true);
    loop {
        evidence["user_request"] = serde_json::json!(crate::context::token_prefix(prompt, share));
        evidence["user_clarifications"] = serde_json::json!(
            run.clarifications()
                .last()
                .map(|text| crate::context::token_prefix(text, share))
        );
        if let Some(review) = report_review {
            evidence["report_review"] = serde_json::json!({
                "excerpt": crate::context::token_prefix(&review.to_string(), share),
            });
        }
        evidence["results"] = serde_json::json!(run.plan().steps().iter().map(|step| {
            let archive = run.original_children().map(|(id, session, _)| (id, session)).chain(run.plus_results()).find_map(|(id, session)| (id == step.id).then_some(session));
            serde_json::json!({
                "step": step.id,
                "child_session": archive,
                "summary_excerpt": run.result(&step.id).map(|result| crate::context::token_prefix(&result.summary, share)),
                "status": run.result(&step.id).map(|result| result.status),
            })
        }).collect::<Vec<_>>());
        let projected = evidence.to_string();
        if fits(&projected) {
            return Some(projected);
        }
        if share == 0 {
            return None;
        }
        share /= 2;
    }
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

#[derive(Default)]
pub(super) struct PlanLinks {
    pub continuation: Option<(RunId, String)>,
    pub clarification: Option<RunId>,
}

pub(super) fn recent_plan_links(factory: &Factory, parent: &str) -> Result<PlanLinks> {
    if factory.orchestration_route().is_err() {
        return Ok(PlanLinks::default());
    }
    let root = run_root(factory, parent);
    if !root.exists() {
        return Ok(PlanLinks::default());
    }
    let directory = fs::private_directory(&root).map_err(|_| "orchestration_storage")?;
    let (names, more) = local::names(&directory, 128).map_err(|_| "orchestration_inventory")?;
    if more {
        return Err("orchestration_inventory_limit");
    }
    let time = now()?;
    let mut continuation = None;
    let mut clarification = None;
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
        let modified = std::fs::metadata(storage.join("ledger"))
            .and_then(|metadata| metadata.modified())
            .map_err(|_| "orchestration_storage")?;
        if !run.clarifications().is_empty()
            && clarification
                .as_ref()
                .is_none_or(|(old, _)| *old < modified)
        {
            clarification = Some((modified, identity.clone()));
        }
        if (run.pending_clarifications().is_empty() || run.active_plus_task().is_some())
            && continuable(&run, time)
            && continuation
                .as_ref()
                .is_none_or(|(old, _, _)| *old < modified)
        {
            continuation = Some((modified, identity, run.plan().summary().to_owned()));
        }
    }
    Ok(PlanLinks {
        continuation: continuation.map(|(_, id, title)| (id, title)),
        clarification: clarification.map(|(_, id)| id),
    })
}

fn continuable(run: &Run, time: u64) -> bool {
    run.may_continue(time) && run.context().is_some() && run.plan().execution_ready().is_ok()
}

pub(super) struct ArchivedPlan {
    pub run: String,
    pub plan: Plan,
    pub prompt: String,
    pub candidate: Result<Option<Candidate>>,
}

pub(super) fn archived_plan(
    factory: &Factory,
    parent: String,
    id: &str,
    stop: &Cancel,
) -> Result<ArchivedPlan> {
    use crate::provider::contracts::valid_id;
    if !valid_id(id) {
        return Err("run_identity");
    }
    let entries = factory.entries()?;
    let mut parent = Some(parent);
    for _ in 0..128 {
        let Some(owner) = parent.take().filter(|owner| valid_id(owner)) else {
            break;
        };
        let entry = entries
            .iter()
            .find(|entry| entry.id == owner)
            .ok_or("session_workspace_mismatch")?;
        parent = entry.parent.clone();
        let identity = RunId {
            parent_session: owner.clone(),
            run: id.into(),
        };
        let storage = run_root(factory, &owner).join(id);
        if !storage.join("ledger").exists() {
            continue;
        }
        let run = Run::open(&storage.join("ledger"), &identity)?;
        let mut prompt = run
            .context()
            .map(|context| context.prompt.clone())
            .ok_or("orchestration_context")?;
        if !run.clarifications().is_empty() {
            prompt.push_str("\n\nClarifications\n");
            prompt.push_str(&run.clarifications().join("\n\n"));
        }
        let candidate = if storage.join("release.json").exists() {
            archived_candidate(factory, &run, &storage, stop).map(Some)
        } else {
            Ok(None)
        };
        return Ok(ArchivedPlan {
            run: id.into(),
            plan: run.plan().clone(),
            prompt,
            candidate,
        });
    }
    Err("orchestration_plan_missing")
}

fn archived_candidate(
    factory: &Factory,
    run: &Run,
    storage: &Path,
    stop: &Cancel,
) -> Result<Candidate> {
    let scope: Vec<_> = run
        .plan()
        .steps()
        .iter()
        .filter(|step| step.access == Access::Write)
        .flat_map(|step| step.paths.iter().cloned())
        .collect();
    let root = Worktree::repository_root(&factory.settings.cwd, stop)?;
    let worktree = Worktree::reopen(&root, storage, stop)?;
    let commit = super::publish::released_candidate(storage, &worktree)?;
    let candidate = worktree.inspect_candidate(&commit, &scope, stop)?;
    let directory = fs::private_directory(storage).map_err(|_| "orchestration_storage")?;
    let attempt = (0..=2)
        .rev()
        .find(|attempt| {
            local::read(&directory, &format!("candidate-{attempt}"), 128)
                .ok()
                .flatten()
                .as_deref()
                == Some(commit.as_bytes())
        })
        .ok_or("worktree_candidate_identity")?;
    let mut checks = Vec::new();
    for (index, check) in run.plan().command_checks().iter().enumerate() {
        stop.check()?;
        let invocation = check_invocation(run, &candidate, attempt, index, &check.argv)?;
        let outcome = ProcessRunner::inspect_outcome(&storage.join("check-jobs"), &invocation)?;
        let output = Json::parse(
            std::str::from_utf8(outcome.output()).map_err(|_| "orchestration_check_result")?,
        )?;
        checks.push(
            passed_check(&check.argv, &outcome, &output).ok_or("orchestration_check_result")?,
        );
    }
    let verdict = if run
        .context()
        .is_some_and(|context| context.plus_profile.is_some())
    {
        plus_review::archived_review(storage, run, attempt, &candidate, &checks)?
    } else {
        let phase = format!("review-{attempt}");
        let id = RunId {
            parent_session: run.identity().parent_session.clone(),
            run: phase.clone(),
        };
        let reviewer = Run::open(&storage.join(&phase), &id)?;
        reviewer
            .recorded_result(&phase)
            .filter(|result| result.status == ChildStatus::Completed)
            .cloned()
            .ok_or("orchestration_review_unresolved")?
    };
    append_review_checks(&mut checks, run.plan(), &verdict);
    Ok(Candidate {
        commit,
        workspace: worktree.path().into(),
        diff: candidate.diff().into(),
        paths: candidate.paths().to_vec(),
        checks,
    })
}

pub(super) fn resume(
    factory: &Factory,
    identity: RunId,
    read_only: bool,
    stop: &Cancel,
    progress: watch::Sender<Option<RunView>>,
    inbox: Clarifications,
) -> Result<Completed> {
    factory.orchestration_route()?;
    stop.check()?;
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "orchestration_reactor")?;
    reactor.block_on(resume_owned(
        factory, identity, read_only, stop, progress, inbox,
    ))
}

async fn resume_owned(
    factory: &Factory,
    identity: RunId,
    read_only: bool,
    stop: &Cancel,
    progress: watch::Sender<Option<RunView>>,
    inbox: Clarifications,
) -> Result<Completed> {
    let storage = run_root(factory, &identity.parent_session).join(&identity.run);
    let mut run = Run::open(&storage.join("ledger"), &identity)?;
    if read_only && !run.plan().is_read_only() {
        return Err("plan_mode_read_only");
    }
    if !run.pending_clarifications().is_empty() && run.active_plus_task().is_none() {
        return Err("orchestration_clarification_needs_review");
    }
    run.check_continuation(now()?)?;
    if !continuable(&run, now()?) {
        return Err("orchestration_continuation_unavailable");
    }
    let context = run.context().ok_or("orchestration_context")?;
    factory.check_orchestration_context(&Request::load(&Json::parse(&context.configuration)?)?)?;
    if let Some(profile) = &context.plus_profile {
        let profile = RoleProfile::decode(&Json::parse(profile)?)?;
        let candidate = writer_workspace(factory, &run, &storage, true, stop)?;
        run.continue_run(&[])?;
        return plus::schedule(
            factory, run, &storage, candidate, &profile, stop, progress, inbox,
        )
        .await;
    }
    factory.require_orchestration_route(stop)?;
    let candidate = writer_workspace(factory, &run, &storage, true, stop)?;
    let active = restore_active(factory, &mut run, stop).await?;
    schedule(factory, run, candidate, active, stop, progress, inbox).await
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
            distinct_turns: false,
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
    run.check_continuation(now()?)?;
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

pub(super) fn run_root(factory: &Factory, parent: &str) -> PathBuf {
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
