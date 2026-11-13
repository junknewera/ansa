//! Descriptor-relative private auth storage. No default profile lookup.
use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
use std::path::{Component, Path};
use std::sync::Arc;

const MAX: u64 = 256 * 1024;
const PRIVATE: Mode = Mode::RUSR.union(Mode::WUSR);
const DIRECTORY: Mode = PRIVATE.union(Mode::XUSR);

/// Test adapters accept only an owned, descriptor-checked fixture marker.
/// This never discovers a profile or accepts a caller-selected filename.
pub fn fixture_owner(path: &Path) -> io::Result<()> {
    let directory = crate::fs::private_directory(path)?;
    let mut file = File::from(fs::openat(
        &directory,
        "fixture-owner",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    guard(&file, false)?;
    if file.metadata()?.len() != b"ansa-m3-synthetic-v1\n".len() as u64 {
        return Err(io::Error::other("fixture_owner"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(65).read_to_end(&mut bytes)?;
    if bytes != b"ansa-m3-synthetic-v1\n" {
        return Err(io::Error::other("fixture_owner"));
    }
    Ok(())
}

pub fn fixture_event(path: &Path, event: &str) -> io::Result<()> {
    if !matches!(
        event,
        "refresh" | "code" | "revoke" | "models" | "inference"
    ) {
        return Err(io::Error::other("fixture_event"));
    }
    fixture_owner(path)?;
    let directory = crate::fs::private_directory(path)?;
    let mut file = File::from(fs::openat(
        &directory,
        "exchanges.log",
        OFlags::WRONLY
            | OFlags::APPEND
            | OFlags::CREATE
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK
            | OFlags::CLOEXEC,
        PRIVATE,
    )?);
    guard(&file, false)?;
    if file.metadata()?.len() > 1024 * 1024 {
        return Err(io::Error::other("fixture_event_limit"));
    }
    file.write_all(format!("{event}\n").as_bytes())?;
    file.sync_all()
}

#[derive(Clone)]
pub struct Directory(Arc<File>);
pub struct Lock {
    _file: File,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    TemporarySynced,
    Replaced,
}

fn valid_name(name: &str) -> bool {
    matches!(
        name,
        "registration.json" | "credentials.json" | "signed-out" | "lock"
    )
}
fn guard(file: &File, bounded: bool) -> io::Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || (bounded && (meta.len() == 0 || meta.len() > MAX))
    {
        return Err(io::Error::other("auth_store_permissions"));
    }
    Ok(())
}
impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("auth_directory_path"));
        }
        let mut fd = fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for component in path.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => return Err(io::Error::other("auth_directory_path")),
            };
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let next = match fs::openat(&fd, name, flags, Mode::empty()) {
                Ok(value) => value,
                Err(rustix::io::Errno::NOENT) => {
                    match fs::mkdirat(&fd, name, DIRECTORY) {
                        Ok(()) => (),
                        Err(rustix::io::Errno::EXIST) => (),
                        Err(e) => return Err(e.into()),
                    }
                    fs::fsync(&fd)?;
                    fs::openat(&fd, name, flags, Mode::empty())?
                }
                Err(e) => return Err(e.into()),
            };
            fd = next;
        }
        let file = File::from(fd);
        let meta = file.metadata()?;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other("auth_directory_permissions"));
        }
        Ok(Self(Arc::new(file)))
    }
    fn open_file(&self, name: &str, flags: OFlags) -> io::Result<File> {
        if !valid_name(name) {
            return Err(io::Error::other("auth_store_name"));
        }
        Ok(File::from(fs::openat(
            &*self.0,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            PRIVATE,
        )?))
    }
    pub fn try_lock(&self) -> io::Result<Lock> {
        let file = self.open_file("lock", OFlags::RDWR | OFlags::CREATE)?;
        guard(&file, false)?;
        fs::flock(&file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Lock { _file: file })
    }
    pub fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let mut file = match self.open_file(name, OFlags::RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        guard(&file, true)?;
        let length = file.metadata()?.len();
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length || bytes.len() as u64 > MAX {
            return Err(io::Error::other("auth_store_limit"));
        }
        Ok(Some(bytes))
    }
    pub fn signed_out(&self) -> io::Result<bool> {
        match self.open_file("signed-out", OFlags::RDONLY) {
            Ok(file) => {
                guard(&file, false)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub fn request_logout(&self) -> io::Result<()> {
        let file = self.open_file("signed-out", OFlags::WRONLY | OFlags::CREATE)?;
        guard(&file, false)?;
        file.sync_all()?;
        self.0.sync_all()
    }
    pub fn remove(&self, name: &str) -> io::Result<()> {
        if !valid_name(name) || name == "lock" {
            return Err(io::Error::other("auth_store_name"));
        }
        match fs::unlinkat(&*self.0, name, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => (),
            Err(e) => return Err(e.into()),
        }
        self.0.sync_all()
    }
    pub fn save(
        &self,
        name: &str,
        bytes: &[u8],
        suffix: &str,
        mut observe: impl FnMut(Point, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        if !matches!(name, "registration.json" | "credentials.json")
            || bytes.is_empty()
            || bytes.len() as u64 > MAX
            || suffix.len() != 22
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(io::Error::other("auth_store_contract"));
        }
        let temporary = format!("pending-{suffix}");
        let fd = fs::openat(
            &*self.0,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            PRIVATE,
        )?;
        struct Cleanup<'a>(&'a File, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = fs::unlinkat(self.0, self.1, AtFlags::empty());
            }
        }
        let _cleanup = Cleanup(&self.0, &temporary);
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        observe(Point::TemporarySynced, name)?;
        fs::renameat(&*self.0, temporary.as_str(), &*self.0, name)?;
        self.0.sync_all()?;
        observe(Point::Replaced, name)
    }
    pub fn clean_pending(&self) -> io::Result<()> {
        for entry in fs::Dir::read_from(&*self.0)? {
            let entry = entry?;
            if !entry.file_name().to_bytes().starts_with(b"pending-") {
                continue;
            }
            let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
            let file = File::from(fs::openat(
                &*self.0,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )?);
            guard(&file, false)?;
            fs::unlinkat(&*self.0, name, AtFlags::empty())?;
        }
        self.0.sync_all()
    }
}
