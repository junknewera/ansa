//! Per-session retention of the raw prefixes captured by the process worker.
use crate::{
    config::{n, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::Descriptor,
    tools::{evidence as v, invocation::Invocation},
};
use ansa_linux::{fs, local};
use base64::Engine;
use std::{fs::File, path::Path};

pub(crate) const REVISION: &str = "ansa_process_v2";
pub(super) const STREAM_BYTES: usize = 256 * 1024;
const RETAINED_BLOBS: usize = 32;
const PAGE_BYTES: usize = 4096;
const STORE: &str = ".output";

pub(crate) fn enabled(invocation: &Invocation) -> bool {
    invocation.name == "run_command" && invocation.revision == REVISION
}

pub(crate) fn legacy_descriptor(current: &Descriptor) -> Option<Descriptor> {
    (current.name == "run_command" && current.revision == REVISION).then(|| Descriptor {
        revision: "ansa_process_v1".into(),
        description: "Run argv without a shell unless explicitly requested. Noninteractive, no sudo, no background services. timeout_ms must be positive and not exceed the configured limit. Exit codes and truncated stdout/stderr are returned as data.".into(),
        ..current.clone()
    })
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn valid_page(args: &Json) -> v::Result<bool> {
    Ok(valid_id(args.text_field("id")?)
        && matches!(args.text_field("stream")?, "stdout" | "stderr")
        && args.usize_field("offset")? <= STREAM_BYTES
        && (1..=PAGE_BYTES).contains(&args.usize_field("limit")?))
}

fn b(value: bool) -> v::Result<Json> {
    Json::parse(if value { "true" } else { "false" })
}

fn stream(bytes: &[u8], seen: usize, pipe_eof: bool) -> v::Result<Json> {
    Json::object(vec![
        ("sha256", s(&sha256(bytes))?),
        ("stored_bytes", n(bytes.len())),
        ("observed_bytes", n(seen)),
        ("pipe_eof", b(pipe_eof)?),
        ("complete", b(pipe_eof && bytes.len() == seen)?),
    ])
}

pub(super) fn retain(
    root: &Path,
    invocation: &Invocation,
    stdout: (&[u8], usize, bool),
    stderr: (&[u8], usize, bool),
    inline_complete: bool,
) -> v::Result<Json> {
    if inline_complete {
        return Json::object(vec![("status", s("inline_only")?)]);
    }
    if stdout.0.is_empty() && stderr.0.is_empty() {
        return Json::object(vec![("status", s("spool_unavailable")?)]);
    }
    let id = sha256(invocation.operation.as_bytes());
    let status = write_blob(root, &id, stdout.0, stderr.0);
    if let Err(code) = status {
        return Json::object(vec![("status", s(code)?)]);
    }
    Json::object(vec![
        ("version", n(1)),
        ("status", s("retained")?),
        ("output_id", s(&id)?),
        ("stdout", stream(stdout.0, stdout.1, stdout.2)?),
        ("stderr", stream(stderr.0, stderr.1, stderr.2)?),
    ])
}

fn write_blob(root: &Path, id: &str, stdout: &[u8], stderr: &[u8]) -> v::Result<()> {
    let path = root.join(STORE);
    fs::create_private(&path).map_err(|_| "spool_unavailable")?;
    let store = fs::private_directory(&path).map_err(|_| "spool_unavailable")?;
    let _lock = local::lock(&store).map_err(|_| "spool_busy")?;
    let (names, more) =
        local::names(&store, RETAINED_BLOBS * 2 + 2).map_err(|_| "spool_unavailable")?;
    if more {
        return Err("spool_quota_exhausted");
    }
    let mut retained = 0;
    for name in &names {
        let name = name.to_str().ok_or("spool_unavailable")?;
        if name == "worker.lock" {
            continue;
        }
        let job_id = name.strip_suffix(".pending").unwrap_or(name);
        if !valid_id(job_id) {
            return Err("spool_unavailable");
        }
        if job_id != id && reclaim_orphan(root, &store, job_id, name) {
            continue;
        }
        retained += 1;
    }
    if retained >= RETAINED_BLOBS {
        return Err("spool_quota_exhausted");
    }
    let mut blob = Vec::with_capacity(stdout.len() + stderr.len());
    blob.extend_from_slice(stdout);
    blob.extend_from_slice(stderr);
    let pending = format!("{id}.pending");
    if local::entry_exists(&store, id).map_err(|_| "spool_unavailable")?
        || local::entry_exists(&store, &pending).map_err(|_| "spool_unavailable")?
    {
        return Err("spool_conflict");
    }
    if local::create(&store, id, &blob).is_err() {
        let _ = local::unlink(&store, &pending);
        let _ = local::unlink(&store, id);
        return Err("spool_unavailable");
    }
    Ok(())
}

pub(super) fn release_unpublished(root: &Path, id: &str) -> v::Result<()> {
    let store = fs::private_directory(&root.join(STORE)).map_err(|_| "spool_unavailable")?;
    local::unlink(&store, id).map_err(|_| "spool_unavailable")
}

// The worker holds the job lock until it publishes evidence. Once evidence
// exists, preserve the blob even if the journal has yet to reconcile the job.
fn reclaim_orphan(root: &Path, store: &File, id: &str, name: &str) -> bool {
    let Ok(job) = fs::private_directory(&root.join(id)) else {
        return false;
    };
    if !matches!(local::entry_exists(&job, "result.json"), Ok(false)) {
        return false;
    }
    let Ok(_job_lock) = local::lock_existing(&job) else {
        return false;
    };
    matches!(local::entry_exists(&job, "result.json"), Ok(false))
        && local::unlink(store, name).is_ok()
}

pub(crate) fn read_page(root: &Path, args: &Json) -> v::Result<Json> {
    if !valid_page(args)? {
        return Err("output_page_arguments");
    }
    let id = args.text_field("id")?;
    let channel = args.text_field("stream")?;
    let offset = args.usize_field("offset")?;
    let limit = args.usize_field("limit")?;
    let job = fs::private_directory(&root.join(id)).map_err(|_| "output_receipt_missing")?;
    let request = v::read(&job, "request.json")?.ok_or("output_receipt_missing")?;
    let invocation = Invocation::load(request.field("invocation")?)?;
    if sha256(invocation.operation.as_bytes()) != id || invocation.name != "run_command" {
        return Err("output_receipt_identity");
    }
    let outcome = v::admit(&job, &invocation)?;
    if !enabled(&invocation) {
        return Err("output_receipt_prefix_only");
    }
    let body = Json::parse(std::str::from_utf8(outcome.output()).map_err(|_| "invalid_utf8")?)?;
    let receipt = body
        .field("output_spool")
        .map_err(|_| "output_spool_missing")?;
    let status = receipt.text_field("status")?;
    if status != "retained" {
        return Json::object(vec![("output_id", s(id)?), ("status", s(status)?)]);
    }
    if receipt.usize_field("version")? != 1 || receipt.text_field("output_id")? != id {
        return Err("output_receipt_identity");
    }

    let stdout = receipt.field("stdout")?;
    let stderr = receipt.field("stderr")?;
    let out_len = stdout.usize_field("stored_bytes")?;
    let err_len = stderr.usize_field("stored_bytes")?;
    if out_len > STREAM_BYTES || err_len > STREAM_BYTES {
        return Err("output_receipt_bounds");
    }
    let store = fs::private_directory(&root.join(STORE)).map_err(|_| "output_blob_missing")?;
    let blob = local::read(&store, id, out_len + err_len)
        .map_err(|_| "output_blob_invalid")?
        .ok_or("output_blob_missing")?;
    if blob.len() != out_len + err_len {
        return Err("output_blob_invalid");
    }
    let (out, err) = blob.split_at(out_len);
    for (bytes, metadata, seen_field) in
        [(out, stdout, "stdout_bytes"), (err, stderr, "stderr_bytes")]
    {
        let seen = metadata.usize_field("observed_bytes")?;
        if sha256(bytes) != metadata.text_field("sha256")?
            || body.usize_field(seen_field)? != seen
            || seen < bytes.len()
            || metadata.field("complete")?.boolean()?
                != (metadata.field("pipe_eof")?.boolean()? && seen == bytes.len())
        {
            return Err("output_blob_invalid");
        }
    }
    let (bytes, metadata) = if channel == "stdout" {
        (out, stdout)
    } else {
        (err, stderr)
    };
    if offset > bytes.len() {
        return Err("output_page_bounds");
    }
    let next = (offset + limit).min(bytes.len());
    let page = &bytes[offset..next];
    Json::object(vec![
        ("output_id", s(id)?),
        ("stream", s(channel)?),
        ("offset", n(offset)),
        ("next_offset", n(next)),
        ("end_of_stored_output", b(next == bytes.len())?),
        (
            "bytes_base64",
            s(&base64::engine::general_purpose::STANDARD.encode(page))?,
        ),
        ("text", s(&fs::utf8_text(page))?),
        ("stream_receipt", metadata.clone()),
    ])
}
