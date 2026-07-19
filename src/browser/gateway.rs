use super::commands::BrowserRegistrationLeaseTicket;
use super::downloads::{verified_app_config_root, verify_prepared_storage_root};
use super::mcp::BrowserMcpServer;
use super::{
    BrowserCommandBridge, BrowserProviderAccess, BrowserRegistrationLease, BrowserResourceLimits,
    BrowserResourceStore, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use tower::Service;

const GATEWAY_THREAD_JOIN_TIMEOUT: Duration = Duration::from_millis(250);

type RegistrationService = StreamableHttpService<BrowserMcpServer, LocalSessionManager>;

struct ActiveRegistration {
    process_session_id: String,
    workspace_key: BrowserWorkspaceKey,
    service: RegistrationService,
    lease: BrowserRegistrationLease,
}

struct RegistrationDispatchSnapshot {
    service: RegistrationService,
    lease: BrowserRegistrationLease,
    ticket: BrowserRegistrationLeaseTicket,
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
    app_config_dir: PathBuf,
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
        let app_config_dir = std::env::temp_dir()
            .join("devmanager-browser-gateway")
            .join(std::process::id().to_string());
        Self::start_with_app_config_dir(bridge, app_config_dir)
    }

    pub fn start_with_app_config_dir(
        bridge: BrowserCommandBridge,
        app_config_dir: impl AsRef<Path>,
    ) -> Result<Self, String> {
        Self::start_with_runtime_builder_and_config(
            bridge,
            app_config_dir.as_ref().to_path_buf(),
            build_gateway_runtime,
        )
    }

    #[cfg(test)]
    fn start_with_runtime_builder<F>(
        bridge: BrowserCommandBridge,
        build_runtime: F,
    ) -> Result<Self, String>
    where
        F: FnOnce() -> Result<tokio::runtime::Runtime, String> + Send + 'static,
    {
        let app_config_dir = std::env::temp_dir()
            .join("devmanager-browser-gateway")
            .join(std::process::id().to_string());
        Self::start_with_runtime_builder_and_config(bridge, app_config_dir, build_runtime)
    }

    fn start_with_runtime_builder_and_config<F>(
        bridge: BrowserCommandBridge,
        app_config_dir: PathBuf,
        build_runtime: F,
    ) -> Result<Self, String>
    where
        F: FnOnce() -> Result<tokio::runtime::Runtime, String> + Send + 'static,
    {
        let app_config_dir = verified_app_config_root(&app_config_dir)
            .map_err(|error| format!("verify browser gateway storage root: {error}"))?;
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
            app_config_dir,
            registrations: Mutex::new(RegistrationStore::default()),
            running: AtomicBool::new(false),
        });
        let server_inner = Arc::clone(&inner);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("devmanager-browser-mcp".to_string())
            .spawn(move || {
                let runtime = match build_runtime() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        stop_and_clear_registrations(&server_inner);
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            stop_and_clear_registrations(&server_inner);
                            let _ = ready_tx.send(Err(format!(
                                "initialize DevManager browser MCP listener: {error}"
                            )));
                            return;
                        }
                    };
                    let app = Router::new()
                        .route("/mcp", any(dispatch_mcp))
                        .with_state(Arc::clone(&server_inner));
                    server_inner.running.store(true, Ordering::Release);
                    if ready_tx.send(Ok(())).is_err() {
                        stop_and_clear_registrations(&server_inner);
                        return;
                    }
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                    stop_and_clear_registrations(&server_inner);
                });
            })
            .map_err(|error| format!("start DevManager browser MCP thread: {error}"))?;
        let handle = Self {
            inner,
            shutdown: Mutex::new(Some(shutdown_tx)),
            thread: Mutex::new(Some(thread)),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(error)) => {
                drop(handle);
                Err(error)
            }
            Err(error) => {
                drop(handle);
                Err(format!(
                    "DevManager browser MCP thread exited before readiness: {error}"
                ))
            }
        }
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

fn build_gateway_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("initialize DevManager browser MCP runtime: {error}"))
}

impl Drop for BrowserGatewayHandle {
    fn drop(&mut self) {
        stop_and_clear_registrations(&self.inner);
        if let Some(shutdown) = lock(&self.shutdown).take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = lock(&self.thread).take() {
            let started = Instant::now();
            while !thread.is_finished() && started.elapsed() < GATEWAY_THREAD_JOIN_TIMEOUT {
                thread::sleep(Duration::from_millis(5));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
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
        self.register_with_project_root(
            process_session_id,
            workspace_key,
            initial_snapshot,
            std::env::current_dir().map_err(|error| {
                format!("resolve browser gateway default project root: {error}")
            })?,
        )
    }

    pub fn register_with_project_root(
        &self,
        process_session_id: impl Into<String>,
        workspace_key: BrowserWorkspaceKey,
        initial_snapshot: BrowserWorkspaceSnapshot,
        project_root: impl AsRef<Path>,
    ) -> Result<BrowserGatewayRegistration, String> {
        self.register_with_before_store(
            process_session_id,
            workspace_key,
            initial_snapshot,
            project_root.as_ref().to_path_buf(),
            || {},
        )
    }

    fn register_with_before_store<F>(
        &self,
        process_session_id: impl Into<String>,
        workspace_key: BrowserWorkspaceKey,
        initial_snapshot: BrowserWorkspaceSnapshot,
        project_root: PathBuf,
        before_store: F,
    ) -> Result<BrowserGatewayRegistration, String>
    where
        F: FnOnce(),
    {
        if !self.inner.running.load(Ordering::Acquire) {
            return Err("DevManager browser MCP gateway is not running".to_string());
        }
        let process_session_id = process_session_id.into();
        if process_session_id.trim().is_empty() {
            return Err("browser gateway process session id cannot be blank".to_string());
        }
        let project_root = project_root
            .canonicalize()
            .map_err(|error| format!("canonicalize browser project root: {error}"))?;
        let token = generate_token()?;
        let access = BrowserProviderAccess::new(self.inner.endpoint.clone(), token.clone())?;
        let lease = BrowserRegistrationLease::new();
        let controller = self.inner.bridge.bind_with_registration_lease(
            workspace_key.clone(),
            Duration::from_secs(30),
            Some(lease.clone()),
        );
        verify_prepared_storage_root(&self.inner.app_config_dir, &self.inner.app_config_dir)
            .map_err(|error| format!("revalidate browser gateway storage root: {error}"))?;
        let resource_store = BrowserResourceStore::open_verified(
            &self.inner.app_config_dir,
            &workspace_key.project_id,
            BrowserResourceLimits::default(),
        )
        .map_err(|error| format!("open DevManager browser resource store: {error}"))?;
        let server =
            BrowserMcpServer::new(controller, initial_snapshot, resource_store, project_root);
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
            lease,
        };
        before_store();
        let mut registrations = lock(&self.inner.registrations);
        if !self.inner.running.load(Ordering::Acquire) {
            return Err("DevManager browser MCP gateway is not running".to_string());
        }
        if let Some(old_token) = registrations
            .token_by_process
            .get(&process_session_id)
            .cloned()
        {
            if let Some(old_registration) = registrations.by_token.get(&old_token) {
                self.inner
                    .bridge
                    .revoke_registration(&old_registration.workspace_key, &old_registration.lease);
            }
            registrations.by_token.remove(&old_token);
        }
        registrations
            .token_by_process
            .insert(process_session_id.clone(), token.clone());
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
        if let Some(active) = registrations.by_token.get(token) {
            self.inner
                .bridge
                .revoke_registration(&active.workspace_key, &active.lease);
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
        let Some(token) = registrations
            .token_by_process
            .get(process_session_id)
            .cloned()
        else {
            return false;
        };
        let Some((workspace_key, lease)) = registrations
            .by_token
            .get(&token)
            .map(|active| (active.workspace_key.clone(), active.lease.clone()))
        else {
            registrations.token_by_process.remove(process_session_id);
            return false;
        };
        self.inner
            .bridge
            .revoke_registration(&workspace_key, &lease);
        registrations.token_by_process.remove(process_session_id);
        let removed = registrations.by_token.remove(&token);
        removed.is_some()
    }

    pub fn revoke_all(&self) {
        let mut registrations = lock(&self.inner.registrations);
        for registration in registrations.by_token.values() {
            self.inner
                .bridge
                .revoke_registration(&registration.workspace_key, &registration.lease);
        }
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

fn stop_and_clear_registrations(inner: &BrowserGatewayInner) {
    let mut registrations = lock(&inner.registrations);
    inner.running.store(false, Ordering::Release);
    for registration in registrations.by_token.values() {
        inner
            .bridge
            .revoke_registration(&registration.workspace_key, &registration.lease);
    }
    registrations.by_token.clear();
    registrations.token_by_process.clear();
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
    let Some(snapshot) = registration_dispatch_snapshot(&inner, token) else {
        return plain_response(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    dispatch_registration(snapshot, request).await
}

fn registration_dispatch_snapshot(
    inner: &BrowserGatewayInner,
    token: &str,
) -> Option<RegistrationDispatchSnapshot> {
    let registrations = lock(&inner.registrations);
    let registration = registrations.by_token.get(token)?;
    let (ticket, _cancellation) = registration.lease.capture().ok()?;
    Some(RegistrationDispatchSnapshot {
        service: registration.service.clone(),
        lease: registration.lease.clone(),
        ticket,
    })
}

async fn dispatch_registration(
    snapshot: RegistrationDispatchSnapshot,
    request: Request<Body>,
) -> Response<Body> {
    let RegistrationDispatchSnapshot {
        mut service,
        lease,
        ticket,
    } = snapshot;
    if !lease.is_current(ticket) {
        return plain_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let response = match service.call(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    };
    if !lease.is_current(ticket) {
        return plain_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    response
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{
        browser_command_channel, compile_browser_replay, BrowserCommand, BrowserError,
        BrowserHostEvent, BrowserRecipeAction, BrowserRecipeInput, BrowserRecipeInputKind,
        BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue,
        BrowserRecipeViewport, BrowserReplayLocatorSlot, BrowserReplayRepairResumeCursor,
        BrowserReplaySecretError, BrowserReplaySecretPromptVault, BrowserReplayStatus,
        BrowserResourceKind, BrowserResourceLimits, BrowserResourceStore, BrowserResponse,
        BrowserRevision, BrowserUserInputKind, BROWSER_RECIPE_SCHEMA_VERSION,
    };

    fn provider_repair_plan(label: &str) -> crate::browser::BrowserReplayPlan {
        compile_browser_replay(
            &BrowserRecipeV1 {
                schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                id: format!("provider-repair-{label}"),
                name: "Provider repair retention".to_string(),
                description: "Registration loss releases repair evidence".to_string(),
                start_url: "https://example.test/repair".to_string(),
                viewport: BrowserRecipeViewport::default(),
                inputs: vec![BrowserRecipeInput {
                    name: "password".to_string(),
                    kind: BrowserRecipeInputKind::Secret,
                    default_value: None,
                }],
                steps: vec![BrowserRecipeStep {
                    id: "type-password".to_string(),
                    action: BrowserRecipeAction::Type {
                        locator: BrowserRecipeLocator {
                            test_id: Some("password".to_string()),
                            ..BrowserRecipeLocator::default()
                        },
                        value: BrowserRecipeValue::Input {
                            name: "password".to_string(),
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                }],
            },
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn start_waits_for_thread_runtime_failure_before_returning() {
        let (bridge, _inbox) = browser_command_channel(1);

        let result = BrowserGatewayHandle::start_with_runtime_builder(bridge, || {
            Err("fixture runtime construction failed".to_string())
        });

        let error = match result {
            Ok(_) => panic!("gateway startup must not publish a handle before runtime readiness"),
            Err(error) => error,
        };
        assert!(error.contains("fixture runtime construction failed"));
    }

    #[test]
    fn registration_cannot_publish_after_shutdown_wins_before_store_lock() {
        let (bridge, _inbox) = browser_command_channel(1);
        let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
        let registrar = gateway.registrar();
        let shutdown = registrar.clone();

        let result = registrar.register_with_before_store(
            "racing-process",
            BrowserWorkspaceKey::new("project", "conversation").unwrap(),
            BrowserWorkspaceSnapshot::default(),
            std::env::current_dir().unwrap(),
            move || {
                shutdown.inner.running.store(false, Ordering::Release);
                shutdown.revoke_all();
            },
        );

        assert!(result
            .expect_err("shutdown must fence a registration that has not reached the store")
            .contains("not running"));
        assert_eq!(registrar.active_registration_count(), 0);
    }

    #[tokio::test]
    async fn authenticated_initialize_snapshot_cannot_dispatch_after_revocation() {
        let (bridge, _inbox) = browser_command_channel(1);
        let gateway = BrowserGatewayHandle::start(bridge).expect("start gateway");
        let registrar = gateway.registrar();
        let registration = registrar
            .register(
                "lease-race-process",
                BrowserWorkspaceKey::new("project", "conversation").unwrap(),
                BrowserWorkspaceSnapshot::default(),
            )
            .expect("register lease race fixture");
        let token = registration.access().bearer_token_for_launch();
        let snapshot = registration_dispatch_snapshot(&gateway.inner, token)
            .expect("capture authenticated dispatch snapshot");

        assert!(registrar.revoke(&registration));

        let request = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"lease-race","version":"1"}}}"#,
            ))
            .unwrap();
        let response = dispatch_registration(snapshot, request).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn revoked_old_controller_stop_cannot_interrupt_replacement_registration_work() {
        let (bridge, mut inbox) = browser_command_channel(2);
        let workspace_key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        let old_lease = BrowserRegistrationLease::new();
        let old_controller = bridge.bind_with_registration_lease(
            workspace_key.clone(),
            Duration::from_secs(1),
            Some(old_lease.clone()),
        );

        bridge.revoke_registration(&workspace_key, &old_lease);
        assert_eq!(bridge.drain_host_controls().len(), 1);

        let replacement_controller = bridge.bind_with_registration_lease(
            workspace_key,
            Duration::from_secs(1),
            Some(BrowserRegistrationLease::new()),
        );
        replacement_controller
            .notify(BrowserCommand::Status)
            .await
            .expect("queue replacement registration work");
        let replacement_request = inbox.recv().await.expect("replacement request");
        assert!(replacement_request.cancellation_is_current());

        assert!(matches!(
            old_controller
                .notify(BrowserCommand::Stop { tab_id: None })
                .await,
            Err(BrowserError::Interrupted)
        ));
        assert!(
            replacement_request.cancellation_is_current(),
            "a revoked controller must not advance shared cancellation epochs"
        );
        let (_controls, lifecycle_requests) =
            bridge.with_locked_host_work(|controls, requests| (controls, requests));
        assert!(lifecycle_requests.is_empty());
    }

    #[test]
    fn browser_provider_registration_loss_releases_retained_repair_evidence_at_every_boundary() {
        #[derive(Clone, Copy, Debug)]
        enum Boundary {
            Replacement,
            ExactRevoke,
            ProcessRevoke,
            RevokeAll,
            GatewayDrop,
        }

        for boundary in [
            Boundary::Replacement,
            Boundary::ExactRevoke,
            Boundary::ProcessRevoke,
            Boundary::RevokeAll,
            Boundary::GatewayDrop,
        ] {
            let label = format!("{boundary:?}").to_ascii_lowercase();
            let root = std::env::temp_dir().join(format!(
                "devmanager-provider-repair-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let resource_store = BrowserResourceStore::open(
                root.join("resources"),
                BrowserResourceLimits {
                    max_temporary_count: 0,
                    max_temporary_bytes: 1024 * 1024,
                    max_resource_bytes: 1024 * 1024,
                },
            )
            .unwrap();
            let (bridge, _inbox) = browser_command_channel(8);
            let mut gateway = Some(
                BrowserGatewayHandle::start_with_app_config_dir(bridge.clone(), root.join("app"))
                    .unwrap(),
            );
            let registrar = gateway.as_ref().unwrap().registrar();
            let key = BrowserWorkspaceKey::new(format!("provider-repair-{label}"), "conversation")
                .unwrap();
            let process_id = format!("provider-repair-process-{label}");
            let registration = registrar
                .register(
                    process_id.clone(),
                    key.clone(),
                    BrowserWorkspaceSnapshot::default(),
                )
                .unwrap();
            let coordinator = bridge.replay_coordinator();
            let started = coordinator
                .start(key.clone(), provider_repair_plan(&label))
                .unwrap();
            let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
                started.instance.clone(),
                vec!["password".to_string()],
            )
            .unwrap();
            prompt
                .edit(&started.instance, "password", "provider-repair-secret")
                .unwrap();
            let (submission, _) = prompt.submit(&started.instance).unwrap();
            coordinator
                .submit_secrets(&started.instance, submission)
                .unwrap();
            let secret_lease = started.execution.secret_lease("password").unwrap();
            assert!(secret_lease
                .expose(|value| value == "provider-repair-secret")
                .unwrap());
            let repair = coordinator
                .reserve_locator_repair_capture(
                    &started.instance,
                    &resource_store,
                    0,
                    BrowserReplayLocatorSlot::PrimaryAction,
                    "runtime-tab",
                    BrowserRevision(1),
                    BrowserReplayRepairResumeCursor::Action,
                )
                .unwrap();
            let snapshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairSnapshot,
                    "application/json",
                    b"{}",
                )
                .unwrap();
            let screenshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairScreenshot,
                    "image/png",
                    b"png",
                )
                .unwrap();
            coordinator
                .publish_locator_repair(&repair, &snapshot, &screenshot)
                .unwrap();

            match boundary {
                Boundary::Replacement => {
                    registrar
                        .register(
                            process_id,
                            BrowserWorkspaceKey::new(
                                format!("provider-repair-replacement-{label}"),
                                "conversation",
                            )
                            .unwrap(),
                            BrowserWorkspaceSnapshot::default(),
                        )
                        .unwrap();
                }
                Boundary::ExactRevoke => assert!(registrar.revoke(&registration)),
                Boundary::ProcessRevoke => assert!(registrar.revoke_process(&process_id)),
                Boundary::RevokeAll => registrar.revoke_all(),
                Boundary::GatewayDrop => drop(gateway.take()),
            }

            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                BrowserReplayStatus::Cancelled
            );
            assert_eq!(
                secret_lease.expose(|_| ()),
                Err(BrowserReplaySecretError::ClosedStore)
            );
            assert!(matches!(
                resource_store.handle(&key, &snapshot.id),
                Err(BrowserError::MissingResource { .. })
            ));
            assert!(matches!(
                resource_store.handle(&key, &screenshot.id),
                Err(BrowserError::MissingResource { .. })
            ));
            drop(gateway);
            drop(resource_store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn browser_native_lifecycle_boundaries_release_every_retained_repair_resource() {
        #[derive(Clone, Copy, Debug)]
        enum Boundary {
            StopTab,
            StopWorkspace,
            LogicalTabClose,
            ResetWorkspace,
            ClearProject,
            DirectInput,
            SelectConversation,
            RestartServer,
            KillPortRestart,
            RestartAiConversation,
            RestartSsh,
            CloseConversation,
            DeleteProject,
            QuitApplication,
        }

        for boundary in [
            Boundary::StopTab,
            Boundary::StopWorkspace,
            Boundary::LogicalTabClose,
            Boundary::ResetWorkspace,
            Boundary::ClearProject,
            Boundary::DirectInput,
            Boundary::SelectConversation,
            Boundary::RestartServer,
            Boundary::KillPortRestart,
            Boundary::RestartAiConversation,
            Boundary::RestartSsh,
            Boundary::CloseConversation,
            Boundary::DeleteProject,
            Boundary::QuitApplication,
        ] {
            let label = format!("{boundary:?}").to_ascii_lowercase();
            let root = std::env::temp_dir().join(format!(
                "devmanager-native-repair-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let resource_store = BrowserResourceStore::open(
                root.join("resources"),
                BrowserResourceLimits {
                    max_temporary_count: 0,
                    max_temporary_bytes: 1024 * 1024,
                    max_resource_bytes: 1024 * 1024,
                },
            )
            .unwrap();
            let project_id = format!("native-repair-{label}");
            let key = BrowserWorkspaceKey::new(project_id.clone(), "conversation").unwrap();
            let sibling_key =
                BrowserWorkspaceKey::new(project_id.clone(), "sibling-conversation").unwrap();
            let isolated_key =
                BrowserWorkspaceKey::new(format!("isolated-{label}"), "conversation").unwrap();
            let (bridge, mut inbox) = browser_command_channel(8);
            let coordinator = bridge.replay_coordinator();
            let started = coordinator
                .start(key.clone(), provider_repair_plan(&label))
                .unwrap();
            let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
                started.instance.clone(),
                vec!["password".to_string()],
            )
            .unwrap();
            prompt
                .edit(&started.instance, "password", "native-repair-secret")
                .unwrap();
            let (submission, _) = prompt.submit(&started.instance).unwrap();
            coordinator
                .submit_secrets(&started.instance, submission)
                .unwrap();
            let secret_lease = started.execution.secret_lease("password").unwrap();
            let repair = coordinator
                .reserve_locator_repair_capture(
                    &started.instance,
                    &resource_store,
                    0,
                    BrowserReplayLocatorSlot::PrimaryAction,
                    "runtime-tab",
                    BrowserRevision(1),
                    BrowserReplayRepairResumeCursor::Action,
                )
                .unwrap();
            let snapshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairSnapshot,
                    "application/json",
                    b"{}",
                )
                .unwrap();
            let screenshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairScreenshot,
                    "image/png",
                    b"png",
                )
                .unwrap();
            coordinator
                .publish_locator_repair(&repair, &snapshot, &screenshot)
                .unwrap();
            let sibling = coordinator
                .start(
                    sibling_key.clone(),
                    provider_repair_plan(&format!("{label}-sibling")),
                )
                .unwrap();
            let isolated = coordinator
                .start(
                    isolated_key.clone(),
                    provider_repair_plan(&format!("{label}-isolated")),
                )
                .unwrap();
            let controller = bridge.bind(key.clone(), Duration::from_secs(1));
            let pending = tokio::spawn(async move {
                controller
                    .request(BrowserCommand::Reload {
                        tab_id: "runtime-tab".to_string(),
                    })
                    .await
            });
            let request = inbox.recv().await.expect("retained native repair request");

            match boundary {
                Boundary::StopTab => {
                    bridge
                        .bind(key.clone(), Duration::from_secs(1))
                        .notify(BrowserCommand::Stop {
                            tab_id: Some("runtime-tab".to_string()),
                        })
                        .await
                        .unwrap();
                }
                Boundary::StopWorkspace => {
                    bridge
                        .bind(key.clone(), Duration::from_secs(1))
                        .notify(BrowserCommand::Stop { tab_id: None })
                        .await
                        .unwrap();
                }
                Boundary::LogicalTabClose => {
                    bridge
                        .bind(key.clone(), Duration::from_secs(1))
                        .notify(BrowserCommand::CloseTab {
                            tab_id: "runtime-tab".to_string(),
                        })
                        .await
                        .unwrap();
                }
                Boundary::ResetWorkspace => {
                    bridge
                        .bind(key.clone(), Duration::from_secs(1))
                        .notify(BrowserCommand::ResetWorkspace)
                        .await
                        .unwrap();
                }
                Boundary::ClearProject => {
                    bridge
                        .bind(key.clone(), Duration::from_secs(1))
                        .notify(BrowserCommand::ClearProjectProfile)
                        .await
                        .unwrap();
                }
                Boundary::DirectInput => {
                    bridge.observe_host_event(&BrowserHostEvent::UserInput {
                        workspace_key: key.clone(),
                        tab_id: "runtime-tab".to_string(),
                        kind: BrowserUserInputKind::Keyboard,
                    });
                }
                Boundary::SelectConversation
                | Boundary::RestartServer
                | Boundary::KillPortRestart
                | Boundary::RestartAiConversation
                | Boundary::RestartSsh
                | Boundary::CloseConversation => bridge.interrupt_workspace(&key),
                Boundary::DeleteProject => bridge.interrupt_project(&project_id),
                Boundary::QuitApplication => bridge.interrupt_all(),
            }

            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                BrowserReplayStatus::Cancelled,
                "{boundary:?} must terminalize replay before releasing evidence"
            );
            assert_eq!(
                secret_lease.expose(|_| ()),
                Err(BrowserReplaySecretError::ClosedStore),
                "{boundary:?} must close the retained secret lease"
            );
            assert!(matches!(
                resource_store.handle(&key, &snapshot.id),
                Err(BrowserError::MissingResource { .. })
            ));
            assert!(matches!(
                resource_store.handle(&key, &screenshot.id),
                Err(BrowserError::MissingResource { .. })
            ));
            request.respond(Ok(BrowserResponse::Acknowledged));
            assert_eq!(
                pending.await.unwrap(),
                Err(BrowserError::Interrupted),
                "{boundary:?} must fence a retained late response"
            );
            assert_eq!(
                coordinator
                    .status(&started.instance)
                    .unwrap()
                    .current_step_index,
                0,
                "{boundary:?} must not advance or write after cancellation"
            );

            let sibling_status = coordinator.status(&sibling.instance).unwrap().status;
            let isolated_status = coordinator.status(&isolated.instance).unwrap().status;
            match boundary {
                Boundary::ClearProject | Boundary::DeleteProject => {
                    assert_eq!(sibling_status, BrowserReplayStatus::Cancelled);
                    assert_eq!(isolated_status, BrowserReplayStatus::NeedsUserSecret);
                }
                Boundary::QuitApplication => {
                    assert_eq!(sibling_status, BrowserReplayStatus::Cancelled);
                    assert_eq!(isolated_status, BrowserReplayStatus::Cancelled);
                }
                _ => {
                    assert_eq!(sibling_status, BrowserReplayStatus::NeedsUserSecret);
                    assert_eq!(isolated_status, BrowserReplayStatus::NeedsUserSecret);
                }
            }
            drop(resource_store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
