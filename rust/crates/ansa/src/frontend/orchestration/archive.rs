//! Rebuild agent cards from the existing orchestration receipts.
use super::{
    AgentLine, AgentRole, Cancel, Factory, Json, Path, Request, RoleProfile, Run, RunId, RunView,
    child_status, fs, local, run_root,
};
use crate::{
    orchestration::run::{InspectedChild, RunInspection},
    session::catalog::{SessionSummary, SummaryStatus},
};
use std::collections::BTreeMap;

pub(in crate::frontend) fn agent_views(
    factory: &Factory,
    parent: &str,
    summaries: &[SessionSummary],
    stop: &Cancel,
) -> Vec<RunView> {
    let root = run_root(factory, parent);
    let Ok(directory) = fs::private_directory(&root) else {
        return Vec::new();
    };
    let mut views = Vec::new();
    let mut budget = 64 * 1024 * 1024;
    let _ = local::visit_names(&directory, |name| {
        stop.check().map_err(|_| std::io::ErrorKind::Interrupted)?;
        let Some(id) = name
            .to_str()
            .filter(|id| crate::provider::contracts::valid_id(id))
        else {
            return Ok(());
        };
        let storage = root.join(id);
        if fs::private_directory(&storage).is_err() {
            return Ok(());
        }
        let ledger = storage.join("ledger");
        let identity = RunId {
            parent_session: parent.into(),
            run: id.into(),
        };
        let Ok(run) = Run::inspect(&ledger, &identity, &mut budget) else {
            return Ok(());
        };
        if let Some(view) = view(&identity, &run, &storage, summaries, stop, &mut budget) {
            let modified = ledger
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok();
            views.push((modified, view));
        }
        Ok(())
    });
    views.sort_by_key(|(modified, _)| *modified);
    views.into_iter().map(|(_, view)| view).collect()
}

fn view(
    identity: &RunId,
    run: &RunInspection,
    storage: &Path,
    summaries: &[SessionSummary],
    stop: &Cancel,
    budget: &mut usize,
) -> Option<RunView> {
    let context = run.context.as_ref()?;
    let agents = if let Some(profile) = &context.plus_profile {
        let profile = RoleProfile::decode(&Json::parse(profile).ok()?).ok()?;
        plus_agents(identity, run, storage, &profile, summaries, stop, budget)
    } else {
        let source = Request::load(&Json::parse(&context.configuration).ok()?).ok()?;
        run.children
            .iter()
            .filter_map(|child| {
                let step = run.plan.steps().iter().find(|step| step.id == child.step)?;
                let line = card(
                    child,
                    &step.task,
                    Some(&step.id),
                    AgentRole::worker(step.access),
                    &source.binding.requested_model,
                    summaries,
                );
                Some((child.session.clone(), line))
            })
            .collect()
    };
    if agents.is_empty() {
        return None;
    }
    Some(RunView {
        id: identity.run.clone(),
        plan: run.plan.clone(),
        completed_steps: Default::default(),
        reviewing: false,
        clarification_pending: false,
        pause_reason: None,
        agents,
    })
}

fn plus_agents(
    parent: &RunId,
    run: &RunInspection,
    storage: &Path,
    profile: &RoleProfile,
    summaries: &[SessionSummary],
    stop: &Cancel,
    budget: &mut usize,
) -> Vec<(String, AgentLine)> {
    let path = storage.join("plus");
    let Ok(directory) = fs::private_directory(&path) else {
        return Vec::new();
    };
    let plan = &run.plan;
    let steps: BTreeMap<_, _> = plan
        .steps()
        .iter()
        .enumerate()
        .map(|(index, step)| (super::plus::phase_step_key(&step.id), (index, step)))
        .collect();
    let mut phases = Vec::new();
    let _ = local::visit_names(&directory, |name| {
        stop.check().map_err(|_| std::io::ErrorKind::Interrupted)?;
        let Some(id) = name
            .to_str()
            .filter(|id| crate::provider::contracts::valid_id(id))
        else {
            return Ok(());
        };
        let Some((order, step, role, model)) = phase(id, &steps, profile) else {
            return Ok(());
        };
        let identity = RunId {
            parent_session: parent.parent_session.clone(),
            run: id.into(),
        };
        let Ok(receipt) = Run::inspect(&path.join(id), &identity, budget) else {
            return Ok(());
        };
        let Some(child) = receipt.children.iter().find(|child| child.step == id) else {
            return Ok(());
        };
        let title = step.map_or(plan.summary(), |step| step.task.as_str());
        let line = card(
            child,
            title,
            step.map(|step| step.id.as_str()),
            role,
            model,
            summaries,
        );
        phases.push((order, child.session.clone(), line));
        Ok(())
    });
    phases.sort_by_key(|phase| std::cmp::Reverse(phase.0));
    let mut seen = std::collections::BTreeSet::new();
    phases
        .into_iter()
        .filter_map(|(_, session, line)| seen.insert(session.clone()).then_some((session, line)))
        .collect()
}

type PhaseOrder = (usize, u32, u32);

fn phase<'a>(
    id: &str,
    steps: &BTreeMap<String, (usize, &'a super::Step)>,
    profile: &'a RoleProfile,
) -> Option<(PhaseOrder, Option<&'a super::Step>, AgentRole, &'a str)> {
    let suffix = id.strip_prefix("plus-")?;
    if let Some(review) = suffix.strip_prefix("review-") {
        let (order, role, model) = match review {
            "report" => (0, AgentRole::Review, &profile.strong),
            "report-repair" => (1, AgentRole::Research, &profile.medium),
            "report-verify" => (2, AgentRole::Review, &profile.strong),
            _ => {
                let (stage, attempt) = review.rsplit_once('-').unwrap_or(("", review));
                let attempt = attempt.parse::<u8>().ok()? as u32;
                match stage {
                    "" => (attempt * 3, AgentRole::Review, &profile.strong),
                    "coordinate" => (attempt * 3 + 1, AgentRole::Coordinator, &profile.medium),
                    "repair" => (attempt * 3 + 2, AgentRole::Implementation, &profile.weak),
                    _ => return None,
                }
            }
        };
        return Some(((usize::MAX, 0, order), None, role, &model.model));
    }
    let (key, phase) = suffix.split_once('-')?;
    let (phase, attempt) = phase.rsplit_once('-')?;
    let (stage, cycle) = phase.rsplit_once('-')?;
    let (index, step) = *steps.get(key)?;
    let cycle = cycle.parse().ok()?;
    let attempt = attempt.parse::<u8>().ok()? as u32;
    let (order, role, model) = match stage {
        "draft" | "draft-replan" => (0, AgentRole::Coordinator, &profile.medium),
        "work" => (
            1 + attempt * 2,
            AgentRole::worker(step.access),
            &profile.weak,
        ),
        "check" => (2 + attempt * 2, AgentRole::Coordinator, &profile.medium),
        "escalate" => (7, AgentRole::Review, &profile.strong),
        "fix" => (8, AgentRole::worker(step.access), &profile.medium),
        "verify" => (9, AgentRole::Coordinator, &profile.medium),
        _ => return None,
    };
    Some(((index, cycle, order), Some(step), role, &model.model))
}

fn card(
    child: &InspectedChild,
    title: &str,
    step: Option<&str>,
    role: AgentRole,
    model: &str,
    summaries: &[SessionSummary],
) -> AgentLine {
    let (state, activity) = match &child.result {
        Some(result) => (child_status(result), result.summary.clone()),
        None if child.cancelled => ("Stopped", String::new()),
        None => summaries
            .iter()
            .find(|summary| summary.entry.id == child.session)
            .filter(|summary| summary.status == SummaryStatus::Paused)
            .map_or(("Unresolved", String::new()), |summary| {
                (
                    "Paused",
                    summary
                        .pause_diagnostic
                        .as_deref()
                        .map_or_else(String::new, crate::frontend::diagnostics::error),
                )
            }),
    };
    AgentLine {
        id: child.session.clone(),
        title: title.lines().next().unwrap_or_default().to_owned(),
        state: state.into(),
        role,
        model: model.to_owned(),
        activity,
        step: step.map(str::to_owned),
        tool_calls: child.tool_calls,
    }
}
