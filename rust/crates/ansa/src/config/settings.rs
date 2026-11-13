use super::{Result, n, path_text, read_text, s};
use crate::protocol::json::Json;
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct AgentSettings {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub directory: PathBuf,
    pub model: String,
    pub returned_model: String,
    pub compact_bytes: usize,
    pub tool_rounds: usize,
    pub tool_calls: usize,
    pub process_timeout_ms: usize,
    pub process_output_bytes: usize,
    pub exclude: Vec<String>,
}
impl AgentSettings {
    pub fn effective(&self) -> Result<Json> {
        Json::object(vec![
            ("schema_version", n(1)),
            ("cwd", s(path_text(&self.cwd)?)?),
            ("model", s(&self.model)?),
            ("returned_model", s(&self.returned_model)?),
            ("compact_bytes", n(self.compact_bytes)),
            ("tool_rounds", n(self.tool_rounds)),
            ("tool_calls", n(self.tool_calls)),
            ("process_timeout_ms", n(self.process_timeout_ms)),
            ("process_output_bytes", n(self.process_output_bytes)),
            (
                "exclude",
                Json::array(self.exclude.iter().map(|v| s(v)).collect::<Result<_>>()?)?,
            ),
            ("access_mode", s("full_access")?),
            ("allow_sudo", Json::parse("false")?),
        ])
    }
    pub fn catalog(&self, profile: &str) -> Result<crate::session::catalog::Catalog> {
        let root = self.directory.join("sessions");
        ansa_linux::fs::create_private(&root).map_err(|_| "local_directory_permissions")?;
        crate::session::catalog::Catalog::open(&root, &self.cwd, profile)
    }
}
fn overlay(settings: &mut AgentSettings, path: &Path, project: bool) -> Result<()> {
    let Some(bytes) = read_text(path, 64 * 1024)? else {
        return Ok(());
    };
    let json = Json::parse_with_limits(&bytes, 64 * 1024, 16, 4096)?;
    if !json.is_object() || json.usize_field("schema_version")? != 1 {
        return Err("config_schema_version");
    }
    for key in json.keys()? {
        match key {
            "schema_version" => {}
            "model" => {
                settings.model = json.text_field(key)?.into();
                if !json.has("returned_model") {
                    settings.returned_model = settings.model.clone();
                }
            }
            "returned_model" => settings.returned_model = json.text_field(key)?.into(),
            "compact_bytes" => settings.compact_bytes = json.usize_field(key)?,
            "tool_rounds" => settings.tool_rounds = json.usize_field(key)?,
            "tool_calls" => settings.tool_calls = json.usize_field(key)?,
            "process_timeout_ms" => settings.process_timeout_ms = json.usize_field(key)?,
            "process_output_bytes" => settings.process_output_bytes = json.usize_field(key)?,
            "exclude" => {
                let entries = json.field(key)?.elements()?;
                if entries.len() > 64 {
                    return Err("config_excludes_limit");
                }
                settings.exclude.clear();
                for entry in entries {
                    let name = entry.text()?;
                    if name.is_empty()
                        || name.len() > 128
                        || name.contains('/')
                        || name.contains('\0')
                    {
                        return Err("config_exclude");
                    }
                    settings.exclude.push(name.into());
                }
            }
            "access_mode" | "allow_sudo" => {
                if project {
                    return Err("project_security_setting");
                }
                if if key == "access_mode" {
                    json.text_field(key)? != "full_access"
                } else {
                    *json.field(key)? != Json::parse("false")?
                } {
                    return Err("unsupported_access_policy");
                }
            }
            _ => return Err("unknown_config_setting"),
        }
    }
    Ok(())
}
pub fn load_agent_settings(cwd: &Path, home: &Path) -> Result<AgentSettings> {
    let cwd = cwd.canonicalize().map_err(|_| "file_access_failed")?;
    let home = home.canonicalize().map_err(|_| "file_access_failed")?;
    let mut settings = AgentSettings {
        directory: home.join(".ansa"),
        cwd,
        home,
        model: "gpt-6.1-sol".into(),
        returned_model: "gpt-6.1-sol".into(),
        compact_bytes: 256 * 1024,
        tool_rounds: 32,
        tool_calls: 64,
        process_timeout_ms: 60000,
        process_output_bytes: 16384,
        exclude: [
            ".git",
            "node_modules",
            ".cache",
            "build",
            "build-ansa",
            "result",
        ]
        .map(String::from)
        .to_vec(),
    };
    let user = settings.directory.join("config.json");
    let project = settings.cwd.join(".ansa/config.json");
    overlay(&mut settings, &user, false)?;
    if settings.cwd != settings.home {
        overlay(&mut settings, &project, true)?;
    }
    if settings.model.is_empty()
        || settings.model.len() > 256
        || settings.returned_model.is_empty()
        || settings.returned_model.len() > 256
        || !(256..=4 * 1024 * 1024).contains(&settings.compact_bytes)
        || !(1..=64).contains(&settings.tool_rounds)
        || !(1..=256).contains(&settings.tool_calls)
        || !(1..=600000).contains(&settings.process_timeout_ms)
        || !(1..=16384).contains(&settings.process_output_bytes)
    {
        return Err("agent_config_limits");
    }
    Ok(settings)
}
