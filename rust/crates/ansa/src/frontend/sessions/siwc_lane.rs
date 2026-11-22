use crate::auth::Cancel;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

/// The SIWC store lease spans a model response or a tool execution. Wait
/// outside the store so an overlapping workspace request cannot time out
/// behind an operation owned by this process.
#[derive(Clone, Default)]
pub(in crate::frontend) struct SiwcLane(Arc<(Mutex<State>, Condvar)>);

#[derive(Default)]
struct State {
    occupied: bool,
    foreground: usize,
}

pub(in crate::frontend) struct Permit(SiwcLane);
pub(in crate::frontend) struct Priority(SiwcLane);

impl SiwcLane {
    pub(in crate::frontend) fn prioritize(&self) -> Priority {
        let (state, _) = &*self.0;
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .foreground += 1;
        Priority(self.clone())
    }

    pub(in crate::frontend) fn enter(
        &self,
        foreground: bool,
        stop: &Cancel,
    ) -> Result<Permit, &'static str> {
        let (state, wake) = &*self.0;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.foreground += usize::from(foreground);
        while !stop.cancelled() && (state.occupied || (!foreground && state.foreground != 0)) {
            (state, _) = wake
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|error| error.into_inner());
        }
        state.foreground -= usize::from(foreground);
        if stop.cancelled() {
            wake.notify_all();
            return Err("auth_cancelled");
        }
        state.occupied = true;
        Ok(Permit(self.clone()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let (state, wake) = &*self.0.0;
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .occupied = false;
        wake.notify_all();
    }
}

impl Drop for Priority {
    fn drop(&mut self) {
        let (state, wake) = &*self.0.0;
        state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .foreground -= 1;
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Instant};

    #[test]
    fn queued_foreground_precedes_background_and_can_cancel() {
        let lane = SiwcLane::default();
        let permit = lane.enter(false, &Cancel::default()).unwrap();
        let (order, observed) = mpsc::channel();
        let waiting_stop = Cancel::default();
        thread::scope(|scope| {
            let waiting_lane = lane.clone();
            let stopped = waiting_stop.clone();
            let waiting = scope.spawn(move || waiting_lane.enter(true, &stopped).err());
            wait_foreground(&lane);
            waiting_stop.cancel();
            assert_eq!(waiting.join().unwrap(), Some("auth_cancelled"));

            let next = lane.clone();
            let background = order.clone();
            scope.spawn(move || {
                let _hold = next.enter(false, &Cancel::default()).unwrap();
                background.send("background").unwrap();
            });
            let next = lane.clone();
            scope.spawn(move || {
                let _hold = next.enter(true, &Cancel::default()).unwrap();
                order.send("foreground").unwrap();
            });
            wait_foreground(&lane);
            drop(permit);
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(5)).unwrap(),
                "foreground"
            );
            assert_eq!(
                observed.recv_timeout(Duration::from_secs(5)).unwrap(),
                "background"
            );
        });
    }

    fn wait_foreground(lane: &SiwcLane) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while lane.0.0.lock().unwrap().foreground == 0 {
            assert!(Instant::now() < deadline, "no foreground waiter");
            thread::yield_now();
        }
    }
}
