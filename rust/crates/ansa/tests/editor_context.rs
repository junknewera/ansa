#[allow(dead_code)]
#[path = "support/tools_fixture.rs"]
mod fixture;
use ansa::{
    config::agent_epoch,
    protocol::json::Json,
    provider::{native, replay::Effects, request::Request},
    tools::{
        admission::{OperationIds, PreparedBatch},
        editor_context::EditorHub,
        files::local_tools_with_editor,
        registry::Snapshot,
    },
};
use fixture::{Root, n, s};
use std::{os::unix::fs::symlink, sync::Arc};

fn context(path: &str) -> Json {
    Json::object(vec![
        ("editor_incarnation", s("editor1")),
        ("buffer", n(1)),
        ("changedtick", n(3)),
        ("sequence", n(1)),
        ("path", s(path)),
        ("filetype", s("rust")),
        ("first_line", n(1)),
        ("text", s("UNSAVED_EDITOR_TEXT Ж界")),
    ])
    .unwrap()
}
fn arguments(root: &Root) -> Json {
    Json::object(vec![
        ("client", s("client1")),
        ("editor_incarnation", s("editor1")),
        ("buffer", n(1)),
        ("changedtick", n(3)),
        ("sequence", n(1)),
        ("instruction_token", s(&fixture::token(root))),
    ])
    .unwrap()
}
fn tools(root: &Root, hub: Arc<EditorHub>) -> Arc<Snapshot> {
    let settings = root.settings();
    local_tools_with_editor(
        settings.clone(),
        agent_epoch(&settings).unwrap(),
        &root.0.join("session"),
        &root.0.join("auth"),
        &fixture::worker(),
        Some(hub),
    )
    .unwrap()
}
fn invoke(root: &Root, tools: Arc<Snapshot>, name: &str, args: Json) -> Json {
    let request = Request::new(
        ansa::frontend::app::binding(),
        agent_epoch(&root.settings()).unwrap(),
        tools.advertisement().clone(),
        vec![],
    )
    .unwrap();
    let response = native::load(
        &Json::object(vec![
            ("request", request.save().unwrap()),
            ("id", s("response")),
            ("model", s("gpt-6.1-sol")),
            (
                "items",
                Json::array(vec![
                    Json::object(vec![
                        ("type", s("function_call")),
                        ("id", s("item")),
                        ("call_id", s("call")),
                        ("name", s(name)),
                        ("namespace", s("omh")),
                        ("arguments", s(&args.dump())),
                        ("status", s("completed")),
                    ])
                    .unwrap(),
                ])
                .unwrap(),
            ),
        ])
        .unwrap(),
    )
    .unwrap();
    let mut ids = OperationIds::new("editor_test", 0).unwrap();
    let results = PreparedBatch::prepare(&response, tools, &mut ids)
        .unwrap()
        .execute(false);
    assert_eq!(results.results().len(), 1);
    assert_eq!(results.results()[0].effects(), Effects::KnownResult);
    Json::parse(results.results()[0].output()).unwrap()
}

#[test]
fn compiled_editor_tool_checks_scoped_instructions_and_never_saves_unsaved_text() {
    let root = Root::new();
    let settings = root.settings();
    let epoch = agent_epoch(&settings).unwrap();
    std::fs::create_dir(root.cwd().join("src")).unwrap();
    std::fs::write(
        root.cwd().join("src/AGENTS.md"),
        "Keep this fixture selection intact.\n",
    )
    .unwrap();
    let hub = EditorHub::new(root.cwd(), vec![root.0.join("auth")]);
    let _attachment = hub.attach("client1").unwrap();
    hub.publish("client1", &context("src/new.rs")).unwrap();
    // Build against the original base scope, just as the production constructor.
    let registry = local_tools_with_editor(
        settings.clone(),
        epoch,
        &root.0.join("session"),
        &root.0.join("auth"),
        &fixture::worker(),
        Some(hub.clone()),
    )
    .unwrap();
    assert!(
        registry
            .read_only()
            .definition("editor_get_context")
            .is_some()
    );
    assert!(
        registry
            .read_only()
            .definition("workspace_describe")
            .is_some()
    );
    let first = invoke(
        &root,
        registry.clone(),
        "editor_get_context",
        arguments(&root),
    );
    assert_eq!(first.text_field("status").unwrap(), "instructions_required");
    assert!(!first.dump().contains("UNSAVED_EDITOR_TEXT"));
    let args = arguments(&root)
        .with(
            "instruction_token",
            first.field("instruction_token").unwrap().clone(),
        )
        .unwrap();
    let shared = invoke(&root, registry.clone(), "editor_get_context", args.clone());
    assert_eq!(
        shared.text_field("text").unwrap(),
        "UNSAVED_EDITOR_TEXT Ж界"
    );
    assert!(!root.cwd().join("src/new.rs").exists());
    hub.invalidate("client1", "editor1", 1, 1).unwrap();
    assert_eq!(
        invoke(&root, registry, "editor_get_context", args)
            .text_field("error")
            .unwrap(),
        "editor_unavailable"
    );
}

#[test]
fn editor_path_retargeted_to_protected_store_is_rejected_before_text_projection() {
    let root = Root::new();
    std::fs::create_dir(root.0.join("auth")).unwrap();
    std::fs::write(root.0.join("auth/fixture-key"), "OWNED_SYNTHETIC_CANARY").unwrap();
    let hub = EditorHub::new(root.cwd(), vec![root.0.join("auth")]);
    let _attachment = hub.attach("client1").unwrap();
    hub.publish("client1", &context("alias.rs")).unwrap();
    symlink(root.0.join("auth/fixture-key"), root.cwd().join("alias.rs")).unwrap();
    let result = invoke(
        &root,
        tools(&root, hub),
        "editor_get_context",
        arguments(&root),
    );
    assert_eq!(result.text_field("error").unwrap(), "protected_file");
    assert!(!result.dump().contains("OWNED_SYNTHETIC_CANARY"));
    assert!(!result.dump().contains("UNSAVED_EDITOR_TEXT"));
}
