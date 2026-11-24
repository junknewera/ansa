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
    context::summary::summarizer,
    openrouter::{MODEL as OPENROUTER_MODEL, OpenRouter},
    protocol::json::Json,
    provider::request::{Binding, Epoch, Request},
    runtime::{Generate, Lease, Limits, Options as RuntimeOptions, Runtime},
    terminal_host::tools as terminals,
    tools::{
        files::{LocalExtensions, local_tools_extended},
        registry::{Definition, Outcome, Policy, Registry},
    },
};
use std::{
    io::{BufRead, IsTerminal, Read, Write},
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

const OPENROUTER_KEY_FILE: &str = "openrouter.key";

fn openrouter_auth_path() -> Result<PathBuf> {
    Ok(home()?.join(".ansa/openrouter"))
}

fn openrouter_client(bytes: Vec<u8>) -> Result<OpenRouter> {
    let key = String::from_utf8(bytes).map_err(|_| "openrouter_key_invalid")?;
    OpenRouter::new(
        crate::auth::Secret::new(key.trim_end_matches(['\r', '\n']).to_owned()),
        Arc::new(crate::transport::client::openrouter_exchange),
    )
}

fn load_openrouter(path: &Path) -> Result<Arc<OpenRouter>> {
    let directory =
        ansa_linux::auth_store::Directory::open(path).map_err(|_| "openrouter_key_store")?;
    let key = directory
        .read(OPENROUTER_KEY_FILE)
        .map_err(|_| "openrouter_key_store")?
        .ok_or("openrouter_key_required")?;
    Ok(Arc::new(openrouter_client(key)?))
}

fn save_openrouter(path: &Path) -> Result<i32> {
    use std::io::Read;
    if std::io::stdin().is_terminal() {
        return Err("openrouter_key_requires_pipe");
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(515)
        .read_to_end(&mut bytes)
        .map_err(|_| "openrouter_key_input")?;
    if bytes.len() > 514 {
        return Err("openrouter_key_invalid");
    }
    openrouter_client(bytes.clone())?;
    let directory =
        ansa_linux::auth_store::Directory::open(path).map_err(|_| "openrouter_key_store")?;
    let _lease = directory
        .try_lock()
        .map_err(|_| "openrouter_key_store_busy")?;
    directory
        .save(
            OPENROUTER_KEY_FILE,
            &bytes,
            &crate::auth::pkce::random_token(16)?,
            |_, _| Ok(()),
        )
        .map_err(|_| "openrouter_key_store")?;
    write_json(&object(vec![("status", s("openrouter_key_saved"))]))?;
    Ok(0)
}

pub(super) fn openrouter_runtime(route: Arc<OpenRouter>) -> (Generate, RuntimeOptions) {
    let catalog = route.clone();
    let select = route.clone();
    let generate: Generate = Arc::new(move |request, stop, _| route.generate(request, stop));
    let options = RuntimeOptions {
        compact: Some(summarizer(generate.clone())),
        model_window: Arc::new(move |request| catalog.cached_window(request)),
        keep_recent_turns: 2,
        select_generation: Arc::new(move |selection, stop| select.validate_role(selection, stop)),
        ..RuntimeOptions::default()
    };
    (generate, options)
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
        .enable_io()
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
            compact: Some(Arc::new(offline::compact_retained)),
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
    authorized_in_workspace(auth, model, &Cancel::default(), Default::default(), true)
}

pub(super) fn authorized_in_workspace(
    auth: Arc<AuthManager>,
    model: String,
    stop: &Cancel,
    lane: super::sessions::SiwcLane,
    foreground: bool,
) -> Result<(String, Generate, RuntimeOptions)> {
    let status = {
        let _permit = lane.enter(foreground, stop)?;
        auth.status_with_cancel(stop)?
    };
    let guard = Authorization::new(auth.clone(), &status, model)?;
    let account = guard.account().to_owned();
    let guard = Arc::new(Mutex::new(guard));
    let check = guard.clone();
    let request_check = guard.clone();
    let selection = guard.clone();
    let provider = guard.clone();
    let catalog = guard.clone();
    let lease = guard;
    let generate_auth = auth.clone();
    let lease_auth = auth;
    let generate_lane = lane.clone();
    let generate: Generate = Arc::new(move |request, cancel, preview| {
        let _permit = generate_lane.enter(foreground, cancel)?;
        let (session, window) = {
            let provider = provider.lock().map_err(|_| "auth_state")?;
            (
                provider.session().to_owned(),
                provider.context_window(request),
            )
        };
        let tokens = request
            .estimated_tokens()
            .unwrap_or_else(|| crate::context::estimate_request_tokens(request));
        if let Some(window) = window {
            crate::context::require_response_room(tokens, window)?;
        }
        generate_auth
            .generate(
                request.clone(),
                cancel,
                &session,
                Some(Box::new(move |text| {
                    let _ = preview(text);
                })),
            )
            .map(|response| {
                response
                    .with_context_window(window)
                    .with_context_estimate(tokens)
            })
    });
    let compact_generate = generate.clone();
    let request_lane = lane.clone();
    let selection_lane = lane.clone();
    let dispatch_lane = lane.clone();
    let options = RuntimeOptions {
        model_window: Arc::new(move |request| catalog.try_lock().ok()?.context_window(request)),
        check_request: Arc::new(move |request, epoch, cancel| {
            let _permit = request_lane.enter(foreground, cancel)?;
            request_check
                .lock()
                .map_err(|_| "auth_state")?
                .check_request(request, epoch, cancel)
        }),
        select_generation: Arc::new(move |value, cancel| {
            let _permit = selection_lane.enter(foreground, cancel)?;
            selection
                .lock()
                .map_err(|_| "auth_state")?
                .check_generation(value, cancel)
        }),
        check_dispatch: Arc::new(move |epoch, cancel| {
            let _permit = dispatch_lane.enter(foreground, cancel)?;
            check
                .lock()
                .map_err(|_| "auth_state")?
                .check_account(epoch, cancel)
        }),
        tool_guard: Arc::new(move |cancel| {
            let permit = lane.enter(foreground, cancel)?;
            let session = lease.lock().map_err(|_| "auth_state")?.session().to_owned();
            let owned = lease_auth.hold_execution(&session, cancel)?;
            let stop = owned.cancel().clone();
            Ok(Lease {
                ownership: Box::new((owned, permit)),
                stop,
            })
        }),
        compact: Some(summarizer(compact_generate)),
        keep_recent_turns: 2,
        streaming: true,
        ..RuntimeOptions::default()
    };
    Ok((account, generate, options))
}
fn text(args: &[String]) -> Result<i32> {
    text_with_providers(args, load_openrouter, auth_manager)
}

fn text_with_providers(
    args: &[String],
    load_openrouter: impl FnOnce(&Path) -> Result<Arc<OpenRouter>>,
    load_siwc: impl FnOnce(&Path) -> Result<Arc<AuthManager>>,
) -> Result<i32> {
    let default_auth = if args.iter().any(|arg| arg == "--openrouter") {
        openrouter_auth_path()?
    } else {
        auth_path()?
    };
    let parsed = Options::parse(
        args,
        std::env::current_dir().map_err(|_| "file_access_failed")?,
        default_auth,
    )?;
    let interactive = parsed.prompt.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let mut settings = load_agent_settings(&parsed.cwd, &home()?)?;
    let use_saved_generation = interactive
        && !parsed.resume
        && parsed.session.is_empty()
        && parsed.model.is_none()
        && parsed.returned_model.is_none()
        && !parsed.openrouter;
    if parsed.openrouter
        && [&parsed.model, &parsed.returned_model]
            .into_iter()
            .flatten()
            .any(|model| model != OPENROUTER_MODEL)
    {
        return Err("model_not_available");
    }
    if let Some(model) = parsed.model {
        settings.model = model.clone();
        settings.returned_model = model;
    }
    if let Some(model) = parsed.returned_model {
        settings.returned_model = model;
    }
    if parsed.openrouter {
        settings.model = OPENROUTER_MODEL.into();
        settings.returned_model = OPENROUTER_MODEL.into();
    }
    if parsed.print_config {
        write_json(&settings.effective()?)?;
        return Ok(0);
    }
    let mut bound = binding();
    bound.requested_model = settings.model.clone();
    bound.returned_model = settings.returned_model.clone();
    let mut workspace_auth = None;
    let siwc_lane = super::sessions::SiwcLane::default();
    let openrouter = parsed
        .openrouter
        .then(|| load_openrouter(&parsed.auth))
        .transpose()?;
    let (generate, mut options) = if let Some(route) = &openrouter {
        bound = route.binding().clone();
        openrouter_runtime(route.clone())
    } else if parsed.offline {
        (
            Arc::new(|request: &Request, stop: &Cancel, _| offline::generate(request, stop))
                as Generate,
            RuntimeOptions {
                compact: Some(Arc::new(offline::compact_retained)),
                keep_recent_turns: 2,
                ..RuntimeOptions::default()
            },
        )
    } else {
        let auth = load_siwc(&parsed.auth)?;
        let authorize = || {
            authorized_in_workspace(
                auth.clone(),
                settings.model.clone(),
                &Cancel::default(),
                siwc_lane.clone(),
                true,
            )
        };
        let (account, generate, options) = match authorize() {
            Err("auth_required") if interactive => {
                if !login_at_startup(&auth)? {
                    return Ok(0);
                }
                authorize()?
            }
            result => result?,
        };
        workspace_auth = Some(auth);
        bound.account = account;
        (generate, options)
    };
    let mut factory = super::sessions::Factory::new(
        settings.clone(),
        parsed.auth.clone(),
        parsed.offline,
        bound.account.clone(),
        workspace_auth,
    )
    .with_carbon_frontend(interactive)
    .with_siwc_lane(siwc_lane);
    factory.harness_trial = parsed.harness_trial;
    if let Some(route) = openrouter {
        factory = factory.with_openrouter(route);
    }
    let mut effort = None;
    if use_saved_generation && let Some(selection) = factory.saved_generation() {
        effort.clone_from(&selection.effort);
        factory.use_generation(selection);
        settings = factory.settings.clone();
        bound.requested_model.clone_from(&settings.model);
        bound.returned_model.clone_from(&settings.returned_model);
    }
    let mut epoch = agent_epoch(&settings)?;
    if parsed.resume && !parsed.session.is_empty() {
        return Err("session_selection_conflict");
    }
    let (profile, skill_notice) = if parsed.resume || !parsed.session.is_empty() {
        (None, None)
    } else {
        crate::harness::select_with_notice(&settings, None, factory.harness_trial.as_deref())?
    };
    if let Some(profile) = &profile {
        profile.attach(&mut epoch)?;
    }
    let catalog = settings.catalog(factory.profile())?;
    let selected = if !parsed.session.is_empty() {
        catalog.explicit(&parsed.session)?
    } else if parsed.resume {
        catalog.latest()?
    } else {
        catalog.create()?
    };
    let editor = parsed.workspace_editor.then(|| {
        crate::tools::editor_context::EditorHub::new(
            settings.cwd.clone(),
            vec![
                parsed.auth.clone(),
                settings.directory.clone(),
                selected.directory.clone(),
            ],
        )
    });
    if let Some(editor) = &editor {
        factory = factory.with_editor(selected.directory.clone(), editor.clone());
    }
    let terminal =
        (!parsed.resume && parsed.session.is_empty() && factory.new_main_uses_terminal())
            .then(|| factory.open_terminal_owner(&selected, false))
            .transpose()?;
    if terminal.is_some() {
        terminals::add_agent_instructions(&mut epoch);
    }
    let shared_writer = terminal.as_ref().map(terminals::workspace_writer);
    // Mask signals before MCP and runtime workers inherit the process mask.
    let mut io = Io::open(parsed.prompt.is_none())?;
    let tools = local_tools_extended(
        settings.clone(),
        epoch.clone(),
        &selected.directory,
        &parsed.auth,
        &std::env::current_exe().map_err(|_| "process_executable")?,
        LocalExtensions {
            editor: editor.clone(),
            shared_writer: shared_writer.clone(),
        },
        |registry| {
            terminal.clone().map_or(Ok(()), |owner| {
                terminals::register(registry, owner, &selected.directory)
            })?;
            if let Some(profile) = &profile {
                crate::tools::skills::register(registry, Some(profile.clone()))?;
            }
            if !parsed.resume && parsed.session.is_empty() {
                crate::mcp::register(
                    registry,
                    &settings.directory,
                    &settings.cwd,
                    None,
                    &Cancel::default(),
                )?;
            }
            Ok(())
        },
    )?;
    let tools = if terminal.is_some() {
        tools.without_tool("run_command")
    } else {
        tools
    };
    let configuration =
        Request::new(bound, epoch, tools.advertisement().clone(), vec![])?.with_effort(effort)?;
    options.directory = Some(selected.directory.clone());
    factory.enable_delegation(&mut options, &selected.id, shared_writer);
    if !parsed.resume && parsed.session.is_empty() {
        options.autocompact_percent = factory.saved_autocompact();
    }
    options.plan_guard = Some(crate::terminal_host::workspace::plan_guard(&settings));
    if parsed.offline {
        let model = settings.model.clone();
        options.select_generation = Arc::new(move |selection, _| {
            if selection.model == model && selection.effort.is_none() {
                Ok(())
            } else {
                Err("model_not_available")
            }
        });
    }
    let (mut runtime, root_kind, parent, skill_commands) =
        if parsed.resume || !parsed.session.is_empty() {
            let restored = factory.resume(&selected.id)?;
            (
                restored.runtime,
                restored.kind,
                restored.parent,
                restored.skill_commands,
            )
        } else {
            (
                Runtime::new(
                    SessionId(selected.id.clone()),
                    configuration,
                    tools,
                    generate,
                    Limits {
                        tool_rounds: settings.tool_rounds,
                        tool_calls: settings.tool_calls,
                        ..Limits::default()
                    },
                    options,
                )?,
                super::sessions::Kind::Main,
                None,
                super::commands::skill_choices(profile.as_deref()),
            )
        };
    factory.retain_terminal_owner(&selected.id, terminal)?;
    io.status(&format!(
        "ansa session {} (full access, sudo disabled)\n",
        selected.id
    ));
    if io.raw.is_some() {
        return reactor()?.block_on(controller::workbench::run_session(
            runtime,
            &mut io,
            &settings.cwd.to_string_lossy(),
            factory,
            selected.id,
            root_kind,
            parent,
            skill_commands,
            skill_notice,
            parsed
                .workspace
                .map(|directory| -> Result<_> {
                    Ok(super::bridge::BridgeOptions {
                        directory,
                        editor,
                        artifact_store: Some(crate::tools::artifacts::Store::new(
                            &selected.directory,
                        )?),
                    })
                })
                .transpose()?,
        ));
    }
    if parsed.workspace.is_some() {
        return Err("workspace_requires_tty");
    }
    if let Some(code) = skill_notice {
        io.status(&format!("Local skills unavailable ({code})\n"));
    }
    let context = format!("{}  {}", settings.cwd.display(), settings.model);
    let result = reactor()?.block_on(controller::run(
        &mut runtime,
        &mut io,
        parsed.prompt.as_deref(),
        false,
        &context,
    ));
    if result.is_ok() && factory.has_terminal_owners() && !factory.close_terminals() {
        return Err("terminal_cleanup_unknown");
    }
    result
}

/// Record the provider's wire-level message history only in an opted-in synthetic fixture.
#[cfg(feature = "frontend-test-adapter")]
fn record_fixture_messages(path: &Path, messages: &serde_json::Value) -> Result<()> {
    match std::fs::OpenOptions::new().append(true).open(path) {
        Ok(mut file) => writeln!(file, "{messages}").map_err(|_| "fixture_record"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("fixture_record"),
    }
}

#[cfg(feature = "frontend-test-adapter")]
pub fn openrouter_carbon_fixture(root: &Path) -> Result<i32> {
    use serde_json::{Value, json};
    ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
    let home = root.join("home");
    if std::env::var_os("HOME").as_deref() != Some(home.as_os_str()) {
        return Err("fixture_home");
    }
    let cwd = home.join("workspace");
    ansa_linux::fs::private_directory(&cwd).map_err(|_| "fixture_workspace")?;
    let cwd = cwd.to_str().ok_or("fixture_workspace")?;
    use crate::{
        auth::manager::HttpReply,
        openrouter::{CATALOG, ENDPOINT, PROVIDER},
        transport::http::Head,
    };
    let inputs = if root.join("fixture-image-model").is_file() {
        vec!["text", "image"]
    } else {
        vec!["text"]
    };
    let wire_capture = root.join("outbound-messages.jsonl");
    let hold_child = root.join("hold-orchestration-child");
    let hold_summary = root.join("hold-orchestration-summary");
    let exchange: Exchange = Arc::new(move |request, stop| {
        let content = if request.url == CATALOG {
            json!({"data":{"id":OPENROUTER_MODEL,"architecture":{"input_modalities":inputs},"endpoints":[{"tag":ENDPOINT,"provider_name":PROVIDER,"supported_parameters":["structured_outputs","response_format","tools","tool_choice"]}]}})
        } else {
            let request: Value =
                serde_json::from_str(request.body.expose()).map_err(|_| "fixture_body")?;
            let format = &request["response_format"]["json_schema"];
            if format["name"] != "ansa_native_schema_probe" {
                record_fixture_messages(&wire_capture, &request["messages"])?;
            }
            let hold = match format["name"].as_str() {
                Some("ansa_child_result_v1") => Some(&hold_child),
                None if request.get("tools").is_none() => Some(&hold_summary),
                _ => None,
            };
            if let Some(hold) = hold {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
                while hold.exists() {
                    stop.check()?;
                    if std::time::Instant::now() >= deadline {
                        return Err("fixture_hold_timeout");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            let text = match format["name"].as_str() {
                Some("ansa_native_schema_probe") if format["strict"] == true => r#"{"value":"schema-enforced"}"#.into(),
                Some("ansa_orchestration_plan_v3") if format["strict"] == true => json!({"schema_version":1,"summary":"OPENROUTER_CARBON_PLAN","steps":[{"id":"one","task":"Проверить вход","access":"read","paths":[],"depends_on":[],"acceptance":"Ответ получен"}],"checks":[],"manual_checks":["Утвердить удаление после отчёта"],"command_checks":[],"read_folder":null}).to_string(),
                Some("ansa_child_result_v1") if format["strict"] == true && request["tool_choice"] == "none" => r#"{"status":"completed","summary":"Синтетическая проверка пройдена","findings":[]}"#.into(),
                None if request.get("tools").is_some() => "Готов к итоговому ответу".into(),
                None => "OPENROUTER_CARBON_DONE".into(),
                _ => return Err("fixture_format"),
            };
            json!({"id":"fixture_response","model":OPENROUTER_MODEL,"provider":PROVIDER,"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":text}}]})
        };
        Ok(HttpReply {
            head: Head {
                status: 200,
                content_types: vec!["application/json".into()],
            },
            body: content.to_string(),
        })
    });
    let route = Arc::new(OpenRouter::new(
        crate::auth::Secret::new("synthetic-fixture-key-0123456789".into()),
        exchange,
    )?);
    text_with_providers(
        &["--openrouter".into(), "--cwd".into(), cwd.into()],
        |_| Ok(route),
        |_| Err("fixture_provider"),
    )
}

#[cfg(feature = "frontend-test-adapter")]
pub fn siwc_carbon_fixture(root: &Path, session: Option<&str>) -> Result<i32> {
    use crate::{auth::manager::HttpReply, transport::http::Head};
    use serde_json::{Value, json};
    const MODEL: &str = "gpt-6.2-sol";
    ansa_linux::journal::fixture_root(root).map_err(|_| "fixture_owner")?;
    let fixture_home = root.join("home");
    if home()? != fixture_home {
        return Err("fixture_home");
    }
    let cwd = fixture_home.join("workspace");
    ansa_linux::fs::private_directory(&cwd).map_err(|_| "fixture_workspace")?;
    let plus = root.join("orchestration-plus").is_file();
    let direct_edit = root.join("direct-edit-check").is_file();
    let inputs = if root.join("fixture-image-model").is_file() {
        vec!["text", "image"]
    } else {
        vec!["text"]
    };
    let wire_capture = root.join("outbound-messages.jsonl");
    let trace = root.join("plus-requests.jsonl");
    let sequence = std::sync::atomic::AtomicUsize::new(0);
    let auth_path = fixture_home.join(".ansa/auth");
    let mut args = vec!["--model".into(), MODEL.into()];
    for (flag, path) in [("--cwd", &cwd), ("--auth-dir", &auth_path)] {
        args.extend([
            flag.into(),
            path.to_str().ok_or("fixture_workspace")?.into(),
        ]);
    }
    if let Some(id) = session {
        args.extend(["--session".into(), id.into()]);
    }
    text_with_providers(
        &args,
        |_| Err("fixture_provider"),
        move |path| {
            if path != auth_path {
                return Err("fixture_provider");
            }
            Ok(Arc::new(AuthManager::new(
                path,
                Arc::new(move |request, _| {
                    if request.url != "https://api.openai.com/v1/models" {
                        return Err("fixture_unexpected_exchange");
                    }
                    Ok(HttpReply {
                        head: Head {
                            status: 200,
                            content_types: vec!["application/json".into()],
                        },
                        body: if plus {
                            json!({"models":[
                                {"slug":MODEL,"visibility":"list","supported_reasoning_levels":["high"]},
                                {"slug":"gpt-6.1-sol","visibility":"list","supported_reasoning_levels":["medium"]},
                                {"slug":"gpt-6.1-luna","visibility":"list","supported_reasoning_levels":["low"]},
                            ]})
                        } else {
                            json!({"models":[{"slug":MODEL,"visibility":"list","input_modalities":inputs}]})
                        }.to_string(),
                    })
                }),
                Arc::new(|| 2_000_000_000),
                None,
                Some(Arc::new(move |request, bearer, feed, stop| {
                    if bearer.expose() != "synthetic-access" {
                        return Err("fixture_bearer");
                    }
                    let request: Value =
                        serde_json::from_str(&request.dump()).map_err(|_| "fixture_body")?;
                    if (!plus && request["model"] != MODEL)
                        || request["store"] != false
                        || request["stream"] != true
                    {
                        return Err("fixture_request");
                    }
                    record_fixture_messages(&wire_capture, &request["input"])?;
                    let input = request["input"].as_array().ok_or("fixture_input")?;
                    let last = input.last().ok_or("fixture_input")?;
                    let text = request.get("text");
                    let format = &request["text"]["format"];
                    let name = format["name"].as_str();
                    if name == Some("ansa_orchestration_plan_v3")
                        && last["content"] == "SYNTHETIC_PLAN_TIMEOUT"
                    {
                        return Err("http_timeout");
                    }
                    if text.is_some()
                        && (format["strict"] != true || format["type"] != "json_schema")
                    {
                        return Err("fixture_format");
                    }
                    if plus || direct_edit {
                        let index = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let items = if direct_edit {
                            direct_edit_fixture::items(&request, input, index)?
                        } else if trace.with_file_name("orchestration-plus-budget").is_file() {
                            plus_budget_fixture::items(&request, input, index)?
                        } else {
                            vec![plus_fixture_response(&request, input, &trace, index, stop)?]
                        };
                        let model = &request["model"];
                        let id = format!("plus-response-{index}");
                        let mut events = vec![
                            json!({"type":"response.created","sequence_number":0,"response":{"id":id,"model":model,"status":"in_progress","output":[]}}),
                        ];
                        for (output_index, item) in items.iter().enumerate() {
                            let mut added = item.clone();
                            added["status"] = json!("in_progress");
                            if item["type"] == "function_call" {
                                added["arguments"] = json!("");
                            }
                            events.push(json!({"type":"response.output_item.added","sequence_number":events.len(),"output_index":output_index,"item":added}));
                            events.push(json!({"type":"response.output_item.done","sequence_number":events.len(),"output_index":output_index,"item":item}));
                        }
                        let mut completed =
                            json!({"id":id,"model":model,"status":"completed","output":items});
                        if let Some(text) = text {
                            completed["text"] = text.clone();
                        }
                        events.push(json!({"type":"response.completed","sequence_number":events.len(),"response":completed}));
                        for event in events {
                            stop.check()?;
                            feed(format!("data: {event}\n\n").as_bytes())?;
                        }
                        return Ok(());
                    }
                    let output: Value = last["output"]
                        .as_str()
                        .and_then(|output| serde_json::from_str(output).ok())
                        .unwrap_or_default();
                    let needs_instructions = output["status"] == "instructions_required";
                    let direct_child = name == Some("ansa_child_result_v1")
                        && input.iter().any(|item| {
                            item["role"] == "user"
                                && item["content"].as_str().is_some_and(|text| {
                                    text.contains("Inspect the synthetic SIWC workspace")
                                })
                        });
                    let correct_directory = direct_child
                        && last["call_id"] == "fixture-rejected-directory"
                        && output["error"] == "tool_arguments_rejected"
                        && output["applied"] == false;
                    let call = name == Some("ansa_child_result_v1")
                        && (last["type"] != "function_call_output"
                            || needs_instructions
                            || correct_directory);
                    let nested = input.iter().any(|item| {
                        item["content"].as_str().is_some_and(|content| {
                            content.starts_with("Inspect nested project instructions")
                        })
                    });
                    let direct = input
                        .iter()
                        .any(|item| item["content"] == "DIRECT_SIWC_FIXTURE");
                    let skill_fixture = last["content"]
                        .as_str()
                        .or_else(|| last["content"][0]["text"].as_str())
                        .is_some_and(|text| {
                            text.starts_with("Use the pinned local skill `inspect-picture`")
                        });
                    let item = if name.is_none() && skill_fixture {
                        json!({"type":"function_call","id":"fixture-skill-item","call_id":"fixture-skill-call","namespace":"omh","name":"read_skill","arguments":r#"{"name":"inspect-picture"}"#,"status":"completed"})
                    } else if name.is_none() && direct && last["type"] != "function_call_output" {
                        if !request["tools"][0]["tools"]
                            .as_array()
                            .is_some_and(|tools| {
                                tools.iter().any(|tool| tool["name"] == "delegate_task")
                            })
                        {
                            return Err("fixture_delegate_tool");
                        }
                        let arguments =
                            json!({"task":"Inspect the synthetic SIWC workspace","paths":[],"read_paths":[]})
                                .to_string();
                        json!({"type":"function_call","id":"fixture-direct-item","call_id":"fixture-direct-call","namespace":"omh","name":"delegate_task","arguments":arguments,"status":"completed"})
                    } else if direct_child && last["type"] != "function_call_output" {
                        json!({"type":"function_call","id":"fixture-invalid-directory","call_id":"fixture-rejected-directory","namespace":"omh","name":"list_directory","arguments":r#"{"path":"."}"#,"status":"completed"})
                    } else if call {
                        if !request["tools"].to_string().contains("list_directory") {
                            return Err("fixture_tools");
                        }
                        let token = if needs_instructions {
                            if !nested
                                || !output["instructions"][0]["text"]
                                    .as_str()
                                    .is_some_and(|text| text.ends_with("END_NESTED_INSTRUCTIONS\n"))
                            {
                                return Err("fixture_nested_instructions");
                            }
                            output["instruction_token"].as_str()
                        } else {
                            request["instructions"]
                                .as_str()
                                .and_then(|instructions| {
                                    instructions.split_once("instruction_token is ")
                                })
                                .and_then(|(_, suffix)| suffix.split_once('.'))
                                .map(|(token, _)| token)
                        }
                        .ok_or("fixture_instruction_token")?;
                        let path = if nested { "ansa" } else { "." };
                        let round = usize::from(needs_instructions);
                        let arguments = json!({"path":path,"instruction_token":token}).to_string();
                        json!({"type":"function_call","id":format!("call-item-{round}"),"call_id":format!("fixture-call-{round}"),"namespace":"omh","name":"list_directory","arguments":arguments,"status":"completed"})
                    } else {
                        let answer = match name {
                        Some("ansa_orchestration_plan_v3") if request["tools"] == json!([]) =>
                            json!({"schema_version":1,"summary":"SIWC_CARBON_PLAN","steps":[
                                {"id":"one","task":"Inspect the fixture","access":"read","paths":[],"depends_on":[],"acceptance":"Read returned"},
                                {"id":"two","task":if last["content"] == "Inspect Workspace/pet" { "Inspect nested project instructions" } else { "Check the fixture again" },"access":"read","paths":[],"depends_on":[],"acceptance":"Read returned"}
                            ],"checks":[],"manual_checks":[],"command_checks":[],"read_folder":match last["content"].as_str() {
                                Some("Inspect Workspace/pet" | "UNNAMED_READ_SCOPE") => Some("Workspace/pet"), _ => None
                            }}).to_string(),
                        Some("ansa_child_result_v1") if output["entries"].is_array() =>
                            r#"{"status":"completed","summary":"SIWC_HELPER_DONE","findings":[]}"#.into(),
                        None if direct => {
                            if last["call_id"] != "fixture-direct-call" || output["status"] != "completed"
                                || output["summary"] != "SIWC_HELPER_DONE" {
                                return Err("fixture_delegate_result");
                            }
                            "DIRECT_SIWC_DONE".into()
                        }
                        None if last["call_id"] == "fixture-skill-call" => {
                            if output["body"] != "Locate the subject.\n" {
                                return Err("fixture_skill_body");
                            }
                            "SIWC_CARBON_DONE".into()
                        }
                        None => "SIWC_CARBON_DONE".into(),
                        _ => return Err("fixture_format"),
                    };
                        json!({"type":"message","id":"fixture-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer}]})
                    };
                    let mut added = item.clone();
                    added["status"] = json!("in_progress");
                    if item["type"] == "function_call" {
                        added["arguments"] = json!("");
                    }
                    let mut completed = json!({"id":"fixture-response","model":MODEL,"status":"completed","output":[item]});
                    if let Some(text) = text
                        && last["content"] != "UNCONFIRMED_FORMAT"
                    {
                        completed["text"] = text.clone();
                        if last["content"] == "UNCONSTRAINED_FORMAT" {
                            completed["text"]["format"]["strict"] = json!(false);
                        }
                    }
                    let events = [
                        json!({"type":"response.created","sequence_number":0,"response":{"id":"fixture-response","model":MODEL,"status":"in_progress","output":[]}}),
                        json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}),
                        json!({"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":item}),
                        json!({"type":"response.completed","sequence_number":3,"response":completed}),
                    ];
                    for event in events {
                        stop.check()?;
                        feed(format!("data: {event}\n\n").as_bytes())?;
                    }
                    Ok(())
                })),
            )?))
        },
    )
}

#[cfg(feature = "frontend-test-adapter")]
mod direct_edit_fixture;
#[cfg(feature = "frontend-test-adapter")]
mod plus_budget_fixture;

#[cfg(feature = "frontend-test-adapter")]
fn plus_fixture_response(
    request: &serde_json::Value,
    input: &[serde_json::Value],
    trace: &Path,
    index: usize,
    stop: &Cancel,
) -> Result<serde_json::Value> {
    use serde_json::{Value, json};
    let prompt = |item: &Value| {
        if item["role"] == "user" {
            item["content"].as_str().map(str::to_owned)
        } else {
            None
        }
    };
    let task = input.iter().rev().find_map(prompt).ok_or("fixture_input")?;
    let format = request["text"]["format"]["name"].as_str();
    let desired = |status, summary, findings: &[&str]| {
        json!({"status":status,"summary":summary,"findings":findings}).to_string()
    };
    let completed = |summary| desired("completed", summary, &["fixture evidence"]);
    let strong = ("gpt-6.2-sol", "high");
    let medium = ("gpt-6.1-sol", "medium");
    let weak = ("gpt-6.1-luna", "low");
    let model = request["model"].as_str().ok_or("fixture_request")?;
    let blocked = trace.with_file_name("orchestration-plus-blocked").is_file();
    let implement = trace
        .with_file_name("orchestration-plus-implement")
        .is_file();
    let checks_passed = task
        .contains(r#""passed_command_checks":[{"command":"./check.sh","status":"passed"}]"#)
        && task.contains(r#""failed_checks":[]"#)
        && !task.contains("fixture-check-output");
    let seen = |marker: &str| {
        input
            .iter()
            .any(|item| item["role"] == "assistant" && item.to_string().contains(marker))
    };
    let mut edit = None;
    let (stage, role, text) = match format {
        Some("ansa_orchestration_plan_v5") if implement => {
            let repair = request["instructions"]
                .as_str()
                .is_some_and(|text| text.contains("Keep requested_outcome as implementation"));
            if request["tools"] != json!([]) || repair != seen("MISLABELED_INVESTIGATION") {
                return Err("fixture_implementation_context");
            }
            if repair {
                ("implement-proposal", strong, json!({
                    "schema_version":1,"requested_outcome":"implementation","summary":"IMPLEMENT_SCOPED_CHANGE","steps":[
                        {"id":"edit","task":"Change the fixture report","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"Report starts with after fixture"}
                    ],"checks":[{"path":"report.txt","contains":"after fixture"}],
                    "manual_checks":[],"command_checks":[{"argv":["./check.sh"]}],"read_folder":null
                }).to_string())
            } else {
                ("mislabeled-investigation", strong, json!({
                    "schema_version":1,"requested_outcome":"investigation","summary":"MISLABELED_INVESTIGATION","steps":[
                        {"id":"inspect","task":"Read the fixture report","access":"read","paths":["report.txt"],"depends_on":[],"acceptance":"Report inspected"}
                    ],"checks":[],"manual_checks":[],"command_checks":[],"read_folder":null
                }).to_string())
            }
        }
        _ if implement => return Err("fixture_dispatch_without_implementation_approval"),
        Some("ansa_orchestration_plan_v5") if task == "PLUS_PLAN_ONLY" => (
            "plan-only", strong,
            json!({
                "schema_version":1,"requested_outcome":"plan_only","summary":"A tiny museum",
                "steps":[{"id":"exhibit","task":"Put a sock on the table","access":"read","paths":[],"depends_on":[],"acceptance":"The reader has a plan"}],
                "checks":[],"manual_checks":[],"command_checks":[],"read_folder":null
            }).to_string(),
        ),
        Some("ansa_orchestration_plan_v5") if blocked => (
            "strong-plan", strong,
            json!({
                "schema_version":1,"requested_outcome":"investigation","summary":"PLUS_BLOCKED_PLAN","steps":[
                    {"id":"status","task":"Inspect Git status","access":"read","paths":[],"depends_on":[],"acceptance":"Verified Git status"}
                ],"checks":[],"manual_checks":[],"command_checks":[],"read_folder":null
            }).to_string(),
        ),
        Some("ansa_orchestration_plan_v5") if !seen("SURVEY_FOR_IMPLEMENTATION") => (
            "survey-for-implementation", strong,
            json!({
                "schema_version":1,"requested_outcome":"implementation","summary":"SURVEY_FOR_IMPLEMENTATION","steps":[
                    {"id":"inspect","task":"Inspect report","access":"read","paths":["report.txt"],"depends_on":[],"acceptance":"Find the fixture"}
                ],"checks":[],"manual_checks":[],"command_checks":[],"read_folder":null
            }).to_string(),
        ),
        Some("ansa_orchestration_plan_v5") => (
            if model == strong.0 { "strong-plan" } else { "medium-plan" },
            if model == strong.0 { strong } else { medium },
            json!({
                "schema_version":1,"requested_outcome":"implementation","summary":"PLUS_CARBON_PLAN","steps":[
                    {"id":"quick","task":"Set report to after fixture","access":"write","paths":["report.txt"],"depends_on":[],"acceptance":"Report starts with after fixture"},
                    {"id":"fresh","task":"Confirm report preserves the fixture","access":"read","paths":["report.txt"],"depends_on":["quick"],"acceptance":"Report preserves the fixture"},
                ],"checks":[{"path":"report.txt","contains":"after fixture"}],"manual_checks":[],
                "command_checks":[{"argv":["./check.sh"]}],"read_folder":null
            }).to_string(),
        ),
        Some("ansa_child_result_v1") if task.starts_with("You are the coordinator") =>
            ("coordinate", medium, completed("ORIGINAL_COORDINATOR")),
        Some("ansa_child_result_v1") if task.starts_with("You are a fresh coordinator") => {
            if seen("ORIGINAL_COORDINATOR") || !task.contains("FRESH_DECISION") {
                return Err("fixture_replan_context");
            }
            ("fresh-coordinate", medium, completed("FRESH_COORDINATOR"))
        }
        Some("ansa_child_result_v1") if task.starts_with("Assigned task") => {
            if blocked {
                ("worker-blocked", weak, desired("blocked", "Read-only Git status unavailable", &[]))
            } else if task.contains("QUICK_DECISION") {
                edit = Some(("before fixture", "after fixture"));
                ("quick-fix", medium, completed("QUICK_FIX_APPLIED"))
            } else if task.contains("FRESH_COORDINATOR") {
                ("fresh-worker", weak, completed("FRESH_EVIDENCE"))
            } else {
                ("worker-rejected", weak, desired("completed", "WORKER_WITHOUT_EVIDENCE", &[]))
            }
        }
        Some("ansa_child_result_v1") if task.starts_with("Evaluate the worker") => {
            if blocked {
                ("verify-blocked", medium, desired("blocked", "Git state could not be independently verified", &[]))
            } else if task.contains("QUICK_FIX_APPLIED") && seen("Use another method") {
                ("quick-verify", medium, completed("QUICK_VERIFIED"))
            } else if task.contains("FRESH_EVIDENCE") && seen("FRESH_COORDINATOR") {
                ("fresh-verify", medium, completed("FRESH_VERIFIED"))
            } else if task.contains("WORKER_WITHOUT_EVIDENCE") && (task.contains("\nApproved task\n") || seen("Use another method")) {
                ("reject", medium, desired("changes_requested", "Use another method", &["Missing evidence"]))
            } else {
                return Err("fixture_coordinator_context");
            }
        }
        Some("ansa_child_result_v1") if task.starts_with("Resolve a failed approved task") => {
            if task.matches("WORKER_WITHOUT_EVIDENCE").count() != 3 {
                return Err("fixture_escalation_context");
            } else if task.contains("\nTask quick\n") {
                ("escalate-quick", strong, completed("QUICK_DECISION"))
            } else {
                ("escalate-fresh", strong, desired("changes_requested", "FRESH_DECISION", &["Start independent context"]))
            }
        }
        Some("ansa_child_result_v1") if task.starts_with("Perform the whole candidate's") => {
            if !checks_passed {
                return Err("fixture_review_checks");
            }
            ("whole-review", strong, desired("changes_requested", "INITIAL_STRONG_REVIEW", &["Append reviewed to the report."]))
        }
        Some("ansa_child_result_v1") if task.starts_with("Recheck the preceding findings") => {
            if !seen("INITIAL_STRONG_REVIEW") || !task.contains("candidate-1/ansa-candidate.diff")
                || !checks_passed
            {
                return Err("fixture_reviewer_context");
            }
            ("targeted-review", strong, completed("TARGETED_STRONG_REVIEW"))
        }
        Some("ansa_child_result_v1") if task.starts_with("Coordinate a focused correction") =>
            ("review-coordinate", medium, completed("Append reviewed to the report.")),
        Some("ansa_child_result_v1") if task.starts_with("Apply the focused correction") => {
            edit = Some(("after fixture", "after fixture reviewed"));
            ("review-fix", weak, completed("REVIEW_FIX_APPLIED"))
        }
        None if task == "PLUS_BASE_CONTEXT" => {
            ("prior-work", (model, ""), "PLUS_CONTEXT_READY".into())
        }
        None if task == "PLUS_CHAT_QUESTION" => {
            if !seen("PLUS_CONTEXT_READY")
                || !seen("PLUS_CARBON_PLAN")
                || request["tools"] != json!([])
                || request["instructions"].as_str().is_none_or(|text| !text.contains("Answer the user's question directly"))
            {
                return Err("fixture_parent_context");
            }
            ("active-chat", strong, "PLUS_CHAT_ANSWER".into())
        }
        None if task == "PLUS_CANCEL_CHAT" => return Err("fixture_cancelled_chat_dispatched"),
        None if task == "PLUS_WORK_FOLLOWUP" => {
            if !seen("PLUS_CARBON_PLAN") || !seen("PLUS_CHAT_ANSWER") || !seen("PLUS_CARBON_DONE") {
                return Err("fixture_parent_context");
            }
            ("later-work", strong, "PLUS_CONTINUITY_OK".into())
        }
        None if blocked && task == "PLUS_BLOCKED_FOLLOWUP" => {
            if !input.iter().filter_map(prompt).any(|message| {
                message.contains("Git state could not be independently verified")
                    && message.contains(r#""orchestration_outcome":"stopped""#)
            }) {
                return Err("fixture_parent_context");
            }
            ("later-work", (model, ""), "PLUS_BLOCKED_CONTEXT_RETAINED".into())
        }
        None if blocked => {
            if !task.contains("Git state could not be independently verified")
                || !task.contains(r#""orchestration_outcome":"stopped""#)
                || !task.contains(r#""phase":"coordinator verification""#)
                || request["tools"].as_array().is_none_or(|tools| !tools.is_empty())
                || task.contains("WORKER_WITHOUT_EVIDENCE")
            {
                return Err("fixture_parent_handoff");
            }
            ("stopped-summary", strong, "PLUS_STOPPED Git status is unavailable within approved read-only scope.".into())
        }
        None => ("summary", strong, "PLUS_CARBON_DONE".into()),
        _ => return Err("fixture_format"),
    };
    if !role.1.is_empty() && (model != role.0 || request["reasoning"]["effort"] != role.1) {
        return Err("fixture_roles");
    }
    let id = format!("plus-item-{index}");
    let last = input.last().ok_or("fixture_input")?;
    if let Some((old, new)) = edit {
        if last["type"] != "function_call_output" {
            let token = request["instructions"]
                .as_str()
                .and_then(|instructions| instructions.split_once("instruction_token is "))
                .and_then(|(_, suffix)| suffix.split_once('.'))
                .map(|(token, _)| token)
                .ok_or("fixture_instruction_token")?;
            if !request["tools"].to_string().contains("edit_file") {
                return Err("fixture_tools");
            }
            let arguments = json!({"path":"report.txt","old_text":old,"new_text":new,"instruction_token":token}).to_string();
            return Ok(
                json!({"type":"function_call","id":id,"call_id":format!("plus-call-{index}"),"namespace":"omh","name":"edit_file","arguments":arguments,"status":"completed"}),
            );
        }
        let result: Value = serde_json::from_str(last["output"].as_str().ok_or("fixture_edit")?)
            .map_err(|_| "fixture_edit")?;
        if result["applied"] != true {
            return Err("fixture_edit");
        }
    }
    let line = json!({"stage":stage,"model":model,"effort":request["reasoning"]["effort"]});
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(trace)
        .and_then(|mut file| writeln!(file, "{line}"))
        .map_err(|_| "fixture_trace")?;
    if stage == "stopped-summary"
        && trace
            .with_file_name("orchestration-plus-reject-summary")
            .is_file()
    {
        return Err("fixture_summary_rejected");
    }
    let hold = trace.with_file_name(format!("hold-plus-{stage}"));
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    while hold.exists() {
        stop.check()?;
        if std::time::Instant::now() >= deadline {
            return Err("fixture_hold_timeout");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(
        json!({"type":"message","id":id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}),
    )
}

#[derive(Default)]
struct BrowserLauncher(Option<std::process::Child>);

impl BrowserLauncher {
    fn open(&mut self, url: &str, show_url: bool) -> Result<()> {
        if url.contains("id_token_hint") {
            return Err("auth_browser_hint");
        }
        if show_url {
            eprintln!("{url}");
        }
        self.0 = std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
        if self.0.is_none() && !show_url {
            return Err("auth_browser_unavailable");
        }
        Ok(())
    }
}

impl Drop for BrowserLauncher {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn login_in_browser(manager: &AuthManager, cancel: &Cancel, consent: bool) -> Result<()> {
    let mut launcher = BrowserLauncher::default();
    eprintln!("Continue with ChatGPT in your browser.");
    manager.login(|url| launcher.open(url, true), cancel, consent)
}

pub(super) fn renew_in_browser(
    manager: &AuthManager,
    account: &str,
    cancel: &Cancel,
) -> Result<()> {
    let mut launcher = BrowserLauncher::default();
    manager.renew_login(account, |url| launcher.open(url, false), cancel)
}

fn login_at_startup(manager: &AuthManager) -> Result<bool> {
    eprintln!("ANSA · ChatGPT sign-in");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        eprintln!("Enter /login to open your browser, or /exit to quit.");
        let mut line = String::new();
        let read = (&mut input)
            .take(128)
            .read_line(&mut line)
            .map_err(|_| "cli_input_read")?;
        if read == 0 {
            return Ok(false);
        }
        if !line.ends_with('\n') {
            return Err("live_cli_options");
        }
        match line.trim() {
            "/exit" => return Ok(false),
            "/login" => match login_in_browser(manager, &Cancel::default(), false) {
                Ok(()) => return Ok(true),
                Err(code) => eprintln!("ChatGPT sign-in failed ({code})"),
            },
            _ => {}
        }
    }
}

fn auth_cli(command: &str, path: &Path) -> Result<i32> {
    let manager = auth_manager(path)?;
    let cancel = Cancel::default();
    let value = match command {
        "--login" | "--login-consent" => {
            login_in_browser(&manager, &cancel, command == "--login-consent")?;
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
                // Preserve the v1 CLI syntax for callers using saved arguments.
                value.parse::<usize>().map_err(|_| "live_cli_options")?;
            }
            _ => return Err("live_cli_unknown_option"),
        }
    }
    let directory = directory
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("live_session_required")?;
    let auth = auth_manager(&auth)?;
    // Match the interactive path: token/refresh checks belong to cancellable
    // dispatch, after signals are installed, never to JSONL startup. No auth
    // watcher or refresh transport runs before the single-thread signal guard.
    let (account, generate, mut options) = authorized(auth, model.clone())?;
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
        Limits::default(),
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
        "--workspace-lease" => {
            let message = crate::terminal_host::workspace::cli(&args[1..], &home()?)?;
            writeln!(std::io::stdout(), "{message}").map_err(|_| "cli_output")?;
            Ok(0)
        }
        "--export-evidence" => {
            let [_, directory] = args else {
                return Err("evidence_export_options");
            };
            let absolute = std::path::absolute(directory).map_err(|_| "evidence_export_options")?;
            let path = absolute.as_path();
            let catalog = crate::tools::files::summary_catalog(
                &crate::config::AgentSettings::defaults(path, path),
            )?;
            let fixture = pure_tools()?;
            let export = crate::session::evidence::export(path, &catalog, &fixture)?;
            write_json(&export)?;
            Ok(0)
        }
        "--mcp" => {
            write_json(&crate::mcp::cli(&args[1..], &home()?.join(".ansa"))?)?;
            Ok(0)
        }
        "--harness" => {
            let result = crate::harness::cli(&args[1..], &home()?)?;
            write_json(&Json::parse(&result.to_string())?)?;
            Ok(0)
        }
        "--openrouter-login" => match &args[1..] {
            [] => save_openrouter(&openrouter_auth_path()?),
            [flag, directory] if flag == "--auth-dir" => save_openrouter(Path::new(directory)),
            _ => Err("openrouter_login_options"),
        },
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
