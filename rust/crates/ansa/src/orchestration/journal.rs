//! Bounded append-only orchestration records. The stores share disk primitives,
//! with disjoint paths and schema headers.
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fs::File, path::Path};

use crate::protocol::{json::Json, legacy_json_v1::sha256};

pub(super) type Result<T> = std::result::Result<T, &'static str>;
const HEADER: &str = "init.json";
const MAX_RECORDS: u16 = 240;
const MAX_BYTES: usize = Json::FRAME_BYTES;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body<T> {
    sequence: u16,
    previous: String,
    event: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame<T> {
    body: Body<T>,
    sha256: String,
}

pub(super) struct Journal {
    directory: File,
    _lock: File,
    next: u16,
    previous: String,
    failed: bool,
    #[cfg(test)]
    pub(super) fault: Option<Fault>,
    #[cfg(test)]
    pub(super) after_publish: Option<Box<dyn FnOnce()>>,
}

#[cfg(test)]
pub(super) enum Fault {
    PartialAppend,
    LostSyncAck,
}

impl Journal {
    pub(super) fn create<H: Serialize>(
        path: &Path,
        header: impl FnOnce(&File) -> Result<H>,
    ) -> Result<(Self, H)> {
        let directory = fs::create_private_leaf(path).map_err(|_| "store_create")?;
        let lock = local::lock_new(&directory).map_err(|_| "store_lock")?;
        let header = header(&directory)?;
        let bytes = encode(&header)?;
        local::create(&directory, HEADER, &bytes).map_err(|_| "store_init")?;
        Ok((Self::new(directory, lock, &bytes), header))
    }

    pub(super) fn open<H: DeserializeOwned, E: DeserializeOwned + Serialize>(
        path: &Path,
    ) -> Result<(Self, H, Vec<E>)> {
        let directory = fs::private_directory(path).map_err(|_| "store_open_existing")?;
        // An empty or partially created directory cannot become a valid store.
        let lock = local::lock_existing(&directory).map_err(|_| "store_lock")?;
        let bytes = read(&directory, HEADER)?.ok_or("store_missing_init")?;
        let header = decode(&bytes)?;
        let mut journal = Self::new(directory, lock, &bytes);
        let (names, more) = local::names(&journal.directory, usize::from(MAX_RECORDS) + 2)
            .map_err(|_| "store_inventory")?;
        let mut count = 0;
        for name in names {
            match name.to_str() {
                Some(HEADER | "worker.lock") => (),
                Some(name) if record_index(name).is_some() => count += 1,
                _ => return Err("store_incomplete_or_foreign_record"),
            }
        }
        if more || count > MAX_RECORDS {
            return Err("store_record_limit");
        }
        let mut events = Vec::with_capacity(usize::from(count));
        for sequence in 0..count {
            let bytes =
                read(&journal.directory, &filename(sequence))?.ok_or("store_missing_record")?;
            let frame: Frame<E> = decode(&bytes)?;
            if frame.body.sequence != sequence
                || frame.body.previous != journal.previous
                || frame.sha256 != sha256(&encode(&frame.body)?)
            {
                return Err("store_record_chain");
            }
            journal.advance(&bytes);
            events.push(frame.body.event);
        }
        // A rename can be visible when the writer lost the directory fsync
        // acknowledgement. Re-establish durability before returning a receipt.
        journal.directory.sync_all().map_err(|_| "store_sync")?;
        Ok((journal, header, events))
    }

    fn new(directory: File, lock: File, header: &[u8]) -> Self {
        Self {
            directory,
            _lock: lock,
            next: 0,
            previous: sha256(header),
            failed: false,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            after_publish: None,
        }
    }

    pub(super) fn append(&mut self, event: &impl Serialize) -> Result<()> {
        if self.failed || self.next == MAX_RECORDS {
            return Err("store_read_only");
        }
        let name = filename(self.next);
        let body = Body {
            sequence: self.next,
            previous: self.previous.clone(),
            event,
        };
        let bytes = encode(&Frame {
            sha256: sha256(&encode(&body)?),
            body,
        })?;
        self.failed = true;
        #[cfg(test)]
        let fault = self.fault.take();
        #[cfg(test)]
        if matches!(fault, Some(Fault::PartialAppend)) {
            local::create(
                &self.directory,
                &format!("{name}.pending"),
                &bytes[..bytes.len() / 2],
            )
            .map_err(|_| "store_append")?;
            return Err("store_append");
        }
        local::create(&self.directory, &name, &bytes).map_err(|_| "store_append")?;
        #[cfg(test)]
        if matches!(fault, Some(Fault::LostSyncAck)) {
            return Err("store_sync");
        }
        self.advance(&bytes);
        self.failed = false;
        #[cfg(test)]
        if let Some(callback) = self.after_publish.take() {
            callback();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn directory(&self) -> &File {
        &self.directory
    }

    fn advance(&mut self, bytes: &[u8]) {
        self.previous = sha256(bytes);
        self.next += 1;
    }
}

fn filename(sequence: u16) -> String {
    format!("event-{sequence:03}.json")
}

fn record_index(name: &str) -> Option<u16> {
    let sequence = name.strip_prefix("event-")?.strip_suffix(".json")?;
    if sequence.len() != 3 || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    sequence.parse().ok().filter(|value| *value < MAX_RECORDS)
}

fn read(directory: &File, name: &str) -> Result<Option<Vec<u8>>> {
    local::read(directory, name, MAX_BYTES).map_err(|_| "store_read")
}

pub(super) fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|_| "store_encode")?;
    if bytes.len() > MAX_BYTES {
        return Err("store_record_size");
    }
    Ok(bytes)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| "store_decode")
}
