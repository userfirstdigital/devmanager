//! Transport-neutral Connect boundaries and bounded projection interfaces.
//!
//! Physical framing stays on [`FramedConnectTransport`]. Production callers
//! must attach a production-grade channel through
//! [`SealedFramedConnectTransport::production`]; source-level openers and
//! [`SealedFramedConnectTransport::new`] are crate-private test/harness paths.
//! Relay forwards already-sealed frames and never opens them.

use std::fmt;
use std::io::{ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

use crate::domain::command::CommandReceipt;
use crate::domain::id::{SnapshotId, TaskId, TransferId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::snapshot::{
    canonical_event_page_size, canonical_snapshot_page_size, EventPage, PageLimits, SnapshotPage,
    SnapshotSection,
};
use crate::protocol::{
    CapabilitySet, FrameLimitsError, PhysicalFrameCodec, PhysicalFrameError, SEALED_NONCE_BYTES,
};

use super::crypto::{ConnectCryptoError, ConnectSealedFrame, EndToEndChannel};
use super::envelope::{
    ChunkContext, ConnectEnvelope, ConnectLimitError, ConnectLimits, EnvelopeError,
};

/// Route metadata is deliberately separate from inner-envelope semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectRoute {
    Direct,
    Relay,
}

/// A transport implementation moves already-authenticated envelopes. It does
/// not receive network, crypto, identity, or persistence responsibilities from
/// this contract-first layer.
pub trait ConnectTransport {
    type Error;

    fn send(&mut self, envelope: ConnectEnvelope) -> Result<(), Self::Error>;
    fn receive(&mut self) -> Result<Option<ConnectEnvelope>, Self::Error>;
    fn close(&mut self) -> Result<(), Self::Error>;
}

pub fn encode_inner(envelope: &ConnectEnvelope) -> Result<Vec<u8>, EnvelopeError> {
    envelope.encode()
}

pub fn decode_inner(bytes: &[u8]) -> Result<ConnectEnvelope, EnvelopeError> {
    ConnectEnvelope::decode(bytes)
}

#[derive(Debug)]
pub enum ConnectTransportError {
    Closed,
    NegotiatedLimitsMismatch,
    Limits(ConnectLimitError),
    FrameLimits(FrameLimitsError),
    Frame(PhysicalFrameError),
    Envelope(EnvelopeError),
    Crypto(ConnectCryptoError),
    Flush { kind: ErrorKind },
}

impl fmt::Display for ConnectTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("Connect transport is closed"),
            Self::NegotiatedLimitsMismatch => {
                formatter.write_str("Connect envelope limits differ from the connection")
            }
            Self::Limits(error) => error.fmt(formatter),
            Self::FrameLimits(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Envelope(error) => error.fmt(formatter),
            Self::Crypto(error) => error.fmt(formatter),
            Self::Flush { kind } => write!(formatter, "Connect transport flush failed: {kind}"),
        }
    }
}

impl std::error::Error for ConnectTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Limits(error) => Some(error),
            Self::FrameLimits(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Envelope(error) => Some(error),
            Self::Crypto(error) => Some(error),
            Self::Closed | Self::NegotiatedLimitsMismatch | Self::Flush { .. } => None,
        }
    }
}

/// One byte-framed Connect connection. The negotiated limits are immutable
/// for the lifetime of this transport and are applied on both directions.
pub struct FramedConnectTransport<T> {
    io: T,
    negotiated: ConnectLimits,
    frame: PhysicalFrameCodec,
    closed: bool,
}

impl<T> FramedConnectTransport<T> {
    pub fn new(
        io: T,
        local: ConnectLimits,
        peer: ConnectLimits,
    ) -> Result<Self, ConnectTransportError> {
        let negotiated = local
            .negotiate(peer)
            .map_err(ConnectTransportError::Limits)?;
        Self::with_negotiated_limits(io, negotiated)
    }

    pub fn with_negotiated_limits(
        io: T,
        negotiated: ConnectLimits,
    ) -> Result<Self, ConnectTransportError> {
        negotiated
            .validate()
            .map_err(ConnectTransportError::Limits)?;
        let frame = PhysicalFrameCodec::from_limits(negotiated.frame_limits())
            .map_err(ConnectTransportError::FrameLimits)?;
        Ok(Self {
            io,
            negotiated,
            frame,
            closed: false,
        })
    }

    pub const fn negotiated_limits(&self) -> ConnectLimits {
        self.negotiated
    }

    pub fn validate_page(&self, items: usize, encoded_bytes: u64) -> Result<(), ConnectLimitError> {
        self.negotiated.validate_page(items, encoded_bytes)
    }

    pub fn validate_chunk(
        &self,
        cumulative_before: u64,
        chunk: &[u8],
    ) -> Result<u64, ConnectLimitError> {
        self.negotiated.validate_chunk(cumulative_before, chunk)
    }

    pub fn chunk_context(
        &self,
        transfer_id: TransferId,
        resume_cursor: Option<Vec<u8>>,
    ) -> Result<ChunkContext, ConnectLimitError> {
        self.negotiated.chunk_context(transfer_id, resume_cursor)
    }

    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: Read + Write> ConnectTransport for FramedConnectTransport<T> {
    type Error = ConnectTransportError;

    fn send(&mut self, envelope: ConnectEnvelope) -> Result<(), Self::Error> {
        if self.closed {
            return Err(ConnectTransportError::Closed);
        }
        if envelope.limits() != self.negotiated {
            self.closed = true;
            return Err(ConnectTransportError::NegotiatedLimitsMismatch);
        }
        let encoded = match envelope.encode() {
            Ok(encoded) => encoded,
            Err(error) => {
                self.closed = true;
                return Err(ConnectTransportError::Envelope(error));
            }
        };
        if let Err(error) = self.frame.write(&mut self.io, &encoded) {
            self.closed = true;
            return Err(ConnectTransportError::Frame(error));
        }
        if let Err(error) = self.io.flush() {
            self.closed = true;
            return Err(ConnectTransportError::Flush { kind: error.kind() });
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<ConnectEnvelope>, Self::Error> {
        if self.closed {
            return Err(ConnectTransportError::Closed);
        }
        let encoded = match self.frame.read(&mut self.io) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.closed = true;
                return Err(ConnectTransportError::Frame(error));
            }
        };
        match ConnectEnvelope::decode_with_limits(&encoded, self.negotiated) {
            Ok(envelope) => Ok(Some(envelope)),
            Err(error) => {
                self.closed = true;
                Err(ConnectTransportError::Envelope(error))
            }
        }
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        if !self.closed {
            self.closed = true;
            if let Err(error) = self.io.flush() {
                return Err(ConnectTransportError::Flush { kind: error.kind() });
            }
        }
        Ok(())
    }
}

/// Sealed Connect path: authenticated channel, then physical framing.
///
/// Direct and relay stay distinct via [`ConnectRoute`] on the channel; this
/// wrapper never opens relay-opaque frames for a mismatched route policy.
/// [`Self::production`] refuses non-production (source-level) channels.
pub struct SealedFramedConnectTransport<T> {
    framed: FramedConnectTransport<T>,
    channel: EndToEndChannel,
    route: ConnectRoute,
    closed: bool,
}

impl<T> fmt::Debug for SealedFramedConnectTransport<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedFramedConnectTransport")
            .field("route", &self.route)
            .field("closed", &self.closed)
            .finish()
    }
}

impl<T> SealedFramedConnectTransport<T> {
    pub(crate) fn new(
        io: T,
        local: ConnectLimits,
        peer: ConnectLimits,
        channel: EndToEndChannel,
    ) -> Result<Self, ConnectTransportError> {
        let route = channel.preferred_route();
        Ok(Self {
            framed: FramedConnectTransport::new(io, local, peer)?,
            channel,
            route,
            closed: false,
        })
    }

    /// Production constructor. Rejects source-level/test channels.
    pub fn production(
        io: T,
        local: ConnectLimits,
        peer: ConnectLimits,
        channel: EndToEndChannel,
    ) -> Result<Self, ConnectTransportError> {
        if !channel.is_production_grade() {
            return Err(ConnectTransportError::Closed);
        }
        Self::new(io, local, peer, channel)
    }

    pub const fn route(&self) -> ConnectRoute {
        self.route
    }

    pub const fn negotiated_limits(&self) -> ConnectLimits {
        self.framed.negotiated_limits()
    }

    pub fn into_parts(self) -> (T, EndToEndChannel)
    where
        T: Sized,
    {
        (self.framed.into_inner(), self.channel)
    }
}

impl<T: Read + Write> SealedFramedConnectTransport<T> {
    pub fn send_sealed(
        &mut self,
        envelope: &ConnectEnvelope,
        nonce: [u8; SEALED_NONCE_BYTES],
        now_unix: u64,
    ) -> Result<(), ConnectTransportError> {
        if self.closed {
            return Err(ConnectTransportError::Closed);
        }
        if self.channel.preferred_route() != self.route {
            self.closed = true;
            return Err(ConnectTransportError::Closed);
        }
        let frame = self
            .channel
            .seal(envelope, nonce, now_unix)
            .map_err(ConnectTransportError::Crypto)?;
        self.write_sealed_frame(&frame)
    }

    pub fn receive_sealed(
        &mut self,
        now_unix: u64,
    ) -> Result<Option<ConnectEnvelope>, ConnectTransportError> {
        if self.closed {
            return Err(ConnectTransportError::Closed);
        }
        let encoded = match self.framed.frame.read(&mut self.framed.io) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.closed = true;
                return Err(ConnectTransportError::Frame(error));
            }
        };
        let frame = ConnectSealedFrame::decode(&encoded).map_err(ConnectTransportError::Crypto)?;
        match self.channel.open(&frame, now_unix) {
            Ok(envelope) => Ok(Some(envelope)),
            Err(error) => {
                self.closed = true;
                Err(ConnectTransportError::Crypto(error))
            }
        }
    }

    /// Relay path: forward an already-sealed frame without opening it.
    pub fn forward_sealed_frame(
        &mut self,
        frame: &ConnectSealedFrame,
    ) -> Result<(), ConnectTransportError> {
        if self.closed {
            return Err(ConnectTransportError::Closed);
        }
        if self.route != ConnectRoute::Relay {
            self.closed = true;
            return Err(ConnectTransportError::Closed);
        }
        self.write_sealed_frame(frame)
    }

    fn write_sealed_frame(
        &mut self,
        frame: &ConnectSealedFrame,
    ) -> Result<(), ConnectTransportError> {
        let encoded = frame.encode().map_err(ConnectTransportError::Crypto)?;
        if let Err(error) = self.framed.frame.write(&mut self.framed.io, &encoded) {
            self.closed = true;
            return Err(ConnectTransportError::Frame(error));
        }
        if let Err(error) = self.framed.io.flush() {
            self.closed = true;
            return Err(ConnectTransportError::Flush { kind: error.kind() });
        }
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), ConnectTransportError> {
        self.closed = true;
        self.framed.close()
    }
}

pub const MAX_CONNECT_RESUME_CURSOR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRequest {
    pub task_id: Option<TaskId>,
    pub section: SnapshotSection,
    pub snapshot_id: Option<SnapshotId>,
    pub resume_cursor: Option<Vec<u8>>,
    pub limits: PageLimits,
}

impl SnapshotRequest {
    pub fn validate(&self) -> Result<(), ProjectionError> {
        self.limits
            .validate()
            .map_err(|_| ProjectionError::InvalidRequest)?;
        validate_cursor(self.resume_cursor.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRequest {
    pub task_id: Option<TaskId>,
    pub after_sequence: u64,
    pub resume_cursor: Option<Vec<u8>>,
    pub limits: PageLimits,
}

impl ReplayRequest {
    pub fn validate(&self) -> Result<(), ProjectionError> {
        self.limits
            .validate()
            .map_err(|_| ProjectionError::InvalidRequest)?;
        validate_cursor(self.resume_cursor.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    InvalidRequest,
    NotFound,
    Unauthorized,
    Unsupported,
    Bounds,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "invalid projection request",
            Self::NotFound => "projection item not found",
            Self::Unauthorized => "projection unauthorized",
            Self::Unsupported => "projection capability unsupported",
            Self::Bounds => "projection page exceeds negotiated bounds",
        })
    }
}

impl std::error::Error for ProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptExtensionDescriptor {
    pub schema_version: u16,
    pub capabilities: CapabilitySet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserExtensionDescriptor {
    pub schema_version: u16,
    pub capabilities: CapabilitySet,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionExtensions {
    pub prompt: Option<PromptExtensionDescriptor>,
    pub browser: Option<BrowserExtensionDescriptor>,
}

/// Read-side adapter for existing bounded domain projections.
pub trait ProjectionSource {
    fn snapshot_page(&self, request: SnapshotRequest) -> Result<SnapshotPage, ProjectionError>;
    fn event_page(&self, request: ReplayRequest) -> Result<EventPage, ProjectionError>;
    fn query(&self, request: QueryEnvelope) -> Result<QueryReply, ProjectionError>;

    fn extensions(&self) -> ProjectionExtensions {
        ProjectionExtensions::default()
    }
}

/// Existing receipt/page types remain the semantic response vocabulary; this
/// enum only gives transport adapters one non-owning envelope for dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionResponse {
    Snapshot(SnapshotPage),
    Events(EventPage),
    Query(QueryReply),
    Receipt(CommandReceipt),
}

pub fn validate_snapshot_page(
    page: &SnapshotPage,
    limits: PageLimits,
) -> Result<(), ProjectionError> {
    limits
        .validate()
        .map_err(|_| ProjectionError::InvalidRequest)?;
    if page.items.len() > usize::try_from(limits.max_items).unwrap_or(usize::MAX)
        || page.encoded_bytes > limits.max_encoded_bytes
    {
        return Err(ProjectionError::Bounds);
    }
    let encoded =
        canonical_snapshot_page_size(page).map_err(|_| ProjectionError::InvalidRequest)?;
    if page.encoded_bytes != encoded {
        return Err(ProjectionError::InvalidRequest);
    }
    if encoded > limits.max_encoded_bytes {
        return Err(ProjectionError::Bounds);
    }
    validate_cursor(page.next_cursor.as_deref())?;
    Ok(())
}

pub fn validate_event_page(page: &EventPage, limits: PageLimits) -> Result<(), ProjectionError> {
    limits
        .validate()
        .map_err(|_| ProjectionError::InvalidRequest)?;
    if page.events.len() > usize::try_from(limits.max_items).unwrap_or(usize::MAX) {
        return Err(ProjectionError::Bounds);
    }
    validate_cursor(page.next_cursor.as_deref())?;
    let encoded = canonical_event_page_size(page).map_err(|_| ProjectionError::InvalidRequest)?;
    if encoded > limits.max_encoded_bytes {
        return Err(ProjectionError::Bounds);
    }
    Ok(())
}

fn validate_cursor(cursor: Option<&[u8]>) -> Result<(), ProjectionError> {
    if cursor.is_some_and(|cursor| cursor.len() > MAX_CONNECT_RESUME_CURSOR_BYTES) {
        return Err(ProjectionError::Bounds);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::crypto::{connect_prologue, EndToEndChannel};
    use crate::protocol::{ChannelKey, ChannelRole, CredentialPurpose};
    use std::io::Cursor;

    #[test]
    fn sealed_transport_keeps_direct_and_relay_routes_distinct() {
        let secret = ChannelKey::from_bytes([9; 32]);
        let prologue =
            connect_prologue(CredentialPurpose::OwnerPairing, [3; 16], [4; 16]).expect("prologue");
        let limits = ConnectLimits::v1_default();
        let direct_channel = EndToEndChannel::open_source_level(
            secret.clone(),
            prologue,
            ChannelRole::Initiator,
            true,
            1,
            false,
        )
        .expect("direct channel");
        let relay_channel = EndToEndChannel::open_source_level(
            secret,
            prologue,
            ChannelRole::Responder,
            false,
            1,
            false,
        )
        .expect("relay channel");

        let direct = SealedFramedConnectTransport::new(
            Cursor::new(Vec::<u8>::new()),
            limits,
            limits,
            direct_channel,
        )
        .expect("direct transport");
        let relay = SealedFramedConnectTransport::new(
            Cursor::new(Vec::<u8>::new()),
            limits,
            limits,
            relay_channel,
        )
        .expect("relay transport");
        assert_eq!(direct.route(), ConnectRoute::Direct);
        assert_eq!(relay.route(), ConnectRoute::Relay);
        assert_ne!(direct.route(), relay.route());
    }

    #[test]
    fn production_sealed_transport_rejects_source_level_channels() {
        let secret = ChannelKey::from_bytes([9; 32]);
        let prologue =
            connect_prologue(CredentialPurpose::OwnerPairing, [3; 16], [4; 16]).expect("prologue");
        let limits = ConnectLimits::v1_default();
        let channel = EndToEndChannel::open_source_level(
            secret,
            prologue,
            ChannelRole::Initiator,
            true,
            1,
            false,
        )
        .expect("source-level");
        assert!(!channel.is_production_grade());
        let err = SealedFramedConnectTransport::production(
            Cursor::new(Vec::<u8>::new()),
            limits,
            limits,
            channel,
        )
        .expect_err("source-level is not production");
        assert!(matches!(err, ConnectTransportError::Closed));
    }
}
