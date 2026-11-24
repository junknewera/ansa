use super::*;
use crate::{
    config::agent_epoch,
    provider::request::Binding,
    tools::{registry::Registry, skills},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
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
        legacy_revision: None,
        directory_skills: Default::default(),
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

fn install_skill(settings: &AgentSettings, name: &str, description: &str, body: &str) -> PathBuf {
    let path = settings.directory.join("skills").join(name);
    create_private(&path).unwrap();
    let file = path.join("SKILL.md");
    fs::write(
        &file,
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
    std::fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    file
}

#[test]
fn missing_or_empty_skills_do_not_create_a_pin() {
    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    assert!(select(&settings, None, None).unwrap().is_none());
    assert!(!settings.directory.exists());

    fs::create_dir(&settings.directory).unwrap();
    fs::set_permissions(&settings.directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(select(&settings, None, None).unwrap().is_none());
    assert_eq!(fs::read_dir(&settings.directory).unwrap().count(), 0);

    fs::set_permissions(&settings.directory, fs::Permissions::from_mode(0o700)).unwrap();
    create_private(&settings.directory.join("skills")).unwrap();
    assert!(select(&settings, None, None).unwrap().is_none());
    assert!(!settings.directory.join("harness-profiles").exists());
}

#[test]
fn legacy_skill_wins_name_collision_and_keeps_its_profile_revision() {
    use crate::tools::skills::Catalog;

    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    let scope = scope(&settings.cwd);
    let path = settings.directory.join("harness-profiles").join(&scope);
    create_private(&path).unwrap();
    let dir = private_directory(&path).unwrap();
    let active = adoption::command(
        &dir,
        &scope,
        &["baseline".into()],
        &revision("legacy body", "handoff-v1"),
    )
    .unwrap();
    let legacy = active["activation"]["revision"].as_str().unwrap();
    let shadow = install_skill(&settings, "build", "Shadow description", "shadow body");
    assert_eq!(
        select(&settings, None, None).unwrap().unwrap().revision,
        legacy
    );
    assert!(!path.join("catalogs").exists());

    install_skill(&settings, "audit", "Audit a release", "directory body");
    let pin = select(&settings, None, None).unwrap().unwrap();
    assert_ne!(pin.revision, legacy);
    assert_eq!(
        pin.complete(""),
        vec![("audit", "Audit a release"), ("build", "Before a release")]
    );
    assert_eq!(pin.complete("bu"), vec![("build", "Before a release")]);
    assert_eq!(pin.list()["revision"].as_str(), Some(pin.revision.as_str()));
    assert_eq!(pin.list()["skills"].as_array().unwrap().len(), 2);
    assert_eq!(pin.read("build").unwrap()["profile_revision"], legacy);
    assert_eq!(pin.read("build").unwrap()["body"], "legacy body");
    assert_eq!(pin.read("audit").unwrap()["body"], "directory body");
    let mut epoch = agent_epoch(&settings).unwrap();
    pin.attach(&mut epoch).unwrap();
    assert_eq!(
        PromptPolicy::resolve(&epoch.exact_state),
        Ok(PromptPolicy::HandoffV1)
    );
    assert!(!epoch.instructions.contains("Shadow description"));
    assert!(!epoch.instructions.contains("directory body"));
    assert_eq!(
        adoption::active(&dir, &scope).unwrap().as_deref(),
        Some(legacy)
    );

    fs::remove_file(shadow).unwrap();
    assert_eq!(
        select(&settings, None, None).unwrap().unwrap().revision,
        pin.revision
    );
}

#[test]
fn discovery_keeps_owned_skill_documents_when_another_entry_is_invalid() {
    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    let file = install_skill(&settings, "good", "original", "body");
    let root = settings.directory.join("skills");
    for name in ["bad-name", "linked", "oversized", "public"] {
        let path = install_skill(&settings, name, "not available", "body");
        match name {
            "bad-name" => {
                fs::write(path, "---\nname: other\ndescription: invalid\n---\nbody").unwrap()
            }
            "linked" => {
                fs::remove_file(&path).unwrap();
                symlink(&file, path).unwrap();
            }
            "oversized" => fs::write(path, vec![b'x'; 16385]).unwrap(),
            _ => fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap(),
        }
    }
    symlink(root.join("good"), root.join("linked-folder")).unwrap();
    let (profile, notice) = select_with_notice(&settings, None, None).unwrap();
    let profile = profile.unwrap();
    assert!(notice.unwrap().contains("skill_"));
    assert_eq!(profile.complete(""), [("good", "original")]);
    let mut epoch = agent_epoch(&settings).unwrap();
    profile.attach(&mut epoch).unwrap();
    assert!(
        epoch
            .instructions
            .contains("When a description matches the task, call read_skill")
    );
    assert!(epoch.instructions.contains("original"));
}

#[test]
fn a_new_long_skill_keeps_all_previous_skills_and_roundtrips_utf8_pages() {
    use crate::tools::skills::Catalog;

    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    for number in 0..32 {
        install_skill(
            &settings,
            &format!("skill-{number:02}"),
            "Review a draft",
            "short",
        );
    }
    let previous = select(&settings, None, None).unwrap().unwrap();
    assert_eq!(previous.complete("").len(), 32);

    let body = format!("{}🦀\n{}\n", "x".repeat(8191), "строка\n".repeat(1600));
    let file = install_skill(&settings, "review-long", "Review a long draft", &body);
    let (pin, notice) = select_with_notice(&settings, None, None).unwrap();
    assert!(notice.is_none());
    let pin = pin.unwrap();
    assert_eq!(pin.complete("").len(), 33);
    for (name, _) in previous.complete("") {
        assert_eq!(pin.read(name).unwrap(), previous.read(name).unwrap());
    }
    assert_eq!(pin.list()["skills"].as_array().unwrap().len(), 33);

    fs::remove_file(file).unwrap();
    let first = pin.read("review-long").unwrap();
    assert_eq!(first["body"].as_str().unwrap().len(), 8191);
    let mut text = first["body"].as_str().unwrap().to_owned();
    let mut next = first["next_offset"].as_u64();
    while let Some(offset) = next {
        assert_eq!(offset as usize, text.len());
        let page = pin.read_page("review-long", offset as usize).unwrap();
        assert!(page["body"].as_str().unwrap().len() <= 8192);
        text.push_str(page["body"].as_str().unwrap());
        next = page["next_offset"].as_u64();
    }
    assert_eq!(text, body);
    assert_eq!(
        pin.read_page("review-long", 8192),
        Err("harness_skill_offset")
    );

    let catalogs = settings
        .directory
        .join("harness-profiles")
        .join(scope(&settings.cwd))
        .join("catalogs");
    let pinned = catalogs.join(format!("body-{}", sha256(body.as_bytes())));
    fs::write(pinned, format!("z{}", &body[1..])).unwrap();
    assert_eq!(pin.read_page("review-long", 0), Err("harness_skill_digest"));
}

#[test]
fn concurrent_catalog_pins_share_an_index_and_keep_exact_bodies() {
    use crate::tools::skills::Catalog;

    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    let first = format!("{} saved version", "x".repeat(14_000));
    let files: Vec<_> = (0..5)
        .map(|i| install_skill(&settings, &format!("skill-{i}"), "Review a draft", &first))
        .collect();
    let barrier = std::sync::Barrier::new(8);
    let profiles = std::thread::scope(|thread| {
        let jobs = (0..8)
            .map(|_| {
                thread.spawn(|| {
                    barrier.wait();
                    let (pin, notice) = select_with_notice(&settings, None, None).unwrap();
                    assert_eq!(notice, None);
                    pin.unwrap()
                })
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    let pin = &profiles[0];
    assert!(profiles.iter().all(|other| other.revision == pin.revision));
    assert_eq!(pin.complete("").len(), 5);

    let catalogs = settings
        .directory
        .join("harness-profiles")
        .join(scope(&settings.cwd))
        .join("catalogs");
    let stored_index =
        fs::read_to_string(catalogs.join(format!("catalog-{}", pin.revision))).unwrap();
    assert!(!stored_index.contains("saved version"));
    assert_eq!(fs::read_dir(&catalogs).unwrap().count(), 2);

    fs::write(
        &files[0],
        "---\nname: skill-0\ndescription: Changed\n---\nnew text",
    )
    .unwrap();
    assert_eq!(pin.read("skill-0").unwrap()["body"], first);
    let mut epoch = agent_epoch(&settings).unwrap();
    pin.attach(&mut epoch).unwrap();
    let request = Request::new(
        crate::frontend::app::binding(),
        epoch,
        Registry::new().snapshot().advertisement().clone(),
        vec![],
    )
    .unwrap();
    let reopened = select(&settings, Some(&request), None).unwrap().unwrap();
    assert_eq!(reopened.read("skill-0").unwrap()["body"], first);
    let digest = sha256(first.as_bytes());
    fs::remove_file(catalogs.join(format!("body-{digest}"))).unwrap();
    assert_eq!(reopened.read("skill-0"), Err("harness_skill_missing"));
}

#[test]
fn unavailable_local_skills_allow_unrelated_chat() {
    let home = Project::new();
    let settings = AgentSettings::defaults(&home.0, &home.0);
    let _ = install_skill(&settings, "skill", "Review a draft", "body");
    fs::set_permissions(
        settings.directory.join("skills"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let (pin, notice) = select_with_notice(&settings, None, None).unwrap();
    assert!(pin.is_none());
    assert_eq!(notice.as_deref(), Some("skill_directory_unavailable"));
}

#[test]
fn legacy_inline_catalog_still_reads_the_exact_body() {
    let home = Project::new();
    let dir = private_directory(&home.0).unwrap();
    let scope = scope(&home.0);
    let bytes = serde_json::to_vec(&json!({
        "version": 1,
        "scope": scope,
        "profile": null,
        "skills": [{"name":"review","description":"Review a draft","body":"legacy body\n"}]
    }))
    .unwrap();
    let digest = sha256(&bytes);
    ansa_linux::local::create(&dir, &format!("catalog-{digest}"), &bytes).unwrap();
    let (profile, skills) = directory_skills::DirectorySkills::load(dir, &scope, &digest)
        .unwrap()
        .unwrap();
    assert_eq!(profile, None);
    assert_eq!(
        skills.read("review", None).unwrap()["body"],
        "legacy body\n"
    );
}
