//! Materialize live working files in a worktree with a private object store.
//! Snapshot commits are local evidence and have no upstream ancestry.
use crate::{
    auth::Cancel,
    process::git::{self, GitOutput},
};
use ansa_linux::{fs as local_fs, journal::ReadIdentity, local};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

type Result<T> = std::result::Result<T, &'static str>;
const GIT_OUTPUT: usize = 2 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FILES: usize = 4096;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

pub struct Worktree {
    path: PathBuf,
    baseline: String,
    source_head: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBinding {
    baseline: String,
    source_head: String,
    workspace_device: u64,
    workspace_inode: u64,
    checkout_device: u64,
    checkout_inode: u64,
}

pub struct Frozen {
    commit: String,
    diff: String,
    paths: Vec<String>,
}

impl Frozen {
    pub fn commit(&self) -> &str {
        &self.commit
    }

    pub fn diff(&self) -> &str {
        &self.diff
    }
    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }
}

struct InputFile {
    relative: String,
    identity: Option<ReadIdentity>,
    executable: bool,
}

impl Worktree {
    pub(crate) fn repository_root(cwd: &Path, stop: &Cancel) -> Result<PathBuf> {
        Ok(PathBuf::from(line(command(
            cwd,
            &["rev-parse", "--show-toplevel"],
            None,
            stop,
        )?)?))
    }

    pub(crate) fn common_directory(cwd: &Path, stop: &Cancel) -> Result<PathBuf> {
        Ok(PathBuf::from(line(command(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
            stop,
        )?)?))
    }

    /// The storage directory belongs to this run and lives outside the source
    /// tree. A caller approves the plan before invoking this operation.
    pub fn capture(
        cwd: &Path,
        storage: &Path,
        excluded: &[PathBuf],
        stop: &Cancel,
    ) -> Result<Self> {
        let root = Self::repository_root(cwd, stop)?;
        let parent = local_fs::private_directory(storage).map_err(|_| "worktree_storage")?;
        let root_dir = local_fs::open_nofollow(&root, local::read_flags())
            .map_err(|_| "worktree_repository")?;
        if !root_dir
            .metadata()
            .map_err(|_| "worktree_repository")?
            .is_dir()
            || storage.starts_with(&root)
        {
            return Err("worktree_storage");
        }
        let head = object_id(command(
            &root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            None,
            stop,
        )?)?;
        let excluded = excluded
            .iter()
            .map(|path| local_fs::weakly_canonical(path).map_err(|_| "worktree_excluded_path"))
            .collect::<Result<Vec<_>>>()?;
        let inputs = file_names(&root, &excluded, stop)?;
        let path = storage.join("checkout");
        local_fs::create_private_leaf(&path).map_err(|_| "worktree_checkout_exists")?;
        let captured = (|| {
            let format = if head.len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            };
            command(
                &path,
                &["init", "--quiet", "--template=", format],
                None,
                stop,
            )?;
            Self::materialize(&root, &path, &parent, &head, inputs, &excluded, stop)
        })();
        match captured {
            Ok(baseline) => {
                let checkout =
                    local_fs::private_directory(&path).map_err(|_| "worktree_storage")?;
                let workspace = root_dir.metadata().map_err(|_| "worktree_repository")?;
                let checkout = checkout.metadata().map_err(|_| "worktree_storage")?;
                let binding = SnapshotBinding {
                    baseline: baseline.clone(),
                    source_head: head.clone(),
                    workspace_device: workspace.dev(),
                    workspace_inode: workspace.ino(),
                    checkout_device: checkout.dev(),
                    checkout_inode: checkout.ino(),
                };
                local::publish_recovered(
                    &parent,
                    "snapshot.json",
                    &serde_json::to_vec(&binding).map_err(|_| "worktree_storage")?,
                )
                .map_err(|_| "worktree_storage")?;
                Ok(Self {
                    path,
                    baseline,
                    source_head: head,
                })
            }
            Err(error) => {
                // This checkout was newly reserved and never exposed to an
                // agent. Keep the failure visible if Git cannot clean it up.
                fs::remove_dir_all(&path).map_err(|_| "worktree_cleanup_unknown")?;
                Err(error)
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn reopen(root: &Path, storage: &Path, stop: &Cancel) -> Result<Self> {
        let directory = local_fs::private_directory(storage).map_err(|_| "worktree_storage")?;
        let bytes = local::read(&directory, "snapshot.json", 2048)
            .map_err(|_| "worktree_storage")?
            .ok_or("worktree_snapshot_missing")?;
        let snapshot: SnapshotBinding =
            serde_json::from_slice(&bytes).map_err(|_| "worktree_storage")?;
        let workspace = local_fs::open_nofollow(root, local::read_flags())
            .and_then(|file| file.metadata())
            .map_err(|_| "worktree_repository")?;
        let path = storage.join("checkout");
        let checkout = local_fs::private_directory(&path)
            .and_then(|file| file.metadata())
            .map_err(|_| "worktree_storage")?;
        if (workspace.dev(), workspace.ino())
            != (snapshot.workspace_device, snapshot.workspace_inode)
            || (checkout.dev(), checkout.ino())
                != (snapshot.checkout_device, snapshot.checkout_inode)
            || !valid_object(&snapshot.baseline)
            || !valid_object(&snapshot.source_head)
            || object_id(command(&path, &["rev-parse", "HEAD"], None, stop)?)? != snapshot.baseline
        {
            return Err("worktree_snapshot_identity");
        }
        Ok(Self {
            path,
            baseline: snapshot.baseline,
            source_head: snapshot.source_head,
        })
    }

    pub fn baseline(&self) -> &str {
        &self.baseline
    }

    pub fn source_head(&self) -> &str {
        &self.source_head
    }

    pub fn freeze(&self, scope: &[String], stop: &Cancel) -> Result<Frozen> {
        let names = file_names(&self.path, &[], stop)?;
        let mut files = Vec::with_capacity(names.len());
        let mut remaining = MAX_BYTES as u64;
        for relative in &names {
            stop.check()?;
            let (identity, executable) = if let Some(file) = open_source(&self.path.join(relative))?
            {
                let identity = ReadIdentity::capture(&file).map_err(|_| "worktree_file_read")?;
                if identity.size() > remaining.min(MAX_FILE_BYTES as u64) {
                    return Err("worktree_copy_limit");
                }
                remaining -= identity.size();
                (
                    Some(identity),
                    file.metadata().map_err(|_| "worktree_file_type")?.mode() & 0o111 != 0,
                )
            } else {
                (None, false)
            };
            files.push(InputFile {
                relative: relative.clone(),
                identity,
                executable,
            });
        }
        let tree = write_snapshot_tree(&self.path, &files, stop)?;
        check_inputs(&self.path, &files, stop)?;
        if file_names(&self.path, &[], stop)? != names {
            return Err("worktree_source_changed");
        }
        let (diff, paths) = self.checked_diff(&tree, scope, stop)?;
        let commit = object_id(command(
            &self.path,
            &[
                "-c",
                "user.name=ansa",
                "-c",
                "user.email=ansa@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-p",
                &self.baseline,
                "-m",
                "Orchestration candidate",
            ],
            None,
            stop,
        )?)?;
        Ok(Frozen {
            commit,
            diff,
            paths,
        })
    }

    pub(crate) fn candidate(&self, number: u8, scope: &[String], stop: &Cancel) -> Result<Frozen> {
        let storage = self.path.parent().ok_or("worktree_storage")?;
        let directory = local_fs::private_directory(storage).map_err(|_| "worktree_storage")?;
        let name = format!("candidate-{number}");
        if let Some(bytes) = local::read(&directory, &name, 128).map_err(|_| "worktree_storage")? {
            let commit = String::from_utf8(bytes).map_err(|_| "worktree_candidate_identity")?;
            if !valid_object(&commit) {
                return Err("worktree_candidate_identity");
            }
            self.check_candidate_parent(&commit, stop)?;
            let (diff, paths) = self.checked_diff(&commit, scope, stop)?;
            return Ok(Frozen {
                commit,
                diff,
                paths,
            });
        }
        let frozen = self.freeze(scope, stop)?;
        local::publish_recovered(&directory, &name, frozen.commit.as_bytes())
            .map_err(|_| "worktree_storage")?;
        Ok(frozen)
    }

    fn checked_diff(
        &self,
        target: &str,
        scope: &[String],
        stop: &Cancel,
    ) -> Result<(String, Vec<String>)> {
        let changed = command(
            &self.path,
            &[
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                &self.baseline,
                target,
                "--",
            ],
            None,
            stop,
        )?;
        let changed = changed.strip_suffix(&[0]).ok_or("worktree_no_changes")?;
        let mut paths = Vec::new();
        for path in changed.split(|byte| *byte == 0) {
            let path = std::str::from_utf8(path).map_err(|_| "worktree_path_encoding")?;
            if !scope
                .iter()
                .any(|allowed| Path::new(path).starts_with(allowed))
            {
                return Err("worktree_change_outside_approval");
            }
            paths.push(path.to_owned());
        }
        command(
            &self.path,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--check",
                &self.baseline,
                target,
                "--",
            ],
            None,
            stop,
        )
        .map_err(|_| "candidate_diff_check")?;
        let diff = command(
            &self.path,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                &self.baseline,
                target,
                "--",
            ],
            None,
            stop,
        )?;
        Ok((
            String::from_utf8(diff).map_err(|_| "worktree_diff_encoding")?,
            paths,
        ))
    }

    fn check_candidate_parent(&self, commit: &str, stop: &Cancel) -> Result<()> {
        let parent = object_id(command(
            &self.path,
            &["rev-parse", &format!("{commit}^")],
            None,
            stop,
        )?)?;
        if parent != self.baseline {
            return Err("worktree_candidate_identity");
        }
        Ok(())
    }

    pub(crate) fn inspect_candidate(
        &self,
        commit: &str,
        scope: &[String],
        stop: &Cancel,
    ) -> Result<Frozen> {
        if !valid_object(commit) {
            return Err("worktree_candidate_identity");
        }
        self.check_candidate_parent(commit, stop)?;
        let (diff, paths) = self.checked_diff(commit, scope, stop)?;
        Ok(Frozen {
            commit: commit.into(),
            diff,
            paths,
        })
    }

    pub fn local_branch(&self, frozen: &Frozen, stop: &Cancel) -> Result<()> {
        self.check_candidate_parent(&frozen.commit, stop)?;
        if let Ok(branch) = command(
            &self.path,
            &["rev-parse", "--verify", "refs/heads/ansa-result"],
            None,
            stop,
        ) {
            return if object_id(branch)? == frozen.commit {
                Ok(())
            } else {
                Err("worktree_branch_conflict")
            };
        }
        command(
            &self.path,
            &[
                "update-ref",
                "refs/heads/ansa-result",
                &frozen.commit,
                &"0".repeat(frozen.commit.len()),
            ],
            None,
            stop,
        )?;
        Ok(())
    }

    pub(crate) fn frozen_view(
        &self,
        frozen: &Frozen,
        number: u8,
        plan: &str,
        stop: &Cancel,
    ) -> Result<PathBuf> {
        let storage = self.path.parent().ok_or("worktree_storage")?;
        self.frozen_view_named(
            frozen,
            &format!("verification-{number}"),
            plan,
            storage.join(format!("review-{number}")).exists(),
            stop,
        )
    }

    pub(crate) fn frozen_view_named(
        &self,
        frozen: &Frozen,
        name: &str,
        plan: &str,
        referenced: bool,
        stop: &Cancel,
    ) -> Result<PathBuf> {
        let storage = self.path.parent().ok_or("worktree_storage")?;
        self.frozen_view_in(frozen, storage, name, plan, referenced, stop)
    }

    pub(crate) fn frozen_view_in(
        &self,
        frozen: &Frozen,
        storage: &Path,
        name: &str,
        plan: &str,
        referenced: bool,
        stop: &Cancel,
    ) -> Result<PathBuf> {
        let path = storage.join(name);
        if path.exists() && !path.join("ansa-candidate.diff").exists() && !referenced {
            let retained = storage.join(format!("interrupted-{name}"));
            if retained.exists() {
                return Err("candidate_view_incomplete");
            }
            fs::rename(&path, retained).map_err(|_| "candidate_view_incomplete")?;
        }
        let reopened = path.exists();
        if referenced && !reopened {
            return Err("candidate_view_missing");
        }
        let dir = if reopened {
            local_fs::private_directory(&path)
        } else {
            local_fs::create_private_leaf(&path)
        }
        .map_err(|_| "candidate_view")?;
        let checkout = path.join("checkout");
        if reopened {
            let (names, more) = local::names(&dir, 3).map_err(|_| "candidate_view")?;
            if more
                || names.len() != 3
                || !names.iter().all(|name| {
                    name == "checkout"
                        || name == "ansa-candidate.diff"
                        || name == "approved-plan.json"
                })
                || local::read(&dir, "ansa-candidate.diff", GIT_OUTPUT)
                    .map_err(|_| "candidate_view")?
                    .as_deref()
                    != Some(frozen.diff.as_bytes())
                || local::read(&dir, "approved-plan.json", 64 * 1024)
                    .map_err(|_| "candidate_view")?
                    .as_deref()
                    != Some(plan.as_bytes())
            {
                return Err("candidate_view_changed");
            }
            local_fs::private_directory(&checkout).map_err(|_| "candidate_view")?;
        } else {
            local_fs::create_private_leaf(&checkout).map_err(|_| "candidate_view")?;
        }
        let list = command(
            &self.path,
            &["ls-tree", "-r", "-z", &frozen.commit],
            None,
            stop,
        )?;
        let list = list.strip_suffix(&[0]).ok_or("candidate_tree")?;
        let mut files = Vec::new();
        let mut objects = String::new();
        for row in list.split(|byte| *byte == 0) {
            let row = std::str::from_utf8(row).map_err(|_| "candidate_tree")?;
            let (metadata, filename) = row.split_once('\t').ok_or("candidate_tree")?;
            let (mode, id) = metadata.split_once(" blob ").ok_or("candidate_tree")?;
            if !matches!(mode, "100644" | "100755")
                || !valid_object(id)
                || !super::plan::valid_path(filename)
                || files.len() >= MAX_FILES
            {
                return Err("candidate_tree");
            }
            files.push((filename, id, mode == "100755"));
            objects.push_str(id);
            objects.push('\n');
        }
        let blobs = input_command(objects.as_bytes(), |input| {
            match git::run(
                &self.path,
                &["cat-file", "--batch"],
                Some(input),
                MAX_BYTES + GIT_OUTPUT,
                GIT_TIMEOUT,
                stop,
            )? {
                GitOutput::Success(data) => Ok(data),
                _ => Err("candidate_blob_read"),
            }
        })?;
        let mut remaining = blobs.as_slice();
        let mut expected = BTreeSet::new();
        for (filename, id, executable) in files {
            stop.check()?;
            let next = remaining
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or("candidate_blob_read")?;
            let header =
                std::str::from_utf8(&remaining[..next]).map_err(|_| "candidate_blob_read")?;
            let size: usize = header
                .strip_prefix(&format!("{id} blob "))
                .and_then(|size| size.parse().ok())
                .ok_or("candidate_blob_read")?;
            if size > MAX_FILE_BYTES
                || next + size + 1 >= remaining.len()
                || remaining[next + size + 1] != b'\n'
            {
                return Err("candidate_blob_read");
            }
            let target = checkout.join(filename);
            let content = &remaining[next + 1..next + size + 1];
            if reopened {
                let existing = open_source(&target)?.ok_or("candidate_view_changed")?;
                let metadata = existing.metadata().map_err(|_| "candidate_view")?;
                if metadata.len() != size as u64 || (metadata.mode() & 0o111 != 0) != executable {
                    return Err("candidate_view_changed");
                }
                let mut bytes = Vec::with_capacity(size);
                existing
                    .take(size as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| "candidate_view")?;
                if bytes != content {
                    return Err("candidate_view_changed");
                }
                expected.insert(filename.to_owned());
            } else {
                fs::create_dir_all(target.parent().ok_or("candidate_tree")?)
                    .map_err(|_| "candidate_view")?;
                File::options()
                    .write(true)
                    .create_new(true)
                    .mode(if executable { 0o700 } else { 0o600 })
                    .open(&target)
                    .and_then(|mut file| file.write_all(content).and_then(|()| file.sync_all()))
                    .map_err(|_| "candidate_view")?;
            }
            remaining = &remaining[next + size + 2..];
        }
        if !remaining.is_empty() {
            return Err("candidate_blob_read");
        }
        if reopened {
            check_view_files(&checkout, &mut expected, stop)?;
        } else {
            local::create(&dir, "approved-plan.json", plan.as_bytes())
                .map_err(|_| "candidate_view")?;
            local::create(&dir, "ansa-candidate.diff", frozen.diff.as_bytes())
                .map_err(|_| "candidate_view")?;
        }
        Ok(path)
    }

    pub(crate) fn failed_checks(
        view: &Path,
        checks: &[super::plan::Check],
        stop: &Cancel,
    ) -> Result<Vec<String>> {
        let mut failures = Vec::new();
        for check in checks {
            stop.check()?;
            let path = view.join("checkout").join(&check.path);
            let mut value = String::new();
            if let Some(file) = open_source(&path)? {
                file.take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_string(&mut value)
                    .map_err(|_| "candidate_check_file")?;
            }
            if value.len() > MAX_FILE_BYTES || !value.contains(&check.contains) {
                failures.push(format!(
                    "В {} не найдена согласованная строка {}",
                    check.path, check.contains
                ));
            }
        }
        Ok(failures)
    }

    fn materialize(
        root: &Path,
        path: &Path,
        parent: &File,
        head: &str,
        inputs: BTreeSet<String>,
        excluded: &[PathBuf],
        stop: &Cancel,
    ) -> Result<String> {
        let mut remaining = MAX_BYTES;
        let mut files = Vec::with_capacity(inputs.len());
        for relative in &inputs {
            stop.check()?;
            files.push(copy_input(root, path, relative, &mut remaining)?);
        }
        if file_names(root, excluded, stop)? != inputs
            || object_id(command(
                root,
                &["rev-parse", "--verify", "HEAD^{commit}"],
                None,
                stop,
            )?)? != head
        {
            return Err("worktree_source_changed");
        }
        check_inputs(root, &files, stop)?;
        let tree = write_snapshot_tree(path, &files, stop)?;
        let baseline = object_id(command(
            path,
            &[
                "-c",
                "user.name=ansa",
                "-c",
                "user.email=ansa@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-m",
                "Capture orchestration inputs",
            ],
            None,
            stop,
        )?)?;
        command(path, &["update-ref", "HEAD", &baseline], None, stop)?;
        parent.sync_all().map_err(|_| "worktree_storage")?;
        Ok(baseline)
    }
}

fn check_view_files(root: &Path, expected: &mut BTreeSet<String>, stop: &Cancel) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        for child in fs::read_dir(path).map_err(|_| "candidate_view")? {
            stop.check()?;
            visited += 1;
            if visited > MAX_FILES * 4 {
                return Err("candidate_view_limit");
            }
            let child = child.map_err(|_| "candidate_view")?;
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or("candidate_view_changed")?;
            let kind = child.file_type().map_err(|_| "candidate_view")?;
            if kind.is_dir()
                && expected
                    .iter()
                    .any(|file| Path::new(file).starts_with(relative))
            {
                pending.push(path);
            } else if !kind.is_file() || !expected.remove(relative) {
                return Err("candidate_view_changed");
            }
        }
    }
    if !expected.is_empty() {
        return Err("candidate_view_changed");
    }
    Ok(())
}

fn check_inputs(root: &Path, files: &[InputFile], stop: &Cancel) -> Result<()> {
    for entry in files {
        stop.check()?;
        let current = open_source(&root.join(&entry.relative))?
            .as_ref()
            .map(ReadIdentity::capture)
            .transpose()
            .map_err(|_| "worktree_source_changed")?;
        if current != entry.identity {
            return Err("worktree_source_changed");
        }
    }
    Ok(())
}

fn write_snapshot_tree(path: &Path, files: &[InputFile], stop: &Cancel) -> Result<String> {
    command(path, &["read-tree", "--empty"], None, stop)?;
    let files: Vec<_> = files
        .iter()
        .filter(|entry| entry.identity.is_some())
        .collect();
    if !files.is_empty() {
        let mut paths = String::new();
        for entry in &files {
            // Prefix paths with "./" so repository filenames cannot be
            // mistaken for hash-object options or special input syntax.
            paths.push_str("./");
            paths.push_str(&entry.relative);
            paths.push('\n');
        }
        let ids = input_command(paths.as_bytes(), |input| {
            command(
                path,
                &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                Some(input),
                stop,
            )
        })?;
        let ids = std::str::from_utf8(&ids)
            .map_err(|_| "worktree_git_object")?
            .lines()
            .collect::<Vec<_>>();
        if ids.len() != files.len() || ids.iter().any(|id| !valid_object(id)) {
            return Err("worktree_git_object");
        }
        let mut index = Vec::new();
        for (entry, id) in files.iter().zip(ids) {
            let mode = if entry.executable { "100755" } else { "100644" };
            write!(index, "{mode} {id}\t{}\0", entry.relative).map_err(|_| "worktree_index")?;
        }
        input_command(&index, |input| {
            command(
                path,
                &["update-index", "-z", "--index-info"],
                Some(input),
                stop,
            )
        })?;
    }
    object_id(command(path, &["write-tree"], None, stop)?)
}

fn file_names(root: &Path, excluded: &[PathBuf], stop: &Cancel) -> Result<BTreeSet<String>> {
    let names = command(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        None,
        stop,
    )?;
    let mut paths = BTreeSet::new();
    if names.is_empty() {
        return Ok(paths);
    }
    let names = names.strip_suffix(&[0]).ok_or("worktree_git_encoding")?;
    for path in names.split(|byte| *byte == 0) {
        let path = std::str::from_utf8(path).map_err(|_| "worktree_path_encoding")?;
        if local_fs::protected_path(Path::new(path))
            || excluded
                .iter()
                .any(|prefix| root.join(path).starts_with(prefix))
        {
            continue;
        }
        if !super::plan::valid_path(path) {
            return Err("worktree_path_scope");
        }
        paths.insert(path.to_owned());
        if paths.len() > MAX_FILES {
            return Err("worktree_file_limit");
        }
    }
    Ok(paths)
}

fn open_source(path: &Path) -> Result<Option<File>> {
    match local_fs::open_nofollow(path, local::read_flags()) {
        Ok(file) if local_fs::regular_single(&file).map_err(|_| "worktree_file_type")? => {
            Ok(Some(file))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        _ => Err("worktree_file_type"),
    }
}

fn copy_input(
    root: &Path,
    checkout: &Path,
    relative: &str,
    remaining: &mut usize,
) -> Result<InputFile> {
    let Some(mut source) = open_source(&root.join(relative))? else {
        return Ok(InputFile {
            relative: relative.into(),
            identity: None,
            executable: false,
        });
    };
    let identity = ReadIdentity::capture(&source).map_err(|_| "worktree_file_read")?;
    let limit = (*remaining).min(MAX_FILE_BYTES);
    if identity.size() > limit as u64 {
        return Err("worktree_copy_limit");
    }
    let executable = source.metadata().map_err(|_| "worktree_file_type")?.mode() & 0o111 != 0;
    let mut data = Vec::with_capacity(identity.size() as usize);
    Read::by_ref(&mut source)
        .take(limit as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "worktree_file_read")?;
    if data.len() > limit
        || ReadIdentity::capture(&source).map_err(|_| "worktree_source_changed")? != identity
    {
        return Err("worktree_source_changed");
    }
    let target = checkout.join(relative);
    fs::create_dir_all(target.parent().ok_or("worktree_path_scope")?)
        .map_err(|_| "worktree_directory_create")?;
    let mut dest = File::options()
        .write(true)
        .create_new(true)
        .mode(if executable { 0o700 } else { 0o600 })
        .open(&target)
        .map_err(|_| "worktree_file_create")?;
    dest.write_all(&data)
        .and_then(|()| dest.sync_all())
        .map_err(|_| "worktree_file_copy")?;
    *remaining -= data.len();
    Ok(InputFile {
        relative: relative.into(),
        identity: Some(identity),
        executable,
    })
}

fn input_command(data: &[u8], execute: impl FnOnce(File) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    local::anonymous_input(data)
        .map_err(|_| "worktree_git_input")
        .and_then(execute)
}

fn command(cwd: &Path, args: &[&str], input: Option<File>, stop: &Cancel) -> Result<Vec<u8>> {
    let mut durable = vec!["-c", "core.fsync=all"];
    durable.extend_from_slice(args);
    match git::run(cwd, &durable, input, GIT_OUTPUT, GIT_TIMEOUT, stop)? {
        GitOutput::Success(data) => Ok(data),
        GitOutput::Failure => Err("worktree_git_failed"),
        GitOutput::TooLarge => Err("worktree_git_output_limit"),
        GitOutput::Missing => Err("worktree_git_unavailable"),
    }
}

fn line(data: Vec<u8>) -> Result<String> {
    let line = String::from_utf8(data).map_err(|_| "worktree_git_encoding")?;
    let value = line.strip_suffix('\n').ok_or("worktree_git_encoding")?;
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("worktree_git_encoding");
    }
    Ok(value.into())
}

fn object_id(data: Vec<u8>) -> Result<String> {
    let value = line(data)?;
    if !valid_object(&value) {
        return Err("worktree_git_object");
    }
    Ok(value)
}

fn valid_object(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn baseline_contains_dirty_files_and_leaves_source_head_and_index_unchanged() {
        let root = Fixture(
            std::env::temp_dir().join(format!("ansa-dirty-worktree-{}", std::process::id())),
        );
        local_fs::create_private_leaf(&root.0).unwrap();
        let repo = root.0.join("source");
        let storage = root.0.join("storage");
        local_fs::create_private_leaf(&repo).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        let stop = Cancel::default();
        let git = |directory: &Path, args: &[&str]| command(directory, args, None, &stop).unwrap();
        git(&repo, &["init", "--quiet"]);
        fs::write(repo.join(".gitignore"), "scratch\n").unwrap();
        fs::write(repo.join("report"), "HEAD version\n").unwrap();
        fs::write(repo.join("removed"), "old file\n").unwrap();
        git(&repo, &["add", "--", ".gitignore", "report", "removed"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let head = git(&repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join("report"), "staged edit\n").unwrap();
        git(&repo, &["add", "--", "report"]);
        fs::write(repo.join("report"), "working edit\n").unwrap();
        fs::remove_file(repo.join("removed")).unwrap();
        fs::write(repo.join("new tool"), "untracked input\n").unwrap();
        fs::set_permissions(repo.join("new tool"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join("scratch"), "ignored\n").unwrap();
        fs::create_dir(repo.join("private-state")).unwrap();
        fs::write(repo.join("private-state/marker"), "synthetic local state\n").unwrap();
        for scratch in [
            "snapshot-index",
            "snapshot-paths.pending",
            "snapshot.json.pending",
        ] {
            File::options()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(storage.join(scratch))
                .unwrap();
        }
        let before = git(&repo, &["status", "--short"]);

        let worktree =
            Worktree::capture(&repo, &storage, &[repo.join("private-state")], &stop).unwrap();
        assert_eq!(format!("{}\n", worktree.source_head()).as_bytes(), head);
        let private_blob = line(git(worktree.path(), &["rev-parse", "HEAD:report"])).unwrap();
        assert!(command(&repo, &["cat-file", "-e", &private_blob], None, &stop).is_err());
        assert!(command(&repo, &["cat-file", "-e", worktree.baseline()], None, &stop).is_err());
        assert_eq!(
            git(worktree.path(), &["rev-list", "--count", "HEAD"]),
            b"1\n"
        );
        assert_eq!(
            git(worktree.path(), &["show", "HEAD:report"]),
            b"working edit\n"
        );
        assert_eq!(
            git(worktree.path(), &["show", "HEAD:new tool"]),
            b"untracked input\n"
        );
        assert!(!worktree.path().join("removed").exists());
        assert!(!worktree.path().join("scratch").exists());
        assert!(!worktree.path().join("private-state").exists());
        assert_eq!(
            fs::metadata(worktree.path().join("new tool"))
                .unwrap()
                .mode()
                & 0o111,
            0o100
        );
        assert_eq!(git(worktree.path(), &["status", "--short"]), b"");
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&repo, &["show", ":report"]), b"staged edit\n");
        assert_eq!(git(&repo, &["status", "--short"]), before);

        fs::write(worktree.path().join("report"), "agent edit\n").unwrap();
        let diff = git(
            worktree.path(),
            &["diff", worktree.baseline(), "--", "report"],
        );
        let diff = std::str::from_utf8(&diff).unwrap();
        assert!(diff.contains("-working edit") && diff.contains("+agent edit"));
        assert!(!diff.contains("HEAD version") && !diff.contains("staged edit"));
        assert!(matches!(
            worktree.freeze(&["new tool".into()], &stop),
            Err("worktree_change_outside_approval")
        ));
        File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(storage.join("candidate-0.pending"))
            .unwrap();
        let frozen = worktree.candidate(0, &["report".into()], &stop).unwrap();
        assert!(frozen.diff().contains("-working edit\n+agent edit"));
        fs::write(worktree.path().join("report"), "later edit\n").unwrap();
        worktree.local_branch(&frozen, &stop).unwrap();
        assert_eq!(
            git(worktree.path(), &["show", "ansa-result:report"]),
            b"agent edit\n"
        );
        assert_eq!(git(&repo, &["status", "--short"]), before);
    }
}
