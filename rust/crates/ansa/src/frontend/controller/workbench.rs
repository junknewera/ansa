//! Interactive workspace. Each open conversation retains its own runtime owner,
//! draft, cursor, transcript page and bounded reply tasks.
use super::*;
use crate::{
    auth::Cancel,
    frontend::{
        carbon::{Choice, Focus, Inspector, InspectorKind, Popup, Selection, Workspace},
        input::{MouseEvent, MouseKind},
        sessions::{Created, Factory, Kind},
        workspace::{DiffStatus, EditorCommand, FileDocument, FileHost, FileListing},
    },
    protocol::json::Json,
    session::catalog::CatalogEntry,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

const MAX_SESSIONS: usize = 16;
const MAX_BACKGROUND: usize = 4;
const MAX_JOBS: usize = 4;

struct Slot {
    id: String,
    kind: Kind,
    parent: Option<String>,
    runtime: Runtime,
    handle: Handle,
    state: State,
    replies: Replies,
    timeline: Option<TranscriptPage>,
    timeline_query: GetTranscript,
    refresh: Instant,
    refresh_transcript: bool,
    settled: Instant,
    resync: bool,
    pending_prompt: Option<String>,
    auto_return: bool,
    returned: Option<RequestId>,
    admissions: Vec<(String, bool)>,
    closed: bool,
    closing: bool,
    ready: bool,
    ready_refresh: bool,
    ready_snapshot: Option<CommandId>,
    startup_cancelled: bool,
    jump_top: bool,
    tool_expanded: std::collections::BTreeSet<String>,
    tool_anchor_limit: Option<usize>,
}
impl Slot {
    fn new(
        runtime: Runtime,
        id: String,
        kind: Kind,
        parent: Option<String>,
        prompt: Option<String>,
        auto_return: bool,
    ) -> Self {
        let handle = runtime.handle();
        let mut state = State::new(&handle);
        if kind == Kind::Btw {
            state.mode = AgentMode::Plan;
            state.mode_ready = true;
        }
        Self {
            id,
            kind,
            parent,
            runtime,
            handle,
            state,
            replies: Replies::new(),
            timeline: None,
            timeline_query: GetTranscript {
                before: 0,
                limit: 64,
                forward: false,
            },
            refresh: Instant::now(),
            refresh_transcript: true,
            settled: Instant::now(),
            resync: true,
            pending_prompt: prompt,
            auto_return,
            returned: None,
            admissions: Vec::new(),
            closed: false,
            closing: false,
            ready: false,
            ready_refresh: true,
            ready_snapshot: None,
            startup_cancelled: false,
            jump_top: false,
            tool_expanded: Default::default(),
            tool_anchor_limit: None,
        }
    }
    fn jump_history(&mut self, top: bool) {
        self.state.reading = None;
        self.jump_top = top;
        if !top {
            self.state.scroll = 0;
        }
        self.state.follow_tail = !top;
        self.timeline_query = GetTranscript {
            before: 0,
            limit: 64,
            forward: top,
        };
        self.refresh = Instant::now();
    }
    fn cancel_history_jump(&mut self) {
        if std::mem::take(&mut self.jump_top)
            && let Some(page) = &self.timeline
        {
            self.timeline_query = GetTranscript {
                before: page.end,
                limit: 64,
                forward: false,
            };
        }
    }
    fn received_timeline(&mut self, page: &TranscriptPage) {
        if self
            .timeline
            .as_ref()
            .is_none_or(|old| (old.begin, old.end) != (page.begin, page.end))
            || self.jump_top
        {
            self.state.reading = None;
        }
        if self.jump_top
            && !self.state.follow_tail
            && page.query == self.timeline_query
            && page.query.forward
        {
            self.state.scroll = usize::MAX;
        }
        self.jump_top = false;
        self.timeline = Some(page.clone());
    }
    fn active(&self) -> bool {
        !self.ready
            || self.ready_refresh
            || self.pending_prompt.is_some()
            || self.state.awaiting()
            || self.state.snapshot.as_ref().is_none_or(|s| {
                s.worker_busy
                    || !matches!(
                        s.state.as_str(),
                        "idle" | "completed" | "cancelled" | "paused"
                    )
            })
    }
    fn input_ready(&self) -> bool {
        self.ready
            && !self.ready_refresh
            && self.state.history_ready
            && self.state.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.state != "loading"
                    && !(snapshot.state == "idle" && snapshot.worker_busy)
                    && self.state.page.2 >= snapshot.transcript_size
                    && self
                        .timeline
                        .as_ref()
                        .is_some_and(|page| page.total >= snapshot.timeline_size)
            })
    }
    fn became_ready(&mut self) {
        self.ready = true;
        self.ready_refresh = true;
        self.ready_snapshot = None;
        self.resync = true;
        self.refresh = Instant::now();
        self.settled = Instant::now();
    }
    fn accepts_snapshot(&mut self, snapshot: &Snapshot) -> bool {
        if !self.ready || self.startup_cancelled || snapshot.state == "loading" {
            return false;
        }
        if self.ready_refresh {
            if snapshot.command.is_none() || snapshot.command != self.ready_snapshot {
                return false;
            }
            self.ready_refresh = false;
            self.ready_snapshot = None;
        }
        true
    }
    fn poll_replies(&mut self, io: &Io) -> Result<bool> {
        let mut dirty = false;
        while let Some(reply) = self.replies.try_join_next() {
            dirty = true;
            let (id, submit, internal, result) = reply.map_err(|_| "cli_post_task")?;
            if self.state.snapshot_read.as_ref() == Some(&id) {
                self.state.snapshot_read = None;
            }
            let retry = internal && matches!(&result, Ok(PostStatus::QueueFull));
            if retry {
                if self.ready_snapshot.as_ref() == Some(&id) {
                    self.ready_snapshot = None;
                    self.resync = true;
                }
                self.state.input_history.retry(Some(&id));
                self.state.copy.retry(Some(&id));
                self.refresh = Instant::now();
                continue;
            }
            let code = match result {
                Ok(PostStatus::Accepted | PostStatus::Duplicate) => None,
                Ok(PostStatus::Closed) if internal && (self.closed || self.closing) => None,
                Ok(PostStatus::QueueFull) => Some("command_queue_full"),
                Ok(PostStatus::Closed) => Some("runtime_closed"),
                Ok(PostStatus::StaleTarget) => Some("stale_cancel_target"),
                Ok(PostStatus::StaleIncarnation) => Some("stale_incarnation"),
                Ok(PostStatus::CommandIdLimit) => Some("submit_id_limit"),
                Ok(PostStatus::CommandConflict) => Some("command_id_conflict"),
                Err(code) => Some(code),
            };
            if !internal {
                self.admissions.push((id.0.clone(), code.is_none()));
            }
            if self
                .state
                .pending_draft
                .as_ref()
                .is_some_and(|(pending, _, _)| *pending == id)
            {
                let (_, bytes, cursor) = self
                    .state
                    .pending_draft
                    .take()
                    .expect("checked pending draft");
                if code.is_some() {
                    self.state.editor.bytes = bytes;
                    self.state.editor.cursor = cursor;
                }
            }
            if let Some(code) = code {
                self.state.exit = 1;
                self.state.input_history.retry(Some(&id));
                self.state.copy.rejected(&id);
                self.state.progress.rejected(&id);
                if submit && self.state.pending.as_ref() == Some(&id) {
                    self.state.pending = None;
                }
                self.state
                    .report(io, &format!("ansa: {}", diagnostics::error(code)));
            }
        }
        Ok(dirty)
    }
    fn refresh(&mut self, io: &Io, now: Instant) {
        if self.closed || self.closing || !self.ready {
            return;
        }
        if self.resync
            && self.state.post_read(
                &self.handle,
                CommandPayload::GetSnapshot(0),
                &mut self.replies,
            )
        {
            if self.ready_refresh {
                self.ready_snapshot = self.state.snapshot_read.clone();
            }
            self.resync = false;
        }
        if self.state.snapshot.as_ref().is_some_and(|s| {
            s.worker_busy
                && matches!(
                    s.state.as_str(),
                    "idle" | "completed" | "cancelled" | "paused"
                )
        }) && now >= self.settled
        {
            self.state.post_read(
                &self.handle,
                CommandPayload::GetSnapshot(0),
                &mut self.replies,
            );
            self.settled = now + Duration::from_millis(40);
        }
        let transcript_stale = !self.state.history_ready
            || self
                .state
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| self.state.page.2 < snapshot.transcript_size);
        let timeline_stale = self.state.snapshot.as_ref().is_some_and(|snapshot| {
            self.timeline.as_ref().is_none_or(|page| {
                page.total < snapshot.timeline_size || page.query != self.timeline_query
            })
        });
        if (now >= self.refresh || transcript_stale || timeline_stale)
            && self.state.snapshot.is_some()
        {
            // Both page kinds share one bounded actor mailbox. Issue one read
            // at a time instead of deterministically starving one with LIFO
            // post tasks; user recall/copy collisions remain retryable reads.
            let transcript = if !self.state.history_ready {
                true
            } else if transcript_stale != timeline_stale {
                transcript_stale
            } else {
                self.refresh_transcript
            };
            let payload = if transcript {
                CommandPayload::GetTranscript(self.state.query.clone())
            } else {
                CommandPayload::GetTimeline(self.timeline_query.clone())
            };
            self.state
                .post_read(&self.handle, payload, &mut self.replies);
            self.refresh_transcript = !transcript;
            self.refresh = now + Duration::from_millis(500);
        }
        if self.state.deferred_cancel {
            if !self.state.awaiting()
                || self
                    .state
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.storage_failed)
            {
                if let Some(target) = self.state.snapshot.as_ref().and_then(|s| s.request.clone()) {
                    self.state.post(
                        &self.handle,
                        CommandPayload::Cancel(target),
                        &mut self.replies,
                    );
                    self.state.deferred_cancel = false;
                }
            } else if now >= self.settled {
                self.state.post_read(
                    &self.handle,
                    CommandPayload::GetSnapshot(0),
                    &mut self.replies,
                );
                self.settled = now + Duration::from_millis(40);
            }
        }
        self.state.recall_page(&self.handle, &mut self.replies);
        self.state.copy_page(&self.handle, &mut self.replies, io);
        if self.input_ready()
            && self.state.available()
            && let Some(prompt) = self.pending_prompt.take()
        {
            let (accepted, _) = self
                .state
                .text(&prompt, &self.handle, io, &mut self.replies);
            if !accepted {
                self.state.editor.bytes = prompt.into_bytes();
                self.state.editor.cursor = self.state.editor.bytes.len();
            }
        }
    }
    fn cancel(&mut self, io: &Io) {
        if self.state.awaiting() {
            self.state.deferred_cancel = true;
            self.state.post_read(
                &self.handle,
                CommandPayload::GetSnapshot(0),
                &mut self.replies,
            );
        } else if self.active() {
            if let Some(target) = self.state.snapshot.as_ref().and_then(|s| s.request.clone()) {
                self.state.post(
                    &self.handle,
                    CommandPayload::Cancel(target),
                    &mut self.replies,
                );
            }
        } else {
            self.state.report(io, "Нет активного шага");
        }
    }
    fn projected(&self) -> Option<Vec<Entry>> {
        let page = self.timeline.as_ref()?;
        let view = self.state.snapshot.as_ref()?;
        let now = Instant::now();
        let mut entries = super::super::tool_ui::project(
            &page.entries,
            view,
            &self.tool_expanded,
            self.state.tools_expanded,
            page.end == page.total,
            |operation| self.state.progress.tool_elapsed(operation, now),
        );
        if page.end == page.total {
            for message in view
                .followups
                .iter()
                .filter(|message| message.state == "queued")
            {
                entries.push(Entry {
                    action: None,
                    role: "user".into(),
                    text: format!("{}\n(ожидает текущий шаг)", message.text),
                });
            }
            if let Some(preview) = &self.state.preview
                && !preview.text.is_empty()
                && preview.state != "committed"
                && self.state.preview_request == view.request
            {
                entries.push(Entry {
                    action: None,
                    role: "assistant".into(),
                    text: format!(
                        "{}\n({})",
                        preview.text,
                        if preview.state == "discarded" {
                            "незавершённый ответ"
                        } else {
                            "предварительный ответ"
                        }
                    ),
                });
            }
        }
        Some(entries)
    }
}

struct CreateIntent {
    source: String,
    kind: Kind,
    prompt: Option<String>,
}
enum FileValue {
    Listing(FileListing),
    Document(FileDocument),
}
enum Job {
    Created {
        source: String,
        prompt: Option<String>,
        auto_return: bool,
        result: Result<Created>,
    },
    Sessions {
        owner: String,
        token: u64,
        result: Result<Vec<CatalogEntry>>,
    },
    Models {
        owner: String,
        token: u64,
        effort: bool,
        result: Result<Json>,
    },
    File {
        token: u64,
        result: Result<FileValue>,
    },
    Preview {
        token: u64,
        width: usize,
        rows: Option<PreviewRows>,
    },
    EditorReady {
        owner: String,
        token: u64,
        request: u64,
        result: Result<EditorCommand>,
    },
}
enum Menu {
    Loading(String),
    Sessions {
        entries: Vec<CatalogEntry>,
        selected: usize,
    },
    Models {
        models: Vec<(String, String, Vec<String>)>,
        selected: usize,
    },
    Efforts {
        values: Vec<Option<String>>,
        selected: usize,
    },
    Help,
}
type PreviewRows = std::sync::Arc<Vec<super::super::markdown::Row>>;
struct PreviewJob {
    token: u64,
    width: usize,
    stop: Cancel,
}
struct Files {
    token: u64,
    listing: Option<FileListing>,
    document: Option<FileDocument>,
    selected: usize,
    tab: usize,
    offset: usize,
    loading: bool,
    jump: Option<String>,
    preview_cache: Option<(usize, PreviewRows)>,
    preview_width: Option<usize>,
    preview_end: bool,
}
impl Files {
    fn preview_rows(&self) -> Option<&PreviewRows> {
        self.preview_cache
            .as_ref()
            .filter(|(width, _)| self.preview_width == Some(*width))
            .map(|(_, rows)| rows)
    }
    fn accept_preview(&mut self, token: u64, width: usize, rows: Option<PreviewRows>) -> bool {
        if self.token != token || self.preview_width != Some(width) {
            return false;
        }
        let Some(rows) = rows else {
            return false;
        };
        if self.tab == 2
            && let Some(anchor) = self
                .preview_cache
                .as_ref()
                .and_then(|(_, old)| old.get(self.offset))
                .map(|row| row.anchor)
        {
            self.offset = super::super::markdown::Row::locate(&rows, anchor);
        }
        if self.tab == 2 && self.preview_end {
            self.offset = rows.len().saturating_sub(1);
        }
        self.preview_end = false;
        self.preview_cache = Some((width, rows));
        true
    }
    fn last_line(&self) -> usize {
        if self.tab == 2 {
            // Navigation keeps using the last completed layout while a resize
            // worker computes its replacement; only display requires the width.
            return self
                .preview_cache
                .as_ref()
                .map_or(0, |(_, rows)| rows.len().saturating_sub(1));
        }
        self.document.as_ref().map_or(0, |document| match self.tab {
            1 => document.diff.text.lines().count().saturating_sub(1),
            _ => document.source.lines().count().saturating_sub(1),
        })
    }
}
enum Panel {
    Files(Box<Files>),
    Agents { selected: usize },
}
struct Workbench {
    factory: Factory,
    files: FileHost,
    slots: Vec<Slot>,
    current: usize,
    root_id: String,
    pumps: JoinSet<()>,
    events: mpsc::Receiver<(String, SessionEvent)>,
    sender: mpsc::Sender<(String, SessionEvent)>,
    jobs: JoinSet<Job>,
    stop: Cancel,
    pending_context: BTreeMap<String, CreateIntent>,
    pending_results: BTreeMap<String, (String, Option<RequestId>)>,
    menu: Option<Menu>,
    panel: Option<Panel>,
    focus: Focus,
    selection: Option<Selection>,
    tool_press: Option<(usize, usize, String)>,
    frames: std::collections::VecDeque<(u64, carbon::Frame)>,
    displayed: Option<carbon::Frame>,
    operational_frame: Option<u64>,
    last_frame: Option<carbon::Frame>,
    file_token: u64,
    preview_job: Option<PreviewJob>,
    editor: Option<super::super::terminal::EditorJob>,
    pending_editor: Option<EditorCommand>,
    editor_preparing: bool,
    editor_request: u64,
    editor_wait: Option<EditorWait>,
    modal_token: u64,
    modal_stop: Option<Cancel>,
    input_closed: bool,
    terminating: bool,
}
struct EditorWait {
    began: Instant,
    progress: Instant,
    written: u64,
}
enum SessionEvent {
    Ready(Result<()>),
    Runtime(RuntimeEvent),
    Closed,
}

async fn pump_session(handle: Handle, id: String, sender: mpsc::Sender<(String, SessionEvent)>) {
    let ready_handle = handle.clone();
    let ready = ready_handle.ready();
    tokio::pin!(ready);
    let mut announced = false;
    loop {
        let event = tokio::select! { biased;
            result = &mut ready, if !announced => { announced = true; SessionEvent::Ready(result) }
            event = handle.next_event() => match event {
                Some(event) => SessionEvent::Runtime(event),
                None => {
                    if !announced && sender.send((id.clone(), SessionEvent::Ready(ready.as_mut().await))).await.is_err() { return; }
                    SessionEvent::Closed
                }
            }
        };
        let closed = matches!(event, SessionEvent::Closed);
        if sender.send((id.clone(), event)).await.is_err() || closed {
            return;
        }
    }
}
impl Workbench {
    fn new(
        runtime: Runtime,
        factory: Factory,
        root_id: String,
        kind: Kind,
        parent: Option<String>,
    ) -> Result<Self> {
        let files = FileHost::new(&factory.settings, &factory.auth_path)?;
        let (sender, events) = mpsc::channel(64);
        let mut value = Self {
            factory,
            files,
            slots: Vec::new(),
            current: 0,
            root_id: root_id.clone(),
            pumps: JoinSet::new(),
            events,
            sender,
            jobs: JoinSet::new(),
            stop: Cancel::default(),
            pending_context: BTreeMap::new(),
            pending_results: BTreeMap::new(),
            menu: None,
            panel: None,
            focus: Focus::Chat,
            selection: None,
            tool_press: None,
            frames: std::collections::VecDeque::new(),
            displayed: None,
            operational_frame: None,
            last_frame: None,
            file_token: 0,
            preview_job: None,
            editor: None,
            pending_editor: None,
            editor_preparing: false,
            editor_request: 0,
            editor_wait: None,
            modal_token: 0,
            modal_stop: None,
            input_closed: false,
            terminating: false,
        };
        value.add(Slot::new(runtime, root_id, kind, parent, None, false));
        Ok(value)
    }
    fn add(&mut self, slot: Slot) {
        let sender = self.sender.clone();
        let handle = slot.handle.clone();
        let id = slot.id.clone();
        self.pumps.spawn(pump_session(handle, id, sender));
        self.slots.push(slot);
    }
    fn report(&mut self, io: &Io, message: &str) {
        self.slots[self.current].state.report(io, message);
    }
    fn cancel_current(&mut self, io: &Io) {
        let index = self.current;
        if !self.slots[index].ready
            || self.slots[index].ready_refresh
            || (self.operational_frame.is_none()
                && self.slots[index]
                    .state
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.request.is_none()))
        {
            let slot = &mut self.slots[index];
            slot.startup_cancelled = true;
            slot.closing = true;
            slot.pending_prompt = None;
            slot.handle.request_shutdown();
            slot.state.report(io, "Открытие сессии отменено");
            let parent = slot.parent.clone();
            let id = slot.id.clone();
            self.discard_contexts(&id);
            if let Some(parent) = parent.and_then(|parent| {
                self.slots
                    .iter()
                    .position(|slot| slot.id == parent && slot.ready)
            }) {
                self.switch(parent);
            } else {
                self.input_closed = true;
                self.cancel_modal();
                self.cancel_editor_request();
            }
        } else {
            self.slots[index].cancel(io);
        }
    }
    fn switch(&mut self, index: usize) {
        self.cancel_modal();
        self.cancel_editor_request();
        self.current = index;
        self.slots[index].refresh = Instant::now();
        self.slots[index].resync = true;
        self.menu = None;
        self.panel = None;
        self.focus = Focus::Chat;
        self.selection = None;
        self.last_frame = None;
        self.frames.clear();
        self.displayed = None;
        self.operational_frame = None;
    }
    fn can_job(&mut self, io: &Io) -> bool {
        if self.jobs.len() + self.pending_context.len() >= MAX_JOBS {
            self.report(io, "Дождитесь текущего открытия");
            false
        } else {
            true
        }
    }
    fn create(&mut self, kind: Kind, prompt: Option<String>, io: &Io) {
        if self.slots.len() + self.jobs.len() + self.pending_context.len() >= MAX_SESSIONS {
            self.report(io, "Достигнут предел открытых сессий (16)");
            return;
        }
        if matches!(kind, Kind::Agent | Kind::Btw)
            && self
                .slots
                .iter()
                .filter(|s| matches!(s.kind, Kind::Agent | Kind::Btw) && s.active())
                .count()
                + self
                    .pending_context
                    .values()
                    .filter(|p| matches!(p.kind, Kind::Agent | Kind::Btw))
                    .count()
                + self.jobs.len()
                >= MAX_BACKGROUND
        {
            self.report(io, "Одновременно доступны четыре фоновых разговора");
            return;
        }
        if !self.can_job(io) {
            return;
        }
        if kind == Kind::Console {
            if self.slots[self.current].active() {
                self.report(
                    io,
                    "Командная панель доступна после остановки текущей задачи",
                );
                return;
            }
            if let Some(index) = self.slots.iter().position(|s| {
                s.kind == Kind::Console
                    && s.parent.as_deref() == Some(self.slots[self.current].id.as_str())
                    && !s.active()
            }) {
                self.switch(index);
                self.slots[index].pending_prompt = prompt;
                return;
            }
            let source = self.slots[self.current].id.clone();
            let factory = self.factory.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                result: factory.create_console(&source),
                source,
                prompt,
                auto_return: false,
            });
            return;
        }
        let slot = &mut self.slots[self.current];
        let command = slot.state.command(&slot.handle, CommandPayload::GetContext);
        let id = command.command.clone();
        let key = id.0.clone();
        let handle = slot.handle.clone();
        slot.replies
            .spawn(async move { (id, false, false, handle.post(command).await) });
        self.pending_context.insert(
            key,
            CreateIntent {
                source: slot.id.clone(),
                kind,
                prompt,
            },
        );
        self.report(io, "Создаю сессию");
    }
    fn open_file(&mut self, path: PathBuf, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        self.file_token = self.file_token.wrapping_add(1);
        let token = self.file_token;
        self.panel = Some(Panel::Files(Box::new(Files {
            token,
            listing: None,
            document: None,
            selected: 0,
            tab: 0,
            offset: 0,
            loading: true,
            jump: None,
            preview_cache: None,
            preview_width: None,
            preview_end: false,
        })));
        self.focus = Focus::Inspector;
        let host = self.files.clone();
        let stop = self.stop.clone();
        self.jobs.spawn_blocking(move || {
            let result = match host.list(&path, &stop) {
                Ok(listing) => Ok(FileValue::Listing(listing)),
                Err("workspace_directory") => host.open(&path, &stop).map(FileValue::Document),
                Err(code) => Err(code),
            };
            Job::File { token, result }
        });
    }
    fn prepare_editor(&mut self, io: &Io) {
        if self.input_closed || self.terminating {
            return;
        }
        if self.editor_preparing || self.pending_editor.is_some() || self.editor.is_some() {
            return;
        }
        if self.slots.iter().any(Slot::active)
            || !self.pending_context.is_empty()
            || !self.pending_results.is_empty()
        {
            self.report(io, "Редактор доступен после остановки текущих задач");
            return;
        }
        let Some(Panel::Files(files)) = &self.panel else {
            return;
        };
        let Some(document) = &files.document else {
            return;
        };
        let path = document.path.clone();
        let token = files.token;
        let owner = self.slots[self.current].id.clone();
        if !self.can_job(io) {
            return;
        }
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "vi".into());
        let host = self.files.clone();
        self.editor_request = self.editor_request.wrapping_add(1);
        let request = self.editor_request;
        self.editor_wait = Some(EditorWait {
            began: Instant::now(),
            progress: Instant::now(),
            written: io.writer.written(),
        });
        self.editor_preparing = true;
        self.jobs.spawn_blocking(move || Job::EditorReady {
            owner,
            token,
            request,
            result: host
                .validate_file(&path)
                .and_then(|path| EditorCommand::parse(&editor, &path)),
        });
    }
    fn cancel_editor_request(&mut self) {
        self.editor_request = self.editor_request.wrapping_add(1);
        self.editor_preparing = false;
        self.pending_editor = None;
        self.editor_wait = None;
    }
    fn eof(&mut self, io: &Io, flush_draft: bool) -> bool {
        let mut finished = false;
        if flush_draft {
            let slot = &mut self.slots[self.current];
            if slot.state.editor.overflow {
                slot.state.exit = 1;
                slot.state.report(io, "ansa: submit_size");
            } else if !slot.state.editor.bytes.is_empty() {
                match slot.state.editor.text().map(str::to_owned) {
                    Ok(text) => finished = self.text(&text, io).1,
                    Err(code) => {
                        let slot = &mut self.slots[self.current];
                        slot.state.exit = 1;
                        slot.state.report(io, code);
                    }
                }
            }
        }
        self.input_closed = true;
        self.cancel_modal();
        self.cancel_editor_request();
        self.menu = None;
        finished
    }
    fn close_owners(&mut self) {
        if !self.input_closed || !self.jobs.is_empty() || !self.pending_context.is_empty() {
            return;
        }
        for slot in &mut self.slots {
            let history_current = slot.state.snapshot.as_ref().is_some_and(|snapshot| {
                slot.state.page.2 >= snapshot.transcript_size
                    && slot
                        .timeline
                        .as_ref()
                        .is_some_and(|page| page.total >= snapshot.timeline_size)
            });
            if !slot.closing
                && (slot.closed
                    || (!slot.active()
                        && history_current
                        && slot.pending_prompt.is_none()
                        && slot.replies.is_empty()
                        && !slot.state.deferred_cancel))
            {
                slot.closing = true;
                slot.handle.close_input();
            }
        }
    }
    fn discard_contexts(&mut self, source: &str) -> bool {
        let before = self.pending_context.len();
        self.pending_context
            .retain(|_, intent| intent.source != source);
        before != self.pending_context.len()
    }
    fn cancel_modal(&mut self) {
        self.modal_token = self.modal_token.wrapping_add(1);
        if let Some(stop) = self.modal_stop.take() {
            stop.cancel();
        }
    }
    fn open_sessions(&mut self, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        self.menu = Some(Menu::Loading("Сессии проекта".into()));
        self.jobs.spawn_blocking(move || Job::Sessions {
            owner,
            token,
            result: factory.entries(),
        });
    }
    fn open_models(&mut self, effort: bool, io: &Io) {
        if self.slots[self.current].kind == Kind::Console {
            self.report(io, "У командной панели нет модели");
            return;
        }
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::default();
        self.modal_stop = Some(stop.clone());
        self.menu = Some(Menu::Loading(
            if effort { "/effort" } else { "/model" }.into(),
        ));
        self.jobs.spawn_blocking(move || Job::Models {
            owner,
            token,
            effort,
            result: factory.models(&stop),
        });
    }
    fn send_result(&mut self, io: &Io) {
        let slot = &self.slots[self.current];
        let Some(parent) = slot.parent.clone() else {
            self.report(io, "У этой сессии нет родительского разговора");
            return;
        };
        let text = slot
            .state
            .snapshot
            .as_ref()
            .filter(|s| s.state == "completed" && s.durable)
            .map(|s| s.text.clone())
            .unwrap_or_default();
        if text.is_empty() {
            self.report(io, "Нет подтверждённого результата для отправки");
            return;
        }
        let label = slot.kind.name();
        let source = slot.id.clone();
        let message = format!("Результат {label} {source}:\n{text}");
        if let Some(index) = self.slots.iter().position(|s| s.id == parent) {
            let target = &mut self.slots[index];
            if target
                .state
                .text(&message, &target.handle, io, &mut target.replies)
                .0
            {
                self.report(io, "Результат отправлен в родительский разговор");
            } else {
                self.report(
                    io,
                    "Родительский разговор пока не принимает сообщения; результат сохранён здесь",
                );
            }
        } else {
            self.report(io, "Сначала откройте родительскую сессию через /sessions");
        }
    }
    fn text(&mut self, text: &str, io: &Io) -> (bool, bool) {
        let (command, argument) = text
            .trim()
            .split_once(char::is_whitespace)
            .map_or((text.trim(), ""), |(c, a)| (c, a.trim()));
        match command {
            "/fork" => {
                self.create(
                    Kind::Fork,
                    (!argument.is_empty()).then(|| argument.to_string()),
                    io,
                );
                (true, false)
            }
            "/btw" => {
                self.create(
                    Kind::Btw,
                    (!argument.is_empty()).then(|| argument.to_string()),
                    io,
                );
                (true, false)
            }
            "/agent" => {
                if argument.is_empty() {
                    self.report(io, "Использование: /agent задача для субагента");
                } else {
                    self.create(Kind::Agent, Some(argument.into()), io);
                }
                (true, false)
            }
            "/agents" => {
                self.panel = Some(Panel::Agents { selected: 0 });
                self.focus = Focus::Inspector;
                (true, false)
            }
            "/sessions" => {
                if argument.is_empty() {
                    self.open_sessions(io);
                } else {
                    self.resume(argument.to_string(), io);
                }
                (true, false)
            }
            "/files" | "/open" => {
                self.open_file(
                    if argument.is_empty() {
                        self.factory.settings.cwd.clone()
                    } else {
                        argument.into()
                    },
                    io,
                );
                (true, false)
            }
            "/run" => {
                self.create(
                    Kind::Console,
                    (!argument.is_empty()).then(|| argument.to_string()),
                    io,
                );
                (true, false)
            }
            "/send" => {
                self.send_result(io);
                (true, false)
            }
            "/model" if argument.is_empty() => {
                self.open_models(false, io);
                (true, false)
            }
            "/effort" | "/thinking" if argument.is_empty() => {
                self.open_models(true, io);
                (true, false)
            }
            "/help" | "/" | "?" => {
                self.menu = Some(Menu::Help);
                (true, false)
            }
            "/basic" if self.slots[self.current].kind == Kind::Btw => {
                self.report(io, "Режим /btw: только чтение");
                (true, false)
            }
            _ => {
                let slot = &mut self.slots[self.current];
                slot.state.text(text, &slot.handle, io, &mut slot.replies)
            }
        }
    }
    fn resume(&mut self, id: String, io: &Io) {
        if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
            self.switch(index);
            return;
        }
        if self.slots.len() + self.jobs.len() >= MAX_SESSIONS {
            self.report(io, "Достигнут предел открытых сессий (16)");
            return;
        }
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        let source = self.slots[self.current].id.clone();
        self.jobs.spawn_blocking(move || Job::Created {
            source,
            prompt: None,
            auto_return: false,
            result: factory.resume(&id),
        });
    }
    fn readiness(&mut self, id: &str, result: Result<()>, io: &Io) {
        let Some(index) = self.slots.iter().position(|slot| slot.id == id) else {
            return;
        };
        match result {
            Ok(()) => {
                let slot = &mut self.slots[index];
                if !slot.startup_cancelled {
                    slot.became_ready();
                }
            }
            Err(code) => {
                if self.slots[index].startup_cancelled {
                    return;
                }
                self.discard_contexts(id);
                let slot = &mut self.slots[index];
                slot.state.exit = 1;
                slot.pending_prompt = None;
                slot.handle.request_shutdown();
                let parent = slot.parent.clone();
                let message = format!("Не удалось открыть сессию: {code}");
                slot.state.report(io, &message);
                if self.current == index {
                    if let Some(parent) = parent.and_then(|parent| {
                        self.slots
                            .iter()
                            .position(|slot| slot.id == parent && slot.ready)
                    }) {
                        self.switch(parent);
                        self.report(io, &message);
                    } else {
                        io.status(&format!("ansa: {code}\n"));
                        self.input_closed = true;
                        self.cancel_modal();
                        self.cancel_editor_request();
                    }
                } else {
                    self.report(io, &message);
                }
                io.writer.invalidate();
            }
        }
    }
    fn event(&mut self, id: &str, event: Option<RuntimeEvent>, io: &Io) {
        let Some(index) = self.slots.iter().position(|slot| slot.id == id) else {
            return;
        };
        let Some(event) = event else {
            let lost = self.discard_contexts(id);
            self.slots[index].closed = true;
            if lost || self.slots[index].pending_prompt.take().is_some() {
                self.slots[index].state.exit = 1;
                self.slots[index]
                    .state
                    .report(io, "Не удалось создать сессию: агент завершил работу");
            }
            return;
        };
        let lost_context = matches!(event.payload, EventPayload::ResyncRequired { .. })
            && self.discard_contexts(id);
        if let EventPayload::ContextSnapshot { command, seed } = &event.payload
            && let Some(intent) = self.pending_context.remove(&command.0)
            && intent.source == id
        {
            let factory = self.factory.clone();
            let seed = seed.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                source: intent.source,
                prompt: intent.prompt,
                auto_return: intent.kind == Kind::Agent,
                result: factory.create(intent.kind, Some(seed)),
            });
        }
        if let EventPayload::RuntimeError {
            command: Some(command),
            ..
        } = &event.payload
        {
            self.pending_context.remove(&command.0);
        }
        let slot = &mut self.slots[index];
        if let EventPayload::FrontendSnapshot(snapshot) = &event.payload
            && !slot.accepts_snapshot(snapshot)
        {
            return;
        }
        match &event.payload {
            EventPayload::ResyncRequired { .. } => {
                slot.resync = true;
                slot.refresh = Instant::now();
            }
            EventPayload::TimelinePage(page)
                if page.query == slot.timeline_query
                    && page.total
                        >= slot
                            .state
                            .snapshot
                            .as_ref()
                            .map_or(0, |snapshot| snapshot.timeline_size) =>
            {
                slot.received_timeline(page);
            }
            _ => {}
        }
        let reading = if !slot.state.follow_tail
            && matches!(event.payload, EventPayload::TranscriptPage(_))
        {
            slot.state.reading.take()
        } else {
            None
        };
        slot.state.apply(&event, io);
        if lost_context {
            slot.state.exit = 1;
            slot.state.report(
                io,
                "Не удалось восстановить контекст; повторите команду создания сессии",
            );
        }
        if reading.is_some() {
            slot.state.reading = reading;
        }
    }
    fn job(&mut self, job: Job, io: &Io) {
        let fresh = match &job {
            Job::Sessions { owner, token, .. } | Job::Models { owner, token, .. } => {
                !self.input_closed
                    && !self.terminating
                    && *owner == self.slots[self.current].id
                    && *token == self.modal_token
            }
            Job::EditorReady {
                owner,
                token,
                request,
                ..
            } => {
                !self.input_closed
                    && !self.terminating
                    && *request == self.editor_request
                    && *owner == self.slots[self.current].id
                    && matches!(&self.panel, Some(Panel::Files(files)) if files.token == *token)
            }
            _ => true,
        };
        if matches!(&job, Job::EditorReady { request, .. } if *request == self.editor_request) {
            self.editor_preparing = false;
        }
        if !fresh {
            return;
        }
        if matches!(&job, Job::Models { .. } | Job::Sessions { .. }) {
            self.menu = None;
            self.modal_stop = None;
        }
        match job {
            Job::Created {
                source,
                prompt,
                auto_return,
                result,
            } => match result {
                Ok(mut created) => {
                    let focus = self.slots[self.current].id == source;
                    if self.slots.iter().any(|slot| slot.id == created.selected.id) {
                        return;
                    }
                    if created.parent.is_none() && created.kind == Kind::Console {
                        created.parent = Some(source);
                    }
                    let id = created.selected.id.clone();
                    self.add(Slot::new(
                        created.runtime,
                        id,
                        created.kind,
                        created.parent,
                        prompt,
                        auto_return,
                    ));
                    if focus && !self.input_closed {
                        self.switch(self.slots.len() - 1);
                    }
                }
                Err(code) => self.report(io, &format!("Не удалось открыть сессию: {code}")),
            },
            Job::Sessions { result, .. } => match result {
                Ok(entries) => {
                    self.menu = Some(Menu::Sessions {
                        entries,
                        selected: 0,
                    })
                }
                Err(code) => self.report(io, &format!("Не удалось прочитать сессии: {code}")),
            },
            Job::Models { effort, result, .. } => match result.and_then(model_choices) {
                Ok(models) => {
                    let current = self.slots[self.current]
                        .state
                        .snapshot
                        .as_ref()
                        .map(|s| &s.generation);
                    if effort {
                        let supported = current
                            .and_then(|current| models.iter().find(|m| m.0 == current.model))
                            .map(|m| m.2.clone())
                            .unwrap_or_default();
                        let mut values = vec![None];
                        values.extend(supported.into_iter().map(Some));
                        let selected = current
                            .and_then(|current| values.iter().position(|e| *e == current.effort))
                            .unwrap_or(0);
                        self.menu = Some(Menu::Efforts { values, selected });
                    } else {
                        let selected = current
                            .and_then(|current| models.iter().position(|m| m.0 == current.model))
                            .unwrap_or(0);
                        self.menu = Some(Menu::Models { models, selected });
                    }
                }
                Err(code) => self.report(io, &format!("Каталог моделей недоступен: {code}")),
            },
            Job::File { token, result } => {
                if let Some(Panel::Files(files)) = &mut self.panel
                    && files.token == token
                {
                    files.loading = false;
                    match result {
                        Ok(FileValue::Listing(listing)) => files.listing = Some(listing),
                        Ok(FileValue::Document(document)) => {
                            files.document = Some(document);
                            files.preview_cache = None;
                        }
                        Err(code) => self.report(io, &format!("Не удалось открыть файл: {code}")),
                    }
                }
            }
            Job::Preview { token, width, rows } => {
                if self
                    .preview_job
                    .as_ref()
                    .is_some_and(|job| job.token == token && job.width == width)
                {
                    self.preview_job = None;
                }
                if !self.input_closed
                    && !self.terminating
                    && let Some(Panel::Files(files)) = &mut self.panel
                {
                    files.accept_preview(token, width, rows);
                }
            }
            Job::EditorReady { result, .. } => match result {
                Ok(command) => self.pending_editor = Some(command),
                Err(code) => self.report(io, &format!("Не удалось открыть редактор: {code}")),
            },
        }
    }
    fn escape(&mut self, io: &Io) {
        self.slots[self.current].cancel_history_jump();
        self.cancel_modal();
        self.cancel_editor_request();
        if self.selection.take().is_some() {
            let slot = &mut self.slots[self.current];
            if slot.kind == Kind::Main {
                slot.state.follow_tail = true;
                slot.state.reading = None;
                slot.state.scroll = 0;
                slot.timeline_query = GetTranscript {
                    before: 0,
                    limit: 64,
                    forward: false,
                };
                slot.refresh = Instant::now();
            }
            return;
        }
        if self.menu.take().is_some() {
            return;
        }
        if let Some(Panel::Files(files)) = &mut self.panel
            && files.jump.take().is_some()
        {
            return;
        }
        self.pending_editor = None;
        if self.panel.take().is_some() {
            self.focus = Focus::Chat;
            return;
        }
        if let Some(parent) = self.slots[self.current].parent.clone() {
            if let Some(index) = self.slots.iter().position(|slot| slot.id == parent) {
                self.switch(index);
            } else {
                self.resume(parent, io);
            }
            return;
        }
        let state = &mut self.slots[self.current].state;
        state.follow_tail = !state.follow_tail;
        state.reading = None;
        if state.follow_tail {
            state.scroll = 0;
            self.slots[self.current].timeline_query = GetTranscript {
                before: 0,
                limit: 64,
                forward: false,
            };
        } else {
            state.reading_total = state.snapshot.as_ref().map_or(0, |s| s.transcript_size);
            if let Some(page) = &self.slots[self.current].timeline {
                self.slots[self.current].timeline_query.before = page.end;
            }
        }
        self.slots[self.current].refresh = Instant::now();
    }
    fn page(&mut self, older: bool) {
        self.selection = None;
        self.slots[self.current].cancel_history_jump();
        if self.focus == Focus::Inspector {
            if let Some(Panel::Files(files)) = &mut self.panel {
                files.preview_end = false;
                files.offset = if older {
                    files.offset.saturating_sub(8)
                } else {
                    files.offset.saturating_add(8).min(files.last_line())
                };
            }
            return;
        }
        let slot = &mut self.slots[self.current];
        let state = &mut slot.state;
        if older {
            if state.follow_tail {
                state.reading_total = state.snapshot.as_ref().map_or(0, |s| s.transcript_size);
            }
            state.follow_tail = false;
            if slot.timeline_query.before == 0
                && let Some(page) = &slot.timeline
            {
                slot.timeline_query = GetTranscript {
                    before: page.end,
                    limit: 64,
                    forward: false,
                };
            }
            if state.scroll >= state.scroll_limit
                && let Some(page) = &slot.timeline
                && page.begin > 0
            {
                slot.timeline_query = GetTranscript {
                    before: page.begin,
                    limit: 64,
                    forward: false,
                };
                state.scroll = 0;
                state.reading = None;
                slot.refresh = Instant::now();
            } else {
                state.scroll = state.scroll.saturating_add(8).min(state.scroll_limit);
            }
        } else if state.scroll == 0
            && let Some(page) = &slot.timeline
            && page.end < page.total
        {
            slot.timeline_query = GetTranscript {
                before: page.end,
                limit: 64,
                forward: true,
            };
            slot.jump_top = true;
            state.reading = None;
            slot.refresh = Instant::now();
        } else {
            state.scroll = state.scroll.saturating_sub(8);
            if state.scroll == 0
                && slot
                    .timeline
                    .as_ref()
                    .is_some_and(|page| page.end == page.total)
            {
                state.follow_tail = true;
                state.reading = None;
                slot.timeline_query = GetTranscript {
                    before: 0,
                    limit: 64,
                    forward: false,
                };
            }
        }
    }
    fn navigate(&mut self, older: bool) {
        self.selection = None;
        if self.focus == Focus::Inspector
            && let Some(Panel::Files(files)) = &mut self.panel
        {
            files.preview_end = false;
        }
        let delta = |selected: &mut usize, length: usize| {
            if older {
                *selected = selected.saturating_sub(1);
            } else {
                *selected = (*selected + 1).min(length.saturating_sub(1));
            }
        };
        match &mut self.menu {
            Some(Menu::Loading(_)) => {}
            Some(Menu::Sessions { entries, selected }) => delta(selected, entries.len()),
            Some(Menu::Models { models, selected }) => delta(selected, models.len()),
            Some(Menu::Efforts { values, selected }) => delta(selected, values.len()),
            Some(Menu::Help) => {}
            None if self.focus == Focus::Inspector => match &mut self.panel {
                Some(Panel::Files(files)) if files.document.is_none() => delta(
                    &mut files.selected,
                    files.listing.as_ref().map_or(0, |l| l.entries.len()),
                ),
                Some(Panel::Files(files)) => {
                    files.offset = if older {
                        files.offset.saturating_sub(1)
                    } else {
                        files.offset.saturating_add(1).min(files.last_line())
                    };
                }
                Some(Panel::Agents { selected }) => delta(
                    selected,
                    self.slots.iter().filter(|s| s.kind == Kind::Agent).count(),
                ),
                None => {}
            },
            None if !self.slots[self.current].state.follow_tail => self.page(older),
            None => {
                let slot = &mut self.slots[self.current];
                let total = slot
                    .state
                    .snapshot
                    .as_ref()
                    .map_or(0, |s| s.transcript_size);
                slot.state
                    .input_history
                    .navigate(older, &mut slot.state.editor, total);
            }
        }
    }
    fn enter(&mut self, io: &Io) -> Option<String> {
        if let Some(menu) = self.menu.take() {
            match menu {
                Menu::Loading(_) => {}
                Menu::Sessions { entries, selected } => {
                    if let Some(entry) = entries.get(selected) {
                        self.resume(entry.id.clone(), io);
                    }
                }
                Menu::Models { models, selected } => {
                    if let Some(model) = models.get(selected) {
                        self.text(&format!("/model {}", model.0), io);
                    }
                }
                Menu::Efforts { values, selected } => {
                    if let Some(effort) = values.get(selected) {
                        self.text(
                            &format!("/effort {}", effort.as_deref().unwrap_or("default")),
                            io,
                        );
                    }
                }
                Menu::Help => {}
            }
            return None;
        }
        if self.focus == Focus::Inspector {
            if let Some(Panel::Files(files)) = &mut self.panel
                && let Some(line) = files.jump.take()
            {
                if let Ok(line) = line.parse::<usize>() {
                    files.preview_end = false;
                    let source = line.saturating_sub(1);
                    files.offset = if files.tab == 2 {
                        files.preview_cache.as_ref().map_or(0, |(_, rows)| {
                            super::super::markdown::Row::source_row(rows, source)
                        })
                    } else {
                        source.min(files.last_line())
                    };
                }
                return None;
            }
            match &self.panel {
                Some(Panel::Files(files)) if files.document.is_none() => {
                    if let Some(entry) = files
                        .listing
                        .as_ref()
                        .and_then(|l| l.entries.get(files.selected))
                    {
                        self.open_file(entry.path.clone(), io);
                    }
                }
                Some(Panel::Agents { selected }) => {
                    if let Some(index) = self
                        .slots
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.kind == Kind::Agent)
                        .nth(*selected)
                        .map(|(i, _)| i)
                    {
                        self.switch(index);
                    }
                }
                _ => {}
            }
            return None;
        }
        let slot = &mut self.slots[self.current];
        match slot.state.editor.text().map(str::to_owned) {
            Ok(text) => Some(text),
            Err(code) => {
                slot.state.report(io, &format!("ansa: {code}"));
                slot.state.exit = 1;
                slot.state.editor.clear();
                slot.state.input_history.reset();
                None
            }
        }
    }
    fn mouse(&mut self, mouse: MouseEvent, io: &Io) {
        if matches!(
            mouse.kind,
            MouseKind::Press | MouseKind::WheelUp | MouseKind::WheelDown
        ) {
            self.update_displayed(io);
            if let Some(frame) = &self.displayed {
                if let Some((begin, end, right)) = frame.chat_bounds {
                    if mouse.row >= begin && mouse.row < end {
                        self.focus = if mouse.column < right {
                            Focus::Chat
                        } else if self.panel.is_some() {
                            Focus::Inspector
                        } else {
                            self.focus
                        };
                    }
                } else if self.panel.is_some() {
                    self.focus = Focus::Inspector;
                }
            }
        }
        match mouse.kind {
            MouseKind::Press => {
                self.slots[self.current].cancel_history_jump();
                self.update_displayed(io);
                self.selection = None;
                self.tool_press = None;
                // A partially painted frame has no single truthful text
                // snapshot. Selection starts only on a completed paint.
                if !self.frames.is_empty() || io.writer.failed() {
                    return;
                }
                if let Some(frame) = &self.displayed {
                    if frame.chat_bounds.is_some_and(|(begin, end, right)| {
                        mouse.row >= begin && mouse.row < end && mouse.column < right
                    }) && let Some((_, key)) =
                        frame.tool_actions.iter().find(|(row, _)| *row == mouse.row)
                    {
                        self.tool_press = Some((mouse.column, mouse.row, key.clone()));
                    }
                    let slot = &mut self.slots[self.current];
                    let reading = slot.projected();
                    slot.state.follow_tail = false;
                    slot.state.reading_total = slot
                        .state
                        .snapshot
                        .as_ref()
                        .map_or(0, |s| s.transcript_size);
                    if let Some(page) = &slot.timeline {
                        slot.timeline_query = GetTranscript {
                            before: page.end,
                            limit: 64,
                            forward: false,
                        };
                    }
                    if let Some(reading) = reading {
                        slot.state.reading = Some(reading);
                    }
                    self.selection = Some(Selection::new(frame.clone(), mouse.column, mouse.row));
                }
            }
            MouseKind::Drag => {
                if self
                    .tool_press
                    .as_ref()
                    .is_some_and(|(column, row, _)| (*column, *row) != (mouse.column, mouse.row))
                {
                    self.tool_press = None;
                }
                if let Some(selection) = &mut self.selection {
                    selection.drag(mouse.column, mouse.row);
                }
            }
            MouseKind::Release => {
                if let Some((column, row, key)) = self.tool_press.take()
                    && (column, row) == (mouse.column, mouse.row)
                {
                    let slot = &mut self.slots[self.current];
                    if !slot.tool_expanded.remove(&key) && slot.tool_expanded.len() < 4096 {
                        slot.tool_expanded.insert(key);
                    }
                    slot.tool_anchor_limit =
                        self.displayed.as_ref().map(|frame| frame.scroll_limit);
                    slot.state.reading = None;
                    self.selection = None;
                    return;
                }
                if let Some(selection) = &mut self.selection
                    && let Some(text) = selection.finish(mouse.column, mouse.row)
                    && !io.writer.clipboard(&text)
                {
                    self.report(
                        io,
                        "Выделение превышает лимит clipboard или очередь вывода заполнена",
                    );
                }
            }
            MouseKind::WheelUp => {
                self.selection = None;
                self.page(true);
            }
            MouseKind::WheelDown => {
                self.selection = None;
                self.page(false);
            }
        }
    }
    fn popup(&self) -> Option<Popup> {
        let menu = self.menu.as_ref()?;
        Some(match menu {
            Menu::Loading(title) => Popup {
                title: title.clone(),
                subtitle: "Загрузка…".into(),
                footer: "Esc отменить".into(),
                ..Popup::default()
            },
            Menu::Sessions { entries, selected } => Popup {
                title: "Сессии проекта".into(),
                subtitle: "Выбрать разговор".into(),
                items: entries
                    .iter()
                    .map(|e| Choice {
                        label: e.id.clone(),
                        detail: e.kind.clone(),
                    })
                    .collect(),
                selected: *selected,
                footer: "Enter открыть · Esc назад".into(),
            },
            Menu::Models { models, selected } => Popup {
                title: "/model".into(),
                subtitle: "Для следующих сообщений".into(),
                items: models
                    .iter()
                    .map(|m| Choice {
                        label: m.0.clone(),
                        detail: m.1.clone(),
                    })
                    .collect(),
                selected: *selected,
                footer: "Enter применить · Esc отменить".into(),
            },
            Menu::Efforts { values, selected } => Popup {
                title: "/effort".into(),
                subtitle: "Подтверждённые уровни модели".into(),
                items: values
                    .iter()
                    .map(|e| Choice {
                        label: e.clone().unwrap_or_else(|| "По умолчанию".into()),
                        detail: String::new(),
                    })
                    .collect(),
                selected: *selected,
                footer: "Enter применить · Esc отменить".into(),
            },
            Menu::Help => Popup {
                title: "Команды ANSA".into(),
                subtitle: "Enter отправляет уточнение и во время работы".into(),
                items: [
                    "/model · /effort · /thinking",
                    "/basic · /plan · /status",
                    "/fork · /btw вопрос · /agent задача",
                    "/sessions · /agents · /send",
                    "/files путь · /run команда",
                    "/copy · /resume · /exit",
                    "Ctrl+J перенос · Ctrl+T подробности",
                    "Ctrl+C прервать текущий шаг",
                    "Esc чтение/возврат · Tab фокус",
                ]
                .iter()
                .map(|label| Choice {
                    label: (*label).into(),
                    detail: String::new(),
                })
                .collect(),
                selected: 0,
                footer: "Esc закрыть".into(),
            },
        })
    }
    fn inspector(&self) -> Option<Inspector> {
        match self.panel.as_ref()? {
            Panel::Agents { selected } => Some(Inspector {
                title: "AGENTS".into(),
                lines: self
                    .slots
                    .iter()
                    .filter(|s| s.kind == Kind::Agent)
                    .map(|s| {
                        format!(
                            "{}  {}",
                            s.id,
                            s.state
                                .snapshot
                                .as_ref()
                                .map_or("loading", |v| v.state.as_str())
                        )
                    })
                    .collect(),
                selected: Some(*selected),
                footer: "Enter разговор · Esc назад".into(),
                kind: InspectorKind::Agents,
                ..Inspector::default()
            }),
            Panel::Files(files) => {
                if files.loading {
                    return Some(Inspector {
                        title: "FILES".into(),
                        lines: vec!["Открываю…".into()],
                        ..Inspector::default()
                    });
                }
                if let Some(document) = &files.document {
                    let (kind, text) = match files.tab {
                        1 => (
                            InspectorKind::Diff,
                            if document.diff.status == DiffStatus::Unchanged {
                                "Изменений относительно HEAD нет"
                            } else if matches!(
                                document.diff.status,
                                DiffStatus::NoRepository
                                    | DiffStatus::NoCommit
                                    | DiffStatus::Unavailable
                            ) {
                                "Diff недоступен: нет Git HEAD либо файл превышает лимит"
                            } else {
                                &document.diff.text
                            },
                        ),
                        2 => (
                            InspectorKind::Markdown,
                            if document.preview.is_some() {
                                if files.preview_rows().is_some() {
                                    ""
                                } else {
                                    "Готовлю просмотр..."
                                }
                            } else {
                                "Preview доступен для Markdown"
                            },
                        ),
                        _ => (InspectorKind::Source, document.source.as_str()),
                    };
                    Some(Inspector {
                        title: format!(
                            "{} · READ ONLY{}",
                            document.path.display(),
                            if document.truncated {
                                " · truncated"
                            } else {
                                ""
                            }
                        ),
                        tabs: vec!["Source".into(), "Diff".into(), "Preview".into()],
                        selected_tab: files.tab,
                        lines: text.lines().map(String::from).collect(),
                        offset: files.offset,
                        footer: files.jump.as_ref().map_or_else(
                            || "s/d/p вкладки · :строка · e редактор · Esc чат".into(),
                            |line| format!(":{line}"),
                        ),
                        kind,
                        markdown: if files.tab == 2 {
                            files.preview_rows().cloned()
                        } else {
                            None
                        },
                        ..Inspector::default()
                    })
                } else {
                    let listing = files.listing.as_ref();
                    Some(Inspector {
                        title: listing
                            .map_or_else(|| "FILES".into(), |l| l.path.display().to_string()),
                        lines: listing.map_or_else(Vec::new, |l| {
                            l.entries
                                .iter()
                                .map(|e| {
                                    format!("{}{}", e.name, if e.directory { "/" } else { "" })
                                })
                                .collect()
                        }),
                        selected: Some(files.selected),
                        offset: files.selected.saturating_sub(8),
                        footer: "Enter открыть · h родитель · Esc чат".into(),
                        ..Inspector::default()
                    })
                }
            }
        }
    }
    fn ui(&self) -> Workspace {
        let slot = &self.slots[self.current];
        let mut breadcrumb = Vec::new();
        let mut current = Some(slot.id.as_str());
        for _ in 0..MAX_SESSIONS {
            let Some(id) = current else {
                break;
            };
            if let Some(ancestor) = self.slots.iter().find(|s| s.id == id) {
                breadcrumb.push(format!(
                    "{} {}",
                    ancestor.kind.name(),
                    &ancestor.id[..ancestor.id.len().min(12)]
                ));
                current = ancestor.parent.as_deref();
            } else {
                break;
            }
        }
        breadcrumb.reverse();
        let snapshot = slot.state.snapshot.as_ref();
        let generation = snapshot.map(|s| &s.generation);
        Workspace {
            breadcrumb: if breadcrumb.is_empty() {
                self.root_id.clone()
            } else {
                breadcrumb.join(" / ")
            },
            model: if slot.kind == Kind::Console {
                "local console".into()
            } else {
                generation.map_or_else(String::new, |g| g.model.clone())
            },
            effort: generation
                .and_then(|g| g.effort.clone())
                .unwrap_or_default(),
            activity: slot.state.progress.elapsed().unwrap_or_default(),
            reading: !slot.state.follow_tail,
            new_events: snapshot.map_or(0, |s| {
                s.transcript_size.saturating_sub(slot.state.reading_total)
            }),
            focus: self.focus,
            inspector: self.inspector(),
            popup: self.popup(),
        }
    }
    fn update_displayed(&mut self, io: &Io) {
        sync_displayed(&mut self.frames, &mut self.displayed, |sequence| {
            io.writer.displayed(sequence)
        });
    }
    fn prepare_preview(&mut self, io: &Io) {
        let width = if self.input_closed || self.terminating || io.size.0 < 12 || io.size.1 < 8 {
            None
        } else {
            let width = io.size.0 - 1;
            Some(if width >= 100 {
                (width * 2 / 5).max(34)
            } else {
                width
            })
        };
        let mut desired = None;
        let mut request = None;
        if let Some(Panel::Files(files)) = &mut self.panel {
            let document = files
                .document
                .as_ref()
                .and_then(|document| document.preview.as_ref());
            files.preview_width = width.filter(|_| document.is_some());
            if let (Some(width), Some(document)) = (files.preview_width, document) {
                desired = Some((files.token, width));
                if files.preview_rows().is_none() {
                    request = Some((files.token, width, document.clone()));
                }
            }
        }
        if let Some(job) = &self.preview_job {
            if desired != Some((job.token, job.width)) {
                job.stop.cancel();
            }
            // An obsolete layout is joined before the latest resize can start.
            return;
        }
        if self.jobs.len() + self.pending_context.len() >= MAX_JOBS {
            return;
        }
        if let Some((token, width, document)) = request {
            let stop = Cancel::linked(&self.stop);
            self.preview_job = Some(PreviewJob {
                token,
                width,
                stop: stop.clone(),
            });
            self.jobs.spawn_blocking(move || Job::Preview {
                token,
                width,
                rows: document
                    .rows_cancelled(width, &stop)
                    .map(std::sync::Arc::new),
            });
        }
    }
    fn render(&mut self, io: &Io, context: &str) {
        self.update_displayed(io);
        self.prepare_preview(io);
        let operational =
            self.operational_frame.is_some() || self.slots[self.current].input_ready();
        let frame = if let Some(selection) = &self.selection {
            let mut frozen = selection.frame_at(io.size, io.color);
            let ui = self.ui();
            let slot = &mut self.slots[self.current];
            let projected = slot.projected();
            if let Some(live) = slot
                .state
                .render_workspace(io, context, &ui, projected.as_deref())
            {
                frozen.update_status_from(&live);
            }
            Some(frozen)
        } else if !operational {
            let status = if !self.slots[self.current].state.status.trim().is_empty() {
                self.slots[self.current].state.status.trim()
            } else if self.slots[self.current].startup_cancelled {
                "Открытие сессии отменено"
            } else {
                "Открытие сессии"
            };
            let mut frame = carbon::layout_full(&[], b"", 0, status, 0, context, io.size, io.color);
            if let Some(frame) = &mut frame
                && let Some(header) = frame.lines.first_mut()
            {
                *header = header.replacen("ansa", "ANSA", 1);
            }
            frame
        } else {
            let ui = self.ui();
            let slot = &mut self.slots[self.current];
            let projected = slot.projected();
            slot.state
                .render_workspace(io, context, &ui, projected.as_deref())
        };
        if let Some(frame) = &frame {
            let slot = &mut self.slots[self.current];
            if let Some(before) = slot.tool_anchor_limit.take() {
                slot.state.scroll = slot
                    .state
                    .scroll
                    .saturating_add(frame.scroll_limit)
                    .saturating_sub(before)
                    .min(frame.scroll_limit);
                return self.render(io, context);
            }
            slot.state.scroll = slot.state.scroll.min(frame.scroll_limit);
        }
        if let Some(frame) = frame
            && self.last_frame.as_ref() != Some(&frame)
            && io.writer.idle()
        {
            let sequence = io.writer.frame(frame.clone());
            if operational && self.operational_frame.is_none() {
                self.operational_frame = Some(sequence);
            }
            self.frames.push_back((sequence, frame.clone()));
            // Writer coalesces pending frames. Keep a bounded set of candidates
            // and never claim a queued frame was already displayed.
            while self.frames.len() > 3 {
                self.frames.pop_front();
            }
            self.last_frame = Some(frame);
        }
    }
}

fn sync_displayed(
    candidates: &mut std::collections::VecDeque<(u64, carbon::Frame)>,
    displayed: &mut Option<carbon::Frame>,
    complete: impl Fn(u64) -> bool,
) {
    while candidates
        .front()
        .is_some_and(|(sequence, _)| complete(*sequence))
    {
        *displayed = candidates.pop_front().map(|(_, frame)| frame);
    }
}

fn command_id(state: &State) -> String {
    format!("c_{}_{}", state.prefix, state.sequence)
}

fn model_choices(value: Json) -> Result<Vec<(String, String, Vec<String>)>> {
    Ok(crate::auth::dispatch::model_capabilities(&value)?
        .into_iter()
        .take(256)
        .map(|model| (model.model, model.display_name, model.efforts))
        .collect())
}

pub async fn run(
    mut runtime: Runtime,
    io: &mut Io,
    context: &str,
    factory: Factory,
    root_id: String,
    kind: Kind,
    parent: Option<String>,
) -> Result<i32> {
    if io.raw.is_none() {
        return super::run(&mut runtime, io, None, false, context).await;
    }
    let mut work = Workbench::new(runtime, factory, root_id, kind, parent)?;
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let input_events = readiness_source(io.input.duplicate().map_err(|_| "cli_input_poll")?).ok();
    let signal_events = readiness_source(
        io.signals
            .readiness_descriptor()
            .map_err(|_| "cli_signal_poll")?,
    )
    .map_err(|_| "cli_signal_poll")?;
    let mut finished = false;
    let mut exit = 0;
    let mut escape_at = None;
    let mut dirty = true;
    let mut deferred_input = std::collections::VecDeque::new();
    let mut failure: Option<&'static str> = None;
    let mut terminating_since = None;
    let mut editor_cancel_since: Option<Instant> = None;
    let result: Result<()> = async {
        while !finished || work.editor.is_some() {
            work.update_displayed(io);
            let mut input_guard = None; let mut signal_guard = None;
            let input_enabled = !finished && !work.input_closed && work.editor.is_none() && work.pending_editor.is_none() && !work.editor_preparing
                && work.slots[work.current].state.pending_draft.is_none()
                && work.slots[work.current].ready && !work.slots[work.current].ready_refresh
                && work.slots[work.current].state.history_ready && work.displayed.is_some()
                && work.operational_frame.is_some_and(|sequence| io.writer.displayed(sequence));
            tokio::select! { biased;
                ready = signal_events.readable() => { signal_guard = Some(ready.map_err(|_| "cli_signal_poll")?); }
                event = work.events.recv() => { if let Some((id, event)) = event {
                    match event {
                        SessionEvent::Ready(result) => work.readiness(&id, result, io),
                        SessionEvent::Runtime(event) => work.event(&id, Some(event), io),
                        SessionEvent::Closed => work.event(&id, None, io),
                    }
                    dirty = true;
                } }
                job = work.jobs.join_next(), if !work.jobs.is_empty() => { if let Some(job) = job { work.job(job.map_err(|_| "workspace_job")?, io); dirty = true; } }
                ready = async { input_events.as_ref().expect("enabled").readable().await }, if input_events.is_some() && input_enabled => { input_guard = Some(ready.map_err(|_| "cli_input_poll")?); }
                _ = io.writer.changed() => { dirty = true; }
                _ = interval.tick() => {}
            }
            while let Some(signal) = io.signals.take().map_err(|_| "cli_signal_read")? {
                dirty = true;
                use ansa_linux::terminal::*;
                match signal {
                    SIGTERM | SIGHUP => {
                        exit = 128 + signal; finished = true; work.terminating = true;
                        terminating_since.get_or_insert(Instant::now());
                        work.cancel_editor_request(); work.cancel_modal();
                        if let Some(editor) = &mut work.editor { editor_cancel_since.get_or_insert(Instant::now()); if let Err(code) = editor.cancel() { failure.get_or_insert(code); } }
                    }
                    SIGINT => {
                        if let Some(editor) = &mut work.editor { editor_cancel_since.get_or_insert(Instant::now()); if let Err(code) = editor.cancel() { failure.get_or_insert(code); finished = true; work.terminating = true; terminating_since.get_or_insert(Instant::now()); } }
                        else if work.pending_editor.is_some() || work.editor_preparing { work.cancel_editor_request(); }
                        else { work.cancel_current(io); }
                    }
                    SIGTSTP => {
                        if let Some(editor) = &mut work.editor { editor_cancel_since.get_or_insert(Instant::now()); if let Err(code) = editor.cancel() { failure.get_or_insert(code); finished = true; work.terminating = true; terminating_since.get_or_insert(Instant::now()); } }
                        else { io.suspend().await?; }
                        work.last_frame = None;
                    }
                    SIGCONT => { io.resume_terminal()?; work.last_frame = None; }
                    SIGWINCH => { io.resize(); work.last_frame = None; }
                    _ => {}
                }
            }
            if work.editor_wait.as_mut().is_some_and(|wait| {
                let now = Instant::now(); let written = io.writer.written();
                if written != wait.written { wait.written = written; wait.progress = now; }
                now.duration_since(wait.progress) >= Duration::from_millis(250) || now.duration_since(wait.began) >= Duration::from_secs(1)
            }) {
                work.cancel_editor_request(); work.report(io, "Не удалось передать терминал редактору: вывод не завершился"); dirty = true;
            }
            if let Some(mut editor) = work.editor.take() {
                if editor_cancel_since.is_some_and(|began| began.elapsed() >= Duration::from_secs(2)) || (finished && terminating_since.is_some_and(|began| began.elapsed() >= Duration::from_secs(2))) {
                    let _ = editor.abort_for_exit(io).map_err(|code| { failure.get_or_insert(code); });
                    failure.get_or_insert("editor_cleanup_unknown");
                    finished = true; work.terminating = true;
                } else { match editor.poll(io) {
                    Ok(Some(status)) => {
                        if status.cleanup_unknown { work.report(io, "Редактор завершён; состояние отделившихся процессов неизвестно"); }
                        else { work.report(io, &format!("Редактор завершён: {}", status.code)); }
                        work.last_frame = None; work.displayed = None; work.frames.clear(); dirty = true;
                        editor_cancel_since = None;
                        if !finished && !work.input_closed && let Some(Panel::Files(files)) = &work.panel && let Some(document) = &files.document { work.open_file(document.path.clone(), io); }
                    }
                    Ok(None) => work.editor = Some(editor),
                    Err(code) => {
                        work.report(io, &format!("Не удалось открыть редактор: {code}")); work.last_frame = None; dirty = true;
                        work.slots[work.current].state.exit = 1;
                        if code == "editor_cleanup_unknown" { failure.get_or_insert(code); finished = true; work.terminating = true; }
                        editor_cancel_since = None;
                    }
                } }
            }
            if !finished && !work.input_closed && work.editor.is_none() && work.pending_editor.is_some() && io.writer.idle()
                && work.jobs.is_empty() && !work.slots.iter().any(Slot::active)
            {
                let command = work.pending_editor.take().expect("checked pending editor");
                work.editor_wait = None;
                match io.start_editor(&command.argv, &work.factory.settings.cwd) {
                    Ok(editor) => { work.editor = Some(editor); editor_cancel_since = None; }
                    Err(code) => work.report(io, &format!("Не удалось открыть редактор: {code}")),
                }
            }
            let now = Instant::now();
            let mut admissions = Vec::new();
            for slot in &mut work.slots {
                dirty |= slot.poll_replies(io)?;
                admissions.append(&mut slot.admissions);
                if work.editor.is_none() && !finished { slot.refresh(io, now); }
                dirty |= slot.state.progress.tick(now);
                if slot.state.explicit_status { dirty |= slot.state.wait_clock.tick(now); }
            }
            for (id, accepted) in admissions {
                if !accepted { work.pending_context.remove(&id); }
                if let Some((child, request)) = work.pending_results.remove(&id)
                    && accepted && let Some(slot) = work.slots.iter_mut().find(|slot| slot.id == child)
                { slot.returned = request; }
            }
            // Authorized child results return through the parent's typed user
            // input path once, and therefore cross the same durable boundary.
            let mut returns = Vec::new();
            for (index, slot) in work.slots.iter().enumerate() {
                if work.editor.is_some() || work.pending_editor.is_some() || work.input_closed || finished { break; }
                if slot.auto_return && let Some(view) = &slot.state.snapshot
                    && view.state == "completed" && view.durable && !view.worker_busy
                    && !view.text.is_empty() && view.request != slot.returned
                    && !work.pending_results.values().any(|(child, request)| child == &slot.id && request == &view.request)
                { returns.push(index); }
            }
            for index in returns {
                let slot = &work.slots[index];
                let Some(parent) = slot.parent.clone() else { continue; };
                let view = slot.state.snapshot.as_ref().expect("checked completion");
                let request = view.request.clone(); let child = slot.id.clone(); let text = format!("Результат субагента {}:\n{}", slot.id, view.text);
                if let Some(parent) = work.slots.iter_mut().find(|slot| slot.id == parent)
                    && parent.state.snapshot.as_ref().is_some_and(|s| !s.storage_failed && s.state != "paused")
                    && parent.state.text(&text, &parent.handle, io, &mut parent.replies).0
                { work.pending_results.insert(command_id(&parent.state), (child, request)); }
            }
            if work.slots[work.current].state.editor.escape_pending() {
                let at = escape_at.get_or_insert(now);
                if now.duration_since(*at) >= Duration::from_millis(35) {
                    if let Some(Action::Escape) = work.slots[work.current].state.editor.flush_escape() { work.escape(io); dirty = true; }
                    escape_at = None;
                }
            } else { escape_at = None; }
            let can_read = !finished && !work.input_closed && work.editor.is_none() && work.pending_editor.is_none() && !work.editor_preparing && work.slots[work.current].state.pending_draft.is_none()
                && work.slots[work.current].ready && !work.slots[work.current].ready_refresh
                && work.slots[work.current].state.history_ready && work.displayed.is_some()
                && work.operational_frame.is_some_and(|sequence| io.writer.displayed(sequence));
            if can_read && (!deferred_input.is_empty() || io.input.ready(false).map_err(|_| "cli_input_poll")?) {
                let mut bytes = [0; 4096];
                let read = if deferred_input.is_empty() { io.input.read(&mut bytes) } else { let count = deferred_input.len().min(bytes.len()); for byte in &mut bytes[..count] { *byte = deferred_input.pop_front().expect("bounded input"); } Ok(count) };
                match read {
                    Ok(0) => finished |= work.eof(io, true),
                    Ok(count) => {
                        dirty = true;
                        for (position, &byte) in bytes[..count].iter().enumerate() {
                            // Modal/read-only navigation never edits a draft.
                            if byte.is_ascii() && !work.slots[work.current].state.editor.escape_pending() && !work.slots[work.current].state.editor.pasting() {
                                let jumping = matches!(&work.panel, Some(Panel::Files(files)) if files.jump.is_some());
                                if matches!(byte, b'j' | b'k') && !jumping && (work.menu.is_some() || work.focus == Focus::Inspector || !work.slots[work.current].state.follow_tail) { work.navigate(byte == b'k'); continue; }
                                let file_visible = work.displayed.as_ref().map_or(1, |frame| frame.inspector_content_rows).max(1);
                                if work.focus == Focus::Inspector && let Some(Panel::Files(files)) = &mut work.panel {
                                    if let Some(line) = &mut files.jump {
                                        if byte.is_ascii_digit() { if line.len() < 9 { line.push(byte as char); } continue; }
                                        if matches!(byte, 8 | 127) { line.pop(); continue; }
                                        if !matches!(byte, 27 | b'\r' | b'\n' | 9) { continue; }
                                    } else {
                                        if byte == b':' && files.document.is_some() { files.jump = Some(String::new()); continue; }
                                        if byte == b'g' { files.preview_end = false; files.offset = 0; continue; }
                                        if byte == b'G' { files.preview_end = files.tab == 2 && files.preview_rows().is_none(); let visible = if files.preview_end { 1 } else { file_visible }; files.offset = files.last_line().saturating_sub(visible - 1); continue; }
                                    }
                                    if let Some(tab) = match byte { b's' => Some(0), b'd' => Some(1), b'p' => Some(2), _ => None } { files.preview_end = false; files.tab = tab; files.offset = 0; continue; }
                                    if byte == b'h' { let parent = files.document.as_ref().and_then(|d| d.path.parent()).or_else(|| files.listing.as_ref().and_then(|l| l.path.parent())).map(Path::to_path_buf); if let Some(parent) = parent { work.open_file(parent, io); } continue; }
                                    if byte == b'e' { work.prepare_editor(io); continue; }
                                    if byte >= 32 && byte != 127 { continue; }
                                }
                                if work.focus == Focus::Chat && !work.slots[work.current].state.follow_tail && matches!(byte, b'g' | b'G') {
                                    work.selection = None;
                                    let slot = &mut work.slots[work.current];
                                    slot.jump_history(byte == b'g');
                                    continue;
                                }
                                if work.menu.is_some() && !matches!(byte, 27 | b'\r' | b'\n' | 9) { continue; }
                            }
                            if byte >= 32 && byte != 127 && !work.slots[work.current].state.editor.escape_pending() && !work.slots[work.current].state.editor.pasting() {
                                work.selection = None;
                            }
                            let slot = &mut work.slots[work.current];
                            let before = (slot.state.editor.bytes.len(), slot.state.editor.cursor);
                            let action = slot.state.editor.feed(byte, true);
                            if before != (slot.state.editor.bytes.len(), slot.state.editor.cursor) { slot.state.input_history.edited(&slot.state.editor); }
                            if let Some(action) = action {
                                match action {
                                    Action::Submit | Action::QueueSubmit => {
                                        let origin = work.current;
                                        if let Some(text) = work.enter(io) {
                                            let (accepted, done) = work.text(&text, io); finished |= done;
                                            if accepted { let slot = &mut work.slots[origin]; slot.state.editor.clear(); slot.state.input_history.reset(); slot.state.follow_tail = true; slot.state.reading = None; slot.timeline_query = GetTranscript { before: 0, limit: 64, forward: false }; slot.refresh = Instant::now(); }
                                        }
                                    }
                                    Action::Escape => work.escape(io),
                                    Action::FocusNext | Action::FocusPrevious => if work.panel.is_some() { work.focus = if work.focus == Focus::Chat { Focus::Inspector } else { Focus::Chat }; },
                                    Action::Mouse(mouse) => work.mouse(mouse, io),
                                    Action::HistoryOlder | Action::HistoryNewer => work.navigate(action == Action::HistoryOlder),
                                    Action::PageUp => work.page(true), Action::PageDown => work.page(false),
                                    Action::ToggleTools => { let slot = &mut work.slots[work.current]; slot.state.tools_expanded = !slot.state.tools_expanded; slot.state.reading = None; }
                                    Action::CopyHistory => work.slots[work.current].state.start_copy(io),
                                    Action::Overflow => { work.report(io, "ansa: submit_size"); work.slots[work.current].state.exit = 1; work.slots[work.current].state.editor.clear(); work.slots[work.current].state.input_history.reset(); }
                                    Action::Eof => finished |= work.eof(io, false),
                                    Action::ToggleHistory | Action::ToggleMode => {}
                                }
                            }
                            if finished || work.input_closed { break; }
                            if work.slots[work.current].state.pending_draft.is_some() { deferred_input.extend(&bytes[position + 1..count]); break; }
                        }
                    }
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) => {}
                    Err(_) => return Err("cli_input_read"),
                }
            }
            work.close_owners();
            if work.jobs.is_empty() && work.pending_context.is_empty() && work.slots.iter().all(|slot| slot.closed) { finished = true; }
            if dirty && !finished && work.editor.is_none() && work.pending_editor.is_none() && !work.editor_preparing { work.render(io, context); dirty = false; }
            if let Some(mut guard) = input_guard && !io.input.ready(false).map_err(|_| "cli_input_poll")? { guard.clear_ready(); }
            if let Some(mut guard) = signal_guard && !signal_events.get_ref().ready(false).map_err(|_| "cli_signal_poll")? { guard.clear_ready(); }
            if io.writer.failed() { return Err("cli_output"); }
            tokio::task::yield_now().await;
        }
        Ok(())
    }.await;
    work.stop.cancel();
    work.cancel_modal();
    work.terminating = true;
    work.cancel_editor_request();
    work.events.close();
    if let Err(code) = result {
        failure.get_or_insert(code);
    }
    if let Some(mut editor) = work.editor.take() {
        if let Err(code) = editor.cancel() {
            failure.get_or_insert(code);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if Instant::now() >= deadline {
                if let Err(code) = editor.abort_for_exit(io) {
                    failure.get_or_insert(code);
                }
                failure.get_or_insert("editor_cleanup_unknown");
                break;
            }
            match editor.poll(io) {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(code) => {
                    failure.get_or_insert(code);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    for slot in &work.slots {
        slot.handle.request_shutdown();
    }
    while let Some(result) = work.jobs.join_next().await {
        if result.is_err() {
            failure.get_or_insert("workspace_job");
        }
    }
    for slot in &mut work.slots {
        while let Some(result) = slot.replies.join_next().await {
            if result.is_err() {
                failure.get_or_insert("cli_post_task");
            }
        }
        if let Err(code) = slot.runtime.shutdown().await {
            failure.get_or_insert(code);
        }
        if exit < 128 && slot.state.exit != 0 {
            exit = 1;
        }
        if exit < 128
            && slot
                .state
                .snapshot
                .as_ref()
                .is_some_and(|s| s.storage_failed || s.state == "paused")
        {
            exit = 1;
        }
    }
    while let Some(result) = work.pumps.join_next().await {
        if result.is_err() {
            failure.get_or_insert("workspace_event_task");
        }
    }
    if let Err(code) = io.stop_reporting() {
        failure.get_or_insert(code);
    }
    if !io.writer.failed() {
        io.writer.text("\x1b[1B\r\n".into());
    }
    let mut drain = Drain::default();
    let drain_began = Instant::now();
    while !io.writer.idle() {
        if exit >= 128 && drain_began.elapsed() >= Duration::from_millis(250) {
            failure.get_or_insert("cli_output_interrupted");
            io.writer.abort();
            break;
        }
        if drain.stalled(io.writer.written(), io.writer.idle(), Instant::now()) {
            failure.get_or_insert("cli_output_stalled");
            io.writer.abort();
            break;
        }
        tokio::select! {
            ready = signal_events.readable() => {
                let mut guard = ready.map_err(|_| "cli_signal_poll").ok();
                while let Ok(Some(signal)) = io.signals.take() {
                    use ansa_linux::terminal::*;
                    match signal {
                        SIGTERM | SIGHUP => exit = 128 + signal,
                        SIGINT => { if exit < 128 { exit = 130; } io.writer.abort(); }
                        _ => {}
                    }
                }
                if let Some(guard) = &mut guard { guard.clear_ready(); }
            }
            _ = io.writer.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    if !io.writer.finish() {
        failure.get_or_insert("cli_output");
    }
    if let Some(code) = failure {
        io.status(&format!("ansa: {}\n", diagnostics::error(code)));
    }
    exit_result(exit, failure)
}

fn exit_result(exit: i32, failure: Option<&'static str>) -> Result<i32> {
    if exit >= 128 {
        return Ok(exit);
    }
    failure.map_or(Ok(exit), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_replies_match_file_width_and_preserve_only_preview_anchors() {
        let document = super::super::super::markdown::Document::parse(&"word ".repeat(40));
        let old = std::sync::Arc::new(document.rows(16));
        let rows = std::sync::Arc::new(document.rows(30));
        let mut files = Files {
            token: 7,
            listing: None,
            document: None,
            selected: 0,
            tab: 2,
            offset: 4,
            loading: false,
            jump: None,
            preview_cache: Some((16, old)),
            preview_width: Some(30),
            preview_end: false,
        };
        assert!(files.preview_rows().is_none());
        assert!(!files.accept_preview(6, 30, Some(rows.clone())));
        assert!(!files.accept_preview(7, 16, Some(rows.clone())));
        assert!(!files.accept_preview(7, 30, None));
        assert_eq!(files.offset, 4);
        files.offset = files.offset.saturating_add(1).min(files.last_line());
        assert_eq!(
            files.offset, 5,
            "navigation during resize must retain the old reading anchor"
        );
        let anchor = files.preview_cache.as_ref().unwrap().1[files.offset].anchor;
        assert!(files.accept_preview(7, 30, Some(rows.clone())));
        assert_eq!(
            files.offset,
            super::super::super::markdown::Row::locate(&rows, anchor)
        );
        assert!(std::sync::Arc::ptr_eq(files.preview_rows().unwrap(), &rows));
        files.preview_width = Some(11);
        files.preview_end = true;
        let narrow = std::sync::Arc::new(document.rows(11));
        assert!(files.accept_preview(7, 11, Some(narrow.clone())));
        assert_eq!(
            files.offset,
            narrow.len() - 1,
            "pending G must show the new layout's actual tail"
        );
        assert!(!files.preview_end);

        files.tab = 0;
        files.offset = 37;
        files.preview_width = Some(40);
        assert!(files.accept_preview(7, 40, Some(std::sync::Arc::new(document.rows(40)))));
        assert_eq!(
            files.offset, 37,
            "Source uses source lines, not preview row anchors"
        );
        files.preview_width = None;
        assert!(files.preview_rows().is_none());
        assert!(!files.accept_preview(7, 40, Some(rows)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn history_top_waits_for_its_page_and_cancelled_jump_keeps_the_tail() {
        let runtime = super::super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let mut slot = Slot::new(runtime, "offline".into(), Kind::Main, None, None, false);
        slot.state.scroll = 2;
        slot.jump_history(true);
        assert_eq!(slot.state.scroll, 2);
        // A paint of the old, shorter page is allowed to clamp its scroll.
        slot.state.scroll = 0;
        let page = TranscriptPage {
            begin: 0,
            end: 1,
            total: 80,
            entries: vec![],
            query: slot.timeline_query.clone(),
        };
        slot.received_timeline(&page);
        let frame = carbon::layout_full(
            &[Entry {
                role: "user".into(),
                text: format!("EARLIEST\n{}", "later\n".repeat(50)),
                action: None,
            }],
            b"draft",
            5,
            "",
            slot.state.scroll,
            "",
            (60, 16),
            false,
        )
        .unwrap();
        assert!(
            frame
                .plain_lines()
                .iter()
                .any(|line| line.contains("EARLIEST"))
        );
        assert!(
            frame
                .plain_lines()
                .iter()
                .filter(|line| line.contains("later"))
                .count()
                > 3
        );
        slot.jump_history(true);
        slot.jump_history(false);
        let tail = TranscriptPage {
            begin: 16,
            end: 80,
            total: 80,
            entries: vec![],
            query: slot.timeline_query.clone(),
        };
        slot.received_timeline(&tail);
        assert_eq!(slot.state.scroll, 0);
        assert!(slot.state.follow_tail && !slot.jump_top);
        for _ in 0..2 {
            slot.jump_history(true);
            slot.cancel_history_jump();
            assert!(!slot.timeline_query.forward);
            assert_eq!(slot.timeline_query.before, tail.end);
            assert!(!slot.jump_top);
        }
        slot.runtime.shutdown().await.unwrap();
    }
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct ReleaseInit(Arc<AtomicBool>);
    impl Drop for ReleaseInit {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn held_initial_sync_requires_ready_and_fresh_snapshot_before_first_prompt() {
        use crate::{
            provider::request::{Epoch, Request},
            runtime::{Limits, Options},
            session::journal_v1::Point,
            tools::registry::{Outcome, Policy, Registry},
        };
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = TestDirectory(std::env::temp_dir().join(format!(
            "ansa-workbench-ready-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory.0)
            .unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let generated = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::new();
        let mut definition = crate::frontend::app::pure_tools()
            .unwrap()
            .definition("get_test_value")
            .unwrap()
            .clone();
        let count = executed.clone();
        definition.executor = Arc::new(move |_| {
            count.fetch_add(1, Ordering::AcqRel);
            Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into())
        });
        registry
            .register_with_policy(
                definition,
                Policy {
                    read_only: true,
                    ..Policy::default()
                },
            )
            .unwrap();
        let tools = registry.snapshot();
        let request = Request::new(
            crate::frontend::app::binding(),
            Epoch {
                id: "ready-fixture".into(),
                instructions: "fixture".into(),
                exact_state: Json::parse("{}").unwrap(),
            },
            tools.advertisement().clone(),
            vec![],
        )
        .unwrap();
        let held = entered.clone();
        let unlock = release.clone();
        let count = generated.clone();
        let runtime = Runtime::new(
            "ready-fixture".into(),
            request,
            tools,
            Arc::new(move |request, stop, _| {
                count.fetch_add(1, Ordering::AcqRel);
                crate::frontend::offline::generate(request, stop)
            }),
            Limits::default(),
            Options {
                directory: Some(directory.0.join("journal")),
                journal: Some(Arc::new(move |point, kind| {
                    if point == Point::BeforeSync && kind == "init" {
                        held.store(true, Ordering::Release);
                        let until = Instant::now() + Duration::from_secs(5);
                        while !unlock.load(Ordering::Acquire) {
                            if Instant::now() >= until {
                                return Err("fixture_init_release_timeout");
                            }
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    Ok(())
                })),
                ..Options::default()
            },
        )
        .unwrap();
        let mut slot = Slot::new(
            runtime,
            "ready-fixture".into(),
            Kind::Main,
            None,
            Some("buffered task".into()),
            false,
        );
        let handle = slot.handle.clone();
        let (sender, mut events) = mpsc::channel(32);
        let mut pumps = JoinSet::new();
        pumps.spawn(pump_session(handle.clone(), slot.id.clone(), sender));
        let _release_on_panic = ReleaseInit(release.clone());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let command = slot.state.command(&handle, CommandPayload::GetSnapshot(0));
        let early_id = command.command.clone();
        handle.post(command).await.unwrap();
        let early = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match events.recv().await.unwrap().1 {
                    SessionEvent::Runtime(RuntimeEvent {
                        payload: EventPayload::FrontendSnapshot(snapshot),
                        ..
                    }) if snapshot.command.as_ref() == Some(&early_id) => break snapshot,
                    SessionEvent::Ready(_) => panic!("ready before held init sync"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(early.state, "loading");
        // This is the original bad combination: an early empty history page
        // does not make a loading runtime accept an interactive prompt.
        slot.state.history_ready = true;
        assert!(!slot.accepts_snapshot(&early));
        assert!(!slot.input_ready());
        assert!(!slot.state.mode_ready);
        assert_eq!(slot.pending_prompt.as_deref(), Some("buffered task"));
        assert_eq!(generated.load(Ordering::Acquire), 0);
        assert_eq!(executed.load(Ordering::Acquire), 0);
        release.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let SessionEvent::Ready(result) = events.recv().await.unwrap().1 {
                    result.unwrap();
                    slot.became_ready();
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(!slot.input_ready());
        let command = slot.state.command(&handle, CommandPayload::GetSnapshot(0));
        slot.ready_snapshot = Some(command.command.clone());
        handle.post(command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let SessionEvent::Runtime(RuntimeEvent {
                    payload: EventPayload::FrontendSnapshot(snapshot),
                    ..
                }) = events.recv().await.unwrap().1
                    && slot.accepts_snapshot(&snapshot)
                {
                    assert_eq!(snapshot.state, "idle");
                    slot.state.snapshot = Some(*snapshot);
                    break;
                }
            }
        })
        .await
        .unwrap();
        let query = slot.timeline_query.clone();
        handle
            .post(
                slot.state
                    .command(&handle, CommandPayload::GetTimeline(query)),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let SessionEvent::Runtime(RuntimeEvent {
                    payload: EventPayload::TimelinePage(page),
                    ..
                }) = events.recv().await.unwrap().1
                {
                    slot.timeline = Some(page);
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(slot.input_ready());
        let text = slot.pending_prompt.take().unwrap();
        handle
            .post(slot.state.command(
                &handle,
                CommandPayload::Submit(Submit {
                    text,
                    mode: AgentMode::Basic,
                }),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let SessionEvent::Runtime(RuntimeEvent {
                    payload: EventPayload::FrontendSnapshot(snapshot),
                    ..
                }) = events.recv().await.unwrap().1
                    && snapshot.state == "completed"
                {
                    assert!(snapshot.durable);
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(executed.load(Ordering::Acquire), 1);
        events.close();
        slot.runtime.shutdown().await.unwrap();
        while let Some(result) = pumps.join_next().await {
            result.unwrap();
        }
    }
    #[test]
    fn completed_frame_is_selected_without_an_intervening_render() {
        let frame = |text: &str| {
            carbon::layout_full(
                &[Entry {
                    action: None,
                    role: "user".into(),
                    text: text.into(),
                }],
                b"",
                0,
                "basic",
                0,
                "",
                (40, 12),
                false,
            )
            .unwrap()
        };
        let old = frame("old-visible");
        let new = frame("new-visible");
        let mut displayed = Some(old);
        let mut pending = std::collections::VecDeque::from([(2, new)]);
        sync_displayed(&mut pending, &mut displayed, |sequence| sequence <= 2);
        let visible = displayed.unwrap();
        let row = visible
            .plain_lines()
            .iter()
            .position(|line| line.contains("new-visible"))
            .unwrap();
        let mut selection = Selection::new(visible, 0, row);
        let text = selection.finish(39, row).unwrap();
        assert!(text.contains("new-visible"));
        assert!(!text.contains("old-visible"));
        assert!(pending.is_empty());
    }
    #[test]
    fn cleanup_failures_preserve_terminal_signal_exit() {
        for exit in [129, 143] {
            assert_eq!(exit_result(exit, Some("cli_output_stalled")), Ok(exit));
        }
        assert_eq!(
            exit_result(0, Some("cli_output_stalled")),
            Err("cli_output_stalled")
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn expanding_running_tool_keeps_canonical_arguments_and_purpose_visible() {
        let runtime = super::super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let mut view = loop {
            if let EventPayload::FrontendSnapshot(view) =
                runtime.next_event().await.unwrap().payload
            {
                break *view;
            }
        };
        view.state = "tools".into();
        view.worker_busy = true;
        view.request = Some("task".into());
        view.timeline_size = 1;
        view.tools = vec![Activity::Started(Started {
            operation: "op".into(),
            call_id: "call".into(),
            name: "get_test_value".into(),
        })];
        let mut slot = Slot::new(runtime, "offline".into(), Kind::Main, None, None, false);
        slot.state.snapshot = Some(view);
        slot.state
            .progress
            .observe(slot.state.snapshot.as_ref().unwrap(), Instant::now());
        assert!(
            !slot
                .state
                .progress
                .entries(slot.state.snapshot.as_ref().unwrap(), true, Instant::now())
                .is_empty()
        );
        slot.timeline = Some(TranscriptPage {
            begin: 0,
            end: 1,
            total: 1,
            query: slot.timeline_query.clone(),
            entries: vec![transcript::Entry {
                tool: None,
                turn: "turn".into(),
                request: "task".into(),
                role: "tool".into(),
                text: "get_test_value (prepared)\ndescriptor purpose\nARGS_CANARY".into(),
                state: "intent".into(),
                mode: AgentMode::Basic,
            }],
        });
        assert!(
            !slot
                .projected()
                .unwrap()
                .iter()
                .any(|e| e.text.contains("ARGS_CANARY"))
        );
        slot.state.tools_expanded = true;
        let shown = slot
            .projected()
            .unwrap()
            .into_iter()
            .map(|e| e.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(shown.contains("descriptor purpose") && shown.contains("ARGS_CANARY"));
        assert!(shown.contains("выполняется"));
        slot.runtime.shutdown().await.unwrap();
    }
}
