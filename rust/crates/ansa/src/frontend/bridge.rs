//! Opt-in local projection of one production runtime. No native context codec.
use super::{Result, protocol_v1::*};
use crate::{
    config::{n, s},
    protocol::json::Json,
    runtime::Handle,
};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};
mod control;

pub const FRAME_BYTES: usize = 128 * 1024;
pub const OUTPUT_BYTES: usize = 512 * 1024;
pub const CLIENTS: usize = 8;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
pub struct BridgeOptions {
    pub directory: PathBuf,
    pub editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
}

fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::object(fields).expect("bounded bridge fields")
}
fn flag(value: bool) -> Json {
    Json::parse(if value { "true" } else { "false" }).unwrap()
}
fn optional(value: Option<&str>) -> Json {
    value.map_or_else(|| Json::parse("null").unwrap(), |v| s(v).unwrap())
}
fn error(code: &str) -> Json {
    obj(vec![
        ("type", s("error").unwrap()),
        ("code", s(code).unwrap()),
    ])
}
fn encoded(value: &Json) -> Result<Arc<String>> {
    let mut text = value.dump();
    if text.len() >= OUTPUT_BYTES {
        return Err("workspace_output_limit");
    }
    text.push('\n');
    Ok(Arc::new(text))
}

#[derive(Clone)]
struct View {
    cursor: usize,
    state: Json,
}
impl View {
    fn frame(&self, handle: &Handle) -> Result<Arc<String>> {
        encoded(&obj(vec![
            ("type", s("snapshot")?),
            ("version", n(1)),
            ("session", s(&handle.session().0)?),
            ("incarnation", s(&handle.incarnation().0)?),
            ("through", n(self.cursor)),
            ("state", self.state.clone()),
        ]))
    }
}

/// The UI's existing event pump calls publish. Socket tasks never consume
/// Handle::next_event and cannot steal Carbon's events.
pub struct Bridge {
    handle: Handle,
    view: View,
    sender: watch::Sender<(usize, Arc<String>)>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    socket: PathBuf,
    identity: (u64, u64),
    pages: Arc<Mutex<Pages>>,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    control: Arc<control::Control>,
}
#[derive(Default)]
struct Pages {
    next: usize,
    latest_total: usize,
    pending: Vec<PendingPage>,
}
struct PendingPage {
    token: usize,
    minimum_total: usize,
    query: GetTranscript,
    reply: oneshot::Sender<TranscriptPage>,
}
struct PageWait {
    token: usize,
    pages: Arc<Mutex<Pages>>,
}
impl Drop for PageWait {
    fn drop(&mut self) {
        self.pages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .retain(|p| p.token != self.token);
    }
}
fn history(page: &TranscriptPage, id: &str, handle: &Handle) -> Result<Arc<String>> {
    let entries = page
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.role.as_str(), "user" | "assistant") && e.tool.is_none())
        .map(|(index, e)| {
            obj(vec![
                ("index", n(page.begin + index)),
                ("role", s(&e.role).unwrap()),
                ("text", s(&e.text).unwrap()),
                ("request", s(&e.request.0).unwrap()),
                ("turn", s(&e.turn.0).unwrap()),
                ("state", s(&e.state).unwrap()),
            ])
        })
        .collect();
    encoded(&obj(vec![
        ("type", s("history")?),
        ("id", s(id)?),
        ("session", s(&handle.session().0)?),
        ("incarnation", s(&handle.incarnation().0)?),
        ("begin", n(page.begin)),
        ("end", n(page.end)),
        ("total", n(page.total)),
        ("entries", Json::array(entries)?),
    ]))
}
impl Bridge {
    pub fn start(directory: &Path, handle: Handle) -> Result<Self> {
        Self::start_with_editor(directory, handle, None)
    }
    pub fn start_with_editor(
        directory: &Path,
        handle: Handle,
        editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    ) -> Result<Self> {
        let directory = std::path::absolute(directory).map_err(|_| "workspace_directory")?;
        ansa_linux::fs::create_private(&directory).map_err(|_| "workspace_permissions")?;
        let owned =
            ansa_linux::fs::private_directory(&directory).map_err(|_| "workspace_permissions")?;
        let uid = owned.metadata().map_err(|_| "workspace_permissions")?.uid();
        let socket = directory.join("workspace.sock");
        // Never remove a socket merely because its former owner is absent.
        let listener = UnixListener::bind(&socket).map_err(|_| "workspace_socket_in_use")?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
            .map_err(|_| "workspace_permissions")?;
        let meta = fs::symlink_metadata(&socket).map_err(|_| "workspace_permissions")?;
        let identity = (meta.dev(), meta.ino());
        let view = View {
            cursor: 0,
            state: obj(vec![("phase", s("loading")?), ("durable", flag(false))]),
        };
        let (sender, receiver) = watch::channel((0, view.frame(&handle)?));
        let (stop, stopped) = watch::channel(false);
        let task_handle = handle.clone();
        let pages = Arc::new(Mutex::new(Pages::default()));
        let task_pages = pages.clone();
        let task_editor = editor.clone();
        let control = control::Control::new(handle.clone(), editor.clone());
        let task_control = control.clone();
        let task = tokio::spawn(async move {
            serve(
                listener,
                task_handle,
                receiver,
                stopped,
                uid,
                task_pages,
                task_editor,
                task_control,
            )
            .await;
        });
        Ok(Self {
            handle,
            view,
            sender,
            stop,
            task: Some(task),
            socket,
            identity,
            pages,
            editor,
            control,
        })
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }
    pub fn set_input_mode(&mut self, mode: AgentMode) {
        if let Some(editor) = &self.editor {
            editor.set_mode(mode);
            let label = if mode == AgentMode::Plan {
                "plan"
            } else {
                "basic"
            };
            if self.view.state.text_field("input_mode").ok() != Some(label) {
                self.view.state = self
                    .view
                    .state
                    .with("input_mode", s(label).unwrap())
                    .unwrap();
                self.changed();
            }
        }
    }

    pub fn publish(&mut self, event: &RuntimeEvent) {
        if event.session != *self.handle.session()
            || event.incarnation != *self.handle.incarnation()
        {
            return;
        }
        self.control.publish(event);
        match &event.payload {
            EventPayload::FrontendSnapshot(snapshot) => {
                self.pages
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .latest_total = snapshot.transcript_size;
                self.view.state = obj(vec![
                    ("phase", s(&snapshot.state).unwrap()),
                    ("turn", s(&event.turn.0).unwrap()),
                    (
                        "request",
                        optional(snapshot.request.as_ref().map(|v| v.0.as_str())),
                    ),
                    (
                        "submit",
                        optional(snapshot.submit.as_ref().map(|v| v.0.as_str())),
                    ),
                    ("durable", flag(snapshot.durable)),
                    ("busy", flag(snapshot.worker_busy)),
                    (
                        "input_mode",
                        self.editor.as_ref().map_or_else(
                            || {
                                s(if snapshot.mode == AgentMode::Plan {
                                    "plan"
                                } else {
                                    "basic"
                                })
                                .unwrap()
                            },
                            |e| {
                                s(if e.mode() == AgentMode::Plan {
                                    "plan"
                                } else {
                                    "basic"
                                })
                                .unwrap()
                            },
                        ),
                    ),
                    ("transcript_total", n(snapshot.transcript_size)),
                    (
                        "mode",
                        s(if snapshot.mode == AgentMode::Plan {
                            "plan"
                        } else {
                            "basic"
                        })
                        .unwrap(),
                    ),
                    ("diagnostic", s(&snapshot.diagnostic).unwrap()),
                    (
                        "text",
                        s(if snapshot.durable && snapshot.state == "completed" {
                            &snapshot.text
                        } else {
                            ""
                        })
                        .unwrap(),
                    ),
                    (
                        "preview",
                        snapshot.preview.as_ref().map_or_else(
                            || Json::parse("null").unwrap(),
                            |preview| {
                                obj(vec![
                                    ("state", s(&preview.state).unwrap()),
                                    ("text", s(&preview.text).unwrap()),
                                ])
                            },
                        ),
                    ),
                ]);
            }
            EventPayload::TranscriptPage(page) => {
                let mut pages = self.pages.lock().unwrap_or_else(|e| e.into_inner());
                let mut index = 0;
                while index < pages.pending.len() {
                    if pages.pending[index].query == page.query
                        && page.total >= pages.pending[index].minimum_total
                    {
                        let pending = pages.pending.swap_remove(index);
                        let _ = pending.reply.send(page.clone());
                    } else {
                        index += 1;
                    }
                }
                return;
            }
            EventPayload::Preview(preview) => {
                if event.request.as_ref().map(|r| r.0.as_str())
                    != self.view.state.text_field("request").ok()
                    || self.view.state.text_field("turn").ok() != Some(event.turn.0.as_str())
                {
                    return;
                }
                if let Ok(state) = self.view.state.with(
                    "preview",
                    obj(vec![
                        ("state", s(&preview.state).unwrap()),
                        ("text", s(&preview.text).unwrap()),
                    ]),
                ) {
                    self.view.state = state;
                }
            }
            EventPayload::ResyncRequired { .. } => {
                // The ordinary Carbon pump requests an authoritative snapshot.
                self.view.state = obj(vec![
                    ("phase", s("resync").unwrap()),
                    ("durable", flag(false)),
                ]);
            }
            _ => return,
        }
        self.changed();
    }
    fn changed(&mut self) {
        self.view.cursor += 1;
        let frame = self
            .view
            .frame(&self.handle)
            .unwrap_or_else(|code| encoded(&error(code)).unwrap());
        self.sender.send_replace((self.view.cursor, frame));
    }
    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if fs::symlink_metadata(&self.socket)
            .is_ok_and(|m| m.file_type().is_socket() && (m.dev(), m.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    listener: UnixListener,
    handle: Handle,
    view: watch::Receiver<(usize, Arc<String>)>,
    mut stop: watch::Receiver<bool>,
    uid: u32,
    pages: Arc<Mutex<Pages>>,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    control: Arc<control::Control>,
) {
    let mut clients = JoinSet::new();
    let mut connection = 0usize;
    loop {
        tokio::select! { biased;
            _ = stop.changed() => break,
            joined = clients.join_next(), if !clients.is_empty() => { let _ = joined; }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                if clients.len() < CLIENTS && stream.peer_cred().is_ok_and(|peer| peer.uid() == uid) {
                    connection += 1;
                    let prefix = format!("w_{}_{}", &handle.incarnation().0[..8], connection);
                    let h = handle.clone(); let v = view.clone(); let stopped = stop.clone();
                    let p = pages.clone();
                    let e = editor.clone();
                    let c=control.clone();
                    clients.spawn(async move { let _ = client(stream, h, v, stopped, p, prefix, e, c).await; });
                }
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}

/// Partial bytes belong to the connection, so completing another query cannot
/// discard a consumed prefix of its next command.
struct FrameReader<R> {
    reader: R,
    bytes: Vec<u8>,
}
impl<R: tokio::io::AsyncBufRead + Unpin> FrameReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            bytes: vec![],
        }
    }
    async fn next(&mut self) -> Result<Option<Json>> {
        loop {
            let buffer = self.reader.fill_buf().await.map_err(|_| "workspace_read")?;
            if buffer.is_empty() {
                return if self.bytes.is_empty() {
                    Ok(None)
                } else {
                    Err("workspace_incomplete_frame")
                };
            }
            let count = buffer
                .iter()
                .position(|v| *v == b'\n')
                .map_or(buffer.len(), |i| i + 1);
            if count > FRAME_BYTES - self.bytes.len() {
                return Err("workspace_frame_limit");
            }
            let complete = buffer[count - 1] == b'\n';
            self.bytes.extend_from_slice(&buffer[..count]);
            self.reader.consume(count);
            if complete {
                let text = std::str::from_utf8(&self.bytes).map_err(|_| "workspace_utf8")?;
                let result = Json::parse_with_limits(text, FRAME_BYTES, 16, 4096).map(Some);
                self.bytes.clear();
                return result;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn client(
    stream: UnixStream,
    handle: Handle,
    mut view: watch::Receiver<(usize, Arc<String>)>,
    mut stop: watch::Receiver<bool>,
    pages: Arc<Mutex<Pages>>,
    prefix: String,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    control: Arc<control::Control>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = FrameReader::new(BufReader::new(read));
    let hello = tokio::time::timeout(IO_TIMEOUT, read.next())
        .await
        .map_err(|_| "workspace_handshake_timeout")??
        .ok_or("workspace_handshake")?;
    let role = hello.text_field("role")?;
    if hello.usize_field("version")? != 1
        || !(role == "viewer" || role == "editor" && editor.is_some())
        || !crate::provider::contracts::valid_id(hello.text_field("client")?)
    {
        let _ = tokio::time::timeout(
            IO_TIMEOUT,
            write.write_all(encoded(&error("workspace_handshake"))?.as_bytes()),
        )
        .await;
        return Err("workspace_handshake");
    }
    let client_id = hello.text_field("client")?;
    let _editor_attachment = if role == "editor" {
        Some(
            editor
                .as_ref()
                .ok_or("workspace_permission_denied")?
                .attach(client_id)?,
        )
    } else {
        None
    };
    let welcome = obj(vec![
        ("type", s("welcome")?),
        ("version", n(1)),
        ("role", s(role)?),
        ("session", s(&handle.session().0)?),
        ("incarnation", s(&handle.incarnation().0)?),
        (
            "capabilities",
            Json::array(if role == "editor" {
                vec![
                    s("snapshot")?,
                    s("history")?,
                    s("publish_context")?,
                    s("invalidate_context")?,
                    s("submit_message")?,
                    s("message_lookup")?,
                    s("cancel_message")?,
                ]
            } else {
                vec![s("snapshot")?, s("history")?]
            })?,
        ),
        ("max_frame_bytes", n(FRAME_BYTES)),
    ]);
    tokio::time::timeout(IO_TIMEOUT, write.write_all(encoded(&welcome)?.as_bytes()))
        .await
        .map_err(|_| "workspace_write_timeout")?
        .map_err(|_| "workspace_write")?;
    let (out, mut responses) = mpsc::channel::<Arc<String>>(4);
    let initial_cursor = view.borrow().0;
    out.try_send(view.borrow_and_update().1.clone())
        .map_err(|_| "workspace_slow_reader")?;
    let writer = async move {
        let mut cursor = initial_cursor;
        loop {
            let frame = tokio::select! { biased;
                frame = responses.recv() => match frame { Some(frame) => frame, None => break },
                changed = view.changed() => {
                    if changed.is_err() { break; }
                    let (next, frame) = view.borrow_and_update().clone();
                    if next > cursor + 1 {
                        let gap = encoded(&obj(vec![("type", s("gap").unwrap()), ("after", n(cursor)), ("through", n(next))])).unwrap();
                        if !matches!(tokio::time::timeout(IO_TIMEOUT, write.write_all(gap.as_bytes())).await, Ok(Ok(()))) { break; }
                    }
                    cursor = next; frame
                }
            };
            if !matches!(
                tokio::time::timeout(IO_TIMEOUT, write.write_all(frame.as_bytes())).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    };
    let mut sequence = 0usize;
    let reader = async {
        type PageJob = std::pin::Pin<Box<dyn std::future::Future<Output = Arc<String>> + Send>>;
        let mut page_job: Option<PageJob> = None;
        let mut control_job: Option<control::Job> = None;
        let mut cancel_job: Option<control::Job> = None;
        loop {
            let frame = tokio::select! { biased;
                _ = stop.changed() => break,
                _ = out.closed() => break,
                response = async { cancel_job.as_mut().expect("enabled").await }, if cancel_job.is_some() => {
                    cancel_job=None;
                    out.try_send(response).map_err(|_|"workspace_slow_reader")?;
                    continue;
                }
                response = async { control_job.as_mut().expect("enabled").await }, if control_job.is_some() => {
                    control_job=None;
                    out.try_send(response).map_err(|_|"workspace_slow_reader")?;
                    continue;
                }
                response = async { page_job.as_mut().expect("enabled").await }, if page_job.is_some() => {
                    page_job = None;
                    out.try_send(response).map_err(|_| "workspace_slow_reader")?;
                    continue;
                }
                frame = read.next() => match frame? { Some(frame) => frame, None => break },
            };
            if frame.usize_field("version")? != 1
                || frame.text_field("session")? != handle.session().0
                || frame.text_field("incarnation")? != handle.incarnation().0
            {
                return Err("workspace_stale_attachment");
            }
            sequence += 1;
            let id = frame.text_field("id")?;
            if !crate::provider::contracts::valid_id(id) {
                return Err("workspace_request_id");
            }
            let kind = frame.text_field("kind")?;
            if role == "editor"
                && matches!(kind, "submit_message" | "cancel_message" | "message_lookup")
            {
                let job = if kind == "cancel_message" {
                    &mut cancel_job
                } else {
                    &mut control_job
                };
                if job.is_some() {
                    out.try_send(encoded(&obj(vec![
                        ("type", s("error")?),
                        ("id", s(id)?),
                        ("code", s("workspace_busy")?),
                    ]))?)
                    .map_err(|_| "workspace_slow_reader")?;
                } else {
                    *job = Some(if kind == "message_lookup" {
                        control.read(client_id, &frame, format!("{prefix}_{sequence}"))
                    } else {
                        control.send(client_id, &frame)
                    });
                }
                continue;
            }
            if kind == "history" && page_job.is_some() {
                out.try_send(encoded(&obj(vec![
                    ("type", s("posted")?),
                    ("id", s(id)?),
                    ("kind", s(kind)?),
                    ("status", s("QueueFull")?),
                ]))?)
                .map_err(|_| "workspace_slow_reader")?;
                continue;
            }
            if kind == "invalidate_context" && role == "editor" {
                let result = editor
                    .as_ref()
                    .ok_or("workspace_permission_denied")?
                    .invalidate(
                        client_id,
                        frame.text_field("editor_incarnation")?,
                        frame.usize_field("buffer")?,
                        frame.usize_field("sequence")?,
                    );
                let response = match result {
                    Ok(()) => obj(vec![("type", s("context_invalidated")?), ("id", s(id)?)]),
                    Err(code) => obj(vec![
                        ("type", s("error")?),
                        ("id", s(id)?),
                        ("code", s(code)?),
                    ]),
                };
                out.try_send(encoded(&response)?)
                    .map_err(|_| "workspace_slow_reader")?;
                continue;
            }
            if kind == "publish_context" && role == "editor" {
                let response = match editor
                    .as_ref()
                    .ok_or("workspace_permission_denied")?
                    .publish(client_id, frame.field("context")?)
                {
                    Ok(context) => obj(vec![
                        ("type", s("context_published")?),
                        ("id", s(id)?),
                        ("buffer", n(context.buffer)),
                        ("changedtick", n(context.changedtick)),
                        ("sequence", n(context.sequence)),
                        (
                            "mode",
                            s(
                                if editor.as_ref().expect("editor grant").mode() == AgentMode::Plan
                                {
                                    "plan"
                                } else {
                                    "basic"
                                },
                            )?,
                        ),
                    ]),
                    Err(code) => obj(vec![
                        ("type", s("error")?),
                        ("id", s(id)?),
                        ("code", s(code)?),
                    ]),
                };
                out.try_send(encoded(&response)?)
                    .map_err(|_| "workspace_slow_reader")?;
                continue;
            }
            let payload = match kind {
                "snapshot" => CommandPayload::GetSnapshot(0),
                "history" => CommandPayload::GetTranscript(GetTranscript {
                    before: frame.usize_field("before")?,
                    limit: frame.usize_field("limit")?,
                    forward: frame.field("forward")?.boolean()?,
                }),
                _ => {
                    out.try_send(encoded(&obj(vec![
                        ("type", s("error")?),
                        ("id", s(id)?),
                        ("code", s("workspace_permission_denied")?),
                    ]))?)
                    .map_err(|_| "workspace_slow_reader")?;
                    continue;
                }
            };
            let command = RuntimeCommand {
                schema_version: 1,
                session: handle.session().clone(),
                incarnation: handle.incarnation().clone(),
                turn: "workspace_read".into(),
                command: CommandId(format!("{prefix}_{sequence}")),
                payload,
            };
            let page_wait = if let CommandPayload::GetTranscript(query) = &command.payload {
                if query.limit == 0 || query.limit > 64 {
                    return Err("transcript_page_limit");
                }
                let (reply, receiver) = oneshot::channel();
                let mut pending = pages.lock().unwrap_or_else(|e| e.into_inner());
                if pending.pending.len() >= CLIENTS {
                    return Err("workspace_reads_full");
                }
                pending.next += 1;
                let token = pending.next;
                let minimum_total = pending.latest_total;
                pending.pending.push(PendingPage {
                    token,
                    minimum_total,
                    query: query.clone(),
                    reply,
                });
                Some((
                    PageWait {
                        token,
                        pages: pages.clone(),
                    },
                    receiver,
                ))
            } else {
                None
            };
            let status = tokio::time::timeout(IO_TIMEOUT, handle.post(command))
                .await
                .map_err(|_| "workspace_post_timeout")??;
            let response = obj(vec![
                ("type", s("posted")?),
                ("id", s(id)?),
                ("kind", s(kind)?),
                ("status", s(&format!("{status:?}"))?),
            ]);
            out.try_send(encoded(&response)?)
                .map_err(|_| "workspace_slow_reader")?;
            if let Some((_guard, receiver)) = page_wait
                && status == crate::runtime::PostStatus::Accepted
            {
                let id = id.to_owned();
                let handle = handle.clone();
                page_job = Some(Box::pin(async move {
                    let _guard = _guard;
                    match tokio::time::timeout(IO_TIMEOUT, receiver).await {
                        Ok(Ok(page)) => history(&page, &id, &handle).unwrap_or_else(error_encoded),
                        _ => error_encoded("workspace_history_timeout"),
                    }
                }));
            }
        }
        Ok(())
    };
    tokio::pin!(writer, reader);
    tokio::select! { result = &mut reader => result, _ = &mut writer => Ok(()) }
}
fn error_encoded(code: &str) -> Arc<String> {
    encoded(&error(code)).expect("bounded diagnostic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::DirBuilderExt,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "ansa-bridge-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("fixture directory: {e}"),
                }
            }
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn framing_bounds_partial_utf8_duplicates_and_incomplete_lines() {
        let (mut write, read) = tokio::io::duplex(4);
        let writer = tokio::spawn(async move {
            for byte in "{\"text\":\"Ж界\"}\n".as_bytes() {
                write.write_all(&[*byte]).await.unwrap();
            }
        });
        let value = FrameReader::new(BufReader::new(read))
            .next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value.text_field("text").unwrap(), "Ж界");
        writer.await.unwrap();
        for (bytes, expected) in [
            (vec![b'x'; FRAME_BYTES + 1], "workspace_frame_limit"),
            (b"{\"v\":1,\"v\":2}\n".to_vec(), "json_duplicate_key"),
            (b"{\"v\":1}".to_vec(), "workspace_incomplete_frame"),
        ] {
            assert_eq!(
                FrameReader::new(BufReader::new(bytes.as_slice()))
                    .next()
                    .await
                    .unwrap_err(),
                expected
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_frame_read_retains_consumed_prefix() {
        let (mut write, read) = tokio::io::duplex(64);
        let mut frames = FrameReader::new(BufReader::new(read));
        write.write_all(b"{\"text\":\"").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frames.next())
                .await
                .is_err()
        );
        assert_eq!(frames.bytes, b"{\"text\":\"");
        write.write_all("Ж界\"}\n".as_bytes()).await.unwrap();
        assert_eq!(
            frames
                .next()
                .await
                .unwrap()
                .unwrap()
                .text_field("text")
                .unwrap(),
            "Ж界"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn history_completion_between_command_fragments_keeps_connection_and_frame() {
        let root = Directory::new();
        let mut runtime = super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let handle = runtime.handle();
        let mut bridge = Bridge::start(&root.0, handle.clone()).unwrap();
        let stream = UnixStream::connect(bridge.socket()).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut replies = FrameReader::new(BufReader::new(read));
        write
            .write_all(b"{\"version\":1,\"role\":\"viewer\",\"client\":\"fragment_test\"}\n")
            .await
            .unwrap();
        async fn wait_kind<R: tokio::io::AsyncBufRead + Unpin>(
            read: &mut FrameReader<R>,
            kind: &str,
        ) -> Json {
            tokio::time::timeout(IO_TIMEOUT, async {
                loop {
                    let frame = read.next().await.unwrap().expect("connection closed");
                    if frame.text_field("type").unwrap() == kind {
                        return frame;
                    }
                }
            })
            .await
            .expect("expected frame")
        }
        wait_kind(&mut replies, "welcome").await;
        let base = obj(vec![
            ("version", n(1)),
            ("session", s(&handle.session().0).unwrap()),
            ("incarnation", s(&handle.incarnation().0).unwrap()),
        ]);
        let query = base
            .with("id", s("page").unwrap())
            .unwrap()
            .with("kind", s("history").unwrap())
            .unwrap()
            .with("before", n(0))
            .unwrap()
            .with("limit", n(1))
            .unwrap()
            .with("forward", flag(false))
            .unwrap();
        write
            .write_all(encoded(&query).unwrap().as_bytes())
            .await
            .unwrap();
        let page = tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                let event = runtime.next_event().await.unwrap();
                if matches!(event.payload, EventPayload::TranscriptPage(_)) {
                    return event;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            wait_kind(&mut replies, "posted")
                .await
                .text_field("status")
                .unwrap(),
            "Accepted"
        );
        let command = base
            .with("id", s("fragment").unwrap())
            .unwrap()
            .with("kind", s("publish_context").unwrap())
            .unwrap()
            .with("text", s("Ж界").unwrap())
            .unwrap();
        let frame = encoded(&command).unwrap();
        let (first, second) = frame.as_bytes().split_at(frame.len() / 2);
        write.write_all(first).await.unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // The accepted read is deliberately withheld until the next command's
        // prefix is already on the socket, then completed in the same session.
        bridge.publish(&page);
        assert_eq!(
            wait_kind(&mut replies, "history")
                .await
                .text_field("id")
                .unwrap(),
            "page"
        );
        write.write_all(second).await.unwrap();
        assert_eq!(
            wait_kind(&mut replies, "error")
                .await
                .text_field("code")
                .unwrap(),
            "workspace_permission_denied"
        );
        bridge.shutdown().await;
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dto_excludes_internal_tool_outputs_and_preview_from_another_request() {
        use crate::provider::replay::{Effects, Execution};
        let root = Directory::new();
        let mut runtime = super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let handle = runtime.handle();
        let mut bridge = Bridge::start(&root.0, handle.clone()).unwrap();
        runtime
            .post(RuntimeCommand {
                schema_version: 1,
                session: handle.session().clone(),
                incarnation: handle.incarnation().clone(),
                turn: "read".into(),
                command: "snapshot".into(),
                payload: CommandPayload::GetSnapshot(0),
            })
            .await
            .unwrap();
        let mut event = loop {
            let event = runtime.next_event().await.unwrap();
            if matches!(event.payload, EventPayload::FrontendSnapshot(_)) {
                break event;
            }
        };
        let EventPayload::FrontendSnapshot(snapshot) = &mut event.payload else {
            unreachable!()
        };
        snapshot.tools.push(Activity::Finished(Finished {
            operation: "private_operation".into(),
            call_id: "private_call".into(),
            execution: Execution::Succeeded,
            effects: Effects::KnownResult,
            output: Arc::new("INTERNAL_OUTPUT_CANARY".into()),
        }));
        bridge.publish(&event);
        let baseline = bridge.sender.borrow().clone();
        assert!(!baseline.1.contains("INTERNAL_OUTPUT_CANARY"));
        assert!(!baseline.1.contains("private_operation"));
        event.request = Some("next_request".into());
        event.payload = EventPayload::Preview(Preview {
            generation: 1,
            text: "WRONG_REQUEST_PREVIEW".into(),
            state: "preview".into(),
        });
        bridge.publish(&event);
        assert_eq!(baseline, *bridge.sender.borrow());
        bridge.shutdown().await;
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cleanup_retains_replacement_socket_path_and_rejects_public_directory() {
        let root = Directory::new();
        let mut runtime = super::super::app::offline_runtime(None, false).unwrap();
        runtime.ready().await.unwrap();
        let bridge = Bridge::start(&root.0, runtime.handle()).unwrap();
        let socket = bridge.socket().to_owned();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, "replacement").unwrap();
        bridge.shutdown().await;
        assert_eq!(fs::read_to_string(&socket).unwrap(), "replacement");
        fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            Bridge::start(&root.0, runtime.handle()),
            Err("workspace_permissions")
        ));
        runtime.shutdown().await.unwrap();
    }
}
