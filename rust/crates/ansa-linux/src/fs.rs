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
