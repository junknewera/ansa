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
        sessions::{Created, Factory, Kind, model_choices},
        theme::{
            self, Catalog as ThemeCatalog, PaletteSource, Preview as ThemePreview,
            Store as ThemeStore, Theme,
        },
        workspace::{DiffStatus, EditorCommand, FileDocument, FileHost, FileListing},
    },
    orchestration::{profile::RoleProfile, run::RunId},
    protocol::json::Json,
    session::{
        catalog::SessionSummary,
        search::{Hit, Matches},
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::sync::mpsc;
mod activity;
mod roles;
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
    conversation_page: Option<TranscriptPage>,
    conversation_query: GetTranscript,
    activity_timeline: Option<TranscriptPage>,
    activity_query: GetTranscript,
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
            conversation_page: None,
            activity_timeline: None,
            activity_query: activity::latest(),
            conversation_query: GetTranscript {
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
        self.conversation_query = GetTranscript {
            before: 0,
            limit: 64,
            forward: top,
        };
        self.refresh = Instant::now();
    }
    fn cancel_history_jump(&mut self) {
        if std::mem::take(&mut self.jump_top)
            && let Some(page) = &self.conversation_page
        {
            self.conversation_query = GetTranscript {
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
        self.conversation_query = GetTranscript {
            before: 0,
            limit: 64,
            forward: false,
        };
        self.refresh = Instant::now();
    }
    fn received_conversation(&mut self, page: &TranscriptPage) {
        if self
            .conversation_page
            .as_ref()
            .is_none_or(|old| (old.begin, old.end) != (page.begin, page.end))
            || self.jump_top
        {
            self.state.reading = None;
        }
        if self.jump_top
            && !self.state.follow_tail
            && page.query == self.conversation_query
            && page.query.forward
        {
            self.state.scroll = usize::MAX;
        }
        self.jump_top = false;
        self.conversation_page = Some(page.clone());
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
                        .conversation_page
                        .as_ref()
                        .is_some_and(|page| page.total >= snapshot.conversation_size)
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
                && (!submit || code.is_some() || self.follow_accepted_submit.as_ref() != Some(&id))
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
        let conversation_stale = self.state.snapshot.as_ref().is_some_and(|snapshot| {
            self.conversation_page.as_ref().is_none_or(|page| {
                page.total < snapshot.conversation_size || page.query != self.conversation_query
            })
        });
        if (now >= self.refresh || transcript_stale || conversation_stale)
            && self.state.snapshot.is_some()
        {
            // Both page kinds share one bounded actor mailbox. Issue one read
            // at a time instead of deterministically starving one with LIFO
            // post tasks; user recall/copy collisions remain retryable reads.
            let transcript = if !self.state.history_ready {
                true
            } else if transcript_stale != conversation_stale {
                transcript_stale
            } else {
                self.refresh_transcript
            };
            let payload = if transcript {
                CommandPayload::GetTranscript(self.state.query.clone())
            } else {
                CommandPayload::GetConversation(self.conversation_query.clone())
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
    fn conversation(&self) -> Option<Vec<Entry>> {
        let page = self.conversation_page.as_ref()?;
        let view = self.state.snapshot.as_ref()?;
        let mut entries = page
            .entries
            .iter()
            .map(|row| Entry {
                role: row.role.clone(),
                text: row.text.clone(),
                action: None,
                edit: None,
                output: None,
                agent: None,
                status: None,
            })
            .collect::<Vec<_>>();
        if page.end == page.total {
            entries.extend(
                view.followups
                    .iter()
                    .filter(|message| message.state == "queued")
                    .map(|message| Entry {
                        action: None,
                        edit: None,
                        output: None,
                        agent: None,
                        status: None,
                        role: "queued_user".into(),
                        text: message.text.clone(),
                    }),
            );
            if let Some(preview) = &self.state.preview
                && !preview.text.is_empty()
                && preview.state != "committed"
                && self.state.preview_request == view.request
                && !view
                    .submit
                    .as_ref()
                    .is_some_and(|id| id.0.starts_with("draft-"))
            {
                entries.push(Entry {
                    action: None,
                    edit: None,
                    output: None,
                    agent: None,
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
        Some(entries)
    }

    fn activity_page(&self) -> Option<&TranscriptPage> {
        self.activity_timeline.as_ref()
    }

    fn projected(&self) -> Option<Vec<Entry>> {
        let page = self.activity_page();
        let rows = page.map_or(&[][..], |page| page.entries.as_slice());
        let at_tail = page.is_none_or(|page| page.end == page.total);
        let view = self.state.snapshot.as_ref()?;
        let now = Instant::now();
        let mut entries = super::super::tool_ui::project(
            rows,
            view,
            &self.tool_expanded,
            self.state.tools_expanded,
            at_tail,
            |operation| self.state.progress.tool_elapsed(operation, now),
        );
        for row in rows.iter().rev() {
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
                    agent: None,
                    status: None,
                    role: "tool_success".into(),
                    text: format!("> Open chart · {}", png.name()),
                },
            );
        }
        if at_tail
            && view.worker_busy
            && !matches!(view.state.as_str(), "completed" | "cancelled" | "paused")
        {
            entries.push(Entry {
                action: None,
                edit: None,
                output: None,
                agent: None,
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum OrchestrationMode {
    Standard,
    Plus,
}

struct PlanningContext {
    profile: Option<RoleProfile>,
    pending_draft: Option<RuntimeCommand>,
    resume: Option<CommandId>,
    awaiting_context: bool,
    submit: CommandId,
    turn: TurnId,
    command: CommandId,
    owner: String,
    token: u64,
}
struct DraftCancellation {
    submit: CommandId,
    request: Option<RequestId>,
    posting: bool,
}

enum FileValue {
    Listing(FileListing),
    Document(FileDocument),
    Image(graphics::Image),
}
enum Job {
    AgentTranscript {
        token: u64,
        result: Result<crate::frontend::sessions::AgentTranscript>,
    },
    OrchestrationPlan {
        owner: String,
        token: u64,
        result: Result<Proposal>,
    },
    OrchestrationDone {
        owner: String,
        generation_checked: bool,
        result: Result<orchestration::Completed>,
    },
    DraftCancel {
        owner: String,
        submit: CommandId,
        result: Result<PostStatus>,
    },
    ClarificationSaved {
        owner: String,
        text: String,
        result: Result<()>,
    },
    OrchestrationReceipt {
        result: Result<()>,
    },
    ArchivedPlan {
        token: u64,
        result: Result<orchestration::ArchivedPlan>,
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
    PaletteGenerated {
        token: u64,
        result: Result<Theme>,
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
        plans: orchestration::PlanLinks,
    },
    SessionSearch {
        owner: String,
        token: u64,
        result: Result<Matches>,
    },
    SessionSearchSource {
        owner: String,
        token: u64,
        hit: Hit,
        result: Result<Vec<(String, String)>>,
    },
    CachedModels {
        result: Result<Option<Json>>,
    },
    AutocompactSaved {
        owner: String,
        result: Result<()>,
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
    RoleModels {
        owner: String,
        token: u64,
        result: Result<Box<roles::RolePicker>>,
    },
    RoleModelsSaved {
        owner: String,
        token: u64,
        result: Result<()>,
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
    Roles(Box<roles::RolePicker>),
    Orchestration {
        proposal: Box<Proposal>,
        selected: usize,
    },
    Themes(ThemePreview),
    Palette {
        original: Theme,
        stage: PaletteStage,
    },
    DraftingPlan,
    Loading(String),
    Sessions {
        entries: Vec<SessionSummary>,
        selected: usize,
        resume_target: Option<RequestId>,
        continuation: Option<(RunId, String)>,
        clarification: Option<RunId>,
    },
    SessionSearch {
        matches: Matches,
        selected: usize,
    },
    SessionSearchSource {
        hit: Hit,
        lines: Vec<Choice>,
        selected: usize,
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
enum PaletteStage {
    Source,
    Scheme {
        source: PaletteSource,
        selected: usize,
    },
    Contrast {
        source: PaletteSource,
        scheme: usize,
    },
    Generating,
    Preview(Theme),
}
impl PaletteStage {
    fn editing(&self) -> bool {
        matches!(self, Self::Source | Self::Contrast { .. })
    }
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
struct AgentReader {
    id: String,
    from_list: bool,
    parent: String,
    title: String,
    pending: bool,
    requested: Option<u8>,
    offset: usize,
    page: Option<crate::frontend::sessions::AgentTranscript>,
    notice: String,
}

enum Panel {
    AgentTranscript(AgentReader),
    Orchestration {
        document: crate::frontend::markdown::Document,
        offset: usize,
    },
    Files(Box<Files>),
    Agents {
        selected: Option<String>,
    },
    Terminal,
    Activity(activity::Navigation),
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
    planning_context: Option<PlanningContext>,
    draft_cancellations: BTreeMap<String, DraftCancellation>,
    orchestration_stop: Option<(String, Cancel)>,
    orchestration_inbox: Option<tokio::sync::mpsc::Sender<orchestration::Clarification>>,
    clarification_pending: bool,
    orchestration_progress: Option<tokio::sync::watch::Receiver<Option<orchestration::RunView>>>,
    orchestration_views: BTreeMap<String, orchestration::RunView>,
    orchestration_handoffs: BTreeMap<String, String>,
    orchestration_summary: BTreeMap<String, CommandId>,
    orchestration_mode: Option<OrchestrationMode>,
    refinement: Option<(String, bool, Option<String>)>,
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
    activity_back: Option<activity::Navigation>,
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
    editor: Option<PanelEditor>,
    pending_editor: Option<EditorCommand>,
    editor_preparing: bool,
    editor_request: u64,
    modal_token: u64,
    modal_stop: Option<Cancel>,
    exit_deadline: Option<Instant>,
    input_closed: bool,
    terminating: bool,
}
struct PanelEditor {
    pane: super::super::terminal_pane::TerminalPane,
    size: (u16, u16),
    revision: u64,
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
        let stop = Cancel::default();
        let factory = factory.with_stop(stop.clone());
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
            stop,
            pending_context: BTreeMap::new(),
            planning_context: None,
            draft_cancellations: BTreeMap::new(),
            orchestration_stop: None,
            orchestration_inbox: None,
            clarification_pending: false,
            orchestration_progress: None,
            orchestration_views: BTreeMap::new(),
            orchestration_handoffs: BTreeMap::new(),
            orchestration_summary: BTreeMap::new(),
            orchestration_mode: None,
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
            activity_back: None,
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
        if matches!(self.menu, Some(Menu::DraftingPlan | Menu::Roles(_)))
            || matches!(&self.menu, Some(Menu::Loading(title)) if title == "Orchestrate+ role models")
        {
            self.disarm_exit();
            self.escape(io);
            return false;
        }
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
        self.orchestration_mode = None;
        self.refinement = None;
        self.slots[index].refresh = Instant::now();
        self.slots[index].resync = true;
        self.menu = None;
        self.completion = None;
        self.completion_closed = None;
        self.requested_model = None;
        self.terminal_decoder.take_pending();
        self.terminal_escape_at = None;
        self.activity_back = None;
        self.output_back = None;
        self.panel = None;
        self.focus = Focus::Chat;
        self.selection = None;
        self.result_selected = None;
        self.result_scroll = None;
        self.result_tail = false;
        self.review_redraw = false;
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
        if kind == Kind::Main {
            let slot = &self.slots[self.current];
            if slot.kind == Kind::Main
                && !slot.closed
                && !slot.closing
                && slot.title.is_none()
                && !slot.active()
                && slot
                    .state
                    .snapshot
                    .as_ref()
                    .is_some_and(|view| view.state == "idle" && view.timeline_size == 0)
            {
                self.switch(self.current);
                return true;
            }
        }
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
            let autocompact_percent = slot
                .state
                .snapshot
                .as_ref()
                .filter(|_| slot.kind != Kind::Console)
                .map(|view| view.autocompact_percent);
            let factory = self.factory.clone();
            self.jobs.spawn_blocking(move || Job::Created {
                result: factory.create_main(generation, autocompact_percent),
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

    fn open_agents(&mut self, io: &Io) {
        self.panel = Some(Panel::Agents { selected: None });
        self.focus = Focus::Inspector;
        if self.can_job(io) {
            let factory = self.factory.clone();
            let stop = Cancel::linked(&self.stop);
            self.jobs.spawn_blocking(move || Job::CachedSummaries {
                result: factory.summaries(&stop),
            });
        }
    }

    fn open_agent(&mut self, id: &str) {
        let rows = self.agent_rows();
        let Some((_, line)) = rows.iter().find(|(key, _)| key == id) else {
            return;
        };
        let parent = self
            .slots
            .iter()
            .find(|slot| slot.id == id)
            .and_then(|slot| slot.parent.clone())
            .or_else(|| {
                self.summaries
                    .iter()
                    .find(|s| s.entry.id == id)
                    .and_then(|s| s.entry.parent.clone())
            })
            .unwrap_or_else(|| self.conversation_root());
        self.panel = Some(Panel::AgentTranscript(AgentReader {
            id: id.into(),
            from_list: matches!(self.panel, Some(Panel::Agents { .. })),
            parent,
            title: line.title.clone(),
            pending: false,
            requested: Some(b'r'),
            offset: 0,
            page: None,
            notice: "Opening transcript…".into(),
        }));
        self.focus = Focus::Inspector;
        self.refresh_agent_reader();
    }

    fn refresh_agent_reader(&mut self) {
        if self.input_closed
            || self.terminating
            || self.jobs.len() + self.pending_context.len() >= MAX_JOBS
        {
            return;
        }
        let Some(Panel::AgentTranscript(reader)) = &mut self.panel else {
            return;
        };
        if reader.pending {
            return;
        }
        let Some(key) = reader.requested.take() else {
            return;
        };
        let (before, forward) = match (key, &reader.page) {
            (b'[', Some(page)) if page.begin > 0 => (page.begin, false),
            (b']', Some(page)) if page.end < page.total => (page.end, true),
            (b'r', _) => (0, false),
            _ => return,
        };
        self.file_token = self.file_token.wrapping_add(1);
        let token = self.file_token;
        reader.pending = true;
        let cached = reader
            .page
            .as_ref()
            .filter(|_| key != b'r')
            .map(|page| page.archive.clone());
        let id = reader.id.clone();
        let parent = reader.parent.clone();
        let factory = self.factory.clone();
        let stop = Cancel::linked(&self.stop);
        self.jobs.spawn_blocking(move || Job::AgentTranscript {
            token,
            result: {
                let query = GetTranscript {
                    before,
                    forward,
                    limit: 64,
                };
                match cached {
                    Some(archive) => Ok(archive.page(query)),
                    None => factory.agent_transcript(&id, &parent, query, &stop),
                }
            },
        });
    }

    fn inspect_terminal(&mut self, name: &str) {
        self.completion = None;
        self.selection = None;
        self.tool_press = None;
        self.result_selected = None;
        self.wide_diff = false;
        self.file_token = self.file_token.wrapping_add(1);
        self.panel = Some(Panel::NativeTerminal {
            token: self.file_token,
            name: name.to_owned(),
            view: self
                .factory
                .terminal_owner(&self.slots[self.current].id)
                .is_none()
                .then(|| carbon::OutputView {
                    title: "Agent terminal".into(),
                    text: "The agent has no terminal in this conversation".into(),
                }),
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
        if files.tab != 0 || self.terminal_dimensions().is_none() {
            return;
        }
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
            .unwrap_or_else(|| option_env!("ANSA_PANEL_EDITOR").unwrap_or("nvim").into());
        let host = self.files.clone();
        self.editor_request = self.editor_request.wrapping_add(1);
        let request = self.editor_request;
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
    }
    fn launch_panel_editor(&mut self, io: &Io) {
        let command = self.pending_editor.take().expect("validated editor");
        let Some((rows, columns)) = self.terminal_dimensions() else {
            self.report(io, "Widen the window to open the editor");
            return;
        };
        match super::super::terminal_pane::TerminalPane::spawn_editor(
            &command.argv,
            &self.factory.settings.cwd,
            rows,
            columns,
        ) {
            Ok(pane) => {
                self.editor = Some(PanelEditor {
                    pane,
                    size: (rows, columns),
                    revision: 0,
                });
                self.terminal_pending_input.clear();
                self.terminal_decoder = Default::default();
                self.terminal_escape_at = None;
                self.navigation_editor.clear();
                self.focus = Focus::Inspector;
                self.selection = None;
                self.tool_press = None;
                self.completion = None;
            }
            Err(error) => self.report(io, &format!("Could not open editor: {error}")),
        }
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
                        .conversation_page
                        .as_ref()
                        .is_some_and(|page| page.total >= snapshot.conversation_size)
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
        match &self.menu {
            Some(Menu::Themes(preview)) => self.set_theme(preview.original()),
            Some(Menu::Palette { original, .. }) => {
                self.set_theme(original.clone());
                self.navigation_editor.clear();
            }
            _ => {}
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
            let result = store.save_theme(&theme);
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
    fn enter_palette(&mut self, original: Theme, stage: PaletteStage, io: &Io) {
        let stage = match stage {
            PaletteStage::Source => {
                let text = self.navigation_editor.text().unwrap_or_default();
                match PaletteSource::parse(
                    text,
                    &self.factory.settings.cwd,
                    &self.factory.settings.home,
                ) {
                    Ok(source) => {
                        self.navigation_editor.clear();
                        PaletteStage::Scheme {
                            source,
                            selected: 0,
                        }
                    }
                    Err(_) => {
                        self.report(io, "Enter an image path or a color such as #c67ca9");
                        PaletteStage::Source
                    }
                }
            }
            PaletteStage::Scheme { source, selected } => PaletteStage::Contrast {
                source,
                scheme: selected,
            },
            PaletteStage::Contrast { source, scheme } => {
                let value = self.navigation_editor.text().unwrap_or_default().trim();
                let contrast = if value.is_empty() {
                    Ok(0)
                } else {
                    value.parse::<i8>()
                };
                if let Ok(contrast) = contrast
                    && (-100..=100).contains(&contrast)
                    && self.can_job(io)
                {
                    self.navigation_editor.clear();
                    let token = self.modal_token;
                    let stop = Cancel::linked(&self.stop);
                    self.modal_stop = Some(stop.clone());
                    self.jobs.spawn_blocking(move || Job::PaletteGenerated {
                        token,
                        result: theme::generate(source, scheme, contrast, &stop),
                    });
                    PaletteStage::Generating
                } else {
                    self.report(io, "Enter a contrast from -100 to 100");
                    PaletteStage::Contrast { source, scheme }
                }
            }
            PaletteStage::Preview(theme) => {
                self.commit_theme(theme);
                return;
            }
            PaletteStage::Generating => PaletteStage::Generating,
        };
        self.menu = Some(Menu::Palette { original, stage });
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
            plans: orchestration::recent_plan_links(&factory, &owner).unwrap_or_default(),
            owner,
            token,
            result: factory.summaries(&Cancel::default()),
        });
    }
    fn search_sessions(&mut self, query: &str, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::linked(&self.stop);
        let query = query.to_owned();
        self.modal_stop = Some(stop.clone());
        self.menu = Some(Menu::Loading("Search project sessions".into()));
        self.jobs.spawn_blocking(move || Job::SessionSearch {
            result: factory.search(&query, &stop),
            owner,
            token,
        });
    }
    fn open_session_search_source(&mut self, hit: Hit, io: &Io) {
        if !self.can_job(io) {
            return;
        }
        let factory = self.factory.clone();
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::linked(&self.stop);
        self.modal_stop = Some(stop.clone());
        self.menu = Some(Menu::Loading("Saved conversation".into()));
        self.jobs.spawn_blocking(move || Job::SessionSearchSource {
            result: factory.search_source(&hit, &stop),
            owner,
            token,
            hit,
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
        self.refinement = None;
        (self.orchestration_mode, slot.state.mode) =
            match (self.orchestration_mode, slot.state.mode) {
                (Some(OrchestrationMode::Standard), _) => {
                    (Some(OrchestrationMode::Plus), AgentMode::Basic)
                }
                (Some(OrchestrationMode::Plus), _) => (None, AgentMode::Basic),
                (None, AgentMode::Plan) if self.factory.orchestration_route().is_ok() => {
                    (Some(OrchestrationMode::Standard), AgentMode::Basic)
                }
                (None, AgentMode::Plan) => (None, AgentMode::Basic),
                (None, AgentMode::Basic) => (None, AgentMode::Plan),
            };
        slot.state.mode_ready = true;
        self.factory
            .set_editor_mode(if self.orchestration_mode.is_some() {
                AgentMode::Plan
            } else {
                slot.state.mode
            });
        let mode = match self.orchestration_mode {
            Some(OrchestrationMode::Standard) => "Orchestrate",
            Some(OrchestrationMode::Plus) => "Orchestrate+",
            None if slot.state.mode == AgentMode::Plan => "Planning",
            None => "Work",
        };
        self.report(io, mode);
    }

    fn start_orchestration_plan(
        &mut self,
        text: &str,
        skill: Option<String>,
        repair: bool,
        io: &Io,
    ) -> bool {
        if !matches!(self.slots[self.current].kind, Kind::Main | Kind::Fork) {
            self.report(io, "Open the main conversation to delegate work");
            return false;
        }
        if self.factory.orchestration_route().is_err() {
            self.report(
                io,
                "Orchestrate requires a route with verified structured outputs",
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
        let profile = if self.orchestration_mode == Some(OrchestrationMode::Plus) {
            match self.factory.role_profile() {
                Ok(Some(profile)) => Some(profile),
                Ok(None) => {
                    self.open_role_picker(io);
                    return false;
                }
                Err(code) => {
                    self.report(
                        io,
                        &format!("Could not read Orchestrate+ role models ({code})"),
                    );
                    return false;
                }
            }
        } else {
            None
        };
        let (previous_prompt, read_only, skill) = self.refinement.as_ref().map_or_else(
            || {
                (
                    None,
                    self.slots[self.current].state.mode == AgentMode::Plan,
                    skill,
                )
            },
            |(previous, read_only, skill)| {
                (
                    Some(previous.clone()),
                    *read_only || self.slots[self.current].state.mode == AgentMode::Plan,
                    skill.clone(),
                )
            },
        );
        let instructions = match skill.as_deref() {
            Some(name) => match self.factory.delegation_skill(name) {
                Some(instructions) => Some(instructions),
                None => {
                    self.report(io, "The selected skill is unavailable");
                    return false;
                }
            },
            None => None,
        };
        let mut draft =
            self.factory
                .plan_draft(read_only, instructions.as_deref(), profile.is_some());
        if repair {
            draft.instructions.push_str("\nRevise the preceding implementation plan. Keep requested_outcome as implementation. Include the requested code changes as scoped write steps and at least one executable test in command_checks. The corrected proposal will be shown for approval before any work starts.");
        }
        draft.previous_prompt = previous_prompt;
        draft.profile = match profile.as_ref().map(RoleProfile::encode).transpose() {
            Ok(profile) => profile.map(|value| value.dump()),
            Err(code) => {
                self.report(
                    io,
                    &format!("Could not capture Orchestrate+ role models ({code})"),
                );
                return false;
            }
        };
        draft.skill = skill;
        self.cancel_modal();
        let token = self.modal_token;
        let slot = &mut self.slots[self.current];
        let mut command = slot.state.command(
            &slot.handle,
            CommandPayload::DraftPlan(
                Submit {
                    text: text.to_owned(),
                    mode: if read_only {
                        AgentMode::Plan
                    } else {
                        AgentMode::Basic
                    },
                },
                Box::new(draft),
            ),
        );
        let prefix = if repair { "draft-revise" } else { "draft" };
        let id = CommandId(format!("{prefix}-{}", command.command.0));
        command.command = id.clone();
        let turn = TurnId(id.0.clone());
        command.turn = turn.clone();
        let selection = profile
            .as_ref()
            .filter(|profile| {
                slot.state
                    .snapshot
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.generation != profile.strong)
            })
            .map(|profile| profile.strong.clone());
        let (next, pending_draft) = if let Some(selection) = selection {
            let select = slot
                .state
                .command(&slot.handle, CommandPayload::SetGeneration(selection));
            (select, Some(command))
        } else {
            (command, None)
        };
        let command = next.command.clone();
        if !slot
            .state
            .post_command(&slot.handle, next, &mut slot.replies, false)
        {
            self.report(io, "Wait for the current requests");
            return false;
        }
        if slot.title.is_none() {
            slot.title = Some(display_title(text));
        }
        self.planning_context = Some(PlanningContext {
            profile,
            pending_draft,
            resume: None,
            awaiting_context: false,
            submit: id.clone(),
            turn,
            command,
            owner: slot.id.clone(),
            token,
        });
        slot.state.status.clear();
        self.menu = Some(Menu::DraftingPlan);
        true
    }

    fn cancel_drafting_plan(&mut self) {
        let Some(intent) = &self.planning_context else {
            return;
        };
        if intent.pending_draft.is_some() || intent.awaiting_context {
            return;
        }
        let slot = &mut self.slots[self.current];
        if slot.id != intent.owner {
            return;
        }
        let request = slot
            .state
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.submit.as_ref() == Some(&intent.submit))
            .and_then(|snapshot| snapshot.request.clone());
        self.draft_cancellations.insert(
            intent.owner.clone(),
            DraftCancellation {
                submit: intent.submit.clone(),
                request,
                posting: false,
            },
        );
        self.dispatch_draft_cancel(self.current);
    }

    fn dispatch_draft_cancel(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        let Some(draft) = self
            .draft_cancellations
            .get_mut(&slot.id)
            .filter(|draft| !draft.posting)
        else {
            return;
        };
        let Some(request) = draft.request.clone() else {
            return;
        };
        draft.posting = true;
        let owner = slot.id.clone();
        let submit = draft.submit.clone();
        let handle = slot.handle.clone();
        let mut cancel = slot.state.command(&handle, CommandPayload::Cancel(request));
        cancel.turn = TurnId(submit.0.clone());
        self.jobs.spawn(async move {
            Job::DraftCancel {
                owner,
                submit,
                result: handle.post(cancel).await,
            }
        });
    }

    fn approve_orchestration(&mut self, proposal: Proposal, io: &Io) {
        let slot = &self.slots[self.current];
        if slot.state.mode == AgentMode::Plan && !proposal.plan.is_read_only() {
            self.report(io, "Plan mode only allows read-only steps");
            return;
        }
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
        let (inbox, commands) = tokio::sync::mpsc::channel(8);
        self.orchestration_inbox = Some(inbox);
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
            generation_checked: false,
            result: orchestration::execute(&factory, proposal, &stop, progress, commands),
        });
    }

    fn resume_orchestration(&mut self, identity: RunId, io: &Io) {
        let slot = &self.slots[self.current];
        let read_only = slot.state.mode == AgentMode::Plan;
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
        let (inbox, commands) = tokio::sync::mpsc::channel(8);
        self.orchestration_inbox = Some(inbox);
        self.orchestration_progress = Some(receiver);
        self.orchestration_views.remove(&owner);
        self.slots[self.current].state.status.clear();
        self.orchestration_stop = Some((owner.clone(), stop.clone()));
        let factory = self.factory.clone();
        self.jobs.spawn_blocking(move || Job::OrchestrationDone {
            owner,
            generation_checked: false,
            result: orchestration::resume(&factory, identity, read_only, &stop, progress, commands),
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

    fn clarify_orchestration(&mut self, text: &str, io: &Io) {
        if self.clarification_pending || !self.can_job(io) {
            return;
        }
        let (acknowledgement, reply) = tokio::sync::oneshot::channel();
        let message = orchestration::Clarification {
            text: text.into(),
            acknowledgement,
        };
        if self
            .orchestration_inbox
            .as_ref()
            .is_none_or(|inbox| inbox.try_send(message).is_err())
        {
            self.report(
                io,
                "The coordinator is finishing · your draft is still here",
            );
            return;
        }
        self.clarification_pending = true;
        let owner = self.slots[self.current].id.clone();
        let text = text.to_owned();
        self.jobs.spawn(async move {
            Job::ClarificationSaved {
                owner,
                text,
                result: reply
                    .await
                    .unwrap_or(Err("orchestration_clarification_closed")),
            }
        });
    }

    fn text(&mut self, text: &str, io: &Io) -> (bool, bool) {
        if !text.trim_start().starts_with('/')
            && !text.trim().is_empty()
            && self.collecting_orchestration()
        {
            self.clarify_orchestration(text, io);
            return (false, false);
        }
        if !text.trim_start().starts_with('/')
            && self
                .orchestration_summary
                .contains_key(&self.slots[self.current].id)
        {
            self.report(
                io,
                "The coordinator is writing the summary · your draft is still here",
            );
            return (false, false);
        }
        let delegation = commands::skill_request(text).map(|(name, _)| name.to_owned());
        if !text.trim().is_empty()
            && !text.trim_start().starts_with('/')
            && (self.orchestration_mode.is_some()
                || self.refinement.is_some()
                || commands::agent_task(text).is_some()
                || delegation.is_some())
        {
            return (
                self.start_orchestration_plan(text, delegation, false, io),
                false,
            );
        }
        let (command, argument) = text
            .trim()
            .split_once(char::is_whitespace)
            .map_or((text.trim(), ""), |(c, a)| (c, a.trim()));
        match commands::canonical(command) {
            "/terminals" => {
                if argument.is_empty() {
                    self.open_activity();
                } else {
                    self.activity_back = Some(activity::Navigation::default());
                    self.inspect_terminal(argument);
                }
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
                    "Ask the main agent to delegate or switch to Orchestrate with Tab",
                );
                (true, false)
            }
            "/agents" => {
                self.open_agents(io);
                (true, false)
            }
            "/sessions" if argument == "search" => {
                self.report(io, "Enter a query with /sessions search QUERY");
                (true, false)
            }
            "/sessions" if argument.split_whitespace().next() == Some("search") => {
                self.search_sessions(argument["search".len()..].trim(), io);
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
            "/pr" => {
                self.report(io, "Ask the agent to open a pull request with gh");
                (true, false)
            }
            "/files" => {
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
            "/autocompact" if self.slots[self.current].kind == Kind::Console => {
                self.report(io, "The command panel has no conversation context");
                (false, false)
            }
            "/autocompact" if !argument.is_empty() => {
                match super::super::commands::autocompact_percent(argument) {
                    Ok(Some(percent)) => (self.apply_autocompact(percent, io), false),
                    Ok(None) => (false, false),
                    Err(message) => {
                        self.report(io, message);
                        (false, false)
                    }
                }
            }
            "/orchestrate+" if argument.is_empty() => (self.open_role_picker(io), false),
            "/model" => {
                self.requested_model = (!argument.is_empty()).then(|| argument.to_string());
                self.open_models(false, io);
                (true, false)
            }
            "/effort" if argument.is_empty() => {
                self.open_models(true, io);
                (true, false)
            }
            "/effort" => {
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
                    self.orchestration_mode = None;
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
            self.draft_cancellations.remove(id);
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
        if let Some(draft) = self.draft_cancellations.get_mut(id) {
            match &event.payload {
                EventPayload::CommandAccepted { command, request } if command == &draft.submit => {
                    draft.request.clone_from(request);
                }
                EventPayload::FrontendSnapshot(snapshot)
                    if snapshot.submit.as_ref() == Some(&draft.submit) =>
                {
                    draft.request.clone_from(&snapshot.request);
                }
                EventPayload::RuntimeError {
                    command: Some(command),
                    ..
                } if command == &draft.submit => {
                    self.draft_cancellations.remove(id);
                }
                _ => {}
            }
            self.dispatch_draft_cancel(index);
        }
        let lost_context = matches!(event.payload, EventPayload::ResyncRequired { .. })
            && self.discard_contexts(id);
        if let Some(intent) = &mut self.planning_context
            && intent.owner == id
        {
            if intent.resume.is_some()
                && match &event.payload {
                    EventPayload::CommandAccepted { command, .. } => {
                        intent.resume.as_ref() == Some(command)
                    }
                    EventPayload::FrontendSnapshot(snapshot) => {
                        snapshot.durable
                            && snapshot.submit.as_ref() == Some(&intent.submit)
                            && !matches!(snapshot.state.as_str(), "paused" | "cancelled")
                    }
                    _ => false,
                }
            {
                intent.resume = None;
            }
            let selected = intent.pending_draft.is_some()
                && intent
                    .profile
                    .as_ref()
                    .is_some_and(|profile| match &event.payload {
                        EventPayload::GenerationSelected { command, selection } => {
                            command == &intent.command && selection == &profile.strong
                        }
                        EventPayload::FrontendSnapshot(snapshot) => {
                            snapshot.durable && snapshot.generation == profile.strong
                        }
                        _ => false,
                    });
            if selected && let Some(command) = intent.pending_draft.take() {
                let slot = &mut self.slots[index];
                intent.command = intent.submit.clone();
                if !slot
                    .state
                    .post_command(&slot.handle, command, &mut slot.replies, false)
                {
                    self.cancel_modal();
                    self.menu = None;
                    self.report(io, "Could not submit the plan request");
                }
            }
        }
        if let Some(intent) = &mut self.planning_context
            && intent.owner == id
            && intent.pending_draft.is_none()
        {
            let complete = match &event.payload {
                EventPayload::Done => event.turn == intent.turn,
                EventPayload::FrontendSnapshot(snapshot) => {
                    snapshot.submit.as_ref() == Some(&intent.command)
                        && snapshot.durable
                        && snapshot.state == "completed"
                }
                _ => false,
            };
            if !intent.awaiting_context && complete {
                let slot = &mut self.slots[index];
                let command = slot.state.command(&slot.handle, CommandPayload::GetContext);
                intent.command = command.command.clone();
                intent.awaiting_context = true;
                if !slot
                    .state
                    .post_command(&slot.handle, command, &mut slot.replies, false)
                {
                    self.cancel_modal();
                    self.menu = None;
                    self.report(io, "Could not load the completed plan");
                }
            } else if matches!(event.payload, EventPayload::ResyncRequired { .. })
                && intent.awaiting_context
            {
                // The next durable snapshot lets us request the context again.
                intent.command = intent.submit.clone();
                intent.awaiting_context = false;
            }
        }
        if let EventPayload::ContextSnapshot { command, seed } = &event.payload
            && self.planning_context.as_ref().is_some_and(|intent| {
                intent.awaiting_context && &intent.command == command && intent.owner == id
            })
            && let Some(PlanningContext {
                owner, turn, token, ..
            }) = self.planning_context.take()
        {
            let factory = self.factory.clone();
            let seed = seed.clone();
            let stop = Cancel::linked(&self.stop);
            self.modal_stop = Some(stop.clone());
            self.jobs.spawn_blocking(move || Job::OrchestrationPlan {
                owner,
                token,
                result: (|| {
                    stop.check()?;
                    let draft = seed
                        .draft_plan()
                        .filter(|_| seed.source_turn() == &turn)
                        .cloned()
                        .ok_or("orchestration_plan_unavailable")?;
                    if let Some(profile) = &draft.profile {
                        let source = seed.request()?;
                        if source.binding.requested_model != profile.strong.model
                            || source.effort != profile.strong.effort
                        {
                            return Err("orchestration_generation_changed");
                        }
                    }
                    Proposal::from_plan(
                        &factory,
                        seed,
                        draft.prompt,
                        draft.plan,
                        draft.read_only,
                        draft.skill,
                        draft.profile,
                    )
                })(),
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
            if self.planning_context.as_ref().is_some_and(|intent| {
                intent.owner == id
                    && (intent.resume.as_ref() == Some(command) || &intent.command == command)
            }) {
                self.cancel_modal();
                self.menu = None;
            }
        }
        let checked_snapshot = match &event.payload {
            EventPayload::FrontendSnapshot(snapshot) => {
                if !self.slots[index].accepts_snapshot(snapshot) {
                    return;
                }
                Some(snapshot)
            }
            _ => None,
        };
        if checked_snapshot.is_some_and(|snapshot| {
            (snapshot.state == "cancelled"
                || (snapshot.state == "paused" && snapshot.durable && !snapshot.worker_busy))
                && self.planning_context.as_ref().is_some_and(|intent| {
                    intent.owner == id
                        && intent.resume.is_none()
                        && snapshot.submit.as_ref() == Some(&intent.submit)
                })
        }) {
            self.cancel_modal();
            self.menu = None;
        }
        let slot = &mut self.slots[index];
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
        match &event.payload {
            EventPayload::CommandAccepted { command, .. }
                if slot.follow_accepted_submit.as_ref() == Some(command) =>
            {
                slot.state.pending_draft = None;
                slot.follow_accepted_submit = None;
                slot.show_latest();
            }
            EventPayload::FrontendSnapshot(snapshot)
                if snapshot.durable
                    && !snapshot.storage_failed
                    && slot.follow_accepted_submit.is_some()
                    && slot.follow_accepted_submit == snapshot.submit =>
            {
                slot.state.pending_draft = None;
                slot.follow_accepted_submit = None;
                slot.show_latest();
            }
            EventPayload::RuntimeError {
                command: Some(command),
                ..
            } if slot.follow_accepted_submit.as_ref() == Some(command) => {
                slot.follow_accepted_submit = None;
                if let Some((_, bytes, cursor)) = slot.state.pending_draft.take() {
                    slot.state.editor.bytes = bytes;
                    slot.state.editor.cursor = cursor;
                }
            }
            EventPayload::ResyncRequired { .. } => {
                slot.resync = true;
                slot.refresh = Instant::now();
            }
            EventPayload::ConversationPage(page) => {
                if page.query == slot.conversation_query
                    && page.total
                        >= slot
                            .state
                            .snapshot
                            .as_ref()
                            .map_or(0, |snapshot| snapshot.conversation_size)
                {
                    slot.received_conversation(page);
                }
            }
            EventPayload::TimelinePage(page) if page.query == slot.activity_query => {
                slot.activity_timeline = Some(page.clone());
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
            Job::PaletteGenerated { token, .. } => {
                !self.input_closed
                    && !self.terminating
                    && *token == self.modal_token
                    && matches!(
                        &self.menu,
                        Some(Menu::Palette {
                            stage: PaletteStage::Generating,
                            ..
                        })
                    )
            }
            Job::Themes {
                request: ThemeLoad::Picker { owner, token, .. },
                ..
            } => {
                !self.input_closed
                    && !self.terminating
                    && *owner == self.slots[self.current].id
                    && *token == self.modal_token
            }
            Job::Sessions { owner, token, .. }
            | Job::SessionSearch { owner, token, .. }
            | Job::SessionSearchSource { owner, token, .. }
            | Job::Models { owner, token, .. }
            | Job::RoleModels { owner, token, .. }
            | Job::RoleModelsSaved { owner, token, .. } => {
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
                | Job::SessionSearch { .. }
                | Job::SessionSearchSource { .. }
                | Job::Themes {
                    request: ThemeLoad::Picker { .. },
                    ..
                }
        ) {
            self.menu = None;
            self.modal_stop = None;
        }
        match job {
            Job::AgentTranscript { token, result } => {
                if token == self.file_token
                    && let Some(Panel::AgentTranscript(reader)) = &mut self.panel
                {
                    reader.pending = false;
                    match result {
                        Ok(page) => {
                            reader.page = Some(page);
                            reader.offset = 0;
                            reader.notice.clear();
                        }
                        Err(code) => {
                            reader.notice = format!("Transcript unavailable ({code}) · r retry")
                        }
                    }
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
                            self.show_orchestration_plan(
                                &archive.plan,
                                Some(&archive.prompt),
                                None,
                                false,
                            );
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
                            if self.activity_back.is_some() {
                                self.return_to_activity();
                            } else {
                                self.panel = None;
                                self.focus = Focus::Chat;
                            }
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
                        if blocked == Some("orchestration_implementation_required")
                            && !proposal.read_only
                            && self.orchestration_mode == Some(OrchestrationMode::Plus)
                            && !proposal.seed.source_turn().0.starts_with("draft-revise-")
                        {
                            self.refinement = None;
                            if self.start_orchestration_plan(
                                &proposal.prompt,
                                proposal.skill.clone(),
                                true,
                                io,
                            ) {
                                return;
                            }
                        }
                        let slot = &mut self.slots[self.current];
                        slot.state.editor.clear();
                        slot.state.input_history.reset();
                        self.refinement = None;
                        self.show_orchestration_plan(
                            &proposal.plan,
                            None,
                            proposal.profile.as_ref(),
                            proposal.can_request_implementation(),
                        );
                        self.frames.clear();
                        self.displayed = None;
                        self.operational_frame = None;
                        self.last_frame = None;
                        self.menu = Some(Menu::Orchestration {
                            proposal: Box::new(proposal),
                            selected: usize::from(blocked.is_some()),
                        });
                    }
                    Err(code) => {
                        if code == "orchestration_profile_required" {
                            self.report(
                                io,
                                "Choose Orchestrate+ role models, then submit the draft again",
                            );
                            self.open_role_picker(io);
                        } else if code == "orchestration_read_folder_not_named" {
                            self.report(io, "Name the read folder in your request");
                        } else if code == "orchestration_read_folder_missing" {
                            self.report(
                                io,
                                "Read folder unavailable · check the path in your request",
                            );
                        } else {
                            self.report(io, &format!("Could not draft plan ({code})"));
                        }
                    }
                }
            }
            Job::DraftCancel {
                owner,
                submit,
                result,
            } => {
                if let Some(draft) = self.draft_cancellations.get_mut(&owner)
                    && draft.submit == submit
                {
                    if result == Ok(PostStatus::QueueFull) {
                        draft.posting = false;
                    } else {
                        self.draft_cancellations.remove(&owner);
                        if let Err(code) = result
                            && let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == owner)
                        {
                            slot.state
                                .report(io, &format!("Could not cancel plan ({code})"));
                        }
                    }
                }
            }
            Job::ClarificationSaved {
                owner,
                text,
                result,
            } => {
                self.clarification_pending = false;
                if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == owner) {
                    match result {
                        Ok(()) => {
                            if slot.state.editor.bytes == text.as_bytes() {
                                slot.state.editor.clear();
                                slot.state.input_history.reset();
                            }
                            slot.show_latest();
                            slot.state
                                .report(io, "Clarification saved for the coordinator");
                        }
                        Err(code) => slot
                            .state
                            .report(io, &format!("Could not save clarification ({code})")),
                    }
                }
            }
            Job::OrchestrationDone {
                owner,
                generation_checked,
                result,
            } => {
                self.update_orchestration_progress();
                self.orchestration_progress = None;
                self.orchestration_inbox = None;
                let result = match &self.orchestration_stop {
                    Some((_, stop)) => stop.check().and(result),
                    None => result,
                };
                if !generation_checked
                    && !self.input_closed
                    && let Ok(completed) = &result
                    && completed.evidence.is_some()
                    && let Some(generation) = &completed.generation
                    && let Some(parent) = self.slots.iter_mut().find(|slot| slot.id == owner)
                    && !parent.active()
                    && parent
                        .state
                        .snapshot
                        .as_ref()
                        .is_none_or(|view| &view.generation != generation)
                    && let Some((_, stop)) = &self.orchestration_stop
                {
                    let stop = stop.clone();
                    let handle = parent.handle.clone();
                    let command = parent
                        .state
                        .command(&handle, CommandPayload::SetGeneration(generation.clone()));
                    self.jobs.spawn(async move {
                        let selection = handle.post(command).await;
                        let result = stop.check().and(match selection {
                            Ok(PostStatus::Accepted | PostStatus::Duplicate) => result,
                            Err(code) => Err(code),
                            _ => Err("orchestration_generation_unavailable"),
                        });
                        Job::OrchestrationDone {
                            owner,
                            generation_checked: true,
                            result,
                        }
                    });
                    return;
                }
                self.orchestration_stop = None;
                if result.is_err()
                    && matches!(self.panel, Some(Panel::Agents { .. }))
                    && self.can_job(io)
                {
                    let factory = self.factory.clone();
                    let stop = Cancel::linked(&self.stop);
                    self.jobs.spawn_blocking(move || Job::CachedSummaries {
                        result: factory.summaries(&stop),
                    });
                }
                let Some(parent) = self.slots.iter_mut().find(|slot| slot.id == owner) else {
                    return;
                };
                match result {
                    Ok(completed) if !self.input_closed && !parent.active() => {
                        if let Some(candidate) = completed.candidate {
                            let review = ux::CandidateView::new(completed.id.clone(), candidate);
                            parent.remember_candidate(review);
                        }
                        let Some(evidence) = completed.evidence else {
                            parent.state.report(io, "The model context is full · reports are in /agents");
                            return;
                        };
                        let mut command = parent.state.command(
                            &parent.handle,
                            CommandPayload::Handoff(Submit {
                                text: evidence,
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
                            self.orchestration_handoffs.insert(owner.clone(), completed.id);
                            if parent.state.follow_tail {
                                parent.show_latest();
                            }
                        } else {
                            parent.state.report(
                                io,
                                if completed.stopped {
                                    "Plan stopped · see the agents' transcripts"
                                } else {
                                    "Results are in the agents' transcripts"
                                },
                            );
                        }
                    }
                    Ok(completed) if completed.stopped => parent
                        .state
                        .report(io, "Plan stopped · see the agents' transcripts"),
                    Ok(_) => parent
                        .state
                        .report(io, "Results are in the agents' transcripts"),
                    Err("auth_cancelled" | "run_cancelled") => {
                        parent.state.report(io, "Plan stopped")
                    }
                    Err("orchestration_paused") => parent
                        .state
                        .report(io, "Agent paused · resume with /sessions"),
                    Err("orchestration_scope_confirmation") => parent.state.report(io,
                        "Plan stopped at the approved scope · submit the revised request to review a new plan"),
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
            Job::PaletteGenerated { result, .. } => {
                self.modal_stop = None;
                if let Some(Menu::Palette { original, .. }) = self.menu.take() {
                    let stage = match result {
                        Ok(theme) => {
                            self.set_theme(theme.clone());
                            PaletteStage::Preview(theme)
                        }
                        Err(code) => {
                            self.report(io, &format!("Could not generate a palette ({code})"));
                            PaletteStage::Source
                        }
                    };
                    self.menu = Some(Menu::Palette { original, stage });
                }
            }
            Job::AutocompactSaved { owner, result } => {
                if let Err(code) = result
                    && let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == owner)
                {
                    if code.starts_with("autocompact_preferences_") {
                        slot.state
                            .report(io, "Could not save default autocompact for new sessions");
                    } else {
                        slot.state.exit = 1;
                        slot.state
                            .report(io, &format!("ansa: {}", diagnostics::error(code)));
                    }
                }
            }
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
            Job::Sessions { result, plans, .. } => match result {
                Ok(entries) => {
                    self.summaries = entries.clone();
                    self.menu = Some(Menu::Sessions {
                        entries: entries
                            .into_iter()
                            .filter(|s| {
                                matches!(s.entry.kind.as_str(), "main" | "fork")
                                    && s.status != crate::session::catalog::SummaryStatus::Empty
                            })
                            .collect(),
                        selected: 0,
                        resume_target: self.resume_target(),
                        continuation: plans.continuation,
                        clarification: plans.clarification,
                    })
                }
                Err(code) => self.report(io, &format!("Could not load sessions: {code}")),
            },
            Job::SessionSearch { result, .. } => match result {
                Ok(matches) => {
                    self.menu = Some(Menu::SessionSearch {
                        matches,
                        selected: 0,
                    })
                }
                Err(code) => self.report(io, &format!("Could not search sessions: {code}")),
            },
            Job::SessionSearchSource { hit, result, .. } => match result {
                Ok(records) => {
                    let mut lines = Vec::new();
                    for (anchor, text) in records {
                        lines.push(Choice {
                            label: anchor,
                            detail: String::new(),
                        });
                        let chars: Vec<_> = text.chars().collect();
                        lines.extend(chars.chunks(52).map(|chunk| Choice {
                            label: chunk.iter().collect(),
                            detail: String::new(),
                        }));
                    }
                    self.menu = Some(Menu::SessionSearchSource {
                        hit,
                        lines,
                        selected: 0,
                    });
                }
                Err(code) => self.report(io, &format!("Could not open saved conversation: {code}")),
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
            Job::RoleModels { result, .. } => {
                self.modal_stop = None;
                match result {
                    Ok(picker) => self.menu = Some(Menu::Roles(picker)),
                    Err(code) => {
                        self.menu = None;
                        self.report(
                            io,
                            &format!("Could not load Orchestrate+ role models: {code}"),
                        );
                    }
                }
            }
            Job::RoleModelsSaved { result, .. } => match result {
                Ok(()) => {
                    self.menu = None;
                    self.report(io, "Orchestrate+ role models saved");
                }
                Err(code) => {
                    if let Some(Menu::Roles(picker)) = &mut self.menu {
                        picker.save_failed();
                    }
                    self.report(
                        io,
                        &format!("Could not save Orchestrate+ role models: {code}"),
                    );
                }
            },
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
        if matches!(&self.menu, Some(Menu::Roles(picker)) if picker.saving()) {
            return;
        }
        self.tool_press = None;
        self.slots[self.current].cancel_history_jump();
        if matches!(self.menu, Some(Menu::DraftingPlan)) {
            self.cancel_drafting_plan();
        }
        self.cancel_modal();
        self.requested_model = None;
        self.cancel_editor_request();
        if self.completion.take().is_some() {
            let editor = &self.slots[self.current].state.editor;
            self.completion_closed = Some((editor.bytes.clone(), editor.cursor));
            return;
        }
        if let Some(Menu::Roles(picker)) = &mut self.menu
            && picker.back()
        {
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
            if !matches!(self.panel, Some(Panel::Activity(_))) {
                self.slots[self.current].show_latest();
            }
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
        if let Some(Panel::AgentTranscript(reader)) = &self.panel
            && (reader.from_list || self.activity_back.is_none())
        {
            self.panel = Some(Panel::Agents {
                selected: Some(reader.id.clone()),
            });
            self.focus = Focus::Inspector;
            return;
        }
        if self.panel.is_some() && self.activity_back.is_some() {
            self.return_to_activity();
            return;
        }
        if self.panel.take().is_some() {
            self.focus = Focus::Chat;
            self.wide_diff = false;
            self.restore_result_scroll();
            return;
        }
        if let Some(parent) = self.side_chat_parent() {
            self.switch(parent);
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
        if self.focus == Focus::Inspector && matches!(self.panel, Some(Panel::Activity(_))) {
            self.scroll_activity(
                older,
                self.displayed.as_ref().map_or(8, |frame| {
                    frame.inspector_content_rows.saturating_sub(1).max(1)
                }),
            );
            return;
        }
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
            let reading = match &mut self.panel {
                Some(Panel::Orchestration { document, offset }) => Some((&*document, offset)),
                Some(Panel::AgentTranscript(reader)) => reader
                    .page
                    .as_ref()
                    .map(|page| (&page.document, &mut reader.offset)),
                _ => None,
            };
            if let Some((document, offset)) = reading {
                let viewport = self
                    .displayed
                    .as_ref()
                    .and_then(|frame| frame.inspector_bounds());
                let (_, _, width, height) = viewport.unwrap_or((0, 0, 80, 6));
                let size = document.viewport_rows(width.max(1)).len();
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
            if slot.conversation_query.before == 0
                && let Some(page) = &slot.conversation_page
            {
                slot.conversation_query = GetTranscript {
                    before: page.end,
                    limit: 64,
                    forward: false,
                };
            }
            if state.scroll >= state.scroll_limit
                && can_page
                && let Some(page) = &slot.conversation_page
                && page.begin > 0
            {
                slot.conversation_query = GetTranscript {
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
            && let Some(page) = &slot.conversation_page
            && page.end < page.total
            && tail.is_none_or(|limit| page.end < limit)
        {
            slot.conversation_query = GetTranscript {
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
                    .conversation_page
                    .as_ref()
                    .is_some_and(|page| page.end == page.total)
            {
                state.follow_tail = true;
                state.reading = None;
                slot.conversation_query = GetTranscript {
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
            .conversation_page
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
        if self.menu.is_none()
            && self.focus == Focus::Inspector
            && matches!(self.panel, Some(Panel::Activity(_)))
        {
            self.select_activity(older);
            return;
        }
        let agent_rows = if matches!(self.panel, Some(Panel::Agents { .. })) {
            self.agent_rows()
        } else {
            Vec::new()
        };
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
            Some(Menu::Roles(picker)) => picker.navigate(delta),
            Some(Menu::Orchestration { selected, proposal }) => delta(
                selected,
                if proposal.can_request_implementation() {
                    4
                } else {
                    3
                },
            ),
            Some(Menu::Themes(preview)) => {
                preview.navigate(older);
                let theme = preview.current();
                self.set_theme(theme);
            }
            Some(Menu::Palette {
                stage: PaletteStage::Scheme { selected, .. },
                ..
            }) => {
                delta(selected, theme::MATUGEN_SCHEMES.len());
            }
            Some(Menu::Palette { .. }) => {}
            Some(Menu::Loading(_) | Menu::DraftingPlan) => {}
            Some(Menu::Sessions {
                entries,
                selected,
                resume_target,
                continuation,
                clarification,
            }) => delta(
                selected,
                entries.len()
                    + usize::from(resume_target.is_some())
                    + usize::from(continuation.is_some())
                    + usize::from(clarification.is_some()),
            ),
            Some(Menu::SessionSearch { matches, selected }) => delta(selected, matches.hits.len()),
            Some(Menu::SessionSearchSource {
                lines, selected, ..
            }) => delta(selected, lines.len()),
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
                Some(Panel::Agents { selected }) => {
                    let mut index = agent_rows
                        .iter()
                        .position(|(id, _)| Some(id) == selected.as_ref())
                        .unwrap_or(0);
                    delta(&mut index, agent_rows.len());
                    *selected = agent_rows.get(index).map(|(id, _)| id.clone());
                }
                Some(Panel::Output { offset, .. }) => delta(offset, output_count),
                Some(
                    Panel::Activity(_)
                    | Panel::Terminal
                    | Panel::Image { .. }
                    | Panel::NativeTerminal { .. },
                ) => {}
                Some(Panel::AgentTranscript(reader)) => {
                    if let Some(page) = &reader.page {
                        let width = self
                            .displayed
                            .as_ref()
                            .and_then(|frame| frame.inspector_bounds())
                            .map_or(72, |bounds| bounds.2);
                        delta(&mut reader.offset, page.document.viewport_rows(width).len());
                    }
                }
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
                Menu::Roles(picker) => self.enter_role_picker(picker, io),
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
                            self.refinement =
                                Some((proposal.prompt, proposal.read_only, proposal.skill));
                            self.report(io, "Enter a clarification for the plan");
                        }
                        2 if proposal.can_request_implementation() => {
                            self.refinement = None;
                            self.start_orchestration_plan(
                                &proposal.prompt,
                                proposal.skill,
                                true,
                                io,
                            );
                        }
                        _ => self.report(io, "Plan cancelled"),
                    }
                }
                Menu::Themes(preview) if preview.generating() => {
                    self.navigation_editor.clear();
                    self.menu = Some(Menu::Palette {
                        original: preview.original(),
                        stage: PaletteStage::Source,
                    });
                }
                Menu::Themes(preview) => self.commit_theme(preview.current()),
                Menu::Palette { original, stage } => self.enter_palette(original, stage, io),
                Menu::Loading(title) => self.menu = Some(Menu::Loading(title)),
                Menu::DraftingPlan => self.menu = Some(Menu::DraftingPlan),
                Menu::Sessions {
                    entries,
                    selected,
                    resume_target,
                    continuation,
                    clarification,
                } => {
                    let native = usize::from(resume_target.is_some());
                    let orchestration = usize::from(continuation.is_some());
                    let archived = usize::from(clarification.is_some());
                    if let Some(target) = resume_target.as_ref().filter(|_| selected == 0) {
                        if self.resume_target().as_ref() == Some(target) {
                            let slot = &mut self.slots[self.current];
                            let draft = slot
                                .state
                                .snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.submit.as_ref())
                                .filter(|submit| submit.0.starts_with("draft-"))
                                .cloned();
                            let resume = slot
                                .state
                                .command(&slot.handle, CommandPayload::Resume(target.clone()));
                            let resume_id = resume.command.clone();
                            if slot.state.post_command(
                                &slot.handle,
                                resume,
                                &mut slot.replies,
                                false,
                            ) && let Some(submit) = draft
                            {
                                self.planning_context = Some(PlanningContext {
                                    profile: None,
                                    pending_draft: None,
                                    resume: Some(resume_id),
                                    awaiting_context: false,
                                    command: submit.clone(),
                                    turn: TurnId(submit.0.clone()),
                                    submit,
                                    owner: slot.id.clone(),
                                    token: self.modal_token,
                                });
                                self.menu = Some(Menu::DraftingPlan);
                            }
                        }
                    } else if let Some((id, _)) = continuation.filter(|_| selected == native) {
                        self.resume_orchestration(id, io);
                    } else if let Some(id) =
                        clarification.filter(|_| selected == native + orchestration)
                    {
                        self.activate_result(format!("plan:{}", id.run), None, None, io);
                    } else if let Some(entry) =
                        entries.get(selected - native - orchestration - archived)
                    {
                        self.resume(entry.entry.id.clone(), io);
                    }
                }
                Menu::SessionSearch { matches, selected } => {
                    if let Some(hit) = matches.hits.into_iter().nth(selected) {
                        self.open_session_search_source(hit, io);
                    }
                }
                source @ Menu::SessionSearchSource { .. } => self.menu = Some(source),
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
                Some(Panel::Activity(_)) => self.enter_activity(io),
                Some(Panel::Agents { selected }) => {
                    if let Some(id) = selected.clone() {
                        self.open_agent(&id);
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
        if matches!(&self.menu, Some(Menu::Themes(_) | Menu::Palette { .. }))
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
        if self.side_chat_parent().is_some() {
            self.update_displayed(io);
            if matches!(mouse.kind, MouseKind::WheelUp | MouseKind::WheelDown)
                && self.displayed.as_ref().is_some_and(|frame| {
                    frame
                        .inspector_bounds()
                        .is_some_and(|(column, row, width, height)| {
                            (column..column + width).contains(&mouse.column)
                                && (row..row + height).contains(&mouse.row)
                        })
                })
            {
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
                if io.writer.failed() {
                    return;
                }
                if matches!(self.panel, Some(Panel::Activity(_))) {
                    self.tool_press = self.activity_press(mouse.column, mouse.row);
                }
                // Text selection needs one complete paint. Activity actions
                // can use the same hit target across an in-flight redraw.
                if !self.frames.is_empty() {
                    return;
                }
                if matches!(self.panel, Some(Panel::Agents { .. }))
                    && let Some(id) = self
                        .displayed
                        .as_ref()
                        .and_then(|frame| frame.agent_at(mouse.column, mouse.row))
                        .map(str::to_owned)
                {
                    self.open_agent(&id);
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
                    if tab == 1 && matches!(self.panel, Some(Panel::Activity(_))) {
                        self.activity_shell();
                        return;
                    }
                    if tab == 0 && matches!(self.panel, Some(Panel::NativeTerminal { .. })) {
                        self.return_to_activity();
                        return;
                    }
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
                    let in_activity = matches!(self.panel, Some(Panel::Activity(_)))
                        && frame
                            .inspector_bounds()
                            .is_some_and(|(left, top, width, height)| {
                                (left..left + width).contains(&mouse.column)
                                    && (top..top + height).contains(&mouse.row)
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
                    } else if in_activity || matches!(self.panel, Some(Panel::Terminal)) {
                        Some(Selection::new(frame.clone(), mouse.column, mouse.row))
                    } else {
                        None
                    };
                    if in_chat && self.selection.is_some() {
                        let reading = slot.conversation();
                        slot.state.follow_tail = false;
                        slot.state.reading_total = slot
                            .state
                            .snapshot
                            .as_ref()
                            .map_or(0, |s| s.transcript_size);
                        if let Some((begin, end)) = frame.timeline_range {
                            slot.conversation_query = GetTranscript {
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
                    if matches!(self.panel, Some(Panel::Activity(_))) {
                        self.activate_activity(key, edit, output, io);
                    } else {
                        self.activate_result(key, edit, output, io);
                    }
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
            Menu::Roles(picker) => picker.popup(),
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
                    .chain(std::iter::once(Choice {
                        label: "Generate with Matugen".into(),
                        detail: "Image or HEX color".into(),
                    }))
                    .collect(),
                selected: preview.selected(),
                footer: if preview.generating() {
                    "Enter continue"
                } else {
                    "Enter save"
                }
                .to_owned()
                    + " · Esc cancel",
            },
            Menu::Palette { stage, .. } => {
                let editor = &self.navigation_editor;
                let input = String::from_utf8_lossy(&editor.bytes);
                let cursor = editor.cursor.min(input.len());
                let before = input.floor_char_boundary(cursor);
                let mut draft = format!("{}▏{}", &input[..before], &input[before..]);
                if draft.chars().count() > 60 {
                    draft = format!(
                        "…{}",
                        draft
                            .chars()
                            .rev()
                            .take(55)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<String>()
                    );
                }
                let (subtitle, items, selected, footer) = match stage {
                    PaletteStage::Source => (
                        "Image path or HEX color",
                        vec![Choice {
                            label: draft,
                            detail: String::new(),
                        }],
                        0,
                        "Enter choose scheme",
                    ),
                    PaletteStage::Scheme { selected, .. } => (
                        "Palette scheme",
                        theme::MATUGEN_SCHEMES
                            .iter()
                            .map(|(name, _)| Choice {
                                label: (*name).into(),
                                detail: String::new(),
                            })
                            .collect(),
                        *selected,
                        "Enter set contrast",
                    ),
                    PaletteStage::Contrast { .. } => (
                        "Contrast from -100 to 100 (default 0)",
                        vec![Choice {
                            label: draft,
                            detail: String::new(),
                        }],
                        0,
                        "Enter preview",
                    ),
                    PaletteStage::Generating => ("Generating palette…", Vec::new(), 0, ""),
                    PaletteStage::Preview(_) => ("Palette preview", Vec::new(), 0, "Enter save"),
                };
                Popup {
                    title: "Matugen".into(),
                    subtitle: subtitle.into(),
                    items,
                    selected,
                    footer: if footer.is_empty() {
                        "Esc cancel".into()
                    } else {
                        format!("{footer} · Esc cancel")
                    },
                }
            }
            Menu::Loading(title) => Popup {
                title: title.clone(),
                subtitle: "Loading…".into(),
                footer: "Esc cancel".into(),
                ..Popup::default()
            },
            Menu::DraftingPlan => return None,
            Menu::Sessions {
                entries,
                selected,
                resume_target,
                continuation,
                clarification,
            } => Popup {
                title: "Project sessions".into(),
                subtitle: if entries.is_empty()
                    && resume_target.is_none()
                    && continuation.is_none()
                    && clarification.is_none()
                {
                    "No saved conversations".into()
                } else {
                    "Choose a conversation".into()
                },
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
                    .chain(clarification.iter().map(|_| Choice {
                        label: "Open plan and saved clarifications".into(),
                        detail: String::new(),
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
            Menu::SessionSearch { matches, selected } => Popup {
                title: "Search project sessions".into(),
                subtitle: match (
                    matches.limit_reached,
                    matches.unavailable,
                    matches.hits.is_empty(),
                ) {
                    (true, true, _) => "Search incomplete · limits and unavailable sessions",
                    (true, false, _) => "Search incomplete · search limit reached",
                    (false, true, _) => "Search incomplete · some sessions unavailable",
                    (false, false, true) => "No matching messages or outcomes",
                    (false, false, false) => "Choose a matching conversation",
                }
                .into(),
                items: matches
                    .hits
                    .iter()
                    .map(|hit| Choice {
                        label: hit.snippet.clone(),
                        detail: format!(
                            "{} · {} / {} / {}",
                            hit.source, hit.session, hit.turn, hit.sequence
                        ),
                    })
                    .collect(),
                selected: *selected,
                footer: "Enter open · Esc back".into(),
            },
            Menu::SessionSearchSource {
                hit,
                lines,
                selected,
            } => Popup {
                title: "Saved conversation".into(),
                subtitle: format!("{} / {} / {}", hit.session, hit.turn, hit.sequence),
                items: lines.clone(),
                selected: *selected,
                footer: "Up/Down scroll · Esc close".into(),
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
        if let (Some(editor), Some(Panel::Files(files))) = (&self.editor, &self.panel)
            && let Some(document) = &files.document
        {
            let snapshot = editor.pane.snapshot();
            return Some(Inspector {
                title: format!(
                    "Editor · {}",
                    document
                        .path
                        .strip_prefix(&self.factory.settings.cwd)
                        .unwrap_or(&document.path)
                        .display()
                ),
                tabs: vec!["Files".into(), "Editor".into()],
                selected_tab: 1,
                kind: InspectorKind::Editor,
                rendered: Some(std::sync::Arc::new(snapshot.rows.clone())),
                cursor: snapshot
                    .cursor
                    .map(|(row, column)| (usize::from(column), usize::from(row))),
                footer: ":w save · :q → Source".into(),
                ..Inspector::default()
            });
        }
        match self.panel.as_ref()? {
            Panel::Orchestration { document, offset } => {
                let footer = if let Some(Menu::Orchestration { selected, proposal }) = &self.menu {
                    let blocked = proposal.plan.execution_ready().err();
                    let start = if blocked.is_some() {
                        "Start unavailable"
                    } else if proposal.profile.is_some() && proposal.plan.is_read_only() {
                        "Start read only"
                    } else {
                        "Start"
                    };
                    let mut actions = vec![start, "Refine"];
                    if proposal.can_request_implementation() {
                        actions.push("Implement");
                    }
                    actions.push("Cancel");
                    let actions = actions
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
                    "PgUp/PgDn or scroll · Esc back".into()
                };
                Some(Inspector {
                    title: "Plan · PgUp/PgDn or scroll".into(),
                    document: Some(document.clone()),
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
            Panel::Activity(navigation) => Some(self.activity_inspector(navigation)),
            Panel::NativeTerminal { view, .. } => Some(Inspector {
                tabs: activity::tabs(),
                selected_tab: 1,
                title: view
                    .as_ref()
                    .map_or_else(|| "Agent terminal".into(), |view| view.title.clone()),
                lines: view.as_ref().map_or_else(Vec::new, |view| {
                    view.text.lines().map(str::to_owned).collect()
                }),
                footer: "a activity · Esc back".into(),
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
                footer: "j/k lines · h/l horizontal · f expand · Esc back".into(),
                ..Inspector::default()
            }),
            Panel::AgentTranscript(reader) => Some(Inspector {
                title: format!("Agent transcript · {}", reader.title),
                document: reader.page.as_ref().map(|page| page.document.clone()),
                lines: vec![reader.notice.clone()],
                offset: reader.offset,
                footer: if reader.notice.is_empty() {
                    let pages = reader
                        .page
                        .as_ref()
                        .filter(|page| page.begin > 0 || page.end < page.total)
                        .map_or("", |_| " · [/] pages");
                    format!("PgUp/PgDn scroll{pages} · r refresh · Esc back")
                } else {
                    format!("{} · Esc back", reader.notice)
                },
                ..Inspector::default()
            }),
            Panel::Agents { selected } => {
                let agents: Vec<_> = self.agent_rows().into_iter().map(|(_, row)| row).collect();
                Some(Inspector {
                    title: "Conversation agents".into(),
                    selected: agents
                        .iter()
                        .position(|agent| Some(&agent.id) == selected.as_ref()),
                    agents,
                    footer: "Click or Enter to open · ↑/↓ select · Esc back".into(),
                    kind: InspectorKind::Agents,
                    ..Inspector::default()
                })
            }
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
    fn side_chat_parent(&self) -> Option<usize> {
        let child = &self.slots[self.current];
        if child.kind != Kind::Btw || self.panel.is_some() {
            return None;
        }
        let parent = child.parent.as_deref()?;
        self.slots.iter().position(|slot| slot.id == parent)
    }
    fn ui(&self) -> Workspace {
        let slot = &self.slots[self.current];
        let parent = self.side_chat_parent().map(|index| &self.slots[index]);
        let main = parent.unwrap_or(slot);
        let mut breadcrumb = Vec::new();
        let mut current = Some(main.id.clone());
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
            } else if id == main.id && main.kind == Kind::Fork {
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
            activity: match self.orchestration_mode {
                Some(OrchestrationMode::Standard) => "Orchestrate".into(),
                Some(OrchestrationMode::Plus) => "Orchestrate+".into(),
                None => String::new(),
            },
            exit_confirmation: self.exit_deadline.is_some(),
            reading: !main.state.follow_tail,
            new_events: main.state.snapshot.as_ref().map_or(0, |s| {
                s.transcript_size.saturating_sub(main.state.reading_total)
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
            drafting_plan: matches!(self.menu, Some(Menu::DraftingPlan)),
            draft_notice: !slot.state.editor.bytes.is_empty()
                && self.orchestration_summary.contains_key(&slot.id),
            interrupt_hint: if matches!(self.menu, Some(Menu::DraftingPlan)) {
                ""
            } else if !slot.state.editor.bytes.is_empty() {
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
            terminal_shortcut: self
                .available_commands()
                .iter()
                .any(|command| command.name == "/terminals"),
            side_chat: parent.map(|parent| carbon::SideChat {
                parent_history: parent
                    .state
                    .reading
                    .clone()
                    .or_else(|| self.projected_slot(parent))
                    .unwrap_or_default(),
                parent_scroll: parent.state.scroll,
                title: "/btw · read-only · Esc → main chat".into(),
            }),
        }
    }
    fn animation_enabled(&self) -> bool {
        let slot = &self.slots[self.current];
        matches!(self.panel, Some(Panel::Activity(_)))
            && self.selection.is_none()
            && slot
                .activity_page()
                .is_none_or(|page| page.end == page.total)
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
        if let Some(Panel::Agents { selected }) = &self.panel {
            let rows = self.agent_rows();
            if !rows.iter().any(|(id, _)| Some(id) == selected.as_ref()) {
                self.panel = Some(Panel::Agents {
                    selected: rows.into_iter().next().map(|(id, _)| id),
                });
            }
        }
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
                live.timeline_range = slot
                    .conversation_page
                    .as_ref()
                    .map(|page| (page.begin, page.end));
                live.timeline_total = slot.conversation_page.as_ref().map_or(0, |page| page.total);
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
            let mut frame = if self.side_chat_parent().is_some() {
                carbon::layout_workspace(
                    &[],
                    b"",
                    0,
                    status,
                    0,
                    context,
                    io.size,
                    io.color,
                    &self.ui(),
                )
            } else {
                carbon::layout_full(&[], b"", 0, status, 0, context, io.size, io.color)
            };
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
        if let Some(editor) = &mut self.editor
            && let Some((_, _, width, height)) =
                frame.as_ref().and_then(|frame| frame.inspector_bounds())
            && width > 0
            && height > 0
        {
            let size = (height.clamp(1, 128) as u16, width.clamp(1, 256) as u16);
            if size != editor.size && editor.pane.resize(size.0, size.1).is_ok() {
                editor.size = size;
            }
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
            && let Some(page) = &self.slots[self.current].conversation_page
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
    let result: Result<()> = async {
        while !finished {
            if let Some(bridge) = &mut bridge
                && let Some(root) = work.slots.iter().find(|slot| slot.id == work.root_id) {
                bridge.set_input_mode(root.state.mode);
            }
            work.update_displayed(io);
            let mut input_guard = None; let mut signal_guard = None;
            let input_enabled = work.terminal_pending_input.is_empty() && !finished && !work.input_closed && work.pending_editor.is_none() && !work.editor_preparing
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
                        work.cancel_editor_request(); work.cancel_modal();
                    }
                    SIGINT => {
                        if work.terminal_focused() { work.terminal_bytes(&[3], io); }
                        else if work.pending_editor.is_some() || work.editor_preparing { work.cancel_editor_request(); deferred_input.clear(); }
                        else if !finished {
                            if work.input_closed { work.cancel_current(io); }
                            else { finished |= work.interrupt(io, Instant::now()); }
                        }
                    }
                    SIGTSTP => {
                        if work.terminal_focused() { work.terminal_bytes(&[26], io); }
                        else { io.suspend().await?; }
                        work.last_frame = None;
                    }
                    SIGCONT => { io.resume_terminal()?; work.last_frame = None; }
                    SIGWINCH => { io.resize(); work.last_frame = None;
                        if (io.size.0 < 12 || io.size.1 < 8) && work.focus == Focus::Inspector && work.editor.is_none() {
                            let pending = work.terminal_decoder.take_pending();
                            if !pending.is_empty() { work.terminal_bytes(&pending, io); }
                            work.terminal_escape_at = None;
                            work.focus = Focus::Chat;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(editor) = &mut work.editor {
                let snapshot = editor.pane.snapshot();
                dirty |= editor.revision != snapshot.revision;
                editor.revision = snapshot.revision;
                if let Some((rows, columns)) = work.terminal_dimensions()
                    && work.editor.as_ref().is_some_and(|editor| editor.size != (rows, columns))
                {
                    dirty = true;
                }
                if work.editor.as_ref().is_some_and(|editor| editor.pane.worker_finished()) {
                    let mut editor = work.editor.take().expect("finished editor");
                    let outcome = editor.pane.shutdown();
                    let snapshot = editor.pane.snapshot();
                    work.terminal_pending_input.clear();
                    work.terminal_decoder = Default::default();
                    work.terminal_escape_at = None;
                    deferred_input.clear();
                    dirty = true;
                    if !snapshot.cleanup_known(&outcome) {
                        work.report(io, "Editor process cleanup is unknown");
                        failure.get_or_insert("editor_cleanup_unknown");
                        finished = true;
                        work.terminating = true;
                    } else if !finished && !work.input_closed {
                        if let Some(error) = &snapshot.error {
                            work.report(io, &format!("Could not open editor: {error}"));
                        } else if let Some(status) = snapshot.exit_status && !status.success() {
                            work.report(io, &format!("Editor exited: {status}"));
                        }
                        if let Some(Panel::Files(files)) = &work.panel && let Some(document) = &files.document {
                            work.open_file(document.path.clone(), io);
                        }
                    }
                }
            }
            if !finished && !work.input_closed && work.pending_editor.is_some()
                && work.jobs.is_empty()
                && !work.slots.iter().any(Slot::active) && work.pending_context.is_empty()
                && work.orchestration_stop.is_none()
            {
                work.launch_panel_editor(io);
                dirty = true;
            }
            let now = Instant::now();
            dirty |= work.update_orchestration_progress();
            dirty |= work.tick_drag(io, now);
            work.refresh_terminal_screen(now);
            work.refresh_agent_reader();
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
            let activity_open = matches!(work.panel, Some(Panel::Activity(_)));
            let current = work.current;
            for (index, slot) in work.slots.iter_mut().enumerate() {
                dirty |= slot.poll_replies(io)?;
                admissions.append(&mut slot.admissions);
                if !finished {
                    if activity_open && current == index { slot.refresh_activity(); }
                    slot.refresh(io, now);
                }
                dirty |= slot.state.progress.tick(now);
                if slot.state.explicit_status { dirty |= slot.state.wait_clock.tick(now); }
            }
            for (id, accepted) in admissions {
                if !accepted {
                    work.pending_context.remove(&id);
                    work.orchestration_summary.retain(|_, pending| pending.0 != id);
                    work.draft_cancellations.retain(|_, pending| pending.submit.0 != id);
                    if work.planning_context.as_ref().is_some_and(|intent|
                        intent.command.0 == id || intent.resume.as_ref().is_some_and(|resume| resume.0 == id)) {
                        work.cancel_modal();
                        work.menu = None;
                    }
                }
            }
            if !work.draft_cancellations.is_empty() {
                for index in 0..work.slots.len() {
                    work.dispatch_draft_cancel(index);
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
            let can_read = work.terminal_pending_input.is_empty() && !finished && !work.input_closed && work.pending_editor.is_none() && !work.editor_preparing && work.slots[work.current].state.pending_draft.is_none()
                && work.slots[work.current].ready && !work.slots[work.current].ready_refresh
                && work.slots[work.current].state.history_ready && work.displayed.is_some()
                && work.operational_frame.is_some_and(|sequence| io.writer.displayed(sequence));
            if can_read && (!deferred_input.is_empty() || io.input.ready(false).map_err(|_| "cli_input_poll")?) {
                let mut bytes = [0; 4096];
                let read = if deferred_input.is_empty() { io.input.read(&mut bytes) } else { let count = deferred_input.len().min(bytes.len()); for byte in &mut bytes[..count] { *byte = deferred_input.pop_front().expect("bounded input"); } Ok(count) };
                match read {
                    Ok(0) => finished |= work.eof(io, work.editor.is_none()),
                    Ok(count) => {
                        dirty = true;
                        let count = if work.terminal_focused() {
                            work.terminal_chunk(&bytes[..count], io, &mut deferred_input);
                            0
                        } else { count };
                        for (position, &byte) in bytes[..count].iter().enumerate() {
                            work.disarm_exit();
                            if matches!(&work.menu, Some(Menu::Palette { stage, .. }) if stage.editing()) {
                                match work.navigation_editor.feed(byte, true) {
                                    Some(Action::Submit | Action::QueueSubmit) => { work.enter(io); }
                                    Some(Action::Escape) => work.escape(io),
                                    _ => {}
                                }
                                continue;
                            }
                            // Modal/read-only navigation never edits a draft.
                            if byte.is_ascii() && !work.input_editor().escape_pending() && !work.input_editor().pasting() {
                                let jumping = matches!(&work.panel, Some(Panel::Files(files)) if files.jump.is_some());
                                if byte == 7 && work.focus == Focus::Chat && work.menu.is_none() && work.completion.is_none() {
                                    work.open_activity();
                                    continue;
                                }
                                if work.menu.is_none() && work.focus == Focus::Inspector && work.activity_key(byte) { continue; }
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
                                if work.focus == Focus::Inspector && matches!(work.panel, Some(Panel::AgentTranscript(_))) && matches!(byte, b'r' | b'[' | b']') {
                                    if let Some(Panel::AgentTranscript(reader)) = &mut work.panel { reader.requested = Some(byte); }
                                    continue;
                                }
                                if work.focus == Focus::Inspector && matches!(work.panel, Some(Panel::Agents { .. } | Panel::AgentTranscript(_))) && byte >= 32 && byte != 127 { continue; }
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
                                    if byte == b'e' && files.tab == 0 && files.document.is_some() { work.prepare_editor(io); if work.editor_preparing { deferred_input.extend(&bytes[position + 1..count]); break; } continue; }
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
                                                if !matches!(work.menu, Some(Menu::DraftingPlan)) {
                                                    slot.state.editor.clear();
                                                    slot.state.input_history.reset();
                                                }
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
                                    Action::OpenFiles => { work.open_file(work.factory.settings.cwd.clone(), io); }
                                    Action::OpenTerminals => { work.open_activity(); }
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
            if dirty && !finished { work.render(io, context); dirty = false; }
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
        match tokio::task::spawn_blocking(move || {
            let outcome = editor.pane.shutdown();
            (outcome, editor.pane.snapshot())
        })
        .await
        {
            Ok((outcome, snapshot)) if snapshot.cleanup_known(&outcome) => {}
            _ => {
                failure.get_or_insert("editor_cleanup_unknown");
            }
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
            Ok(Job::AutocompactSaved {
                result: Err(code), ..
            }) if code.starts_with("autocompact_preferences_") => {
                failure.get_or_insert(code);
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
            query: slot.conversation_query.clone(),
        };
        slot.received_conversation(&page);
        let frame = carbon::layout_full(
            &[Entry {
                status: None,
                role: "user".into(),
                text: format!("EARLIEST\n{}", "later\n".repeat(50)),
                action: None,
                edit: None,
                output: None,
                agent: None,
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
            query: slot.conversation_query.clone(),
        };
        slot.received_conversation(&tail);
        assert_eq!(slot.state.scroll, 0);
        assert!(slot.state.follow_tail && !slot.jump_top);
        for _ in 0..2 {
            slot.jump_history(true);
            slot.cancel_history_jump();
            assert!(!slot.conversation_query.forward);
            assert_eq!(slot.conversation_query.before, tail.end);
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
    fn delayed_autocompact_save_preserves_newer_choice_per_account() {
        let root = TestDirectory(std::env::temp_dir().join(format!(
            "ansa-autocompact-preferences-{}",
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
        let selected = SystemTime::now();
        first
            .save_autocompact(selected + Duration::from_secs(1), 80)
            .unwrap();
        first.save_autocompact(selected, 20).unwrap();
        assert_eq!(first.saved_autocompact(), Some(80));
        assert_eq!(second.saved_autocompact(), None);
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
        let query = slot.conversation_query.clone();
        handle
            .post(
                slot.state
                    .command(&handle, CommandPayload::GetConversation(query)),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let SessionEvent::Runtime(RuntimeEvent {
                    payload: EventPayload::ConversationPage(page),
                    ..
                }) = events.recv().await.unwrap().1
                {
                    slot.conversation_page = Some(page);
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
                    agent: None,
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
        slot.activity_timeline = Some(TranscriptPage {
            begin: 0,
            end: 1,
            total: 1,
            query: slot.activity_query.clone(),
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
