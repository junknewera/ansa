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
    tools::files::local_tools,
};
use std::{path::PathBuf, sync::Arc};
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
        }
    }
    fn profile(&self) -> &'static str {
        if self.offline { "offline" } else { "live" }
    }
    pub fn entries(&self) -> Result<Vec<CatalogEntry>> {
        self.settings.catalog(self.profile())?.entries()
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
        let tools = local_tools(
            self.settings.clone(),
            initial_epoch.clone(),
            &selected.directory,
            &self.auth_path,
            &std::env::current_exe().map_err(|_| "process_executable")?,
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
                    compact: Some(Arc::new(offline::compact)),
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
        Ok(Created {
            runtime,
            selected,
            kind,
            parent,
        })
    }
    pub fn models(&self, stop: &Cancel) -> Result<Json> {
        if let Some(auth) = &self.auth {
            auth.models(stop)
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
