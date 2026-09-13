//! Linux local transport. Abstract addresses have no filesystem lifecycle;
//! authenticate the kernel peer credentials before exchanging any protocol data.

use super::IpcError;
use crate::protocol::ProfileFingerprint;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};

const PREFIX: &str = "unix-abstract:";

pub(crate) fn endpoint(fingerprint: ProfileFingerprint) -> String {
    // SAFETY: geteuid has no preconditions and owns no resource.
    format!(
        "{PREFIX}devmanager-{}-{}",
        unsafe { libc::geteuid() },
        fingerprint.to_hex()
    )
}

fn abstract_name(endpoint: &str) -> Result<&str, IpcError> {
    let name = endpoint
        .strip_prefix(PREFIX)
        .ok_or_else(|| IpcError::Security("expected a Linux local host endpoint".into()))?;
    let prefix = format!("devmanager-{}-", unsafe { libc::geteuid() });
    let digest = name
        .strip_prefix(&prefix)
        .ok_or_else(|| IpcError::Security("local host endpoint belongs to another user".into()))?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(IpcError::Security(
            "invalid local host endpoint fingerprint".into(),
        ));
    }
    Ok(name)
}

pub(crate) fn bind(endpoint: &str) -> Result<tokio::net::UnixListener, IpcError> {
    let address = SocketAddr::from_abstract_name(abstract_name(endpoint)?).map_err(IpcError::Io)?;
    let listener = UnixListener::bind_addr(&address).map_err(IpcError::Io)?;
    listener.set_nonblocking(true).map_err(IpcError::Io)?;
    tokio::net::UnixListener::from_std(listener).map_err(IpcError::Io)
}

pub(crate) fn authenticate_peer(stream: &tokio::net::UnixStream) -> Result<(), IpcError> {
    let credentials = stream.peer_cred().map_err(IpcError::Io)?;
    if credentials.uid() != unsafe { libc::geteuid() }
        || !credentials.pid().is_some_and(|pid| pid > 0)
    {
        return Err(IpcError::Security(
            "local host peer credentials do not match this user".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn connect(endpoint: &str) -> Result<tokio::net::UnixStream, IpcError> {
    let name = abstract_name(endpoint)?;
    // Tokio recognizes a leading NUL as a Linux abstract address, and polls
    // nonblocking connect readiness on its executor (including a full backlog).
    let address = format!("\0{name}");
    let stream = tokio::time::timeout(
        super::handshake_timeout(),
        tokio::net::UnixStream::connect(address),
    )
    .await
    .map_err(|_| IpcError::Timeout)?
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            IpcError::Unavailable
        }
        std::io::ErrorKind::WouldBlock => IpcError::Busy,
        _ => IpcError::Io(error),
    })?;
    authenticate_peer(&stream)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn linux_local_socket_is_exclusive_authenticated_and_rebinds_after_drop() {
        let name = endpoint(ProfileFingerprint::hash_normalized(
            &uuid::Uuid::now_v7().to_string(),
        ));
        assert!(matches!(connect(&name).await, Err(IpcError::Unavailable)));
        let listener = bind(&name).unwrap();
        assert!(bind(&name).is_err());
        let client = connect(&name).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        authenticate_peer(&server).unwrap();
        drop(client);
        drop(server);
        drop(listener);
        let listener = bind(&name).unwrap();
        drop(listener);
        assert!(abstract_name("unix-abstract:devmanager-foreign-1234").is_err());
        assert!(abstract_name("/tmp/host.sock").is_err());
    }
}
