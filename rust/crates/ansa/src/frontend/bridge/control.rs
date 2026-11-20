//! Editor-origin messages use the production command IDs and admission path.
use super::*;
use crate::{protocol::legacy_json_v1::sha256, runtime::PostStatus};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct ReceiptReply {
    command: CommandId,
    receipt: Option<MessageReceiptInfo>,
    ready: bool,
    storage_failed: bool,
    recovery_required: bool,
}
#[derive(Default)]
struct State {
    ready: bool,
    proofs: BTreeMap<String, (TurnId, RequestId)>,
    reads: BTreeMap<String, oneshot::Sender<ReceiptReply>>,
}
pub(super) struct Control {
    handle: Handle,
    editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    state: Mutex<State>,
}
struct Waiting {
    control: Arc<Control>,
    query: String,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        self.control
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reads
            .remove(&self.query);
    }
}
pub(super) type Job = std::pin::Pin<Box<dyn std::future::Future<Output = Arc<String>> + Send>>;

fn native_id(client: &str, id: &str) -> Result<CommandId> {
    if !crate::provider::contracts::valid_id(client)
        || !crate::provider::contracts::valid_id(id)
        || id.len() > 32
    {
        return Err("workspace_message_id");
    }
    Ok(CommandId(format!(
        "n_{}_{}",
        &sha256(client.as_bytes())[..16],
        id
    )))
}
fn reply_error(id: &str, code: &str) -> Arc<String> {
    encoded(&obj(vec![
        ("type", s("error").unwrap()),
        ("id", s(id).unwrap()),
        ("code", s(code).unwrap()),
    ]))
    .unwrap()
}
fn info_json(receipt: &MessageReceiptInfo) -> Json {
    obj(vec![
        (
            "kind",
            s(match receipt.kind {
                MessageKind::Submit => "submit",
                MessageKind::Steer => "steer",
            })
            .unwrap(),
        ),
        ("turn", s(&receipt.turn.0).unwrap()),
        (
            "request",
            optional(receipt.request.as_ref().map(|r| r.0.as_str())),
        ),
        ("state", s(&receipt.state).unwrap()),
        (
            "mode",
            receipt.mode.map_or_else(
                || Json::parse("null").unwrap(),
                |m| {
                    s(if m == AgentMode::Plan {
                        "plan"
                    } else {
                        "basic"
                    })
                    .unwrap()
                },
            ),
        ),
        ("admission_durable", flag(receipt.admission_durable)),
        ("text_sha256", s(&receipt.text_sha256).unwrap()),
        ("state_durable", flag(receipt.state_durable)),
    ])
}
impl Control {
    pub fn new(
        handle: Handle,
        editor: Option<Arc<crate::tools::editor_context::EditorHub>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle,
            editor,
            state: Mutex::new(State::default()),
        })
    }
    fn prove(state: &mut State, command: &CommandId, turn: &TurnId, request: &RequestId) {
        if command.0.starts_with("n_")
            && (state.proofs.len() < 4096 || state.proofs.contains_key(&command.0))
        {
            state
                .proofs
                .insert(command.0.clone(), (turn.clone(), request.clone()));
        }
    }
    pub fn publish(&self, event: &RuntimeEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &event.payload {
            EventPayload::FrontendSnapshot(snapshot) => {
                state.ready = snapshot.state != "loading";
                if let (Some(command), Some(request)) = (&snapshot.submit, &snapshot.request) {
                    Self::prove(&mut state, command, &event.turn, request);
                }
            }
            EventPayload::CommandAccepted {
                command,
                request: Some(request),
            } => Self::prove(&mut state, command, &event.turn, request),
            EventPayload::MessageReceipt {
                query,
                command,
                receipt,
                ready,
                storage_failed,
                recovery_required,
            } => {
                if let Some(receipt) = receipt
                    && receipt.kind == MessageKind::Submit
                    && let Some(request) = &receipt.request
                {
                    Self::prove(&mut state, command, &receipt.turn, request);
                }
                if let Some(sender) = state.reads.remove(&query.0) {
                    let _ = sender.send(ReceiptReply {
                        command: command.clone(),
                        receipt: receipt.as_deref().cloned(),
                        ready: *ready,
                        storage_failed: *storage_failed,
                        recovery_required: *recovery_required,
                    });
                }
            }
            _ => {}
        }
    }
    fn command(&self, id: CommandId, turn: TurnId, payload: CommandPayload) -> RuntimeCommand {
        RuntimeCommand {
            schema_version: 1,
            session: self.handle.session().clone(),
            incarnation: self.handle.incarnation().clone(),
            command: id,
            turn,
            payload,
        }
    }
    fn prepare(&self, client: &str, value: &Json) -> Result<RuntimeCommand> {
        let allowed = [
            "version",
            "session",
            "incarnation",
            "id",
            "kind",
            "text",
            "turn",
            "target",
            "expected_mode",
            "message",
        ];
        if value.keys()?.iter().any(|key| !allowed.contains(key)) {
            return Err("workspace_command_fields");
        }
        if !self.state.lock().unwrap_or_else(|e| e.into_inner()).ready {
            return Err("workspace_loading");
        }
        let id = native_id(client, value.text_field("id")?)?;
        let turn = value.text_field("turn")?;
        if !crate::provider::contracts::valid_id(turn) {
            return Err("workspace_turn");
        }
        if value.text_field("kind")? == "cancel_message" {
            let message = native_id(client, value.text_field("message")?)?;
            let target = value.text_field("target")?;
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .proofs
                .get(&message.0)
                .is_none_or(|(known_turn, known_request)| {
                    known_turn.0 != turn || known_request.0 != target
                })
            {
                return Err("workspace_target_unconfirmed");
            }
            return Ok(self.command(id, turn.into(), CommandPayload::Cancel(target.into())));
        }
        let text = value.text_field("text")?;
        if text.trim().is_empty() || text.contains('\0') {
            return Err("submit_size");
        }
        let target = value.field("target")?;
        if !target.is_null() {
            let target = target.text()?;
            if !crate::provider::contracts::valid_id(target) {
                return Err("request_id");
            }
            return Ok(self.command(
                id,
                turn.into(),
                CommandPayload::Steer(Steer {
                    target: target.into(),
                    text: text.into(),
                }),
            ));
        }
        if turn != value.text_field("id")? {
            return Err("workspace_turn");
        }
        let mode = self
            .editor
            .as_ref()
            .ok_or("workspace_permission_denied")?
            .mode();
        if value.text_field("expected_mode")?
            != if mode == AgentMode::Plan {
                "plan"
            } else {
                "basic"
            }
        {
            return Err("workspace_mode_changed");
        }
        let turn = TurnId(id.0.clone());
        Ok(self.command(
            id,
            turn,
            CommandPayload::Submit(Submit {
                text: text.into(),
                mode,
            }),
        ))
    }
    pub fn send(self: &Arc<Self>, client: &str, value: &Json) -> Job {
        let id = value.text_field("id").unwrap_or("invalid").to_owned();
        let command = self.prepare(client, value);
        let control = self.clone();
        Box::pin(async move {
            let command = match command {
                Ok(c) => c,
                Err(code) => return reply_error(&id, code),
            };
            let command_id = command.command.clone();
            let result = match tokio::time::timeout(IO_TIMEOUT, control.handle.post(command)).await
            {
                Ok(Ok(status)) => format!("{status:?}"),
                _ => "Unknown".into(),
            };
            encoded(&obj(vec![
                ("type", s("message_status").unwrap()),
                ("id", s(&id).unwrap()),
                ("command", s(&command_id.0).unwrap()),
                ("status", s(&result).unwrap()),
            ]))
            .unwrap()
        })
    }
    async fn lookup(
        self: &Arc<Self>,
        client: &str,
        message: &str,
        query: &str,
    ) -> Result<ReceiptReply> {
        let command = native_id(client, message)?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.reads.len() >= CLIENTS || state.reads.contains_key(query) {
                return Err("workspace_reads_full");
            }
            state.reads.insert(query.into(), sender);
        }
        let _waiting = Waiting {
            control: self.clone(),
            query: query.into(),
        };
        let status = tokio::time::timeout(
            IO_TIMEOUT,
            self.handle.post(self.command(
                query.into(),
                "workspace_lookup".into(),
                CommandPayload::GetMessageReceipt(command),
            )),
        )
        .await
        .map_err(|_| "workspace_lookup_timeout")??;
        if status != PostStatus::Accepted {
            return Err("workspace_read_busy");
        }
        tokio::time::timeout(IO_TIMEOUT, receiver)
            .await
            .map_err(|_| "workspace_lookup_timeout")?
            .map_err(|_| "workspace_closed")
    }
    pub fn read(self: &Arc<Self>, client: &str, value: &Json, query: String) -> Job {
        let control = self.clone();
        let id = value.text_field("id").unwrap_or("invalid").to_owned();
        let message = value.text_field("message").map(str::to_owned);
        let client = client.to_owned();
        Box::pin(async move {
            let message = match message {
                Ok(m) => m,
                Err(code) => return reply_error(&id, code),
            };
            match control.lookup(&client, &message, &query).await {
                Ok(reply) => encoded(&obj(vec![
                    ("type", s("message_receipt").unwrap()),
                    ("id", s(&id).unwrap()),
                    ("message", s(&message).unwrap()),
                    ("session", s(&control.handle.session().0).unwrap()),
                    ("incarnation", s(&control.handle.incarnation().0).unwrap()),
                    ("command", s(&reply.command.0).unwrap()),
                    ("ready", flag(reply.ready)),
                    ("storage_failed", flag(reply.storage_failed)),
                    ("recovery_required", flag(reply.recovery_required)),
                    (
                        "receipt",
                        reply
                            .receipt
                            .as_ref()
                            .map_or_else(|| Json::parse("null").unwrap(), info_json),
                    ),
                ]))
                .unwrap(),
                Err(code) => reply_error(&id, code),
            }
        })
    }
}
