use super::{Cancel, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub fn decode_form(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let value = match bytes[index] {
            b'+' => b' ',
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err("auth_callback_encoding");
                }
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .map_err(|_| "auth_callback_encoding")?;
                if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("auth_callback_encoding");
                }
                let value = u8::from_str_radix(hex, 16).map_err(|_| "auth_callback_encoding")?;
                index += 2;
                value
            }
            other => other,
        };
        if value < 0x20 || value == 0x7f {
            return Err("auth_callback_encoding");
        }
        out.push(value);
        index += 1;
    }
    String::from_utf8(out).map_err(|_| "auth_callback_encoding")
}
pub fn query_fields(query: &str) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for field in query.split_terminator('&') {
        let (key, value) = field.split_once('=').ok_or("auth_callback_fields")?;
        if fields.len() >= 16
            || fields
                .insert(decode_form(key)?, decode_form(value)?)
                .is_some()
        {
            return Err("auth_callback_duplicate");
        }
    }
    Ok(fields)
}
// This pinned ring public API performs constant-time comparison. Deprecation
// is confined to this boundary; no application-owned unsafe or ad-hoc loop.
#[allow(deprecated)]
fn same_state(a: &str, b: &str) -> bool {
    ring::constant_time::verify_slices_are_equal(a.as_bytes(), b.as_bytes()).is_ok()
}

pub fn validate_request(request: &str, state: &str, port: u16) -> Result<BTreeMap<String, String>> {
    if request.len() >= 16384 {
        return Err("auth_callback_limit");
    }
    let (head, _) = request.split_once("\r\n\r\n").ok_or("auth_callback_path")?;
    let mut lines = head.split("\r\n");
    let line = lines.next().ok_or("auth_callback_path")?;
    let query = line
        .strip_prefix("GET /auth/callback?")
        .and_then(|s| s.strip_suffix(" HTTP/1.1"))
        .ok_or("auth_callback_path")?;
    let mut host = None;
    for line in lines {
        if line.starts_with([' ', '\t']) {
            return Err("auth_callback_path");
        }
        let (name, value) = line.split_once(':').ok_or("auth_callback_path")?;
        if name.eq_ignore_ascii_case("host")
            && host.replace(value.trim_matches([' ', '\t'])).is_some()
        {
            return Err("auth_callback_path");
        }
    }
    if host != Some(format!("127.0.0.1:{port}").as_str()) {
        return Err("auth_callback_path");
    }
    let fields = query_fields(query)?;
    if fields
        .get("state")
        .is_none_or(|value| !same_state(value, state))
    {
        return Err("auth_callback_state");
    }
    if fields.contains_key("error") {
        return Err("auth_access_denied");
    }
    if fields
        .get("code")
        .is_none_or(|value| value.is_empty() || value.len() > 8192)
    {
        return Err("auth_callback_code");
    }
    Ok(fields)
}

pub(crate) struct Listener(TcpListener);
impl Listener {
    pub(crate) fn bind() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| "auth_listener")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "auth_listener")?;
        Ok(Self(listener))
    }
    pub(crate) fn port(&self) -> Result<u16> {
        Ok(self.0.local_addr().map_err(|_| "auth_listener")?.port())
    }
    pub(crate) fn receive(&self, state: &str, cancel: &Cancel) -> Result<BTreeMap<String, String>> {
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut attempts = 0;
        while attempts < 64 {
            cancel.check()?;
            if Instant::now() >= deadline {
                return Err("auth_callback_timeout");
            }
            let (mut stream, peer) = match self.0.accept() {
                Ok(value) => value,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    cancel.wait(Duration::from_millis(25));
                    continue;
                }
                Err(_) => return Err("auth_callback_read"),
            };
            attempts += 1;
            if peer.ip() != Ipv4Addr::LOCALHOST {
                return Err("auth_callback_peer");
            }
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .map_err(|_| "auth_callback_read")?;
            stream
                .set_write_timeout(Some(Duration::from_millis(100)))
                .map_err(|_| "auth_callback_read")?;
            let result = (|| {
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut bytes = Vec::new();
                loop {
                    cancel.check()?;
                    if Instant::now() >= deadline || bytes.len() >= 16384 {
                        return Err("auth_callback_limit");
                    }
                    let mut buffer = [0; 2048];
                    match stream.read(&mut buffer) {
                        Ok(0) => return Err("auth_callback_read"),
                        Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => return Err("auth_callback_read"),
                    }
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                validate_request(
                    std::str::from_utf8(&bytes).map_err(|_| "auth_callback_encoding")?,
                    state,
                    self.port()?,
                )
            })();
            browser_reply(&mut stream, result.is_ok());
            match result {
                Ok(fields) => return Ok(fields),
                Err(e @ ("auth_access_denied" | "auth_cancelled")) => return Err(e),
                Err(_) => (),
            }
        }
        Err("auth_callback_limit")
    }
}
fn browser_reply(stream: &mut TcpStream, success: bool) {
    let body = if success {
        "Sign-in received. You can return to oh-my-harness."
    } else {
        "Sign-in request rejected."
    };
    let status = if success { "200 OK" } else { "400 Bad Request" };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes());
}
