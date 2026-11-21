//! Observed task/tool timings are frontend state, never a second session log.
use super::{carbon::Entry, protocol_v1::*};
use crate::provider::replay::{Effects, Execution};
use std::{collections::BTreeMap, time::Instant};

struct Clock {
    started: Instant,
    stopped: Option<Instant>,
    resumed: bool,
}

struct ToolClock {
    name: String,
    started: Option<Instant>,
    stopped: Option<Instant>,
}
#[derive(Default)]
pub(super) struct Progress {
    pending: Option<CommandId>,
    pending_resume: Option<(CommandId, RequestId, Instant)>,
    request: Option<RequestId>,
    clock: Option<Clock>,
    tools: BTreeMap<String, ToolClock>,
    seconds: u64,
}
impl Progress {
    pub fn tool_elapsed(&self, operation: &str, now: Instant) -> String {
        self.tools
            .get(operation)
            .and_then(|clock| {
                clock.started.map(|started| {
                    format!(
                        " · {} ms",
                        clock
                            .stopped
                            .unwrap_or(now)
                            .saturating_duration_since(started)
                            .as_millis()
                    )
                })
            })
            .unwrap_or_default()
    }
    pub fn submitted(&mut self, command: CommandId, now: Instant) {
        *self = Self {
            pending: Some(command),
            clock: Some(Clock {
                started: now,
                stopped: None,
                resumed: false,
            }),
            ..Self::default()
        };
    }
    pub fn rejected(&mut self, command: &CommandId) {
        if self.pending.as_ref() == Some(command) {
            *self = Self::default();
        }
        if self
            .pending_resume
            .as_ref()
            .is_some_and(|(id, _, _)| id == command)
        {
            self.pending_resume = None;
        }
    }
    pub fn resume_sent(&mut self, command: CommandId, target: RequestId, now: Instant) {
        if self.pending_resume.is_none() {
            self.pending_resume = Some((command, target, now));
        }
    }
    pub fn observe(&mut self, view: &Snapshot, now: Instant) {
        if let Some(command) = &self.pending {
            if view.submit.as_ref() != Some(command) {
                return;
            }
            self.pending = None;
            self.request = view.request.clone();
        }
        if self.request != view.request {
            self.request = view.request.clone();
            self.clock = None;
            self.tools.clear();
        }
        let running = view.worker_busy && view.request.is_some();
        if let Some((_, target, started)) = &self.pending_resume
            && view.request.as_ref() == Some(target)
            && running
            && !matches!(view.state.as_str(), "loading" | "paused")
        {
            if self.clock.is_none() {
                self.clock = Some(Clock {
                    started: *started,
                    stopped: None,
                    resumed: true,
                });
            }
            self.pending_resume = None;
        }
        if running {
            if let Some(clock) = &mut self.clock {
                clock.stopped = None;
            }
        } else if let Some(clock) = &mut self.clock {
            clock.stopped.get_or_insert(now);
        }
        for activity in &view.tools {
            match activity {
                Activity::Started(tool) => {
                    if self.tools.len() < 256 {
                        self.tools
                            .entry(tool.operation.0.clone())
                            .or_insert_with(|| ToolClock {
                                name: tool.name.clone(),
                                started: None,
                                stopped: None,
                            });
                    }
                }
                Activity::Finished(tool) => self.finished(tool, now),
            }
        }
        self.tick(now);
    }
    pub fn started(&mut self, tool: &Started, now: Instant) {
        if self.tools.len() < 256 {
            let clock = self
                .tools
                .entry(tool.operation.0.clone())
                .or_insert_with(|| ToolClock {
                    name: tool.name.clone(),
                    started: None,
                    stopped: None,
                });
            clock.started.get_or_insert(now);
        }
    }
    pub fn finished(&mut self, tool: &Finished, now: Instant) {
        if let Some(clock) = self.tools.get_mut(&tool.operation.0) {
            clock.stopped.get_or_insert(now);
        }
    }
    pub fn project(&mut self, view: &mut Snapshot, activity: Activity, now: Instant) {
        if let Some(index) = view
            .tools
            .iter()
            .position(|old| old.operation() == activity.operation())
        {
            if matches!(
                (&view.tools[index], &activity),
                (Activity::Finished(_), Activity::Started(_))
            ) {
                return;
            }
            view.tools[index] = activity.clone();
        } else if view.tools.len() < 256 {
            view.tools.push(activity.clone());
        } else {
            return;
        }
        match activity {
            Activity::Started(tool) => self.started(&tool, now),
            Activity::Finished(tool) => self.finished(&tool, now),
        }
        view.completed_tools = view
            .tools
            .iter()
            .filter(|tool| matches!(tool, Activity::Finished(result) if result.effects == Effects::KnownResult))
            .count();
    }
    pub fn tick(&mut self, now: Instant) -> bool {
        let seconds = self.clock.as_ref().map_or(0, |c| {
            c.stopped
                .unwrap_or(now)
                .saturating_duration_since(c.started)
                .as_secs()
        });
        let changed = seconds != self.seconds;
        self.seconds = seconds;
        changed
    }
    pub fn elapsed(&self) -> Option<String> {
        self.clock.as_ref().map(|c| {
            format!(
                "{}{} s",
                if c.resumed { "since /resume: " } else { "" },
                self.seconds
            )
        })
    }
    pub fn phase(view: &Snapshot, preview: bool) -> &'static str {
        if view.storage_failed {
            return "save error";
        }
        if view.recovery_operation.is_some() {
            return "effect_unknown";
        }
        match view.state.as_str() {
            "idle" | "completed" => "ready",
            "cancelled" if view.worker_busy => "cancelling",
            "cancelled" => "cancelled",
            "paused" => "paused",
            "generating" if preview => "receiving response",
            "generating" => "waiting for model",
            "executing" => "running tool",
            "compacting" => "compacting context",
            "persisting" | "finishing" => "saving",
            "loading" => "opening session",
            _ => "working",
        }
    }
    pub fn entries(&self, view: &Snapshot, expanded: bool, now: Instant) -> Vec<Entry> {
        if self.pending.is_some() || self.request != view.request {
            return vec![];
        }
        let mut result = vec![];
        let begin = if expanded {
            0
        } else {
            view.tools.len().saturating_sub(3)
        };
        if begin > 0 {
            result.push(Entry {
                action: None,
                edit: None,
                output: None,
                agent: None,
                status: None,
                role: "tool".into(),
                text: format!("{begin} more calls · Ctrl+O details"),
            });
        }
        for activity in &view.tools[begin..] {
            let clock = self.tools.get(&activity.operation().0);
            let (name, state, output) = match activity {
                Activity::Started(tool) => (
                    tool.name.as_str(),
                    if view.recovery_operation.as_ref() == Some(&tool.operation) {
                        "effect_unknown"
                    } else if !view.worker_busy {
                        "pending validation"
                    } else {
                        "running"
                    },
                    None,
                ),
                Activity::Finished(tool) => (
                    clock.map_or(tool.call_id.as_str(), |c| c.name.as_str()),
                    if tool.effects == Effects::Unknown {
                        "effect_unknown"
                    } else {
                        match tool.execution {
                            Execution::Succeeded => "done",
                            Execution::Failed => "error",
                            Execution::Cancelled => "cancelled",
                            Execution::Denied => "denied",
                        }
                    },
                    Some(tool.output.as_str()),
                ),
            };
            let elapsed = clock
                .and_then(|c| {
                    c.started.map(|started| {
                        format!(
                            " ~{} ms",
                            c.stopped
                                .unwrap_or(now)
                                .saturating_duration_since(started)
                                .as_millis()
                        )
                    })
                })
                .unwrap_or_default();
            let mut text = format!("{name}: {state}{elapsed}");
            if let Some(output) = output {
                let limit = if expanded { 2048 } else { 160 };
                let preview: String = output.chars().take(limit).collect();
                if !preview.is_empty() {
                    text.push('\n');
                    text.push_str(&preview);
                }
                if output.chars().nth(limit).is_some() {
                    text.push_str("\n[output truncated]");
                }
            }
            result.push(Entry {
                action: None,
                edit: None,
                output: None,
                agent: None,
                status: None,
                role: "tool".into(),
                text,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    fn view() -> Snapshot {
        Snapshot {
            timeline_size: 0,
            conversation_size: 0,
            generation: super::super::protocol_v1::GenerationSelection {
                model: "fixture-model".into(),
                effort: None,
            },
            followups: vec![],
            through_sequence: 1,
            state: "generating".into(),
            request: Some("request".into()),
            worker_busy: true,
            completed_tools: 0,
            text: String::new(),
            diagnostic: String::new(),
            recovery_operation: None,
            submit: Some("submit".into()),
            command: None,
            tools: vec![],
            history_items: 0,
            compactions: 0,
            autocompact_percent: 50,
            storage_failed: false,
            durable: true,
            resume_available: false,
            mode: AgentMode::Basic,
            transcript_size: 1,
            preview: None,
        }
    }
    #[test]
    fn elapsed_covers_rounds_freezes_at_done_and_does_not_claim_restart_time() {
        let start = Instant::now();
        let mut progress = Progress::default();
        let mut v = view();
        progress.submitted("submit".into(), start);
        v.submit = Some("old".into());
        v.state = "completed".into();
        v.worker_busy = false;
        progress.observe(&v, start + Duration::from_secs(1));
        progress.tick(start + Duration::from_secs(2));
        assert_eq!(progress.elapsed().as_deref(), Some("2 s"));
        v.submit = Some("submit".into());
        v.worker_busy = true;
        for (index, stage) in ["generating", "executing", "generating", "finishing"]
            .into_iter()
            .enumerate()
        {
            v.state = stage.into();
            progress.observe(&v, start + Duration::from_secs(index as u64 + 3));
        }
        v.worker_busy = false;
        v.state = "completed".into();
        progress.observe(&v, start + Duration::from_secs(9));
        progress.observe(&v, start + Duration::from_secs(20));
        assert_eq!(progress.elapsed().as_deref(), Some("9 s"));
        let mut reopened = Progress::default();
        v.state = "paused".into();
        v.worker_busy = true;
        reopened.observe(&v, start);
        assert!(reopened.elapsed().is_none());
        reopened.resume_sent("resume".into(), "request".into(), start);
        v.worker_busy = true;
        v.state = "generating".into();
        reopened.observe(&v, start);
        reopened.tick(start + Duration::from_secs(2));
        assert_eq!(reopened.elapsed().as_deref(), Some("since /resume: 2 s"));
    }
    #[test]
    fn tool_snapshots_do_not_reset_timing_or_hide_unknown_effects() {
        let start = Instant::now();
        let mut progress = Progress::default();
        let mut v = view();
        let started = Started {
            operation: "operation".into(),
            call_id: "call".into(),
            name: "run_command".into(),
        };
        v.tools = vec![Activity::Started(started.clone())];
        progress.observe(&v, start);
        progress.started(&started, start);
        progress.observe(&v, start + Duration::from_secs(2));
        let finished = Finished {
            operation: "operation".into(),
            call_id: "call".into(),
            execution: Execution::Failed,
            effects: Effects::Unknown,
            output: Arc::new("\x1b[31mfailure".into()),
        };
        v.tools = vec![Activity::Finished(finished)];
        progress.observe(&v, start + Duration::from_secs(3));
        progress.observe(&v, start + Duration::from_secs(5));
        let entries = progress.entries(&v, false, start + Duration::from_secs(8));
        assert!(
            entries[0]
                .text
                .contains("run_command: effect_unknown ~3000 ms")
        );
        let frame = super::super::carbon::layout(
            &entries,
            b"draft",
            5,
            "basic",
            false,
            0,
            "fixture",
            (80, 24),
            false,
        )
        .unwrap();
        assert!(!frame.lines.join("\n").contains('\x1b'));
        let mut reopened = Progress::default();
        reopened.observe(&v, start);
        assert!(!reopened.entries(&v, false, start)[0].text.contains(" ms"));
        v.recovery_operation = Some("operation".into());
        assert_eq!(Progress::phase(&v, false), "effect_unknown");
    }
    #[test]
    fn live_tools_render_before_the_next_snapshot_without_duplicates() {
        let start = Instant::now();
        let mut progress = Progress::default();
        let mut v = view();
        progress.observe(&v, start);
        let started = Started {
            operation: "op".into(),
            call_id: "call".into(),
            name: "read_file".into(),
        };
        progress.project(&mut v, Activity::Started(started.clone()), start);
        assert!(
            progress.entries(&v, false, start)[0]
                .text
                .contains("read_file: running")
        );
        let finished = Finished {
            operation: "op".into(),
            call_id: "call".into(),
            execution: Execution::Succeeded,
            effects: Effects::KnownResult,
            output: Arc::new("result".into()),
        };
        progress.project(&mut v, Activity::Finished(finished.clone()), start);
        progress.project(&mut v, Activity::Finished(finished), start);
        progress.project(&mut v, Activity::Started(started), start);
        assert_eq!(v.tools.len(), 1);
        assert_eq!(v.completed_tools, 1);
        assert!(
            progress.entries(&v, false, start)[0]
                .text
                .contains("read_file: done")
        );
        let mut unknown = match &v.tools[0] {
            Activity::Finished(result) => result.clone(),
            _ => unreachable!(),
        };
        unknown.effects = Effects::Unknown;
        progress.project(&mut v, Activity::Finished(unknown), start);
        assert_eq!(v.completed_tools, 0);
    }
}
