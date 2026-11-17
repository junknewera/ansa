//! Fixed-name, private, bounded display metadata. Never an auth store.
use rustix::fs::{self, Mode, OFlags};
use std::{
    fs::File,
    io::{self, Write},
    os::unix::fs::MetadataExt,
};

pub const LIMIT: usize = 256 * 1024;
const NAME: &str = "models-cache.json";
pub fn read(directory: &File) -> io::Result<Option<Vec<u8>>> {
    crate::local::read(directory, NAME, LIMIT)
}
pub fn replace(directory: &File, bytes: &[u8], suffix: &str) -> io::Result<()> {
    let meta = directory.metadata()?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || bytes.is_empty()
        || bytes.len() > LIMIT
        || suffix.len() != 22
        || !suffix
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'_' || v == b'-')
    {
        return Err(io::Error::other("model_catalog_contract"));
    }
    let _ = read(directory)?;
    let pending = format!(".models-cache.pending-{suffix}");
    let fd = fs::openat(
        directory,
        pending.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    struct Cleanup<'a>(&'a File, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = fs::unlinkat(self.0, self.1, fs::AtFlags::empty());
        }
    }
    let _cleanup = Cleanup(directory, &pending);
    let mut file = File::from(fd);
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::renameat(directory, pending.as_str(), directory, NAME)?;
    directory.sync_all()
}
