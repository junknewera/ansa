//! Explicit session selection. Never discovers HOME or opens a user profile.
use super::{Result, need, num};
use crate::{protocol::json::Json, provider::contracts::valid_id};
use std::path::{Path, PathBuf};
pub struct Catalog {
    files: ansa_linux::journal::Catalog,
    root: PathBuf,
    cwd: String,
    profile: String,
}
pub struct Selected {
    pub id: String,
    pub directory: PathBuf,
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
        self.create_named(&id)
    }
    pub fn create_named(&self, id: &str) -> Result<Selected> {
        need(valid_id(id), "invalid_session_id")?;
        let info = Json::object(vec![
            ("schema_version", num(1)?),
            ("id", Json::string(id)?),
            ("cwd", Json::string(&self.cwd)?),
            ("profile", Json::string(&self.profile)?),
        ])?;
        self.files
            .create(id, info.dump().as_bytes())
            .map_err(|_| "session_directory_create")?;
        Ok(Selected {
            id: id.into(),
            directory: self.root.join(id),
        })
    }
}
