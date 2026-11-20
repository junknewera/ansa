//! Synthetic-only JSONL adapter over production session/journal APIs.
use ansa::{
    context::{
        epoch::compact_epoch,
        legacy_compact::{CompactedContext, summary_request},
    },
    protocol::json::Json,
    provider::{
        contracts::{AgentMode, GetTranscript},
        request::{Advertisement, Request},
    },
    session::{
        self,
        codec_v1::{self, SavedResult},
        journal_v1::{Journal, Observer, Point},
        state::{Receipt, State},
        store::Store,
    },
};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::Arc,
};
type Result<T> = session::Result<T>;
fn s(value: &str) -> Result<Json> {
    Json::string(value)
}
fn n(value: usize) -> Result<Json> {
    Json::parse(&value.to_string())
}
fn b(value: bool) -> Result<Json> {
    Json::parse(if value { "true" } else { "false" })
}
fn null() -> Result<Json> {
    Json::parse("null")
}
fn mode(value: AgentMode) -> &'static str {
    if value == AgentMode::Plan {
        "plan"
    } else {
        "basic"
    }
}
fn receipt(r: &Receipt) -> Result<Json> {
    Json::object(vec![
        ("command", s(&r.command.0)?),
        ("turn", s(&r.turn.0)?),
        ("request", s(&r.request.0)?),
        ("text", s(&r.text)?),
        ("state", s(&r.state)?),
        ("mode", s(mode(r.mode))?),
    ])
}
fn snapshot(state: &State) -> Result<Json> {
    let submits = state
        .submits
        .values()
        .map(receipt)
        .collect::<Result<Vec<_>>>()?;
    let turn = if let Some(t) = &state.turn {
        Json::object(vec![
            ("receipt", receipt(&t.receipt)?),
            ("run", s(&t.operation_run)?),
            ("next", t.next.save()?),
            (
                "response",
                t.response
                    .as_ref()
                    .map(|r| codec_v1::save_response(r))
                    .transpose()?
                    .unwrap_or(null()?),
            ),
            (
                "tools_response",
                t.tools_response
                    .as_ref()
                    .map(|r| codec_v1::save_response(r))
                    .transpose()?
                    .unwrap_or(null()?),
            ),
            (
                "results",
                Json::array(
                    t.results
                        .iter()
                        .map(SavedResult::save)
                        .collect::<Result<_>>()?,
                )?,
            ),
            (
                "previous_results",
                Json::array(
                    t.previous_results
                        .iter()
                        .map(SavedResult::save)
                        .collect::<Result<_>>()?,
                )?,
            ),
            (
                "intent",
                t.intent
                    .as_ref()
                    .map(|i| i.save())
                    .transpose()?
                    .unwrap_or(null()?),
            ),
            ("stage", s(&t.stage)?),
            ("diagnostic", s(&t.diagnostic)?),
            ("paused", b(t.paused)?),
            (
                "resumes",
                Json::array(t.resumes.iter().map(|id| s(id)).collect::<Result<_>>()?)?,
            ),
            ("rounds", n(t.rounds)?),
            ("operation_offset", n(t.operation_offset)?),
            ("round_limit", n(t.round_limit)?),
            ("call_limit", n(t.call_limit)?),
            ("resume_round", n(t.resume_round)?),
            ("resume_operation", n(t.resume_operation)?),
        ])?
    } else {
        null()?
    };
    let context = Request::new(
        state.configuration.binding.clone(),
        state.epoch.clone(),
        state.configuration.tools.clone(),
        state.history.clone(),
    )?;
    Json::object(vec![
        ("session", s(&state.session.0)?),
        ("initialized", b(state.initialized)?),
        ("configuration", state.configuration.save()?),
        ("context", context.save()?),
        ("history", Json::array(state.history.clone())?),
        ("history_bytes", n(state.history_bytes)?),
        ("next_request", n(state.next_request)?),
        ("compactions", n(state.compactions)?),
        ("submits", Json::array(submits)?),
        ("turn", turn),
        (
            "unknown",
            state
                .unknown_operation()
                .map(s)
                .transpose()?
                .unwrap_or(null()?),
        ),
    ])
}
fn state_for(row: &Json) -> Result<State> {
    Ok(State::new(
        row.text_field("session")?.into(),
        Request::load(row.field("configuration")?)?,
        if row.has("plan_tools") {
            Request::load(row.field("plan_tools")?)?.tools
        } else {
            Advertisement {
                revision: String::new(),
                descriptors: vec![],
            }
        },
    ))
}
fn observer(row: &Json) -> Result<Option<Observer>> {
    if !row.has("fault") {
        return Ok(None);
    }
    let f = row.field("fault")?;
    let kind = f.text_field("kind")?.to_owned();
    let point = f.text_field("point")?.to_owned();
    let action = f.text_field("action")?.to_owned();
    Ok(Some(Arc::new(move |p, k| {
        let p = match p {
            Point::BeforeWrite => "before_write",
            Point::AfterWrite => "after_write",
            Point::BeforeSync => "before_sync",
            Point::AfterSync => "after_sync",
        };
        if k == kind && p == point {
            if action == "exit" {
                std::process::exit(71);
            }
            return Err("journal_injected_io");
        }
        Ok(())
    })))
}
fn run(row: &Json) -> Result<Json> {
    match row.text_field("op")? {
        "request" => Request::load(row.field("value")?)?.save(),
        "response" => codec_v1::save_response(&codec_v1::load_response(row.field("value")?)?),
        "result" => SavedResult::load(row.field("value")?)?.save(),
        "summary_request" => summary_request(&Request::load(row.field("value")?)?)?.save(),
        "compact" => {
            let r = Request::load(row.field("request")?)?;
            let c = CompactedContext::admit(r.clone(), row.field("native")?.clone())?;
            let e = compact_epoch(
                &r.epoch,
                if row.has("generation") {
                    row.usize_field("generation")?
                } else {
                    1
                },
                &c,
            )?;
            Json::object(vec![
                ("native", c.native().clone()),
                (
                    "epoch",
                    Request::new(r.binding, e, r.tools, c.output()?.to_vec())?
                        .save()?
                        .field("epoch")?
                        .clone(),
                ),
            ])
        }
        "summary" => Ok(CompactedContext::from_summary(
            Request::load(row.field("request")?)?,
            &codec_v1::load_response(row.field("response")?)?,
        )?
        .native()
        .clone()),
        "state" => {
            let mut state = state_for(row)?;
            let mut steps = vec![];
            for event in row.field("events")?.elements()? {
                let result = state.apply(event);
                let mut fields = vec![
                    ("accepted", b(result.is_ok())?),
                    ("state", snapshot(&state)?),
                ];
                if let Err(code) = result {
                    fields.push(("diagnostic", s(code)?));
                }
                steps.push(Json::object(fields)?);
            }
            let mut pages = vec![];
            if row.has("queries") {
                for q in row.field("queries")?.elements()? {
                    let page = state.transcript().page(GetTranscript {
                        before: q.usize_field("before")?,
                        limit: q.usize_field("limit")?,
                        forward: q.has("forward") && q.field("forward")?.boolean()?,
                    });
                    let entries = page
                        .entries
                        .iter()
                        .map(|e| {
                            Json::object(vec![
                                ("turn", s(&e.turn.0)?),
                                ("request", s(&e.request.0)?),
                                ("role", s(&e.role)?),
                                ("text", s(&e.text)?),
                                ("state", s(&e.state)?),
                                ("mode", s(mode(e.mode))?),
                            ])
                        })
                        .collect::<Result<_>>()?;
                    pages.push(Json::object(vec![
                        ("begin", n(page.begin)?),
                        ("end", n(page.end)?),
                        ("total", n(page.total)?),
                        ("entries", Json::array(entries)?),
                    ])?);
                }
            }
            Json::object(vec![
                ("steps", Json::array(steps)?),
                ("pages", Json::array(pages)?),
                ("state", snapshot(&state)?),
            ])
        }
        "catalog" => {
            let root = Path::new(row.text_field("root")?);
            ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
            let catalog = session::catalog::Catalog::open(
                &root.join("sessions"),
                &root.join("workspace"),
                "offline",
            )?;
            let explicit = if row.has("id") {
                row.text_field("id")?
            } else {
                ""
            };
            let resume = row.has("resume") && row.field("resume")?.boolean()?;
            if resume && !explicit.is_empty() {
                return Err("session_selection_conflict");
            }
            let selected = if !explicit.is_empty() {
                catalog.explicit(explicit)?
            } else if resume {
                catalog.latest()?
            } else {
                catalog.create()?
            };
            Json::object(vec![
                ("id", s(&selected.id)?),
                (
                    "directory",
                    s(selected.directory.to_str().ok_or("session_directory")?)?,
                ),
            ])
        }
        op @ ("journal" | "store" | "hold") => {
            let root = Path::new(row.text_field("root")?);
            ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
            let path = root.join("journal");
            let mut events = vec![];
            let mut appends = vec![];
            let (diagnostic, empty, state) = if op == "store" {
                let mut store =
                    Store::open_with_replay(&path, state_for(row)?, observer(row)?, |e| {
                        events.push(e.clone());
                        Ok(())
                    })?;
                if row.has("events") {
                    for event in row.field("events")?.elements()? {
                        appends.push(outcome(store.commit(event))?);
                    }
                }
                (store.diagnostic(), store.empty(), snapshot(store.state())?)
            } else {
                let mut journal = Journal::open(
                    &path,
                    |e| {
                        events.push(e.clone());
                        Ok(())
                    },
                    observer(row)?,
                )?;
                if op == "hold" {
                    println!("{{\"ready\":true}}");
                    io::stdout().flush().map_err(|_| "adapter_output")?;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                if row.has("events") {
                    for event in row.field("events")?.elements()? {
                        appends.push(outcome(journal.append(event))?);
                    }
                }
                (journal.diagnostic(), journal.empty(), null()?)
            };
            Json::object(vec![
                ("events", Json::array(events)?),
                ("appends", Json::array(appends)?),
                ("diagnostic", s(diagnostic)?),
                ("empty", b(empty)?),
                ("state", state),
            ])
        }
        _ => Err("session_adapter_operation"),
    }
}
fn outcome(result: Result<()>) -> Result<Json> {
    let mut fields = vec![("accepted", b(result.is_ok())?)];
    if let Err(code) = result {
        fields.push(("diagnostic", s(code)?));
    }
    Json::object(fields)
}
fn main() {
    let mut line = Vec::new();
    let mut stdout = io::stdout().lock();
    for byte in io::stdin().lock().bytes() {
        let Ok(byte) = byte else {
            std::process::exit(2)
        };
        if byte != b'\n' {
            if line.len() == 4 * 1024 * 1024 {
                std::process::exit(2);
            }
            line.push(byte);
            continue;
        }
        let result = std::str::from_utf8(&line)
            .map_err(|_| "json_invalid")
            .and_then(Json::parse)
            .and_then(|row| run(&row));
        let value = match result {
            Ok(value) => Json::object(vec![("accepted", b(true).unwrap()), ("value", value)]),
            Err(code) => Json::object(vec![
                ("accepted", b(false).unwrap()),
                ("diagnostic", s(code).unwrap()),
            ]),
        }
        .unwrap();
        if writeln!(stdout, "{}", value.dump()).is_err() {
            std::process::exit(2);
        }
        line.clear();
    }
    if !line.is_empty() {
        std::process::exit(2);
    }
}
