use super::mcp::BrowserMcpServer;
use super::{
    BrowserCommandBridge, BrowserProviderAccess, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::collections::HashMap;
use std::fmt;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tower::Service;

type RegistrationService = StreamableHttpService<BrowserMcpServer, LocalSessionManager>;

struct ActiveRegistration {
    process_session_id: String,
    workspace_key: BrowserWorkspaceKey,
    service: RegistrationService,
}

#[derive(Default)]
struct RegistrationStore {
    by_token: HashMap<String, ActiveRegistration>,
    token_by_process: HashMap<String, String>,
}

struct BrowserGatewayInner {
    port: u16,
    endpoint: String,
    bridge: BrowserCommandBridge,
    registrations: Mutex<RegistrationStore>,
    running: AtomicBool,
}

#[derive(Clone)]
pub struct BrowserGatewayRegistrar {
    inner: Arc<BrowserGatewayInner>,
}

impl fmt::Debug for BrowserGatewayRegistrar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserGatewayRegistrar")
            .field("endpoint", &self.inner.endpoint)
            .field("active_registrations", &self.active_registration_count())
            .finish()
    }
}

#[derive(Clone)]
pub struct BrowserGatewayRegistration {
    process_session_id: String,
    workspace_key: BrowserWorkspaceKey,
    access: BrowserProviderAccess,
}

impl BrowserGatewayRegistration {
    pub fn process_session_id(&self) -> &str {
        &self.process_session_id
    }

    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn access(&self) -> &BrowserProviderAccess {
        &self.access
    }
}

impl fmt::Debug for BrowserGatewayRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserGatewayRegistration")
            .field("process_session_id", &self.process_session_id)
            .field("workspace_key", &self.workspace_key)
            .field("access", &self.access)
            .finish()
    }
}

pub struct BrowserGatewayHandle {
    inner: Arc<BrowserGatewayInner>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl BrowserGatewayHandle {
    pub fn start(bridge: BrowserCommandBridge) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("bind DevManager browser MCP gateway: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("configure DevManager browser MCP listener: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("read DevManager browser MCP listener address: {error}"))?
            .port();
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let inner = Arc::new(BrowserGatewayInner {
            port,
            endpoint,
            bridge,
            registrations: Mutex::new(RegistrationStore::default()),
            running: AtomicBool::new(true),
        });
        let server_inner = Arc::clone(&inner);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let thread = thread::Builder::new()
            .name("devmanager-browser-mcp".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        server_inner.running.store(false, Ordering::Release);
                        eprintln!("DevManager browser MCP runtime failed: {error}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            server_inner.running.store(false, Ordering::Release);
                            eprintln!("DevManager browser MCP listener failed: {error}");
                            return;
                        }
                    };
                    let app = Router::new()
                        .route("/mcp", any(dispatch_mcp))
                        .with_state(Arc::clone(&server_inner));
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                    server_inner.running.store(false, Ordering::Release);
                });
            })
            .map_err(|error| format!("start DevManager browser MCP thread: {error}"))?;
        Ok(Self {
            inner,
            shutdown: Mutex::new(Some(shutdown_tx)),
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn registrar(&self) -> BrowserGatewayRegistrar {
        BrowserGatewayRegistrar {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }
}

impl Drop for BrowserGatewayHandle {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Release);
        self.registrar().revoke_all();
        if let Some(shutdown) = lock(&self.shutdown).take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

impl BrowserGatewayRegistrar {
    pub fn register(
        &self,
        process_session_id: impl Into<String>,
        workspace_key: BrowserWorkspaceKey,
        initial_snapshot: BrowserWorkspaceSnapshot,
    ) -> Result<BrowserGatewayRegistration, String> {
        if !self.inner.running.load(Ordering::Acquire) {
            return Err("DevManager browser MCP gateway is not running".to_string());
        }
        let process_session_id = process_session_id.into();
        if process_session_id.trim().is_empty() {
            return Err("browser gateway process session id cannot be blank".to_string());
        }
        let token = generate_token()?;
        let access = BrowserProviderAccess::new(self.inner.endpoint.clone(), token.clone())?;
        let controller = self
            .inner
            .bridge
            .bind(workspace_key.clone(), Duration::from_secs(30));
        let server = BrowserMcpServer::new(controller, initial_snapshot);
        let allowed_hosts = [
            format!("127.0.0.1:{}", self.inner.port),
            format!("localhost:{}", self.inner.port),
        ];
        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Default::default(),
            StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
        );
        let active = ActiveRegistration {
            process_session_id: process_session_id.clone(),
            workspace_key: workspace_key.clone(),
            service,
        };
        let mut registrations = lock(&self.inner.registrations);
        if let Some(old_token) = registrations
            .token_by_process
            .insert(process_session_id.clone(), token.clone())
        {
            registrations.by_token.remove(&old_token);
        }
        registrations.by_token.insert(token, active);
        Ok(BrowserGatewayRegistration {
            process_session_id,
            workspace_key,
            access,
        })
    }

    pub fn revoke(&self, registration: &BrowserGatewayRegistration) -> bool {
        let mut registrations = lock(&self.inner.registrations);
        let token = registration.access.bearer_token();
        let matches = registrations.by_token.get(token).is_some_and(|active| {
            active.process_session_id == registration.process_session_id
                && active.workspace_key == registration.workspace_key
        });
        if !matches {
            return false;
        }
        registrations.by_token.remove(token);
        if registrations
            .token_by_process
            .get(&registration.process_session_id)
            .is_some_and(|current| current == token)
        {
            registrations
                .token_by_process
                .remove(&registration.process_session_id);
        }
        true
    }

    pub fn revoke_process(&self, process_session_id: &str) -> bool {
        let mut registrations = lock(&self.inner.registrations);
        let Some(token) = registrations.token_by_process.remove(process_session_id) else {
            return false;
        };
        registrations.by_token.remove(&token).is_some()
    }

    pub fn revoke_all(&self) {
        let mut registrations = lock(&self.inner.registrations);
        registrations.by_token.clear();
        registrations.token_by_process.clear();
    }

    pub fn active_registration_count(&self) -> usize {
        lock(&self.inner.registrations).by_token.len()
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }
}

async fn dispatch_mcp(
    State(inner): State<Arc<BrowserGatewayInner>>,
    request: Request<Body>,
) -> Response<Body> {
    if !matches!(
        *request.method(),
        Method::GET | Method::POST | Method::DELETE
    ) {
        return plain_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if let Err(response) = validate_host(&request, inner.port) {
        return response;
    }
    let token = match bearer_token(&request) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let service = {
        let registrations = lock(&inner.registrations);
        registrations
            .by_token
            .get(token)
            .map(|registration| registration.service.clone())
    };
    let Some(mut service) = service else {
        return plain_response(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    match service.call(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    }
}

fn validate_host(request: &Request<Body>, port: u16) -> Result<(), Response<Body>> {
    let values = request.headers().get_all(header::HOST);
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return Err(plain_response(StatusCode::BAD_REQUEST, "missing Host"));
    };
    if values.next().is_some() {
        return Err(plain_response(
            StatusCode::BAD_REQUEST,
            "multiple Host headers",
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| plain_response(StatusCode::BAD_REQUEST, "invalid Host"))?;
    let authority: axum::http::uri::Authority = value
        .parse()
        .map_err(|_| plain_response(StatusCode::BAD_REQUEST, "invalid Host"))?;
    let allowed_host = matches!(authority.host(), "127.0.0.1" | "localhost");
    if !allowed_host || authority.port_u16() != Some(port) {
        return Err(plain_response(StatusCode::FORBIDDEN, "forbidden Host"));
    }
    Ok(())
}

fn bearer_token(request: &Request<Body>) -> Result<&str, Response<Body>> {
    let values = request.headers().get_all(header::AUTHORIZATION);
    let mut values = values.iter();
    let Some(value) = values.next() else {
        return Err(plain_response(StatusCode::UNAUTHORIZED, "unauthorized"));
    };
    if values.next().is_some() {
        return Err(plain_response(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let value = value
        .to_str()
        .map_err(|_| plain_response(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    let token = value
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty() && !token.contains(char::is_whitespace))
        .ok_or_else(|| plain_response(StatusCode::UNAUTHORIZED, "unauthorized"))?;
    Ok(token)
}

fn plain_response(status: StatusCode, message: &'static str) -> Response<Body> {
    (status, message).into_response()
}

fn generate_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("generate DevManager browser token: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
