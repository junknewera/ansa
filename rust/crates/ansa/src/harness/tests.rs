use super::*;
use crate::{
    config::agent_epoch,
    provider::request::Binding,
    tools::{registry::Registry, skills},
};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ansa-skills-{}-{nonce}", std::process::id()));
        ansa_linux::fs::create_private_leaf(&path).unwrap();
        Self(path)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn revision(body: &str, policy: &str) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"version":1,"guidance":"Use the source tree.", "prompt_policy":policy,
            "skills":[{"name":"build","trigger":"Before a release","body":body}],
            "memory":[{"fact":"Build locally","source":"maintainer","applies_when":"release"}]
        }),
    )
    .unwrap()
}

#[test]
fn skills_reload_exact_content_and_policy_after_wrapped_epoch_and_new_revision() {
    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    let scope = scope(&settings.cwd);
    assert!(select(&settings, None, None).unwrap().is_none());
    let path = settings.directory.join("harness-profiles").join(&scope);
    create_private(&path).unwrap();
    let dir = private_directory(&path).unwrap();
    let active = adoption::command(
        &dir,
        &scope,
        &["baseline".into()],
        &revision("unread procedure", "legacy-v1"),
    )
    .unwrap();
    let old = active["activation"]["revision"].as_str().unwrap();
    let newer =
        store_manifest(&dir, &scope, &revision("candidate procedure", "handoff-v1")).unwrap();
    let profile = Arc::new(Pinned {
        directory: dir.try_clone().unwrap(),
        revision: newer.clone(),
        manifest: load_manifest(&dir, &scope, &newer).unwrap(),
        trial: None,
    });
    let mut epoch = agent_epoch(&settings).unwrap();
    profile.attach(&mut epoch).unwrap();
    assert!(!epoch.instructions.contains("candidate procedure"));
    assert!(epoch.instructions.contains("Build locally"));
    assert_eq!(
        PromptPolicy::resolve(&epoch.exact_state),
        Ok(PromptPolicy::HandoffV1)
    );
    assert_ne!(epoch.id, "ansa_local_v1");
    let pinned = epoch.exact_state.clone();
    epoch.exact_state = Json::object(vec![
        ("base", pinned.clone()),
        ("base_id", Json::string(&epoch.id).unwrap()),
        ("compaction_generation", Json::parse("1").unwrap()),
        (
            "compaction_digest",
            Json::string(&sha256(b"summary")).unwrap(),
        ),
    ])
    .unwrap();
    let mut registry = Registry::new();
    skills::register(&mut registry, Some(profile)).unwrap();
    let tools = registry.snapshot().read_only();
    let mut request = Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: "offline-fixture".into(),
            requested_model: "model".into(),
            returned_model: "model".into(),
        },
        epoch,
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let restored = select(&settings, Some(&request), None).unwrap().unwrap();
    assert_eq!(restored.revision, newer);
    assert_eq!(
        select(&settings, None, None).unwrap().unwrap().revision,
        old
    );
    let mut registry = Registry::new();
    skills::register(&mut registry, Some(restored)).unwrap();
    let restored_tools = registry
        .snapshot()
        .read_only()
        .compatible(&request.tools)
        .unwrap();
    let list = restored_tools.definition("list_skills").unwrap();
    let listing = (list.executor)(&Json::parse("{}").unwrap());
    let listing: serde_json::Value = serde_json::from_slice(listing.output()).unwrap();
    assert_eq!(listing["skills"][0]["name"], "build");
    assert!(listing["skills"][0].get("body").is_none());
    let read = restored_tools.definition("read_skill").unwrap();
    let name = Json::parse(r#"{"name":"build"}"#).unwrap();
    assert!((read.validator)(&name));
    let result = (read.executor)(&name);
    let result: serde_json::Value = serde_json::from_slice(result.output()).unwrap();
    assert_eq!(result["body"], "candidate procedure");
    assert!(!(read.validator)(
        &Json::parse(r#"{"name":"build","path":"/"}"#).unwrap()
    ));
    fs::write(
        path.join(format!("skill-{}", sha256(b"candidate procedure"))),
        b"wrong bytes",
    )
    .unwrap();
    let corrupt = (read.executor)(&name);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(corrupt.output()).unwrap(),
        json!({"error": "harness_skill_digest"})
    );
    let id = pinned.field("harness_profile").unwrap().clone();
    request.epoch.exact_state = request
        .epoch
        .exact_state
        .with("harness_profile", id)
        .unwrap();
    assert!(matches!(
        select(&settings, Some(&request), None),
        Err("harness_profile_identity")
    ));
    request.epoch.exact_state = pinned.without("prompt_policy").unwrap();
    assert!(matches!(
        select(&settings, Some(&request), None),
        Err("harness_prompt_policy_mismatch")
    ));
}

#[test]
fn malformed_skill_catalogue_writes_no_blobs() {
    let root = Project::new();
    let dir = private_directory(&root.0).unwrap();
    let scope = scope(&root.0);
    let input =
        serde_json::from_slice::<serde_json::Value>(&revision("body", "legacy-v1")).unwrap();
    for name in ["../outside", "", "build"] {
        let mut invalid = input.clone();
        invalid["skills"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":name,"trigger":"new","body":"never published"}));
        assert_eq!(
            store_manifest(&dir, &scope, &serde_json::to_vec(&invalid).unwrap()),
            Err("harness_skill")
        );
    }
    assert_eq!(
        store_manifest(&dir, &scope, &revision("body", "future")),
        Err("harness_profile_format")
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}
