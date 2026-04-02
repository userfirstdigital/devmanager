use super::RemoteHostConfig;
use rcgen::generate_simple_self_signed;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error as RustlsError, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::{BufReader, Cursor, ErrorKind};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub type ClientTlsStream = StreamOwned<ClientConnection, TcpStream>;
pub type ServerTlsStream = StreamOwned<ServerConnection, TcpStream>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const ACTIVE_READ_TIMEOUT: Duration = Duration::from_millis(40);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_TLS_NAME: &str = "devmanager.remote";

fn tls_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
pub struct TlsConnectResult {
    pub stream: ClientTlsStream,
    pub certificate_fingerprint: String,
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

pub fn accept_tls(stream: TcpStream, config: &RemoteHostConfig) -> Result<ServerTlsStream, String> {
    let mut socket = stream;
    socket
        .set_nonblocking(false)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_nodelay(true)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;

    let mut connection = ServerConnection::new(server_config(config)?)
        .map_err(|error| format!("Remote TLS setup failed: {error}"))?;
    while connection.is_handshaking() {
        match connection.complete_io(&mut socket) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("Remote TLS handshake failed: {error}")),
        }
    }

    socket
        .set_read_timeout(Some(ACTIVE_READ_TIMEOUT))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    Ok(StreamOwned::new(connection, socket))
}

pub fn connect_tls(
    address: &str,
    port: u16,
    expected_fingerprint: Option<&str>,
) -> Result<TlsConnectResult, String> {
    let mut socket =
        TcpStream::connect((address, port)).map_err(|error| format!("Connect failed: {error}"))?;
    socket
        .set_nonblocking(false)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_nodelay(true)
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;
    socket
        .set_write_timeout(Some(WRITE_TIMEOUT))
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
        match connection.complete_io(&mut socket) {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(format!("Remote TLS handshake failed: {error}")),
        }
    }

    let certificate_fingerprint = verifier
        .observed_fingerprint()
        .ok_or_else(|| "Remote TLS fingerprint was unavailable.".to_string())?;
    socket
        .set_read_timeout(Some(ACTIVE_READ_TIMEOUT))
        .map_err(|error| format!("Failed to configure remote socket: {error}"))?;

    Ok(TlsConnectResult {
        stream: StreamOwned::new(connection, socket),
        certificate_fingerprint,
    })
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
