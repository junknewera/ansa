//! Bounded file publication anchored to an owned, no-follow parent directory.
//! The final preimage check detects observed conflicts; it is not atomic CAS
//! against external writers between that check and rename.
use rustix::fs::{Mode, OFlags};
use std::{
    ffi::OsString,
    fs::{File, Metadata, Permissions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Cancelled,
    Rejected(&'static str),
    Unknown,
}

struct Existing {
    // Retaining the FD also prevents reuse of this inode after an unlink.
    file: File,
    metadata: Metadata,
    bytes: Vec<u8>,
}

pub struct Snapshot {
    directory: File,
    name: OsString,
    existing: Option<Existing>,
}

fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.nlink() == b.nlink()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn ordinary_mode(mode: u32) -> bool {
    mode & 0o7000 == 0
}

fn read_current(directory: &File, name: &OsString) -> Result<Option<Existing>, &'static str> {
    let fd = match rustix::fs::openat(
        directory,
        name.as_os_str(),
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err("file_access_failed"),
    };
    let mut file = File::from(fd);
    let metadata = file.metadata().map_err(|_| "file_access_failed")?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || !ordinary_mode(metadata.mode())
    {
        return Err("file_not_regular");
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err("file_edit_limit");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "file_read_failed")?;
    if bytes.len() > MAX_BYTES {
        return Err("file_edit_limit");
    }
    if !same_metadata(
        &metadata,
        &file.metadata().map_err(|_| "file_access_failed")?,
    ) {
        return Err("file_changed");
    }
    Ok(Some(Existing {
        file,
        metadata,
        bytes,
    }))
}

impl Snapshot {
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        if !path.is_absolute() || crate::fs::protected_path(path) {
            return Err("protected_file");
        }
        let path = crate::fs::normalize(path);
        let name = path.file_name().ok_or("file_path")?.to_os_string();
        let directory = crate::fs::open_nofollow(
            path.parent().ok_or("file_path")?,
            OFlags::RDONLY | OFlags::DIRECTORY,
        )
        .map_err(|_| "file_access_failed")?;
        Self::open_named(directory, name)
    }

    pub fn open_in(path: &Path, directory: &File) -> Result<Self, &'static str> {
        if !path.is_absolute() || crate::fs::protected_path(path) {
            return Err("protected_file");
        }
        let name = path.file_name().ok_or("file_path")?.to_os_string();
        let directory = directory.try_clone().map_err(|_| "file_access_failed")?;
        Self::open_named(directory, name)
    }

    fn open_named(directory: File, name: OsString) -> Result<Self, &'static str> {
        let existing = read_current(&directory, &name)?;
        Ok(Self {
            directory,
            name,
            existing,
        })
    }

    pub fn before(&self) -> Option<&[u8]> {
        self.existing.as_ref().map(|old| old.bytes.as_slice())
    }

    fn unchanged(&self) -> bool {
        match (&self.existing, read_current(&self.directory, &self.name)) {
            (None, Ok(None)) => true,
            (Some(before), Ok(Some(current))) => {
                same_metadata(&before.metadata, &current.metadata)
                    && before.bytes == current.bytes
                    && before
                        .file
                        .metadata()
                        .is_ok_and(|m| same_metadata(&before.metadata, &m))
            }
            _ => false,
        }
    }

    pub fn publish(self, bytes: &[u8], cancelled: impl FnMut() -> bool) -> Result<(), Failure> {
        self.publish_with_sync(bytes, cancelled, File::sync_all)
    }

    fn publish_with_sync(
        self,
        bytes: &[u8],
        mut cancelled: impl FnMut() -> bool,
        sync_directory: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<(), Failure> {
        if bytes.len() > MAX_BYTES {
            return Err(Failure::Rejected("file_edit_limit"));
        }
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        let mut pending = Pending::create(&self.directory)?;
        let result = (|| {
            pending
                .file
                .write_all(bytes)
                .map_err(|_| Failure::Rejected("file_write_failed"))?;
            let mode = self
                .existing
                .as_ref()
                .map_or(0o600, |old| old.metadata.mode() & 0o777);
            pending
                .file
                .set_permissions(Permissions::from_mode(mode))
                .map_err(|_| Failure::Rejected("file_write_failed"))?;
            pending
                .file
                .sync_all()
                .map_err(|_| Failure::Rejected("file_sync_failed"))?;
            if cancelled() {
                return Err(Failure::Cancelled);
            }
            if !self.unchanged() {
                return Err(Failure::Rejected("file_changed"));
            }
            let staged = read_current(&self.directory, &pending.name)
                .map_err(Failure::Rejected)?
                .ok_or(Failure::Rejected("file_changed"))?;
            if staged.bytes != bytes
                || !pending
                    .file
                    .metadata()
                    .is_ok_and(|metadata| same_metadata(&metadata, &staged.metadata))
            {
                return Err(Failure::Rejected("file_changed"));
            }
            // The new-file case has a kernel-enforced no-replace condition.
            // Existing files have the observed preimage check above.
            let renamed = if self.existing.is_none() {
                rustix::fs::renameat_with(
                    &self.directory,
                    pending.name.as_os_str(),
                    &self.directory,
                    self.name.as_os_str(),
                    rustix::fs::RenameFlags::NOREPLACE,
                )
            } else {
                rustix::fs::renameat(
                    &self.directory,
                    pending.name.as_os_str(),
                    &self.directory,
                    self.name.as_os_str(),
                )
            };
            match renamed {
                Ok(()) => pending.exists = false,
                Err(rustix::io::Errno::EXIST) => return Err(Failure::Rejected("file_changed")),
                Err(_) => return Err(Failure::Unknown),
            }
            // Cancellation after publication cannot erase this known outcome.
            sync_directory(&self.directory).map_err(|_| Failure::Unknown)
        })();
        if pending.exists && pending.remove().is_err() {
            return Err(Failure::Unknown);
        }
        result
    }
}

struct Pending<'a> {
    directory: &'a File,
    name: OsString,
    exists: bool,
    file: File,
}
impl<'a> Pending<'a> {
    fn create(directory: &'a File) -> Result<Self, Failure> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            let name: OsString = format!(
                ".ansa-edit-{}-{}.pending",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
            .into();
            match rustix::fs::openat(
                directory,
                name.as_os_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => {
                    return Ok(Self {
                        directory,
                        name,
                        exists: true,
                        file: File::from(fd),
                    });
                }
                Err(rustix::io::Errno::EXIST) => continue,
                Err(_) => return Err(Failure::Rejected("file_write_failed")),
            }
        }
        Err(Failure::Rejected("file_write_failed"))
    }
    fn remove(&mut self) -> io::Result<()> {
        let owned = self.file.metadata()?;
        let named = rustix::fs::statat(
            self.directory,
            self.name.as_os_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )?;
        if named.st_dev != owned.dev() || named.st_ino != owned.ino() {
            return Err(io::Error::other("staging_entry_replaced"));
        }
        // This rejects an observed foreign replacement. Linux unlinkat has no
        // inode condition: external namespace writers can still race this
        // final check. The retained FD keeps the owned inode alive throughout.
        rustix::fs::unlinkat(
            self.directory,
            self.name.as_os_str(),
            rustix::fs::AtFlags::empty(),
        )?;
        self.exists = false;
        Ok(())
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.exists {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{DirBuilderExt, symlink},
        path::PathBuf,
    };
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ansa-edit-native-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("file")
        }
        fn no_pending(&self) {
            assert!(std::fs::read_dir(&self.0).unwrap().all(|e| {
                !e.unwrap()
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(b".ansa-edit-")
            }));
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn publication_preserves_permissions_and_has_exact_new_contents() {
        let root = Root::new();
        std::fs::write(root.file(), b"before").unwrap();
        std::fs::set_permissions(root.file(), Permissions::from_mode(0o751)).unwrap();
        let snapshot = Snapshot::open(&root.file()).unwrap();
        assert_eq!(snapshot.before(), Some(b"before".as_slice()));
        snapshot.publish("после\n".as_bytes(), || false).unwrap();
        assert_eq!(std::fs::read(root.file()).unwrap(), "после\n".as_bytes());
        assert_eq!(
            std::fs::metadata(root.file()).unwrap().mode() & 0o777,
            0o751
        );
        root.no_pending();
    }
    #[test]
    fn changed_preimage_and_creation_collision_leave_current_file_untouched() {
        let root = Root::new();
        let empty = Snapshot::open(&root.file()).unwrap();
        std::fs::write(root.file(), b"external").unwrap();
        assert_eq!(
            empty.publish(b"mine", || false),
            Err(Failure::Rejected("file_changed"))
        );
        let existing = Snapshot::open(&root.file()).unwrap();
        std::fs::write(root.file(), b"external change").unwrap();
        assert_eq!(
            existing.publish(b"mine", || false),
            Err(Failure::Rejected("file_changed"))
        );
        assert_eq!(std::fs::read(root.file()).unwrap(), b"external change");
        root.no_pending();
    }
    #[test]
    fn cancellation_before_and_after_staging_has_no_publication() {
        for cancel_at in [1, 2] {
            let root = Root::new();
            let mut calls = 0;
            assert_eq!(
                Snapshot::open(&root.file()).unwrap().publish(b"new", || {
                    calls += 1;
                    calls == cancel_at
                }),
                Err(Failure::Cancelled)
            );
            assert!(!root.file().exists());
            root.no_pending();
        }
    }
    #[test]
    fn missing_directory_sync_is_unknown_after_publication() {
        let root = Root::new();
        let result = Snapshot::open(&root.file()).unwrap().publish_with_sync(
            b"written",
            || false,
            |_| Err(io::Error::other("injected directory fsync failure")),
        );
        assert_eq!(result, Err(Failure::Unknown));
        assert_eq!(std::fs::read(root.file()).unwrap(), b"written");
        root.no_pending();
    }
    #[test]
    fn file_links_parent_links_special_modes_and_oversized_files_are_rejected() {
        let root = Root::new();
        std::fs::write(root.file(), b"safe").unwrap();
        symlink(root.file(), root.0.join("link")).unwrap();
        assert!(Snapshot::open(&root.0.join("link")).is_err());
        std::fs::hard_link(root.file(), root.0.join("hardlink")).unwrap();
        assert!(Snapshot::open(&root.file()).is_err());
        std::fs::remove_file(root.0.join("hardlink")).unwrap();
        symlink(&root.0, root.0.join("parent-link")).unwrap();
        assert!(Snapshot::open(&root.0.join("parent-link/file")).is_err());
        // Nix forbids chmod with setuid/setgid bits. Exercise a real special
        // mode without those privileges, and check every permission/special-bit
        // combination through the production predicate.
        for mode in 0..=0o7777 {
            assert_eq!(ordinary_mode(0o100000 | mode), mode <= 0o777);
        }
        std::fs::set_permissions(root.file(), Permissions::from_mode(0o1755)).unwrap();
        assert_eq!(
            std::fs::metadata(root.file()).unwrap().mode() & 0o7777,
            0o1755
        );
        assert!(Snapshot::open(&root.file()).is_err());
        std::fs::set_permissions(root.file(), Permissions::from_mode(0o600)).unwrap();
        std::fs::write(root.file(), vec![b'x'; MAX_BYTES + 1]).unwrap();
        assert!(matches!(
            Snapshot::open(&root.file()),
            Err("file_edit_limit")
        ));
    }
    #[test]
    fn mode_or_inode_changes_are_conflicts_even_for_identical_bytes() {
        let root = Root::new();
        std::fs::write(root.file(), b"same").unwrap();
        let snapshot = Snapshot::open(&root.file()).unwrap();
        std::fs::rename(root.file(), root.0.join("old")).unwrap();
        std::fs::write(root.file(), b"same").unwrap();
        assert_eq!(
            snapshot.publish(b"mine", || false),
            Err(Failure::Rejected("file_changed"))
        );
        std::fs::set_permissions(root.file(), Permissions::from_mode(0o644)).unwrap();
        let snapshot = Snapshot::open(&root.file()).unwrap();
        std::fs::set_permissions(root.file(), Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            snapshot.publish(b"mine", || false),
            Err(Failure::Rejected("file_changed"))
        );
        assert_eq!(std::fs::read(root.file()).unwrap(), b"same");
        root.no_pending();
    }
    #[test]
    fn staging_changes_are_rejected_before_publishing_reported_bytes() {
        let root = Root::new();
        std::fs::write(root.file(), b"before").unwrap();
        let snapshot = Snapshot::open(&root.file()).unwrap();
        let mut calls = 0;
        let result = snapshot.publish(b"mine", || {
            calls += 1;
            if calls == 2 {
                let pending = std::fs::read_dir(&root.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .as_encoded_bytes()
                            .starts_with(b".ansa-edit-")
                    })
                    .unwrap();
                std::fs::write(pending, b"foreign").unwrap();
            }
            false
        });
        assert_eq!(result, Err(Failure::Rejected("file_changed")));
        assert_eq!(std::fs::read(root.file()).unwrap(), b"before");
        root.no_pending();
    }
    #[test]
    fn cancellation_after_publication_does_not_discard_the_known_outcome() {
        use std::cell::Cell;
        let root = Root::new();
        let cancelled = Cell::new(false);
        let result = Snapshot::open(&root.file()).unwrap().publish_with_sync(
            b"written",
            || cancelled.get(),
            |directory| {
                cancelled.set(true);
                directory.sync_all()
            },
        );
        assert_eq!(result, Ok(()));
        assert!(cancelled.get());
        assert_eq!(std::fs::read(root.file()).unwrap(), b"written");
        root.no_pending();
    }
    #[test]
    fn cleanup_preserves_a_foreign_replacement_and_reports_unknown() {
        let root = Root::new();
        std::fs::write(root.file(), b"before").unwrap();
        let mut calls = 0;
        let mut replaced = None;
        let result = Snapshot::open(&root.file()).unwrap().publish(b"mine", || {
            calls += 1;
            if calls == 2 {
                let path = std::fs::read_dir(&root.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .as_encoded_bytes()
                            .starts_with(b".ansa-edit-")
                    })
                    .unwrap();
                std::fs::rename(&path, root.0.join("held-staging")).unwrap();
                std::fs::write(&path, b"foreign").unwrap();
                replaced = Some(path);
            }
            false
        });
        assert_eq!(result, Err(Failure::Unknown));
        assert_eq!(std::fs::read(replaced.unwrap()).unwrap(), b"foreign");
        assert_eq!(std::fs::read(root.0.join("held-staging")).unwrap(), b"mine");
        assert_eq!(std::fs::read(root.file()).unwrap(), b"before");
    }
}
