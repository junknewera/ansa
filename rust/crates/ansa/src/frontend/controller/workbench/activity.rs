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

#[derive(Clone, Default)]
pub(super) struct Navigation {
    scroll: usize,
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
            ..Navigation::default()
        }));
        self.focus = Focus::Chat;
        self.refresh_agent_summaries();
    }

    fn activity_entries(&self, navigation: &Navigation) -> Vec<Entry> {
        if !navigation.agents_only {
            return self.terminal_activity();
        }
        self.agent_rows()
            .into_iter()
            .rev()
            .map(|(id, agent)| Entry {
                role: "agent_card".into(),
                text: String::new(),
                action: Some(format!("agent:{id}")),
                status: None,
                edit: None,
                output: None,
                agent: Some(Arc::new(agent)),
            })
            .collect()
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
            return Inspector {
                title: "Agents".into(),
                tabs: tabs(),
                lines: vec!["No agents in this conversation".into()],
                footer: "Click to open · Tab focus · Esc close".into(),
                kind: InspectorKind::Agents,
                activity: Some(carbon::ActivityViewport {
                    entries,
                    scroll: navigation.scroll,
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
                frame
                    .tool_actions
                    .iter()
                    .rev()
                    .find(|(_, key)| selected(key))
                    .map(|(_, key)| (key.clone(), frame.edit_view(key), frame.output_view(key)))
            })
            .or_else(|| {
                self.activity_entries(navigation)
                    .into_iter()
                    .rev()
                    .find_map(|entry| {
                        let key = entry.action.filter(selected)?;
                        Some((key, entry.edit, entry.output))
                    })
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
        navigation.selected = Some(key.clone());
        if let Some(frame) = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.activity.as_ref())
        {
            navigation.scroll = frame.activity_scroll;
        }
        let mut previous = navigation.clone();
        if previous.scroll == 0 && key.starts_with("agent:") {
            previous.selected = None;
        }
        let agents_only = previous.agents_only;
        self.activate_result(key, edit, output, io);
        if agents_only && matches!(self.panel, Some(Panel::AgentTranscript(_))) {
            self.focus = Focus::Chat;
        }
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
