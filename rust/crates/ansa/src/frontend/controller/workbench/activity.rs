//! Navigation and paging for the session's operational transcript.
use super::*;
use std::sync::Arc;

const PAGE_SIZE: usize = 64;

pub(super) fn latest() -> GetTranscript {
    GetTranscript {
        before: 0,
        limit: PAGE_SIZE,
        forward: false,
    }
}

pub(super) fn tabs() -> Vec<String> {
    vec!["Agents".into(), "Activity".into(), "Shell".into()]
}

fn agent_entry(
    role: &str,
    text: String,
    agent: Option<&carbon::AgentLine>,
    openable: bool,
) -> Entry {
    Entry {
        role: role.into(),
        text,
        action: agent
            .filter(|_| openable)
            .map(|line| format!("agent:{}", line.id)),
        status: None,
        edit: None,
        output: None,
        agent: agent.map(|line| Arc::new(line.clone())),
    }
}

fn delegated_entries(mut rows: Vec<ux::ConversationAgent>) -> Vec<Entry> {
    // A delegate call starts a new session, even when the lead reuses a model.
    // Keep each assignment addressable inside its parent's role/model group.
    rows.sort_by_key(|row| row.line.state != "Running");
    let mut groups: Vec<Vec<ux::ConversationAgent>> = Vec::new();
    for row in rows {
        let group = groups.iter_mut().find(|group| {
            let first = &group[0];
            !row.line.model.is_empty()
                && first.parent == row.parent
                && first.line.role == row.line.role
                && first.line.model == row.line.model
        });
        if let Some(group) = group {
            group.push(row);
        } else {
            groups.push(vec![row]);
        }
    }
    let mut entries = Vec::new();
    for mut group in groups {
        if group.len() > 1 {
            entries.push(agent_entry(
                "agent_role",
                format!("{} runs", group.len()),
                Some(&group[0].line),
                false,
            ));
            group.sort_by(|a, b| a.line.id.cmp(&b.line.id));
            let mut numbered: Vec<_> = group.into_iter().enumerate().rev().collect();
            numbered.sort_by_key(|(_, row)| row.line.state != "Running");
            for (index, row) in numbered {
                entries.push(agent_entry(
                    "tool_agent_run",
                    format!("{:02}", index + 1),
                    Some(&row.line),
                    true,
                ));
            }
        } else if let Some(row) = group.pop() {
            entries.push(agent_entry(
                "agent_direct",
                String::new(),
                Some(&row.into_tree_line()),
                true,
            ));
        }
    }
    entries
}

#[derive(Clone, Default)]
pub(super) struct Navigation {
    scroll: usize,
    initial_agents_position: bool,
    selected: Option<String>,
    pub(super) agents_only: bool,
}

impl Slot {
    pub(super) fn refresh_activity(&mut self) {
        if self.closed || self.closing || !self.ready {
            return;
        }
        if let Some(snapshot) = &self.state.snapshot
            && self.activity_timeline.as_ref().is_none_or(|page| {
                page.query != self.activity_query
                    || (self.activity_query.before == 0 && page.total < snapshot.timeline_size)
            })
        {
            self.state.post_read(
                &self.handle,
                CommandPayload::GetTimeline(self.activity_query.clone()),
                &mut self.replies,
            );
        }
    }
}

impl Workbench {
    pub(super) fn open_activity(&mut self) {
        if matches!(&self.panel, Some(Panel::Activity(pane)) if !pane.agents_only) {
            self.focus = Focus::Inspector;
            return;
        }
        self.file_token = self.file_token.wrapping_add(1);
        self.completion = None;
        self.selection = None;
        self.tool_press = None;
        self.result_selected = None;
        self.wide_diff = false;
        self.output_back = None;
        self.activity_back = None;
        let slot = &mut self.slots[self.current];
        slot.activity_query = latest();
        if slot
            .activity_timeline
            .as_ref()
            .is_some_and(|page| page.query != slot.activity_query)
        {
            slot.activity_timeline = None;
        }
        self.panel = Some(Panel::Activity(Navigation::default()));
        self.focus = Focus::Inspector;
        self.refresh_agent_summaries();
    }

    pub(super) fn open_agents(&mut self) {
        self.file_token = self.file_token.wrapping_add(1);
        self.selection = None;
        self.tool_press = None;
        self.wide_diff = false;
        self.activity_back = None;
        self.panel = Some(Panel::Activity(Navigation {
            agents_only: true,
            initial_agents_position: true,
            ..Navigation::default()
        }));
        self.focus = Focus::Chat;
        self.refresh_agent_summaries();
    }

    fn activity_entries(&self, navigation: &Navigation) -> Vec<Entry> {
        if !navigation.agents_only {
            return self.terminal_activity();
        }
        let rows = self.agent_rows();
        let mut entries = Vec::new();
        let mut grouped = std::collections::BTreeSet::new();
        for (view, step) in self
            .agent_views(&self.conversation_root())
            .flat_map(|view| view.plan.steps().iter().map(move |step| (view, step)))
        {
            let agents: Vec<_> = view
                .agents
                .iter()
                .rev()
                .map(|(_, agent)| agent)
                .filter(|agent| agent.step.as_deref() == Some(step.id.as_str()))
                .collect();
            if agents.is_empty() {
                continue;
            }
            grouped.extend(agents.iter().map(|agent| agent.id.as_str()));
            if agents.len() == 1 {
                entries.push(agent_entry(
                    "agent_card",
                    String::new(),
                    Some(agents[0]),
                    true,
                ));
                continue;
            }
            let title = step.task.lines().next().unwrap_or_default();
            entries.push(agent_entry(
                "agent_step",
                format!("{} / {title}", step.id),
                None,
                false,
            ));
            let mut roles: Vec<Vec<&carbon::AgentLine>> = Vec::new();
            for agent in agents {
                if let Some(role) = roles
                    .iter_mut()
                    .find(|role| role[0].role == agent.role && role[0].model == agent.model)
                {
                    role.push(agent);
                } else {
                    roles.push(vec![agent]);
                }
            }
            for role in roles {
                let count = role.len();
                if count == 1 {
                    entries.push(agent_entry(
                        "agent_role",
                        String::new(),
                        Some(role[0]),
                        true,
                    ));
                    continue;
                }
                entries.push(agent_entry(
                    "agent_role",
                    format!("{count} attempts"),
                    Some(role[0]),
                    false,
                ));
                for (index, agent) in role.into_iter().enumerate() {
                    let branch = if index + 1 == count {
                        "└─"
                    } else {
                        "├─"
                    };
                    entries.push(agent_entry(
                        "tool_agent_attempt",
                        format!("{branch} {:02}", index + 1),
                        Some(agent),
                        true,
                    ));
                }
            }
        }
        let root = self.conversation_root();
        let planned: std::collections::BTreeSet<_> = self
            .agent_views(&root)
            .flat_map(|view| view.agents.iter().map(|(id, _)| id.as_str()))
            .collect();
        let mut direct = Vec::new();
        for row in rows
            .into_iter()
            .rev()
            .filter(|r| !grouped.contains(r.line.id.as_str()))
        {
            if planned.contains(row.line.id.as_str()) {
                entries.push(agent_entry(
                    "agent_card",
                    String::new(),
                    Some(&row.into_tree_line()),
                    true,
                ));
            } else {
                direct.push(row);
            }
        }
        let mut runs = delegated_entries(direct);
        runs.extend(entries);
        runs
    }

    pub(super) fn return_to_activity(&mut self) {
        if let Some(navigation) = self.activity_back.take() {
            self.file_token = self.file_token.wrapping_add(1);
            self.focus = if navigation.agents_only {
                Focus::Chat
            } else {
                Focus::Inspector
            };
            self.panel = Some(Panel::Activity(navigation));
            self.selection = None;
            self.tool_press = None;
            self.wide_diff = false;
        } else {
            self.open_activity();
        }
    }

    pub(super) fn activity_shell(&mut self) {
        if matches!(&self.panel, Some(Panel::NativeTerminal { .. })) {
            return;
        }
        if let Some(Panel::Activity(navigation)) = self.panel.take() {
            self.activity_back = Some(navigation);
            self.inspect_terminal("");
        }
    }

    pub(super) fn activity_inspector(&self, navigation: &Navigation) -> Inspector {
        let slot = &self.slots[self.current];
        let page = slot.activity_timeline.as_ref();
        let entries = self.activity_entries(navigation);
        if navigation.agents_only {
            let has_delegates = entries
                .iter()
                .any(|e| matches!(e.role.as_str(), "agent_direct" | "tool_agent_run"));
            let scroll = if navigation.initial_agents_position && has_delegates {
                usize::MAX
            } else {
                navigation.scroll
            };
            let preview = navigation.selected.as_ref().and_then(|selected| {
                let entry = entries
                    .iter()
                    .find(|entry| entry.action.as_ref() == Some(selected))?;
                let agent = entry
                    .agent
                    .as_ref()
                    .filter(|agent| !agent.activity.is_empty())?;
                let title = entry
                    .text
                    .rsplit_once(' ')
                    .filter(|_| entry.role == "tool_agent_attempt")
                    .map_or_else(
                        || agent.state.clone(),
                        |(_, number)| format!("Attempt {number} · {}", agent.state),
                    );
                Some((title, agent.activity.clone()))
            });
            return Inspector {
                title: "Agents".into(),
                tabs: tabs(),
                lines: vec!["No agents in this conversation".into()],
                footer: "Click to open · Tab focus · Esc close".into(),
                preview,
                kind: InspectorKind::Agents,
                activity: Some(carbon::ActivityViewport {
                    entries,
                    scroll,
                    selected: navigation.selected.clone(),
                }),
                ..Inspector::default()
            };
        }
        let title = page.filter(|page| page.end < page.total).map_or_else(
            || "Terminals".into(),
            |page| {
                format!(
                    "Terminals · Events {}–{} of {}",
                    page.begin + 1,
                    page.end,
                    page.total
                )
            },
        );
        Inspector {
            title,
            tabs: tabs(),
            selected_tab: 1,
            kind: InspectorKind::Agents,
            lines: vec![if slot.state.snapshot.is_some() && page.is_some() {
                "No activity in this part of the conversation".into()
            } else {
                "Loading activity…".into()
            }],
            footer: "↑/↓ select · Enter open · PgUp/PgDn scroll · s shell · Esc".into(),
            activity: Some(carbon::ActivityViewport {
                entries,
                scroll: navigation.scroll,
                selected: navigation.selected.clone(),
            }),
            ..Inspector::default()
        }
    }

    pub(super) fn scroll_activity(&mut self, older: bool, step: usize) {
        self.selection = None;
        self.tool_press = None;
        let Some(Panel::Activity(navigation)) = &mut self.panel else {
            return;
        };
        navigation.selected = None;
        let (limit, scroll) = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.activity.as_ref())
            .map_or((0, 0), |frame| (frame.scroll_limit, frame.activity_scroll));
        if navigation.agents_only {
            navigation.initial_agents_position = false;
            navigation.scroll = if older {
                scroll.saturating_add(step).min(limit)
            } else {
                scroll.saturating_sub(step)
            };
            return;
        }
        let slot = &mut self.slots[self.current];
        if let Some(page) = &slot.activity_timeline
            && page.query != slot.activity_query
        {
            return;
        }
        navigation.scroll = scroll;
        if let Some(page) = &slot.activity_timeline {
            let query = if older && navigation.scroll == limit && page.begin > 0 {
                Some((page.begin, false))
            } else if !older && navigation.scroll == 0 && page.end < page.total {
                Some((page.end, true))
            } else {
                None
            };
            if !older && navigation.scroll == 0 && page.end == page.total {
                slot.activity_query = latest();
                return;
            }
            if let Some((before, forward)) = query {
                slot.activity_query = GetTranscript {
                    before,
                    forward,
                    limit: PAGE_SIZE,
                };
                navigation.scroll = if forward { usize::MAX } else { 0 };
                return;
            }
        }
        navigation.scroll = if older {
            navigation.scroll.saturating_add(step).min(limit)
        } else {
            navigation.scroll.saturating_sub(step)
        };
    }

    pub(super) fn select_activity(&mut self, older: bool) {
        let Some(Panel::Activity(pane)) = &self.panel else {
            return;
        };
        let frame = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.activity.as_ref());
        let pending;
        let keys = if let Some(frame) = frame {
            &frame.activity_keys
        } else {
            pending = self
                .activity_entries(pane)
                .into_iter()
                .filter_map(|entry| entry.action)
                .collect::<Vec<_>>();
            &pending
        };
        let Some(Panel::Activity(navigation)) = &mut self.panel else {
            return;
        };
        if keys.is_empty() {
            self.scroll_activity(older, 1);
            return;
        }
        navigation.initial_agents_position = false;
        let index = navigation
            .selected
            .as_ref()
            .and_then(|selected| keys.iter().position(|key| key == selected));
        let next = if older {
            index.map_or(keys.len() - 1, |index| index.saturating_sub(1))
        } else {
            index.map_or(0, |index| (index + 1).min(keys.len() - 1))
        };
        if let Some(frame) = frame {
            navigation.scroll = frame.activity_scroll;
        }
        navigation.selected = Some(keys[next].clone());
    }

    pub(super) fn enter_activity(&mut self, io: &Io) {
        let Some(Panel::Activity(navigation)) = &self.panel else {
            return;
        };
        let selected =
            |action: &String| navigation.selected.as_ref().is_none_or(|key| key == action);
        let action = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.activity.as_ref())
            .and_then(|frame| {
                let mut matches = frame.tool_actions.iter().filter(|(_, key)| selected(key));
                (if navigation.agents_only {
                    matches.next()
                } else {
                    matches.next_back()
                })
                .map(|(_, key)| (key.clone(), frame.edit_view(key), frame.output_view(key)))
            })
            .or_else(|| {
                let mut matches =
                    self.activity_entries(navigation)
                        .into_iter()
                        .filter_map(|entry| {
                            let key = entry.action.filter(selected)?;
                            Some((key, entry.edit, entry.output))
                        });
                if navigation.agents_only {
                    matches.next()
                } else {
                    matches.next_back()
                }
            });
        if let Some((key, edit, output)) = action {
            self.activate_activity(key, edit, output, io);
        }
    }

    pub(super) fn activity_press(&self, column: usize, row: usize) -> Option<ToolPress> {
        let hit = |frame: &carbon::Frame| {
            let (activity, key) = frame.activity_at(column, row)?;
            Some((
                column,
                row,
                key.to_owned(),
                activity.edit_view(key),
                activity.output_view(key),
            ))
        };
        let target = hit(self.displayed.as_ref()?)?;
        self.frames
            .iter()
            .all(|(_, frame)| hit(frame).as_ref() == Some(&target))
            .then_some(target)
    }

    pub(super) fn activate_activity(
        &mut self,
        key: String,
        edit: Option<Arc<crate::frontend::diff::EditView>>,
        output: Option<Arc<carbon::OutputView>>,
        io: &Io,
    ) {
        let Some(Panel::Activity(navigation)) = &mut self.panel else {
            return;
        };
        let keep_selection = navigation.agents_only && navigation.selected.as_ref() == Some(&key);
        navigation.initial_agents_position = false;
        navigation.selected = Some(key.clone());
        if let Some(frame) = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.activity.as_ref())
        {
            navigation.scroll = frame.activity_scroll;
        }
        let mut previous = navigation.clone();
        if !keep_selection && previous.scroll == 0 && key.starts_with("agent:") {
            previous.selected = None;
        }
        self.activate_result(key, edit, output, io);
        match &self.panel {
            Some(Panel::Activity(_) | Panel::Output { .. }) => {}
            Some(_) => self.activity_back = Some(previous),
            None => self.return_to_activity(),
        }
    }

    pub(super) fn activity_key(&mut self, byte: u8) -> bool {
        if matches!(self.panel, Some(Panel::NativeTerminal { .. })) && byte == b'a' {
            self.open_activity();
            return true;
        }
        if !matches!(self.panel, Some(Panel::Activity(_))) {
            return false;
        }
        match byte {
            b's' => self.activity_shell(),
            b'j' | b'k' => self.select_activity(byte == b'k'),
            b'g' | b'G' => {
                let Some(Panel::Activity(navigation)) = &mut self.panel else {
                    return false;
                };
                navigation.selected = None;
                navigation.initial_agents_position = false;
                navigation.scroll = if byte == b'g' { usize::MAX } else { 0 };
                if byte == b'G' && !navigation.agents_only {
                    self.slots[self.current].activity_query = latest();
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_runs_share_headers_only_for_the_same_parent_role_and_model() {
        use carbon::AgentRole::{Implementation, Research};
        let row = |id: &str, parent: &str, role, model: &str, state: &str| ux::ConversationAgent {
            parent: parent.into(),
            branch: String::new(),
            line: carbon::AgentLine {
                id: id.into(),
                title: format!("Assignment {id}"),
                state: state.into(),
                role,
                model: model.into(),
                activity: String::new(),
                step: None,
                tool_calls: None,
            },
        };
        let entries = delegated_entries(vec![
            row("other", "fork", Implementation, "sol", "Paused"),
            row("one", "main", Implementation, "sol", "Blocked"),
            row("research", "main", Research, "sol", "Completed"),
            row("two", "main", Implementation, "sol", "Running"),
            row("third-model", "main", Implementation, "astra", "Completed"),
        ]);
        assert_eq!(entries[0].text, "2 runs");
        assert!(entries[0].action.is_none());
        assert_eq!(entries[1].action.as_deref(), Some("agent:two"));
        assert_eq!(entries[2].action.as_deref(), Some("agent:one"));
        assert_eq!(entries[2].agent.as_ref().unwrap().title, "Assignment one");
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[1].text, "02");
        assert_eq!(entries[2].text, "01");
        assert!(
            entries[3..]
                .iter()
                .all(|entry| entry.role == "agent_direct")
        );
    }
}
