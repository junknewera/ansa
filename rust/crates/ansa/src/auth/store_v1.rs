use super::{Result, pkce::random_token};
use crate::protocol::json::Json;
use ansa_linux::auth_store::Directory;
pub use ansa_linux::auth_store::{Lock, Point};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorePoint {
    TemporarySynced,
    Replaced,
    BeforeDispatch,
    TokensRemoved,
}
pub type Observer = Arc<dyn Fn(StorePoint, &str) -> Result<()> + Send + Sync>;
#[derive(Clone)]
pub struct Store {
    pub(crate) directory: Directory,
    observer: Option<Observer>,
}
impl Store {
    pub fn open(path: &Path, observer: Option<Observer>) -> Result<Self> {
        Ok(Self {
            directory: Directory::open(path).map_err(|_| "auth_directory_permissions")?,
            observer,
        })
    }
    pub(crate) fn lock(&self, cancel: &super::Cancel) -> Result<Lock> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cancel.check()?;
            match self.directory.try_lock() {
                Ok(lock) => return Ok(lock),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err("auth_lock_permissions"),
            }
            if Instant::now() >= deadline {
                return Err("auth_busy");
            }
            cancel.wait(Duration::from_millis(25));
        }
    }
    pub(crate) fn read(&self, name: &str) -> Result<Option<Json>> {
        self.directory
            .read(name)
            .map_err(|_| "auth_store_permissions")?
            .map(|bytes| {
                Json::parse_with_limits(
                    std::str::from_utf8(&bytes).map_err(|_| "auth_store_read")?,
                    256 * 1024,
                    64,
                    131072,
                )
            })
            .transpose()
    }
    pub(crate) fn save(&self, name: &str, value: &Json) -> Result<()> {
        self.directory
            .save(
                name,
                value.dump().as_bytes(),
                &random_token(16)?,
                |point, name| {
                    self.observe(
                        match point {
                            Point::TemporarySynced => StorePoint::TemporarySynced,
                            Point::Replaced => StorePoint::Replaced,
                        },
                        name,
                    )
                    .map_err(std::io::Error::other)
                },
            )
            .map_err(|_| "auth_store_write")
    }
    pub(crate) fn observe(&self, point: StorePoint, name: &str) -> Result<()> {
        if let Some(observer) = &self.observer {
            observer(point, name)?;
        }
        Ok(())
    }
    pub(crate) fn clean(&self) -> Result<()> {
        self.directory
            .clean_pending()
            .map_err(|_| "auth_pending_permissions")
    }
    pub(crate) fn signed_out(&self) -> Result<bool> {
        self.directory.signed_out().map_err(|_| "auth_logout_state")
    }
    pub(crate) fn request_logout(&self) -> Result<()> {
        self.directory
            .request_logout()
            .map_err(|_| "auth_logout_state")
    }
    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        self.directory.remove(name).map_err(|_| "auth_store_remove")
    }
}
