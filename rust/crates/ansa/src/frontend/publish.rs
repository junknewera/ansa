//! Local proof linking an orchestration result to the reviewed candidate.
use super::Result;
use crate::orchestration::worktree::{Frozen, Worktree};
use ansa_linux::{fs as local_fs, local};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    candidate: String,
    baseline: String,
    source_head: String,
}

pub(super) fn record_release(
    storage: &Path,
    worktree: &Worktree,
    candidate: &Frozen,
) -> Result<()> {
    record_candidate(storage, worktree, candidate, "release.json")
}

pub(super) fn record_pending_manual(
    storage: &Path,
    worktree: &Worktree,
    candidate: &Frozen,
) -> Result<()> {
    record_candidate(storage, worktree, candidate, "manual-candidate.json")
}

fn record_candidate(
    storage: &Path,
    worktree: &Worktree,
    candidate: &Frozen,
    name: &str,
) -> Result<()> {
    let release = Release {
        candidate: candidate.commit().to_owned(),
        baseline: worktree.baseline().to_owned(),
        source_head: worktree.source_head().to_owned(),
    };
    let bytes = serde_json::to_vec(&release).map_err(|_| "draft_release")?;
    let directory = local_fs::private_directory(storage).map_err(|_| "draft_release")?;
    local::publish_recovered(&directory, name, &bytes).map_err(|_| "draft_release")
}

pub(super) fn released_candidate(storage: &Path, worktree: &Worktree) -> Result<String> {
    read_candidate(storage, worktree, "release.json")
}

pub(super) fn pending_manual_candidate(storage: &Path, worktree: &Worktree) -> Result<String> {
    read_candidate(storage, worktree, "manual-candidate.json")
}

fn read_candidate(storage: &Path, worktree: &Worktree, name: &str) -> Result<String> {
    let directory = local_fs::private_directory(storage).map_err(|_| "draft_release")?;
    let bytes = local::read(&directory, name, 4096)
        .map_err(|_| "draft_release")?
        .ok_or("draft_release_missing")?;
    let release: Release = serde_json::from_slice(&bytes).map_err(|_| "draft_release")?;
    if release.baseline != worktree.baseline()
        || release.source_head != worktree.source_head()
        || !valid_oid(&release.candidate)
    {
        return Err("draft_release");
    }
    Ok(release.candidate)
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
