//! Provider-neutral semantic journal: binding-first normalizer and bounded store.
//!
//! Trusted identity comes only from an authenticated adapter-delivery binding.
//! Provider payload bytes are content. Unknown/malformed facts never enter the
//! task reducer. Stock adapter ingress is unavailable until Tasks 4.3–4.5 exist.

use crate::domain::{
    AgentSessionId, DomainEvent, EventId, PageLimits, PrivacyClass, ResourceId,
    SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload, TaskId,
};
use crate::kernel::semantic_journal::{SemanticJournalAuthorityRecord, SemanticJournalFactRow};
use crate::kernel::{KernelStore, StoreError};
use crate::protocol::{FrameLimits, MessagePackCodec, MessagePackError};
use crate::providers::capabilities::ProviderKind;
use hmac::{Hmac, Mac};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::Instant;

pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
pub const MAX_JOURNAL_DOCUMENT_BYTES: usize = 64 * 1024;
pub const MAX_JOURNAL_DOCUMENT_HARD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_JOURNAL_NESTING: usize = 8;
pub const MAX_JOURNAL_MAP_ENTRIES: usize = 32;
pub const MAX_JOURNAL_ARRAY_ITEMS: usize = 16;
pub const MAX_PROVIDER_EVENT_ID_BYTES: usize = 256;
pub const MAX_DELIVERY_ID_BYTES: usize = 256;
pub const MAX_JOURNAL_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_SOURCE_TYPE_BYTES: usize = 128;
pub const MAX_EXTENSION_ENTRIES: usize = 8;
pub const MAX_EXTENSION_KEY_BYTES: usize = 64;
pub const MAX_EXTENSION_VALUE_BYTES: usize = 256;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
pub const MAX_CALL_ID_BYTES: usize = 128;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_QUESTION_OPTIONS: usize = 16;
pub const MAX_JOURNAL_JSON_NODES: usize = 512;
pub const MAX_JOURNAL_EVENTS: u32 = 4_096;
pub const MAX_JOURNAL_DEDUPE_KEYS: u32 = 8_192;
pub const DEFAULT_MAX_INGEST_STEPS: u32 = 65_536;
const FORBIDDEN_ROOT_KEYS: &[&str] = &[
    "task_id",
    "agent_session_id",
    "runtime_generation",
    "provider",
    "delivery_id",
    "resource_id",
    "action_epoch",
    "managed_root",
];
const EXTENSION_ALLOWLIST: &[&str] = &["hook_event_name", "codex_item", "cursor_surface"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterIngressUnavailable;

impl fmt::Display for AdapterIngressUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stock Claude/Codex/Cursor adapter ingress is unavailable until Tasks 4.3-4.5 exist"
        )
    }
}

impl std::error::Error for AdapterIngressUnavailable {}

pub const fn stock_adapter_ingress_available() -> bool {
    false
}

pub fn stock_adapter_ingress() -> Result<std::convert::Infallible, AdapterIngressUnavailable> {
    Err(AdapterIngressUnavailable)
}

/// Content-only adapter output. It cannot carry EventId/sequence and has no
/// public constructor, so adapters cannot mint committed journal identity.
pub struct NormalizedAdapterDelivery {
    _private: (),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalNormalizeError {
    Unavailable(AdapterIngressUnavailable),
}

impl fmt::Display for JournalNormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for JournalNormalizeError {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JournalError {
    Empty,
    TooLong,
    ContainsControlCharacter,
    ContainsBidiCharacter,
    TooManyExtensions,
    UnsupportedSchemaVersion,
    InvalidEnvelope,
    DuplicateKey,
    NestingTooDeep,
    Oversized,
    ForgedIdentity,
    Expired,
    Foreign,
    Store,
}

impl fmt::Debug for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl JournalError {
    const fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::ContainsControlCharacter => "control",
            Self::ContainsBidiCharacter => "bidi",
            Self::TooManyExtensions => "too_many_extensions",
            Self::UnsupportedSchemaVersion => "unsupported_schema",
            Self::InvalidEnvelope => "invalid_envelope",
            Self::DuplicateKey => "duplicate_key",
            Self::NestingTooDeep => "nesting",
            Self::Oversized => "oversized",
            Self::ForgedIdentity => "forged_identity",
            Self::Expired => "expired",
            Self::Foreign => "foreign",
            Self::Store => "store",
        }
    }
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for JournalError {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JournalRejectReason {
    Unbound,
    Expired,
    Foreign,
    ForgedIdentity,
    SequenceOverflow,
    TimestampRegression,
    TimestampOverflow,
    InvalidEnvelope,
}

impl fmt::Debug for JournalRejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unbound => "unbound",
            Self::Expired => "expired",
            Self::Foreign => "foreign",
            Self::ForgedIdentity => "forged_identity",
            Self::SequenceOverflow => "sequence_overflow",
            Self::TimestampRegression => "timestamp_regression",
            Self::TimestampOverflow => "timestamp_overflow",
            Self::InvalidEnvelope => "invalid_envelope",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum JournalBackpressure {
    EventCapacity,
    DedupeCapacity,
    WorkBudget,
    PageBudget,
}

impl fmt::Debug for JournalBackpressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EventCapacity => "event_capacity",
            Self::DedupeCapacity => "dedupe_capacity",
            Self::WorkBudget => "work_budget",
            Self::PageBudget => "page_budget",
        })
    }
}

fn reject_display_bound(value: &str, max_bytes: usize) -> Result<(), JournalError> {
    if value.is_empty() {
        return Err(JournalError::Empty);
    }
    if value.len() > max_bytes {
        return Err(JournalError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(JournalError::ContainsControlCharacter);
    }
    if value.chars().any(is_bidi) {
        return Err(JournalError::ContainsBidiCharacter);
    }
    Ok(())
}

fn is_bidi(character: char) -> bool {
    matches!(
        character,
        '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ProviderEventId(String);

impl fmt::Debug for ProviderEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ProviderEventId")
            .field(&self.0.len())
            .finish()
    }
}

impl ProviderEventId {
    pub fn new(value: impl Into<String>) -> Result<Self, JournalError> {
        let value = value.into();
        reject_display_bound(&value, MAX_PROVIDER_EVENT_ID_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ProviderEventId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(BoundedString::<MAX_PROVIDER_EVENT_ID_BYTES>::deserialize(deserializer)?.0)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct RelayDeliveryId(String);

impl fmt::Debug for RelayDeliveryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RelayDeliveryId")
            .field(&self.0.len())
            .finish()
    }
}

impl RelayDeliveryId {
    pub fn new(value: impl Into<String>) -> Result<Self, JournalError> {
        let value = value.into();
        reject_display_bound(&value, MAX_DELIVERY_ID_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ManagedRoot([u8; 32]);

impl fmt::Debug for ManagedRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ManagedRoot").field(&self.0.len()).finish()
    }
}

impl ManagedRoot {
    const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalSessionAuthority {
    provider: ProviderKind,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    resource_id: ResourceId,
    runtime_generation: u64,
    action_epoch: u64,
    managed_root: ManagedRoot,
}

impl fmt::Debug for JournalSessionAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalSessionAuthority")
            .field("provider", &self.provider)
            .field("runtime_generation", &self.runtime_generation)
            .field("action_epoch", &self.action_epoch)
            .finish_non_exhaustive()
    }
}

impl JournalSessionAuthority {
    const fn new(
        provider: ProviderKind,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
        managed_root: ManagedRoot,
    ) -> Self {
        Self {
            provider,
            task_id,
            agent_session_id,
            resource_id,
            runtime_generation,
            action_epoch,
            managed_root,
        }
    }

    fn digest(self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(provider_kind_sql(self.provider).as_bytes());
        hasher.update([0]);
        hasher.update(self.task_id.as_bytes());
        hasher.update(self.agent_session_id.as_bytes());
        hasher.update(self.resource_id.as_bytes());
        hasher.update(self.runtime_generation.to_be_bytes());
        hasher.update(self.action_epoch.to_be_bytes());
        hasher.update(self.managed_root.as_bytes());
        hasher.finalize().into()
    }

    fn to_record(self, opened_at_ms: i64) -> Result<SemanticJournalAuthorityRecord, JournalError> {
        Ok(SemanticJournalAuthorityRecord {
            digest: self.digest(),
            provider_kind: provider_kind_sql(self.provider).to_string(),
            task_id: *self.task_id.as_bytes(),
            agent_session_id: *self.agent_session_id.as_bytes(),
            resource_id: *self.resource_id.as_bytes(),
            runtime_generation: i64::try_from(self.runtime_generation)
                .map_err(|_| JournalError::InvalidEnvelope)?,
            action_epoch: i64::try_from(self.action_epoch)
                .map_err(|_| JournalError::InvalidEnvelope)?,
            managed_root: self.managed_root.as_bytes(),
            opened_at_ms,
        })
    }

    pub const fn task_id(self) -> TaskId {
        self.task_id
    }

    pub const fn agent_session_id(self) -> AgentSessionId {
        self.agent_session_id
    }

    pub const fn runtime_generation(self) -> u64 {
        self.runtime_generation
    }

    pub const fn provider(self) -> ProviderKind {
        self.provider
    }
}

/// One-shot authenticated adapter/relay delivery permit. Not Clone.
pub struct AdapterDeliveryPermit {
    authority: JournalSessionAuthority,
    delivery_id: RelayDeliveryId,
    nonce: [u8; 16],
    issued_at_ms: i64,
    expires_at_ms: i64,
}

impl fmt::Debug for AdapterDeliveryPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdapterDeliveryPermit")
            .field("delivery_bytes", &self.delivery_id.0.len())
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

impl AdapterDeliveryPermit {
    fn issue(
        provider: ProviderKind,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
        managed_root: [u8; 32],
        delivery_id: impl Into<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<Self, JournalError> {
        if issued_at_ms <= 0 || expires_at_ms <= issued_at_ms {
            return Err(JournalError::InvalidEnvelope);
        }
        Ok(Self {
            authority: JournalSessionAuthority::new(
                provider,
                task_id,
                agent_session_id,
                resource_id,
                runtime_generation,
                action_epoch,
                ManagedRoot::from_bytes(managed_root),
            ),
            delivery_id: RelayDeliveryId::new(delivery_id)?,
            nonce: *EventId::new().as_bytes(),
            issued_at_ms,
            expires_at_ms,
        })
    }

    #[cfg(test)]
    pub(in crate::providers::journal) fn issue_for_test(
        provider: ProviderKind,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
        managed_root: [u8; 32],
        delivery_id: impl Into<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<Self, JournalError> {
        Self::issue(
            provider,
            task_id,
            agent_session_id,
            resource_id,
            runtime_generation,
            action_epoch,
            managed_root,
            delivery_id,
            issued_at_ms,
            expires_at_ms,
        )
    }

    pub fn delivery_id(&self) -> &str {
        self.delivery_id.as_str()
    }

    fn validate_against(
        &self,
        expected: &JournalSessionAuthority,
        now_ms: i64,
    ) -> Result<(), JournalRejectReason> {
        if now_ms < 0 {
            return Err(JournalRejectReason::TimestampOverflow);
        }
        if now_ms < self.issued_at_ms || now_ms >= self.expires_at_ms {
            return Err(JournalRejectReason::Expired);
        }
        if self.authority != *expected {
            return Err(JournalRejectReason::Foreign);
        }
        Ok(())
    }
}

fn provider_kind_sql(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::ClaudeCode => "claude_code",
        ProviderKind::Codex => "codex",
        ProviderKind::Cursor => "cursor",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalSemanticKind {
    UserMessage,
    AssistantText,
    ReasoningSummary,
    ToolCall,
    ToolResult,
    ApprovalRequest,
    ApprovalResult,
    Question,
    PlanStep,
    UsageObservation,
    Error,
    TurnState,
    SessionState,
    ArtifactReference,
    UnknownProviderEvent,
}

impl JournalSemanticKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AssistantText => "assistant_text",
            Self::ReasoningSummary => "reasoning_summary",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ApprovalRequest => "approval_request",
            Self::ApprovalResult => "approval_result",
            Self::Question => "question",
            Self::PlanStep => "plan_step",
            Self::UsageObservation => "usage_observation",
            Self::Error => "error",
            Self::TurnState => "turn_state",
            Self::SessionState => "session_state",
            Self::ArtifactReference => "artifact_reference",
            Self::UnknownProviderEvent => "unknown_provider_event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalVisibility {
    Semantic,
    Diagnostic,
    RuntimeOnly,
}

impl JournalVisibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Diagnostic => "diagnostic",
            Self::RuntimeOnly => "runtime_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalRedactionClass {
    Persistable,
    PersistableLocalOnly,
    RedactOnPersist,
    MetadataOnly,
    NeverPersist,
}

impl JournalRedactionClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Persistable => "persistable",
            Self::PersistableLocalOnly => "persistable_local_only",
            Self::RedactOnPersist => "redact_on_persist",
            Self::MetadataOnly => "metadata_only",
            Self::NeverPersist => "never_persist",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnLifecycleState {
    Started,
    Completed,
    Failed,
}

#[derive(Clone, PartialEq, Eq)]
pub enum NativeJournalPayload {
    UserMessage {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolCall {
        tool_name: String,
        call_id: String,
    },
    ToolResult {
        call_id: String,
        status: String,
    },
    ApprovalRequest {
        request_id: String,
        summary: String,
    },
    ApprovalResult {
        request_id: String,
        decision: String,
    },
    Question {
        question_id: String,
        prompt: String,
        options: Vec<String>,
    },
    PlanStep {
        step_id: String,
        title: String,
        status: String,
    },
    UsageObservation {
        remaining_percent: Option<u8>,
    },
    Error {
        code: String,
        message: String,
    },
    TurnState {
        state: TurnLifecycleState,
    },
    SessionState {
        state: SessionLifecycleState,
    },
    ArtifactReference {
        label: String,
    },
    TerminalBytes,
    Unknown,
}

impl fmt::Debug for NativeJournalPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UserMessage { .. } => "UserMessage",
            Self::AssistantText { .. } => "AssistantText",
            Self::ReasoningSummary { .. } => "ReasoningSummary",
            Self::ToolCall { .. } => "ToolCall",
            Self::ToolResult { .. } => "ToolResult",
            Self::ApprovalRequest { .. } => "ApprovalRequest",
            Self::ApprovalResult { .. } => "ApprovalResult",
            Self::Question { .. } => "Question",
            Self::PlanStep { .. } => "PlanStep",
            Self::UsageObservation { .. } => "UsageObservation",
            Self::Error { .. } => "Error",
            Self::TurnState { .. } => "TurnState",
            Self::SessionState { .. } => "SessionState",
            Self::ArtifactReference { .. } => "ArtifactReference",
            Self::TerminalBytes => "TerminalBytes",
            Self::Unknown => "Unknown",
        })
    }
}

struct BoundedString<const MAX: usize>(String);

impl<'de, const MAX: usize> Deserialize<'de> for BoundedString<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BoundedVisitor<const MAX: usize>;
        impl<const MAX: usize> Visitor<'_> for BoundedVisitor<MAX> {
            type Value = BoundedString<MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a string of at most {MAX} bytes")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > MAX {
                    return Err(E::custom(JournalError::TooLong));
                }
                Ok(BoundedString(value.to_string()))
            }
        }
        deserializer.deserialize_str(BoundedVisitor)
    }
}

impl<'de> Deserialize<'de> for NativeJournalPayload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            #[serde(default)]
            text: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            tool_name: Option<BoundedString<MAX_TOOL_NAME_BYTES>>,
            #[serde(default)]
            call_id: Option<BoundedString<MAX_CALL_ID_BYTES>>,
            #[serde(default)]
            status: Option<BoundedString<MAX_SOURCE_TYPE_BYTES>>,
            #[serde(default)]
            request_id: Option<BoundedString<MAX_REQUEST_ID_BYTES>>,
            #[serde(default)]
            summary: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            decision: Option<BoundedString<MAX_SOURCE_TYPE_BYTES>>,
            #[serde(default)]
            question_id: Option<BoundedString<MAX_REQUEST_ID_BYTES>>,
            #[serde(default)]
            prompt: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            options: Option<Vec<BoundedString<MAX_EXTENSION_VALUE_BYTES>>>,
            #[serde(default)]
            step_id: Option<BoundedString<MAX_REQUEST_ID_BYTES>>,
            #[serde(default)]
            title: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            remaining_percent: Option<u8>,
            #[serde(default)]
            code: Option<BoundedString<MAX_SOURCE_TYPE_BYTES>>,
            #[serde(default)]
            message: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            state: Option<String>,
            #[serde(default)]
            label: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            bytes_b64: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
            #[serde(default)]
            raw_preview: Option<BoundedString<MAX_JOURNAL_TEXT_BYTES>>,
        }

        let wire = Wire::deserialize(deserializer)?;
        match wire.kind.as_str() {
            "user_message" => Ok(Self::UserMessage {
                text: wire
                    .text
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "assistant_text" => Ok(Self::AssistantText {
                text: wire
                    .text
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "reasoning_summary" => Ok(Self::ReasoningSummary {
                text: wire
                    .text
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "tool_call" => Ok(Self::ToolCall {
                tool_name: wire
                    .tool_name
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                call_id: wire
                    .call_id
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "tool_result" => Ok(Self::ToolResult {
                call_id: wire
                    .call_id
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                status: wire
                    .status
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "approval_request" => Ok(Self::ApprovalRequest {
                request_id: wire
                    .request_id
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                summary: wire
                    .summary
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "approval_result" => Ok(Self::ApprovalResult {
                request_id: wire
                    .request_id
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                decision: wire
                    .decision
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "question" => {
                let options = wire.options.unwrap_or_default();
                if options.len() > MAX_QUESTION_OPTIONS {
                    return Err(de::Error::custom(JournalError::TooLong));
                }
                Ok(Self::Question {
                    question_id: wire
                        .question_id
                        .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                        .0,
                    prompt: wire
                        .prompt
                        .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                        .0,
                    options: options.into_iter().map(|item| item.0).collect(),
                })
            }
            "plan_step" => Ok(Self::PlanStep {
                step_id: wire
                    .step_id
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                title: wire
                    .title
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                status: wire
                    .status
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "usage_observation" => Ok(Self::UsageObservation {
                remaining_percent: wire.remaining_percent,
            }),
            "error" => Ok(Self::Error {
                code: wire
                    .code
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
                message: wire
                    .message
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "turn_state" => {
                let state = wire
                    .state
                    .as_deref()
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?;
                let state = match state {
                    "started" => TurnLifecycleState::Started,
                    "completed" => TurnLifecycleState::Completed,
                    "failed" => TurnLifecycleState::Failed,
                    _ => return Err(de::Error::custom(JournalError::InvalidEnvelope)),
                };
                Ok(Self::TurnState { state })
            }
            "session_state" => {
                let state = wire
                    .state
                    .as_deref()
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?;
                let state = match state {
                    "open" => SessionLifecycleState::Open,
                    "closed" => SessionLifecycleState::Closed,
                    _ => return Err(de::Error::custom(JournalError::InvalidEnvelope)),
                };
                Ok(Self::SessionState { state })
            }
            "artifact_reference" => Ok(Self::ArtifactReference {
                label: wire
                    .label
                    .ok_or_else(|| de::Error::custom(JournalError::Empty))?
                    .0,
            }),
            "terminal_bytes" => {
                let _ = wire.bytes_b64;
                Ok(Self::TerminalBytes)
            }
            "unknown" => {
                let _ = wire.raw_preview;
                Ok(Self::Unknown)
            }
            _ => Ok(Self::Unknown),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct NativeJournalContent {
    schema_version: u32,
    source_type: String,
    provider_event_id: Option<ProviderEventId>,
    occurred_at_ms: i64,
    payload: NativeJournalPayload,
    extensions: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for NativeJournalContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u32,
            source_type: BoundedString<MAX_SOURCE_TYPE_BYTES>,
            #[serde(default)]
            provider_event_id: Option<ProviderEventId>,
            occurred_at_ms: i64,
            payload: NativeJournalPayload,
            #[serde(default)]
            extensions: BTreeMap<String, String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(de::Error::custom(JournalError::UnsupportedSchemaVersion));
        }
        reject_display_bound(&wire.source_type.0, MAX_SOURCE_TYPE_BYTES)
            .map_err(de::Error::custom)?;
        if wire.extensions.len() > MAX_EXTENSION_ENTRIES {
            return Err(de::Error::custom(JournalError::TooManyExtensions));
        }
        let mut extensions = BTreeMap::new();
        for (key, value) in wire.extensions {
            if !EXTENSION_ALLOWLIST.contains(&key.as_str()) {
                return Err(de::Error::custom(JournalError::InvalidEnvelope));
            }
            reject_display_bound(&key, MAX_EXTENSION_KEY_BYTES).map_err(de::Error::custom)?;
            reject_display_bound(&value, MAX_EXTENSION_VALUE_BYTES).map_err(de::Error::custom)?;
            if extensions.insert(key, value).is_some() {
                return Err(de::Error::custom(JournalError::DuplicateKey));
            }
        }
        Ok(Self {
            schema_version: wire.schema_version,
            source_type: wire.source_type.0,
            provider_event_id: wire.provider_event_id,
            occurred_at_ms: wire.occurred_at_ms,
            payload: wire.payload,
            extensions,
        })
    }
}

fn preflight_journal_bytes(bytes: &[u8]) -> Result<(), JournalError> {
    if bytes.is_empty() {
        return Err(JournalError::Empty);
    }
    if bytes.len() > MAX_JOURNAL_DOCUMENT_HARD_BYTES || bytes.len() > MAX_JOURNAL_DOCUMENT_BYTES {
        return Err(JournalError::Oversized);
    }
    if bytes[0] == b'{' || bytes[0] == b'[' {
        scan_json(bytes)
    } else {
        Err(JournalError::InvalidEnvelope)
    }
}

fn charge_json_node(nodes: &mut usize) -> Result<(), JournalError> {
    *nodes = nodes.checked_add(1).ok_or(JournalError::Oversized)?;
    if *nodes > MAX_JOURNAL_JSON_NODES {
        return Err(JournalError::Oversized);
    }
    Ok(())
}

fn charge_array_item(array_items: &mut [usize]) -> Result<(), JournalError> {
    let Some(count) = array_items.last_mut() else {
        return Ok(());
    };
    *count = count.checked_add(1).ok_or(JournalError::TooLong)?;
    if *count > MAX_JOURNAL_ARRAY_ITEMS {
        return Err(JournalError::TooLong);
    }
    Ok(())
}

fn scan_json(bytes: &[u8]) -> Result<(), JournalError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0usize;
    let mut object_keys: Vec<HashSet<String>> = Vec::new();
    let mut array_items: Vec<usize> = Vec::new();
    let mut in_object: Vec<bool> = Vec::new();
    let mut pending_key = false;
    let mut current_key = String::new();
    let mut capturing_key = false;
    let mut nodes = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
                string_bytes = string_bytes.saturating_add(1);
                if capturing_key {
                    if byte == b'u' {
                        if index + 4 >= bytes.len() {
                            return Err(JournalError::InvalidEnvelope);
                        }
                        let hex = std::str::from_utf8(&bytes[index + 1..index + 5])
                            .map_err(|_| JournalError::InvalidEnvelope)?;
                        let code = u32::from_str_radix(hex, 16)
                            .map_err(|_| JournalError::InvalidEnvelope)?;
                        current_key
                            .push(char::from_u32(code).ok_or(JournalError::InvalidEnvelope)?);
                        index += 5;
                        continue;
                    }
                    let decoded = match byte {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000c}',
                        other => char::from(other),
                    };
                    current_key.push(decoded);
                }
                index += 1;
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => {
                    in_string = false;
                    if capturing_key {
                        capturing_key = false;
                        pending_key = true;
                    }
                    string_bytes = 0;
                }
                _ => {
                    string_bytes = string_bytes.saturating_add(1);
                    if string_bytes > MAX_JOURNAL_TEXT_BYTES {
                        return Err(JournalError::TooLong);
                    }
                    if capturing_key {
                        current_key.push(char::from(byte));
                        if current_key.len() > MAX_EXTENSION_KEY_BYTES + 32 {
                            return Err(JournalError::TooLong);
                        }
                    }
                }
            }
            index += 1;
            continue;
        }
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => {}
            b'"' => {
                let as_key = in_object.last() == Some(&true) && !pending_key;
                if !as_key {
                    charge_json_node(&mut nodes)?;
                    charge_array_item(&mut array_items)?;
                }
                in_string = true;
                if as_key {
                    capturing_key = true;
                    current_key.clear();
                }
            }
            b'{' => {
                charge_json_node(&mut nodes)?;
                charge_array_item(&mut array_items)?;
                depth = depth.checked_add(1).ok_or(JournalError::NestingTooDeep)?;
                if depth > MAX_JOURNAL_NESTING {
                    return Err(JournalError::NestingTooDeep);
                }
                object_keys.push(HashSet::new());
                in_object.push(true);
                pending_key = false;
            }
            b'[' => {
                charge_json_node(&mut nodes)?;
                charge_array_item(&mut array_items)?;
                depth = depth.checked_add(1).ok_or(JournalError::NestingTooDeep)?;
                if depth > MAX_JOURNAL_NESTING {
                    return Err(JournalError::NestingTooDeep);
                }
                array_items.push(0);
                in_object.push(false);
            }
            b'}' => {
                object_keys.pop();
                in_object.pop();
                depth = depth.checked_sub(1).ok_or(JournalError::InvalidEnvelope)?;
                pending_key = false;
            }
            b']' => {
                array_items.pop();
                in_object.pop();
                depth = depth.checked_sub(1).ok_or(JournalError::InvalidEnvelope)?;
            }
            b':' => {
                if pending_key {
                    if FORBIDDEN_ROOT_KEYS.contains(&current_key.as_str()) && object_keys.len() == 1
                    {
                        return Err(JournalError::ForgedIdentity);
                    }
                    if let Some(keys) = object_keys.last_mut() {
                        if keys.len() >= MAX_JOURNAL_MAP_ENTRIES {
                            return Err(JournalError::TooLong);
                        }
                        if !keys.insert(current_key.clone()) {
                            return Err(JournalError::DuplicateKey);
                        }
                    }
                    pending_key = false;
                }
            }
            b',' => pending_key = false,
            b't' | b'f' | b'n' | b'-' | b'0'..=b'9' => {
                charge_json_node(&mut nodes)?;
                charge_array_item(&mut array_items)?;
                while index + 1 < bytes.len() {
                    let next = bytes[index + 1];
                    if next.is_ascii_alphanumeric() || next == b'+' || next == b'-' || next == b'.'
                    {
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => return Err(JournalError::InvalidEnvelope),
        }
        index += 1;
    }
    if in_string || depth != 0 {
        return Err(JournalError::InvalidEnvelope);
    }
    Ok(())
}

fn parse_journal_content(bytes: &[u8]) -> Result<NativeJournalContent, JournalError> {
    preflight_journal_bytes(bytes)?;
    serde_json::from_slice(bytes).map_err(|_| JournalError::InvalidEnvelope)
}

#[derive(Clone, PartialEq, Eq)]
pub struct UnknownProviderEvent {
    provider: ProviderKind,
    source_type: String,
    schema_version: u32,
    diagnostic_ref: String,
}

impl fmt::Debug for UnknownProviderEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnknownProviderEvent")
            .field("provider", &self.provider)
            .field("source_type_bytes", &self.source_type.len())
            .field("schema_version", &self.schema_version)
            .field("diagnostic_ref_bytes", &self.diagnostic_ref.len())
            .finish()
    }
}

impl UnknownProviderEvent {
    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn source_type(&self) -> &str {
        &self.source_type
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn diagnostic_ref(&self) -> &str {
        &self.diagnostic_ref
    }
}

fn diagnostic_ref(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn diagnostic_metadata(bytes: &[u8]) -> (u32, String, Option<String>) {
    #[derive(Deserialize)]
    struct Wire {
        schema_version: Option<u32>,
        source_type: Option<String>,
        provider_event_id: Option<String>,
    }
    let Ok(wire) = serde_json::from_slice::<Wire>(bytes) else {
        return (JOURNAL_SCHEMA_VERSION, "malformed".to_string(), None);
    };
    let source_type = wire
        .source_type
        .filter(|source| reject_display_bound(source, MAX_SOURCE_TYPE_BYTES).is_ok())
        .unwrap_or_else(|| "malformed".to_string());
    let provider_event_id = wire
        .provider_event_id
        .and_then(|value| ProviderEventId::new(value).ok())
        .map(|value| value.as_str().to_owned());
    (
        wire.schema_version
            .filter(|version| *version > 0)
            .unwrap_or(JOURNAL_SCHEMA_VERSION),
        source_type,
        provider_event_id,
    )
}

#[derive(Clone, PartialEq, Eq)]
pub struct JournalEvent {
    id: EventId,
    schema_version: u32,
    provider: ProviderKind,
    provider_event_id: Option<ProviderEventId>,
    delivery_id: RelayDeliveryId,
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    resource_id: ResourceId,
    runtime_generation: u64,
    action_epoch: u64,
    sequence: u64,
    kind: JournalSemanticKind,
    occurred_at_ms: i64,
    ingested_at_ms: i64,
    visibility: JournalVisibility,
    redaction_class: JournalRedactionClass,
    privacy_class: PrivacyClass,
    text: Option<String>,
    extensions: BTreeMap<String, String>,
    unknown: Option<UnknownProviderEvent>,
    payload: SemanticJournalPayload,
    payload_hash: [u8; 32],
}

impl fmt::Debug for JournalEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalEvent")
            .field("kind", &self.kind)
            .field("sequence", &self.sequence)
            .field("visibility", &self.visibility)
            .field("redaction_class", &self.redaction_class)
            .field("text_bytes", &self.text.as_ref().map(String::len))
            .finish_non_exhaustive()
    }
}

impl JournalEvent {
    pub const fn id(&self) -> EventId {
        self.id
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn provider_event_id(&self) -> Option<&str> {
        self.provider_event_id.as_ref().map(ProviderEventId::as_str)
    }

    pub fn delivery_id(&self) -> &str {
        self.delivery_id.as_str()
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn agent_session_id(&self) -> AgentSessionId {
        self.agent_session_id
    }

    pub const fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn kind(&self) -> JournalSemanticKind {
        self.kind
    }

    pub const fn visibility(&self) -> JournalVisibility {
        self.visibility
    }

    pub const fn redaction_class(&self) -> JournalRedactionClass {
        self.redaction_class
    }

    pub const fn privacy_class(&self) -> PrivacyClass {
        self.privacy_class
    }

    pub fn extension(&self, key: &str) -> Option<&str> {
        self.extensions.get(key).map(String::as_str)
    }

    pub fn projected_text(&self) -> Option<&str> {
        match self.redaction_class {
            JournalRedactionClass::RedactOnPersist
            | JournalRedactionClass::MetadataOnly
            | JournalRedactionClass::NeverPersist => None,
            _ => self.text.as_deref(),
        }
    }

    pub fn unknown_provider_event(&self) -> Option<&UnknownProviderEvent> {
        self.unknown.as_ref()
    }

    pub fn as_domain_event(&self) -> Option<DomainEvent> {
        None
    }

    pub const fn drives_task_question_approval_or_settlement(&self) -> bool {
        false
    }

    pub fn to_snapshot_fact(&self) -> SemanticJournalFact {
        SemanticJournalFact {
            id: self.id,
            sequence: self.sequence,
            provider: provider_kind_sql(self.provider).to_string(),
            schema_version: self.schema_version,
            kind: self.kind.as_str().to_string(),
            visibility: self.visibility.as_str().to_string(),
            privacy_class: self.privacy_class,
            redacted: !matches!(self.redaction_class, JournalRedactionClass::Persistable),
            payload: self.payload.clone(),
        }
    }
}

pub(crate) struct JournalDraft {
    event: JournalEvent,
    authority_digest: [u8; 32],
    managed_root: [u8; 32],
    permit_nonce: [u8; 16],
    permit_issued_at_ms: i64,
    permit_expires_at_ms: i64,
    store_id: [u8; 16],
    seal: [u8; 32],
}

impl fmt::Debug for JournalDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalDraft")
            .field("kind", &self.event.kind)
            .field("sequence", &self.event.sequence)
            .finish_non_exhaustive()
    }
}

impl JournalDraft {
    fn event(&self) -> &JournalEvent {
        &self.event
    }

    fn sealed(
        event: JournalEvent,
        instance_secret: &[u8; 32],
        authority: &JournalSessionAuthority,
        authority_digest: [u8; 32],
        permit_nonce: [u8; 16],
        permit_issued_at_ms: i64,
        permit_expires_at_ms: i64,
        store_id: [u8; 16],
    ) -> Self {
        let seal = compute_draft_seal(
            instance_secret,
            authority,
            &authority_digest,
            &event,
            &permit_nonce,
            permit_issued_at_ms,
            permit_expires_at_ms,
            &store_id,
        );
        Self {
            event,
            authority_digest,
            managed_root: authority.managed_root.as_bytes(),
            permit_nonce,
            permit_issued_at_ms,
            permit_expires_at_ms,
            store_id,
            seal,
        }
    }

    fn verify(
        &self,
        instance_secret: &[u8; 32],
        authority_digest: &[u8; 32],
        authority: &JournalSessionAuthority,
        store_id: &[u8; 16],
    ) -> bool {
        if self.authority_digest != *authority_digest || self.store_id != *store_id {
            return false;
        }
        if self.managed_root != authority.managed_root.as_bytes() {
            return false;
        }
        let bytes = journal_auth_envelope_bytes(
            authority,
            authority_digest,
            &self.event,
            &self.permit_nonce,
            self.permit_issued_at_ms,
            self.permit_expires_at_ms,
            store_id,
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(instance_secret).expect("hmac key length");
        mac.update(b"devmanager.semantic_journal.auth.v1\0");
        mac.update(&bytes);
        mac.verify_slice(&self.seal).is_ok()
    }
}

#[derive(Serialize)]
struct JournalAuthUnknown<'a> {
    provider: ProviderKind,
    source_type: &'a str,
    schema_version: u32,
    diagnostic_ref: &'a str,
}

#[derive(Serialize)]
struct JournalAuthEnvelope<'a> {
    envelope_version: u8,
    authority_digest: [u8; 32],
    store_id: [u8; 16],
    permit_nonce: [u8; 16],
    permit_issued_at_ms: i64,
    permit_expires_at_ms: i64,
    managed_root: [u8; 32],
    provider: ProviderKind,
    task_id: [u8; 16],
    agent_session_id: [u8; 16],
    resource_id: [u8; 16],
    runtime_generation: u64,
    action_epoch: u64,
    event_id: [u8; 16],
    delivery_id: &'a str,
    provider_event_id: Option<&'a str>,
    schema_version: u32,
    sequence: u64,
    kind: JournalSemanticKind,
    occurred_at_ms: i64,
    ingested_at_ms: i64,
    visibility: JournalVisibility,
    redaction_class: JournalRedactionClass,
    privacy_class: PrivacyClass,
    text: Option<&'a str>,
    extensions: &'a BTreeMap<String, String>,
    unknown: Option<JournalAuthUnknown<'a>>,
    payload: &'a SemanticJournalPayload,
    payload_hash: [u8; 32],
}

fn compute_draft_seal(
    secret: &[u8; 32],
    authority: &JournalSessionAuthority,
    authority_digest: &[u8; 32],
    event: &JournalEvent,
    permit_nonce: &[u8; 16],
    permit_issued_at_ms: i64,
    permit_expires_at_ms: i64,
    store_id: &[u8; 16],
) -> [u8; 32] {
    let bytes = journal_auth_envelope_bytes(
        authority,
        authority_digest,
        event,
        permit_nonce,
        permit_issued_at_ms,
        permit_expires_at_ms,
        store_id,
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac key length");
    mac.update(b"devmanager.semantic_journal.auth.v1\0");
    mac.update(&bytes);
    mac.finalize().into_bytes().into()
}

fn journal_auth_envelope_bytes(
    authority: &JournalSessionAuthority,
    authority_digest: &[u8; 32],
    event: &JournalEvent,
    permit_nonce: &[u8; 16],
    permit_issued_at_ms: i64,
    permit_expires_at_ms: i64,
    store_id: &[u8; 16],
) -> Vec<u8> {
    let unknown = event.unknown.as_ref().map(|unknown| JournalAuthUnknown {
        provider: unknown.provider,
        source_type: &unknown.source_type,
        schema_version: unknown.schema_version,
        diagnostic_ref: &unknown.diagnostic_ref,
    });
    let envelope = JournalAuthEnvelope {
        envelope_version: 1,
        authority_digest: *authority_digest,
        store_id: *store_id,
        permit_nonce: *permit_nonce,
        permit_issued_at_ms,
        permit_expires_at_ms,
        managed_root: authority.managed_root.as_bytes(),
        provider: event.provider,
        task_id: *event.task_id.as_bytes(),
        agent_session_id: *event.agent_session_id.as_bytes(),
        resource_id: *event.resource_id.as_bytes(),
        runtime_generation: event.runtime_generation,
        action_epoch: event.action_epoch,
        event_id: *event.id.as_bytes(),
        delivery_id: event.delivery_id.as_str(),
        provider_event_id: event
            .provider_event_id
            .as_ref()
            .map(ProviderEventId::as_str),
        schema_version: event.schema_version,
        sequence: event.sequence,
        kind: event.kind,
        occurred_at_ms: event.occurred_at_ms,
        ingested_at_ms: event.ingested_at_ms,
        visibility: event.visibility,
        redaction_class: event.redaction_class,
        privacy_class: event.privacy_class,
        text: event.text.as_deref(),
        extensions: &event.extensions,
        unknown,
        payload: &event.payload,
        payload_hash: event.payload_hash,
    };
    rmp_serde::to_vec_named(&envelope).expect("journal auth envelope serializes")
}

fn journal_key(authority: &JournalSessionAuthority, store_id: &[u8; 16]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(&authority.managed_root.as_bytes())
        .expect("hmac key length");
    mac.update(b"devmanager.semantic_journal.v1");
    mac.update(&authority.digest());
    mac.update(store_id);
    mac.finalize().into_bytes().into()
}

#[derive(Clone, PartialEq, Eq)]
pub enum JournalIngestOutcome {
    Accepted(JournalEvent),
    Duplicate { existing_id: EventId },
    Conflict { existing_id: EventId },
    Quarantined(JournalEvent),
    IgnoredNeverPersist,
    IgnoredTerminal,
    Rejected(JournalRejectReason),
    Backpressure(JournalBackpressure),
    NeedsResync,
}

impl fmt::Debug for JournalIngestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accepted(event) => f.debug_tuple("Accepted").field(&event.kind).finish(),
            Self::Duplicate { .. } => f.write_str("Duplicate"),
            Self::Conflict { .. } => f.write_str("Conflict"),
            Self::Quarantined(event) => f.debug_tuple("Quarantined").field(&event.kind).finish(),
            Self::IgnoredNeverPersist => f.write_str("IgnoredNeverPersist"),
            Self::IgnoredTerminal => f.write_str("IgnoredTerminal"),
            Self::Rejected(reason) => f.debug_tuple("Rejected").field(reason).finish(),
            Self::Backpressure(kind) => f.debug_tuple("Backpressure").field(kind).finish(),
            Self::NeedsResync => f.write_str("NeedsResync"),
        }
    }
}

impl JournalIngestOutcome {
    fn accepted(&self) -> Option<JournalEvent> {
        match self {
            Self::Accepted(event) => Some(event.clone()),
            _ => None,
        }
    }

    fn quarantined(&self) -> Option<JournalEvent> {
        match self {
            Self::Quarantined(event) => Some(event.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub max_events: u32,
    pub max_dedupe_keys: u32,
    pub max_ingest_steps: u32,
}

impl JournalLimits {
    pub fn new(
        max_events: u32,
        max_dedupe_keys: u32,
        max_ingest_steps: u32,
    ) -> Result<Self, JournalError> {
        let limits = Self {
            max_events,
            max_dedupe_keys,
            max_ingest_steps,
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<(), JournalError> {
        if self.max_events == 0 || self.max_dedupe_keys == 0 || self.max_ingest_steps == 0 {
            return Err(JournalError::InvalidEnvelope);
        }
        if self.max_events > MAX_JOURNAL_EVENTS
            || self.max_dedupe_keys > MAX_JOURNAL_DEDUPE_KEYS
            || self.max_ingest_steps > DEFAULT_MAX_INGEST_STEPS
        {
            return Err(JournalError::Oversized);
        }
        Ok(())
    }
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            max_events: MAX_JOURNAL_EVENTS,
            max_dedupe_keys: MAX_JOURNAL_DEDUPE_KEYS,
            max_ingest_steps: DEFAULT_MAX_INGEST_STEPS,
        }
    }
}

pub struct SemanticJournal {
    store: KernelStore,
    authority: JournalSessionAuthority,
    authority_digest: [u8; 32],
    instance_secret: [u8; 32],
    store_id: [u8; 16],
    limits: JournalLimits,
    next_sequence: u64,
    ingest_steps: u32,
}

impl SemanticJournal {
    pub fn open(
        path: &Path,
        permit: &AdapterDeliveryPermit,
        limits: JournalLimits,
        now_ms: i64,
    ) -> Result<Self, JournalError> {
        permit
            .validate_against(&permit.authority, now_ms)
            .map_err(|reason| match reason {
                JournalRejectReason::Expired => JournalError::Expired,
                JournalRejectReason::Foreign => JournalError::Foreign,
                _ => JournalError::InvalidEnvelope,
            })?;
        limits.validate()?;
        let mut store = KernelStore::open(path).map_err(|_| JournalError::Store)?;
        let record = permit.authority.to_record(permit.issued_at_ms)?;
        let store_id = store
            .semantic_journal_ensure_session(&record)
            .map_err(|_| JournalError::Store)?;
        let (next_sequence, _) = store
            .semantic_journal_high_water(&record.digest)
            .map_err(|_| JournalError::Store)?;
        Ok(Self {
            store,
            authority: permit.authority,
            authority_digest: record.digest,
            instance_secret: journal_key(&permit.authority, &store_id),
            store_id,
            limits,
            next_sequence,
            ingest_steps: 0,
        })
    }

    fn propose(
        &self,
        permit: AdapterDeliveryPermit,
        bytes: &[u8],
        now_ms: i64,
    ) -> Result<JournalDraft, JournalIngestOutcome> {
        permit
            .validate_against(&self.authority, now_ms)
            .map_err(JournalIngestOutcome::Rejected)?;
        match parse_journal_content(bytes) {
            Ok(content) => self.draft_from_content(&permit, content, bytes, now_ms),
            Err(JournalError::ForgedIdentity) => Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::ForgedIdentity,
            )),
            Err(
                JournalError::Oversized
                | JournalError::NestingTooDeep
                | JournalError::TooLong
                | JournalError::DuplicateKey,
            ) => Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::InvalidEnvelope,
            )),
            Err(_) => {
                let (schema_version, source_type, provider_event_id) = diagnostic_metadata(bytes);
                Ok(self.diagnostic_draft(
                    &permit,
                    bytes,
                    now_ms,
                    schema_version,
                    source_type,
                    provider_event_id,
                )?)
            }
        }
    }

    fn commit(&mut self, draft: JournalDraft) -> JournalIngestOutcome {
        if !draft.verify(
            &self.instance_secret,
            &self.authority_digest,
            &self.authority,
            &self.store_id,
        ) {
            return JournalIngestOutcome::Rejected(JournalRejectReason::Foreign);
        }
        if let Err(outcome) = self.charge_work() {
            return outcome;
        }
        let mut event = draft.event;
        let row = match persist_row(&event) {
            Ok(Some(row)) => row,
            Ok(None) => return JournalIngestOutcome::IgnoredNeverPersist,
            Err(_) => return JournalIngestOutcome::NeedsResync,
        };
        let authority = self.authority;
        match self.store.semantic_journal_write_fact(
            &self.authority_digest,
            event.delivery_id.as_str(),
            event
                .provider_event_id
                .as_ref()
                .map(ProviderEventId::as_str),
            event.payload_hash,
            row,
            self.limits.max_events,
            self.limits.max_dedupe_keys,
            |row| validate_restored_row(authority, row),
        ) {
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::Inserted { sequence }) => {
                event.sequence = sequence;
                self.next_sequence = sequence.saturating_add(1);
                if event.kind == JournalSemanticKind::UnknownProviderEvent {
                    JournalIngestOutcome::Quarantined(event)
                } else {
                    JournalIngestOutcome::Accepted(event)
                }
            }
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::Duplicate {
                event_id,
                content_hash,
            }) => classify_store_repeat(event_id, content_hash, event.payload_hash),
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::Conflict {
                event_id,
                content_hash,
            }) => classify_store_repeat(event_id, content_hash, event.payload_hash),
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::KeyConflict { .. }) => {
                JournalIngestOutcome::NeedsResync
            }
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::EventCapacity) => {
                JournalIngestOutcome::Backpressure(JournalBackpressure::EventCapacity)
            }
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::DedupeCapacity) => {
                JournalIngestOutcome::Backpressure(JournalBackpressure::DedupeCapacity)
            }
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::SequenceOverflow) => {
                JournalIngestOutcome::Rejected(JournalRejectReason::SequenceOverflow)
            }
            Ok(crate::kernel::semantic_journal::SemanticJournalWrite::TimestampRegression) => {
                JournalIngestOutcome::Rejected(JournalRejectReason::TimestampRegression)
            }
            Err(_) => JournalIngestOutcome::NeedsResync,
        }
    }

    pub fn ingest(
        &mut self,
        permit: AdapterDeliveryPermit,
        bytes: &[u8],
        now_ms: i64,
    ) -> JournalIngestOutcome {
        match self.propose(permit, bytes, now_ms) {
            Ok(draft) => self.commit(draft),
            Err(outcome) => outcome,
        }
    }

    pub fn ingest_until(
        &mut self,
        permit: AdapterDeliveryPermit,
        bytes: &[u8],
        now_ms: i64,
        deadline: Instant,
    ) -> JournalIngestOutcome {
        if Instant::now() >= deadline {
            return JournalIngestOutcome::NeedsResync;
        }
        self.ingest(permit, bytes, now_ms)
    }

    pub fn persist_page(
        &self,
        after_sequence: u64,
        high_water: Option<u64>,
        limits: PageLimits,
    ) -> Result<SemanticJournalPage, JournalIngestOutcome> {
        self.page_from_store(after_sequence, high_water, limits, true)
    }

    pub fn projected_page(
        &self,
        after_sequence: u64,
        high_water: Option<u64>,
        limits: PageLimits,
    ) -> Result<SemanticJournalPage, JournalIngestOutcome> {
        self.page_from_store(after_sequence, high_water, limits, false)
    }

    pub fn event_at(&self, sequence: u64) -> Result<Option<JournalEvent>, JournalIngestOutcome> {
        let sequence = i64::try_from(sequence).map_err(|_| JournalIngestOutcome::NeedsResync)?;
        match self
            .store
            .semantic_journal_load_fact(&self.authority_digest, sequence, |row| {
                validate_restored_row(self.authority, row)
            }) {
            Ok(Some(row)) => restore_event(&self.authority, row)
                .map(Some)
                .map_err(|_| JournalIngestOutcome::NeedsResync),
            Ok(None) => Ok(None),
            Err(_) => Err(JournalIngestOutcome::NeedsResync),
        }
    }

    pub fn retained_len(&self) -> Result<usize, JournalIngestOutcome> {
        self.store
            .semantic_journal_validate(&self.authority_digest, |row| {
                validate_restored_row(self.authority, row)
            })
            .and_then(|(count, _)| usize::try_from(count).map_err(|_| StoreError::Corruption))
            .map_err(|_| JournalIngestOutcome::NeedsResync)
    }

    #[cfg(test)]
    pub(crate) fn debug_set_next_sequence(&mut self, next_sequence: u64) {
        self.next_sequence = next_sequence;
    }

    #[cfg(test)]
    pub(crate) fn debug_delete_sequence(&mut self, sequence: u64) {
        let sequence = i64::try_from(sequence).expect("sequence fits");
        self.store
            .debug_delete_semantic_journal_fact(&self.authority_digest, sequence)
            .expect("delete sequence");
    }

    #[cfg(test)]
    pub(crate) fn debug_zero_event_id(&mut self, sequence: u64) {
        let sequence = i64::try_from(sequence).expect("sequence fits");
        self.store
            .debug_zero_semantic_journal_event_id(&self.authority_digest, sequence)
            .expect("zero event id");
    }

    fn charge_work(&mut self) -> Result<(), JournalIngestOutcome> {
        self.ingest_steps = self
            .ingest_steps
            .checked_add(1)
            .ok_or(JournalIngestOutcome::NeedsResync)?;
        if self.ingest_steps > self.limits.max_ingest_steps {
            return Err(JournalIngestOutcome::NeedsResync);
        }
        Ok(())
    }

    fn page_from_store(
        &self,
        after_sequence: u64,
        requested_high_water: Option<u64>,
        limits: PageLimits,
        persist_only: bool,
    ) -> Result<SemanticJournalPage, JournalIngestOutcome> {
        limits
            .validate()
            .map_err(|_| JournalIngestOutcome::Backpressure(JournalBackpressure::PageBudget))?;
        let after = i64::try_from(after_sequence).unwrap_or(i64::MAX);
        let codec = MessagePackCodec::from_limits(FrameLimits::v1_default())
            .map_err(|_| JournalIngestOutcome::Backpressure(JournalBackpressure::PageBudget))?;
        let mut expected = after_sequence.saturating_add(1);
        let mut candidates = Vec::with_capacity(usize::try_from(limits.max_items).unwrap_or(0));
        let mut overflow_sequence = None;
        let mut scanned_through = after_sequence;
        let mut stream_error = None;
        let high_water = self
            .store
            .semantic_journal_stream_page(
                &self.authority_digest,
                after,
                requested_high_water,
                |row| validate_restored_row(self.authority, row),
                |high_water, row| {
                    let sequence = match u64::try_from(row.sequence) {
                        Ok(sequence) => sequence,
                        Err(_) => {
                            stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    if sequence != expected || sequence > high_water {
                        stream_error = Some(JournalIngestOutcome::NeedsResync);
                        return Ok(false);
                    }
                    expected = expected.saturating_add(1);
                    scanned_through = sequence;
                    if candidates.len() as u32 >= limits.max_items {
                        overflow_sequence = Some(sequence);
                        return Ok(false);
                    }
                    // The persisted body contains the typed payload plus its
                    // envelope. A first body already at the complete page
                    // budget cannot fit after page/fact metadata is added.
                    // Reject it before restoring/materializing a candidate;
                    // the bounded serializer below remains authoritative for
                    // all rows that pass this cheap preflight.
                    if candidates.is_empty()
                        && row.payload.len()
                            >= usize::try_from(limits.max_encoded_bytes).unwrap_or(usize::MAX)
                    {
                        overflow_sequence = Some(sequence);
                        return Ok(false);
                    }
                    let event = match restore_event(&self.authority, row) {
                        Ok(event) => event,
                        Err(_) => {
                            stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    if !persist_only && event.visibility == JournalVisibility::RuntimeOnly {
                        return Ok(true);
                    }
                    let candidate = event.to_snapshot_fact();
                    candidates.push(candidate);
                    let next_sequence = (event.sequence < high_water).then_some(event.sequence + 1);
                    let mut page = SemanticJournalPage {
                        after_sequence,
                        through_sequence: event.sequence,
                        high_water,
                        encoded_bytes: 0,
                        next_sequence,
                        facts: std::mem::take(&mut candidates),
                    };
                    match page_encoded_len(&codec, &mut page, limits.max_encoded_bytes) {
                        Ok(encoded_bytes) => {
                            page.encoded_bytes = encoded_bytes;
                            candidates = page.facts;
                            Ok(true)
                        }
                        Err(PageMeasureError::TooLarge) => {
                            let removed = page.facts.pop().expect("candidate just pushed");
                            candidates = page.facts;
                            overflow_sequence = Some(removed.sequence);
                            Ok(false)
                        }
                        Err(PageMeasureError::Encode) => {
                            candidates = page.facts;
                            stream_error = Some(JournalIngestOutcome::Backpressure(
                                JournalBackpressure::PageBudget,
                            ));
                            Ok(false)
                        }
                    }
                },
            )
            .map_err(|_| JournalIngestOutcome::NeedsResync)?;
        if let Some(error) = stream_error {
            return Err(error);
        }
        if after_sequence > high_water {
            return Err(JournalIngestOutcome::NeedsResync);
        }
        let through_sequence = candidates
            .last()
            .map(|fact| fact.sequence)
            // An oversized first candidate was fetched and decoded only to
            // establish that it cannot fit. It was not returned, so the page
            // must leave the durable cursor at `after_sequence` and expose
            // the candidate again through `next_sequence`.
            .or_else(|| overflow_sequence.is_none().then_some(scanned_through))
            .unwrap_or(after_sequence);
        let next_sequence = overflow_sequence
            .or_else(|| (scanned_through < high_water).then_some(scanned_through + 1));
        let mut page = SemanticJournalPage {
            after_sequence,
            through_sequence,
            high_water,
            encoded_bytes: 0,
            next_sequence,
            facts: candidates,
        };
        let encoded_bytes =
            page_encoded_len(&codec, &mut page, limits.max_encoded_bytes).map_err(|error| {
                match error {
                    PageMeasureError::TooLarge | PageMeasureError::Encode => {
                        JournalIngestOutcome::Backpressure(JournalBackpressure::PageBudget)
                    }
                }
            })?;
        page.encoded_bytes = encoded_bytes;
        Ok(page)
    }

    fn draft_from_content(
        &self,
        binding: &AdapterDeliveryPermit,
        content: NativeJournalContent,
        raw: &[u8],
        now_ms: i64,
    ) -> Result<JournalDraft, JournalIngestOutcome> {
        if content.occurred_at_ms < 0 {
            return Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::TimestampOverflow,
            ));
        }
        if matches!(content.payload, NativeJournalPayload::TerminalBytes) {
            return Err(JournalIngestOutcome::IgnoredTerminal);
        }
        if self.next_sequence == 0 {
            return Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::SequenceOverflow,
            ));
        }
        let classified = classify_payload(&content);
        if matches!(classified, Classified::Terminal) {
            return Err(JournalIngestOutcome::IgnoredTerminal);
        }
        let (kind, visibility, redaction_class, privacy_class, text, unknown, payload) =
            match classified {
                Classified::Unknown => (
                    JournalSemanticKind::UnknownProviderEvent,
                    JournalVisibility::Diagnostic,
                    JournalRedactionClass::MetadataOnly,
                    PrivacyClass::LocalOnly,
                    None,
                    Some(UnknownProviderEvent {
                        provider: binding.authority.provider,
                        source_type: content.source_type.clone(),
                        schema_version: content.schema_version,
                        diagnostic_ref: diagnostic_ref(raw),
                    }),
                    SemanticJournalPayload::Unknown {
                        provider: provider_kind_sql(binding.authority.provider).to_string(),
                        source_type: content.source_type.clone(),
                        schema_version: content.schema_version,
                        diagnostic_ref: diagnostic_ref(raw),
                    },
                ),
                Classified::Semantic {
                    kind,
                    text,
                    redaction_class,
                    privacy_class,
                    payload,
                } => (
                    kind,
                    JournalVisibility::Semantic,
                    redaction_class,
                    privacy_class,
                    text,
                    None,
                    payload,
                ),
                Classified::Terminal => unreachable!("terminal handled above"),
            };
        if self.next_sequence == u64::MAX {
            return Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::SequenceOverflow,
            ));
        }
        let event = JournalEvent {
            id: EventId::new(),
            schema_version: JOURNAL_SCHEMA_VERSION,
            provider: binding.authority.provider,
            provider_event_id: content.provider_event_id,
            delivery_id: binding.delivery_id.clone(),
            task_id: binding.authority.task_id,
            agent_session_id: binding.authority.agent_session_id,
            resource_id: binding.authority.resource_id,
            runtime_generation: binding.authority.runtime_generation,
            action_epoch: binding.authority.action_epoch,
            sequence: self.next_sequence,
            kind,
            occurred_at_ms: content.occurred_at_ms,
            ingested_at_ms: now_ms,
            visibility,
            redaction_class,
            privacy_class,
            text,
            extensions: content.extensions,
            unknown,
            payload,
            payload_hash: Sha256::digest(raw).into(),
        };
        Ok(JournalDraft::sealed(
            event,
            &self.instance_secret,
            &self.authority,
            self.authority_digest,
            binding.nonce,
            binding.issued_at_ms,
            binding.expires_at_ms,
            self.store_id,
        ))
    }

    fn diagnostic_draft(
        &self,
        binding: &AdapterDeliveryPermit,
        raw: &[u8],
        now_ms: i64,
        schema_version: u32,
        source_type: String,
        provider_event_id: Option<String>,
    ) -> Result<JournalDraft, JournalIngestOutcome> {
        if self.next_sequence == u64::MAX {
            return Err(JournalIngestOutcome::Rejected(
                JournalRejectReason::SequenceOverflow,
            ));
        }
        Ok(JournalDraft::sealed(
            JournalEvent {
                id: EventId::new(),
                schema_version,
                provider: binding.authority.provider,
                provider_event_id: provider_event_id
                    .map(ProviderEventId::new)
                    .transpose()
                    .map_err(|_| {
                        JournalIngestOutcome::Rejected(JournalRejectReason::InvalidEnvelope)
                    })?,
                delivery_id: binding.delivery_id.clone(),
                task_id: binding.authority.task_id,
                agent_session_id: binding.authority.agent_session_id,
                resource_id: binding.authority.resource_id,
                runtime_generation: binding.authority.runtime_generation,
                action_epoch: binding.authority.action_epoch,
                sequence: self.next_sequence,
                kind: JournalSemanticKind::UnknownProviderEvent,
                occurred_at_ms: now_ms,
                ingested_at_ms: now_ms,
                visibility: JournalVisibility::Diagnostic,
                redaction_class: JournalRedactionClass::MetadataOnly,
                privacy_class: PrivacyClass::LocalOnly,
                text: None,
                extensions: BTreeMap::new(),
                unknown: Some(UnknownProviderEvent {
                    provider: binding.authority.provider,
                    source_type: source_type.clone(),
                    schema_version,
                    diagnostic_ref: diagnostic_ref(raw),
                }),
                payload: SemanticJournalPayload::Unknown {
                    provider: provider_kind_sql(binding.authority.provider).to_string(),
                    source_type,
                    schema_version,
                    diagnostic_ref: diagnostic_ref(raw),
                },
                payload_hash: Sha256::digest(raw).into(),
            },
            &self.instance_secret,
            &self.authority,
            self.authority_digest,
            binding.nonce,
            binding.issued_at_ms,
            binding.expires_at_ms,
            self.store_id,
        ))
    }
}

fn classify_store_repeat(
    existing_id: [u8; 16],
    existing_hash: [u8; 32],
    new_hash: [u8; 32],
) -> JournalIngestOutcome {
    let existing_id = match EventId::from_bytes(existing_id) {
        Ok(id) => id,
        Err(_) => return JournalIngestOutcome::NeedsResync,
    };
    if existing_hash == new_hash {
        JournalIngestOutcome::Duplicate { existing_id }
    } else {
        JournalIngestOutcome::Conflict { existing_id }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageMeasureError {
    TooLarge,
    Encode,
}

fn page_encoded_len(
    codec: &MessagePackCodec,
    page: &mut SemanticJournalPage,
    maximum: u32,
) -> Result<u32, PageMeasureError> {
    let mut encoded_bytes = 0_u32;
    for _ in 0..8 {
        page.encoded_bytes = encoded_bytes;
        let measured = match codec.encoded_len_bounded(page, maximum) {
            Ok(measured) => measured,
            Err(MessagePackError::Oversized { .. }) => return Err(PageMeasureError::TooLarge),
            Err(_) => return Err(PageMeasureError::Encode),
        };
        if measured == encoded_bytes {
            return Ok(measured);
        }
        encoded_bytes = measured;
    }
    Err(PageMeasureError::Encode)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedJournalBody {
    text: Option<String>,
    extensions: BTreeMap<String, String>,
    unknown_source_type: Option<String>,
    unknown_schema_version: Option<u32>,
    unknown_diagnostic_ref: Option<String>,
    provider_event_id: Option<String>,
    payload: SemanticJournalPayload,
}

fn persist_row(event: &JournalEvent) -> Result<Option<SemanticJournalFactRow>, JournalError> {
    if event.redaction_class == JournalRedactionClass::NeverPersist {
        return Ok(None);
    }
    let mut body = PersistedJournalBody {
        text: event.text.clone(),
        extensions: event.extensions.clone(),
        unknown_source_type: event.unknown.as_ref().map(|item| item.source_type.clone()),
        unknown_schema_version: event.unknown.as_ref().map(|item| item.schema_version),
        unknown_diagnostic_ref: event
            .unknown
            .as_ref()
            .map(|item| item.diagnostic_ref.clone()),
        provider_event_id: event
            .provider_event_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        payload: event.payload.clone(),
    };
    if matches!(
        event.redaction_class,
        JournalRedactionClass::NeverPersist
            | JournalRedactionClass::MetadataOnly
            | JournalRedactionClass::RedactOnPersist
    ) {
        body.text = None;
    }
    let payload = rmp_serde::to_vec_named(&body).map_err(|_| JournalError::Store)?;
    Ok(Some(SemanticJournalFactRow {
        sequence: 0,
        event_id: *event.id.as_bytes(),
        delivery_id: event.delivery_id.as_str().to_owned(),
        provider_event_id: event
            .provider_event_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        content_hash: event.payload_hash,
        kind: event.kind.as_str().to_string(),
        visibility: event.visibility.as_str().to_string(),
        privacy_class: match event.privacy_class {
            PrivacyClass::LocalOnly => "local_only".into(),
            PrivacyClass::Shareable => "shareable".into(),
        },
        redaction_class: event.redaction_class.as_str().to_string(),
        occurred_at_ms: event.occurred_at_ms,
        ingested_at_ms: event.ingested_at_ms,
        schema_version: i64::from(event.schema_version),
        payload,
    }))
}

fn restore_event(
    authority: &JournalSessionAuthority,
    row: SemanticJournalFactRow,
) -> Result<JournalEvent, JournalError> {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default())
        .map_err(|_| JournalError::Store)?;
    let mut body: PersistedJournalBody = codec
        .decode(&row.payload)
        .map_err(|_| JournalError::Store)?;
    let schema_version =
        u32::try_from(row.schema_version).map_err(|_| JournalError::InvalidEnvelope)?;
    let sequence = u64::try_from(row.sequence).map_err(|_| JournalError::InvalidEnvelope)?;
    if sequence == 0 || row.occurred_at_ms < 0 || row.ingested_at_ms < 0 {
        return Err(JournalError::InvalidEnvelope);
    }
    let kind = parse_kind(&row.kind)?;
    if schema_version != JOURNAL_SCHEMA_VERSION && kind != JournalSemanticKind::UnknownProviderEvent
    {
        return Err(JournalError::UnsupportedSchemaVersion);
    }
    let visibility = parse_visibility(&row.visibility)?;
    let redaction_class = parse_redaction(&row.redaction_class)?;
    let privacy_class = match row.privacy_class.as_str() {
        "local_only" => PrivacyClass::LocalOnly,
        "shareable" => PrivacyClass::Shareable,
        _ => return Err(JournalError::InvalidEnvelope),
    };
    let row_provider_event_id = row.provider_event_id.clone();
    if body.provider_event_id != row_provider_event_id {
        return Err(JournalError::InvalidEnvelope);
    }
    if redaction_class == JournalRedactionClass::NeverPersist {
        return Err(JournalError::InvalidEnvelope);
    }
    validate_semantic_payload(&kind, &body.payload)?;
    if matches!(
        redaction_class,
        JournalRedactionClass::NeverPersist
            | JournalRedactionClass::MetadataOnly
            | JournalRedactionClass::RedactOnPersist
    ) {
        if body.text.is_some() && matches!(redaction_class, JournalRedactionClass::NeverPersist) {
            return Err(JournalError::InvalidEnvelope);
        }
        body.text = None;
    }
    if let Some(text) = &body.text {
        reject_display_bound(text, MAX_JOURNAL_TEXT_BYTES)?;
    }
    if body.extensions.len() > MAX_EXTENSION_ENTRIES {
        return Err(JournalError::TooManyExtensions);
    }
    for (key, value) in &body.extensions {
        if !EXTENSION_ALLOWLIST.contains(&key.as_str()) {
            return Err(JournalError::InvalidEnvelope);
        }
        reject_display_bound(key, MAX_EXTENSION_KEY_BYTES)?;
        reject_display_bound(value, MAX_EXTENSION_VALUE_BYTES)?;
    }
    if let Some(source_type) = &body.unknown_source_type {
        reject_display_bound(source_type, MAX_SOURCE_TYPE_BYTES)?;
    }
    if let Some(diagnostic_ref) = &body.unknown_diagnostic_ref {
        if diagnostic_ref.len() != 64
            || !diagnostic_ref
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(JournalError::InvalidEnvelope);
        }
    }
    match kind {
        JournalSemanticKind::UnknownProviderEvent => {
            let Some(unknown_schema_version) = body.unknown_schema_version else {
                return Err(JournalError::InvalidEnvelope);
            };
            let SemanticJournalPayload::Unknown {
                provider,
                source_type,
                schema_version: payload_schema_version,
                diagnostic_ref,
            } = &body.payload
            else {
                return Err(JournalError::InvalidEnvelope);
            };
            if provider != provider_kind_sql(authority.provider)
                || *payload_schema_version != unknown_schema_version
                || *payload_schema_version != schema_version
                || body.unknown_source_type.as_deref() != Some(source_type.as_str())
                || body.unknown_diagnostic_ref.as_deref() != Some(diagnostic_ref.as_str())
            {
                return Err(JournalError::InvalidEnvelope);
            }
            if visibility != JournalVisibility::Diagnostic {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        _ => {
            if body.unknown_source_type.is_some()
                || body.unknown_schema_version.is_some()
                || body.unknown_diagnostic_ref.is_some()
            {
                return Err(JournalError::InvalidEnvelope);
            }
        }
    }
    Ok(JournalEvent {
        id: EventId::from_bytes(row.event_id).map_err(|_| JournalError::Store)?,
        schema_version,
        provider: authority.provider,
        provider_event_id: body
            .provider_event_id
            .map(ProviderEventId::new)
            .transpose()?,
        delivery_id: RelayDeliveryId::new(row.delivery_id)?,
        task_id: authority.task_id,
        agent_session_id: authority.agent_session_id,
        resource_id: authority.resource_id,
        runtime_generation: authority.runtime_generation,
        action_epoch: authority.action_epoch,
        sequence,
        kind,
        occurred_at_ms: row.occurred_at_ms,
        ingested_at_ms: row.ingested_at_ms,
        visibility,
        redaction_class,
        privacy_class,
        text: body.text,
        extensions: body.extensions,
        unknown: body
            .unknown_source_type
            .map(|source_type| UnknownProviderEvent {
                provider: authority.provider,
                source_type,
                schema_version: body.unknown_schema_version.unwrap_or(schema_version),
                diagnostic_ref: body.unknown_diagnostic_ref.unwrap_or_default(),
            }),
        payload: body.payload,
        payload_hash: row.content_hash,
    })
}

fn validate_restored_row(
    authority: JournalSessionAuthority,
    row: &SemanticJournalFactRow,
) -> Result<(), StoreError> {
    restore_event(&authority, row.clone())
        .map(|_| ())
        .map_err(|_| StoreError::Corruption)
}

fn parse_kind(value: &str) -> Result<JournalSemanticKind, JournalError> {
    Ok(match value {
        "user_message" => JournalSemanticKind::UserMessage,
        "assistant_text" => JournalSemanticKind::AssistantText,
        "reasoning_summary" => JournalSemanticKind::ReasoningSummary,
        "tool_call" => JournalSemanticKind::ToolCall,
        "tool_result" => JournalSemanticKind::ToolResult,
        "approval_request" => JournalSemanticKind::ApprovalRequest,
        "approval_result" => JournalSemanticKind::ApprovalResult,
        "question" => JournalSemanticKind::Question,
        "plan_step" => JournalSemanticKind::PlanStep,
        "usage_observation" => JournalSemanticKind::UsageObservation,
        "error" => JournalSemanticKind::Error,
        "turn_state" => JournalSemanticKind::TurnState,
        "session_state" => JournalSemanticKind::SessionState,
        "artifact_reference" => JournalSemanticKind::ArtifactReference,
        "unknown_provider_event" => JournalSemanticKind::UnknownProviderEvent,
        _ => return Err(JournalError::InvalidEnvelope),
    })
}

fn validate_semantic_payload(
    kind: &JournalSemanticKind,
    payload: &SemanticJournalPayload,
) -> Result<(), JournalError> {
    let matches_kind = matches!(
        (kind, payload),
        (
            JournalSemanticKind::UserMessage,
            SemanticJournalPayload::UserMessage { .. }
        ) | (
            JournalSemanticKind::AssistantText,
            SemanticJournalPayload::AssistantText { .. }
        ) | (
            JournalSemanticKind::ReasoningSummary,
            SemanticJournalPayload::ReasoningSummary { .. }
        ) | (
            JournalSemanticKind::ToolCall,
            SemanticJournalPayload::ToolCall { .. }
        ) | (
            JournalSemanticKind::ToolResult,
            SemanticJournalPayload::ToolResult { .. }
        ) | (
            JournalSemanticKind::ApprovalRequest,
            SemanticJournalPayload::ApprovalRequest { .. }
        ) | (
            JournalSemanticKind::ApprovalResult,
            SemanticJournalPayload::ApprovalResult { .. }
        ) | (
            JournalSemanticKind::Question,
            SemanticJournalPayload::Question { .. }
        ) | (
            JournalSemanticKind::PlanStep,
            SemanticJournalPayload::PlanStep { .. }
        ) | (
            JournalSemanticKind::UsageObservation,
            SemanticJournalPayload::UsageObservation { .. }
        ) | (
            JournalSemanticKind::Error,
            SemanticJournalPayload::Error { .. }
        ) | (
            JournalSemanticKind::TurnState,
            SemanticJournalPayload::TurnState { .. }
        ) | (
            JournalSemanticKind::SessionState,
            SemanticJournalPayload::SessionState { .. }
        ) | (
            JournalSemanticKind::ArtifactReference,
            SemanticJournalPayload::ArtifactReference { .. }
        ) | (
            JournalSemanticKind::UnknownProviderEvent,
            SemanticJournalPayload::Unknown { .. }
        )
    );
    if !matches_kind {
        return Err(JournalError::InvalidEnvelope);
    }
    match payload {
        SemanticJournalPayload::UserMessage { text }
        | SemanticJournalPayload::AssistantText { text }
        | SemanticJournalPayload::ReasoningSummary { text } => {
            reject_display_bound(text, MAX_JOURNAL_TEXT_BYTES)?;
        }
        SemanticJournalPayload::ToolCall { tool_name, call_id } => {
            reject_display_bound(tool_name, MAX_TOOL_NAME_BYTES)?;
            reject_display_bound(call_id, MAX_CALL_ID_BYTES)?;
        }
        SemanticJournalPayload::ToolResult { call_id, status } => {
            reject_display_bound(call_id, MAX_CALL_ID_BYTES)?;
            reject_display_bound(status, MAX_SOURCE_TYPE_BYTES)?;
        }
        SemanticJournalPayload::ApprovalRequest {
            request_id,
            summary,
        } => {
            reject_display_bound(request_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(summary, MAX_JOURNAL_TEXT_BYTES)?;
        }
        SemanticJournalPayload::ApprovalResult {
            request_id,
            decision,
        } => {
            reject_display_bound(request_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(decision, MAX_SOURCE_TYPE_BYTES)?;
        }
        SemanticJournalPayload::Question {
            question_id,
            prompt,
            options,
        } => {
            reject_display_bound(question_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(prompt, MAX_JOURNAL_TEXT_BYTES)?;
            if options.len() > MAX_QUESTION_OPTIONS {
                return Err(JournalError::TooLong);
            }
            for option in options {
                reject_display_bound(option, MAX_EXTENSION_VALUE_BYTES)?;
            }
        }
        SemanticJournalPayload::PlanStep {
            step_id,
            title,
            status,
        } => {
            reject_display_bound(step_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(title, MAX_JOURNAL_TEXT_BYTES)?;
            reject_display_bound(status, MAX_SOURCE_TYPE_BYTES)?;
        }
        SemanticJournalPayload::UsageObservation { remaining_percent } => {
            if remaining_percent.is_some_and(|percent| percent > 100) {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        SemanticJournalPayload::Error { code, message } => {
            reject_display_bound(code, MAX_SOURCE_TYPE_BYTES)?;
            reject_display_bound(message, MAX_JOURNAL_TEXT_BYTES)?;
        }
        SemanticJournalPayload::TurnState { state } => {
            reject_display_bound(state, MAX_SOURCE_TYPE_BYTES)?;
            if !matches!(state.as_str(), "started" | "completed" | "failed") {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        SemanticJournalPayload::SessionState { state } => {
            reject_display_bound(state, MAX_SOURCE_TYPE_BYTES)?;
            if !matches!(state.as_str(), "open" | "closed") {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        SemanticJournalPayload::ArtifactReference { label } => {
            reject_display_bound(label, MAX_JOURNAL_TEXT_BYTES)?;
        }
        SemanticJournalPayload::Unknown {
            provider,
            source_type,
            schema_version,
            diagnostic_ref,
        } => {
            reject_display_bound(provider, MAX_SOURCE_TYPE_BYTES)?;
            reject_display_bound(source_type, MAX_SOURCE_TYPE_BYTES)?;
            if *schema_version == 0
                || diagnostic_ref.len() != 64
                || !diagnostic_ref
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(JournalError::InvalidEnvelope);
            }
        }
    }
    Ok(())
}

fn parse_visibility(value: &str) -> Result<JournalVisibility, JournalError> {
    Ok(match value {
        "semantic" => JournalVisibility::Semantic,
        "diagnostic" => JournalVisibility::Diagnostic,
        "runtime_only" => JournalVisibility::RuntimeOnly,
        _ => return Err(JournalError::InvalidEnvelope),
    })
}

fn parse_redaction(value: &str) -> Result<JournalRedactionClass, JournalError> {
    Ok(match value {
        "persistable" => JournalRedactionClass::Persistable,
        "persistable_local_only" => JournalRedactionClass::PersistableLocalOnly,
        "redact_on_persist" => JournalRedactionClass::RedactOnPersist,
        "metadata_only" => JournalRedactionClass::MetadataOnly,
        "never_persist" => JournalRedactionClass::NeverPersist,
        _ => return Err(JournalError::InvalidEnvelope),
    })
}

enum Classified {
    Terminal,
    Unknown,
    Semantic {
        kind: JournalSemanticKind,
        text: Option<String>,
        redaction_class: JournalRedactionClass,
        privacy_class: PrivacyClass,
        payload: SemanticJournalPayload,
    },
}

fn classify_payload(content: &NativeJournalContent) -> Classified {
    match &content.payload {
        NativeJournalPayload::TerminalBytes => Classified::Terminal,
        NativeJournalPayload::Unknown => Classified::Unknown,
        NativeJournalPayload::UserMessage { text }
        | NativeJournalPayload::AssistantText { text }
        | NativeJournalPayload::ReasoningSummary { text } => {
            if reject_display_bound(text, MAX_JOURNAL_TEXT_BYTES).is_err() {
                Classified::Unknown
            } else {
                let kind = match &content.payload {
                    NativeJournalPayload::UserMessage { .. } => JournalSemanticKind::UserMessage,
                    NativeJournalPayload::AssistantText { .. } => {
                        JournalSemanticKind::AssistantText
                    }
                    _ => JournalSemanticKind::ReasoningSummary,
                };
                Classified::Semantic {
                    kind,
                    text: Some(text.clone()),
                    redaction_class: JournalRedactionClass::PersistableLocalOnly,
                    privacy_class: PrivacyClass::LocalOnly,
                    payload: match &content.payload {
                        NativeJournalPayload::UserMessage { .. } => {
                            SemanticJournalPayload::UserMessage { text: text.clone() }
                        }
                        NativeJournalPayload::AssistantText { .. } => {
                            SemanticJournalPayload::AssistantText { text: text.clone() }
                        }
                        NativeJournalPayload::ReasoningSummary { .. } => {
                            SemanticJournalPayload::ReasoningSummary { text: text.clone() }
                        }
                        _ => unreachable!("text payload kind already classified"),
                    },
                }
            }
        }
        NativeJournalPayload::Question { prompt, .. } => {
            if reject_display_bound(prompt, MAX_JOURNAL_TEXT_BYTES).is_err() {
                Classified::Unknown
            } else {
                Classified::Semantic {
                    kind: JournalSemanticKind::Question,
                    text: None,
                    redaction_class: JournalRedactionClass::PersistableLocalOnly,
                    privacy_class: PrivacyClass::LocalOnly,
                    payload: SemanticJournalPayload::Question {
                        question_id: match &content.payload {
                            NativeJournalPayload::Question { question_id, .. } => {
                                question_id.clone()
                            }
                            _ => unreachable!("question payload kind already classified"),
                        },
                        prompt: prompt.clone(),
                        options: match &content.payload {
                            NativeJournalPayload::Question { options, .. } => options.clone(),
                            _ => unreachable!("question payload kind already classified"),
                        },
                    },
                }
            }
        }
        NativeJournalPayload::ApprovalRequest { summary, .. }
        | NativeJournalPayload::Error {
            message: summary, ..
        } => {
            if summary.len() > MAX_JOURNAL_TEXT_BYTES {
                Classified::Unknown
            } else {
                let kind = match &content.payload {
                    NativeJournalPayload::ApprovalRequest { .. } => {
                        JournalSemanticKind::ApprovalRequest
                    }
                    _ => JournalSemanticKind::Error,
                };
                Classified::Semantic {
                    kind,
                    text: None,
                    redaction_class: JournalRedactionClass::PersistableLocalOnly,
                    privacy_class: PrivacyClass::LocalOnly,
                    payload: match &content.payload {
                        NativeJournalPayload::ApprovalRequest {
                            request_id,
                            summary,
                        } => SemanticJournalPayload::ApprovalRequest {
                            request_id: request_id.clone(),
                            summary: summary.clone(),
                        },
                        NativeJournalPayload::Error { code, message } => {
                            SemanticJournalPayload::Error {
                                code: code.clone(),
                                message: message.clone(),
                            }
                        }
                        _ => unreachable!("summary payload kind already classified"),
                    },
                }
            }
        }
        NativeJournalPayload::ApprovalResult { .. } => Classified::Semantic {
            kind: JournalSemanticKind::ApprovalResult,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::LocalOnly,
            payload: match &content.payload {
                NativeJournalPayload::ApprovalResult {
                    request_id,
                    decision,
                } => SemanticJournalPayload::ApprovalResult {
                    request_id: request_id.clone(),
                    decision: decision.clone(),
                },
                _ => unreachable!("approval result kind already classified"),
            },
        },
        NativeJournalPayload::ToolCall { .. } => Classified::Semantic {
            kind: JournalSemanticKind::ToolCall,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::ToolCall { tool_name, call_id } => {
                    SemanticJournalPayload::ToolCall {
                        tool_name: tool_name.clone(),
                        call_id: call_id.clone(),
                    }
                }
                _ => unreachable!("tool call kind already classified"),
            },
        },
        NativeJournalPayload::ToolResult { .. } => Classified::Semantic {
            kind: JournalSemanticKind::ToolResult,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::ToolResult { call_id, status } => {
                    SemanticJournalPayload::ToolResult {
                        call_id: call_id.clone(),
                        status: status.clone(),
                    }
                }
                _ => unreachable!("tool result kind already classified"),
            },
        },
        NativeJournalPayload::PlanStep { .. } => Classified::Semantic {
            kind: JournalSemanticKind::PlanStep,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::PlanStep {
                    step_id,
                    title,
                    status,
                } => SemanticJournalPayload::PlanStep {
                    step_id: step_id.clone(),
                    title: title.clone(),
                    status: status.clone(),
                },
                _ => unreachable!("plan step kind already classified"),
            },
        },
        NativeJournalPayload::UsageObservation { remaining_percent } => {
            if remaining_percent.is_some_and(|percent| percent > 100) {
                Classified::Unknown
            } else {
                Classified::Semantic {
                    kind: JournalSemanticKind::UsageObservation,
                    text: None,
                    redaction_class: JournalRedactionClass::MetadataOnly,
                    privacy_class: PrivacyClass::Shareable,
                    payload: SemanticJournalPayload::UsageObservation {
                        remaining_percent: *remaining_percent,
                    },
                }
            }
        }
        NativeJournalPayload::TurnState { .. } => Classified::Semantic {
            kind: JournalSemanticKind::TurnState,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::TurnState { state } => SemanticJournalPayload::TurnState {
                    state: format!("{state:?}").to_lowercase(),
                },
                _ => unreachable!("turn state kind already classified"),
            },
        },
        NativeJournalPayload::SessionState { .. } => Classified::Semantic {
            kind: JournalSemanticKind::SessionState,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::SessionState { state } => {
                    SemanticJournalPayload::SessionState {
                        state: format!("{state:?}").to_lowercase(),
                    }
                }
                _ => unreachable!("session state kind already classified"),
            },
        },
        NativeJournalPayload::ArtifactReference { .. } => Classified::Semantic {
            kind: JournalSemanticKind::ArtifactReference,
            text: None,
            redaction_class: JournalRedactionClass::MetadataOnly,
            privacy_class: PrivacyClass::Shareable,
            payload: match &content.payload {
                NativeJournalPayload::ArtifactReference { label } => {
                    SemanticJournalPayload::ArtifactReference {
                        label: label.clone(),
                    }
                }
                _ => unreachable!("artifact reference kind already classified"),
            },
        },
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
