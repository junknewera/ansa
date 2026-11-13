#![forbid(unsafe_code)]
#[path = "../tests/support/auth_fixture.rs"]
mod fixture;
use ansa::{
    auth::{self, Cancel, Result, manager::Status},
    protocol::{json::Json, legacy_json_v1::sha256},
};
use std::{
    io::{self, BufRead, Write},
    path::Path,
    sync::atomic::Ordering,
};
fn flag(value: bool) -> Result<Json> {
    Json::parse(if value { "true" } else { "false" })
}
fn status_json(s: &Status) -> Result<Json> {
    Json::object(vec![
        ("logged_in", flag(s.logged_in)?),
        ("refresh_pending", flag(s.refresh_pending)?),
        ("plan_enabled", flag(s.plan_enabled)?),
        ("account", Json::string(&s.account)?),
        ("expires_in", fixture::number(s.expires_in)?),
        ("refresh_wait", fixture::number(s.refresh_wait)?),
        ("session", Json::string(&s.session)?),
    ])
}
fn store(request: &Json) -> Result<Json> {
    let root = Path::new(request.text_field("root")?);
    fixture::validate_root(root)?;
    let server = fixture::Server::new(root)?;
    if request.has("now") {
        server
            .now
            .store(request.usize_field("now")? as u64, Ordering::Release);
    }
    if request.has("permission") {
        server
            .permission
            .store(request.field("permission")?.boolean()?, Ordering::Release);
    }
    if request.has("revoke_failure") {
        server.revoke_failure.store(
            request.field("revoke_failure")?.boolean()?,
            Ordering::Release,
        );
    }
    let manager = server.manager(None, None)?;
    let mut out = vec![("accepted", flag(true)?)];
    match request.text_field("action")? {
        "login" => server.login(&manager)?,
        action @ ("access" | "refresh") => {
            let lease = manager.access(&Cancel::default(), action == "refresh")?;
            out.extend([
                ("account", Json::string(&lease.account)?),
                ("session", Json::string(&lease.session)?),
                ("expires_at", fixture::number(lease.expires_at)?),
                ("bearer_present", flag(!lease.bearer().expose().is_empty())?),
            ]);
        }
        "logout" => out.push(("confirmed", flag(manager.logout(&Cancel::default())?)?)),
        "status" => (),
        _ => return Err("fixture_store_action"),
    }
    out.push(("status", status_json(&manager.status()?)?));
    Json::object(out)
}
fn evaluate(r: &Json) -> Result<Json> {
    match r.text_field("op")? {
        "jwt" => {
            let claims = auth::claims::verify_id_token(
                r.text_field("jwt")?,
                r.field("jwks")?,
                r.text_field("client")?,
                r.text_field("nonce")?,
                r.usize_field("now")? as u64,
                r.has("refresh") && r.field("refresh")?.boolean()?,
            )?;
            Json::object(vec![
                ("accepted", flag(true)?),
                ("subject", claims.field("sub")?.clone()),
                (
                    "claims_sha256",
                    Json::string(&sha256(claims.dump().as_bytes()))?,
                ),
            ])
        }
        "callback" => {
            let port = r.usize_field("port")?;
            if !(1..=65535).contains(&port) {
                return Err("fixture_port");
            }
            let fields = auth::callback::validate_request(
                r.text_field("request")?,
                r.text_field("state")?,
                port as u16,
            )?;
            Json::object(vec![
                ("accepted", flag(true)?),
                ("field_count", fixture::number(fields.len() as u64)?),
                ("code_present", flag(fields.contains_key("code"))?),
            ])
        }
        "pkce" => Json::object(vec![
            ("accepted", flag(true)?),
            (
                "challenge",
                Json::string(&auth::pkce::challenge(r.text_field("verifier")?))?,
            ),
        ]),
        "form" => {
            let fields = r
                .field("fields")?
                .elements()?
                .iter()
                .map(|row| Ok((row.text_field("key")?, row.text_field("value")?)))
                .collect::<Result<Vec<_>>>()?;
            Json::object(vec![
                ("accepted", flag(true)?),
                ("encoded", Json::string(&auth::pkce::form_encode(&fields))?),
            ])
        }
        "base64" => {
            let bytes = auth::pkce::unbase64url(r.text_field("encoded")?)?;
            Json::object(vec![
                ("accepted", flag(true)?),
                ("encoded", Json::string(&auth::pkce::base64url(&bytes)?)?),
                ("bytes", fixture::number(bytes.len() as u64)?),
            ])
        }
        "discovery" => {
            auth::registration::discover(r.field("discovery")?)?;
            Json::object(vec![("accepted", flag(true)?)])
        }
        "scenario" => fixture::run_case(r.text_field("case")?),
        "store" => store(r),
        _ => Err("fixture_unknown_operation"),
    }
}
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|s| s == "--fixture-child") {
        let result = (|| {
            if args.len() != 5 {
                return Err("fixture_child_args");
            }
            fixture::helper(
                Path::new(&args[2]),
                args[3].to_str().ok_or("fixture_action")?,
                args[4]
                    .to_str()
                    .ok_or("fixture_clock")?
                    .parse()
                    .map_err(|_| "fixture_clock")?,
            )
        })();
        return if result.is_ok() {
            std::process::ExitCode::SUCCESS
        } else {
            eprintln!("fixture_child_failed");
            std::process::ExitCode::FAILURE
        };
    }
    for line in io::stdin().lock().lines() {
        let result = line
            .map_err(|_| "fixture_read")
            .and_then(|s| Json::parse_with_limits(&s, 4 * 1024 * 1024, 64, 131072))
            .and_then(|r| evaluate(&r));
        let out = match result {
            Ok(value) => value,
            Err(code) => Json::object(vec![
                ("accepted", flag(false).expect("boolean")),
                ("diagnostic", Json::string(code).expect("static diagnostic")),
            ])
            .expect("bounded error"),
        };
        println!("{}", out.dump());
        if io::stdout().flush().is_err() {
            return std::process::ExitCode::FAILURE;
        }
    }
    std::process::ExitCode::SUCCESS
}
