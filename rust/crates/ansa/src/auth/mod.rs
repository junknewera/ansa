//! Own-profile auth/OIDC. Callers supply an explicit private directory and exchange.
pub mod callback;
pub mod claims;
pub mod dispatch;
pub mod manager;
pub mod pkce;
pub mod registration;
pub mod store_v1;

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub type Result<T> = std::result::Result<T, &'static str>;

/// Cancellation contains no runtime state and never reports rollback of an effect.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    wake: Condvar,
    mutex: Mutex<()>,
    parent: Option<Cancel>,
    second_parent: Option<Cancel>,
}
impl Cancel {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_all();
    }
    pub fn cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || self.0.parent.as_ref().is_some_and(Cancel::cancelled)
            || self.0.second_parent.as_ref().is_some_and(Cancel::cancelled)
    }
    pub(crate) fn linked(parent: &Cancel) -> Self {
        Self(Arc::new(CancelState {
            parent: Some(parent.clone()),
            ..CancelState::default()
        }))
    }
    pub(crate) fn combined(first: &Cancel, second: &Cancel) -> Self {
        Self(Arc::new(CancelState {
            parent: Some(first.clone()),
            second_parent: Some(second.clone()),
            ..CancelState::default()
        }))
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled() {
            Err("auth_cancelled")
        } else {
            Ok(())
        }
    }
    pub(crate) fn wait(&self, duration: Duration) {
        let lock = self.0.mutex.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .0
            .wake
            .wait_timeout_while(lock, duration, |_| !self.cancelled());
    }
}

/// Debug intentionally excludes bearer, code, refresh and ID token contents.
#[derive(Clone)]
pub struct Secret(String);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}
pub(crate) fn number(value: u64) -> crate::protocol::json::Json {
    crate::protocol::json::Json::parse(&value.to_string()).expect("u64 is bounded JSON")
}
pub(crate) fn boolean(value: bool) -> crate::protocol::json::Json {
    crate::protocol::json::Json::parse(if value { "true" } else { "false" }).expect("boolean JSON")
}
