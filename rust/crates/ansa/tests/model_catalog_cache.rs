#![forbid(unsafe_code)]
#[path = "support/auth_fixture.rs"]
mod fixture;
use ansa::{
    auth::{
        Cancel,
        dispatch::{Authorization, model_capabilities},
        manager::AuthManager,
    },
    config::load_agent_settings,
    frontend::sessions::Factory,
    protocol::json::Json,
    provider::contracts::GenerationSelection,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn setup() -> (
    fixture::Temp,
    Arc<fixture::Server>,
    Arc<AuthManager>,
    Factory,
) {
    let temp = fixture::Temp::new().unwrap();
    let server = fixture::Server::new(&temp.root).unwrap();
    let auth = server.manager(None, None).unwrap();
    server.login(&auth).unwrap();
    server.model_available.store(true, Ordering::Release);
    let factory = factory(&temp, auth.clone());
    (temp, server, auth, factory)
}
fn factory(temp: &fixture::Temp, auth: Arc<AuthManager>) -> Factory {
    for name in ["home", "workspace"] {
        ansa_linux::fs::create_private(&temp.root.join(name)).unwrap();
    }
    let settings =
        load_agent_settings(&temp.root.join("workspace"), &temp.root.join("home")).unwrap();
    let account = auth.status().unwrap().account;
    Factory::new(settings, temp.auth.clone(), false, account, Some(auth))
}
fn selection(model: &str, effort: Option<&str>) -> GenerationSelection {
    GenerationSelection {
        model: model.into(),
        effort: effort.map(str::to_owned),
    }
}
#[test]
fn shared_live_catalog_avoids_repeated_gets_but_checks_identity_and_ttl() {
    let (_temp, server, auth, factory) = setup();
    let stop = Cancel::default();
    assert!(factory.cached_models(&stop).unwrap().is_none());
    assert_eq!(server.catalogs.load(Ordering::Acquire), 0);
    factory.models(&stop).unwrap();
    factory.models(&stop).unwrap();
    let status = auth.status().unwrap();
    let mut guard = Authorization::new(auth.clone(), &status, "fixture-model".into()).unwrap();
    guard
        .check_generation(&selection("fixture-model", None), &stop)
        .unwrap();
    guard
        .check_generation(&selection("fixture-model", None), &stop)
        .unwrap();
    assert_eq!(
        guard.check_generation(&selection("fixture-model", Some("guessed")), &stop),
        Err("reasoning_effort_not_supported")
    );
    guard.check_dispatch(0, &stop).unwrap();
    guard.check_dispatch(1, &stop).unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 1);
    server.now.fetch_add(300, Ordering::AcqRel);
    guard
        .check_generation(&selection("fixture-model", None), &stop)
        .unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 2);
    assert_eq!(server.inference.load(Ordering::Acquire), 0);
}
#[test]
fn disk_display_cache_survives_manager_restart_and_never_authorizes_models() {
    let (temp, server, auth, factory1) = setup();
    let stop = Cancel::default();
    factory1.models(&stop).unwrap();
    let cache = factory1
        .settings
        .directory
        .join("ui-cache/models-cache.json");
    let original = fixture::read_json(&cache).unwrap();
    let forged = Json::parse(r#"{"models":[{"slug":"forged","visibility":"list","display_name":"Saved display only","supported_reasoning_levels":["invented"]}]}"#).unwrap();
    fixture::write_file(
        &cache,
        fixture::set(&original, "catalog", forged)
            .unwrap()
            .dump()
            .as_bytes(),
    )
    .unwrap();
    let restarted = server.manager(None, None).unwrap();
    let factory2 = factory(&temp, restarted.clone());
    let display = factory2.cached_models(&stop).unwrap().unwrap();
    assert_eq!(model_capabilities(&display).unwrap()[0].model, "forged");
    assert_eq!(server.catalogs.load(Ordering::Acquire), 1);
    let mut guard = Authorization::new(
        restarted.clone(),
        &restarted.status().unwrap(),
        "fixture-model".into(),
    )
    .unwrap();
    assert_eq!(
        guard.check_generation(&selection("forged", Some("invented")), &stop),
        Err("model_not_available")
    );
    assert_eq!(server.catalogs.load(Ordering::Acquire), 2);
    guard
        .check_generation(&selection("fixture-model", None), &stop)
        .unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 3);
    auth.logout(&stop).unwrap();
    server.login(&auth).unwrap();
    assert!(factory2.cached_models(&stop).unwrap().is_none());
    assert_eq!(
        guard.check_generation(&selection("fixture-model", None), &stop),
        Err("auth_required")
    );
    guard.check_dispatch(1, &stop).unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 4);
}
#[test]
fn catalog_metadata_is_sanitized_private_and_invalid_cache_is_ignored() {
    use std::os::unix::fs::MetadataExt;
    let (temp, server, auth, factory) = setup();
    let stop = Cancel::default();
    factory.models(&stop).unwrap();
    let cache = factory
        .settings
        .directory
        .join("ui-cache/models-cache.json");
    let saved = fixture::read_json(&cache).unwrap();
    assert_eq!(std::fs::metadata(&cache).unwrap().mode() & 0o777, 0o600);
    let model = &saved
        .field("catalog")
        .unwrap()
        .field("models")
        .unwrap()
        .elements()
        .unwrap()[0];
    assert!(!model.has("access_token") && !saved.has("credentials"));
    assert!(factory.cached_models(&stop).unwrap().is_some());
    fixture::write_file(
        &cache,
        fixture::set(&saved, "binding", Json::string("other-profile").unwrap())
            .unwrap()
            .dump()
            .as_bytes(),
    )
    .unwrap();
    assert!(factory.cached_models(&stop).unwrap().is_none());
    fixture::write_file(&cache, b"invalid JSON").unwrap();
    assert!(factory.cached_models(&stop).unwrap().is_none());
    fixture::write_file(&cache, saved.dump().as_bytes()).unwrap();
    std::fs::hard_link(&cache, temp.root.join("cache-alias")).unwrap();
    assert!(factory.cached_models(&stop).unwrap().is_none());
    factory.models(&stop).unwrap(); // Derived-cache publication failure is harmless.
    assert_eq!(
        fixture::read_json(&temp.root.join("cache-alias")).unwrap(),
        saved
    );
    assert_eq!(server.catalogs.load(Ordering::Acquire), 1);
    let mut guard = Authorization::new(
        auth.clone(),
        &auth.status().unwrap(),
        "fixture-model".into(),
    )
    .unwrap();
    let credentials = fixture::read_json(&temp.auth.join("credentials.json")).unwrap();
    let registration = fixture::read_json(&temp.auth.join("registration.json")).unwrap();
    fixture::write_file(
        &temp.auth.join("credentials.json"),
        fixture::set(
            &credentials,
            "subject",
            Json::string("other-synthetic-subject").unwrap(),
        )
        .unwrap()
        .dump()
        .as_bytes(),
    )
    .unwrap();
    fixture::write_file(
        &temp.auth.join("registration.json"),
        fixture::set(
            &registration,
            "subject",
            Json::string("other-synthetic-subject").unwrap(),
        )
        .unwrap()
        .dump()
        .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        guard.check_generation(&selection("fixture-model", None), &stop),
        Err("auth_account_mismatch")
    );
    auth.models(&stop).unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 2);
}
#[test]
fn cancelled_and_failed_catalog_exchanges_do_not_populate_live_cache() {
    let (temp, server, _auth, _factory) = setup();
    let behavior = Arc::new(AtomicUsize::new(0));
    let during = Cancel::default();
    let exchange_cancel = during.clone();
    let exchange_server = server.clone();
    let clock_server = server.clone();
    let exchange_behavior = behavior.clone();
    let auth = AuthManager::new(
        &temp.auth,
        Arc::new(move |request, cancel| {
            let reply = exchange_server.exchange(request, cancel)?;
            if request.url.ends_with("/v1/models") {
                match exchange_behavior.load(Ordering::Acquire) {
                    0 => exchange_cancel.cancel(),
                    1 => return Err("synthetic_catalog_failure"),
                    _ => (),
                }
            }
            Ok(reply)
        }),
        Arc::new(move || clock_server.now.load(Ordering::Acquire)),
        None,
        None,
    )
    .unwrap();
    assert_eq!(auth.models(&during).unwrap_err(), "auth_cancelled");
    behavior.store(1, Ordering::Release);
    assert_eq!(
        auth.models(&Cancel::default()).unwrap_err(),
        "synthetic_catalog_failure"
    );
    behavior.store(2, Ordering::Release);
    auth.models(&Cancel::default()).unwrap();
    auth.models(&Cancel::default()).unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 3);
    let cancelled = Cancel::default();
    cancelled.cancel();
    assert_eq!(auth.models(&cancelled).unwrap_err(), "auth_cancelled");
    assert_eq!(server.catalogs.load(Ordering::Acquire), 3);
}
#[test]
fn explicit_efforts_are_shared_and_private_disk_projection_omits_provider_extras() {
    let (temp, server, _auth, _factory) = setup();
    let exchange_server = server.clone();
    let clock_server = server.clone();
    let auth = Arc::new(AuthManager::new(&temp.auth, Arc::new(move |request, cancel| {
        let mut reply = exchange_server.exchange(request, cancel)?;
        if request.url.ends_with("/v1/models") {
            reply.body = r#"{"models":[{"slug":"fixture-model","visibility":"list","display_name":"Fixture","access_token":"synthetic-provider-extra","supported_reasoning_levels":[{"effort":"high"},"low"]}]}"#.into();
        }
        Ok(reply)
    }), Arc::new(move || clock_server.now.load(Ordering::Acquire)), None, None).unwrap());
    let factory = factory(&temp, auth.clone());
    factory.models(&Cancel::default()).unwrap();
    let mut guard = Authorization::new(
        auth.clone(),
        &auth.status().unwrap(),
        "fixture-model".into(),
    )
    .unwrap();
    for effort in [None, Some("high"), Some("low")] {
        guard
            .check_generation(&selection("fixture-model", effort), &Cancel::default())
            .unwrap();
    }
    assert_eq!(
        guard.check_generation(
            &selection("fixture-model", Some("medium")),
            &Cancel::default()
        ),
        Err("reasoning_effort_not_supported")
    );
    assert_eq!(server.catalogs.load(Ordering::Acquire), 1);
    let cache = factory
        .settings
        .directory
        .join("ui-cache/models-cache.json");
    let bytes = std::fs::read_to_string(cache).unwrap();
    assert!(!bytes.contains("synthetic-provider-extra") && !bytes.contains("access_token"));
    assert_eq!(
        model_capabilities(&factory.cached_models(&Cancel::default()).unwrap().unwrap()).unwrap()
            [0]
        .efforts,
        ["high", "low"]
    );
}
#[test]
fn identity_change_during_exchange_rejects_and_does_not_publish_live_cache() {
    let (temp, server, _auth, _factory) = setup();
    let mutate_once = Arc::new(AtomicBool::new(true));
    let exchange_once = mutate_once.clone();
    let credentials_path = temp.auth.join("credentials.json");
    let exchange_server = server.clone();
    let clock_server = server.clone();
    let auth = AuthManager::new(
        &temp.auth,
        Arc::new(move |request, cancel| {
            let reply = exchange_server.exchange(request, cancel)?;
            if request.url.ends_with("/v1/models") && exchange_once.swap(false, Ordering::AcqRel) {
                let record = fixture::read_json(&credentials_path)?;
                fixture::write_file(
                    &credentials_path,
                    fixture::set(
                        &record,
                        "session",
                        Json::string("replaced-synthetic-session")?,
                    )?
                    .dump()
                    .as_bytes(),
                )?;
            }
            Ok(reply)
        }),
        Arc::new(move || clock_server.now.load(Ordering::Acquire)),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        auth.models(&Cancel::default()).unwrap_err(),
        "auth_required"
    );
    auth.models(&Cancel::default()).unwrap();
    auth.models(&Cancel::default()).unwrap();
    assert_eq!(server.catalogs.load(Ordering::Acquire), 2);
    assert!(!mutate_once.load(Ordering::Acquire));
}
