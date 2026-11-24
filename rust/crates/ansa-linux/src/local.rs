//! Owned v1 local/evidence files. No parsing or runtime policy here.
use rustix::fs::{Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};
pub type ReadFlags = OFlags;
pub fn read_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK
}
pub fn append_flags() -> OFlags {
    OFlags::WRONLY | OFlags::APPEND | OFlags::NONBLOCK
}
pub fn read(directory: &File, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
    read_impl(directory, name, limit, true, false)
}
/// Bounded public UI palette data, anchored to an already opened directory.
pub fn read_regular(directory: &File, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
    read_impl(directory, name, limit, false, false)
}
fn read_impl(
    directory: &File,
    name: &str,
    limit: usize,
    private: bool,
    allow_empty: bool,
) -> io::Result<Option<Vec<u8>>> {
    if name.contains('/') || name.contains('\0') {
        return Err(io::Error::other("local_file_name"));
    }
    let fd = match rustix::fs::openat(
        directory,
        name,
        read_flags() | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || private
            && (meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0)
        || !allow_empty && meta.len() == 0
        || meta.len() > limit as u64
    {
        return Err(io::Error::other("local_file_permissions"));
    }
    let mut bytes = vec![];
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("local_file_limit"));
    }
    Ok(Some(bytes))
}
pub fn names(directory: &File, limit: usize) -> io::Result<(Vec<std::ffi::OsString>, bool)> {
    use std::os::unix::ffi::OsStrExt;
    if !(1..=8192).contains(&limit) {
        return Err(io::Error::other("local_directory_limit"));
    }
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        if names.len() == limit {
            return Ok((names, true));
        }
        names.push(std::ffi::OsStr::from_bytes(name).to_os_string());
    }
    Ok((names, false))
}

pub fn visit_names(
    directory: &File,
    mut visit: impl FnMut(&std::ffi::OsStr) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if !matches!(name, b"." | b"..") {
            visit(std::ffi::OsStr::from_bytes(name))?;
        }
    }
    Ok(())
}

pub fn count_names(directory: &File, excluded: &[&str], limit: usize) -> io::Result<usize> {
    let mut count = 0;
    for entry in rustix::fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if !matches!(name, b"." | b"..") && !excluded.iter().any(|value| name == value.as_bytes()) {
            count += 1;
            if count == limit {
                break;
            }
        }
    }
    Ok(count)
}
pub fn entry_exists(directory: &File, name: &str) -> io::Result<bool> {
    match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(rustix::io::Errno::NOENT) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub fn publish_directory(directory: &File, pending: &str, name: &str) -> io::Result<()> {
    if [pending, name]
        .into_iter()
        .any(|value| value.is_empty() || matches!(value, "." | "..") || value.contains(['/', '\0']))
    {
        return Err(io::Error::other("local_file_name"));
    }
    rustix::fs::renameat_with(
        directory,
        pending,
        directory,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    directory.sync_all()
}

pub fn create(directory: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    if name.contains('/') || name.contains('\0') {
        return Err(io::Error::other("local_file_name"));
    }
    let pending = format!("{name}.pending");
    let fd = rustix::fs::openat(
        directory,
        pending.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut file = File::from(fd);
    file.write_all(bytes)?;
    file.sync_all()?;
    rustix::fs::renameat_with(
        directory,
        pending.as_str(),
        directory,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    directory.sync_all()
}

pub fn anonymous_input(bytes: &[u8]) -> io::Result<File> {
    let mut file = File::from(rustix::fs::memfd_create(
        "ansa-input",
        rustix::fs::MemfdFlags::CLOEXEC,
    )?);
    file.write_all(bytes)?;
    file.rewind()?;
    Ok(file)
}

/// Retry publication of an immutable record while holding its owner's lock.
/// A pending file has never been visible as the final record.
pub fn publish_recovered(directory: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    let limit = bytes.len().max(4096);
    if let Some(existing) = read(directory, name, limit)? {
        if existing != bytes {
            return Err(io::Error::other("local_record_conflict"));
        }
        sync_regular(directory, name)?;
        return directory.sync_all();
    }
    let pending = format!("{name}.pending");
    match sync_regular(directory, &pending) {
        Ok(()) => unlink(directory, &pending)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    create(directory, name, bytes)
}
/// Sync an owned immutable output through the same anchored, nofollow boundary.
pub fn sync_regular(directory: &File, name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return Err(io::Error::other("local_file_name"));
    }
    let fd = rustix::fs::openat(
        directory,
        name,
        read_flags() | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other("local_file_permissions"));
    }
    file.sync_all()
}
/// Atomically publish the small UI preference file inside an owned private
/// directory. It carries no runtime state and has last-writer-wins semantics.
pub fn replace_ui(directory: &File, bytes: &[u8], suffix: &str) -> io::Result<()> {
    replace_preference(directory, "ui.json", bytes, suffix)
}

pub fn replace_generation(directory: &File, bytes: &[u8], suffix: &str) -> io::Result<()> {
    replace_preference(directory, "generation.json", bytes, suffix)
}

pub fn replace_autocompact(directory: &File, bytes: &[u8], suffix: &str) -> io::Result<()> {
    replace_preference(directory, "autocompact.json", bytes, suffix)
}

fn replace_preference(directory: &File, name: &str, bytes: &[u8], suffix: &str) -> io::Result<()> {
    let meta = directory.metadata()?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || bytes.is_empty()
        || bytes.len() > 4096
        || suffix.is_empty()
        || suffix.len() > 64
        || !suffix
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'-')
    {
        return Err(io::Error::other("ui_preferences_contract"));
    }
    // Validate any existing destination before publication, including its link
    // count and owner. This never follows a substituted symlink.
    let _ = read_regular(directory, name, 4096)?;
    write_pending(directory, name, bytes, suffix, false)
}

/// Publish a private record without replacing a concurrent writer's record.
/// The caller verifies an existing destination on AlreadyExists.
pub fn create_immutable(
    directory: &File,
    name: &str,
    bytes: &[u8],
    suffix: &str,
) -> io::Result<()> {
    if [name, suffix].into_iter().any(|value| {
        value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return Err(io::Error::other("local_file_name"));
    }
    write_pending(directory, name, bytes, suffix, true)
}

fn write_pending(
    directory: &File,
    name: &str,
    bytes: &[u8],
    suffix: &str,
    immutable: bool,
) -> io::Result<()> {
    let pending = format!(".{name}.pending-{suffix}");
    let fd = rustix::fs::openat(
        directory,
        pending.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    struct Cleanup<'a>(&'a File, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = rustix::fs::unlinkat(self.0, self.1, rustix::fs::AtFlags::empty());
        }
    }
    let _cleanup = Cleanup(directory, &pending);
    let mut file = File::from(fd);
    file.write_all(bytes)?;
    file.sync_all()?;
    if immutable {
        rustix::fs::renameat_with(
            directory,
            pending.as_str(),
            directory,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
    } else {
        rustix::fs::renameat(directory, pending.as_str(), directory, name)?;
    }
    directory.sync_all()
}
pub fn lock(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::CREATE,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
}

/// Reacquire an already initialized store without creating any lock file.
pub fn lock_existing(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::empty(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
}

pub fn lock_shared(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::CREATE,
        rustix::fs::FlockOperation::NonBlockingLockShared,
    )
}

pub fn lock_new(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::CREATE | OFlags::EXCL,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
}

pub const ACTIVE_TERMINAL_SCOPE: &str = "active-terminal-scope.json";
/// Pin the exact marker for inspection and operator confirmation.
pub fn terminal_marker_stamp(directory: &File) -> io::Result<std::fs::Metadata> {
    let fd = rustix::fs::openat(
        directory,
        ACTIVE_TERMINAL_SCOPE,
        read_flags() | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    File::from(fd).metadata()
}
const WRITER_PREFIX: &str = "workspace-writer-";
const ROOT_WRITER_PREFIX: &str = "workspace-root-writer-";

pub fn workspace_records(
    registry: &Path,
    workspace: &Path,
) -> io::Result<(File, File, (u64, u64))> {
    if registry.starts_with(workspace)
        || !registry.is_absolute()
        || crate::fs::normalize(registry) != registry
    {
        return Err(io::Error::other("terminal_workspace_registry"));
    }
    let workspace = crate::fs::open_nofollow(workspace, read_flags())?;
    let (directory, identity) = workspace_entry(registry, &workspace)?;
    Ok((workspace, directory, identity))
}

fn workspace_entry(registry: &Path, workspace: &File) -> io::Result<(File, (u64, u64))> {
    let metadata = workspace.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::other("terminal_workspace_identity"));
    }
    let identity = (metadata.dev(), metadata.ino());
    let path = registry.join(format!("{:x}-{:x}", identity.0, identity.1));
    crate::fs::create_private(&path)?;
    Ok((crate::fs::private_directory(&path)?, identity))
}

fn ensure_no_writers(directory: &File, roots_only: bool) -> Result<(), &'static str> {
    ensure_no_other_writers(directory, roots_only, None, None)
}

fn ensure_no_other_writers(
    directory: &File,
    roots_only: bool,
    operation: Option<&str>,
    terminal_marker: Option<&[u8]>,
) -> Result<(), &'static str> {
    let owned = operation.map(|operation| writer_marker(operation, !roots_only));
    if let Some((name, operation)) = owned.as_deref().zip(operation)
        && read(directory, name, 64)
            .map_err(|_| "terminal_workspace_evidence")?
            .as_deref()
            != Some(operation.as_bytes())
    {
        return Err("terminal_workspace_quarantined");
    }
    let (entries, more) = names(directory, 256).map_err(|_| "terminal_workspace_evidence")?;
    if more
        || entries.iter().any(|entry| {
            let name = entry.as_encoded_bytes();
            owned.as_ref().is_none_or(|owned| name != owned.as_bytes())
                && (name.starts_with(ROOT_WRITER_PREFIX.as_bytes())
                    || !roots_only
                        && (name.starts_with(WRITER_PREFIX.as_bytes())
                            || name.starts_with(b"ordinary-writer-")))
        })
    {
        return Err("terminal_workspace_quarantined");
    }
    let active =
        read(directory, ACTIVE_TERMINAL_SCOPE, 8192).map_err(|_| "terminal_workspace_evidence")?;
    if active.as_deref() != terminal_marker {
        return Err("terminal_workspace_quarantined");
    }
    Ok(())
}

/// Directory and admission locks for one operation. Descendant workspaces keep
/// a shared lock on eligible ancestors and an exclusive lock at each write root.
pub struct WorkspaceWrite {
    directory: File,
    registry: std::path::PathBuf,
    scopes: Vec<std::sync::Arc<WriteScope>>,
    operation: Option<String>,
    writer: bool,
}

struct WriteScope {
    record: File,
    _lock: File,
    _presence: Option<File>,
    identity: (u64, u64),
    root: bool,
}

impl WorkspaceWrite {
    pub fn acquire(registry: &Path, workspace: &Path, target: &Path) -> Result<Self, &'static str> {
        Self::acquire_tree(registry, workspace, target, true)
    }

    fn acquire_tree(
        registry: &Path,
        workspace: &Path,
        target: &Path,
        writer: bool,
    ) -> Result<Self, &'static str> {
        if !registry.is_absolute()
            || crate::fs::normalize(registry) != registry
            || !target.is_absolute()
            || crate::fs::normalize(target) != target
            || !workspace.is_absolute()
            || crate::fs::normalize(workspace) != workspace
        {
            return Err("terminal_workspace_registry");
        }
        let mut scopes = Vec::new();
        if !target.starts_with(workspace) && !workspace.starts_with(target) {
            open_workspace_tree(registry, workspace, &mut scopes, writer, false)?;
        }
        let directory = open_workspace_tree(registry, target, &mut scopes, writer, false)?;
        Ok(Self {
            directory,
            registry: registry.to_path_buf(),
            scopes,
            operation: None,
            writer,
        })
    }

    /// Open a path covered by an existing writer lease. Both operations
    /// retain the same locks, and each publishes its own recovery marker.
    pub fn write_descendant(
        &self,
        relative: &Path,
        terminal_marker: Option<&[u8]>,
    ) -> Result<Self, &'static str> {
        if !self.writer {
            return Err("terminal_workspace_evidence");
        }
        let operation = self
            .operation
            .as_deref()
            .ok_or("terminal_workspace_evidence")?;
        for scope in &self.scopes {
            ensure_no_other_writers(
                &scope.record,
                !scope.root,
                Some(operation),
                terminal_marker.filter(|_| scope.root),
            )?;
        }
        let mut directory = self
            .directory
            .try_clone()
            .map_err(|_| "terminal_workspace_identity")?;
        let mut scopes = self.scopes.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let std::path::Component::Normal(name) = component else {
                return Err("terminal_workspace_identity");
            };
            directory = crate::fs::open_relative_directory(&directory, Path::new(name))
                .map_err(|_| "terminal_workspace_identity")?;
            hold_scope(
                &self.registry,
                &directory,
                components.peek().is_none(),
                &mut scopes,
                true,
                false,
            )?;
        }
        Ok(Self {
            directory,
            registry: self.registry.clone(),
            scopes,
            operation: None,
            writer: true,
        })
    }

    pub fn begin(&mut self, operation: &str) -> Result<(), &'static str> {
        if self.operation.is_some() || !valid_writer_operation(operation) {
            return Err("terminal_workspace_evidence");
        }
        for (index, scope) in self.scopes.iter().enumerate() {
            let marker = writer_marker(operation, scope.root);
            if create(&scope.record, &marker, operation.as_bytes()).is_err() {
                // No effect has started. Remove only publications from this attempt.
                for previous in &self.scopes[..index] {
                    let _ = unlink(&previous.record, &writer_marker(operation, previous.root));
                }
                return Err("terminal_workspace_evidence");
            }
        }
        self.operation = Some(operation.to_owned());
        Ok(())
    }

    pub fn directory(&self) -> &File {
        &self.directory
    }

    pub fn records(&self) -> impl Iterator<Item = ((u64, u64), bool)> + '_ {
        self.scopes.iter().map(|scope| (scope.identity, scope.root))
    }

    pub fn finish(self) -> Result<(), &'static str> {
        let Some(operation) = self.operation else {
            return Ok(());
        };
        let mut known = true;
        for scope in self.scopes.iter() {
            known &= remove_writer(&scope.record, &operation, scope.root).is_ok();
        }
        known.then_some(()).ok_or("terminal_workspace_evidence")
    }

    /// The exclusive root lock remains held until the final marker unlink is durable.
    pub fn finish_terminal(self, marker: &[u8]) -> Result<(), &'static str> {
        let operation = self
            .operation
            .as_deref()
            .ok_or("terminal_workspace_evidence")?;
        retire_terminal_markers(&self.scopes, marker, operation)
    }

    pub fn start_terminal(&mut self, marker: &[u8], operation: &str) -> Result<(), &'static str> {
        let root = self
            .scopes
            .iter()
            .find(|s| s.root)
            .ok_or("terminal_workspace_identity")?;
        publish_recovered(&root.record, ACTIVE_TERMINAL_SCOPE, marker)
            .map_err(|_| "terminal_workspace_evidence")?;
        if let Err(error) = self.begin(operation) {
            retire_terminal_markers(&self.scopes, marker, operation)?;
            return Err(error);
        }
        Ok(())
    }
}

/// Recovery can only remove a verified terminal's own publications. This path
/// never returns a usable writer lease or removes another operation's records.
pub fn retire_terminal_claim(
    registry: &Path,
    workspace: &Path,
    operation: &str,
    verify: impl FnOnce(&File, &File) -> Result<Vec<u8>, &'static str>,
) -> Result<(), &'static str> {
    if !workspace.is_absolute()
        || crate::fs::normalize(workspace) != workspace
        || registry.starts_with(workspace)
        || !registry.is_absolute()
        || crate::fs::normalize(registry) != registry
        || !valid_writer_operation(operation)
    {
        return Err("terminal_workspace_registry");
    }
    let mut scopes = Vec::new();
    let directory = open_workspace_tree(registry, workspace, &mut scopes, true, true)?;
    let root = scopes
        .iter()
        .find(|scope| scope.root)
        .ok_or("terminal_workspace_identity")?;
    let marker = verify(&directory, &root.record)?;
    retire_terminal_markers(&scopes, &marker, operation)
}

fn retire_terminal_markers(
    scopes: &[std::sync::Arc<WriteScope>],
    marker: &[u8],
    operation: &str,
) -> Result<(), &'static str> {
    let root = scopes
        .iter()
        .find(|scope| scope.root)
        .ok_or("terminal_workspace_identity")?;
    if read(&root.record, ACTIVE_TERMINAL_SCOPE, 8192)
        .map_err(|_| "terminal_workspace_evidence")?
        .as_deref()
        != Some(marker)
    {
        return Err("terminal_workspace_evidence");
    }
    for scope in scopes {
        let pending = format!("{}.pending", writer_marker(operation, scope.root));
        if let Some(bytes) = read_impl(&scope.record, &pending, 64, true, true)
            .map_err(|_| "terminal_workspace_evidence")?
        {
            if !operation.as_bytes().starts_with(&bytes) {
                return Err("terminal_workspace_evidence");
            }
            unlink(&scope.record, &pending).map_err(|_| "terminal_workspace_evidence")?;
        }
        remove_writer(&scope.record, operation, scope.root)
            .map_err(|_| "terminal_workspace_evidence")?;
    }
    unlink(&root.record, ACTIVE_TERMINAL_SCOPE).map_err(|_| "terminal_workspace_evidence")
}

fn valid_writer_operation(operation: &str) -> bool {
    operation.len() == 64 && operation.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn writer_marker(operation: &str, root: bool) -> String {
    let prefix = if root {
        ROOT_WRITER_PREFIX
    } else {
        WRITER_PREFIX
    };
    format!("{prefix}{operation}.v1")
}

fn remove_writer(directory: &File, operation: &str, root: bool) -> io::Result<()> {
    let marker = writer_marker(operation, root);
    match read(directory, &marker, 64)? {
        Some(value) if value == operation.as_bytes() => unlink(directory, &marker),
        None => Ok(()),
        Some(_) => Err(io::Error::other("terminal_workspace_evidence")),
    }
}

/// Reconcile a known outcome using the original scope identities from its
/// verified process request, even if the paths have since moved.
pub fn reconcile_writer(
    registry: &Path,
    operation: &str,
    identity: (u64, u64),
    root: bool,
) -> io::Result<()> {
    if !valid_writer_operation(operation) {
        return Err(io::Error::other("terminal_workspace_evidence"));
    }
    let path = registry.join(format!("{:x}-{:x}", identity.0, identity.1));
    let directory = crate::fs::private_directory(&path)?;
    remove_writer(&directory, operation, root)
}

fn open_workspace_tree(
    registry: &Path,
    target: &Path,
    scopes: &mut Vec<std::sync::Arc<WriteScope>>,
    writer: bool,
    retiring_terminal: bool,
) -> Result<File, &'static str> {
    let mut directory = crate::fs::open_nofollow(Path::new("/"), read_flags())
        .map_err(|_| "terminal_workspace_identity")?;
    let mut path = std::path::PathBuf::from("/");
    hold_scope(
        registry,
        &directory,
        path == target,
        scopes,
        writer,
        retiring_terminal,
    )?;
    for name in target.iter().skip(1) {
        path.push(name);
        directory = crate::fs::open_relative_directory(&directory, Path::new(name))
            .map_err(|_| "terminal_workspace_identity")?;
        hold_scope(
            registry,
            &directory,
            path == target,
            scopes,
            writer,
            retiring_terminal,
        )?;
    }
    Ok(directory)
}

fn hold_scope(
    registry: &Path,
    directory: &File,
    root: bool,
    scopes: &mut Vec<std::sync::Arc<WriteScope>>,
    writer: bool,
    retiring_terminal: bool,
) -> Result<(), &'static str> {
    let (record, identity) =
        workspace_entry(registry, directory).map_err(|_| "terminal_workspace_registry")?;
    if let Some(index) = scopes.iter().position(|scope| scope.identity == identity) {
        if !root || scopes[index].root {
            return Ok(());
        }
        scopes.swap_remove(index);
    }
    let lock = if root && writer {
        lock(&record)
    } else {
        lock_shared(&record)
    }
    .map_err(|_| "terminal_workspace_busy")?;
    // Root readers may share the main lock. A separate admission lane
    // keeps them mutually exclusive with writers in descendant scopes.
    let presence = if writer != root {
        Some(workspace_presence(&record, writer).map_err(|_| "terminal_workspace_busy")?)
    } else {
        None
    };
    if !retiring_terminal {
        ensure_no_writers(&record, !root)?;
    }
    scopes.push(std::sync::Arc::new(WriteScope {
        record,
        _lock: lock,
        _presence: presence,
        identity,
        root,
    }));
    Ok(())
}

pub fn workspace_participant(
    registry: &Path,
    workspace: &Path,
) -> Result<WorkspaceWrite, &'static str> {
    WorkspaceWrite::acquire_tree(registry, workspace, workspace, false)
}

fn workspace_presence(directory: &File, writer: bool) -> io::Result<File> {
    use rustix::fs::FlockOperation::{NonBlockingLockExclusive, NonBlockingLockShared};
    use std::time::{Duration, Instant};
    const PLAN: &str = "plans-here.lock";
    const WRITERS: &str = "writers-below.lock";
    let (own, other) = if writer {
        (WRITERS, PLAN)
    } else {
        (PLAN, WRITERS)
    };
    let deadline = Instant::now() + Duration::from_millis(50);
    let _gate = loop {
        match open_named_lock(
            directory,
            "workspace-admission.lock",
            OFlags::CREATE,
            NonBlockingLockExclusive,
        ) {
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            result => break result?,
        }
    };
    let _opposite = open_named_lock(directory, other, OFlags::CREATE, NonBlockingLockExclusive)?;
    open_named_lock(directory, own, OFlags::CREATE, NonBlockingLockShared)
}

/// Release a checked mutable marker while its caller holds the owner lock.
pub fn unlink(directory: &File, name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || matches!(name, "." | "..") || name.contains('\0') {
        return Err(io::Error::other("local_file_name"));
    }
    rustix::fs::unlinkat(directory, name, rustix::fs::AtFlags::empty())?;
    directory.sync_all()
}

fn open_lock(
    directory: &File,
    extra: OFlags,
    operation: rustix::fs::FlockOperation,
) -> io::Result<File> {
    open_named_lock(directory, "worker.lock", extra, operation)
}

fn open_named_lock(
    directory: &File,
    name: &str,
    extra: OFlags,
    operation: rustix::fs::FlockOperation,
) -> io::Result<File> {
    let fd = rustix::fs::openat(
        directory,
        name,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | extra,
        Mode::RUSR | Mode::WUSR,
    )?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(io::Error::other("process_job_lock"));
    }
    rustix::fs::flock(&file, operation)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };
    struct Root(std::path::PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ansa-ui-local-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::fs::create_private(&path).unwrap();
            Self(path)
        }
        fn directory(&self) -> File {
            crate::fs::private_directory(&self.0).unwrap()
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn ui_preferences_replace_preserves_private_permissions_and_rejects_links() {
        let root = Root::new();
        let directory = root.directory();
        replace_ui(&directory, b"first", "1").unwrap();
        replace_ui(&directory, b"second", "2").unwrap();
        assert_eq!(
            read(&directory, "ui.json", 4096).unwrap().unwrap(),
            b"second"
        );
        assert_eq!(
            std::fs::metadata(root.0.join("ui.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::hard_link(root.0.join("ui.json"), root.0.join("alias")).unwrap();
        assert!(replace_ui(&directory, b"changed", "3").is_err());
        assert_eq!(std::fs::read(root.0.join("alias")).unwrap(), b"second");
        std::fs::remove_file(root.0.join("ui.json")).unwrap();
        symlink(root.0.join("alias"), root.0.join("ui.json")).unwrap();
        assert!(replace_ui(&directory, b"changed", "4").is_err());
        assert_eq!(std::fs::read(root.0.join("alias")).unwrap(), b"second");
        assert!(
            !std::fs::read_dir(&root.0).unwrap().any(|v| v
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("pending"))
        );
    }
    #[test]
    fn ui_preferences_reject_unbounded_and_invalid_temporary_names_before_creation() {
        let root = Root::new();
        let directory = root.directory();
        for suffix in ["", "../outside", "symlink/name", "name\0suffix"] {
            assert!(replace_ui(&directory, b"value", suffix).is_err());
        }
        assert!(replace_ui(&directory, b"", "1").is_err());
        assert!(replace_ui(&directory, &vec![b'x'; 4097], "1").is_err());
        assert!(read(&directory, "ui.json", 4096).unwrap().is_none());
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
    }
}
