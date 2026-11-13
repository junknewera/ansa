//! Synthetic-only adapter over production config, local tools and process APIs.
#[allow(dead_code)]
#[path = "../tests/support/tools_fixture.rs"]
mod fixture;
use ansa::{
    auth::Cancel,
    config::{agent_epoch, load_agent_settings},
    protocol::json::Json,
    provider::{
        native,
        request::{Binding, Request},
    },
    tools::{
        admission::{OperationIds, PreparedBatch},
        command::{ProcessOptions, ProcessRunner},
        files::local_tools,
        invocation::Invocation,
        registry::Outcome,
    },
};
use std::{
    io::{self, BufRead},
    path::Path,
};
type Result<T> = std::result::Result<T, &'static str>;
fn binding() -> Binding {
    Binding {
        provider: "openai-siwc".into(),
        account: "offline-fixture".into(),
        requested_model: "gpt-6.1-sol".into(),
        returned_model: "gpt-6.1-sol".into(),
    }
}
fn s(v: &str) -> Json {
    Json::string(v).unwrap()
}
fn n(v: usize) -> Json {
    Json::parse(&v.to_string()).unwrap()
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).unwrap()
}
fn outcome(v: Outcome) -> Result<Json> {
    Json::object(vec![
        ("execution", n(v.execution() as usize)),
        ("effects", n(v.effects() as usize)),
        (
            "output",
            s(std::str::from_utf8(v.output()).map_err(|_| "invalid_utf8")?),
        ),
    ])
}
fn run(row: &Json) -> Result<Json> {
    let root = Path::new(row.text_field("root")?);
    ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
    let home = root.join("home");
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).map_err(|_| "file_access_failed")?;
    let op = row.text_field("op")?;
    if op == "case" {
        match row.text_field("suite")? {
            "config" => fixture::configuration(),
            "instruction-links" => fixture::instruction_links(),
            "files" => fixture::files(),
            "raw-names" => fixture::raw_names(),
            "future-auth" => fixture::future_auth_root(),
            "basic" => fixture::basic(),
            "cancel" => fixture::cancellation(false),
            "timeout" => fixture::cancellation(true),
            "detached" => fixture::detached(),
            "output" => fixture::process_output(),
            "privileges" => fixture::privileges(),
            "evidence" => fixture::evidence(),
            "runtime-nonzero" => fixture::runtime_loop(false),
            "runtime-cancel" => fixture::runtime_loop(true),
            _ => return Err("tools_fixture_suite"),
        }
        return Json::object(vec![
            ("suite", row.field("suite")?.clone()),
            ("assertions_completed", b(true)),
        ]);
    }
    if matches!(op, "process" | "recover") {
        if row.has("inherit_signals") && row.field("inherit_signals")?.boolean()? {
            ansa_linux::process::inherited_signal_fault().map_err(|_| "fixture_signal_fault")?;
        }
        let runner = ProcessRunner::new(
            &root.join("jobs"),
            &std::env::current_exe().map_err(|_| "process_executable")?,
        )?;
        let invocation = if row.has("invocation") {
            Invocation::load(row.field("invocation")?)?
        } else {
            Invocation {
                operation: "operation".into(),
                call: "call".into(),
                name: "execute".into(),
                revision: "1".into(),
                arguments: Json::parse("{}")?,
            }
        };
        if op == "recover" {
            return runner
                .recover(&invocation)?
                .map(outcome)
                .transpose()
                .map(|v| v.unwrap_or_else(|| Json::parse("null").unwrap()));
        }
        let options = ProcessOptions {
            cwd: if row.has("cwd") {
                row.text_field("cwd")?.into()
            } else {
                cwd
            },
            argv: row
                .field("argv")?
                .elements()?
                .iter()
                .map(|v| v.text().map(String::from))
                .collect::<Result<_>>()?,
            timeout_ms: if row.has("timeout_ms") {
                row.usize_field("timeout_ms")?
            } else {
                3000
            },
            output_bytes: if row.has("output_bytes") {
                row.usize_field("output_bytes")?
            } else {
                16384
            },
        };
        return outcome(runner.run(&invocation, &options, &Cancel::default())?);
    }
    let settings = load_agent_settings(&cwd, &home)?;
    let epoch = agent_epoch(&settings)?;
    let tools = local_tools(
        settings.clone(),
        epoch.clone(),
        &root.join("session"),
        &root.join("auth"),
        &std::env::current_exe().map_err(|_| "process_executable")?,
    )?;
    if op == "config" {
        return Json::object(vec![
            ("settings", settings.effective()?),
            (
                "epoch",
                Json::object(vec![
                    ("id", s(&epoch.id)),
                    ("instructions", s(&epoch.instructions)),
                    ("exact_state", epoch.exact_state.clone()),
                ])?,
            ),
            (
                "tools",
                Request::new(
                    binding(),
                    epoch.clone(),
                    tools.advertisement().clone(),
                    vec![],
                )?
                .save()?
                .field("tools")?
                .clone(),
            ),
            (
                "readonly",
                Request::new(
                    binding(),
                    epoch.clone(),
                    tools.read_only().advertisement().clone(),
                    vec![],
                )?
                .save()?
                .field("tools")?
                .clone(),
            ),
        ]);
    }
    if op != "local" {
        return Err("tools_adapter_operation");
    }
    let mut args = row.field("arguments")?.clone();
    if row.has("initial_token") && row.field("initial_token")?.boolean()? {
        let token = ansa::protocol::legacy_json_v1::sha256(
            epoch.exact_state.field("instructions")?.dump().as_bytes(),
        );
        let mut fields = vec![];
        for key in args.keys()? {
            if key != "instruction_token" {
                fields.push((key, args.field(key)?.clone()));
            }
        }
        fields.push(("instruction_token", s(&token)));
        args = Json::object(fields)?;
    }
    let request = Request::new(
        binding(),
        epoch.clone(),
        tools.advertisement().clone(),
        vec![],
    )?;
    let response = native::load(&Json::object(vec![
        ("request", request.save()?),
        ("id", s("response")),
        ("model", s(&request.binding.returned_model)),
        (
            "items",
            Json::array(vec![Json::object(vec![
                ("type", s("function_call")),
                ("id", s("item")),
                ("call_id", s("call")),
                ("name", row.field("name")?.clone()),
                ("namespace", s("omh")),
                ("arguments", s(&args.dump())),
                ("status", s("completed")),
            ])?])?,
        ),
    ])?)?;
    let mut ids = OperationIds::new("fixture", 0)?;
    let result = PreparedBatch::prepare(&response, tools, &mut ids)?.execute(false);
    if result.results().len() != 1 {
        return Err("tools_result_count");
    }
    let value = &result.results()[0];
    Json::object(vec![
        ("execution", n(value.execution() as usize)),
        ("effects", n(value.effects() as usize)),
        ("output", s(value.output())),
    ])
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if let Some(result) = ansa::process::runner::private_entry(&args) {
        std::process::exit(if result.is_ok() { 0 } else { 125 });
    }
    if let Some(result) = ansa::process::supervisor::private_entry(&args) {
        std::process::exit(if result.is_ok() { 0 } else { 125 });
    }
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        if line.len() > 128 * 1024 {
            std::process::exit(2);
        }
        let value = match Json::parse(&line).and_then(|row| run(&row)) {
            Ok(value) => Json::object(vec![("accepted", b(true)), ("value", value)]).unwrap(),
            Err(code) => {
                Json::object(vec![("accepted", b(false)), ("diagnostic", s(code))]).unwrap()
            }
        };
        println!("{}", value.dump());
    }
}
