//! Transport-neutral Connect boundaries and bounded projection interfaces.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::command::CommandReceipt;
use crate::domain::id::{SnapshotId, TaskId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::snapshot::{
    canonical_event_page_size, canonical_snapshot_page_size, EventPage, PageLimits, SnapshotPage,
    SnapshotSection,
};
use crate::protocol::CapabilitySet;

use super::envelope::{ConnectEnvelope, EnvelopeError};

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
