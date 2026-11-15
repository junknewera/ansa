//! Serialized frontend state. Runtime state is changed only by typed commands.
use super::{
    Result,
    carbon::{self, Entry},
    copy::CopyHistory,
    diagnostics::{self, WaitClock},
    history::History,
    input::{Action, Editor},
    jsonl,
    progress::Progress,
    protocol_v1::*,
    terminal::{Drain, Io},
};
use crate::{
    runtime::{Handle, PostStatus, Runtime},
    session::transcript,
};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
pub(crate) mod workbench;
pub fn history_page_current(page: &TranscriptPage, query: &GetTranscript, total: usize) -> bool {
    page.query == *query && page.total >= total
}
pub fn preview_visible(
    entries: &[transcript::Entry],
    end: usize,
    total: usize,
    request: Option<&RequestId>,
    preview: Option<&Preview>,
    completed: bool,
) -> bool {
    end == total
        && entries
            .last()
            .is_some_and(|e| e.role == "user" && Some(&e.request) == request)
        && request.is_some()
        && preview.is_some_and(|p| !p.text.is_empty() && (p.state != "committed" || completed))
}
type Replies = JoinSet<(CommandId, bool, bool, Result<PostStatus>)>;
pub struct State {
    pub snapshot: Option<Snapshot>,
    pub turn: TurnId,
    pub entries: Vec<transcript::Entry>,
    pub page: (usize, usize, usize),
    pub history_ready: bool,
    pub query: GetTranscript,
    pub editor: Editor,
    pub mode: AgentMode,
    pub expanded: bool,
    pub scroll: usize,
    pub scroll_limit: usize,
    pub status: String,
    pub pending: Option<CommandId>,
    pub deferred_cancel: bool,
    pub cancelled: bool,
    pub exit: i32,
    preview: Option<Preview>,
    preview_request: Option<RequestId>,
    sequence: u64,
    prefix: String,
    mode_ready: bool,
    printed: Option<RequestId>,
    first: bool,
    pending_draft: Option<(CommandId, Vec<u8>, usize)>,
    last_pause: String,
    wait_clock: WaitClock,
    explicit_status: bool,
    input_history: History,
    progress: Progress,
    tools_expanded: bool,
    reading: Option<Vec<Entry>>,
    follow_tail: bool,
    reading_total: usize,
    copy: CopyHistory,
    snapshot_read: Option<CommandId>,
}
impl State {
    pub fn new(handle: &Handle) -> Self {
        Self {
            snapshot: None,
            turn: "idle".into(),
            entries: vec![],
            page: (0, 0, 0),
            history_ready: false,
            query: GetTranscript {
                before: 0,
                limit: 32,
                forward: false,
            },
            editor: Editor::default(),
            mode: AgentMode::Basic,
            expanded: true,
            scroll: 0,
            scroll_limit: 0,
            status: String::new(),
            pending: None,
            deferred_cancel: false,
            cancelled: false,
            exit: 0,
            preview: None,
            preview_request: None,
            sequence: 0,
            prefix: handle.incarnation().0[..12].into(),
            mode_ready: false,
            printed: None,
            first: true,
            pending_draft: None,
            last_pause: String::new(),
            wait_clock: WaitClock::default(),
            explicit_status: false,
            input_history: History::default(),
            progress: Progress::default(),
            tools_expanded: false,
            reading: None,
            follow_tail: true,
            reading_total: 0,
            copy: CopyHistory::default(),
            snapshot_read: None,
        }
    }
    fn command(&mut self, handle: &Handle, payload: CommandPayload) -> RuntimeCommand {
        self.sequence += 1;
        RuntimeCommand {
            schema_version: 1,
            session: handle.session().clone(),
            turn: self.turn.clone(),
            incarnation: handle.incarnation().clone(),
            command: CommandId(format!("c_{}_{}", self.prefix, self.sequence)),
            payload,
        }
    }
    fn post(&mut self, handle: &Handle, payload: CommandPayload, replies: &mut Replies) -> bool {
        self.post_with_origin(handle, payload, replies, false)
    }
    fn post_read(
        &mut self,
        handle: &Handle,
        payload: CommandPayload,
        replies: &mut Replies,
    ) -> bool {
        if matches!(payload, CommandPayload::GetSnapshot(_)) && self.snapshot_read.is_some() {
            return false;
        }
        debug_assert!(matches!(
            payload,
            CommandPayload::GetSnapshot(_)
                | CommandPayload::GetTranscript(_)
                | CommandPayload::GetTimeline(_)
        ));
        self.post_with_origin(handle, payload, replies, true)
    }
    fn post_with_origin(
        &mut self,
        handle: &Handle,
        payload: CommandPayload,
        replies: &mut Replies,
        internal_read: bool,
    ) -> bool {
        if replies.len() >= 64 {
            return false;
        }
        let command = self.command(handle, payload);
        if internal_read && matches!(command.payload, CommandPayload::GetSnapshot(_)) {
            self.snapshot_read = Some(command.command.clone());
        }
        let new_turn = matches!(command.payload, CommandPayload::Submit(_));
        let submit = matches!(
            command.payload,
            CommandPayload::Submit(_) | CommandPayload::Steer(_)
        );
        if new_turn {
            self.pending = Some(command.command.clone());
        }
        if let CommandPayload::Resume(target) = &command.payload {
            self.progress
                .resume_sent(command.command.clone(), target.clone(), Instant::now());
        }
        let h = handle.clone();
        replies.spawn(async move {
            let id = command.command.clone();
            (id, submit, internal_read, h.post(command).await)
        });
        true
    }
    fn awaiting(&self) -> bool {
        self.pending.is_some()
            && self
                .snapshot
                .as_ref()
                .is_none_or(|v| v.submit != self.pending)
    }
    fn recall_page(&mut self, handle: &Handle, replies: &mut Replies) {
        if replies.len() >= 32 {
            return;
        }
        let Some(query) = self.input_history.query() else {
            return;
        };
        let command = self.command(handle, CommandPayload::GetTranscript(query));
        self.input_history.sent(command.command.clone());
        let h = handle.clone();
        replies.spawn(async move {
            let id = command.command.clone();
            (id, false, true, h.post(command).await)
        });
    }
    fn available(&self) -> bool {
        !self.awaiting()
            && self.snapshot.as_ref().is_some_and(|v| {
                !v.worker_busy && matches!(v.state.as_str(), "idle" | "completed" | "cancelled")
            })
    }
    fn start_copy(&mut self, io: &Io) {
        let result = if io.raw.is_some() {
            self.copy
                .start(self.snapshot.as_ref().map_or(0, |v| v.transcript_size))
        } else {
            Err("/copy доступен в интерактивном терминале")
        };
        self.report(io, result.err().unwrap_or("Копирование диалога..."));
    }
    fn copy_page(&mut self, handle: &Handle, replies: &mut Replies, io: &Io) {
        if let Some(result) = self.copy.take() {
            let message = match result {
                Ok(text) if io.writer.clipboard(&text) => {
                    "Диалог отправлен в буфер терминала (OSC 52)"
                }
                Ok(_) => "Копирование недоступно: очередь вывода заполнена",
                Err(message) => message,
            };
            self.report(io, message);
        }
        if replies.len() >= 32 {
            return;
        }
        let Some(query) = self.copy.query() else {
            return;
        };
        let command = self.command(handle, CommandPayload::GetTranscript(query));
        self.copy.sent(command.command.clone());
        let h = handle.clone();
        replies.spawn(async move {
            let id = command.command.clone();
            (id, false, true, h.post(command).await)
        });
    }
    fn report(&mut self, io: &Io, text: &str) {
        if io.raw.is_some() {
            self.status = text.into();
        } else {
            io.status(text);
        }
    }
    fn status_text(&self) -> String {
        let Some(view) = &self.snapshot else {
            return "ansa: loading\n".into();
        };
        let mut text = format!("ansa: {} tools={}", view.state, view.completed_tools);
        if diagnostics::needs_attention(view) {
            text.push('\n');
            text.push_str(&diagnostics::pause(view));
        }
        if let Some(wait) = self.wait_clock.text() {
            text.push('\n');
            text.push_str(&wait);
        }
        if let Some(elapsed) = self.progress.elapsed() {
            text.push_str(&format!("\nВремя задачи: {elapsed}"));
        }
        text.push('\n');
        text
    }
    fn text(
        &mut self,
        text: &str,
        handle: &Handle,
        io: &Io,
        replies: &mut Replies,
    ) -> (bool, bool) {
        if text != "/status" {
            self.explicit_status = false;
        }
        let mut words = text.split_whitespace();
        let first = words.next();
        if matches!(first, Some("/model" | "/thinking" | "/effort")) {
            let Some(value) = words.next() else {
                self.report(
                    io,
                    "Укажите /model MODEL или /effort LEVEL (default - выбор провайдера)\n",
                );
                return (true, false);
            };
            if words.next().is_some() {
                self.report(io, "Ожидалось одно значение\n");
                return (false, false);
            }
            let Some(snapshot) = &self.snapshot else {
                return (false, false);
            };
            let selection = if first == Some("/model") {
                GenerationSelection {
                    model: value.into(),
                    effort: None,
                }
            } else {
                GenerationSelection {
                    model: snapshot.generation.model.clone(),
                    effort: if value == "default" {
                        None
                    } else {
                        Some(value.into())
                    },
                }
            };
            self.post(handle, CommandPayload::SetGeneration(selection), replies);
            self.report(io, "Проверяю выбор для следующих сообщений...\n");
            return (true, false);
        }
        match text {
            "/copy" => {
                self.start_copy(io);
                return (true, false);
            }
            "/exit" | "/quit" => {
                handle.request_shutdown();
                return (true, true);
            }
            "/basic" | "/plan" => {
                if self.available() {
                    self.mode = if text == "/plan" {
                        AgentMode::Plan
                    } else {
                        AgentMode::Basic
                    };
                }
                return (true, false);
            }
            "/status" => {
                self.wait_clock.tick(Instant::now());
                if self.snapshot.is_some() {
                    let text = self.status_text();
                    if io.raw.is_some() {
                        self.explicit_status = true;
                    } else {
                        io.status(&text);
                    }
                }
                return (true, false);
            }
            "/cancel" => {
                if self.awaiting() {
                    self.deferred_cancel = true;
                    self.post_read(handle, CommandPayload::GetSnapshot(0), replies);
                } else if let Some(target) = self.snapshot.as_ref().and_then(|v| v.request.clone())
                {
                    self.post(handle, CommandPayload::Cancel(target), replies);
                } else {
                    self.report(io, "ansa: no_active_request\n");
                }
                return (true, false);
            }
            "/resume" => {
                if let Some(target) = self
                    .snapshot
                    .as_ref()
                    .filter(|v| v.resume_available)
                    .and_then(|v| v.request.clone())
                {
                    self.post(handle, CommandPayload::Resume(target), replies);
                } else {
                    self.report(io, "ansa: resume_not_available\n");
                }
                return (true, false);
            }
            "" => return (true, false),
            _ => {}
        }
        if let Some(view) = self.snapshot.as_ref().filter(|v| v.state == "paused") {
            let message = format!("ansa: {}\n", diagnostics::pause(view));
            if io.raw.is_none() {
                self.report(io, &message);
            } else {
                self.status.clear();
            }
            self.exit = 1;
            return (false, false);
        }
        if !self.available() {
            if let Some(target) = self
                .snapshot
                .as_ref()
                .filter(|v| {
                    !v.storage_failed
                        && !matches!(
                            v.state.as_str(),
                            "paused" | "cancelled" | "completed" | "idle"
                        )
                })
                .and_then(|v| v.request.clone())
            {
                let id = CommandId(format!("c_{}_{}", self.prefix, self.sequence + 1));
                let posted = self.post(
                    handle,
                    CommandPayload::Steer(Steer {
                        target,
                        text: text.into(),
                    }),
                    replies,
                );
                if posted && io.raw.is_some() {
                    self.pending_draft = Some((id, self.editor.bytes.clone(), self.editor.cursor));
                    self.status = "Сообщение ожидает завершения текущего шага".into();
                }
                return (posted, false);
            }
            self.report(io, "ansa: busy; wait or use Ctrl+C\n");
            self.exit = 1;
            return (false, false);
        }
        self.turn = TurnId(format!("t_{}_{}", self.prefix, self.sequence + 1));
        let posted = self.post(
            handle,
            CommandPayload::Submit(Submit {
                text: text.into(),
                mode: self.mode,
            }),
            replies,
        );
        if posted {
            self.progress
                .submitted(self.pending.clone().expect("posted Submit"), Instant::now());
            self.tools_expanded = false;
            self.cancelled = false;
            if io.raw.is_some() {
                self.pending_draft = self
                    .pending
                    .clone()
                    .map(|id| (id, self.editor.bytes.clone(), self.editor.cursor));
            }
        }
        if !posted {
            self.report(io, "ansa: submit_rejected\n");
            self.exit = 1;
        }
        (posted, false)
    }
    pub fn apply(&mut self, event: &RuntimeEvent, io: &Io) {
        match &event.payload {
            EventPayload::FrontendSnapshot(v) => {
                self.progress.observe(v, Instant::now());
                if diagnostics::needs_attention(v)
                    && self.snapshot.as_ref().is_none_or(|old| {
                        !diagnostics::needs_attention(old)
                            || old.diagnostic != v.diagnostic
                            || old.request != v.request
                            || old.storage_failed != v.storage_failed
                            || old.recovery_operation != v.recovery_operation
                    })
                {
                    self.status.clear();
                }
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|old| old.state != v.state || old.request != v.request)
                {
                    self.explicit_status = false;
                }
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(diagnostics::needs_attention)
                    && !diagnostics::needs_attention(v)
                {
                    self.status.clear();
                }
                self.wait_clock.observe(v, Instant::now());
                if diagnostics::needs_attention(v) {
                    let message = diagnostics::pause(v);
                    if io.raw.is_none() && self.last_pause != message {
                        io.status(&format!("ansa: {message}\n"));
                    }
                    self.last_pause = message;
                } else {
                    self.last_pause.clear();
                }
                let fallback = !self.first
                    && v.state == "completed"
                    && !v.text.is_empty()
                    && self.printed != event.request;
                if self.first && v.state == "completed" {
                    self.printed = event.request.clone();
                }
                self.first = false;
                if fallback && io.raw.is_none() {
                    io.writer.text(format!("{}\n", v.text));
                    self.printed = event.request.clone();
                }
                if !self.mode_ready {
                    self.mode = v.mode;
                    self.mode_ready = true;
                }
                self.preview = v.preview.clone();
                self.preview_request = v.request.clone();
                self.snapshot = Some((**v).clone());
                self.turn = event.turn.clone();
            }
            EventPayload::TextDelta { text } => {
                if io.raw.is_none() {
                    io.writer.text(format!("{text}\n"));
                }
                self.printed = event.request.clone();
            }
            EventPayload::TranscriptPage(v) => {
                self.input_history.apply(v, &mut self.editor);
                self.copy.apply(v);
                if history_page_current(
                    v,
                    &self.query,
                    self.snapshot.as_ref().map_or(0, |v| v.transcript_size),
                ) {
                    if (v.begin, v.end) != (self.page.0, self.page.1) {
                        self.reading = None;
                    }
                    self.entries = v.entries.clone();
                    self.page = (v.begin, v.end, v.total);
                    self.history_ready = true;
                }
            }
            EventPayload::Preview(v) => {
                if v.state != "committed" {
                    self.preview = Some(v.clone());
                    self.preview_request = event.request.clone();
                }
            }
            EventPayload::ToolStarted(v) => {
                if let Some(view) = &mut self.snapshot
                    && event.request.is_some()
                    && event.request == view.request
                {
                    self.progress
                        .project(view, Activity::Started(v.clone()), Instant::now());
                }
            }
            EventPayload::ToolFinished(v) => {
                if let Some(view) = &mut self.snapshot
                    && event.request.is_some()
                    && event.request == view.request
                {
                    self.progress
                        .project(view, Activity::Finished(v.clone()), Instant::now());
                }
            }
            EventPayload::ResyncRequired { .. } => {
                self.input_history.retry(None);
                self.copy.retry(None);
            }
            EventPayload::FollowupQueued { .. } => {
                self.report(io, "Сообщение сохранено; ожидает текущий шаг\n")
            }
            EventPayload::FollowupDelivered { .. } => {
                self.cancelled = false;
                self.report(io, "Уточнение передано агенту\n");
            }
            EventPayload::GenerationSelected { selection, .. } => {
                self.report(
                    io,
                    &format!(
                        "{} · {} - со следующего сообщения\n",
                        selection.model,
                        selection.effort.as_deref().unwrap_or("default")
                    ),
                );
            }
            EventPayload::RuntimeError {
                command,
                diagnostic,
            } => {
                if let Some(command) = command {
                    self.progress.rejected(command);
                    self.copy.rejected(command);
                }
                self.report(io, &format!("ansa: {}\n", diagnostics::error(diagnostic)))
            }
            _ => {}
        }
    }
    fn render(&mut self, io: &Io, context: &str) -> Option<carbon::Frame> {
        self.render_inner(io, context, None, None)
    }
    fn render_workspace(
        &mut self,
        io: &Io,
        context: &str,
        ui: &carbon::Workspace,
        projected: Option<&[Entry]>,
    ) -> Option<carbon::Frame> {
        self.render_inner(io, context, Some(ui), projected)
    }
    fn render_inner(
        &mut self,
        io: &Io,
        context: &str,
        ui: Option<&carbon::Workspace>,
        projected: Option<&[Entry]>,
    ) -> Option<carbon::Frame> {
        let view = self.snapshot.as_ref()?;
        if !self.history_ready {
            return None;
        }
        let users = if self
            .entries
            .last()
            .is_some_and(|e| e.role == "user" && e.state != "completed")
        {
            4
        } else {
            3
        };
        let mut begin = 0;
        let mut candidate = 0;
        let mut seen = 0;
        if !self.expanded {
            for (i, e) in self.entries.iter().enumerate().rev() {
                if e.role == "user" {
                    seen += 1;
                    if seen == users + 1 {
                        begin = candidate;
                        break;
                    }
                    candidate = i;
                }
            }
        }
        let mut entries: Vec<_> = self.entries[begin..]
            .iter()
            .map(|e| Entry {
                role: e.role.clone(),
                text: e.text.clone(),
            })
            .collect();
        if let Some(index) = self.entries[begin..]
            .iter()
            .position(|e| e.role == "user" && Some(&e.request) == view.request.as_ref())
        {
            entries.splice(
                index + 1..index + 1,
                self.progress
                    .entries(view, self.tools_expanded, Instant::now()),
            );
        }
        if preview_visible(
            &self.entries,
            self.page.1,
            self.page.2,
            self.preview_request.as_ref(),
            self.preview.as_ref(),
            view.state == "completed" && view.durable,
        ) {
            let preview = self.preview.as_ref().expect("checked");
            entries.push(Entry {
                role: "assistant".into(),
                text: format!(
                    "{}{}",
                    preview.text,
                    if preview.state == "discarded" {
                        " (незавершённый ответ)"
                    } else if view.state != "completed" {
                        " (предварительный ответ)"
                    } else {
                        ""
                    }
                ),
            });
        }
        let preview = self
            .preview
            .as_ref()
            .is_some_and(|p| !p.text.is_empty() && p.state == "preview")
            && self.preview_request == view.request;
        let phase = Progress::phase(view, preview);
        let mut status = format!("{}  {phase}", jsonl::mode(self.mode));
        if let Some(elapsed) = self.progress.elapsed() {
            status.push_str(&format!("  {elapsed}"));
        }
        if !view.tools.is_empty() {
            status.push_str(&format!(
                "  tools {}/{}  Ctrl+T",
                view.completed_tools,
                view.tools.len()
            ));
        }
        let queued = view
            .followups
            .iter()
            .filter(|f| f.state == "queued")
            .count();
        if queued > 0 {
            status.push_str(&format!("  сообщений ожидает: {queued}"));
        }
        if let Some(projected) = projected {
            entries = projected.to_vec();
        }
        if !self.follow_tail && self.reading.is_none() {
            self.reading = Some(entries.clone());
        }
        if !self.follow_tail {
            status.push_str(&format!(
                "  чтение, новых: {}",
                view.transcript_size.saturating_sub(self.reading_total)
            ));
        }
        // A live notice has a reserved row outside the frozen conversation.
        // Keep the code first so even a narrow terminal shows the failure.
        let notice = if diagnostics::needs_attention(view) {
            diagnostics::pause_notice(view)
        } else {
            String::new()
        };
        status.push('\n');
        status.push_str(&notice);
        status.push('\n');
        let detail = if self.explicit_status {
            format!("ansa: {} tools={}", view.state, view.completed_tools)
        } else if self.status.is_empty() && diagnostics::needs_attention(view) {
            if view.resume_available && !view.storage_failed && view.recovery_operation.is_none() {
                "/resume - продолжить; /cancel - отменить запрос".into()
            } else {
                "/resume недоступен; /cancel - отменить запрос".into()
            }
        } else {
            self.status.trim().to_owned()
        };
        status.push_str(&diagnostics::status_notice(view, &detail));
        let visible = self.reading.as_deref().unwrap_or(&entries);
        let frame = if let Some(ui) = ui {
            carbon::layout_workspace(
                visible,
                &self.editor.bytes,
                self.editor.cursor,
                &status,
                self.scroll,
                context,
                io.size,
                io.color,
                ui,
            )?
        } else {
            carbon::layout_full(
                visible,
                &self.editor.bytes,
                self.editor.cursor,
                &status,
                self.scroll,
                context,
                io.size,
                io.color,
            )?
        };
        self.scroll_limit = frame.scroll_limit;
        Some(frame)
    }
}
fn readiness_source(
    descriptor: ansa_linux::terminal::Descriptor,
) -> std::io::Result<tokio::io::unix::AsyncFd<ansa_linux::terminal::Descriptor>> {
    // Pinned Tokio's legacy safe constructor requires a stable owned FD in
    // practice. Descriptor owns a private OwnedFd; these local registrations
    // only expose immutable access and never replace/close their inner value.
    // Registration drops before Descriptor and before Io restores its guards.
    #[allow(deprecated)]
    tokio::io::unix::AsyncFd::with_interest(descriptor, tokio::io::Interest::READABLE)
}

pub async fn run(
    runtime: &mut Runtime,
    io: &mut Io,
    prompt: Option<&str>,
    jsonl_mode: bool,
    context: &str,
) -> Result<i32> {
    let handle = runtime.handle();
    let mut state = State::new(&handle);
    let mut replies = Replies::new();
    let mut eof = false;
    let mut closed = false;
    let mut sent_prompt = false;
    let mut finished = false;
    let mut actor_done = false;
    let mut events_done = false;
    let mut prompt_id = None;
    let mut deferred_input = std::collections::VecDeque::new();
    let mut first_frame = None;
    let mut drain = Drain::default();
    let mut force_drain = false;
    let mut output_error = None;
    let mut resync = false;
    let mut next_history = Instant::now();
    let mut next_cancel = Instant::now();
    let mut next_settled = Instant::now();
    let mut last_frame = None;
    let mut dirty = true;
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Regular files do not support epoll: keep the bounded polling fallback.
    let input_events = if prompt.is_none() {
        readiness_source(io.input.duplicate().map_err(|_| "cli_input_poll")?).ok()
    } else {
        None
    };
    let signal_events = readiness_source(
        io.signals
            .readiness_descriptor()
            .map_err(|_| "cli_signal_poll")?,
    )
    .map_err(|_| "cli_signal_poll")?;
    loop {
        if finished || io.writer.failed() {
            handle.request_shutdown();
        }
        if actor_done {
            if io.writer.failed() || (events_done && io.writer.idle()) {
                break;
            }
            if force_drain || drain.stalled(io.writer.written(), io.writer.idle(), Instant::now()) {
                output_error = Some(if force_drain {
                    "cli_output_interrupted"
                } else {
                    "cli_output_stalled"
                });
                io.writer.abort();
                break;
            }
        }
        let can_read = !eof
            && !finished
            && (!jsonl_mode || replies.is_empty())
            && state.pending_draft.is_none()
            && prompt.is_none()
            && (jsonl_mode
                || (state.snapshot.is_some()
                    && (io.raw.is_none()
                        || (state.history_ready
                            && first_frame.is_some_and(|v| io.writer.displayed(v))))));
        let mut input_guard = None;
        let mut signal_guard = None;
        tokio::select! {biased;
            reply=replies.join_next(),if !replies.is_empty()=>{
                if let Some(reply)=reply {dirty=true;let (id,submit,internal_read,result)=reply.map_err(|_|"cli_post_task")?;
                    // post() replies only after owner admission, so the single
                    // snapshot mailbox is free before another internal read.
                    if state.snapshot_read.as_ref()==Some(&id) {state.snapshot_read=None;}
                    let retry_read=internal_read&&matches!(&result,Ok(PostStatus::QueueFull));
                    if retry_read {
                        state.input_history.retry(Some(&id));
                        state.copy.retry(Some(&id));
                    }
                    // An internal read already queued at a local shutdown can
                    // lose its reply. Explicit/user commands still report Closed.
                    let expected_close=internal_read&&(closed||finished)&&matches!(&result,Ok(PostStatus::Closed));
                    let code=if expected_close||retry_read {None}else{match result {Ok(PostStatus::Accepted|PostStatus::Duplicate)=>None,Ok(PostStatus::QueueFull)=>Some("command_queue_full"),Ok(PostStatus::Closed)=>Some("runtime_closed"),Ok(PostStatus::StaleTarget)=>Some("stale_cancel_target"),Ok(PostStatus::StaleIncarnation)=>Some("stale_incarnation"),Ok(PostStatus::CommandIdLimit)=>Some("submit_id_limit"),Ok(PostStatus::CommandConflict)=>Some("command_id_conflict"),Err(code)=>Some(code)}};
                    if state.pending_draft.as_ref().is_some_and(|(draft,_,_)|draft==&id){let (_,bytes,cursor)=state.pending_draft.take().expect("checked");if code.is_some(){state.editor.bytes=bytes;state.editor.cursor=cursor;}}
                    if let Some(code)=code {
                        state.input_history.retry(Some(&id));
                        state.copy.rejected(&id);
                        state.progress.rejected(&id);
                        if submit && state.pending.as_ref()==Some(&id) {
                            state.pending=None;
                            if prompt.is_some(){finished=true;}
                        }
                        if jsonl_mode {
                            io.status(&(jsonl::error(code,Some(id.0.as_str()).filter(|id|crate::provider::contracts::valid_id(id))).dump()+"\n"));
                        } else {
                            state.report(io,&format!("ansa: {code}\n"));state.exit=1;
                        }
                    }
                }
            },
            event=handle.next_event(),if !events_done && io.writer.capacity()=>{
                if let Some(event)=event {dirty=true;if jsonl_mode {io.writer.text(jsonl::encode(&event).dump()+"\n");}else{if matches!(event.payload,EventPayload::ResyncRequired {..}){resync=true;}state.apply(&event,io);}}else{events_done=true;}
            },
            result=runtime.join(),if !actor_done=>{result?;actor_done=true;},
            ready=async {input_events.as_ref().expect("enabled").readable().await},if can_read&&input_events.is_some()=>{input_guard=Some(ready.map_err(|_|"cli_input_poll")?);},
            ready=signal_events.readable()=>{signal_guard=Some(ready.map_err(|_|"cli_signal_poll")?);},
            _=io.writer.changed()=>{},
            _=interval.tick()=>{},
        }
        while let Some(signal) = io.signals.take().map_err(|_| "cli_signal_read")? {
            dirty = true;
            use ansa_linux::terminal::*;
            match signal {
                SIGTERM | SIGHUP => {
                    state.exit = 128 + signal;
                    finished = true;
                    force_drain = true;
                    handle.request_shutdown();
                }
                SIGINT => {
                    force_drain |= actor_done || finished || state.cancelled;
                    if prompt.is_some() || jsonl_mode {
                        state.exit = 130;
                    }
                    if !state.cancelled && state.awaiting() {
                        state.cancelled = true;
                        state.deferred_cancel = true;
                        state.post_read(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                    } else if !state.cancelled
                        && state.snapshot.as_ref().is_some_and(|v| {
                            v.request.is_some()
                                && !matches!(v.state.as_str(), "completed" | "cancelled" | "idle")
                        })
                    {
                        let target = state
                            .snapshot
                            .as_ref()
                            .and_then(|v| v.request.clone())
                            .expect("request");
                        state.cancelled = true;
                        state.post(&handle, CommandPayload::Cancel(target), &mut replies);
                    } else {
                        finished = true;
                        handle.request_shutdown();
                    }
                }
                SIGTSTP => {
                    io.suspend().await?;
                    last_frame = None;
                    io.writer.invalidate();
                }
                SIGCONT => {
                    io.resume_terminal()?;
                    last_frame = None;
                    io.writer.invalidate();
                }
                SIGWINCH => {
                    io.resize();
                    last_frame = None;
                }
                _ => {}
            }
        }
        let ready = state.snapshot.is_some();
        if !jsonl_mode && ready && !closed && !finished && !actor_done {
            // Late tool outcomes need not publish a new snapshot. Refresh the
            // terminal state until cleanup/storage has actually settled, so
            // timers and admission cannot stay stuck on a stale busy flag.
            if state.snapshot.as_ref().is_some_and(|v| {
                v.worker_busy && matches!(v.state.as_str(), "cancelled" | "paused" | "completed")
            }) && Instant::now() >= next_settled
            {
                state.post_read(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                next_settled = Instant::now() + Duration::from_millis(40);
            }
            if resync {
                state.post_read(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                resync = false;
            }
            if io.raw.is_some() && Instant::now() >= next_history {
                state.post_read(
                    &handle,
                    CommandPayload::GetTranscript(state.query.clone()),
                    &mut replies,
                );
                next_history = Instant::now() + Duration::from_millis(500);
            }
            if state.deferred_cancel {
                if !state.awaiting() || state.snapshot.as_ref().is_some_and(|v| v.storage_failed) {
                    if let Some(target) = state.snapshot.as_ref().and_then(|v| v.request.clone()) {
                        state.post(&handle, CommandPayload::Cancel(target), &mut replies);
                    }
                    state.deferred_cancel = false;
                } else if Instant::now() >= next_cancel {
                    state.post_read(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                    next_cancel = Instant::now() + Duration::from_millis(40);
                }
            }
            if io.raw.is_some() {
                state.recall_page(&handle, &mut replies);
                state.copy_page(&handle, &mut replies, io);
            }
            if let Some(prompt) = prompt {
                if !sent_prompt {
                    let (_, exit) = state.text(prompt, &handle, io, &mut replies);
                    prompt_id = state.pending.clone();
                    sent_prompt = prompt_id.is_some();
                    finished |= exit;
                    if !sent_prompt {
                        state.exit = 1;
                        finished = true;
                    }
                }
                if sent_prompt
                    && prompt_id.is_some()
                    && state.snapshot.as_ref().is_some_and(|v| {
                        v.submit == prompt_id
                            && matches!(v.state.as_str(), "completed" | "cancelled" | "paused")
                    })
                {
                    if state.exit != 130 {
                        state.exit = if state
                            .snapshot
                            .as_ref()
                            .is_some_and(|v| v.state == "completed")
                        {
                            0
                        } else {
                            1
                        };
                    }
                    finished = true;
                    handle.request_shutdown();
                }
            }
        }
        if !eof
            && !finished
            && (!jsonl_mode || replies.is_empty())
            && state.pending_draft.is_none()
            && prompt.is_none()
            && (jsonl_mode
                || (ready
                    && (io.raw.is_none()
                        || (state.history_ready
                            && first_frame.is_some_and(|v| io.writer.displayed(v))))))
            && (!deferred_input.is_empty()
                || io.input.ready(false).map_err(|_| "cli_input_poll")?)
        {
            dirty = true;
            let mut bytes = [0; 4096];
            let read = if deferred_input.is_empty() {
                io.input.read(&mut bytes)
            } else {
                let size = deferred_input.len();
                for dest in &mut bytes[..size] {
                    *dest = deferred_input.pop_front().expect("size");
                }
                Ok(size)
            };
            match read {
                Ok(0) => {
                    if state.editor.overflow {
                        if jsonl_mode {
                            io.status(&(jsonl::error("command_line_limit", None).dump() + "\n"));
                        } else {
                            state.report(io, "ansa: submit_size\n");
                            state.exit = 1;
                        }
                    } else if !state.editor.bytes.is_empty() {
                        if jsonl_mode {
                            post_line(&state.editor.bytes, &handle, io, &mut replies);
                        } else {
                            let text = state.editor.text()?.to_owned();
                            let (_, exit) = state.text(&text, &handle, io, &mut replies);
                            finished |= exit;
                        }
                    }
                    state.editor.clear();
                    state.input_history.reset();
                    eof = true;
                }
                Ok(size) => {
                    for (index, &byte) in bytes[..size].iter().enumerate() {
                        // JSONL has a distinct 128KiB limit, retaining only bounded bytes.
                        if jsonl_mode {
                            if byte == b'\n' {
                                if state.editor.overflow {
                                    io.status(
                                        &(jsonl::error("command_line_limit", None).dump() + "\n"),
                                    );
                                } else if !state.editor.bytes.is_empty() {
                                    post_line(&state.editor.bytes, &handle, io, &mut replies);
                                }
                                state.editor.clear();
                                if !replies.is_empty() {
                                    deferred_input.extend(&bytes[index + 1..size]);
                                    break;
                                }
                            } else if state.editor.bytes.len() < 128 * 1024 {
                                state.editor.bytes.push(byte);
                            } else {
                                state.editor.overflow = true;
                            }
                            continue;
                        }
                        let before = (
                            state.editor.bytes.len(),
                            state.editor.cursor,
                            state.editor.overflow,
                        );
                        let action = state.editor.feed(byte, io.raw.is_some());
                        if before
                            != (
                                state.editor.bytes.len(),
                                state.editor.cursor,
                                state.editor.overflow,
                            )
                        {
                            state.input_history.edited(&state.editor);
                        }
                        if let Some(action) = action {
                            match action {
                                Action::Overflow => {
                                    state.report(io, "ansa: submit_size\n");
                                    state.exit = 1;
                                    state.editor.clear();
                                    state.input_history.reset();
                                }
                                Action::Submit | Action::QueueSubmit => {
                                    // A delayed recall may not replace a draft rejected as busy.
                                    state.input_history.edited(&state.editor);
                                    match state.editor.text().map(str::to_owned) {
                                        Ok(text) => {
                                            let (accepted, exit) =
                                                state.text(&text, &handle, io, &mut replies);
                                            if accepted || io.raw.is_none() {
                                                state.input_history.reset();
                                                state.editor.clear();
                                                state.scroll = 0;
                                                state.follow_tail = true;
                                                state.reading = None;
                                                state.query = GetTranscript {
                                                    before: 0,
                                                    limit: 32,
                                                    forward: false,
                                                };
                                            }
                                            finished |= exit;
                                        }
                                        Err(code) => {
                                            state.report(io, &format!("ansa: {code}\n"));
                                            state.exit = 1;
                                            state.editor.clear();
                                            state.input_history.reset();
                                        }
                                    }
                                }
                                Action::ToggleMode => {
                                    if state.available() {
                                        state.mode = if state.mode == AgentMode::Basic {
                                            AgentMode::Plan
                                        } else {
                                            AgentMode::Basic
                                        };
                                    }
                                }
                                Action::ToggleHistory => {
                                    state.expanded = true;
                                    state.follow_tail = true;
                                    state.reading = None;
                                    state.scroll = 0;
                                    state.query = GetTranscript {
                                        before: 0,
                                        limit: 32,
                                        forward: false,
                                    };
                                    next_history = Instant::now();
                                }
                                Action::HistoryOlder | Action::HistoryNewer => {
                                    let total =
                                        state.snapshot.as_ref().map_or(0, |s| s.transcript_size);
                                    state.input_history.navigate(
                                        action == Action::HistoryOlder,
                                        &mut state.editor,
                                        total,
                                    );
                                }
                                Action::CopyHistory => state.start_copy(io),
                                Action::FocusNext | Action::FocusPrevious | Action::Mouse(_) => {}
                                Action::Escape => {
                                    state.follow_tail = !state.follow_tail;
                                    state.expanded = true;
                                    state.reading = None;
                                    if state.follow_tail {
                                        state.scroll = 0;
                                    } else {
                                        state.reading_total = state.page.2;
                                    }
                                }
                                Action::ToggleTools => {
                                    state.tools_expanded = !state.tools_expanded;
                                    state.reading = None;
                                }
                                Action::PageUp => {
                                    if state.page.1 == 0 {
                                        continue;
                                    }
                                    if state.follow_tail {
                                        state.reading_total = state
                                            .snapshot
                                            .as_ref()
                                            .map_or(0, |v| v.transcript_size);
                                    }
                                    state.follow_tail = false;
                                    if state.query.before == 0 {
                                        state.query.before = state.page.1;
                                    }
                                    if state.expanded {
                                        if state.scroll >= state.scroll_limit && state.page.0 > 0 {
                                            state.query = GetTranscript {
                                                before: state.page.0,
                                                limit: 32,
                                                forward: false,
                                            };
                                            state.scroll = 0;
                                            next_history = Instant::now();
                                        } else {
                                            state.scroll =
                                                (state.scroll + 8).min(state.scroll_limit);
                                        }
                                    }
                                }
                                Action::PageDown => {
                                    if state.expanded
                                        && state.scroll == 0
                                        && state.page.1 < state.page.2
                                    {
                                        state.query = GetTranscript {
                                            before: state.page.1,
                                            limit: 32,
                                            forward: true,
                                        };
                                        next_history = Instant::now();
                                    } else {
                                        state.scroll = state.scroll.saturating_sub(8);
                                        if state.scroll == 0 && state.page.1 == state.page.2 {
                                            state.follow_tail = true;
                                            state.reading = None;
                                            state.query = GetTranscript {
                                                before: 0,
                                                limit: 32,
                                                forward: false,
                                            };
                                        }
                                    }
                                }
                                Action::Eof => eof = true,
                            }
                        }
                        if finished || eof {
                            break;
                        }
                        if state.pending_draft.is_some() {
                            deferred_input.extend(&bytes[index + 1..size]);
                            break;
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err("cli_input_read"),
            }
        }
        if io.raw.is_some() && state.explicit_status && state.wait_clock.tick(Instant::now()) {
            dirty = true;
        }
        if io.raw.is_some() && state.progress.tick(Instant::now()) {
            dirty = true;
        }
        if io.raw.is_some() && dirty {
            if let Some(frame) = state.render(io, context)
                && last_frame.as_ref() != Some(&frame)
            {
                let sequence = io.writer.frame(frame.clone());
                first_frame.get_or_insert(sequence);
                last_frame = Some(frame);
            }
            dirty = false;
        }
        // Preserve cached readiness while bytes remain, including deferred input.
        if let Some(mut guard) = input_guard
            && !io.input.ready(false).map_err(|_| "cli_input_poll")?
        {
            guard.clear_ready();
        }
        if let Some(mut guard) = signal_guard
            && !signal_events
                .get_ref()
                .ready(false)
                .map_err(|_| "cli_signal_poll")?
        {
            guard.clear_ready();
        }
        if eof && !closed && !state.deferred_cancel && replies.is_empty() {
            handle.close_input();
            closed = true;
        }
        // A perpetually readable TTY must yield to post tasks, timers and the
        // reactor; AsyncFd::readable itself does not consume Tokio's budget.
        tokio::task::yield_now().await;
    }
    handle.request_shutdown();
    while replies.join_next().await.is_some() {}
    runtime.join().await?;
    let reporting_ok = io.stop_reporting().is_ok();
    if io.raw.is_some() {
        io.writer.text("\x1b[1B\r\n".into());
    }
    if state.exit < 128
        && state
            .snapshot
            .as_ref()
            .is_some_and(|v| v.state == "paused" || v.storage_failed)
    {
        state.exit = 1;
    }
    if !io.writer.finish() || !reporting_ok {
        let code = output_error.unwrap_or("cli_output");
        if jsonl_mode {
            io.status(&(jsonl::error(code, None).dump() + "\n"));
        } else {
            io.status(&format!("ansa: {}\n", diagnostics::error(code)));
        }
        if state.exit == 0 {
            state.exit = 1;
        }
    }
    Ok(state.exit)
}
fn post_line(bytes: &[u8], handle: &Handle, io: &Io, replies: &mut Replies) {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| "json_encoding")
        .and_then(jsonl::decode);
    match value {
        Ok(command) => {
            let id = command.command.clone();
            let correlated =
                Some(id.0.as_str()).filter(|id| crate::provider::contracts::valid_id(id));
            let validation = validate_command(&command).map_err(|code| {
                if code == "request_id" {
                    if matches!(command.payload, CommandPayload::Cancel(_)) {
                        "cancel_target"
                    } else {
                        "resume_target"
                    }
                } else {
                    code
                }
            });
            if let Err(code) = validation {
                io.status(&(jsonl::error(code, correlated).dump() + "\n"));
            } else if replies.len() >= 64 {
                io.status(&(jsonl::error("command_queue_full", correlated).dump() + "\n"));
            } else {
                let h = handle.clone();
                replies.spawn(async move { (id, false, false, h.post(command).await) });
            }
        }
        Err(code) => io.status(&(jsonl::error(code, None).dump() + "\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn simultaneous_resync_settle_and_cancel_share_one_snapshot_mailbox() {
        let mut runtime = super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let handle = runtime.handle();
        let mut state = State::new(&handle);
        let mut replies = Replies::new();
        // All three housekeeping paths run before the owner can receive any
        // request. Its snapshot mailbox has capacity one.
        for accepted in [true, false, false] {
            assert_eq!(
                state.post_read(&handle, CommandPayload::GetSnapshot(0), &mut replies),
                accepted
            );
        }
        assert_eq!(replies.len(), 1);
        let (_, _, internal, result) = replies.join_next().await.unwrap().unwrap();
        assert!(internal);
        assert_eq!(result.unwrap(), PostStatus::Accepted);
        assert_eq!(state.exit, 0);
        runtime.shutdown().await.unwrap();
    }
}
