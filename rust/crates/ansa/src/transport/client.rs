//! CLI transport bridge. Construction does no network I/O; requests are explicit.
use super::{
    frame_probe::{Counted, HeaderBudget, Route, proxy_for},
    http::Head,
};
use crate::{
    auth::{
        Cancel, Result, Secret,
        manager::{HttpReply, HttpRequest},
    },
    protocol::json::Json,
};
use base64::Engine;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Uri, body::Bytes};
use hyper_util::client::legacy::connect::Connection;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tower_service::Service;
type Sink<'a> = dyn FnMut(&[u8]) -> Result<()> + 'a;
#[derive(Clone, Copy)]
enum StreamAdmission<'a> {
    Strict,
    SiwcResponses(&'a Json),
}
struct RoutePolicy<'a> {
    local_only: bool,
    admission: StreamAdmission<'a>,
    mcp: Option<&'a mut McpExchange>,
}
pub(crate) struct McpExchange {
    pub version: Option<String>,
    pub session: Option<String>,
    pub response_session: Option<String>,
    pub response_id: Option<usize>,
}
fn tls() -> Result<rustls::ClientConfig> {
    let path = std::env::var_os("SSL_CERT_FILE")
        .or_else(|| std::env::var_os("NIX_SSL_CERT_FILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new("/etc/ssl/certs/ca-certificates.crt").into());
    let raw = read_ca(&path)?;
    tls_bytes(&raw)
}
pub fn read_ca(path: &Path) -> Result<String> {
    // System certificate paths may be symlinks. Resolve them, then pin a
    // nonblocking nofollow regular file; FIFO/device substitutions never block.
    let path = path.canonicalize().map_err(|_| "tls_ca_unavailable")?;
    let file = ansa_linux::fs::open_nofollow(&path, ansa_linux::local::read_flags())
        .map_err(|_| "tls_ca_unavailable")?;
    if !file.metadata().map_err(|_| "tls_ca_invalid")?.is_file() {
        return Err("tls_ca_invalid");
    }
    use std::io::Read;
    let mut raw = String::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut raw)
        .map_err(|_| "tls_ca_invalid")?;
    if raw.len() > 4 * 1024 * 1024 {
        return Err("tls_ca_invalid");
    }
    Ok(raw)
}
fn tls_bytes(raw: &str) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for block in raw.split("-----BEGIN CERTIFICATE-----").skip(1) {
        let body = block
            .split_once("-----END CERTIFICATE-----")
            .ok_or("tls_ca_invalid")?
            .0;
        let body: String = body.chars().filter(|v| !v.is_ascii_whitespace()).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .map_err(|_| "tls_ca_invalid")?;
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|_| "tls_ca_invalid")?;
    }
    if roots.is_empty() {
        return Err("tls_ca_invalid");
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| "tls_configuration")
        .map(|v| v.with_root_certificates(roots).with_no_client_auth())
}
async fn cancelled(stop: &Cancel) {
    while !stop.cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// Only dial the endpoint here. The HTTP request and its body stay with the
// caller until a connection is ready, including TLS and proxy negotiation.
async fn connect<C: Service<Uri>>(
    connector: &mut C,
    uri: &Uri,
    stop: &Cancel,
    retry: bool,
    trace_responses: bool,
) -> Result<C::Response> {
    if !retry {
        return connector
            .call(uri.clone())
            .await
            .map_err(|_| "http_connect");
    }
    let mut delays = [Duration::from_millis(250), Duration::from_secs(1)].into_iter();
    loop {
        stop.check()?;
        let dial = tokio::time::timeout(Duration::from_secs(10), connector.call(uri.clone()));
        if let Ok(Ok(stream)) = dial.await {
            return Ok(stream);
        }
        let Some(delay) = delays.next() else {
            return Err("http_connect");
        };
        tokio::time::sleep(delay).await;
        stop.check()?;
        if trace_responses {
            crate::benchmark_trace::connect_retry();
        }
    }
}
fn head(headers: &hyper::HeaderMap, status: u16) -> Result<Head> {
    let mut bytes = 0;
    if headers.len() > 256 {
        return Err("http_header_fields_limit");
    }
    for (name, value) in headers {
        bytes += name.as_str().len() + value.as_bytes().len() + 4;
    }
    if bytes > 64 * 1024 {
        return Err("http_header_bytes_limit");
    }
    Ok(Head {
        status,
        content_types: headers
            .get_all("content-type")
            .iter()
            .map(|v| {
                v.to_str()
                    .map(String::from)
                    .map_err(|_| "http_header_value")
            })
            .collect::<Result<_>>()?,
    })
}
/// Uses the existing proxy connector; never redirects/retries an ambiguous POST.
fn perform(
    request: &HttpRequest,
    stop: &Cancel,
    sink: Option<&mut Sink<'_>>,
    admission: StreamAdmission<'_>,
) -> Result<HttpReply> {
    let uri: Uri = request.url.parse().map_err(|_| "url_invalid")?;
    if uri.scheme_str() != Some("https")
        || !matches!(uri.host(), Some("auth.openai.com" | "api.openai.com"))
    {
        return Err("url_not_authorized");
    }
    let mut env = BTreeMap::new();
    for key in [
        "http_proxy",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    let proxy = proxy_for(&uri, &env)?;
    let tls = tls()?;
    perform_route(
        request,
        stop,
        sink,
        uri,
        tls,
        proxy,
        RoutePolicy {
            local_only: false,
            admission,
            mcp: None,
        },
    )
}
fn perform_route(
    request: &HttpRequest,
    stop: &Cancel,
    mut sink: Option<&mut Sink<'_>>,
    uri: Uri,
    tls: rustls::ClientConfig,
    proxy: Option<hyper_util::client::proxy::matcher::Intercept>,
    policy: RoutePolicy<'_>,
) -> Result<HttpReply> {
    if let StreamAdmission::SiwcResponses(value) = policy.admission {
        super::http::validate_request_profile(request.url, request.method, value)?;
    }
    let retry = policy.mcp.is_none()
        && (request.method == "GET"
            || matches!(
                request.url,
                super::http::RESPONSES_ENDPOINT | crate::openrouter::API
            ));
    let mut connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls.clone())
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(Route {
            proxy,
            tls,
            local_only: policy.local_only,
            tcp_timeout: (!retry).then_some(Duration::from_secs(1)),
        });
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "http_runtime")?;
    reactor.block_on(async {
        let deadline = request.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut driver:Option<tokio::task::JoinHandle<()>>=None;
        let transfer=async {
            let trace_responses = matches!(policy.admission, StreamAdmission::SiwcResponses(_));
            let stream = connect(&mut connector, &uri, stop, retry, trace_responses).await?;
            stop.check()?;
            let h2=stream.connected().is_negotiated_h2();let wire=Arc::new(AtomicUsize::new(0));
            let stream=hyper_util::rt::TokioIo::new(Counted {inner:hyper_util::rt::TokioIo::new(stream),bytes:wire.clone(),limit:if h2 {usize::MAX}else{request.body_limit+64*1024}});
            let budget=Arc::new(Mutex::new(HeaderBudget::default()));let (failure,mut failed)=tokio::sync::watch::channel(false);let _failure_lifetime=failure.clone();
            let mut builder=Request::builder().method(request.method).uri(if h2 {uri.to_string()}else{uri.path_and_query().map_or("/",|v|v.as_str()).to_owned()}).header("host",uri.authority().ok_or("url_invalid")?.as_str());
            let mut mcp = policy.mcp;
            if let Some(mcp) = mcp.as_ref() {
                builder=builder.header("accept", "application/json, text/event-stream");
                if let Some(version) = &mcp.version {builder=builder.header("mcp-protocol-version",version);}
                if let Some(session) = &mcp.session {builder=builder.header("mcp-session-id",session);}
            } else {
                builder=builder.header("accept",if sink.is_some(){"text/event-stream"}else{"application/json"});
            }
            if !request.content_type.is_empty(){builder=builder.header("content-type",request.content_type);}
            if let Some(bearer)=&request.bearer {builder=builder.header("authorization",format!("Bearer {}",bearer.expose()));}
            let mut req=builder.body(Full::new(Bytes::copy_from_slice(request.body.expose().as_bytes()))).map_err(|_|"http_request")?;
            let info=budget.clone();hyper::ext::on_informational(&mut req,move |head| {
                let mut guard=info.lock().unwrap_or_else(|e|e.into_inner());
                if guard.interim.len()>=256 {guard.failed=true;}
                if guard.observe(head.headers(),true){guard.interim.push(head.status().as_u16());}
                if guard.failed{let _=failure.send(true);}
            });
            let mut reply=if h2 {
                let (mut sender,connection)=hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new()).max_header_list_size(64*1024).handshake(stream).await.map_err(|_|"http_handshake")?;
                driver=Some(tokio::spawn(async move{let _=connection.await;}));
                if matches!(policy.admission, StreamAdmission::SiwcResponses(_)) {
                    crate::benchmark_trace::http_sent();
                }
                tokio::select!{reply=sender.send_request(req)=>reply.map_err(|_|"http_transfer")?,_=failed.changed()=>return Err("informational_header_limit")}
            }else{
                let (mut sender,connection)=hyper::client::conn::http1::Builder::new().max_headers(256).max_buf_size(64*1024).handshake(stream).await.map_err(|_|"http_handshake")?;
                driver=Some(tokio::spawn(async move{let _=connection.await;}));
                if matches!(policy.admission, StreamAdmission::SiwcResponses(_)) {
                    crate::benchmark_trace::http_sent();
                }
                tokio::select!{reply=sender.send_request(req)=>reply.map_err(|_|"http_transfer")?,_=failed.changed()=>return Err("informational_header_limit")}
            };
            if !budget.lock().map_err(|_|"http_budget")?.observe(reply.headers(),true){return Err("http_header_limit");}
            let head=head(reply.headers(),reply.status().as_u16())?;
            if let Some(mcp) = mcp.as_mut() {
                let mut sessions = reply.headers().get_all("mcp-session-id").iter();
                if let Some(session) = sessions.next() {
                    let session = session.to_str().map_err(|_| "mcp_session")?;
                    if session.is_empty() || !session.bytes().all(|b| (0x21..=0x7e).contains(&b))
                        || sessions.next().is_some() {
                        return Err("mcp_session");
                    }
                    mcp.response_session = Some(session.into());
                }
            }
            if sink.is_some() {
                if head.status!=200 {return Err(match head.status {401=>"auth_required",403=>"provider_policy_denied",429=>"provider_usage_limit",_=>"provider_http_failure"});}
                match policy.admission {
                    StreamAdmission::Strict => super::http::validate_head(&head)?,
                    StreamAdmission::SiwcResponses(value) => super::http::admit_request(&head, request.url, request.method, value, true)?,
                }
            }
            let response_id = mcp.as_ref().and_then(|mcp| mcp.response_id);
            let mut mcp_sse = response_id.filter(|_| super::http::validate_head(&head).is_ok()).map(|id| {
                (id, crate::protocol::sse::Parser::new(crate::protocol::sse::Limits {
                    line: request.body_limit, event: request.body_limit, ..Default::default()
                }))
            });
            let mut bytes=Vec::new();let mut count=0;
            while let Some(frame)=reply.body_mut().frame().await {
                let frame=frame.map_err(|_|"http_transfer")?;
                if let Some(data)=frame.data_ref() {
                    count+=data.len();if count>request.body_limit{return Err("http_body_limit");}
                    if let Some((id, parser)) = &mut mcp_sse {
                        parser.feed(data)?;
                        for event in parser.take() {
                            if event.data.is_empty() { continue; }
                            let message = Json::parse(&event.data)?;
                            if message.usize_field("id").ok() == Some(*id) && !message.has("method") {
                                return Ok(HttpReply {head, body: event.data});
                            }
                            if message.has("method") && message.has("id") {
                                mcp_server_reply(
                                    request, mcp.as_deref().ok_or("mcp_server_request")?, &message, stop,
                                    deadline.ok_or("mcp_timeout_required")?,
                                )?;
                            }
                            if message.text_field("method").ok() == Some("notifications/tools/list_changed") {
                                return Err("mcp_tools_changed");
                            }
                        }
                    } else if let Some(sink)=&mut sink {sink(data)?;}else{bytes.extend_from_slice(data);}
                }
                if let Some(trailers)=frame.trailers_ref() && (trailers.contains_key(hyper::header::CONTENT_TYPE) || !budget.lock().map_err(|_|"http_budget")?.observe(trailers,false)){return Err("forbidden_trailer");}
            }
            if !h2 && wire.load(Ordering::Relaxed).saturating_sub(count)>64*1024 {return Err("wire_header_limit");}
            if mcp_sse.is_some() { return Err("mcp_missing_response"); }
            Ok(HttpReply {head,body:String::from_utf8(bytes).map_err(|_|"http_body_utf8")?})
        };
        let transfer = async {
            match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), transfer)
                    .await
                    .unwrap_or(Err("http_timeout")),
                None => transfer.await,
            }
        };
        let result = tokio::select! {
            biased;
            _ = cancelled(stop) => Err("auth_cancelled"),
            result = transfer => result,
        };
        if let Some(driver)=driver.take(){driver.abort();let _=driver.await;}result
    })
}
/// Opt-in loopback fixture route through the actual CLI bridge. No host trust
/// configuration or credentials are read, and the endpoint cannot be remote.
#[cfg(feature = "frontend-test-adapter")]
pub fn fixture_stream(
    url: &str,
    ca: &[u8],
    value: &Json,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    stop: &Cancel,
) -> Result<()> {
    let uri: Uri = url.parse().map_err(|_| "fixture_url")?;
    if !matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]"))
        || !matches!(uri.scheme_str(), Some("http" | "https"))
    {
        return Err("fixture_url");
    }
    let mut roots = rustls::RootCertStore::empty();
    if !ca.is_empty() {
        roots
            .add(rustls::pki_types::CertificateDer::from(ca.to_vec()))
            .map_err(|_| "fixture_ca")?;
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
    .map_err(|_| "fixture_ca")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    perform_route(
        &HttpRequest {
            method: "POST",
            url: super::http::RESPONSES_ENDPOINT,
            content_type: "application/json",
            body: Secret::new(value.dump()),
            bearer: Some(Secret::new("synthetic-bridge-bearer".into())),
            body_limit: 16 * 1024 * 1024,
            timeout_ms: Some(3000),
        },
        stop,
        Some(sink),
        uri,
        tls,
        None,
        RoutePolicy {
            local_only: true,
            admission: StreamAdmission::Strict,
            mcp: None,
        },
    )
    .map(|_| ())
}
pub fn exchange(request: &HttpRequest, stop: &Cancel) -> Result<HttpReply> {
    perform(request, stop, None, StreamAdmission::Strict)
}
pub(crate) fn openrouter_exchange(request: &HttpRequest, stop: &Cancel) -> Result<HttpReply> {
    let uri: Uri = request.url.parse().map_err(|_| "url_invalid")?;
    let role_catalog = request.url == crate::openrouter::ROLE_CATALOG;
    let model_endpoint = uri
        .path()
        .strip_prefix("/api/v1/models/")
        .and_then(|path| path.strip_suffix("/endpoints"))
        .is_some_and(crate::provider::request::openrouter_model_id);
    if uri.scheme_str() != Some("https")
        || uri.authority().map(|value| value.as_str()) != Some("openrouter.ai")
        || (uri.query().is_some() && !role_catalog)
        || !(request.method == "GET"
            && (role_catalog || model_endpoint || uri.path() == "/api/v1/key")
            || request.method == "POST"
                && uri.path() == "/api/v1/chat/completions"
                && uri.query().is_none())
        || (request.bearer.is_some() && (role_catalog || model_endpoint))
    {
        return Err("openrouter_endpoint");
    }
    perform_route(
        request,
        stop,
        None,
        uri,
        tls()?,
        None,
        RoutePolicy {
            local_only: false,
            admission: StreamAdmission::Strict,
            mcp: None,
        },
    )
}
pub(crate) fn mcp_exchange(
    request: &HttpRequest,
    stop: &Cancel,
    fields: &mut McpExchange,
) -> Result<HttpReply> {
    let uri: Uri = request.url.parse().map_err(|_| "mcp_endpoint")?;
    let host = uri.host().ok_or("mcp_endpoint")?;
    let local = matches!(host, "127.0.0.1" | "[::1]");
    if uri
        .authority()
        .is_none_or(|authority| authority.as_str().contains('@'))
        || !matches!(uri.scheme_str(), Some("https"))
            && !(local && uri.scheme_str() == Some("http"))
    {
        return Err("mcp_endpoint");
    }
    let tls = if uri.scheme_str() == Some("http") {
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|_| "tls_configuration")?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth()
    } else {
        tls()?
    };
    perform_route(
        request,
        stop,
        None,
        uri,
        tls,
        None,
        RoutePolicy {
            local_only: local,
            admission: StreamAdmission::Strict,
            mcp: Some(fields),
        },
    )
}
fn mcp_server_reply(
    current: &HttpRequest,
    fields: &McpExchange,
    message: &Json,
    stop: &Cancel,
    deadline: Instant,
) -> Result<()> {
    stop.check()?;
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    if remaining == 0 {
        return Err("http_timeout");
    }
    let id = message.field("id")?;
    if message.text_field("jsonrpc")? != "2.0" || !(id.is_string() || id.is_integer()) {
        return Err("mcp_server_request");
    }
    let (member, value) = if message.text_field("method")? == "ping" {
        ("result", Json::parse("{}")?)
    } else {
        (
            "error",
            Json::parse(r#"{"code":-32601,"message":"Method not found"}"#)?,
        )
    };
    let response = Json::object(vec![
        ("jsonrpc", Json::string("2.0")?),
        ("id", id.clone()),
        (member, value),
    ])?;
    let mut fields = McpExchange {
        version: fields.version.clone(),
        session: fields
            .session
            .clone()
            .or_else(|| fields.response_session.clone()),
        response_session: None,
        response_id: None,
    };
    let exchange = HttpRequest {
        method: "POST",
        url: current.url,
        content_type: "application/json",
        body: Secret::new(response.dump()),
        bearer: current.bearer.clone(),
        body_limit: 4096,
        timeout_ms: Some(remaining.min(1000) as u64),
    };
    // The request that yielded this SSE event still owns its reactor.
    let reply = std::thread::scope(|scope| {
        scope
            .spawn(|| mcp_exchange(&exchange, stop, &mut fields))
            .join()
            .map_err(|_| "mcp_worker")?
    })?;
    if reply.head.status != 202 || !reply.body.is_empty() {
        return Err("mcp_server_request");
    }
    Ok(())
}
fn responses_request(value: &Json, bearer: &Secret) -> HttpRequest<'static> {
    HttpRequest {
        method: "POST",
        url: super::http::RESPONSES_ENDPOINT,
        content_type: "application/json",
        body: Secret::new(value.dump()),
        bearer: Some(bearer.clone()),
        body_limit: 16 * 1024 * 1024,
        // Long reasoning requests may take longer than a fixed transfer
        // deadline. Cancellation still covers connection and body reads.
        timeout_ms: None,
    }
}

pub fn stream(
    value: &Json,
    bearer: &Secret,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    stop: &Cancel,
) -> Result<()> {
    crate::benchmark_trace::http(|| {
        perform(
            &responses_request(value, bearer),
            stop,
            Some(sink),
            StreamAdmission::SiwcResponses(value),
        )
        .map(|_| ())
    })
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
