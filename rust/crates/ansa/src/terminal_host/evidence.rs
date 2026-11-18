use super::protocol::{Binding, Effect, Id, Intent, VERSION, sha256};
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
}

impl FileJournal {
    /// Reserve a fresh private store. Existing headers and remnants require an
    /// explicit open and never become permission to start a replacement host.
    pub(super) fn create(path: &Path, binding: &Binding) -> Result<Self, &'static str> {
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
        })
    }

    pub(super) fn open(path: &Path, expected: &Binding) -> Result<Restored, &'static str> {
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
        let (names, more) = local::names(&directory, 256).map_err(|_| "terminal_store")?;
        let names: Vec<_> = names
            .into_iter()
            .filter(|name| name != "worker.lock" && name != HEADER)
            .collect();
        let mut records = Vec::new();
        let mut tail = Tail::End;
        for sequence in 0..MAX_RECORDS {
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
        if tail == Tail::End && (more || names.len() != records.len()) {
            let pending = format!("{}.pending", record_name(records.len()));
            // A valid published prefix remains readable even if the directory
            // listing overflows or contains a foreign record after the cap.
            tail = if !more
                && records.len() < MAX_RECORDS
                && names.len() == records.len() + 1
                && names.iter().any(|name| name == pending.as_str())
            {
                Tail::Unpublished
            } else {
                Tail::Damaged
            };
        }
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
            },
            records,
            tail,
        })
    }

    fn lock(path: &Path) -> Result<(File, File), &'static str> {
        let directory = fs::private_directory(path).map_err(|_| "terminal_store")?;
        let lock = local::lock(&directory).map_err(|_| "terminal_store_locked")?;
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
        if self.failed || self.sequence >= MAX_RECORDS {
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
