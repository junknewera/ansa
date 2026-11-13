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
    time::Duration,
};
type Sink<'a> = dyn FnMut(&[u8]) -> Result<()> + 'a;
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
fn perform(request: &HttpRequest, stop: &Cancel, sink: Option<&mut Sink<'_>>) -> Result<HttpReply> {
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
    perform_route(request, stop, sink, uri, tls, proxy, false)
}
fn perform_route(
    request: &HttpRequest,
    stop: &Cancel,
    mut sink: Option<&mut Sink<'_>>,
    uri: Uri,
    tls: rustls::ClientConfig,
    proxy: Option<hyper_util::client::proxy::matcher::Intercept>,
    local_only: bool,
) -> Result<HttpReply> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls.clone())
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(Route {
            proxy,
            tls,
            local_only,
        });
    let reactor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "http_runtime")?;
    reactor.block_on(async {
        let mut driver:Option<tokio::task::JoinHandle<()>>=None;
        let transfer=async {
            use tower_service::Service;
            let mut connector=connector;let stream=connector.call(uri.clone()).await.map_err(|_|"http_connect")?;
            let h2=stream.connected().is_negotiated_h2();let wire=Arc::new(AtomicUsize::new(0));
            let stream=hyper_util::rt::TokioIo::new(Counted {inner:hyper_util::rt::TokioIo::new(stream),bytes:wire.clone(),limit:if h2 {usize::MAX}else{request.body_limit+64*1024}});
            let budget=Arc::new(Mutex::new(HeaderBudget::default()));let (failure,mut failed)=tokio::sync::watch::channel(false);let _failure_lifetime=failure.clone();
            let mut builder=Request::builder().method(request.method).uri(if h2 {uri.to_string()}else{uri.path_and_query().map_or("/",|v|v.as_str()).to_owned()}).header("host",uri.authority().ok_or("url_invalid")?.as_str()).header("accept",if sink.is_some(){"text/event-stream"}else{"application/json"});
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
                tokio::select!{reply=sender.send_request(req)=>reply.map_err(|_|"http_transfer")?,_=failed.changed()=>return Err("informational_header_limit")}
            }else{
                let (mut sender,connection)=hyper::client::conn::http1::Builder::new().max_headers(256).max_buf_size(64*1024).handshake(stream).await.map_err(|_|"http_handshake")?;
                driver=Some(tokio::spawn(async move{let _=connection.await;}));
                tokio::select!{reply=sender.send_request(req)=>reply.map_err(|_|"http_transfer")?,_=failed.changed()=>return Err("informational_header_limit")}
            };
            if !budget.lock().map_err(|_|"http_budget")?.observe(reply.headers(),true){return Err("http_header_limit");}
            let head=head(reply.headers(),reply.status().as_u16())?;
            if sink.is_some() {
                if head.status!=200 {return Err(match head.status {401=>"auth_required",403=>"provider_policy_denied",429=>"provider_usage_limit",_=>"provider_http_failure"});}
                super::http::validate_head(&head)?;
            }
            let mut bytes=Vec::new();let mut count=0;
            while let Some(frame)=reply.body_mut().frame().await {
                let frame=frame.map_err(|_|"http_transfer")?;
                if let Some(data)=frame.data_ref() {
                    count+=data.len();if count>request.body_limit{return Err("http_body_limit");}
                    if let Some(sink)=&mut sink {sink(data)?;}else{bytes.extend_from_slice(data);}
                }
                if let Some(trailers)=frame.trailers_ref() && (trailers.contains_key(hyper::header::CONTENT_TYPE) || !budget.lock().map_err(|_|"http_budget")?.observe(trailers,false)){return Err("forbidden_trailer");}
            }
            if !h2 && wire.load(Ordering::Relaxed).saturating_sub(count)>64*1024 {return Err("wire_header_limit");}
            Ok(HttpReply {head,body:String::from_utf8(bytes).map_err(|_|"http_body_utf8")?})
        };
        let result=tokio::select! {biased; _=cancelled(stop)=>Err("auth_cancelled"), result=tokio::time::timeout(Duration::from_millis(request.timeout_ms),transfer)=>result.unwrap_or(Err("http_timeout"))};
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
            timeout_ms: 3000,
        },
        stop,
        Some(sink),
        uri,
        tls,
        None,
        true,
    )
    .map(|_| ())
}
pub fn exchange(request: &HttpRequest, stop: &Cancel) -> Result<HttpReply> {
    perform(request, stop, None)
}
pub fn stream(
    value: &Json,
    bearer: &Secret,
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    stop: &Cancel,
) -> Result<()> {
    perform(
        &HttpRequest {
            method: "POST",
            url: super::http::RESPONSES_ENDPOINT,
            content_type: "application/json",
            body: Secret::new(value.dump()),
            bearer: Some(bearer.clone()),
            body_limit: 16 * 1024 * 1024,
            timeout_ms: 75000,
        },
        stop,
        Some(sink),
    )
    .map(|_| ())
}
