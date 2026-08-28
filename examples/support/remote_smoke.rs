//! Shared isolated-host fixture helpers for remote smoke examples.
//!
//! Owns tempfile roots, named profiles, ManagedChildGuard host jobs, and the
//! canonical project/task/remote-setup bootstrap. Never touches installed or
//! watch profiles. Teardown is fixture-owned and bounded.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{future::select, future::Either};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::header::LOCATION;
use hyper::{Method, Request, Uri};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose, SanType};
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::WebSocketStream;

use devmanager::client::{
    build_rustls_client_config, validate_http_header_value, HostClient, HostClientConfig,
    RemoteEndpoint, RemoteHttpResponse, RemoteIo, RemoteTlsOptions, REMOTE_HTTP_MAX_BODY_BYTES,
};
use devmanager::domain::cockpit::{TaskCockpitQuery, TaskCockpitResult};
use devmanager::domain::{ClientId, CommandId, EnvironmentId, ProjectId, TaskId};
use devmanager::host::remote_setup::{
    RemoteListenOptions, RemoteSetupReply, RemoteSetupRequest, RemoteSetupState,
};
use devmanager::process::job::ManagedChildGuard;
use devmanager::protocol::{
    Capability, CapabilitySet, FrameLimits, MAX_HANDSHAKE_MESSAGE_BYTES, MAX_SEALED_FRAME_BYTES,
};

/// Cleared on every fixture spawn so an ambient shell profile cannot leak in.
pub const CLEARED_PROFILE_ENV: &[&str] = &[
    "DEVMANAGER_PROFILE",
    "DEVMANAGER_INSTANCE_LABEL",
    "DEVMANAGER_RUNTIME_KIND",
    "DEVMANAGER_CONFIG_DIR",
    "DEVMANAGER_APP_IDENTITY",
];

/// Bounded ephemeral CA + `127.0.0.1` SAN leaf written under a fixture temp root.
///
/// Root PEM is for this smoke's HTTP/WSS client trust only — never OS-installed.
#[derive(Clone)]
pub struct EphemeralLoopbackTlsFiles {
    pub certificate_path: PathBuf,
    pub private_key_path: PathBuf,
    /// Trust-anchor PEM passed into `RemoteTlsOptions::additional_ca_pem` only.
    pub ca_pem: String,
}

impl std::fmt::Debug for EphemeralLoopbackTlsFiles {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EphemeralLoopbackTlsFiles")
            .field("certificate_path", &self.certificate_path)
            .field("private_key_path", &"<redacted-path>")
            .field("ca_pem_bytes", &self.ca_pem.len())
            .finish()
    }
}

/// Generate a short-lived test CA and IP-SAN leaf under `dir` (rcgen, same shape
/// as `src/remote/web/tls.rs` chain helpers).
pub fn generate_ephemeral_loopback_tls(
    dir: &Path,
) -> Result<EphemeralLoopbackTlsFiles, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(vec!["DevManager smoke CA".to_string()])?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key)?;

    let leaf_key = KeyPair::generate()?;
    let mut leaf_params = CertificateParams::new(Vec::<String>::new())?;
    leaf_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    ))];
    leaf_params
        .key_usages
        .push(KeyUsagePurpose::DigitalSignature);
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key)?;

    let chain_pem = format!("{}{}", leaf_cert.pem(), ca_cert.pem());
    let key_pem = leaf_key.serialize_pem();
    let certificate_path = dir.join("loopback-chain.pem");
    let private_key_path = dir.join("loopback-leaf.key.pem");
    std::fs::write(&certificate_path, chain_pem.as_bytes())?;
    std::fs::write(&private_key_path, key_pem.as_bytes())?;
    Ok(EphemeralLoopbackTlsFiles {
        certificate_path,
        private_key_path,
        ca_pem: ca_cert.pem(),
    })
}

pub fn advertised_https_loopback_origin(port: u16) -> String {
    format!("https://127.0.0.1:{port}")
}

pub fn tls_options_trusting_ca(ca_pem: &str) -> RemoteTlsOptions {
    RemoteTlsOptions {
        additional_ca_pem: Some(ca_pem.to_string()),
    }
}

fn remaining_until(deadline_at: Instant) -> Result<Duration, Box<dyn std::error::Error>> {
    let remaining = deadline_at.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("deadline".into())
    } else {
        Ok(remaining)
    }
}

async fn dial_remote_io(
    endpoint: &RemoteEndpoint,
    tls: &RemoteTlsOptions,
) -> Result<RemoteIo, Box<dyn std::error::Error>> {
    let (host, port) = endpoint_host_port(endpoint)?;
    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    if !endpoint.requires_tls() {
        return Ok(RemoteIo::Plain(tcp));
    }
    let config =
        build_rustls_client_config(tls).map_err(|error| format!("tls config: {error:?}"))?;
    let server_name = ServerName::try_from(host).map_err(|_| "server name for TLS dial")?;
    let tls_stream = TlsConnector::from(config)
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("tls handshake: {error}"))?;
    Ok(RemoteIo::Tls(tls_stream))
}

fn endpoint_host_port(
    endpoint: &RemoteEndpoint,
) -> Result<(String, u16), Box<dyn std::error::Error>> {
    let parsed = url::Url::parse(endpoint.http_base()).map_err(|_| "endpoint http_base")?;
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err("endpoint host".into()),
    };
    let port = parsed.port_or_known_default().ok_or("endpoint port")?;
    Ok((host, port))
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

/// HTTPS JSON POST with caller Origin and optional pairing cookie (no redirect follow).
///
/// Stock `exchange_http` always stamps Origin from the dialed endpoint; cross-origin
/// pair needs a distinct Origin while still dialing the host authority.
pub async fn https_json_post_with_origin_until(
    endpoint: &RemoteEndpoint,
    path: &str,
    origin: &str,
    cookie_header: Option<&str>,
    json_body: &[u8],
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<RemoteHttpResponse, Box<dyn std::error::Error>> {
    validate_http_header_value(origin).map_err(|error| format!("origin: {error:?}"))?;
    if let Some(cookie) = cookie_header {
        validate_http_header_value(cookie).map_err(|error| format!("cookie: {error:?}"))?;
    }
    if json_body.len() > REMOTE_HTTP_MAX_BODY_BYTES {
        return Err("request body oversized".into());
    }
    let remaining = remaining_until(deadline_at)?;
    let uri = format!(
        "{}{}",
        endpoint.http_base().trim_end_matches('/'),
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    );
    tokio::time::timeout(remaining, async {
        let io = dial_remote_io(endpoint, tls).await?;
        let parsed: Uri = uri.parse().map_err(|_| "request uri")?;
        let authority = parsed
            .authority()
            .map(|value| value.as_str().to_string())
            .ok_or("request authority")?;
        let path_and_query = parsed
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path_and_query)
            .header(hyper::header::HOST, authority)
            .header(hyper::header::ORIGIN, origin)
            .header(hyper::header::ACCEPT, "application/json")
            .header(hyper::header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie_header {
            builder = builder.header(hyper::header::COOKIE, cookie);
        }
        let request = builder
            .body(Full::new(Bytes::copy_from_slice(json_body)))
            .map_err(|_| "request body")?;

        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
                .await
                .map_err(|error| format!("http handshake: {error}"))?;
        let mut connection = std::pin::pin!(connection);
        let send = sender.send_request(request);
        let mut send = std::pin::pin!(send);
        let mut connection_finished = false;
        let response = match select(&mut send, &mut connection).await {
            Either::Left((Ok(response), _)) => response,
            Either::Left((Err(error), _)) => {
                return Err(format!("http send: {error}").into());
            }
            Either::Right((conn_result, pending_send)) => {
                conn_result.map_err(|error| format!("http connection: {error}"))?;
                connection_finished = true;
                pending_send
                    .await
                    .map_err(|error| format!("http send after close: {error}"))?
            }
        };

        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let set_cookie = None;

        let collect = Limited::new(response.into_body(), REMOTE_HTTP_MAX_BODY_BYTES).collect();
        let mut collect = std::pin::pin!(collect);
        let collected = if connection_finished {
            collect
                .await
                .map_err(|_| "response body oversized or truncated")?
        } else {
            match select(&mut collect, &mut connection).await {
                Either::Left((Ok(body), _)) => body,
                Either::Left((Err(_), _)) => {
                    return Err("response body oversized or truncated".into());
                }
                Either::Right((conn_result, pending)) => {
                    conn_result.map_err(|error| format!("http connection: {error}"))?;
                    pending
                        .await
                        .map_err(|_| "response body oversized or truncated")?
                }
            }
        };
        let body = collected.to_bytes().to_vec();
        let _ = sender;
        Ok(RemoteHttpResponse {
            status,
            body,
            set_cookie,
            location,
        })
    })
    .await
    .map_err(|_| Box::<dyn std::error::Error>::from("https post deadline"))?
}

/// Open WSS with a caller Origin (no cookie). Dial still uses the endpoint host.
pub async fn open_wss_with_origin_until(
    endpoint: &RemoteEndpoint,
    origin: &str,
    tls: &RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<WebSocketStream<RemoteIo>, Box<dyn std::error::Error>> {
    validate_http_header_value(origin).map_err(|error| format!("origin: {error:?}"))?;
    let remaining = remaining_until(deadline_at)?;
    tokio::time::timeout(remaining, async {
        let mut request = endpoint
            .ws_url()
            .into_client_request()
            .map_err(|_| "websocket request")?;
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::ORIGIN,
            origin.parse().map_err(|_| "origin header")?,
        );
        let io = dial_remote_io(endpoint, tls).await?;
        let (socket, _response) =
            tokio_tungstenite::client_async_with_config(request, io, Some(connect_ws_config()))
                .await
                .map_err(|error| format!("websocket upgrade: {error}"))?;
        Ok(socket)
    })
    .await
    .map_err(|_| Box::<dyn std::error::Error>::from("wss open deadline"))?
}

fn resolve_listen_port(preferred_port: Option<u16>) -> Result<u16, Box<dyn std::error::Error>> {
    match preferred_port {
        Some(port) => Ok(port),
        None => {
            let socket = std::net::TcpListener::bind("127.0.0.1:0")?;
            let port = socket.local_addr()?.port();
            drop(socket);
            Ok(port)
        }
    }
}

fn path_as_string(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "TLS path is not valid UTF-8".into())
}

pub fn require_windows_debug() -> Result<(), Box<dyn std::error::Error>> {
    if !cfg!(all(debug_assertions, windows)) {
        return Err("smoke fixture requires a Windows debug build".into());
    }
    Ok(())
}

pub fn sibling_host_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let host = executable
        .parent()
        .and_then(|path| path.parent())
        .ok_or("example target path")?
        .join("devmanager-host.exe");
    if !host.is_file() {
        return Err("build the sibling devmanager-host binary first".into());
    }
    Ok(host)
}

pub fn host_client_config(profile: &str, client_build: &str) -> HostClientConfig {
    HostClientConfig {
        named_profile: profile.to_string(),
        client_build: client_build.into(),
        client_id: ClientId::new(),
        requested: CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::EventReplay,
            Capability::TaskCockpit,
            Capability::ProviderInput,
            Capability::HostShutdown,
        ]),
        limits: FrameLimits::v1_default(),
    }
}

pub async fn connect_fixture_client(config: HostClientConfig) -> Result<HostClient, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match HostClient::connect(config.clone()).await {
            Ok(client) => return Ok(client),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(format!("host did not start: {error}"));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// One isolated `devmanager-host` job under a unique tempfile + named profile.
pub struct IsolatedHostFixture {
    // Fields drop in declaration order: join the owned Job before TempDir
    // attempts to remove the profile, database, keys and provider journals.
    owner: ManagedChildGuard,
    profile: String,
    workspace: PathBuf,
    host_command: Command,
    pub client_build: String,
    _temp: tempfile::TempDir,
}

impl IsolatedHostFixture {
    pub fn spawn(
        prefix: &str,
        instance_label: &str,
        client_build: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp = tempfile::Builder::new().prefix(prefix).tempdir()?;
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let profile = format!(
            "{}-{}",
            prefix.trim_matches('-'),
            uuid::Uuid::new_v4().simple()
        );
        let host = sibling_host_binary()?;
        let mut host_command = Command::new(host);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            host_command.creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0);
        }
        host_command
            .args([
                "--foreground",
                "--profile",
                &profile,
                "--instance-label",
                instance_label,
                "--parent-pid",
                &std::process::id().to_string(),
                "--config-base",
            ])
            .arg(temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for key in CLEARED_PROFILE_ENV {
            host_command.env_remove(key);
        }
        let owner = ManagedChildGuard::attach(host_command.spawn()?)?;
        Ok(Self {
            _temp: temp,
            profile,
            workspace,
            host_command,
            owner,
            client_build: client_build.into(),
        })
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn fixture_root(&self) -> &Path {
        self._temp.path()
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn owner_pid(&mut self) -> u32 {
        self.owner.child_mut().id()
    }

    pub fn local_config(&self) -> HostClientConfig {
        host_client_config(&self.profile, &self.client_build)
    }

    pub async fn connect_local(&self) -> Result<HostClient, String> {
        connect_fixture_client(self.local_config()).await
    }

    /// Create the smoke project and two tasks. `first_task_id` is caller-owned so
    /// two hosts can deliberately share the same raw TaskId. With Codex enabled,
    /// the second task has a provider claim but stays unstarted until first Send.
    pub async fn create_project_and_tasks(
        &self,
        client: &mut HostClient,
        with_codex: bool,
        start_first_provider: bool,
        first_task_id: TaskId,
        first_title: &str,
        second_title: &str,
    ) -> Result<(ProjectId, TaskId), Box<dyn std::error::Error>> {
        let config = client
            .query_task_cockpit(
                TaskId::new(),
                TaskCockpitQuery::ConfigCreateProject {
                    name: format!("{} workspace", first_title),
                    root_path: self.workspace.to_string_lossy().into_owned(),
                },
            )
            .await
            .map_err(|error| format!("project request: {error}"))?
            .map_err(|error| format!("project: {error:?}"))?;
        let TaskCockpitResult::Config(config) = config else {
            return Err("missing project snapshot".into());
        };
        let project_id: ProjectId = config
            .projects
            .first()
            .ok_or("missing smoke project")?
            .workspace_id
            .parse()?;
        let second_task_id = TaskId::new();
        for (index, (task_id, title)) in
            [(first_task_id, first_title), (second_task_id, second_title)]
                .into_iter()
                .enumerate()
        {
            let command = devmanager::client::action::task_create_v2_command(
                CommandId::new(),
                client.client_id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64,
                devmanager::client::action::TaskCreateV2Arguments {
                    task_id,
                    environment_id: EnvironmentId::new(),
                    title: title.into(),
                    description: None,
                    project_id,
                    workspace: devmanager::workspace::WorkspaceRequest::confirmed_external(
                        &self.workspace,
                    ),
                    primary_provider: with_codex
                        .then_some(devmanager::providers::ProviderKind::Codex),
                    defer_primary_provider_start: !(with_codex
                        && start_first_provider
                        && index == 0),
                },
            )?;
            let receipt = client
                .execute_command(command)
                .await
                .map_err(|error| format!("task create request: {error}"))?;
            if !matches!(receipt, devmanager::domain::CommandReceipt::Accepted { .. }) {
                return Err(format!("task create: {receipt:?}").into());
            }
        }
        Ok((project_id, second_task_id))
    }

    /// Bind loopback remote access and wait until Listening.
    ///
    /// When `preferred_port` is `None`, picks an ephemeral free port. Pass the
    /// same port after a same-profile restart so saved trust endpoints remain
    /// valid for `connect_trusted_host` without re-pairing.
    pub async fn enable_remote_listening(
        &self,
        client: &mut HostClient,
    ) -> Result<u16, Box<dyn std::error::Error>> {
        self.enable_remote_listening_on(client, None).await
    }

    pub async fn enable_remote_listening_on(
        &self,
        client: &mut HostClient,
        preferred_port: Option<u16>,
    ) -> Result<u16, Box<dyn std::error::Error>> {
        let port = resolve_listen_port(preferred_port)?;
        self.enable_remote_listening_with_options(
            client,
            RemoteListenOptions {
                bind_address: "127.0.0.1".into(),
                port,
                advertised_origin: None,
                certificate_path: None,
                private_key_path: None,
            },
        )
        .await?;
        Ok(port)
    }

    /// Enable HTTPS/WSS listening with advertised origin + on-disk chain/key.
    ///
    /// Returns `(listen_port, advertised_origin)`. Origin port always matches
    /// the chosen listen port. Existing plaintext helpers stay unchanged.
    pub async fn enable_remote_listening_tls(
        &self,
        client: &mut HostClient,
        tls_files: &EphemeralLoopbackTlsFiles,
        preferred_port: Option<u16>,
    ) -> Result<(u16, String), Box<dyn std::error::Error>> {
        let port = resolve_listen_port(preferred_port)?;
        let advertised_origin = advertised_https_loopback_origin(port);
        self.enable_remote_listening_with_options(
            client,
            RemoteListenOptions {
                bind_address: "127.0.0.1".into(),
                port,
                advertised_origin: Some(advertised_origin.clone()),
                certificate_path: Some(path_as_string(&tls_files.certificate_path)?),
                private_key_path: Some(path_as_string(&tls_files.private_key_path)?),
            },
        )
        .await?;
        Ok((port, advertised_origin))
    }

    /// Reusable Enable + wait-until-Listening using caller-owned options.
    pub async fn enable_remote_listening_with_options(
        &self,
        client: &mut HostClient,
        options: RemoteListenOptions,
    ) -> Result<(), Box<dyn std::error::Error>> {
        client
            .query_remote_access(RemoteSetupRequest::Enable {
                command_id: CommandId::new(),
                options,
            })
            .await
            .map_err(|error| format!("enable request: {error}"))?
            .map_err(|error| format!("enable: {error:?}"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let reply = client
                .query_remote_access(RemoteSetupRequest::Snapshot)
                .await
                .map_err(|error| format!("status request: {error}"))?
                .map_err(|error| format!("status: {error:?}"))?;
            if let RemoteSetupReply::Snapshot { status } = reply {
                if status.state == RemoteSetupState::Listening {
                    break;
                }
                if status.state == RemoteSetupState::Failed {
                    return Err(format!("setup: {:?}", status.error).into());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("setup deadline".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    pub async fn pairing_info(
        &self,
        client: &mut HostClient,
    ) -> Result<RemoteSetupReply, Box<dyn std::error::Error>> {
        client
            .query_remote_access(RemoteSetupRequest::PairingInfo)
            .await
            .map_err(|error| format!("pairing request: {error}"))?
            .map_err(|error| format!("pairing: {error:?}"))
            .map_err(Into::into)
    }

    /// Stop only this fixture Job, then respawn with the same profile/keys.
    pub fn restart_same_profile(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.owner
            .terminate_and_join_until(Instant::now() + Duration::from_secs(15))?;
        self.owner = ManagedChildGuard::attach(self.host_command.spawn()?)?;
        Ok(())
    }

    /// Hard-stop the owned host Job without a graceful quit (fleet stop-A case).
    pub fn terminate_owned_job(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.owner
            .terminate_and_join_until(Instant::now() + Duration::from_secs(15))?;
        Ok(())
    }

    pub async fn disable_remote_and_quit(
        &mut self,
        client: &mut HostClient,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = client
            .query_remote_access(RemoteSetupRequest::Disable {
                command_id: CommandId::new(),
            })
            .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if matches!(
                client.query_remote_access(RemoteSetupRequest::Snapshot).await,
                Ok(Ok(RemoteSetupReply::Snapshot { status }))
                    if status.state == RemoteSetupState::Disabled
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Ok(Ok(inspection)) = client.inspect_host_quit().await {
            let _ = client
                .confirm_host_quit(CommandId::new(), inspection.inspection_id, false)
                .await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if self.owner.child_mut().try_wait().ok().flatten().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

/// Production native trust pair + disk-reopen reconnect path (original smoke).
pub async fn exercise_native_client(
    port: u16,
    code: &str,
    fixture_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use devmanager::client::{
        connect_trusted_host, pair_enroll_and_connect, ConnectTrustedOptions, PairEnrollRequest,
        RemoteTrustStore,
    };
    let trust_root = fixture_root.join("native-client");
    let store = RemoteTrustStore::open(trust_root.clone())?;
    let (mut native, record) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port}"),
            pairing_code: zeroize::Zeroizing::new(code.to_string()),
            label: Some("Native smoke client".into()),
            ..PairEnrollRequest::default()
        },
    )
    .await?;
    let mut subscription = devmanager::client::ClientSubscription::new();
    subscription.synchronize(&mut native).await?;
    let model = subscription.model().ok_or("native model missing")?;
    println!(
        "Native trusted client synchronized owner {}",
        uuid::Uuid::from_bytes(record.host_public_id)
    );
    let _ = model;
    subscription.release(&mut native).await?;
    let assigned = native.client_id();
    drop(native);
    drop(store);
    let reopened = RemoteTrustStore::open(trust_root)?;
    let mut native = connect_trusted_host(
        &reopened,
        record.host_public_id,
        ConnectTrustedOptions::default(),
    )
    .await?;
    if native.client_id() != assigned {
        return Err("reconnect changed native client identity".into());
    }
    let mut subscription = devmanager::client::ClientSubscription::new();
    subscription.synchronize(&mut native).await?;
    subscription.release(&mut native).await?;
    println!("Native persisted-trust reconnect preserved assigned client identity");
    Ok(())
}
