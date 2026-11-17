//! Owned v1 local/evidence files. No parsing or runtime policy here.
use rustix::fs::{Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
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
    let fd = rustix::fs::openat(
        directory,
        "worker.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
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
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
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
