//! M1 localhost-only frame/proxy/TLS probe. No provider or tool admission path.
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Request, Uri, body::Bytes};
use hyper_util::client::legacy::connect::{
    Connected, Connection, HttpConnector,
    proxy::{SocksV4, SocksV5, Tunnel},
};
use hyper_util::client::proxy::matcher::{Intercept, Matcher};
use hyper_util::rt::TokioExecutor;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tower_service::Service;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
trait Wire: hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send {}
impl<T: hyper::rt::Read + hyper::rt::Write + Connection + Unpin + Send> Wire for T {}
struct Io(Box<dyn Wire>);
struct Counted<T> {
    inner: T,
    bytes: Arc<AtomicUsize>,
    limit: usize,
}
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Counted<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buffer);
        let read = buffer.filled().len() - before;
        let total = this
            .bytes
            .fetch_add(read, Ordering::Relaxed)
            .saturating_add(read);
        if total > this.limit {
            return Poll::Ready(Err(std::io::Error::other("wire_limit")));
        }
        result
    }
}
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Counted<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
impl Connection for Io {
    fn connected(&self) -> Connected {
        self.0.connected()
    }
}
impl hyper::rt::Read for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_read(cx, buf)
    }
}
impl hyper::rt::Write for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_shutdown(cx)
    }
}

/// Never reads actual environment values. Caller supplies a synthetic configuration.
pub fn proxy_for(
    uri: &Uri,
    env: &BTreeMap<String, String>,
) -> Result<Option<Intercept>, &'static str> {
    let value = |lower: &str, upper: &str| {
        env.get(lower)
            .filter(|v| !v.is_empty())
            .or_else(|| env.get(upper).filter(|v| !v.is_empty()))
            .cloned()
            .unwrap_or_default()
    };
    let no = value("no_proxy", "NO_PROXY");
    let bypass = Matcher::builder().all("http://127.0.0.1:1").no(no).build();
    if bypass.intercept(uri).is_none() {
        return Ok(None);
    }
    let selected = match uri.scheme_str() {
        Some("https") => value("https_proxy", "HTTPS_PROXY"),
        Some("http") => env
            .get("http_proxy")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_default(),
        _ => return Err("url_scheme"),
    };
    let selected = if selected.is_empty() {
        value("all_proxy", "ALL_PROXY")
    } else {
        selected
    };
    if selected.is_empty() {
        return Ok(None);
    }
    let selected = if selected.contains("://") {
        selected
    } else {
        format!("http://{selected}")
    };
    let scheme = selected
        .split("://")
        .next()
        .ok_or("proxy_scheme")?
        .to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    ) {
        return Err("unsupported_proxy");
    }
    Matcher::builder()
        .all(selected)
        .build()
        .intercept(uri)
        .map(Some)
        .ok_or("invalid_proxy")
}

#[derive(Clone)]
struct Route {
    proxy: Option<Intercept>,
    tls: rustls::ClientConfig,
}
impl Service<Uri> for Route {
    type Response = Io;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Io, BoxError>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let proxy = self.proxy.clone();
        let tls = self.tls.clone();
        Box::pin(async move {
            let mut tcp = HttpConnector::new();
            tcp.enforce_http(false);
            tcp.set_connect_timeout(Some(Duration::from_secs(1)));
            let Some(proxy) = proxy else {
                return Ok(Io(Box::new(tcp.call(uri).await?)));
            };
            if !matches!(
                proxy.uri().host(),
                Some("localhost" | "127.0.0.1" | "[::1]")
            ) {
                return Err("probe_proxy_not_local".into());
            }
            let target = proxy.uri().clone();
            match target.scheme_str() {
                Some("http" | "https") => {
                    let mut connector = hyper_rustls::HttpsConnectorBuilder::new()
                        .with_tls_config(tls)
                        .https_or_http()
                        .enable_http1()
                        .wrap_connector(tcp);
                    if uri.scheme_str() == Some("http") {
                        return Ok(Io(Box::new(connector.call(target).await?)));
                    }
                    let mut tunnel = Tunnel::new(target, connector);
                    if let Some(auth) = proxy.basic_auth() {
                        tunnel = tunnel.with_auth(auth.clone());
                    }
                    Ok(Io(Box::new(tunnel.call(uri).await?)))
                }
                Some("socks5" | "socks5h") => {
                    let local = target.scheme_str() == Some("socks5");
                    let mut socks = SocksV5::new(target, tcp).local_dns(local);
                    if let Some((user, pass)) = proxy.raw_auth() {
                        socks = socks.with_auth(user.into(), pass.into());
                    }
                    Ok(Io(Box::new(socks.call(uri).await?)))
                }
                Some("socks4" | "socks4a") => {
                    let local = target.scheme_str() == Some("socks4");
                    let mut socks = SocksV4::new(target, tcp).local_dns(local);
                    Ok(Io(Box::new(socks.call(uri).await?)))
                }
                _ => Err("unsupported_proxy".into()),
            }
        })
    }
}

#[derive(Default)]
struct HeaderBudget {
    bytes: usize,
    fields: usize,
    failed: bool,
    interim: Vec<u16>,
}
impl HeaderBudget {
    fn observe(&mut self, headers: &HeaderMap, new_head: bool) -> bool {
        if new_head {
            self.fields = 0;
        }
        self.fields = self.fields.saturating_add(headers.len());
        if self.fields > 256 {
            self.failed = true;
        }
        for (name, value) in headers {
            self.bytes = self
                .bytes
                .saturating_add(name.as_str().len() + value.as_bytes().len() + 4);
            if self.bytes > 64 * 1024 {
                self.failed = true;
            }
        }
        !self.failed
    }
}
pub fn admit_head(status: u16, headers: &HeaderMap) -> Result<(), &'static str> {
    if status != 200 {
        return Err("http_status");
    }
    let values: Vec<_> = headers
        .get_all(hyper::header::CONTENT_TYPE)
        .iter()
        .collect();
    if values.len() != 1 {
        return Err("http_mime_count");
    }
    let value = values[0]
        .to_str()
        .map_err(|_| "http_mime")?
        .trim()
        .to_ascii_lowercase();
    let mut parts = value.splitn(2, ';');
    if parts.next().unwrap_or("").trim() != "text/event-stream" {
        return Err("http_mime");
    }
    if let Some(parameter) = parts.next()
        && !matches!(parameter.trim(), "charset=utf-8" | "charset=\"utf-8\"")
    {
        return Err("http_mime_parameter");
    }
    Ok(())
}

pub struct Probe {
    pub url: String,
    pub ca_der: Option<Vec<u8>>,
    pub proxy_env: BTreeMap<String, String>,
    pub body_limit: usize,
    pub h2_only: bool,
}
#[derive(Default)]
struct Driver(Option<tokio::task::JoinHandle<()>>);
impl Drop for Driver {
    fn drop(&mut self) {
        // External cancellation must also close the connection. Normal fetch
        // completion aborts and joins below; a dropped future cannot await.
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
pub async fn fetch(spec: Probe) -> serde_json::Value {
    let mut driver = Driver::default();
    let received = Arc::new(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        exchange(spec, &mut driver, &received),
    )
    .await;
    if let Some(driver) = driver.0.take() {
        driver.abort();
        let _ = driver.await;
    }
    match result {
        Ok(Ok(value)) => value,
        _ => {
            serde_json::json!({"accepted":false,"executors":0,"uncommitted_body_bytes":received.load(Ordering::Relaxed)})
        }
    }
}
async fn exchange(
    spec: Probe,
    driver: &mut Driver,
    received: &AtomicUsize,
) -> Result<serde_json::Value, BoxError> {
    let uri: Uri = spec.url.parse()?;
    if !matches!(uri.host(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || !matches!(uri.scheme_str(), Some("http" | "https"))
        || spec.body_limit == 0
        || spec.body_limit > 16 * 1024 * 1024
    {
        return Err("probe_contract".into());
    }
    let proxy = proxy_for(&uri, &spec.proxy_env)?;
    let forward = uri.scheme_str() == Some("http")
        && proxy
            .as_ref()
            .is_some_and(|p| matches!(p.uri().scheme_str(), Some("http" | "https")));
    let forward_auth = if forward {
        proxy.as_ref().and_then(|p| p.basic_auth()).cloned()
    } else {
        None
    };
    let mut roots = rustls::RootCertStore::empty();
    if let Some(der) = spec.ca_der {
        roots.add(rustls::pki_types::CertificateDer::from(der))?;
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let mut connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls.clone())
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(Route { proxy, tls });
    let stream = connector.call(uri.clone()).await?;
    let h2 = stream.connected().is_negotiated_h2() || spec.h2_only;
    let wire_bytes = Arc::new(AtomicUsize::new(0));
    let stream = hyper_util::rt::TokioIo::new(Counted {
        inner: hyper_util::rt::TokioIo::new(stream),
        bytes: wire_bytes.clone(),
        limit: if h2 {
            usize::MAX
        } else {
            spec.body_limit + 64 * 1024
        },
    });
    let budget = Arc::new(Mutex::new(HeaderBudget::default()));
    let (failure, mut failed) = tokio::sync::watch::channel(false);
    let _failure_lifetime = failure.clone();
    let mut request = Request::builder()
        .method("POST")
        .uri(if h2 || forward {
            uri.clone()
        } else {
            uri.path_and_query()
                .map(|v| v.as_str())
                .unwrap_or("/")
                .parse()?
        })
        .header(
            hyper::header::HOST,
            uri.authority().ok_or("authority")?.as_str(),
        )
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"stream":true,"store":false}"#,
        )))?;
    if let Some(auth) = forward_auth {
        request
            .headers_mut()
            .insert(hyper::header::PROXY_AUTHORIZATION, auth);
    }
    let info_budget = budget.clone();
    hyper::ext::on_informational(&mut request, move |head| {
        let mut guard = info_budget.lock().expect("header budget lock");
        if guard.interim.len() >= 256 {
            guard.failed = true;
        }
        if guard.observe(head.headers(), true) {
            guard.interim.push(head.status().as_u16());
        }
        if guard.failed {
            let _ = failure.send(true);
        }
    });
    let response = if h2 {
        let (mut sender, connection) =
            hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .max_header_list_size(64 * 1024)
                .handshake(stream)
                .await?;
        driver.0 = Some(tokio::spawn(async move {
            let _ = connection.await;
        }));
        tokio::select! { result = sender.send_request(request) => result?, _ = failed.changed() => return Err("informational_header_limit".into()) }
    } else {
        let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
            .max_headers(256)
            .max_buf_size(64 * 1024)
            .handshake(stream)
            .await?;
        driver.0 = Some(tokio::spawn(async move {
            let _ = connection.await;
        }));
        tokio::select! { result = sender.send_request(request) => result?, _ = failed.changed() => return Err("informational_header_limit".into()) }
    };
    {
        let mut guard = budget.lock().expect("header budget lock");
        if !guard.observe(response.headers(), true) {
            return Err("header_limit".into());
        }
    }
    admit_head(response.status().as_u16(), response.headers())?;
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    let mut trailers = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame?; // A terminal-looking body is not success before transport EOF.
        match frame.into_data() {
            Ok(data) => {
                if data.len() > spec.body_limit - bytes.len() {
                    return Err("body_limit".into());
                }
                bytes.extend_from_slice(&data);
                received.store(bytes.len(), Ordering::Relaxed);
            }
            Err(frame) => {
                if let Ok(headers) = frame.into_trailers() {
                    if headers.contains_key(hyper::header::CONTENT_TYPE)
                        || !budget
                            .lock()
                            .expect("header budget lock")
                            .observe(&headers, false)
                    {
                        return Err("forbidden_trailer".into());
                    }
                    for (key, value) in &headers {
                        trailers.push(format!("{}:{}", key.as_str(), value.to_str()?));
                    }
                }
            }
        }
    }
    // No additional HTTP parser: account every decrypted H1 wire byte, including
    // reason phrases, OWS and trailers. Chunk framing conservatively consumes
    // the same 64KiB allowance; BODY bytes alone are excluded. Check before
    // returning success, after complete transport EOF.
    if !h2
        && wire_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(bytes.len())
            > 64 * 1024
    {
        return Err("wire_header_limit".into());
    }
    trailers.sort();
    let interim = budget.lock().expect("header budget lock").interim.clone();
    Ok(
        serde_json::json!({"accepted":true,"executors":0,"body_hex":bytes.iter().map(|v| format!("{v:02x}")).collect::<String>(),"interim":interim,"trailers":trailers}),
    )
}
