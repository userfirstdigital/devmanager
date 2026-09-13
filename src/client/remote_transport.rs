//! Production remote Connect transport: endpoint admission, bounded Hyper HTTP,
//! and TLS/WebSocket dial under one absolute deadline owner.
//!
//! Pairing cookies and Origin are validated before use. This module never
//! follows redirects, never disables certificate verification, never places
//! secrets in errors, and never detaches Hyper connection tasks.

use std::fmt;
use std::io::{BufReader, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::future::{select, Either};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::{HeaderValue, LOCATION};
use hyper::{Method, Request, StatusCode, Uri};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::WebSocketStream;
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::protocol::{MAX_HANDSHAKE_MESSAGE_BYTES, MAX_SEALED_FRAME_BYTES};
use crate::remote::web::WEB_COOKIE_NAME;

/// Default absolute bound covering DNS/TCP/TLS/HTTP/WS plus Noise/Hello owner.
pub const REMOTE_TRANSPORT_DEFAULT_DEADLINE: Duration = Duration::from_secs(15);
/// Hard cap for pairing POST and connect-meta response bodies.
pub const REMOTE_HTTP_MAX_BODY_BYTES: usize = 64 * 1024;
/// Hard cap for a single Set-Cookie / Cookie header value we will retain.
pub const REMOTE_COOKIE_MAX_BYTES: usize = 4 * 1024;
/// Hard cap for optional additional CA PEM text.
pub const REMOTE_CA_PEM_MAX_BYTES: usize = 128 * 1024;
/// Bound for Connect path text (matches published marker endpoint bound).
pub const REMOTE_CONNECT_PATH_MAX_BYTES: usize =
    crate::connect::CONNECT_WEB_MARKER_MAX_ENDPOINT_BYTES;

const CONNECT_DEFAULT_PATH: &str = "/api/connect";
const WEB_COOKIE_NAME_PREFIX: &str = "dm_web_";

/// Fail-closed remote transport errors. Never carry cookie, PEM, or URL secrets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransportError {
    Unauthorized,
    Timeout,
    Unavailable,
    Corrupt,
    Unsupported,
    RedirectForbidden,
    Oversized,
    Endpoint,
    Tls,
    Header,
    Cancelled,
}

impl fmt::Debug for RemoteTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for RemoteTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for RemoteTransportError {}

impl RemoteTransportError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "remote transport unauthorized",
            Self::Timeout => "remote transport deadline exceeded",
            Self::Unavailable => "remote transport unavailable",
            Self::Corrupt => "remote transport corrupt response",
            Self::Unsupported => "remote transport unsupported",
            Self::RedirectForbidden => "remote transport redirects are forbidden",
            Self::Oversized => "remote transport response exceeded bound",
            Self::Endpoint => "remote endpoint rejected",
            Self::Tls => "remote TLS handshake failed",
            Self::Header => "remote header value rejected",
            Self::Cancelled => "remote transport cancelled",
        }
    }
}

/// Validated remote endpoint. Development plaintext is exact `127.0.0.1` only.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    scheme: RemoteScheme,
    /// DNS / rustls ServerName host (IPv6 without brackets).
    host: String,
    port: u16,
    path: String,
    origin: String,
    http_base: String,
    ws_url: String,
}

impl fmt::Debug for RemoteEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteEndpoint")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteScheme {
    HttpLoopback,
    Https,
    WsLoopback,
    Wss,
}

impl RemoteScheme {
    const fn is_loopback_plaintext(self) -> bool {
        matches!(self, Self::HttpLoopback | Self::WsLoopback)
    }

    const fn requires_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss)
    }

    const fn http_scheme(self) -> &'static str {
        match self {
            Self::HttpLoopback | Self::WsLoopback => "http",
            Self::Https | Self::Wss => "https",
        }
    }

    const fn ws_scheme(self) -> &'static str {
        match self {
            Self::HttpLoopback | Self::WsLoopback => "ws",
            Self::Https | Self::Wss => "wss",
        }
    }

    const fn default_port(self) -> u16 {
        match self {
            Self::HttpLoopback | Self::WsLoopback => 80,
            Self::Https | Self::Wss => 443,
        }
    }
}

impl RemoteEndpoint {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn http_base(&self) -> &str {
        &self.http_base
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    pub fn requires_tls(&self) -> bool {
        self.scheme.requires_tls()
    }

    pub fn with_connect_path(&self, path: &str) -> Result<Self, RemoteTransportError> {
        validate_remote_path(path)?;
        let mut next = self.clone();
        next.path = path.to_string();
        next.ws_url = format!(
            "{}://{}{}",
            self.scheme.ws_scheme(),
            format_authority_for_url(&self.host, self.port, self.scheme.default_port()),
            path
        );
        Ok(next)
    }
}

/// Optional additional CA PEM (appended to webpki roots). Never an insecure skip.
#[derive(Clone, Default)]
pub struct RemoteTlsOptions {
    pub additional_ca_pem: Option<String>,
}

impl fmt::Debug for RemoteTlsOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteTlsOptions")
            .field(
                "additional_ca_pem_bytes",
                &self.additional_ca_pem.as_ref().map(String::len),
            )
            .finish()
    }
}

/// Typed TCP or TLS stream for `client_async`.
pub enum RemoteIo {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl AsyncRead for RemoteIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RemoteIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Validate a caller-supplied remote URL for trusted desktop Connect use.
pub fn validate_remote_endpoint(raw: &str) -> Result<RemoteEndpoint, RemoteTransportError> {
    if raw.len() > 2_048 || raw.is_empty() {
        return Err(RemoteTransportError::Endpoint);
    }
    if raw.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(RemoteTransportError::Endpoint);
    }
    if raw.contains('#') || raw.contains('@') {
        return Err(RemoteTransportError::Endpoint);
    }
    let parsed = Url::parse(raw).map_err(|_| RemoteTransportError::Endpoint)?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RemoteTransportError::Endpoint);
    }
    if parsed.fragment().is_some() || parsed.query().is_some() {
        return Err(RemoteTransportError::Endpoint);
    }
    let scheme = match parsed.scheme() {
        "http" => RemoteScheme::HttpLoopback,
        "https" => RemoteScheme::Https,
        "ws" => RemoteScheme::WsLoopback,
        "wss" => RemoteScheme::Wss,
        _ => return Err(RemoteTransportError::Endpoint),
    };
    let host = match parsed.host() {
        Some(Host::Domain(domain)) => domain.to_string(),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(RemoteTransportError::Endpoint),
    };
    if scheme.is_loopback_plaintext() && host != "127.0.0.1" {
        return Err(RemoteTransportError::Endpoint);
    }
    let port = parsed
        .port_or_known_default()
        .ok_or(RemoteTransportError::Endpoint)?;
    if port == 0 {
        return Err(RemoteTransportError::Endpoint);
    }
    let path = if parsed.path().is_empty() || parsed.path() == "/" {
        CONNECT_DEFAULT_PATH.to_string()
    } else {
        validate_remote_path(parsed.path())?;
        parsed.path().to_string()
    };
    let authority = format_authority_for_url(&host, port, scheme.default_port());
    let origin = format!("{}://{}", scheme.http_scheme(), authority);
    let http_base = origin.clone();
    let ws_url = format!("{}://{}{}", scheme.ws_scheme(), authority, path);
    Ok(RemoteEndpoint {
        scheme,
        host,
        port,
        path,
        origin,
        http_base,
        ws_url,
    })
}

fn validate_remote_path(path: &str) -> Result<(), RemoteTransportError> {
    if path.len() > REMOTE_CONNECT_PATH_MAX_BYTES {
        return Err(RemoteTransportError::Endpoint);
    }
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(RemoteTransportError::Endpoint);
    }
    if path.contains('\\')
        || path.contains('#')
        || path.contains('@')
        || path.contains('?')
        || path.contains("://")
        || path.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(RemoteTransportError::Endpoint);
    }
    Ok(())
}

fn format_authority_for_url(host: &str, port: u16, default_port: u16) -> String {
    let host = format_host_for_url(host);
    if port == default_port {
        host
    } else {
        format!("{host}:{port}")
    }
}

fn format_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Reject header values that could inject Cookie/Origin/CRLF sequences.
pub fn validate_http_header_value(value: &str) -> Result<(), RemoteTransportError> {
    if value.is_empty() || value.len() > REMOTE_COOKIE_MAX_BYTES {
        return Err(RemoteTransportError::Header);
    }
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(RemoteTransportError::Header);
    }
    HeaderValue::from_str(value).map_err(|_| RemoteTransportError::Header)?;
    Ok(())
}

fn is_expected_pairing_cookie_name(name: &str) -> bool {
    if name == WEB_COOKIE_NAME {
        return true;
    }
    let Some(suffix) = name.strip_prefix(WEB_COOKIE_NAME_PREFIX) else {
        return false;
    };
    suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Extract an expected DevManager pairing cookie (`dm_web` / `dm_web_<16hex>`).
pub fn extract_pairing_cookie_header(set_cookie: &str) -> Result<String, RemoteTransportError> {
    if set_cookie.len() > REMOTE_COOKIE_MAX_BYTES {
        return Err(RemoteTransportError::Oversized);
    }
    let pair = set_cookie.split(';').next().unwrap_or("").trim();
    if pair.is_empty() {
        return Err(RemoteTransportError::Corrupt);
    }
    let (name, value) = pair.split_once('=').ok_or(RemoteTransportError::Corrupt)?;
    if name.is_empty() || value.is_empty() || !is_expected_pairing_cookie_name(name) {
        return Err(RemoteTransportError::Corrupt);
    }
    let cookie = format!("{name}={value}");
    validate_http_header_value(&cookie)?;
    Ok(cookie)
}

/// Validate size and parseable certificate material. Never enables TLS bypass.
pub fn validate_additional_ca_pem(pem: &str) -> Result<(), RemoteTransportError> {
    if pem.is_empty() || pem.len() > REMOTE_CA_PEM_MAX_BYTES {
        return Err(if pem.len() > REMOTE_CA_PEM_MAX_BYTES {
            RemoteTransportError::Oversized
        } else {
            RemoteTransportError::Tls
        });
    }
    let mut roots = RootCertStore::empty();
    for cert in parse_cert_chain_pem(pem)? {
        roots.add(cert).map_err(|_| RemoteTransportError::Tls)?;
    }
    Ok(())
}

/// Build a rustls client config with webpki roots plus optional extra CA PEMs.
pub fn build_rustls_client_config(
    options: &RemoteTlsOptions,
) -> Result<Arc<rustls::ClientConfig>, RemoteTransportError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = options.additional_ca_pem.as_deref() {
        validate_additional_ca_pem(pem)?;
        for cert in parse_cert_chain_pem(pem)? {
            roots.add(cert).map_err(|_| RemoteTransportError::Tls)?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| RemoteTransportError::Tls)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn parse_cert_chain_pem(
    pem: &str,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, RemoteTransportError> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RemoteTransportError::Tls)?;
    if certs.is_empty() {
        return Err(RemoteTransportError::Tls);
    }
    Ok(certs)
}

fn connect_ws_config() -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    let sealed = usize::try_from(MAX_SEALED_FRAME_BYTES).unwrap_or(usize::MAX);
    let handshake = usize::try_from(MAX_HANDSHAKE_MESSAGE_BYTES).unwrap_or(usize::MAX);
    let bound = sealed.max(handshake);
    config.max_message_size = Some(bound);
    config.max_frame_size = Some(bound);
    config
}

fn remaining_until(deadline_at: Instant) -> Result<Duration, RemoteTransportError> {
    let remaining = deadline_at.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(RemoteTransportError::Timeout)
    } else {
        Ok(remaining)
    }
}

/// Open an authenticated Connect WebSocket before `deadline_at`.
pub async fn open_remote_connect_ws_until(
    endpoint: &RemoteEndpoint,
    cookie_header: Option<&str>,
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<WebSocketStream<RemoteIo>, RemoteTransportError> {
    if let Some(cookie) = cookie_header {
        validate_http_header_value(cookie)?;
    }
    validate_http_header_value(endpoint.origin())?;
    let remaining = remaining_until(deadline_at)?;
    tokio::time::timeout(
        remaining,
        open_remote_connect_ws_inner(endpoint, cookie_header, tls),
    )
    .await
    .map_err(|_| RemoteTransportError::Timeout)?
}

/// Open Connect WS under a relative deadline from now.
pub async fn open_remote_connect_ws(
    endpoint: &RemoteEndpoint,
    cookie_header: Option<&str>,
    tls: &RemoteTlsOptions,
    deadline: Duration,
) -> Result<WebSocketStream<RemoteIo>, RemoteTransportError> {
    open_remote_connect_ws_until(endpoint, cookie_header, tls, Instant::now() + deadline).await
}

async fn open_remote_connect_ws_inner(
    endpoint: &RemoteEndpoint,
    cookie_header: Option<&str>,
    tls: &RemoteTlsOptions,
) -> Result<WebSocketStream<RemoteIo>, RemoteTransportError> {
    let mut request = endpoint
        .ws_url
        .as_str()
        .into_client_request()
        .map_err(|_| RemoteTransportError::Endpoint)?;
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::ORIGIN,
        endpoint
            .origin
            .parse()
            .map_err(|_| RemoteTransportError::Header)?,
    );
    if let Some(cookie) = cookie_header {
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::COOKIE,
            cookie.parse().map_err(|_| RemoteTransportError::Header)?,
        );
    }
    let io = dial_remote_io(endpoint, tls).await?;
    let (socket, _response) =
        tokio_tungstenite::client_async_with_config(request, io, Some(connect_ws_config()))
            .await
            .map_err(|_| RemoteTransportError::Unavailable)?;
    Ok(socket)
}

async fn dial_remote_io(
    endpoint: &RemoteEndpoint,
    tls: &RemoteTlsOptions,
) -> Result<RemoteIo, RemoteTransportError> {
    let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|_| RemoteTransportError::Unavailable)?;
    if !endpoint.requires_tls() {
        return Ok(RemoteIo::Plain(tcp));
    }
    let config = build_rustls_client_config(tls)?;
    let server_name =
        ServerName::try_from(endpoint.host.clone()).map_err(|_| RemoteTransportError::Endpoint)?;
    let tls_stream = TlsConnector::from(config)
        .connect(server_name, tcp)
        .await
        .map_err(|_| RemoteTransportError::Tls)?;
    Ok(RemoteIo::Tls(tls_stream))
}

/// Bounded HTTP response (no secrets in Debug).
pub struct RemoteHttpResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub set_cookie: Option<String>,
    pub location: Option<String>,
}

impl fmt::Debug for RemoteHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteHttpResponse")
            .field("status", &self.status.as_u16())
            .field("body_len", &self.body.len())
            .field("set_cookie_present", &self.set_cookie.is_some())
            .field("location_present", &self.location.is_some())
            .finish()
    }
}

/// GET a bounded page under an absolute deadline.
pub async fn get_bounded_until(
    endpoint: &RemoteEndpoint,
    path: &str,
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<RemoteHttpResponse, RemoteTransportError> {
    validate_remote_path(path)?;
    let uri = format!("{}{}", endpoint.http_base.trim_end_matches('/'), path);
    exchange_http_until(endpoint, Method::GET, &uri, None, tls, deadline_at).await
}

pub async fn get_bounded(
    endpoint: &RemoteEndpoint,
    path: &str,
    tls: &RemoteTlsOptions,
    deadline: Duration,
) -> Result<RemoteHttpResponse, RemoteTransportError> {
    get_bounded_until(endpoint, path, tls, Instant::now() + deadline).await
}

/// POST `/pair`, collect expected pairing cookie. Same-origin redirect only as
/// cookie carrier — Location is never followed. Single Hyper path.
pub async fn post_pair_collect_cookie_until(
    endpoint: &RemoteEndpoint,
    json_body: &[u8],
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<Zeroizing<String>, RemoteTransportError> {
    if json_body.len() > crate::connect::MAX_DIRECT_PAIRING_BODY_BYTES as usize {
        return Err(RemoteTransportError::Oversized);
    }
    let uri = format!("{}/pair", endpoint.http_base.trim_end_matches('/'));
    let response = exchange_http_until(
        endpoint,
        Method::POST,
        &uri,
        Some(("application/json", json_body)),
        tls,
        deadline_at,
    )
    .await?;
    if let Some(location) = response.location.as_deref() {
        assert_same_origin_redirect(endpoint, location)?;
    } else if response.status.is_redirection() {
        return Err(RemoteTransportError::RedirectForbidden);
    }
    let Some(cookie) = response.set_cookie else {
        return Err(
            if response.status.is_success() || response.status.is_redirection() {
                RemoteTransportError::Corrupt
            } else {
                RemoteTransportError::Unauthorized
            },
        );
    };
    if !(response.status.is_success() || response.status.is_redirection()) {
        return Err(RemoteTransportError::Unauthorized);
    }
    Ok(Zeroizing::new(cookie))
}

pub async fn post_pair_collect_cookie(
    endpoint: &RemoteEndpoint,
    json_body: &[u8],
    tls: &RemoteTlsOptions,
    deadline: Duration,
) -> Result<Zeroizing<String>, RemoteTransportError> {
    post_pair_collect_cookie_until(endpoint, json_body, tls, Instant::now() + deadline).await
}

fn assert_same_origin_redirect(
    endpoint: &RemoteEndpoint,
    location: &str,
) -> Result<(), RemoteTransportError> {
    if location.starts_with('/') && !location.starts_with("//") {
        if location.contains('\\')
            || location.contains('@')
            || location.contains('#')
            || location.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        {
            return Err(RemoteTransportError::RedirectForbidden);
        }
        return Ok(());
    }
    let parsed = Url::parse(location).map_err(|_| RemoteTransportError::RedirectForbidden)?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(RemoteTransportError::RedirectForbidden);
    }
    let host = match parsed.host() {
        Some(Host::Domain(domain)) => domain.to_string(),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(RemoteTransportError::RedirectForbidden),
    };
    if !host.eq_ignore_ascii_case(endpoint.host()) {
        return Err(RemoteTransportError::RedirectForbidden);
    }
    let scheme = parsed.scheme();
    let http_scheme = endpoint.scheme.http_scheme();
    if !scheme.eq_ignore_ascii_case(http_scheme) {
        return Err(RemoteTransportError::RedirectForbidden);
    }
    let port = parsed
        .port_or_known_default()
        .ok_or(RemoteTransportError::RedirectForbidden)?;
    if port != endpoint.port() {
        return Err(RemoteTransportError::RedirectForbidden);
    }
    Ok(())
}

async fn exchange_http_until(
    endpoint: &RemoteEndpoint,
    method: Method,
    uri: &str,
    body: Option<(&str, &[u8])>,
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<RemoteHttpResponse, RemoteTransportError> {
    validate_http_header_value(endpoint.origin())?;
    let remaining = remaining_until(deadline_at)?;
    tokio::time::timeout(remaining, async {
        let io = dial_remote_io(endpoint, tls).await?;
        let parsed: Uri = uri.parse().map_err(|_| RemoteTransportError::Endpoint)?;
        let authority = parsed
            .authority()
            .map(|value| value.as_str().to_string())
            .ok_or(RemoteTransportError::Endpoint)?;
        let path_and_query = parsed
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let mut builder = Request::builder()
            .method(method)
            .uri(path_and_query)
            .header(hyper::header::HOST, authority)
            .header(hyper::header::ORIGIN, endpoint.origin())
            .header(hyper::header::ACCEPT, "application/json");
        let request = if let Some((content_type, bytes)) = body {
            builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
            builder
                .body(Full::new(Bytes::copy_from_slice(bytes)))
                .map_err(|_| RemoteTransportError::Corrupt)?
        } else {
            builder
                .body(Full::new(Bytes::new()))
                .map_err(|_| RemoteTransportError::Corrupt)?
        };

        // Own the Hyper connection future; never spawn/detach it.
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
                .await
                .map_err(|_| RemoteTransportError::Unavailable)?;
        let mut connection = std::pin::pin!(connection);
        let send = sender.send_request(request);
        let mut send = std::pin::pin!(send);
        let mut connection_finished = false;
        let response = match select(&mut send, &mut connection).await {
            Either::Left((Ok(response), _)) => response,
            Either::Left((Err(_), _)) => return Err(RemoteTransportError::Unavailable),
            Either::Right((conn_result, pending_send)) => {
                conn_result.map_err(|_| RemoteTransportError::Unavailable)?;
                connection_finished = true;
                // A Connection: close peer may finish after publishing the
                // response but before send_request gets its next poll.
                pending_send
                    .await
                    .map_err(|_| RemoteTransportError::Unavailable)?
            }
        };

        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let set_cookie = response
            .headers()
            .get_all(hyper::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| extract_pairing_cookie_header(value).ok());

        let collect = Limited::new(response.into_body(), REMOTE_HTTP_MAX_BODY_BYTES).collect();
        let mut collect = std::pin::pin!(collect);
        let collected = if connection_finished {
            collect.await.map_err(|_| RemoteTransportError::Oversized)?
        } else {
            match select(&mut collect, &mut connection).await {
                Either::Left((Ok(collected), _)) => collected,
                Either::Left((Err(_), _)) => return Err(RemoteTransportError::Oversized),
                Either::Right((conn_result, pending_collect)) => {
                    conn_result.map_err(|_| RemoteTransportError::Unavailable)?;
                    connection_finished = true;
                    pending_collect
                        .await
                        .map_err(|_| RemoteTransportError::Oversized)?
                }
            }
        };
        let body = collected.to_bytes().to_vec();
        if body.len() > REMOTE_HTTP_MAX_BODY_BYTES {
            return Err(RemoteTransportError::Oversized);
        }

        // Close the request sender then drive the connection to EOF so the
        // peer observes orderly shutdown (no detached socket owner).
        drop(sender);
        if !connection_finished {
            let _ = connection.await;
        }

        Ok(RemoteHttpResponse {
            status,
            body,
            set_cookie,
            location,
        })
    })
    .await
    .map_err(|_| RemoteTransportError::Timeout)?
}

/// Parse `devmanager-connect` meta JSON for host public id + key only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedHostIdentity {
    pub host_public_id: [u8; 16],
    pub host_public_key: [u8; 32],
}

pub fn parse_devmanager_connect_meta(
    html_or_json: &str,
) -> Result<PublishedHostIdentity, RemoteTransportError> {
    let json = if let Some(content) = extract_meta_content(html_or_json, "devmanager-connect") {
        content
    } else {
        html_or_json.trim().to_string()
    };
    if json.len() > crate::connect::CONNECT_WEB_MARKER_MAX_JSON_BYTES {
        return Err(RemoteTransportError::Oversized);
    }
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|_| RemoteTransportError::Corrupt)?;
    let obj = value.as_object().ok_or(RemoteTransportError::Corrupt)?;
    let host_public_id = obj
        .get("hostPublicId")
        .and_then(|value| value.as_str())
        .ok_or(RemoteTransportError::Corrupt)?;
    let host_public_key = obj
        .get("hostPublicKey")
        .and_then(|value| value.as_str())
        .ok_or(RemoteTransportError::Corrupt)?;
    let id = parse_host_public_id(host_public_id)?;
    let key = parse_host_public_key_hex(host_public_key)?;
    if id == [0_u8; 16] || key == [0_u8; 32] {
        return Err(RemoteTransportError::Corrupt);
    }
    Ok(PublishedHostIdentity {
        host_public_id: id,
        host_public_key: key,
    })
}

fn extract_meta_content(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let idx = html.find(&needle)?;
    let after = &html[idx..];
    let content_key = "content=\"";
    let content_at = after.find(content_key)?;
    let start = content_at + content_key.len();
    let rest = &after[start..];
    let end = rest.find('"')?;
    let raw = &rest[..end];
    Some(
        raw.replace("&quot;", "\"")
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

pub fn parse_host_public_id(raw: &str) -> Result<[u8; 16], RemoteTransportError> {
    if let Ok(uuid) = uuid::Uuid::parse_str(raw) {
        let bytes = *uuid.as_bytes();
        if bytes == [0_u8; 16] {
            return Err(RemoteTransportError::Corrupt);
        }
        return Ok(bytes);
    }
    decode_exact_hex::<16>(raw).and_then(|bytes| {
        if bytes == [0_u8; 16] {
            Err(RemoteTransportError::Corrupt)
        } else {
            Ok(bytes)
        }
    })
}

pub fn parse_host_public_key_hex(raw: &str) -> Result<[u8; 32], RemoteTransportError> {
    decode_exact_hex::<32>(raw).and_then(|bytes| {
        if bytes == [0_u8; 32] {
            Err(RemoteTransportError::Corrupt)
        } else {
            Ok(bytes)
        }
    })
}

fn decode_exact_hex<const N: usize>(raw: &str) -> Result<[u8; N], RemoteTransportError> {
    if raw.len() != N * 2 {
        return Err(RemoteTransportError::Corrupt);
    }
    let mut out = [0_u8; N];
    for (index, chunk) in raw.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[index] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, RemoteTransportError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(RemoteTransportError::Corrupt),
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_validation_rejects_pem_with_invalid_certificate_der() {
        assert_eq!(
            validate_additional_ca_pem(
                "-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----\n"
            ),
            Err(RemoteTransportError::Tls)
        );
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn rejects_userinfo_fragment_lan_plaintext_and_port_zero() {
        assert!(validate_remote_endpoint("wss://user:pass@example.com/api/connect").is_err());
        assert!(validate_remote_endpoint("wss://example.com/api/connect#frag").is_err());
        assert!(validate_remote_endpoint("ws://192.168.1.10/api/connect").is_err());
        assert!(validate_remote_endpoint("http://localhost:8080/api/connect").is_err());
        assert!(validate_remote_endpoint("ws://127.0.0.1:0/api/connect").is_err());
    }

    #[test]
    fn canonical_origin_omits_default_https_port_and_formats_ipv6() {
        let https = validate_remote_endpoint("https://host.example/api/connect").unwrap();
        assert_eq!(https.origin(), "https://host.example");
        assert!(!https.origin().contains(":443"));

        let https_custom = validate_remote_endpoint("https://host.example:8443/").unwrap();
        assert_eq!(https_custom.origin(), "https://host.example:8443");

        let v6 = validate_remote_endpoint("https://[2001:db8::1]:8443/api/connect").unwrap();
        assert_eq!(v6.host(), "2001:db8::1");
        assert_eq!(v6.origin(), "https://[2001:db8::1]:8443");
        assert!(!v6.host().starts_with('['));
    }

    #[test]
    fn rejects_header_injection_and_non_pairing_cookies() {
        assert!(validate_http_header_value("dm_web=value").is_ok());
        assert!(validate_http_header_value("bad\r\nInjected: 1").is_err());
        assert!(extract_pairing_cookie_header("dm_web=abc; HttpOnly").is_ok());
        assert!(extract_pairing_cookie_header("dm_web_0123456789abcdef=abc").is_ok());
        assert!(extract_pairing_cookie_header("session=abc; HttpOnly").is_err());
        assert!(extract_pairing_cookie_header("dm_web=abc\r\nSet-Cookie: x=y").is_err());
    }

    #[test]
    fn parses_published_host_identity_from_meta_and_json() {
        let key = "11".repeat(32);
        let id = "01900000-0000-7000-8000-000000000001";
        let json =
            format!(r#"{{"transport":"connect","hostPublicId":"{id}","hostPublicKey":"{key}"}}"#);
        let html = format!(
            r#"<html><head><meta name="devmanager-connect" content="{}"></head></html>"#,
            json.replace('"', "&quot;")
        );
        let from_html = parse_devmanager_connect_meta(&html).unwrap();
        let from_json = parse_devmanager_connect_meta(&json).unwrap();
        assert_eq!(from_html, from_json);
    }

    #[tokio::test]
    async fn connect_ws_deadline_covers_hung_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        let _accept = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = stream;
            std::future::pending::<()>().await;
        });
        let endpoint =
            validate_remote_endpoint(&format!("ws://127.0.0.1:{port}/api/connect")).unwrap();
        let error = open_remote_connect_ws(
            &endpoint,
            None,
            &RemoteTlsOptions::default(),
            Duration::from_millis(200),
        )
        .await
        .err()
        .expect("deadline");
        assert_eq!(error, RemoteTransportError::Timeout);
    }

    #[tokio::test]
    async fn http_cancel_closes_peer_without_detached_owner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = [0_u8; 8];
            // After client cancel/drop, read should observe EOF (0).
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        });
        let endpoint = validate_remote_endpoint(&format!("http://127.0.0.1:{port}/")).unwrap();
        let error = get_bounded(
            &endpoint,
            "/",
            &RemoteTlsOptions::default(),
            Duration::from_millis(150),
        )
        .await
        .expect_err("timeout while server holds headers");
        assert_eq!(error, RemoteTransportError::Timeout);
        let saw_eof = tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .expect("peer join")
            .expect("peer task");
        assert!(saw_eof, "server must observe EOF after client cancel");
    }

    #[tokio::test]
    async fn pair_redirect_foreign_location_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().unwrap().port();
        let _server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = concat!(
                "HTTP/1.1 302 Found\r\n",
                "Location: https://evil.example/\r\n",
                "Set-Cookie: dm_web=steal; Path=/\r\n",
                "Content-Length: 0\r\n",
                "Connection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let endpoint = validate_remote_endpoint(&format!("http://127.0.0.1:{port}/")).unwrap();
        let error = post_pair_collect_cookie(
            &endpoint,
            br#"{"t":"ABCD1234","browserInstallId":"11","label":""}"#,
            &RemoteTlsOptions::default(),
            Duration::from_secs(2),
        )
        .await
        .expect_err("foreign redirect");
        assert_eq!(error, RemoteTransportError::RedirectForbidden);
    }
}
