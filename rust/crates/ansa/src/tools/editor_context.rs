use crate::{
    config::{n, s},
    protocol::json::Json,
    provider::contracts::{AgentMode, valid_id},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub type Result<T> = std::result::Result<T, &'static str>;
pub const CONTEXT_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct Context {
    pub client: String,
    pub incarnation: String,
    pub buffer: usize,
    pub changedtick: usize,
    pub sequence: usize,
    pub path: Option<PathBuf>,
    pub filetype: String,
    pub first_line: usize,
    pub text: String,
}
impl Context {
    pub fn json(&self) -> Result<Json> {
        Json::object(vec![
            ("client", s(&self.client)?),
            ("editor_incarnation", s(&self.incarnation)?),
            ("buffer", n(self.buffer)),
            ("changedtick", n(self.changedtick)),
            ("sequence", n(self.sequence)),
            ("filetype", s(&self.filetype)?),
            (
                "path",
                self.path.as_ref().map_or_else(
                    || Json::parse("null"),
                    |p| s(p.to_str().ok_or("editor_path")?),
                )?,
            ),
            ("first_line", n(self.first_line)),
            ("text", s(&self.text)?),
        ])
    }
}

struct State {
    contexts: BTreeMap<String, Context>,
    versions: BTreeMap<String, (String, usize)>,
    connected: BTreeSet<String>,
    mode: AgentMode,
}
/// A bounded ephemeral projection of explicitly shared buffers. It is not an
/// editor backup or a second conversation journal.
pub struct EditorHub {
    cwd: PathBuf,
    protected: Vec<PathBuf>,
    state: Mutex<State>,
}
impl EditorHub {
    pub fn new(cwd: PathBuf, protected: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            cwd,
            protected,
            state: Mutex::new(State {
                contexts: BTreeMap::new(),
                versions: BTreeMap::new(),
                connected: BTreeSet::new(),
                mode: AgentMode::Basic,
            }),
        })
    }
    pub fn attach(self: &Arc<Self>, client: &str) -> Result<EditorAttachment> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !valid_id(client) || state.connected.len() >= 8 || !state.connected.insert(client.into())
        {
            return Err("editor_attachment_conflict");
        }
        Ok(EditorAttachment {
            hub: self.clone(),
            client: client.into(),
        })
    }
    pub fn mode(&self) -> AgentMode {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).mode
    }
    pub fn set_mode(&self, mode: AgentMode) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).mode = mode;
    }
    pub fn list(&self) -> Result<Json> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Json::array(
            state
                .contexts
                .values()
                .map(|c| {
                    Json::object(vec![
                        ("client", s(&c.client)?),
                        ("editor_incarnation", s(&c.incarnation)?),
                        ("buffer", n(c.buffer)),
                        ("changedtick", n(c.changedtick)),
                        ("sequence", n(c.sequence)),
                    ])
                })
                .collect::<Result<_>>()?,
        )
    }
    fn path(&self, text: &str) -> Result<Option<PathBuf>> {
        if text.is_empty() {
            return Ok(None);
        }
        if text.len() > 4096 || text.contains('\0') {
            return Err("editor_path");
        }
        let path = Path::new(text);
        let path = ansa_linux::fs::normalize(&if path.is_absolute() {
            path.to_owned()
        } else {
            self.cwd.join(path)
        });
        if !path.starts_with(&self.cwd)
            || ansa_linux::fs::protected_path(&path)
            || self.protected.iter().any(|p| path.starts_with(p))
        {
            return Err("protected_editor_path");
        }
        Ok(Some(path))
    }
    pub fn publish(&self, client: &str, value: &Json) -> Result<Context> {
        let incarnation = value.text_field("editor_incarnation")?;
        let text = value.text_field("text")?;
        let filetype = value.text_field("filetype")?;
        if !valid_id(client)
            || !valid_id(incarnation)
            || text.len() > CONTEXT_BYTES
            || text.contains('\0')
            || filetype.len() > 64
            || filetype.chars().any(char::is_control)
            || value.usize_field("buffer")? == 0
            || value.usize_field("first_line")? == 0
            || value.usize_field("sequence")? == 0
        {
            return Err("editor_context_limit");
        }
        let context = Context {
            client: client.into(),
            incarnation: incarnation.into(),
            buffer: value.usize_field("buffer")?,
            changedtick: value.usize_field("changedtick")?,
            sequence: value.usize_field("sequence")?,
            path: self.path(value.text_field("path")?)?,
            filetype: filetype.into(),
            first_line: value.usize_field("first_line")?,
            text: text.into(),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.connected.contains(client) {
            return Err("editor_unavailable");
        }
        if state.contexts.len() >= 8 && !state.contexts.contains_key(client) {
            return Err("editor_clients_full");
        }
        if let Some((incarnation, sequence)) = state.versions.get(client) {
            if incarnation != &context.incarnation {
                return Err("stale_editor_incarnation");
            }
            if context.sequence <= *sequence {
                return Err("stale_editor_context");
            }
        }
        if state.contexts.get(client).is_some_and(|old| {
            old.buffer == context.buffer && context.changedtick < old.changedtick
        }) {
            return Err("stale_editor_context");
        }
        state.versions.insert(
            client.into(),
            (context.incarnation.clone(), context.sequence),
        );
        state.contexts.insert(client.into(), context.clone());
        Ok(context)
    }
    pub fn get(&self, args: &Json) -> Result<Context> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let context = state
            .contexts
            .get(args.text_field("client")?)
            .ok_or("editor_unavailable")?;
        if context.incarnation != args.text_field("editor_incarnation")?
            || context.buffer != args.usize_field("buffer")?
            || context.changedtick != args.usize_field("changedtick")?
            || context.sequence != args.usize_field("sequence")?
        {
            return Err("stale_editor_context");
        }
        Ok(context.clone())
    }
    pub fn invalidate(
        &self,
        client: &str,
        incarnation: &str,
        buffer: usize,
        sequence: usize,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.connected.contains(client) {
            return Err("editor_unavailable");
        }
        if let Some(context) = state.contexts.get(client) {
            if context.incarnation != incarnation {
                return Err("stale_editor_incarnation");
            }
            if context.buffer == buffer && context.sequence == sequence {
                state.contexts.remove(client);
            }
        }
        Ok(())
    }
}
pub struct EditorAttachment {
    hub: Arc<EditorHub>,
    client: String,
}
impl Drop for EditorAttachment {
    fn drop(&mut self) {
        let mut state = self.hub.state.lock().unwrap_or_else(|e| e.into_inner());
        state.contexts.remove(&self.client);
        state.versions.remove(&self.client);
        state.connected.remove(&self.client);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(sequence: usize, tick: usize) -> Json {
        Json::object(vec![
            ("editor_incarnation", s("editor1").unwrap()),
            ("buffer", n(1)),
            ("changedtick", n(tick)),
            ("sequence", n(sequence)),
            ("path", s("src/file.rs").unwrap()),
            ("filetype", s("rust").unwrap()),
            ("first_line", n(4)),
            ("text", s("unsaved Ж界").unwrap()),
        ])
        .unwrap()
    }
    fn query(sequence: usize, tick: usize) -> Json {
        Json::object(vec![
            ("client", s("client1").unwrap()),
            ("editor_incarnation", s("editor1").unwrap()),
            ("buffer", n(1)),
            ("changedtick", n(tick)),
            ("sequence", n(sequence)),
        ])
        .unwrap()
    }
    #[test]
    fn exact_unsaved_context_expires_on_edit_disconnect_and_editor_restart() {
        let hub = EditorHub::new("/project".into(), vec!["/project/private".into()]);
        let lease = hub.attach("client1").unwrap();
        assert!(hub.attach("client1").is_err());
        hub.publish("client1", &context(1, 2)).unwrap();
        assert_eq!(hub.get(&query(1, 2)).unwrap().text, "unsaved Ж界");
        assert!(hub.get(&query(2, 2)).is_err());
        hub.invalidate("client1", "editor1", 1, 1).unwrap();
        assert!(hub.get(&query(1, 2)).is_err());
        assert!(hub.publish("client1", &context(1, 2)).is_err());
        hub.publish("client1", &context(2, 3)).unwrap();
        drop(lease);
        assert!(hub.get(&query(2, 3)).is_err());
        let _lease = hub.attach("client1").unwrap();
        let next = context(1, 1)
            .with("editor_incarnation", s("editor2").unwrap())
            .unwrap();
        hub.publish("client1", &next).unwrap();
        assert!(hub.get(&query(2, 3)).is_err());
    }
    #[test]
    fn editor_scope_and_size_limits_fail_closed_without_silently_truncating() {
        let hub = EditorHub::new("/project".into(), vec!["/project/private".into()]);
        let _lease = hub.attach("client1").unwrap();
        for path in [
            "../outside",
            "/outside",
            "private/credentials",
            ".claude/config",
        ] {
            let value = context(1, 2).with("path", s(path).unwrap()).unwrap();
            assert!(hub.publish("client1", &value).is_err(), "{path}");
        }
        let huge = context(1, 2)
            .with("text", s(&"x".repeat(CONTEXT_BYTES + 1)).unwrap())
            .unwrap();
        assert!(hub.publish("client1", &huge).is_err());
        assert_eq!(hub.list().unwrap().elements().unwrap().len(), 0);
        hub.set_mode(AgentMode::Plan);
        assert_eq!(hub.mode(), AgentMode::Plan);
    }
}
