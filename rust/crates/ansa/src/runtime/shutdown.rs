//! Public handles own the actor thread; dropping a future never detaches I/O.
use super::{dispatch::Gate, outbox::Outbox, owner::Owner, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, JoinHandle},
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
pub(crate) struct Envelope {
    pub command: RuntimeCommand,
    pub reply: oneshot::Sender<Result<PostStatus>>,
}
pub(crate) struct Mailboxes {
    pub submits: mpsc::Receiver<Envelope>,
    pub cancel: mpsc::Receiver<Envelope>,
    pub snapshot: mpsc::Receiver<Envelope>,
    pub resume: mpsc::Receiver<Envelope>,
    pub transcript: mpsc::Receiver<Envelope>,
}
pub(crate) struct Closing {
    pub mode: AtomicU8,
    pub wake: Notify,
}
#[derive(Clone)]
pub struct Handle {
    pub(crate) session: SessionId,
    pub(crate) incarnation: RuntimeIncarnation,
    submits: mpsc::Sender<Envelope>,
    cancel: mpsc::Sender<Envelope>,
    snapshot: mpsc::Sender<Envelope>,
    resume: mpsc::Sender<Envelope>,
    transcript: mpsc::Sender<Envelope>,
    pub(crate) closing: Arc<Closing>,
    pub(crate) outbox: Arc<Outbox>,
    pub(crate) gate: Arc<Gate>,
    ready: watch::Receiver<Option<Result<()>>>,
}
impl Handle {
    pub fn incarnation(&self) -> &RuntimeIncarnation {
        &self.incarnation
    }
    pub fn session(&self) -> &SessionId {
        &self.session
    }
    pub async fn ready(&self) -> Result<()> {
        let mut receiver = self.ready.clone();
        loop {
            if let Some(result) = *receiver.borrow_and_update() {
                return result;
            }
            receiver.changed().await.map_err(|_| "runtime_closed")?;
        }
    }
    pub async fn post(&self, command: RuntimeCommand) -> Result<PostStatus> {
        validate_command(&command)?;
        if command.incarnation != self.incarnation {
            return Ok(PostStatus::StaleIncarnation);
        }
        if command.session != self.session {
            return Err("unknown_session");
        }
        if self.closing.mode.load(Ordering::Acquire) != 0 {
            return Ok(PostStatus::Closed);
        }
        let sender = match &command.payload {
            CommandPayload::Cancel(_) => &self.cancel,
            CommandPayload::GetSnapshot(_) => &self.snapshot,
            CommandPayload::Resume(_) => &self.resume,
            CommandPayload::GetTranscript(_) => &self.transcript,
            CommandPayload::Submit(_) => &self.submits,
        };
        let (reply, receiver) = oneshot::channel();
        match sender.try_send(Envelope { command, reply }) {
            Ok(()) => receiver.await.unwrap_or(Ok(PostStatus::Closed)),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(PostStatus::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Ok(PostStatus::Closed),
        }
    }
    pub async fn next_event(&self) -> Option<RuntimeEvent> {
        self.outbox.next().await
    }
    pub fn close_input(&self) {
        let _ = self
            .closing
            .mode
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        self.closing.wake.notify_one();
    }
    pub fn request_shutdown(&self) {
        self.closing.mode.store(2, Ordering::Release);
        self.gate.cancel();
        self.closing.wake.notify_one();
    }
}
pub struct Runtime {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
    finished: watch::Receiver<bool>,
}
impl Runtime {
    /// Starts the owner without blocking the caller on journal/auth I/O.
    /// Wait on ready() before normal submissions; Snapshot can show loading.
    pub fn new(
        session: SessionId,
        configuration: Request,
        tools: Arc<ToolSnapshot>,
        generate: Generate,
        limits: Limits,
        options: Options,
    ) -> Result<Self> {
        valid_configuration(&session, &configuration, &tools, limits, &options)?;
        use ring::rand::{SecureRandom, SystemRandom};
        let mut random = [0; 16];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| "incarnation_generation_failed")?;
        let incarnation = RuntimeIncarnation(random.iter().map(|b| format!("{b:02x}")).collect());
        let (submits, submit_rx) = mpsc::channel(limits.commands);
        let (cancel, cancel_rx) = mpsc::channel(1);
        let (snapshot, snapshot_rx) = mpsc::channel(1);
        let (resume, resume_rx) = mpsc::channel(1);
        let (transcript, transcript_rx) = mpsc::channel(1);
        let closing = Arc::new(Closing {
            mode: AtomicU8::new(0),
            wake: Notify::new(),
        });
        let outbox = Outbox::new(limits.events);
        let gate = Gate::new();
        let (ready_sender, ready) = watch::channel(None);
        let (finish_sender, finished) = watch::channel(false);
        let handle = Handle {
            session: session.clone(),
            incarnation: incarnation.clone(),
            submits,
            cancel,
            snapshot,
            resume,
            transcript,
            closing: closing.clone(),
            outbox: outbox.clone(),
            gate: gate.clone(),
            ready,
        };
        let thread = thread::Builder::new()
            .name("ansa-owner".into())
            .spawn(move || {
                let execution =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let reactor = tokio::runtime::Builder::new_current_thread()
                            .enable_time()
                            .build()
                            .map_err(|_| "runtime_reactor")?;
                        let (done, completions) = mpsc::channel(8);
                        let (start, starts) = mpsc::channel(1);
                        let (preview_sender, previews) = watch::channel(None);
                        let (boot, ready) = oneshot::channel();
                        let plan_tools = tools.read_only();
                        let state = crate::session::state::State::new(
                            session,
                            configuration,
                            plan_tools.advertisement().clone(),
                        );
                        let workers = super::work::Workers::start(
                            state.clone(),
                            options.clone(),
                            generate,
                            gate.clone(),
                            done,
                            start,
                            preview_sender,
                            boot,
                        )?;
                        let owner = Owner::new(
                            state,
                            tools,
                            plan_tools,
                            limits,
                            options,
                            incarnation,
                            closing.clone(),
                            outbox.clone(),
                            gate.clone(),
                            Mailboxes {
                                submits: submit_rx,
                                cancel: cancel_rx,
                                snapshot: snapshot_rx,
                                resume: resume_rx,
                                transcript: transcript_rx,
                            },
                            completions,
                            starts,
                            previews,
                        );
                        let result = reactor.block_on(owner.run(ready, &workers, &ready_sender));
                        drop(workers);
                        result
                    }));
                gate.cancel();
                closing.mode.store(2, Ordering::Release);
                if !matches!(execution, Ok(Ok(()))) && ready_sender.borrow().is_none() {
                    ready_sender.send_replace(Some(Err("runtime_failure")));
                }
                outbox.close();
                finish_sender.send_replace(true);
            })
            .map_err(|_| "runtime_thread")?;
        Ok(Self {
            handle,
            thread: Some(thread),
            finished,
        })
    }
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
    pub async fn ready(&self) -> Result<()> {
        self.handle.ready().await
    }
    pub async fn post(&self, command: RuntimeCommand) -> Result<PostStatus> {
        self.handle.post(command).await
    }
    pub async fn next_event(&self) -> Option<RuntimeEvent> {
        self.handle.next_event().await
    }
    pub fn close_input(&self) {
        self.handle.close_input();
    }
    pub async fn join(&mut self) -> Result<()> {
        while !*self.finished.borrow_and_update() {
            self.finished.changed().await.map_err(|_| "runtime_join")?;
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "runtime_join")?;
        }
        Ok(())
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.handle.request_shutdown();
        self.join().await
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, task::Poll};

    #[tokio::test(flavor = "current_thread")]
    async fn full_cancel_slot_never_acknowledges_an_unaccepted_request() {
        let (submits, _) = mpsc::channel(1);
        let (cancel, mut receiver) = mpsc::channel(1);
        let (snapshot, _) = mpsc::channel(1);
        let (resume, _) = mpsc::channel(1);
        let (transcript, _) = mpsc::channel(1);
        let (_, ready) = watch::channel(Some(Ok(())));
        let handle = Handle {
            session: "session".into(),
            incarnation: "incarnation".into(),
            submits,
            cancel,
            snapshot,
            resume,
            transcript,
            closing: Arc::new(Closing {
                mode: AtomicU8::new(0),
                wake: Notify::new(),
            }),
            outbox: Outbox::new(2),
            gate: Gate::new(),
            ready,
        };
        let command = |id: &str, target: &str| RuntimeCommand {
            schema_version: 1,
            session: "session".into(),
            turn: "turn".into(),
            incarnation: "incarnation".into(),
            command: id.into(),
            payload: CommandPayload::Cancel(target.into()),
        };
        let mut stale = Box::pin(handle.post(command("stale", "old")));
        std::future::poll_fn(|context| {
            assert!(matches!(stale.as_mut().poll(context), Poll::Pending));
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            handle.post(command("valid", "active")).await,
            Ok(PostStatus::QueueFull)
        );
        let queued = receiver.try_recv().unwrap();
        assert_eq!(queued.command.command.0, "stale");
        queued.reply.send(Ok(PostStatus::StaleTarget)).unwrap();
        assert_eq!(stale.await, Ok(PostStatus::StaleTarget));
        let mut valid = Box::pin(handle.post(command("valid", "active")));
        std::future::poll_fn(|context| {
            assert!(matches!(valid.as_mut().poll(context), Poll::Pending));
            Poll::Ready(())
        })
        .await;
        let accepted = receiver.try_recv().unwrap();
        assert_eq!(accepted.command.command.0, "valid");
        accepted.reply.send(Ok(PostStatus::Accepted)).unwrap();
        assert_eq!(valid.await, Ok(PostStatus::Accepted));
    }
}
