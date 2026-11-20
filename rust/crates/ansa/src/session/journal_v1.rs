//! OMHJ1 framing. Torn tails are retained and linked by a new part.
use super::{Result, need, num};
use crate::protocol::{json::Json, legacy_json_v1::sha256};
pub use ansa_linux::journal::DirectoryIdentity;
use ansa_linux::journal::Files;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::FileExt,
    path::Path,
    sync::Arc,
};

pub const MAX_RECORD: usize = Json::JOURNAL_RECORD_BYTES;
const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    BeforeWrite,
    AfterWrite,
    BeforeSync,
    AfterSync,
}
pub type Observer = Arc<dyn Fn(Point, &str) -> Result<()> + Send + Sync>;
pub struct Journal {
    files: Files,
    file: Option<File>,
    part: usize,
    sequence: u64,
    previous_part: String,
    previous_record: String,
    expected_size: u64,
    new_part: bool,
    failed: bool,
    diagnostic: &'static str,
    observe: Option<Observer>,
}
fn name(index: usize) -> String {
    format!("part-{index:020}.log")
}
fn record_body(sequence: u64, previous: &str, event: &Json) -> Result<Json> {
    Json::object_with_limits(
        vec![
            ("schema_version", num(1)?),
            ("sequence", Json::parse(&sequence.to_string())?),
            ("previous", Json::string(previous)?),
            ("event", event.clone()),
        ],
        MAX_RECORD,
    )
}

pub(crate) fn check_record_frame(event: &Json) -> Result<()> {
    record_body(u64::MAX, ZERO, event).map(|_| ())
}
fn header(index: usize, previous: &str) -> String {
    format!("OMHJ1 {index:020} {previous}\n")
}
fn frame_length(frame: &[u8]) -> Result<usize> {
    need(
        frame.len() == 83 && frame[8] == b' ' && frame[17] == b' ' && frame[82] == b'\n',
        "journal_frame",
    )?;
    let parse = |bytes: &[u8]| -> Result<u32> {
        need(bytes.iter().all(u8::is_ascii_hexdigit), "journal_frame")?;
        u32::from_str_radix(std::str::from_utf8(bytes).map_err(|_| "journal_frame")?, 16)
            .map_err(|_| "journal_frame")
    };
    let length = parse(&frame[..8])?;
    need(
        length ^ parse(&frame[9..17])? == u32::MAX && length > 0 && length as usize <= MAX_RECORD,
        "journal_frame_length",
    )?;
    Ok(length as usize)
}

/// Bounded canonical framing/chain inspection. Unlike `open`, this never
/// creates a lock or a recovery part, and it never acquires the writer lease.
/// A raced append or unfinished final frame is conservatively unavailable.
pub fn inspect_read_only(
    path: &Path,
    budget: &mut usize,
    check: impl Fn() -> Result<()>,
    mut replay: impl FnMut(&Json) -> Result<()>,
) -> Result<()> {
    inspect_records_read_only(path, budget, check, |_, _, event| replay(event))?;
    Ok(())
}

pub(super) fn inspect_records_read_only(
    path: &Path,
    budget: &mut usize,
    check: impl Fn() -> Result<()>,
    mut replay: impl FnMut(u64, &str, &Json) -> Result<()>,
) -> Result<String> {
    use ansa_linux::journal::{ReadIdentity, ReadOnly};
    check()?;
    let files = ReadOnly::open(path).map_err(|_| "journal_directory_open")?;
    let names = files.names().map_err(|_| "journal_parts_limit")?;
    need(!names.is_empty(), "session_missing_init")?;
    let mut previous_part = ZERO.to_owned();
    let mut previous_record = ZERO.to_owned();
    let mut sequence = 0u64;
    let mut observed = Vec::new();
    for (index, filename) in names.iter().enumerate() {
        check()?;
        let filename = filename.to_str().ok_or("journal_part_sequence")?;
        need(filename == name(index), "journal_part_sequence")?;
        let mut file = files.part(filename).map_err(|_| "journal_part_open")?;
        // Capture before the first read, including nanosecond mtime/ctime.
        // A same-length rewrite must not validate bytes from an older version.
        let identity = ReadIdentity::capture(&file).map_err(|_| "journal_part_type")?;
        let size = usize::try_from(identity.size()).map_err(|_| "journal_summary_limit")?;
        need(size <= *budget, "journal_summary_limit")?;
        *budget -= size;
        let mut bytes = Vec::with_capacity(size.min(65536));
        let mut chunk = [0; 65536];
        while bytes.len() < size {
            check()?;
            let remaining = (size - bytes.len()).min(chunk.len());
            let read = file
                .read(&mut chunk[..remaining])
                .map_err(|_| "journal_read")?;
            need(read != 0, "journal_write_conflict")?;
            bytes.extend_from_slice(&chunk[..read]);
        }
        let expected = header(index, &previous_part);
        let actual = &bytes[..bytes.len().min(expected.len())];
        need(
            expected.as_bytes().starts_with(actual),
            "journal_part_header",
        )?;
        let mut offset = expected.len();
        let mut torn = bytes.len() < expected.len();
        while !torn && offset < bytes.len() {
            check()?;
            if bytes.len() - offset < 83 {
                torn = true;
                break;
            }
            let frame = &bytes[offset..offset + 83];
            let length = frame_length(frame)?;
            offset += 83;
            if bytes.len() - offset < length {
                torn = true;
                break;
            }
            let body = &bytes[offset..offset + length];
            let hash = sha256(body);
            need(hash.as_bytes() == &frame[18..82], "journal_checksum")?;
            let envelope = Json::parse_with_limits(
                std::str::from_utf8(body).map_err(|_| "json_invalid")?,
                MAX_RECORD,
                64,
                131072,
            )?;
            sequence = sequence.checked_add(1).ok_or("journal_sequence")?;
            need(
                envelope.usize_field("schema_version")? == 1
                    && envelope.field("sequence")?.index()? as u64 == sequence
                    && envelope.text_field("previous")? == previous_record,
                "journal_sequence",
            )?;
            replay(sequence, &hash, envelope.field("event")?)?;
            previous_record = hash;
            offset += length;
        }
        need(!torn || index + 1 < names.len(), "journal_incomplete")?;
        previous_part = sha256(&bytes);
        need(
            files
                .unchanged(filename, &file, &identity)
                .map_err(|_| "journal_write_conflict")?,
            "journal_write_conflict",
        )?;
        observed.push((filename.to_owned(), identity));
    }
    check()?;
    need(
        files.names().map_err(|_| "journal_parts_limit")? == names,
        "journal_write_conflict",
    )?;
    for (filename, identity) in observed {
        check()?;
        let file = files.part(&filename).map_err(|_| "journal_part_open")?;
        need(
            files
                .unchanged(&filename, &file, &identity)
                .map_err(|_| "journal_write_conflict")?,
            "journal_write_conflict",
        )?;
    }
    Ok(previous_record)
}
fn read(file: &mut File, count: usize) -> Result<Vec<u8>> {
    let mut value = Vec::with_capacity(count.min(65536));
    file.take(count as u64)
        .read_to_end(&mut value)
        .map_err(|_| "journal_read")?;
    Ok(value)
}
fn digest(file: &File) -> Result<String> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; 65536];
    let mut offset = 0;
    loop {
        match file.read_at(&mut buffer, offset) {
            Ok(0) => break,
            Ok(n) => {
                hash.update(&buffer[..n]);
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("journal_read"),
        }
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
impl Journal {
    pub fn open(
        path: &Path,
        replay: impl FnMut(&Json) -> Result<()>,
        observe: Option<Observer>,
    ) -> Result<Self> {
        Self::load(Files::open(path), false, replay, observe)
    }

    pub fn open_existing(
        path: &Path,
        identity: DirectoryIdentity,
        replay: impl FnMut(&Json) -> Result<()>,
        observe: Option<Observer>,
    ) -> Result<Self> {
        Self::load(Files::open_existing(path, identity), true, replay, observe)
    }

    fn load(
        files: std::io::Result<Files>,
        strict_recovery: bool,
        mut replay: impl FnMut(&Json) -> Result<()>,
        observe: Option<Observer>,
    ) -> Result<Self> {
        let files = files.map_err(|error| match error.to_string().as_str() {
            "session_busy" => "session_busy",
            "journal_directory_identity" => "journal_directory_identity",
            _ => "journal_directory_open",
        })?;
        let names = files.names().map_err(|_| "journal_parts_limit")?;
        let mut journal = Self {
            files,
            file: None,
            part: 0,
            sequence: 0,
            previous_part: ZERO.into(),
            previous_record: ZERO.into(),
            expected_size: 0,
            new_part: true,
            failed: false,
            diagnostic: "",
            observe,
        };
        let result = (|| {
            for (index, entry) in names.iter().enumerate() {
                let filename = name(index);
                need(
                    entry.to_str() == Some(filename.as_str()),
                    "journal_part_sequence",
                )?;
                let mut file = if strict_recovery {
                    journal.files.private_part(&filename)
                } else {
                    journal.files.part(&filename, false)
                }
                .map_err(|_| "journal_part_open")?;
                let expected_size = file.metadata().map_err(|_| "journal_part_type")?.len();
                let expected = header(index, &journal.previous_part);
                let actual = read(&mut file, expected.len())?;
                need(
                    expected.as_bytes().starts_with(&actual),
                    "journal_part_header",
                )?;
                journal.new_part = actual.len() != expected.len();
                if !journal.new_part {
                    loop {
                        let frame = read(&mut file, 83)?;
                        if frame.is_empty() {
                            journal.new_part = false;
                            break;
                        }
                        if frame.len() < 83 {
                            journal.new_part = true;
                            break;
                        }
                        let length = frame_length(&frame)?;
                        let body = read(&mut file, length)?;
                        if body.len() < length {
                            journal.new_part = true;
                            break;
                        }
                        let hash = sha256(&body);
                        need(hash.as_bytes() == &frame[18..82], "journal_checksum")?;
                        let envelope = Json::parse_with_limits(
                            std::str::from_utf8(&body).map_err(|_| "json_invalid")?,
                            MAX_RECORD,
                            64,
                            131072,
                        )?;
                        need(
                            envelope.usize_field("schema_version")? == 1
                                && journal.sequence < u64::MAX
                                && envelope.field("sequence")?.index()? as u64
                                    == journal.sequence + 1
                                && envelope.text_field("previous")? == journal.previous_record,
                            "journal_sequence",
                        )?;
                        replay(envelope.field("event")?)?;
                        journal.previous_record = hash;
                        journal.sequence += 1;
                    }
                }
                need(
                    file.metadata().map_err(|_| "journal_part_type")?.len() == expected_size,
                    "journal_write_conflict",
                )?;
                if strict_recovery {
                    need(!journal.new_part, "journal_incomplete")?;
                    need(
                        journal
                            .files
                            .unchanged(&filename, &file, expected_size)
                            .map_err(|_| "journal_write_conflict")?,
                        "journal_write_conflict",
                    )?;
                    // The old writer could have lost the fsync acknowledgement.
                    file.sync_all().map_err(|_| "journal_sync")?;
                }
                journal.expected_size = expected_size;
                journal.previous_part = digest(&file)?;
                journal.file = Some(file);
                journal.part = index;
            }
            if !names.is_empty() && journal.new_part {
                journal.part += 1;
                journal.file = None;
            }
            if strict_recovery {
                need(!journal.empty(), "session_missing_init")?;
                journal.files.sync().map_err(|_| "journal_sync")?;
            }
            Ok(())
        })();
        if let Err(code) = result {
            if strict_recovery {
                return Err(code);
            }
            journal.failed = true;
            journal.diagnostic = code;
        }
        Ok(journal)
    }
    pub(crate) fn identity(&self) -> Result<DirectoryIdentity> {
        self.files
            .identity()
            .map_err(|_| "journal_directory_identity")
    }
    pub fn empty(&self) -> bool {
        self.sequence == 0
    }
    pub fn diagnostic(&self) -> &'static str {
        self.diagnostic
    }
    pub fn append(&mut self, event: &Json) -> Result<()> {
        need(!self.failed, "journal_read_only")?;
        let result = self.append_inner(event);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn append_inner(&mut self, event: &Json) -> Result<()> {
        if self.new_part {
            need(self.part < 4096, "journal_parts_limit")?;
            let mut file = self
                .files
                .part(&name(self.part), true)
                .map_err(|_| "journal_part_create")?;
            let bytes = header(self.part, &self.previous_part);
            file.write_all(bytes.as_bytes())
                .map_err(|_| "journal_write")?;
            file.sync_all().map_err(|_| "journal_sync")?;
            self.files.sync().map_err(|_| "journal_sync")?;
            self.expected_size = bytes.len() as u64;
            self.file = Some(file);
            self.new_part = false;
        }
        let file = self.file.as_mut().ok_or("journal_part_open")?;
        need(
            self.files
                .unchanged(&name(self.part), file, self.expected_size)
                .map_err(|_| "journal_write_conflict")?,
            "journal_write_conflict",
        )?;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or("journal_sequence_limit")?;
        let body = record_body(sequence, &self.previous_record, event)?.dump();
        need(body.len() <= MAX_RECORD, "journal_record_limit")?;
        let length = body.len() as u32;
        let hash = sha256(body.as_bytes());
        let frame = format!("{length:08x} {:08x} {hash}\n", !length);
        let kind = event.text_field("kind")?;
        let observe = |point| match &self.observe {
            Some(f) => f(point, kind),
            None => Ok(()),
        };
        observe(Point::BeforeWrite)?;
        file.write_all(frame.as_bytes())
            .and_then(|()| file.write_all(body.as_bytes()))
            .map_err(|_| "journal_write")?;
        observe(Point::AfterWrite)?;
        observe(Point::BeforeSync)?;
        file.sync_all().map_err(|_| "journal_sync")?;
        observe(Point::AfterSync)?;
        self.expected_size += (frame.len() + body.len()) as u64;
        self.previous_record = hash;
        self.sequence = sequence;
        Ok(())
    }
}
