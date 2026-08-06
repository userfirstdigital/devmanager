//! Profile-scoped named-pipe ClientHello/ServerHello handshake transport.
//!
//! After Hello, each connection serves synchronous ClientRequest/ServerResponse
//! frames. Concurrent connections submit decoded requests to the single
//! host-owned CommandBus executor; transport tasks never touch the bus.
//! Snapshots, subscriptions, and fan-out live later.

use std::time::Duration;

use uuid::Uuid;

use crate::config::paths::AppProfile;
use crate::domain::ClientId;
use crate::kernel::CommandBus;
use crate::protocol::{
    Capability, CapabilitySet, ClientHello, ClientHelloError, ClientRequest, FrameLimits,
    MessagePackCodec, MessagePackError, NegotiatedParameters, PhysicalFrameCodec,
    PhysicalFrameError, ProfileFingerprint, ServerBuildError, ServerHello, ServerHelloError,
};

use super::connection::{dispatch_authenticated_request, HostRequestHandle};

const PIPE_PRODUCT_PREFIX: &str = r"\\.\pipe\devmanager-";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
// SnapshotPage/EventPage encoded_bytes cover the page body, not the surrounding
// QueryResult/QueryReply/ServerResponse named maps. Protocol v1 sends that
// wrapped response in one physical frame, so reserve bounded headroom for the
// fixed correlation/envelope fields before granting PagedSnapshots or EventReplay.
const PAGE_RESPONSE_ENVELOPE_HEADROOM_BYTES: u32 = 1024;

/// Exact protected two-ACE SDDL form: LocalSystem + one caller-supplied user SID.
pub(crate) fn protected_pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})")
}

/// Configuration for a one-shot ClientHello acceptor.
#[derive(Debug, Clone)]
pub struct AcceptHelloConfig {
    pub host_boot_id: Uuid,
    pub server_build: String,
    pub supported: CapabilitySet,
    pub local_limits: FrameLimits,
}

/// Successful handshake outcome observed by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedHello {
    pub client_id: ClientId,
    pub negotiated: NegotiatedParameters,
    pub server_hello: ServerHello,
}

/// Errors from pipe endpoint derivation or the handshake transport.
#[derive(Debug)]
pub enum IpcError {
    InvalidProfile(String),
    Unsupported,
    UnsupportedCapability,
    Io(std::io::Error),
    Timeout,
    Frame(PhysicalFrameError),
    MessagePack(MessagePackError),
    ClientHello(ClientHelloError),
    ServerHello(ServerHelloError),
    ProfileMismatch,
    HelloInconsistent,
    Unauthorized,
    UnexpectedResponse,
    CorrelationMismatch,
    ConnectionPoisoned,
    Busy,
    Unavailable,
    Security(String),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(name) => write!(f, "invalid host ipc profile name: {name:?}"),
            Self::Unsupported => write!(f, "named-pipe ipc is unsupported on this platform"),
            Self::UnsupportedCapability => {
                write!(f, "requested capability is not granted on this connection")
            }
            Self::Io(error) => write!(f, "named-pipe ipc I/O error: {error}"),
            Self::Timeout => write!(f, "named-pipe operation timed out"),
            Self::Frame(error) => error.fmt(f),
            Self::MessagePack(error) => error.fmt(f),
            Self::ClientHello(error) => error.fmt(f),
            Self::ServerHello(error) => error.fmt(f),
            Self::ProfileMismatch => {
                write!(
                    f,
                    "client hello profile fingerprint does not match this pipe"
                )
            }
            Self::HelloInconsistent => {
                write!(f, "server hello is inconsistent with the sent client hello")
            }
            Self::Unauthorized => write!(f, "request client_id does not match authenticated hello"),
            Self::UnexpectedResponse => write!(f, "response variant did not match the request"),
            Self::CorrelationMismatch => {
                write!(f, "response correlation id did not match the request")
            }
            Self::ConnectionPoisoned => {
                write!(
                    f,
                    "named-pipe connection is poisoned and must not be reused"
                )
            }
            Self::Busy => write!(f, "kernel store is busy"),
            Self::Unavailable => write!(f, "kernel store is temporarily unavailable"),
            Self::Security(message) => write!(f, "named-pipe security error: {message}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::MessagePack(error) => Some(error),
            Self::ClientHello(error) => Some(error),
            Self::ServerHello(error) => Some(error),
            Self::InvalidProfile(_)
            | Self::Unsupported
            | Self::UnsupportedCapability
            | Self::Timeout
            | Self::ProfileMismatch
            | Self::HelloInconsistent
            | Self::Unauthorized
            | Self::UnexpectedResponse
            | Self::CorrelationMismatch
            | Self::ConnectionPoisoned
            | Self::Busy
            | Self::Unavailable
            | Self::Security(_) => None,
        }
    }
}

/// Fingerprint for a validated named profile (same digest used by the pipe endpoint).
pub fn profile_fingerprint_for_named_profile(
    profile: &str,
) -> Result<ProfileFingerprint, IpcError> {
    let profile = normalize_named_profile(profile)?;
    Ok(ProfileFingerprint::hash_normalized(&profile))
}

/// Derive the boot-independent pipe endpoint for a normalized named profile.
pub fn pipe_endpoint_for_named_profile(profile: &str) -> Result<String, IpcError> {
    let fingerprint = profile_fingerprint_for_named_profile(profile)?;
    Ok(format!("{PIPE_PRODUCT_PREFIX}{}", fingerprint.to_hex()))
}

fn normalize_named_profile(profile: &str) -> Result<String, IpcError> {
    match AppProfile::named(profile) {
        Ok(AppProfile::Named(name)) => Ok(name),
        Ok(_) => Err(IpcError::InvalidProfile(profile.to_string())),
        Err(_) => Err(IpcError::InvalidProfile(profile.to_string())),
    }
}

pub(crate) fn handshake_codecs() -> Result<(PhysicalFrameCodec, MessagePackCodec), IpcError> {
    let limits = FrameLimits::v1_default();
    let physical = PhysicalFrameCodec::from_limits(limits)
        .map_err(|error| IpcError::ClientHello(ClientHelloError::FrameLimits(error)))?;
    let message = MessagePackCodec::from_limits(limits)
        .map_err(|error| IpcError::ClientHello(ClientHelloError::FrameLimits(error)))?;
    Ok((physical, message))
}

fn negotiated_codecs(
    limits: FrameLimits,
) -> Result<(PhysicalFrameCodec, MessagePackCodec), IpcError> {
    let physical = PhysicalFrameCodec::from_limits(limits)
        .map_err(|error| IpcError::ClientHello(ClientHelloError::FrameLimits(error)))?;
    let message = MessagePackCodec::from_limits(limits)
        .map_err(|error| IpcError::ClientHello(ClientHelloError::FrameLimits(error)))?;
    Ok((physical, message))
}

fn fence_capabilities_by_transport(mut negotiated: NegotiatedParameters) -> NegotiatedParameters {
    if !page_response_fits_transport(negotiated.limits) {
        let mut bits = negotiated.capabilities.bits();
        if negotiated.capabilities.contains(Capability::PagedSnapshots) {
            bits &= !Capability::PagedSnapshots.bit();
        }
        if negotiated.capabilities.contains(Capability::EventReplay) {
            bits &= !Capability::EventReplay.bit();
        }
        negotiated.capabilities = CapabilitySet::from_bits(bits);
    }
    negotiated
}

fn page_response_fits_transport(limits: FrameLimits) -> bool {
    let Some(required_payload_bytes) = limits
        .max_page_encoded_bytes
        .checked_add(PAGE_RESPONSE_ENVELOPE_HEADROOM_BYTES)
    else {
        return false;
    };
    required_payload_bytes <= limits.max_physical_frame_bytes
        && required_payload_bytes <= limits.max_reassembled_message_bytes
}

/// Bound named-pipe listener that accepts exactly one ClientHello.
pub struct HelloListener {
    endpoint: String,
    expected_fingerprint: ProfileFingerprint,
    config: AcceptHelloConfig,
    #[cfg(windows)]
    server: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl HelloListener {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn expected_fingerprint(&self) -> ProfileFingerprint {
        self.expected_fingerprint
    }

    pub fn bind(profile: &str, config: AcceptHelloConfig) -> Result<Self, IpcError> {
        let expected_fingerprint = profile_fingerprint_for_named_profile(profile)?;
        let endpoint = format!("{PIPE_PRODUCT_PREFIX}{}", expected_fingerprint.to_hex());
        config
            .local_limits
            .validate_offer()
            .map_err(|error| IpcError::ClientHello(ClientHelloError::FrameLimits(error)))?;
        if config.server_build.is_empty() {
            return Err(IpcError::ServerHello(ServerHelloError::Build(
                ServerBuildError::Empty,
            )));
        }
        #[cfg(windows)]
        {
            windows_bind(endpoint, expected_fingerprint, config, true)
        }
        #[cfg(not(windows))]
        {
            let _ = endpoint;
            let _ = expected_fingerprint;
            let _ = config;
            Err(IpcError::Unsupported)
        }
    }

    /// Accept one connection, complete Hello, and retain the pipe for requests.
    pub async fn accept(self) -> Result<HostConnection, IpcError> {
        #[cfg(windows)]
        {
            windows_accept(self).await
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            Err(IpcError::Unsupported)
        }
    }

    /// Accept one connection while preserving a same-config successor pipe.
    ///
    /// The outer error means the listener chain could not be preserved and is
    /// fatal to the host. The inner error is scoped to the attempted handshake;
    /// the returned successor remains ready for another client.
    pub async fn accept_with_successor(
        self,
    ) -> Result<(Result<HostConnection, IpcError>, Self), IpcError> {
        #[cfg(windows)]
        {
            windows_accept_with_successor(self).await
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            Err(IpcError::Unsupported)
        }
    }

    /// Accept Hello and drop the retained pipe (compatibility wrapper).
    pub async fn accept_hello(self) -> Result<AcceptedHello, IpcError> {
        let connection = self.accept().await?;
        Ok(connection.accepted_hello())
    }
}

/// Host-side authenticated pipe after Hello, serving one request at a time.
pub struct HostConnection {
    client_id: ClientId,
    negotiated: NegotiatedParameters,
    server_hello: ServerHello,
    physical: PhysicalFrameCodec,
    message: MessagePackCodec,
    poisoned: bool,
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl HostConnection {
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    pub fn negotiated(&self) -> NegotiatedParameters {
        self.negotiated
    }

    pub fn server_hello(&self) -> &ServerHello {
        &self.server_hello
    }

    pub fn accepted_hello(&self) -> AcceptedHello {
        AcceptedHello {
            client_id: self.client_id,
            negotiated: self.negotiated,
            server_hello: self.server_hello.clone(),
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Wait indefinitely for the first request byte, then complete under one deadline.
    ///
    /// Exclusive compatibility path used by ipc_protocol tests: dispatches
    /// directly against a caller-owned [`CommandBus`]. Concurrent host serving
    /// uses [`Self::serve_request_on_executor`] instead.
    pub async fn serve_request(&mut self, bus: &mut CommandBus) -> Result<(), IpcError> {
        connection_ensure_live(self.poisoned)?;
        #[cfg(windows)]
        {
            let result = windows_serve_request(self, bus).await;
            connection_fail_closed(&mut self.poisoned, result)
        }
        #[cfg(not(windows))]
        {
            let _ = bus;
            connection_fail_closed(&mut self.poisoned, Err(IpcError::Unsupported))
        }
    }

    /// Serve one request via the host-owned CommandBus executor.
    ///
    /// Fully reads and decodes the client request, awaits the executor reply,
    /// then encodes and writes the response. Never receives or calls CommandBus.
    pub async fn serve_request_on_executor(
        &mut self,
        requests: &HostRequestHandle,
    ) -> Result<(), IpcError> {
        connection_ensure_live(self.poisoned)?;
        #[cfg(windows)]
        {
            let result = windows_serve_request_on_executor(self, requests).await;
            connection_fail_closed(&mut self.poisoned, result)
        }
        #[cfg(not(windows))]
        {
            let _ = requests;
            connection_fail_closed(&mut self.poisoned, Err(IpcError::Unsupported))
        }
    }
}

#[cfg(windows)]
fn windows_bind(
    endpoint: String,
    expected_fingerprint: ProfileFingerprint,
    config: AcceptHelloConfig,
    first_pipe_instance: bool,
) -> Result<HelloListener, IpcError> {
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    let mut security = windows_security::PipeSecurity::current_user_and_system()?;
    let server = security.with_attributes(|attrs| unsafe {
        ServerOptions::new()
            .first_pipe_instance(first_pipe_instance)
            .reject_remote_clients(true)
            .pipe_mode(PipeMode::Byte)
            .create_with_security_attributes_raw(&endpoint, attrs)
    })?;
    drop(security);

    Ok(HelloListener {
        endpoint,
        expected_fingerprint,
        config,
        server,
    })
}

#[cfg(windows)]
async fn windows_accept(listener: HelloListener) -> Result<HostConnection, IpcError> {
    // Idle accept waits for a client indefinitely; host shutdown cancels this later.
    listener.server.connect().await.map_err(IpcError::Io)?;
    windows_finish_handshake(listener).await
}

#[cfg(windows)]
async fn windows_accept_with_successor(
    listener: HelloListener,
) -> Result<(Result<HostConnection, IpcError>, HelloListener), IpcError> {
    // Keep the connected instance alive while creating its successor. Even a
    // rejected or malformed Hello therefore cannot leave the pipe name vacant.
    let connected = listener.server.connect().await.map_err(IpcError::Io);
    let successor = windows_bind(
        listener.endpoint.clone(),
        listener.expected_fingerprint,
        listener.config.clone(),
        false,
    )?;
    let connection = match connected {
        Ok(()) => windows_finish_handshake(listener).await,
        Err(error) => Err(error),
    };
    Ok((connection, successor))
}

#[cfg(windows)]
async fn windows_finish_handshake(mut listener: HelloListener) -> Result<HostConnection, IpcError> {
    use tokio::io::AsyncWriteExt;

    let (hello_physical, hello_message) = handshake_codecs()?;

    let (client_id, negotiated, server_hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let payload = read_physical_frame(&mut listener.server, &hello_physical).await?;
        let hello = hello_message
            .decode::<ClientHello>(&payload)
            .map_err(IpcError::MessagePack)?;
        if hello.profile_fingerprint != listener.expected_fingerprint {
            return Err(IpcError::ProfileMismatch);
        }
        let negotiated = fence_capabilities_by_transport(
            hello
                .negotiate(listener.config.supported, listener.config.local_limits)
                .map_err(IpcError::ClientHello)?,
        );
        let server_hello = ServerHello::from_negotiated(
            listener.config.server_build.clone(),
            listener.config.host_boot_id,
            listener.expected_fingerprint,
            negotiated,
        )
        .map_err(IpcError::ServerHello)?;
        let encoded = hello_message
            .encode(&server_hello)
            .map_err(IpcError::MessagePack)?;
        write_physical_frame(&mut listener.server, &hello_physical, &encoded).await?;
        listener.server.flush().await.map_err(IpcError::Io)?;
        Ok((negotiated.client_id, negotiated, server_hello))
    })
    .await
    .map_err(|_| IpcError::Timeout)??;

    let (physical, message) = negotiated_codecs(negotiated.limits)?;
    Ok(HostConnection {
        client_id,
        negotiated,
        server_hello,
        physical,
        message,
        poisoned: false,
        pipe: listener.server,
    })
}

#[cfg(windows)]
async fn windows_serve_request(
    connection: &mut HostConnection,
    bus: &mut CommandBus,
) -> Result<(), IpcError> {
    use tokio::io::AsyncWriteExt;

    let first = read_first_request_byte_idle(&mut connection.pipe).await?;
    tokio::time::timeout(REQUEST_COMPLETION_TIMEOUT, async {
        let payload =
            read_physical_frame_after_first_byte(&mut connection.pipe, &connection.physical, first)
                .await?;
        let request = connection
            .message
            .decode::<ClientRequest>(&payload)
            .map_err(IpcError::MessagePack)?;
        let response = dispatch_authenticated_request(connection.client_id, bus, request)?;
        let encoded = connection
            .message
            .encode(&response)
            .map_err(IpcError::MessagePack)?;
        write_physical_frame(&mut connection.pipe, &connection.physical, &encoded).await?;
        connection.pipe.flush().await.map_err(IpcError::Io)?;
        Ok(())
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

#[cfg(windows)]
async fn windows_serve_request_on_executor(
    connection: &mut HostConnection,
    requests: &HostRequestHandle,
) -> Result<(), IpcError> {
    use tokio::io::AsyncWriteExt;

    let first = read_first_request_byte_idle(&mut connection.pipe).await?;
    tokio::time::timeout(REQUEST_COMPLETION_TIMEOUT, async {
        let payload =
            read_physical_frame_after_first_byte(&mut connection.pipe, &connection.physical, first)
                .await?;
        let request = connection
            .message
            .decode::<ClientRequest>(&payload)
            .map_err(IpcError::MessagePack)?;
        let response = requests.execute(connection.negotiated, request).await?;
        let encoded = connection
            .message
            .encode(&response)
            .map_err(IpcError::MessagePack)?;
        write_physical_frame(&mut connection.pipe, &connection.physical, &encoded).await?;
        connection.pipe.flush().await.map_err(IpcError::Io)?;
        Ok(())
    })
    .await
    .map_err(|_| IpcError::Timeout)?
}

fn connection_ensure_live(poisoned: bool) -> Result<(), IpcError> {
    if poisoned {
        Err(IpcError::ConnectionPoisoned)
    } else {
        Ok(())
    }
}

fn connection_fail_closed<T>(
    poisoned: &mut bool,
    result: Result<T, IpcError>,
) -> Result<T, IpcError> {
    if result.is_err() {
        *poisoned = true;
    }
    result
}

pub(crate) async fn read_physical_frame<R>(
    reader: &mut R,
    codec: &PhysicalFrameCodec,
) -> Result<Vec<u8>, IpcError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|error| IpcError::Frame(PhysicalFrameError::ReadHeader { kind: error.kind() }))?;
    read_physical_payload(reader, codec, header).await
}

/// Wait indefinitely for the first request byte (idle connections do not expire).
pub(crate) async fn read_first_request_byte_idle<R>(reader: &mut R) -> Result<u8, IpcError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let mut first = [0_u8; 1];
    reader
        .read_exact(&mut first)
        .await
        .map_err(|error| IpcError::Frame(PhysicalFrameError::ReadHeader { kind: error.kind() }))?;
    Ok(first[0])
}

/// After the first byte is observed, finish one physical frame under `completion`.
/// Production uses the same idle-first-byte + completion split with a longer body;
/// this helper exists so unit tests can inject a short deadline.
#[cfg(test)]
pub(crate) async fn read_physical_frame_idle_then_deadline<R>(
    reader: &mut R,
    codec: &PhysicalFrameCodec,
    completion: Duration,
) -> Result<Vec<u8>, IpcError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let first = read_first_request_byte_idle(reader).await?;
    tokio::time::timeout(
        completion,
        read_physical_frame_after_first_byte(reader, codec, first),
    )
    .await
    .map_err(|_| IpcError::Timeout)?
}

pub(crate) async fn read_physical_frame_after_first_byte<R>(
    reader: &mut R,
    codec: &PhysicalFrameCodec,
    first: u8,
) -> Result<Vec<u8>, IpcError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let mut rest = [0_u8; 3];
    reader
        .read_exact(&mut rest)
        .await
        .map_err(|error| IpcError::Frame(PhysicalFrameError::ReadHeader { kind: error.kind() }))?;
    let header = [first, rest[0], rest[1], rest[2]];
    read_physical_payload(reader, codec, header).await
}

async fn read_physical_payload<R>(
    reader: &mut R,
    codec: &PhysicalFrameCodec,
    header: [u8; 4],
) -> Result<Vec<u8>, IpcError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let declared = u32::from_be_bytes(header);
    let payload_len = codec
        .validated_payload_len(header)
        .map_err(IpcError::Frame)?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| IpcError::Frame(PhysicalFrameError::Allocation { declared }))?;
    payload.resize(payload_len, 0);
    reader.read_exact(&mut payload).await.map_err(|error| {
        IpcError::Frame(PhysicalFrameError::ReadPayload {
            declared,
            kind: error.kind(),
        })
    })?;
    Ok(payload)
}

pub(crate) async fn write_physical_frame<W>(
    writer: &mut W,
    codec: &PhysicalFrameCodec,
    payload: &[u8],
) -> Result<(), IpcError>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    let mut encoded = Vec::new();
    codec
        .write(&mut encoded, payload)
        .map_err(IpcError::Frame)?;
    writer.write_all(&encoded).await.map_err(|error| {
        if encoded.len() <= 4 {
            IpcError::Frame(PhysicalFrameError::WriteHeader { kind: error.kind() })
        } else {
            IpcError::Frame(PhysicalFrameError::WritePayload {
                declared: u32::try_from(payload.len()).unwrap_or(u32::MAX),
                kind: error.kind(),
            })
        }
    })?;
    Ok(())
}

pub(crate) fn handshake_timeout() -> Duration {
    HANDSHAKE_TIMEOUT
}

pub(crate) fn request_completion_timeout() -> Duration {
    REQUEST_COMPLETION_TIMEOUT
}

pub(crate) fn codecs_for_limits(
    limits: FrameLimits,
) -> Result<(PhysicalFrameCodec, MessagePackCodec), IpcError> {
    negotiated_codecs(limits)
}

#[cfg(windows)]
mod windows_security {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::core::{Owned, HSTRING, PWSTR};
    use windows::Win32::Foundation::{HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{protected_pipe_sddl, IpcError};

    /// Explicit SDDL-backed DACL: LocalSystem + current process user only.
    pub struct PipeSecurity {
        _token_info: Vec<usize>,
        descriptor: Owned<HLOCAL>,
    }

    impl PipeSecurity {
        pub fn current_user_and_system() -> Result<Self, IpcError> {
            unsafe { build_current_user_and_system() }
        }

        pub fn with_attributes<R>(
            &mut self,
            f: impl FnOnce(*mut c_void) -> std::io::Result<R>,
        ) -> Result<R, IpcError> {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: (*self.descriptor).0,
                bInheritHandle: false.into(),
            };
            f((&mut attrs as *mut SECURITY_ATTRIBUTES).cast()).map_err(IpcError::Io)
        }
    }

    unsafe fn build_current_user_and_system() -> Result<PipeSecurity, IpcError> {
        // GetCurrentProcess returns a borrowed pseudo-handle; never close it.
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|_| IpcError::Security("OpenProcessToken failed".into()))?;
        let token = Owned::new(token);

        let mut needed = 0_u32;
        let _ = GetTokenInformation(*token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(IpcError::Security(
                "GetTokenInformation returned empty TokenUser size".into(),
            ));
        }
        let words = (needed as usize).div_ceil(size_of::<usize>());
        let mut token_info = vec![0_usize; words];
        GetTokenInformation(
            *token,
            TokenUser,
            Some(token_info.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .map_err(|_| IpcError::Security("GetTokenInformation failed".into()))?;
        let token_user = &*(token_info.as_ptr() as *const TOKEN_USER);

        let mut sid_string = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_string)
            .map_err(|_| IpcError::Security("ConvertSidToStringSidW failed".into()))?;
        let sid_local = Owned::new(HLOCAL(sid_string.as_ptr().cast()));
        let sid_text = sid_string
            .to_string()
            .map_err(|_| IpcError::Security("SID string conversion failed".into()))?;
        let sddl = protected_pipe_sddl(&sid_text);
        drop(sid_local);
        drop(token);

        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl.as_str()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|_| {
            IpcError::Security("ConvertStringSecurityDescriptorToSecurityDescriptorW failed".into())
        })?;
        let descriptor = Owned::new(HLOCAL(descriptor.0));

        Ok(PipeSecurity {
            _token_info: token_info,
            descriptor,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;

    use super::{
        connection_ensure_live, connection_fail_closed, protected_pipe_sddl,
        read_physical_frame_idle_then_deadline, IpcError,
    };
    use crate::protocol::{FrameLimits, PhysicalFrameCodec};

    #[test]
    fn protected_pipe_sddl_is_exact_protected_two_ace_form() {
        assert_eq!(
            protected_pipe_sddl("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    fn fail_closed_poisons_and_blocks_reuse() {
        let mut poisoned = false;
        assert!(connection_ensure_live(poisoned).is_ok());
        assert!(matches!(
            connection_fail_closed(&mut poisoned, Err::<(), _>(IpcError::Unauthorized)),
            Err(IpcError::Unauthorized)
        ));
        assert!(poisoned);
        assert!(matches!(
            connection_ensure_live(poisoned),
            Err(IpcError::ConnectionPoisoned)
        ));
    }

    #[test]
    fn transport_fence_strips_paged_snapshots_and_event_replay_together() {
        use super::{fence_capabilities_by_transport, page_response_fits_transport};
        use crate::domain::ClientId;
        use crate::protocol::{
            Capability, CapabilitySet, FrameLimits, NegotiatedParameters, ProtocolVersion,
        };

        let defaults = FrameLimits::v1_default();
        assert!(
            page_response_fits_transport(defaults),
            "default frame limits must leave headroom for a one-frame page reply"
        );

        let client_id = ClientId::new();
        let granted = CapabilitySet::from_capabilities([
            Capability::PagedSnapshots,
            Capability::EventReplay,
            Capability::OperationSettlement,
        ]);
        let default_negotiated = fence_capabilities_by_transport(NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id,
            capabilities: granted,
            limits: defaults,
        });
        assert!(default_negotiated
            .capabilities
            .contains(Capability::PagedSnapshots));
        assert!(default_negotiated
            .capabilities
            .contains(Capability::EventReplay));
        assert!(default_negotiated
            .capabilities
            .contains(Capability::OperationSettlement));

        let tight = FrameLimits {
            max_physical_frame_bytes: 64 * 1024,
            max_reassembled_message_bytes: 16 * 1024 * 1024,
            max_page_items: 250,
            max_page_encoded_bytes: 512 * 1024,
        };
        assert!(!page_response_fits_transport(tight));
        let fenced = fence_capabilities_by_transport(NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id,
            capabilities: granted,
            limits: tight,
        });
        assert!(!fenced.capabilities.contains(Capability::PagedSnapshots));
        assert!(!fenced.capabilities.contains(Capability::EventReplay));
        assert!(fenced
            .capabilities
            .contains(Capability::OperationSettlement));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_before_first_byte_then_completion_deadline_on_partial_header() {
        let codec = PhysicalFrameCodec::from_limits(FrameLimits::v1_default()).expect("codec");
        let short = Duration::from_millis(40);
        let (mut writer, mut reader) = tokio::io::duplex(64);

        let read_task = tokio::spawn(async move {
            read_physical_frame_idle_then_deadline(&mut reader, &codec, short).await
        });

        tokio::time::sleep(short + Duration::from_millis(80)).await;
        assert!(
            !read_task.is_finished(),
            "zero incoming bytes must remain idle beyond the short completion duration"
        );

        writer.write_all(&[0x00]).await.expect("write first byte");
        writer.flush().await.expect("flush first byte");

        let result = tokio::time::timeout(Duration::from_secs(2), read_task)
            .await
            .expect("join timeout")
            .expect("task join");
        assert!(
            matches!(result, Err(IpcError::Timeout)),
            "partial header after first byte must fail under the injected completion deadline, got {result:?}"
        );
    }
}
