//! Only canonical timeline outcomes grant access to an artifact. Store receipts
//! are execution evidence and are not independently advertised as committed.
use super::*;
use crate::{
    provider::replay::{Effects, Execution},
    session::transcript::ToolPhase,
    tools::artifacts::{Store, validate_descriptor},
};
use std::collections::{BTreeMap, VecDeque};

type Key = (String, String);
struct Read {
    descriptor: Json,
    reply: oneshot::Sender<Result<Json>>,
}
pub(super) struct Assets {
    session: String,
    known: Mutex<BTreeMap<Key, Json>>,
    documents: Mutex<(BTreeMap<String, Json>, VecDeque<String>)>,
    requests: Mutex<Option<mpsc::Sender<Read>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
fn key(value: &Json) -> Result<Key> {
    Ok((
        value.text_field("artifact_id")?.into(),
        value.text_field("revision")?.into(),
    ))
}
fn extract(page: &TranscriptPage, session: &str) -> Vec<Json> {
    let mut descriptors = vec![];
    for entry in &page.entries {
        let Some(tool) = &entry.tool else { continue };
        if entry.role != "tool_result"
            || tool.name != "python_execute"
            || tool.continued
            || !tool.last_chunk
            || tool.phase
                != (ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                })
        {
            continue;
        }
        let Some(text) = entry.text.get(tool.content_start..) else {
            continue;
        };
        let Ok(value) = Json::parse_with_limits(text, 128 * 1024, 16, 4096) else {
            continue;
        };
        if value.text_field("status").ok() != Some("ready") {
            continue;
        }
        let Ok(items) = value.field("artifacts").and_then(Json::elements) else {
            continue;
        };
        if items.len() != 3
            || items
                .iter()
                .any(|item| validate_descriptor(item, session, Some(&tool.operation)).is_err())
        {
            continue;
        }
        descriptors.extend(items.iter().cloned());
    }
    descriptors
}
fn documents(page: &TranscriptPage, session: &str) -> Vec<Json> {
    let mut result = vec![];
    for entry in &page.entries {
        let Some(tool) = &entry.tool else { continue };
        if entry.role != "tool_result"
            || !matches!(
                tool.name.as_str(),
                "editor_open_file" | "editor_propose_diff"
            )
            || tool.continued
            || !tool.last_chunk
            || tool.phase
                != (ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                })
        {
            continue;
        }
        let Some(text) = entry.text.get(tool.content_start..) else {
            continue;
        };
        let Ok(value) = Json::parse_with_limits(text, 120 * 1024, 16, 4096) else {
            continue;
        };
        if value.text_field("status").ok() != Some("presentation_requested") {
            continue;
        }
        let Ok(doc) = value.field("document") else {
            continue;
        };
        let valid = (|| -> Result<bool> {
            Ok(doc.keys()?.len() == 14
                && doc.text_field("session_id")? == session
                && doc.text_field("producer_operation_id")? == tool.operation
                && doc.text_field("document_id")?
                    == format!(
                        "d_{}",
                        crate::protocol::legacy_json_v1::sha256(tool.operation.as_bytes())
                    )
                && doc.text_field("kind")?
                    == if tool.name == "editor_open_file" {
                        "source"
                    } else {
                        "diff"
                    }
                && doc.text_field("before")?.len() <= 16 * 1024
                && doc.text_field("after")?.len() <= 16 * 1024
                && doc.text_field("filetype")?.len() <= 32
                && doc.usize_field("first_line")? > 0
                && doc
                    .text_field("filetype")?
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && (doc.field("path")?.is_null() || doc.text_field("path")?.len() <= 4096))
        })()
        .unwrap_or(false);
        if valid {
            result.push(doc.clone());
        }
    }
    result
}
fn metadata(doc: &Json) -> Json {
    obj([
        "document_id",
        "session_id",
        "kind",
        "path",
        "filetype",
        "first_line",
    ]
    .into_iter()
    .map(|key| (key, doc.field(key).unwrap().clone()))
    .collect())
}
impl Assets {
    pub fn new(store: Store, session: &str) -> Arc<Self> {
        let (sender, mut receiver) = mpsc::channel::<Read>(CLIENTS);
        // One owned filesystem worker for this endpoint. Client timeout/EOF
        // drops its reply, while bridge shutdown closes the queue and joins it.
        let worker = tokio::task::spawn_blocking(move || {
            while let Some(request) = receiver.blocking_recv() {
                let answer = (|| -> Result<_> {
                    let (path, bytes) = store.load(&request.descriptor)?;
                    let content = if request.descriptor.text_field("mime")? == "image/png" {
                        Json::parse("null")?
                    } else {
                        s(std::str::from_utf8(&bytes).map_err(|_| "artifact_text")?)?
                    };
                    Ok(obj(vec![
                        ("availability", s("available")?),
                        ("descriptor", request.descriptor.clone()),
                        ("path", s(path.to_str().ok_or("artifact_path")?)?),
                        ("content", content),
                    ]))
                })();
                let _ = request.reply.send(answer);
            }
        });
        Arc::new(Self {
            session: session.into(),
            known: Mutex::new(BTreeMap::new()),
            documents: Mutex::new((BTreeMap::new(), VecDeque::new())),
            requests: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
        })
    }
    pub fn observe(&self, page: &TranscriptPage) {
        let mut known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        for descriptor in extract(page, &self.session) {
            let Ok(key) = key(&descriptor) else { continue };
            if known.len() < 384 && !known.contains_key(&key) {
                known.insert(key, descriptor);
            }
        }
        drop(known);
        let mut known = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        for document in documents(page, &self.session) {
            let id = document.text_field("document_id").unwrap().to_owned();
            if !known.0.contains_key(&id) {
                if known.0.len() == 64
                    && let Some(old) = known.1.pop_front()
                {
                    known.0.remove(&old);
                }
                known.1.push_back(id.clone());
                known.0.insert(id, document);
            }
        }
    }
    pub fn page(&self, page: &TranscriptPage, id: &str, handle: &Handle) -> Arc<String> {
        let known = self.known.lock().unwrap_or_else(|e| e.into_inner());
        let descriptors = extract(page, &self.session)
            .into_iter()
            .filter(|d| key(d).ok().and_then(|k| known.get(&k)) == Some(d))
            .collect();
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let published = documents(page, &self.session)
            .iter()
            .filter(|doc| docs.0.get(doc.text_field("document_id").unwrap()) == Some(*doc))
            .map(metadata)
            .collect();
        encoded(&obj(vec![
            ("type", s("artifact_catalog").unwrap()),
            ("id", s(id).unwrap()),
            ("session", s(&handle.session().0).unwrap()),
            ("incarnation", s(&handle.incarnation().0).unwrap()),
            ("begin", n(page.begin)),
            ("end", n(page.end)),
            ("total", n(page.total)),
            ("artifacts", Json::array(descriptors).unwrap()),
            ("documents", Json::array(published).unwrap()),
        ]))
        .unwrap_or_else(error_encoded)
    }
    pub fn read(self: &Arc<Self>, value: &Json) -> control::Job {
        let id = value.text_field("id").unwrap_or("invalid").to_owned();
        let descriptor = (|| -> Result<Json> {
            if value.keys()?.iter().any(|k| {
                ![
                    "version",
                    "session",
                    "incarnation",
                    "id",
                    "kind",
                    "artifact_id",
                    "revision",
                ]
                .contains(k)
            }) {
                return Err("workspace_command_fields");
            }
            let key = key(value)?;
            self.known
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .cloned()
                .ok_or("artifact_not_committed")
        })();
        let sender = self
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Box::pin(async move {
            let result = async {
                let descriptor = descriptor?;
                let sender = sender.ok_or("workspace_closed")?;
                let (reply, receiver) = oneshot::channel();
                sender
                    .try_send(Read { descriptor, reply })
                    .map_err(|_| "workspace_reads_full")?;
                tokio::time::timeout(IO_TIMEOUT, receiver)
                    .await
                    .map_err(|_| "artifact_read_timeout")?
                    .map_err(|_| "workspace_closed")?
            }
            .await;
            let value = match result {
                Ok(value) => value
                    .with("type", s("artifact").unwrap())
                    .unwrap()
                    .with("id", s(&id).unwrap())
                    .unwrap(),
                Err(code) => obj(vec![
                    ("type", s("error").unwrap()),
                    ("id", s(&id).unwrap()),
                    ("code", s(code).unwrap()),
                ]),
            };
            encoded(&value).unwrap_or_else(error_encoded)
        })
    }
    pub fn read_document(&self, value: &Json) -> control::Job {
        let id = value.text_field("id").unwrap_or("invalid").to_owned();
        let document = (|| -> Result<Json> {
            if value.keys()?.iter().any(|k| {
                ![
                    "version",
                    "session",
                    "incarnation",
                    "id",
                    "kind",
                    "document_id",
                ]
                .contains(k)
            }) {
                return Err("workspace_command_fields");
            }
            self.documents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
                .get(value.text_field("document_id")?)
                .cloned()
                .ok_or("document_not_loaded")
        })();
        Box::pin(async move {
            encoded(&match document {
                Ok(doc) => obj(vec![
                    ("type", s("document").unwrap()),
                    ("id", s(&id).unwrap()),
                    ("document", doc),
                ]),
                Err(code) => obj(vec![
                    ("type", s("error").unwrap()),
                    ("id", s(&id).unwrap()),
                    ("code", s(code).unwrap()),
                ]),
            })
            .unwrap_or_else(error_encoded)
        })
    }
    pub async fn shutdown(&self) {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::{Entry, ToolDetail};
    #[test]
    fn document_projection_requires_exact_canonical_producer_session_and_kind() {
        let id = format!(
            "d_{}",
            crate::protocol::legacy_json_v1::sha256(b"operation")
        );
        let document = obj(vec![
            ("document_id", s(&id).unwrap()),
            ("session_id", s("session").unwrap()),
            ("producer_operation_id", s("operation").unwrap()),
            ("kind", s("diff").unwrap()),
            ("path", Json::parse("null").unwrap()),
            ("filetype", s("rust").unwrap()),
            ("first_line", n(1)),
            ("before", s("SCOPED_SOURCE_CANARY").unwrap()),
            ("after", s("replacement").unwrap()),
            ("client", s("editor").unwrap()),
            ("editor_incarnation", s("editor").unwrap()),
            ("buffer", n(1)),
            ("changedtick", n(2)),
            ("sequence", n(1)),
        ]);
        let output = obj(vec![
            ("status", s("presentation_requested").unwrap()),
            ("document", document),
        ]);
        let tool = ToolDetail {
            operation: "operation".into(),
            name: "editor_propose_diff".into(),
            call: "call".into(),
            purpose: "".into(),
            target: String::new(),
            phase: ToolPhase::Finished {
                execution: Execution::Succeeded,
                effects: Effects::KnownResult,
            },
            content_start: 0,
            continued: false,
            last_chunk: true,
        };
        let mut page = TranscriptPage {
            begin: 0,
            end: 1,
            total: 1,
            query: GetTranscript {
                before: 0,
                limit: 1,
                forward: false,
            },
            entries: vec![Entry {
                turn: "turn".into(),
                request: "request".into(),
                role: "tool_result".into(),
                text: output.dump(),
                state: "finished".into(),
                mode: AgentMode::Basic,
                tool: Some(Arc::new(tool.clone())),
            }],
        };
        assert_eq!(documents(&page, "session").len(), 1);
        assert!(documents(&page, "another_session").is_empty());
        for name in ["read_file", "editor_open_file"] {
            let mut wrong = tool.clone();
            wrong.name = name.into();
            page.entries[0].tool = Some(Arc::new(wrong));
            assert!(documents(&page, "session").is_empty());
        }
        let mut wrong = tool.clone();
        wrong.operation = "another_operation".into();
        page.entries[0].tool = Some(Arc::new(wrong));
        assert!(documents(&page, "session").is_empty());
        let mut unknown = tool;
        unknown.phase = ToolPhase::Finished {
            execution: Execution::Failed,
            effects: Effects::Unknown,
        };
        page.entries[0].tool = Some(Arc::new(unknown));
        assert!(documents(&page, "session").is_empty());
    }
}
