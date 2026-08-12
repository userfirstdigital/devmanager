//! On-demand artifact content paging: HMAC resume cursors and a small session registry.
//!
//! Snapshot pages carry metadata only (`ArtifactSummary`). Clients open an
//! `ArtifactContentSession` to page InlineUtf8 body bytes under negotiated limits.
//!
//! Host QueryError mapping (for connection wiring):
//! - InvalidCursor / CursorContextMismatch / changed-limits InvalidRequest /
//!   BodyTooLarge / ContentDigestMismatch → QueryError::InvalidRequest
//! - NotFound → QueryError::NotFound
//! - Unauthorized → QueryError::Unauthorized
//! - store / entropy / page-too-large on open → transport Unavailable

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::domain::artifact::{
    verify_inline_content_digest, ArtifactContentRef, ArtifactValidationError,
};
use crate::domain::id::{ArtifactId, ClientId, RequestId, SubscriptionId, TaskId};
use crate::domain::snapshot::{ArtifactContentPage, PageLimits, PageLimitsError};
use crate::kernel::command_bus;
use crate::kernel::store::{KernelStore, StoreError};
use crate::kernel::SessionScope;

const ARTIFACT_CONTENT_CURSOR_VERSION: u16 = 1;
const ARTIFACT_CONTENT_CURSOR_DOMAIN: &[u8] = b"devmanager:artifact-content-cursor:v1\0";
const CURSOR_TAG_BYTES: usize = 32;
const MAX_CURSOR_BYTES: usize = 4_096;
const PAGE_RESPONSE_ENVELOPE_HEADROOM: u32 = 1024;
const MAX_ARTIFACT_CONTENT_SESSIONS: usize = 32;
const ARTIFACT_CONTENT_IDLE_TTL: Duration = Duration::from_secs(30);

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactContentError {
    InvalidLimits(PageLimitsError),
    EntropyUnavailable,
    NotFound,
    Unauthorized,
    InvalidRequest,
    InvalidCursor,
    CursorContextMismatch,
    ContentDigestMismatch,
    BodyTooLarge {
        body_bytes: u64,
        max_reassembled_message_bytes: u32,
    },
    PageEnvelopeTooLarge {
        encoded_bytes: u32,
        max_encoded_bytes: u32,
    },
    Store(StoreError),
}

impl fmt::Display for ArtifactContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(error) => error.fmt(f),
            Self::EntropyUnavailable => {
                write!(f, "artifact content cursor entropy unavailable")
            }
            Self::NotFound => write!(f, "artifact content not found"),
            Self::Unauthorized => write!(f, "artifact content unauthorized"),
            Self::InvalidRequest => write!(f, "invalid artifact content request"),
            Self::InvalidCursor => write!(f, "invalid artifact content cursor"),
            Self::CursorContextMismatch => {
                write!(f, "artifact content cursor context mismatch")
            }
            Self::ContentDigestMismatch => {
                write!(f, "artifact inline content SHA-256 does not match declared digest")
            }
            Self::BodyTooLarge {
                body_bytes,
                max_reassembled_message_bytes,
            } => write!(
                f,
                "artifact body is {body_bytes} bytes, exceeding reassembled limit {max_reassembled_message_bytes}"
            ),
            Self::PageEnvelopeTooLarge {
                encoded_bytes,
                max_encoded_bytes,
            } => write!(
                f,
                "artifact content page envelope is {encoded_bytes} bytes, exceeding {max_encoded_bytes}"
            ),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ArtifactContentError {}

impl From<StoreError> for ArtifactContentError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<PageLimitsError> for ArtifactContentError {
    fn from(error: PageLimitsError) -> Self {
        Self::InvalidLimits(error)
    }
}

impl From<rusqlite::Error> for ArtifactContentError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactContentCursorDocument {
    version: u16,
    subscription_id: SubscriptionId,
    request_id: RequestId,
    scope: SessionScope,
    task_id: TaskId,
    artifact_id: ArtifactId,
    next_offset: u64,
    total_bytes: u64,
    sha256: [u8; 32],
    limits: PageLimits,
}

/// One in-memory InlineUtf8 content view bound to negotiated page and transport limits.
pub(crate) struct ArtifactContentSession {
    client_id: ClientId,
    task_id: TaskId,
    request_id: RequestId,
    scope: SessionScope,
    artifact_id: ArtifactId,
    sha256: [u8; 32],
    content: Vec<u8>,
    limits: PageLimits,
    max_reassembled_message_bytes: u32,
    max_physical_frame_bytes: u32,
    subscription_id: SubscriptionId,
    cursor_hmac_key: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for ArtifactContentSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArtifactContentSession")
            .field("client_id", &self.client_id)
            .field("task_id", &self.task_id)
            .field("artifact_id", &self.artifact_id)
            .field("subscription_id", &self.subscription_id)
            .field("limits", &self.limits)
            .field(
                "max_reassembled_message_bytes",
                &self.max_reassembled_message_bytes,
            )
            .field("max_physical_frame_bytes", &self.max_physical_frame_bytes)
            .field("total_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

impl ArtifactContentSession {
    pub(crate) fn subscription_id(&self) -> SubscriptionId {
        self.subscription_id
    }

    pub(crate) fn scope(&self) -> SessionScope {
        self.scope
    }

    #[cfg(test)]
    fn test_stub(
        client_id: ClientId,
        task_id: TaskId,
        artifact_id: ArtifactId,
        subscription_id: SubscriptionId,
        limits: PageLimits,
    ) -> Self {
        Self::test_stub_scoped(
            SessionScope {
                client_id: Some(client_id),
                task_id: Some(task_id),
                connection_id: None,
                action_epoch: None,
                runtime_generation: None,
            },
            RequestId::new(),
            artifact_id,
            subscription_id,
            limits,
        )
    }

    #[cfg(test)]
    fn test_stub_scoped(
        scope: SessionScope,
        request_id: RequestId,
        artifact_id: ArtifactId,
        subscription_id: SubscriptionId,
        limits: PageLimits,
    ) -> Self {
        let client_id = scope.client_id.expect("stub client scope");
        let task_id = scope.task_id.expect("stub task scope");
        Self {
            client_id,
            task_id,
            request_id,
            scope,
            artifact_id,
            sha256: [0u8; 32],
            content: vec![b'x'; 2_000],
            limits,
            max_reassembled_message_bytes: 16 * 1024 * 1024,
            max_physical_frame_bytes: 1024 * 1024,
            subscription_id,
            cursor_hmac_key: Zeroizing::new([0u8; 32]),
        }
    }

    /// Read one bounded content page. Cursor failures leave this session intact.
    pub(crate) fn page(
        &self,
        resume_cursor: Option<&[u8]>,
    ) -> Result<ArtifactContentPage, ArtifactContentError> {
        let offset = match resume_cursor {
            Some(cursor) => self.decode_cursor(cursor)?.next_offset,
            None => 0,
        };
        self.assemble_page(offset)
    }

    fn assemble_page(&self, offset: u64) -> Result<ArtifactContentPage, ArtifactContentError> {
        let total_bytes = u64::try_from(self.content.len()).map_err(|_| StoreError::Corruption)?;
        if offset > total_bytes {
            return Err(ArtifactContentError::InvalidCursor);
        }
        let remaining =
            usize::try_from(total_bytes - offset).map_err(|_| StoreError::Corruption)?;
        if remaining == 0 {
            let encoded_bytes = self.canonical_page_encoded_bytes(offset, &[], &None)?;
            self.ensure_page_fits(encoded_bytes)?;
            return Ok(ArtifactContentPage {
                artifact_id: self.artifact_id,
                offset,
                total_bytes,
                sha256: self.sha256,
                payload: Vec::new(),
                encoded_bytes,
                next_cursor: None,
            });
        }

        let start = usize::try_from(offset).map_err(|_| StoreError::Corruption)?;
        // Prefer the entire remaining cursorless page when it fits. Partial-page
        // binary search must not assume encoded-size ordering across HMAC cursors.
        let remaining_payload = &self.content[start..];
        let full_encoded = self.canonical_page_encoded_bytes(offset, remaining_payload, &None)?;
        if self.page_fits(full_encoded) {
            return Ok(ArtifactContentPage {
                artifact_id: self.artifact_id,
                offset,
                total_bytes,
                sha256: self.sha256,
                payload: remaining_payload.to_vec(),
                encoded_bytes: full_encoded,
                next_cursor: None,
            });
        }

        // Partial pages always carry a resume cursor and leave at least one byte.
        if remaining == 1 {
            return Err(ArtifactContentError::PageEnvelopeTooLarge {
                encoded_bytes: full_encoded,
                max_encoded_bytes: self.limits.max_encoded_bytes,
            });
        }

        let mut lo = 1usize;
        let mut hi = remaining - 1;
        let mut best: Option<(usize, Option<Vec<u8>>, u32)> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let end = offset
                .checked_add(u64::try_from(mid).map_err(|_| StoreError::Corruption)?)
                .ok_or(StoreError::Corruption)?;
            let next_cursor = Some(self.encode_cursor(end)?);
            let payload = &self.content[start..start + mid];
            let encoded_bytes = self.canonical_page_encoded_bytes(offset, payload, &next_cursor)?;
            if self.page_fits(encoded_bytes) {
                best = Some((mid, next_cursor, encoded_bytes));
                lo = mid.saturating_add(1);
            } else if mid == 1 {
                return Err(ArtifactContentError::PageEnvelopeTooLarge {
                    encoded_bytes,
                    max_encoded_bytes: self.limits.max_encoded_bytes,
                });
            } else {
                hi = mid - 1;
            }
        }

        let (len, next_cursor, encoded_bytes) =
            best.ok_or(ArtifactContentError::PageEnvelopeTooLarge {
                encoded_bytes: 0,
                max_encoded_bytes: self.limits.max_encoded_bytes,
            })?;
        Ok(ArtifactContentPage {
            artifact_id: self.artifact_id,
            offset,
            total_bytes,
            sha256: self.sha256,
            payload: self.content[start..start + len].to_vec(),
            encoded_bytes,
            next_cursor,
        })
    }

    fn page_fits(&self, encoded_bytes: u32) -> bool {
        if encoded_bytes > self.limits.max_encoded_bytes {
            return false;
        }
        let Some(with_headroom) = encoded_bytes.checked_add(PAGE_RESPONSE_ENVELOPE_HEADROOM) else {
            return false;
        };
        with_headroom <= self.max_reassembled_message_bytes
            && with_headroom <= self.max_physical_frame_bytes
    }

    fn ensure_page_fits(&self, encoded_bytes: u32) -> Result<(), ArtifactContentError> {
        if self.page_fits(encoded_bytes) {
            Ok(())
        } else {
            Err(ArtifactContentError::PageEnvelopeTooLarge {
                encoded_bytes,
                max_encoded_bytes: self.limits.max_encoded_bytes,
            })
        }
    }

    fn canonical_page_encoded_bytes(
        &self,
        offset: u64,
        payload: &[u8],
        next_cursor: &Option<Vec<u8>>,
    ) -> Result<u32, ArtifactContentError> {
        let total_bytes = u64::try_from(self.content.len()).map_err(|_| StoreError::Corruption)?;
        let mut encoded_bytes = 0u32;
        for _ in 0..8 {
            let page = ArtifactContentPage {
                artifact_id: self.artifact_id,
                offset,
                total_bytes,
                sha256: self.sha256,
                payload: payload.to_vec(),
                encoded_bytes,
                next_cursor: next_cursor.clone(),
            };
            let bytes =
                rmp_serde::to_vec_named(&page).map_err(|error| StoreError::CodecMismatch {
                    detail: format!("encode artifact content page: {error}"),
                })?;
            let actual = u32::try_from(bytes.len()).map_err(|_| StoreError::IntegerOutOfRange {
                field: "artifact_content_page.encoded_bytes",
                value: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            })?;
            if actual == encoded_bytes {
                return Ok(actual);
            }
            encoded_bytes = actual;
        }
        Err(StoreError::CodecMismatch {
            detail: "artifact content page encoded length did not converge".into(),
        }
        .into())
    }

    fn encode_cursor(&self, next_offset: u64) -> Result<Vec<u8>, ArtifactContentError> {
        let total_bytes = u64::try_from(self.content.len()).map_err(|_| StoreError::Corruption)?;
        if next_offset == 0 || next_offset > total_bytes {
            return Err(ArtifactContentError::InvalidCursor);
        }
        let document = ArtifactContentCursorDocument {
            version: ARTIFACT_CONTENT_CURSOR_VERSION,
            subscription_id: self.subscription_id,
            request_id: self.request_id,
            scope: self.scope,
            task_id: self.task_id,
            artifact_id: self.artifact_id,
            next_offset,
            total_bytes,
            sha256: self.sha256,
            limits: self.limits,
        };
        let payload =
            rmp_serde::to_vec_named(&document).map_err(|error| StoreError::CodecMismatch {
                detail: format!("encode artifact content cursor: {error}"),
            })?;
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| ArtifactContentError::InvalidCursor)?;
        mac.update(ARTIFACT_CONTENT_CURSOR_DOMAIN);
        mac.update(&payload);
        let tag = mac.finalize().into_bytes();
        let mut cursor = Vec::with_capacity(payload.len() + tag.len());
        cursor.extend_from_slice(&payload);
        cursor.extend_from_slice(&tag);
        Ok(cursor)
    }

    fn decode_cursor(
        &self,
        cursor: &[u8],
    ) -> Result<ArtifactContentCursorDocument, ArtifactContentError> {
        if cursor.len() <= CURSOR_TAG_BYTES || cursor.len() > MAX_CURSOR_BYTES {
            return Err(ArtifactContentError::InvalidCursor);
        }
        let (payload, tag) = cursor.split_at(cursor.len() - CURSOR_TAG_BYTES);
        let mut mac = HmacSha256::new_from_slice(self.cursor_hmac_key.as_ref())
            .map_err(|_| ArtifactContentError::InvalidCursor)?;
        mac.update(ARTIFACT_CONTENT_CURSOR_DOMAIN);
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| ArtifactContentError::InvalidCursor)?;

        let document: ArtifactContentCursorDocument =
            rmp_serde::from_slice(payload).map_err(|_| ArtifactContentError::InvalidCursor)?;
        let canonical =
            rmp_serde::to_vec_named(&document).map_err(|_| ArtifactContentError::InvalidCursor)?;
        if canonical.as_slice() != payload || document.version != ARTIFACT_CONTENT_CURSOR_VERSION {
            return Err(ArtifactContentError::InvalidCursor);
        }
        document.limits.validate()?;
        let total_bytes = u64::try_from(self.content.len()).map_err(|_| StoreError::Corruption)?;
        if document.subscription_id != self.subscription_id
            || document.request_id != self.request_id
            || document.scope != self.scope
            || document.task_id != self.task_id
            || document.artifact_id != self.artifact_id
            || document.total_bytes != total_bytes
            || document.sha256 != self.sha256
            || document.limits != self.limits
        {
            return Err(ArtifactContentError::CursorContextMismatch);
        }
        if document.next_offset == 0 || document.next_offset > document.total_bytes {
            return Err(ArtifactContentError::InvalidCursor);
        }
        Ok(document)
    }
}

impl KernelStore {
    /// Load InlineUtf8 artifact bytes into a paged content session.
    pub(crate) fn begin_artifact_content(
        &self,
        client_id: ClientId,
        task_id: TaskId,
        artifact_id: ArtifactId,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
    ) -> Result<ArtifactContentSession, ArtifactContentError> {
        self.begin_artifact_content_scoped(
            SessionScope {
                client_id: Some(client_id),
                task_id: Some(task_id),
                connection_id: None,
                action_epoch: None,
                runtime_generation: None,
            },
            RequestId::new(),
            artifact_id,
            limits,
            max_reassembled_message_bytes,
            max_physical_frame_bytes,
        )
    }

    pub(crate) fn begin_artifact_content_scoped(
        &self,
        scope: SessionScope,
        request_id: RequestId,
        artifact_id: ArtifactId,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
    ) -> Result<ArtifactContentSession, ArtifactContentError> {
        limits.validate()?;
        if max_reassembled_message_bytes == 0 || max_physical_frame_bytes == 0 {
            return Err(ArtifactContentError::InvalidRequest);
        }
        let client_id = scope
            .client_id
            .ok_or(ArtifactContentError::InvalidRequest)?;
        let task_id = scope.task_id.ok_or(ArtifactContentError::InvalidRequest)?;

        let conn = self.open_query_connection()?;
        let artifact = command_bus::load_artifact(&conn, artifact_id)?
            .ok_or(ArtifactContentError::NotFound)?;
        drop(conn);

        if artifact.task_id != task_id {
            return Err(ArtifactContentError::Unauthorized);
        }

        match verify_inline_content_digest(&artifact) {
            Ok(()) => {}
            Err(ArtifactValidationError::ContentDigestMismatch) => {
                return Err(ArtifactContentError::ContentDigestMismatch);
            }
            Err(_) => return Err(ArtifactContentError::InvalidRequest),
        }

        let content = match &artifact.content_ref {
            ArtifactContentRef::InlineUtf8(body) => {
                if body.is_empty() {
                    return Err(ArtifactContentError::InvalidRequest);
                }
                body.as_bytes().to_vec()
            }
            ArtifactContentRef::ContentAddressed { .. } => {
                // Never surface the digest; treated as unavailable content.
                return Err(ArtifactContentError::NotFound);
            }
        };

        let body_bytes = u64::try_from(content.len()).map_err(|_| StoreError::Corruption)?;
        if body_bytes > u64::from(max_reassembled_message_bytes) {
            return Err(ArtifactContentError::BodyTooLarge {
                body_bytes,
                max_reassembled_message_bytes,
            });
        }

        let mut cursor_hmac_key = Zeroizing::new([0u8; 32]);
        getrandom::fill(cursor_hmac_key.as_mut())
            .map_err(|_| ArtifactContentError::EntropyUnavailable)?;

        Ok(ArtifactContentSession {
            client_id,
            task_id,
            request_id,
            scope,
            artifact_id: artifact.id,
            sha256: artifact.sha256,
            content,
            limits,
            max_reassembled_message_bytes,
            max_physical_frame_bytes,
            subscription_id: SubscriptionId::new(),
            cursor_hmac_key,
        })
    }
}

struct ArtifactContentRegistryEntry {
    session: ArtifactContentSession,
    /// Current host authorization scope.  The session's immutable cursor
    /// scope intentionally remains the issuance scope across an authenticated
    /// reconnect; this field is what gates the new physical connection.
    scope: SessionScope,
    last_touch: Instant,
}

/// Bounded LRU registry for open artifact content sessions (host + tests).
pub(crate) struct ArtifactContentRegistry {
    entries: HashMap<SubscriptionId, ArtifactContentRegistryEntry>,
}

impl ArtifactContentRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, session: ArtifactContentSession, now: Instant) {
        while self.entries.len() >= MAX_ARTIFACT_CONTENT_SESSIONS {
            let Some((&victim_id, _)) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_touch)
            else {
                break;
            };
            self.entries.remove(&victim_id);
        }
        let subscription_id = session.subscription_id();
        self.entries.insert(
            subscription_id,
            ArtifactContentRegistryEntry {
                scope: session.scope(),
                session,
                last_touch: now,
            },
        );
    }

    pub(crate) fn get(
        &self,
        subscription_id: SubscriptionId,
        client_id: ClientId,
        task_id: TaskId,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
        now: Instant,
    ) -> Result<&ArtifactContentSession, ArtifactContentError> {
        self.get_scoped(
            subscription_id,
            SessionScope {
                client_id: Some(client_id),
                task_id: Some(task_id),
                connection_id: None,
                action_epoch: None,
                runtime_generation: None,
            },
            limits,
            max_reassembled_message_bytes,
            max_physical_frame_bytes,
            now,
        )
    }

    pub(crate) fn get_scoped(
        &self,
        subscription_id: SubscriptionId,
        scope: SessionScope,
        limits: PageLimits,
        max_reassembled_message_bytes: u32,
        max_physical_frame_bytes: u32,
        now: Instant,
    ) -> Result<&ArtifactContentSession, ArtifactContentError> {
        let Some(entry) = self.entries.get(&subscription_id) else {
            return Err(ArtifactContentError::NotFound);
        };
        if now.duration_since(entry.last_touch) >= ARTIFACT_CONTENT_IDLE_TTL {
            return Err(ArtifactContentError::NotFound);
        }
        if entry.scope.client_id != scope.client_id {
            return Err(ArtifactContentError::Unauthorized);
        }
        if entry.scope.task_id != scope.task_id {
            return Err(ArtifactContentError::Unauthorized);
        }
        if entry.scope != scope {
            return Err(ArtifactContentError::Unauthorized);
        }
        if entry.session.limits != limits
            || entry.session.max_reassembled_message_bytes != max_reassembled_message_bytes
            || entry.session.max_physical_frame_bytes != max_physical_frame_bytes
        {
            return Err(ArtifactContentError::InvalidRequest);
        }
        Ok(&entry.session)
    }

    pub(crate) fn touch(&mut self, subscription_id: SubscriptionId, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&subscription_id) {
            entry.last_touch = now;
        }
    }

    pub(crate) fn release_scoped(
        &mut self,
        subscription_id: SubscriptionId,
        scope: SessionScope,
    ) -> Result<(), ArtifactContentError> {
        match self.entries.get(&subscription_id) {
            // A release is an authorization operation, not an idempotent
            // best-effort cleanup.  Treat an expired/evicted/already-released
            // subscription as stale so callers cannot turn a stale scope into
            // a successful release acknowledgement.
            None => Err(ArtifactContentError::NotFound),
            Some(entry) if entry.scope.client_id != scope.client_id => {
                Err(ArtifactContentError::Unauthorized)
            }
            Some(entry) if entry.scope.task_id != scope.task_id => {
                Err(ArtifactContentError::Unauthorized)
            }
            Some(entry) if entry.scope != scope => Err(ArtifactContentError::Unauthorized),
            Some(_) => {
                self.entries.remove(&subscription_id);
                Ok(())
            }
        }
    }

    pub(crate) fn reap(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_touch) < ARTIFACT_CONTENT_IDLE_TTL);
    }

    /// Migrate the registry authorization scope after a host-authenticated
    /// reconnect.  The session keeps its original cursor scope so a cursor
    /// issued before the handoff remains verifiable, while registry lookup
    /// requires the new physical connection identity.
    pub(crate) fn rebind_output(
        &mut self,
        client_id: ClientId,
        old_output: uuid::Uuid,
        new_output: uuid::Uuid,
    ) {
        for entry in self.entries.values_mut() {
            if entry.scope.client_id != Some(client_id)
                || entry.scope.connection_id != Some(old_output)
            {
                continue;
            }
            entry.scope.connection_id = Some(new_output);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn contains(&self, subscription_id: SubscriptionId) -> bool {
        self.entries.contains_key(&subscription_id)
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::domain::artifact::{ArtifactFacts, ArtifactKind, PrivacyClass};
    use crate::domain::command::{Command, CommandEnvelope, CommandReceipt, CreateTaskIntent};
    use crate::domain::id::{CommandId, EnvironmentId, ProjectId};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
        WorkspaceRef,
    };

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    macro_rules! test_id {
        ($name:ident, $ty:ty) => {
            fn $name(tail: u8) -> $ty {
                <$ty>::from_bytes(fixed_uuid_v7(tail)).expect(stringify!($name))
            }
        };
    }

    test_id!(task_id, TaskId);
    test_id!(environment_id, EnvironmentId);
    test_id!(project_id, ProjectId);
    test_id!(command_id, CommandId);
    test_id!(client_id, ClientId);
    test_id!(artifact_id, ArtifactId);

    fn envelope(
        command_id: CommandId,
        task_id: Option<TaskId>,
        expected_task_revision: Option<u64>,
        command: Command,
    ) -> CommandEnvelope {
        CommandEnvelope {
            command_id,
            client_id: client_id(0x01),
            task_id,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision,
            command,
        }
    }

    fn create_task(store: &mut KernelStore, task: TaskId, command: CommandId) {
        let intent = CreateTaskIntent {
            id: task,
            environment_id: environment_id(0x02),
            title: "Artifact content task".into(),
            description: Some("paged body".into()),
            project_id: project_id(0x03),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1_725_000_000_000,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        };
        assert!(matches!(
            store
                .execute_for_test(envelope(command, None, None, Command::CreateTask(intent)))
                .expect("create task"),
            CommandReceipt::Accepted { .. }
        ));
    }

    fn sha256_of(body: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        hasher.finalize().into()
    }

    fn register_artifact(
        store: &mut KernelStore,
        task: TaskId,
        artifact: ArtifactId,
        label: &str,
        body: &str,
        command: CommandId,
        expected_revision: u64,
    ) {
        let facts = ArtifactFacts {
            id: artifact,
            task_id: task,
            kind: ArtifactKind::Evidence,
            label: label.into(),
            content_ref: ArtifactContentRef::inline_utf8(body).expect("body"),
            sha256: sha256_of(body),
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_200,
        };
        store
            .execute(envelope(
                command,
                Some(task),
                Some(expected_revision),
                Command::RegisterArtifact { artifact: facts },
            ))
            .expect("register artifact");
    }

    #[test]
    fn artifact_content_pages_resume_exact_bytes_within_negotiated_limits() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x10);
        let artifact = artifact_id(0x11);
        let client = client_id(0x12);
        create_task(&mut store, task, command_id(0x13));

        // Multi-page UTF-8 body: force several pages under a tight encoded bound.
        let body = "αβγδεζηθικλμνξοπρστυφχψω"
            .repeat(40)
            .chars()
            .cycle()
            .take(2_400)
            .collect::<String>();
        let expected_digest = sha256_of(&body);
        register_artifact(
            &mut store,
            task,
            artifact,
            "Paged",
            &body,
            command_id(0x14),
            1,
        );

        let limits = PageLimits::new(10, 1_024).expect("limits");
        let session = store
            .begin_artifact_content(
                client,
                task,
                artifact,
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
            )
            .expect("begin");

        let mut reconstructed = Vec::new();
        let mut resume = None;
        let mut pages = 0usize;
        let mut last_end = 0u64;
        loop {
            let page = session
                .page(resume.as_deref())
                .expect("page within negotiated limits");
            assert!(page.encoded_bytes <= limits.max_encoded_bytes);
            assert_eq!(
                usize::try_from(page.encoded_bytes).expect("fits"),
                rmp_serde::to_vec_named(&page).expect("encode").len()
            );
            assert_eq!(page.artifact_id, artifact);
            assert_eq!(page.sha256, expected_digest);
            assert_eq!(page.total_bytes, body.len() as u64);
            assert_eq!(page.offset, last_end);
            if !page.payload.is_empty() {
                assert!(page.offset + page.payload.len() as u64 <= page.total_bytes);
            }
            reconstructed.extend_from_slice(&page.payload);
            last_end = page.offset + page.payload.len() as u64;
            pages += 1;
            match page.next_cursor {
                Some(cursor) => resume = Some(cursor),
                None => break,
            }
            assert!(pages < 64, "must make forward progress");
        }

        assert!(pages > 1, "tight limit must require multiple pages");
        assert_eq!(reconstructed, body.as_bytes());
        assert_eq!(sha256_of(&body), expected_digest);
        let mut hasher = Sha256::new();
        hasher.update(&reconstructed);
        assert_eq!(<[u8; 32]>::from(hasher.finalize()), expected_digest);
    }

    #[test]
    fn artifact_content_cursor_rejects_tamper_foreign_client_target_and_limits() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x20);
        let other_task = task_id(0x21);
        let artifact = artifact_id(0x22);
        let client = client_id(0x23);
        let foreign = client_id(0x24);
        create_task(&mut store, task, command_id(0x25));
        create_task(&mut store, other_task, command_id(0x26));

        let body = "x".repeat(4_000);
        register_artifact(
            &mut store,
            task,
            artifact,
            "Secure",
            &body,
            command_id(0x27),
            1,
        );

        let limits = PageLimits::new(10, 1_024).expect("limits");
        let max_reassembled = 16 * 1024 * 1024u32;
        let max_physical = 1024 * 1024u32;
        let session = store
            .begin_artifact_content(
                client,
                task,
                artifact,
                limits,
                max_reassembled,
                max_physical,
            )
            .expect("begin");
        let subscription_id = session.subscription_id();

        let mut registry = ArtifactContentRegistry::new();
        let now = Instant::now();
        registry.insert(session, now);

        let first = registry
            .get(
                subscription_id,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("owner get")
            .page(None)
            .expect("first page");
        let cursor = first.next_cursor.clone().expect("multi-page cursor");

        let mut tampered = cursor.clone();
        let last = tampered.last_mut().expect("cursor bytes");
        *last ^= 0x5a;
        let session = registry
            .get(
                subscription_id,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("session survives");
        assert!(matches!(
            session.page(Some(&tampered)),
            Err(ArtifactContentError::InvalidCursor)
                | Err(ArtifactContentError::CursorContextMismatch)
        ));

        assert!(matches!(
            registry.get(
                subscription_id,
                foreign,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            ),
            Err(ArtifactContentError::Unauthorized)
        ));
        assert!(matches!(
            registry.get(
                subscription_id,
                client,
                other_task,
                limits,
                max_reassembled,
                max_physical,
                now,
            ),
            Err(ArtifactContentError::Unauthorized)
        ));
        let changed_limits = PageLimits::new(10, 1_025).expect("changed");
        assert!(matches!(
            registry.get(
                subscription_id,
                client,
                task,
                changed_limits,
                max_reassembled,
                max_physical,
                now,
            ),
            Err(ArtifactContentError::InvalidRequest)
        ));
        assert!(matches!(
            registry.get(
                subscription_id,
                client,
                task,
                limits,
                max_reassembled - 1,
                max_physical,
                now,
            ),
            Err(ArtifactContentError::InvalidRequest)
        ));

        let continued = registry
            .get(
                subscription_id,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("still present after failures")
            .page(Some(&cursor))
            .expect("valid continue after failures");
        assert_eq!(continued.offset, first.payload.len() as u64);
        assert!(!continued.payload.is_empty() || continued.next_cursor.is_none());
    }

    #[test]
    fn artifact_content_rejects_mismatched_inline_digest() {
        // Catches: serving InlineUtf8 without verifying declared SHA-256.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x30);
        let artifact = artifact_id(0x31);
        create_task(&mut store, task, command_id(0x32));

        let body = "DIGEST_MISMATCH_BODY_TOKEN";
        let facts = ArtifactFacts {
            id: artifact,
            task_id: task,
            kind: ArtifactKind::Evidence,
            label: "Mismatch".into(),
            content_ref: ArtifactContentRef::inline_utf8(body).expect("body"),
            sha256: [0xab; 32],
            privacy_class: PrivacyClass::LocalOnly,
            created_at_ms: 1_725_000_000_200,
        };
        assert_ne!(sha256_of(body), facts.sha256);
        store
            .execute(envelope(
                command_id(0x33),
                Some(task),
                Some(1),
                Command::RegisterArtifact { artifact: facts },
            ))
            .expect("register mismatched digest artifact");

        let err = store
            .begin_artifact_content(
                client_id(0x34),
                task,
                artifact,
                PageLimits::new(10, 64 * 1024).expect("limits"),
                16 * 1024 * 1024,
                1024 * 1024,
            )
            .expect_err("mismatched digest must fail closed");
        assert!(
            matches!(err, ArtifactContentError::ContentDigestMismatch),
            "expected ContentDigestMismatch, got {err:?}"
        );
        assert_eq!(
            crate::domain::artifact::ArtifactSummary::from_facts(
                &command_bus::load_artifact(
                    &store.open_query_connection().expect("conn"),
                    artifact,
                )
                .expect("load")
                .expect("artifact")
            )
            .expect_err("summary must reject mismatch"),
            crate::domain::artifact::ArtifactValidationError::ContentDigestMismatch
        );
    }

    #[test]
    fn artifact_content_final_page_prefers_cursorless_when_it_fits() {
        // Catches: binary search skipping the cursorless final page when a partial
        // page-with-cursor overflows the encoded bound.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x40);
        let artifact = artifact_id(0x41);
        let client = client_id(0x42);
        create_task(&mut store, task, command_id(0x43));

        let body = "Z".repeat(900);
        register_artifact(
            &mut store,
            task,
            artifact,
            "FinalFit",
            &body,
            command_id(0x44),
            1,
        );

        // Tight bound: first page must be partial; remaining must fit only without a cursor.
        // The cursor now carries the complete authenticated request scope, so
        // leave enough room for that metadata while still forcing the first
        // page to be partial.  The final remainder must fit only cursorless.
        let limits = PageLimits::new(10, 900).expect("limits");
        let session = store
            .begin_artifact_content(
                client,
                task,
                artifact,
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
            )
            .expect("begin");
        let first = session.page(None).expect("first page");
        assert!(
            first.next_cursor.is_some(),
            "fixture must force a continuation page"
        );
        assert!(first.payload.len() < body.len());

        let second = session
            .page(first.next_cursor.as_deref())
            .expect("cursorless final page must fit");
        assert!(second.next_cursor.is_none());
        assert_eq!(
            first.payload.len() + second.payload.len(),
            body.len(),
            "final page must consume the entire remainder"
        );
        assert_eq!(
            [&first.payload[..], &second.payload[..]].concat(),
            body.as_bytes()
        );
    }

    #[test]
    fn artifact_content_wrong_artifact_cursor_is_rejected_without_destroying_source() {
        // Catches: accepting a foreign session cursor against another artifact session.
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("kernel.sqlite3");
        let mut store = KernelStore::open(&path).expect("open");
        let task = task_id(0x50);
        let artifact_a = artifact_id(0x51);
        let artifact_b = artifact_id(0x52);
        let client = client_id(0x53);
        create_task(&mut store, task, command_id(0x54));

        let body_a = "A".repeat(3_000);
        let body_b = "B".repeat(3_000);
        register_artifact(
            &mut store,
            task,
            artifact_a,
            "A",
            &body_a,
            command_id(0x55),
            1,
        );
        register_artifact(
            &mut store,
            task,
            artifact_b,
            "B",
            &body_b,
            command_id(0x56),
            2,
        );

        let limits = PageLimits::new(10, 1_024).expect("limits");
        let max_reassembled = 16 * 1024 * 1024u32;
        let max_physical = 1024 * 1024u32;
        let session_a = store
            .begin_artifact_content(
                client,
                task,
                artifact_a,
                limits,
                max_reassembled,
                max_physical,
            )
            .expect("begin a");
        let session_b = store
            .begin_artifact_content(
                client,
                task,
                artifact_b,
                limits,
                max_reassembled,
                max_physical,
            )
            .expect("begin b");
        let sub_a = session_a.subscription_id();
        let sub_b = session_b.subscription_id();

        let mut registry = ArtifactContentRegistry::new();
        let now = Instant::now();
        registry.insert(session_a, now);
        registry.insert(session_b, now);

        let first_a = registry
            .get(
                sub_a,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("get a")
            .page(None)
            .expect("page a");
        let cursor_a = first_a.next_cursor.clone().expect("cursor a");

        let foreign = registry
            .get(
                sub_b,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("get b");
        assert!(matches!(
            foreign.page(Some(&cursor_a)),
            Err(ArtifactContentError::InvalidCursor)
                | Err(ArtifactContentError::CursorContextMismatch)
        ));

        let continued = registry
            .get(
                sub_a,
                client,
                task,
                limits,
                max_reassembled,
                max_physical,
                now,
            )
            .expect("source session retained")
            .page(Some(&cursor_a))
            .expect("original cursor still resumes");
        assert_eq!(continued.offset, first_a.payload.len() as u64);
    }

    #[test]
    fn artifact_content_registry_evicts_lru_at_capacity_and_honors_idle_ttl() {
        // Catches: unbounded registry growth, FIFO-instead-of-LRU eviction, and
        // TTL/reap drift without Instant-controlled fixtures.
        let client = client_id(0x60);
        let task = task_id(0x61);
        let limits = PageLimits::new(10, 64 * 1024).expect("limits");
        let mut registry = ArtifactContentRegistry::new();
        let t0 = Instant::now();

        let mut ids = Vec::with_capacity(33);
        for i in 0..32u8 {
            let subscription = SubscriptionId::from_bytes(fixed_uuid_v7(i)).expect("sub");
            let artifact = ArtifactId::from_bytes(fixed_uuid_v7(0x80 ^ i)).expect("artifact");
            registry.insert(
                ArtifactContentSession::test_stub(client, task, artifact, subscription, limits),
                t0 + Duration::from_millis(u64::from(i)),
            );
            ids.push(subscription);
        }
        assert_eq!(registry.len(), 32);

        // Touch the oldest session so the second-oldest becomes the LRU victim.
        registry.touch(ids[0], t0 + Duration::from_millis(1_000));
        let overflow = SubscriptionId::from_bytes(fixed_uuid_v7(0x70)).expect("overflow");
        registry.insert(
            ArtifactContentSession::test_stub(
                client,
                task,
                ArtifactId::from_bytes(fixed_uuid_v7(0x71)).expect("artifact"),
                overflow,
                limits,
            ),
            t0 + Duration::from_millis(1_001),
        );
        assert_eq!(registry.len(), 32);
        assert!(registry.contains(ids[0]), "touched session must survive");
        assert!(registry.contains(overflow), "33rd insert must retain");
        assert!(
            !registry.contains(ids[1]),
            "true LRU victim is the oldest untouched session"
        );

        let survivor = ids[2];
        let survivor_last_touch = t0 + Duration::from_millis(2);
        let below_ttl = survivor_last_touch + ARTIFACT_CONTENT_IDLE_TTL - Duration::from_millis(1);
        assert!(
            registry
                .get(
                    survivor,
                    client,
                    task,
                    limits,
                    16 * 1024 * 1024,
                    1024 * 1024,
                    below_ttl,
                )
                .is_ok(),
            "session must survive immediately below the idle TTL"
        );

        let at_ttl = survivor_last_touch + ARTIFACT_CONTENT_IDLE_TTL;
        assert!(matches!(
            registry.get(
                survivor,
                client,
                task,
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
                at_ttl,
            ),
            Err(ArtifactContentError::NotFound)
        ));

        registry.reap(at_ttl);
        assert!(
            !registry.contains(survivor),
            "reap must drop idle-expired sessions"
        );
        assert!(
            registry.contains(ids[0]),
            "recently touched session remains after reap of older idle entries"
        );
    }

    #[test]
    fn artifact_cursor_requires_exact_connection_and_reconnect_migrates_once() {
        let client = client_id(0x72);
        let task = task_id(0x73);
        let artifact = ArtifactId::from_bytes(fixed_uuid_v7(0x74)).expect("artifact");
        let subscription = SubscriptionId::from_bytes(fixed_uuid_v7(0x75)).expect("subscription");
        let old_connection = Uuid::now_v7();
        let new_connection = Uuid::now_v7();
        let scope = |connection_id| SessionScope {
            client_id: Some(client),
            task_id: Some(task),
            connection_id: Some(connection_id),
            action_epoch: Some(4),
            runtime_generation: Some(9),
        };
        let limits = PageLimits::new(1, 1_024).expect("limits");
        let mut registry = ArtifactContentRegistry::new();
        registry.insert(
            ArtifactContentSession::test_stub_scoped(
                scope(old_connection),
                RequestId::new(),
                artifact,
                subscription,
                limits,
            ),
            Instant::now(),
        );
        let now = Instant::now();
        let first = registry
            .get_scoped(
                subscription,
                scope(old_connection),
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
                now,
            )
            .expect("old connection owns cursor")
            .page(None)
            .expect("first page");
        let first_payload_len = first.payload.len() as u64;
        let cursor = first.next_cursor.clone().expect("bounded cursor");

        assert!(matches!(
            registry.get_scoped(
                subscription,
                scope(new_connection),
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
                now,
            ),
            Err(ArtifactContentError::Unauthorized)
        ));

        registry.rebind_output(client, old_connection, new_connection);
        assert!(matches!(
            registry.get_scoped(
                subscription,
                scope(old_connection),
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
                now,
            ),
            Err(ArtifactContentError::Unauthorized)
        ));
        assert!(matches!(
            registry.release_scoped(subscription, scope(old_connection)),
            Err(ArtifactContentError::Unauthorized)
        ));
        let resumed = registry
            .get_scoped(
                subscription,
                scope(new_connection),
                limits,
                16 * 1024 * 1024,
                1024 * 1024,
                now,
            )
            .expect("new connection owns migrated cursor")
            .page(Some(cursor.as_slice()))
            .expect("cursor remains valid only through the explicit migration");
        assert_eq!(resumed.offset, first_payload_len);
        registry
            .release_scoped(subscription, scope(new_connection))
            .expect("new connection may release migrated cursor");
        assert!(!registry.contains(subscription));
        assert!(matches!(
            registry.release_scoped(subscription, scope(new_connection)),
            Err(ArtifactContentError::NotFound)
        ));
    }
}
