//! Provider-neutral semantic journal: binding-first normalizer and bounded store.
//!
//! Trusted identity comes only from an authenticated adapter-delivery binding.
//! Provider payload bytes are content. Unknown/malformed facts never enter the
//! task reducer. Free stock adapter ingress stays unavailable: Claude/Codex
//! journal content is produced only after authenticated current-generation hook
//! registry admission. Cursor remains typed unsupported.

use crate::domain::{
    AgentSessionId, DomainEvent, EventId, PageLimits, PrivacyClass, ResourceId,
    SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload, TaskId,
};
use crate::kernel::semantic_journal::{
    SemanticJournalAuthorityRecord, SemanticJournalFactRef, SemanticJournalFactRow,
    SemanticJournalPageRowAction, SemanticJournalPageRowMeta,
};
use crate::kernel::{KernelStore, StoreError};
use crate::protocol::{FrameLimits, MessagePackCodec, MessagePackError, MAX_MESSAGEPACK_DEPTH};
use crate::providers::capabilities::ProviderKind;
use hmac::{Hmac, Mac};
use serde::de::{self, Deserializer, Visitor};
use serde::ser::{SerializeSeq, SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::time::Instant;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static JOURNAL_PAGE_EVENT_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
    static JOURNAL_PAGE_FACT_MATERIALIZATIONS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn debug_reset_journal_page_materialization_counters() {
    JOURNAL_PAGE_EVENT_MATERIALIZATIONS.with(|counter| counter.set(0));
    JOURNAL_PAGE_FACT_MATERIALIZATIONS.with(|counter| counter.set(0));
    crate::kernel::semantic_journal::debug_reset_semantic_journal_materialization_counters();
    JOURNAL_PAGE_PAYLOAD_PROBES.with(|counter| counter.set(0));
}

#[cfg(test)]
fn debug_journal_page_materialization_counters() -> (usize, usize) {
    (
        JOURNAL_PAGE_EVENT_MATERIALIZATIONS.with(Cell::get),
        JOURNAL_PAGE_FACT_MATERIALIZATIONS.with(Cell::get),
    )
}

#[cfg(test)]
fn debug_journal_page_preflight_counters() -> (usize, usize) {
    (
        crate::kernel::semantic_journal::debug_semantic_journal_materialization_counters(),
        JOURNAL_PAGE_PAYLOAD_PROBES.with(Cell::get),
    )
}

#[cfg(test)]
fn debug_record_page_event_materialization() {
    JOURNAL_PAGE_EVENT_MATERIALIZATIONS.with(|counter| {
        counter.set(counter.get().saturating_add(1));
    });
}

#[cfg(test)]
fn debug_record_page_fact_materialization() {
    JOURNAL_PAGE_FACT_MATERIALIZATIONS.with(|counter| {
        counter.set(counter.get().saturating_add(1));
    });
}

#[cfg(test)]
thread_local! {
    static JOURNAL_PAGE_PAYLOAD_PROBES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn debug_record_page_payload_probe() {
    JOURNAL_PAGE_PAYLOAD_PROBES.with(|counter| {
        counter.set(counter.get().saturating_add(1));
    });
}

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
const MAX_JOURNAL_MESSAGEPACK_VALUES: usize = MAX_JOURNAL_JSON_NODES;
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
            "free stock adapter ingress is unavailable; Claude/Codex require authenticated current-generation hook admission before journal normalize"
        )
    }
}

impl std::error::Error for AdapterIngressUnavailable {}

/// Free stock ingress remains closed. Claude/Codex use admission-gated adapter
/// bridges; Cursor has no proven semantic surface.
pub const fn stock_adapter_ingress_available() -> bool {
    false
}

pub fn stock_adapter_ingress() -> Result<std::convert::Infallible, AdapterIngressUnavailable> {
    Err(AdapterIngressUnavailable)
}

/// Content-only adapter output. It cannot carry EventId/sequence and has no
/// public constructor, so adapters cannot mint committed journal identity.
pub struct NormalizedAdapterDelivery {
    content: Vec<u8>,
}

impl fmt::Debug for NormalizedAdapterDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NormalizedAdapterDelivery")
            .field("content_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

impl NormalizedAdapterDelivery {
    pub(crate) fn sealed_from_content(content: Vec<u8>) -> Result<Self, JournalNormalizeError> {
        parse_journal_content(&content).map_err(|error| match error {
            JournalError::ForgedIdentity => JournalNormalizeError::ForgedIdentity,
            _ => JournalNormalizeError::InvalidPayload,
        })?;
        Ok(Self { content })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.content
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalNormalizeError {
    Unavailable(AdapterIngressUnavailable),
    InvalidPayload,
    ForgedIdentity,
    ProviderMismatch,
    AdmissionRejected,
}

impl fmt::Display for JournalNormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => error.fmt(f),
            Self::InvalidPayload => write!(f, "adapter delivery payload is invalid"),
            Self::ForgedIdentity => write!(f, "adapter delivery forged journal identity"),
            Self::ProviderMismatch => {
                write!(f, "adapter delivery permit provider does not match adapter")
            }
            Self::AdmissionRejected => {
                write!(
                    f,
                    "hook failed authenticated current-generation registry admission"
                )
            }
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
    pub(crate) fn issue_for_test(
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

    pub const fn provider(&self) -> ProviderKind {
        self.authority.provider()
    }

    /// Match the durable journal permit to an already authenticated provider
    /// launch correlation before normalized content can be handed to ingest.
    pub(crate) fn matches_correlation(
        &self,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        runtime_generation: u64,
        action_epoch: u64,
    ) -> bool {
        self.authority.task_id == task_id
            && self.authority.agent_session_id == agent_session_id
            && self.authority.runtime_generation == runtime_generation
            && self.authority.action_epoch == action_epoch
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

    pub const fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    pub const fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    #[cfg(test)]
    pub(crate) fn from_correlated_test(
        provider: ProviderKind,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
    ) -> Self {
        Self {
            id: EventId::new(),
            schema_version: JOURNAL_SCHEMA_VERSION,
            provider,
            provider_event_id: None,
            delivery_id: RelayDeliveryId::new("specialist-lineage").expect("delivery"),
            task_id,
            agent_session_id,
            resource_id,
            runtime_generation,
            action_epoch,
            sequence: 1,
            kind: JournalSemanticKind::ToolResult,
            occurred_at_ms: 1,
            ingested_at_ms: 1,
            visibility: JournalVisibility::Semantic,
            redaction_class: JournalRedactionClass::Persistable,
            privacy_class: PrivacyClass::LocalOnly,
            text: Some("specialist complete".into()),
            extensions: BTreeMap::new(),
            unknown: None,
            payload: crate::domain::snapshot::SemanticJournalPayload::ToolResult {
                call_id: "specialist-result".into(),
                status: "completed".into(),
            },
            payload_hash: [0x51; 32],
        }
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
        #[cfg(test)]
        debug_record_page_fact_materialization();
        SemanticJournalFact {
            id: self.id,
            sequence: self.sequence,
            occurred_at_ms: Some(self.occurred_at_ms),
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
            .semantic_journal_high_water_validated(&record.digest, |row| {
                validate_restored_row(permit.authority, row)
            })
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

    /// Ingest adapter-normalized journal content under the same permit rules.
    pub fn ingest_normalized(
        &mut self,
        permit: AdapterDeliveryPermit,
        delivery: NormalizedAdapterDelivery,
        now_ms: i64,
    ) -> JournalIngestOutcome {
        self.ingest(permit, delivery.as_bytes(), now_ms)
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
            .and_then(|(count, _, _)| usize::try_from(count).map_err(|_| StoreError::Corruption))
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
        // Do not reserve candidate storage until a row passes the byte
        // preflight below; a tiny page budget must not allocate a page-sized
        // fact buffer just to reject its first row.
        let page_state = RefCell::new(PageBuildState {
            expected: after_sequence.saturating_add(1),
            candidates: Vec::new(),
            next_candidate_sequences: Vec::new(),
            overflow_sequence: None,
            scanned_through: after_sequence,
            stream_error: None,
            preflight_encoded_bytes: None,
        });
        let high_water = self
            .store
            .semantic_journal_stream_page(
                &self.authority_digest,
                after,
                requested_high_water,
                |_high_water, rows: &[SemanticJournalPageRowMeta]| {
                    let mut state = page_state.borrow_mut();
                    let mut next_candidate = None;
                    let mut next_candidates = Vec::with_capacity(rows.len());
                    for row in rows.iter().rev() {
                        next_candidates.push((row.sequence, next_candidate));
                        if persist_only || !row.runtime_only {
                            next_candidate = Some(row.sequence);
                        }
                    }
                    next_candidates.reverse();
                    state.next_candidate_sequences = next_candidates;
                    Ok(())
                },
                |row| validate_metadata_row(self.authority, &row),
                |high_water, row| {
                    let mut state = page_state.borrow_mut();
                    let sequence = match u64::try_from(row.sequence) {
                        Ok(sequence) => sequence,
                        Err(_) => {
                            state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(SemanticJournalPageRowAction::Stop);
                        }
                    };
                    if sequence != state.expected || sequence > high_water {
                        state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                        return Ok(SemanticJournalPageRowAction::Stop);
                    }
                    state.expected = state.expected.saturating_add(1);
                    state.scanned_through = sequence;
                    if !persist_only && row.visibility == JournalVisibility::RuntimeOnly.as_str() {
                        return Ok(SemanticJournalPageRowAction::Skip);
                    }
                    if state.candidates.len() as u32 >= limits.max_items {
                        state.overflow_sequence = Some(sequence);
                        return Ok(SemanticJournalPageRowAction::Stop);
                    }
                    let candidate =
                        match page_fact_projection_preflight(&row, self.authority.provider) {
                            Ok(candidate) => candidate,
                            Err(_) => {
                                state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                                return Ok(SemanticJournalPageRowAction::Stop);
                            }
                        };
                    let next_sequence = state
                        .next_candidate_sequences
                        .iter()
                        .find(|(candidate, _)| *candidate == sequence)
                        .and_then(|(_, next)| *next);
                    // Measure the exact projected page before restoring the
                    // candidate event or allocating its owned fact. The
                    // borrowed projection reuses the durable payload's
                    // canonical MessagePack shape and bounded counting writer;
                    // no oversized page buffer is built.
                    let encoded_bytes = match page_encoded_len_with_candidate(
                        &codec,
                        after_sequence,
                        sequence,
                        high_water,
                        next_sequence,
                        &state.candidates,
                        &candidate,
                        limits.max_encoded_bytes,
                    ) {
                        Ok(encoded_bytes) => encoded_bytes,
                        Err(PageMeasureError::TooLarge) => {
                            state.overflow_sequence = Some(sequence);
                            return Ok(SemanticJournalPageRowAction::Stop);
                        }
                        Err(PageMeasureError::Encode) => {
                            state.stream_error = Some(JournalIngestOutcome::Backpressure(
                                JournalBackpressure::PageBudget,
                            ));
                            return Ok(SemanticJournalPageRowAction::Stop);
                        }
                    };
                    state.preflight_encoded_bytes = Some(encoded_bytes);
                    Ok(SemanticJournalPageRowAction::Fetch)
                },
                |high_water, row| {
                    let mut state = page_state.borrow_mut();
                    if validate_restored_row(self.authority, &row).is_err() {
                        state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                        return Ok(false);
                    }
                    let sequence = match u64::try_from(row.sequence) {
                        Ok(sequence) => sequence,
                        Err(_) => {
                            state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    let encoded_bytes = match state.preflight_encoded_bytes.take() {
                        Some(encoded_bytes) => encoded_bytes,
                        None => {
                            state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    let _candidate = match page_fact_projection(&row, self.authority.provider) {
                        Ok(candidate) => candidate,
                        Err(_) => {
                            state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    let next_sequence = state
                        .next_candidate_sequences
                        .iter()
                        .find(|(candidate, _)| *candidate == sequence)
                        .and_then(|(_, next)| *next);
                    let event = match restore_event(&self.authority, row) {
                        Ok(event) => event,
                        Err(_) => {
                            state.stream_error = Some(JournalIngestOutcome::NeedsResync);
                            return Ok(false);
                        }
                    };
                    if !persist_only && event.visibility == JournalVisibility::RuntimeOnly {
                        return Ok(true);
                    }
                    let candidate = event.to_snapshot_fact();
                    state.candidates.push(candidate);
                    let mut page = SemanticJournalPage {
                        oldest_sequence: 0,
                        cursor_rolled_over: false,
                        after_sequence,
                        through_sequence: event.sequence,
                        high_water,
                        encoded_bytes: 0,
                        next_sequence,
                        facts: std::mem::take(&mut state.candidates),
                    };
                    page.encoded_bytes = encoded_bytes;
                    state.candidates = page.facts;
                    Ok(true)
                },
            )
            .map_err(|_| JournalIngestOutcome::NeedsResync)?;
        let PageBuildState {
            candidates,
            overflow_sequence,
            scanned_through,
            stream_error,
            ..
        } = page_state.into_inner();
        if let Some(error) = stream_error {
            return Err(error);
        }
        if after_sequence > high_water {
            return Err(JournalIngestOutcome::NeedsResync);
        }
        let through_sequence = candidates
            .last()
            .map(|fact| fact.sequence)
            // An oversized first candidate was admitted to neither the owned
            // row nor payload path. It was not returned, so the page leaves
            // the durable cursor at `after_sequence` and exposes the
            // candidate again through `next_sequence`.
            .or_else(|| overflow_sequence.is_none().then_some(scanned_through))
            .unwrap_or(after_sequence);
        let next_sequence = overflow_sequence
            .or_else(|| (scanned_through < high_water).then_some(scanned_through + 1));
        let mut page = SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
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

struct PageBuildState {
    expected: u64,
    candidates: Vec<SemanticJournalFact>,
    next_candidate_sequences: Vec<(u64, Option<u64>)>,
    overflow_sequence: Option<u64>,
    scanned_through: u64,
    stream_error: Option<JournalIngestOutcome>,
    preflight_encoded_bytes: Option<u32>,
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

/// Borrowed page payload used only for the byte preflight.  The durable row's
/// payload has the same canonical MessagePack representation as the projected
/// fact payload, so this probe can measure the exact wire shape without first
/// allocating a `JournalEvent` or an owned `SemanticJournalFact`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SemanticJournalPayloadRef<'a> {
    UserMessage {
        text: Cow<'a, str>,
    },
    AssistantText {
        text: Cow<'a, str>,
    },
    ReasoningSummary {
        text: Cow<'a, str>,
    },
    ToolCall {
        tool_name: Cow<'a, str>,
        call_id: Cow<'a, str>,
    },
    ToolResult {
        call_id: Cow<'a, str>,
        status: Cow<'a, str>,
    },
    ApprovalRequest {
        request_id: Cow<'a, str>,
        summary: Cow<'a, str>,
    },
    ApprovalResult {
        request_id: Cow<'a, str>,
        decision: Cow<'a, str>,
    },
    Question {
        question_id: Cow<'a, str>,
        prompt: Cow<'a, str>,
        options: Vec<Cow<'a, str>>,
    },
    PlanStep {
        step_id: Cow<'a, str>,
        title: Cow<'a, str>,
        status: Cow<'a, str>,
    },
    UsageObservation {
        remaining_percent: Option<u8>,
    },
    Error {
        code: Cow<'a, str>,
        message: Cow<'a, str>,
    },
    TurnState {
        state: Cow<'a, str>,
    },
    SessionState {
        state: Cow<'a, str>,
    },
    ArtifactReference {
        label: Cow<'a, str>,
    },
    Unknown {
        provider: Cow<'a, str>,
        source_type: Cow<'a, str>,
        schema_version: u32,
        diagnostic_ref: Cow<'a, str>,
    },
}

/// Zero-allocation payload shape used only for the page byte preflight. It
/// reads the persisted MessagePack value directly and borrows every string;
/// the owned `Cow` probe below is reserved for rows that already passed the
/// cap.
#[derive(Debug, Clone, Copy)]
enum RawPayloadShape<'a> {
    UserMessage {
        text: &'a str,
    },
    AssistantText {
        text: &'a str,
    },
    ReasoningSummary {
        text: &'a str,
    },
    ToolCall {
        tool_name: &'a str,
        call_id: &'a str,
    },
    ToolResult {
        call_id: &'a str,
        status: &'a str,
    },
    ApprovalRequest {
        request_id: &'a str,
        summary: &'a str,
    },
    ApprovalResult {
        request_id: &'a str,
        decision: &'a str,
    },
    Question {
        question_id: &'a str,
        prompt: &'a str,
        options: RawPayloadOptions<'a>,
    },
    PlanStep {
        step_id: &'a str,
        title: &'a str,
        status: &'a str,
    },
    UsageObservation {
        remaining_percent: Option<u8>,
    },
    Error {
        code: &'a str,
        message: &'a str,
    },
    TurnState {
        state: &'a str,
    },
    SessionState {
        state: &'a str,
    },
    ArtifactReference {
        label: &'a str,
    },
    Unknown {
        provider: &'a str,
        source_type: &'a str,
        schema_version: u32,
        diagnostic_ref: &'a str,
    },
}

#[derive(Debug, Clone, Copy)]
struct RawPayloadOptions<'a> {
    values: [&'a str; MAX_JOURNAL_ARRAY_ITEMS],
    len: usize,
}

impl Serialize for RawPayloadOptions<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.len))?;
        for value in &self.values[..self.len] {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}

impl Serialize for RawPayloadShape<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::UserMessage { text } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "user_message")?;
                payload.serialize_field("text", text)?;
                payload.end()
            }
            Self::AssistantText { text } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "assistant_text")?;
                payload.serialize_field("text", text)?;
                payload.end()
            }
            Self::ReasoningSummary { text } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "reasoning_summary")?;
                payload.serialize_field("text", text)?;
                payload.end()
            }
            Self::ToolCall { tool_name, call_id } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 3)?;
                payload.serialize_field("kind", "tool_call")?;
                payload.serialize_field("tool_name", tool_name)?;
                payload.serialize_field("call_id", call_id)?;
                payload.end()
            }
            Self::ToolResult { call_id, status } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 3)?;
                payload.serialize_field("kind", "tool_result")?;
                payload.serialize_field("call_id", call_id)?;
                payload.serialize_field("status", status)?;
                payload.end()
            }
            Self::ApprovalRequest {
                request_id,
                summary,
            } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 3)?;
                payload.serialize_field("kind", "approval_request")?;
                payload.serialize_field("request_id", request_id)?;
                payload.serialize_field("summary", summary)?;
                payload.end()
            }
            Self::ApprovalResult {
                request_id,
                decision,
            } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 3)?;
                payload.serialize_field("kind", "approval_result")?;
                payload.serialize_field("request_id", request_id)?;
                payload.serialize_field("decision", decision)?;
                payload.end()
            }
            Self::Question {
                question_id,
                prompt,
                options,
            } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 4)?;
                payload.serialize_field("kind", "question")?;
                payload.serialize_field("question_id", question_id)?;
                payload.serialize_field("prompt", prompt)?;
                payload.serialize_field("options", options)?;
                payload.end()
            }
            Self::PlanStep {
                step_id,
                title,
                status,
            } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 4)?;
                payload.serialize_field("kind", "plan_step")?;
                payload.serialize_field("step_id", step_id)?;
                payload.serialize_field("title", title)?;
                payload.serialize_field("status", status)?;
                payload.end()
            }
            Self::UsageObservation { remaining_percent } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "usage_observation")?;
                payload.serialize_field("remaining_percent", remaining_percent)?;
                payload.end()
            }
            Self::Error { code, message } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 3)?;
                payload.serialize_field("kind", "error")?;
                payload.serialize_field("code", code)?;
                payload.serialize_field("message", message)?;
                payload.end()
            }
            Self::TurnState { state } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "turn_state")?;
                payload.serialize_field("state", state)?;
                payload.end()
            }
            Self::SessionState { state } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "session_state")?;
                payload.serialize_field("state", state)?;
                payload.end()
            }
            Self::ArtifactReference { label } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 2)?;
                payload.serialize_field("kind", "artifact_reference")?;
                payload.serialize_field("label", label)?;
                payload.end()
            }
            Self::Unknown {
                provider,
                source_type,
                schema_version,
                diagnostic_ref,
            } => {
                let mut payload = serializer.serialize_struct("SemanticJournalPayload", 5)?;
                payload.serialize_field("kind", "unknown")?;
                payload.serialize_field("provider", provider)?;
                payload.serialize_field("source_type", source_type)?;
                payload.serialize_field("schema_version", schema_version)?;
                payload.serialize_field("diagnostic_ref", diagnostic_ref)?;
                payload.end()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RawPersistedBody<'a> {
    text: Option<&'a str>,
    extensions: RawPersistedExtensions<'a>,
    unknown_source_type: Option<&'a str>,
    unknown_schema_version: Option<u32>,
    unknown_diagnostic_ref: Option<&'a str>,
    provider_event_id: Option<&'a str>,
    payload: RawPayloadShape<'a>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RawPersistedExtensions<'a> {
    hook_event_name: Option<&'a str>,
    codex_item: Option<&'a str>,
    cursor_surface: Option<&'a str>,
}

struct RawMessagePack<'a> {
    bytes: &'a [u8],
    offset: usize,
    values_seen: usize,
}

impl<'a> RawMessagePack<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            values_seen: 0,
        }
    }

    fn persisted_payload_shape(mut self) -> Result<RawPayloadShape<'a>, JournalError> {
        self.persisted_body().map(|body| body.payload)
    }

    fn persisted_body(mut self) -> Result<RawPersistedBody<'a>, JournalError> {
        let fields = self.map_len()?;
        let mut text = None;
        let mut extensions = None;
        let mut unknown_source_type = None;
        let mut unknown_schema_version = None;
        let mut unknown_diagnostic_ref = None;
        let mut provider_event_id = None;
        let mut payload = None;
        for _ in 0..fields {
            let key = self.string()?;
            match key {
                "text" => set_once(&mut text, self.optional_string()?)?,
                "extensions" => set_once(&mut extensions, self.extensions()?)?,
                "unknown_source_type" => {
                    set_once(&mut unknown_source_type, self.optional_string()?)?
                }
                "unknown_schema_version" => {
                    set_once(&mut unknown_schema_version, self.optional_u32()?)?
                }
                "unknown_diagnostic_ref" => {
                    set_once(&mut unknown_diagnostic_ref, self.optional_string()?)?
                }
                "provider_event_id" => set_once(&mut provider_event_id, self.optional_string()?)?,
                "payload" => set_once(&mut payload, self.payload_shape(1)?)?,
                _ => return Err(JournalError::InvalidEnvelope),
            }
        }
        if self.offset != self.bytes.len() {
            return Err(JournalError::Store);
        }
        Ok(RawPersistedBody {
            text: text.unwrap_or(None),
            extensions: extensions.ok_or(JournalError::InvalidEnvelope)?,
            unknown_source_type: unknown_source_type.unwrap_or(None),
            unknown_schema_version: unknown_schema_version.unwrap_or(None),
            unknown_diagnostic_ref: unknown_diagnostic_ref.unwrap_or(None),
            provider_event_id: provider_event_id.unwrap_or(None),
            payload: payload.ok_or(JournalError::InvalidEnvelope)?,
        })
    }

    fn payload_shape(&mut self, depth: usize) -> Result<RawPayloadShape<'a>, JournalError> {
        if depth > MAX_JOURNAL_NESTING {
            return Err(JournalError::NestingTooDeep);
        }
        let fields = self.map_len()?;
        let mut values = RawPayloadFields::default();
        for _ in 0..fields {
            let key = self.string()?;
            values.read_field(self, key, depth + 1)?;
        }
        values.finish()
    }

    fn optional_string(&mut self) -> Result<Option<&'a str>, JournalError> {
        if self.bytes.get(self.offset).copied() == Some(0xc0) {
            self.value_marker()?;
            Ok(None)
        } else {
            self.string().map(Some)
        }
    }

    fn optional_u32(&mut self) -> Result<Option<u32>, JournalError> {
        if self.bytes.get(self.offset).copied() == Some(0xc0) {
            self.value_marker()?;
            Ok(None)
        } else {
            u32::try_from(self.unsigned()?)
                .map(Some)
                .map_err(|_| JournalError::InvalidEnvelope)
        }
    }

    fn extensions(&mut self) -> Result<RawPersistedExtensions<'a>, JournalError> {
        let fields = self.map_len()?;
        if fields > MAX_EXTENSION_ENTRIES {
            return Err(JournalError::TooManyExtensions);
        }
        let mut extensions = RawPersistedExtensions::default();
        for _ in 0..fields {
            let key = self.string()?;
            let value = self.string()?;
            match key {
                "hook_event_name" => set_once(&mut extensions.hook_event_name, value)?,
                "codex_item" => set_once(&mut extensions.codex_item, value)?,
                "cursor_surface" => set_once(&mut extensions.cursor_surface, value)?,
                _ => return Err(JournalError::InvalidEnvelope),
            }
        }
        Ok(extensions)
    }

    fn map_len(&mut self) -> Result<usize, JournalError> {
        let marker = self.value_marker()?;
        let length = match marker {
            0x80..=0x8f => usize::from(marker - 0x80),
            0xde => usize::from(self.u16()?),
            0xdf => usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?,
            _ => return Err(JournalError::InvalidEnvelope),
        };
        if length > MAX_JOURNAL_MAP_ENTRIES {
            return Err(JournalError::Oversized);
        }
        Ok(length)
    }

    fn array_len(&mut self) -> Result<usize, JournalError> {
        let marker = self.value_marker()?;
        let length = match marker {
            0x90..=0x9f => usize::from(marker - 0x90),
            0xdc => usize::from(self.u16()?),
            0xdd => usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?,
            _ => return Err(JournalError::InvalidEnvelope),
        };
        if length > MAX_JOURNAL_ARRAY_ITEMS {
            return Err(JournalError::Oversized);
        }
        Ok(length)
    }

    fn string(&mut self) -> Result<&'a str, JournalError> {
        let marker = self.value_marker()?;
        let length = match marker {
            0xa0..=0xbf => usize::from(marker - 0xa0),
            0xd9 => usize::from(self.byte()?),
            0xda => usize::from(self.u16()?),
            0xdb => usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?,
            _ => return Err(JournalError::InvalidEnvelope),
        };
        if length > MAX_JOURNAL_TEXT_BYTES {
            return Err(JournalError::TooLong);
        }
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes).map_err(|_| JournalError::InvalidEnvelope)
    }

    fn optional_u8(&mut self) -> Result<Option<u8>, JournalError> {
        if self.bytes.get(self.offset).copied() == Some(0xc0) {
            self.value_marker()?;
            return Ok(None);
        }
        let value = self.unsigned()?;
        u8::try_from(value)
            .map(Some)
            .map_err(|_| JournalError::InvalidEnvelope)
    }

    fn unsigned(&mut self) -> Result<u64, JournalError> {
        let marker = self.value_marker()?;
        match marker {
            0x00..=0x7f => Ok(u64::from(marker)),
            0xcc => Ok(u64::from(self.byte()?)),
            0xcd => Ok(u64::from(self.u16()?)),
            0xce => Ok(u64::from(self.u32()?)),
            0xcf => Ok(self.u64()?),
            _ => Err(JournalError::InvalidEnvelope),
        }
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), JournalError> {
        if depth > usize::from(MAX_MESSAGEPACK_DEPTH) {
            return Err(JournalError::NestingTooDeep);
        }
        let marker = self.value_marker()?;
        match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
            0x80..=0x8f => {
                let length = usize::from(marker - 0x80);
                self.skip_children(length, depth + 1)
            }
            0x90..=0x9f => {
                let length = usize::from(marker - 0x90);
                self.skip_children(length, depth + 1)
            }
            0xa0..=0xbf => self.skip(usize::from(marker - 0xa0)),
            0xc4 => {
                let length = usize::from(self.byte()?);
                self.skip(length)
            }
            0xc5 => {
                let length = usize::from(self.u16()?);
                self.skip(length)
            }
            0xc6 => {
                let length = usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?;
                self.skip(length)
            }
            0xca | 0xce | 0xd2 => self.skip(4),
            0xcb | 0xcf | 0xd3 => self.skip(8),
            0xcc | 0xd0 => self.skip(1),
            0xcd | 0xd1 => self.skip(2),
            0xd4 => self.skip(3),
            0xd5 => self.skip(4),
            0xd6 => self.skip(6),
            0xd7 => self.skip(10),
            0xd8 => self.skip(18),
            0xd9 => {
                let length = usize::from(self.byte()?);
                self.skip(length)
            }
            0xda => {
                let length = usize::from(self.u16()?);
                self.skip(length)
            }
            0xdb => {
                let length = usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?;
                self.skip(length)
            }
            0xdc => {
                let length = usize::from(self.u16()?);
                if length > MAX_JOURNAL_ARRAY_ITEMS {
                    return Err(JournalError::Oversized);
                }
                self.skip_children(length, depth + 1)
            }
            0xdd => {
                let length = usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?;
                if length > MAX_JOURNAL_ARRAY_ITEMS {
                    return Err(JournalError::Oversized);
                }
                self.skip_children(length, depth + 1)
            }
            0xde => {
                let length = usize::from(self.u16()?);
                if length > MAX_JOURNAL_MAP_ENTRIES {
                    return Err(JournalError::Oversized);
                }
                self.skip_children(length.saturating_mul(2), depth + 1)
            }
            0xdf => {
                let length = usize::try_from(self.u32()?).map_err(|_| JournalError::Store)?;
                if length > MAX_JOURNAL_MAP_ENTRIES {
                    return Err(JournalError::Oversized);
                }
                self.skip_children(length.saturating_mul(2), depth + 1)
            }
            0xc1 | 0xc7..=0xc9 => Err(JournalError::InvalidEnvelope),
            _ => Err(JournalError::InvalidEnvelope),
        }
    }

    fn skip_children(&mut self, count: usize, depth: usize) -> Result<(), JournalError> {
        for _ in 0..count {
            self.skip_value(depth)?;
        }
        Ok(())
    }

    fn value_marker(&mut self) -> Result<u8, JournalError> {
        self.values_seen = self
            .values_seen
            .checked_add(1)
            .ok_or(JournalError::Oversized)?;
        if self.values_seen > MAX_JOURNAL_MESSAGEPACK_VALUES {
            return Err(JournalError::Oversized);
        }
        self.byte()
    }

    fn byte(&mut self) -> Result<u8, JournalError> {
        let value = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or(JournalError::Store)?;
        self.offset = self.offset.saturating_add(1);
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, JournalError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, JournalError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, JournalError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn skip(&mut self, length: usize) -> Result<(), JournalError> {
        self.take(length).map(|_| ())
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], JournalError> {
        let end = self.offset.checked_add(length).ok_or(JournalError::Store)?;
        if end > self.bytes.len() {
            return Err(JournalError::Store);
        }
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }
}

#[derive(Default)]
struct RawPayloadFields<'a> {
    kind: Option<&'a str>,
    text: Option<&'a str>,
    tool_name: Option<&'a str>,
    call_id: Option<&'a str>,
    status: Option<&'a str>,
    request_id: Option<&'a str>,
    summary: Option<&'a str>,
    decision: Option<&'a str>,
    question_id: Option<&'a str>,
    prompt: Option<&'a str>,
    options: Option<RawPayloadOptions<'a>>,
    step_id: Option<&'a str>,
    title: Option<&'a str>,
    remaining_percent: Option<u8>,
    remaining_percent_seen: bool,
    code: Option<&'a str>,
    message: Option<&'a str>,
    state: Option<&'a str>,
    label: Option<&'a str>,
    provider: Option<&'a str>,
    source_type: Option<&'a str>,
    schema_version: Option<u32>,
    diagnostic_ref: Option<&'a str>,
}

const RAW_FIELD_KIND: u32 = 1 << 0;
const RAW_FIELD_TEXT: u32 = 1 << 1;
const RAW_FIELD_TOOL_NAME: u32 = 1 << 2;
const RAW_FIELD_CALL_ID: u32 = 1 << 3;
const RAW_FIELD_STATUS: u32 = 1 << 4;
const RAW_FIELD_REQUEST_ID: u32 = 1 << 5;
const RAW_FIELD_SUMMARY: u32 = 1 << 6;
const RAW_FIELD_DECISION: u32 = 1 << 7;
const RAW_FIELD_QUESTION_ID: u32 = 1 << 8;
const RAW_FIELD_PROMPT: u32 = 1 << 9;
const RAW_FIELD_OPTIONS: u32 = 1 << 10;
const RAW_FIELD_STEP_ID: u32 = 1 << 11;
const RAW_FIELD_TITLE: u32 = 1 << 12;
const RAW_FIELD_REMAINING_PERCENT: u32 = 1 << 13;
const RAW_FIELD_CODE: u32 = 1 << 14;
const RAW_FIELD_MESSAGE: u32 = 1 << 15;
const RAW_FIELD_STATE: u32 = 1 << 16;
const RAW_FIELD_LABEL: u32 = 1 << 17;
const RAW_FIELD_PROVIDER: u32 = 1 << 18;
const RAW_FIELD_SOURCE_TYPE: u32 = 1 << 19;
const RAW_FIELD_SCHEMA_VERSION: u32 = 1 << 20;
const RAW_FIELD_DIAGNOSTIC_REF: u32 = 1 << 21;

impl<'a> RawPayloadFields<'a> {
    fn read_field(
        &mut self,
        reader: &mut RawMessagePack<'a>,
        key: &str,
        depth: usize,
    ) -> Result<(), JournalError> {
        if depth > MAX_JOURNAL_NESTING {
            return Err(JournalError::NestingTooDeep);
        }
        match key {
            "kind" => set_once(&mut self.kind, reader.string()?),
            "text" => set_once(&mut self.text, reader.string()?),
            "tool_name" => set_once(&mut self.tool_name, reader.string()?),
            "call_id" => set_once(&mut self.call_id, reader.string()?),
            "status" => set_once(&mut self.status, reader.string()?),
            "request_id" => set_once(&mut self.request_id, reader.string()?),
            "summary" => set_once(&mut self.summary, reader.string()?),
            "decision" => set_once(&mut self.decision, reader.string()?),
            "question_id" => set_once(&mut self.question_id, reader.string()?),
            "prompt" => set_once(&mut self.prompt, reader.string()?),
            "options" => set_once(&mut self.options, read_options(reader)?),
            "step_id" => set_once(&mut self.step_id, reader.string()?),
            "title" => set_once(&mut self.title, reader.string()?),
            "remaining_percent" => {
                if self.remaining_percent_seen {
                    return Err(JournalError::DuplicateKey);
                }
                self.remaining_percent_seen = true;
                self.remaining_percent = reader.optional_u8()?;
                Ok(())
            }
            "code" => set_once(&mut self.code, reader.string()?),
            "message" => set_once(&mut self.message, reader.string()?),
            "state" => set_once(&mut self.state, reader.string()?),
            "label" => set_once(&mut self.label, reader.string()?),
            "provider" => set_once(&mut self.provider, reader.string()?),
            "source_type" => set_once(&mut self.source_type, reader.string()?),
            "schema_version" => set_once(
                &mut self.schema_version,
                u32::try_from(reader.unsigned()?).map_err(|_| JournalError::InvalidEnvelope)?,
            ),
            "diagnostic_ref" => set_once(&mut self.diagnostic_ref, reader.string()?),
            _ => Err(JournalError::InvalidEnvelope),
        }
    }

    fn present_mask(&self) -> u32 {
        let mut fields = 0;
        if self.kind.is_some() {
            fields |= RAW_FIELD_KIND;
        }
        if self.text.is_some() {
            fields |= RAW_FIELD_TEXT;
        }
        if self.tool_name.is_some() {
            fields |= RAW_FIELD_TOOL_NAME;
        }
        if self.call_id.is_some() {
            fields |= RAW_FIELD_CALL_ID;
        }
        if self.status.is_some() {
            fields |= RAW_FIELD_STATUS;
        }
        if self.request_id.is_some() {
            fields |= RAW_FIELD_REQUEST_ID;
        }
        if self.summary.is_some() {
            fields |= RAW_FIELD_SUMMARY;
        }
        if self.decision.is_some() {
            fields |= RAW_FIELD_DECISION;
        }
        if self.question_id.is_some() {
            fields |= RAW_FIELD_QUESTION_ID;
        }
        if self.prompt.is_some() {
            fields |= RAW_FIELD_PROMPT;
        }
        if self.options.is_some() {
            fields |= RAW_FIELD_OPTIONS;
        }
        if self.step_id.is_some() {
            fields |= RAW_FIELD_STEP_ID;
        }
        if self.title.is_some() {
            fields |= RAW_FIELD_TITLE;
        }
        if self.remaining_percent_seen {
            fields |= RAW_FIELD_REMAINING_PERCENT;
        }
        if self.code.is_some() {
            fields |= RAW_FIELD_CODE;
        }
        if self.message.is_some() {
            fields |= RAW_FIELD_MESSAGE;
        }
        if self.state.is_some() {
            fields |= RAW_FIELD_STATE;
        }
        if self.label.is_some() {
            fields |= RAW_FIELD_LABEL;
        }
        if self.provider.is_some() {
            fields |= RAW_FIELD_PROVIDER;
        }
        if self.source_type.is_some() {
            fields |= RAW_FIELD_SOURCE_TYPE;
        }
        if self.schema_version.is_some() {
            fields |= RAW_FIELD_SCHEMA_VERSION;
        }
        if self.diagnostic_ref.is_some() {
            fields |= RAW_FIELD_DIAGNOSTIC_REF;
        }
        fields
    }

    fn finish(self) -> Result<RawPayloadShape<'a>, JournalError> {
        let kind = self.kind.ok_or(JournalError::InvalidEnvelope)?;
        let present = self.present_mask();
        let allowed = match kind {
            "user_message" | "assistant_text" | "reasoning_summary" => {
                RAW_FIELD_KIND | RAW_FIELD_TEXT
            }
            "tool_call" => RAW_FIELD_KIND | RAW_FIELD_TOOL_NAME | RAW_FIELD_CALL_ID,
            "tool_result" => RAW_FIELD_KIND | RAW_FIELD_CALL_ID | RAW_FIELD_STATUS,
            "approval_request" => RAW_FIELD_KIND | RAW_FIELD_REQUEST_ID | RAW_FIELD_SUMMARY,
            "approval_result" => RAW_FIELD_KIND | RAW_FIELD_REQUEST_ID | RAW_FIELD_DECISION,
            "question" => {
                RAW_FIELD_KIND | RAW_FIELD_QUESTION_ID | RAW_FIELD_PROMPT | RAW_FIELD_OPTIONS
            }
            "plan_step" => RAW_FIELD_KIND | RAW_FIELD_STEP_ID | RAW_FIELD_TITLE | RAW_FIELD_STATUS,
            "usage_observation" => RAW_FIELD_KIND | RAW_FIELD_REMAINING_PERCENT,
            "error" => RAW_FIELD_KIND | RAW_FIELD_CODE | RAW_FIELD_MESSAGE,
            "turn_state" | "session_state" => RAW_FIELD_KIND | RAW_FIELD_STATE,
            "artifact_reference" => RAW_FIELD_KIND | RAW_FIELD_LABEL,
            "unknown" => {
                RAW_FIELD_KIND
                    | RAW_FIELD_PROVIDER
                    | RAW_FIELD_SOURCE_TYPE
                    | RAW_FIELD_SCHEMA_VERSION
                    | RAW_FIELD_DIAGNOSTIC_REF
            }
            _ => return Err(JournalError::InvalidEnvelope),
        };
        if present != allowed {
            return Err(JournalError::InvalidEnvelope);
        }
        match kind {
            "user_message" => Ok(RawPayloadShape::UserMessage {
                text: required(self.text)?,
            }),
            "assistant_text" => Ok(RawPayloadShape::AssistantText {
                text: required(self.text)?,
            }),
            "reasoning_summary" => Ok(RawPayloadShape::ReasoningSummary {
                text: required(self.text)?,
            }),
            "tool_call" => Ok(RawPayloadShape::ToolCall {
                tool_name: required(self.tool_name)?,
                call_id: required(self.call_id)?,
            }),
            "tool_result" => Ok(RawPayloadShape::ToolResult {
                call_id: required(self.call_id)?,
                status: required(self.status)?,
            }),
            "approval_request" => Ok(RawPayloadShape::ApprovalRequest {
                request_id: required(self.request_id)?,
                summary: required(self.summary)?,
            }),
            "approval_result" => Ok(RawPayloadShape::ApprovalResult {
                request_id: required(self.request_id)?,
                decision: required(self.decision)?,
            }),
            "question" => Ok(RawPayloadShape::Question {
                question_id: required(self.question_id)?,
                prompt: required(self.prompt)?,
                options: self.options.ok_or(JournalError::InvalidEnvelope)?,
            }),
            "plan_step" => Ok(RawPayloadShape::PlanStep {
                step_id: required(self.step_id)?,
                title: required(self.title)?,
                status: required(self.status)?,
            }),
            "usage_observation" => Ok(RawPayloadShape::UsageObservation {
                remaining_percent: if self.remaining_percent_seen {
                    self.remaining_percent
                } else {
                    return Err(JournalError::InvalidEnvelope);
                },
            }),
            "error" => Ok(RawPayloadShape::Error {
                code: required(self.code)?,
                message: required(self.message)?,
            }),
            "turn_state" => Ok(RawPayloadShape::TurnState {
                state: required(self.state)?,
            }),
            "session_state" => Ok(RawPayloadShape::SessionState {
                state: required(self.state)?,
            }),
            "artifact_reference" => Ok(RawPayloadShape::ArtifactReference {
                label: required(self.label)?,
            }),
            "unknown" => Ok(RawPayloadShape::Unknown {
                provider: required(self.provider)?,
                source_type: required(self.source_type)?,
                schema_version: self.schema_version.ok_or(JournalError::InvalidEnvelope)?,
                diagnostic_ref: required(self.diagnostic_ref)?,
            }),
            _ => unreachable!("payload kind was checked above"),
        }
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), JournalError> {
    if slot.is_some() {
        return Err(JournalError::DuplicateKey);
    }
    *slot = Some(value);
    Ok(())
}

fn required<T>(value: Option<T>) -> Result<T, JournalError> {
    value.ok_or(JournalError::InvalidEnvelope)
}

fn read_options<'a>(
    reader: &mut RawMessagePack<'a>,
) -> Result<RawPayloadOptions<'a>, JournalError> {
    let len = reader.array_len()?;
    let mut values = [""; MAX_JOURNAL_ARRAY_ITEMS];
    for value in &mut values[..len] {
        *value = reader.string()?;
    }
    Ok(RawPayloadOptions { values, len })
}

#[derive(Debug, Deserialize)]
struct PersistedJournalPayloadProbe<'a> {
    #[serde(borrow)]
    payload: SemanticJournalPayloadRef<'a>,
}

#[derive(Debug)]
struct JournalFactProjection<'a, P> {
    id: EventId,
    sequence: u64,
    occurred_at_ms: i64,
    provider: &'a str,
    schema_version: u32,
    kind: &'a str,
    visibility: &'a str,
    privacy_class: PrivacyClass,
    redacted: bool,
    payload: P,
}

impl<P: Serialize> Serialize for JournalFactProjection<'_, P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut fact = serializer.serialize_struct("SemanticJournalFact", 10)?;
        fact.serialize_field("id", &self.id)?;
        fact.serialize_field("sequence", &self.sequence)?;
        fact.serialize_field("occurred_at_ms", &self.occurred_at_ms)?;
        fact.serialize_field("provider", &self.provider)?;
        fact.serialize_field("schema_version", &self.schema_version)?;
        fact.serialize_field("kind", &self.kind)?;
        fact.serialize_field("visibility", &self.visibility)?;
        fact.serialize_field("privacy_class", &self.privacy_class)?;
        fact.serialize_field("redacted", &self.redacted)?;
        fact.serialize_field("payload", &self.payload)?;
        fact.end()
    }
}

struct JournalFactSequence<'a, 'b, P> {
    existing: &'a [SemanticJournalFact],
    candidate: &'a JournalFactProjection<'b, P>,
}

impl<P: Serialize> Serialize for JournalFactSequence<'_, '_, P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.existing.len() + 1))?;
        for fact in self.existing {
            sequence.serialize_element(fact)?;
        }
        sequence.serialize_element(self.candidate)?;
        sequence.end()
    }
}

struct JournalPageProjection<'a, 'b, P> {
    after_sequence: u64,
    through_sequence: u64,
    high_water: u64,
    encoded_bytes: u32,
    next_sequence: Option<u64>,
    facts: JournalFactSequence<'a, 'b, P>,
}

impl<P: Serialize> Serialize for JournalPageProjection<'_, '_, P> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Match the complete wire envelope, including retention metadata, before
        // admitting an owned fact. These defaults match page_from_store.
        let mut page = serializer.serialize_struct("SemanticJournalPage", 8)?;
        page.serialize_field("oldest_sequence", &0_u64)?;
        page.serialize_field("cursor_rolled_over", &false)?;
        page.serialize_field("after_sequence", &self.after_sequence)?;
        page.serialize_field("through_sequence", &self.through_sequence)?;
        page.serialize_field("high_water", &self.high_water)?;
        page.serialize_field("encoded_bytes", &self.encoded_bytes)?;
        page.serialize_field("next_sequence", &self.next_sequence)?;
        page.serialize_field("facts", &self.facts)?;
        page.end()
    }
}

fn page_fact_projection<'a>(
    row: &'a SemanticJournalFactRow,
    provider: ProviderKind,
) -> Result<JournalFactProjection<'a, SemanticJournalPayloadRef<'a>>, JournalError> {
    #[cfg(test)]
    debug_record_page_payload_probe();
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default())
        .map_err(|_| JournalError::Store)?;
    if row.payload.is_empty() || row.payload.len() > codec.max_document_bytes() as usize {
        return Err(JournalError::Store);
    }
    let mut deserializer = rmp_serde::Deserializer::new(Cursor::new(&row.payload));
    deserializer.set_max_depth(usize::from(MAX_MESSAGEPACK_DEPTH) + 1);
    let body: PersistedJournalPayloadProbe<'a> =
        Deserialize::deserialize(&mut deserializer).map_err(|_| JournalError::Store)?;
    let position = usize::try_from(deserializer.position()).unwrap_or(usize::MAX);
    if position != row.payload.len() {
        return Err(JournalError::Store);
    }
    let schema_version = u32::try_from(row.schema_version).map_err(|_| JournalError::Store)?;
    let sequence = u64::try_from(row.sequence).map_err(|_| JournalError::Store)?;
    let privacy_class = match row.privacy_class.as_str() {
        "local_only" => PrivacyClass::LocalOnly,
        "shareable" => PrivacyClass::Shareable,
        _ => return Err(JournalError::Store),
    };
    Ok(JournalFactProjection {
        id: EventId::from_bytes(row.event_id).map_err(|_| JournalError::Store)?,
        sequence,
        occurred_at_ms: row.occurred_at_ms,
        provider: provider_kind_sql(provider),
        schema_version,
        kind: &row.kind,
        visibility: &row.visibility,
        privacy_class,
        redacted: row.redaction_class != JournalRedactionClass::Persistable.as_str(),
        payload: body.payload,
    })
}

fn page_fact_projection_preflight<'a>(
    row: &SemanticJournalFactRef<'a>,
    provider: ProviderKind,
) -> Result<JournalFactProjection<'a, RawPayloadShape<'a>>, JournalError> {
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default())
        .map_err(|_| JournalError::Store)?;
    if row.payload.is_empty() || row.payload.len() > codec.max_document_bytes() as usize {
        return Err(JournalError::Store);
    }
    let payload = RawMessagePack::new(row.payload).persisted_payload_shape()?;
    let schema_version = u32::try_from(row.schema_version).map_err(|_| JournalError::Store)?;
    let sequence = u64::try_from(row.sequence).map_err(|_| JournalError::Store)?;
    let privacy_class = match row.privacy_class {
        "local_only" => PrivacyClass::LocalOnly,
        "shareable" => PrivacyClass::Shareable,
        _ => return Err(JournalError::Store),
    };
    let event_id = <[u8; 16]>::try_from(row.event_id).map_err(|_| JournalError::Store)?;
    Ok(JournalFactProjection {
        id: EventId::from_bytes(event_id).map_err(|_| JournalError::Store)?,
        sequence,
        occurred_at_ms: row.occurred_at_ms,
        provider: provider_kind_sql(provider),
        schema_version,
        kind: row.kind,
        visibility: row.visibility,
        privacy_class,
        redacted: row.redaction_class != JournalRedactionClass::Persistable.as_str(),
        payload,
    })
}

fn page_encoded_len_with_candidate<P: Serialize>(
    codec: &MessagePackCodec,
    after_sequence: u64,
    through_sequence: u64,
    high_water: u64,
    next_sequence: Option<u64>,
    existing: &[SemanticJournalFact],
    candidate: &JournalFactProjection<'_, P>,
    maximum: u32,
) -> Result<u32, PageMeasureError> {
    let mut encoded_bytes = 0_u32;
    for _ in 0..8 {
        let page = JournalPageProjection {
            after_sequence,
            through_sequence,
            high_water,
            encoded_bytes,
            next_sequence,
            facts: JournalFactSequence {
                existing,
                candidate,
            },
        };
        let measured = match codec.encoded_len_bounded(&page, maximum) {
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
    #[cfg(test)]
    debug_record_page_event_materialization();
    restore_event_internal(authority, &row, true)?.ok_or(JournalError::Store)
}

fn restore_event_internal(
    authority: &JournalSessionAuthority,
    row: &SemanticJournalFactRow,
    materialize: bool,
) -> Result<Option<JournalEvent>, JournalError> {
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
    // Identity constructors are part of row integrity, even when this pass
    // intentionally avoids allocating a page event. Otherwise a persisted
    // bidi/control identity would pass the non-materializing validator and be
    // observed only on a later event restore.
    let provider_event_id = body
        .provider_event_id
        .map(ProviderEventId::new)
        .transpose()?;
    let delivery_id = RelayDeliveryId::new(row.delivery_id.clone())?;
    if !materialize {
        return Ok(None);
    }
    Ok(Some(JournalEvent {
        id: EventId::from_bytes(row.event_id).map_err(|_| JournalError::Store)?,
        schema_version,
        provider: authority.provider,
        provider_event_id,
        delivery_id,
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
    }))
}

fn validate_restored_row(
    authority: JournalSessionAuthority,
    row: &SemanticJournalFactRow,
) -> Result<(), StoreError> {
    // The store's global integrity pass must decode and validate every row,
    // but it must not allocate a page event. Page materialization is gated by
    // the byte preflight in `page_from_store` below.
    restore_event_internal(&authority, row, false)
        .map(|_| ())
        .map_err(|_| StoreError::Corruption)
}

fn validate_metadata_row(
    authority: JournalSessionAuthority,
    row: &SemanticJournalFactRef<'_>,
) -> Result<(), StoreError> {
    if row.redaction_class == JournalRedactionClass::NeverPersist.as_str() {
        return Err(StoreError::Corruption);
    }
    reject_display_bound(row.delivery_id, MAX_DELIVERY_ID_BYTES)
        .map_err(|_| StoreError::Corruption)?;
    if let Some(provider_event_id) = row.provider_event_id {
        reject_display_bound(provider_event_id, MAX_PROVIDER_EVENT_ID_BYTES)
            .map_err(|_| StoreError::Corruption)?;
    }
    validate_borrowed_persisted_body(authority, row).map_err(|_| StoreError::Corruption)?;
    Ok(())
}

/// Validate every persisted body while it is still borrowed from SQLite. This
/// is the page transaction's global integrity pass: it covers rows that are
/// runtime-only, beyond max_items, or rejected by the page byte cap without
/// allocating an owned row, payload, event, or fact. The admission preflight
/// may then reuse the same bounded parser for the candidate projection.
fn validate_borrowed_persisted_body(
    authority: JournalSessionAuthority,
    row: &SemanticJournalFactRef<'_>,
) -> Result<(), JournalError> {
    if row.payload.is_empty() || row.payload.len() > MAX_JOURNAL_DOCUMENT_BYTES {
        return Err(JournalError::Oversized);
    }
    let body = RawMessagePack::new(row.payload).persisted_body()?;
    let schema_version =
        u32::try_from(row.schema_version).map_err(|_| JournalError::UnsupportedSchemaVersion)?;
    let sequence = u64::try_from(row.sequence).map_err(|_| JournalError::InvalidEnvelope)?;
    if sequence == 0 || row.occurred_at_ms < 0 || row.ingested_at_ms < 0 {
        return Err(JournalError::InvalidEnvelope);
    }
    let kind = parse_kind(row.kind)?;
    if schema_version != JOURNAL_SCHEMA_VERSION && kind != JournalSemanticKind::UnknownProviderEvent
    {
        return Err(JournalError::UnsupportedSchemaVersion);
    }
    let visibility = parse_visibility(row.visibility)?;
    let redaction_class = parse_redaction(row.redaction_class)?;
    if redaction_class == JournalRedactionClass::NeverPersist {
        return Err(JournalError::InvalidEnvelope);
    }
    match row.privacy_class {
        "local_only" | "shareable" => {}
        _ => return Err(JournalError::InvalidEnvelope),
    }
    if body.provider_event_id != row.provider_event_id {
        return Err(JournalError::InvalidEnvelope);
    }
    if let Some(text) = body.text {
        reject_display_bound(text, MAX_JOURNAL_TEXT_BYTES)?;
    }
    validate_raw_extensions(&body.extensions)?;
    validate_raw_semantic_payload(&kind, &body.payload)?;
    if let Some(source_type) = body.unknown_source_type {
        reject_display_bound(source_type, MAX_SOURCE_TYPE_BYTES)?;
    }
    if let Some(diagnostic_ref) = body.unknown_diagnostic_ref {
        validate_diagnostic_ref(diagnostic_ref)?;
    }
    match kind {
        JournalSemanticKind::UnknownProviderEvent => {
            let Some(unknown_schema_version) = body.unknown_schema_version else {
                return Err(JournalError::InvalidEnvelope);
            };
            let RawPayloadShape::Unknown {
                provider,
                source_type,
                schema_version: payload_schema_version,
                diagnostic_ref,
            } = body.payload
            else {
                return Err(JournalError::InvalidEnvelope);
            };
            if provider != provider_kind_sql(authority.provider)
                || payload_schema_version != unknown_schema_version
                || payload_schema_version != schema_version
                || body.unknown_source_type != Some(source_type)
                || body.unknown_diagnostic_ref != Some(diagnostic_ref)
                || visibility != JournalVisibility::Diagnostic
            {
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
    Ok(())
}

fn validate_raw_extensions(extensions: &RawPersistedExtensions<'_>) -> Result<(), JournalError> {
    for value in [
        extensions.hook_event_name,
        extensions.codex_item,
        extensions.cursor_surface,
    ]
    .into_iter()
    .flatten()
    {
        reject_display_bound(value, MAX_EXTENSION_VALUE_BYTES)?;
    }
    Ok(())
}

fn validate_raw_semantic_payload(
    kind: &JournalSemanticKind,
    payload: &RawPayloadShape<'_>,
) -> Result<(), JournalError> {
    let matches_kind = matches!(
        (kind, payload),
        (
            JournalSemanticKind::UserMessage,
            RawPayloadShape::UserMessage { .. }
        ) | (
            JournalSemanticKind::AssistantText,
            RawPayloadShape::AssistantText { .. }
        ) | (
            JournalSemanticKind::ReasoningSummary,
            RawPayloadShape::ReasoningSummary { .. }
        ) | (
            JournalSemanticKind::ToolCall,
            RawPayloadShape::ToolCall { .. }
        ) | (
            JournalSemanticKind::ToolResult,
            RawPayloadShape::ToolResult { .. }
        ) | (
            JournalSemanticKind::ApprovalRequest,
            RawPayloadShape::ApprovalRequest { .. }
        ) | (
            JournalSemanticKind::ApprovalResult,
            RawPayloadShape::ApprovalResult { .. }
        ) | (
            JournalSemanticKind::Question,
            RawPayloadShape::Question { .. }
        ) | (
            JournalSemanticKind::PlanStep,
            RawPayloadShape::PlanStep { .. }
        ) | (
            JournalSemanticKind::UsageObservation,
            RawPayloadShape::UsageObservation { .. }
        ) | (JournalSemanticKind::Error, RawPayloadShape::Error { .. })
            | (
                JournalSemanticKind::TurnState,
                RawPayloadShape::TurnState { .. }
            )
            | (
                JournalSemanticKind::SessionState,
                RawPayloadShape::SessionState { .. }
            )
            | (
                JournalSemanticKind::ArtifactReference,
                RawPayloadShape::ArtifactReference { .. }
            )
            | (
                JournalSemanticKind::UnknownProviderEvent,
                RawPayloadShape::Unknown { .. }
            )
    );
    if !matches_kind {
        return Err(JournalError::InvalidEnvelope);
    }
    match payload {
        RawPayloadShape::UserMessage { text }
        | RawPayloadShape::AssistantText { text }
        | RawPayloadShape::ReasoningSummary { text } => {
            reject_display_bound(text, MAX_JOURNAL_TEXT_BYTES)?;
        }
        RawPayloadShape::ToolCall { tool_name, call_id } => {
            reject_display_bound(tool_name, MAX_TOOL_NAME_BYTES)?;
            reject_display_bound(call_id, MAX_CALL_ID_BYTES)?;
        }
        RawPayloadShape::ToolResult { call_id, status } => {
            reject_display_bound(call_id, MAX_CALL_ID_BYTES)?;
            reject_display_bound(status, MAX_SOURCE_TYPE_BYTES)?;
        }
        RawPayloadShape::ApprovalRequest {
            request_id,
            summary,
        } => {
            reject_display_bound(request_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(summary, MAX_JOURNAL_TEXT_BYTES)?;
        }
        RawPayloadShape::ApprovalResult {
            request_id,
            decision,
        } => {
            reject_display_bound(request_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(decision, MAX_SOURCE_TYPE_BYTES)?;
        }
        RawPayloadShape::Question {
            question_id,
            prompt,
            options,
        } => {
            reject_display_bound(question_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(prompt, MAX_JOURNAL_TEXT_BYTES)?;
            if options.len > MAX_QUESTION_OPTIONS {
                return Err(JournalError::TooLong);
            }
            for option in &options.values[..options.len] {
                reject_display_bound(option, MAX_EXTENSION_VALUE_BYTES)?;
            }
        }
        RawPayloadShape::PlanStep {
            step_id,
            title,
            status,
        } => {
            reject_display_bound(step_id, MAX_REQUEST_ID_BYTES)?;
            reject_display_bound(title, MAX_JOURNAL_TEXT_BYTES)?;
            reject_display_bound(status, MAX_SOURCE_TYPE_BYTES)?;
        }
        RawPayloadShape::UsageObservation { remaining_percent } => {
            if remaining_percent.is_some_and(|percent| percent > 100) {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        RawPayloadShape::Error { code, message } => {
            reject_display_bound(code, MAX_SOURCE_TYPE_BYTES)?;
            reject_display_bound(message, MAX_JOURNAL_TEXT_BYTES)?;
        }
        RawPayloadShape::TurnState { state } => {
            reject_display_bound(state, MAX_SOURCE_TYPE_BYTES)?;
            if !matches!(*state, "started" | "completed" | "failed") {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        RawPayloadShape::SessionState { state } => {
            reject_display_bound(state, MAX_SOURCE_TYPE_BYTES)?;
            if !matches!(*state, "open" | "closed") {
                return Err(JournalError::InvalidEnvelope);
            }
        }
        RawPayloadShape::ArtifactReference { label } => {
            reject_display_bound(label, MAX_JOURNAL_TEXT_BYTES)?;
        }
        RawPayloadShape::Unknown {
            provider,
            source_type,
            schema_version,
            diagnostic_ref,
        } => {
            reject_display_bound(provider, MAX_SOURCE_TYPE_BYTES)?;
            reject_display_bound(source_type, MAX_SOURCE_TYPE_BYTES)?;
            if *schema_version == 0 {
                return Err(JournalError::InvalidEnvelope);
            }
            validate_diagnostic_ref(diagnostic_ref)?;
        }
    }
    Ok(())
}

fn validate_diagnostic_ref(value: &str) -> Result<(), JournalError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(JournalError::InvalidEnvelope);
    }
    Ok(())
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
