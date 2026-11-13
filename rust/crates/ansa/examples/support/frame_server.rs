//! Synthetic local server shared by the M1 example and integration tests.
use http_body_util::BodyExt;
use hyper::{
    Request, Response,
    body::{Body, Bytes, Frame, Incoming},
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::future::Future;
use std::{
    collections::VecDeque,
    convert::Infallible,
    io,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

struct Frames(
    VecDeque<Result<Frame<Bytes>, io::Error>>,
    Option<Pin<Box<tokio::time::Sleep>>>,
);
impl Body for Frames {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        if this.0.front().is_some_and(Result::is_err) {
            let delay = this.1.get_or_insert_with(|| {
                Box::pin(tokio::time::sleep(std::time::Duration::from_millis(100)))
            });
            if delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
        }
        Poll::Ready(this.0.pop_front())
    }
    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }
}
async fn respond(
    mut request: Request<Incoming>,
    case: String,
    body: Bytes,
    observation: Option<Arc<Mutex<Option<serde_json::Value>>>>,
) -> Result<Response<Frames>, Infallible> {
    let mut received = Vec::new();
    while let Some(frame) = request.body_mut().frame().await {
        if let Ok(frame) = frame {
            if let Some(bytes) = frame.data_ref() {
                if received.len() + bytes.len() > 1024 * 1024 {
                    break;
                }
                received.extend_from_slice(bytes);
            }
        } else {
            break;
        }
    }
    if let Some(observation) = observation {
        let headers = request.headers();
        let values = |key: &str| {
            headers
                .get_all(key)
                .iter()
                .map(|v| v.to_str().unwrap_or("invalid").to_owned())
                .collect::<Vec<_>>()
        };
        *observation.lock().unwrap() = Some(
            serde_json::json!({"method":request.method().as_str(),"target":request.uri().path_and_query().map_or("/",|v|v.as_str()),"authority":request.uri().authority().map(|v|v.as_str()),"request_body_hex":received.iter().map(|v|format!("{v:02x}")).collect::<String>(),"authorization_present":headers.contains_key("authorization"),"authorization_matches":values("authorization")==["Bearer synthetic-bridge-bearer"],"content_types":values("content-type"),"hosts":values("host")}),
        );
    }
    let mut frames = VecDeque::from([Ok(Frame::data(body))]);
    if case == "h2-trailer" {
        let mut trailer = hyper::HeaderMap::new();
        trailer.insert("x-check", hyper::header::HeaderValue::from_static("1"));
        frames.push_back(Ok(Frame::trailers(trailer)));
    } else if case == "h2-mime-trailer" {
        let mut trailer = hyper::HeaderMap::new();
        trailer.insert(
            "content-type",
            hyper::header::HeaderValue::from_static("text/event-stream"),
        );
        frames.push_back(Ok(Frame::trailers(trailer)));
    } else if case == "h2-reset" {
        frames.push_back(Err(io::Error::other("synthetic_transfer_failure")));
    }
    let mut response = Response::new(Frames(frames, None));
    response.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    if case == "h2-duplicate-mime" {
        response.headers_mut().append(
            "content-type",
            hyper::header::HeaderValue::from_static("text/event-stream"),
        );
    }
    if case == "h2-large-headers" {
        response.headers_mut().insert(
            "x-large",
            hyper::header::HeaderValue::from_str(&"a".repeat(70 * 1024)).expect("fixture value"),
        );
    }
    Ok(response)
}
pub async fn serve(
    listener: tokio::net::TcpListener,
    fixture: &Path,
    case: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_body(
        listener,
        fixture,
        case,
        Bytes::from_static(b"data: response.completed\n\n"),
    )
    .await
}
pub async fn serve_body(
    listener: tokio::net::TcpListener,
    fixture: &Path,
    case: &str,
    body: Bytes,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_recorded(listener, fixture, case, body, None).await
}
pub async fn serve_recorded(
    listener: tokio::net::TcpListener,
    fixture: &Path,
    case: &str,
    body: Bytes,
    observation: Option<Arc<Mutex<Option<serde_json::Value>>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cert = rustls::pki_types::CertificateDer::from(std::fs::read(
        fixture.join("transport/tls/server.der"),
    )?);
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(std::fs::read(
        fixture.join("transport/tls/server-key.der"),
    )?);
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
    .with_no_client_auth()
    .with_single_cert(vec![cert], key.into())?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let (socket, _) = listener.accept().await?;
    let stream = acceptor.accept(socket).await?;
    let case = case.to_owned();
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .serve_connection(
            TokioIo::new(stream),
            hyper::service::service_fn(move |request| {
                respond(request, case.clone(), body.clone(), observation.clone())
            }),
        )
        .await?;
    Ok(())
}
