//! Workspace interactions. Typed runtime state remains the source of truth.
use super::*;
use crate::frontend::terminal_input::TerminalEvent;
use std::sync::Arc;

pub(super) struct CandidateView {
    run: String,
    heading: String,
    title: String,
    rows: Arc<Vec<crate::frontend::diff::Row>>,
    checks: Vec<Arc<carbon::OutputView>>,
}

impl CandidateView {
    pub(super) fn new(run: String, candidate: orchestration::Candidate) -> Self {
        use crate::frontend::diff::{Kind, patch_rows};
        let rows = patch_rows(&candidate.diff);
        let added = rows.iter().filter(|row| row.kind == Kind::Add).count();
        let removed = rows.iter().filter(|row| row.kind == Kind::Remove).count();
        let changed = match candidate.paths.as_slice() {
            [path] => path.clone(),
            paths => format!("Files: {}", paths.len()),
        };
        let heading = format!("{changed} · +{added} −{removed}");
        let title = format!("{heading} · project snapshot → ansa-result");
        Self {
            run,
            heading,
            title,
            rows: Arc::new(rows),
            checks: candidate.checks.into_iter().map(Arc::new).collect(),
        }
    }

    pub(super) fn panel(&self) -> Panel {
        Panel::EditDiff {
            title: format!("{} · Verified changes", self.title),
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
        let slot = &self.slots[self.current];
        let mut entries = slot.projected()?;
        for review in &slot.candidate_reviews {
            let plan_key = format!("plan:{}", review.run);
            if let Some(index) = entries
                .iter()
                .position(|entry| entry.action.as_ref() == Some(&plan_key))
            {
                let diff = Some(format!("candidate:{}", review.run));
                let mut review_entries = vec![result_entry(review.heading.clone(), diff, None)];
                review_entries.extend(review.checks.iter().enumerate().map(|(index, view)| {
                    result_entry(
                        format!("{} · passed", view.title),
                        Some(format!("output:verification:{}:{index}", review.run)),
                        Some(view.clone()),
                    )
                }));
                entries.splice(index + 1..index + 1, review_entries);
            }
        }
        if !slot.state.follow_tail {
            return Some(entries);
        }
        if self.collecting_orchestration() {
            let view = self.orchestration_views.get(&slot.id);
            entries.push(Entry {
                text: view.map_or_else(
                    || "Starting agents".into(),
                    |view| {
                        if view.reviewing {
                            "Verifying agent changes".into()
                        } else {
                            format!(
                                "{} · {}/{} agents done",
                                view.title,
                                view.done,
                                view.plan.steps().len()
                            )
                        }
                    },
                ),
                role: "activity".into(),
                action: view.map(|_| "plan:active".into()),
                status: None,
                edit: None,
                output: None,
            });
        } else if self.orchestration_summary.contains_key(&slot.id) {
            if entries.last().is_some_and(|entry| entry.role == "activity") {
                entries.pop();
            }
            entries.push(Entry {
                text: "Coordinator is writing the summary".into(),
                role: "activity".into(),
                action: None,
                status: None,
                edit: None,
                output: None,
            });
        }
        Some(entries)
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
    ) {
        use crate::orchestration::plan::Access;
        let mut lines = Vec::new();
        if let Some(folder) = plan.read_folder() {
            lines.push(format!("Read folder · {folder}"));
            lines.push(String::new());
        }
        lines.extend([plan.summary().to_owned(), String::new()]);
        if let Some(prompt) = prompt {
            lines.push("Original request".into());
            lines.extend(prompt.lines().map(str::to_owned));
            lines.push(String::new());
        }
        if let Err(code) = plan.execution_ready() {
            lines.push(if code == "orchestration_manual_checks_required" {
                "A manual check is required before starting. Select Refine.".into()
            } else {
                "Select Refine to add a check for the changes.".into()
            });
            lines.push(String::new());
        }
        for check in plan.command_checks() {
            lines.push(format!(
                "Command runs in the project copy with your permissions: {}",
                serde_json::json!(check.argv)
            ));
        }
        for (index, step) in plan.steps().iter().enumerate() {
            let access = match step.access {
                Access::Read => "Read",
                Access::Write => "Edit workspace copy",
            };
            lines.push(format!("{}. {} · {access}", index + 1, step.id));
            lines.extend(step.task.lines().map(str::to_owned));
            lines.extend(step.paths.iter().cloned());
            if !step.depends_on.is_empty() {
                lines.push(format!("After steps {}", step.depends_on.join(", ")));
            }
            lines.extend(step.acceptance.lines().map(str::to_owned));
            lines.push(String::new());
        }
        if plan.steps().iter().any(|step| step.access == Access::Write) {
            lines.push("Changes go through a diff check and independent review. Up to two fixes in the listed paths.".into());
        }
        for check in plan.checks() {
            lines.push(format!("Check file {}", check.path));
            lines.extend(check.contains.lines().map(str::to_owned));
        }
        for check in plan.manual_checks() {
            lines.push(if plan.execution_ready().is_ok() {
                "Follow-up recommendation".into()
            } else {
                "Manual check".into()
            });
            lines.extend(check.lines().map(str::to_owned));
        }
        self.file_token = self.file_token.wrapping_add(1);
        self.panel = Some(Panel::Orchestration { lines, offset: 0 });
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
                if let Some(page) = &slot.timeline {
                    slot.timeline_query.before = page.end;
                    slot.timeline_query.forward = false;
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
        if let Some(operation) = key.strip_prefix("image:") {
            let source = slot.timeline.as_ref().and_then(|page| {
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
        } else if let Some(run) = key.strip_prefix("plan:") {
            if run == "active" {
                if let Some(view) = self.orchestration_views.get(&slot.id) {
                    self.show_orchestration_plan(&view.plan.clone(), None);
                }
            } else if self.can_job(io) {
                let parent = self.slots[self.current].id.clone();
                let run = run.to_owned();
                self.file_token = self.file_token.wrapping_add(1);
                let token = self.file_token;
                self.panel = Some(Panel::Orchestration {
                    lines: vec!["Loading plan".into()],
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
            slot.tool_anchor_limit = self.displayed.as_ref().map(|frame| frame.scroll_limit);
            slot.state.reading = None;
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
        commands::available(
            self.factory
                .terminal_owner(&self.slots[self.current].id)
                .is_some(),
        )
    }
    pub(super) fn refresh_completion(&mut self) {
        if self.menu.is_some()
            || self.focus != Focus::Chat
            || self.selection.as_ref().is_some_and(Selection::is_chat)
        {
            self.completion = None;
            return;
        }
        let editor = &self.slots[self.current].state.editor;
        if self
            .completion_closed
            .as_ref()
            .is_some_and(|(bytes, cursor)| *bytes == editor.bytes && *cursor == editor.cursor)
        {
            self.completion = None;
            return;
        }
        self.completion_closed = None;
        let mut next = Completion::at_with_commands(editor, self.available_commands());
        if let (Some(old), Some(next)) = (&self.completion, &mut next)
            && old.begin == next.begin
            && old.query == next.query
        {
            next.selected = old.selected;
        }
        self.completion = next;
    }
    pub(super) fn complete_command(&mut self) {
        if let Some(completion) = self.completion.take() {
            let slot = &mut self.slots[self.current];
            if completion.apply(&mut slot.state.editor) {
                slot.state.input_history.edited(&slot.state.editor);
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
    pub(super) fn agent_rows(&self) -> Vec<(String, carbon::AgentLine)> {
        let root = self.conversation_root();
        let mut candidates: Vec<(String, Option<String>, String)> = self
            .summaries
            .iter()
            .map(|s| {
                (
                    s.entry.id.clone(),
                    s.entry.parent.clone(),
                    s.entry.kind.clone(),
                )
            })
            .collect();
        let progress = self.orchestration_views.get(&root);
        if let Some(view) = progress {
            for (id, _) in &view.agents {
                if !candidates.iter().any(|(candidate, _, _)| candidate == id) {
                    candidates.push((id.clone(), Some(root.clone()), "agent".into()));
                }
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
            let Some((id, _, kind)) = candidates.iter().find(|(id, _, _)| *id == parent) else {
                continue;
            };
            if kind != "agent" {
                continue;
            }
            if let Some(mut agent) = progress
                .and_then(|view| view.agents.iter().find(|(agent, _)| agent == id))
                .map(|(_, agent)| agent.clone())
            {
                agent.title = format!("{tree}{branch}{}", agent.title);
                agent.detail = format!("{indent}{}", agent.detail);
                result.push((id.clone(), agent));
                continue;
            }
            let live = self.slots.iter().find(|slot| slot.id == *id);
            let state = live
                .and_then(|slot| slot.state.snapshot.as_ref())
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
                .unwrap_or_else(|| {
                    self.summaries
                        .iter()
                        .find(|s| s.entry.id == *id)
                        .map_or("Unavailable", |s| s.status.label())
                });
            let generation = live
                .and_then(|slot| slot.state.snapshot.as_ref())
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
            let detail = if let Some(preview) = self
                .summaries
                .iter()
                .find(|summary| summary.entry.id == *id)
                .and_then(|summary| summary.preview.as_ref())
            {
                format!("{indent}{preview}")
            } else {
                match (generation.is_empty(), elapsed) {
                    (false, Some(elapsed)) => format!("{indent}{generation} · {elapsed}"),
                    (false, None) => format!("{indent}{generation}"),
                    (true, Some(elapsed)) => format!("{indent}{elapsed}"),
                    (true, None) => String::new(),
                }
            };
            result.push((
                id.clone(),
                carbon::AgentLine {
                    title: format!("{tree}{branch}{}", self.title(id)),
                    state: state.into(),
                    detail,
                },
            ));
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
    fn terminal_dimensions(&self) -> Option<(u16, u16)> {
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
        self.menu.is_none()
            && self.focus == Focus::Inspector
            && matches!(self.panel, Some(Panel::Terminal))
            && self.terminal_dimensions().is_some()
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
        if bytes == [3] {
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
        let Some(terminal) = &self.terminal else {
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
                if self.terminal_focused()
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
                let screen = self.terminal.as_ref().map(|terminal| terminal.snapshot());
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
                } else {
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
            .terminal
            .as_ref()
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
