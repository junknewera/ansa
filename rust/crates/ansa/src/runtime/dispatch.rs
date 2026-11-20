//! The single linearization point for external dispatch and accepted Cancel.
use super::{Result, work::Identity};
use crate::frontend::protocol_v1::Started;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
struct State {
    identity: Option<Identity>,
    cancelled: bool,
    unused: bool,
}
pub(crate) struct Gate(Mutex<State>);
impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(State {
            identity: None,
            cancelled: false,
            unused: false,
        })))
    }
    pub fn reset(&self) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.identity = None;
        s.cancelled = false;
        s.unused = false;
    }
    pub fn allow(&self, identity: &Identity) -> Result<()> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.cancelled {
            return Err("dispatch_cancelled");
        }
        s.identity = Some(identity.clone());
        s.unused = true;
        Ok(())
    }
    pub fn active(&self, identity: &Identity) -> bool {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !s.cancelled && s.identity.as_ref() == Some(identity)
    }
    pub fn begin(
        &self,
        identity: &Identity,
        started: Option<(&mpsc::Sender<(Identity, Started)>, Started)>,
    ) -> bool {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.cancelled || !s.unused || s.identity.as_ref() != Some(identity) {
            return false;
        }
        if let Some((sender, value)) = started
            && sender.try_send((identity.clone(), value)).is_err()
        {
            return false;
        }
        s.unused = false;
        true
    }
    pub fn cancel(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).cancelled = true;
    }
}
/// An intent can only be authorized by a successful private storage completion.
/// ```compile_fail,E0451
/// use ansa::runtime::DurableIntent;
/// let forged = DurableIntent { identity: todo!(), invocation: todo!(), durable: true };
/// ```
/// ```compile_fail,E0277
/// use ansa::runtime::DurableIntent;
/// fn needs_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// needs_deserialize::<DurableIntent>();
/// ```
pub struct DurableIntent {
    pub(super) identity: Identity,
    pub(super) invocation: crate::tools::invocation::Invocation,
    pub(super) durable: bool,
}
