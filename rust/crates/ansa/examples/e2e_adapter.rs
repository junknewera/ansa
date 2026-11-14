//! Owned synthetic composition: config -> native admission -> local tools -> journal -> v1 events.
use ansa::{
    config::{agent_epoch, load_agent_settings},
    frontend::{app, jsonl, offline},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{native, request::Request},
    runtime::*,
    tools::files::local_tools,
};
use std::{
    io::{self, BufRead},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
type Result<T> = std::result::Result<T, &'static str>;
fn s(v: &str) -> Json {
    Json::string(v).unwrap()
}
fn n(v: usize) -> Json {
    Json::parse(&v.to_string()).unwrap()
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).unwrap()
}
fn o(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).unwrap()
}
fn option(row: &Json, key: &str) -> bool {
    row.has(key) && row.field(key).unwrap().boolean().unwrap()
}
async fn run(row: &Json) -> Result<Json> {
    let root = Path::new(row.text_field("root")?);
    ansa_linux::journal::fixture_workspace(root).map_err(|_| "fixture_owner")?;
    let home = root.join("home");
    let cwd = home.join("workspace");
    let settings = load_agent_settings(&cwd, &home)?;
    let epoch = agent_epoch(&settings)?;
    let token = sha256(epoch.exact_state.field("instructions")?.dump().as_bytes());
    let tools = local_tools(
        settings,
        epoch.clone(),
        &root.join("journal"),
        &root.join("auth"),
        &std::env::current_exe().map_err(|_| "fixture_executable")?,
    )?;
    let configuration = Request::new(app::binding(), epoch, tools.advertisement().clone(), vec![])?;
    let quiet = option(row, "quiet");
    let plan = row.text_field("mode")? == "plan";
    let turns = row.usize_field("turns")?;
    let start = row.usize_field("start")?;
    if turns > (if quiet { 4096 } else { 8 }) || start > 4096 - turns {
        return Err("fixture_turn_limit");
    }
    let requests = Arc::new(Mutex::new(vec![]));
    let saved = requests.clone();
    let generations = Arc::new(AtomicUsize::new(0));
    let gen_count = generations.clone();
    let generate: Generate = Arc::new(move |request, stop, preview| {
        if stop.cancelled() {
            return Err("fixture_cancelled");
        }
        gen_count.fetch_add(1, Ordering::SeqCst);
        if !quiet {
            saved.lock().unwrap().push(request.save()?);
        }
        let first = request
            .input
            .iter()
            .rposition(|v| v.has("role") && v.text_field("role") == Ok("user"))
            .ok_or("fixture_user")?;
        let label = request.input[first].text_field("content")?;
        let outputs: Vec<_> = request.input[first + 1..]
            .iter()
            .filter(|v| v.has("type") && v.text_field("type") == Ok("function_call_output"))
            .collect();
        let index = outputs.len();
        let final_index = if plan { 1 } else { 3 };
        let item = if index == final_index {
            if !plan {
                let result = Json::parse(outputs[1].text_field("output")?)?;
                if result.usize_field("exit_code")? != 7 {
                    return Err("fixture_known_failure_lost");
                }
            }
            preview("E2E_PREVIEW")?;
            o(vec![
                ("type", s("message")),
                ("id", s(&format!("message_{label}"))),
                ("role", s("assistant")),
                ("status", s("completed")),
                (
                    "content",
                    Json::array(vec![o(vec![
                        ("type", s("output_text")),
                        ("text", s(if plan { "PLAN_OK" } else { "E2E_OK:exit7" })),
                    ])])?,
                ),
            ])
        } else {
            let (name, args) = if index == 1 {
                (
                    "run_command",
                    o(vec![
                        (
                            "argv",
                            Json::array(vec![
                                s("sh"),
                                s("-c"),
                                s("printf x >> marker; printf RUN_OK; exit 7"),
                            ])?,
                        ),
                        ("cwd", s(cwd.to_str().ok_or("fixture_utf8")?)),
                        ("timeout_ms", n(3000)),
                        ("instruction_token", s(&token)),
                    ]),
                )
            } else {
                (
                    "read_file",
                    o(vec![
                        ("path", s(if index == 0 { "value.txt" } else { "marker" })),
                        ("offset", n(0)),
                        ("max_bytes", n(8192)),
                        ("instruction_token", s(&token)),
                    ]),
                )
            };
            o(vec![
                ("type", s("function_call")),
                ("id", s(&format!("item_{label}_{index}"))),
                ("call_id", s(&format!("call_{label}_{index}"))),
                ("name", s(name)),
                ("namespace", s("omh")),
                ("arguments", s(&args.dump())),
                ("status", s("completed")),
            ])
        };
        native::load(&o(vec![
            ("request", request.save()?),
            ("id", s(&format!("response_{label}_{index}"))),
            ("model", s(&request.binding.returned_model)),
            ("items", Json::array(vec![item])?),
        ]))
    });
    let compactions = Arc::new(AtomicUsize::new(0));
    let compact_count = compactions.clone();
    let compact: Compact = Arc::new(move |request, stop| {
        compact_count.fetch_add(1, Ordering::SeqCst);
        offline::compact(request, stop)
    });
    let rounds = if option(row, "limited") { 1 } else { 8 };
    let journal = if row.has("fault") {
        use ansa::session::journal_v1::{Observer, Point};
        let fault = row.field("fault")?;
        let kind = fault.text_field("kind")?.to_owned();
        let wanted = fault.usize_field("occurrence")?;
        let hits = AtomicUsize::new(0);
        Some(Arc::new(move |point, k: &str| {
            if point == Point::AfterSync
                && k == kind
                && hits.fetch_add(1, Ordering::SeqCst) + 1 == wanted
            {
                std::process::exit(71);
            }
            Ok(())
        }) as Observer)
    } else {
        None
    };
    let mut runtime = Runtime::new(
        "e2e".into(),
        configuration,
        tools,
        generate,
        Limits {
            tool_rounds: rounds,
            compact_bytes: if option(row, "compact") {
                256
            } else {
                256 * 1024
            },
            ..Limits::default()
        },
        Options {
            directory: Some(root.join("journal")),
            compact: Some(compact),
            keep_recent_turns: 2,
            streaming: true,
            journal,
            ..Options::default()
        },
    )?;
    runtime.ready().await?;
    let handle = runtime.handle();
    let mut events = vec![];
    let mut last = Json::parse("null")?;
    let mut resume_count = 0;
    let command = |id: String, turn: String, payload| RuntimeCommand {
        schema_version: 1,
        session: "e2e".into(),
        turn: TurnId(turn),
        incarnation: handle.incarnation().clone(),
        command: CommandId(id),
        payload,
    };
    for index in start..start + turns {
        let payload = if option(row, "resume") {
            loop {
                let e = runtime.next_event().await.ok_or("fixture_snapshot")?;
                if let EventPayload::FrontendSnapshot(view) = &e.payload {
                    let id = view.request.clone().ok_or("fixture_resume")?;
                    last = jsonl::encode(&e);
                    if !quiet {
                        events.push(last.clone());
                    }
                    break CommandPayload::Resume(id);
                }
                if !quiet {
                    events.push(jsonl::encode(&e));
                }
            }
        } else {
            CommandPayload::Submit(Submit {
                text: format!("fixture{index}"),
                mode: if plan {
                    AgentMode::Plan
                } else {
                    AgentMode::Basic
                },
            })
        };
        if handle
            .post(command(
                format!(
                    "{}{index}",
                    if option(row, "resume") {
                        "restart"
                    } else {
                        "command"
                    }
                ),
                format!("turn{index}"),
                payload,
            ))
            .await?
            != PostStatus::Accepted
        {
            return Err("fixture_post");
        }
        loop {
            let event = runtime.next_event().await.ok_or("fixture_done")?;
            let encoded = jsonl::encode(&event);
            if !quiet {
                events.push(encoded.clone());
            }
            if let EventPayload::RuntimeError { diagnostic, .. } = &event.payload {
                if diagnostic != "tool_round_limit" {
                    return Err("fixture_runtime_error");
                }
            }
            if let EventPayload::FrontendSnapshot(view) = &event.payload {
                last = encoded;
                if view.resume_available && view.state == "paused" {
                    if option(row, "pause") {
                        break;
                    }
                    resume_count += 1;
                    if resume_count > 8 * turns {
                        return Err("fixture_resume_limit");
                    }
                    if handle
                        .post(command(
                            format!("resume{index}_{resume_count}"),
                            format!("turn{index}"),
                            CommandPayload::Resume(view.request.clone().ok_or("fixture_resume")?),
                        ))
                        .await?
                        != PostStatus::Accepted
                    {
                        return Err("fixture_resume_post");
                    }
                }
            }
            if matches!(event.payload, EventPayload::Done) {
                break;
            }
        }
    }
    runtime.close_input();
    while let Some(event) = runtime.next_event().await {
        if matches!(event.payload, EventPayload::FrontendSnapshot(_)) {
            last = jsonl::encode(&event);
        }
        if !quiet {
            events.push(jsonl::encode(&event));
        }
    }
    runtime.join().await?;
    Ok(o(vec![
        ("events", Json::array(events)?),
        ("requests", Json::array(requests.lock().unwrap().clone())?),
        ("snapshot", last),
        ("generation_count", n(generations.load(Ordering::SeqCst))),
        ("summary_count", n(compactions.load(Ordering::SeqCst))),
        ("resume_count", n(resume_count)),
        ("plan", b(plan)),
    ]))
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
        let result = line
            .map_err(|_| "fixture_input")
            .and_then(|line| Json::parse(&line))
            .and_then(|row| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|_| "fixture_runtime")?
                    .block_on(run(&row))
            });
        match result {
            Ok(value) => println!(
                "{}",
                o(vec![("accepted", b(true)), ("value", value)]).dump()
            ),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1)
            }
        }
    }
}
