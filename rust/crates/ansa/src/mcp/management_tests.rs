use super::{
    config::{CONFIG_FILE, disable_marker},
    *,
};
use serde_json::{Value, json};
use std::io::Write;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, symlink},
};

pub(super) struct Fixture(pub(super) PathBuf);

impl Fixture {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ansa-mcp-fixture-{}-{}",
            std::process::id(),
            crate::auth::pkce::random_token(16).unwrap()
        ));
        ansa_linux::fs::create_private_leaf(&path).unwrap();
        Self(path)
    }

    pub(super) fn config(&self, servers: Value) {
        let data = json!({"servers": servers}).to_string();
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.0.join(CONFIG_FILE))
            .unwrap()
            .write_all(data.as_bytes())
            .unwrap();
    }

    fn cli(&self, args: &[&str]) -> Result<Value> {
        let args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
        cli(&args, &self.0).map(|value| serde_json::from_str(&value.dump()).unwrap())
    }

    fn check_registration(&self, saved: Option<&Advertisement>, expected: Result<()>) {
        let mut registry = Registry::new();
        assert_eq!(
            register(&mut registry, &self.0, &self.0, saved, &Cancel::default()),
            expected
        );
        assert!(registry.snapshot().advertisement().descriptors.is_empty());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stdio() -> Value {
    // This relative command is rejected before any process can be spawned,
    // so an unexpected registration fails without running any executable.
    json!({
        "connection": {"transport": "stdio", "command": "command-canary", "args": ["arg-canary"]},
        "tools": ["echo"],
        "read_only": ["echo"],
        "resources": true,
    })
}

fn saved(names: Vec<String>) -> Advertisement {
    Advertisement {
        revision: "fixture".into(),
        descriptors: names
            .into_iter()
            .map(|name| Descriptor {
                name,
                description: "fixture".into(),
                revision: "fixture".into(),
                schema: Json::parse(WRAPPER).unwrap(),
            })
            .collect(),
    }
}

#[test]
fn management_returns_only_static_metadata_and_never_overwrites_source() {
    let fixture = Fixture::new();
    assert_eq!(fixture.cli(&["list"]).unwrap(), json!({"servers": []}));
    fixture.config(json!({
        "remote": {
            "connection": {
                "transport": "http",
                "url": "https://invalid.example/mcp?token=url-canary",
                "bearer_env": "MCP_ENV_CANARY",
                "headers": {"Authorization": "header-canary"},
            },
            "prompts": true,
        },
        "local": stdio(),
    }));
    let source = fixture.0.join(CONFIG_FILE);
    let original = fs::read(&source).unwrap();
    let inode = fs::metadata(&source).unwrap().ino();
    assert_eq!(
        fixture.cli(&["enable", "unknown"]),
        Err("mcp_server_unknown")
    );
    assert_eq!(
        fixture.cli(&["disable", "../local"]),
        Err("mcp_cli_options")
    );
    assert_eq!(fixture.cli(&["list", "local"]), Err("mcp_cli_options"));
    assert_eq!(
        fixture.cli(&["inspect", "local"]).unwrap(),
        json!({"name": "local", "enabled": true, "transport": "stdio",
            "tool_count": 1, "read_only_tool_count": 1, "resources": true, "prompts": false})
    );
    assert_eq!(
        fixture.cli(&["inspect", "remote"]).unwrap(),
        json!({"name": "remote", "enabled": true, "transport": "http",
            "tool_count": 0, "read_only_tool_count": 0, "resources": false, "prompts": true})
    );
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    for _ in 0..2 {
        assert_eq!(
            fixture.cli(&["disable", "local"]).unwrap(),
            json!({"name": "local", "enabled": false, "scope": "future_session_opens"})
        );
    }
    assert_eq!(
        fixture.cli(&["list"]).unwrap(),
        json!({"servers": [{"name": "local", "enabled": false}, {"name": "remote", "enabled": true}]})
    );
    for _ in 0..2 {
        assert_eq!(
            fixture.cli(&["enable", "local"]).unwrap(),
            json!({"name": "local", "enabled": true, "scope": "future_session_opens"})
        );
    }
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(fs::metadata(&source).unwrap().ino(), inode);
}

#[test]
fn disabled_server_skips_all_methods_and_enable_does_not_extend_pinned_catalog() {
    let fixture = Fixture::new();
    fixture.config(json!({"local": stdio()}));
    fixture.cli(&["disable", "local"]).unwrap();
    fixture.check_registration(None, Ok(()));
    fs::remove_file(fixture.0.join(CONFIG_FILE)).unwrap();
    fixture.config(json!({"local": stdio()}));
    fixture.check_registration(None, Ok(()));
    fixture.cli(&["enable", "local"]).unwrap();
    fixture.check_registration(Some(&saved(vec!["unrelated_builtin".into()])), Ok(()));
    // A fresh registration now reaches the deliberately invalid command.
    fixture.check_registration(None, Err("mcp_command"));
}

#[test]
fn removed_or_disabled_pinned_capability_blocks_reopen_before_any_worker() {
    let fixture = Fixture::new();
    let a = local_name("a", "echo");
    for b in [
        local_name("b", "echo"),
        Method::ResourcesList.local_name("b"),
    ] {
        let pinned = saved(vec![a.clone(), b]);
        fixture.config(json!({"a": stdio(), "b": stdio()}));
        fixture.cli(&["disable", "b"]).unwrap();
        fixture.check_registration(Some(&pinned), Err("session_fork_tools"));
        fixture.cli(&["enable", "b"]).unwrap();
        fixture.config(json!({"a": stdio(), "replacement": stdio()}));
        // `a` sorts first; reaching its invalid command would prove the
        // static check ran too late, even if no tools were registered.
        fixture.check_registration(Some(&pinned), Err("session_fork_tools"));
        fs::remove_file(fixture.0.join(CONFIG_FILE)).unwrap();
        fixture.check_registration(Some(&pinned), Err("session_fork_tools"));
    }
}

#[test]
fn refuse_linked_config_and_malformed_policy_without_activation() {
    let fixture = Fixture::new();
    fixture.config(json!({"local": stdio()}));
    let source = fixture.0.join(CONFIG_FILE);
    let marker = fixture.0.join(disable_marker("local"));
    symlink(&source, &marker).unwrap();
    assert_eq!(fixture.cli(&["enable", "local"]), Err("mcp_policy_file"));
    assert_eq!(fs::read_link(&marker).unwrap(), source);
    fixture.check_registration(None, Err("mcp_policy_file"));
    fs::remove_file(&marker).unwrap();
    let other = fixture.0.join("other");
    fs::hard_link(&source, &other).unwrap();
    assert_eq!(fixture.cli(&["disable", "local"]), Err("mcp_config_file"));
    assert!(!marker.exists());
}
