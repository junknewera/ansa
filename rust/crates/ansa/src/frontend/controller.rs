//! Serialized frontend state. Runtime state is changed only by typed commands.
use super::{
    Result,
    carbon::{self, Entry},
    input::{Action, Editor},
    jsonl,
    protocol_v1::*,
    terminal::Io,
};
use crate::{
    runtime::{Handle, PostStatus, Runtime},
    session::transcript,
};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
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
type Replies = JoinSet<(CommandId, bool, Result<PostStatus>)>;
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
                limit: 16,
                forward: false,
            },
            editor: Editor::default(),
            mode: AgentMode::Basic,
            expanded: false,
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
        if replies.len() >= 64 {
            return false;
        }
        let command = self.command(handle, payload);
        let submit = matches!(command.payload, CommandPayload::Submit(_));
        if submit {
            self.pending = Some(command.command.clone());
        }
        let h = handle.clone();
        replies.spawn(async move {
            let id = command.command.clone();
            (id, submit, h.post(command).await)
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
    fn available(&self) -> bool {
        !self.awaiting()
            && self.snapshot.as_ref().is_some_and(|v| {
                !v.worker_busy && matches!(v.state.as_str(), "idle" | "completed" | "cancelled")
            })
    }
    fn report(&mut self, io: &Io, text: &str) {
        if io.raw.is_some() {
            self.status = text.into();
        } else {
            io.status(text);
        }
    }
    fn text(
        &mut self,
        text: &str,
        handle: &Handle,
        io: &Io,
        replies: &mut Replies,
    ) -> (bool, bool) {
        match text {
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
                if let Some(v) = &self.snapshot {
                    let text = format!("ansa: {} tools={}\n", v.state, v.completed_tools);
                    self.report(io, &text);
                }
                return (true, false);
            }
            "/cancel" => {
                if self.awaiting() {
                    self.deferred_cancel = true;
                    self.post(handle, CommandPayload::GetSnapshot(0), replies);
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
        if self.snapshot.as_ref().is_some_and(|v| v.state == "paused") {
            self.report(io, "ansa: paused; use /resume, /cancel or a new session\n");
            self.exit = 1;
            return (false, false);
        }
        if !self.available() {
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
                if history_page_current(
                    v,
                    &self.query,
                    self.snapshot.as_ref().map_or(0, |v| v.transcript_size),
                ) {
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
                if io.raw.is_some() {
                    self.status = format!(
                        "tool: {}",
                        if v.name.is_empty() {
                            "running"
                        } else {
                            &v.name
                        }
                    );
                }
            }
            EventPayload::RuntimeError { diagnostic, .. } => {
                self.report(io, &format!("ansa: {diagnostic}\n"))
            }
            _ => {}
        }
    }
    fn render(&mut self, io: &Io, context: &str) -> Option<carbon::Frame> {
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
                    } else {
                        ""
                    }
                ),
            });
        }
        if !self.status.is_empty() {
            entries.push(Entry {
                role: "status".into(),
                text: self.status.clone(),
            });
        }
        let phase = match view.state.as_str() {
            "idle" | "completed" => "готов",
            "cancelled" => "отменено",
            "paused" => "пауза",
            _ => "в работе",
        };
        let frame = carbon::layout(
            &entries,
            &self.editor.bytes,
            self.editor.cursor,
            &format!("{}  {phase}", jsonl::mode(self.mode)),
            self.expanded,
            self.scroll,
            context,
            io.size,
            io.color,
        )?;
        self.scroll_limit = frame.scroll_limit;
        Some(frame)
    }
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
    let mut actor_deadline = None;
    let mut resync = false;
    let mut next_history = Instant::now();
    let mut next_cancel = Instant::now();
    let mut last_frame = None;
    let mut dirty = true;
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if finished || io.writer.failed() {
            handle.request_shutdown();
        }
        if actor_done
            && ((events_done && io.writer.idle())
                || actor_deadline.is_some_and(|v| Instant::now() >= v))
        {
            if !io.writer.idle() {
                io.writer.abort();
            }
            break;
        }
        tokio::select! {biased;
            reply=replies.join_next(),if !replies.is_empty()=>{
                if let Some(reply)=reply {dirty=true;let (id,submit,result)=reply.map_err(|_|"cli_post_task")?;
                    let code=match result {Ok(PostStatus::Accepted|PostStatus::Duplicate)=>None,Ok(PostStatus::QueueFull)=>Some("command_queue_full"),Ok(PostStatus::Closed)=>Some("runtime_closed"),Ok(PostStatus::StaleTarget)=>Some("stale_cancel_target"),Ok(PostStatus::StaleIncarnation)=>Some("stale_incarnation"),Ok(PostStatus::CommandIdLimit)=>Some("submit_id_limit"),Ok(PostStatus::CommandConflict)=>Some("command_id_conflict"),Err(code)=>Some(code)};
                    if state.pending_draft.as_ref().is_some_and(|(draft,_,_)|draft==&id){let (_,bytes,cursor)=state.pending_draft.take().expect("checked");if code.is_some(){state.editor.bytes=bytes;state.editor.cursor=cursor;}}
                    if let Some(code)=code {
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
            result=runtime.join(),if !actor_done=>{result?;actor_done=true;actor_deadline=Some(Instant::now()+Duration::from_millis(250));},
            _=interval.tick()=>{},
        }
        while let Some(signal) = io.signals.take().map_err(|_| "cli_signal_read")? {
            dirty = true;
            use ansa_linux::terminal::*;
            match signal {
                SIGTERM | SIGHUP => {
                    state.exit = 128 + signal;
                    finished = true;
                    handle.request_shutdown();
                }
                SIGINT => {
                    if prompt.is_some() || jsonl_mode {
                        state.exit = 130;
                    }
                    if !state.cancelled && state.awaiting() {
                        state.cancelled = true;
                        state.deferred_cancel = true;
                        state.post(&handle, CommandPayload::GetSnapshot(0), &mut replies);
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
                    if let Some(raw) = &mut io.raw {
                        raw.restore();
                    }
                    io.signals.suspend().map_err(|_| "cli_suspend")?;
                    if let Some(raw) = &mut io.raw {
                        raw.resume().map_err(|_| "cli_termios")?;
                    }
                    last_frame = None;
                }
                SIGCONT => {
                    if let Some(raw) = &mut io.raw {
                        raw.resume().map_err(|_| "cli_termios")?;
                    }
                    last_frame = None;
                }
                SIGWINCH => {
                    io.resize();
                    last_frame = None;
                }
                _ => {}
            }
        }
        let ready = state.snapshot.is_some();
        if !jsonl_mode && ready {
            if resync {
                state.post(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                resync = false;
            }
            if io.raw.is_some() && Instant::now() >= next_history {
                state.post(
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
                    state.post(&handle, CommandPayload::GetSnapshot(0), &mut replies);
                    next_cancel = Instant::now() + Duration::from_millis(40);
                }
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
                        if let Some(action) = state.editor.feed(byte, io.raw.is_some()) {
                            match action {
                                Action::Overflow => {
                                    state.report(io, "ansa: submit_size\n");
                                    state.exit = 1;
                                    state.editor.clear();
                                }
                                Action::Submit => match state.editor.text().map(str::to_owned) {
                                    Ok(text) => {
                                        let (accepted, exit) =
                                            state.text(&text, &handle, io, &mut replies);
                                        if accepted || io.raw.is_none() {
                                            state.editor.clear();
                                            state.scroll = 0;
                                        }
                                        finished |= exit;
                                    }
                                    Err(code) => {
                                        state.report(io, &format!("ansa: {code}\n"));
                                        state.exit = 1;
                                        state.editor.clear();
                                    }
                                },
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
                                    state.expanded = !state.expanded;
                                    state.scroll = 0;
                                    if !state.expanded {
                                        state.query = GetTranscript {
                                            before: 0,
                                            limit: 16,
                                            forward: false,
                                        };
                                        next_history = Instant::now();
                                    }
                                }
                                Action::PageUp => {
                                    if state.expanded {
                                        if state.scroll >= state.scroll_limit && state.page.0 > 0 {
                                            state.query = GetTranscript {
                                                before: state.page.0,
                                                limit: 16,
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
                                            limit: 16,
                                            forward: true,
                                        };
                                        next_history = Instant::now();
                                    } else {
                                        state.scroll = state.scroll.saturating_sub(8);
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
        if eof && !closed && !state.deferred_cancel && replies.is_empty() {
            handle.close_input();
            closed = true;
        }
    }
    handle.request_shutdown();
    while replies.join_next().await.is_some() {}
    runtime.join().await?;
    if state.exit != 130
        && state
            .snapshot
            .as_ref()
            .is_some_and(|v| v.state == "paused" || v.storage_failed)
    {
        state.exit = 1;
    }
    if !io.writer.finish() && state.exit == 0 {
        state.exit = 1;
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
                replies.spawn(async move { (id, false, h.post(command).await) });
            }
        }
        Err(code) => io.status(&(jsonl::error(code, None).dump() + "\n")),
    }
}
