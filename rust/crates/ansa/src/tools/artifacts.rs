//! Immutable per-operation analysis files. The journal remains the authority
//! for which descriptors are committed; a receipt alone is execution evidence.
use crate::{
    config::{Result, n, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    tools::{evidence, invocation::Invocation},
};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub const DATASET_BYTES: usize = 4 * 1024 * 1024;
pub const BLOB_BYTES: usize = 16 * 1024 * 1024;
const STORE_BYTES: usize = 64 * 1024 * 1024;
const RESERVATION: usize = 24 * 1024 * 1024;
pub const OUTPUTS: [(&str, &str); 3] = [
    ("report.md", "text/markdown"),
    ("table.csv", "text/csv"),
    ("chart.png", "image/png"),
];

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    session: String,
}
impl Store {
    pub fn new(session: &Path) -> Result<Self> {
        let session_id = session
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or("artifact_session")?;
        if !crate::provider::contracts::valid_id(session_id) {
            return Err("artifact_session");
        }
        Ok(Self {
            root: session.join("artifacts"),
            session: session_id.into(),
        })
    }
    pub fn job_path(&self, invocation: &Invocation) -> PathBuf {
        self.root.join(sha256(invocation.operation.as_bytes()))
    }
    pub fn directory(&self) -> Result<File> {
        ansa_linux::fs::create_private(&self.root).map_err(|_| "artifact_store_permissions")?;
        ansa_linux::fs::private_directory(&self.root).map_err(|_| "artifact_store_permissions")
    }
    pub fn reserve(&self) -> Result<File> {
        let directory = self.directory()?;
        let lease = ansa_linux::local::lock(&directory).map_err(|_| "analysis_busy")?;
        let (names, more) =
            ansa_linux::local::names(&directory, 128).map_err(|_| "artifact_quota")?;
        if more {
            return Err("artifact_quota");
        }
        let mut used = 0usize;
        for name in names {
            if name == "worker.lock" {
                continue;
            }
            let name = name.to_str().ok_or("artifact_store_entry")?;
            if !hash(name) {
                return Err("artifact_store_entry");
            }
            let path = self.root.join(name);
            let job = ansa_linux::fs::private_directory(&path)
                .map_err(|_| "artifact_store_permissions")?;
            let bytes = measure(&path, 0)?;
            let ready = read(&job, "receipt.json", 128 * 1024)?.is_some();
            used = used
                .checked_add(if ready { bytes } else { bytes.max(RESERVATION) })
                .ok_or("artifact_quota")?;
        }
        if used > STORE_BYTES - RESERVATION {
            return Err("artifact_quota");
        }
        Ok(lease)
    }
    pub(super) fn publish(&self, invocation: &Invocation, proof: &Json) -> Result<Json> {
        let path = self.job_path(invocation);
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "artifact_store_permissions")?;
        let request =
            evidence::read(&directory, "analysis.json")?.ok_or("analysis_evidence_missing")?;
        if *request.field("invocation")? != invocation.save()? {
            return Err("analysis_evidence_identity");
        }
        let worker_hash = analysis_input(&directory, &request)?;
        if proof.field("inputs")?.text_field("input.csv")? != worker_hash {
            return Err("analysis_input_changed");
        }
        for (name, limit) in [("worker.py", 64 * 1024), ("parameters.json", 4096)] {
            let bytes = read(&directory, name, limit)?.ok_or("analysis_evidence_missing")?;
            if proof.field("inputs")?.text_field(name)? != sha256(&bytes) {
                return Err("analysis_input_changed");
            }
        }
        let summary =
            evidence::read(&directory, "summary.json")?.ok_or("analysis_summary_missing")?;
        if *proof.field("summary")? != summary || proof.field("outputs")?.keys()?.len() != 3 {
            return Err("analysis_output_proof");
        }
        if summary.usize_field("rows")? > 100000
            || summary.usize_field("months")? > 1200
            || summary.text_field("total")?.len() > 64
        {
            return Err("analysis_summary");
        }
        let revision = sha256(invocation.operation.as_bytes());
        let key = invocation.arguments.text_field("report_key")?;
        let mut descriptors = vec![];
        let mut bytes = 0;
        for (name, mime) in OUTPUTS {
            let blob = read(&directory, name, BLOB_BYTES)?.ok_or("artifact_missing")?;
            bytes += blob.len();
            if bytes > BLOB_BYTES {
                return Err("artifact_output_limit");
            }
            let known = proof.field("outputs")?.field(name)?;
            if known.text_field("sha256")? != sha256(&blob)
                || known.usize_field("size_bytes")? != blob.len()
            {
                return Err("analysis_output_changed");
            }
            check_blob(mime, &blob)?;
            // Reopen through the anchored directory, checking identity/type as
            // well as bytes, then sync before publishing the complete receipt.
            ansa_linux::local::sync_regular(&directory, name).map_err(|_| "artifact_sync")?;
            descriptors.push(Json::object(vec![
                (
                    "artifact_id",
                    s(&format!(
                        "a_{}_{}",
                        &sha256(key.as_bytes())[..16],
                        name.split('.').next().unwrap()
                    ))?,
                ),
                ("revision", s(&revision)?),
                ("session_id", s(&self.session)?),
                ("producer_operation_id", s(&invocation.operation)?),
                ("mime", s(mime)?),
                ("size_bytes", n(blob.len())),
                ("sha256", s(&sha256(&blob))?),
                ("title", s(name)?),
                ("storage_ref", s(&format!("{revision}/{name}"))?),
            ])?);
        }
        let value = Json::object(vec![
            ("status", s("ready")?),
            ("input_sha256", request.field("input_sha256")?.clone()),
            ("summary", summary),
            ("artifacts", Json::array(descriptors)?),
        ])?;
        if let Some(prior) = evidence::read(&directory, "receipt.json")? {
            if *prior.field("invocation")? != invocation.save()? || *prior.field("result")? != value
            {
                return Err("artifact_receipt_conflict");
            }
        } else {
            evidence::create(
                &directory,
                "receipt.json",
                &Json::object(vec![
                    ("invocation", invocation.save()?),
                    ("result", value.clone()),
                ])?,
            )?;
        }
        directory.sync_all().map_err(|_| "artifact_sync")?;
        Ok(value)
    }
    pub(super) fn record_failure(&self, invocation: &Invocation, value: &Json) -> Result<()> {
        let directory = ansa_linux::fs::private_directory(&self.job_path(invocation))
            .map_err(|_| "artifact_store_permissions")?;
        let receipt = Json::object(vec![
            ("invocation", invocation.save()?),
            ("result", value.clone()),
        ])?;
        if let Some(prior) = evidence::read(&directory, "receipt.json")? {
            if prior != receipt {
                return Err("artifact_receipt_conflict");
            }
        } else {
            evidence::create(&directory, "receipt.json", &receipt)?;
        }
        Ok(())
    }
    pub(super) fn saved_receipt(&self, invocation: &Invocation) -> Result<Option<Json>> {
        let directory = ansa_linux::fs::private_directory(&self.job_path(invocation))
            .map_err(|_| "artifact_store_permissions")?;
        let Some(receipt) = evidence::read(&directory, "receipt.json")? else {
            return Ok(None);
        };
        let (names, more) =
            ansa_linux::local::names(&directory, 7).map_err(|_| "artifact_store_permissions")?;
        if more
            || names.iter().any(|name| {
                !matches!(
                    name.to_str(),
                    Some(
                        "analysis.json"
                            | "input.csv"
                            | "leading-empty.csv"
                            | "parameters.json"
                            | "worker.py"
                            | "receipt.json"
                    )
                )
            })
        {
            return Err("analysis_pre_start_receipt");
        }
        if receipt.keys()?.len() != 2 || *receipt.field("invocation")? != invocation.save()? {
            return Err("artifact_receipt_conflict");
        }
        Ok(Some(receipt.field("result")?.clone()))
    }
    /// Caller supplies a descriptor obtained from a canonical transcript page,
    /// never an arbitrary client path or an uncommitted execution receipt.
    pub fn resolve(&self, descriptor: &Json) -> Result<PathBuf> {
        self.load(descriptor).map(|(path, _)| path)
    }
    pub fn load(&self, descriptor: &Json) -> Result<(PathBuf, Vec<u8>)> {
        validate_descriptor(descriptor, &self.session, None)?;
        let operation = descriptor.text_field("producer_operation_id")?;
        let revision = descriptor.text_field("revision")?;
        if sha256(operation.as_bytes()) != revision {
            return Err("artifact_identity");
        }
        let filename = descriptor
            .text_field("storage_ref")?
            .strip_prefix(&format!("{revision}/"))
            .ok_or("artifact_identity")?;
        let path = self.root.join(revision);
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "artifact_unavailable")?;
        let receipt = evidence::read(&directory, "receipt.json")?.ok_or("artifact_unavailable")?;
        if receipt.field("invocation")?.text_field("operation")? != operation
            || !receipt
                .field("result")?
                .field("artifacts")?
                .elements()?
                .contains(descriptor)
        {
            return Err("artifact_identity");
        }
        let bytes = read(&directory, filename, BLOB_BYTES)?.ok_or("artifact_missing")?;
        if bytes.len() != descriptor.usize_field("size_bytes")?
            || sha256(&bytes) != descriptor.text_field("sha256")?
        {
            return Err("artifact_corrupt");
        }
        check_blob(descriptor.text_field("mime")?, &bytes)?;
        Ok((path.join(filename), bytes))
    }
}
fn measure(path: &Path, depth: usize) -> Result<usize> {
    if depth > 2 {
        return Err("artifact_quota");
    }
    let dir = ansa_linux::fs::private_directory(path).map_err(|_| "artifact_store_permissions")?;
    let (names, more) = ansa_linux::local::names(&dir, 128).map_err(|_| "artifact_quota")?;
    if more {
        return Err("artifact_quota");
    }
    let mut total = 0usize;
    for name in names {
        let p = path.join(name);
        let meta = std::fs::symlink_metadata(&p).map_err(|_| "artifact_quota")?;
        let bytes = if meta.is_dir() {
            measure(&p, depth + 1)?
        } else {
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.uid() != dir.metadata().map_err(|_| "artifact_quota")?.uid()
                || meta.mode() & 0o077 != 0
            {
                return Err("artifact_store_permissions");
            }
            usize::try_from(meta.len()).map_err(|_| "artifact_quota")?
        };
        total = total.checked_add(bytes).ok_or("artifact_quota")?;
        if total > STORE_BYTES {
            return Err("artifact_quota");
        }
    }
    Ok(total)
}
pub fn read(dir: &File, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
    ansa_linux::local::read(dir, name, limit).map_err(|_| "artifact_file_permissions")
}

pub(super) fn analysis_input(directory: &File, saved: &Json) -> Result<String> {
    let input = read(directory, "input.csv", DATASET_BYTES)?.ok_or("analysis_input_missing")?;
    let hash = sha256(&input);
    let original = if saved.has("worker_input_sha256") {
        if saved.text_field("worker_input_sha256")? != hash {
            return Err("analysis_input_changed");
        }
        let mut prefix = read(directory, "leading-empty.csv", DATASET_BYTES - input.len())?
            .ok_or("analysis_input_missing")?;
        if prefix.is_empty() || super::analysis::csv_prefix(&prefix) != prefix.len() {
            return Err("analysis_input_changed");
        }
        prefix.extend(input);
        sha256(&prefix)
    } else {
        hash.clone()
    };
    if saved.text_field("input_sha256")? != original {
        return Err("analysis_input_changed");
    }
    Ok(hash)
}
fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
pub fn validate_descriptor(d: &Json, session: &str, operation: Option<&str>) -> Result<()> {
    if d.keys()?.len() != 9
        || !crate::provider::contracts::valid_id(d.text_field("artifact_id")?)
        || d.text_field("session_id")? != session
        || operation.is_some_and(|v| d.text_field("producer_operation_id").ok() != Some(v))
        || !hash(d.text_field("revision")?)
        || !hash(d.text_field("sha256")?)
        || !(1..=BLOB_BYTES).contains(&d.usize_field("size_bytes")?)
    {
        return Err("artifact_descriptor");
    }
    let revision = d.text_field("revision")?;
    let matches = OUTPUTS.iter().any(|(name, mime)| {
        d.text_field("mime").ok() == Some(*mime)
            && d.text_field("title").ok() == Some(*name)
            && d.text_field("storage_ref").ok() == Some(format!("{revision}/{name}").as_str())
    });
    if !matches {
        return Err("artifact_descriptor");
    }
    Ok(())
}
fn check_blob(mime: &str, bytes: &[u8]) -> Result<()> {
    if mime == "image/png" {
        if bytes.len() < 33 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
            return Err("artifact_png");
        }
        let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap()) as u64;
        let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap()) as u64;
        if w == 0 || h == 0 || w * h > 8_000_000 {
            return Err("artifact_png_limit");
        }
        let mut cursor = 8;
        let mut data = false;
        let mut first = true;
        let mut count = 0;
        while cursor + 12 <= bytes.len() {
            count += 1;
            if count > 4096 {
                return Err("artifact_png");
            }
            let size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            let end = cursor
                .checked_add(12)
                .and_then(|v| v.checked_add(size))
                .ok_or("artifact_png")?;
            if end > bytes.len() {
                return Err("artifact_png");
            }
            let kind = &bytes[cursor + 4..cursor + 8];
            if !kind.iter().all(u8::is_ascii_alphabetic)
                || first && (kind != b"IHDR" || size != 13)
                || !first && kind == b"IHDR"
            {
                return Err("artifact_png");
            }
            first = false;
            let stored = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
            if crc32(&bytes[cursor + 4..end - 4]) != stored {
                return Err("artifact_png_crc");
            }
            if kind == b"IDAT" && size > 0 {
                data = true;
            }
            if kind == b"IEND" {
                return if data && size == 0 && end == bytes.len() {
                    Ok(())
                } else {
                    Err("artifact_png")
                };
            }
            cursor = end;
        }
        return Err("artifact_png");
    } else if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() || bytes.len() > 256 * 1024
    {
        return Err("artifact_text");
    }
    Ok(())
}
fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0; 256];
        let mut i = 0;
        while i < 256 {
            let mut value = i as u32;
            let mut bit = 0;
            while bit < 8 {
                value = (value >> 1) ^ if value & 1 == 1 { 0xedb88320 } else { 0 };
                bit += 1;
            }
            t[i] = value;
            i += 1;
        }
        t
    };
    let mut crc = u32::MAX;
    for byte in bytes {
        crc = (crc >> 8) ^ TABLE[((crc ^ u32::from(*byte)) & 255) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_requires_crc_data_and_complete_end_with_bounded_dimensions() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        let mut header = b"\x89PNG\r\n\x1a\n".to_vec();
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&1u32.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        header.extend_from_slice(&crc32(&header[12..]).to_be_bytes());
        assert_eq!(header.len(), 33);
        assert!(
            check_blob("image/png", &header).is_err(),
            "header-only image was accepted"
        );
        let mut corrupt = header.clone();
        corrupt[32] ^= 1;
        assert!(matches!(
            check_blob("image/png", &corrupt),
            Err("artifact_png_crc")
        ));
        let mut huge = header;
        huge[16..20].copy_from_slice(&9000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&9000u32.to_be_bytes());
        assert!(matches!(
            check_blob("image/png", &huge),
            Err("artifact_png_limit")
        ));
    }
}
