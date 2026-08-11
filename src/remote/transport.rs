use super::RemoteHostConfig;
use rcgen::generate_simple_self_signed;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error as RustlsError, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::collections::HashMap;
use std::fmt;
use std::io::{BufReader, Cursor, ErrorKind};
#[cfg(not(windows))]
use std::net::ToSocketAddrs;
#[cfg(windows)]
use std::net::{Ipv4Addr, Ipv6Addr};
use std::net::{SocketAddr, TcpStream};
#[cfg(windows)]
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub type ClientTlsStream = StreamOwned<ClientConnection, TcpStream>;
pub type ServerTlsStream = StreamOwned<ServerConnection, TcpStream>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_TLS_NAME: &str = "devmanager.remote";

fn tls_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
pub struct TlsConnectResult {
    pub stream: ClientTlsStream,
    pub certificate_fingerprint: String,
    pub handshake_deadline: Instant,
}

#[derive(Debug)]
pub struct TlsAcceptResult {
    pub stream: ServerTlsStream,
}

struct ResolverCancellation {
    cancelled: AtomicBool,
    #[cfg(windows)]
    native_handle: Mutex<Option<usize>>,
}

impl ResolverCancellation {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cancelled: AtomicBool::new(false),
            #[cfg(windows)]
            native_handle: Mutex::new(None),
        })
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(windows)]
        self.cancel_native_operation();
    }

    #[cfg(windows)]
    fn install_native_handle(&self, handle: *mut std::ffi::c_void) {
        let should_cancel = self.is_cancelled();
        let mut slot = self
            .native_handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if should_cancel {
            drop(slot);
            self.cancel_native_handle(handle);
        } else {
            *slot = Some(handle as usize);
        }
    }

    #[cfg(windows)]
    fn clear_native_handle(&self) {
        if let Ok(mut slot) = self.native_handle.lock() {
            *slot = None;
        }
    }

    #[cfg(windows)]
    fn cancel_native_operation(&self) {
        let handle = self
            .native_handle
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            self.cancel_native_handle(handle as *mut std::ffi::c_void);
        }
    }

    #[cfg(windows)]
    fn cancel_native_handle(&self, handle: *mut std::ffi::c_void) {
        if handle.is_null() {
            return;
        }
        // GetAddrInfoExCancel is explicitly safe to call from the deadline
        // owner while the resolver worker owns the OVERLAPPED operation.
        unsafe {
            let mut handle = handle;
            let _ = GetAddrInfoExCancel(&mut handle);
        }
    }
}

enum ResolverCommand {
    Resolve {
        id: u64,
        address: String,
        port: u16,
        result: mpsc::SyncSender<Result<Vec<SocketAddr>, String>>,
        cancelled: Arc<ResolverCancellation>,
        capacity: ResolverCapacityGuard,
    },
    Finished(u64),
}

struct ResolverPool {
    sender: mpsc::SyncSender<ResolverCommand>,
    active: Arc<AtomicUsize>,
    next_id: AtomicU64,
    _coordinator: thread::JoinHandle<()>,
}

const MAX_RESOLVER_OPERATIONS: usize = 4;

fn resolver_pool() -> &'static ResolverPool {
    static RESOLVER_POOL: OnceLock<ResolverPool> = OnceLock::new();
    RESOLVER_POOL.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<ResolverCommand>(MAX_RESOLVER_OPERATIONS);
        let active = Arc::new(AtomicUsize::new(0));
        let coordinator_sender = sender.clone();
        let coordinator = thread::Builder::new()
            .name("remote-dns-resolver-owner".to_string())
            .spawn(move || {
                let mut workers = std::collections::HashMap::<u64, thread::JoinHandle<()>>::new();
                while let Ok(command) = receiver.recv() {
                    match command {
                        ResolverCommand::Resolve {
                            id,
                            address,
                            port,
                            result,
                            cancelled,
                            capacity,
                        } => {
                            let completion_sender = coordinator_sender.clone();
                            let result_for_worker = result.clone();
                            let handle = thread::Builder::new()
                                .name(format!("remote-dns-{id}"))
                                .spawn(move || {
                                    let _capacity = capacity;
                                    if cancelled.is_cancelled() {
                                        let _ = result_for_worker
                                            .send(Err("DNS resolution cancelled".to_string()));
                                    } else {
                                        let resolved =
                                            resolve_native_addresses(&address, port, &cancelled);
                                        let _ = result_for_worker.send(resolved);
                                    }
                                    let _ = completion_sender.send(ResolverCommand::Finished(id));
                                });
                            if let Ok(handle) = handle {
                                workers.insert(id, handle);
                            } else {
                                let _ = coordinator_sender.send(ResolverCommand::Finished(id));
                                let _ = result
                                    .send(Err("DNS resolver worker could not start".to_string()));
                            }
                        }
                        ResolverCommand::Finished(id) => {
                            if let Some(handle) = workers.remove(&id) {
                                let _ = handle.join();
                            }
                        }
                    }
                }
                for (_, handle) in workers {
                    let _ = handle.join();
                }
            })
            .expect("remote DNS resolver owner should start");
        ResolverPool {
            sender,
            active,
            next_id: AtomicU64::new(1),
            _coordinator: coordinator,
        }
    })
}

struct ResolverCapacityGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ResolverCapacityGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/*
 * A DNS request is an owned operation, not a detached future. Capacity is
 * acquired before queueing and released only after the native resolver call
 * has returned and its worker has been joined by the owner thread. The
 * cancellation bit prevents queued work from starting. On Windows the native
 * operation is GetAddrInfoExW with an OVERLAPPED cancellation handle; other
 * platforms keep the native resolver call owned and charged until it returns.
 */
fn resolve_with_deadline(
    address: &str,
    port: u16,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(format!(
            "DNS resolution deadline expired for {address}:{port}"
        ));
    }
    let pool = resolver_pool();
    let mut active = pool.active.load(Ordering::Acquire);
    loop {
        if active >= MAX_RESOLVER_OPERATIONS {
            return Err(format!(
                "DNS resolver capacity is exhausted for {address}:{port}; active resolution remains owned until completion"
            ));
        }
        match pool.active.compare_exchange_weak(
            active,
            active + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(next) => active = next,
        }
    }
    let capacity = ResolverCapacityGuard {
        active: pool.active.clone(),
    };
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let cancelled = ResolverCancellation::new();
    let id = pool.next_id.fetch_add(1, Ordering::Relaxed);
    pool.sender
        .try_send(ResolverCommand::Resolve {
            id,
            address: address.to_string(),
            port,
            result: result_tx,
            cancelled: cancelled.clone(),
            capacity,
        })
        .map_err(|_| {
            format!(
                "DNS resolver capacity is exhausted for {address}:{port}; resolution remains owned by the bounded resolver pool"
            )
        })?;
    match result_rx.recv_timeout(remaining) {
        Ok(Ok(addresses)) if !addresses.is_empty() => Ok(addresses),
        Ok(Ok(_)) => Err(format!(
            "DNS resolution returned no addresses for {address}:{port}"
        )),
        Ok(Err(error)) => Err(format!("Could not resolve {address}:{port}: {error}")),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancelled.cancel();
            Err(format!(
                "DNS resolution exceeded its absolute deadline for {address}:{port}; the bounded resolver worker remains owned until native completion"
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("DNS resolver pool disconnected unexpectedly".to_string())
        }
    }
}

fn resolve_native_addresses(
    address: &str,
    port: u16,
    cancelled: &Arc<ResolverCancellation>,
) -> Result<Vec<SocketAddr>, String> {
    #[cfg(windows)]
    {
        return resolve_native_addresses_windows(address, port, cancelled);
    }

    #[cfg(not(windows))]
    {
        let _ = cancelled;
        (address, port)
            .to_socket_addrs()
            .map(|addresses| addresses.collect())
            .map_err(|error| error.to_string())
    }
}

#[cfg(windows)]
#[repr(C)]
struct WindowsAddrInfoExW {
    ai_flags: i32,
    ai_family: i32,
    ai_socktype: i32,
    ai_protocol: i32,
    ai_addrlen: usize,
    ai_canonname: *mut u16,
    ai_addr: *mut WindowsSockAddr,
    ai_blob: *mut std::ffi::c_void,
    ai_bloblen: usize,
    ai_provider: *mut std::ffi::c_void,
    ai_next: *mut WindowsAddrInfoExW,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsSockAddr {
    sa_family: u16,
    sa_data: [i8; 14],
}

#[cfg(windows)]
#[repr(C)]
struct WindowsSockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

#[cfg(windows)]
#[repr(C)]
struct WindowsSockAddrIn6 {
    sin6_family: u16,
    sin6_port: u16,
    sin6_flowinfo: u32,
    sin6_addr: [u8; 16],
    sin6_scope_id: u32,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    pointer: *mut std::ffi::c_void,
    h_event: *mut std::ffi::c_void,
}

#[cfg(windows)]
#[repr(C)]
struct WindowsWsaData {
    version: u16,
    high_version: u16,
    description: [i8; 257],
    system_status: [i8; 129],
    max_sockets: u16,
    max_udp_datagram: u16,
    vendor_info: *mut i8,
}

#[cfg(windows)]
struct WindowsResolverCompletion {
    sender: mpsc::SyncSender<u32>,
}

#[cfg(windows)]
static WINDOWS_RESOLVER_COMPLETIONS: OnceLock<
    Mutex<HashMap<usize, Arc<WindowsResolverCompletion>>>,
> = OnceLock::new();

#[cfg(windows)]
fn windows_resolver_completions() -> &'static Mutex<HashMap<usize, Arc<WindowsResolverCompletion>>>
{
    WINDOWS_RESOLVER_COMPLETIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(windows)]
const WINDOWS_AF_INET: u16 = 2;
#[cfg(windows)]
const WINDOWS_AF_INET6: u16 = 23;
#[cfg(windows)]
const WINDOWS_SOCK_STREAM: i32 = 1;
#[cfg(windows)]
const WINDOWS_IPPROTO_TCP: i32 = 6;
#[cfg(windows)]
const WINDOWS_NS_DEFAULT: u32 = 0;
#[cfg(windows)]
const WINDOWS_WSA_IO_PENDING: i32 = 997;
#[cfg(windows)]
const WINDOWS_WSA_OPERATION_ABORTED: u32 = 995;
#[cfg(windows)]
const WINDOWS_WINSOCK_VERSION_2_2: u16 = 0x0202;

#[cfg(windows)]
static WINDOWS_WSA_STARTUP: OnceLock<Result<(), String>> = OnceLock::new();

#[cfg(windows)]
#[link(name = "Ws2_32")]
unsafe extern "system" {
    fn WSAStartup(version: u16, data: *mut WindowsWsaData) -> i32;

    fn GetAddrInfoExW(
        pname: *const u16,
        pservicename: *const u16,
        dwnamespace: u32,
        lpnspid: *const std::ffi::c_void,
        hints: *const WindowsAddrInfoExW,
        ppresult: *mut *mut WindowsAddrInfoExW,
        timeout: *const std::ffi::c_void,
        lpoverlapped: *mut WindowsOverlapped,
        lpcompletionroutine: Option<unsafe extern "system" fn(u32, u32, *mut WindowsOverlapped)>,
        lphandle: *mut *mut std::ffi::c_void,
    ) -> i32;

    fn GetAddrInfoExCancel(lphandle: *const *mut std::ffi::c_void) -> i32;

    fn FreeAddrInfoExW(paddrinfoex: *const WindowsAddrInfoExW);
}

#[cfg(windows)]
fn ensure_windows_sockets_started() -> Result<(), String> {
    WINDOWS_WSA_STARTUP
        .get_or_init(|| {
            let mut data = unsafe { std::mem::zeroed::<WindowsWsaData>() };
            let status = unsafe { WSAStartup(WINDOWS_WINSOCK_VERSION_2_2, &mut data) };
            if status == 0 {
                Ok(())
            } else {
                Err(format!(
                    "Windows Winsock startup failed with status {status}"
                ))
            }
        })
        .clone()
}

#[cfg(windows)]
unsafe extern "system" fn windows_resolver_completion(
    error: u32,
    _bytes: u32,
    overlapped: *mut WindowsOverlapped,
) {
    if overlapped.is_null() {
        return;
    }
    let completion = windows_resolver_completions()
        .lock()
        .ok()
        .and_then(|completions| completions.get(&(overlapped as usize)).cloned());
    if let Some(completion) = completion {
        let _ = completion.sender.send(error);
    }
}

#[cfg(windows)]
fn resolve_native_addresses_windows(
    address: &str,
    port: u16,
    cancelled: &Arc<ResolverCancellation>,
) -> Result<Vec<SocketAddr>, String> {
    ensure_windows_sockets_started()?;
    let host = address
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let service = port.to_string();
    let service = service
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let hints = WindowsAddrInfoExW {
        ai_flags: 0,
        ai_family: 0,
        ai_socktype: WINDOWS_SOCK_STREAM,
        ai_protocol: WINDOWS_IPPROTO_TCP,
        ai_addrlen: 0,
        ai_canonname: ptr::null_mut(),
        ai_addr: ptr::null_mut(),
        ai_blob: ptr::null_mut(),
        ai_bloblen: 0,
        ai_provider: ptr::null_mut(),
        ai_next: ptr::null_mut(),
    };
    let (completion_tx, completion_rx) = mpsc::sync_channel(1);
    let completion = Arc::new(WindowsResolverCompletion {
        sender: completion_tx,
    });
    let mut overlapped = Box::new(WindowsOverlapped {
        internal: 0,
        internal_high: 0,
        pointer: ptr::null_mut(),
        h_event: ptr::null_mut(),
    });
    let overlapped_key = (&*overlapped as *const WindowsOverlapped) as usize;
    windows_resolver_completions()
        .lock()
        .map_err(|_| "Windows DNS resolver completion registry was poisoned".to_string())?
        .insert(overlapped_key, completion.clone());
    let mut results = ptr::null_mut();
    let mut native_handle = ptr::null_mut();
    let status = unsafe {
        GetAddrInfoExW(
            host.as_ptr(),
            service.as_ptr(),
            WINDOWS_NS_DEFAULT,
            ptr::null(),
            &hints,
            &mut results,
            ptr::null(),
            &mut *overlapped,
            Some(windows_resolver_completion),
            &mut native_handle,
        )
    };
    if status != 0 && status != WINDOWS_WSA_IO_PENDING {
        windows_resolver_completions()
            .lock()
            .ok()
            .and_then(|mut completions| completions.remove(&overlapped_key));
        return Err(format!("Windows DNS resolver failed with status {status}"));
    }
    cancelled.install_native_handle(native_handle);

    let completion_status = if status == 0 {
        // A numeric address commonly completes synchronously. In that case
        // Winsock has already populated `results` and does not schedule the
        // completion routine.
        windows_resolver_completions()
            .lock()
            .ok()
            .and_then(|mut completions| completions.remove(&overlapped_key));
        0
    } else {
        completion_rx.recv().map_err(|_| {
            "Windows DNS resolver completion channel disconnected before native completion"
                .to_string()
        })?
    };
    cancelled.clear_native_handle();
    windows_resolver_completions()
        .lock()
        .ok()
        .and_then(|mut completions| completions.remove(&overlapped_key));
    drop(completion);
    if completion_status != 0 {
        if completion_status == WINDOWS_WSA_OPERATION_ABORTED || cancelled.is_cancelled() {
            return Err("DNS resolution cancelled".to_string());
        }
        return Err(format!(
            "Windows DNS resolver completed with status {completion_status}"
        ));
    }

    let mut addresses = Vec::new();
    let mut current = results;
    while !current.is_null() {
        let result = unsafe { &*current };
        if !result.ai_addr.is_null() {
            let socket = unsafe { windows_socket_addr(result.ai_addr, result.ai_addrlen) };
            if let Some(socket) = socket {
                addresses.push(socket);
            }
        }
        current = result.ai_next;
    }
    if !results.is_null() {
        unsafe {
            FreeAddrInfoExW(results);
        }
    }
    Ok(addresses)
}

#[cfg(windows)]
unsafe fn windows_socket_addr(
    address: *const WindowsSockAddr,
    address_len: usize,
) -> Option<SocketAddr> {
    if address_len < std::mem::size_of::<u16>() {
        return None;
    }
    match (*address).sa_family {
        WINDOWS_AF_INET if address_len >= std::mem::size_of::<WindowsSockAddrIn>() => {
            let address = &*(address.cast::<WindowsSockAddrIn>());
            Some(SocketAddr::new(
                Ipv4Addr::from(u32::from_be(address.sin_addr)).into(),
                u16::from_be(address.sin_port),
            ))
        }
        WINDOWS_AF_INET6 if address_len >= std::mem::size_of::<WindowsSockAddrIn6>() => {
            let address = &*(address.cast::<WindowsSockAddrIn6>());
            Some(SocketAddr::new(
                Ipv6Addr::from(address.sin6_addr).into(),
                u16::from_be(address.sin6_port),
            ))
        }
        _ => None,
    }
}

pub fn ensure_host_tls_material(config: &mut RemoteHostConfig) -> Result<(), String> {
    if !config.certificate_pem.trim().is_empty() && !config.private_key_pem.trim().is_empty() {
        if validate_host_tls_material(config).is_ok() {
            if config.certificate_fingerprint.trim().is_empty() {
                config.certificate_fingerprint =
                    certificate_fingerprint_from_pem(&config.certificate_pem)?;
            }
            return Ok(());
        }
    }

    let mut subject_alt_names = vec![
        REMOTE_TLS_NAME.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let bind_address = config.bind_address.trim();
    if !bind_address.is_empty()
        && bind_address != "0.0.0.0"
        && bind_address != "::"
        && !subject_alt_names.iter().any(|value| value == bind_address)
    {
        subject_alt_names.push(bind_address.to_string());
    }

    let certified_key = generate_simple_self_signed(subject_alt_names)
        .map_err(|error| format!("Failed to generate remote TLS certificate: {error}"))?;
    config.certificate_fingerprint = certificate_fingerprint(certified_key.cert.der().as_ref());
    config.certificate_pem = certified_key.cert.pem();
    config.private_key_pem = certified_key.key_pair.serialize_pem();
    Ok(())
}

pub fn accept_tls(
    stream: TcpStream,
    config: &RemoteHostConfig,
    should_stop: impl FnMut() -> bool,
) -> Result<ServerTlsStream, String> {
    return accept_tls_with_deadline(
        stream,
        config,
        Instant::now() + HANDSHAKE_TIMEOUT,
        should_stop,
    )
    .map(|result| result.stream);
}

pub fn accept_tls_with_deadline(
    stream: TcpStream,
    config: &RemoteHostConfig,
    handshake_deadline: Instant,
    mut should_stop: impl FnMut() -> bool,
) -> Result<TlsAcceptResult, String> {
    let mut socket = stream;
    socket
        .set_nonblocking(false)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_nodelay(true)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    let mut connection = ServerConnection::new(server_config(config)?)
        .map_err(|error| format!("Remote TLS setup failed: {error}"))?;
    while connection.is_handshaking() {
        if should_stop() {
            return Err("Remote host stopped during the TLS handshake.".to_string());
        }
        let remaining = handshake_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Remote TLS handshake timed out.".to_string());
        }
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
        socket
            .set_write_timeout(Some(remaining.min(WRITE_TIMEOUT)))
            .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
        match connection.complete_io(&mut socket) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::TimedOut => {
                return Err("Remote TLS handshake timed out.".to_string())
            }
            Err(error) => return Err(format!("Remote TLS handshake failed: {error}")),
        }
    }
    socket
        .set_read_timeout(None)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_write_timeout(None)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    Ok(TlsAcceptResult {
        stream: StreamOwned::new(connection, socket),
    })
}

pub fn connect_tls_with_deadline(
    address: &str,
    port: u16,
    expected_fingerprint: Option<&str>,
    connect_deadline: Instant,
) -> Result<TlsConnectResult, String> {
    let addresses = resolve_with_deadline(address, port, connect_deadline)?;
    let mut socket = None;
    let mut last_error = None;
    for socket_address in addresses {
        let remaining = connect_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&socket_address, remaining) {
            Ok(connected) => {
                socket = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut socket = socket.ok_or_else(|| {
        format!(
            "Connect failed: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "connection attempt timed out".to_string())
        )
    })?;
    socket
        .set_nonblocking(false)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_nodelay(true)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    let remaining = connect_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Remote TLS handshake deadline expired before setup.".to_string());
    }
    socket
        .set_read_timeout(Some(remaining))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_write_timeout(Some(remaining.min(WRITE_TIMEOUT)))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;

    let verifier = Arc::new(PinnedFingerprintVerifier::new(expected_fingerprint));
    let config = ClientConfig::builder_with_provider(tls_crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("Remote TLS config failed: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let server_name = ServerName::try_from(REMOTE_TLS_NAME.to_string())
        .map_err(|_| "Invalid remote TLS server name.".to_string())?;
    let mut connection = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| format!("Remote TLS setup failed: {error}"))?;
    while connection.is_handshaking() {
        let remaining = connect_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Remote TLS handshake timed out.".to_string());
        }
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
        socket
            .set_write_timeout(Some(remaining.min(WRITE_TIMEOUT)))
            .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
        match connection.complete_io(&mut socket) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::TimedOut => {
                return Err("Remote TLS handshake timed out.".to_string())
            }
            Err(error) => return Err(format!("Remote TLS handshake failed: {error}")),
        }
    }
    let certificate_fingerprint = verifier
        .observed_fingerprint()
        .ok_or_else(|| "Remote TLS fingerprint was unavailable.".to_string())?;
    socket
        .set_read_timeout(None)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_write_timeout(None)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    Ok(TlsConnectResult {
        stream: StreamOwned::new(connection, socket),
        certificate_fingerprint,
        handshake_deadline: connect_deadline,
    })
}

pub fn connect_tls(
    address: &str,
    port: u16,
    expected_fingerprint: Option<&str>,
) -> Result<TlsConnectResult, String> {
    connect_tls_with_deadline(
        address,
        port,
        expected_fingerprint,
        Instant::now() + HANDSHAKE_TIMEOUT,
    )
}

pub fn certificate_fingerprint_from_pem(pem: &str) -> Result<String, String> {
    let cert_chain = parse_cert_chain(pem)?;
    let Some(first) = cert_chain.first() else {
        return Err("Remote TLS certificate chain is empty.".to_string());
    };
    Ok(certificate_fingerprint(first.as_ref()))
}

fn server_config(config: &RemoteHostConfig) -> Result<Arc<ServerConfig>, String> {
    let cert_chain = parse_cert_chain(&config.certificate_pem)?;
    let key_der = parse_private_key(&config.private_key_pem)?;
    let server_config = ServerConfig::builder_with_provider(tls_crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("Remote TLS config failed: {error}"))?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|error| format!("Remote TLS config failed: {error}"))?;
    Ok(Arc::new(server_config))
}

fn validate_host_tls_material(config: &RemoteHostConfig) -> Result<(), String> {
    let _ = certificate_fingerprint_from_pem(&config.certificate_pem)?;
    let _ = server_config(config)?;
    Ok(())
}

fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Remote TLS certificate parse failed: {error}"))?;
    if certs.is_empty() {
        return Err("Remote TLS certificate chain is empty.".to_string());
    }
    Ok(certs)
}

fn parse_private_key(pem: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>, String> {
    let mut reader = BufReader::new(Cursor::new(pem.as_bytes()));
    rustls_pemfile::private_key(&mut reader)
        .map_err(|error| format!("Remote TLS private key parse failed: {error}"))?
        .ok_or_else(|| "Remote TLS private key is missing.".to_string())
}

fn certificate_fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut fingerprint = String::with_capacity(digest.len() * 2);
    for byte in digest {
        fingerprint.push_str(&format!("{byte:02x}"));
    }
    fingerprint
}

#[derive(Clone)]
struct PinnedFingerprintVerifier {
    expected_fingerprint: Option<String>,
    observed_fingerprint: Arc<Mutex<Option<String>>>,
    crypto_provider: Arc<rustls::crypto::CryptoProvider>,
}

impl fmt::Debug for PinnedFingerprintVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedFingerprintVerifier")
            .field("expected_fingerprint", &self.expected_fingerprint)
            .finish_non_exhaustive()
    }
}

impl PinnedFingerprintVerifier {
    fn new(expected_fingerprint: Option<&str>) -> Self {
        Self {
            expected_fingerprint: expected_fingerprint
                .map(|value| value.trim().to_ascii_lowercase())
                .filter(|value| !value.is_empty()),
            observed_fingerprint: Arc::new(Mutex::new(None)),
            crypto_provider: tls_crypto_provider(),
        }
    }

    fn observed_fingerprint(&self) -> Option<String> {
        self.observed_fingerprint
            .lock()
            .ok()
            .and_then(|value| value.clone())
    }
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        let fingerprint = certificate_fingerprint(end_entity.as_ref());
        if let Some(expected) = self.expected_fingerprint.as_ref() {
            if expected != &fingerprint {
                return Err(RustlsError::General(
                    "Remote host certificate fingerprint changed.".to_string(),
                ));
            }
        }
        if let Ok(mut observed) = self.observed_fingerprint.lock() {
            *observed = Some(fingerprint);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.crypto_provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.crypto_provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.crypto_provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_resolver_completes_owned_localhost_lookup() {
        let deadline = Instant::now() + Duration::from_secs(3);
        let addresses = resolve_with_deadline("localhost", 443, deadline)
            .expect("bounded resolver should complete a local lookup");
        assert!(
            addresses.iter().any(|address| address.ip().is_loopback()),
            "localhost lookup should return a loopback address: {addresses:?}"
        );
    }

    #[test]
    fn ensure_host_tls_material_regenerates_invalid_persisted_values() {
        let mut config = RemoteHostConfig {
            certificate_pem: "invalid cert".to_string(),
            private_key_pem: "invalid key".to_string(),
            certificate_fingerprint: String::new(),
            ..RemoteHostConfig::default()
        };

        ensure_host_tls_material(&mut config).expect("tls material should regenerate");

        assert!(config.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert!(config.private_key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(!config.certificate_fingerprint.trim().is_empty());
        validate_host_tls_material(&config).expect("regenerated tls material should validate");
    }
}
