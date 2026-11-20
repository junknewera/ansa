//! Explicit session selection. Never discovers HOME or opens a user profile.
pub use super::summary::{SessionSummary, SummaryStatus};
use super::{Result, need, num};
use crate::{protocol::json::Json, provider::contracts::valid_id};
use std::path::{Path, PathBuf};
pub struct Catalog {
    files: ansa_linux::journal::Catalog,
    root: PathBuf,
    cwd: String,
    profile: String,
}
#[derive(Clone, Debug)]
pub struct Selected {
    pub id: String,
    pub directory: PathBuf,
}
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub kind: String,
    pub parent: Option<String>,
}
impl Catalog {
    pub fn open(root: &Path, cwd: &Path, profile: &str) -> Result<Self> {
        Ok(Self {
            files: ansa_linux::journal::Catalog::open(root)
                .map_err(|_| "local_directory_permissions")?,
            root: root.into(),
            cwd: cwd.to_str().ok_or("session_workspace_mismatch")?.into(),
            profile: profile.into(),
        })
    }
    fn metadata(&self, id: &str) -> Result<Option<Json>> {
        self.files
            .info(id)
            .map_err(|_| "local_file_permissions")?
            .map(|text| Json::parse_with_limits(&text, 4096, 64, 131072))
            .transpose()
    }
    pub fn explicit(&self, id: &str) -> Result<Selected> {
        need(valid_id(id), "invalid_session_id")?;
        let info = self.metadata(id)?.ok_or("session_workspace_mismatch")?;
        need(
            info.usize_field("schema_version")? == 1
                && info.text_field("id")? == id
                && info.text_field("cwd")? == self.cwd
                && info.text_field("profile")? == self.profile,
            "session_workspace_mismatch",
        )?;
        Ok(Selected {
            id: id.into(),
            directory: self.root.join(id),
        })
    }
    pub fn latest(&self) -> Result<Selected> {
        let mut latest = None;
        for id in self.files.entries().map_err(|_| "session_catalog_limit")? {
            let Some(info) = self.metadata(&id)? else {
                continue;
            };
            if info.text_field("cwd")? != self.cwd || info.text_field("profile")? != self.profile {
                continue;
            }
            if info.has("kind") && matches!(info.text_field("kind")?, "agent" | "btw" | "console") {
                continue;
            }
            if let Some(updated) = self
                .files
                .modified(&id)
                .map_err(|_| "session_parts_limit")?
                && latest
                    .as_ref()
                    .is_none_or(|(old, _): &((i64, u64), String)| updated > *old)
            {
                latest = Some((updated, id));
            }
        }
        self.explicit(&latest.ok_or("no_previous_session")?.1)
    }
    pub fn create(&self) -> Result<Selected> {
        self.create_kind("main", None)
    }
    pub fn entries(&self) -> Result<Vec<CatalogEntry>> {
        self.entries_checked(|| Ok(()))
    }
    fn entries_checked(&self, check: impl Fn() -> Result<()>) -> Result<Vec<CatalogEntry>> {
        let mut entries = Vec::new();
        for id in self.files.entries().map_err(|_| "session_catalog_limit")? {
            check()?;
            let Some(info) = self.metadata(&id)? else {
                continue;
            };
            if info.text_field("cwd")? != self.cwd || info.text_field("profile")? != self.profile {
                continue;
            }
            entries.push(CatalogEntry {
                id,
                kind: if info.has("kind") {
                    info.text_field("kind")?.into()
                } else {
                    "main".into()
                },
                parent: if info.has("parent") {
                    Some(info.text_field("parent")?.into())
                } else {
                    None
                },
            });
        }
        entries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(entries)
    }
    pub fn create_kind(&self, kind: &str, parent: Option<&str>) -> Result<Selected> {
        need(
            matches!(kind, "main" | "fork" | "btw" | "agent" | "console")
                && parent.is_none_or(valid_id),
            "session_kind",
        )?;
        use base64::Engine;
        use ring::rand::{SecureRandom, SystemRandom};
        let mut bytes = [0; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "session_random")?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "session_clock")?
            .as_millis();
        let id = format!(
            "s_{millis}_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        );
        self.create_named_kind(&id, kind, parent)
    }
    /// Derived display observations only. A shared byte budget prevents a large
    /// catalogue from multiplying per-journal limits into unbounded I/O.
    pub fn summaries(&self, stop: &crate::auth::Cancel) -> Result<Vec<SessionSummary>> {
        self.summaries_with_catalog(stop, None)
    }

    pub(crate) fn summaries_with_catalog(
        &self,
        stop: &crate::auth::Cancel,
        compiled: Option<&crate::tools::registry::Snapshot>,
    ) -> Result<Vec<SessionSummary>> {
        let mut budget = 64 * 1024 * 1024;
        let mut summaries = Vec::new();
        for entry in self.entries_checked(|| stop.check())? {
            stop.check()?;
            summaries.push(super::summary::read(
                &self.root.join(&entry.id),
                entry,
                &mut budget,
                stop,
                compiled,
            )?);
        }
        Ok(summaries)
    }
    pub fn create_named(&self, id: &str) -> Result<Selected> {
        self.create_named_kind(id, "main", None)
    }
    fn create_named_kind(&self, id: &str, kind: &str, parent: Option<&str>) -> Result<Selected> {
        need(valid_id(id), "invalid_session_id")?;
        let mut fields = vec![
            ("schema_version", num(1)?),
            ("id", Json::string(id)?),
            ("cwd", Json::string(&self.cwd)?),
            ("profile", Json::string(&self.profile)?),
        ];
        if kind != "main" {
            fields.push(("kind", Json::string(kind)?));
        }
        if let Some(parent) = parent {
            fields.push(("parent", Json::string(parent)?));
        }
        let info = Json::object(fields)?;
        self.files
            .create(id, info.dump().as_bytes())
            .map_err(|_| "session_directory_create")?;
        Ok(Selected {
            id: id.into(),
            directory: self.root.join(id),
        })
    }
}
