use super::{
    HostMode,
    protocol::{Binding, Effect, Id, Intent, VERSION, sha256},
};
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::Path};

// Two immutable records per operation, plus the header and host lock.
// Keep enough unpublished slots to close terminals after normal admission stops.
pub const NORMAL_OPERATIONS: usize = 96;
pub const MAX_OPERATIONS: usize = 112;
const MAX_RECORDS: usize = MAX_OPERATIONS * 2;
// JSON can double the maximum combined cwd/workspace/argv lengths when
// escaping quotes and backslashes. Leave room for the typed envelope as well.
const MAX_RECORD_BYTES: usize = 16 * 1024;
const HEADER: &str = "terminal-v1.json";
const SEALED: &str = "closed-v1.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Intent { value: Box<Intent> },
    Receipt { operation: Id, effect: Effect },
}

pub trait Journal {
    fn publish(&mut self, record: &Record) -> Result<(), &'static str>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tail {
    End,
    Unpublished,
    Damaged,
}

pub(super) struct Restored {
    pub journal: FileJournal,
    pub records: Vec<Record>,
    pub tail: Tail,
    pub sealed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seal {
    version: u16,
    count: usize,
    last: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u16,
    binding: Binding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sequence: usize,
    previous: String,
    record: Record,
    record_digest: String,
}

pub struct FileJournal {
    directory: File,
    _lock: File,
    sequence: usize,
    previous: String,
    failed: bool,
    record_limit: usize,
}

impl FileJournal {
    pub(super) fn identity(&self) -> Result<(u64, u64), &'static str> {
        use std::os::unix::fs::MetadataExt;
        let m = self.directory.metadata().map_err(|_| "terminal_store")?;
        Ok((m.dev(), m.ino()))
    }
    /// Reserve a fresh private store. Existing headers and remnants require an
    /// explicit open and never become permission to start a replacement host.
    pub(super) fn create(
        path: &Path,
        binding: &Binding,
        mode: HostMode,
    ) -> Result<Self, &'static str> {
        if !binding.validate() || path.starts_with(&binding.workspace) {
            return Err("terminal_binding");
        }
        let directory =
            fs::create_private_leaf(path).map_err(|_| "terminal_store_exists_or_unavailable")?;
        let lock = local::lock_new(&directory).map_err(|_| "terminal_store_locked")?;
        let header = encode(&Header {
            version: VERSION,
            binding: binding.clone(),
        })?;
        local::create(&directory, HEADER, &header).map_err(|_| "terminal_store_publish")?;
        Ok(Self {
            directory,
            _lock: lock,
            sequence: 0,
            previous: sha256(&header),
            failed: false,
            record_limit: record_limit(mode),
        })
    }

    pub(super) fn open(
        path: &Path,
        expected: &Binding,
        mode: HostMode,
    ) -> Result<Restored, &'static str> {
        if !expected.validate() || path.starts_with(&expected.workspace) {
            return Err("terminal_binding");
        }
        let (directory, lock) = Self::lock(path)?;
        let bytes = read(&directory, HEADER)?;
        let header: Header = decode(&bytes)?;
        if header.version != VERSION || &header.binding != expected {
            return Err("terminal_binding");
        }
        let mut previous = sha256(&bytes);
        let record_limit = record_limit(mode);
        let entries = local::count_names(
            &directory,
            &["worker.lock", HEADER, SEALED],
            record_limit.saturating_add(1),
        )
        .map_err(|_| "terminal_store")?;
        let mut records = Vec::new();
        let mut tail = Tail::End;
        for sequence in 0..entries.min(record_limit) {
            let filename = record_name(sequence);
            match read_entry(&directory, &filename, sequence, &previous) {
                Ok(Some((record, digest))) => {
                    previous = digest;
                    records.push(record);
                }
                Ok(None) => break,
                Err(_) => {
                    tail = Tail::Damaged;
                    break;
                }
            }
        }
        if tail == Tail::End && entries != records.len() {
            let pending = format!("{}.pending", record_name(records.len()));
            // A valid published prefix remains readable even if the directory
            // listing overflows or contains a foreign record after the cap.
            tail = if records.len() < record_limit
                && entries == records.len() + 1
                && local::entry_exists(&directory, &pending).unwrap_or(false)
            {
                Tail::Unpublished
            } else {
                Tail::Damaged
            };
        }
        let sealed = match local::read(&directory, SEALED, MAX_RECORD_BYTES) {
            Ok(None) => false,
            Ok(Some(bytes))
                if tail == Tail::End
                    && decode::<Seal>(&bytes).is_ok_and(|seal| {
                        seal.version == VERSION
                            && seal.count == records.len()
                            && seal.last == previous
                    }) =>
            {
                true
            }
            _ => {
                tail = Tail::Damaged;
                false
            }
        };
        // An earlier owner may have stopped between rename and directory
        // fsync. Establish durability before reporting a visible receipt.
        directory.sync_all().map_err(|_| "terminal_store_sync")?;
        Ok(Restored {
            journal: Self {
                directory,
                _lock: lock,
                sequence: records.len(),
                previous,
                failed: true,
                record_limit,
            },
            records,
            tail,
            sealed,
        })
    }

    pub(super) fn seal(&mut self) -> Result<(), &'static str> {
        if self.failed {
            return Err("terminal_store_unavailable");
        }
        let data = encode(&Seal {
            version: VERSION,
            count: self.sequence,
            last: self.previous.clone(),
        })?;
        self.failed = true;
        local::create(&self.directory, SEALED, &data).map_err(|_| "terminal_store_publish")
    }

    fn lock(path: &Path) -> Result<(File, File), &'static str> {
        let directory = fs::private_directory(path).map_err(|_| "terminal_store")?;
        let lock = local::lock_existing(&directory).map_err(|_| "terminal_store_locked")?;
        Ok((directory, lock))
    }
}

fn read_entry(
    directory: &File,
    filename: &str,
    sequence: usize,
    previous: &str,
) -> Result<Option<(Record, String)>, &'static str> {
    let Some(bytes) =
        local::read(directory, filename, MAX_RECORD_BYTES).map_err(|_| "terminal_store_read")?
    else {
        return Ok(None);
    };
    let entry: Entry = decode(&bytes)?;
    if entry.sequence != sequence
        || entry.previous != previous
        || entry.record_digest != sha256(&encode(&entry.record)?)
    {
        return Err("terminal_store_sequence");
    }
    Ok(Some((entry.record, sha256(&bytes))))
}

impl Journal for FileJournal {
    fn publish(&mut self, record: &Record) -> Result<(), &'static str> {
        if self.failed || self.sequence >= self.record_limit {
            return Err("terminal_store_unavailable");
        }
        let entry = Entry {
            sequence: self.sequence,
            previous: self.previous.clone(),
            record: record.clone(),
            record_digest: sha256(&encode(record)?),
        };
        let bytes = encode(&entry)?;
        if local::create(&self.directory, &record_name(self.sequence), &bytes).is_err() {
            self.failed = true;
            return Err("terminal_store_publish");
        }
        self.previous = sha256(&bytes);
        self.sequence += 1;
        Ok(())
    }
}

fn record_limit(mode: HostMode) -> usize {
    match mode {
        HostMode::Bounded => MAX_RECORDS,
        HostMode::Conversation => usize::MAX,
    }
}

fn record_name(sequence: usize) -> String {
    format!("{sequence:04}.json")
}

fn read(directory: &File, name: &str) -> Result<Vec<u8>, &'static str> {
    local::read(directory, name, MAX_RECORD_BYTES)
        .map_err(|_| "terminal_store_read")?
        .ok_or("terminal_store_missing")
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, &'static str> {
    let bytes = serde_json::to_vec(value).map_err(|_| "terminal_protocol")?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("terminal_store_limit");
    }
    Ok(bytes)
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, &'static str> {
    serde_json::from_slice(bytes).map_err(|_| "terminal_protocol")
}
