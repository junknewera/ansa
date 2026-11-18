//! Owned v1 local/evidence files. No parsing or runtime policy here.
use rustix::fs::{Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};
pub type ReadFlags = OFlags;
pub fn read_flags() -> OFlags {
    OFlags::RDONLY | OFlags::NONBLOCK
}
pub fn read(directory: &File, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
    read_impl(directory, name, limit, true)
}
/// Bounded public UI palette data, anchored to an already opened directory.
pub fn read_regular(directory: &File, name: &str, limit: usize) -> io::Result<Option<Vec<u8>>> {
    read_impl(directory, name, limit, false)
}
fn read_impl(
    directory: &File,
    name: &str,
    limit: usize,
    private: bool,
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
        || meta.len() == 0
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
    if !(1..=256).contains(&limit) {
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
    let _ = read_regular(directory, "ui.json", 4096)?;
    let pending = format!(".ui.json.pending-{suffix}");
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
    rustix::fs::renameat(directory, pending.as_str(), directory, "ui.json")?;
    directory.sync_all()
}
pub fn lock(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::empty(),
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
}

pub fn lock_shared(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::empty(),
        rustix::fs::FlockOperation::NonBlockingLockShared,
    )
}

pub fn lock_new(directory: &File) -> io::Result<File> {
    open_lock(
        directory,
        OFlags::EXCL,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
}

pub const ACTIVE_TERMINAL_SCOPE: &str = "active-terminal-scope.json";
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
    let (entries, more) = names(directory, 256).map_err(|_| "terminal_workspace_evidence")?;
    if more
        || entries.iter().any(|entry| {
            let name = entry.as_encoded_bytes();
            name.starts_with(ROOT_WRITER_PREFIX.as_bytes())
                || !roots_only
                    && (name.starts_with(WRITER_PREFIX.as_bytes())
                        || name.starts_with(b"ordinary-writer-"))
        })
    {
        return Err("terminal_workspace_quarantined");
    }
    if read(directory, ACTIVE_TERMINAL_SCOPE, 8192)
        .map_err(|_| "terminal_workspace_evidence")?
        .is_some()
    {
        return Err("terminal_workspace_quarantined");
    }
    Ok(())
}

/// Directory and admission locks for one operation. Descendant workspaces keep
/// a shared lock on eligible ancestors and an exclusive lock at each write root.
pub struct WorkspaceWrite {
    directory: File,
    scopes: Vec<WriteScope>,
    operation: Option<String>,
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
            open_workspace_tree(registry, workspace, &mut scopes, writer)?;
        }
        let directory = open_workspace_tree(registry, target, &mut scopes, writer)?;
        Ok(Self {
            directory,
            scopes,
            operation: None,
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
        for scope in &self.scopes {
            known &= remove_writer(&scope.record, &operation, scope.root).is_ok();
        }
        known.then_some(()).ok_or("terminal_workspace_evidence")
    }
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
    scopes: &mut Vec<WriteScope>,
    writer: bool,
) -> Result<File, &'static str> {
    let mut directory = crate::fs::open_nofollow(Path::new("/"), read_flags())
        .map_err(|_| "terminal_workspace_identity")?;
    let mut path = std::path::PathBuf::from("/");
    hold_scope(registry, &directory, path == target, scopes, writer)?;
    for name in target.iter().skip(1) {
        path.push(name);
        directory = crate::fs::open_relative_directory(&directory, Path::new(name))
            .map_err(|_| "terminal_workspace_identity")?;
        hold_scope(registry, &directory, path == target, scopes, writer)?;
    }
    Ok(directory)
}

fn hold_scope(
    registry: &Path,
    directory: &File,
    root: bool,
    scopes: &mut Vec<WriteScope>,
    writer: bool,
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
    ensure_no_writers(&record, !root)?;
    scopes.push(WriteScope {
        record,
        _lock: lock,
        _presence: presence,
        identity,
        root,
    });
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
            OFlags::empty(),
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
    let _opposite = open_named_lock(directory, other, OFlags::empty(), NonBlockingLockExclusive)?;
    open_named_lock(directory, own, OFlags::empty(), NonBlockingLockShared)
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
        OFlags::RDWR
            | OFlags::CREATE
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | extra,
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
