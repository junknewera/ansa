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

pub const FRAME_BYTES: usize = 128 * 1024;
pub const OUTPUT_BYTES: usize = 512 * 1024;
pub const CLIENTS: usize = 8;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

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
        let task = tokio::spawn(async move {
            serve(listener, task_handle, receiver, stopped, uid, task_pages).await;
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
        })
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn publish(&mut self, event: &RuntimeEvent) {
        if event.session != *self.handle.session()
            || event.incarnation != *self.handle.incarnation()
        {
            return;
        }
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

async fn serve(
    listener: UnixListener,
    handle: Handle,
    view: watch::Receiver<(usize, Arc<String>)>,
    mut stop: watch::Receiver<bool>,
    uid: u32,
    pages: Arc<Mutex<Pages>>,
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
                    clients.spawn(async move { let _ = client(stream, h, v, stopped, p, prefix).await; });
                }
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
}

/// Enforce the frame bound before a complete line or JSON tree is allocated.
pub async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Json>> {
    let mut bytes = Vec::new();
    loop {
        let buffer = reader.fill_buf().await.map_err(|_| "workspace_read")?;
        if buffer.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err("workspace_incomplete_frame")
            };
        }
        let count = buffer
            .iter()
            .position(|v| *v == b'\n')
            .map_or(buffer.len(), |i| i + 1);
        if count > FRAME_BYTES - bytes.len() {
            return Err("workspace_frame_limit");
        }
        let complete = buffer[count - 1] == b'\n';
        bytes.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if complete {
            let text = std::str::from_utf8(&bytes).map_err(|_| "workspace_utf8")?;
            return Json::parse_with_limits(text, FRAME_BYTES, 16, 4096).map(Some);
        }
    }
}

async fn client(
    stream: UnixStream,
    handle: Handle,
    mut view: watch::Receiver<(usize, Arc<String>)>,
    mut stop: watch::Receiver<bool>,
    pages: Arc<Mutex<Pages>>,
    prefix: String,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let hello = tokio::time::timeout(IO_TIMEOUT, read_frame(&mut read))
        .await
        .map_err(|_| "workspace_handshake_timeout")??
        .ok_or("workspace_handshake")?;
    if hello.usize_field("version")? != 1
        || hello.text_field("role")? != "viewer"
        || !crate::provider::contracts::valid_id(hello.text_field("client")?)
    {
        let _ = tokio::time::timeout(
            IO_TIMEOUT,
            write.write_all(encoded(&error("workspace_handshake"))?.as_bytes()),
        )
        .await;
        return Err("workspace_handshake");
    }
    let welcome = obj(vec![
        ("type", s("welcome")?),
        ("version", n(1)),
        ("role", s("viewer")?),
        ("session", s(&handle.session().0)?),
        ("incarnation", s(&handle.incarnation().0)?),
        (
            "capabilities",
            Json::array(vec![s("snapshot")?, s("history")?])?,
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
        loop {
            let frame = tokio::select! { biased;
                _ = stop.changed() => break,
                _ = out.closed() => break,
                frame = read_frame(&mut read) => match frame? { Some(frame) => frame, None => break },
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
            let payload = match kind {
                "snapshot" => CommandPayload::GetSnapshot(0),
                "history" => CommandPayload::GetTranscript(GetTranscript {
                    before: frame.usize_field("before")?,
                    limit: frame.usize_field("limit")?,
                    forward: frame.field("forward")?.boolean()?,
                }),
                _ => {
                    out.try_send(encoded(&error("workspace_permission_denied"))?)
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
                let response = match tokio::time::timeout(IO_TIMEOUT, receiver).await {
                    Ok(Ok(page)) => history(&page, id, &handle).unwrap_or_else(error_encoded),
                    _ => error_encoded("workspace_history_timeout"),
                };
                out.try_send(response)
                    .map_err(|_| "workspace_slow_reader")?;
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
        let value = read_frame(&mut BufReader::new(read))
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
                read_frame(&mut BufReader::new(bytes.as_slice()))
                    .await
                    .unwrap_err(),
                expected
            );
        }
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
