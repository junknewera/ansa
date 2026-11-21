//! Read Git status without loading configuration from the source repository.
use super::{
    files::{Host, error, result},
    registry::{Definition, Outcome, Policy, Registry},
};
use crate::{
    auth::{Cancel, pkce::random_token},
    config::{Result, agents::saved_instructions},
    process::git::{self, GitOutput},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::Descriptor,
};
use ansa_linux::{fs as local_fs, local::read_flags};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        },
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const METADATA_LIMIT: usize = 8 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 64 * 1024;
const STATUS_ROWS: usize = 100;
const OBJECT_FILES: usize = 512;
const TREE_BYTES: u64 = 64 * 1024 * 1024;
const TREE_ENTRIES: usize = 20_000;
static SNAPSHOT_SLOT: Mutex<()> = Mutex::new(());

fn descriptor() -> Result<Descriptor> {
    Ok(Descriptor {
        name: "git_snapshot".into(),
        description: "Report branch, full HEAD, recent commit IDs, and staged, unstaged and untracked status for a workspace-owned Git repository. Start with an empty instruction_tokens array. To include a reported instruction scope, use list_directory there and retry with its instruction token. Status uses a sanitized configuration and omits protected paths and submodules. External Git metadata is unsupported.".into(),
        revision: "ansa_local_v1".into(),
        schema: Json::parse(r#"{"type":"object","properties":{"instruction_tokens":{"type":"array","items":{"type":"string"}}},"required":["instruction_tokens"],"additionalProperties":false}"#)?,
    })
}

pub(super) fn register_inspection(registry: &mut Registry) -> Result<()> {
    registry.register_with_policy(
        Definition {
            descriptor: descriptor()?,
            validator: Arc::new(|_| false),
            executor: Arc::new(|_| Outcome::not_started()),
        },
        Policy {
            read_only: true,
            ..Policy::default()
        },
    )
}

pub(super) fn register(registry: &mut Registry, host: Arc<Host>) -> Result<()> {
    let collect = Arc::new(move |args: &Json, stop: &Cancel| {
        args.field("instruction_tokens")
            .and_then(Json::elements)
            .and_then(|tokens| snapshot(&host, tokens, stop))
            .map_or_else(error, result)
    });
    let legacy = collect.clone();
    registry.register_with_policy(
        Definition {
            descriptor: descriptor()?,
            validator: Arc::new(|args| {
                args.field("instruction_tokens")
                    .and_then(Json::elements)
                    .is_ok_and(|tokens| {
                        tokens.len() <= 16
                            && tokens.iter().all(|token| {
                                token.text().is_ok_and(|token| {
                                    token.len() == 64
                                        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
                                })
                            })
                    })
            }),
            executor: Arc::new(move |args| legacy(args, &Cancel::default())),
        },
        Policy {
            read_only: true,
            invoke: Some(Arc::new(move |invocation, stop| {
                collect(&invocation.arguments, stop)
            })),
            ..Policy::default()
        },
    )
}

struct View(PathBuf);

impl View {
    fn create(storage: &Path) -> Result<Self> {
        let path = storage.join(format!("git-snapshot-{}", random_token(16)?));
        local_fs::create_private_leaf(&path).map_err(|_| "git_snapshot_storage")?;
        Ok(Self(path))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn read(path: &Path, max: usize) -> Result<Option<Vec<u8>>> {
    let file = match local_fs::open_nofollow(path, read_flags()) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("git_snapshot_metadata"),
    };
    let meta = file.metadata().map_err(|_| "git_snapshot_metadata")?;
    if !local_fs::regular_single(&file).map_err(|_| "git_snapshot_metadata")?
        || meta.len() > max as u64
    {
        return Err("git_snapshot_metadata");
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "git_snapshot_metadata")?;
    if bytes.len() > max {
        return Err("git_snapshot_metadata");
    }
    Ok(Some(bytes))
}

fn line(bytes: &[u8]) -> Result<&str> {
    let value = std::str::from_utf8(bytes).map_err(|_| "git_snapshot_metadata")?;
    let value = value.trim_end_matches('\n');
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("git_snapshot_metadata");
    }
    Ok(value)
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_dir(root: &Path) -> Result<PathBuf> {
    let marker = root.join(".git");
    let meta = match fs::symlink_metadata(&marker) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err("git_snapshot_not_repository");
        }
        Err(_) => return Err("git_snapshot_metadata"),
    };
    if !meta.is_dir() {
        return Err("git_snapshot_external_metadata");
    }
    Ok(marker)
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).map_err(|_| "git_snapshot_storage")
}

fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|_| "git_snapshot_storage")
}

fn copy_head(git_dir: &Path, view: &Path) -> Result<(Option<String>, Option<String>)> {
    let head_file = read(&git_dir.join("HEAD"), 4096)?.ok_or("git_snapshot_metadata")?;
    let head_line = line(&head_file)?;
    mkdir(&view.join("refs/heads"))?;
    write(&view.join("HEAD"), &head_file)?;
    let Some(branch) = head_line.strip_prefix("ref: refs/heads/") else {
        return if oid(head_line) {
            Ok((None, Some(head_line.to_owned())))
        } else {
            Err("git_snapshot_metadata")
        };
    };
    let name = Path::new(branch);
    if name.as_os_str().is_empty()
        || name
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || local_fs::protected_path(name)
        || branch.contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
    {
        return Err("git_snapshot_metadata");
    }
    let reference = Path::new("refs/heads").join(name);
    let hash = if let Some(loose) = read(&git_dir.join(&reference), 128)? {
        Some(line(&loose)?.to_owned())
    } else {
        let packed = read(&git_dir.join("packed-refs"), METADATA_LIMIT)?.unwrap_or_default();
        let reference = reference.to_str().ok_or("git_snapshot_metadata")?;
        std::str::from_utf8(&packed)
            .map_err(|_| "git_snapshot_metadata")?
            .lines()
            .filter_map(|entry| entry.split_once(' '))
            .find_map(|(id, name)| (name == reference).then(|| id.to_owned()))
    };
    if let Some(hash) = &hash {
        if !oid(hash) {
            return Err("git_snapshot_metadata");
        }
        let path = view.join(reference);
        mkdir(path.parent().ok_or("git_snapshot_metadata")?)?;
        write(&path, format!("{hash}\n").as_bytes())?;
    }
    Ok((Some(branch.to_owned()), hash))
}

fn build_view(
    root: &Path,
    view: &Path,
    objects: &mut Vec<File>,
    stop: &Cancel,
) -> Result<(Option<String>, Option<String>)> {
    let git_dir = git_dir(root)?;
    for name in [
        "reftable",
        "commondir",
        "info/sparse-checkout",
        "objects/info/alternates",
    ] {
        if !matches!(fs::symlink_metadata(git_dir.join(name)), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            return Err("git_snapshot_unsupported_repository");
        }
    }
    let (branch, head) = copy_head(&git_dir, view)?;
    if head.as_ref().is_some_and(|hash| hash.len() == 64) {
        write(
            &view.join("config"),
            b"[core]\nrepositoryformatversion=1\n[extensions]\nobjectformat=sha256\n",
        )?;
    }
    if let Some(index) = read(&git_dir.join("index"), METADATA_LIMIT)? {
        write(&view.join("index"), &index)?;
    }
    if let Some(exclude) = read(&git_dir.join("info/exclude"), METADATA_LIMIT)? {
        mkdir(&view.join("info"))?;
        write(&view.join("info/exclude"), &exclude)?;
    }
    if let Some(shallow) = read(&git_dir.join("shallow"), METADATA_LIMIT)? {
        write(&view.join("shallow"), &shallow)?;
    }
    pin_objects(
        &git_dir.join("objects"),
        &view.join("objects"),
        objects,
        stop,
    )?;
    Ok((branch, head))
}

fn pin_objects(objects: &Path, target: &Path, pinned: &mut Vec<File>, stop: &Cancel) -> Result<()> {
    mkdir(target)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let max_descriptors = object_descriptor_budget()?;
    let descriptors = PathBuf::from(format!("/proc/{}/fd", std::process::id()));
    let directories =
        local_fs::read_directory(objects, false).map_err(|_| "git_snapshot_metadata")?;
    for directory in directories {
        stop.check()?;
        if Instant::now() >= deadline {
            return Err("git_snapshot_object_limit");
        }
        let directory = directory.map_err(|_| "git_snapshot_metadata")?;
        let name = directory.name.as_encoded_bytes();
        let pack = name == b"pack";
        if !pack && !(name.len() == 2 && name.iter().all(u8::is_ascii_hexdigit)) {
            continue;
        }
        let source = objects.join(&directory.name);
        let destination = target.join(&directory.name);
        let entries =
            local_fs::read_directory(&source, false).map_err(|_| "git_snapshot_metadata")?;
        mkdir(&destination)?;
        for entry in entries {
            stop.check()?;
            if Instant::now() >= deadline {
                return Err("git_snapshot_object_limit");
            }
            let entry = entry.map_err(|_| "git_snapshot_metadata")?;
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            let valid = if pack {
                name.strip_prefix("pack-")
                    .and_then(|name| name.split_once('.'))
                    .is_some_and(|(hash, extension)| {
                        oid(hash) && matches!(extension, "idx" | "pack")
                    })
            } else {
                matches!(name.len(), 38 | 62) && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            };
            if !valid {
                continue;
            }
            if pinned.len() >= max_descriptors {
                return Err("git_snapshot_object_limit");
            }
            let file = local_fs::open_nofollow(&source.join(name), read_flags())
                .map_err(|_| "git_snapshot_object_storage")?;
            if !local_fs::regular_single(&file).map_err(|_| "git_snapshot_object_storage")? {
                return Err("git_snapshot_object_storage");
            }
            // Git opens object data on demand. The link names an owned Linux FD,
            // so replacing the source pathname cannot redirect the private view.
            symlink(
                descriptors.join(file.as_raw_fd().to_string()),
                destination.join(name),
            )
            .map_err(|_| "git_snapshot_storage")?;
            pinned.push(file);
        }
    }
    Ok(())
}

fn object_descriptor_budget() -> Result<usize> {
    const RESERVE: usize = 128;
    let limits =
        fs::read_to_string("/proc/self/limits").map_err(|_| "git_snapshot_object_storage")?;
    let soft_limit = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max open files"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or("git_snapshot_object_storage")?;
    let soft_limit = if soft_limit == "unlimited" {
        usize::MAX
    } else {
        soft_limit
            .parse::<usize>()
            .map_err(|_| "git_snapshot_object_storage")?
    };
    let entries = fs::read_dir("/proc/self/fd").map_err(|_| "git_snapshot_object_storage")?;
    let open = entries.take(4096).count();
    if open == 4096 {
        return Err("git_snapshot_object_limit");
    }
    Ok(soft_limit.saturating_sub(open + RESERVE).min(OBJECT_FILES))
}

fn output(root: &Path, view: &Path, args: &[&str], stop: &Cancel) -> Result<Option<Vec<u8>>> {
    match isolated_git(root, view, args, None, Duration::from_secs(3), stop)? {
        GitOutput::Success(bytes) => Ok(Some(bytes)),
        GitOutput::TooLarge => Ok(None),
        GitOutput::Missing => Err("git_snapshot_git_missing"),
        GitOutput::Failure => Err("git_snapshot_unsupported_repository"),
    }
}

fn isolated_git(
    root: &Path,
    view: &Path,
    args: &[&str],
    input: Option<File>,
    timeout: Duration,
    stop: &Cancel,
) -> Result<GitOutput> {
    let git_dir = view.to_str().ok_or("git_snapshot_storage")?;
    let work_tree = root.to_str().ok_or("git_snapshot_metadata")?;
    let mut options = vec![
        "--git-dir",
        git_dir,
        "--work-tree",
        work_tree,
        "-c",
        "core.attributesFile=/dev/null",
        "-c",
        "core.excludesFile=/dev/null",
        "-c",
        "protocol.allow=never",
    ];
    options.extend_from_slice(args);
    git::run(root, &options, input, OUTPUT_LIMIT, timeout, stop)
}

fn prune_ignored(
    pending: &mut Vec<PathBuf>,
    tracked_directories: &BTreeSet<&Path>,
    tree: &Path,
    view: &Path,
    deadline: Instant,
    stop: &Cancel,
) -> Result<()> {
    let mut paths = Vec::new();
    for path in pending
        .iter()
        .filter(|path| !tracked_directories.contains(path.as_path()))
    {
        paths.extend_from_slice(path.as_os_str().as_encoded_bytes());
        paths.extend_from_slice(b"/\0");
        if paths.len() > OUTPUT_LIMIT {
            return Err("git_snapshot_tree_limit");
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    let args_path = view.join("ignore-input");
    write(&args_path, &paths)?;
    let input = File::open(args_path).map_err(|_| "git_snapshot_storage")?;
    let bytes = match isolated_git(
        tree,
        view,
        &["check-ignore", "--no-index", "-z", "--stdin"],
        Some(input),
        deadline.saturating_duration_since(Instant::now()),
        stop,
    )? {
        GitOutput::Success(bytes) => bytes,
        GitOutput::Failure => return Ok(()), // Git exits 1 when no path is ignored.
        GitOutput::Missing => return Err("git_snapshot_git_missing"),
        GitOutput::TooLarge => return Err("git_snapshot_tree_limit"),
    };
    let ignored = bytes
        .split(|&byte| byte == 0)
        .filter_map(|name| name.strip_suffix(b"/"))
        .collect::<BTreeSet<_>>();
    pending.retain(|path| !ignored.contains(path.as_os_str().as_encoded_bytes()));
    Ok(())
}

#[derive(Default)]
struct TreeCoverage {
    omitted: bool,
    instruction_scopes: Vec<String>,
}

struct InstructionTokens<'a> {
    home: &'a Path,
    tokens: &'a [Json],
}

impl InstructionTokens<'_> {
    fn acknowledges(&self, directory: &Path) -> bool {
        if self.tokens.is_empty() {
            return false;
        }
        saved_instructions(directory, self.home)
            .and_then(Json::array)
            .is_ok_and(|instructions| {
                let expected = sha256(instructions.dump().as_bytes());
                self.tokens
                    .iter()
                    .any(|token| token.text() == Ok(expected.as_str()))
            })
    }
}

fn next_directory(
    directories: &mut Vec<PathBuf>,
    pending: &mut Vec<PathBuf>,
    tracked_directories: &BTreeSet<&Path>,
    tree: &Path,
    view: &Path,
    deadline: Instant,
    stop: &Cancel,
) -> Result<Option<PathBuf>> {
    if directories.is_empty() && !pending.is_empty() {
        prune_ignored(pending, tracked_directories, tree, view, deadline, stop)?;
        std::mem::swap(directories, pending);
    }
    Ok(directories.pop())
}

fn prepare_tree(
    root: &Path,
    tree: &Path,
    view: &Path,
    exclusions: &mut Vec<PathBuf>,
    acknowledgements: &InstructionTokens<'_>,
    stop: &Cancel,
) -> Result<TreeCoverage> {
    mkdir(tree)?;
    let tracked = output(tree, view, &["ls-files", "--cached", "-z"], stop)?
        .ok_or("git_snapshot_tree_limit")?;
    let tracked = tracked.split(|&byte| byte == 0).collect::<BTreeSet<_>>();
    let tracked_directories = tracked
        .iter()
        .flat_map(|path| Path::new(OsStr::from_bytes(path)).ancestors().skip(1))
        .collect::<BTreeSet<_>>();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut remaining_entries = TREE_ENTRIES;
    let mut remaining_bytes = TREE_BYTES;
    let mut directories = vec![PathBuf::new()];
    let mut pending = Vec::new();
    let mut coverage = TreeCoverage::default();
    while let Some(directory) = next_directory(
        &mut directories,
        &mut pending,
        &tracked_directories,
        tree,
        view,
        deadline,
        stop,
    )? {
        let source = root.join(&directory);
        if !directory.as_os_str().is_empty() {
            let nested_git = !matches!(fs::symlink_metadata(source.join(".git")), Err(error) if error.kind() == io::ErrorKind::NotFound);
            let instruction_scope = !matches!(fs::symlink_metadata(source.join("AGENTS.md")), Err(error) if error.kind() == io::ErrorKind::NotFound);
            if nested_git || instruction_scope && !acknowledgements.acknowledges(&source) {
                if instruction_scope
                    && !nested_git
                    && let Some(directory) = directory.to_str()
                {
                    coverage.instruction_scopes.push(directory.to_owned());
                }
                exclusions.push(source);
                coverage.omitted = true;
                continue;
            }
        }
        mkdir(&tree.join(&directory))?;
        let entries = local_fs::read_directory(&source, false).map_err(|_| "git_snapshot_tree")?;
        for entry in entries {
            stop.check()?;
            remaining_entries = remaining_entries
                .checked_sub(1)
                .ok_or("git_snapshot_tree_limit")?;
            if Instant::now() >= deadline || exclusions.len() > 128 {
                return Err("git_snapshot_tree_limit");
            }
            let entry = entry.map_err(|_| "git_snapshot_tree")?;
            let path = directory.join(&entry.name);
            let source_path = root.join(&path);
            if entry.name == ".git"
                || local_fs::protected_path(&path)
                || exclusions
                    .iter()
                    .any(|excluded| source_path.starts_with(excluded))
            {
                continue;
            }
            if entry.directory {
                pending.push(path);
                continue;
            }
            if !entry.regular {
                exclusions.push(source_path);
                coverage.omitted = true;
                continue;
            }
            let destination = tree.join(&path);
            if !tracked.contains(path.as_os_str().as_encoded_bytes())
                && !matches!(entry.name.to_str(), Some(".gitignore" | ".gitattributes"))
            {
                File::create_new(destination).map_err(|_| "git_snapshot_storage")?;
                continue;
            }
            let file = local_fs::open_nofollow(&source_path, read_flags())
                .map_err(|_| "git_snapshot_tree")?;
            if !local_fs::regular_single(&file).map_err(|_| "git_snapshot_tree")? {
                exclusions.push(source_path);
                coverage.omitted = true;
                continue;
            }
            let metadata = file.metadata().map_err(|_| "git_snapshot_tree")?;
            let length = metadata.len();
            remaining_bytes = remaining_bytes
                .checked_sub(length)
                .ok_or("git_snapshot_tree_limit")?;
            let mut destination =
                File::create_new(destination).map_err(|_| "git_snapshot_storage")?;
            let mut file = file.take(length + 1);
            let mut copied = 0;
            let mut buffer = [0; 32 * 1024];
            loop {
                stop.check()?;
                if Instant::now() >= deadline {
                    return Err("git_snapshot_tree_limit");
                }
                let size = file.read(&mut buffer).map_err(|_| "git_snapshot_tree")?;
                if size == 0 {
                    break;
                }
                copied += size as u64;
                if copied > length {
                    return Err("git_snapshot_tree");
                }
                destination
                    .write_all(&buffer[..size])
                    .map_err(|_| "git_snapshot_storage")?;
            }
            if copied != length {
                return Err("git_snapshot_tree");
            }
            destination
                .set_permissions(fs::Permissions::from_mode(
                    0o600 | (metadata.permissions().mode() & 0o111),
                ))
                .map_err(|_| "git_snapshot_storage")?;
        }
    }
    if exclusions.len() > 128 {
        return Err("git_snapshot_tree_limit");
    }
    coverage.instruction_scopes.sort();
    Ok(coverage)
}

fn report_status(
    root: &Path,
    tree: &Path,
    view: &Path,
    exclusions: &[PathBuf],
    stop: &Cancel,
) -> Result<(Vec<serde_json::Value>, bool)> {
    let mut args = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=all",
        "--no-renames",
        "--",
        ".",
        ":(exclude,glob)**/.env",
        ":(exclude,glob)**/.env.*",
        ":(exclude,glob)**/openrouter.key",
        ":(exclude,glob)**/openrouter.key.pending-*",
        ":(exclude,glob)**/.codex",
        ":(exclude,glob)**/.codex/**",
        ":(exclude,glob)**/.claude",
        ":(exclude,glob)**/.claude/**",
        ":(exclude,glob)**/.gemini",
        ":(exclude,glob)**/.gemini/**",
    ];
    let mut protected = Vec::new();
    for directory in exclusions {
        if let Ok(path) = directory.strip_prefix(root) {
            let path = path
                .to_str()
                .filter(|path| !path.is_empty())
                .ok_or("git_snapshot_scope")?;
            protected.push(format!(":(exclude,literal){path}"));
        }
    }
    args.extend(protected.iter().map(String::as_str));
    let Some(status) = output(tree, view, &args, stop)? else {
        return Ok((Vec::new(), true));
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    for row in status
        .split(|&byte| byte == 0)
        .filter(|row| !row.is_empty())
    {
        let [index, worktree, b' ', bytes @ ..] = row else {
            return Err("git_snapshot_status");
        };
        let Ok(name) = std::str::from_utf8(bytes) else {
            truncated = true;
            continue;
        };
        let path = Path::new(name);
        if name.is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(value) if value != ".git"))
            || local_fs::protected_path(path)
            || exclusions
                .iter()
                .any(|directory| root.join(path).starts_with(directory))
        {
            continue;
        }
        if entries.len() == STATUS_ROWS {
            truncated = true;
            break;
        }
        entries.push(serde_json::json!({
            "path": name,
            "index": char::from(*index),
            "worktree": char::from(*worktree),
            "untracked": *index == b'?' && *worktree == b'?',
        }));
    }
    Ok((entries, truncated))
}

fn snapshot(host: &Host, tokens: &[Json], stop: &Cancel) -> Result<Json> {
    let _slot = SNAPSHOT_SLOT.try_lock().map_err(|_| "git_snapshot_busy")?;
    let root = &host.settings.cwd;
    let exclusions = [
        host.auth.clone(),
        host.session.clone(),
        host.settings.directory.join("auth"),
        host.settings.directory.join("openrouter"),
        host.settings.directory.join("sessions"),
        host.settings.directory.join("harness-profiles"),
        host.settings.directory.join("terminal-workspaces"),
    ];
    if host.protected_path(root) || exclusions.iter().any(|path| root.starts_with(path)) {
        return Err("git_snapshot_scope");
    }
    collect_with_tokens(
        root,
        &host.session,
        &exclusions,
        &InstructionTokens {
            home: &host.settings.home,
            tokens,
        },
        stop,
    )
}

fn collect_with_tokens(
    root: &Path,
    storage: &Path,
    exclusions: &[PathBuf],
    acknowledgements: &InstructionTokens<'_>,
    stop: &Cancel,
) -> Result<Json> {
    stop.check()?;
    if root.canonicalize().map_err(|_| "git_snapshot_scope")? != root {
        return Err("git_snapshot_scope");
    }
    let view = View::create(storage)?;
    let mut objects = Vec::new();
    let (branch, head) = build_view(root, &view.0, &mut objects, stop)?;
    let tree = view.0.join("worktree");
    mkdir(&tree)?;
    let mut recent = Vec::new();
    if let Some(head) = &head {
        let history = output(
            &tree,
            &view.0,
            &[
                "log",
                "--format=%H",
                "-n5",
                "--no-show-signature",
                "--no-decorate",
                "HEAD",
                "--",
            ],
            stop,
        )?
        .ok_or("git_snapshot_history")?;
        let history = std::str::from_utf8(&history).map_err(|_| "git_snapshot_history")?;
        recent.extend(history.lines().map(str::to_owned));
        if recent.first() != Some(head) || recent.iter().any(|id| !oid(id)) {
            return Err("git_snapshot_history");
        }
    }
    let mut exclusions = exclusions.to_vec();
    if view.0.starts_with(root) {
        exclusions.push(view.0.clone());
    }
    let coverage = prepare_tree(
        root,
        &tree,
        &view.0,
        &mut exclusions,
        acknowledgements,
        stop,
    )?;
    let (entries, truncated) = report_status(root, &tree, &view.0, &exclusions, stop)?;
    drop(objects);
    Json::parse(
        &serde_json::json!({
            "branch": branch,
            "head": head,
            "recent_commits": recent,
            "entries": entries,
            "truncated": truncated,
            "status_paths_omitted": coverage.omitted,
            "excluded_instruction_scopes": coverage.instruction_scopes,
            "status_semantics": "Sanitized best-effort status. Repository filters and local configuration are ignored. Protected paths, narrower instruction scopes, links and submodules are excluded.",
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(root: &Path, storage: &Path, exclusions: &[PathBuf], stop: &Cancel) -> Result<Json> {
        collect_with_tokens(
            root,
            storage,
            exclusions,
            &InstructionTokens {
                home: storage,
                tokens: &[],
            },
            stop,
        )
    }

    fn fixture_root() -> View {
        View::create(&std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let GitOutput::Success(bytes) = super::git::run(
            root,
            args,
            None,
            OUTPUT_LIMIT,
            Duration::from_secs(3),
            &Cancel::default(),
        )
        .unwrap() else {
            panic!("fixture Git command failed: {args:?}")
        };
        String::from_utf8(bytes).unwrap()
    }

    fn seeded(root: &Path) -> String {
        git(root, &["init", "-b", "topic"]);
        for path in ["staged.txt", "modified.txt"] {
            fs::write(root.join(path), "before\n").unwrap();
        }
        fs::write(root.join(".gitattributes"), "*.txt filter=fixture\n").unwrap();
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "seed",
            ],
        );
        git(root, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    #[test]
    fn snapshot_reports_changes_without_loading_local_filters_or_updating_index() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        let head = seeded(&root);
        fs::write(root.join("staged.txt"), "staged changed\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        fs::write(root.join("modified.txt"), "worktree changed\n").unwrap();
        fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        let excluded = root.join("private");
        mkdir(&excluded).unwrap();
        fs::write(excluded.join("hidden.txt"), "omitted\n").unwrap();
        let marker = fixture.0.join("ran-external-filter");
        git(
            &root,
            &[
                "config",
                "filter.fixture.clean",
                &format!("touch '{}'; cat", marker.display()),
            ],
        );
        let index = fs::read(root.join(".git/index")).unwrap();
        let value = collect(&root, &storage, &[excluded], &Cancel::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&value.dump()).unwrap();
        assert_eq!(value["branch"], "topic");
        assert_eq!(value["head"], head);
        assert_eq!(value["recent_commits"], serde_json::json!([head]));
        assert_eq!(
            value["entries"],
            serde_json::json!([
                {"path":"modified.txt","index":" ","worktree":"M","untracked":false},
                {"path":"staged.txt","index":"M","worktree":" ","untracked":false},
                {"path":"untracked.txt","index":"?","worktree":"?","untracked":true}
            ])
        );
        assert!(!marker.exists());
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read_dir(&storage).unwrap().count(), 0);
    }

    #[test]
    fn snapshot_rejects_external_worktree_metadata_and_handles_nonrepositories() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let linked = fixture.0.join("linked");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        seeded(&root);
        git(&root, &["pack-refs", "--all"]);
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(
            collect(&linked, &storage, &[], &Cancel::default()).unwrap_err(),
            "git_snapshot_external_metadata"
        );
        assert_eq!(
            collect(&fixture.0, &storage, &[], &Cancel::default()).unwrap_err(),
            "git_snapshot_not_repository"
        );
        let cancelled = Cancel::default();
        cancelled.cancel();
        assert_eq!(
            collect(&root, &storage, &[], &cancelled).unwrap_err(),
            "auth_cancelled"
        );
    }

    #[test]
    fn snapshot_omits_nested_instruction_scopes_and_tracked_hardlinks() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        seeded(&root);
        let nested = root.join("nested");
        mkdir(&nested).unwrap();
        fs::write(nested.join("tracked.txt"), "before\n").unwrap();
        git(&root, &["add", "nested"]);
        fs::write(nested.join("AGENTS.md"), "Separate instructions\n").unwrap();
        fs::write(nested.join("tracked.txt"), "after\n").unwrap();
        let outside = fixture.0.join("external-file");
        fs::write(&outside, "outside contents\n").unwrap();
        fs::remove_file(root.join("modified.txt")).unwrap();
        fs::hard_link(&outside, root.join("modified.txt")).unwrap();
        let value = collect(&root, &storage, &[], &Cancel::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&value.dump()).unwrap();
        assert_eq!(value["entries"], serde_json::json!([]));
        assert_eq!(value["status_paths_omitted"], true);
        assert_eq!(
            value["excluded_instruction_scopes"],
            serde_json::json!(["nested"])
        );
        let instructions = Json::array(saved_instructions(&nested, &storage).unwrap()).unwrap();
        let acknowledged = Json::string(&sha256(instructions.dump().as_bytes())).unwrap();
        for (token, included) in [
            (Json::string(&"0".repeat(64)).unwrap(), false),
            (acknowledged, true),
        ] {
            let value = collect_with_tokens(
                &root,
                &storage,
                &[],
                &InstructionTokens {
                    home: &storage,
                    tokens: &[token],
                },
                &Cancel::default(),
            )
            .unwrap();
            let value: serde_json::Value = serde_json::from_str(&value.dump()).unwrap();
            assert_eq!(
                value["excluded_instruction_scopes"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                included
            );
            let entries = value["entries"].as_array().unwrap();
            assert_eq!(
                entries
                    .iter()
                    .any(|entry| entry["path"] == "nested/tracked.txt"),
                included
            );
            assert!(!entries.iter().any(|entry| entry["path"] == "modified.txt"));
        }
    }

    #[test]
    fn snapshot_prunes_ignored_build_subtree_before_opening_its_large_files() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        seeded(&root);
        fs::write(root.join(".gitignore"), "build-quality/\n").unwrap();
        let build = root.join("build-quality");
        mkdir(&build).unwrap();
        File::create_new(build.join(".gitattributes"))
            .unwrap()
            .set_len(TREE_BYTES + 1)
            .unwrap();
        let value = collect(&root, &storage, &[], &Cancel::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&value.dump()).unwrap();
        assert_eq!(
            value["entries"],
            serde_json::json!([
                {"path":".gitignore","index":"?","worktree":"?","untracked":true}
            ])
        );
    }

    #[test]
    fn snapshot_rejects_hardlinked_object_storage() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        let head = seeded(&root);
        fs::hard_link(
            root.join(".git/objects").join(&head[..2]).join(&head[2..]),
            fixture.0.join("external-object"),
        )
        .unwrap();
        assert_eq!(
            collect(&root, &storage, &[], &Cancel::default()).unwrap_err(),
            "git_snapshot_object_storage"
        );
    }

    #[test]
    fn snapshot_reads_packed_objects_through_pinned_descriptors() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        let head = seeded(&root);
        git(&root, &["repack", "-ad"]);
        git(&root, &["pack-refs", "--all"]);
        assert!(
            !root
                .join(".git/objects")
                .join(&head[..2])
                .join(&head[2..])
                .exists()
        );
        let value = collect(&root, &storage, &[], &Cancel::default()).unwrap();
        assert_eq!(value.text_field("head").unwrap(), head);
    }

    #[test]
    fn private_view_ignores_object_replacement_after_pinning_and_rejects_source_symlinks() {
        let fixture = fixture_root();
        let root = fixture.0.join("repo");
        let storage = fixture.0.join("storage");
        mkdir(&root).unwrap();
        local_fs::create_private_leaf(&storage).unwrap();
        let head = seeded(&root);
        let source = root.join(".git/objects").join(&head[..2]).join(&head[2..]);
        let view = View::create(&storage).unwrap();
        let stop = Cancel::default();
        let mut objects = Vec::new();
        build_view(&root, &view.0, &mut objects, &stop).unwrap();
        let outside = fixture.0.join("outside-object");
        fs::write(&outside, "not a Git object").unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();
        let tree = fixture.0.join("scratch");
        mkdir(&tree).unwrap();
        let value = output(
            &tree,
            &view.0,
            &["log", "-1", "--format=%H", "HEAD", "--"],
            &stop,
        )
        .unwrap()
        .unwrap();
        assert_eq!(String::from_utf8(value).unwrap().trim(), head);
        assert_eq!(
            collect(&root, &storage, &[], &stop).unwrap_err(),
            "git_snapshot_object_storage"
        );
    }
}
