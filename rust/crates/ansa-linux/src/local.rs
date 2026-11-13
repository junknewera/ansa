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
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
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
