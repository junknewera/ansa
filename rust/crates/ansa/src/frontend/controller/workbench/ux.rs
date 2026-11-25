//! Workspace interactions. Typed runtime state remains the source of truth.
use super::*;
use crate::frontend::terminal_input::TerminalEvent;
use std::sync::Arc;

pub(super) struct ConversationAgent {
    pub parent: String,
    pub branch: String,
    pub line: carbon::AgentLine,
}

impl ConversationAgent {
    pub fn into_tree_line(mut self) -> carbon::AgentLine {
        self.line.title.insert_str(0, &self.branch);
        self.line
    }
}

pub(super) struct CandidateView {
    owner: String,
    run: String,
    commit: String,
    heading: String,
    title: String,
    rows: Arc<Vec<crate::frontend::diff::Row>>,
    checks: Vec<Arc<carbon::OutputView>>,
    manual: Option<Arc<carbon::OutputView>>,
}

fn step_progress(label: &str, view: &orchestration::RunView) -> String {
    format!(
        "{label} · {}/{} steps done",
        view.completed_steps.len(),
        view.plan.steps().len()
    )
}

impl CandidateView {
    pub(super) fn new(owner: String, run: String, candidate: orchestration::Candidate) -> Self {
        use crate::frontend::diff::{Kind, patch_rows};
        let rows = patch_rows(&candidate.diff);
        let added = rows.iter().filter(|row| row.kind == Kind::Add).count();
        let removed = rows.iter().filter(|row| row.kind == Kind::Remove).count();
        let changed = match candidate.paths.as_slice() {
            [path] => path.clone(),
            paths => format!("Files: {}", paths.len()),
        };
        let heading = format!("{changed} · +{added} −{removed}");
        let (title, manual) = if candidate.manual_checks.is_empty() {
            (format!("{heading} · project snapshot → ansa-result"), None)
        } else {
            let text = format!(
                "Candidate {}\nCheckout {}\n\n{}",
                candidate.commit,
                candidate.workspace.display(),
                candidate.manual_checks.join("\n\n"),
            );
            (
                format!("{heading} · Manual checks pending"),
                Some(Arc::new(carbon::OutputView {
                    title: "Manual checks pending".into(),
                    text,
                })),
            )
        };
        Self {
            owner,
            run,
            commit: candidate.commit,
            heading,
            title,
            rows: Arc::new(rows),
            checks: candidate.checks.into_iter().map(Arc::new).collect(),
            manual,
        }
    }

    pub(super) fn panel(&self) -> Panel {
        Panel::EditDiff {
            title: if self.manual.is_some() {
                self.title.clone()
            } else {
                format!("{} · Verified changes", self.title)
            },
            rows: self.rows.clone(),
            offset: 0,
            column: 0,
        }
    }
}

impl Slot {
    pub(super) fn remember_candidate(&mut self, review: CandidateView) {
        self.candidate_reviews
            .retain(|previous| previous.run != review.run);
        if self.candidate_reviews.len() >= 4 {
            self.candidate_reviews.pop_front();
        }
        self.candidate_reviews.push_back(review);
    }
}

impl Workbench {
    pub(super) fn projected(&self) -> Option<Vec<Entry>> {
        self.projected_slot(&self.slots[self.current])
    }

    pub(super) fn projected_slot(&self, slot: &Slot) -> Option<Vec<Entry>> {
        let activity_open = matches!(&self.panel, Some(Panel::Activity(nav)) if !nav.agents_only);
        let delegates = self.live_delegates.borrow();
        let delegate = delegates
            .agents
            .values()
            .find(|agent| agent.parent == slot.id);
        slot.conversation(!activity_open, delegate)
    }

    pub(super) fn orchestration_notice(&self) -> Option<String> {
        let owner = &self.slots[self.current].id;
        if self.orchestration_summary.contains_key(owner) {
            return Some("Writing the summary".into());
        }
        if !self.collecting_orchestration() {
            return None;
        }
        Some(self.orchestration_views.get(owner).map_or_else(
            || "Starting agents".into(),
            |view| {
                let label = if view.reviewing {
                    "Reviewing changes"
                } else {
                    "Agents"
                };
                step_progress(label, view)
            },
        ))
    }

    pub(super) fn paused_orchestration_notice(&self) -> Option<String> {
        let owner = &self.slots[self.current].id;
        let reason = self.orchestration_views.get(owner)?.pause_reason.as_ref()?;
        (!self.collecting_orchestration())
            .then(|| format!("Orchestration paused · {reason} · /sessions for options"))
    }

    pub(super) fn terminal_activity(&self) -> Vec<Entry> {
        let slot = &self.slots[self.current];
        let mut entries = slot.projected().unwrap_or_default();
        let at_tail = slot
            .activity_page()
            .is_none_or(|page| page.end == page.total);
        for review in &slot.candidate_reviews {
            let plan_key = format!("plan:{}", review.run);
            let index = entries
                .iter()
                .position(|entry| entry.action.as_ref() == Some(&plan_key));
            let index = match index {
                Some(index) => Some(index + 1),
                None if at_tail => {
                    entries.push(result_entry("Approved plan".into(), Some(plan_key), None));
                    Some(entries.len())
                }
                None => None,
            };
            if let Some(index) = index {
                let diff = Some(format!("candidate:{}", review.run));
                let mut review_entries = vec![result_entry(review.heading.clone(), diff, None)];
                let changed = |row: &crate::frontend::diff::Row| {
                    use crate::frontend::diff::Kind;
                    matches!(row.kind, Kind::Add | Kind::Remove)
                };
                review_entries.extend(
                    review
                        .rows
                        .iter()
                        .skip_while(|row| !changed(row))
                        .take_while(|row| changed(row))
                        .take(3)
                        .map(|row| Entry {
                            role: row.role().into(),
                            text: row.display(),
                            action: None,
                            status: None,
                            edit: None,
                            output: None,
                            agent: None,
                        }),
                );
                review_entries.extend(review.checks.iter().enumerate().map(|(index, view)| {
                    result_entry(
                        format!("{} · passed", view.title),
                        Some(format!("output:verification:{}:{index}", review.run)),
                        Some(view.clone()),
                    )
                }));
                if let Some(manual) = &review.manual {
                    review_entries.push(result_entry(
                        manual.title.clone(),
                        Some(format!("output:manual:{}", review.run)),
                        Some(manual.clone()),
                    ));
                    if review.owner == slot.id {
                        review_entries.push(result_entry(
                            "Confirm checks".into(),
                            Some(format!("checks:{}", review.run)),
                            None,
                        ));
                    }
                }
                entries.splice(index..index, review_entries);
            }
        }
        if !at_tail {
            return entries;
        }
        let collecting = self.collecting_orchestration();
        let view = self.orchestration_views.get(&slot.id);
        if let Some(view) = view {
            let saved_plan = format!("plan:{}", view.id);
            if !entries
                .iter()
                .any(|entry| entry.action.as_ref() == Some(&saved_plan))
            {
                entries.push(result_entry(
                    "Approved plan".into(),
                    Some("plan:active".into()),
                    None,
                ));
            }
            if collecting {
                entries.push(result_entry(
                    "Clarify plan".into(),
                    Some(format!("clarify:{}", view.id)),
                    None,
                ));
            }
        }
        let notice = self
            .orchestration_notice()
            .or_else(|| view.map(|view| step_progress("Agents", view)));
        if let Some(notice) = notice {
            entries.push(Entry {
                text: if view.is_some_and(|view| view.clarification_pending) && collecting {
                    "Coordinator receives the clarification after the current step".into()
                } else {
                    notice
                },
                role: if collecting || self.orchestration_summary.contains_key(&slot.id) {
                    "activity"
                } else {
                    "tool_detail"
                }
                .into(),
                action: None,
                status: None,
                edit: None,
                output: None,
                agent: None,
            });
        }
        entries.extend(
            self.agent_rows()
                .into_iter()
                .rev()
                .map(ConversationAgent::into_tree_line)
                .map(|agent| Entry {
                    role: "agent_card".into(),
                    text: String::new(),
                    action: Some(format!("agent:{}", agent.id)),
                    status: None,
                    edit: None,
                    output: None,
                    agent: Some(Arc::new(agent)),
                }),
        );
        if let Some(text) = self.paused_orchestration_notice() {
            entries.push(Entry {
                role: "tool_warning".into(),
                text,
                action: None,
                status: None,
                edit: None,
                output: None,
                agent: None,
            });
        }
        if !collecting && let Some(view) = view.filter(|view| view.clarification_pending) {
            entries.push(Entry {
                text: "Plan and saved clarification".into(),
                role: "activity".into(),
                action: Some(format!("plan:{}", view.id)),
                status: None,
                edit: None,
                output: None,
                agent: None,
            });
        }
        entries
    }
    pub(super) fn navigating(&self) -> bool {
        self.menu.is_some()
            || (self.focus == Focus::Chat && self.result_selected.is_some())
            || (self.focus == Focus::Inspector && !matches!(self.panel, Some(Panel::Terminal)))
    }

    pub(super) fn relative_directory(&self, path: &Path) -> String {
        match path.strip_prefix(&self.factory.settings.cwd) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    pub(super) fn show_orchestration_plan(
        &mut self,
        plan: &crate::orchestration::plan::Plan,
        prompt: Option<&str>,
        profile: Option<&RoleProfile>,
        can_request_implementation: bool,
    ) {
        let mut sections = Vec::new();
        if can_request_implementation {
            sections.push("**Read-only investigation**  \nThis run produces a report. Select Implement to request changes and tests.".into());
        }
        if let Some(profile) = profile {
            sections.push(roles::profile_lines(profile).join("  \n"));
        }
        if let Err(code) = plan.execution_ready() {
            sections.push(match code {
                "orchestration_implementation_required" => "**Implementation requires a write step and an executable test. Select Refine.**",
                "orchestration_investigation_only" => "**An investigation plan must contain only read steps. Select Refine.**",
                _ => "**Select Refine to add a check for the changes.**",
            }.into());
        }
        if !plan.is_read_only() && !plan.manual_checks().is_empty() {
            sections.push(
                "**Manual checks are due after implementation and automated verification.**".into(),
            );
        }
        if let Some(prompt) = prompt {
            sections.push(format!("## Original request\n\n{prompt}"));
        }
        sections.extend(plan.markdown_sections());
        self.file_token = self.file_token.wrapping_add(1);
        self.panel = Some(Panel::Orchestration {
            document: crate::frontend::markdown::Document::sections(&sections),
            offset: 0,
        });
        self.focus = Focus::Inspector;
    }

    pub(super) fn navigate_results(&mut self, older: Option<bool>) {
        let Some(frame) = &self.displayed else { return };
        let mut seen = std::collections::BTreeSet::new();
        let keys: Vec<_> = frame
            .tool_actions
            .iter()
            .filter(|(_, key)| seen.insert(key))
            .collect();
        let selected = self
            .result_selected
            .as_ref()
            .and_then(|key| keys.iter().position(|(_, shown)| key == shown));
        let index = match (selected, older) {
            (_, None) if self.result_selected.is_some() => {
                self.result_selected = None;
                self.leave_results();
                return;
            }
            (Some(index), None) => index,
            (Some(index), Some(true)) => index.saturating_sub(1),
            (Some(index), Some(false)) => (index + 1).min(keys.len().saturating_sub(1)),
            (None, _) => 0,
        };
        let next = keys.get(index).map(|(_, key)| key.clone());
        if self.result_selected.is_none() && next.is_some() {
            let projected = self.projected();
            let slot = &mut self.slots[self.current];
            self.result_tail = slot.state.follow_tail;
            if slot.state.follow_tail {
                slot.state.follow_tail = false;
                slot.state.reading = projected;
                slot.state.reading_total = slot
                    .state
                    .snapshot
                    .as_ref()
                    .map_or(0, |s| s.transcript_size);
                if let Some(page) = &slot.conversation_page {
                    slot.conversation_query.before = page.end;
                    slot.conversation_query.forward = false;
                }
            }
        }
        self.result_selected = next;
    }

    pub(super) fn activate_result(
        &mut self,
        key: String,
        edit: Option<std::sync::Arc<crate::frontend::diff::EditView>>,
        output: Option<std::sync::Arc<carbon::OutputView>>,
        io: &Io,
    ) {
        self.selection = None;
        let slot = &self.slots[self.current];
        if self.result_selected.is_some() && self.result_scroll.is_none() {
            self.result_scroll = Some(slot.state.scroll);
        }
        if let Some(request) = key.strip_prefix("resume:") {
            let payload = if let Some((id, operation)) = request.split_once(':') {
                slot.state
                    .snapshot
                    .as_ref()
                    .and_then(recoverable_request)
                    .filter(|(target, op)| target.0 == id && op.0 == operation)
                    .map(|(target, op)| CommandPayload::ResumeUnknown(target.clone(), op.clone()))
            } else {
                self.resume_target()
                    .filter(|target| target.0 == request)
                    .map(CommandPayload::Resume)
            };
            if let Some(payload) = payload {
                self.result_selected = None;
                self.slots[self.current].show_latest();
                self.post_resume(payload);
            }
        } else if let Some(id) = key.strip_prefix("agent:") {
            self.open_agent(id);
        } else if let Some(operation) = key.strip_prefix("image:") {
            let source = slot.activity_page().and_then(|page| {
                page.entries.iter().find_map(|entry| {
                    graphics::Source::from_entry(entry, &slot.id)
                        .filter(|(id, _)| *id == operation)
                        .map(|(_, source)| source)
                })
            });
            if let Some(source) = source {
                self.show_image(source, io);
                if self.result_selected.is_some() {
                    self.focus = Focus::Inspector;
                }
            }
        } else if let Some(turn) = key.strip_prefix("plan:draft:") {
            let Some(page) = slot.activity_page() else {
                return;
            };
            let plan: String = page
                .entries
                .iter()
                .filter(|row| row.turn.0 == turn && crate::session::timeline::draft_material(row))
                .map(|row| row.text.as_str())
                .collect();
            if !plan.is_empty() {
                self.panel = Some(Panel::Orchestration {
                    document: crate::frontend::markdown::Document::parse(&plan),
                    offset: 0,
                });
                self.focus = Focus::Inspector;
            }
        } else if let Some(run) = key.strip_prefix("plan:") {
            if run == "active" {
                if let Some(view) = self.orchestration_views.get(&slot.id) {
                    self.show_orchestration_plan(&view.plan.clone(), None, None, false);
                }
            } else if self.can_job(io) {
                let parent = self.slots[self.current].id.clone();
                let run = run.to_owned();
                self.file_token = self.file_token.wrapping_add(1);
                let token = self.file_token;
                self.panel = Some(Panel::Orchestration {
                    document: crate::frontend::markdown::Document::parse("Loading plan"),
                    offset: 0,
                });
                self.focus = Focus::Inspector;
                let factory = self.factory.clone();
                let stop = Cancel::linked(&self.stop);
                self.jobs.spawn_blocking(move || Job::ArchivedPlan {
                    token,
                    result: orchestration::archived_plan(&factory, parent, &run, &stop),
                });
            }
        } else if let Some(run) = key.strip_prefix("clarify:") {
            if self.collecting_orchestration()
                && !self.clarification_pending
                && self
                    .orchestration_views
                    .get(&slot.id)
                    .is_some_and(|view| view.id == run)
            {
                self.navigation_editor.clear();
                self.menu = Some(Menu::Clarify { run: run.into() });
            }
        } else if let Some(run) = key.strip_prefix("checks:") {
            if let Some(candidate) = slot.candidate_reviews.iter().find(|candidate| {
                candidate.run == run && candidate.owner == slot.id && candidate.manual.is_some()
            }) {
                self.operational_frame = None;
                self.last_frame = None;
                self.menu = Some(Menu::ConfirmChecks {
                    run: candidate.run.clone(),
                    commit: candidate.commit.clone(),
                });
            }
        } else if let Some(run) = key.strip_prefix("candidate:") {
            if let Some(review) = slot
                .candidate_reviews
                .iter()
                .find(|review| review.run == run)
            {
                self.panel = Some(review.panel());
                self.focus = Focus::Inspector;
            }
        } else if let Some((line, path)) = key
            .strip_prefix("file:")
            .and_then(|value| value.split_once(':'))
        {
            let token = self.file_token;
            self.open_file(PathBuf::from(path), io);
            if self.file_token != token
                && let Some(Panel::Files(files)) = &mut self.panel
            {
                files.offset = line.parse::<usize>().unwrap_or(0).saturating_sub(1);
            }
        } else if let Some(view) = output {
            if !matches!(self.panel, Some(Panel::Output { .. })) {
                self.output_back = self.panel.take().map(Box::new);
            }
            self.panel = Some(Panel::Output { view, offset: 0 });
            self.focus = Focus::Inspector;
        } else if let Some(edit) = edit {
            let filename = Path::new(&edit.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&edit.path);
            self.panel = Some(Panel::EditDiff {
                title: format!(
                    "{} · +{} -{} · Edit diff",
                    filename, edit.diff.added, edit.diff.removed
                ),
                rows: std::sync::Arc::new(edit.diff.rows.clone()),
                offset: 0,
                column: 0,
            });
            self.focus = Focus::Inspector;
        } else {
            let slot = &mut self.slots[self.current];
            if !slot.tool_expanded.remove(&key) && slot.tool_expanded.len() < 4096 {
                slot.tool_expanded.insert(key);
            }
            if !matches!(self.panel, Some(Panel::Activity(_))) {
                slot.tool_anchor_limit = self.displayed.as_ref().map(|frame| frame.scroll_limit);
                slot.state.reading = None;
            }
        }
    }

    pub(super) fn restore_result_scroll(&mut self) {
        if let Some(scroll) = self.result_scroll.take() {
            self.slots[self.current].state.scroll = scroll;
        }
        if self.result_selected.is_none() && std::mem::take(&mut self.review_redraw) {
            self.slots[self.current].state.reading = None;
        }
    }

    pub(super) fn leave_results(&mut self) {
        self.restore_result_scroll();
        if std::mem::take(&mut self.result_tail) {
            self.slots[self.current].show_latest();
        }
    }

    pub(super) fn input_editor(&self) -> &crate::frontend::input::Editor {
        if self.navigating() {
            &self.navigation_editor
        } else {
            &self.slots[self.current].state.editor
        }
    }
    pub(super) fn input_editor_mut(&mut self) -> &mut crate::frontend::input::Editor {
        if self.navigating() {
            &mut self.navigation_editor
        } else {
            &mut self.slots[self.current].state.editor
        }
    }
    pub(super) fn available_commands(&self) -> &'static [commands::Command] {
        commands::available()
    }
    pub(super) fn refresh_completion(&mut self) {
        let editor = &self.slots[self.current].state.editor;
        let suppressed = editor.pasting()
            || self.input_closed
            || self.terminating
            || self.menu.is_some()
            || self.focus != Focus::Chat
            || self.selection.as_ref().is_some_and(Selection::is_chat)
            || self
                .completion_closed
                .as_ref()
                .is_some_and(|(bytes, cursor)| *bytes == editor.bytes && *cursor == editor.cursor);
        let token = (!suppressed)
            .then(|| {
                commands::mention::Mention::at_with_skills(
                    editor,
                    &self.slots[self.current].skill_commands,
                )
            })
            .flatten();
        if let Some((directory, prefix, stop)) = &self.mention_listing_job
            && token
                .as_ref()
                .is_none_or(|t| t.directory != *directory || t.prefix != *prefix)
        {
            stop.cancel();
        }
        if suppressed {
            self.completion = None;
            self.mention_listing = None;
            return;
        }
        self.completion_closed = None;
        let mut next = if let Some(token) = token {
            let cached = self
                .mention_listing
                .as_ref()
                .filter(|(dir, prefix, listing)| {
                    dir == &token.directory
                        && (prefix == &token.prefix
                            || listing.as_ref().is_some_and(|l| !l.truncated)
                                && token.prefix.starts_with(prefix))
                });
            if let Some((_, _, listing)) = cached {
                listing.as_ref().and_then(|list| {
                    let choices = token.choices(list);
                    (!choices.is_empty()).then(|| Completion::paths(&token, choices))
                })
            } else {
                if self.mention_listing_job.is_none()
                    && self.jobs.len() + self.pending_context.len() < MAX_JOBS
                {
                    let host = self.files.clone();
                    let stop = Cancel::linked(&self.stop);
                    let directory = token.directory.clone();
                    let prefix = token.prefix.clone();
                    self.mention_listing_job =
                        Some((directory.clone(), prefix.clone(), stop.clone()));
                    let path = self.factory.settings.cwd.join(&directory);
                    self.jobs.spawn_blocking(move || {
                        let listing = host.list_matching(&path, &prefix, &stop).ok();
                        Job::MentionFiles {
                            directory,
                            prefix,
                            listing,
                        }
                    });
                }
                Some(Completion::paths(&token, Vec::new()))
            }
        } else {
            self.mention_listing = None;
            Completion::at_with_skills(
                editor,
                self.available_commands(),
                &self.slots[self.current].skill_commands,
            )
        };
        if let (Some(old), Some(next)) = (&self.completion, &mut next)
            && old.begin == next.begin
            && old.query == next.query
        {
            next.selected = old.selected;
        }
        self.completion = next;
    }
    pub(super) fn complete_command(&mut self) {
        if self.completion.as_ref().is_some_and(Completion::loading) {
            return;
        }
        if let Some(completion) = self.completion.take() {
            let slot = &mut self.slots[self.current];
            let descend = completion.directory_selected();
            if completion.apply(&mut slot.state.editor) {
                slot.state.input_history.edited(&slot.state.editor);
                if descend {
                    self.refresh_completion();
                    return;
                }
            }
        }
        let editor = &self.slots[self.current].state.editor;
        self.completion_closed = Some((editor.bytes.clone(), editor.cursor));
    }
    pub(super) fn title(&self, id: &str) -> String {
        self.known_title(id)
            .unwrap_or_else(|| "New conversation".into())
    }
    pub(super) fn known_title(&self, id: &str) -> Option<String> {
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .and_then(|slot| slot.title.clone())
            .or_else(|| {
                self.summaries
                    .iter()
                    .find(|summary| summary.entry.id == id)
                    .and_then(|summary| summary.title.clone())
            })
    }
    pub(super) fn conversation_root(&self) -> String {
        let mut id = self.slots[self.current].id.clone();
        for _ in 0..128 {
            if let Some(slot) = self.slots.iter().find(|slot| slot.id == id) {
                if matches!(slot.kind, Kind::Main | Kind::Fork) {
                    break;
                }
                if let Some(parent) = &slot.parent {
                    id = parent.clone();
                    continue;
                }
            } else if let Some(summary) =
                self.summaries.iter().find(|summary| summary.entry.id == id)
            {
                if matches!(summary.entry.kind.as_str(), "main" | "fork") {
                    break;
                }
                if let Some(parent) = &summary.entry.parent {
                    id = parent.clone();
                    continue;
                }
            }
            break;
        }
        id
    }
    pub(super) fn agent_views(&self, root: &str) -> impl Iterator<Item = &orchestration::RunView> {
        let live = self.orchestration_views.get(root);
        self.agent_history
            .as_ref()
            .filter(|(parent, _)| parent == root)
            .map(|(_, views)| views)
            .into_iter()
            .flatten()
            .filter(move |view| live.is_none_or(|current| view.id != current.id))
            .chain(live)
    }

    pub(super) fn agent_rows(&self) -> Vec<ConversationAgent> {
        let root = self.conversation_root();
        let delegates = self.live_delegates.borrow();
        let progress: Vec<_> = self.agent_views(&root).collect();
        let mut candidates: Vec<_> = progress
            .iter()
            .rev()
            .flat_map(|view| &view.agents)
            .map(|(id, _)| (id.clone(), Some(root.clone()), String::from("agent")))
            .collect();
        candidates.extend(delegates.agents.iter().map(|(id, agent)| {
            (
                id.clone(),
                Some(agent.parent.clone()),
                String::from("agent"),
            )
        }));
        for summary in &self.summaries {
            let entry = &summary.entry;
            if !candidates.iter().any(|(id, _, _)| id == &entry.id) {
                candidates.push((entry.id.clone(), entry.parent.clone(), entry.kind.clone()));
            }
        }
        for slot in &self.slots {
            if !candidates.iter().any(|(id, _, _)| *id == slot.id) {
                candidates.push((
                    slot.id.clone(),
                    slot.parent.clone(),
                    slot.kind.name().into(),
                ));
            }
        }
        let mut result = Vec::new();
        let mut pending = vec![(root.clone(), String::new(), String::new())];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((parent, tree, branch)) = pending.pop() {
            if !seen.insert(parent.clone()) || seen.len() > 128 {
                continue;
            }
            let children: Vec<_> = candidates
                .iter()
                .filter(|(_, candidate_parent, kind)| {
                    candidate_parent.as_ref() == Some(&parent) && kind != "fork"
                })
                .collect();
            // Depth-first order keeps a child's rows adjacent to its parent.
            let indent = format!(
                "{tree}{}",
                match branch.as_str() {
                    "├─ " => "│  ",
                    "└─ " => "   ",
                    _ => "",
                }
            );
            let mut visible_after = false;
            for (id, _, kind) in children.into_iter().rev() {
                let connector = if kind != "agent" {
                    ""
                } else if visible_after {
                    "├─ "
                } else {
                    "└─ "
                };
                visible_after |= kind == "agent";
                pending.push((id.clone(), indent.clone(), connector.into()));
            }
            if parent == root {
                continue;
            }
            let Some((id, Some(owner), kind)) = candidates.iter().find(|(id, _, _)| *id == parent)
            else {
                continue;
            };
            if kind != "agent" {
                continue;
            }
            let row = |line| ConversationAgent {
                parent: owner.clone(),
                branch: format!("{tree}{branch}"),
                line,
            };
            if let Some(agent) = delegates
                .agents
                .get(id)
                .map(|delegate| &delegate.line)
                .or_else(|| {
                    progress
                        .iter()
                        .rev()
                        .flat_map(|view| &view.agents)
                        .find(|(agent, _)| agent == id)
                        .map(|(_, agent)| agent)
                })
            {
                result.push(row(agent.clone()));
                continue;
            }
            let live = self.slots.iter().find(|slot| slot.id == *id);
            let snapshot = live.and_then(|slot| slot.state.snapshot.as_ref());
            let archived = self
                .summaries
                .iter()
                .find(|summary| summary.entry.id == *id);
            let state = snapshot
                .map(|view| {
                    if view.worker_busy {
                        "Running"
                    } else {
                        match view.state.as_str() {
                            "completed" => "Completed",
                            "cancelled" => "Stopped",
                            "paused" => "Paused",
                            _ => "Ready",
                        }
                    }
                })
                .unwrap_or_else(|| archived.map_or("Unavailable", |s| s.status.label()));
            let state = archived
                .and_then(|s| s.agent.as_ref())
                .and_then(|a| a.outcome)
                .filter(|_| snapshot.is_none() && state == "Completed")
                .map_or(state, |outcome| match outcome {
                    crate::provider::structured::ChildStatus::Completed => "Completed",
                    crate::provider::structured::ChildStatus::ChangesRequested => "Needs changes",
                    crate::provider::structured::ChildStatus::Blocked => "Blocked",
                });
            let generation = snapshot
                .map(|view| {
                    format!(
                        "{}{}",
                        view.generation.model,
                        view.generation
                            .effort
                            .as_ref()
                            .map_or(String::new(), |effort| format!(" · {effort}"))
                    )
                })
                .unwrap_or_default();
            let elapsed = live.and_then(|slot| slot.state.progress.elapsed());
            let pause_diagnostic = match snapshot {
                Some(view) if state == "Paused" && !view.diagnostic.is_empty() => {
                    Some(view.diagnostic.as_str())
                }
                Some(_) => None,
                None => archived.and_then(|summary| summary.pause_diagnostic.as_deref()),
            };
            let activity = if let Some(code) = pause_diagnostic {
                crate::frontend::diagnostics::error(code)
            } else if let Some(preview) = archived.and_then(|summary| summary.preview.as_ref()) {
                preview.clone()
            } else {
                match (generation.is_empty(), elapsed) {
                    (false, Some(elapsed)) => format!("{generation} · {elapsed}"),
                    (false, None) => generation,
                    (true, Some(elapsed)) => elapsed,
                    (true, None) => String::new(),
                }
            };
            result.push(row(carbon::AgentLine {
                id: id.clone(),
                title: self.title(id),
                state: state.into(),
                role: if archived
                    .and_then(|s| s.agent.as_ref())
                    .is_some_and(|a| a.writable)
                {
                    carbon::AgentRole::Implementation
                } else {
                    carbon::AgentRole::Research
                },
                model: archived
                    .and_then(|s| s.agent.as_ref())
                    .map_or_else(String::new, |a| a.model.clone()),
                activity,
                step: None,
                tool_calls: None,
            }));
        }
        result
    }
    pub(super) fn show_models(&mut self, models: Vec<(String, String, Vec<String>)>, effort: bool) {
        if !effort
            && let Some(requested) = self.requested_model.take()
            && let Some(selected) = models.iter().position(|model| model.0 == requested)
        {
            self.choose_model_effort(models, selected);
            return;
        }
        let current = self.slots[self.current]
            .state
            .snapshot
            .as_ref()
            .map(|view| view.generation.clone());
        let selected = current
            .as_ref()
            .and_then(|current| models.iter().position(|m| m.0 == current.model))
            .unwrap_or(0);
        if effort {
            let model = current
                .as_ref()
                .map(|g| g.model.clone())
                .unwrap_or_else(|| self.factory.settings.model.clone());
            let supported = models
                .iter()
                .find(|m| m.0 == model)
                .map(|m| m.2.clone())
                .unwrap_or_default();
            let mut values = vec![None];
            values.extend(supported.into_iter().map(Some));
            let selected = current
                .as_ref()
                .and_then(|g| values.iter().position(|effort| *effort == g.effort))
                .unwrap_or(0);
            self.menu = Some(Menu::Efforts {
                model,
                values,
                selected,
                previous: None,
            });
        } else {
            self.menu = Some(Menu::Models { models, selected });
        }
    }
    pub(super) fn choose_model_effort(
        &mut self,
        models: Vec<(String, String, Vec<String>)>,
        selected: usize,
    ) {
        let Some(model) = models.get(selected) else {
            return;
        };
        let name = model.0.clone();
        let mut values = vec![None];
        values.extend(model.2.iter().cloned().map(Some));
        let current = self.slots[self.current]
            .state
            .snapshot
            .as_ref()
            .and_then(|view| view.generation.effort.as_ref());
        let effort = current
            .and_then(|value| values.iter().position(|e| e.as_ref() == Some(value)))
            .unwrap_or(0);
        self.menu = Some(Menu::Efforts {
            model: name,
            values,
            selected: effort,
            previous: Some((models, selected)),
        });
    }
    pub(super) fn apply_generation(
        &mut self,
        model: String,
        effort: Option<String>,
        io: &Io,
    ) -> bool {
        if self.slots[self.current].kind == Kind::Console {
            self.report(io, "The command panel has no model");
            return false;
        }
        let owner = &self.slots[self.current].id;
        if self
            .orchestration_stop
            .as_ref()
            .is_some_and(|(id, _)| id == owner)
            || self.orchestration_summary.contains_key(owner)
            || self.slots[self.current].pending_summary.is_some()
        {
            self.report(
                io,
                "Wait for orchestration to finish before switching models",
            );
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        let slot = &mut self.slots[self.current];
        let selection = GenerationSelection { model, effort };
        let command = slot.state.command(
            &slot.handle,
            CommandPayload::SetGeneration(selection.clone()),
        );
        let handle = slot.handle.clone();
        let owner = slot.id.clone();
        let factory = self.factory.clone();
        let selected_at = SystemTime::now();
        self.jobs.spawn(async move {
            let result = match handle.post(command).await {
                Ok(PostStatus::Accepted) => tokio::task::spawn_blocking(move || {
                    factory.save_generation(selected_at, &selection)
                })
                .await
                .unwrap_or(Err("model_preferences_write")),
                Ok(PostStatus::Duplicate) => Ok(()),
                Ok(PostStatus::Closed) => Err("runtime_closed"),
                Ok(PostStatus::QueueFull) => Err("command_queue_full"),
                Ok(PostStatus::CommandIdLimit) => Err("submit_id_limit"),
                Ok(_) => Err("generation_selection"),
                Err(code) => Err(code),
            };
            Job::GenerationSaved { owner, result }
        });
        slot.state
            .report(io, "Applying model and reasoning effort…");
        true
    }
    pub(super) fn apply_autocompact(&mut self, percent: u8, io: &Io) -> bool {
        if !self.can_job(io) {
            return false;
        }
        let slot = &mut self.slots[self.current];
        let command = slot
            .state
            .command(&slot.handle, CommandPayload::SetAutocompact(percent));
        let handle = slot.handle.clone();
        let owner = slot.id.clone();
        let factory = self.factory.clone();
        let selected_at = SystemTime::now();
        self.jobs.spawn(async move {
            let result = match handle.post(command).await {
                Ok(PostStatus::Accepted) => tokio::task::spawn_blocking(move || {
                    factory.save_autocompact(selected_at, percent)
                })
                .await
                .unwrap_or(Err("autocompact_preferences_write")),
                Ok(PostStatus::Duplicate) => Ok(()),
                Ok(PostStatus::Closed) => Err("runtime_closed"),
                Ok(PostStatus::QueueFull) => Err("command_queue_full"),
                Ok(PostStatus::CommandIdLimit) => Err("submit_id_limit"),
                Ok(_) => Err("autocompact_selection"),
                Err(code) => Err(code),
            };
            Job::AutocompactSaved { owner, result }
        });
        slot.state.report(io, "Applying autocompact percentage…");
        true
    }
    pub(super) fn terminal_dimensions(&self) -> Option<(u16, u16)> {
        // A key can arrive after the Files footer has been written but before
        // the writer acknowledges the whole frame. Use the newest geometry.
        self.last_frame
            .as_ref()
            .or(self.displayed.as_ref())
            .and_then(|frame| frame.inspector_bounds())
            .filter(|(_, _, width, height)| *width > 0 && *height > 0)
            .map(|(_, _, width, height)| (height.clamp(1, 128) as u16, width.clamp(1, 256) as u16))
    }
    pub(super) fn terminal_focused(&self) -> bool {
        self.editor.is_some()
            || self.menu.is_none()
                && self.focus == Focus::Inspector
                && matches!(self.panel, Some(Panel::Terminal))
                && self.terminal_dimensions().is_some()
    }
    fn input_terminal(&self) -> Option<&crate::frontend::terminal_pane::TerminalPane> {
        self.editor
            .as_ref()
            .map(|editor| &editor.pane)
            .or(self.terminal.as_ref())
    }
    pub(super) fn open_terminal(&mut self, io: &Io) {
        let Some((rows, columns)) = self.terminal_dimensions() else {
            self.focus = Focus::Chat;
            self.report(io, "Widen the window to open the terminal panel");
            return;
        };
        let directory = match &self.panel {
            Some(Panel::Files(files)) => files
                .document
                .as_ref()
                .and_then(|d| d.path.parent().map(Path::to_path_buf))
                .or_else(|| files.listing.as_ref().map(|listing| listing.path.clone())),
            _ => None,
        }
        .unwrap_or_else(|| self.factory.settings.cwd.clone());
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.snapshot().exited)
        {
            if let Some(mut terminal) = self.terminal.take() {
                terminal.shutdown();
            }
            self.terminal_grid = None;
            self.terminal_size = None;
        }
        if self.terminal.is_none() {
            match crate::frontend::terminal_pane::TerminalPane::spawn(&directory, rows, columns) {
                Ok(terminal) => self.terminal = Some(terminal),
                Err(error) => {
                    self.report(io, &format!("Could not open terminal: {error}"));
                    return;
                }
            }
        } else if let Some(terminal) = &self.terminal {
            let snapshot = terminal.snapshot();
            if snapshot.shell_ready
                && snapshot.cwd != directory
                && let Err(error) = terminal.change_directory(&directory)
            {
                self.report(
                    io,
                    &format!("Terminal is still in {}: {error}", snapshot.cwd.display()),
                );
            }
        }
        if let Some(Panel::Files(files)) = self.panel.take() {
            self.terminal_back = Some(files);
        }
        self.panel = Some(Panel::Terminal);
        self.focus = Focus::Inspector;
        self.selection = None;
        self.tool_press = None;
        self.completion = None;
    }
    pub(super) fn hide_terminal(&mut self, io: &Io) {
        let directory = self.terminal.as_ref().map(|terminal| {
            terminal.cancel_directory_change();
            terminal.snapshot().cwd.clone()
        });
        self.panel = self.terminal_back.take().map(Panel::Files);
        self.focus = Focus::Inspector;
        self.selection = None;
        self.tool_press = None;
        if let Some(directory) = directory {
            self.open_file(directory, io);
        } else {
            self.focus = Focus::Chat;
        }
    }
    pub(super) fn terminal_bytes(&mut self, bytes: &[u8], io: &Io) {
        if !bytes.is_empty() {
            self.selection = None;
            self.tool_press = None;
        }
        if self.editor.is_none() && bytes == [3] {
            self.terminal_pending_input.clear();
        }
        if !self.terminal_pending_input.is_empty() {
            if self.terminal_pending_input.len() + bytes.len() <= 64 * 1024 {
                self.terminal_pending_input.extend_from_slice(bytes);
            } else {
                self.report(io, "Terminal busy. Wait for pending input to finish");
            }
            return;
        }
        let Some(terminal) = self.input_terminal() else {
            return;
        };
        for (index, part) in bytes.chunks(4096).enumerate() {
            match terminal.input(part) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.terminal_pending_input
                        .extend_from_slice(&bytes[index * 4096..]);
                    break;
                }
                Err(error) => {
                    self.report(io, &format!("Terminal input unavailable: {error}"));
                    break;
                }
            }
        }
    }
    pub(super) fn flush_terminal_input(&mut self, io: &Io) {
        if !self.terminal_pending_input.is_empty() {
            let pending = std::mem::take(&mut self.terminal_pending_input);
            self.terminal_bytes(&pending, io);
        }
    }
    pub(super) fn terminal_event(
        &mut self,
        event: TerminalEvent,
        io: &Io,
        deferred: &mut std::collections::VecDeque<u8>,
    ) {
        match event {
            TerminalEvent::Bytes(bytes) => {
                if self.terminal_focused() {
                    self.terminal_bytes(&bytes, io);
                } else {
                    deferred.extend(bytes);
                }
            }
            TerminalEvent::Escape => {
                if self.editor.is_none()
                    && self.terminal_focused()
                    && self.terminal_pending_input.is_empty()
                    && self
                        .terminal
                        .as_ref()
                        .is_some_and(crate::frontend::terminal_pane::TerminalPane::shell_ready)
                {
                    self.hide_terminal(io);
                } else if self.terminal_focused() {
                    self.terminal_bytes(b"\x1b", io);
                } else {
                    deferred.push_back(27);
                }
            }
            TerminalEvent::Mouse {
                event,
                button,
                release,
            } => {
                let bounds = self
                    .displayed
                    .as_ref()
                    .and_then(|frame| frame.inspector_bounds());
                let in_pane = bounds.is_some_and(|(x, y, width, height)| {
                    event.column >= x
                        && event.column < x + width
                        && event.row >= y
                        && event.row < y + height
                });
                let screen = self.input_terminal().map(|terminal| terminal.snapshot());
                if in_pane
                    && button & 4 == 0
                    && screen.as_ref().is_some_and(|s| {
                        s.mouse_protocol != crate::frontend::terminal_pane::MouseProtocolMode::None
                    })
                {
                    let (x, y, _, _) = bounds.expect("pane hit");
                    let screen = screen.expect("terminal exists");
                    let column = event.column - x + 1;
                    let row = event.row - y + 1;
                    let bytes = terminal_mouse(
                        button,
                        release,
                        column,
                        row,
                        screen.mouse_protocol,
                        screen.mouse_encoding,
                    );
                    if let Some(bytes) = bytes {
                        self.terminal_bytes(&bytes, io);
                    }
                } else if self.editor.is_none() {
                    self.mouse(event, io);
                }
            }
        }
    }
    pub(super) fn terminal_chunk(
        &mut self,
        bytes: &[u8],
        io: &Io,
        deferred: &mut std::collections::VecDeque<u8>,
    ) {
        let paste = self
            .input_terminal()
            .is_some_and(|terminal| terminal.snapshot().bracketed_paste);
        let events = self.terminal_decoder.feed(bytes, paste);
        for event in events {
            self.terminal_event(event, io, deferred);
        }
        if !self.terminal_focused() {
            deferred.extend(self.terminal_decoder.take_pending());
            self.terminal_escape_at = None;
        }
    }
    pub(super) fn output_lines(&self, text: &str) -> Vec<String> {
        let width = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.inspector_bounds())
            .map_or(72, |(_, _, width, _)| width)
            .max(1);
        let mut result = Vec::new();
        for line in text.lines() {
            let mut part = String::new();
            let mut used = 0;
            for glyph in crate::frontend::cells::graphemes(line) {
                let cells = crate::frontend::cells::width(glyph);
                if used + cells > width && !part.is_empty() {
                    result.push(std::mem::take(&mut part));
                    used = 0;
                }
                part.push_str(glyph);
                used += cells;
            }
            result.push(part);
        }
        result
    }
}

fn result_entry(
    text: String,
    action: Option<String>,
    output: Option<Arc<carbon::OutputView>>,
) -> Entry {
    Entry {
        role: "tool_success".into(),
        text,
        action,
        output,
        agent: None,
        edit: None,
        status: None,
    }
}

fn terminal_mouse(
    button: u16,
    release: bool,
    column: usize,
    row: usize,
    mode: crate::frontend::terminal_pane::MouseProtocolMode,
    encoding: crate::frontend::terminal_pane::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use crate::frontend::terminal_pane::{
        MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode,
    };
    if button > 223 || column == 0 || row == 0 || column > 65535 || row > 65535 {
        return None;
    }
    let motion = button & 32 != 0;
    if mode == Mode::None
        || mode == Mode::Press && (release || motion)
        || mode == Mode::PressRelease && motion
        || mode == Mode::ButtonMotion && motion && button & 3 == 3
    {
        return None;
    }
    if encoding == Encoding::Sgr {
        return Some(
            format!(
                "\x1b[<{button};{column};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    let code = (if release { 3 } else { button }).checked_add(32)?;
    if encoding == Encoding::Utf8 {
        let mut encoded = String::from("\x1b[M");
        for value in [
            usize::from(code),
            column.checked_add(32)?,
            row.checked_add(32)?,
        ] {
            encoded.push(char::from_u32(u32::try_from(value).ok()?)?);
        }
        Some(encoded.into_bytes())
    } else {
        Some(vec![
            27,
            b'[',
            b'M',
            u8::try_from(code).ok()?,
            u8::try_from(column.checked_add(32)?).ok()?,
            u8::try_from(row.checked_add(32)?).ok()?,
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::terminal_mouse;
    use crate::frontend::terminal_pane::{
        MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode,
    };
    #[test]
    fn terminal_mouse_checks_protocol_and_encoding_bounds() {
        for button in [224, 255, 65535] {
            assert!(
                terminal_mouse(button, false, 1, 1, Mode::AnyMotion, Encoding::Default).is_none()
            );
        }
        assert!(terminal_mouse(32, false, 1, 1, Mode::PressRelease, Encoding::Sgr).is_none());
        assert!(terminal_mouse(0, true, 1, 1, Mode::Press, Encoding::Sgr).is_none());
        assert!(terminal_mouse(35, false, 1, 1, Mode::ButtonMotion, Encoding::Sgr).is_none());
        assert_eq!(
            terminal_mouse(0, false, 1, 1, Mode::Press, Encoding::Default),
            Some(vec![27, b'[', b'M', 32, 33, 33])
        );
        assert!(terminal_mouse(0, false, 224, 1, Mode::Press, Encoding::Default).is_none());
        assert!(
            std::str::from_utf8(
                &terminal_mouse(0, false, 256, 128, Mode::Press, Encoding::Utf8).unwrap()
            )
            .is_ok()
        );
        assert_eq!(
            terminal_mouse(52, false, 256, 128, Mode::ButtonMotion, Encoding::Sgr).unwrap(),
            b"\x1b[<52;256;128M"
        );
    }
}
