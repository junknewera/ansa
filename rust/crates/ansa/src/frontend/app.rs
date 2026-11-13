//! Executable composition. Persistent/runtime/provider ownership stays in M3-M6.
use super::{
    Result,
    args::{HELP, Options},
    controller, offline,
    protocol_v1::*,
    terminal::Io,
};
use crate::{
    auth::{
        Cancel,
        dispatch::Authorization,
        manager::{AuthManager, Clock, Exchange},
    },
    config::{agent_epoch, load_agent_settings},
    context::legacy_compact::{CompactedContext, summary_request},
    protocol::json::Json,
    provider::request::{Binding, Epoch, Request},
    runtime::{Generate, Lease, Limits, Options as RuntimeOptions, Runtime},
    tools::{
        files::local_tools,
        registry::{Definition, Outcome, Policy, Registry},
    },
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn s(v: &str) -> Json {
    Json::string(v).expect("UTF-8")
}
fn b(v: bool) -> Json {
    Json::parse(if v { "true" } else { "false" }).expect("boolean")
}
fn object(v: Vec<(&str, Json)>) -> Json {
    Json::object(v).expect("static fields")
}
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or("home_directory_unavailable")
}
/// AgentAuthorization's local admission error is protocol/frontend; the same
/// AuthManager code at legacy auth entry points is validation/frontend.
pub fn startup_diagnostic(args: &[String], code: &str) -> Json {
    if code == "live_cli_unknown_option" {
        return object(vec![
            ("schema_version", Json::parse("1").unwrap()),
            ("code", Json::parse("7").unwrap()),
            ("stage", Json::parse("7").unwrap()),
            ("diagnostic", s("live_cli_options")),
        ]);
    }
    let diagnostic = super::jsonl::diagnostic(code);
    let auth_entry = args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "--login"
                | "--login-consent"
                | "--logout"
                | "--auth-status"
                | "--refresh-auth"
                | "--models"
                | "--live-jsonl"
        )
    });
    if code == "auth_required" && !auth_entry {
        diagnostic
            .without("code")
            .and_then(|d| d.with("code", Json::parse("0")?))
            .expect("diagnostic field")
    } else {
        diagnostic
    }
}
pub fn auth_path_with(home: Option<&Path>, xdg: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let home = home
        .filter(|v| !v.as_os_str().is_empty())
        .ok_or("auth_home_directory")?;
    let legacy = if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        PathBuf::from(xdg).join("oh-my-harness")
    } else {
        home.join(".local/state/oh-my-harness")
    };
    if legacy.exists() {
        return Ok(legacy);
    }
    Ok(home.join(".ansa/auth"))
}
fn auth_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let xdg = std::env::var_os("XDG_STATE_HOME");
    auth_path_with(home.as_deref(), xdg.as_deref())
}
pub fn write_json(value: &Json) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all((value.dump() + "\n").as_bytes())
        .map_err(|_| "cli_output")
}
pub fn auth_manager(path: &Path) -> Result<Arc<AuthManager>> {
    let exchange: Exchange = Arc::new(crate::transport::client::exchange);
    let clock: Clock = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |v| v.as_secs())
    });
    Ok(Arc::new(AuthManager::new(
        path,
        exchange,
        clock,
        None,
        Some(Arc::new(crate::transport::client::stream)),
    )?))
}
pub fn pure_tools() -> Result<Arc<crate::tools::registry::Snapshot>> {
    let mut registry = Registry::new();
    registry.register_with_policy(Definition {descriptor:crate::provider::request::Descriptor {name:"get_test_value".into(),description:"Return a fixed test JSON value without side effects".into(),revision:"1".into(),schema:Json::parse(r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#)?},validator:Arc::new(|_|true),executor:Arc::new(|_|Outcome::succeeded(r#"{"ok":true,"value":"OMH_FIXED_VALUE_42"}"#.into()))},Policy {read_only:true,..Policy::default()})?;
    Ok(registry.snapshot())
}
pub fn binding() -> Binding {
    Binding {
        provider: "openai-siwc".into(),
        account: "offline-fixture".into(),
        requested_model: "gpt-6.1-sol".into(),
        returned_model: "gpt-6.1-sol".into(),
    }
}
fn reactor() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| "cli_runtime")
}
pub fn offline_runtime(directory: Option<PathBuf>, delayed: bool) -> Result<Runtime> {
    let directory = directory.filter(|p| !p.as_os_str().is_empty());
    let tools = pure_tools()?;
    let configuration = Request::new(
        binding(),
        Epoch {
            id: "offline_epoch".into(),
            instructions: "Use the pure fixture tool".into(),
            exact_state: Json::parse(r#"{"mode":"offline"}"#)?,
        },
        tools.advertisement().clone(),
        vec![],
    )?;
    let generate: Generate = Arc::new(move |request, stop, _| {
        if delayed {
            let until = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < until && !stop.cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        offline::generate(request, stop)
    });
    Runtime::new(
        "offline".into(),
        configuration,
        tools,
        generate,
        Limits::default(),
        RuntimeOptions {
            directory,
            compact: Some(Arc::new(offline::compact)),
            keep_recent_turns: 2,
            ..RuntimeOptions::default()
        },
    )
}
fn jsonl_runtime(mut io: Io, mut runtime: Runtime) -> Result<i32> {
    reactor()?.block_on(controller::run(&mut runtime, &mut io, None, true, ""))
}
fn demo() -> Result<i32> {
    let tools = pure_tools()?;
    let configuration = Request::new(
        binding(),
        Epoch {
            id: "demo_epoch".into(),
            instructions: "Use the pure fixture tool".into(),
            exact_state: Json::parse("{}")?,
        },
        tools.advertisement().clone(),
        vec![],
    )?;
    let mut runtime = Runtime::new(
        "offline".into(),
        configuration,
        tools,
        Arc::new(|r, c, _| offline::generate(r, c)),
        Limits::default(),
        RuntimeOptions::default(),
    )?;
    let valid = reactor()?.block_on(async {
        runtime.ready().await?;
        runtime
            .post(RuntimeCommand {
                schema_version: 1,
                session: "offline".into(),
                turn: "demo".into(),
                incarnation: runtime.handle().incarnation().clone(),
                command: "demo_submit".into(),
                payload: CommandPayload::Submit(Submit {
                    text: "Call omh.get_test_value".into(),
                    mode: AgentMode::Basic,
                }),
            })
            .await?;
        runtime.close_input();
        let mut final_view = None;
        while let Some(event) = runtime.next_event().await {
            if let EventPayload::FrontendSnapshot(v) = event.payload {
                final_view = Some(v);
            }
        }
        runtime.join().await?;
        Ok::<_, &'static str>(final_view.is_some_and(|v| {
            v.state == "completed"
                && v.completed_tools == 1
                && v.text == "OMH_TOOL_OK:OMH_FIXED_VALUE_42"
        }))
    })?;
    if !valid {
        return Err("demo_roundtrip");
    }
    write_json(&Json::parse(
        r#"{"status":"passed","mode":"offline","tool_invocations":1,"native_replay":true,"final_text":"OMH_TOOL_OK:OMH_FIXED_VALUE_42"}"#,
    )?)?;
    Ok(0)
}
/// Auth callbacks are lazy. Only this narrow authorization record is shared;
/// the auth worker owns checks and leases, never the frontend/control owner.
pub fn authorized(
    auth: Arc<AuthManager>,
    model: String,
) -> Result<(String, Generate, RuntimeOptions)> {
    let guard = Authorization::new(auth.clone(), &auth.status()?, model)?;
    let account = guard.account().to_owned();
    let guard = Arc::new(Mutex::new(guard));
    let check = guard.clone();
    let provider = guard.clone();
    let lease = guard;
    let generate_auth = auth.clone();
    let lease_auth = auth;
    let generate: Generate = Arc::new(move |request, cancel, preview| {
        let session = provider
            .lock()
            .map_err(|_| "auth_state")?
            .session()
            .to_owned();
        generate_auth.generate(
            request.clone(),
            cancel,
            &session,
            Some(Box::new(move |text| {
                let _ = preview(text);
            })),
        )
    });
    let compact_generate = generate.clone();
    let options = RuntimeOptions {
        check_dispatch: Arc::new(move |epoch, cancel| {
            check
                .lock()
                .map_err(|_| "auth_state")?
                .check_dispatch(epoch, cancel)
        }),
        tool_guard: Arc::new(move |cancel| {
            let session = lease.lock().map_err(|_| "auth_state")?.session().to_owned();
            let owned = lease_auth.hold_execution(&session, cancel)?;
            let stop = owned.cancel().clone();
            Ok(Lease {
                ownership: Box::new(owned),
                stop,
            })
        }),
        compact: Some(Arc::new(move |request, cancel| {
            let response =
                compact_generate(&summary_request(request)?, cancel, Arc::new(|_| Ok(())))?;
            CompactedContext::from_summary(request.clone(), &response)
        })),
        keep_recent_turns: 2,
        streaming: true,
        ..RuntimeOptions::default()
    };
    Ok((account, generate, options))
}
fn text(args: &[String]) -> Result<i32> {
    let parsed = Options::parse(
        args,
        std::env::current_dir().map_err(|_| "file_access_failed")?,
        auth_path()?,
    )?;
    let mut settings = load_agent_settings(&parsed.cwd, &home()?)?;
    if let Some(model) = parsed.model {
        settings.model = model.clone();
        settings.returned_model = model;
    }
    if let Some(model) = parsed.returned_model {
        settings.returned_model = model;
    }
    if parsed.print_config {
        write_json(&settings.effective()?)?;
        return Ok(0);
    }
    let mut bound = binding();
    bound.requested_model = settings.model.clone();
    bound.returned_model = settings.returned_model.clone();
    let (generate, mut options) = if parsed.offline {
        (
            Arc::new(|request: &Request, stop: &Cancel, _| offline::generate(request, stop))
                as Generate,
            RuntimeOptions {
                compact: Some(Arc::new(offline::compact)),
                keep_recent_turns: 2,
                ..RuntimeOptions::default()
            },
        )
    } else {
        let (account, generate, options) =
            authorized(auth_manager(&parsed.auth)?, settings.model.clone())?;
        bound.account = account;
        (generate, options)
    };
    let epoch = agent_epoch(&settings)?;
    if parsed.resume && !parsed.session.is_empty() {
        return Err("session_selection_conflict");
    }
    let catalog = settings.catalog(if parsed.offline { "offline" } else { "live" })?;
    let selected = if !parsed.session.is_empty() {
        catalog.explicit(&parsed.session)?
    } else if parsed.resume {
        catalog.latest()?
    } else {
        catalog.create()?
    };
    let tools = local_tools(
        settings.clone(),
        epoch.clone(),
        &selected.directory,
        &parsed.auth,
        &std::env::current_exe().map_err(|_| "process_executable")?,
    )?;
    let configuration = Request::new(bound, epoch, tools.advertisement().clone(), vec![])?;
    options.directory = Some(selected.directory);
    // Signals must be blocked before starting owner and output threads.
    let mut io = Io::open(parsed.prompt.is_none())?;
    let mut runtime = Runtime::new(
        SessionId(selected.id.clone()),
        configuration,
        tools,
        generate,
        Limits {
            compact_bytes: settings.compact_bytes,
            tool_rounds: settings.tool_rounds,
            tool_calls: settings.tool_calls,
            ..Limits::default()
        },
        options,
    )?;
    io.status(&format!(
        "ansa session {} (full access, sudo disabled)\n",
        selected.id
    ));
    let context = format!("{}  {}", settings.cwd.display(), settings.model);
    reactor()?.block_on(controller::run(
        &mut runtime,
        &mut io,
        parsed.prompt.as_deref(),
        false,
        &context,
    ))
}
fn auth_cli(command: &str, path: &Path) -> Result<i32> {
    let manager = auth_manager(path)?;
    let cancel = Cancel::default();
    let value = match command {
        "--login" | "--login-consent" => {
            // Retain the launcher child until login finishes; never detach a
            // reaper or let xdg-open's lifetime block callback acceptance.
            struct Launcher(Option<std::process::Child>);
            impl Drop for Launcher {
                fn drop(&mut self) {
                    if let Some(child) = &mut self.0 {
                        if child.try_wait().ok().flatten().is_none() {
                            let _ = child.kill();
                        }
                        let _ = child.wait();
                    }
                }
            }
            let mut launcher = Launcher(None);
            eprintln!("Continue with ChatGPT в открывшемся браузере.");
            manager.login(
                |url| {
                    if url.contains("id_token_hint") {
                        return Err("auth_browser_hint");
                    }
                    eprintln!("{url}");
                    launcher.0 = std::process::Command::new("xdg-open")
                        .arg(url)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .ok();
                    Ok(())
                },
                &cancel,
                command == "--login-consent",
            )?;
            object(vec![
                ("status", s("signed_in")),
                ("plan_enabled", b(manager.status()?.plan_enabled)),
            ])
        }
        "--logout" => object(vec![
            ("status", s("signed_out")),
            ("remote_revocation_confirmed", b(manager.logout(&cancel)?)),
        ]),
        "--refresh-auth" => {
            manager.access(&cancel, true)?;
            object(vec![("status", s("refreshed"))])
        }
        "--models" => {
            let catalog = manager.models(&cancel)?;
            let mut entries = vec![];
            for model in catalog.field("models")?.elements()? {
                if model.text_field("visibility")? == "list" {
                    entries.push(object(vec![
                        ("slug", model.field("slug")?.clone()),
                        ("display_name", model.field("display_name")?.clone()),
                    ]));
                }
            }
            Json::array(entries)?
        }
        _ => {
            let status = manager.status()?;
            object(vec![
                ("signed_in", b(status.logged_in)),
                ("refresh_pending", b(status.refresh_pending)),
                ("plan_enabled", b(status.plan_enabled)),
                ("account", s(&status.account)),
                ("expires_in", Json::parse(&status.expires_in.to_string())?),
                (
                    "refresh_wait",
                    Json::parse(&status.refresh_wait.to_string())?,
                ),
            ])
        }
    };
    write_json(&value)?;
    Ok(0)
}
fn live_jsonl(args: &[String]) -> Result<i32> {
    let mut auth = auth_path()?;
    let mut directory = None;
    let mut model = "gpt-6.1-sol".to_owned();
    let mut returned = None;
    let mut compact_bytes = 256 * 1024;
    let mut seen = std::collections::BTreeSet::new();
    for chunk in args.chunks(2) {
        if chunk.len() != 2 || !seen.insert(&chunk[0]) {
            return Err("live_cli_options");
        }
        let value = &chunk[1];
        match chunk[0].as_str() {
            "--session" => directory = Some(PathBuf::from(value)),
            "--auth-dir" => auth = value.into(),
            "--model" => model = value.clone(),
            "--returned-model" => returned = Some(value.clone()),
            "--compact-bytes" => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("live_cli_options");
                }
                compact_bytes = value.parse().map_err(|_| "live_cli_options")?;
            }
            _ => return Err("live_cli_unknown_option"),
        }
    }
    let directory = directory
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("live_session_required")?;
    let auth = auth_manager(&auth)?;
    let initial = auth.access(&Cancel::default(), false)?;
    let (account, generate, mut options) = authorized(auth, model.clone())?;
    if account != initial.account {
        return Err("auth_account_mismatch");
    }
    options.directory = Some(directory);
    let tools = pure_tools()?;
    let bound = Binding {
        account,
        requested_model: model.clone(),
        returned_model: returned.unwrap_or(model),
        ..binding()
    };
    let epoch=Epoch {id:"live_epoch_v1".into(),instructions:"You are oh-my-harness. Use the compiled get_test_value tool when requested. Keep replies concise and preserve facts from the conversation.".into(),exact_state:Json::parse(r#"{"mode":"live","compaction":"prompt_summary_keep_two_v1"}"#)?};
    let io = Io::open(false)?;
    let runtime = Runtime::new(
        "live".into(),
        Request::new(bound, epoch, tools.advertisement().clone(), vec![])?,
        tools,
        generate,
        Limits {
            compact_bytes,
            ..Limits::default()
        },
        options,
    )?;
    jsonl_runtime(io, runtime)
}
pub fn dispatch(args: &[String]) -> Result<i32> {
    let command = args.first().map_or("", String::as_str);
    match command {
        "--help" | "-h" => {
            std::io::stdout()
                .write_all(HELP.as_bytes())
                .map_err(|_| "cli_output")?;
            Ok(0)
        }
        "--live-jsonl" => live_jsonl(&args[1..]),
        "--login" | "--login-consent" | "--logout" | "--auth-status" | "--refresh-auth"
        | "--models" => {
            if args.len() == 1 {
                auth_cli(command, &auth_path()?)
            } else if args.len() == 3 && args[1] == "--auth-dir" {
                auth_cli(command, Path::new(&args[2]))
            } else {
                eprintln!("Usage: ansa {command} [--auth-dir PATH]");
                Ok(2)
            }
        }
        "--offline-jsonl" => {
            if args.len() == 3 && args[1] == "--session" {
                let io = Io::open(false)?;
                let runtime = offline_runtime(Some((&args[2]).into()), true)?;
                jsonl_runtime(io, runtime)
            } else if args.len() == 1 {
                let io = Io::open(false)?;
                let runtime = offline_runtime(None, true)?;
                jsonl_runtime(io, runtime)
            } else {
                eprintln!(
                    "Usage: ansa [--self-check|--offline-demo|--offline-jsonl [--session PATH]]"
                );
                Ok(2)
            }
        }
        "--self-check" | "--offline-demo" => {
            if args.len() != 1 {
                eprintln!(
                    "Usage: ansa [--self-check|--offline-demo|--offline-jsonl [--session PATH]]"
                );
                return Ok(2);
            }
            if command == "--offline-demo" {
                demo()
            } else {
                write_json(&object(vec![
                    ("project", s("ansa")),
                    ("target", s(&format!("{}-linux", std::env::consts::ARCH))),
                    ("curl", s("none (Hyper)")),
                    ("tls", s("rustls/ring")),
                    ("yyjson", s("none (Rust strict JSON)")),
                    ("openssl", s("none (ring)")),
                ]))?;
                Ok(0)
            }
        }
        _ => text(args),
    }
}
