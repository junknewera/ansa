use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

/// Lexical normalization matching the local v1 path contract.
pub fn normalize(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(part.as_os_str()),
        }
    }
    out
}
pub fn weakly_canonical(path: &Path) -> io::Result<std::path::PathBuf> {
    let path = std::path::absolute(path)?;
    let mut current = path.as_path();
    let mut missing = vec![];
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(normalize(&resolved));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        missing.push(
            current
                .components()
                .next_back()
                .ok_or_else(|| io::Error::other("path_component"))?
                .as_os_str()
                .to_os_string(),
        );
        current = current
            .parent()
            .ok_or_else(|| io::Error::other("path_parent"))?;
    }
}
pub fn protected_path(path: &Path) -> bool {
    path.components().any(|part| {
        let name = part.as_os_str().as_encoded_bytes();
        name == b".env"
            || name.starts_with(b".env.")
            || name == b"openrouter.key"
            || name.starts_with(b"openrouter.key.pending-")
            || matches!(name, b".codex" | b".claude" | b".gemini")
    })
}
pub fn open_nofollow(path: &Path, flags: rustix::fs::OFlags) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    if !path.is_absolute() {
        return Err(io::Error::other("absolute_path_required"));
    }
    let path = normalize(path);
    let mut directory = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let parts: Vec<_> = path
        .components()
        .filter_map(|p| match p {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect();
    for (index, name) in parts.iter().enumerate() {
        let options = if index + 1 == parts.len() {
            flags
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY
        };
        directory = openat(
            &directory,
            *name,
            options | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
    }
    Ok(File::from(directory))
}
pub fn regular_single(file: &File) -> io::Result<bool> {
    let meta = file.metadata()?;
    Ok(meta.is_file() && meta.nlink() == 1)
}

pub struct DirectoryEntry {
    pub name: std::ffi::OsString,
    pub directory: bool,
    pub symlink: bool,
    pub regular: bool,
}

pub struct DirectoryEntries {
    entries: rustix::fs::Dir,
    follow_link_directories: bool,
    offset: i64,
}

impl DirectoryEntries {
    /// Resume after a cookie returned by this directory's iterator.
    pub fn seek(&mut self, offset: i64) -> io::Result<()> {
        self.entries.seek(offset)?;
        self.offset = offset;
        Ok(())
    }

    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn open_child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        let directory = self.open_entry(
            name,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        )?;
        Ok(Self {
            entries: rustix::fs::Dir::new(directory)?,
            follow_link_directories: self.follow_link_directories,
            offset: 0,
        })
    }

    pub fn open_regular(&self, name: &std::ffi::OsStr) -> io::Result<File> {
        let file = self.open_entry(name, crate::local::read_flags())?;
        if !regular_single(&file)? {
            return Err(io::Error::other("file_not_regular"));
        }
        Ok(file)
    }

    fn open_entry(&self, name: &std::ffi::OsStr, flags: rustix::fs::OFlags) -> io::Result<File> {
        let name = Path::new(name);
        if !matches!(name.components().next(), Some(Component::Normal(_)))
            || name.as_os_str().as_encoded_bytes().contains(&b'/')
        {
            return Err(io::Error::other("workspace_relative_path"));
        }
        Ok(rustix::fs::openat(
            self.entries.fd()?,
            name,
            flags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?
        .into())
    }

    pub fn has_instructions(&self) -> io::Result<bool> {
        match rustix::fs::statat(
            self.entries.fd()?,
            "AGENTS.md",
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn metadata(&self) -> io::Result<std::fs::Metadata> {
        let fd = self.entries.fd()?.try_clone_to_owned()?;
        std::fs::File::from(fd).metadata()
    }
}

pub fn read_directory(path: &Path, follow_link_directories: bool) -> io::Result<DirectoryEntries> {
    use rustix::fs::{Dir, OFlags};
    let directory = open_nofollow(path, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let entries = Dir::new(directory)?;
    Ok(DirectoryEntries {
        entries,
        follow_link_directories,
        offset: 0,
    })
}

impl Iterator for DirectoryEntries {
    type Item = io::Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        use rustix::fs::{AtFlags, FileType, statat};
        use std::os::unix::ffi::OsStrExt;
        let entry = loop {
            match self.entries.next()? {
                Ok(entry) if matches!(entry.file_name().to_bytes(), b"." | b"..") => continue,
                Ok(entry) => break entry,
                Err(error) => return Some(Err(error.into())),
            }
        };
        self.offset = entry.offset();
        let name = entry.file_name();
        let stat = |flags| self.entries.fd().and_then(|fd| statat(fd, name, flags));
        let kind = match entry.file_type() {
            FileType::Unknown => match stat(AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => FileType::from_raw_mode(stat.st_mode),
                Err(error) => return Some(Err(error.into())),
            },
            kind => kind,
        };
        let symlink = kind == FileType::Symlink;
        let directory = kind == FileType::Directory
            || (symlink
                && self.follow_link_directories
                && stat(AtFlags::empty()).is_ok_and(|stat| {
                    FileType::from_raw_mode(stat.st_mode) == FileType::Directory
                }));
        Some(Ok(DirectoryEntry {
            name: std::ffi::OsStr::from_bytes(name.to_bytes()).to_owned(),
            directory,
            symlink,
            regular: kind == FileType::RegularFile,
        }))
    }
}

/// Traverse workspace-relative directories from a pinned descriptor.
pub fn open_relative_directory(directory: &File, path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let mut current = directory.try_clone()?;
    for part in path.components() {
        let Component::Normal(name) = part else {
            return Err(io::Error::other("workspace_relative_path"));
        };
        current = rustix::fs::openat(
            &current,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
    }
    Ok(current)
}
pub fn create_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !path.is_absolute() {
        return Err(io::Error::other("absolute_path_required"));
    }
    if path.exists() {
        return private_directory(path).map(|_| ());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("directory_parent"))?;
    if !parent.exists() {
        create_private(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    open_nofollow(
        parent,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
    )?
    .sync_all()?;
    private_directory(path).map(|_| ())
}

/// Reserve a new private leaf. An existing directory is never a fresh store.
pub fn create_private_leaf(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    if !path.is_absolute() || normalize(path) != path {
        return Err(io::Error::other("absolute_normalized_path_required"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("directory_parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("directory_name"))?;
    let directory = open_nofollow(parent, OFlags::RDONLY | OFlags::DIRECTORY)?;
    rustix::fs::mkdirat(&directory, name, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
    let child = rustix::fs::openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let child = checked_private_directory(child.into())?;
    directory.sync_all()?;
    Ok(child)
}
/// Bytewise replacement agrees with legacy v1, including incomplete UTF-8 tails.
pub fn utf8_text(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let lead = bytes[offset];
        let size = match lead {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        if size > 0
            && offset + size <= bytes.len()
            && std::str::from_utf8(&bytes[offset..offset + size]).is_ok()
        {
            text.push_str(
                std::str::from_utf8(&bytes[offset..offset + size]).expect("checked UTF-8"),
            );
            offset += size;
        } else {
            text.push('\u{fffd}');
            offset += 1;
        }
    }
    text
}

pub fn private_directory(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    if !path.is_absolute() {
        return Err(io::Error::other("absolute_private_directory_required"));
    }
    let mut fd = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )?
            }
            _ => return Err(io::Error::other("private_directory_component")),
        }
    }
    checked_private_directory(File::from(fd))
}

fn checked_private_directory(file: File) -> io::Result<File> {
    let meta = file.metadata()?;
    if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other("private_directory_permissions"));
    }
    Ok(file)
}

pub fn consume(directory: &File) -> io::Result<()> {
    let fd = rustix::fs::openat(
        directory,
        "consumed",
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    let mut marker = File::from(fd);
    marker.write_all(b"m1-fixture-operation-v1\n")?;
    marker.sync_all()?;
    directory.sync_all()
}

pub fn identity(directory: &File) -> io::Result<String> {
    let meta = directory.metadata()?;
    Ok(format!("{}:{}", meta.dev(), meta.ino()))
}

pub fn authorize_helper(directory: &File, expected: &str, claim: &str) -> io::Result<()> {
    if identity(directory)? != expected
        || read_record(directory, "consumed")?.as_deref() != Some("m1-fixture-operation-v1\n")
    {
        return Err(io::Error::other("fixture_operation_identity"));
    }
    record(directory, claim, "claimed\n")
}

pub fn record(directory: &File, name: &str, text: &str) -> io::Result<()> {
    if !matches!(
        name,
        "primary.started" | "detached.pid" | "outcome" | "primary.claim" | "detached.claim"
    ) || text.len() > 256
    {
        return Err(io::Error::other("fixture_record_contract"));
    }
    publish_record(directory, name, |file| file.write_all(text.as_bytes()))
}

fn publish_record(
    directory: &File,
    name: &str,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let pending = format!(".{name}.pending");
    let fd = rustix::fs::openat(
        directory,
        &pending,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    let mut file = File::from(fd);
    // The ready name must never expose the create-before-write window. The
    // directory FD anchors publication; NOREPLACE keeps consume-once claims.
    let mut published = false;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        rustix::fs::renameat_with(
            directory,
            &pending,
            directory,
            name,
            rustix::fs::RenameFlags::NOREPLACE,
        )?;
        published = true;
        directory.sync_all()
    })();
    if result.is_err() && !published {
        let _ = rustix::fs::unlinkat(directory, &pending, rustix::fs::AtFlags::empty());
    }
    result
}

pub fn read_record(directory: &File, name: &str) -> io::Result<Option<String>> {
    use std::io::Read;
    if !matches!(
        name,
        "primary.started" | "detached.pid" | "outcome" | "consumed"
    ) {
        return Err(io::Error::other("fixture_record_contract"));
    }
    let fd = match rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.len() > 256
    {
        return Err(io::Error::other("fixture_record_permissions"));
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(257)
        .read_to_string(&mut text)?;
    if text.len() > 256 {
        return Err(io::Error::other("fixture_record_limit"));
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_listing_uses_the_original_descriptor() {
        let root = std::env::temp_dir().join(format!("ansa-listing-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let folder = root.join("folder");
        let replacement = root.join("replacement");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(folder.join("first"), b"fixture").unwrap();
        std::fs::write(replacement.join("second"), b"fixture").unwrap();
        let mut entries = read_directory(&folder, false).unwrap();
        std::fs::rename(&folder, root.join("original")).unwrap();
        std::os::unix::fs::symlink(&replacement, &folder).unwrap();
        assert!(read_directory(&folder, false).is_err());
        assert_eq!(entries.next().unwrap().unwrap().name, "first");
        assert!(entries.next().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn readiness_record_is_invisible_until_complete_and_cannot_replace_a_claim() {
        use std::os::unix::fs::DirBuilderExt;
        let root = std::env::temp_dir().join(format!("ansa-m1-publication-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let directory = private_directory(&root).unwrap();
        publish_record(&directory, "detached.pid", |file| {
            file.write_all(b"12")?;
            assert_eq!(read_record(&directory, "detached.pid")?, None);
            file.write_all(b"345")?;
            assert_eq!(read_record(&directory, "detached.pid")?, None);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            read_record(&directory, "detached.pid").unwrap().as_deref(),
            Some("12345")
        );
        assert!(record(&directory, "detached.pid", "987").is_err());
        assert_eq!(
            read_record(&directory, "detached.pid").unwrap().as_deref(),
            Some("12345")
        );
        assert!(!root.join(".detached.pid.pending").exists());
        assert!(
            publish_record(&directory, "primary.started", |file| {
                file.write_all(b"partial")?;
                Err(io::Error::other("injected_write_failure"))
            })
            .is_err()
        );
        assert_eq!(read_record(&directory, "primary.started").unwrap(), None);
        assert!(!root.join(".primary.started.pending").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
