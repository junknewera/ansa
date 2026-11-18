//! Workspace composition. Each conversation has one independent runtime owner.
use super::{Result, app, offline, workspace};
use crate::{
    auth::{Cancel, manager::AuthManager},
    config::{AgentSettings, agent_epoch},
    protocol::json::Json,
    provider::request::Request,
    runtime::{ForkSeed, Generate, Limits, Options, Runtime},
    session::{
        catalog::{CatalogEntry, Selected},
        journal_v1::Journal,
    },
    terminal_host::tools as terminals,
    tools::files::local_tools_extended,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
const CONSOLE_ACCOUNT: &str = "ansa-local-console";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Main,
    Fork,
    Btw,
    Agent,
    Console,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Fork => "fork",
            Self::Btw => "btw",
            Self::Agent => "agent",
            Self::Console => "console",
        }
    }
    pub fn read_only(self) -> bool {
        self == Self::Btw
    }
}
#[derive(Clone)]
pub struct Factory {
    pub settings: AgentSettings,
    pub auth_path: PathBuf,
    pub offline: bool,
    account: String,
    auth: Option<Arc<AuthManager>>,
    editor: Option<(PathBuf, Arc<crate::tools::editor_context::EditorHub>)>,
    native_terminals: bool,
    terminal_owners: Arc<Mutex<BTreeMap<String, terminals::Owner>>>,
}
pub struct Created {
    pub runtime: Runtime,
    pub selected: Selected,
    pub kind: Kind,
    pub parent: Option<String>,
}
impl Factory {
    pub fn new(
        settings: AgentSettings,
        auth_path: PathBuf,
        offline: bool,
        account: String,
        auth: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            settings,
            auth_path,
            offline,
            account,
            auth,
            editor: None,
            native_terminals: false,
            terminal_owners: Arc::default(),
        }
    }
    pub fn with_native_terminals(mut self, enabled: bool) -> Self {
        self.native_terminals = enabled;
        self
    }

    pub(crate) fn open_terminal_owner(
        &self,
        selected: &Selected,
        restoring: bool,
    ) -> Result<terminals::Owner> {
        terminals::open_for(&self.settings, &selected.id, &selected.directory, restoring)
    }

    pub(crate) fn retain_terminal_owner(
        &self,
        id: &str,
        owner: Option<terminals::Owner>,
    ) -> Result<()> {
        let Some(owner) = owner else {
            return Ok(());
        };
        self.terminal_owners
            .lock()
            .map_err(|_| "terminal_owner_unavailable")?
            .insert(id.to_owned(), owner);
        Ok(())
    }

    pub(crate) fn terminal_owner(&self, id: &str) -> Option<terminals::Owner> {
        self.terminal_owners.lock().ok()?.get(id).cloned()
    }

    pub(crate) fn has_terminal_owners(&self) -> bool {
        self.terminal_owners
            .lock()
            .map_or(true, |owners| !owners.is_empty())
    }

    pub(crate) fn close_terminals(&self) -> bool {
        let owners = match self.terminal_owners.lock() {
            Ok(mut owners) => std::mem::take(&mut *owners),
            Err(_) => return false,
        };
        let mut known = true;
        for owner in owners.into_values() {
            known &= terminals::close(&owner);
        }
        known
    }
    pub fn with_editor(
        mut self,
        directory: PathBuf,
        editor: Arc<crate::tools::editor_context::EditorHub>,
    ) -> Self {
        self.editor = Some((directory, editor));
        self
    }
    pub fn set_editor_mode(&self, mode: crate::runtime::AgentMode) {
        if let Some((_, hub)) = &self.editor {
            hub.set_mode(mode);
        }
    }
    fn profile(&self) -> &'static str {
        if self.offline { "offline" } else { "live" }
    }
    pub fn entries(&self) -> Result<Vec<CatalogEntry>> {
        self.settings.catalog(self.profile())?.entries()
    }
    pub fn summaries(&self, stop: &Cancel) -> Result<Vec<crate::session::catalog::SessionSummary>> {
        self.settings.catalog(self.profile())?.summaries(stop)
    }
    pub(super) fn artifacts(&self, id: &str) -> Result<crate::tools::artifacts::Store> {
        let selected = self.settings.catalog(self.profile())?.explicit(id)?;
        crate::tools::artifacts::Store::new(&selected.directory)
    }
    pub fn create(&self, kind: Kind, seed: Option<ForkSeed>) -> Result<Created> {
        if matches!(kind, Kind::Fork | Kind::Btw | Kind::Agent) && seed.is_none() {
            return Err("session_context_required");
        }
        if seed
            .as_ref()
            .map(ForkSeed::request)
            .transpose()?
            .is_some_and(|r| r.binding.account != self.account)
        {
            return Err("session_provider_context");
        }
        let parent = seed.as_ref().map(|v| v.source_session().0.clone());
        let selected = self
            .settings
            .catalog(self.profile())?
            .create_kind(kind.name(), parent.as_deref())?;
        self.build(selected, kind, parent, seed, None)
    }
    pub fn create_console(&self, parent: &str) -> Result<Created> {
        let selected = self
            .settings
            .catalog(self.profile())?
            .create_kind("console", Some(parent))?;
        self.build(selected, Kind::Console, Some(parent.into()), None, None)
    }
    pub fn resume(&self, id: &str) -> Result<Created> {
        let catalog = self.settings.catalog(self.profile())?;
        let selected = catalog.explicit(id)?;
        let metadata = catalog
            .entries()?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or("unknown_session")?;
        let kind = match metadata.kind.as_str() {
            "btw" => Kind::Btw,
            "agent" => Kind::Agent,
            "console" => Kind::Console,
            "fork" => Kind::Fork,
            _ => Kind::Main,
        };
        let mut configuration = None;
        let mut canonical_parent = None;
        let journal = Journal::open(
            &selected.directory,
            |event| {
                if configuration.is_none() {
                    if event.text_field("kind")? != "init" {
                        return Err("session_missing_init");
                    }
                    let saved = Request::load(event.field("configuration")?)?;
                    if event.has("fork_seed") {
                        canonical_parent = Some(
                            event
                                .field("fork_seed")?
                                .text_field("source_session")?
                                .to_owned(),
                        );
                    } else if saved.epoch.exact_state.has("local_console_parent") {
                        canonical_parent = Some(
                            saved
                                .epoch
                                .exact_state
                                .text_field("local_console_parent")?
                                .to_owned(),
                        );
                    }
                    configuration = Some(saved);
                }
                Ok(())
            },
            None,
        )?;
        if !journal.diagnostic().is_empty() {
            return Err("journal_read_only");
        }
        drop(journal);
        let configuration = configuration.ok_or("session_missing_init")?;
        let expected_account = if kind == Kind::Console {
            CONSOLE_ACCOUNT
        } else {
            self.account.as_str()
        };
        if configuration.binding.account != expected_account {
            return Err("auth_account_mismatch");
        }
        // Explicitly selected history may carry another model, but cannot
        // silently restore old workspace permissions or instruction files.
        let current = agent_epoch(&self.settings)?;
        if base_state(&configuration.epoch.exact_state)? != base_state(&current.exact_state)? {
            return Err("session_configuration_mismatch");
        }
        self.build(selected, kind, canonical_parent, None, Some(configuration))
    }
    fn build(
        &self,
        selected: Selected,
        kind: Kind,
        parent: Option<String>,
        mut seed: Option<ForkSeed>,
        restored: Option<Request>,
    ) -> Result<Created> {
        let initial_epoch = agent_epoch(&self.settings)?;
        let source_configuration = restored
            .clone()
            .or(seed.as_ref().map(ForkSeed::request).transpose()?);
        if let Some(source) = &source_configuration
            && base_state(&source.epoch.exact_state)? != base_state(&initial_epoch.exact_state)?
        {
            return Err("session_configuration_mismatch");
        }
        let wants_terminals = source_configuration.as_ref().map_or(
            self.native_terminals && kind == Kind::Main,
            |source| {
                source
                    .tools
                    .descriptors
                    .iter()
                    .any(|descriptor| descriptor.name == "terminal_create")
            },
        );
        if wants_terminals && !self.offline && !self.native_terminals {
            return Err("native_terminal_opt_in_required");
        }
        let terminal = wants_terminals
            .then(|| self.open_terminal_owner(&selected, restored.is_some()))
            .transpose()?;
        let tools = local_tools_extended(
            self.settings.clone(),
            initial_epoch.clone(),
            &selected.directory,
            &self.auth_path,
            &std::env::current_exe().map_err(|_| "process_executable")?,
            self.editor
                .as_ref()
                .filter(|(directory, _)| directory == &selected.directory)
                .map(|(_, hub)| hub.clone()),
            |registry| {
                terminal.clone().map_or(Ok(()), |owner| {
                    terminals::register(registry, owner, &selected.directory)
                })
            },
        )?;
        // Resume/fork retains the exact saved tool catalogue. Adding a compiled
        // tool only affects new sessions; saved descriptors cannot install code.
        let tools = if let Some(source) = &source_configuration {
            tools
                .compatible(&source.tools)
                .ok_or("session_fork_tools")?
        } else {
            tools
        };
        let tools = if kind.read_only() {
            tools.read_only()
        } else {
            tools
        };
        if kind.read_only()
            && restored
                .as_ref()
                .is_some_and(|source| source.tools != *tools.advertisement())
        {
            return Err("session_fork_tools");
        }
        if kind.read_only()
            && let Some(source) = seed.take()
        {
            seed = Some(source.restricted_tools(tools.advertisement())?);
        }
        let base = restored.or(seed.as_ref().map(ForkSeed::request).transpose()?);
        let mut epoch = base
            .as_ref()
            .map(|r| r.epoch.clone())
            .unwrap_or(initial_epoch);
        if base.is_none() && terminal.is_some() {
            terminals::add_agent_instructions(&mut epoch);
        }
        let mut bound = base
            .as_ref()
            .map(|r| r.binding.clone())
            .unwrap_or_else(app::binding);
        bound.account = self.account.clone();
        if base.is_none() {
            bound.requested_model = self.settings.model.clone();
            bound.returned_model = self.settings.returned_model.clone();
        }
        if kind == Kind::Console {
            // The native codec is shared, but these local adapter records are
            // never attributed to the authenticated provider account.
            bound.account = CONSOLE_ACCOUNT.into();
            bound.requested_model = "local-console".into();
            bound.returned_model = "local-console".into();
            if base.is_none()
                && let Some(parent) = &parent
            {
                epoch.exact_state = epoch
                    .exact_state
                    .with("local_console_parent", Json::string(parent)?)?;
                epoch.id =
                    crate::protocol::legacy_json_v1::sha256(epoch.exact_state.dump().as_bytes());
            }
        }
        let (generate, mut options): (Generate, Options) = if kind == Kind::Console {
            let shell = std::env::var_os("SHELL")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| "/bin/sh".into());
            (
                workspace::console_generate(&self.settings, shell)?,
                Options::default(),
            )
        } else if self.offline {
            (
                Arc::new(|r, c, _| offline::generate(r, c)),
                Options {
                    compact: Some(Arc::new(offline::compact_retained)),
                    keep_recent_turns: 2,
                    ..Options::default()
                },
            )
        } else {
            let (account, generate, options) = app::authorized(
                self.auth.clone().ok_or("auth_required")?,
                bound.requested_model.clone(),
            )?;
            if account != self.account {
                return Err("auth_account_mismatch");
            }
            (generate, options)
        };
        options.directory = Some(selected.directory.clone());
        options.plan_guard = Some(crate::terminal_host::workspace::plan_guard(&self.settings));
        options.fork_seed = seed;
        if self.offline && kind != Kind::Console {
            let model = bound.requested_model.clone();
            options.select_generation = Arc::new(move |selection, _| {
                if selection.model == model && selection.effort.is_none() {
                    Ok(())
                } else {
                    Err("model_not_available")
                }
            });
        }
        let effort = base.as_ref().and_then(|r| r.effort.clone());
        let configuration = Request::new(bound, epoch, tools.advertisement().clone(), vec![])?
            .with_effort(effort)?;
        let runtime = Runtime::new(
            selected.id.as_str().into(),
            configuration,
            tools,
            generate,
            Limits {
                compact_bytes: self.settings.compact_bytes,
                tool_rounds: self.settings.tool_rounds,
                tool_calls: self.settings.tool_calls,
                ..Limits::default()
            },
            options,
        )?;
        self.retain_terminal_owner(&selected.id, terminal)?;
        Ok(Created {
            runtime,
            selected,
            kind,
            parent,
        })
    }
    pub fn models(&self, stop: &Cancel) -> Result<Json> {
        if let Some(auth) = &self.auth {
            let before = auth.status_with_cancel(stop)?;
            if !before.logged_in || !before.plan_enabled || before.account != self.account {
                return Err("auth_required");
            }
            let models = auth.models(stop)?;
            let after = auth.status_with_cancel(stop)?;
            if !after.logged_in
                || !after.plan_enabled
                || after.account != before.account
                || after.session != before.session
            {
                return Err("auth_required");
            }
            stop.check()?;
            // UI metadata is optional, derived and never consulted by dispatch.
            let _ = self.save_display_models(&after, &models);
            Ok(models)
        } else {
            Json::object(vec![(
                "models",
                Json::array(vec![Json::object(vec![
                    ("slug", Json::string(&self.settings.model)?),
                    ("display_name", Json::string(&self.settings.model)?),
                    ("visibility", Json::string("list")?),
                ])?])?,
            )])
        }
    }
    /// Worker-only disk lookup. It never performs discovery, token refresh,
    /// provider calls, or runtime authorization from saved display metadata.
    pub fn cached_models(&self, stop: &Cancel) -> Result<Option<Json>> {
        stop.check()?;
        let Some(auth) = &self.auth else {
            return self.models(stop).map(Some);
        };
        let before = auth.status_with_cancel(stop)?;
        if !before.logged_in || !before.plan_enabled || before.account != self.account {
            return Ok(None);
        }
        let loaded = (|| {
            let directory =
                ansa_linux::fs::private_directory(&self.settings.directory.join("ui-cache"))
                    .ok()?;
            let bytes = ansa_linux::model_catalog::read(&directory).ok()??;
            let value = Json::parse_with_limits(
                std::str::from_utf8(&bytes).ok()?,
                ansa_linux::model_catalog::LIMIT,
                32,
                16384,
            )
            .ok()?;
            if value.usize_field("version").ok()? != 1
                || value.text_field("binding").ok()? != self.model_cache_binding(&before).ok()?
            {
                return None;
            }
            let catalog = value.field("catalog").ok()?.clone();
            display_models(&catalog).ok()
        })();
        stop.check()?;
        let after = auth.status_with_cancel(stop)?;
        stop.check()?;
        if !after.logged_in
            || !after.plan_enabled
            || after.account != before.account
            || after.session != before.session
        {
            return Ok(None);
        }
        Ok(loaded)
    }
    fn model_cache_binding(&self, status: &crate::auth::manager::Status) -> Result<String> {
        Ok(crate::protocol::legacy_json_v1::sha256(
            Json::array(vec![
                Json::string("openai-siwc")?,
                Json::string(self.profile())?,
                Json::string(&self.auth_path.to_string_lossy())?,
                Json::string(&status.account)?,
                Json::string(&status.session)?,
            ])?
            .dump()
            .as_bytes(),
        ))
    }
    fn save_display_models(
        &self,
        status: &crate::auth::manager::Status,
        models: &Json,
    ) -> Result<()> {
        let value = Json::object(vec![
            ("version", Json::parse("1")?),
            ("binding", Json::string(&self.model_cache_binding(status)?)?),
            ("catalog", display_models(models)?),
        ])?
        .dump();
        if value.len() > ansa_linux::model_catalog::LIMIT {
            return Err("model_catalog_limit");
        }
        let path = self.settings.directory.join("ui-cache");
        ansa_linux::fs::create_private(&path).map_err(|_| "model_catalog_cache")?;
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "model_catalog_cache")?;
        ansa_linux::model_catalog::replace(
            &directory,
            value.as_bytes(),
            &crate::auth::pkce::random_token(16)?,
        )
        .map_err(|_| "model_catalog_cache")
    }
}
fn display_models(models: &Json) -> Result<Json> {
    let capabilities = crate::auth::dispatch::model_capabilities(models)?;
    if capabilities.len() > 256 {
        return Err("model_catalog_limit");
    }
    let mut models = Vec::new();
    for value in capabilities {
        if value.display_name.len() > 256
            || value.display_name.chars().any(char::is_control)
            || value.efforts.len() > 32
        {
            return Err("model_catalog_limit");
        }
        models.push(Json::object(vec![
            ("slug", Json::string(&value.model)?),
            ("display_name", Json::string(&value.display_name)?),
            ("visibility", Json::string("list")?),
            (
                "supported_reasoning_levels",
                Json::array(
                    value
                        .efforts
                        .iter()
                        .map(|v| Json::string(v))
                        .collect::<Result<_>>()?,
                )?,
            ),
        ])?);
    }
    Json::object(vec![("models", Json::array(models)?)])
}
fn base_state(value: &Json) -> Result<Json> {
    let mut value = value;
    for _ in 0..16 {
        if value.has("base_state") {
            value = value.field("base_state")?;
        } else if value.has("base") {
            value = value.field("base")?;
        } else {
            let mut value = value.clone();
            if value.has("local_console_parent") {
                value = value.without("local_console_parent")?;
            }
            if value.has("settings") {
                let settings = value
                    .field("settings")?
                    .without("model")?
                    .without("returned_model")?;
                value = value.with("settings", settings)?;
            }
            return Ok(value);
        }
    }
    Err("session_epoch_depth")
}
