//! Owned descriptor-relative journal files and process-wide exclusive lease.
use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags};
use std::{fs::File, io, os::unix::fs::MetadataExt, path::Path};

pub struct Files {
    directory: File,
    _lock: File,
}

pub struct Catalog {
    directory: File,
}
impl Catalog {
    /// The configured parent exists before session selection; only sessions/ is created.
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("session_directory"))?,
        )?
        .sync_all()?;
        Ok(Self {
            directory: crate::fs::private_directory(path)?,
        })
    }
    fn child(&self, id: &str) -> io::Result<File> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(io::Error::other("invalid_session_id"));
        }
        let file = File::from(fs::openat(
            &self.directory,
            id,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let meta = file.metadata()?;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other("local_directory_permissions"));
        }
        Ok(file)
    }
    pub fn entries(&self) -> io::Result<Vec<String>> {
        let mut entries = Vec::new();
        let mut count = 0;
        for entry in fs::Dir::read_from(&self.directory)? {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            count += 1;
            if count > 4096 {
                return Err(io::Error::other("session_catalog_limit"));
            }
            if let Ok(name) = std::str::from_utf8(bytes) {
                if name.is_empty()
                    || name.len() > 64
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    continue;
                }
                let meta = fs::statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
                if fs::FileType::from_raw_mode(meta.st_mode) == fs::FileType::Directory {
                    entries.push(name.into());
                }
            }
        }
        Ok(entries)
    }
    pub fn info(&self, id: &str) -> io::Result<Option<String>> {
        use std::io::Read;
        let directory = self.child(id)?;
        let mut file = match fs::openat(
            &directory,
            "info.json",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
            || meta.len() == 0
            || meta.len() > 4096
        {
            return Err(io::Error::other("local_file_permissions"));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
        if bytes.len() > 4096 {
            return Err(io::Error::other("local_file_limit"));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| io::Error::other("json_invalid"))
    }
    pub fn modified(&self, id: &str) -> io::Result<Option<(i64, u64)>> {
        let directory = self.child(id)?;
        let mut result = None;
        let mut count = 0;
        for entry in fs::Dir::read_from(&directory)? {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            count += 1;
            if count > 4100 {
                return Err(io::Error::other("session_parts_limit"));
            }
            if !bytes.starts_with(b"part-") {
                continue;
            }
            let meta = fs::statat(&directory, entry.file_name(), AtFlags::empty())?;
            if fs::FileType::from_raw_mode(meta.st_mode) == fs::FileType::RegularFile {
                result = Some(result.map_or((meta.st_mtime, meta.st_mtime_nsec), |old| {
                    std::cmp::max(old, (meta.st_mtime, meta.st_mtime_nsec))
                }));
            }
        }
        Ok(result)
    }
    pub fn create(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || bytes.is_empty()
            || bytes.len() > 4096
        {
            return Err(io::Error::other("session_directory_create"));
        }
        fs::mkdirat(&self.directory, id, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
        self.directory.sync_all()?;
        let directory = self.child(id)?;
        let mut file = File::from(fs::openat(
            &directory,
            "info.json.pending",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::renameat_with(
            &directory,
            "info.json.pending",
            &directory,
            "info.json",
            fs::RenameFlags::NOREPLACE,
        )?;
        directory.sync_all()
    }
}
impl Files {
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        let directory = File::from(fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        let lock = File::from(fs::openat(
            &directory,
            "lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        if !lock.metadata()?.is_file() || lock.metadata()?.nlink() != 1 {
            return Err(io::Error::other("journal_lock_type"));
        }
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| io::Error::other("session_busy"))?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }
    pub fn names(&self) -> io::Result<Vec<std::ffi::OsString>> {
        use std::os::unix::ffi::OsStringExt;
        let mut dir = fs::Dir::read_from(&self.directory)?;
        let mut names = Vec::new();
        for entry in &mut dir {
            let entry = entry?;
            let bytes = entry.file_name().to_bytes();
            if bytes.starts_with(b"part-") && bytes.ends_with(b".log") {
                if names.len() == 4096 {
                    return Err(io::Error::other("journal_parts_limit"));
                }
                names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
            }
        }
        names.sort();
        Ok(names)
    }
    pub fn part(&self, name: &str, create: bool) -> io::Result<File> {
        if name.len() != 29
            || !name.starts_with("part-")
            || !name.ends_with(".log")
            || !name.as_bytes()[5..25].iter().all(u8::is_ascii_digit)
        {
            return Err(io::Error::other("journal_part_sequence"));
        }
        let mut flags =
            OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if create {
            flags |= OFlags::CREATE | OFlags::EXCL;
        }
        let file = File::from(fs::openat(
            &self.directory,
            name,
            flags,
            Mode::RUSR | Mode::WUSR,
        )?);
        if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
            return Err(io::Error::other("journal_part_type"));
        }
        Ok(file)
    }
    pub fn unchanged(&self, name: &str, file: &File, size: u64) -> io::Result<bool> {
        let open = file.metadata()?;
        let named = fs::statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(
            fs::FileType::from_raw_mode(named.st_mode) == fs::FileType::RegularFile
                && open.dev() == named.st_dev
                && open.ino() == named.st_ino
                && open.nlink() == 1
                && open.len() == size,
        )
    }
    pub fn sync(&self) -> io::Result<()> {
        self.directory.sync_all()
    }
}
/// Narrow test-adapter capability: only a fresh private marked /tmp root.
pub fn fixture_root(path: &Path) -> io::Result<()> {
    use std::{io::Read, os::unix::ffi::OsStrExt, path::Component};
    if path.parent() != Some(std::env::temp_dir().as_path())
        || !path
            .file_name()
            .is_some_and(|n| n.as_bytes().starts_with(b"ansa-m4-"))
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(io::Error::other("fixture_root"));
    }
    let directory = File::from(fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = directory.metadata()?;
    if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other("fixture_owner"));
    }
    let mut file = File::from(fs::openat(
        &directory,
        "fixture-owner",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
    {
        return Err(io::Error::other("fixture_owner"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(65).read_to_end(&mut bytes)?;
    if bytes != b"ansa-m4-synthetic-v1\n" {
        return Err(io::Error::other("fixture_owner"));
    }
    Ok(())
}
