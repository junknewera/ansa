//! Durable reducer commit boundary and a capacity-one owned storage worker.
use super::{
    Result,
    codec_v1::{Invocation, SavedResult},
    journal_v1::{Journal, Observer},
    need,
    state::State,
};
use crate::protocol::json::Json;
use std::{
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

pub struct Store {
    journal: Journal,
    state: Arc<State>,
    diagnostic: &'static str,
    recovered_pause: bool,
    known_outcome: Option<SavedResult>,
}
impl Store {
    pub fn open(path: &Path, state: State, observe: Option<Observer>) -> Result<Self> {
        Self::open_with_replay(path, state, observe, |_| Ok(()))
    }
    pub fn open_with_replay(
        path: &Path,
        mut state: State,
        observe: Option<Observer>,
        mut replay: impl FnMut(&Json) -> Result<()>,
    ) -> Result<Self> {
        let mut journal = Journal::open(
            path,
            |event| {
                state.apply(event)?;
                replay(event)
            },
            observe,
        )?;
        let diagnostic = journal.diagnostic();
        if diagnostic.is_empty() && journal.empty() {
            let init = state.initialization()?;
            journal.append(&init)?;
            state.apply(&init)?;
            replay(&init)?;
        }
        let recovered_pause = state
            .turn
            .as_ref()
            .is_some_and(|t| t.stage != "completed" && t.stage != "cancelled");
        Ok(Self {
            journal,
            state: Arc::new(state),
            diagnostic,
            recovered_pause,
            known_outcome: None,
        })
    }
    pub fn state(&self) -> &Arc<State> {
        &self.state
    }
    pub fn diagnostic(&self) -> &'static str {
        self.diagnostic
    }
    pub fn empty(&self) -> bool {
        self.journal.empty()
    }
    pub fn requires_resume(&self) -> bool {
        self.recovered_pause
    }
    pub fn known_outcome(&self) -> Option<&SavedResult> {
        self.known_outcome.as_ref()
    }
    pub fn commit(&mut self, event: &Json) -> Result<()> {
        need(self.diagnostic.is_empty(), "journal_read_only")?;
        let mut next = (*self.state).clone();
        next.apply(event)?;
        match self.journal.append(event) {
            Ok(()) => {
                self.state = Arc::new(next);
                if event.text_field("kind")? == "resume" {
                    self.recovered_pause = false;
                }
                Ok(())
            }
            Err(code) => {
                self.diagnostic = code;
                if event.text_field("kind")? == "outcome" {
                    self.known_outcome = Some(SavedResult::load(event.field("result")?)?);
                }
                Err(code)
            }
        }
    }
    /// Recovery consumes evidence supplied by a compiled tool's reader; it never
    /// executes an operation. Failed append retains the known result separately.
    pub fn recover(
        &mut self,
        reader: impl FnOnce(&Invocation) -> Result<Option<SavedResult>>,
    ) -> Result<()> {
        need(self.diagnostic.is_empty(), "journal_read_only")?;
        let Some(turn) = self.state.turn.as_ref() else {
            return Ok(());
        };
        let Some(intent) = &turn.intent else {
            return Ok(());
        };
        let Some(result) = reader(intent)? else {
            return Ok(());
        };
        need(result.dispatched, "session_outcome_dispatch")?;
        let event = Json::object(vec![
            ("kind", Json::string("outcome")?),
            ("turn", Json::string(&turn.receipt.turn.0)?),
            ("result", result.save()?),
        ])?;
        self.commit(&event)
    }
}
pub struct Completion {
    pub result: Result<Arc<State>>,
    pub known_outcome: Option<SavedResult>,
}
struct Work {
    event: Json,
    reply: SyncSender<Completion>,
}
pub struct Worker {
    sender: Option<SyncSender<Work>>,
    join: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(mut store: Store) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Work>(1);
        let join = thread::Builder::new()
            .name("ansa-journal".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    let result = store.commit(&work.event).map(|()| store.state.clone());
                    let _ = work.reply.send(Completion {
                        result,
                        known_outcome: store.known_outcome.clone(),
                    });
                }
            })
            .map_err(|_| "journal_worker")?;
        Ok(Self {
            sender: Some(sender),
            join: Some(join),
        })
    }
    /// Admission is nonblocking; the caller retains its own event on full queue.
    pub fn submit(&self, event: &Json) -> Result<Receiver<Completion>> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or("journal_worker_closed")?
            .try_send(Work {
                event: event.clone(),
                reply,
            })
            .map_err(|e| match e {
                TrySendError::Full(_) => "journal_queue_full",
                TrySendError::Disconnected(_) => "journal_worker_closed",
            })?;
        Ok(receiver)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
