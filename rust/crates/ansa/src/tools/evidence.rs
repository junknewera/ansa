//! Legacy process records share the v1 JSON codec and publish-once file helper.
use crate::{
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::replay::{Effects, Execution},
    tools::{invocation::Invocation, registry::Outcome},
};
use std::fs::File;
pub type Result<T> = std::result::Result<T, &'static str>;
pub(crate) fn read(directory: &File, name: &str) -> Result<Option<Json>> {
    ansa_linux::local::read(directory, name, 128 * 1024)
        .map_err(|_| "local_file_permissions")?
        .map(|bytes| {
            Json::parse_with_limits(
                std::str::from_utf8(&bytes).map_err(|_| "invalid_utf8")?,
                128 * 1024,
                64,
                131072,
            )
        })
        .transpose()
}
pub(crate) fn create(directory: &File, name: &str, value: &Json) -> Result<()> {
    ansa_linux::local::create(directory, name, value.dump().as_bytes())
        .map_err(|_| "local_file_publish")
}
pub(crate) fn unknown() -> Outcome {
    Outcome::from_bytes(
        Execution::Failed,
        Effects::Unknown,
        br#"{"error":"process_result_unknown"}"#.to_vec(),
    )
}
pub(crate) fn admit(directory: &File, invocation: &Invocation) -> Result<Outcome> {
    let spec = read(directory, "request.json")?.ok_or("process_evidence_identity")?;
    let started = read(directory, "started.json")?.ok_or("process_evidence_identity")?;
    let result = read(directory, "result.json")?.ok_or("process_evidence_identity")?;
    let hash = sha256(spec.dump().as_bytes());
    if spec.usize_field("version")? != 1
        || started.usize_field("version")? != 1
        || result.usize_field("version")? != 1
        || started.text_field("request_hash")? != hash
        || result.text_field("request_hash")? != hash
        || *spec.field("invocation")? != invocation.save()?
    {
        return Err("process_evidence_identity");
    }
    let execution = match result.usize_field("execution")? {
        0 => Execution::Succeeded,
        1 => Execution::Failed,
        2 => Execution::Cancelled,
        3 => Execution::Denied,
        _ => return Err("process_evidence_outcome"),
    };
    let effects = match result.usize_field("effects")? {
        1 => Effects::KnownResult,
        2 => Effects::Unknown,
        _ => return Err("process_evidence_outcome"),
    };
    let output = result.text_field("output")?;
    if output.len() > 128 * 1024 {
        return Err("process_evidence_limit");
    }
    Ok(Outcome::from_bytes(
        execution,
        effects,
        output.as_bytes().to_vec(),
    ))
}
pub(crate) fn publish(directory: &File, spec: &Json, outcome: &Outcome) -> Result<()> {
    create(
        directory,
        "result.json",
        &Json::object(vec![
            ("version", crate::config::n(1)),
            (
                "request_hash",
                Json::string(&sha256(spec.dump().as_bytes()))?,
            ),
            ("execution", crate::config::n(outcome.execution as usize)),
            ("effects", crate::config::n(outcome.effects as usize)),
            (
                "output",
                Json::string(std::str::from_utf8(&outcome.output).map_err(|_| "invalid_utf8")?)?,
            ),
        ])?,
    )
}
