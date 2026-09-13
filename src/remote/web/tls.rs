//! Optional LAN HTTPS/WSS material and verified direct-transport markers.
//!
//! Certificate generation, browser trust-store import, and proxy-header trust
//! are intentionally out of scope. Configured PEM material is validated with
//! rustls [`WebPkiServerVerifier`] against the **last** configured certificate as
//! the sole trust anchor; the HTTPS marker is minted only after a completed
//! rustls server handshake. Connection admission spans the HTTP future **and**
//! any Axum `on_upgrade` lifetime via an owned semaphore permit on the TLS I/O.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufReader, Cursor};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use rustls::client::danger::ServerCertVerifier;
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ServerConfig;
use rustls::RootCertStore;
use rustls_pemfile::{certs, private_key};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use url::Url;

/// Hard bound for persisted advertised HTTPS origin text before parse.
pub const MAX_ADVERTISED_ORIGIN_BYTES: usize = 2048;
/// Hard bound for persisted certificate PEM text before parse.
pub const MAX_CERTIFICATE_PEM_BYTES: usize = 128 * 1024;
/// Hard bound for persisted private key PEM text before parse.
pub const MAX_PRIVATE_KEY_PEM_BYTES: usize = 64 * 1024;

const MAX_TLS_CONNECTIONS: usize = 64;
const TLS_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);
const HTTP_HEADER_READ_DEADLINE: Duration = Duration::from_secs(10);
const SHUTDOWN_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// User-supplied persisted HTTPS listener material for the LAN web bind.
///
/// Private key material is never printed via [`Debug`]. Field lengths are
/// enforced both by a bounded serde visitor and by [`prepare_web_tls`].
#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebTlsConfig {
    pub advertised_origin: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
}

impl fmt::Debug for WebTlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebTlsConfig")
            .field("advertised_origin", &self.advertised_origin)
            .field("certificate_pem_bytes", &self.certificate_pem.len())
            .field("private_key_pem", &"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for WebTlsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "advertisedOrigin",
            "advertised_origin",
            "certificatePem",
            "certificate_pem",
            "privateKeyPem",
            "private_key_pem",
        ];

        struct WebTlsConfigVisitor;

        impl<'de> Visitor<'de> for WebTlsConfigVisitor {
            type Value = WebTlsConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a bounded WebTlsConfig object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut advertised_origin = None;
                let mut certificate_pem = None;
                let mut private_key_pem = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "advertisedOrigin" | "advertised_origin" => {
                            if advertised_origin.is_some() {
                                return Err(de::Error::duplicate_field("advertisedOrigin"));
                            }
                            let value = map.next_value::<String>()?;
                            if value.len() > MAX_ADVERTISED_ORIGIN_BYTES {
                                return Err(de::Error::custom(format!(
                                    "advertisedOrigin exceeds {MAX_ADVERTISED_ORIGIN_BYTES} bytes"
                                )));
                            }
                            advertised_origin = Some(value);
                        }
                        "certificatePem" | "certificate_pem" => {
                            if certificate_pem.is_some() {
                                return Err(de::Error::duplicate_field("certificatePem"));
                            }
                            let value = map.next_value::<String>()?;
                            if value.len() > MAX_CERTIFICATE_PEM_BYTES {
                                return Err(de::Error::custom(format!(
                                    "certificatePem exceeds {MAX_CERTIFICATE_PEM_BYTES} bytes"
                                )));
                            }
                            certificate_pem = Some(value);
                        }
                        "privateKeyPem" | "private_key_pem" => {
                            if private_key_pem.is_some() {
                                return Err(de::Error::duplicate_field("privateKeyPem"));
                            }
                            let value = map.next_value::<String>()?;
                            if value.len() > MAX_PRIVATE_KEY_PEM_BYTES {
                                return Err(de::Error::custom(format!(
                                    "privateKeyPem exceeds {MAX_PRIVATE_KEY_PEM_BYTES} bytes"
                                )));
                            }
                            private_key_pem = Some(value);
                        }
                        other => {
                            return Err(de::Error::unknown_field(other, FIELDS));
                        }
                    }
                }
                Ok(WebTlsConfig {
                    advertised_origin: advertised_origin
                        .ok_or_else(|| de::Error::missing_field("advertisedOrigin"))?,
                    certificate_pem: certificate_pem
                        .ok_or_else(|| de::Error::missing_field("certificatePem"))?,
                    private_key_pem: private_key_pem
                        .ok_or_else(|| de::Error::missing_field("privateKeyPem"))?,
                })
            }
        }

        deserializer.deserialize_struct("WebTlsConfig", FIELDS, WebTlsConfigVisitor)
    }
}

/// Request extension minted only from the actual accept path.
///
/// There is no public constructor: HTTPS markers come from a completed rustls
/// handshake; plain markers come from the non-TLS listener path. Client
/// `Forwarded` / `X-Forwarded-*` headers cannot produce this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedDirectTransport {
    scheme: &'static str,
    advertised_authority: String,
    is_tls: bool,
}

impl VerifiedDirectTransport {
    pub fn scheme(&self) -> &'static str {
        self.scheme
    }

    pub fn advertised_authority(&self) -> &str {
        &self.advertised_authority
    }

    pub fn is_tls(&self) -> bool {
        self.is_tls
    }

    pub(super) fn mint_after_rustls_handshake(advertised_authority: String) -> Self {
        Self {
            scheme: "https",
            advertised_authority,
            is_tls: true,
        }
    }

    pub(super) fn mint_plain(advertised_authority: String) -> Self {
        Self {
            scheme: "http",
            advertised_authority,
            is_tls: false,
        }
    }
}

/// Validated TLS listener material ready for bind.
#[derive(Clone)]
pub(crate) struct PreparedWebTls {
    pub server_config: Arc<ServerConfig>,
    pub advertised_origin: String,
    pub advertised_authority: String,
    pub advertised_host: String,
}

impl fmt::Debug for PreparedWebTls {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedWebTls")
            .field("advertised_origin", &self.advertised_origin)
            .finish_non_exhaustive()
    }
}

impl PreparedWebTls {
    pub fn advertised_host(&self) -> &str {
        &self.advertised_host
    }
}

fn tls_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Parse, bound, and verify user-supplied web TLS material before bind.
pub(crate) fn prepare_web_tls(config: &WebTlsConfig) -> Result<PreparedWebTls, String> {
    if config.advertised_origin.len() > MAX_ADVERTISED_ORIGIN_BYTES {
        return Err(format!(
            "web TLS advertisedOrigin exceeds {MAX_ADVERTISED_ORIGIN_BYTES} bytes"
        ));
    }
    if config.certificate_pem.len() > MAX_CERTIFICATE_PEM_BYTES {
        return Err(format!(
            "web TLS certificatePem exceeds {MAX_CERTIFICATE_PEM_BYTES} bytes"
        ));
    }
    if config.private_key_pem.len() > MAX_PRIVATE_KEY_PEM_BYTES {
        return Err(format!(
            "web TLS privateKeyPem exceeds {MAX_PRIVATE_KEY_PEM_BYTES} bytes"
        ));
    }

    let parsed = parse_advertised_https_origin(&config.advertised_origin)?;
    let cert_chain = parse_cert_chain(&config.certificate_pem)?;
    let key_der = parse_private_key(&config.private_key_pem)?;
    verify_configured_certificate_chain(&cert_chain, &parsed.advertised_host)?;

    let server_config = ServerConfig::builder_with_provider(tls_crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("web TLS protocol versions failed: {error}"))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|error| format!("web TLS certificate/key mismatch: {error}"))?;

    Ok(PreparedWebTls {
        server_config: Arc::new(server_config),
        advertised_origin: parsed.advertised_origin,
        advertised_authority: parsed.advertised_authority,
        advertised_host: parsed.advertised_host,
    })
}

struct ParsedAdvertisedOrigin {
    advertised_origin: String,
    advertised_authority: String,
    advertised_host: String,
}

/// Cheap presentation-only normalization. Certificate and key verification
/// belongs to listener preparation, never to repeatedly rendering settings.
pub(super) fn display_advertised_origin(raw: &str) -> Option<String> {
    parse_advertised_https_origin(raw)
        .ok()
        .map(|parsed| parsed.advertised_origin)
}

fn parse_advertised_https_origin(raw: &str) -> Result<ParsedAdvertisedOrigin, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("web TLS advertisedOrigin is empty".to_string());
    }
    let url = Url::parse(trimmed)
        .map_err(|error| format!("web TLS advertisedOrigin parse failed: {error}"))?;
    if url.scheme() != "https" {
        return Err("web TLS advertisedOrigin must use the https scheme".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("web TLS advertisedOrigin must not include credentials".to_string());
    }
    if url.query().is_some() {
        return Err("web TLS advertisedOrigin must not include a query".to_string());
    }
    if url.fragment().is_some() {
        return Err("web TLS advertisedOrigin must not include a fragment".to_string());
    }
    let path = url.path();
    if path != "/" && !path.is_empty() {
        return Err("web TLS advertisedOrigin must not include a path".to_string());
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "web TLS advertisedOrigin is missing a port".to_string())?;
    // Normalize via url.host(): IPv6 is bracket-free for ServerName / SAN checks.
    let (advertised_host, advertised_authority) = match url.host() {
        Some(url::Host::Domain(domain)) => {
            if domain.is_empty() {
                return Err("web TLS advertisedOrigin is missing a host".to_string());
            }
            (domain.to_string(), format!("{domain}:{port}"))
        }
        Some(url::Host::Ipv4(ip)) => (ip.to_string(), format!("{ip}:{port}")),
        Some(url::Host::Ipv6(ip)) => (ip.to_string(), format!("[{ip}]:{port}")),
        None => return Err("web TLS advertisedOrigin is missing a host".to_string()),
    };
    let advertised_origin = format!("https://{advertised_authority}");
    Ok(ParsedAdvertisedOrigin {
        advertised_origin,
        advertised_authority,
        advertised_host,
    })
}

fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    let certs = certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("web TLS certificate parse failed: {error}"))?;
    if certs.is_empty() {
        return Err("web TLS certificate chain is empty".to_string());
    }
    Ok(certs)
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    private_key(&mut reader)
        .map_err(|error| format!("web TLS private key parse failed: {error}"))?
        .ok_or_else(|| "web TLS private key is missing".to_string())
}

fn verify_configured_certificate_chain(
    cert_chain: &[CertificateDer<'static>],
    advertised_host: &str,
) -> Result<(), String> {
    let trust_anchor = cert_chain
        .last()
        .ok_or_else(|| "web TLS certificate chain is empty".to_string())?;
    let mut roots = RootCertStore::empty();
    roots
        .add(trust_anchor.clone())
        .map_err(|error| format!("web TLS trust anchor rejected: {error}"))?;
    let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), tls_crypto_provider())
            .build()
            .map_err(|error| format!("web TLS verifier build failed: {error}"))?;

    let server_name = ServerName::try_from(advertised_host.to_string()).map_err(|_| {
        format!("web TLS advertised host is not a valid server name: {advertised_host}")
    })?;
    let end_entity = cert_chain
        .first()
        .ok_or_else(|| "web TLS certificate chain is empty".to_string())?;
    // Intermediates are everything between the leaf and the trust-anchor root.
    let intermediates = if cert_chain.len() > 1 {
        &cert_chain[1..cert_chain.len() - 1]
    } else {
        &[][..]
    };
    verifier
        .verify_server_cert(
            end_entity,
            intermediates,
            &server_name,
            &[],
            UnixTime::now(),
        )
        .map_err(|error| format!("web TLS certificate validation failed: {error}"))?;
    Ok(())
}

/// Strip client-supplied forwarding headers and set proto from the verified path.
pub(super) fn apply_verified_transport(
    headers: &mut HeaderMap,
    transport: &VerifiedDirectTransport,
) {
    strip_client_forwarded_headers(headers);
    let proto = if transport.is_tls() { "https" } else { "http" };
    if let Ok(value) = HeaderValue::from_str(proto) {
        headers.insert(HeaderName::from_static("x-forwarded-proto"), value);
    }
}

fn strip_client_forwarded_headers(headers: &mut HeaderMap) {
    headers.remove(HeaderName::from_static("forwarded"));
    let forwarded: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            let lower = name.as_str();
            lower == "forwarded" || lower.starts_with("x-forwarded-")
        })
        .cloned()
        .collect();
    for name in forwarded {
        headers.remove(name);
    }
}

/// Axum middleware for the plain TCP listener: injects a non-TLS marker.
pub(super) async fn inject_plain_verified_transport(
    mut request: Request,
    next: Next,
    advertised_authority: String,
) -> Response {
    let transport = VerifiedDirectTransport::mint_plain(advertised_authority);
    apply_verified_transport(request.headers_mut(), &transport);
    request.extensions_mut().insert(transport);
    next.run(request).await
}

/// Cloned std sockets used to wake blocked upgraded I/O on shutdown.
struct ConnectionRegistry {
    next_id: AtomicU64,
    sockets: Mutex<HashMap<u64, std::net::TcpStream>>,
}

impl ConnectionRegistry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sockets: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, socket: std::net::TcpStream) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.sockets.lock() {
            guard.insert(id, socket);
        }
        id
    }

    fn unregister(&self, id: u64) {
        if let Ok(mut guard) = self.sockets.lock() {
            guard.remove(&id);
        }
    }

    fn shutdown_all(&self) {
        let Ok(mut guard) = self.sockets.lock() else {
            return;
        };
        for (_, socket) in guard.drain() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// TLS I/O that carries admission for the HTTP **and** upgrade lifetime.
struct AdmittedTlsStream {
    inner: TlsStream<tokio::net::TcpStream>,
    _permit: OwnedSemaphorePermit,
    _registration: ConnectionRegistration,
}

/// Arm before the handshake await, then transfer through HTTP and upgrades.
struct ConnectionRegistration {
    registry: Arc<ConnectionRegistry>,
    registration_id: u64,
}

impl Drop for ConnectionRegistration {
    fn drop(&mut self) {
        self.registry.unregister(self.registration_id);
    }
}

impl AsyncRead for AdmittedTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for AdmittedTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn tcp_with_shutdown_handle(
    tcp: tokio::net::TcpStream,
) -> io::Result<(tokio::net::TcpStream, std::net::TcpStream)> {
    let std_tcp = tcp.into_std()?;
    std_tcp.set_nonblocking(true)?;
    let shutdown_handle = std_tcp.try_clone()?;
    shutdown_handle.set_nonblocking(true)?;
    let tcp = tokio::net::TcpStream::from_std(std_tcp)?;
    Ok((tcp, shutdown_handle))
}

/// Cancellation-owned TLS accept loop with semaphore-bounded concurrent connections.
pub(super) async fn serve_tls_accept_loop(
    listener: TcpListener,
    app: Router,
    prepared: PreparedWebTls,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let admission = Arc::new(Semaphore::new(MAX_TLS_CONNECTIONS));
    serve_tls_accept_loop_with_admission(listener, app, prepared, shutdown_rx, admission).await;
}

/// Same as [`serve_tls_accept_loop`] but shares the admission semaphore with tests.
pub(super) async fn serve_tls_accept_loop_with_admission(
    listener: TcpListener,
    app: Router,
    prepared: PreparedWebTls,
    mut shutdown_rx: oneshot::Receiver<()>,
    admission: Arc<Semaphore>,
) {
    let acceptor = TlsAcceptor::from(prepared.server_config.clone());
    let advertised_authority = prepared.advertised_authority.clone();
    let registry = Arc::new(ConnectionRegistry::new());
    // Abort/drop owns the same cleanup as an explicit stop. Upgraded WebSockets
    // can outlive their HTTP futures, so closing the underlying sockets is vital.
    struct CloseSockets(Arc<ConnectionRegistry>);
    impl Drop for CloseSockets {
        fn drop(&mut self) {
            self.0.shutdown_all();
        }
    }
    let _close_sockets = CloseSockets(Arc::clone(&registry));
    let mut connections: JoinSet<()> = JoinSet::new();

    loop {
        let permit = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            permit = admission.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let acceptor = acceptor.clone();
                let app = app.clone();
                let advertised_authority = advertised_authority.clone();
                let registry = registry.clone();
                connections.spawn(async move {
                    serve_one_tls_connection(
                        tcp,
                        peer,
                        acceptor,
                        app,
                        advertised_authority,
                        permit,
                        registry,
                    )
                    .await;
                });
            }
            Err(_) => drop(permit),
        }
    }
    registry.shutdown_all();
    // Release pending handshakes/HTTP tasks before waiting for upgrade owners.
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let _ = tokio::time::timeout(
        SHUTDOWN_DRAIN_DEADLINE,
        admission.acquire_many(MAX_TLS_CONNECTIONS as u32),
    )
    .await;
}

async fn serve_one_tls_connection(
    tcp: tokio::net::TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    app: Router,
    advertised_authority: String,
    permit: OwnedSemaphorePermit,
    registry: Arc<ConnectionRegistry>,
) {
    let _ = tcp.set_nodelay(true);
    let (tcp, shutdown_handle) = match tcp_with_shutdown_handle(tcp) {
        Ok(pair) => pair,
        Err(_) => return,
    };
    let registration_id = registry.register(shutdown_handle);
    let registration = ConnectionRegistration {
        registry,
        registration_id,
    };
    let tls = match tokio::time::timeout(TLS_HANDSHAKE_DEADLINE, acceptor.accept(tcp)).await {
        Ok(Ok(stream)) => stream,
        _ => return,
    };

    // Permit stays on the I/O object so Axum `on_upgrade` retains admission.
    let admitted = AdmittedTlsStream {
        inner: tls,
        _permit: permit,
        _registration: registration,
    };
    let transport = VerifiedDirectTransport::mint_after_rustls_handshake(advertised_authority);
    let service = hyper_util::service::TowerToHyperService::new(TlsConnectionService {
        app,
        peer,
        transport,
    });
    let io = hyper_util::rt::TokioIo::new(admitted);
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(hyper_util::rt::TokioTimer::new());
    builder.header_read_timeout(HTTP_HEADER_READ_DEADLINE);
    // After this future ends, upgraded sockets still own `AdmittedTlsStream`.
    let _ = builder.serve_connection(io, service).with_upgrades().await;
}

#[derive(Clone)]
struct TlsConnectionService {
    app: Router,
    peer: SocketAddr,
    transport: VerifiedDirectTransport,
}

impl tower::Service<hyper::Request<hyper::body::Incoming>> for TlsConnectionService {
    type Response = Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: hyper::Request<hyper::body::Incoming>) -> Self::Future {
        let mut app = self.app.clone();
        let peer = self.peer;
        let transport = self.transport.clone();
        Box::pin(async move {
            let mut request = request.map(Body::new);
            apply_verified_transport(request.headers_mut(), &transport);
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(transport);
            Ok(tower::Service::call(&mut app, request)
                .await
                .unwrap_or_else(|error| match error {}))
        })
    }
}

/// Build a rustls client that trusts only the last PEM certificate (test helper).
#[cfg(test)]
fn client_config_trusting_pem(certificate_pem: &str) -> Result<Arc<rustls::ClientConfig>, String> {
    let certs = parse_cert_chain(certificate_pem)?;
    let trust_anchor = certs
        .last()
        .ok_or_else(|| "test certificate chain is empty".to_string())?;
    let mut roots = RootCertStore::empty();
    roots
        .add(trust_anchor.clone())
        .map_err(|error| format!("test trust anchor rejected: {error}"))?;
    let config = rustls::ClientConfig::builder_with_provider(tls_crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("test client TLS config failed: {error}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use futures_util::{SinkExt, StreamExt};
    use rcgen::{
        date_time_ymd, generate_simple_self_signed, BasicConstraints, CertificateParams, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    fn origin_for(host: &str, port: u16) -> String {
        if host.contains(':') && !host.starts_with('[') {
            format!("https://[{host}]:{port}")
        } else {
            format!("https://{host}:{port}")
        }
    }

    fn self_signed_material(san_host: &str) -> (String, String) {
        let certified = generate_simple_self_signed(vec![san_host.to_string()])
            .expect("self-signed test certificate");
        (certified.cert.pem(), certified.key_pair.serialize_pem())
    }

    fn ca_signed_leaf_material(san_host: &str) -> (String, String, String) {
        let ca_key = KeyPair::generate().expect("ca key");
        let mut ca_params = CertificateParams::new(vec!["Test CA".to_string()]).expect("ca params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

        let leaf_key = KeyPair::generate().expect("leaf key");
        let mut leaf_params =
            CertificateParams::new(vec![san_host.to_string()]).expect("leaf params");
        leaf_params
            .key_usages
            .push(KeyUsagePurpose::DigitalSignature);
        let leaf_cert = leaf_params
            .signed_by(&leaf_key, &ca_cert, &ca_key)
            .expect("leaf cert");

        let chain_pem = format!("{}{}", leaf_cert.pem(), ca_cert.pem());
        (chain_pem, leaf_key.serialize_pem(), ca_cert.pem())
    }

    #[test]
    fn wrong_san_is_rejected_before_bind() {
        let (certificate_pem, private_key_pem) = self_signed_material("other.example");
        let error = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("expected.example", 8443),
            certificate_pem,
            private_key_pem,
        })
        .expect_err("wrong SAN must fail closed");
        assert!(
            error.to_ascii_lowercase().contains("certificate")
                || error.to_ascii_lowercase().contains("name"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn expired_certificate_is_rejected() {
        let key_pair = KeyPair::generate().expect("key");
        let mut params =
            CertificateParams::new(vec!["expired.example".to_string()]).expect("params");
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2020, 12, 31);
        let cert = params.self_signed(&key_pair).expect("expired cert");
        let error = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("expired.example", 8443),
            certificate_pem: cert.pem(),
            private_key_pem: key_pair.serialize_pem(),
        })
        .expect_err("expired certificate must fail closed");
        assert!(
            error.to_ascii_lowercase().contains("certificate")
                || error.to_ascii_lowercase().contains("valid"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn malformed_certificate_and_key_are_rejected() {
        assert!(prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("host.example", 8443),
            certificate_pem: "not-a-cert".to_string(),
            private_key_pem: "not-a-key".to_string(),
        })
        .is_err());
        let (certificate_pem, _) = self_signed_material("host.example");
        assert!(prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("host.example", 8443),
            certificate_pem,
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nQQ==\n-----END PRIVATE KEY-----\n"
                .to_string(),
        })
        .is_err());
    }

    #[test]
    fn mismatched_private_key_is_rejected() {
        let (certificate_pem, _) = self_signed_material("match.example");
        let (_, other_key) = self_signed_material("match.example");
        let error = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("match.example", 8443),
            certificate_pem,
            private_key_pem: other_key,
        })
        .expect_err("mismatched key must fail");
        assert!(
            error.to_ascii_lowercase().contains("mismatch")
                || error.to_ascii_lowercase().contains("key"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn oversized_inputs_are_rejected_before_parse() {
        let oversized_origin = "https://".to_string() + &"a".repeat(MAX_ADVERTISED_ORIGIN_BYTES);
        assert!(prepare_web_tls(&WebTlsConfig {
            advertised_origin: oversized_origin,
            certificate_pem: String::new(),
            private_key_pem: String::new(),
        })
        .is_err());
    }

    #[test]
    fn serde_rejects_oversized_private_key_field() {
        let huge = "x".repeat(MAX_PRIVATE_KEY_PEM_BYTES + 1);
        let json = format!(
            r#"{{"advertisedOrigin":"https://host.example:443","certificatePem":"c","privateKeyPem":"{huge}"}}"#
        );
        let error = serde_json::from_str::<WebTlsConfig>(&json)
            .expect_err("oversized privateKeyPem must fail at deserialize");
        assert!(
            error.to_string().contains("privateKeyPem") || error.to_string().contains("exceeds"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn https_origin_rejects_credentials_path_query_and_hash() {
        let (certificate_pem, private_key_pem) = self_signed_material("host.example");
        for origin in [
            "https://user:pass@host.example:8443",
            "https://host.example:8443/path",
            "https://host.example:8443?x=1",
            "https://host.example:8443#frag",
            "http://host.example:8443",
        ] {
            assert!(
                prepare_web_tls(&WebTlsConfig {
                    advertised_origin: origin.to_string(),
                    certificate_pem: certificate_pem.clone(),
                    private_key_pem: private_key_pem.clone(),
                })
                .is_err(),
                "origin should be rejected: {origin}"
            );
        }
    }

    #[test]
    fn ipv6_and_default_https_port_normalize_for_server_name() {
        let (certificate_pem, private_key_pem) = self_signed_material("::1");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: "https://[::1]:8443".to_string(),
            certificate_pem,
            private_key_pem,
        })
        .expect("IPv6 advertised origin");
        assert_eq!(prepared.advertised_host(), "::1");
        assert_eq!(prepared.advertised_authority, "[::1]:8443");
        assert!(!prepared.advertised_host().contains('['));

        let (certificate_pem, private_key_pem) = self_signed_material("lan.example");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: "https://lan.example".to_string(),
            certificate_pem,
            private_key_pem,
        })
        .expect("default https port");
        assert_eq!(prepared.advertised_authority, "lan.example:443");
        assert_eq!(prepared.advertised_origin, "https://lan.example:443");
    }

    #[test]
    fn certificate_chain_trusts_only_last_anchor() {
        let (chain_pem, leaf_key, _ca_pem) = ca_signed_leaf_material("leaf.example");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("leaf.example", 8443),
            certificate_pem: chain_pem,
            private_key_pem: leaf_key,
        })
        .expect("leaf+ca chain should validate with last cert as trust anchor");
        assert_eq!(prepared.advertised_host(), "leaf.example");
    }

    #[test]
    fn private_key_is_redacted_in_debug() {
        let config = WebTlsConfig {
            advertised_origin: origin_for("host.example", 8443),
            certificate_pem: "CERT".to_string(),
            private_key_pem: "SECRET_KEY_MATERIAL".to_string(),
        };
        let rendered = format!("{config:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("SECRET_KEY_MATERIAL"));
    }

    #[test]
    fn forwarded_tls_claim_does_not_manufacture_https_marker() {
        let mut headers = HeaderMap::new();
        headers.insert("forwarded", HeaderValue::from_static("proto=https"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert("x-forwarded-host", HeaderValue::from_static("evil.example"));
        let transport = VerifiedDirectTransport::mint_plain("localhost:43872".to_string());
        apply_verified_transport(&mut headers, &transport);
        assert!(!transport.is_tls());
        assert_eq!(transport.scheme(), "http");
        assert!(headers.get("forwarded").is_none());
        assert!(headers.get("x-forwarded-host").is_none());
        assert_eq!(
            headers
                .get("x-forwarded-proto")
                .and_then(|value| value.to_str().ok()),
            Some("http")
        );
    }

    #[tokio::test]
    async fn tls_client_handshake_serves_http_route_with_trusted_root() {
        let host = "127.0.0.1";
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (certificate_pem, private_key_pem) = self_signed_material(host);
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for(host, port),
            certificate_pem: certificate_pem.clone(),
            private_key_pem,
        })
        .expect("valid TLS material");

        let app = Router::new().route(
            "/api/health",
            get(|request: Request| async move {
                let marker = request
                    .extensions()
                    .get::<VerifiedDirectTransport>()
                    .cloned()
                    .expect("verified transport marker");
                assert!(marker.is_tls());
                assert_eq!(marker.scheme(), "https");
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"ok":true}"#,
                )
            }),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop(listener, app, prepared, shutdown_rx));

        let connector =
            TlsConnector::from(client_config_trusting_pem(&certificate_pem).expect("client trust"));
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("tcp connect");
        let server_name = ServerName::try_from(host.to_string()).expect("server name");
        let tls = connector
            .connect(server_name, tcp)
            .await
            .expect("tls handshake");
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
                .await
                .expect("http handshake");
        let connection_task = tokio::spawn(async move {
            let _ = connection.await;
        });
        let response = sender
            .send_request(
                hyper::Request::builder()
                    .method("GET")
                    .uri("/api/health")
                    .header("host", format!("{host}:{port}"))
                    .header("x-forwarded-proto", "http")
                    .body(http_body_util::Empty::<axum::body::Bytes>::new())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        drop(sender);
        let _ = connection_task.await;
        let _ = shutdown_tx.send(());
        let _ = server.await;
    }

    #[tokio::test]
    async fn cancelled_handshake_releases_registration_and_admission() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (certificate_pem, private_key_pem) = self_signed_material("127.0.0.1");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("127.0.0.1", port),
            certificate_pem,
            private_key_pem,
        })
        .unwrap();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let (tcp, peer) = listener.accept().await.unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        let registry = Arc::new(ConnectionRegistry::new());
        let handshake = tokio::spawn(serve_one_tls_connection(
            tcp,
            peer,
            TlsAcceptor::from(prepared.server_config),
            Router::new(),
            format!("127.0.0.1:{port}"),
            permit,
            registry.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while registry.sockets.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handshake.abort();
        assert!(handshake.await.unwrap_err().is_cancelled());
        assert!(registry.sockets.lock().unwrap().is_empty());
        assert_eq!(permits.available_permits(), 1);
        drop(client);
    }

    #[tokio::test]
    async fn idle_tls_listener_observes_shutdown_while_waiting_for_accept() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (certificate_pem, private_key_pem) = self_signed_material("127.0.0.1");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("127.0.0.1", port),
            certificate_pem,
            private_key_pem,
        })
        .unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop(
            listener,
            Router::new(),
            prepared,
            stopped,
        ));
        tokio::task::yield_now().await;
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("idle accept must not swallow shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn plain_tcp_cannot_speak_http_to_tls_listener() {
        let host = "127.0.0.1";
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (certificate_pem, private_key_pem) = self_signed_material(host);
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for(host, port),
            certificate_pem,
            private_key_pem,
        })
        .expect("valid TLS material");
        let app = Router::new().route("/api/health", get(|| async { "ok" }));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop(listener, app, prepared, shutdown_rx));

        let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("tcp connect");
        tcp.write_all(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .ok();
        let mut buf = [0_u8; 64];
        let read = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) | Err(_) => {}
            Ok(Ok(n)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                assert!(
                    !text.starts_with("HTTP/1."),
                    "plain TCP must not receive an HTTP response from the TLS listener: {text}"
                );
            }
            Ok(Err(_)) => {}
        }

        let _ = shutdown_tx.send(());
        let _ = server.await;
    }

    #[tokio::test]
    async fn excessive_connections_are_bounded() {
        let host = "127.0.0.1";
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (certificate_pem, private_key_pem) = self_signed_material(host);
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for(host, port),
            certificate_pem: certificate_pem.clone(),
            private_key_pem,
        })
        .expect("valid TLS material");
        let active = StdArc::new(AtomicUsize::new(0));
        let peak = StdArc::new(AtomicUsize::new(0));
        let active_task = active.clone();
        let peak_task = peak.clone();
        let app = Router::new().route(
            "/hold",
            get(move || {
                let active = active_task.clone();
                let peak = peak_task.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    "ok"
                }
            }),
        );
        let admission = Arc::new(Semaphore::new(MAX_TLS_CONNECTIONS));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop_with_admission(
            listener,
            app,
            prepared,
            shutdown_rx,
            admission.clone(),
        ));
        let connector =
            TlsConnector::from(client_config_trusting_pem(&certificate_pem).expect("client trust"));

        let mut holds = JoinSet::new();
        for _ in 0..(MAX_TLS_CONNECTIONS + 8) {
            let connector = connector.clone();
            holds.spawn(async move {
                let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .ok()?;
                let server_name = ServerName::try_from(host.to_string()).ok()?;
                let tls = connector.connect(server_name, tcp).await.ok()?;
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls))
                        .await
                        .ok()?;
                let connection_task = tokio::spawn(async move {
                    let _ = connection.await;
                });
                let _ = sender
                    .send_request(
                        hyper::Request::builder()
                            .uri("/hold")
                            .header("host", format!("{host}:{port}"))
                            .body(http_body_util::Empty::<axum::body::Bytes>::new())
                            .ok()?,
                    )
                    .await;
                drop(sender);
                let _ = connection_task.await;
                Some(())
            });
        }
        while holds.join_next().await.is_some() {}
        assert!(
            peak.load(Ordering::SeqCst) <= MAX_TLS_CONNECTIONS,
            "peak concurrent TLS application work exceeded bound: {}",
            peak.load(Ordering::SeqCst)
        );
        let _ = shutdown_tx.send(());
        let _ = server.await;
    }

    async fn hold_websocket(mut socket: WebSocket) {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn websocket_upgrade_keeps_admission_and_shutdown_drops_io() {
        let host = "127.0.0.1";
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (certificate_pem, private_key_pem) = self_signed_material(host);
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for(host, port),
            certificate_pem: certificate_pem.clone(),
            private_key_pem,
        })
        .expect("valid TLS material");
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(hold_websocket) }),
        );
        let admission = Arc::new(Semaphore::new(MAX_TLS_CONNECTIONS));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop_with_admission(
            listener,
            app,
            prepared,
            shutdown_rx,
            admission.clone(),
        ));
        let connector =
            TlsConnector::from(client_config_trusting_pem(&certificate_pem).expect("client trust"));

        let mut client_tasks = JoinSet::new();
        for _ in 0..MAX_TLS_CONNECTIONS {
            let connector = connector.clone();
            let (ready_tx, ready_rx) = oneshot::channel::<()>();
            client_tasks.spawn(async move {
                let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .expect("tcp");
                let server_name = ServerName::try_from(host.to_string()).expect("server name");
                let tls = connector.connect(server_name, tcp).await.expect("tls");
                let mut request = format!("wss://{host}:{port}/ws")
                    .into_client_request()
                    .expect("ws request");
                request
                    .headers_mut()
                    .insert("host", format!("{host}:{port}").parse().expect("host"));
                let (ws, _) = client_async(request, tls).await.expect("websocket upgrade");
                let _ = ready_tx.send(());
                let (mut write, mut read) = ws.split();
                // Retain the upgraded socket until the server cancels I/O.
                while let Some(item) = read.next().await {
                    if item.is_err() {
                        break;
                    }
                }
                let _ = write.close().await;
            });
            ready_rx.await.expect("upgrade ready");
        }
        assert_eq!(
            admission.available_permits(),
            0,
            "each upgraded WebSocket must retain its admission permit"
        );

        let sixty_fifth = tokio::spawn({
            let connector = connector.clone();
            async move {
                let Ok(tcp) = tokio::time::timeout(
                    Duration::from_millis(300),
                    tokio::net::TcpStream::connect(("127.0.0.1", port)),
                )
                .await
                else {
                    return false;
                };
                let Ok(tcp) = tcp else {
                    return false;
                };
                let Ok(server_name) = ServerName::try_from(host.to_string()) else {
                    return false;
                };
                let Ok(tls) = tokio::time::timeout(
                    Duration::from_millis(300),
                    connector.connect(server_name, tcp),
                )
                .await
                else {
                    return false;
                };
                let Ok(tls) = tls else {
                    return false;
                };
                let Ok(request) = format!("wss://{host}:{port}/ws").into_client_request() else {
                    return false;
                };
                tokio::time::timeout(Duration::from_millis(300), client_async(request, tls))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .is_some()
            }
        });
        let admitted_extra = tokio::time::timeout(Duration::from_secs(1), sixty_fifth)
            .await
            .expect("65th attempt join")
            .expect("65th attempt task");
        assert!(
            !admitted_extra,
            "65th upgraded connection must not admit while 64 permits are held"
        );

        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown must drain upgraded sockets")
            .expect("accept loop");
        while client_tasks.join_next().await.is_some() {}
        assert_eq!(
            admission.available_permits(),
            MAX_TLS_CONNECTIONS,
            "shutdown must release every upgraded admission permit"
        );
    }

    #[tokio::test]
    async fn shutdown_drops_retained_tls_socket_io() {
        let host = "127.0.0.1";
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (certificate_pem, private_key_pem) = self_signed_material(host);
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for(host, port),
            certificate_pem: certificate_pem.clone(),
            private_key_pem,
        })
        .expect("valid TLS material");
        let app = Router::new().route("/api/health", get(|| async { "ok" }));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve_tls_accept_loop(listener, app, prepared, shutdown_rx));

        let connector =
            TlsConnector::from(client_config_trusting_pem(&certificate_pem).expect("client trust"));
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("tcp connect");
        let server_name = ServerName::try_from(host.to_string()).expect("server name");
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .expect("tls handshake");
        // Keep the TLS socket open across shutdown so registry shutdown is proven.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("shutdown must join accept loop")
            .expect("accept loop task");

        let mut buf = [0_u8; 8];
        let read = tokio::time::timeout(Duration::from_secs(2), tls.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
            Ok(Ok(n)) => {
                panic!("retained TLS socket should be dropped on shutdown, read {n} bytes")
            }
        }
    }

    #[test]
    fn matching_san_material_prepares_lan_ready_config() {
        let (certificate_pem, private_key_pem) = self_signed_material("lan.example");
        let prepared = prepare_web_tls(&WebTlsConfig {
            advertised_origin: origin_for("lan.example", 8443),
            certificate_pem,
            private_key_pem,
        })
        .expect("matching SAN should prepare");
        assert_eq!(prepared.advertised_host(), "lan.example");
        assert_eq!(prepared.advertised_authority, "lan.example:8443");
        assert_eq!(prepared.advertised_origin, "https://lan.example:8443");
    }
}
