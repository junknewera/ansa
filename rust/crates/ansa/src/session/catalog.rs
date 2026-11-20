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
    pub(crate) fn open_read_only(root: &Path, cwd: &Path, profile: &str) -> Result<Self> {
        Ok(Self {
            files: ansa_linux::journal::Catalog::open_read_only(root)
                .map_err(|_| "local_directory_permissions")?,
            root: root.into(),
            cwd: cwd.to_str().ok_or("session_workspace_mismatch")?.into(),
            profile: profile.into(),
        })
    }
    pub(crate) fn search(
        &self,
        query: &str,
        account: &str,
        provider: &str,
        compiled: &crate::tools::registry::Snapshot,
        stop: &crate::auth::Cancel,
    ) -> Result<super::search::Matches> {
        self.search_bounded(query, account, provider, compiled, stop, 64 * 1024 * 1024)
    }
    pub(super) fn search_bounded(
        &self,
        query: &str,
        account: &str,
        provider: &str,
        compiled: &crate::tools::registry::Snapshot,
        stop: &crate::auth::Cancel,
        mut budget: usize,
    ) -> Result<super::search::Matches> {
        use super::search::{self, Matches, Scope};
        let query = search::normalize_query(query)?;
        let scope = Scope {
            account,
            provider,
            cwd: &self.cwd,
        };
        let mut matches = Matches::default();
        for entry in self.entries_checked(|| stop.check())? {
            if !matches!(entry.kind.as_str(), "main" | "fork") {
                continue;
            }
            if budget == 0 || matches.hits.len() == search::HIT_LIMIT {
                matches.limit_reached = true;
                break;
            }
            let result = search::read(
                &self.root.join(&entry.id),
                &entry.id,
                scope,
                compiled,
                &query,
                &mut budget,
                search::HIT_LIMIT - matches.hits.len(),
                stop,
            );
            stop.check()?;
            match result {
                Ok(mut records) => {
                    matches.hits.append(&mut records);
                    matches.limit_reached |= matches.hits.len() == search::HIT_LIMIT;
                }
                Err("session_search_scope") => {}
                Err("journal_summary_limit") => matches.limit_reached = true,
                Err(_) => matches.unavailable = true,
            }
        }
        Ok(matches)
    }
    pub(crate) fn search_source(
        &self,
        hit: &super::search::Hit,
        account: &str,
        provider: &str,
        compiled: &crate::tools::registry::Snapshot,
        stop: &crate::auth::Cancel,
    ) -> Result<Vec<(String, String)>> {
        let selected = self.explicit(&hit.session)?;
        let metadata = self.metadata(&hit.session)?.ok_or("unknown_session")?;
        need(
            !metadata.has("kind") || matches!(metadata.text_field("kind")?, "main" | "fork"),
            "session_search_scope",
        )?;
        super::search::open(
            &selected.directory,
            hit,
            super::search::Scope {
                account,
                provider,
                cwd: &self.cwd,
            },
            compiled,
            &mut (64 * 1024 * 1024),
            stop,
        )
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
