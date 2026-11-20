//! One bounded, supervised monthly aggregate; never arbitrary Python source.
use super::{
    artifacts::{DATASET_BYTES, Store},
    command::ProcessOptions,
    evidence,
    files::{Host, error, path_argument, regular, result, schema},
    invocation::Invocation,
    registry::{Definition, Outcome, Policy, Registry},
};
use crate::{
    auth::Cancel,
    config::{Result, n, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        replay::{Effects, Execution},
        request::Descriptor,
    },
};
use std::{io::Read, os::unix::fs::MetadataExt, sync::Arc};
const SCRIPT: &str = include_str!("analysis/worker.py");
const PYTHON: Option<&str> = option_env!("ANSA_ANALYSIS_PYTHON");
fn analysis_revision(script_hash: &str) -> String {
    let identity = format!("{script_hash}:{}", PYTHON.unwrap_or("unavailable"));
    format!("ansa_analysis_{}", sha256(identity.as_bytes()))
}

fn rejected(code: &str) -> Outcome {
    if code == "cancelled" {
        return Outcome::not_started();
    }
    Outcome::from_bytes(
        Execution::Failed,
        Effects::KnownResult,
        error(code).output().to_vec(),
    )
}

enum Input {
    Instructions(Outcome),
    Data(Vec<u8>),
}
fn dataset(host: &Host, args: &Json, stop: &Cancel) -> Result<Input> {
    let path = host.path(args.text_field("path")?)?;
    if !path.starts_with(&host.settings.cwd) {
        return Err("dataset_scope");
    }
    if let Some(value) = host.instructions(path.parent().ok_or("file_path")?, args)? {
        return Ok(Input::Instructions(value));
    }
    if stop.cancelled() {
        return Err("cancelled");
    }
    let mut file = regular(&path).map_err(|_| "dataset_regular_file")?;
    let before = file.metadata().map_err(|_| "dataset_metadata")?;
    if before.len() == 0 || before.len() > DATASET_BYTES as u64 {
        return Err("dataset_size");
    }
    let mut bytes = vec![];
    file.by_ref()
        .take(DATASET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "dataset_read")?;
    let after = file.metadata().map_err(|_| "dataset_metadata")?;
    let current = std::fs::symlink_metadata(&path).map_err(|_| "dataset_changed")?;
    let identity = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.nlink(),
        )
    };
    if bytes.len() > DATASET_BYTES
        || identity(&before) != identity(&after)
        || identity(&before) != identity(&current)
        || host.path(args.text_field("path")?)? != path
    {
        return Err("dataset_changed");
    }
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Err("dataset_utf8");
    }
    if stop.cancelled() {
        return Err("cancelled");
    }
    Ok(Input::Data(bytes))
}

/// CSV with quoted fields/newlines and CRLF. Malformed/oversized records are
/// rejected; inspection never reports a successful truncated row count.
fn csv(bytes: &[u8], stop: &Cancel, mut row: impl FnMut(Vec<String>) -> Result<()>) -> Result<()> {
    let mut fields = vec![];
    let mut field = vec![];
    let (mut quoted, mut closed, mut started) = (false, false, false);
    let (mut i, mut rows) = (0, 0);
    while i < bytes.len() {
        let byte = bytes[i];
        i += 1;
        if quoted {
            if byte == b'"' {
                if bytes.get(i) == Some(&b'"') {
                    field.push(b'"');
                    i += 1;
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(byte);
            }
        } else if byte == b',' || byte == b'\n' || byte == b'\r' {
            if byte == b'\r' {
                if bytes.get(i) != Some(&b'\n') {
                    return Err("csv_newline");
                }
                i += 1;
            }
            let blank = byte != b',' && !started && fields.is_empty() && field.is_empty();
            if !blank {
                fields.push(String::from_utf8(std::mem::take(&mut field)).map_err(|_| "csv_utf8")?);
                if fields.len() > 64 {
                    return Err("csv_columns");
                }
                if byte != b',' {
                    rows += 1;
                    if rows > 100001 {
                        return Err("csv_rows");
                    }
                    row(std::mem::take(&mut fields))?;
                }
            }
            closed = false;
            started = false;
            if stop.cancelled() {
                return Err("cancelled");
            }
        } else if closed {
            return Err("csv_quote");
        } else if byte == b'"' {
            if started || !field.is_empty() {
                return Err("csv_quote");
            }
            quoted = true;
            started = true;
        } else {
            field.push(byte);
            started = true;
        }
        if field.len() > 4096 {
            return Err("csv_field");
        }
    }
    if quoted {
        return Err("csv_quote");
    }
    if started || closed || !field.is_empty() || !fields.is_empty() {
        fields.push(String::from_utf8(field).map_err(|_| "csv_utf8")?);
        if fields.len() > 64 || rows >= 100001 {
            return Err("csv_rows");
        }
        row(fields)?;
    }
    Ok(())
}
fn scaled(value: &str) -> Option<i128> {
    let (sign, value) = value.strip_prefix('-').map_or((1, value), |v| (-1, v));
    let mut parts = value.split('.');
    let integer = parts.next()?;
    let fraction = parts.next().unwrap_or("");
    if integer.is_empty()
        || integer.len() > 18
        || fraction.len() > 6
        || parts.next().is_some()
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || value.ends_with('.')
    {
        return None;
    }
    Some(
        sign * (integer.parse::<i128>().ok()? * 1_000_000
            + fraction.parse::<i128>().unwrap_or(0) * 10i128.pow(6 - fraction.len() as u32)),
    )
}
fn decimal(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let value = value.abs();
    if value % 1_000_000 == 0 {
        return format!("{sign}{}", value / 1_000_000);
    }
    format!("{sign}{}.{:06}", value / 1_000_000, value % 1_000_000)
        .trim_end_matches('0')
        .into()
}
#[derive(Default)]
struct Column {
    missing: usize,
    nonempty: usize,
    invalid: bool,
    min: Option<i128>,
    max: Option<i128>,
    sum: i128,
}
fn inspect(bytes: &[u8], stop: &Cancel) -> Result<Json> {
    let mut header: Vec<String> = vec![];
    let mut stats: Vec<Column> = vec![];
    let mut rows = 0;
    let mut sample = vec![];
    csv(bytes, stop, |fields| {
        if header.is_empty() {
            let mut names = std::collections::BTreeSet::new();
            if fields
                .iter()
                .any(|v| v.is_empty() || !names.insert(v.clone()))
            {
                return Err("csv_header");
            }
            stats = (0..fields.len()).map(|_| Column::default()).collect();
            header = fields;
            return Ok(());
        }
        if fields.len() != header.len() {
            return Err("csv_shape");
        }
        rows += 1;
        if sample.len() < 5 {
            sample.push(Json::array(
                fields
                    .iter()
                    .map(|v| s(&v.chars().take(128).collect::<String>()))
                    .collect::<Result<_>>()?,
            )?);
        }
        for (value, column) in fields.iter().zip(&mut stats) {
            if value.is_empty() {
                column.missing += 1;
                continue;
            }
            column.nonempty += 1;
            if let Some(value) = scaled(value) {
                column.min = Some(column.min.map_or(value, |v| v.min(value)));
                column.max = Some(column.max.map_or(value, |v| v.max(value)));
                column.sum += value;
            } else {
                column.invalid = true;
            }
        }
        Ok(())
    })?;
    if header.is_empty() {
        return Err("csv_header");
    }
    let columns = header
        .iter()
        .zip(stats)
        .map(|(name, c)| {
            let mut fields = vec![
                ("name", s(name)?),
                ("missing", n(c.missing)),
                ("nonempty", n(c.nonempty)),
            ];
            if !c.invalid
                && let (Some(min), Some(max)) = (c.min, c.max)
            {
                fields.extend([
                    ("min", s(&decimal(min))?),
                    ("max", s(&decimal(max))?),
                    ("sum", s(&decimal(c.sum))?),
                ]);
            }
            Json::object(fields)
        })
        .collect::<Result<Vec<_>>>()?;
    Json::object(vec![
        ("format", s("csv")?),
        ("rows", n(rows)),
        ("bytes", n(bytes.len())),
        ("sha256", s(&sha256(bytes))?),
        ("columns", Json::array(columns)?),
        ("sample", Json::array(sample)?),
    ])
}
fn parameters(invocation: &Invocation) -> Result<Json> {
    Json::object(vec![
        (
            "date_column",
            s(invocation.arguments.text_field("date_column")?)?,
        ),
        (
            "value_column",
            s(invocation.arguments.text_field("value_column")?)?,
        ),
    ])
}

fn job_parameters(invocation: &Invocation, saved: &Json) -> Result<Json> {
    let parameters = parameters(invocation)?;
    if saved.has("worker_input_sha256") {
        parameters.with("source_sha256", saved.field("input_sha256")?.clone())
    } else {
        Ok(parameters)
    }
}

fn verify_inputs(host: &Host, invocation: &Invocation) -> Result<()> {
    let store = Store::new(&host.session)?;
    let directory = ansa_linux::fs::private_directory(&store.job_path(invocation))
        .map_err(|_| "analysis_job_permissions")?;
    let saved = evidence::read(&directory, "analysis.json")?.ok_or("analysis_evidence_missing")?;
    let script = super::artifacts::read(&directory, "worker.py", 64 * 1024)?
        .ok_or("analysis_evidence_missing")?;
    super::artifacts::analysis_input(&directory, &saved)?;
    if *saved.field("invocation")? != invocation.save()?
        || saved.text_field("python")? != PYTHON.ok_or("analysis_environment_unavailable")?
        || saved.text_field("script_sha256")? != sha256(SCRIPT.as_bytes())
        || script != SCRIPT.as_bytes()
        || evidence::read(&directory, "parameters.json")?
            != Some(job_parameters(invocation, &saved)?)
    {
        return Err("analysis_evidence_identity");
    }
    Ok(())
}

pub(super) fn csv_prefix(mut bytes: &[u8]) -> usize {
    let len = bytes.len();
    while let Some(rest) = bytes
        .strip_prefix(b"\n")
        .or_else(|| bytes.strip_prefix(b"\r\n"))
    {
        bytes = rest;
    }
    len - bytes.len()
}
fn failure_result(code: &str, status: &str, process: Json) -> Result<Json> {
    Json::object(vec![
        ("status", s(status)?),
        ("error", s(code)?),
        ("process", process),
    ])
}

fn finish(host: &Host, invocation: &Invocation, outcome: Outcome) -> Result<Outcome> {
    if outcome.effects() != Effects::KnownResult {
        return Ok(outcome);
    }
    verify_inputs(host, invocation)?;
    let process =
        Json::parse(std::str::from_utf8(outcome.output()).map_err(|_| "analysis_result")?)?;
    let succeeded = outcome.execution() == Execution::Succeeded
        && process.field("exit_code")?.dump() == "0"
        && process.field("signal")?.dump() == "0"
        && !process.field("cancelled")?.boolean()?
        && !process.field("timed_out")?.boolean()?
        && process.field("cleanup_complete")?.boolean()?;
    let store = Store::new(&host.session)?;
    if succeeded {
        if process.field("truncated")?.boolean()? {
            return Err("analysis_result_truncated");
        }
        let proof = Json::parse(process.text_field("stdout")?)?;
        return Ok(result(store.publish(invocation, &proof)?));
    }
    let cancelled = outcome.execution() == Execution::Cancelled
        || process
            .field("cancelled")
            .and_then(Json::boolean)
            .unwrap_or(false);
    let timed_out = process
        .field("timed_out")
        .and_then(Json::boolean)
        .unwrap_or(false);
    let code = if cancelled {
        "analysis_cancelled"
    } else if timed_out {
        "analysis_timed_out"
    } else {
        "analysis_failed"
    };
    let value = failure_result(
        code,
        if cancelled { "cancelled" } else { "failed" },
        process,
    )?;
    store.record_failure(invocation, &value)?;
    Ok(Outcome::from_bytes(
        if cancelled {
            Execution::Cancelled
        } else {
            Execution::Failed
        },
        Effects::KnownResult,
        value.dump().into_bytes(),
    ))
}
fn recover(host: &Host, invocation: &Invocation) -> Result<Option<Outcome>> {
    let store = Store::new(&host.session)?;
    if !store.job_path(invocation).exists() {
        return Ok(None);
    }
    verify_inputs(host, invocation)?;
    if host.processes.has_job(invocation)? {
        return host
            .processes
            .recover(invocation)?
            .map(|value| finish(host, invocation, value))
            .transpose();
    }
    // ProcessRunner can report known cancellation before creating its job.
    // Only its exact pre-start outcome is recoverable from an analysis receipt.
    let Some(saved) = store.saved_receipt(invocation)? else {
        return Ok(None);
    };
    let cancelled = failure_result(
        "analysis_cancelled",
        "cancelled",
        Json::parse(r#"{"error":"cancelled"}"#)?,
    )?;
    if saved != cancelled {
        return Err("analysis_pre_start_receipt");
    }
    Ok(Some(Outcome::from_bytes(
        Execution::Cancelled,
        Effects::KnownResult,
        saved.dump().into_bytes(),
    )))
}
fn run(host: &Host, invocation: &Invocation, stop: &Cancel) -> Result<Outcome> {
    let store = Store::new(&host.session)?;
    // A consumed operation can only be reconciled, never prepared again using
    // a changed dataset, interpreter or command ID.
    if store.job_path(invocation).exists() {
        return Ok(recover(host, invocation)?.unwrap_or_else(Outcome::failed_unknown));
    }
    let Some(python) = PYTHON else {
        return Ok(rejected("analysis_environment_unavailable"));
    };
    let bytes = match dataset(host, &invocation.arguments, stop) {
        Ok(Input::Instructions(value)) => return Ok(value),
        Ok(Input::Data(bytes)) => bytes,
        Err(code) => return Ok(rejected(code)),
    };
    if let Err(code) = inspect(&bytes, stop) {
        return Ok(rejected(code));
    }
    if stop.cancelled() {
        return Ok(Outcome::not_started());
    }
    let _lease = match store.reserve() {
        Ok(lease) => lease,
        Err(code) => return Ok(rejected(code)),
    };
    let path = store.job_path(invocation);
    ansa_linux::fs::create_private(&path).map_err(|_| "analysis_job_create")?;
    let directory =
        ansa_linux::fs::private_directory(&path).map_err(|_| "analysis_job_permissions")?;
    // The inspection parser skips empty records before the header. Feed the
    // same records to DictReader and keep enough evidence for the exact input.
    let (prefix, input) = bytes.split_at(csv_prefix(&bytes));
    let mut saved = Json::object(vec![
        ("invocation", invocation.save()?),
        ("input_sha256", s(&sha256(&bytes))?),
        ("script_sha256", s(&sha256(SCRIPT.as_bytes()))?),
        ("python", s(python)?),
    ])?;
    if !prefix.is_empty() {
        saved = saved.with("worker_input_sha256", s(&sha256(input))?)?;
        ansa_linux::local::create(&directory, "leading-empty.csv", prefix)
            .map_err(|_| "analysis_input_publish")?;
    }
    evidence::create(&directory, "analysis.json", &saved)?;
    ansa_linux::local::create(&directory, "input.csv", input)
        .map_err(|_| "analysis_input_publish")?;
    ansa_linux::local::create(&directory, "worker.py", SCRIPT.as_bytes())
        .map_err(|_| "analysis_script_publish")?;
    evidence::create(
        &directory,
        "parameters.json",
        &job_parameters(invocation, &saved)?,
    )?;
    verify_inputs(host, invocation)?;
    let process = host.processes.run(
        invocation,
        &ProcessOptions {
            cwd: path.clone(),
            argv: vec![
                python.into(),
                "-I".into(),
                "-B".into(),
                path_text(&path.join("worker.py"))?.into(),
            ],
            timeout_ms: 60000,
            output_bytes: 16384,
        },
        stop,
    )?;
    finish(host, invocation, process)
}
pub(super) fn register(registry: &mut Registry, host: Arc<Host>) -> Result<()> {
    let inspect_host = host.clone();
    let inspect_legacy = host.clone();
    let read = |host: &Host, args: &Json, stop: &Cancel| -> Outcome {
        (|| -> Result<_> {
            Ok(match dataset(host, args, stop)? {
                Input::Instructions(value) => value,
                Input::Data(bytes) => result(inspect(&bytes, stop)?),
            })
        })()
        .unwrap_or_else(error)
    };
    registry.register_with_policy(Definition{descriptor:Descriptor{name:"dataset_inspect".into(),description:"Inspect a UTF-8 CSV inside the workspace: exact row count, columns, missing values, bounded sample and exact decimal statistics. Maximum 4 MiB, 64 columns and 100000 rows.".into(),revision:"ansa_csv_v1".into(),schema:schema(&[("path","string")])?},validator:Arc::new(|a|path_argument(a,"path").unwrap_or(false)),executor:Arc::new(move|a|read(&inspect_legacy,a,&Cancel::default()))},
        Policy{read_only:true,invoke:Some(Arc::new(move|i,stop|read(&inspect_host,&i.arguments,stop))),..Policy::default()})?;
    let execute_host = host.clone();
    let recover_host = host;
    let revision = analysis_revision(&sha256(SCRIPT.as_bytes()));
    registry.register_with_policy(Definition{descriptor:Descriptor{name:"python_execute".into(),description:"Run the fixed monthly CSV sum analysis in a pinned supervised Python/Matplotlib job (60s). date_column is YYYY-MM-DD; value_column accepts signed decimals with at most 18 integral and 6 fractional digits. Missing date/value rows are counted and skipped. report_key links immutable revisions. Returns exact totals plus Markdown, CSV and PNG descriptors; arbitrary source and package installation are unavailable.".into(),revision,schema:schema(&[("path","string"),("date_column","string"),("value_column","string"),("report_key","string")])?},
        validator:Arc::new(|a|(||->Result<bool>{Ok(path_argument(a,"path")? && ["date_column","value_column"].iter().all(|k|a.text_field(k).is_ok_and(|s|!s.is_empty() && s.len()<=128 && !s.chars().any(char::is_control))) && crate::provider::contracts::valid_id(a.text_field("report_key")?) && a.text_field("report_key")?.len()<=64)})().unwrap_or(false)),
        executor:Arc::new(|_|Outcome::failed_unknown())},Policy{requires_journal:true,
        invoke:Some(Arc::new(move|i,stop|run(&execute_host,i,stop).unwrap_or_else(|_|Outcome::failed_unknown()))),
        recover:Some(Arc::new(move|i|recover(&recover_host,i))),..Policy::default()})?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_quoted_multiline_crlf_exact_statistics_and_limits() {
        let value=inspect(b"day,value,note\r\n2026-01-02,0.1,\"hello,\r\nworld\"\r\n2026-01-03,0.2,\"\"\r\n2026-02-01,,end\r\n",&Cancel::default()).unwrap();
        assert_eq!(value.usize_field("rows").unwrap(), 3);
        let numeric = &value.field("columns").unwrap().elements().unwrap()[1];
        assert_eq!(numeric.text_field("sum").unwrap(), "0.3");
        assert_eq!(numeric.usize_field("missing").unwrap(), 1);
        for bytes in [
            b"x,x\n1,2".as_slice(),
            b"x,y\n1",
            b"x\n\"unfinished",
            b"x\n\"closed\"bad",
            b"x\r1",
        ] {
            assert!(inspect(bytes, &Cancel::default()).is_err());
        }
        assert_eq!(
            scaled("-999999999999999999.999999"),
            Some(-999999999999999999999999)
        );
        for v in ["NaN", "1e3", "+1", "1.", ".3", "1.1234567"] {
            assert_eq!(scaled(v), None);
        }
        assert!(
            inspect(
                format!("x\n{}", "x".repeat(4097)).as_bytes(),
                &Cancel::default()
            )
            .is_err()
        );
    }
}
