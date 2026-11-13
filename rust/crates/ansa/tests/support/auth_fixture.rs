#![allow(dead_code)]
//! Synthetic provider and owned stores shared by native tests and JSONL adapter.
use ansa::{
    auth::{
        self, Cancel, Result,
        manager::{AuthManager, HttpReply, HttpRequest, StreamExchange},
        store_v1::Observer,
    },
    protocol::json::Json,
    transport::http::Head,
};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub fn text(value: &str) -> Result<Json> {
    Json::string(value)
}
pub fn number(value: u64) -> Result<Json> {
    Json::parse(&value.to_string())
}
fn json_reply(value: Json) -> Result<HttpReply> {
    Ok(HttpReply {
        head: Head {
            status: 200,
            content_types: vec!["application/json".into()],
        },
        body: value.dump(),
    })
}
pub fn set(source: &Json, name: &str, value: Json) -> Result<Json> {
    source.without(name)?.with(name, value)
}
pub fn ensure(value: bool, code: &'static str) -> Result<()> {
    if value { Ok(()) } else { Err(code) }
}
pub fn wait(flag: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::Acquire) {
        ensure(Instant::now() < deadline, "fixture_barrier_timeout")?;
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
pub fn fixture_path() -> PathBuf {
    std::env::var_os("ANSA_AUTH_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/fixtures/auth")
        })
}
pub struct Temp {
    pub root: PathBuf,
    pub auth: PathBuf,
}
impl Temp {
    pub fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "ansa-m3-fixture-{}-{}",
            std::process::id(),
            auth::pkce::random_token(16)?
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|_| "fixture_directory")?;
        write_file(&root.join("fixture-owner"), b"ansa-m3-synthetic-v1\n")?;
        Ok(Self {
            auth: root.join("auth"),
            root,
        })
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
pub fn validate_root(root: &Path) -> Result<()> {
    ensure(
        root.is_absolute()
            && root.parent() == Some(std::env::temp_dir().as_path())
            && root
                .file_name()
                .is_some_and(|n| n.as_encoded_bytes().starts_with(b"ansa-m3-fixture-")),
        "fixture_root",
    )?;
    ansa_linux::auth_store::fixture_owner(root).map_err(|_| "fixture_owner")
}
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "fixture_write")?;
    file.write_all(bytes).map_err(|_| "fixture_write")?;
    file.sync_all().map_err(|_| "fixture_sync")
}
pub fn read_json(path: &Path) -> Result<Json> {
    Json::parse(&fs::read_to_string(path).map_err(|_| "fixture_read")?)
}
pub fn read_events(root: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(root.join("exchanges.log")) {
        Ok(s) => Ok(s.lines().map(str::to_owned).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err("fixture_events"),
    }
}
pub struct Server {
    key: ring::signature::RsaKeyPair,
    pub now: AtomicU64,
    pub nonce: Mutex<String>,
    pub host: Mutex<String>,
    challenge: Mutex<String>,
    redirect: Mutex<String>,
    pub refreshes: AtomicUsize,
    pub grants: AtomicUsize,
    pub catalogs: AtomicUsize,
    pub inference: AtomicUsize,
    pub revokes: AtomicUsize,
    pub permission: AtomicBool,
    pub revoke_failure: AtomicBool,
    pub block_refresh: AtomicBool,
    pub refresh_entered: AtomicBool,
    pub crash_refresh: AtomicBool,
    pub catalog_malformed_tail: AtomicBool,
    pub model_available: AtomicBool,
    pub omit_nonce: AtomicBool,
    pub invalid_nonce: AtomicBool,
    pub invalid_subject: AtomicBool,
    pub exchange_failure: AtomicBool,
    pub browser_bad_first: AtomicBool,
    pub issued: Mutex<Vec<String>>,
    pub revoked: Mutex<Vec<String>>,
    threads: Mutex<Vec<JoinHandle<Result<()>>>>,
    pub root: PathBuf,
}
impl Server {
    pub fn new(root: &Path) -> Result<Arc<Self>> {
        validate_root(root)?;
        let key = ring::signature::RsaKeyPair::from_pkcs8(
            &fs::read(fixture_path().join("fixture-private.der")).map_err(|_| "fixture_key")?,
        )
        .map_err(|_| "fixture_key")?;
        Ok(Arc::new(Self {
            key,
            now: AtomicU64::new(1800000000),
            nonce: Mutex::new(String::new()),
            host: Mutex::new(String::new()),
            challenge: Mutex::new(String::new()),
            redirect: Mutex::new(String::new()),
            refreshes: AtomicUsize::new(0),
            grants: AtomicUsize::new(0),
            catalogs: AtomicUsize::new(0),
            inference: AtomicUsize::new(0),
            revokes: AtomicUsize::new(0),
            permission: AtomicBool::new(true),
            revoke_failure: AtomicBool::new(false),
            block_refresh: AtomicBool::new(false),
            refresh_entered: AtomicBool::new(false),
            crash_refresh: AtomicBool::new(false),
            catalog_malformed_tail: AtomicBool::new(false),
            model_available: AtomicBool::new(false),
            omit_nonce: AtomicBool::new(false),
            invalid_nonce: AtomicBool::new(false),
            invalid_subject: AtomicBool::new(false),
            exchange_failure: AtomicBool::new(false),
            browser_bad_first: AtomicBool::new(false),
            issued: Mutex::new(Vec::new()),
            revoked: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            root: root.to_owned(),
        }))
    }
    pub fn manager(
        self: &Arc<Self>,
        observer: Option<Observer>,
        streaming: Option<StreamExchange>,
    ) -> Result<Arc<AuthManager>> {
        self.manager_at(&self.root.join("auth"), observer, streaming)
    }
    pub fn manager_at(
        self: &Arc<Self>,
        path: &Path,
        observer: Option<Observer>,
        streaming: Option<StreamExchange>,
    ) -> Result<Arc<AuthManager>> {
        let exchange = self.clone();
        let clock = self.clone();
        Ok(Arc::new(AuthManager::new(
            path,
            Arc::new(move |r, c| exchange.exchange(r, c)),
            Arc::new(move || clock.now.load(Ordering::Acquire)),
            observer,
            streaming,
        )?))
    }
    pub fn jwks(&self) -> Result<Json> {
        let components: ring::signature::RsaPublicKeyComponents<Vec<u8>> = self.key.public().into();
        Json::object(vec![(
            "keys",
            Json::array(vec![Json::object(vec![
                ("kty", text("RSA")?),
                ("kid", text("fixture_key")?),
                ("alg", text("RS256")?),
                ("use", text("sig")?),
                ("n", text(&auth::pkce::base64url(&components.n)?)?),
                ("e", text(&auth::pkce::base64url(&components.e)?)?),
            ])?])?,
        )])
    }
    pub fn claims(&self, nonce: &str) -> Result<Json> {
        let now = self.now.load(Ordering::Acquire);
        Json::object(vec![
            ("iss", text(auth::ISSUER)?),
            ("sub", text("fixture_subject")?),
            ("aud", text("fixture_client")?),
            ("iat", number(now)?),
            ("exp", number(now + 3600)?),
            ("nonce", text(nonce)?),
        ])
    }
    pub fn sign(&self, claims: &Json, algorithm: &str) -> Result<String> {
        let header = Json::object(vec![
            ("alg", text(algorithm)?),
            ("kid", text("fixture_key")?),
        ])?;
        let message = format!(
            "{}.{}",
            auth::pkce::base64url(header.dump().as_bytes())?,
            auth::pkce::base64url(claims.dump().as_bytes())?
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                message.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "fixture_sign")?;
        Ok(format!(
            "{}.{}",
            message,
            auth::pkce::base64url(&signature)?
        ))
    }
    fn log(&self, event: &str) -> Result<()> {
        ansa_linux::auth_store::fixture_event(&self.root, event).map_err(|_| "fixture_events")
    }
    pub fn exchange(&self, request: &HttpRequest, cancel: &Cancel) -> Result<HttpReply> {
        use auth::registration::*;
        cancel.check()?;
        if request.url == DISCOVERY {
            return json_reply(Json::object(vec![
                ("issuer", text(auth::ISSUER)?),
                ("authorization_endpoint", text(AUTHORIZE)?),
                ("token_endpoint", text(TOKEN)?),
                ("jwks_uri", text(JWKS)?),
                ("revocation_endpoint", text(REVOKE)?),
            ])?);
        }
        if request.url == JWKS {
            return json_reply(self.jwks()?);
        }
        if request.url == TOKEN {
            let fields = auth::callback::query_fields(request.body.expose())?;
            ensure(
                fields.get("client_id").map(String::as_str) == Some("fixture_client")
                    && fields.get("resource").map(String::as_str) == Some(auth::RESOURCE),
                "fixture_client_resource",
            )?;
            let refresh = fields.get("grant_type").map(String::as_str) == Some("refresh_token");
            if refresh {
                self.refreshes.fetch_add(1, Ordering::AcqRel);
                self.log("refresh")?;
                if self.crash_refresh.load(Ordering::Acquire) {
                    std::process::exit(9);
                }
                ensure(!fields.contains_key("scope"), "fixture_refresh_scope")?;
                self.refresh_entered.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(5);
                while self.block_refresh.load(Ordering::Acquire)
                    && !cancel.cancelled()
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                cancel.check()?;
                let bytes = ansa_linux::auth_store::Directory::open(&self.root.join("auth"))
                    .map_err(|_| "fixture_store")?
                    .read("credentials.json")
                    .map_err(|_| "fixture_store")?
                    .ok_or("fixture_store")?;
                let record =
                    Json::parse(std::str::from_utf8(&bytes).map_err(|_| "fixture_store")?)?;
                ensure(
                    record.field("refresh_pending")?.boolean()?
                        && fields.get("refresh_token").map(String::as_str)
                            == Some(record.text_field("refresh_token")?),
                    "fixture_refresh_intent",
                )?;
                *self.nonce.lock().unwrap() = record.text_field("nonce")?.to_owned();
            } else {
                self.grants.fetch_add(1, Ordering::AcqRel);
                self.log("code")?;
                ensure(
                    fields.get("code").map(String::as_str) == Some("fixture_code"),
                    "fixture_code",
                )?;
                ensure(
                    fields.get("redirect_uri") == Some(&*self.redirect.lock().unwrap()),
                    "fixture_redirect",
                )?;
                ensure(
                    fields.get("code_verifier").is_some_and(|v| {
                        auth::pkce::challenge(v) == *self.challenge.lock().unwrap()
                    }),
                    "fixture_pkce",
                )?;
            }
            if self.exchange_failure.load(Ordering::Acquire) {
                return Err("fixture_exchange_ambiguous");
            }
            let token = format!(
                "fixture_refresh_{}_{}_{}",
                self.grants.load(Ordering::Acquire),
                self.refreshes.load(Ordering::Acquire),
                self.now.load(Ordering::Acquire)
            );
            self.issued.lock().unwrap().push(token.clone());
            let mut claims = self.claims(&self.nonce.lock().unwrap())?;
            if self.omit_nonce.load(Ordering::Acquire) {
                claims = claims.without("nonce")?;
            }
            if self.invalid_nonce.load(Ordering::Acquire) {
                claims = set(&claims, "nonce", text("wrong")?)?;
            }
            if self.invalid_subject.load(Ordering::Acquire) {
                claims = set(&claims, "sub", text("other_subject")?)?;
            }
            let scope = if self.permission.load(Ordering::Acquire) {
                auth::SCOPES
            } else {
                "openid"
            };
            return json_reply(Json::object(vec![
                (
                    "access_token",
                    text(&format!(
                        "fixture_access_{}",
                        self.refreshes.load(Ordering::Acquire)
                    ))?,
                ),
                ("refresh_token", text(&token)?),
                ("id_token", text(&self.sign(&claims, "RS256")?)?),
                ("token_type", text("Bearer")?),
                ("expires_in", number(3600)?),
                (
                    "earliest_refresh_at",
                    number(self.now.load(Ordering::Acquire))?,
                ),
                ("scope", text(scope)?),
            ])?);
        }
        if request.url == REVOKE {
            self.revokes.fetch_add(1, Ordering::AcqRel);
            self.log("revoke")?;
            self.revoked.lock().unwrap().push(
                auth::callback::query_fields(request.body.expose())?
                    .get("token")
                    .ok_or("fixture_revoke")?
                    .clone(),
            );
            return Ok(HttpReply {
                head: Head {
                    status: if self.revoke_failure.load(Ordering::Acquire) {
                        503
                    } else {
                        200
                    },
                    content_types: Vec::new(),
                },
                body: String::new(),
            });
        }
        ensure(
            request
                .bearer
                .as_ref()
                .is_some_and(|s| s.expose().starts_with("fixture_access_")),
            "fixture_bearer",
        )?;
        if request.url == "https://api.openai.com/v1/models" {
            self.catalogs.fetch_add(1, Ordering::AcqRel);
            self.log("models")?;
            return json_reply(Json::parse(
                if !self.model_available.load(Ordering::Acquire) {
                    r#"{"models":[]}"#
                } else if self.catalog_malformed_tail.load(Ordering::Acquire) {
                    r#"{"models":[{"slug":"fixture-model","visibility":"list"},{}]}"#
                } else {
                    r#"{"models":[{"slug":"fixture-model","visibility":"list"}]}"#
                },
            )?);
        }
        if request.url == "https://api.openai.com/v1/responses" {
            self.inference.fetch_add(1, Ordering::AcqRel);
            self.log("inference")?;
            return Ok(HttpReply {
                head: Head {
                    status: 500,
                    content_types: vec!["application/json".into()],
                },
                body: "{}".into(),
            });
        }
        Err("fixture_endpoint")
    }
    pub fn browser(&self, url: &str) -> Result<()> {
        let query = url
            .strip_prefix(auth::registration::AUTHORIZE)
            .and_then(|s| s.strip_prefix('?'))
            .ok_or("fixture_authorize")?;
        let fields = auth::callback::query_fields(query)?;
        ensure(
            fields.get("code_challenge_method").map(String::as_str) == Some("S256")
                && fields.get("scope").map(String::as_str) == Some(auth::SCOPES)
                && fields.get("resource").map(String::as_str) == Some(auth::RESOURCE),
            "fixture_authorization_fields",
        )?;
        *self.nonce.lock().unwrap() = fields.get("nonce").ok_or("fixture_nonce")?.clone();
        *self.challenge.lock().unwrap() = fields
            .get("code_challenge")
            .ok_or("fixture_challenge")?
            .clone();
        *self.redirect.lock().unwrap() = fields
            .get("redirect_uri")
            .ok_or("fixture_redirect")?
            .clone();
        let mut host = self.host.lock().unwrap();
        let new_host = fields.get("ext_agent_host_id").ok_or("fixture_host")?;
        ensure(
            host.is_empty() || host.as_str() == new_host,
            "fixture_host_changed",
        )?;
        *host = new_host.clone();
        drop(host);
        let port = fields
            .get("redirect_uri")
            .and_then(|s| s.strip_prefix("http://127.0.0.1:"))
            .and_then(|s| s.strip_suffix("/auth/callback"))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or("fixture_redirect")?;
        let state = fields.get("state").ok_or("fixture_state")?.clone();
        let bad = self.browser_bad_first.load(Ordering::Acquire);
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::spawn(move || {
                if bad {
                    send_callback(port, &state, true)?;
                }
                send_callback(port, &state, false)
            }));
        Ok(())
    }
    pub fn join_browser(&self) -> Result<()> {
        let threads = std::mem::take(&mut *self.threads.lock().unwrap());
        for thread in threads {
            thread.join().map_err(|_| "fixture_browser_thread")??;
        }
        Ok(())
    }
    pub fn login(&self, manager: &AuthManager) -> Result<()> {
        let cancel = Cancel::default();
        let end = Arc::new(AtomicBool::new(false));
        let (child, done) = (cancel.clone(), end.clone());
        let timer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !done.load(Ordering::Acquire) {
                child.cancel();
            }
        });
        let result = manager.login(|u| self.browser(u), &cancel, false);
        end.store(true, Ordering::Release);
        timer.join().map_err(|_| "fixture_timer")?;
        self.join_browser()?;
        result
    }
}
fn send_callback(port: u16, state: &str, bad: bool) -> Result<()> {
    let mut stream =
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).map_err(|_| "fixture_connect")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|_| "fixture_connect")?;
    let query = auth::pkce::form_encode(&[
        ("state", state),
        ("code", "fixture_code"),
        ("client_id", "fixture_client"),
    ]);
    let extra = if bad {
        "host: attacker.invalid\r\n"
    } else {
        ""
    };
    stream
        .write_all(
            format!("GET /auth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}\r\n")
                .as_bytes(),
        )
        .map_err(|_| "fixture_send")?;
    let mut response = [0; 256];
    let n = stream.read(&mut response).map_err(|_| "fixture_reply")?;
    ensure(
        response[..n].starts_with(if bad {
            b"HTTP/1.1 400"
        } else {
            b"HTTP/1.1 200"
        }),
        "fixture_reply_status",
    )
}

pub const CASES: &[&str] = &[
    "jwt",
    "lifecycle",
    "permission",
    "refresh-crash",
    "pending-cleanup",
    "dispatch-logout",
    "logout-login",
    "logout-refresh",
    "logout-crash",
    "logout-marker",
    "process-refresh",
    "refresh-scope",
    "uncertain-login",
    "held-dispatch",
    "committed-login-crash",
    "execution-logout",
];
pub fn run_case(name: &str) -> Result<Json> {
    match name {
        "jwt" => case_jwt()?,
        "lifecycle" => {
            case_lifecycle()?;
            case_lazy()?;
        }
        "permission" => case_permission()?,
        "refresh-crash" => case_refresh_crash()?,
        "pending-cleanup" => case_pending_cleanup()?,
        "dispatch-logout" => {
            case_dispatch_logout()?;
            case_streaming_logout()?;
        }
        "logout-login" => case_logout_login()?,
        "logout-refresh" => case_logout_refresh()?,
        "logout-crash" => case_logout_crash()?,
        "logout-marker" => case_logout_marker()?,
        "process-refresh" => case_process_refresh()?,
        "refresh-scope" => case_refresh_scope()?,
        "uncertain-login" => case_uncertain_login()?,
        "held-dispatch" => case_held_dispatch()?,
        "committed-login-crash" => case_committed_login_crash()?,
        "execution-logout" => case_execution_logout()?,
        _ => return Err("fixture_unknown_case"),
    }
    Json::object(vec![
        ("passed", Json::parse("true")?),
        ("case", text(name)?),
    ])
}
fn fresh() -> Result<(Temp, Arc<Server>, Arc<AuthManager>)> {
    let temp = Temp::new()?;
    let server = Server::new(&temp.root)?;
    let manager = server.manager(None, None)?;
    Ok((temp, server, manager))
}
fn case_jwt() -> Result<()> {
    let (_temp, s, _m) = fresh()?;
    let good = s.claims("nonce")?;
    let now = s.now.load(Ordering::Acquire);
    let jwks = s.jwks()?;
    let verify = |value: &Json, refresh| {
        auth::claims::verify_id_token(
            &s.sign(value, "RS256")?,
            &jwks,
            "fixture_client",
            "nonce",
            now,
            refresh,
        )
    };
    ensure(
        verify(&good, false)?.text_field("sub")? == "fixture_subject",
        "good_jwt_rejected",
    )?;
    for invalid in [
        set(&good, "iss", text("https://attacker.invalid")?)?,
        set(&good, "aud", text("other")?)?,
        set(&good, "nonce", text("other")?)?,
        set(&good, "exp", number(now)?)?,
        set(&good, "nbf", number(now + 1000)?)?,
        set(&good, "iat", number(now + 1000)?)?,
    ] {
        ensure(verify(&invalid, false).is_err(), "invalid_jwt_accepted")?;
    }
    ensure(
        auth::claims::verify_id_token(
            &s.sign(&good, "none")?,
            &jwks,
            "fixture_client",
            "nonce",
            now,
            false,
        )
        .is_err(),
        "none_algorithm_accepted",
    )?;
    let mut damaged = s.sign(&good, "RS256")?.into_bytes();
    let last = damaged.len() - 1;
    damaged[last] = if damaged[last] == b'A' { b'B' } else { b'A' };
    ensure(
        auth::claims::verify_id_token(
            std::str::from_utf8(&damaged).map_err(|_| "fixture_utf8")?,
            &jwks,
            "fixture_client",
            "nonce",
            now,
            false,
        )
        .is_err(),
        "signature_damage_accepted",
    )?;
    let key = jwks.field("keys")?.elements()?[0].clone();
    let duplicate = Json::object(vec![("keys", Json::array(vec![key.clone(), key])?)])?;
    ensure(
        auth::claims::verify_id_token(
            &s.sign(&good, "RS256")?,
            &duplicate,
            "fixture_client",
            "nonce",
            now,
            false,
        )
        .is_err(),
        "duplicate_kid_accepted",
    )?;
    ensure(
        auth::pkce::challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")
            == "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        "rfc_pkce_oracle",
    )?;
    let single = set(&good, "aud", Json::parse("[\"fixture_client\"]")?)?;
    ensure(verify(&single, false).is_ok(), "single_aud_array_rejected")?;
    let multi = set(&good, "aud", Json::parse("[\"fixture_client\",\"other\"]")?)?;
    ensure(verify(&multi, false).is_err(), "missing_azp_accepted")?;
    ensure(
        verify(&set(&multi, "azp", text("fixture_client")?)?, false).is_ok(),
        "valid_azp_rejected",
    )?;
    ensure(
        verify(
            &set(
                &good,
                "aud",
                Json::parse("[\"fixture_client\",\"fixture_client\"]")?,
            )?,
            false,
        )
        .is_err(),
        "duplicate_aud_accepted",
    )?;
    ensure(
        verify(&good.without("nonce")?, true).is_ok()
            && verify(&good.without("nonce")?, false).is_err(),
        "refresh_nonce_policy",
    )?;
    Ok(())
}
fn case_lifecycle() -> Result<()> {
    let (temp, s, m) = fresh()?;
    ensure(!m.status()?.logged_in, "new_store_signed_in")?;
    s.login(&m)?;
    let first = m.access(&Cancel::default(), false)?;
    ensure(
        m.status()?.logged_in && first.account.len() == 64,
        "login_not_persisted",
    )?;
    drop(m);
    let m = s.manager(None, None)?;
    ensure(
        m.access(&Cancel::default(), false)?.session == first.session,
        "restart_changed_session",
    )?;
    s.now.fetch_add(3550, Ordering::AcqRel);
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let m = m.clone();
        jobs.push(std::thread::spawn(move || {
            m.access(&Cancel::default(), false)
        }));
    }
    for job in jobs {
        job.join().map_err(|_| "refresh_thread")??;
    }
    ensure(
        s.refreshes.load(Ordering::Acquire) == 1
            && m.access(&Cancel::default(), false)?.bearer().expose() != first.bearer().expose(),
        "rotating_refresh_raced",
    )?;
    use std::os::unix::fs::MetadataExt;
    ensure(
        fs::metadata(temp.auth.join("credentials.json"))
            .map_err(|_| "fixture_metadata")?
            .mode()
            & 0o077
            == 0,
        "credentials_exposed",
    )?;
    let debug = format!("{first:?}");
    ensure(
        !debug.contains("fixture_access_"),
        "lease_debug_exposed_bearer",
    )?;
    s.revoke_failure.store(true, Ordering::Release);
    ensure(
        !m.logout(&Cancel::default())?
            && !m.status()?.logged_in
            && !temp.auth.join("credentials.json").exists(),
        "logout_retained_credentials",
    )?;
    ensure(
        !m.logout(&Cancel::default())? && m.access(&Cancel::default(), false).is_err(),
        "unconfirmed_revocation_confirmed",
    )?;
    s.login(&m)?;
    ensure(
        m.status()?.logged_in && m.access(&Cancel::default(), false)?.session != first.session,
        "reauthorization_failed",
    )
}
fn case_lazy() -> Result<()> {
    let (_temp, s, m) = fresh()?;
    s.login(&m)?;
    let original = m.status()?;
    s.now.fetch_add(3601, Ordering::AcqRel);
    let expired = m.status()?;
    ensure(
        expired.logged_in
            && expired.expires_in == 0
            && expired.session == original.session
            && s.refreshes.load(Ordering::Acquire) == 0
            && s.catalogs.load(Ordering::Acquire) == 0,
        "status_performed_network",
    )?;
    let mut guard =
        auth::dispatch::Authorization::new(m.clone(), &expired, "fixture-model".into())?;
    ensure(
        guard.check_dispatch(0, &Cancel::default()).is_err(),
        "missing_model_allowed",
    )?;
    ensure(
        s.refreshes.load(Ordering::Acquire) == 1
            && s.catalogs.load(Ordering::Acquire) == 1
            && s.inference.load(Ordering::Acquire) == 0,
        "missing_model_not_closed",
    )?;
    s.model_available.store(true, Ordering::Release);
    s.catalog_malformed_tail.store(true, Ordering::Release);
    guard.check_dispatch(0, &Cancel::default())?;
    guard.check_dispatch(0, &Cancel::default())?;
    ensure(
        s.catalogs.load(Ordering::Acquire) == 2 && guard.session() == original.session,
        "catalog_cache_or_short_circuit",
    )?;
    let mut wrong = expired.clone();
    wrong.account = "other".into();
    let mut mismatch =
        auth::dispatch::Authorization::new(m.clone(), &wrong, "fixture-model".into())?;
    ensure(
        mismatch.check_dispatch(0, &Cancel::default()).is_err(),
        "wrong_account_allowed",
    )?;
    m.logout(&Cancel::default())?;
    ensure(
        guard.check_dispatch(0, &Cancel::default()).is_err(),
        "logout_dispatch_allowed",
    )?;
    s.login(&m)?;
    ensure(
        guard.check_dispatch(0, &Cancel::default()).is_err(),
        "session_changed_without_resume",
    )?;
    guard.check_dispatch(1, &Cancel::default())?;
    ensure(
        guard.session() != original.session && s.catalogs.load(Ordering::Acquire) == 3,
        "resume_failed_revalidation",
    )
}
fn case_permission() -> Result<()> {
    let (_temp, s, m) = fresh()?;
    s.permission.store(false, Ordering::Release);
    s.login(&m)?;
    ensure(
        m.status()?.logged_in
            && !m.status()?.plan_enabled
            && m.access(&Cancel::default(), false).is_err()
            && s.inference.load(Ordering::Acquire) == 0,
        "identity_only_grant_dispatch",
    )?;
    case_file_guards()
}
fn case_file_guards() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    let credentials = temp.auth.join("credentials.json");
    let alias = temp.root.join("alias");
    let held = temp.root.join("held.json");
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o644))
        .map_err(|_| "fixture_chmod")?;
    ensure(m.status().is_err(), "public_credentials_accepted")?;
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600))
        .map_err(|_| "fixture_chmod")?;
    fs::hard_link(&credentials, &alias).map_err(|_| "fixture_hardlink")?;
    ensure(m.status().is_err(), "hardlinked_credentials_accepted")?;
    fs::remove_file(&alias).map_err(|_| "fixture_unlink")?;
    fs::rename(&credentials, &held).map_err(|_| "fixture_rename")?;
    symlink(&held, &credentials).map_err(|_| "fixture_symlink")?;
    ensure(m.status().is_err(), "symlink_credentials_accepted")?;
    fs::remove_file(&credentials).map_err(|_| "fixture_unlink")?;
    fs::rename(&held, &credentials).map_err(|_| "fixture_rename")?;
    fs::remove_file(&credentials).map_err(|_| "fixture_unlink")?;
    ensure(
        std::process::Command::new("mkfifo")
            .arg(&credentials)
            .status()
            .map_err(|_| "fixture_fifo")?
            .success(),
        "fixture_fifo",
    )?;
    let before = Instant::now();
    ensure(
        m.status().is_err() && before.elapsed() < Duration::from_secs(2),
        "fifo_credentials_blocked",
    )?;
    fs::remove_file(&credentials).map_err(|_| "fixture_unlink")?;
    let marker = temp.auth.join("signed-out");
    ensure(
        std::process::Command::new("mkfifo")
            .arg(&marker)
            .status()
            .map_err(|_| "fixture_fifo")?
            .success(),
        "fixture_fifo",
    )?;
    let before = Instant::now();
    ensure(
        m.logout(&Cancel::default()).is_err() && before.elapsed() < Duration::from_secs(2),
        "fifo_logout_blocked",
    )?;
    fs::remove_file(&marker).map_err(|_| "fixture_unlink")?;
    fs::set_permissions(&temp.auth, fs::Permissions::from_mode(0o777))
        .map_err(|_| "fixture_chmod")?;
    ensure(s.manager(None, None).is_err(), "public_directory_accepted")?;
    fs::set_permissions(&temp.auth, fs::Permissions::from_mode(0o700))
        .map_err(|_| "fixture_chmod")?;
    ensure(
        ansa_linux::auth_store::Directory::open(Path::new("/tmp")).is_err(),
        "unowned_directory_accepted",
    )?;
    Ok(())
}
pub fn request(account: &str) -> Result<ansa::provider::request::Request> {
    use ansa::provider::request::*;
    Request::new(
        Binding {
            provider: "openai-siwc".into(),
            account: account.into(),
            requested_model: "gpt-6.1-sol".into(),
            returned_model: "gpt-6.1-sol".into(),
        },
        Epoch {
            id: "fixture".into(),
            instructions: "fixture".into(),
            exact_state: Json::parse("{}")?,
        },
        Advertisement {
            revision: "empty".into(),
            descriptors: Vec::new(),
        },
        Vec::new(),
    )
}
fn case_dispatch_logout() -> Result<()> {
    let temp = Temp::new()?;
    let s = Server::new(&temp.root)?;
    let waiting = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (w, r) = (waiting.clone(), release.clone());
    let m = s.manager(
        Some(Arc::new(move |point, _| {
            if point == auth::store_v1::StorePoint::BeforeDispatch {
                w.store(true, Ordering::Release);
                wait(&r)?;
            }
            Ok(())
        })),
        None,
    )?;
    s.login(&m)?;
    let lease = m.access(&Cancel::default(), false)?;
    let req = request(&lease.account)?;
    let child = m.clone();
    let job =
        std::thread::spawn(move || child.generate(req, &Cancel::default(), "", None).is_err());
    wait(&waiting)?;
    ensure(m.logout(&Cancel::default())?, "dispatch_logout_failed")?;
    release.store(true, Ordering::Release);
    ensure(
        job.join().map_err(|_| "dispatch_thread")? && s.inference.load(Ordering::Acquire) == 0,
        "request_after_completed_logout",
    )
}
fn case_streaming_logout() -> Result<()> {
    let temp = Temp::new()?;
    let s = Server::new(&temp.root)?;
    let delivered = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let preview = Arc::new(AtomicBool::new(false));
    let (d, t) = (delivered.clone(), stopped.clone());
    let streaming: StreamExchange = Arc::new(move |_, _, sink, cancel| {
        for event in [
            r#"{"type":"response.created","sequence_number":0,"response":{"id":"r","model":"gpt-6.1-sol","status":"in_progress","output":[]}}"#,
            r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"m","role":"assistant","status":"in_progress","content":[]}}"#,
            r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"m","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
            r#"{"type":"response.output_text.delta","sequence_number":3,"item_id":"m","output_index":0,"content_index":0,"delta":"hi"}"#,
        ] {
            sink(format!("data: {event}\n\n").as_bytes())?;
        }
        d.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cancel.cancelled() {
            ensure(Instant::now() < deadline, "stream_cancel_timeout")?;
            std::thread::sleep(Duration::from_millis(1));
        }
        t.store(true, Ordering::Release);
        Ok(())
    });
    let m = s.manager(None, Some(streaming))?;
    s.login(&m)?;
    let lease = m.access(&Cancel::default(), false)?;
    let req = request(&lease.account)?;
    let (child, p) = (m.clone(), preview.clone());
    let job = std::thread::spawn(move || {
        child
            .generate(
                req,
                &Cancel::default(),
                "",
                Some(Box::new(move |text| {
                    p.store(text == "hi", Ordering::Release)
                })),
            )
            .is_err()
    });
    wait(&delivered)?;
    ensure(preview.load(Ordering::Acquire), "stream_preview_missing")?;
    ensure(m.logout(&Cancel::default())?, "stream_logout_failed")?;
    ensure(
        job.join().map_err(|_| "stream_thread")? && stopped.load(Ordering::Acquire),
        "logout_failed_cancel_stream",
    )
}
fn case_logout_login() -> Result<()> {
    let (_temp, s, m) = fresh()?;
    let listening = Arc::new(AtomicBool::new(false));
    let flag = listening.clone();
    let child = m.clone();
    let job = std::thread::spawn(move || {
        child.login(
            |_| {
                flag.store(true, Ordering::Release);
                Ok(())
            },
            &Cancel::default(),
            false,
        )
    });
    wait(&listening)?;
    let other = s.manager(None, None)?;
    ensure(
        other.logout(&Cancel::default())?,
        "new_logout_client_blocked",
    )?;
    ensure(
        job.join().map_err(|_| "login_thread")? == Err("auth_cancelled") && !m.status()?.logged_in,
        "logout_failed_cancel_login",
    )
}
fn case_logout_refresh() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    s.block_refresh.store(true, Ordering::Release);
    let child = m.clone();
    let job = std::thread::spawn(move || child.access(&Cancel::default(), true));
    wait(&s.refresh_entered)?;
    let other = s.manager(None, None)?;
    ensure(
        !other.logout(&Cancel::default())?,
        "ambiguous_refresh_revoked",
    )?;
    ensure(
        matches!(
            job.join().map_err(|_| "refresh_thread")?,
            Err("auth_cancelled")
        ) && !other.status()?.logged_in
            && !temp.auth.join("credentials.json").exists(),
        "logout_retained_refresh",
    )
}
fn case_refresh_scope() -> Result<()> {
    let (_temp, s, m) = fresh()?;
    s.login(&m)?;
    s.permission.store(false, Ordering::Release);
    ensure(
        m.access(&Cancel::default(), true).is_err(),
        "refresh_scope_loss_allowed",
    )?;
    ensure(
        m.status()?.logged_in
            && !m.status()?.plan_enabled
            && m.models(&Cancel::default()).is_err()
            && s.inference.load(Ordering::Acquire) == 0,
        "scope_loss_not_retained",
    )
}
fn wait_marker(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        ensure(Instant::now() < deadline, "logout_marker_timeout")?;
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
fn case_held_dispatch() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    let lease = m.access(&Cancel::default(), false)?;
    let hold = m.hold_session(&lease.session, &Cancel::default())?;
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let other = s.manager(None, None)?;
    let job = std::thread::spawn(move || {
        let result = other.logout(&Cancel::default());
        flag.store(true, Ordering::Release);
        result
    });
    wait_marker(&temp.auth.join("signed-out"))?;
    ensure(!done.load(Ordering::Acquire), "logout_bypassed_lease")?;
    drop(hold);
    job.join().map_err(|_| "logout_thread")??;
    ensure(
        done.load(Ordering::Acquire) && m.hold_session(&lease.session, &Cancel::default()).is_err(),
        "lease_release_failed",
    )
}
fn case_execution_logout() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    let session = m.access(&Cancel::default(), false)?.session;
    let parent = Cancel::default();
    let execution = m.hold_execution(&session, &parent)?;
    parent.cancel();
    ensure(
        execution.cancel().cancelled(),
        "parent_cancel_not_immediate",
    )?;
    drop(execution);
    let execution = m.hold_execution(&session, &Cancel::default())?;
    drop(m);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let other = s.manager(None, None)?;
    let job = std::thread::spawn(move || {
        let result = other.logout(&Cancel::default());
        flag.store(true, Ordering::Release);
        result
    });
    wait_marker(&temp.auth.join("signed-out"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !execution.cancel().cancelled() {
        ensure(Instant::now() < deadline, "execution_cancel_timeout")?;
        std::thread::sleep(Duration::from_millis(1));
    }
    ensure(
        !done.load(Ordering::Acquire),
        "logout_bypassed_execution_lease",
    )?;
    drop(execution);
    job.join().map_err(|_| "execution_logout_thread")??;
    ensure(
        done.load(Ordering::Acquire),
        "logout_blocked_after_execution_release",
    )
}

// Real child termination tests leave kernel-visible state for the next process.
pub fn helper(root: &Path, action: &str, now: u64) -> Result<()> {
    validate_root(root)?;
    let s = Server::new(root)?;
    s.now.store(now, Ordering::Release);
    let action_owned = action.to_owned();
    let observer: Observer = Arc::new(move |point, name| {
        use auth::store_v1::StorePoint::*;
        let code = match (action_owned.as_str(), point, name) {
            ("pending-crash" | "login-uncertain", TemporarySynced, "credentials.json") => Some(8),
            ("logout-crash", TokensRemoved, _) => Some(7),
            ("logout-marker", Replaced, "registration.json") => Some(6),
            ("login-committed", Replaced, "credentials.json") => Some(8),
            _ => None,
        };
        if let Some(code) = code {
            std::process::exit(code);
        }
        Ok(())
    });
    let m = s.manager(Some(observer), None)?;
    match action {
        "refresh-exchange-crash" => {
            s.crash_refresh.store(true, Ordering::Release);
            m.access(&Cancel::default(), true)?;
        }
        "pending-crash" => {
            m.access(&Cancel::default(), true)?;
        }
        "access" => {
            m.access(&Cancel::default(), false)?;
        }
        "logout-crash" | "logout-marker" => {
            m.logout(&Cancel::default())?;
        }
        "login-uncertain" | "login-committed" => s.login(&m)?,
        _ => return Err("fixture_helper_action"),
    }
    Ok(())
}
fn spawn_helper(root: &Path, action: &str, now: u64) -> Result<std::process::Child> {
    let executable = std::env::current_exe().map_err(|_| "fixture_executable")?;
    let mut command = std::process::Command::new(&executable);
    if executable
        .file_name()
        .is_some_and(|n| n.as_encoded_bytes().starts_with(b"auth_adapter"))
    {
        command
            .arg("--fixture-child")
            .arg(root)
            .arg(action)
            .arg(now.to_string());
    } else {
        command
            .args(["--exact", "process_refresh", "--nocapture"])
            .env("ANSA_M3_FIXTURE_ROOT", root)
            .env("ANSA_M3_FIXTURE_ACTION", action)
            .env("ANSA_M3_FIXTURE_NOW", now.to_string());
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().map_err(|_| "fixture_spawn")
}
fn wait_child(mut child: std::process::Child, code: i32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().map_err(|_| "fixture_wait")? {
            return ensure(status.code() == Some(code), "fixture_child_exit");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("fixture_child_timeout");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn crash(root: &Path, action: &str, now: u64, code: i32) -> Result<()> {
    wait_child(spawn_helper(root, action, now)?, code)
}
fn case_refresh_crash() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    crash(
        &temp.root,
        "refresh-exchange-crash",
        s.now.load(Ordering::Acquire),
        9,
    )?;
    ensure(
        m.status()?.refresh_pending
            && !m.status()?.logged_in
            && m.access(&Cancel::default(), false).is_err(),
        "ambiguous_refresh_reused",
    )?;
    ensure(
        s.refreshes.load(Ordering::Acquire) == 0
            && read_events(&temp.root)?
                .iter()
                .filter(|v| v.as_str() == "refresh")
                .count()
                == 1,
        "ambiguous_refresh_retried",
    )?;
    s.login(&m)?;
    ensure(
        !m.logout(&Cancel::default())?,
        "reauthorization_concealed_unknown_refresh",
    )
}
fn case_pending_cleanup() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    crash(
        &temp.root,
        "pending-crash",
        s.now.load(Ordering::Acquire),
        8,
    )?;
    m.status()?;
    for entry in fs::read_dir(&temp.auth).map_err(|_| "fixture_entries")? {
        ensure(
            !entry
                .map_err(|_| "fixture_entries")?
                .file_name()
                .as_encoded_bytes()
                .starts_with(b"pending-"),
            "crash_copy_retained",
        )?;
    }
    ensure(m.logout(&Cancel::default())?, "cleanup_logout_failed")
}
fn case_logout_crash() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    crash(&temp.root, "logout-crash", s.now.load(Ordering::Acquire), 7)?;
    ensure(
        !m.logout(&Cancel::default())? && s.revokes.load(Ordering::Acquire) == 0,
        "revoke_confirmed_without_request",
    )
}
fn case_logout_marker() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    crash(
        &temp.root,
        "logout-marker",
        s.now.load(Ordering::Acquire),
        6,
    )?;
    ensure(
        m.login(
            |_| Err("fixture_browser_failure"),
            &Cancel::default(),
            false,
        )
        .is_err(),
        "failed_login_succeeded",
    )?;
    ensure(
        !m.status()?.logged_in && m.access(&Cancel::default(), false).is_err(),
        "failed_login_revived_tokens",
    )
}
fn case_process_refresh() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    let session = m.access(&Cancel::default(), false)?.session;
    let now = s.now.fetch_add(3550, Ordering::AcqRel) + 3550;
    let one = spawn_helper(&temp.root, "access", now)?;
    let two = spawn_helper(&temp.root, "access", now)?;
    wait_child(one, 0)?;
    wait_child(two, 0)?;
    ensure(
        read_events(&temp.root)?
            .iter()
            .filter(|v| v.as_str() == "refresh")
            .count()
            == 1
            && m.access(&Cancel::default(), false)?.session == session,
        "processes_raced_refresh",
    )
}
fn case_uncertain_login() -> Result<()> {
    let (temp, s, m) = fresh()?;
    s.login(&m)?;
    crash(
        &temp.root,
        "login-uncertain",
        s.now.load(Ordering::Acquire),
        8,
    )?;
    s.login(&m)?;
    ensure(
        !m.logout(&Cancel::default())?,
        "unknown_login_fully_revoked",
    )?;
    let (_active, server, current) = fresh()?;
    server.login(&current)?;
    server.login(&current)?;
    let issued = server.issued.lock().unwrap().clone();
    ensure(
        issued.len() == 2 && issued[0] != issued[1],
        "fixture_grants_not_distinct",
    )?;
    ensure(
        !current.logout(&Cancel::default())?,
        "replacement_revoke_confirmed_old_grant",
    )?;
    ensure(
        server.revoked.lock().unwrap().as_slice() == [issued[1].clone()],
        "logout_wrong_grant",
    )?;
    drop(current);
    let reopened = server.manager(None, None)?;
    ensure(
        !reopened.status()?.logged_in && !reopened.logout(&Cancel::default())?,
        "restart_lost_uncertainty",
    )
}
fn case_committed_login_crash() -> Result<()> {
    let (temp, s, m) = fresh()?;
    crash(
        &temp.root,
        "login-committed",
        s.now.load(Ordering::Acquire),
        8,
    )?;
    ensure(
        m.status()?.logged_in && !m.access(&Cancel::default(), false)?.session.is_empty(),
        "committed_login_lost_binding",
    )
}
