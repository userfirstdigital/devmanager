//! Profile-scoped named-pipe ClientHello/ServerHello handshake transport.
//!
//! This module is the lean Phase 2 vertical slice: one local client, one
//! handshake document, then stop. Command routing and reconnect live later.

use std::time::Duration;

use uuid::Uuid;

use crate::config::paths::AppProfile;
use crate::domain::ClientId;
use crate::protocol::{
    CapabilitySet, ClientHello, ClientHelloError, FrameLimits, MessagePackCodec, MessagePackError,
    NegotiatedParameters, PhysicalFrameCodec, PhysicalFrameError, ProfileFingerprint,
    ServerBuildError, ServerHello, ServerHelloError,
};

const PIPE_PRODUCT_PREFIX: &str = r"\\.\pipe\devmanager-";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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
    Io(std::io::Error),
    Timeout,
    Frame(PhysicalFrameError),
    MessagePack(MessagePackError),
    ClientHello(ClientHelloError),
    ServerHello(ServerHelloError),
    ProfileMismatch,
    HelloInconsistent,
    Security(String),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(name) => write!(f, "invalid host ipc profile name: {name:?}"),
            Self::Unsupported => write!(f, "named-pipe ipc is unsupported on this platform"),
            Self::Io(error) => write!(f, "named-pipe ipc I/O error: {error}"),
            Self::Timeout => write!(f, "named-pipe handshake timed out"),
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
            | Self::Timeout
            | Self::ProfileMismatch
            | Self::HelloInconsistent
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
            windows_bind(endpoint, expected_fingerprint, config)
        }
        #[cfg(not(windows))]
        {
            let _ = endpoint;
            let _ = expected_fingerprint;
            let _ = config;
            Err(IpcError::Unsupported)
        }
    }

    pub async fn accept_hello(self) -> Result<AcceptedHello, IpcError> {
        #[cfg(windows)]
        {
            windows_accept_hello(self).await
        }
        #[cfg(not(windows))]
        {
            let _ = self;
            Err(IpcError::Unsupported)
        }
    }
}

#[cfg(windows)]
fn windows_bind(
    endpoint: String,
    expected_fingerprint: ProfileFingerprint,
    config: AcceptHelloConfig,
) -> Result<HelloListener, IpcError> {
    use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

    let mut security = windows_security::PipeSecurity::current_user_and_system()?;
    let server = security.with_attributes(|attrs| unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
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
async fn windows_accept_hello(mut listener: HelloListener) -> Result<AcceptedHello, IpcError> {
    use tokio::io::AsyncWriteExt;

    let (physical, message) = handshake_codecs()?;
    // Idle accept waits for a client indefinitely; host shutdown cancels this later.
    listener.server.connect().await.map_err(IpcError::Io)?;

    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let payload = read_physical_frame(&mut listener.server, &physical).await?;
        let hello = message
            .decode::<ClientHello>(&payload)
            .map_err(IpcError::MessagePack)?;
        if hello.profile_fingerprint != listener.expected_fingerprint {
            return Err(IpcError::ProfileMismatch);
        }
        let negotiated = hello
            .negotiate(listener.config.supported, listener.config.local_limits)
            .map_err(IpcError::ClientHello)?;
        let server_hello = ServerHello::from_negotiated(
            listener.config.server_build.clone(),
            listener.config.host_boot_id,
            listener.expected_fingerprint,
            negotiated,
        )
        .map_err(IpcError::ServerHello)?;
        let encoded = message
            .encode(&server_hello)
            .map_err(IpcError::MessagePack)?;
        write_physical_frame(&mut listener.server, &physical, &encoded).await?;
        listener.server.flush().await.map_err(IpcError::Io)?;
        Ok(AcceptedHello {
            client_id: negotiated.client_id,
            negotiated,
            server_hello,
        })
    })
    .await
    .map_err(|_| IpcError::Timeout)?
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
    use super::protected_pipe_sddl;

    #[test]
    fn protected_pipe_sddl_is_exact_protected_two_ace_form() {
        assert_eq!(
            protected_pipe_sddl("S-1-5-21-1-2-3-1001"),
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)"
        );
    }
}
