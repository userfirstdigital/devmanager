//! Minimal local ClientHello helper for the Phase 2 pipe handshake slice.
//!
//! This is intentionally not the reusable HostClient multiplexer planned for
//! Task 2.3.

use crate::domain::{MAX_SNAPSHOT_PAGE_ENCODED_BYTES, MAX_SNAPSHOT_PAGE_ITEMS};
use crate::host::{
    handshake_codecs, handshake_timeout, read_physical_frame, write_physical_frame, IpcError,
};
use crate::protocol::{
    ClientHello, FrameLimits, ServerHello, MAX_PHYSICAL_FRAME_BYTES, MAX_REASSEMBLED_MESSAGE_BYTES,
};

/// Connect to a host pipe endpoint, send `ClientHello`, and decode `ServerHello`.
pub async fn perform_client_hello(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ServerHello, IpcError> {
    #[cfg(windows)]
    {
        windows_perform_client_hello(endpoint, hello).await
    }
    #[cfg(not(windows))]
    {
        let _ = endpoint;
        let _ = hello;
        Err(IpcError::Unsupported)
    }
}

#[cfg(windows)]
async fn windows_perform_client_hello(
    endpoint: &str,
    hello: &ClientHello,
) -> Result<ServerHello, IpcError> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let (physical, message) = handshake_codecs()?;
    let encoded = message.encode(hello).map_err(IpcError::MessagePack)?;

    let mut client = ClientOptions::new().open(endpoint).map_err(IpcError::Io)?;

    let server_hello = tokio::time::timeout(handshake_timeout(), async {
        write_physical_frame(&mut client, &physical, &encoded).await?;
        client.flush().await.map_err(IpcError::Io)?;
        let payload = read_physical_frame(&mut client, &physical).await?;
        message
            .decode::<ServerHello>(&payload)
            .map_err(IpcError::MessagePack)
    })
    .await
    .map_err(|_| IpcError::Timeout)??;

    validate_server_hello(hello, &server_hello)?;
    Ok(server_hello)
}

fn validate_server_hello(sent: &ClientHello, received: &ServerHello) -> Result<(), IpcError> {
    if received.profile_fingerprint != sent.profile_fingerprint {
        return Err(IpcError::ProfileMismatch);
    }
    if received.protocol_major != sent.protocol_major {
        return Err(IpcError::HelloInconsistent);
    }
    if received.protocol_minor > sent.protocol_minor {
        return Err(IpcError::HelloInconsistent);
    }
    if received.granted.bits() & !sent.requested.bits() != 0 {
        return Err(IpcError::HelloInconsistent);
    }
    if !limits_within_offer_and_caps(received.limits, sent.limits) {
        return Err(IpcError::HelloInconsistent);
    }
    Ok(())
}

fn limits_within_offer_and_caps(got: FrameLimits, offer: FrameLimits) -> bool {
    got.validate_offer().is_ok()
        && got.max_physical_frame_bytes
            <= offer.max_physical_frame_bytes.min(MAX_PHYSICAL_FRAME_BYTES)
        && got.max_reassembled_message_bytes
            <= offer
                .max_reassembled_message_bytes
                .min(MAX_REASSEMBLED_MESSAGE_BYTES)
        && got.max_page_items <= offer.max_page_items.min(MAX_SNAPSHOT_PAGE_ITEMS)
        && got.max_page_encoded_bytes
            <= offer
                .max_page_encoded_bytes
                .min(MAX_SNAPSHOT_PAGE_ENCODED_BYTES)
}
