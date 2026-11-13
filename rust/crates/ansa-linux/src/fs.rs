use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

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
    let file = File::from(fd);
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
    let fd = rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )?;
    let mut file = File::from(fd);
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    directory.sync_all()
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
