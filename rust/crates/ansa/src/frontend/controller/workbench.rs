//! Interactive workspace. Each open conversation retains its own runtime owner,
//! draft, cursor, transcript page and bounded reply tasks.
use super::*;
use crate::{
    auth::Cancel,
    frontend::{
        carbon::{Choice, Focus, Inspector, InspectorKind, Popup, Selection, Workspace},
        commands::{self, Completion},
        graphics,
        input::{MouseEvent, MouseKind},
        orchestration::{self, Proposal},
        sessions::{Created, Factory, Kind},
        theme::{Catalog as ThemeCatalog, Preview as ThemePreview, Store as ThemeStore, Theme},
        workspace::{DiffStatus, EditorCommand, FileDocument, FileHost, FileListing},
    },
    orchestration::run::RunId,
    protocol::json::Json,
    session::catalog::SessionSummary,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::sync::mpsc;
mod ux;

const MAX_SESSIONS: usize = 16;
const MAX_BACKGROUND: usize = 4;
const MAX_JOBS: usize = 4;

fn display_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(72)
        .collect()
}

struct Slot {
    id: String,
    kind: Kind,
    parent: Option<String>,
    title: Option<String>,
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
    follow_accepted_submit: Option<CommandId>,
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
    images_through: Option<usize>,
    candidate_reviews: std::collections::VecDeque<ux::CandidateView>,
}
impl Slot {
    fn new(
        runtime: Runtime,
        id: String,
        kind: Kind,
        parent: Option<String>,
        prompt: Option<String>,
    ) -> Self {
        let handle = runtime.handle();
        let mut state = State::new(&handle);
        if kind == Kind::Btw {
            state.mode = AgentMode::Plan;
            state.mode_ready = true;
        }
        let title = prompt.as_deref().map(display_title);
        Self {
            id,
            kind,
            parent,
            title,
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
            follow_accepted_submit: None,
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
            images_through: None,
            candidate_reviews: std::collections::VecDeque::new(),
        }
    }
    #[cfg(test)]
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
    fn show_latest(&mut self) {
        self.jump_top = false;
        self.state.follow_tail = true;
        self.state.reading = None;
        self.state.scroll = 0;
        self.timeline_query = GetTranscript {
            before: 0,
            limit: 64,
            forward: false,
        };
        self.refresh = Instant::now();
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
    fn new_image(&mut self, page: &TranscriptPage) -> Option<graphics::Source> {
        if page.end != page.total {
            return None;
        }
        let previous = self.images_through.replace(page.end)?;
        page.entries
            .iter()
            .skip(previous.saturating_sub(page.begin))
            .rev()
            .find_map(|entry| {
                graphics::Source::from_entry(entry, &self.id).map(|(_, source)| source)
            })
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
    fn interruptible(&self) -> bool {
        self.active()
            || self
                .state
                .snapshot
                .as_ref()
                .is_some_and(|view| view.state == "paused")
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
            self.state.read_replied(&id);
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
                } else if self.state.pending.as_ref() != Some(&id) {
                    // A Steer reply follows its journal write. Submit replies
                    // only acknowledge queueing; the runtime event confirms it.
                    self.show_latest();
                }
            }
            if let Some(code) = code {
                if self.follow_accepted_submit.as_ref() == Some(&id) {
                    self.follow_accepted_submit = None;
                }
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
        self.state.recall_page(&self.handle, &mut self.replies);
        self.state.copy_page(&self.handle, &mut self.replies, io);
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
        } else if self.active()
            || self
                .state
                .snapshot
                .as_ref()
                .is_some_and(|s| s.state == "paused")
        {
            if let Some(target) = self.state.snapshot.as_ref().and_then(|s| s.request.clone()) {
                self.state.post(
                    &self.handle,
                    CommandPayload::Cancel(target),
                    &mut self.replies,
                );
            }
        } else {
            self.state.report(io, "No active step");
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
        for row in page.entries.iter().rev() {
            let Some((operation, png)) = graphics::Source::from_entry(row, &self.id) else {
                continue;
            };
            let tool_key = format!("{}:{operation}", row.request.0);
            let Some(index) = entries
                .iter()
                .position(|entry| entry.action.as_deref() == Some(&tool_key))
            else {
                continue;
            };
            entries.insert(
                index + 1,
                Entry {
                    action: Some(format!("image:{operation}")),
                    edit: None,
                    output: None,
                    status: None,
                    role: "tool_success".into(),
                    text: format!("> Open chart · {}", png.name()),
                },
            );
        }
        if page.end == page.total {
            for message in view
                .followups
                .iter()
                .filter(|message| message.state == "queued")
            {
                entries.push(Entry {
                    action: None,
                    edit: None,
                    output: None,
                    status: None,
                    role: "queued_user".into(),
                    text: message.text.clone(),
                });
            }
            if let Some(preview) = &self.state.preview
                && !preview.text.is_empty()
                && preview.state != "committed"
                && self.state.preview_request == view.request
            {
                entries.push(Entry {
                    action: None,
                    edit: None,
                    output: None,
                    status: None,
                    role: "assistant".into(),
                    text: format!(
                        "{}\n({})",
                        preview.text,
                        if preview.state == "discarded" {
                            "incomplete response"
                        } else {
                            "response preview"
                        }
                    ),
                });
            }
        }
        if page.end == page.total
            && view.worker_busy
            && !matches!(view.state.as_str(), "completed" | "cancelled" | "paused")
        {
            entries.push(Entry {
                action: None,
                edit: None,
                output: None,
                status: None,
                role: "activity".into(),
                text: format!(
                    "{} · {}",
                    Progress::phase(view, self.state.preview.is_some()),
                    self.state.progress.elapsed().unwrap_or_default()
                ),
            });
        }
        Some(entries)
    }
}

struct CreateIntent {
    source: String,
    kind: Kind,
    prompt: Option<String>,
    foreground: bool,
    plan_only: bool,
}
enum FileValue {
    Listing(FileListing),
    Document(FileDocument),
    Image(graphics::Image),
}
enum Job {
    OrchestrationPlan {
        owner: String,
        token: u64,
        result: Result<Proposal>,
    },
    OrchestrationDone {
        owner: String,
        result: Result<orchestration::Completed>,
    },
    OrchestrationReceipt {
        result: Result<()>,
    },
    ArchivedPlan {
        token: u64,
        result: Result<orchestration::ArchivedPlan>,
    },
    DraftPullRequest {
        owner: String,
        result: Result<String>,
    },
    Themes {
        request: ThemeLoad,
        result: Result<ThemeCatalog>,
    },
    ThemeSaved {
        revision: u64,
        theme: Theme,
        result: Result<()>,
    },
    ThemeReload {
        revision: u64,
        result: Result<Theme>,
    },
    Created {
        source: String,
        prompt: Option<String>,
        foreground: bool,
        plan_only: bool,
        result: Result<Created>,
    },
    Sessions {
        owner: String,
        token: u64,
        result: Result<Vec<SessionSummary>>,
        continuation: Option<(RunId, String)>,
    },
    CachedModels {
        result: Result<Option<Json>>,
    },
    GenerationSaved {
        owner: String,
        result: Result<()>,
    },
    Models {
        owner: String,
        token: u64,
        effort: bool,
        result: Result<Json>,
    },
    CachedSummaries {
        result: Result<Vec<SessionSummary>>,
    },
    File {
        token: u64,
        result: Result<FileValue>,
    },
    TerminalScreen {
        token: u64,
        result: Result<carbon::OutputView>,
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
enum ThemeLoad {
    Startup,
    Picker {
        owner: String,
        token: u64,
        requested: Option<String>,
    },
}
enum Menu {
    Orchestration {
        proposal: Box<Proposal>,
        selected: usize,
    },
    Themes(ThemePreview),
    Loading(String),
    Sessions {
        entries: Vec<SessionSummary>,
        selected: usize,
        resume_target: Option<RequestId>,
        continuation: Option<(RunId, String)>,
    },
    Models {
        models: Vec<(String, String, Vec<String>)>,
        selected: usize,
    },
    Efforts {
        model: String,
        previous: Option<(ModelChoices, usize)>,
        values: Vec<Option<String>>,
        selected: usize,
    },
    Help {
        selected: usize,
        entries: &'static [commands::Command],
    },
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
    Orchestration {
        lines: Vec<String>,
        offset: usize,
    },
    Files(Box<Files>),
    Agents {
        selected: usize,
    },
    Terminal,
    NativeTerminal {
        token: u64,
        name: String,
        view: Option<carbon::OutputView>,
        pending: bool,
        next: Instant,
    },
    Image {
        token: u64,
        name: String,
        view: Option<graphics::Image>,
    },
    Output {
        view: std::sync::Arc<carbon::OutputView>,
        offset: usize,
    },
    EditDiff {
        title: String,
        rows: std::sync::Arc<Vec<super::super::diff::Row>>,
        offset: usize,
        column: usize,
    },
}
type ModelChoices = Vec<(String, String, Vec<String>)>;
type ToolPress = (
    usize,
    usize,
    String,
    Option<std::sync::Arc<super::super::diff::EditView>>,
    Option<std::sync::Arc<carbon::OutputView>>,
);
struct Workbench {
    factory: Factory,
    files: FileHost,
    theme_store: ThemeStore,
    theme: Theme,
    saved_theme: Theme,
    theme_initialized: bool,
    theme_revision: u64,
    theme_save: Option<u64>,
    pending_theme: Option<(u64, Theme)>,
    theme_reload: bool,
    theme_refresh: Instant,
    theme_error: Option<&'static str>,
    slots: Vec<Slot>,
    current: usize,
    root_id: String,
    pumps: JoinSet<()>,
    events: mpsc::Receiver<(String, SessionEvent)>,
    sender: mpsc::Sender<(String, SessionEvent)>,
    jobs: JoinSet<Job>,
    stop: Cancel,
    pending_context: BTreeMap<String, CreateIntent>,
    planning_context: Option<(CommandId, String, String, u64)>,
    orchestration_stop: Option<(String, Cancel)>,
    orchestration_progress: Option<tokio::sync::watch::Receiver<Option<orchestration::RunView>>>,
    orchestration_views: BTreeMap<String, orchestration::RunView>,
    orchestration_handoffs: BTreeMap<String, String>,
    orchestration_summary: BTreeMap<String, CommandId>,
    orchestration_mode: bool,
    refinement: Option<String>,
    menu: Option<Menu>,
    completion: Option<Completion>,
    navigation_editor: super::super::input::Editor,
    completion_closed: Option<(Vec<u8>, usize)>,
    models_cache: Option<Json>,
    models_refreshed: Option<Instant>,
    models_refreshing: bool,
    requested_model: Option<String>,
    summaries: Vec<SessionSummary>,
    panel: Option<Panel>,
    output_back: Option<Box<Panel>>,
    terminal_back: Option<Box<Files>>,
    terminal: Option<super::super::terminal_pane::TerminalPane>,
    terminal_decoder: super::super::terminal_input::TerminalInput,
    terminal_escape_at: Option<Instant>,
    terminal_pending_input: Vec<u8>,
    terminal_grid: Option<(u64, std::sync::Arc<Vec<String>>)>,
    terminal_size: Option<(u16, u16)>,
    focus: Focus,
    selection: Option<Selection>,
    selection_frame: Option<(u64, Selection)>,
    next_drag_scroll: Option<Instant>,
    tool_press: Option<ToolPress>,
    result_selected: Option<String>,
    agents_return: Option<(String, usize)>,
    result_scroll: Option<usize>,
    result_tail: bool,
    review_redraw: bool,
    wide_diff: bool,
    frames: std::collections::VecDeque<(u64, carbon::Frame)>,
    displayed: Option<carbon::Frame>,
    operational_frame: Option<u64>,
    last_frame: Option<carbon::Frame>,
    file_token: u64,
    graphics_id: Option<u32>,
    preview_job: Option<PreviewJob>,
    editor: Option<super::super::terminal::EditorJob>,
    pending_editor: Option<EditorCommand>,
    editor_preparing: bool,
    editor_request: u64,
    editor_wait: Option<EditorWait>,
    modal_token: u64,
    modal_stop: Option<Cancel>,
    exit_deadline: Option<Instant>,
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
        let theme_store = ThemeStore::new(&factory.settings.home, &factory.settings.directory);
        let (sender, events) = mpsc::channel(64);
        let mut value = Self {
            factory,
            files,
            theme_store,
            theme: Theme::default(),
            saved_theme: Theme::default(),
            theme_initialized: false,
            theme_revision: 0,
            theme_save: None,
            pending_theme: None,
            theme_reload: false,
            theme_refresh: Instant::now(),
            theme_error: None,
            slots: Vec::new(),
            current: 0,
            root_id: root_id.clone(),
            pumps: JoinSet::new(),
            events,
            sender,
            jobs: JoinSet::new(),
            stop: Cancel::default(),
            pending_context: BTreeMap::new(),
            planning_context: None,
            orchestration_stop: None,
            orchestration_progress: None,
            orchestration_views: BTreeMap::new(),
            orchestration_handoffs: BTreeMap::new(),
            orchestration_summary: BTreeMap::new(),
            orchestration_mode: false,
            refinement: None,
            menu: None,
            completion: None,
            navigation_editor: Default::default(),
            completion_closed: None,
            models_cache: None,
            models_refreshed: None,
            models_refreshing: false,
            requested_model: None,
            summaries: Vec::new(),
            panel: None,
            output_back: None,
            terminal_back: None,
            terminal: None,
            terminal_decoder: Default::default(),
            terminal_escape_at: None,
            terminal_pending_input: Vec::new(),
            terminal_grid: None,
            terminal_size: None,
            focus: Focus::Chat,
            selection: None,
            selection_frame: None,
            next_drag_scroll: None,
            tool_press: None,
            result_selected: None,
            agents_return: None,
            result_scroll: None,
            result_tail: false,
            review_redraw: false,
            wide_diff: false,
            frames: std::collections::VecDeque::new(),
            displayed: None,
            operational_frame: None,
            last_frame: None,
            file_token: 0,
            graphics_id: graphics::supported().then(graphics::image_id).flatten(),
            preview_job: None,
            editor: None,
            pending_editor: None,
            editor_preparing: false,
            editor_request: 0,
            editor_wait: None,
            modal_token: 0,
            modal_stop: None,
            input_closed: false,
            exit_deadline: None,
            terminating: false,
        };
        value.add(Slot::new(runtime, root_id, kind, parent, None));
        let store = value.theme_store.clone();
        let stop = Cancel::linked(&value.stop);
        value.jobs.spawn_blocking(move || Job::Themes {
            request: ThemeLoad::Startup,
            result: store.load(&stop),
        });
        let factory = value.factory.clone();
        let stop = Cancel::linked(&value.stop);
        value.jobs.spawn_blocking(move || Job::CachedModels {
            result: factory.cached_models(&stop),
        });
        let factory = value.factory.clone();
        let stop = Cancel::linked(&value.stop);
        value.jobs.spawn_blocking(move || Job::CachedSummaries {
            result: factory.summaries(&stop),
        });
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
    fn disarm_exit(&mut self) -> bool {
        self.exit_deadline.take().is_some()
    }
    fn interrupt(&mut self, io: &Io, now: Instant) -> bool {
        let slot = &mut self.slots[self.current];
        if !slot.state.editor.bytes.is_empty()
            || slot.state.editor.overflow
            || slot.state.editor.pasting()
            || slot.state.editor.escape_pending()
        {
            slot.state.editor.clear();
            slot.state.input_history.reset();
            self.completion = None;
            self.completion_closed = None;
            self.disarm_exit();
        } else if self.menu.is_some() {
            self.disarm_exit();
            self.escape(io);
        } else if let Some((owner, stop)) = &self.orchestration_stop
            && owner == &slot.id
        {
            stop.cancel();
            self.disarm_exit();
            self.report(io, "Stopping the current task");
        } else if slot.interruptible() {
            self.disarm_exit();
            self.cancel_current(io);
        } else if self.exit_deadline.is_some_and(|deadline| now < deadline) {
            self.disarm_exit();
            return true;
        } else {
            self.exit_deadline = Some(now + Duration::from_secs(2));
        }
        false
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
            slot.state.report(io, "Session open cancelled");
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
        self.disarm_exit();
        self.cancel_modal();
        self.cancel_editor_request();
        self.current = index;
        self.orchestration_mode = false;
        self.refinement = None;
        self.slots[index].refresh = Instant::now();
        self.slots[index].resync = true;
        self.menu = None;
        self.completion = None;
        self.completion_closed = None;
        self.requested_model = None;
        self.terminal_decoder.take_pending();
        self.terminal_escape_at = None;
        self.panel = None;
        self.focus = Focus::Chat;
        self.selection = None;
        self.result_selected = None;
        self.result_scroll = None;
        self.result_tail = false;
        self.review_redraw = false;
        if self
            .agents_return
            .as_ref()
            .is_some_and(|(id, _)| id == &self.slots[index].id)
            && let Some((_, selected)) = self.agents_return.take()
        {
            self.panel = Some(Panel::Agents { selected });
            self.focus = Focus::Inspector;
        }
        self.wide_diff = false;
        self.tool_press = None;
        self.last_frame = None;
        self.frames.clear();
        self.displayed = None;
        self.operational_frame = None;
    }
    fn can_job(&mut self, io: &Io) -> bool {
        if self.jobs.len() + self.pending_context.len() >= MAX_JOBS {
            self.report(io, "Wait for the current window to open");
            false
        } else {
            true
        }
    }
    fn create(&mut self, kind: Kind, prompt: Option<String>, foreground: bool, io: &Io) -> bool {
        if self.slots.len() + self.jobs.len() + self.pending_context.len() >= MAX_SESSIONS {
            self.report(io, "Open session limit reached (16)");
            return false;
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
            self.report(io, "Up to four background conversations can run");
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        if kind == Kind::Main {
            let slot = &self.slots[self.current];
            let source = slot.id.clone();
            let generation = slot
                .state
                .snapshot
                .as_ref()
                .map(|view| view.generation.clone());
            let generation = generation.filter(|_| slot.kind != Kind::Console);
            let factory = self.factory.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                result: factory.create_main(generation),
                source,
                prompt: None,
                foreground,
                plan_only: false,
            });
            return true;
        }
        if kind == Kind::Console {
            if self.slots[self.current].active() {
                self.report(io, "The command panel is available once the task stops");
                return false;
            }
            if let Some(index) = self.slots.iter().position(|s| {
                s.kind == Kind::Console
                    && s.parent.as_deref() == Some(self.slots[self.current].id.as_str())
                    && !s.active()
            }) {
                self.switch(index);
                self.slots[index].pending_prompt = prompt;
                return true;
            }
            let source = self.slots[self.current].id.clone();
            let factory = self.factory.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                result: factory.create_console(&source),
                source,
                prompt,
                foreground,
                plan_only: false,
            });
            return true;
        }
        let slot = &mut self.slots[self.current];
        let command = slot.state.command(&slot.handle, CommandPayload::GetContext);
        let id = command.command.clone();
        let key = id.0.clone();
        if !slot
            .state
            .post_command(&slot.handle, command, &mut slot.replies, false)
        {
            slot.state.report(io, "Wait for the current requests");
            return false;
        }
        self.pending_context.insert(
            key,
            CreateIntent {
                source: slot.id.clone(),
                kind,
                prompt,
                foreground,
                plan_only: slot.state.mode == AgentMode::Plan,
            },
        );
        self.report(io, "Creating session");
        true
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
        let factory = self.factory.clone();
        let stop = self.stop.clone();
        self.jobs.spawn_blocking(move || {
            let result = match host.list(&path, &stop) {
                Ok(listing) => Ok(FileValue::Listing(listing)),
                Err("workspace_directory") if graphics::is_png(&path) => {
                    let source = graphics::Source::File { path, hash: None };
                    source.load(&host, &factory).map(FileValue::Image)
                }
                Err("workspace_directory") => host.open(&path, &stop).map(FileValue::Document),
                Err(code) => Err(code),
            };
            Job::File { token, result }
        });
    }
    fn show_image(&mut self, source: graphics::Source, io: &Io) {
        if self.input_closed || self.terminating || !self.can_job(io) {
            return;
        }
        self.file_token = self.file_token.wrapping_add(1);
        let token = self.file_token;
        self.panel = Some(Panel::Image {
            token,
            name: source.name(),
            view: None,
        });
        self.selection = None;
        let host = self.files.clone();
        let factory = self.factory.clone();
        self.jobs.spawn_blocking(move || {
            let result = source.load(&host, &factory).map(FileValue::Image);
            Job::File { token, result }
        });
    }

    fn inspect_terminal(&mut self, name: &str, io: &Io) {
        if self
            .factory
            .terminal_owner(&self.slots[self.current].id)
            .is_none()
        {
            self.report(io, "Open a new conversation with --native-terminals");
            return;
        }
        self.file_token = self.file_token.wrapping_add(1);
        self.panel = Some(Panel::NativeTerminal {
            token: self.file_token,
            name: name.to_owned(),
            view: None,
            pending: false,
            next: Instant::now(),
        });
        self.focus = Focus::Inspector;
        self.selection = None;
        self.refresh_terminal_screen(Instant::now());
    }

    fn refresh_terminal_screen(&mut self, now: Instant) {
        if self.input_closed || self.terminating || self.jobs.len() >= 6 {
            return;
        }
        let Some(Panel::NativeTerminal {
            token,
            name,
            pending,
            next,
            ..
        }) = &mut self.panel
        else {
            return;
        };
        if *pending || now < *next {
            return;
        }
        let Some(owner) = self.factory.terminal_owner(&self.slots[self.current].id) else {
            return;
        };
        *pending = true;
        *next = now + Duration::from_millis(300);
        let token = *token;
        let name = name.clone();
        self.jobs.spawn_blocking(move || {
            use crate::terminal_host::{Lifecycle, protocol::Writer};
            let result = (|| {
                let mut host = owner.lock().map_err(|_| "terminal_owner_unavailable")?;
                let (id, terminal) = host
                    .terminals()
                    .iter()
                    .find(|(id, terminal)| {
                        terminal.lifecycle == Lifecycle::Live
                            && (name.is_empty()
                                || name == id.as_str()
                                || name == terminal.launch.label)
                    })
                    .ok_or("terminal_not_open")?;
                let (id, epoch) = (id.clone(), terminal.epoch);
                let mut title = terminal.launch.label.clone();
                let screen = host.capture(Writer::Human, &id, epoch, 0)?;
                if let Some(exit) = &screen.exit {
                    title.push_str(if exit.cleanup_known {
                        " · exited"
                    } else {
                        " · stop unconfirmed"
                    });
                }
                let mut text = screen.text;
                if screen.truncated {
                    text.push_str("\nScreen truncated");
                }
                Ok(carbon::OutputView { title, text })
            })();
            Job::TerminalScreen { token, result }
        });
    }
    fn copy_image_path(&mut self, io: &Io) {
        let Some(Panel::Image {
            view: Some(image), ..
        }) = &self.panel
        else {
            return;
        };
        let copied = io.writer.clipboard(&image.path.to_string_lossy());
        self.report(
            io,
            if copied {
                "Path copied"
            } else {
                "Could not copy the path"
            },
        );
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
            || self.orchestration_stop.is_some()
        {
            self.report(io, "The editor is available once the current tasks stop");
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
        self.planning_context = None;
        if let Some(Menu::Themes(preview)) = &self.menu {
            let original = preview.original();
            self.set_theme(original);
        }
        self.modal_token = self.modal_token.wrapping_add(1);
        if let Some(stop) = self.modal_stop.take() {
            stop.cancel();
        }
    }
    fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.selection = None;
            self.tool_press = None;
            self.last_frame = None;
        }
    }
    fn open_themes(&mut self, requested: Option<String>, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        self.cancel_modal();
        self.selection = None;
        self.tool_press = None;
        let request = ThemeLoad::Picker {
            owner: self.slots[self.current].id.clone(),
            token: self.modal_token,
            requested,
        };
        let store = self.theme_store.clone();
        let stop = Cancel::linked(&self.stop);
        self.modal_stop = Some(stop.clone());
        self.menu = Some(Menu::Loading("/theme".into()));
        self.jobs.spawn_blocking(move || Job::Themes {
            request,
            result: store.load(&stop),
        });
    }
    fn commit_theme(&mut self, theme: Theme) {
        self.theme_initialized = true;
        self.theme_revision = self.theme_revision.wrapping_add(1);
        self.pending_theme = Some((self.theme_revision, theme.clone()));
        self.theme_error = None;
        self.set_theme(theme);
        self.start_theme_save();
    }
    fn start_theme_save(&mut self) {
        if self.theme_save.is_some() || self.jobs.len() + self.pending_context.len() >= MAX_JOBS {
            return;
        }
        let Some((revision, theme)) = self.pending_theme.take() else {
            return;
        };
        self.theme_save = Some(revision);
        let store = self.theme_store.clone();
        // Enter accepted this preference. Finish its publication even if the
        // user exits; only previews and palette reads are cancellable.
        self.jobs.spawn_blocking(move || {
            let result = store.save(theme.id());
            Job::ThemeSaved {
                revision,
                theme,
                result,
            }
        });
    }
    fn saved_theme(&mut self, revision: u64, theme: Theme, result: Result<()>, io: &Io) {
        if self.theme_save != Some(revision) {
            return;
        }
        self.theme_save = None;
        match result {
            Ok(()) => {
                self.saved_theme = theme;
                if self.theme_revision == revision && !self.terminating && !self.input_closed {
                    self.report(io, &format!("Theme {} saved", self.saved_theme.name()));
                }
            }
            Err(code) => {
                if self.theme_revision == revision {
                    if let Some(Menu::Themes(preview)) = &mut self.menu {
                        preview.replace_original(self.saved_theme.clone());
                    } else {
                        self.set_theme(self.saved_theme.clone());
                    }
                }
                if !self.terminating && !self.input_closed {
                    self.report(io, &format!("Could not save theme: {code}"));
                }
            }
        }
        self.start_theme_save();
    }
    fn prepare_theme(&mut self, now: Instant) {
        self.start_theme_save();
        if !self.theme.is_dynamic()
            || self.menu.is_some()
            || self.theme_reload
            || self.input_closed
            || self.terminating
            || now < self.theme_refresh
            || self.jobs.len() + self.pending_context.len() >= MAX_JOBS
        {
            return;
        }
        self.theme_reload = true;
        self.theme_refresh = now + Duration::from_secs(1);
        let revision = self.theme_revision;
        let store = self.theme_store.clone();
        let stop = Cancel::linked(&self.stop);
        self.jobs.spawn_blocking(move || Job::ThemeReload {
            revision,
            result: store.dynamic(&stop),
        });
    }
    fn open_sessions(&mut self, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        self.menu = Some(Menu::Loading("Project sessions".into()));
        self.jobs.spawn_blocking(move || Job::Sessions {
            continuation: orchestration::latest_continuation(&factory, &owner)
                .ok()
                .flatten(),
            owner,
            token,
            result: factory.summaries(&Cancel::default()),
        });
    }
    fn resume_target(&self) -> Option<RequestId> {
        self.slots[self.current]
            .state
            .snapshot
            .as_ref()
            .filter(|view| view.resume_available)
            .and_then(|view| view.request.clone())
    }
    fn open_models(&mut self, effort: bool, io: &Io) {
        if self.slots[self.current].kind == Kind::Console {
            self.report(io, "The command panel has no model");
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::linked(&self.stop);
        self.modal_stop = Some(stop.clone());
        if let Some(catalog) = self
            .models_cache
            .clone()
            .and_then(|raw| model_choices(raw).ok())
        {
            self.show_models(catalog, effort);
            if self
                .models_refreshed
                .is_some_and(|time| time.elapsed() < Duration::from_secs(300))
                || self.models_refreshing
            {
                return;
            }
        } else {
            self.menu = Some(Menu::Loading(
                if effort { "/effort" } else { "/model" }.into(),
            ));
        }
        if !self.can_job(io) {
            return;
        }
        self.models_refreshing = true;
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
            self.report(io, "This session has no parent conversation");
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
            self.report(io, "No confirmed result to send");
            return;
        }
        let label = slot.kind.name();
        let source = slot.id.clone();
        let message = format!("Result from {label} {source}:\n{text}");
        if let Some(index) = self.slots.iter().position(|s| s.id == parent) {
            let target = &mut self.slots[index];
            if target
                .state
                .text(&message, &target.handle, io, &mut target.replies)
                .0
            {
                self.report(io, "Result sent to parent");
            } else {
                self.report(io, "Result saved here. The parent is busy");
            }
        } else {
            self.report(io, "Open the parent session with /sessions");
        }
    }

    fn cycle_mode(&mut self, io: &Io) {
        let slot = &mut self.slots[self.current];
        if !matches!(slot.kind, Kind::Main | Kind::Fork) || slot.active() {
            return;
        }
        if self.orchestration_mode {
            self.orchestration_mode = false;
            self.refinement = None;
            slot.state.mode = AgentMode::Basic;
        } else if slot.state.mode == AgentMode::Plan {
            self.orchestration_mode = true;
            slot.state.mode = AgentMode::Basic;
        } else {
            slot.state.mode = AgentMode::Plan;
        }
        slot.state.mode_ready = true;
        self.factory.set_editor_mode(if self.orchestration_mode {
            AgentMode::Plan
        } else {
            slot.state.mode
        });
        let mode = if self.orchestration_mode {
            "Orchestration"
        } else if slot.state.mode == AgentMode::Plan {
            "Planning"
        } else {
            "Work"
        };
        self.report(io, mode);
    }

    fn start_orchestration_plan(&mut self, text: &str, io: &Io) -> bool {
        if self.factory.orchestration_route().is_err() {
            self.report(
                io,
                "Orchestration requires a route with verified structured outputs",
            );
            return false;
        }
        if self.orchestration_stop.is_some() || self.slots[self.current].active() {
            self.report(io, "Wait for the current task to finish");
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        let prompt = self.refinement.as_ref().map_or_else(
            || text.to_owned(),
            |previous| format!("{previous}\n\nClarification: {text}"),
        );
        if prompt.len() > 16 * 1024 {
            self.report(io, "The request and its clarifications are too long");
            return false;
        }
        self.cancel_modal();
        let token = self.modal_token;
        let slot = &mut self.slots[self.current];
        let command = slot.state.command(&slot.handle, CommandPayload::GetContext);
        let id = command.command.clone();
        if !slot
            .state
            .post_command(&slot.handle, command, &mut slot.replies, false)
        {
            self.report(io, "Wait for the current requests");
            return false;
        }
        self.planning_context = Some((id, slot.id.clone(), prompt, token));
        slot.state.status.clear();
        self.refinement = None;
        self.menu = Some(Menu::Loading("Drafting a plan".into()));
        true
    }

    fn approve_orchestration(&mut self, proposal: Proposal, io: &Io) {
        let slot = &self.slots[self.current];
        if self.orchestration_stop.is_some()
            || slot.active()
            || slot.id != proposal.seed.source_session().0
            || !self.can_job(io)
        {
            self.report(io, "Wait for the current task to finish");
            return;
        }
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::linked(&self.stop);
        let (progress, receiver) = tokio::sync::watch::channel(None);
        self.orchestration_progress = Some(receiver);
        self.orchestration_views.remove(&owner);
        let slot = &mut self.slots[self.current];
        slot.state.status.clear();
        slot.title
            .get_or_insert_with(|| display_title(&proposal.prompt));
        self.orchestration_stop = Some((owner.clone(), stop.clone()));
        let factory = self.factory.clone();
        self.jobs.spawn_blocking(move || Job::OrchestrationDone {
            owner,
            result: orchestration::execute(&factory, proposal, &stop, progress),
        });
    }

    fn resume_orchestration(&mut self, identity: RunId, io: &Io) {
        let slot = &self.slots[self.current];
        if self.orchestration_stop.is_some()
            || slot.active()
            || slot.id != identity.parent_session
            || !self.can_job(io)
        {
            self.report(io, "Wait for the current task to finish");
            return;
        }
        let owner = identity.parent_session.clone();
        let stop = Cancel::linked(&self.stop);
        let (progress, receiver) = tokio::sync::watch::channel(None);
        self.orchestration_progress = Some(receiver);
        self.orchestration_views.remove(&owner);
        self.slots[self.current].state.status.clear();
        self.orchestration_stop = Some((owner.clone(), stop.clone()));
        let factory = self.factory.clone();
        self.jobs.spawn_blocking(move || Job::OrchestrationDone {
            owner,
            result: orchestration::resume(&factory, identity, &stop, progress),
        });
    }

    fn update_orchestration_progress(&mut self) -> bool {
        let Some((owner, _)) = &self.orchestration_stop else {
            return false;
        };
        let Some(progress) = &mut self.orchestration_progress else {
            return false;
        };
        let previous = self.orchestration_views.get(owner);
        let latest = progress.borrow_and_update();
        if latest.as_ref() == previous {
            return false;
        }
        if let Some(view) = latest.as_ref() {
            self.orchestration_views.insert(owner.clone(), view.clone());
        }
        true
    }

    fn collecting_orchestration(&self) -> bool {
        self.orchestration_progress.is_some()
            && self
                .orchestration_stop
                .as_ref()
                .is_some_and(|(owner, _)| owner == &self.slots[self.current].id)
    }

    fn text(&mut self, text: &str, io: &Io) -> (bool, bool) {
        if !text.trim_start().starts_with('/')
            && !text.trim().is_empty()
            && (self.collecting_orchestration()
                || self
                    .orchestration_summary
                    .contains_key(&self.slots[self.current].id))
        {
            return (false, false);
        }
        if !text.trim().is_empty()
            && !text.trim_start().starts_with('/')
            && (self.orchestration_mode || commands::agent_task(text).is_some())
        {
            return (self.start_orchestration_plan(text, io), false);
        }
        let (command, argument) = text
            .trim()
            .split_once(char::is_whitespace)
            .map_or((text.trim(), ""), |(c, a)| (c, a.trim()));
        match command {
            "/terminals" => {
                self.inspect_terminal(argument, io);
                (true, false)
            }
            "/new" if argument.is_empty() => (self.create(Kind::Main, None, true, io), false),
            "/fork" => {
                self.create(
                    Kind::Fork,
                    (!argument.is_empty()).then(|| argument.to_string()),
                    true,
                    io,
                );
                (true, false)
            }
            "/btw" => {
                self.create(
                    Kind::Btw,
                    (!argument.is_empty()).then(|| argument.to_string()),
                    true,
                    io,
                );
                (true, false)
            }
            "/agent" => {
                self.report(
                    io,
                    "Ask the main agent to delegate or switch to Orchestration with Tab",
                );
                (true, false)
            }
            "/agents" => {
                self.panel = Some(Panel::Agents { selected: 0 });
                self.focus = Focus::Inspector;
                if self.can_job(io) {
                    let factory = self.factory.clone();
                    let stop = Cancel::linked(&self.stop);
                    self.jobs.spawn_blocking(move || Job::CachedSummaries {
                        result: factory.summaries(&stop),
                    });
                }
                (true, false)
            }
            "/sessions" | "/resume" => {
                if argument.is_empty() {
                    self.open_sessions(io);
                } else {
                    self.resume(argument.to_string(), io);
                }
                (true, false)
            }
            "/pr" => {
                if argument.is_empty() {
                    self.report(io, "Specify a base branch, for example /pr main");
                } else if self.orchestration_stop.is_some() || self.slots[self.current].active() {
                    self.report(io, "Wait for the current task to finish");
                } else if self.can_job(io) {
                    let owner = self.slots[self.current].id.clone();
                    let factory = self.factory.clone();
                    let base = argument.to_owned();
                    let stop = Cancel::linked(&self.stop);
                    self.orchestration_stop = Some((owner.clone(), stop.clone()));
                    self.jobs.spawn_blocking(move || Job::DraftPullRequest {
                        result: super::super::publish::draft(&factory, &owner, &base, &stop),
                        owner,
                    });
                    self.report(io, "Preparing a draft PR · Ctrl+C stop");
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
                self.report(io, "Project terminal: /files, then q");
                (false, false)
            }
            "/theme" => {
                self.open_themes((!argument.is_empty()).then(|| argument.to_string()), io);
                (true, false)
            }
            "/send" => {
                self.send_result(io);
                (true, false)
            }
            "/model" => {
                self.requested_model = (!argument.is_empty()).then(|| argument.to_string());
                self.open_models(false, io);
                (true, false)
            }
            "/effort" | "/thinking" if argument.is_empty() => {
                self.open_models(true, io);
                (true, false)
            }
            "/effort" | "/thinking" => {
                if argument.split_whitespace().count() != 1 {
                    self.report(io, "Expected one value");
                    return (false, false);
                }
                let Some(view) = &self.slots[self.current].state.snapshot else {
                    return (false, false);
                };
                let model = view.generation.model.clone();
                (
                    self.apply_generation(
                        model,
                        (argument != "default").then(|| argument.into()),
                        io,
                    ),
                    false,
                )
            }
            "/help" | "/" | "?" => {
                self.menu = Some(Menu::Help {
                    selected: 0,
                    entries: self.available_commands(),
                });
                (true, false)
            }
            "/basic" if self.slots[self.current].kind == Kind::Btw => {
                self.report(io, "/btw is read-only");
                (true, false)
            }
            _ => {
                if !text.starts_with('/')
                    && self
                        .orchestration_stop
                        .as_ref()
                        .is_some_and(|(owner, _)| owner == &self.slots[self.current].id)
                {
                    self.report(io, "Wait for the current task to finish");
                    return (false, false);
                }
                if matches!(command, "/basic" | "/plan") && argument.is_empty() {
                    self.orchestration_mode = false;
                    self.refinement = None;
                }
                let slot = &mut self.slots[self.current];
                if slot.kind == Kind::Console && !text.starts_with('/') {
                    slot.state.report(
                        io,
                        "Console archive · Esc parent · /files files and terminal",
                    );
                    return (false, false);
                }
                if !text.starts_with('/') && !text.trim().is_empty() && slot.title.is_none() {
                    slot.title = Some(display_title(text));
                }
                let routed = if command == "/plan"
                    && argument.is_empty()
                    && slot.state.mode == AgentMode::Plan
                    && slot.kind != Kind::Btw
                {
                    "/basic"
                } else if argument.is_empty()
                    && matches!(
                        command,
                        "/basic" | "/plan" | "/status" | "/copy" | "/cancel" | "/exit"
                    )
                {
                    command
                } else {
                    text
                };
                let result = slot.state.text(routed, &slot.handle, io, &mut slot.replies);
                if slot.id == self.root_id {
                    self.factory.set_editor_mode(slot.state.mode);
                }
                result
            }
        }
    }
    fn resume(&mut self, id: String, io: &Io) {
        if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
            self.switch(index);
            return;
        }
        if self.slots.len() + self.jobs.len() >= MAX_SESSIONS {
            self.report(io, "Open session limit reached (16)");
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
            foreground: true,
            plan_only: false,
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
                let message = format!("Could not open session: {code}");
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
                    .report(io, "Could not create session: agent exited");
            }
            return;
        };
        let lost_context = matches!(event.payload, EventPayload::ResyncRequired { .. })
            && self.discard_contexts(id);
        if let EventPayload::ContextSnapshot { command, seed } = &event.payload
            && self
                .planning_context
                .as_ref()
                .is_some_and(|(pending, owner, _, _)| pending == command && owner == id)
            && let Some((_, owner, prompt, token)) = self.planning_context.take()
        {
            let factory = self.factory.clone();
            let seed = seed.clone();
            let stop = Cancel::linked(&self.stop);
            self.modal_stop = Some(stop.clone());
            self.jobs.spawn_blocking(move || Job::OrchestrationPlan {
                owner,
                token,
                result: Proposal::generate(&factory, seed, prompt, &stop),
            });
        }
        if let EventPayload::ContextSnapshot { command, seed } = &event.payload
            && let Some(intent) = self.pending_context.remove(&command.0)
            && intent.source == id
        {
            let factory = self.factory.clone();
            let seed = seed.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                source: intent.source,
                prompt: intent.prompt,
                foreground: intent.foreground,
                plan_only: intent.plan_only,
                result: factory.create(intent.kind, Some(seed)),
            });
        }
        if let EventPayload::RuntimeError {
            command: Some(command),
            ..
        } = &event.payload
        {
            self.pending_context.remove(&command.0);
            if self
                .planning_context
                .as_ref()
                .is_some_and(|(pending, _, _, _)| pending == command)
            {
                self.cancel_modal();
                self.menu = None;
            }
        }
        let slot = &mut self.slots[index];
        if let EventPayload::FrontendSnapshot(snapshot) = &event.payload
            && !slot.accepts_snapshot(snapshot)
        {
            return;
        }
        if self
            .orchestration_summary
            .get(id)
            .is_some_and(|summary| match &event.payload {
                EventPayload::Done => event.turn.0 == summary.0,
                EventPayload::CommandReplayed { command, state } => {
                    command == summary
                        && matches!(state.as_str(), "completed" | "cancelled" | "paused")
                }
                EventPayload::RuntimeError { command, .. } => {
                    command.as_ref() == Some(summary) || event.turn.0 == summary.0
                }
                EventPayload::FrontendSnapshot(snapshot) => {
                    snapshot.submit.as_ref() == Some(summary)
                        && !snapshot.worker_busy
                        && matches!(
                            snapshot.state.as_str(),
                            "completed" | "cancelled" | "paused"
                        )
                }
                _ => false,
            })
        {
            self.orchestration_summary.remove(id);
        }
        let committed = match &event.payload {
            EventPayload::FrontendSnapshot(snapshot)
                if snapshot.durable && !snapshot.storage_failed =>
            {
                snapshot.submit.as_ref()
            }
            EventPayload::CommandReplayed { command, .. } if event.request.is_some() => {
                Some(command)
            }
            _ => None,
        };
        if let Some(run) = self.orchestration_handoffs.get(id)
            && committed.is_some_and(|command| command.0.strip_prefix("summary-") == Some(run))
            && let Some(run) = self.orchestration_handoffs.remove(id)
        {
            let owner = id.to_owned();
            let factory = self.factory.clone();
            self.jobs.spawn_blocking(move || Job::OrchestrationReceipt {
                result: orchestration::mark_handoff(&factory, &owner, &run),
            });
        }
        let mut image = None;
        match &event.payload {
            EventPayload::CommandAccepted { command, .. }
                if slot.follow_accepted_submit.as_ref() == Some(command) =>
            {
                slot.follow_accepted_submit = None;
                slot.show_latest();
            }
            EventPayload::FrontendSnapshot(snapshot)
                if snapshot.durable
                    && !snapshot.storage_failed
                    && slot.follow_accepted_submit.is_some()
                    && slot.follow_accepted_submit == snapshot.submit =>
            {
                slot.follow_accepted_submit = None;
                slot.show_latest();
            }
            EventPayload::RuntimeError {
                command: Some(command),
                ..
            } if slot.follow_accepted_submit.as_ref() == Some(command) => {
                slot.follow_accepted_submit = None;
            }
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
                image = slot.new_image(page);
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
            slot.state
                .report(io, "Could not restore context. Retry the session command");
        }
        if reading.is_some() {
            slot.state.reading = reading;
        }
        if let Some(image) = image
            && index == self.current
            && slot.state.follow_tail
            && self.menu.is_none()
            && self.selection.is_none()
            && self.editor.is_none()
            && self.pending_editor.is_none()
            && !self.editor_preparing
            && !matches!(self.panel, Some(Panel::Terminal))
        {
            self.show_image(image, io);
        }
    }
    fn job(&mut self, job: Job, io: &Io) {
        if let Job::Models { result, .. } = &job {
            self.models_refreshing = false;
            if let Ok(raw) = result
                && model_choices(raw.clone()).is_ok()
            {
                self.models_cache = Some(raw.clone());
                self.models_refreshed = Some(Instant::now());
            }
        }
        let fresh = match &job {
            Job::Themes {
                request: ThemeLoad::Startup,
                ..
            } => !self.input_closed && !self.terminating && !self.theme_initialized,
            Job::Themes {
                request: ThemeLoad::Picker { owner, token, .. },
                ..
            } => {
                !self.input_closed
                    && !self.terminating
                    && *owner == self.slots[self.current].id
                    && *token == self.modal_token
            }
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
        if matches!(&job, Job::ThemeReload { .. }) {
            self.theme_reload = false;
        }
        if !fresh {
            return;
        }
        if matches!(
            &job,
            Job::Sessions { .. }
                | Job::Themes {
                    request: ThemeLoad::Picker { .. },
                    ..
                }
        ) {
            self.menu = None;
            self.modal_stop = None;
        }
        match job {
            Job::DraftPullRequest { owner, result } => {
                self.orchestration_stop = None;
                let message = match result {
                    Ok(url) => format!("Draft PR · {url}"),
                    Err("draft_release_missing") => "This conversation has no verified branch yet".into(),
                    Err("draft_snapshot_private_changes") => "The snapshot contains local changes. Publishing requires a candidate verified on a clean base".into(),
                    Err("draft_base_changed") => "The GitHub base branch has changed".into(),
                    Err("draft_requires_network") => "Opening a draft PR requires network access".into(),
                    Err(code) => format!("Could not open draft PR ({code})"),
                };
                if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == owner) {
                    slot.state.report(io, &message);
                }
            }
            Job::OrchestrationReceipt { result } => {
                if let Err(code) = result {
                    self.report(io, &format!("Could not record result delivery ({code})"));
                }
            }
            Job::ArchivedPlan { token, result } => {
                if token == self.file_token
                    && matches!(self.panel, Some(Panel::Orchestration { .. }))
                    && self.menu.is_none()
                {
                    match result {
                        Ok(archive) => {
                            self.show_orchestration_plan(&archive.plan, Some(&archive.prompt));
                            match archive.candidate {
                                Ok(Some(candidate)) => {
                                    let view = ux::CandidateView::new(archive.run, candidate);
                                    self.slots[self.current].remember_candidate(view);
                                    self.review_redraw = true;
                                }
                                Err(code) => self
                                    .report(io, &format!("Diff and checks unavailable ({code})")),
                                Ok(None) => {}
                            }
                        }
                        Err(code) => {
                            self.panel = None;
                            self.focus = Focus::Chat;
                            self.report(io, &format!("Could not open plan ({code})"));
                        }
                    }
                }
            }
            Job::OrchestrationPlan {
                owner,
                token,
                result,
            } => {
                if self.modal_token != token
                    || self.slots[self.current].id != owner
                    || self.input_closed
                {
                    return;
                }
                self.modal_stop = None;
                self.menu = None;
                match result {
                    Ok(proposal) => {
                        let blocked = proposal.plan.execution_ready().err();
                        self.show_orchestration_plan(&proposal.plan, None);
                        self.frames.clear();
                        self.displayed = None;
                        self.operational_frame = None;
                        self.last_frame = None;
                        self.menu = Some(Menu::Orchestration {
                            proposal: Box::new(proposal),
                            selected: usize::from(blocked.is_some()),
                        });
                    }
                    Err(code) => self.report(io, &format!("Could not draft plan ({code})")),
                }
            }
            Job::OrchestrationDone { owner, result } => {
                self.update_orchestration_progress();
                self.orchestration_progress = None;
                self.orchestration_stop = None;
                if result.is_err() {
                    self.orchestration_views.remove(&owner);
                    if matches!(self.panel, Some(Panel::Agents { .. })) && self.can_job(io) {
                        let factory = self.factory.clone();
                        let stop = Cancel::linked(&self.stop);
                        self.jobs.spawn_blocking(move || Job::CachedSummaries {
                            result: factory.summaries(&stop),
                        });
                    }
                }
                let foreground = self.slots[self.current].id == owner;
                let Some(parent) = self.slots.iter_mut().find(|slot| slot.id == owner) else {
                    return;
                };
                match result {
                    Ok(completed) if !self.input_closed && !parent.active() => {
                        if let Some(candidate) = completed.candidate {
                            let review = ux::CandidateView::new(completed.id.clone(), candidate);
                            if foreground && self.panel.is_none() {
                                self.panel = Some(review.panel());
                            }
                            parent.remember_candidate(review);
                        }
                        let mut command = parent.state.command(
                            &parent.handle,
                            CommandPayload::Handoff(Submit {
                                text: completed.evidence,
                                mode: AgentMode::Plan,
                            }),
                        );
                        command.command = CommandId(format!("summary-{}", completed.id));
                        command.turn = TurnId(format!("summary-{}", completed.id));
                        let summary = command.command.clone();
                        if parent.state.post_command(
                            &parent.handle,
                            command,
                            &mut parent.replies,
                            false,
                        ) {
                            parent.state.status.clear();
                            self.orchestration_summary.insert(owner.clone(), summary);
                            self.orchestration_handoffs
                                .insert(owner.clone(), completed.id);
                            if parent.state.follow_tail {
                                parent.show_latest();
                            }
                        } else {
                            parent
                                .state
                                .report(io, "Results are in the agents' transcripts");
                        }
                    }
                    Ok(_) => parent
                        .state
                        .report(io, "Results are in the agents' transcripts"),
                    Err("auth_cancelled" | "run_cancelled") => {
                        parent.state.report(io, "Plan stopped")
                    }
                    Err("orchestration_paused") => parent
                        .state
                        .report(io, "Agent paused · resume with /sessions"),
                    Err(code) => parent.state.report(io, &format!("Plan stopped ({code})")),
                }
            }
            Job::TerminalScreen { token, result } => {
                if let Some(Panel::NativeTerminal {
                    token: expected,
                    view,
                    pending,
                    ..
                }) = &mut self.panel
                    && *expected == token
                {
                    *pending = false;
                    *view = Some(result.unwrap_or_else(|error| carbon::OutputView {
                        title: "Agent terminal".into(),
                        text: if error == "terminal_not_open" {
                            "The agent has not opened a terminal yet".into()
                        } else {
                            diagnostics::error(error)
                        },
                    }));
                }
            }
            Job::Themes { request, result } => match result {
                Ok(catalog) => {
                    if !self.theme_initialized {
                        let initial = catalog.selected();
                        self.saved_theme = initial.clone();
                        self.set_theme(initial);
                        self.theme_initialized = true;
                    }
                    if !catalog.warnings.is_empty() {
                        self.report(
                            io,
                            &catalog
                                .warnings
                                .iter()
                                .take(2)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join("; "),
                        );
                    }
                    if let ThemeLoad::Picker { requested, .. } = request {
                        if let Some(requested) = requested {
                            let requested = match requested.to_ascii_lowercase().as_str() {
                                "auto" | "auto-dms" => "dms".to_owned(),
                                _ => requested,
                            };
                            if let Some(theme) = catalog.find(&requested) {
                                self.commit_theme(theme.clone());
                            } else {
                                self.report(io, "Theme unavailable. Choose a theme with /theme");
                            }
                        } else {
                            let preview = ThemePreview::new(catalog.themes, self.theme.clone());
                            self.set_theme(preview.current());
                            self.menu = Some(Menu::Themes(preview));
                        }
                    }
                }
                Err(code) => {
                    if matches!(request, ThemeLoad::Startup) {
                        self.theme_initialized = true;
                    }
                    if code != "theme_cancelled" {
                        self.report(io, &format!("Could not load themes: {code}"));
                    }
                }
            },
            Job::ThemeSaved {
                revision,
                theme,
                result,
            } => self.saved_theme(revision, theme, result, io),
            Job::GenerationSaved { owner, result } => {
                if let Err(code) = result
                    && let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == owner)
                {
                    if code.starts_with("model_preferences_") {
                        slot.state
                            .report(io, "Could not save default model for new sessions");
                    } else {
                        slot.state.exit = 1;
                        slot.state
                            .report(io, &format!("ansa: {}", diagnostics::error(code)));
                    }
                }
            }
            Job::ThemeReload { revision, result } => {
                if revision == self.theme_revision
                    && self.theme.is_dynamic()
                    && self.menu.is_none()
                    && !self.input_closed
                    && !self.terminating
                {
                    match result {
                        Ok(theme) => {
                            if self.saved_theme.is_dynamic() {
                                self.saved_theme = theme.clone();
                            }
                            self.set_theme(theme);
                            self.theme_error = None;
                        }
                        Err(code) if self.theme_error != Some(code) => {
                            self.theme_error = Some(code);
                            self.report(
                                io,
                                &format!("Auto DMS: could not refresh palette ({code})"),
                            );
                        }
                        Err(_) => {}
                    }
                }
            }
            Job::Created {
                source,
                prompt,
                foreground,
                plan_only,
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
                    let mut slot =
                        Slot::new(created.runtime, id, created.kind, created.parent, prompt);
                    if plan_only {
                        slot.state.mode = AgentMode::Plan;
                        slot.state.mode_ready = true;
                    }
                    self.add(slot);
                    if focus && !self.input_closed {
                        if foreground {
                            self.switch(self.slots.len() - 1);
                        } else {
                            self.report(io, "Agent started · /agents");
                        }
                    }
                }
                Err(code) => self.report(io, &format!("Could not open session: {code}")),
            },
            Job::Sessions {
                result,
                continuation,
                ..
            } => match result {
                Ok(entries) => {
                    self.summaries = entries.clone();
                    self.menu = Some(Menu::Sessions {
                        entries: entries
                            .into_iter()
                            .filter(|s| matches!(s.entry.kind.as_str(), "main" | "fork"))
                            .collect(),
                        selected: 0,
                        resume_target: self.resume_target(),
                        continuation,
                    })
                }
                Err(code) => self.report(io, &format!("Could not load sessions: {code}")),
            },
            Job::CachedSummaries { result } => {
                if let Ok(summaries) = result {
                    for slot in &mut self.slots {
                        if slot.title.is_none() {
                            slot.title = summaries
                                .iter()
                                .find(|s| s.entry.id == slot.id)
                                .and_then(|s| s.title.clone());
                        }
                    }
                    self.summaries = summaries;
                }
            }
            Job::CachedModels { result } => {
                if self.models_cache.is_none()
                    && let Ok(Some(raw)) = result
                    && let Ok(models) = model_choices(raw.clone())
                {
                    self.models_cache = Some(raw);
                    if let Some(Menu::Loading(title)) = &self.menu {
                        let effort = title == "/effort";
                        if effort || title == "/model" {
                            self.show_models(models, effort);
                        }
                    }
                }
            }
            Job::Models { effort, result, .. } => {
                self.modal_stop = None;
                match result.and_then(model_choices) {
                    Ok(models) => {
                        if matches!(self.menu, Some(Menu::Loading(_))) {
                            self.show_models(models, effort);
                        }
                    }
                    Err(code) => {
                        if matches!(self.menu, Some(Menu::Loading(_))) {
                            self.menu = None;
                        }
                        self.report(io, &format!("Could not refresh model list: {code}"));
                    }
                }
            }
            Job::File { token, result } => {
                if !matches!(&self.panel, Some(Panel::Files(files)) if files.token == token)
                    && !matches!(&self.panel, Some(Panel::Image { token: active, .. }) if *active == token)
                {
                    return;
                }
                if let Ok(FileValue::Image(image)) = result {
                    self.panel = Some(Panel::Image {
                        token,
                        name: image
                            .path
                            .file_name()
                            .unwrap_or(image.path.as_os_str())
                            .to_string_lossy()
                            .into_owned(),
                        view: Some(image),
                    });
                    return;
                }
                if let Some(Panel::Files(files)) = &mut self.panel
                    && files.token == token
                {
                    files.loading = false;
                    match result {
                        Ok(FileValue::Listing(listing)) => files.listing = Some(listing),
                        Ok(FileValue::Document(document)) => {
                            files.document = Some(document);
                            files.offset = files.offset.min(files.last_line());
                            files.preview_cache = None;
                        }
                        Ok(FileValue::Image(_)) => unreachable!("handled image"),
                        Err(code) => self.report(io, &format!("Could not open file: {code}")),
                    }
                } else if let Err(code) = result {
                    self.panel = None;
                    self.focus = Focus::Chat;
                    self.report(io, &format!("Could not open chart ({code})"));
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
                Err(code) => self.report(io, &format!("Could not open editor: {code}")),
            },
        }
    }
    fn escape(&mut self, io: &Io) {
        self.tool_press = None;
        self.slots[self.current].cancel_history_jump();
        self.cancel_modal();
        self.requested_model = None;
        self.cancel_editor_request();
        if self.completion.take().is_some() {
            let editor = &self.slots[self.current].state.editor;
            self.completion_closed = Some((editor.bytes.clone(), editor.cursor));
            return;
        }
        if let Some(menu) = self.menu.take() {
            if matches!(menu, Menu::Orchestration { .. }) {
                self.panel = None;
                self.focus = Focus::Chat;
            }
            if let Menu::Efforts {
                previous: Some((models, selected)),
                ..
            } = menu
            {
                self.menu = Some(Menu::Models { models, selected });
            }
            return;
        }
        if self.selection.take().is_some() {
            self.slots[self.current].show_latest();
            return;
        }
        if matches!(self.panel, Some(Panel::Terminal)) {
            self.hide_terminal(io);
            return;
        }
        if matches!(self.panel, Some(Panel::Output { .. })) {
            self.panel = self.output_back.take().map(|panel| *panel);
            self.focus = if self.panel.is_some() {
                Focus::Inspector
            } else {
                Focus::Chat
            };
            self.restore_result_scroll();
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
            self.wide_diff = false;
            self.restore_result_scroll();
            return;
        }
        if self.result_selected.take().is_some() {
            self.leave_results();
            return;
        }
        if self.slots[self.current].kind != Kind::Agent
            && !self.slots[self.current].state.follow_tail
        {
            self.slots[self.current].show_latest();
            return;
        }
        if let Some(parent) = self.slots[self.current].parent.clone() {
            if let Some(index) = self.slots.iter().position(|slot| slot.id == parent) {
                self.switch(index);
            } else {
                self.resume(parent, io);
            }
        }
    }
    fn page(&mut self, older: bool) {
        let output_limit = match &self.panel {
            Some(Panel::Output { view, .. }) => {
                self.output_lines(&view.text).len().saturating_sub(1)
            }
            _ => 0,
        };
        self.selection = None;
        self.tool_press = None;
        self.slots[self.current].cancel_history_jump();
        if self.focus == Focus::Inspector {
            if let Some(Panel::Orchestration { lines, offset }) = &mut self.panel {
                let viewport = self
                    .displayed
                    .as_ref()
                    .and_then(|frame| frame.inspector_bounds());
                let (_, _, width, height) = viewport.unwrap_or((0, 0, 80, 6));
                let size = crate::frontend::cells::wrap_lines(lines, width.max(1)).len();
                let step = height.saturating_sub(1).max(1);
                *offset = if older {
                    offset.saturating_sub(step)
                } else {
                    (*offset + step).min(size.saturating_sub(height.max(1)))
                };
                return;
            }
            if let Some(Panel::EditDiff { rows, offset, .. }) = &mut self.panel {
                *offset = if older {
                    offset.saturating_sub(8)
                } else {
                    offset.saturating_add(8).min(rows.len().saturating_sub(1))
                };
            }
            if let Some(Panel::Output { offset, .. }) = &mut self.panel {
                *offset = if older {
                    offset.saturating_sub(8)
                } else {
                    offset.saturating_add(8).min(output_limit)
                };
            }
            if let Some(Panel::Files(files)) = &mut self.panel {
                files.preview_end = false;
                if files.document.is_none() {
                    let last = files
                        .listing
                        .as_ref()
                        .map_or(0, |listing| listing.entries.len().saturating_sub(1));
                    files.selected = if older {
                        files.selected.saturating_sub(8)
                    } else {
                        files.selected.saturating_add(8).min(last)
                    };
                } else {
                    files.offset = if older {
                        files.offset.saturating_sub(8)
                    } else {
                        files.offset.saturating_add(8).min(files.last_line())
                    };
                }
            }
            return;
        }
        self.scroll_chat(older, 8, false);
    }
    fn scroll_chat(&mut self, older: bool, step: usize, dragging: bool) -> bool {
        let neighbor = self
            .selection
            .as_ref()
            .and_then(|selection| selection.neighbor(older));
        let tail = self.selection.as_ref().and_then(Selection::tail);
        let can_page =
            neighbor.is_some() || self.selection.as_ref().is_none_or(Selection::can_add_page);
        let slot = &mut self.slots[self.current];
        let state = &mut slot.state;
        let original_scroll = state.scroll;
        let mut crossing = None;
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
                && can_page
                && let Some(page) = &slot.timeline
                && page.begin > 0
            {
                slot.timeline_query = GetTranscript {
                    before: page.begin,
                    limit: neighbor.map_or(64, |(begin, end)| end - begin),
                    forward: false,
                };
                crossing = Some((page.begin, page.end));
                state.scroll = 0;
                state.reading = None;
                slot.refresh = Instant::now();
            } else {
                state.scroll = state.scroll.saturating_add(step).min(state.scroll_limit);
            }
        } else if state.scroll == 0
            && can_page
            && let Some(page) = &slot.timeline
            && page.end < page.total
            && tail.is_none_or(|limit| page.end < limit)
        {
            slot.timeline_query = GetTranscript {
                before: page.end,
                limit: neighbor.map_or_else(
                    || tail.map_or(64, |end| (end - page.end).min(64)),
                    |(begin, end)| end - begin,
                ),
                forward: true,
            };
            crossing = Some((page.begin, page.end));
            slot.jump_top = true;
            state.reading = None;
            slot.refresh = Instant::now();
        } else {
            state.scroll = state.scroll.saturating_sub(step);
            if state.scroll == 0
                && !dragging
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
        let changed = crossing.is_some() || state.scroll != original_scroll;
        if dragging
            && changed
            && let Some(selection) = &mut self.selection
        {
            selection.scroll_pending = true;
            selection.pending_range = crossing;
            self.tool_press = None;
        }
        changed
    }
    fn drag_scroll(&mut self, older: bool, lines: usize, io: &Io) -> bool {
        let range = self.slots[self.current]
            .timeline
            .as_ref()
            .map(|page| (page.begin, page.end));
        self.selection.as_ref().is_some_and(|selection| {
            selection.is_chat()
                && selection.can_scroll(io.size)
                && selection.timeline_range() == range
        }) && self.scroll_chat(older, lines, true)
    }
    fn tick_drag(&mut self, io: &Io, now: Instant) -> bool {
        let Some(older) = self.selection.as_ref().and_then(Selection::edge) else {
            self.next_drag_scroll = None;
            return false;
        };
        let next = self
            .next_drag_scroll
            .get_or_insert(now + Duration::from_millis(240));
        if now < *next {
            return false;
        }
        *next = now + Duration::from_millis(80);
        self.drag_scroll(older, 1, io)
    }
    fn navigate(&mut self, older: bool) {
        self.selection = None;
        self.tool_press = None;
        if self.menu.is_none() && self.focus == Focus::Chat && self.result_selected.is_some() {
            self.navigate_results(Some(older));
            return;
        }
        if self.menu.is_none()
            && let Some(completion) = &mut self.completion
        {
            completion.navigate(older);
            return;
        }
        let agent_count = self.agent_rows().len();
        let output_count = match &self.panel {
            Some(Panel::Output { view, .. }) => self.output_lines(&view.text).len(),
            _ => 0,
        };
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
            Some(Menu::Orchestration { selected, .. }) => delta(selected, 3),
            Some(Menu::Themes(preview)) => {
                preview.navigate(older);
                let theme = preview.current();
                self.set_theme(theme);
            }
            Some(Menu::Loading(_)) => {}
            Some(Menu::Sessions {
                entries,
                selected,
                resume_target,
                continuation,
            }) => delta(
                selected,
                entries.len()
                    + usize::from(resume_target.is_some())
                    + usize::from(continuation.is_some()),
            ),
            Some(Menu::Models { models, selected }) => delta(selected, models.len()),
            Some(Menu::Efforts {
                values, selected, ..
            }) => delta(selected, values.len()),
            Some(Menu::Help { selected, entries }) => delta(selected, entries.len()),
            None if self.focus == Focus::Inspector => match &mut self.panel {
                Some(Panel::EditDiff { rows, offset, .. }) => delta(offset, rows.len()),
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
                Some(Panel::Agents { selected }) => delta(selected, agent_count),
                Some(Panel::Output { offset, .. }) => delta(offset, output_count),
                Some(Panel::Terminal | Panel::Image { .. } | Panel::NativeTerminal { .. }) => {}
                Some(Panel::Orchestration { .. }) => {}
                None => {}
            },
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
        if self.menu.is_none()
            && self.focus == Focus::Chat
            && let Some(key) = self.result_selected.clone()
        {
            if let Some(frame) = &self.displayed
                && frame.tool_actions.iter().any(|(_, action)| action == &key)
            {
                let edit = frame.edit_view(&key);
                let output = frame.output_view(&key);
                self.activate_result(key, edit, output, io);
            }
            return None;
        }
        if let Some(completion) = &self.completion
            && self.menu.is_none()
            && self.focus == Focus::Chat
            && !completion.exact_command(&self.slots[self.current].state.editor)
        {
            self.complete_command();
            return None;
        }
        if let Some(menu) = self.menu.take() {
            match menu {
                Menu::Orchestration { proposal, selected } => {
                    if selected == 0
                        && (proposal.plan.execution_ready().is_err()
                            || !self
                                .displayed
                                .as_ref()
                                .and_then(|frame| frame.inspector_bounds())
                                .is_some_and(|(_, _, width, height)| width >= 30 && height > 0))
                    {
                        let text = if proposal.plan.execution_ready().is_err() {
                            "Refine the verification step before starting"
                        } else {
                            "Widen the terminal to read the plan"
                        };
                        self.report(io, text);
                        self.menu = Some(Menu::Orchestration { proposal, selected });
                        return None;
                    }
                    self.panel = None;
                    self.focus = Focus::Chat;
                    match selected {
                        0 => self.approve_orchestration(*proposal, io),
                        1 => {
                            self.refinement = Some(proposal.prompt);
                            self.report(io, "Enter a clarification for the plan");
                        }
                        _ => self.report(io, "Plan cancelled"),
                    }
                }
                Menu::Themes(preview) => self.commit_theme(preview.current()),
                Menu::Loading(title) => self.menu = Some(Menu::Loading(title)),
                Menu::Sessions {
                    entries,
                    selected,
                    resume_target,
                    continuation,
                } => {
                    let native = usize::from(resume_target.is_some());
                    let orchestration = usize::from(continuation.is_some());
                    if let Some(target) = resume_target.as_ref().filter(|_| selected == 0) {
                        if self.resume_target().as_ref() == Some(target) {
                            let slot = &mut self.slots[self.current];
                            slot.state.post(
                                &slot.handle,
                                CommandPayload::Resume(target.clone()),
                                &mut slot.replies,
                            );
                        }
                    } else if let Some((id, _)) = continuation.filter(|_| selected == native) {
                        self.resume_orchestration(id, io);
                    } else if let Some(entry) = entries.get(selected - native - orchestration) {
                        self.resume(entry.entry.id.clone(), io);
                    }
                }
                Menu::Models { models, selected } => {
                    self.choose_model_effort(models, selected);
                }
                Menu::Efforts {
                    model,
                    values,
                    selected,
                    ..
                } => {
                    if let Some(effort) = values.get(selected) {
                        self.apply_generation(model, effort.clone(), io);
                    }
                }
                Menu::Help { selected, entries } => {
                    if let Some(command) = entries.get(selected) {
                        let slot = &mut self.slots[self.current];
                        let end = slot.state.editor.bytes.len();
                        slot.state.editor.replace(0, end, command.name.as_bytes());
                        self.completion_closed =
                            Some((slot.state.editor.bytes.clone(), slot.state.editor.cursor));
                    }
                }
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
                        .filter(|entry| !entry.directory)
                    {
                        self.open_file(entry.path.clone(), io);
                    }
                }
                Some(Panel::Agents { selected }) => {
                    if let Some((id, agent)) = self.agent_rows().get(*selected) {
                        if agent.state == "Running" && self.collecting_orchestration() {
                            self.report(io, "The transcript is available once the agent finishes");
                            return None;
                        }
                        let id = id.clone();
                        self.agents_return = Some((self.slots[self.current].id.clone(), *selected));
                        self.resume(id, io);
                    }
                }
                Some(Panel::Image { .. }) => self.copy_image_path(io),
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
        if matches!(&self.menu, Some(Menu::Themes(_)))
            || matches!(&self.menu, Some(Menu::Loading(title)) if title == "/theme")
        {
            if matches!(mouse.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                self.navigate(mouse.kind == MouseKind::WheelUp);
            }
            return;
        }
        if matches!(&self.menu, Some(Menu::Orchestration { .. })) {
            if matches!(mouse.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
                self.page(mouse.kind == MouseKind::WheelUp);
            }
            return;
        }
        let mut hovered = self.focus;
        if matches!(
            mouse.kind,
            MouseKind::Press | MouseKind::WheelUp | MouseKind::WheelDown
        ) {
            self.update_displayed(io);
            if let Some(frame) = &self.displayed
                && let Some((begin, end, right)) = frame.chat_bounds
            {
                if mouse.row >= begin && mouse.row < end {
                    hovered = if mouse.column < right {
                        Focus::Chat
                    } else if self.panel.is_some() {
                        Focus::Inspector
                    } else {
                        self.focus
                    };
                }
                let mut body_end = end;
                if let Some((column, row, width, height)) = frame.inspector_bounds() {
                    let panel = self.inspector();
                    let footer =
                        usize::from(panel.as_ref().is_some_and(|panel| !panel.footer.is_empty()));
                    body_end = body_end.max(row + height + footer);
                    if mouse.column >= column.saturating_sub(3)
                        && mouse.column < column + width
                        && mouse.row >= row.saturating_sub(3)
                        && mouse.row < row + height + footer
                    {
                        hovered = Focus::Inspector;
                    }
                }
                if mouse.row >= body_end {
                    hovered = Focus::Chat;
                }
            }
            if mouse.kind == MouseKind::Press {
                self.focus = hovered;
            }
        }
        if matches!(mouse.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            if let Some(selection) = self
                .selection
                .as_mut()
                .filter(|selection| selection.is_chat())
            {
                selection.drag(mouse.column, mouse.row);
                self.drag_scroll(mouse.kind == MouseKind::WheelUp, 3, io);
                return;
            }
            let focus = self.focus;
            self.focus = hovered;
            self.page(mouse.kind == MouseKind::WheelUp);
            self.focus = focus;
            return;
        }
        match mouse.kind {
            MouseKind::Press => {
                self.slots[self.current].cancel_history_jump();
                self.update_displayed(io);
                self.selection = None;
                self.selection_frame = None;
                self.next_drag_scroll = None;
                self.tool_press = None;
                // A partially painted frame has no single truthful text
                // snapshot. Selection starts only on a completed paint.
                if !self.frames.is_empty() || io.writer.failed() {
                    return;
                }
                let tab = self
                    .displayed
                    .as_ref()
                    .and_then(|frame| frame.inspector_bounds())
                    .zip(self.inspector())
                    .and_then(|((column, row, _, _), panel)| {
                        if panel.tabs.is_empty() || mouse.row != row.saturating_sub(2) {
                            return None;
                        }
                        let mut at = column;
                        for (index, label) in panel.tabs.iter().enumerate() {
                            let width = super::super::cells::width(label);
                            if mouse.column >= at && mouse.column < at + width {
                                return Some(index);
                            }
                            at += width + 2;
                        }
                        None
                    });
                if let Some(tab) = tab {
                    if tab == 1 && matches!(self.panel, Some(Panel::Files(_))) {
                        self.open_terminal(io);
                        return;
                    }
                    if tab == 0 && matches!(self.panel, Some(Panel::Terminal)) {
                        self.hide_terminal(io);
                        return;
                    }
                    if let Some(Panel::Files(files)) = &mut self.panel {
                        if (2..5).contains(&tab) && files.document.is_some() {
                            files.tab = tab - 2;
                            files.offset = 0;
                            files.preview_end = false;
                        }
                        return;
                    }
                }
                if let Some(frame) = &self.displayed {
                    let in_chat = frame.chat_bounds.is_some_and(|(begin, end, right)| {
                        mouse.row >= begin && mouse.row < end && mouse.column < right
                    });
                    if in_chat
                        && let Some((_, key)) =
                            frame.tool_actions.iter().find(|(row, _)| *row == mouse.row)
                    {
                        self.tool_press = Some((
                            mouse.column,
                            mouse.row,
                            key.clone(),
                            frame.edit_view(key),
                            frame.output_view(key),
                        ));
                    }
                    if !in_chat && self.focus == Focus::Chat {
                        return;
                    }
                    let slot = &mut self.slots[self.current];
                    self.selection = if in_chat {
                        Selection::chat(frame.clone(), mouse.column, mouse.row)
                    } else if matches!(self.panel, Some(Panel::Terminal)) {
                        Some(Selection::new(frame.clone(), mouse.column, mouse.row))
                    } else {
                        None
                    };
                    if in_chat && self.selection.is_some() {
                        let reading = slot.projected();
                        slot.state.follow_tail = false;
                        slot.state.reading_total = slot
                            .state
                            .snapshot
                            .as_ref()
                            .map_or(0, |s| s.transcript_size);
                        if let Some((begin, end)) = frame.timeline_range {
                            slot.timeline_query = GetTranscript {
                                before: end,
                                limit: end.saturating_sub(begin).max(1),
                                forward: false,
                            };
                            slot.refresh = Instant::now();
                        }
                        if let Some(reading) = reading {
                            slot.state.reading = Some(reading);
                        }
                    }
                }
            }
            MouseKind::Drag => {
                if self
                    .tool_press
                    .as_ref()
                    .is_some_and(|(column, row, _, _, _)| {
                        (*column, *row) != (mouse.column, mouse.row)
                    })
                {
                    self.tool_press = None;
                }
                if let Some(selection) = &mut self.selection {
                    selection.drag(mouse.column, mouse.row);
                }
            }
            MouseKind::Release => {
                self.update_displayed(io);
                if let Some((column, row, key, edit, output)) = self.tool_press.take()
                    && (column, row) == (mouse.column, mouse.row)
                {
                    self.activate_result(key, edit, output, io);
                    return;
                }
                if let Some(mut selection) = self.selection.take()
                    && let Some(text) = selection.finish(mouse.column, mouse.row)
                    && !io.writer.clipboard(&text)
                {
                    self.report(
                        io,
                        "Could not copy selection. The clipboard limit or output queue is full",
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
            Menu::Orchestration { .. } => return None,
            Menu::Themes(preview) => Popup {
                title: "/theme".into(),
                subtitle: "Theme preview".into(),
                items: preview
                    .themes()
                    .iter()
                    .map(|theme| Choice {
                        label: theme.name().into(),
                        detail: if theme.id() == preview.original().id() {
                            "current".into()
                        } else if theme.is_dynamic() {
                            "system palette".into()
                        } else {
                            theme.id().into()
                        },
                    })
                    .collect(),
                selected: preview.selected(),
                footer: "Enter save · Esc cancel".into(),
            },
            Menu::Loading(title) => Popup {
                title: title.clone(),
                subtitle: "Loading…".into(),
                footer: "Esc cancel".into(),
                ..Popup::default()
            },
            Menu::Sessions {
                entries,
                selected,
                resume_target,
                continuation,
            } => Popup {
                title: "Project sessions".into(),
                subtitle: "Choose a conversation".into(),
                items: resume_target
                    .iter()
                    .map(|_| Choice {
                        label: "Resume current request".into(),
                        detail: String::new(),
                    })
                    .chain(continuation.iter().map(|(_, title)| Choice {
                        label: "Resume orchestration".into(),
                        detail: title.clone(),
                    }))
                    .chain(entries.iter().map(|e| Choice {
                        label: e.title.clone().unwrap_or_else(|| "New conversation".into()),
                        detail: format!(
                            "{} · {}{}",
                            if e.entry.kind == "fork" {
                                "Fork"
                            } else {
                                "Conversation"
                            },
                            e.status.label(),
                            e.preview
                                .as_ref()
                                .map_or(String::new(), |preview| format!(" · {preview}"))
                        ),
                    }))
                    .collect(),
                selected: *selected,
                footer: "Enter open · Esc back".into(),
            },
            Menu::Models { models, selected } => Popup {
                title: "/model".into(),
                subtitle: "Applies to future messages".into(),
                items: models
                    .iter()
                    .map(|m| Choice {
                        label: m.0.clone(),
                        detail: if m.0 == m.1 {
                            String::new()
                        } else {
                            m.1.clone()
                        },
                    })
                    .collect(),
                selected: *selected,
                footer: "Enter choose effort · Esc cancel".into(),
            },
            Menu::Efforts {
                model,
                values,
                selected,
                previous,
            } => Popup {
                title: "/effort".into(),
                subtitle: format!("{model} · future messages"),
                items: values
                    .iter()
                    .map(|e| Choice {
                        label: e.clone().unwrap_or_else(|| "Default".into()),
                        detail: String::new(),
                    })
                    .collect(),
                selected: *selected,
                footer: if previous.is_some() {
                    "Enter apply · Esc models"
                } else {
                    "Enter apply · Esc cancel"
                }
                .into(),
            },
            Menu::Help { selected, entries } => commands::help_with_commands(*selected, entries),
        })
    }
    fn inspector(&self) -> Option<Inspector> {
        match self.panel.as_ref()? {
            Panel::Orchestration { lines, offset } => {
                let footer = if let Some(Menu::Orchestration { selected, proposal }) = &self.menu {
                    let blocked = proposal.plan.execution_ready().err();
                    let actions = [
                        if blocked.is_none() {
                            "Start"
                        } else {
                            "Start unavailable"
                        },
                        "Refine",
                        "Cancel",
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        if index == *selected {
                            format!("[{label}]")
                        } else {
                            label.into()
                        }
                    })
                    .collect::<Vec<String>>()
                    .join("   ");
                    format!("{actions}   ·   ↑/↓ · Enter")
                } else {
                    "PgUp/PgDn or scroll · Esc chat".into()
                };
                Some(Inspector {
                    title: "Plan · PgUp/PgDn or scroll".into(),
                    lines: lines.clone(),
                    offset: *offset,
                    footer,
                    kind: InspectorKind::Plan,
                    ..Inspector::default()
                })
            }
            Panel::Terminal => {
                let snapshot = self.terminal.as_ref()?.snapshot();
                Some(Inspector {
                    title: format!("Terminal · {}", snapshot.cwd.display()),
                    tabs: vec!["Files".into(), "Terminal".into()],
                    selected_tab: 1,
                    kind: InspectorKind::Command,
                    rendered: Some(
                        self.terminal_grid
                            .as_ref()
                            .filter(|(revision, _)| *revision == snapshot.revision)
                            .map_or_else(
                                || std::sync::Arc::new(snapshot.rows.clone()),
                                |(_, rows)| rows.clone(),
                            ),
                    ),
                    cursor: snapshot
                        .cursor
                        .map(|(row, column)| (usize::from(column), usize::from(row))),
                    footer: snapshot.error.clone().unwrap_or_else(|| {
                        if snapshot.exited {
                            "Terminal exited · select Files".into()
                        } else if snapshot.shell_ready {
                            "Esc files · commands run locally".into()
                        } else {
                            "Ctrl+C interrupt · keys go to the program".into()
                        }
                    }),
                    ..Inspector::default()
                })
            }
            Panel::Output { view, offset } => Some(Inspector {
                title: view.title.clone(),
                lines: self.output_lines(&view.text),
                offset: *offset,
                footer: "j/k lines · PgUp/PgDn pages · g/G start/end · Esc back".into(),
                ..Inspector::default()
            }),
            Panel::NativeTerminal { view, .. } => Some(Inspector {
                title: view
                    .as_ref()
                    .map_or_else(|| "Agent terminal".into(), |view| view.title.clone()),
                lines: view.as_ref().map_or_else(Vec::new, |view| {
                    view.text.lines().map(str::to_owned).collect()
                }),
                footer: "Esc chat".into(),
                ..Inspector::default()
            }),
            Panel::EditDiff {
                title,
                rows,
                offset,
                column,
            } => Some(Inspector {
                title: title.clone(),
                offset: *offset,
                kind: InspectorKind::Diff,
                diff: Some(rows.clone()),
                diff_column: *column,
                footer: "j/k lines · h/l horizontal · f expand · Esc chat".into(),
                ..Inspector::default()
            }),
            Panel::Agents { selected } => Some(Inspector {
                title: "Conversation agents".into(),
                agents: self.agent_rows().into_iter().map(|(_, row)| row).collect(),
                selected: Some(*selected),
                footer: "↑/↓ select · Enter transcript · Esc back".into(),
                kind: InspectorKind::Agents,
                ..Inspector::default()
            }),
            Panel::Image { name, view, .. } => Some(Inspector {
                title: format!("Chart · {name}"),
                lines: view.as_ref().map_or_else(
                    || vec!["Opening PNG…".into()],
                    |image| {
                        let mut lines = vec![format!("PNG · {} × {}", image.width, image.height)];
                        if self.graphics_id.is_none() {
                            lines.push(image.path.display().to_string());
                        }
                        lines
                    },
                ),
                footer: if self.focus == Focus::Inspector {
                    "Enter copy path · Esc close"
                } else {
                    "Tab chart · Esc close"
                }
                .into(),
                kind: InspectorKind::Image,
                ..Inspector::default()
            }),
            Panel::Files(files) => {
                if files.loading {
                    return Some(Inspector {
                        title: "Files".into(),
                        tabs: vec!["Files".into(), "Terminal".into()],
                        lines: vec!["Opening…".into()],
                        footer: "q terminal · Esc chat".into(),
                        ..Inspector::default()
                    });
                }
                if let Some(document) = &files.document {
                    let (kind, text) = match files.tab {
                        1 => (
                            InspectorKind::Diff,
                            if document.diff.status == DiffStatus::Unchanged {
                                "No changes since HEAD"
                            } else if matches!(
                                document.diff.status,
                                DiffStatus::NoRepository
                                    | DiffStatus::NoCommit
                                    | DiffStatus::Unavailable
                            ) {
                                "Diff unavailable: HEAD is missing or the file is too large"
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
                                    "Preparing preview…"
                                }
                            } else {
                                "Preview supports Markdown"
                            },
                        ),
                        _ => (InspectorKind::Source, document.source.as_str()),
                    };
                    Some(Inspector {
                        title: format!(
                            "{} · {}{}{}",
                            document
                                .path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy(),
                            if files.tab == 1 {
                                if matches!(
                                    document.diff.status,
                                    DiffStatus::Modified
                                        | DiffStatus::Untracked
                                        | DiffStatus::Unchanged
                                ) {
                                    "HEAD → working file"
                                } else {
                                    "Diff"
                                }
                            } else {
                                "read-only"
                            },
                            if document.truncated {
                                " · excerpt"
                            } else {
                                ""
                            },
                            document
                                .path
                                .parent()
                                .map(|parent| self.relative_directory(parent))
                                .filter(|parent| parent != ".")
                                .map_or_else(String::new, |parent| format!(" · {parent}"))
                        ),
                        tabs: vec![
                            "Files".into(),
                            "Terminal".into(),
                            "Source".into(),
                            "Diff".into(),
                            "Preview".into(),
                        ],
                        selected_tab: files.tab + 2,
                        lines: text.lines().map(String::from).collect(),
                        offset: files.offset,
                        footer: files.jump.as_ref().map_or_else(
                            || {
                                if files.tab == 1 {
                                    "f expand diff · s/d/p tabs · Esc chat".into()
                                } else {
                                    "s/d/p tabs · :line · e editor · q terminal · Esc chat".into()
                                }
                            },
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
                            .map_or_else(|| "Files".into(), |l| self.relative_directory(&l.path)),
                        tabs: vec!["Files".into(), "Terminal".into()],
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
                        footer: "j/k move · h parent · l directory · Enter file · q terminal"
                            .into(),
                        ..Inspector::default()
                    })
                }
            }
        }
    }
    fn ui(&self) -> Workspace {
        let slot = &self.slots[self.current];
        let mut breadcrumb = Vec::new();
        let mut current = Some(slot.id.clone());
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_SESSIONS {
            let Some(id) = current else {
                break;
            };
            if !seen.insert(id.clone()) {
                break;
            }
            if let Some(title) = self.known_title(&id) {
                breadcrumb.push(title);
            } else if id == slot.id && slot.kind == Kind::Fork {
                breadcrumb.push("Fork".into());
            }
            if let Some(ancestor) = self.slots.iter().find(|s| s.id == id) {
                current = ancestor.parent.clone();
            } else if let Some(ancestor) =
                self.summaries.iter().find(|summary| summary.entry.id == id)
            {
                current = ancestor.entry.parent.clone();
            } else {
                break;
            }
        }
        breadcrumb.reverse();
        let snapshot = slot.state.snapshot.as_ref();
        let generation = snapshot.map(|s| &s.generation);
        Workspace {
            breadcrumb: breadcrumb.join(" / "),
            model: if slot.kind == Kind::Console {
                "Local console".into()
            } else {
                generation.map_or_else(String::new, |g| g.model.clone())
            },
            effort: generation
                .and_then(|g| g.effort.clone())
                .unwrap_or_default(),
            activity: if self.orchestration_mode {
                "Orchestration".into()
            } else {
                String::new()
            },
            exit_confirmation: self.exit_deadline.is_some(),
            reading: !slot.state.follow_tail,
            new_events: snapshot.map_or(0, |s| {
                s.transcript_size.saturating_sub(slot.state.reading_total)
            }),
            focus: self.focus,
            inspector: self.inspector(),
            popup: self.popup(),
            suggestions: self
                .completion
                .as_ref()
                .filter(|_| self.menu.is_none() && self.focus == Focus::Chat)
                .map(|completion| completion.popup(completion.exact_command(&slot.state.editor))),
            animate: self.animation_enabled(),
            animation_frame: Self::animation_frame(),
            draft_notice: !slot.state.editor.bytes.is_empty()
                && (self.collecting_orchestration()
                    || self.orchestration_summary.contains_key(&slot.id)),
            interrupt_hint: if !slot.state.editor.bytes.is_empty() {
                "Ctrl+C clear draft"
            } else if self.collecting_orchestration() || slot.interruptible() {
                "Ctrl+C stop"
            } else {
                ""
            },
            result_selected: self
                .result_selected
                .clone()
                .filter(|_| self.focus == Focus::Chat),
            wide_diff: self.wide_diff
                && self.focus == Focus::Inspector
                && (matches!(&self.panel, Some(Panel::EditDiff { .. }))
                    || matches!(&self.panel, Some(Panel::Files(files)) if files.tab == 1)),
        }
    }
    fn animation_enabled(&self) -> bool {
        let slot = &self.slots[self.current];
        self.selection.is_none()
            && slot.state.follow_tail
            && self.menu.is_none()
            && self.completion.is_none()
            && (self.collecting_orchestration()
                || slot.state.snapshot.as_ref().is_some_and(|view| {
                    view.worker_busy
                        && !matches!(view.state.as_str(), "completed" | "cancelled" | "paused")
                }))
            && std::env::var_os("ANSA_NO_ANIMATION").is_none()
    }
    fn animation_frame() -> u8 {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        ((START.get_or_init(Instant::now).elapsed().as_millis() / 120) % 32) as u8
    }
    fn update_displayed(&mut self, io: &Io) {
        sync_displayed(&mut self.frames, &mut self.displayed, |sequence| {
            io.writer.displayed(sequence)
        });
        if self.selection.is_none() {
            self.selection_frame = None;
        } else if self
            .selection_frame
            .as_ref()
            .is_some_and(|(sequence, _)| io.writer.displayed(*sequence))
            && let (Some(selection), Some((_, shown))) =
                (&mut self.selection, self.selection_frame.take())
        {
            selection.commit_scroll(shown);
        }
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
        if self.selection_frame.is_some() {
            return;
        }
        self.prepare_preview(io);
        if matches!(self.panel, Some(Panel::Terminal))
            && let Some(terminal) = &self.terminal
        {
            let snapshot = terminal.snapshot();
            if self
                .terminal_grid
                .as_ref()
                .is_none_or(|(revision, _)| *revision != snapshot.revision)
            {
                self.terminal_grid = Some((
                    snapshot.revision,
                    std::sync::Arc::new(snapshot.rows.clone()),
                ));
            }
        }
        let operational =
            self.operational_frame.is_some() || self.slots[self.current].input_ready();
        let mut selection_frame = None;
        let mut frame = if let Some(selection) = &self.selection {
            let mut ui = self.ui();
            ui.animate = false;
            let projected = self.projected();
            let slot = &mut self.slots[self.current];
            let mut live = slot
                .state
                .render_workspace(io, context, &ui, projected.as_deref());
            if let Some(live) = &mut live {
                live.timeline_range = slot.timeline.as_ref().map(|page| (page.begin, page.end));
                live.timeline_total = slot.timeline.as_ref().map_or(0, |page| page.total);
                if selection.scroll_pending && self.selection_frame.is_none() {
                    let mut next = selection.clone();
                    next.update(
                        live.clone(),
                        slot.state.scroll,
                        io.color.then_some(&self.theme),
                    );
                    if !next.scroll_pending {
                        selection_frame = Some(next);
                    }
                }
                if io.color {
                    live.apply_theme(&self.theme);
                }
            }
            let mut frozen = selection_frame
                .as_ref()
                .unwrap_or(selection)
                .frame_at(io.size, io.color);
            if let Some(live) = live {
                frozen.update_status_from(&live);
            }
            Some(frozen)
        } else if !operational {
            let status = if !self.slots[self.current].state.status.trim().is_empty() {
                self.slots[self.current].state.status.trim()
            } else if self.slots[self.current].startup_cancelled {
                "Session open cancelled"
            } else {
                "Opening session"
            };
            let mut frame = carbon::layout_full(&[], b"", 0, status, 0, context, io.size, io.color);
            if let Some(frame) = &mut frame
                && let Some(header) = frame.lines.first_mut()
            {
                *header = header.replacen("ansa", "ANSA", 1);
            }
            frame
        } else {
            let mut ui = self.ui();
            ui.animate &= io.color;
            let projected = self.projected();
            let slot = &mut self.slots[self.current];
            slot.state
                .render_workspace(io, context, &ui, projected.as_deref())
        };
        if self.selection.is_none()
            && let (Some(Panel::Output { view, .. }), Some((_, _, width, _))) = (
                &self.panel,
                frame.as_ref().and_then(|frame| frame.inspector_bounds()),
            )
            && self
                .displayed
                .as_ref()
                .and_then(|frame| frame.inspector_bounds())
                .is_none_or(|(_, _, old_width, _)| old_width != width)
        {
            let mut lines = Vec::new();
            for line in view.text.lines() {
                let mut part = String::new();
                let mut used = 0;
                for glyph in super::super::cells::graphemes(line) {
                    let cells = super::super::cells::width(glyph);
                    if used + cells > width.max(1) && !part.is_empty() {
                        lines.push(std::mem::take(&mut part));
                        used = 0;
                    }
                    part.push_str(glyph);
                    used += cells;
                }
                lines.push(part);
            }
            let mut ui = self.ui();
            ui.animate &= io.color;
            if let Some(panel) = &mut ui.inspector {
                panel.lines = lines;
            }
            let projected = self.projected();
            let slot = &mut self.slots[self.current];
            frame = slot
                .state
                .render_workspace(io, context, &ui, projected.as_deref());
        }
        if self.selection.is_none()
            && matches!(self.panel, Some(Panel::Terminal))
            && let Some(bounds) = frame.as_ref().and_then(|frame| frame.inspector_bounds())
            && self.terminal_size
                != Some((bounds.3.clamp(1, 128) as u16, bounds.2.clamp(1, 256) as u16))
            && let Some(terminal) = &self.terminal
        {
            let size = (bounds.3.clamp(1, 128) as u16, bounds.2.clamp(1, 256) as u16);
            if let Err(error) = terminal.resize(size.0, size.1) {
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    self.report(io, &format!("Could not resize terminal: {error}"));
                }
            } else {
                self.terminal_size = Some(size);
            }
        }
        if io.color
            && self.selection.is_none()
            && let Some(frame) = &mut frame
        {
            frame.apply_theme(&self.theme);
        }
        if self.selection.is_none()
            && let Some(frame) = &mut frame
            && let Some(page) = &self.slots[self.current].timeline
        {
            frame.timeline_range = Some((page.begin, page.end));
            frame.timeline_total = page.total;
        }
        if self.menu.is_none()
            && let Some(id) = self.graphics_id
            && io.color
            && let Some(Panel::Image {
                view: Some(image), ..
            }) = &self.panel
            && let Some(frame) = &mut frame
            && let Some((column, row, width, height)) = frame.inspector_bounds()
            && height > 1
            && width > 0
        {
            frame.image = Some(graphics::Placement::new(
                id,
                image,
                (column, row + 1, width, height - 1),
                io.input.cell_pixels(),
            ));
        }
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
            slot.state.scroll_limit = frame.scroll_limit;
        }
        if let Some(frame) = frame
            && self.last_frame.as_ref() != Some(&frame)
            && io.writer.idle()
        {
            let sequence = io.writer.frame(frame.clone());
            if let Some(selection) = selection_frame {
                self.selection_frame = Some((sequence, selection));
            }
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

fn model_choices(value: Json) -> Result<Vec<(String, String, Vec<String>)>> {
    Ok(crate::auth::dispatch::model_capabilities(&value)?
        .into_iter()
        .take(256)
        .map(|model| (model.model, model.display_name, model.efforts))
        .collect())
}

pub async fn run(
    runtime: Runtime,
    io: &mut Io,
    context: &str,
    factory: Factory,
    root_id: String,
    kind: Kind,
    parent: Option<String>,
) -> Result<i32> {
    run_attached(runtime, io, context, factory, root_id, kind, parent, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_attached(
    mut runtime: Runtime,
    io: &mut Io,
    context: &str,
    factory: Factory,
    root_id: String,
    kind: Kind,
    parent: Option<String>,
    bridge_options: Option<super::super::bridge::BridgeOptions>,
) -> Result<i32> {
    if io.raw.is_none() {
        if bridge_options.is_some() {
            return Err("workspace_requires_tty");
        }
        return super::run(&mut runtime, io, None, false, context).await;
    }
    let mut bridge = bridge_options
        .as_ref()
        .map(|options| {
            super::super::bridge::Bridge::start_with_artifacts(
                &options.directory,
                runtime.handle(),
                options.editor.clone(),
                options.artifact_store.clone(),
            )
        })
        .transpose()?;
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
    let mut animated_frame = None;
    let mut deferred_input = std::collections::VecDeque::new();
    let mut failure: Option<&'static str> = None;
    let mut terminating_since = None;
    let mut editor_cancel_since: Option<Instant> = None;
    let result: Result<()> = async {
        while !finished || work.editor.is_some() {
            if let Some(bridge) = &mut bridge
                && let Some(root) = work.slots.iter().find(|slot| slot.id == work.root_id) {
                bridge.set_input_mode(root.state.mode);
            }
            work.update_displayed(io);
            let mut input_guard = None; let mut signal_guard = None;
            let input_enabled = work.terminal_pending_input.is_empty() && !finished && !work.input_closed && work.editor.is_none() && work.pending_editor.is_none() && !work.editor_preparing
                && work.slots[work.current].state.pending_draft.is_none()
                && work.slots[work.current].ready && !work.slots[work.current].ready_refresh
                && work.slots[work.current].state.history_ready && work.displayed.is_some()
                && work.operational_frame.is_some_and(|sequence| io.writer.displayed(sequence));
            tokio::select! { biased;
                ready = signal_events.readable() => { signal_guard = Some(ready.map_err(|_| "cli_signal_poll")?); }
                event = work.events.recv() => { if let Some((id, event)) = event {
                    match event {
                        SessionEvent::Ready(result) => work.readiness(&id, result, io),
                        SessionEvent::Runtime(event) => {
                            if let Some(bridge) = &mut bridge { bridge.publish(&event); }
                            work.event(&id, Some(event), io)
                        },
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
                        if work.terminal_focused() { work.terminal_bytes(&[3], io); }
                        else if let Some(editor) = &mut work.editor { editor_cancel_since.get_or_insert(Instant::now()); if let Err(code) = editor.cancel() { failure.get_or_insert(code); finished = true; work.terminating = true; terminating_since.get_or_insert(Instant::now()); } }
                        else if work.pending_editor.is_some() || work.editor_preparing { work.cancel_editor_request(); }
                        else if !finished {
                            if work.input_closed { work.cancel_current(io); }
                            else { finished |= work.interrupt(io, Instant::now()); }
                        }
                    }
                    SIGTSTP => {
                        if work.terminal_focused() { work.terminal_bytes(&[26], io); }
                        else if let Some(editor) = &mut work.editor { editor_cancel_since.get_or_insert(Instant::now()); if let Err(code) = editor.cancel() { failure.get_or_insert(code); finished = true; work.terminating = true; terminating_since.get_or_insert(Instant::now()); } }
                        else { io.suspend().await?; }
                        work.last_frame = None;
                    }
                    SIGCONT => { io.resume_terminal()?; work.last_frame = None; }
                    SIGWINCH => { io.resize(); work.last_frame = None;
                        if (io.size.0 < 12 || io.size.1 < 8) && work.focus == Focus::Inspector {
                            let pending = work.terminal_decoder.take_pending();
                            if !pending.is_empty() { work.terminal_bytes(&pending, io); }
                            work.terminal_escape_at = None;
                            work.focus = Focus::Chat;
                        }
                    }
                    _ => {}
                }
            }
            if work.editor_wait.as_mut().is_some_and(|wait| {
                let now = Instant::now(); let written = io.writer.written();
                if written != wait.written { wait.written = written; wait.progress = now; }
                now.duration_since(wait.progress) >= Duration::from_millis(250) || now.duration_since(wait.began) >= Duration::from_secs(1)
            }) {
                work.cancel_editor_request(); work.report(io, "Could not hand off the terminal to the editor: output is still pending"); dirty = true;
            }
            if let Some(mut editor) = work.editor.take() {
                if editor_cancel_since.is_some_and(|began| began.elapsed() >= Duration::from_secs(2)) || (finished && terminating_since.is_some_and(|began| began.elapsed() >= Duration::from_secs(2))) {
                    let _ = editor.abort_for_exit(io).map_err(|code| { failure.get_or_insert(code); });
                    failure.get_or_insert("editor_cleanup_unknown");
                    finished = true; work.terminating = true;
                } else { match editor.poll(io) {
                    Ok(Some(status)) => {
                        if status.cleanup_unknown { work.report(io, "Editor exited. Detached process state is unknown"); }
                        else { work.report(io, &format!("Editor exited: {}", status.code)); }
                        work.last_frame = None; work.displayed = None; work.frames.clear(); dirty = true;
                        editor_cancel_since = None;
                        if !finished && !work.input_closed && let Some(Panel::Files(files)) = &work.panel && let Some(document) = &files.document { work.open_file(document.path.clone(), io); }
                    }
                    Ok(None) => work.editor = Some(editor),
                    Err(code) => {
                        work.report(io, &format!("Could not open editor: {code}")); work.last_frame = None; dirty = true;
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
                    Err(code) => work.report(io, &format!("Could not open editor: {code}")),
                }
            }
            let now = Instant::now();
            dirty |= work.update_orchestration_progress();
            dirty |= work.tick_drag(io, now);
            work.refresh_terminal_screen(now);
            work.flush_terminal_input(io);
            if let Some(terminal) = &work.terminal {
                let snapshot = terminal.snapshot();
                if work.terminal_grid.as_ref().is_none_or(|(revision, _)| *revision != snapshot.revision) {
                    work.terminal_grid = Some((snapshot.revision, std::sync::Arc::new(snapshot.rows.clone())));
                    dirty |= matches!(work.panel, Some(Panel::Terminal));
                }
                if matches!(work.panel, Some(Panel::Terminal)) && let Some((_, _, width, height)) = work.displayed.as_ref().and_then(|frame| frame.inspector_bounds()) {
                    dirty |= work.terminal_size != Some((height.clamp(1, 128) as u16, width.clamp(1, 256) as u16));
                }
            }
            if work.terminal_decoder.pending_escape() {
                let at = work.terminal_escape_at.get_or_insert(now);
                if now.duration_since(*at) >= Duration::from_millis(35) {
                    if let Some(event) = work.terminal_decoder.flush_escape() { work.terminal_event(event, io, &mut deferred_input); dirty = true; }
                    work.terminal_escape_at = None;
                }
            } else { work.terminal_escape_at = None; }
            work.prepare_theme(now);
            let next_animation = (io.color && work.animation_enabled()).then(Workbench::animation_frame);
            dirty |= next_animation != animated_frame;
            animated_frame = next_animation;
            let mut admissions = Vec::new();
            for slot in &mut work.slots {
                dirty |= slot.poll_replies(io)?;
                admissions.append(&mut slot.admissions);
                if work.editor.is_none() && !finished { slot.refresh(io, now); }
                dirty |= slot.state.progress.tick(now);
                if slot.state.explicit_status { dirty |= slot.state.wait_clock.tick(now); }
            }
            for (id, accepted) in admissions {
                if !accepted {
                    work.pending_context.remove(&id);
                    work.orchestration_summary.retain(|_, pending| pending.0 != id);
                }
            }
            if work.exit_deadline.is_some_and(|deadline| now >= deadline || work.slots[work.current].interruptible()) {
                dirty |= work.disarm_exit();
            }
            if work.input_editor().escape_pending() {
                let at = escape_at.get_or_insert(now);
                if now.duration_since(*at) >= Duration::from_millis(35) {
                    if let Some(Action::Escape) = work.input_editor_mut().flush_escape() { work.escape(io); dirty = true; }
                    escape_at = None;
                }
            } else { escape_at = None; }
            let can_read = work.terminal_pending_input.is_empty() && !finished && !work.input_closed && work.editor.is_none() && work.pending_editor.is_none() && !work.editor_preparing && work.slots[work.current].state.pending_draft.is_none()
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
                        if work.terminal_focused() {
                            work.terminal_chunk(&bytes[..count], io, &mut deferred_input);
                            continue;
                        }
                        for (position, &byte) in bytes[..count].iter().enumerate() {
                            work.disarm_exit();
                            // Modal/read-only navigation never edits a draft.
                            if byte.is_ascii() && !work.input_editor().escape_pending() && !work.input_editor().pasting() {
                                let jumping = matches!(&work.panel, Some(Panel::Files(files)) if files.jump.is_some());
                                if byte == 7 && work.focus == Focus::Chat && work.menu.is_none() && work.completion.is_none() {
                                    work.navigate_results(None);
                                    continue;
                                }
                                if byte == b'f' && work.focus == Focus::Inspector
                                    && (matches!(&work.panel, Some(Panel::EditDiff { .. }))
                                        || matches!(&work.panel, Some(Panel::Files(files)) if files.tab == 1 && files.document.is_some())) {
                                    work.wide_diff = !work.wide_diff;
                                    continue;
                                }
                                if matches!(byte, b'j' | b'k') && !jumping && (work.focus == Focus::Inspector && matches!(work.panel, Some(Panel::Files(_) | Panel::EditDiff { .. } | Panel::Output { .. }))) { work.navigate(byte == b'k'); continue; }
                                let file_visible = work.displayed.as_ref().map_or(1, |frame| frame.inspector_content_rows).max(1);
                                let inspector_width = work.displayed.as_ref().and_then(|frame|frame.chat_bounds).map_or(io.size.0.saturating_sub(1),|(_,_,right)|io.size.0.saturating_sub(right+4));
                                if work.focus == Focus::Inspector && matches!(work.panel, Some(Panel::Output { .. })) {
                                    let maximum = match &work.panel { Some(Panel::Output { view, .. }) => work.output_lines(&view.text).len().saturating_sub(file_visible), _ => 0 };
                                    if let Some(Panel::Output { offset, .. }) = &mut work.panel {
                                        if byte == b'g' { *offset = 0; continue; }
                                        if byte == b'G' { *offset = maximum; continue; }
                                    }
                                    if byte >= 32 && byte != 127 { continue; }
                                }
                                if work.focus == Focus::Inspector && matches!(work.panel, Some(Panel::Agents { .. })) && byte >= 32 && byte != 127 { continue; }
                                if work.focus == Focus::Inspector && let Some(Panel::EditDiff { rows, offset, column, .. }) = &mut work.panel {
                                    if byte == b'g' { *offset = 0; continue; }
                                    if byte == b'G' { *offset = rows.len().saturating_sub(file_visible); continue; }
                                    if matches!(byte,b'h'|b'l'|b'0'|b'$') {
                                        let maximum = rows.iter().map(|row|row.horizontal_limit(inspector_width)).max().unwrap_or(0);
                                        *column = match byte { b'h'=>column.saturating_sub(4),b'l'=>column.saturating_add(4).min(maximum),b'$'=>maximum,_=>0 };
                                        continue;
                                    }
                                    if byte >= 32 && byte != 127 { continue; }
                                }
                                if work.focus == Focus::Inspector && let Some(Panel::Files(files)) = &mut work.panel {
                                    if let Some(line) = &mut files.jump {
                                        if byte.is_ascii_digit() { if line.len() < 9 { line.push(byte as char); } continue; }
                                        if matches!(byte, 8 | 127) { line.pop(); continue; }
                                        if !matches!(byte, 27 | b'\r' | b'\n' | 9) { continue; }
                                    } else {
                                        if byte == b':' && files.document.is_some() { files.jump = Some(String::new()); continue; }
                                        if byte == b'g' { files.preview_end = false; files.offset = 0; if files.document.is_none() { files.selected = 0; } continue; }
                                        if byte == b'G' { if files.document.is_none() { files.selected = files.listing.as_ref().map_or(0, |listing| listing.entries.len().saturating_sub(1)); } else { files.preview_end = files.tab == 2 && files.preview_rows().is_none(); let visible = if files.preview_end { 1 } else { file_visible }; files.offset = files.last_line().saturating_sub(visible - 1); } continue; }
                                    }
                                    if byte == b'l' && files.document.is_none() {
                                        if let Some(path) = files.listing.as_ref().and_then(|listing| listing.entries.get(files.selected)).filter(|entry| entry.directory).map(|entry| entry.path.clone()) {
                                            work.open_file(path, io);
                                        }
                                        continue;
                                    }
                                    if let Some(tab) = match byte { b's' => Some(0), b'd' => Some(1), b'p' => Some(2), _ => None } { files.preview_end = false; files.tab = tab; files.offset = 0; continue; }
                                    if byte == b'h' { let parent = files.document.as_ref().and_then(|d| d.path.parent()).or_else(|| files.listing.as_ref().and_then(|l| l.path.parent())).map(Path::to_path_buf); if let Some(parent) = parent { work.open_file(parent, io); } continue; }
                                    if byte == b'e' { work.prepare_editor(io); continue; }
                                    if byte == b'q' { work.open_terminal(io); deferred_input.extend(&bytes[position + 1..count]); break; }
                                    if byte >= 32 && byte != 127 { continue; }
                                }
                                if work.focus == Focus::Chat && work.menu.is_none() && byte == b'?' && work.slots[work.current].state.editor.bytes.is_empty() {
                                    work.menu = Some(Menu::Help { selected: 0, entries: work.available_commands() }); work.completion = None; continue;
                                }
                                if work.menu.is_some() && !matches!(byte, 27 | b'\r' | b'\n' | 9) { continue; }
                            }
                            if byte >= 32 && byte != 127 && !work.input_editor().escape_pending() && !work.input_editor().pasting() {
                                work.selection = None;
                                work.tool_press = None;
                            }
                            let navigating = work.navigating();
                            let complete = (work.completion.is_some() && work.menu.is_none()) || matches!(work.menu, Some(Menu::Help { .. }));
                            let action = if navigating {
                                work.navigation_editor.set_completion(complete);
                                let action = work.navigation_editor.feed(byte, true);
                                // This editor owns only escape/paste framing. Read-only
                                // views must never modify the conversation's draft.
                                work.navigation_editor.bytes.clear();
                                work.navigation_editor.cursor = 0;
                                work.navigation_editor.overflow = false;
                                action.filter(|action| !matches!(action, Action::Overflow))
                            } else {
                                let slot = &mut work.slots[work.current];
                                let before = (slot.state.editor.bytes.len(), slot.state.editor.cursor);
                                slot.state.editor.set_completion(complete);
                                let action = slot.state.editor.feed(byte, true);
                                if before != (slot.state.editor.bytes.len(), slot.state.editor.cursor) {
                                    slot.state.input_history.edited(&slot.state.editor); slot.state.status.clear();
                                }
                                action
                            };
                            if let Some(action) = action {
                                match action {
                                    Action::Submit | Action::QueueSubmit => {
                                        let origin = work.current;
                                        if let Some(text) = work.enter(io) {
                                            let (accepted, done) = work.text(&text, io); finished |= done;
                                            if accepted {
                                                work.selection = None;
                                                let slot = &mut work.slots[origin];
                                                if let Some((id, _, _)) = &slot.state.pending_draft
                                                    && slot.state.pending.as_ref() == Some(id)
                                                {
                                                    slot.follow_accepted_submit = Some(id.clone());
                                                }
                                                slot.state.editor.clear();
                                                slot.state.input_history.reset();
                                                slot.refresh = Instant::now();
                                            }
                                        }
                                    }
                                    Action::Complete => { if matches!(work.menu, Some(Menu::Help { .. })) { work.enter(io); } else { work.complete_command(); } }
                                    Action::Escape => work.escape(io),
                                    Action::FocusNext if work.completion.is_some() && work.menu.is_none() && work.focus == Focus::Chat => work.complete_command(),
                                    Action::FocusNext if work.panel.is_none() && work.menu.is_none() => work.cycle_mode(io),
                                    Action::FocusNext | Action::FocusPrevious => if work.panel.is_some() { work.focus = if work.focus == Focus::Chat && work.displayed.as_ref().and_then(|frame| frame.inspector_bounds()).is_some_and(|(_, _, width, height)| width > 0 && height > 0) { Focus::Inspector } else { Focus::Chat }; },
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
                            work.refresh_completion();
                            if work.terminal_focused() { deferred_input.extend(&bytes[position + 1..count]); break; }
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
    if let Some(mut terminal) = work.terminal.take() {
        match tokio::task::spawn_blocking(move || terminal.shutdown()).await {
            Ok(outcome) if outcome.shell_reaped && outcome.session_empty => {}
            _ => {
                failure.get_or_insert("terminal_cleanup_unknown");
            }
        }
    }
    while let Some(result) = work.jobs.join_next().await {
        match result {
            Ok(Job::ThemeSaved {
                revision,
                theme,
                result,
            }) => {
                if result.is_err() {
                    failure.get_or_insert("theme_preferences_write");
                }
                work.saved_theme(revision, theme, result, io);
            }
            Ok(Job::GenerationSaved {
                result: Err(code), ..
            }) if code.starts_with("model_preferences_") => {
                failure.get_or_insert(code);
            }
            Ok(_) => {}
            Err(_) => {
                failure.get_or_insert("workspace_job");
            }
        }
        // A newer accepted choice can have waited behind an earlier save or
        // another bounded frontend job. Drain that final preference as well.
        work.start_theme_save();
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
    if work.factory.has_terminal_owners() {
        let terminal_factory = work.factory.clone();
        if !tokio::task::spawn_blocking(move || terminal_factory.close_terminals())
            .await
            .unwrap_or(false)
        {
            failure.get_or_insert("terminal_cleanup_unknown");
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
    if let Some(bridge) = bridge.take() {
        bridge.shutdown().await;
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
        let mut slot = Slot::new(runtime, "offline".into(), Kind::Main, None, None);
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
                status: None,
                role: "user".into(),
                text: format!("EARLIEST\n{}", "later\n".repeat(50)),
                action: None,
                edit: None,
                output: None,
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

    #[test]
    fn delayed_model_save_preserves_newer_choice_and_reasoning_effort() {
        let root = TestDirectory(std::env::temp_dir().join(format!(
            "ansa-model-preferences-{}",
            crate::auth::pkce::random_token(16).unwrap()
        )));
        ansa_linux::fs::create_private(&root.0).unwrap();
        let settings = crate::config::load_agent_settings(&root.0, &root.0).unwrap();
        let first = Factory::new(
            settings.clone(),
            root.0.join("auth"),
            false,
            "first".into(),
            None,
        );
        let second = Factory::new(settings, root.0.join("auth"), false, "second".into(), None);
        let older = GenerationSelection {
            model: "earlier".into(),
            effort: None,
        };
        let latest = GenerationSelection {
            model: "latest".into(),
            effort: Some("high".into()),
        };
        let selected = SystemTime::now();
        first
            .save_generation(selected + Duration::from_secs(1), &latest)
            .unwrap();
        first.save_generation(selected, &older).unwrap();
        assert_eq!(first.saved_generation(), Some(latest));
        assert_eq!(second.saved_generation(), None);
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
                    edit: None,
                    output: None,
                    status: None,
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
        let mut slot = Slot::new(runtime, "offline".into(), Kind::Main, None, None);
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
        assert!(shown.contains("running"));
        slot.runtime.shutdown().await.unwrap();
    }
}
