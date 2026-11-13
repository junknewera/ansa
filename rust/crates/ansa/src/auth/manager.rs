use super::{Cancel, ISSUER, RESOURCE, Result, SCOPES, Secret, boolean, number};
use super::{
    callback::Listener,
    claims::verify_id_token,
    pkce, registration as endpoint,
    store_v1::{Lock, Observer, Store, StorePoint},
};
use crate::{
    protocol::{json::Json, legacy_json_v1::sha256, sse::Limits},
    provider::{
        request::Request,
        responses::{AdmittedResponse, ResponseStream, TextPreview},
    },
    transport::http::{Head, RESPONSES_ENDPOINT, admit_request},
};
use std::{path::Path, sync::Arc, thread::JoinHandle, time::Duration};

#[derive(Debug)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: &'static str,
    pub content_type: &'static str,
    pub body: Secret,
    pub bearer: Option<Secret>,
    pub body_limit: usize,
    pub timeout_ms: u64,
}
pub struct HttpReply {
    pub head: Head,
    pub body: String,
}
pub type Exchange = Arc<dyn Fn(&HttpRequest, &Cancel) -> Result<HttpReply> + Send + Sync>;
pub type StreamExchange = Arc<
    dyn Fn(&Json, &Secret, &mut dyn FnMut(&[u8]) -> Result<()>, &Cancel) -> Result<()>
        + Send
        + Sync,
>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
struct TokenBinding<'a> {
    client: &'a str,
    nonce: &'a str,
    refresh: bool,
    session: &'a str,
}

#[derive(Clone, Debug)]
pub struct Lease {
    bearer: Secret,
    pub account: String,
    pub session: String,
    pub expires_at: u64,
}
impl Lease {
    pub fn bearer(&self) -> &Secret {
        &self.bearer
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub logged_in: bool,
    pub refresh_pending: bool,
    pub plan_enabled: bool,
    pub account: String,
    pub expires_in: u64,
    pub refresh_wait: u64,
    pub session: String,
}
pub struct AuthManager {
    store: Store,
    exchange: Exchange,
    streaming: Option<StreamExchange>,
    clock: Clock,
}

struct Watch {
    operation: Cancel,
    done: Cancel,
    join: Option<JoinHandle<()>>,
}
impl Watch {
    fn new(store: Store, parent: &Cancel) -> Result<Self> {
        let operation = Cancel::linked(parent);
        let done = Cancel::default();
        let (child, finish) = (operation.clone(), done.clone());
        let join = std::thread::Builder::new()
            .name("ansa-auth-logout".into())
            .spawn(move || {
                while !finish.cancelled() {
                    if store.signed_out().unwrap_or(true) {
                        child.cancel();
                        break;
                    }
                    finish.wait(Duration::from_millis(25));
                }
            })
            .map_err(|_| "auth_logout_watcher")?;
        Ok(Self {
            operation,
            done,
            join: Some(join),
        })
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.done.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
/// Owns its descriptor-backed lock and watcher independently of AuthManager.
pub struct ExecutionLease {
    watch: Watch,
    _lock: Lock,
}
impl ExecutionLease {
    pub fn cancel(&self) -> &Cancel {
        &self.watch.operation
    }
}

fn text(value: &str) -> Result<Json> {
    Json::string(value)
}
fn set_last(value: &Json, key: &str, replacement: Json) -> Result<Json> {
    value.without(key)?.with(key, replacement)
}
pub fn account_id(record: &Json) -> Result<String> {
    Ok(sha256(
        Json::array(vec![
            text(ISSUER)?,
            record.field("client_id")?.clone(),
            record.field("subject")?.clone(),
        ])?
        .dump()
        .as_bytes(),
    ))
}
pub fn plan_enabled(scope: &str) -> bool {
    let fields: std::collections::HashSet<_> = scope.split_ascii_whitespace().collect();
    [
        "offline_access",
        "resource.invoke",
        "chatgpt.tokens.use.direct",
    ]
    .iter()
    .all(|v| fields.contains(v))
}
fn truth(value: &Json, key: &str, wanted: bool) -> Result<bool> {
    Ok(value.has(key) && value.field(key)? == &boolean(wanted))
}
pub fn json_reply(reply: HttpReply) -> Result<Json> {
    if reply.head.status != 200 || reply.head.content_types.len() != 1 {
        return Err("auth_http_response");
    }
    let mime = reply.head.content_types[0].to_ascii_lowercase();
    if ![
        "application/json",
        "application/json; charset=utf-8",
        "application/json;charset=utf-8",
    ]
    .contains(&mime.as_str())
    {
        return Err("auth_http_mime");
    }
    Json::parse_with_limits(&reply.body, 2 * 1024 * 1024, 64, 131072)
}
pub fn timestamp(value: &Json) -> Result<u64> {
    if value.is_integer() {
        return Ok(value.index()? as u64);
    }
    let s = value.text()?.as_bytes();
    if s.len() < 20
        || s.last() != Some(&b'Z')
        || (s.len() != 20 && (s[19] != b'.' || !s[20..s.len() - 1].iter().all(u8::is_ascii_digit)))
    {
        return Err("auth_refresh_time");
    }
    if [4, 7, 10, 13, 16].iter().any(|&i| {
        s[i] != match i {
            4 | 7 => b'-',
            10 => b'T',
            _ => b':',
        }
    }) {
        return Err("auth_refresh_time");
    }
    let n = |start, end| -> Result<u32> {
        std::str::from_utf8(&s[start..end])
            .map_err(|_| "auth_refresh_time")?
            .parse()
            .map_err(|_| "auth_refresh_time")
    };
    let (year, month, day, hour, minute, second) = (
        n(0, 4)?,
        n(5, 7)?,
        n(8, 10)?,
        n(11, 13)?,
        n(14, 16)?,
        n(17, 19)?,
    );
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return Err("auth_refresh_time");
    }
    let date = chrono::NaiveDate::from_ymd_opt(year as i32, month, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .ok_or("auth_refresh_time")?;
    // C++ timegm normalizes day-of-month and leap seconds after get_time.
    let seconds = date.and_utc().timestamp()
        + i64::from((day - 1) * 86400 + hour * 3600 + minute * 60 + second);
    if seconds <= 0 {
        return Err("auth_refresh_time");
    }
    Ok(seconds as u64)
}
impl AuthManager {
    pub fn new(
        path: &Path,
        exchange: Exchange,
        clock: Clock,
        observer: Option<Observer>,
        streaming: Option<StreamExchange>,
    ) -> Result<Self> {
        let path = std::path::absolute(path).map_err(|_| "auth_directory_path")?;
        Ok(Self {
            store: Store::open(&path, observer)?,
            exchange,
            clock,
            streaming,
        })
    }
    fn request(
        &self,
        method: &'static str,
        url: &'static str,
        fields: &[(&str, &str)],
        cancel: &Cancel,
    ) -> Result<HttpReply> {
        cancel.check()?;
        (self.exchange)(
            &HttpRequest {
                method,
                url,
                body: Secret::new(pkce::form_encode(fields)),
                content_type: if method == "POST" {
                    "application/x-www-form-urlencoded"
                } else {
                    ""
                },
                bearer: None,
                body_limit: 2 * 1024 * 1024,
                timeout_ms: 20000,
            },
            cancel,
        )
    }
    fn discover(&self, cancel: &Cancel) -> Result<()> {
        endpoint::discover(&json_reply(self.request(
            "GET",
            endpoint::DISCOVERY,
            &[],
            cancel,
        )?)?)
    }
    fn registration(&self) -> Result<Json> {
        let record = self
            .store
            .read("registration.json")?
            .ok_or("auth_registration")?;
        if record.usize_field("version")? != 1 || record.text_field("host_id")?.is_empty() {
            return Err("auth_registration");
        }
        Ok(record)
    }
    fn revocation_status(&self, confirmed: bool) -> Result<()> {
        self.store.save(
            "registration.json",
            &set_last(
                &self.registration()?,
                "last_revocation_confirmed",
                boolean(confirmed),
            )?,
        )
    }
    fn validate_tokens(
        &self,
        response: Json,
        registration: &Json,
        binding: TokenBinding<'_>,
        cancel: &Cancel,
    ) -> Result<Json> {
        let TokenBinding {
            client,
            nonce,
            refresh,
            session,
        } = binding;
        let granted = response.text_field("scope")?;
        if !granted.split_ascii_whitespace().any(|v| v == "openid") {
            return Err("auth_identity_permission");
        }
        let now = (self.clock)();
        let expiry = response.usize_field("expires_in")? as u64;
        if response.text_field("token_type")? != "Bearer"
            || !(1..=86400).contains(&expiry)
            || ["access_token", "refresh_token"]
                .iter()
                .any(|k| match response.text_field(k) {
                    Ok(v) => v.is_empty() || v.len() > 16384,
                    Err(_) => true,
                })
        {
            return Err("auth_token_response");
        }
        let verify = || {
            verify_id_token(
                response.text_field("id_token")?,
                &json_reply(self.request("GET", endpoint::JWKS, &[], cancel)?)?,
                client,
                nonce,
                now,
                refresh,
            )
        };
        let claims = match verify() {
            Err("auth_unknown_key") => verify()?,
            other => other?,
        };
        if registration.has("subject")
            && claims.text_field("sub")? != registration.text_field("subject")?
        {
            return Err("auth_account_mismatch");
        }
        let earliest = if response.has("earliest_refresh_at") {
            timestamp(response.field("earliest_refresh_at")?)?
        } else {
            now
        };
        Json::object(vec![
            ("version", number(1)),
            ("client_id", text(client)?),
            ("host_id", registration.field("host_id")?.clone()),
            ("subject", claims.field("sub")?.clone()),
            ("nonce", text(nonce)?),
            ("session", text(session)?),
            ("id_token", response.field("id_token")?.clone()),
            ("access_token", response.field("access_token")?.clone()),
            ("refresh_token", response.field("refresh_token")?.clone()),
            ("scope", response.field("scope")?.clone()),
            (
                "expires_at",
                number(now.checked_add(expiry).ok_or("auth_token_response")?),
            ),
            ("earliest_refresh_at", number(earliest)),
            ("refresh_pending", boolean(false)),
        ])
    }
    pub fn login(
        &self,
        browser: impl FnOnce(&str) -> Result<()>,
        cancel: &Cancel,
        consent: bool,
    ) -> Result<()> {
        let _lock = self.store.lock(cancel)?;
        self.store.clean()?;
        let previous = self.store.read("credentials.json")?;
        if self.store.read("registration.json")?.is_none() {
            self.store.save(
                "registration.json",
                &Json::object(vec![
                    ("version", number(1)),
                    ("host_id", text(&pkce::host_id()?)?),
                ])?,
            )?;
        }
        if self.store.signed_out()? && self.store.read("credentials.json")?.is_some() {
            self.revocation_status(false)?;
            self.store.remove("credentials.json")?;
        }
        self.store.remove("signed-out")?;
        let watch = Watch::new(self.store.clone(), cancel)?;
        let cancel = &watch.operation;
        let saved = self.registration()?;
        self.discover(cancel)?;
        let (state, nonce, verifier) = (
            pkce::random_token(32)?,
            pkce::random_token(32)?,
            pkce::random_token(32)?,
        );
        let listener = Listener::bind()?;
        let redirect = format!("http://127.0.0.1:{}/auth/callback", listener.port()?);
        let mut client = if saved.has("client_id") {
            saved.text_field("client_id")?.to_owned()
        } else {
            "dynamic_agent_client".into()
        };
        let challenge = pkce::challenge(&verifier);
        let mut fields = vec![
            ("client_id", client.as_str()),
            ("ext_agent_host_id", saved.text_field("host_id")?),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
        ];
        if client == "dynamic_agent_client" {
            fields.push(("agent_name_hint", "ansa"));
        }
        if consent {
            fields.push(("prompt", "consent"));
        }
        browser(&format!(
            "{}?{}",
            endpoint::AUTHORIZE,
            pkce::form_encode(&fields)
        ))?;
        let returned = listener.receive(&state, cancel)?;
        if client == "dynamic_agent_client" {
            client = returned
                .get("client_id")
                .filter(|v| !v.is_empty())
                .ok_or("auth_registration_incomplete")?
                .clone();
        } else if returned.get("client_id").is_some_and(|v| v != &client) {
            return Err("auth_client_mismatch");
        }
        if client == "dynamic_agent_client" || client.len() > 256 {
            return Err("auth_client_id");
        }
        let uncertain = previous.is_some()
            || truth(&saved, "last_revocation_confirmed", false)?
            || truth(&saved, "exchange_pending", true)?
            || truth(&saved, "revocation_uncertain", true)?;
        let mut registration = Json::object(vec![
            ("version", number(1)),
            ("host_id", saved.field("host_id")?.clone()),
            ("client_id", text(&client)?),
            ("revocation_uncertain", boolean(uncertain)),
            ("exchange_pending", boolean(true)),
        ])?;
        if saved.has("subject") {
            registration = registration.with("subject", saved.field("subject")?.clone())?;
        }
        self.store.save("registration.json", &registration)?;
        let reply = self.request(
            "POST",
            endpoint::TOKEN,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &client),
                ("code", &returned["code"]),
                ("code_verifier", &verifier),
                ("redirect_uri", &redirect),
                ("resource", RESOURCE),
            ],
            cancel,
        )?;
        let record = self.validate_tokens(
            json_reply(reply)?,
            &saved,
            TokenBinding {
                client: &client,
                nonce: &nonce,
                refresh: false,
                session: &pkce::random_token(32)?,
            },
            cancel,
        )?;
        if !saved.has("subject") {
            registration = registration.with("subject", record.field("subject")?.clone())?;
        }
        self.store.save("registration.json", &registration)?;
        self.store.save("credentials.json", &record)?;
        self.store.save(
            "registration.json",
            &Json::object(vec![
                ("version", number(1)),
                ("host_id", saved.field("host_id")?.clone()),
                ("client_id", text(&client)?),
                ("subject", record.field("subject")?.clone()),
                ("revocation_uncertain", boolean(uncertain)),
                ("exchange_pending", boolean(false)),
            ])?,
        )
    }
    pub fn access(&self, cancel: &Cancel, force_refresh: bool) -> Result<Lease> {
        let _lock = self.store.lock(cancel)?;
        self.store.clean()?;
        if self.store.signed_out()? {
            return Err("auth_required");
        }
        let watch = Watch::new(self.store.clone(), cancel)?;
        let cancel = &watch.operation;
        let mut record = self
            .store
            .read("credentials.json")?
            .ok_or("auth_required")?;
        if record.usize_field("version")? != 1
            || record.field("refresh_pending")? != &boolean(false)
        {
            return Err("auth_required");
        }
        if !plan_enabled(record.text_field("scope")?) {
            return Err("auth_plan_permission");
        }
        let saved = self.registration()?;
        for field in ["client_id", "subject", "host_id"] {
            if record.text_field(field)? != saved.text_field(field)? {
                return Err("auth_account_mismatch");
            }
        }
        let now = (self.clock)();
        if force_refresh || record.usize_field("expires_at")? as u64 <= now.saturating_add(60) {
            if now < record.usize_field("earliest_refresh_at")? as u64 {
                return Err("auth_refresh_not_ready");
            }
            self.discover(cancel)?;
            self.store.save(
                "credentials.json",
                &record.with("refresh_pending", boolean(true))?,
            )?;
            let reply = self.request(
                "POST",
                endpoint::TOKEN,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", record.text_field("client_id")?),
                    ("refresh_token", record.text_field("refresh_token")?),
                    ("resource", RESOURCE),
                ],
                cancel,
            )?;
            record = self.validate_tokens(
                json_reply(reply)?,
                &saved,
                TokenBinding {
                    client: saved.text_field("client_id")?,
                    nonce: record.text_field("nonce")?,
                    refresh: true,
                    session: record.text_field("session")?,
                },
                cancel,
            )?;
            self.store.save("credentials.json", &record)?;
        }
        if !plan_enabled(record.text_field("scope")?) {
            return Err("auth_plan_permission");
        }
        Ok(Lease {
            bearer: Secret::new(record.text_field("access_token")?.to_owned()),
            account: account_id(&record)?,
            session: record.text_field("session")?.to_owned(),
            expires_at: record.usize_field("expires_at")? as u64,
        })
    }
    pub fn status(&self) -> Result<Status> {
        let _lock = self.store.lock(&Cancel::default())?;
        self.store.clean()?;
        let Some(record) = self.store.read("credentials.json")? else {
            return Ok(Status::default());
        };
        let now = (self.clock)();
        let expiry = record.usize_field("expires_at")? as u64;
        let earliest = record.usize_field("earliest_refresh_at")? as u64;
        let pending = record.field("refresh_pending")? == &boolean(true);
        Ok(Status {
            logged_in: !pending && !self.store.signed_out()?,
            refresh_pending: pending,
            plan_enabled: plan_enabled(record.text_field("scope")?),
            account: account_id(&record)?,
            expires_in: expiry.saturating_sub(now),
            refresh_wait: earliest.saturating_sub(now),
            session: record.text_field("session")?.to_owned(),
        })
    }
    pub fn current(&self, lease: &Lease) -> Result<bool> {
        let _lock = self.store.lock(&Cancel::default())?;
        let Some(record) = self.store.read("credentials.json")? else {
            return Ok(false);
        };
        Ok(!self.store.signed_out()?
            && record.field("refresh_pending")? == &boolean(false)
            && account_id(&record)? == lease.account
            && record.text_field("session")? == lease.session)
    }
    pub fn hold_session(&self, session: &str, cancel: &Cancel) -> Result<Lock> {
        let lock = self.store.lock(cancel)?;
        let record = self
            .store
            .read("credentials.json")?
            .ok_or("auth_required")?;
        if self.store.signed_out()?
            || record.text_field("session")? != session
            || record.field("refresh_pending")? != &boolean(false)
            || record.usize_field("expires_at")? as u64 <= (self.clock)()
            || !plan_enabled(record.text_field("scope")?)
        {
            return Err("auth_required");
        }
        Ok(lock)
    }
    pub fn hold_execution(&self, session: &str, cancel: &Cancel) -> Result<ExecutionLease> {
        let lock = self.hold_session(session, cancel)?;
        Ok(ExecutionLease {
            watch: Watch::new(self.store.clone(), cancel)?,
            _lock: lock,
        })
    }
    pub fn logout(&self, cancel: &Cancel) -> Result<bool> {
        self.store.request_logout()?;
        let _lock = self.store.lock(cancel)?;
        self.store.clean()?;
        let Some(record) = self.store.read("credentials.json")? else {
            let saved = self.store.read("registration.json")?;
            return Ok(match saved {
                None => true,
                Some(saved) => {
                    !saved.has("client_id") || truth(&saved, "last_revocation_confirmed", true)?
                }
            });
        };
        self.revocation_status(false)?;
        self.store.remove("credentials.json")?;
        self.store
            .observe(StorePoint::TokensRemoved, "credentials.json")?;
        let confirmed = (|| {
            self.discover(cancel)?;
            let reply = self.request(
                "POST",
                endpoint::REVOKE,
                &[
                    ("token", record.text_field("refresh_token")?),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", record.text_field("client_id")?),
                ],
                cancel,
            )?;
            let saved = self.registration()?;
            Ok::<bool, &'static str>(
                reply.head.status == 200
                    && reply.body.is_empty()
                    && record.field("refresh_pending")? == &boolean(false)
                    && !truth(&saved, "exchange_pending", true)?
                    && !truth(&saved, "revocation_uncertain", true)?,
            )
        })()
        .unwrap_or(false);
        self.revocation_status(confirmed)?;
        Ok(confirmed)
    }
    fn authorized(
        &self,
        mut request: HttpRequest,
        lease: &Lease,
        cancel: &Cancel,
    ) -> Result<HttpReply> {
        self.store.observe(StorePoint::BeforeDispatch, "request")?;
        let _lock = self.store.lock(cancel)?;
        let record = self.admit_lease(lease)?;
        let watch = Watch::new(self.store.clone(), cancel)?;
        request.bearer = Some(Secret::new(record.text_field("access_token")?.to_owned()));
        let reply = (self.exchange)(&request, &watch.operation)?;
        if self.store.signed_out()? {
            return Err("auth_required");
        }
        Ok(reply)
    }
    fn admit_lease(&self, lease: &Lease) -> Result<Json> {
        let record = self
            .store
            .read("credentials.json")?
            .ok_or("auth_required")?;
        if self.store.signed_out()?
            || record.field("refresh_pending")? != &boolean(false)
            || !plan_enabled(record.text_field("scope")?)
            || account_id(&record)? != lease.account
            || record.text_field("session")? != lease.session
        {
            return Err("auth_required");
        }
        Ok(record)
    }
    pub fn models(&self, cancel: &Cancel) -> Result<Json> {
        let lease = self.access(cancel, false)?;
        json_reply(self.authorized(
            HttpRequest {
                method: "GET",
                url: "https://api.openai.com/v1/models",
                body: Secret::new(String::new()),
                content_type: "",
                bearer: None,
                body_limit: 2 * 1024 * 1024,
                timeout_ms: 20000,
            },
            &lease,
            cancel,
        )?)
    }
    pub fn generate(
        &self,
        request: Request,
        cancel: &Cancel,
        expected_session: &str,
        preview: Option<TextPreview>,
    ) -> Result<AdmittedResponse> {
        let lease = self.access(cancel, false)?;
        if request.binding.account != lease.account {
            return Err("auth_account_mismatch");
        }
        if !expected_session.is_empty() && expected_session != lease.session {
            return Err("auth_required");
        }
        if let Some(streaming) = &self.streaming {
            self.store.observe(StorePoint::BeforeDispatch, "request")?;
            let _lock = self.store.lock(cancel)?;
            let record = self.admit_lease(&lease)?;
            let watch = Watch::new(self.store.clone(), cancel)?;
            let stopped = watch.operation.clone();
            let mut stream = ResponseStream::new_with_callbacks(
                request.clone(),
                Limits::default(),
                preview,
                Some(Box::new(move || stopped.cancelled())),
            );
            streaming(
                &request.json()?,
                &Secret::new(record.text_field("access_token")?.to_owned()),
                &mut |bytes| stream.feed(bytes),
                &watch.operation,
            )?;
            if self.store.signed_out()? {
                return Err("auth_required");
            }
            return stream.finish();
        }
        let reply = self.authorized(
            HttpRequest {
                method: "POST",
                url: RESPONSES_ENDPOINT,
                body: Secret::new(request.json()?.dump()),
                content_type: "application/json",
                bearer: None,
                body_limit: 16 * 1024 * 1024,
                timeout_ms: 75000,
            },
            &lease,
            cancel,
        )?;
        if reply.head.status != 200 {
            return Err(match reply.head.status {
                401 => "auth_required",
                403 => "provider_policy_denied",
                429 => "provider_usage_limit",
                _ => "provider_http_failure",
            });
        }
        admit_request(
            &reply.head,
            RESPONSES_ENDPOINT,
            "POST",
            &request.json()?,
            true,
        )?;
        let stopped = cancel.clone();
        let mut stream = ResponseStream::new_with_callbacks(
            request,
            Limits::default(),
            preview,
            Some(Box::new(move || stopped.cancelled())),
        );
        stream.feed(reply.body.as_bytes())?;
        stream.finish()
    }
}
