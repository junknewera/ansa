//! OMHJ1 framing. Torn tails are retained and linked by a new part.
use super::{Result, need, num};
use crate::protocol::{json::Json, legacy_json_v1::sha256};
use ansa_linux::journal::Files;
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::FileExt,
    path::Path,
    sync::Arc,
};

pub const MAX_RECORD: usize = 32 * 1024 * 1024;
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
fn header(index: usize, previous: &str) -> String {
    format!("OMHJ1 {index:020} {previous}\n")
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
        mut replay: impl FnMut(&Json) -> Result<()>,
        observe: Option<Observer>,
    ) -> Result<Self> {
        let files = Files::open(path).map_err(|e| {
            if e.to_string() == "session_busy" {
                "session_busy"
            } else {
                "journal_directory_open"
            }
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
            for (index, filename) in names.iter().enumerate() {
                need(
                    filename.to_str() == Some(name(index).as_str()),
                    "journal_part_sequence",
                )?;
                let mut file = journal
                    .files
                    .part(&name(index), false)
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
                        need(
                            frame[8] == b' ' && frame[17] == b' ' && frame[82] == b'\n',
                            "journal_frame",
                        )?;
                        let parse = |bytes: &[u8]| -> Result<u32> {
                            need(bytes.iter().all(u8::is_ascii_hexdigit), "journal_frame")?;
                            u32::from_str_radix(
                                std::str::from_utf8(bytes).map_err(|_| "journal_frame")?,
                                16,
                            )
                            .map_err(|_| "journal_frame")
                        };
                        let length = parse(&frame[..8])?;
                        let complement = parse(&frame[9..17])?;
                        need(
                            length ^ complement == u32::MAX
                                && length > 0
                                && length as usize <= MAX_RECORD,
                            "journal_frame_length",
                        )?;
                        let body = read(&mut file, length as usize)?;
                        if body.len() < length as usize {
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
                journal.expected_size = expected_size;
                journal.previous_part = digest(&file)?;
                journal.file = Some(file);
                journal.part = index;
            }
            if !names.is_empty() && journal.new_part {
                journal.part += 1;
                journal.file = None;
            }
            Ok(())
        })();
        if let Err(code) = result {
            journal.failed = true;
            journal.diagnostic = code;
        }
        Ok(journal)
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
        let body = Json::object(vec![
            ("schema_version", num(1)?),
            ("sequence", Json::parse(&sequence.to_string())?),
            ("previous", Json::string(&self.previous_record)?),
            ("event", event.clone()),
        ])?
        .dump();
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
