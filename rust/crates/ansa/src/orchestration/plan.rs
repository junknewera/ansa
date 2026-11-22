//! Validate proposed tasks before the user can approve dispatch.
use crate::provider::contracts::{AgentMode, Submit, valid_id};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

type Result<T> = std::result::Result<T, &'static str>;

const PLAN_STEPS: usize = 16;
const ACTIVE_CHILDREN: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub task: String,
    pub access: Access,
    pub paths: Vec<String>,
    pub depends_on: Vec<String>,
    pub acceptance: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestedOutcome {
    Implementation,
    Investigation,
}

/// Visible label for typed helper assignments, including journals written before Carbon had a roster.
pub(crate) fn assignment_title(text: &str) -> String {
    let first = text.lines().next().unwrap_or(text);
    let Some((prompt, metadata)) = text.rsplit_once("\n\nШаг ") else {
        return first.into();
    };
    let stage = metadata.lines().next().unwrap_or_default();
    if !stage.starts_with("plus-") {
        return first.into();
    }
    if stage.starts_with("plus-review") {
        return if stage.contains("-repair-") {
            "Review fixes"
        } else if stage.contains("-coordinate-") {
            "Plan review fixes"
        } else {
            "Code and security review"
        }
        .into();
    }
    let role =
        if stage.contains("-draft-") || stage.contains("-check-") || stage.contains("-verify-") {
            "Coordinator"
        } else if stage.contains("-escalate-") {
            "Escalation"
        } else {
            "Worker"
        };
    let task = prompt
        .lines()
        .skip_while(|line| *line != "Task" && !line.starts_with("Task "))
        .nth(1)
        .or_else(|| {
            prompt
                .strip_prefix("Assigned task ")
                .and_then(|rest| rest.lines().nth(1))
        });
    match task.filter(|task| !task.trim().is_empty()) {
        Some(task) => format!("{role} · {task}"),
        None => role.to_owned(),
    }
}

impl Step {
    pub fn assignment(&self) -> Submit {
        let scope = if self.paths.is_empty() {
            "весь проект".to_owned()
        } else {
            self.paths.join(", ")
        };
        Submit {
            text: format!(
                "{}\n\nШаг {}\nОбласть — {}\nРезультат — {}",
                self.task, self.id, scope, self.acceptance
            ),
            mode: match self.access {
                Access::Read => AgentMode::Plan,
                Access::Write => AgentMode::Basic,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_outcome: Option<RequestedOutcome>,
    schema_version: u8,
    summary: String,
    steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_folder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command_checks: Vec<CommandCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    manual_checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Check {
    pub path: String,
    pub contains: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandCheck {
    pub argv: Vec<String>,
}

/// Immutable, validated description of the tasks offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan(Proposal);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Running,
    Succeeded,
    Failed,
}

impl Plan {
    pub fn parse(input: &str) -> Result<Self> {
        crate::protocol::json::Json::parse(input).map_err(|_| "plan_schema")?;
        let proposal: Proposal = serde_json::from_str(input).map_err(|_| "plan_schema")?;
        if proposal.schema_version != 1
            || proposal.steps.len() > PLAN_STEPS
            || proposal.checks.len() > 16
            || proposal.command_checks.len() > 8
            || proposal.manual_checks.len() > 8
            || !valid_text(&proposal.summary)
        {
            return Err("plan_limits");
        }
        if proposal
            .checks
            .iter()
            .any(|check| !valid_path(&check.path) || !valid_text(&check.contains))
            || proposal
                .manual_checks
                .iter()
                .any(|check| !valid_text(check))
            || proposal.command_checks.iter().any(|check| {
                check.argv.is_empty()
                    || check.argv.len() > 64
                    || check.argv[0].trim().is_empty()
                    || check
                        .argv
                        .iter()
                        .any(|arg| arg.chars().any(char::is_control))
                    || check.argv.iter().map(String::len).sum::<usize>() > 4096
            })
        {
            return Err("plan_check_scope");
        }
        let mut ids = BTreeSet::new();
        let mut writes = BTreeSet::new();
        for step in &proposal.steps {
            if !valid_id(&step.id) || !ids.insert(step.id.as_str()) {
                return Err("plan_step_identity");
            }
            if !valid_text(&step.task) || !valid_text(&step.acceptance) {
                return Err("plan_step_description");
            }
            if step.paths.len() > 32 || step.access == Access::Write && step.paths.is_empty() {
                return Err("plan_step_scope");
            }
            let mut paths = BTreeSet::new();
            if step
                .paths
                .iter()
                .any(|path| !valid_path(path) || !paths.insert(path))
            {
                return Err("plan_step_path");
            }
            if !crate::protocol::json::Json::text_fits_frame(&step.assignment().text) {
                return Err("plan_step_description");
            }
            if step.access == Access::Write {
                writes.extend(step.paths.iter());
                if writes.len() > 32 {
                    return Err("plan_step_scope");
                }
            }
        }
        if (!proposal.checks.is_empty() || !proposal.command_checks.is_empty()) && writes.is_empty()
        {
            return Err("plan_check_scope");
        }
        if let Some(folder) = &proposal.read_folder
            && (folder.trim().is_empty()
                || folder.len() > 4096
                || folder.chars().any(char::is_control)
                || !writes.is_empty())
        {
            return Err("plan_read_folder");
        }
        for step in &proposal.steps {
            let mut dependencies = BTreeSet::new();
            if step
                .depends_on
                .iter()
                .any(|id| id == &step.id || !ids.contains(id.as_str()) || !dependencies.insert(id))
            {
                return Err("plan_dependency");
            }
        }
        let mut visited = BTreeSet::new();
        while visited.len() < ids.len() {
            let before = visited.len();
            for step in &proposal.steps {
                if step
                    .depends_on
                    .iter()
                    .all(|id| visited.contains(id.as_str()))
                {
                    visited.insert(step.id.as_str());
                }
            }
            if before == visited.len() {
                return Err("plan_dependency_cycle");
            }
        }
        Ok(Self(proposal))
    }

    pub fn summary(&self) -> &str {
        &self.0.summary
    }

    pub fn steps(&self) -> &[Step] {
        &self.0.steps
    }

    pub(crate) fn read_folder(&self) -> Option<&str> {
        self.0.read_folder.as_deref()
    }

    pub(crate) fn resolve_read_folder(&mut self, folder: String) -> Result<()> {
        if self.0.read_folder.is_none()
            || !Path::new(&folder).is_absolute()
            || folder.len() > 4096
            || folder.chars().any(char::is_control)
        {
            return Err("plan_read_folder");
        }
        self.0.read_folder = Some(folder);
        Ok(())
    }

    pub(crate) fn checks(&self) -> &[Check] {
        &self.0.checks
    }

    pub(crate) fn command_checks(&self) -> &[CommandCheck] {
        &self.0.command_checks
    }

    pub(crate) fn manual_checks(&self) -> &[String] {
        &self.0.manual_checks
    }

    pub(crate) fn require_implementation(&mut self) {
        self.0.requested_outcome = Some(RequestedOutcome::Implementation);
    }

    pub(crate) fn execution_ready(&self) -> Result<()> {
        if let Some(outcome) = self.0.requested_outcome {
            match outcome {
                RequestedOutcome::Implementation
                    if self.is_read_only() || self.command_checks().is_empty() =>
                {
                    return Err("orchestration_implementation_required");
                }
                RequestedOutcome::Investigation if !self.is_read_only() => {
                    return Err("orchestration_investigation_only");
                }
                _ => {}
            }
        }
        if self.is_read_only() {
            return Ok(());
        }
        if self.checks().is_empty() && self.command_checks().is_empty() {
            return Err("orchestration_missing_checks");
        }
        Ok(())
    }
    pub(crate) fn is_read_only(&self) -> bool {
        self.steps().iter().all(|step| step.access == Access::Read)
    }

    pub(crate) fn candidate_access(&self, step: &Step) -> bool {
        if step.access == Access::Write {
            return true;
        }
        let mut pending: Vec<_> = step.depends_on.iter().collect();
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if visited.insert(id)
                && let Some(dependency) = self.steps().iter().find(|task| &task.id == id)
            {
                if dependency.access == Access::Write {
                    return true;
                }
                pending.extend(&dependency.depends_on);
            }
        }
        false
    }

    pub fn markdown(&self) -> String {
        self.markdown_sections().join("\n\n")
    }

    /// Parse these separately in approval views so model-authored Markdown cannot
    /// change how a subsequent permission, path or command appears.
    pub(crate) fn markdown_sections(&self) -> Vec<String> {
        let mut sections = vec![self.summary().to_owned()];
        if let Some(folder) = self.read_folder() {
            sections.push(format!(
                "**Read folder**\n\n{}",
                literal_json(serde_json::json!(folder))
            ));
        }
        if !self.command_checks().is_empty() {
            sections.push("**Command runs in the project copy with your permissions**".into());
            for check in self.command_checks() {
                sections.push(literal_json(serde_json::json!(check.argv)));
            }
        }
        for (index, step) in self.steps().iter().enumerate() {
            let access = if step.access == Access::Read {
                "read"
            } else {
                "write"
            };
            sections.push(format!("### {}. {} ({access})", index + 1, step.id));
            sections.push(step.task.clone());
            if step.paths.is_empty() {
                sections.push("**Read scope** · entire workspace".into());
            } else {
                sections.push(format!(
                    "**Paths**\n\n{}",
                    literal_json(serde_json::json!(step.paths))
                ));
            }
            if !step.depends_on.is_empty() {
                sections.push(format!("**Requires** · {}", step.depends_on.join(", ")));
            }
            sections.push(format!("**Expected result** · {}", step.acceptance));
        }
        if !self.is_read_only() {
            sections
                .push("**Verification**\n\nThe final changes require independent review.".into());
        }
        for check in self.checks() {
            sections.push(literal_json(
                serde_json::json!({"path": check.path, "contains": check.contains}),
            ));
        }
        if !self.manual_checks().is_empty() {
            sections.push(
                if self.is_read_only() {
                    "**Follow-up recommendation**"
                } else {
                    "**Manual checks**"
                }
                .into(),
            );
            sections.extend(self.manual_checks().iter().cloned());
        }
        sections
    }

    pub fn encode(&self) -> Result<String> {
        serde_json::to_string(&self.0).map_err(|_| "plan_schema")
    }

    /// A step may start after its dependencies succeed and while there is a
    /// compatible slot. This projection does not itself authorize dispatch.
    pub fn ready(&self, progress: &BTreeMap<String, Progress>) -> Result<Vec<&Step>> {
        let steps = self.steps();
        for id in progress.keys() {
            let step = steps
                .iter()
                .find(|step| step.id == *id)
                .ok_or("plan_progress_identity")?;
            if !dependencies_succeeded(step, progress) {
                return Err("plan_progress_dependency");
            }
        }
        let mut active: Vec<_> = steps
            .iter()
            .filter(|step| progress.get(&step.id) == Some(&Progress::Running))
            .collect();
        if active.len() > ACTIVE_CHILDREN
            || active.iter().enumerate().any(|(index, step)| {
                active[index + 1..]
                    .iter()
                    .any(|other| !compatible(step, other))
            })
        {
            return Err("plan_progress_conflict");
        }
        let mut ready = Vec::new();
        for step in steps {
            if active.len() == ACTIVE_CHILDREN {
                break;
            }
            if !progress.contains_key(&step.id)
                && dependencies_succeeded(step, progress)
                && active.iter().all(|other| compatible(step, other))
            {
                active.push(step);
                ready.push(step);
            }
        }
        Ok(ready)
    }
}

/// ASCII JSON preserves argument boundaries and exposes invisible and bidi characters.
fn literal_json(value: serde_json::Value) -> String {
    use std::fmt::Write as _;
    let mut literal = String::from("```\n");
    for ch in value.to_string().chars() {
        if ch.is_ascii_graphic() || ch == ' ' {
            literal.push(ch);
        } else {
            for unit in ch.encode_utf16(&mut [0; 2]) {
                let _ = write!(literal, "\\u{unit:04x}");
            }
        }
    }
    literal.push_str("\n```");
    literal
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
}

pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".." | ".git"))
        && !ansa_linux::fs::protected_path(Path::new(path))
}

fn dependencies_succeeded(step: &Step, progress: &BTreeMap<String, Progress>) -> bool {
    step.depends_on
        .iter()
        .all(|dependency| progress.get(dependency) == Some(&Progress::Succeeded))
}

fn compatible(first: &Step, second: &Step) -> bool {
    match (first.access, second.access) {
        (Access::Read, Access::Read) => true,
        (Access::Write, Access::Write) => false,
        _ => {
            !first.paths.is_empty()
                && !second.paths.is_empty()
                && first.paths.iter().all(|first| {
                    let first = Path::new(first);
                    second.paths.iter().all(|second| {
                        let second = Path::new(second);
                        !first.starts_with(second) && !second.starts_with(first)
                    })
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, access: Access, path: &str, depends_on: &[&str]) -> Step {
        Step {
            id: id.into(),
            task: format!("Inspect or address {id}"),
            access,
            paths: vec![path.into()],
            depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(),
            acceptance: "Explain the evidence for the result".into(),
        }
    }

    fn plan(steps: Vec<Step>) -> Result<Plan> {
        Plan::parse(
            &serde_json::to_string(&Proposal {
                requested_outcome: None,
                schema_version: 1,
                summary: "Find out why the report sometimes comes back empty".into(),
                steps,
                read_folder: None,
                checks: Vec::new(),
                command_checks: Vec::new(),
                manual_checks: Vec::new(),
            })
            .unwrap(),
        )
    }

    #[test]
    fn implementation_requires_an_approved_edit_and_executable_check() {
        let mut proposed = plan(vec![step("inspect", Access::Read, "src", &[])]).unwrap();
        assert!(proposed.execution_ready().is_ok());
        proposed.require_implementation();
        assert_eq!(
            proposed.execution_ready(),
            Err("orchestration_implementation_required")
        );
        proposed.0.steps[0].access = Access::Write;
        assert_eq!(
            proposed.execution_ready(),
            Err("orchestration_implementation_required")
        );
        proposed.0.command_checks.push(CommandCheck {
            argv: vec!["test".into(), "-s".into(), "src".into()],
        });
        assert!(proposed.execution_ready().is_ok());
        proposed.0.requested_outcome = Some(RequestedOutcome::Investigation);
        assert_eq!(
            proposed.execution_ready(),
            Err("orchestration_investigation_only")
        );
    }

    fn ids<'a>(plan: &'a Plan, progress: &BTreeMap<String, Progress>) -> Vec<&'a str> {
        plan.ready(progress)
            .unwrap()
            .iter()
            .map(|step| step.id.as_str())
            .collect()
    }

    #[test]
    fn vague_task_can_propose_two_independent_reads_before_writing() {
        let plan = plan(vec![
            step("fix", Access::Write, "src/report.rs", &["input", "format"]),
            step("input", Access::Read, "src/input.rs", &[]),
            step("format", Access::Read, "src/report.rs", &[]),
            step("tests", Access::Read, "tests", &[]),
        ])
        .unwrap();
        assert_eq!(Plan::parse(&plan.encode().unwrap()), Ok(plan.clone()));
        assert_eq!(ids(&plan, &BTreeMap::new()), ["input", "format"]);
        let mut progress = BTreeMap::from([
            ("input".into(), Progress::Succeeded),
            ("format".into(), Progress::Running),
        ]);
        assert_eq!(ids(&plan, &progress), ["tests"]);
        progress.insert("format".into(), Progress::Succeeded);
        assert_eq!(ids(&plan, &progress), ["fix", "tests"]);
        progress.insert("format".into(), Progress::Failed);
        assert_eq!(ids(&plan, &progress), ["tests"]);
        progress.insert("fix".into(), Progress::Succeeded);
        assert_eq!(plan.ready(&progress), Err("plan_progress_dependency"));
    }

    #[test]
    fn writer_blocks_other_writers_and_overlapping_reads() {
        let plan = plan(vec![
            step("fix", Access::Write, "src/report.rs", &[]),
            step("other-fix", Access::Write, "src/input.rs", &[]),
            step("source", Access::Read, "src", &[]),
            step("tests", Access::Read, "tests", &[]),
        ])
        .unwrap();
        assert_eq!(ids(&plan, &BTreeMap::new()), ["fix", "tests"]);
        let mut progress = BTreeMap::from([("fix".into(), Progress::Running)]);
        assert_eq!(ids(&plan, &progress), ["tests"]);
        progress.insert("source".into(), Progress::Running);
        assert_eq!(plan.ready(&progress), Err("plan_progress_conflict"));
    }

    #[test]
    fn rejects_a_cycle_missing_dependency_and_protected_paths() {
        assert_eq!(
            plan(vec![
                step("first", Access::Read, "src", &["second"]),
                step("second", Access::Read, "tests", &["first"]),
            ]),
            Err("plan_dependency_cycle")
        );
        assert_eq!(
            plan(vec![step("first", Access::Read, "src", &["missing"])]),
            Err("plan_dependency")
        );
        for path in [
            "/etc",
            "src/../secret",
            ".git/index",
            "src/.env.local",
            "src//report",
        ] {
            assert_eq!(
                plan(vec![step("first", Access::Write, path, &[])]),
                Err("plan_step_path"),
                "{path}"
            );
        }
        let mut write = step("first", Access::Write, "src", &[]);
        write.paths.clear();
        assert_eq!(plan(vec![write]), Err("plan_step_scope"));
    }
}
