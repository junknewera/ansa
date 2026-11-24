//! Static MCP configuration and enable/disable policy. This module does not connect to servers.
use super::{FRAME, Result, valid_name};
use crate::protocol::json::Json;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata, OpenOptions},
    io::{ErrorKind, Read},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub(super) const CONFIG_FILE: &str = "mcp.json";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Server {
    pub(super) connection: Transport,
    #[serde(default)]
    pub(super) tools: BTreeSet<String>,
    #[serde(default)]
    pub(super) read_only: BTreeSet<String>,
    #[serde(default)]
    pub(super) resources: bool,
    #[serde(default)]
    pub(super) prompts: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    servers: BTreeMap<String, Server>,
}

// The serialized representation is part of the legacy MCP tool revision.
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub(super) enum Transport {
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        bearer_env: Option<String>,
    },
}

pub(super) struct Config {
    directory: File,
    held_path: PathBuf,
    owner: u32,
    pub(super) servers: BTreeMap<String, Server>,
}

struct PolicyLock<'a>(&'a File);

impl Drop for PolicyLock<'_> {
    fn drop(&mut self) {
        // A forked child can keep the directory descriptor open until exec.
        // Release the lock as soon as the policy operation finishes.
        let _ = self.0.unlock();
    }
}

fn owned_regular(
    path: &Path,
    owner: u32,
    forbidden_mode: u32,
    error: &'static str,
) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.is_file()
                && meta.uid() == owner
                && meta.nlink() == 1
                && meta.mode() & forbidden_mode == 0 =>
        {
            Ok(Some(meta))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        _ => Err(error),
    }
}

pub(super) fn disable_marker(server: &str) -> String {
    // Keep a disable in effect if the user removes and re-adds the same ID.
    format!("mcp-disabled.{server}")
}

impl Config {
    pub(super) fn load(path: &Path) -> Result<Option<Self>> {
        match path.join(CONFIG_FILE).symlink_metadata() {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("mcp_config_file"),
            Ok(_) => {}
        }
        let directory = match ansa_linux::fs::private_directory(path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("mcp_config_directory"),
        };
        let meta = directory.metadata().map_err(|_| "mcp_config_directory")?;
        // Anchor operations to the checked directory, not a parent pathname
        // that can be replaced. Missing procfs must not look like no markers.
        let held_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let held_meta = held_path.metadata().map_err(|_| "mcp_config_directory")?;
        if (held_meta.dev(), held_meta.ino()) != (meta.dev(), meta.ino()) {
            return Err("mcp_config_directory");
        }
        let owner = meta.uid();
        let source = held_path.join(CONFIG_FILE);
        if owned_regular(&source, owner, 0o022, "mcp_config_file")?.is_none() {
            return Ok(None);
        }
        // Check permissions on the actual opened file, not only the entry
        // checked above. Keep the descriptor relative to the pinned directory.
        let flags = ansa_linux::local::read_flags() | ansa_linux::local::ReadFlags::NOFOLLOW;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(flags.bits() as i32)
            .open(source)
        {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("mcp_config_file"),
        };
        let meta = file.metadata().map_err(|_| "mcp_config_file")?;
        if !meta.is_file()
            || meta.uid() != owner
            || meta.nlink() != 1
            || meta.mode() & 0o022 != 0
            || !(1..=FRAME as u64).contains(&meta.len())
        {
            return Err("mcp_config_file");
        }
        let mut bytes = Vec::new();
        file.take(FRAME as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "mcp_config_file")?;
        if bytes.len() > FRAME {
            return Err("mcp_config_file");
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "mcp_config_utf8")?;
        let json = Json::parse(text).map_err(|_| "mcp_config_json")?;
        let Settings { servers } =
            serde_json::from_str(&json.dump()).map_err(|_| "mcp_config_schema")?;
        if servers.len() > 8 {
            return Err("mcp_config_servers");
        }
        for (name, server) in &servers {
            if !valid_name(name)
                || (server.tools.is_empty() && !server.resources && !server.prompts)
                || server.tools.len() > 32
                || server.tools.iter().any(|tool| !valid_name(tool))
                || !server.read_only.is_subset(&server.tools)
            {
                return Err("mcp_config_tools");
            }
        }
        Ok(Some(Self {
            directory,
            held_path,
            owner,
            servers,
        }))
    }

    pub(super) fn enabled(&self, name: &str) -> Result<bool> {
        let marker = self.held_path.join(disable_marker(name));
        match owned_regular(&marker, self.owner, 0o077, "mcp_policy_file")? {
            None => Ok(true),
            Some(meta) if meta.len() == 0 => Ok(false),
            Some(_) => Err("mcp_policy_file"),
        }
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        // Serialize cooperating writers on the checked directory. The marker
        // is an ANSA-owned empty file; never rewrite the user's source config.
        self.directory.try_lock().map_err(|_| "mcp_policy_busy")?;
        let _lock = PolicyLock(&self.directory);
        let marker = self.held_path.join(disable_marker(name));
        match (self.enabled(name)?, enabled) {
            (true, false) => OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(marker)
                .and_then(|file| file.sync_all())
                .map_err(|_| "mcp_policy_write")?,
            (false, true) => fs::remove_file(marker).map_err(|_| "mcp_policy_write")?,
            _ => {}
        }
        self.directory.sync_all().map_err(|_| "mcp_policy_sync")
    }
}

/// Handles args after `--mcp`. All results are static config metadata, not
/// server status; no transport or runtime owner is opened by this entry point.
pub(crate) fn cli(args: &[String], directory: &Path) -> Result<Json> {
    use serde_json::json;

    let (action, name) = match args {
        [action] if action == "list" => (action.as_str(), None),
        [action, name]
            if matches!(action.as_str(), "inspect" | "enable" | "disable") && valid_name(name) =>
        {
            (action.as_str(), Some(name))
        }
        _ => return Err("mcp_cli_options"),
    };
    let config = Config::load(directory)?;
    let result = if let Some(name) = name {
        let mut config = config.ok_or("mcp_server_unknown")?;
        let server = config.servers.get(name).ok_or("mcp_server_unknown")?;
        if action == "inspect" {
            json!({
                "name": name,
                "enabled": config.enabled(name)?,
                "transport": match &server.connection {
                    Transport::Stdio { .. } => "stdio",
                    Transport::Http { .. } => "http",
                },
                "tool_count": server.tools.len(),
                "read_only_tool_count": server.read_only.len(),
                "resources": server.resources,
                "prompts": server.prompts,
            })
        } else {
            let enabled = action == "enable";
            config.set_enabled(name, enabled)?;
            json!({"name": name, "enabled": enabled, "scope": "future_session_opens"})
        }
    } else {
        let mut servers = Vec::new();
        if let Some(config) = config {
            for name in config.servers.keys() {
                servers.push(json!({"name": name, "enabled": config.enabled(name)?}));
            }
        }
        json!({"servers": servers})
    };
    Json::parse(&result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::management_tests::{Fixture, stdio};
    use serde_json::{Value, json};
    use std::{
        os::unix::fs::symlink,
        process::{Command, Stdio},
    };

    #[test]
    fn policy_toggle_releases_lock_while_child_waits_for_exec() {
        let fixture = Fixture::new();
        fixture.config(json!({"local": stdio()}));
        let mut config = Config::load(&fixture.0).unwrap().unwrap();
        // The pipe keeps the child alive across both operations. Its stdout
        // holds a duplicate of the actual directory used by set_enabled.
        let mut child = Command::new("sh")
            .args(["-c", "IFS= read -r gate; exec true"])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(config.directory.try_clone().unwrap()))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let disabled = config.set_enabled("local", false);
        drop(config);
        let disabled_state = fixture.cli(&["inspect", "local"]);
        let enabled = fixture.cli(&["enable", "local"]);
        let enabled_state = fixture.cli(&["inspect", "local"]);
        let child_status = child.try_wait();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        assert!(child_status.unwrap().is_none());
        assert_eq!(disabled, Ok(()));
        assert_eq!(disabled_state.unwrap()["enabled"], Value::Bool(false));
        assert_eq!(enabled.unwrap()["enabled"], Value::Bool(true));
        assert_eq!(enabled_state.unwrap()["enabled"], Value::Bool(true));
    }

    #[test]
    fn policy_error_releases_lock_and_live_writers_keep_exclusivity() {
        let fixture = Fixture::new();
        fixture.config(json!({"local": stdio()}));
        let marker = fixture.0.join(disable_marker("local"));
        let competitor = ansa_linux::fs::private_directory(&fixture.0).unwrap();
        competitor.try_lock().unwrap();
        assert_eq!(fixture.cli(&["disable", "local"]), Err("mcp_policy_busy"));
        assert!(!marker.exists());
        competitor.unlock().unwrap();

        let source = fixture.0.join(CONFIG_FILE);
        symlink(&source, &marker).unwrap();
        let mut config = Config::load(&fixture.0).unwrap().unwrap();
        let inherited = config.directory.try_clone().unwrap();
        assert_eq!(config.set_enabled("local", false), Err("mcp_policy_file"));
        drop(config);
        assert_eq!(fs::read_link(&marker).unwrap(), source);
        fs::remove_file(&marker).unwrap();
        assert_eq!(
            fixture.cli(&["disable", "local"]).unwrap()["enabled"],
            Value::Bool(false)
        );
        drop(inherited);
    }
}
