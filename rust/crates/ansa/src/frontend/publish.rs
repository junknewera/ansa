//! Publish a verified tree with a public parent. The snapshot history stays local.
use super::{Result, orchestration::run_root, sessions::Factory};
use crate::{
    auth::Cancel,
    orchestration::worktree::{Frozen, Worktree},
    process::git::{self, GitOutput},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        contracts::valid_id,
        replay::{Effects, Execution},
    },
    tools::{
        command::{ProcessOptions, ProcessRunner},
        invocation::Invocation,
    },
};
use ansa_linux::{fs as local_fs, local};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const TITLE: &str = "Проверенные изменения ansa";
const PR_FIELDS: &str =
    "url,headRefName,headRefOid,baseRefName,baseRefOid,isDraft,isCrossRepository,state";

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
    let release = Release {
        candidate: candidate.commit().to_owned(),
        baseline: worktree.baseline().to_owned(),
        source_head: worktree.source_head().to_owned(),
    };
    let bytes = serde_json::to_vec(&release).map_err(|_| "draft_release")?;
    let directory = local_fs::private_directory(storage).map_err(|_| "draft_release")?;
    local::publish_recovered(&directory, "release.json", &bytes).map_err(|_| "draft_release")
}

fn latest_release(factory: &Factory, parent: &str) -> Result<(PathBuf, Release, String)> {
    let root = run_root(factory, parent);
    let directory = local_fs::private_directory(&root).map_err(|_| "draft_release_missing")?;
    let (names, more) = local::names(&directory, 128).map_err(|_| "draft_release")?;
    if more {
        return Err("orchestration_inventory_limit");
    }
    let mut latest: Option<(std::time::SystemTime, PathBuf, Release, String)> = None;
    for name in names {
        let Some(id) = name.to_str().filter(|id| valid_id(id)) else {
            continue;
        };
        let path = root.join(id);
        let Ok(directory) = local_fs::private_directory(&path) else {
            continue;
        };
        let Some(bytes) =
            local::read(&directory, "release.json", 4096).map_err(|_| "draft_release")?
        else {
            continue;
        };
        let modified = std::fs::metadata(path.join("release.json"))
            .and_then(|entry| entry.modified())
            .map_err(|_| "draft_release")?;
        let release: Release = serde_json::from_slice(&bytes).map_err(|_| "draft_release")?;
        if ![&release.candidate, &release.baseline, &release.source_head]
            .into_iter()
            .all(|oid| valid_oid(oid))
        {
            return Err("draft_release");
        }
        if latest.as_ref().is_none_or(|(stamp, ..)| modified > *stamp) {
            latest = Some((modified, path, release, id.to_owned()));
        }
    }
    latest
        .map(|(_, path, release, id)| (path, release, id))
        .ok_or("draft_release_missing")
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn github_remote(remote: &str) -> Result<(String, String)> {
    let (slug, ssh) = if let Some(slug) = remote.strip_prefix("git@github.com:") {
        (slug, true)
    } else if let Some(slug) = remote.strip_prefix("https://github.com/") {
        (slug, false)
    } else {
        return Err("draft_github_origin");
    };
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    let (owner, repo) = slug.split_once('/').ok_or("draft_github_origin")?;
    if ![owner, repo].into_iter().all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    }) {
        return Err("draft_github_origin");
    }
    let prefix = if ssh {
        "git@github.com:"
    } else {
        "https://github.com/"
    };
    Ok((slug.to_owned(), format!("{prefix}{slug}.git")))
}

fn local_git(cwd: &Path, args: &[&str], stop: &Cancel) -> Result<String> {
    match git::run(cwd, args, None, 16 * 1024, Duration::from_secs(30), stop)? {
        GitOutput::Success(bytes) => String::from_utf8(bytes)
            .map(|value| value.trim_end().to_owned())
            .map_err(|_| "draft_git_output"),
        _ => Err("draft_git"),
    }
}

fn tree(cwd: &Path, oid: &str, stop: &Cancel) -> Result<String> {
    let value = local_git(
        cwd,
        &["rev-parse", "--verify", &format!("{oid}^{{tree}}")],
        stop,
    )?;
    if valid_oid(&value) {
        Ok(value)
    } else {
        Err("draft_git_output")
    }
}

fn one_parent(cwd: &Path, oid: &str, parent: &str, stop: &Cancel) -> Result<bool> {
    let commit = local_git(cwd, &["cat-file", "commit", oid], stop)?;
    let header = commit.split_once("\n\n").ok_or("draft_git_output")?.0;
    Ok(header
        .lines()
        .filter_map(|line| line.strip_prefix("parent "))
        .eq([parent]))
}

fn valid_export_commit(
    cwd: &Path,
    oid: &str,
    tree: &str,
    parent: &str,
    stop: &Cancel,
) -> Result<bool> {
    if !valid_oid(oid) {
        return Ok(false);
    }
    let GitOutput::Success(bytes) = git::run(
        cwd,
        &["cat-file", "commit", oid],
        None,
        1024,
        Duration::from_secs(30),
        stop,
    )?
    else {
        return Ok(false);
    };
    let data = std::str::from_utf8(&bytes).map_err(|_| "draft_git_output")?;
    let Some((headers, message)) = data.split_once("\n\n") else {
        return Ok(false);
    };
    let mut lines = headers.lines();
    let identity = |line: Option<&str>, role: &str| {
        let Some((timestamp, zone)) = line
            .and_then(|line| line.strip_prefix(role))
            .and_then(|line| line.strip_prefix(" ansa <ansa@localhost> "))
            .and_then(|date| date.split_once(' '))
        else {
            return false;
        };
        !timestamp.is_empty()
            && timestamp.bytes().all(|byte| byte.is_ascii_digit())
            && zone.len() == 5
            && matches!(zone.as_bytes()[0], b'+' | b'-')
            && zone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    };
    Ok(
        lines.next().and_then(|line| line.strip_prefix("tree ")) == Some(tree)
            && lines.next().and_then(|line| line.strip_prefix("parent ")) == Some(parent)
            && identity(lines.next(), "author")
            && identity(lines.next(), "committer")
            && lines.next().is_none()
            && message == format!("{TITLE}\n"),
    )
}

fn export_tree(
    path: &Path,
    source: &Path,
    checkout: &Path,
    release: &Release,
    stop: &Cancel,
) -> Result<String> {
    let baseline = tree(checkout, &release.baseline, stop)?;
    if baseline != tree(source, &release.source_head, stop)? {
        return Err("draft_snapshot_private_changes");
    }
    let branch = local_git(
        checkout,
        &["rev-parse", "--verify", "refs/heads/ansa-result"],
        stop,
    )?;
    if branch != release.candidate
        || !one_parent(checkout, &release.candidate, &release.baseline, stop)?
    {
        return Err("draft_candidate_changed");
    }
    let tree_id = tree(checkout, &release.candidate, stop)?;
    if !path.exists() {
        local_fs::create_private_leaf(path).map_err(|_| "draft_export_directory")?;
    }
    local_fs::private_directory(path).map_err(|_| "draft_export_directory")?;
    if !path.join("HEAD").exists() {
        let format = if release.source_head.len() == 64 {
            "sha256"
        } else {
            "sha1"
        };
        local_git(
            path,
            &[
                "init",
                "--bare",
                "--template=",
                &format!("--object-format={format}"),
            ],
            stop,
        )?;
    }
    if let Ok(exported) = local_git(path, &["rev-parse", "--verify", "refs/heads/export"], stop) {
        return if valid_export_commit(path, &exported, &tree_id, &release.source_head, stop)? {
            Ok(exported)
        } else {
            Err("draft_export_conflict")
        };
    }
    for (repository, oid) in [
        (source, release.source_head.as_str()),
        (checkout, release.candidate.as_str()),
    ] {
        local_git(
            path,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                repository.to_str().ok_or("draft_git_output")?,
                oid,
            ],
            stop,
        )?;
    }
    let exported = local_git(
        path,
        &[
            "-c",
            "user.name=ansa",
            "-c",
            "user.email=ansa@localhost",
            "commit-tree",
            &tree_id,
            "-p",
            &release.source_head,
            "-m",
            TITLE,
        ],
        stop,
    )?;
    if !valid_export_commit(path, &exported, &tree_id, &release.source_head, stop)? {
        return Err("draft_git_output");
    }
    local_git(
        path,
        &[
            "update-ref",
            "refs/heads/export",
            &exported,
            &"0".repeat(exported.len()),
        ],
        stop,
    )?;
    Ok(exported)
}

fn remote_job(
    runner: &ProcessRunner,
    cwd: &Path,
    phase: &str,
    argv: Vec<String>,
    stop: &Cancel,
) -> Result<String> {
    stop.check()?;
    let arguments = Json::parse(&serde_json::json!({"argv": argv}).to_string())?;
    let identity = if matches!(phase, "push" | "create") {
        phase.to_owned()
    } else {
        format!("{phase}-{}", crate::auth::pkce::random_token(16)?)
    };
    let invocation = Invocation {
        operation: sha256(format!("{identity}\n{}", arguments.dump()).as_bytes()),
        call: phase.into(),
        name: "draft_pr".into(),
        revision: "ansa_draft_pr_v1".into(),
        arguments,
    };
    let outcome = runner.run_github(
        &invocation,
        &ProcessOptions {
            cwd: cwd.into(),
            argv,
            timeout_ms: 60_000,
            output_bytes: 8192,
        },
        stop,
    )?;
    if outcome.effects() != Effects::KnownResult {
        return Err("draft_effect_unknown");
    }
    let result =
        Json::parse(std::str::from_utf8(outcome.output()).map_err(|_| "draft_remote_output")?)?;
    if outcome.execution() != Execution::Succeeded
        || result.usize_field("exit_code") != Ok(0)
        || result.field("truncated").and_then(Json::boolean) != Ok(false)
        || result.field("cleanup_complete").and_then(Json::boolean) != Ok(true)
    {
        return Err(match phase {
            "base" | "head" => "draft_remote_read",
            "push" => "draft_push",
            "create" => "draft_create",
            _ => "draft_remote_pr_state",
        });
    }
    Ok(result.text_field("stdout")?.trim().to_owned())
}

fn remote_git(args: &[&str]) -> Vec<String> {
    [
        "git",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "credential.helper=!gh auth git-credential",
        "-c",
        "core.sshCommand=ssh -F /dev/null -o BatchMode=yes -o StrictHostKeyChecking=yes",
    ]
    .into_iter()
    .chain(args.iter().copied())
    .map(str::to_owned)
    .collect()
}

fn draft_url<'a>(repository: &str, url: &'a str) -> Result<&'a str> {
    let (path, number) = url.rsplit_once("/pull/").ok_or("draft_remote_output")?;
    if !path.eq_ignore_ascii_case(&format!("https://github.com/{repository}"))
        || number.is_empty()
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("draft_remote_output");
    }
    Ok(url)
}

fn confirm_pull_request<'a>(
    value: &'a Json,
    repository: &str,
    base: &str,
    source: &str,
    branch: &str,
    commit: &str,
) -> Result<&'a str> {
    if value.text_field("baseRefName") != Ok(base)
        || value.text_field("baseRefOid") != Ok(source)
        || value.text_field("headRefName") != Ok(branch)
        || value.text_field("headRefOid") != Ok(commit)
        || value.text_field("state") != Ok("OPEN")
        || value.field("isDraft").and_then(Json::boolean) != Ok(true)
        || value.field("isCrossRepository").and_then(Json::boolean) != Ok(false)
    {
        return Err("draft_remote_pr_state");
    }
    draft_url(repository, value.text_field("url")?)
}

pub(super) fn draft(factory: &Factory, parent: &str, base: &str, stop: &Cancel) -> Result<String> {
    if factory.offline {
        return Err("draft_requires_network");
    }
    use std::os::unix::fs::PermissionsExt;
    if !std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            directory.join("gh").metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    }) {
        return Err("draft_github_cli_required");
    }
    let (storage, release, id) = latest_release(factory, parent)?;
    let source = Worktree::repository_root(&factory.settings.cwd, stop)?;
    let reference = format!("refs/heads/{base}");
    local_git(&source, &["check-ref-format", &reference], stop).map_err(|_| "draft_base_branch")?;
    let (repository, remote) =
        github_remote(&local_git(&source, &["remote", "get-url", "origin"], stop)?)?;
    let directory = local_fs::private_directory(&storage).map_err(|_| "draft_release")?;
    let _owner = local::lock(&directory).map_err(|_| "draft_busy")?;
    let export = storage.join("publish");
    let checkout = storage.join("checkout");
    let commit = export_tree(&export, &source, &checkout, &release, stop)?;
    let request =
        serde_json::json!({"repository":repository,"remote":remote,"base":base,"export":commit})
            .to_string();
    if local::read(&directory, "draft-target.json", 4096)
        .map_err(|_| "draft_release")?
        .is_some_and(|existing| existing != request.as_bytes())
    {
        return Err("draft_target_conflict");
    }
    let runner = ProcessRunner::new(
        &storage.join("draft-jobs"),
        &std::env::current_exe().map_err(|_| "process_executable")?,
    )?;
    let advertised = |phase, reference: &str| -> Result<Option<String>> {
        let line = remote_job(
            &runner,
            &export,
            phase,
            remote_git(&["ls-remote", "--refs", "--", &remote, reference]),
            stop,
        )?;
        if line.is_empty() {
            return Ok(None);
        }
        let (oid, found) = line.split_once('\t').ok_or("draft_remote_output")?;
        if !valid_oid(oid) || found != reference {
            return Err("draft_remote_output");
        }
        Ok(Some(oid.to_owned()))
    };
    if advertised("base", &reference)?.as_deref() != Some(&release.source_head) {
        return Err("draft_base_changed");
    }
    let branch = format!("ansa-{id}");
    let head = format!("refs/heads/{branch}");
    let existing = advertised("head", &head)?;
    if existing.as_ref().is_some_and(|sha| sha != &commit) {
        return Err("draft_head_changed");
    }
    let repo = format!("github.com/{repository}");
    let gh = |phase, args: &[&str]| {
        remote_job(
            &runner,
            &export,
            phase,
            ["gh", "pr"]
                .into_iter()
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect(),
            stop,
        )
    };
    let confirm = |pull: &Json| {
        confirm_pull_request(
            pull,
            &repository,
            base,
            &release.source_head,
            &branch,
            &commit,
        )
        .map(str::to_owned)
    };
    let lookup = |has_head| -> Result<Option<String>> {
        let pulls = Json::parse(&gh(
            "list",
            &[
                "list", "--repo", &repo, "--state", "all", "--head", &branch, "--limit", "2",
                "--json", PR_FIELDS,
            ],
        )?)?;
        match pulls.elements()? {
            [] => Ok(None),
            [pull] if has_head => confirm(pull).map(Some),
            _ => Err("draft_remote_pr_state"),
        }
    };
    let url = match lookup(existing.is_some())? {
        Some(url) => url,
        None => {
            local::publish_recovered(&directory, "draft-target.json", request.as_bytes())
                .map_err(|_| "draft_target_conflict")?;
            if existing.is_none() {
                remote_job(
                    &runner,
                    &export,
                    "push",
                    remote_git(&[
                        "push",
                        "--no-follow-tags",
                        &format!("--force-with-lease={head}:"),
                        "--",
                        &remote,
                        &format!("{commit}:{head}"),
                    ]),
                    stop,
                )?;
            }
            if advertised("base", &reference)?.as_deref() != Some(&release.source_head) {
                return Err("draft_base_changed");
            }
            if advertised("head", &head)?.as_deref() != Some(&commit) {
                return Err("draft_head_changed");
            }
            if let Some(url) = lookup(true)? {
                url
            } else {
                let url = gh(
                    "create",
                    &[
                        "create",
                        "--repo",
                        &repo,
                        &format!("--base={base}"),
                        "--head",
                        &branch,
                        "--title",
                        TITLE,
                        "--body",
                        "Кандидат прошёл проверки и независимое ревью в ansa.",
                        "--draft",
                    ],
                )?;
                let url = draft_url(&repository, &url)?;
                let pull = Json::parse(&gh(
                    "verify",
                    &["view", url, "--repo", &repo, "--json", PR_FIELDS],
                )?)?;
                confirm(&pull)?
            }
        }
    };
    local::publish_recovered(&directory, "draft-url", url.as_bytes())
        .map_err(|_| "draft_release")?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_export_uses_public_ancestry_and_refuses_private_snapshots() {
        let path = std::env::temp_dir().join(format!(
            "ansa-draft-fixture-{}",
            crate::auth::pkce::random_token(16).unwrap()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        local_fs::create_private_leaf(&path).unwrap();
        let _cleanup = Cleanup(path.clone());
        let source = path.join("source");
        let clean = path.join("clean");
        let dirty = path.join("dirty");
        for directory in [&source, &clean, &dirty] {
            local_fs::create_private_leaf(directory).unwrap();
        }
        let stop = Cancel::default();
        local_git(&source, &["init", "--template=", "--quiet"], &stop).unwrap();
        std::fs::write(source.join("report"), "first\n").unwrap();
        local_git(&source, &["add", "report"], &stop).unwrap();
        local_git(
            &source,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "-qm",
                "source",
            ],
            &stop,
        )
        .unwrap();
        let worktree = Worktree::capture(&source, &clean, &[], &stop).unwrap();
        std::fs::write(worktree.path().join("report"), "second\n").unwrap();
        let candidate = worktree.candidate(0, &["report".into()], &stop).unwrap();
        worktree.local_branch(&candidate, &stop).unwrap();
        let release = Release {
            candidate: candidate.commit().into(),
            baseline: worktree.baseline().into(),
            source_head: worktree.source_head().into(),
        };
        let export = clean.join("publish");
        let result = export_tree(&export, &source, worktree.path(), &release, &stop).unwrap();
        assert_eq!(
            local_git(&export, &["rev-parse", &format!("{result}^")], &stop).unwrap(),
            release.source_head
        );
        assert_eq!(
            local_git(&export, &["show", &format!("{result}:report")], &stop).unwrap(),
            "second"
        );
        assert!(
            local_git(
                &export,
                &["merge-base", "--is-ancestor", &release.baseline, &result],
                &stop
            )
            .is_err()
        );
        assert_eq!(
            export_tree(&export, &source, worktree.path(), &release, &stop).unwrap(),
            result
        );
        let wrong_ancestry = local_git(
            &export,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit-tree",
                &tree(&export, &result, &stop).unwrap(),
                "-p",
                &release.source_head,
                "-p",
                &release.baseline,
                "-m",
                "invalid private ancestry",
            ],
            &stop,
        )
        .unwrap();
        local_git(
            &export,
            &["update-ref", "refs/heads/export", &wrong_ancestry],
            &stop,
        )
        .unwrap();
        assert_eq!(
            export_tree(&export, &source, worktree.path(), &release, &stop),
            Err("draft_export_conflict")
        );
        let wrong_metadata = local_git(
            &export,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit-tree",
                &tree(&export, &result, &stop).unwrap(),
                "-p",
                &release.source_head,
                "-m",
                "private message",
            ],
            &stop,
        )
        .unwrap();
        local_git(
            &export,
            &["update-ref", "refs/heads/export", &wrong_metadata],
            &stop,
        )
        .unwrap();
        assert_eq!(
            export_tree(&export, &source, worktree.path(), &release, &stop),
            Err("draft_export_conflict")
        );

        std::fs::write(source.join("local-only"), "private fixture\n").unwrap();
        let worktree = Worktree::capture(&source, &dirty, &[], &stop).unwrap();
        let release = Release {
            baseline: worktree.baseline().into(),
            ..release
        };
        local_git(
            &source,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--no-tags",
                "--",
                worktree.path().to_str().unwrap(),
                &release.baseline,
            ],
            &stop,
        )
        .unwrap();
        local_git(
            &source,
            &[
                "update-ref",
                &format!("refs/replace/{}", release.source_head),
                &release.baseline,
            ],
            &stop,
        )
        .unwrap();
        assert_eq!(
            export_tree(
                &dirty.join("publish"),
                &source,
                worktree.path(),
                &release,
                &stop
            ),
            Err("draft_snapshot_private_changes")
        );
        assert!(!dirty.join("publish").exists());
    }
}
