//! Bounded nonblocking publisher; overflow advertises a cursor gap.
use crate::frontend::protocol_v1::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;
struct Queue {
    events: VecDeque<RuntimeEvent>,
    sequence: u64,
    closed: bool,
}
pub(crate) struct Outbox {
    queue: Mutex<Queue>,
    wake: Notify,
    capacity: usize,
}
impl Outbox {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue {
                events: VecDeque::new(),
                sequence: 0,
                closed: false,
            }),
            wake: Notify::new(),
            capacity,
        })
    }
    pub fn publish(&self, mut event: RuntimeEvent) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.events.len() == self.capacity {
            queue.events.clear();
            queue.sequence += 1;
            let mut marker = event.clone();
            marker.stream_sequence = queue.sequence;
            marker.payload = EventPayload::ResyncRequired {
                oldest_available: queue.sequence + 1,
            };
            queue.events.push_back(marker);
        }
        queue.sequence += 1;
        event.stream_sequence = queue.sequence;
        if let EventPayload::FrontendSnapshot(snapshot) = &mut event.payload {
            snapshot.through_sequence = queue.sequence;
        }
        queue.events.push_back(event);
        drop(queue);
        self.wake.notify_one();
    }
    pub fn sequence(&self) -> u64 {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sequence
    }
    pub async fn next(&self) -> Option<RuntimeEvent> {
        loop {
            let notified = self.wake.notified();
            {
                let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(event) = queue.events.pop_front() {
                    return Some(event);
                }
                if queue.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
    pub fn close(&self) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.wake.notify_waiters();
    }
}
