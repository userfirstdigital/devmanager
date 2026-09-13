//! Pure checkpoint and targeted-recovery state machine.
//!
//! This module deliberately does not know about paths, Git, the file service,
//! SQLite, or the running application.  A future host adapter must implement
//! [`SealedWorkspaceAuthority`] and [`AtomicCheckpointStateStore`].  The
//! authority is the only component allowed to resolve an opaque object
//! identity to a file or artifact.
//!
//! The production composition is deliberately a conditional union: a host
//! must provide both [`SealedWorkspaceAuthority`] and
//! [`AtomicCheckpointStateStore`] implementations in the same release crate
//! before [`CheckpointRegistry`] can be used.  The unit harness supplies only
//! test adapters; this core does not claim FileService, Git, SQLite, or path
//! ownership in a production build.  Missing adapters remain typed boundary
//! outcomes (`AuthorityFailure::Unsupported` / `StateStoreFailure::Unavailable`)
//! rather than an invented fallback.

#[cfg(test)]
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[cfg(test)]
use rusqlite::{params, Connection, TransactionBehavior};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::id::TaskId;

const CHECKPOINT_DOMAIN: &[u8] = b"devmanager-checkpoint-v1\0";
const OBJECT_DOMAIN: &[u8] = b"devmanager-checkpoint-object-v1\0";
const PLAN_DOMAIN: &[u8] = b"devmanager-recovery-plan-v1\0";
const MAX_PROJECTION_PLANS: usize = 4096;
const HARD_MAX_FILES: usize = 4096;
const HARD_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
const HARD_MAX_HISTORY: usize = 256;
const HARD_MAX_CONTENT_RECORDS: usize = 1_048_576;
const HARD_MAX_CONTENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const HARD_MAX_EXTERNAL_BYTES: usize = 1024 * 1024;
const HARD_MAX_EXTERNAL_DEPTH: usize = 32;
const HARD_MAX_STATE_WIRE_BYTES: usize = 16 * 1024 * 1024;
const HARD_MAX_PERSISTENCE_DEPTH: usize = 32;
const HARD_MAX_PERSISTENCE_VARIABLE_BYTES: usize = 1024 * 1024;
const PERSISTED_CODEC_VERSION: u16 = 1;

// These boundaries are deliberately sealed.  The blanket implementations are
// compiled only into the in-crate unit harness; a production build has no
// implementation path until the host supplies a policy-checked adapter.
pub(crate) mod private {
    pub(crate) trait AuthoritySeal {}
    pub(crate) trait StoreSeal {}

    #[cfg(test)]
    impl<T> AuthoritySeal for T {}

    #[cfg(test)]
    impl<T> StoreSeal for T {}
}

/// Capture and registry limits.  Every limit is enforced before bytes are
/// retained.  The history cap also bounds the amount of content that the
/// registry can retain when each checkpoint is captured at its byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointLimits {
    max_files: usize,
    max_total_bytes: u64,
    max_object_bytes: u64,
    max_history: usize,
}

impl CheckpointLimits {
    pub const DEFAULT: Self = Self {
        max_files: 256,
        max_total_bytes: 8 * 1024 * 1024,
        max_object_bytes: 1024 * 1024,
        max_history: 32,
    };

    pub fn new(
        max_files: usize,
        max_total_bytes: u64,
        max_object_bytes: u64,
        max_history: usize,
    ) -> Self {
        Self {
            max_files,
            max_total_bytes,
            max_object_bytes,
            max_history,
        }
    }

    pub fn max_files(self) -> usize {
        self.max_files
    }

    pub fn max_total_bytes(self) -> u64 {
        self.max_total_bytes
    }

    pub fn max_object_bytes(self) -> u64 {
        self.max_object_bytes
    }

    pub fn max_history(self) -> usize {
        self.max_history
    }

    fn validate(self) -> Result<(), CheckpointFailure> {
        let content_records = self.max_history.checked_mul(self.max_files);
        let content_bytes = u64::try_from(self.max_history)
            .ok()
            .and_then(|history| history.checked_mul(self.max_total_bytes));
        if self.max_files == 0
            || self.max_total_bytes == 0
            || self.max_object_bytes == 0
            || self.max_history == 0
            || self.max_files > HARD_MAX_FILES
            || self.max_total_bytes > HARD_MAX_TOTAL_BYTES
            || self.max_object_bytes > HARD_MAX_OBJECT_BYTES
            || self.max_history > HARD_MAX_HISTORY
            || content_records.map_or(true, |records| records > HARD_MAX_CONTENT_RECORDS)
            || content_bytes.map_or(true, |bytes| bytes > HARD_MAX_CONTENT_BYTES)
        {
            return Err(CheckpointFailure::InvalidLimits);
        }
        Ok(())
    }

    fn state_limits(self) -> StateLoadLimits {
        StateLoadLimits {
            max_checkpoints: self.max_history,
            max_plans: self.max_history,
            max_content_records: self.max_history.saturating_mul(self.max_files).max(1),
            max_object_bytes: self.max_object_bytes,
            max_content_bytes: self
                .max_history
                .try_into()
                .unwrap_or(u64::MAX)
                .saturating_mul(self.max_total_bytes)
                .max(self.max_total_bytes),
            max_wire_bytes: state_wire_limit(
                self.max_files,
                self.max_history,
                self.max_history.saturating_mul(self.max_files).max(1),
            ),
            max_nested_items: self.max_files,
        }
    }
}

/// A host-issued opaque workspace identity.  It contains no path and cannot
/// be used to open anything without a host authority.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceToken([u8; 32]);

impl WorkspaceToken {
    pub fn new() -> Self {
        Self(opaque_bytes(b"workspace"))
    }
}

impl Default for WorkspaceToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WorkspaceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceToken(REDACTED)")
    }
}

/// All checkpoint operations are fenced by task, workspace, generation, and
/// action epoch.  A stale lease therefore cannot be reused for recovery.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckpointScope {
    task_id: TaskId,
    workspace: WorkspaceToken,
    generation: u64,
    action_epoch: u64,
}

impl CheckpointScope {
    pub fn new(
        task_id: TaskId,
        workspace: WorkspaceToken,
        generation: u64,
        action_epoch: u64,
    ) -> Self {
        Self {
            task_id,
            workspace,
            generation,
            action_epoch,
        }
    }

    pub fn task_id(self) -> TaskId {
        self.task_id
    }

    pub fn workspace(self) -> WorkspaceToken {
        self.workspace
    }

    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn action_epoch(self) -> u64 {
        self.action_epoch
    }
}

impl fmt::Debug for CheckpointScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointScope")
            .field("task_id", &"REDACTED")
            .field("workspace", &self.workspace)
            .field("generation", &self.generation)
            .field("action_epoch", &self.action_epoch)
            .finish()
    }
}

/// Opaque host-issued identity for an agent or provider turn.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentToken([u8; 32]);

impl AgentToken {
    pub fn new() -> Self {
        Self(opaque_bytes(b"agent"))
    }
}

impl Default for AgentToken {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AgentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentToken(REDACTED)")
    }
}

/// Opaque identity for one explicitly supplied file or artifact.  No path,
/// URI, or path-like string is represented here.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    pub fn new() -> Self {
        Self(opaque_bytes(b"object"))
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObjectId(REDACTED)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    File,
    Artifact,
}

/// A typed object identity that must be supplied by the caller.  The
/// authority, not this module, decides what it denotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectRef {
    id: ObjectId,
    kind: ObjectKind,
}

impl ObjectRef {
    pub fn file(id: ObjectId) -> Self {
        Self {
            id,
            kind: ObjectKind::File,
        }
    }

    pub fn artifact(id: ObjectId) -> Self {
        Self {
            id,
            kind: ObjectKind::Artifact,
        }
    }

    pub fn id(self) -> ObjectId {
        self.id
    }

    pub fn kind(self) -> ObjectKind {
        self.kind
    }
}

/// An immutable revision identity supplied by a future Git/host adapter.
/// Checkpoint capture stores it as metadata; it is not interpreted as a path
/// or a Git command by this module.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceRevision([u8; 32]);

impl WorkspaceRevision {
    pub fn new() -> Self {
        Self(opaque_bytes(b"revision"))
    }

    /// Construct a revision from a host-issued digest.  The bytes are an
    /// opaque identity and are never treated as a locator.
    pub fn from_host_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }
}

impl Default for WorkspaceRevision {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WorkspaceRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceRevision(REDACTED)")
    }
}

/// A revision-safe representation of one object returned by the sealed
/// authority.  Present bytes are only transient capture/apply input; durable
/// checkpoint manifests retain content addresses instead.
#[derive(Clone, PartialEq, Eq)]
pub enum ObjectState {
    Absent,
    Present(Vec<u8>),
}

impl fmt::Debug for ObjectState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("Absent"),
            Self::Present(bytes) => f
                .debug_struct("Present")
                .field("bytes", &bytes.len())
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SealedObject {
    object: ObjectRef,
    state: ObjectState,
}

impl SealedObject {
    pub fn new(object: ObjectRef, state: ObjectState) -> Self {
        Self { object, state }
    }

    pub fn object(&self) -> ObjectRef {
        self.object
    }

    pub fn state(&self) -> &ObjectState {
        &self.state
    }

    pub fn fingerprint(&self) -> ObjectFingerprint {
        fingerprint(self.object, &self.state)
    }
}

impl fmt::Debug for SealedObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedObject")
            .field("object", &self.object)
            .finish()
    }
}

/// Exact content/presence precondition used by a targeted recovery operation.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectFingerprint {
    digest: [u8; 32],
    bytes: u64,
    present: bool,
}

impl ObjectFingerprint {
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }

    pub fn bytes(self) -> u64 {
        self.bytes
    }

    pub fn is_present(self) -> bool {
        self.present
    }
}

impl fmt::Debug for ObjectFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObjectFingerprint(REDACTED)")
    }
}

/// Content address retained by a durable manifest.  The bytes themselves are
/// held only by the state store's content-addressed blob table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentAddress {
    digest: [u8; 32],
    bytes: u64,
}

impl ContentAddress {
    pub fn digest(self) -> [u8; 32] {
        self.digest
    }

    pub fn bytes(self) -> u64 {
        self.bytes
    }
}

impl fmt::Debug for ContentAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentAddress(REDACTED)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointReason {
    BeforeTurn,
    AfterCompletion,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CaptureContext {
    reason: CheckpointReason,
    agent: AgentToken,
    turn: u64,
    captured_at_ms: i64,
}

impl CaptureContext {
    pub fn new(
        reason: CheckpointReason,
        agent: AgentToken,
        turn: u64,
        captured_at_ms: i64,
    ) -> Self {
        Self {
            reason,
            agent,
            turn,
            captured_at_ms,
        }
    }

    pub fn reason(self) -> CheckpointReason {
        self.reason
    }

    pub fn agent(self) -> AgentToken {
        self.agent
    }

    pub fn turn(self) -> u64 {
        self.turn
    }

    pub fn captured_at_ms(self) -> i64 {
        self.captured_at_ms
    }
}

impl fmt::Debug for CaptureContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureContext")
            .field("reason", &self.reason)
            .field("agent", &self.agent)
            .field("turn", &self.turn)
            .field("captured_at_ms", &self.captured_at_ms)
            .finish()
    }
}

pub struct CaptureRequest {
    scope: CheckpointScope,
    objects: Vec<ObjectRef>,
    context: CaptureContext,
}

impl CaptureRequest {
    pub fn new(scope: CheckpointScope, objects: Vec<ObjectRef>, context: CaptureContext) -> Self {
        Self {
            scope,
            objects,
            context,
        }
    }

    pub fn scope(&self) -> CheckpointScope {
        self.scope
    }

    pub fn objects(&self) -> &[ObjectRef] {
        &self.objects
    }

    pub fn context(&self) -> CaptureContext {
        self.context
    }
}

/// Cancellation/deadline input is checked between every authority read and
/// before durable state replacement.
#[derive(Clone, Default)]
pub struct CaptureBudget {
    deadline: Option<Instant>,
    cancelled: Option<Arc<AtomicBool>>,
    work: Option<Arc<AtomicU64>>,
}

/// Bounds supplied to a state-store read.  A production store must enforce
/// these while streaming its durable representation, before deserializing or
/// allocating a state image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateLoadLimits {
    max_checkpoints: usize,
    max_plans: usize,
    max_content_records: usize,
    max_object_bytes: u64,
    max_content_bytes: u64,
    max_wire_bytes: usize,
    max_nested_items: usize,
}

impl StateLoadLimits {
    pub fn max_checkpoints(self) -> usize {
        self.max_checkpoints
    }

    pub fn max_plans(self) -> usize {
        self.max_plans
    }

    pub fn max_content_records(self) -> usize {
        self.max_content_records
    }

    pub fn max_object_bytes(self) -> u64 {
        self.max_object_bytes
    }

    pub fn max_content_bytes(self) -> u64 {
        self.max_content_bytes
    }

    /// Maximum encoded state bytes a store may copy for this caller.  This is
    /// separate from the hard codec ceiling so an adapter can reject a
    /// caller-oversize row before materializing its payload.
    pub fn max_wire_bytes(self) -> usize {
        self.max_wire_bytes
    }

    pub fn max_nested_items(self) -> usize {
        self.max_nested_items
    }
}

impl CaptureBudget {
    pub fn unbounded() -> Self {
        Self::default()
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_cancellation(mut self, cancelled: Arc<AtomicBool>) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    /// Shares a finite work budget with every clone.  A unit is consumed for
    /// each authority operation and durable state transition, so capture,
    /// preview, apply, and crash replay can use one bounded lease.
    pub fn with_work_limit(mut self, units: u64) -> Self {
        self.work = Some(Arc::new(AtomicU64::new(units)));
        self
    }

    /// Checks cancellation and the absolute deadline without charging a work
    /// unit.  Adapters use this while polling a lock or other wait so a single
    /// logical operation does not exhaust its work lease merely by retrying.
    pub(crate) fn check_control(&self) -> Result<(), CheckpointFailure> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(CheckpointFailure::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(CheckpointFailure::DeadlineExceeded);
        }
        if self
            .work
            .as_ref()
            .is_some_and(|work| work.load(Ordering::Acquire) == 0)
        {
            return Err(CheckpointFailure::WorkLimitExceeded);
        }
        Ok(())
    }

    /// Crate-internal adapters must call this at every operation and effect
    /// boundary.  It stays out of the external API so a caller cannot mint or
    /// alter a host-issued budget.
    pub(crate) fn check(&self) -> Result<(), CheckpointFailure> {
        self.check_control()?;
        if let Some(work) = &self.work {
            let mut remaining = work.load(Ordering::Acquire);
            loop {
                if remaining == 0 {
                    return Err(CheckpointFailure::WorkLimitExceeded);
                }
                match work.compare_exchange_weak(
                    remaining,
                    remaining - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(current) => remaining = current,
                }
            }
        }
        Ok(())
    }

    pub(crate) fn remaining_duration(&self) -> Option<Duration> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Some(Duration::ZERO);
        }
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
}

/// Shared deadline, cancellation, and work-meter contract used by every
/// authority and durable-store operation.  `CaptureBudget` remains the
/// compatibility name for callers of the checkpoint API.
pub type OperationBudget = CaptureBudget;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityFailure {
    Unavailable,
    Conflict,
    Oversize,
    Unsupported,
}

/// Future FileService/Git/artifact code must implement this boundary.  It is
/// intentionally identity-only: no method receives or returns a raw path.
/// This trait is one half of the production-only adapter union documented at
/// the module boundary; its test-only seal blanket implementation is not a
/// production integration.
#[allow(private_bounds)]
pub trait SealedWorkspaceAuthority: private::AuthoritySeal {
    fn scope(&self, budget: &OperationBudget) -> &CheckpointScope;
    fn revision(&self, budget: &OperationBudget) -> WorkspaceRevision;
    /// Reads one explicitly identified object under a hard byte cap.  A
    /// production adapter must enforce the cap while streaming, before
    /// allocating or returning object bytes.
    fn read_bounded(
        &mut self,
        object: &ObjectRef,
        max_bytes: u64,
        budget: &OperationBudget,
    ) -> Result<SealedObject, AuthorityFailure>;
    fn write(
        &mut self,
        object: &ObjectRef,
        bytes: &[u8],
        expected: &ObjectFingerprint,
        budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure>;
    fn remove(
        &mut self,
        object: &ObjectRef,
        expected: &ObjectFingerprint,
        budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure>;
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckpointId(Uuid);

impl CheckpointId {
    fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Debug for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckpointId(REDACTED)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanId(Uuid);

impl fmt::Debug for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlanId(REDACTED)")
    }
}

#[derive(Clone, PartialEq, Eq)]
enum CheckpointObjectState {
    Absent,
    Present(ContentAddress),
}

#[derive(Clone, PartialEq, Eq)]
pub struct CheckpointObject {
    object: ObjectRef,
    state: CheckpointObjectState,
    fingerprint: ObjectFingerprint,
}

impl CheckpointObject {
    pub fn object(&self) -> ObjectRef {
        self.object
    }

    pub fn fingerprint(&self) -> ObjectFingerprint {
        self.fingerprint
    }

    pub fn content(&self) -> Option<ContentAddress> {
        match self.state {
            CheckpointObjectState::Absent => None,
            CheckpointObjectState::Present(address) => Some(address),
        }
    }

    pub fn is_present(&self) -> bool {
        matches!(self.state, CheckpointObjectState::Present(_))
    }
}

impl fmt::Debug for CheckpointObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointObject")
            .field("object", &self.object)
            .field("state", &self.state_label())
            .finish()
    }
}

impl CheckpointObject {
    fn state_label(&self) -> &'static str {
        match self.state {
            CheckpointObjectState::Absent => "absent",
            CheckpointObjectState::Present(_) => "present",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CheckpointManifest {
    scope: CheckpointScope,
    revision: WorkspaceRevision,
    context: CaptureContext,
    objects: Vec<CheckpointObject>,
    fingerprint: [u8; 32],
    total_bytes: u64,
}

impl CheckpointManifest {
    pub fn scope(&self) -> CheckpointScope {
        self.scope
    }

    pub fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub fn context(&self) -> CaptureContext {
        self.context
    }

    pub fn objects(&self) -> &[CheckpointObject] {
        &self.objects
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Checkpoint {
    id: CheckpointId,
    manifest: CheckpointManifest,
}

impl Checkpoint {
    pub fn id(&self) -> CheckpointId {
        self.id
    }

    pub fn manifest(&self) -> &CheckpointManifest {
        &self.manifest
    }
}

impl fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checkpoint")
            .field("id", &self.id)
            .field("object_count", &self.manifest.objects.len())
            .field("total_bytes", &self.manifest.total_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryTarget {
    Absent,
    Present(ContentAddress),
}

#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryOperation {
    object: ObjectRef,
    expected: ObjectFingerprint,
    target: RecoveryTarget,
}

impl RecoveryOperation {
    pub fn object(&self) -> ObjectRef {
        self.object
    }

    pub fn expected(&self) -> ObjectFingerprint {
        self.expected
    }

    pub fn target(&self) -> RecoveryTarget {
        self.target
    }
}

impl fmt::Debug for RecoveryOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryOperation")
            .field("object", &self.object)
            .field("target", &self.target)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    id: PlanId,
    checkpoint_id: CheckpointId,
    scope: CheckpointScope,
    planned_revision: WorkspaceRevision,
    operations: Vec<RecoveryOperation>,
    fingerprint: [u8; 32],
}

impl RecoveryPlan {
    pub fn id(&self) -> PlanId {
        self.id
    }

    pub fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }

    pub fn scope(&self) -> CheckpointScope {
        self.scope
    }

    pub fn planned_revision(&self) -> WorkspaceRevision {
        self.planned_revision
    }

    pub fn operations(&self) -> &[RecoveryOperation] {
        &self.operations
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

impl fmt::Debug for RecoveryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryPlan")
            .field("id", &self.id)
            .field("checkpoint_id", &self.checkpoint_id)
            .field("scope", &self.scope)
            .field("operation_count", &self.operations.len())
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RecoveryApproval {
    plan_id: PlanId,
    scope: CheckpointScope,
    fingerprint: [u8; 32],
    nonce: [u8; 32],
    attempt_generation: Option<[u8; 32]>,
    expected_state_version: u64,
}

impl fmt::Debug for RecoveryApproval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryApproval(REDACTED)")
    }
}

pub struct RestoreRequest {
    scope: CheckpointScope,
    checkpoint_id: CheckpointId,
    objects: Vec<ObjectRef>,
}

impl RestoreRequest {
    pub fn new(
        scope: CheckpointScope,
        checkpoint_id: CheckpointId,
        objects: Vec<ObjectRef>,
    ) -> Self {
        Self {
            scope,
            checkpoint_id,
            objects,
        }
    }

    pub fn scope(&self) -> CheckpointScope {
        self.scope
    }

    pub fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }

    pub fn objects(&self) -> &[ObjectRef] {
        &self.objects
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryApplyOutcome {
    Applied { changed: usize },
    Replayed { changed: usize },
    AlreadyApplied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictTransitionOutcome {
    Completed,
    Conflicted,
    AlreadyApplied,
    Superseded,
}

#[derive(Clone, Copy)]
struct AttemptOwnership {
    approval_nonce: [u8; 32],
    attempt_generation: [u8; 32],
}

enum OperationClaimOutcome {
    Claimed { completed: bool },
    AlreadyApplied,
    Conflicted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryPlanStatus {
    Planned,
    Applying,
    Applied,
    Conflicted,
}

/// Safe status metadata for UI/IPC.  It intentionally contains no plan
/// fingerprint, object fingerprint, content address, or bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatusProjection {
    Planned,
    Applying,
    Conflicted,
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPlanProjection {
    id: PlanId,
    checkpoint_id: CheckpointId,
    scope: CheckpointScope,
    status: RecoveryStatusProjection,
    completed_operations: usize,
    operation_count: usize,
    tombstoned: bool,
}

impl RecoveryPlanProjection {
    pub fn id(self) -> PlanId {
        self.id
    }

    pub fn checkpoint_id(self) -> CheckpointId {
        self.checkpoint_id
    }

    pub fn scope(self) -> CheckpointScope {
        self.scope
    }

    pub fn status(self) -> RecoveryStatusProjection {
        self.status
    }

    pub fn completed_operations(self) -> usize {
        self.completed_operations
    }

    pub fn operation_count(self) -> usize {
        self.operation_count
    }

    pub fn tombstoned(self) -> bool {
        self.tombstoned
    }
}

/// Strict, metadata-only checkpoint projection intended for external
/// transport.  It deliberately omits content addresses, fingerprints,
/// operation preconditions, and all object bytes.
#[derive(Debug, Clone, Serialize)]
pub struct DurableCheckpointMetadata {
    version: u64,
    checkpoints: Vec<ExternalCheckpointMetadata>,
    content_record_count: usize,
    content_bytes: u64,
    plans: Vec<ExternalPlanMetadata>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalCheckpointMetadata {
    id: String,
    scope: ExternalScopeMetadata,
    object_count: usize,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExternalScopeMetadata {
    generation: u64,
    action_epoch: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum ExternalRecoveryStatus {
    Planned,
    Applying,
    Conflicted,
    Applied,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalPlanMetadata {
    id: String,
    checkpoint_id: String,
    scope: ExternalScopeMetadata,
    status: ExternalRecoveryStatus,
    completed_operations: usize,
    operation_count: usize,
    tombstoned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMetadataDecodeError {
    TooLarge,
    TooDeep,
    Invalid,
    LimitExceeded,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableCheckpointMetadataWire {
    version: u64,
    #[serde(deserialize_with = "deserialize_bounded_external_checkpoints")]
    checkpoints: Vec<ExternalCheckpointMetadataWire>,
    content_record_count: usize,
    content_bytes: u64,
    #[serde(deserialize_with = "deserialize_bounded_external_plans")]
    plans: Vec<ExternalPlanMetadataWire>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalCheckpointMetadataWire {
    id: String,
    scope: ExternalScopeMetadataWire,
    object_count: usize,
    total_bytes: u64,
}

#[derive(Debug, Copy, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalScopeMetadataWire {
    generation: u64,
    action_epoch: u64,
}

#[derive(Debug, Copy, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
enum ExternalRecoveryStatusWire {
    Planned,
    Applying,
    Conflicted,
    Applied,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalPlanMetadataWire {
    id: String,
    checkpoint_id: String,
    scope: ExternalScopeMetadataWire,
    status: ExternalRecoveryStatusWire,
    completed_operations: usize,
    operation_count: usize,
    tombstoned: bool,
}

impl DurableCheckpointMetadata {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn checkpoints(&self) -> &[ExternalCheckpointMetadata] {
        &self.checkpoints
    }

    pub fn content_record_count(&self) -> usize {
        self.content_record_count
    }

    pub fn content_bytes(&self) -> u64 {
        self.content_bytes
    }

    pub fn plans(&self) -> &[ExternalPlanMetadata] {
        &self.plans
    }

    /// Bounded external JSON ingress.  The public state itself is not a
    /// serde codec; callers receive only this strict metadata projection.
    pub fn decode_json(input: &[u8]) -> Result<Self, CheckpointMetadataDecodeError> {
        if input.len() > HARD_MAX_EXTERNAL_BYTES {
            return Err(CheckpointMetadataDecodeError::TooLarge);
        }
        if json_depth_exceeds(input, HARD_MAX_EXTERNAL_DEPTH) {
            return Err(CheckpointMetadataDecodeError::TooDeep);
        }
        let wire: DurableCheckpointMetadataWire =
            serde_json::from_slice(input).map_err(|_| CheckpointMetadataDecodeError::Invalid)?;
        let metadata = DurableCheckpointMetadata {
            version: wire.version,
            checkpoints: wire
                .checkpoints
                .into_iter()
                .map(|checkpoint| {
                    Ok(ExternalCheckpointMetadata {
                        id: validate_external_id(&checkpoint.id)?,
                        scope: ExternalScopeMetadata {
                            generation: checkpoint.scope.generation,
                            action_epoch: checkpoint.scope.action_epoch,
                        },
                        object_count: checkpoint.object_count,
                        total_bytes: checkpoint.total_bytes,
                    })
                })
                .collect::<Result<Vec<_>, CheckpointMetadataDecodeError>>()?,
            content_record_count: wire.content_record_count,
            content_bytes: wire.content_bytes,
            plans: wire
                .plans
                .into_iter()
                .map(|plan| {
                    Ok(ExternalPlanMetadata {
                        id: validate_external_id(&plan.id)?,
                        checkpoint_id: validate_external_id(&plan.checkpoint_id)?,
                        scope: ExternalScopeMetadata {
                            generation: plan.scope.generation,
                            action_epoch: plan.scope.action_epoch,
                        },
                        status: match plan.status {
                            ExternalRecoveryStatusWire::Planned => ExternalRecoveryStatus::Planned,
                            ExternalRecoveryStatusWire::Applying => {
                                ExternalRecoveryStatus::Applying
                            }
                            ExternalRecoveryStatusWire::Conflicted => {
                                ExternalRecoveryStatus::Conflicted
                            }
                            ExternalRecoveryStatusWire::Applied => ExternalRecoveryStatus::Applied,
                        },
                        completed_operations: plan.completed_operations,
                        operation_count: plan.operation_count,
                        tombstoned: plan.tombstoned,
                    })
                })
                .collect::<Result<Vec<_>, CheckpointMetadataDecodeError>>()?,
        };
        metadata.validate()
    }

    fn validate(self) -> Result<Self, CheckpointMetadataDecodeError> {
        if self.checkpoints.len() > HARD_MAX_HISTORY
            || self.plans.len() > HARD_MAX_HISTORY
            || self.content_record_count > HARD_MAX_CONTENT_RECORDS
            || self.content_bytes > HARD_MAX_CONTENT_BYTES
            || self.checkpoints.iter().any(|checkpoint| {
                checkpoint.object_count > HARD_MAX_FILES
                    || checkpoint.total_bytes > HARD_MAX_TOTAL_BYTES
            })
            || self.plans.iter().any(|plan| {
                plan.operation_count > HARD_MAX_FILES
                    || plan.completed_operations > plan.operation_count
            })
        {
            return Err(CheckpointMetadataDecodeError::LimitExceeded);
        }
        Ok(self)
    }
}

fn validate_external_id(value: &str) -> Result<String, CheckpointMetadataDecodeError> {
    if value.len() > 64 || Uuid::parse_str(value).is_err() {
        return Err(CheckpointMetadataDecodeError::Invalid);
    }
    Ok(value.to_owned())
}

fn checkpoint_id_from_external(value: &str) -> Result<CheckpointId, CheckpointMetadataDecodeError> {
    Uuid::parse_str(value)
        .map(CheckpointId)
        .map_err(|_| CheckpointMetadataDecodeError::Invalid)
}

fn plan_id_from_external(value: &str) -> Result<PlanId, CheckpointMetadataDecodeError> {
    Uuid::parse_str(value)
        .map(PlanId)
        .map_err(|_| CheckpointMetadataDecodeError::Invalid)
}

impl ExternalCheckpointMetadata {
    pub fn id(&self) -> CheckpointId {
        checkpoint_id_from_external(&self.id).expect("validated external checkpoint id")
    }

    pub fn scope(&self) -> ExternalScopeMetadata {
        self.scope
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

impl ExternalScopeMetadata {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn action_epoch(self) -> u64 {
        self.action_epoch
    }
}

impl ExternalPlanMetadata {
    pub fn id(&self) -> PlanId {
        plan_id_from_external(&self.id).expect("validated external plan id")
    }

    pub fn checkpoint_id(&self) -> CheckpointId {
        checkpoint_id_from_external(&self.checkpoint_id).expect("validated external checkpoint id")
    }

    pub fn scope(&self) -> ExternalScopeMetadata {
        self.scope
    }

    pub fn status(&self) -> ExternalRecoveryStatus {
        self.status
    }

    pub fn completed_operations(&self) -> usize {
        self.completed_operations
    }

    pub fn operation_count(&self) -> usize {
        self.operation_count
    }

    pub fn tombstoned(&self) -> bool {
        self.tombstoned
    }
}

#[derive(Clone)]
struct PlanRecord {
    plan: RecoveryPlan,
    status: RecoveryPlanStatus,
    completed: BTreeSet<usize>,
    active_attempt: Option<ActiveAttempt>,
    issued_approval_nonce: Option<[u8; 32]>,
    tombstoned: bool,
}

/// Durable ownership fence for one applying attempt. The operation id is
/// claimed before an authority effect and cleared only by its matching
/// completion CAS. These values never enter a projection or transport DTO.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ActiveAttempt {
    approval_nonce: [u8; 32],
    attempt_generation: [u8; 32],
    operation_id: Option<usize>,
    state_version: u64,
}

#[derive(Clone)]
struct ContentRecord {
    address: ContentAddress,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct DurableCheckpointStateWire {
    version: u64,
    checkpoints: Vec<Checkpoint>,
    contents: Vec<ContentRecord>,
    plans: Vec<PlanRecord>,
}

fn deserialize_bounded_vec<'de, D, T>(deserializer: D, max: usize) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVecVisitor<T> {
        max: usize,
        marker: std::marker::PhantomData<fn() -> T>,
    }

    impl<'de, T> Visitor<'de> for BoundedVecVisitor<T>
    where
        T: Deserialize<'de>,
    {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a sequence with at most {} entries", self.max)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            if sequence.size_hint().is_some_and(|hint| hint > self.max) {
                return Err(de::Error::custom("checkpoint sequence exceeds hard limit"));
            }
            let capacity = sequence.size_hint().unwrap_or(0).min(self.max);
            let mut values = Vec::with_capacity(capacity);
            while values.len() < self.max {
                match sequence.next_element()? {
                    Some(value) => values.push(value),
                    None => return Ok(values),
                }
            }
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("checkpoint sequence exceeds hard limit"));
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVecVisitor {
        max,
        marker: std::marker::PhantomData,
    })
}

fn deserialize_bounded_external_checkpoints<'de, D>(
    deserializer: D,
) -> Result<Vec<ExternalCheckpointMetadataWire>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_HISTORY)
}

fn deserialize_bounded_external_plans<'de, D>(
    deserializer: D,
) -> Result<Vec<ExternalPlanMetadataWire>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_HISTORY)
}

fn json_depth_exceeds(input: &[u8], max_depth: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in input {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                if depth > max_depth {
                    return true;
                }
            }
            b'}' | b']' => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    in_string || depth != 0
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedStateWire {
    codec_version: u16,
    state_version: u64,
    #[serde(deserialize_with = "deserialize_bounded_persisted_checkpoints")]
    checkpoints: Vec<PersistedCheckpoint>,
    #[serde(deserialize_with = "deserialize_bounded_persisted_contents")]
    contents: Vec<PersistedContentRecord>,
    #[serde(deserialize_with = "deserialize_bounded_persisted_plans")]
    plans: Vec<PersistedPlanRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedScope {
    task_id: [u8; 16],
    workspace: [u8; 32],
    generation: u64,
    action_epoch: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PersistedObjectKind {
    File,
    Artifact,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedObjectRef {
    id: [u8; 32],
    kind: PersistedObjectKind,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PersistedCheckpointReason {
    BeforeTurn,
    AfterCompletion,
    Manual,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedContext {
    reason: PersistedCheckpointReason,
    agent: [u8; 32],
    turn: u64,
    captured_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedFingerprint {
    digest: [u8; 32],
    bytes: u64,
    present: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedContentAddress {
    digest: [u8; 32],
    bytes: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PersistedObjectState {
    Absent,
    Present(PersistedContentAddress),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCheckpointObject {
    object: PersistedObjectRef,
    state: PersistedObjectState,
    fingerprint: PersistedFingerprint,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedManifest {
    scope: PersistedScope,
    revision: [u8; 32],
    context: PersistedContext,
    #[serde(deserialize_with = "deserialize_bounded_persisted_objects")]
    objects: Vec<PersistedCheckpointObject>,
    fingerprint: [u8; 32],
    total_bytes: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedCheckpoint {
    id: [u8; 16],
    manifest: PersistedManifest,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PersistedTarget {
    Absent,
    Present(PersistedContentAddress),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedOperation {
    object: PersistedObjectRef,
    expected: PersistedFingerprint,
    target: PersistedTarget,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPlan {
    id: [u8; 16],
    checkpoint_id: [u8; 16],
    scope: PersistedScope,
    planned_revision: [u8; 32],
    #[serde(deserialize_with = "deserialize_bounded_persisted_operations")]
    operations: Vec<PersistedOperation>,
    fingerprint: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PersistedPlanStatus {
    Planned,
    Applying,
    Applied,
    Conflicted,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedActiveAttempt {
    approval_nonce: [u8; 32],
    attempt_generation: [u8; 32],
    operation_id: Option<usize>,
    state_version: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPlanRecord {
    plan: PersistedPlan,
    status: PersistedPlanStatus,
    #[serde(deserialize_with = "deserialize_bounded_persisted_completed")]
    completed: Vec<usize>,
    active_attempt: Option<PersistedActiveAttempt>,
    issued_approval_nonce: Option<[u8; 32]>,
    tombstoned: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedContentRecord {
    address: PersistedContentAddress,
    bytes_len: u64,
}

fn deserialize_bounded_persisted_checkpoints<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedCheckpoint>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_HISTORY)
}

fn deserialize_bounded_persisted_contents<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedContentRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_CONTENT_RECORDS)
}

fn deserialize_bounded_persisted_plans<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedPlanRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_HISTORY)
}

fn deserialize_bounded_persisted_objects<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedCheckpointObject>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_FILES)
}

fn deserialize_bounded_persisted_operations<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedOperation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_FILES)
}

fn deserialize_bounded_persisted_completed<'de, D>(deserializer: D) -> Result<Vec<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec(deserializer, HARD_MAX_FILES)
}

fn hard_state_load_limits() -> StateLoadLimits {
    StateLoadLimits {
        max_checkpoints: HARD_MAX_HISTORY,
        max_plans: HARD_MAX_HISTORY,
        max_content_records: HARD_MAX_CONTENT_RECORDS,
        max_object_bytes: HARD_MAX_OBJECT_BYTES,
        max_content_bytes: HARD_MAX_CONTENT_BYTES,
        max_wire_bytes: HARD_MAX_STATE_WIRE_BYTES,
        max_nested_items: HARD_MAX_FILES,
    }
}

/// Upper-bounds the encoded metadata envelope for one caller's state limits.
/// Content bodies live in separate rows, so this is intentionally based on
/// record counts rather than body bytes.  The constants are conservative
/// envelopes for the fixed MsgPack maps and their bounded nested collections.
fn state_wire_limit(max_files: usize, max_history: usize, max_content_records: usize) -> usize {
    const BASE_BYTES: usize = 4096;
    const CHECKPOINT_BASE_BYTES: usize = 2048;
    const PLAN_BASE_BYTES: usize = 4096;
    const CHECKPOINT_OBJECT_BYTES: usize = 512;
    const PLAN_OPERATION_BYTES: usize = 512;
    const CONTENT_RECORD_BYTES: usize = 128;

    let per_checkpoint =
        CHECKPOINT_BASE_BYTES.saturating_add(max_files.saturating_mul(CHECKPOINT_OBJECT_BYTES));
    let per_plan = PLAN_BASE_BYTES.saturating_add(max_files.saturating_mul(PLAN_OPERATION_BYTES));
    BASE_BYTES
        .saturating_add(max_history.saturating_mul(per_checkpoint.saturating_add(per_plan)))
        .saturating_add(max_content_records.saturating_mul(CONTENT_RECORD_BYTES))
        .min(HARD_MAX_STATE_WIRE_BYTES)
}

fn persisted_scope(scope: CheckpointScope) -> PersistedScope {
    PersistedScope {
        task_id: *scope.task_id.as_bytes(),
        workspace: scope.workspace.0,
        generation: scope.generation,
        action_epoch: scope.action_epoch,
    }
}

fn scope_from_persisted(scope: PersistedScope) -> Result<CheckpointScope, StateStoreFailure> {
    let task_id = TaskId::from_bytes(scope.task_id).map_err(|_| StateStoreFailure::Corrupt)?;
    Ok(CheckpointScope {
        task_id,
        workspace: WorkspaceToken(scope.workspace),
        generation: scope.generation,
        action_epoch: scope.action_epoch,
    })
}

fn persisted_object_kind(kind: ObjectKind) -> PersistedObjectKind {
    match kind {
        ObjectKind::File => PersistedObjectKind::File,
        ObjectKind::Artifact => PersistedObjectKind::Artifact,
    }
}

fn object_kind_from_persisted(kind: PersistedObjectKind) -> ObjectKind {
    match kind {
        PersistedObjectKind::File => ObjectKind::File,
        PersistedObjectKind::Artifact => ObjectKind::Artifact,
    }
}

fn persisted_object(object: ObjectRef) -> PersistedObjectRef {
    PersistedObjectRef {
        id: object.id.0,
        kind: persisted_object_kind(object.kind),
    }
}

fn object_from_persisted(object: PersistedObjectRef) -> ObjectRef {
    ObjectRef {
        id: ObjectId(object.id),
        kind: object_kind_from_persisted(object.kind),
    }
}

fn persisted_reason(reason: CheckpointReason) -> PersistedCheckpointReason {
    match reason {
        CheckpointReason::BeforeTurn => PersistedCheckpointReason::BeforeTurn,
        CheckpointReason::AfterCompletion => PersistedCheckpointReason::AfterCompletion,
        CheckpointReason::Manual => PersistedCheckpointReason::Manual,
    }
}

fn reason_from_persisted(reason: PersistedCheckpointReason) -> CheckpointReason {
    match reason {
        PersistedCheckpointReason::BeforeTurn => CheckpointReason::BeforeTurn,
        PersistedCheckpointReason::AfterCompletion => CheckpointReason::AfterCompletion,
        PersistedCheckpointReason::Manual => CheckpointReason::Manual,
    }
}

fn persisted_context(context: CaptureContext) -> PersistedContext {
    PersistedContext {
        reason: persisted_reason(context.reason),
        agent: context.agent.0,
        turn: context.turn,
        captured_at_ms: context.captured_at_ms,
    }
}

fn context_from_persisted(context: PersistedContext) -> CaptureContext {
    CaptureContext {
        reason: reason_from_persisted(context.reason),
        agent: AgentToken(context.agent),
        turn: context.turn,
        captured_at_ms: context.captured_at_ms,
    }
}

fn persisted_checkpoint_id(id: CheckpointId) -> [u8; 16] {
    *id.0.as_bytes()
}

fn checkpoint_id_from_persisted(id: [u8; 16]) -> CheckpointId {
    CheckpointId(Uuid::from_bytes(id))
}

fn persisted_plan_id(id: PlanId) -> [u8; 16] {
    *id.0.as_bytes()
}

fn plan_id_from_persisted(id: [u8; 16]) -> PlanId {
    PlanId(Uuid::from_bytes(id))
}

fn persisted_fingerprint(fingerprint: ObjectFingerprint) -> PersistedFingerprint {
    PersistedFingerprint {
        digest: fingerprint.digest,
        bytes: fingerprint.bytes,
        present: fingerprint.present,
    }
}

fn object_fingerprint(persisted: PersistedFingerprint) -> ObjectFingerprint {
    ObjectFingerprint {
        digest: persisted.digest,
        bytes: persisted.bytes,
        present: persisted.present,
    }
}

fn persisted_address(address: ContentAddress) -> PersistedContentAddress {
    PersistedContentAddress {
        digest: address.digest,
        bytes: address.bytes,
    }
}

fn content_address_from_persisted(address: PersistedContentAddress) -> ContentAddress {
    ContentAddress {
        digest: address.digest,
        bytes: address.bytes,
    }
}

fn persisted_object_state(state: CheckpointObjectState) -> PersistedObjectState {
    match state {
        CheckpointObjectState::Absent => PersistedObjectState::Absent,
        CheckpointObjectState::Present(address) => {
            PersistedObjectState::Present(persisted_address(address))
        }
    }
}

fn object_state_from_persisted(state: PersistedObjectState) -> CheckpointObjectState {
    match state {
        PersistedObjectState::Absent => CheckpointObjectState::Absent,
        PersistedObjectState::Present(address) => {
            CheckpointObjectState::Present(content_address_from_persisted(address))
        }
    }
}

fn persisted_target(target: RecoveryTarget) -> PersistedTarget {
    match target {
        RecoveryTarget::Absent => PersistedTarget::Absent,
        RecoveryTarget::Present(address) => PersistedTarget::Present(persisted_address(address)),
    }
}

fn target_from_persisted(target: PersistedTarget) -> RecoveryTarget {
    match target {
        PersistedTarget::Absent => RecoveryTarget::Absent,
        PersistedTarget::Present(address) => {
            RecoveryTarget::Present(content_address_from_persisted(address))
        }
    }
}

fn persisted_status(status: RecoveryPlanStatus) -> PersistedPlanStatus {
    match status {
        RecoveryPlanStatus::Planned => PersistedPlanStatus::Planned,
        RecoveryPlanStatus::Applying => PersistedPlanStatus::Applying,
        RecoveryPlanStatus::Applied => PersistedPlanStatus::Applied,
        RecoveryPlanStatus::Conflicted => PersistedPlanStatus::Conflicted,
    }
}

fn status_from_persisted(status: PersistedPlanStatus) -> RecoveryPlanStatus {
    match status {
        PersistedPlanStatus::Planned => RecoveryPlanStatus::Planned,
        PersistedPlanStatus::Applying => RecoveryPlanStatus::Applying,
        PersistedPlanStatus::Applied => RecoveryPlanStatus::Applied,
        PersistedPlanStatus::Conflicted => RecoveryPlanStatus::Conflicted,
    }
}

fn persisted_active_attempt(attempt: Option<ActiveAttempt>) -> Option<PersistedActiveAttempt> {
    attempt.map(|attempt| PersistedActiveAttempt {
        approval_nonce: attempt.approval_nonce,
        attempt_generation: attempt.attempt_generation,
        operation_id: attempt.operation_id,
        state_version: attempt.state_version,
    })
}

fn active_attempt_from_persisted(attempt: Option<PersistedActiveAttempt>) -> Option<ActiveAttempt> {
    attempt.map(|attempt| ActiveAttempt {
        approval_nonce: attempt.approval_nonce,
        attempt_generation: attempt.attempt_generation,
        operation_id: attempt.operation_id,
        state_version: attempt.state_version,
    })
}

fn encode_persisted_state(
    state: &DurableCheckpointState,
    budget: &OperationBudget,
) -> Result<Vec<u8>, StateStoreFailure> {
    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    enforce_state_limits(state, hard_state_load_limits())
        .map_err(|_| StateStoreFailure::Oversize)?;
    for _ in &state.0.checkpoints {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    }
    for _ in &state.0.contents {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    }
    for _ in &state.0.plans {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    }
    for checkpoint in &state.0.checkpoints {
        for _ in &checkpoint.manifest.objects {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        }
    }
    for record in &state.0.plans {
        for _ in &record.plan.operations {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        }
        for _ in &record.completed {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        }
    }
    let wire = PersistedStateWire {
        codec_version: PERSISTED_CODEC_VERSION,
        state_version: state.version(),
        checkpoints: state
            .0
            .checkpoints
            .iter()
            .map(|checkpoint| PersistedCheckpoint {
                id: persisted_checkpoint_id(checkpoint.id),
                manifest: PersistedManifest {
                    scope: persisted_scope(checkpoint.manifest.scope),
                    revision: checkpoint.manifest.revision.0,
                    context: persisted_context(checkpoint.manifest.context),
                    objects: checkpoint
                        .manifest
                        .objects
                        .iter()
                        .map(|object| PersistedCheckpointObject {
                            object: persisted_object(object.object),
                            state: persisted_object_state(object.state.clone()),
                            fingerprint: persisted_fingerprint(object.fingerprint),
                        })
                        .collect(),
                    fingerprint: checkpoint.manifest.fingerprint,
                    total_bytes: checkpoint.manifest.total_bytes,
                },
            })
            .collect(),
        contents: state
            .0
            .contents
            .iter()
            .map(|record| PersistedContentRecord {
                address: persisted_address(record.address),
                bytes_len: u64::try_from(record.bytes.len()).unwrap_or(u64::MAX),
            })
            .collect(),
        plans: state
            .0
            .plans
            .iter()
            .map(|record| PersistedPlanRecord {
                plan: PersistedPlan {
                    id: persisted_plan_id(record.plan.id),
                    checkpoint_id: persisted_checkpoint_id(record.plan.checkpoint_id),
                    scope: persisted_scope(record.plan.scope),
                    planned_revision: record.plan.planned_revision.0,
                    operations: record
                        .plan
                        .operations
                        .iter()
                        .map(|operation| PersistedOperation {
                            object: persisted_object(operation.object),
                            expected: persisted_fingerprint(operation.expected),
                            target: persisted_target(operation.target),
                        })
                        .collect(),
                    fingerprint: record.plan.fingerprint,
                },
                status: persisted_status(record.status),
                completed: record.completed.iter().copied().collect(),
                active_attempt: persisted_active_attempt(record.active_attempt),
                issued_approval_nonce: record.issued_approval_nonce,
                tombstoned: record.tombstoned,
            })
            .collect(),
    };
    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    let encoded = rmp_serde::to_vec_named(&wire).map_err(|_| StateStoreFailure::Unavailable)?;
    if encoded.len() > HARD_MAX_STATE_WIRE_BYTES {
        return Err(StateStoreFailure::Oversize);
    }
    Ok(encoded)
}

#[derive(Clone, Copy)]
enum MsgpackScanFailure {
    Corrupt,
    Oversize,
    Budget,
}

fn scan_msgpack_value_with_array_limit(
    bytes: &[u8],
    offset: &mut usize,
    depth: usize,
    budget: &OperationBudget,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    scan_msgpack_value_with_limits(bytes, offset, depth, budget, None, dynamic_array_limit)
}

fn scan_msgpack_value_with_limits(
    bytes: &[u8],
    offset: &mut usize,
    depth: usize,
    budget: &OperationBudget,
    current_array_limit: Option<usize>,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    budget.check().map_err(|_| MsgpackScanFailure::Budget)?;
    if depth > HARD_MAX_PERSISTENCE_DEPTH {
        return Err(MsgpackScanFailure::Oversize);
    }
    let marker = take_msgpack_byte(bytes, offset)?;
    let array_limit = current_array_limit.unwrap_or(dynamic_array_limit);
    match marker {
        0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => Ok(()),
        0x80..=0x8f => scan_msgpack_map_with_array_limit(
            bytes,
            offset,
            usize::from(marker & 0x0f),
            depth,
            budget,
            dynamic_array_limit,
        ),
        0x90..=0x9f => scan_msgpack_children_with_limits(
            bytes,
            offset,
            usize::from(marker & 0x0f),
            depth,
            budget,
            array_limit,
            dynamic_array_limit,
        ),
        0xa0..=0xbf => scan_msgpack_bytes(bytes, offset, usize::from(marker & 0x1f)),
        0xc1 => Err(MsgpackScanFailure::Corrupt),
        0xc4 => scan_msgpack_blob_len(bytes, offset, 1),
        0xc5 => scan_msgpack_blob_len(bytes, offset, 2),
        0xc6 => scan_msgpack_blob_len(bytes, offset, 4),
        0xc7 => scan_msgpack_ext_len(bytes, offset, 1),
        0xc8 => scan_msgpack_ext_len(bytes, offset, 2),
        0xc9 => scan_msgpack_ext_len(bytes, offset, 4),
        0xca => skip_msgpack_bytes(bytes, offset, 4),
        0xcb => skip_msgpack_bytes(bytes, offset, 8),
        0xcc | 0xd0 => skip_msgpack_bytes(bytes, offset, 1),
        0xcd | 0xd1 => skip_msgpack_bytes(bytes, offset, 2),
        0xce | 0xd2 => skip_msgpack_bytes(bytes, offset, 4),
        0xcf | 0xd3 => skip_msgpack_bytes(bytes, offset, 8),
        0xd4 => skip_msgpack_bytes(bytes, offset, 2),
        0xd5 => skip_msgpack_bytes(bytes, offset, 3),
        0xd6 => skip_msgpack_bytes(bytes, offset, 5),
        0xd7 => skip_msgpack_bytes(bytes, offset, 9),
        0xd8 => skip_msgpack_bytes(bytes, offset, 17),
        0xd9 => scan_msgpack_blob_len(bytes, offset, 1),
        0xda => scan_msgpack_blob_len(bytes, offset, 2),
        0xdb => scan_msgpack_blob_len(bytes, offset, 4),
        0xdc => scan_msgpack_children_len_with_limits(
            bytes,
            offset,
            2,
            depth,
            budget,
            array_limit,
            dynamic_array_limit,
        ),
        0xdd => scan_msgpack_children_len_with_limits(
            bytes,
            offset,
            4,
            depth,
            budget,
            array_limit,
            dynamic_array_limit,
        ),
        0xde => {
            scan_msgpack_map_len_with_limits(bytes, offset, 2, depth, budget, dynamic_array_limit)
        }
        0xdf => {
            scan_msgpack_map_len_with_limits(bytes, offset, 4, depth, budget, dynamic_array_limit)
        }
    }
}

fn scan_msgpack_children_with_limits(
    bytes: &[u8],
    offset: &mut usize,
    count: usize,
    depth: usize,
    budget: &OperationBudget,
    current_array_limit: usize,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    if count > HARD_MAX_CONTENT_RECORDS || count > current_array_limit {
        return Err(MsgpackScanFailure::Oversize);
    }
    for _ in 0..count {
        scan_msgpack_value_with_limits(
            bytes,
            offset,
            depth.saturating_add(1),
            budget,
            None,
            dynamic_array_limit,
        )?;
    }
    Ok(())
}

fn scan_msgpack_map_with_array_limit(
    bytes: &[u8],
    offset: &mut usize,
    pairs: usize,
    depth: usize,
    budget: &OperationBudget,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    if pairs > HARD_MAX_CONTENT_RECORDS / 2 {
        return Err(MsgpackScanFailure::Oversize);
    }
    for _ in 0..pairs {
        let key_start = *offset;
        let field_array_limit = match take_msgpack_text(bytes, offset) {
            Ok(key) => persisted_field_array_limit(key, dynamic_array_limit),
            Err(MsgpackScanFailure::Corrupt) => {
                *offset = key_start;
                scan_msgpack_value_with_limits(
                    bytes,
                    offset,
                    depth.saturating_add(1),
                    budget,
                    None,
                    dynamic_array_limit,
                )?;
                None
            }
            Err(error) => return Err(error),
        };
        scan_msgpack_value_with_limits(
            bytes,
            offset,
            depth.saturating_add(1),
            budget,
            field_array_limit,
            dynamic_array_limit,
        )?;
    }
    Ok(())
}

fn persisted_field_array_limit(key: &[u8], dynamic_array_limit: usize) -> Option<usize> {
    match key {
        b"objects" | b"operations" | b"completed" => Some(dynamic_array_limit),
        // Serde's MsgPack representation for fixed byte arrays is an array of
        // integers rather than a binary blob. Keep those schema-owned fields
        // independently bounded without treating their 16/32 bytes as a
        // caller-sized collection.
        b"task_id" | b"checkpoint_id" => Some(16),
        b"workspace"
        | b"id"
        | b"agent"
        | b"digest"
        | b"revision"
        | b"fingerprint"
        | b"planned_revision"
        | b"approval_nonce"
        | b"attempt_generation"
        | b"issued_approval_nonce" => Some(32),
        _ => None,
    }
}

fn scan_msgpack_blob_len(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
) -> Result<(), MsgpackScanFailure> {
    let len = take_msgpack_len(bytes, offset, width)?;
    scan_msgpack_bytes(bytes, offset, len)
}

fn scan_msgpack_ext_len(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
) -> Result<(), MsgpackScanFailure> {
    let len = take_msgpack_len(bytes, offset, width)?;
    scan_msgpack_ext(bytes, offset, len)
}

fn scan_msgpack_children_len_with_limits(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
    depth: usize,
    budget: &OperationBudget,
    current_array_limit: usize,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    let count = take_msgpack_len(bytes, offset, width)?;
    scan_msgpack_children_with_limits(
        bytes,
        offset,
        count,
        depth,
        budget,
        current_array_limit,
        dynamic_array_limit,
    )
}

fn scan_msgpack_map_len_with_limits(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
    depth: usize,
    budget: &OperationBudget,
    dynamic_array_limit: usize,
) -> Result<(), MsgpackScanFailure> {
    let pairs = take_msgpack_len(bytes, offset, width)?;
    scan_msgpack_map_with_array_limit(bytes, offset, pairs, depth, budget, dynamic_array_limit)
}

fn scan_msgpack_ext(
    bytes: &[u8],
    offset: &mut usize,
    data_len: usize,
) -> Result<(), MsgpackScanFailure> {
    let total_len = data_len
        .checked_add(1)
        .ok_or(MsgpackScanFailure::Oversize)?;
    scan_msgpack_bytes(bytes, offset, total_len)
}

fn scan_msgpack_bytes(
    bytes: &[u8],
    offset: &mut usize,
    len: usize,
) -> Result<(), MsgpackScanFailure> {
    if len > HARD_MAX_PERSISTENCE_VARIABLE_BYTES {
        return Err(MsgpackScanFailure::Oversize);
    }
    skip_msgpack_bytes(bytes, offset, len)
}

fn skip_msgpack_bytes(
    bytes: &[u8],
    offset: &mut usize,
    len: usize,
) -> Result<(), MsgpackScanFailure> {
    let end = offset.checked_add(len).ok_or(MsgpackScanFailure::Corrupt)?;
    if end > bytes.len() {
        return Err(MsgpackScanFailure::Corrupt);
    }
    *offset = end;
    Ok(())
}

fn take_msgpack_byte(bytes: &[u8], offset: &mut usize) -> Result<u8, MsgpackScanFailure> {
    let byte = *bytes.get(*offset).ok_or(MsgpackScanFailure::Corrupt)?;
    *offset += 1;
    Ok(byte)
}

fn take_msgpack_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, MsgpackScanFailure> {
    let first = take_msgpack_byte(bytes, offset)?;
    let second = take_msgpack_byte(bytes, offset)?;
    Ok(u16::from_be_bytes([first, second]))
}

fn take_msgpack_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, MsgpackScanFailure> {
    let first = take_msgpack_byte(bytes, offset)?;
    let second = take_msgpack_byte(bytes, offset)?;
    let third = take_msgpack_byte(bytes, offset)?;
    let fourth = take_msgpack_byte(bytes, offset)?;
    Ok(u32::from_be_bytes([first, second, third, fourth]))
}

fn take_msgpack_len(
    bytes: &[u8],
    offset: &mut usize,
    width: usize,
) -> Result<usize, MsgpackScanFailure> {
    match width {
        1 => Ok(usize::from(take_msgpack_byte(bytes, offset)?)),
        2 => Ok(usize::from(take_msgpack_u16(bytes, offset)?)),
        4 => usize::try_from(take_msgpack_u32(bytes, offset)?)
            .map_err(|_| MsgpackScanFailure::Oversize),
        _ => Err(MsgpackScanFailure::Corrupt),
    }
}

fn take_msgpack_array_len(bytes: &[u8], offset: &mut usize) -> Result<usize, MsgpackScanFailure> {
    let marker = take_msgpack_byte(bytes, offset)?;
    match marker {
        0x90..=0x9f => Ok(usize::from(marker & 0x0f)),
        0xdc => take_msgpack_len(bytes, offset, 2),
        0xdd => take_msgpack_len(bytes, offset, 4),
        _ => Err(MsgpackScanFailure::Corrupt),
    }
}

fn take_msgpack_map_len(bytes: &[u8], offset: &mut usize) -> Result<usize, MsgpackScanFailure> {
    let marker = take_msgpack_byte(bytes, offset)?;
    match marker {
        0x80..=0x8f => Ok(usize::from(marker & 0x0f)),
        0xde => take_msgpack_len(bytes, offset, 2),
        0xdf => take_msgpack_len(bytes, offset, 4),
        _ => Err(MsgpackScanFailure::Corrupt),
    }
}

fn take_msgpack_text<'a>(
    bytes: &'a [u8],
    offset: &mut usize,
) -> Result<&'a [u8], MsgpackScanFailure> {
    let marker = take_msgpack_byte(bytes, offset)?;
    let len = match marker {
        0xa0..=0xbf => usize::from(marker & 0x1f),
        0xd9 => take_msgpack_len(bytes, offset, 1)?,
        0xda => take_msgpack_len(bytes, offset, 2)?,
        0xdb => take_msgpack_len(bytes, offset, 4)?,
        _ => return Err(MsgpackScanFailure::Corrupt),
    };
    if len > HARD_MAX_PERSISTENCE_VARIABLE_BYTES {
        return Err(MsgpackScanFailure::Oversize);
    }
    let start = *offset;
    skip_msgpack_bytes(bytes, offset, len)?;
    bytes.get(start..*offset).ok_or(MsgpackScanFailure::Corrupt)
}

fn take_msgpack_uint(bytes: &[u8], offset: &mut usize) -> Result<u64, MsgpackScanFailure> {
    let marker = take_msgpack_byte(bytes, offset)?;
    match marker {
        0x00..=0x7f => Ok(u64::from(marker)),
        0xcc => Ok(u64::from(take_msgpack_byte(bytes, offset)?)),
        0xcd => Ok(u64::from(take_msgpack_u16(bytes, offset)?)),
        0xce => Ok(u64::from(take_msgpack_u32(bytes, offset)?)),
        0xcf => {
            let end = offset.checked_add(8).ok_or(MsgpackScanFailure::Corrupt)?;
            if end > bytes.len() {
                return Err(MsgpackScanFailure::Corrupt);
            }
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[*offset..end]);
            *offset = end;
            Ok(u64::from_be_bytes(value))
        }
        _ => Err(MsgpackScanFailure::Corrupt),
    }
}

fn decode_persisted_state(
    bytes: &[u8],
    budget: &OperationBudget,
) -> Result<PersistedStateWire, StateStoreFailure> {
    decode_persisted_state_bounded(bytes, hard_state_load_limits(), budget)
}

fn decode_persisted_state_bounded(
    bytes: &[u8],
    limits: StateLoadLimits,
    budget: &OperationBudget,
) -> Result<PersistedStateWire, StateStoreFailure> {
    let codec_version = scan_persisted_state(bytes, limits, budget)?;
    if codec_version != PERSISTED_CODEC_VERSION {
        return Err(StateStoreFailure::InvalidVersion);
    }
    rmp_serde::from_slice(bytes).map_err(|_| StateStoreFailure::Corrupt)
}

fn scan_persisted_state(
    bytes: &[u8],
    limits: StateLoadLimits,
    budget: &OperationBudget,
) -> Result<u16, StateStoreFailure> {
    if bytes.len() > HARD_MAX_STATE_WIRE_BYTES || bytes.len() > limits.max_wire_bytes {
        return Err(StateStoreFailure::Oversize);
    }

    let mut offset = 0usize;
    if !bytes
        .first()
        .is_some_and(|marker| matches!(*marker, 0x80..=0x8f | 0xde | 0xdf))
    {
        // Preserve the hard depth/size classification for malformed values
        // that are not even a state map, without allocating or attempting
        // serde.  A deeply nested adversarial array is still Oversize rather
        // than being collapsed into a generic schema error.
        let mut probe = 0usize;
        scan_msgpack_value_with_array_limit(bytes, &mut probe, 0, budget, HARD_MAX_CONTENT_RECORDS)
            .map_err(map_msgpack_scan_failure)?;
        if probe != bytes.len() {
            return Err(StateStoreFailure::Corrupt);
        }
        return Err(StateStoreFailure::Corrupt);
    }
    let pairs = take_msgpack_map_len(bytes, &mut offset).map_err(map_msgpack_scan_failure)?;
    if pairs > HARD_MAX_CONTENT_RECORDS / 2 {
        return Err(StateStoreFailure::Oversize);
    }

    let mut codec_version = None;
    let mut state_version_seen = false;
    let mut checkpoints_seen = false;
    let mut contents_seen = false;
    let mut plans_seen = false;
    for _ in 0..pairs {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        let key = take_msgpack_text(bytes, &mut offset).map_err(map_msgpack_scan_failure)?;
        match key {
            b"codec_version" => {
                if codec_version.is_some() {
                    return Err(StateStoreFailure::Corrupt);
                }
                codec_version =
                    Some(take_msgpack_uint(bytes, &mut offset).map_err(map_msgpack_scan_failure)?);
            }
            b"state_version" => {
                if state_version_seen {
                    return Err(StateStoreFailure::Corrupt);
                }
                state_version_seen = true;
                scan_msgpack_value_with_array_limit(
                    bytes,
                    &mut offset,
                    1,
                    budget,
                    limits.max_nested_items,
                )
                .map_err(map_msgpack_scan_failure)?;
            }
            b"checkpoints" => {
                if checkpoints_seen {
                    return Err(StateStoreFailure::Corrupt);
                }
                checkpoints_seen = true;
                let count =
                    take_msgpack_array_len(bytes, &mut offset).map_err(map_msgpack_scan_failure)?;
                scan_msgpack_children_with_limits(
                    bytes,
                    &mut offset,
                    count,
                    1,
                    budget,
                    limits.max_checkpoints,
                    limits.max_nested_items,
                )
                .map_err(map_msgpack_scan_failure)?;
            }
            b"contents" => {
                if contents_seen {
                    return Err(StateStoreFailure::Corrupt);
                }
                contents_seen = true;
                let count =
                    take_msgpack_array_len(bytes, &mut offset).map_err(map_msgpack_scan_failure)?;
                scan_msgpack_children_with_limits(
                    bytes,
                    &mut offset,
                    count,
                    1,
                    budget,
                    limits.max_content_records,
                    limits.max_nested_items,
                )
                .map_err(map_msgpack_scan_failure)?;
            }
            b"plans" => {
                if plans_seen {
                    return Err(StateStoreFailure::Corrupt);
                }
                plans_seen = true;
                let count =
                    take_msgpack_array_len(bytes, &mut offset).map_err(map_msgpack_scan_failure)?;
                scan_msgpack_children_with_limits(
                    bytes,
                    &mut offset,
                    count,
                    1,
                    budget,
                    limits.max_plans,
                    limits.max_nested_items,
                )
                .map_err(map_msgpack_scan_failure)?;
            }
            _ => return Err(StateStoreFailure::Corrupt),
        }
    }
    if !state_version_seen || !checkpoints_seen || !contents_seen || !plans_seen {
        return Err(StateStoreFailure::Corrupt);
    }
    if offset != bytes.len() {
        return Err(StateStoreFailure::Corrupt);
    }
    let codec_version = codec_version.ok_or(StateStoreFailure::Corrupt)?;
    u16::try_from(codec_version).map_err(|_| StateStoreFailure::InvalidVersion)
}

fn scan_persisted_codec_version(
    bytes: &[u8],
    budget: &OperationBudget,
) -> Result<u16, StateStoreFailure> {
    scan_persisted_state(bytes, hard_state_load_limits(), budget)
}

fn map_msgpack_scan_failure(error: MsgpackScanFailure) -> StateStoreFailure {
    match error {
        MsgpackScanFailure::Corrupt => StateStoreFailure::Corrupt,
        MsgpackScanFailure::Oversize => StateStoreFailure::Oversize,
        MsgpackScanFailure::Budget => StateStoreFailure::Unavailable,
    }
}

/// Validates the bounded content index before any durable blob is copied.
/// Duplicate addresses would otherwise cause one SQL body to be retained and
/// cloned once per wire record before semantic validation notices the error.
fn validate_persisted_content_records(
    records: &[PersistedContentRecord],
    limits: StateLoadLimits,
    budget: &OperationBudget,
) -> Result<(), StateStoreFailure> {
    if records.len() > limits.max_content_records {
        return Err(StateStoreFailure::Oversize);
    }
    let mut addresses = BTreeSet::new();
    let mut content_bytes = 0u64;
    for record in records {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if record.bytes_len > limits.max_object_bytes {
            return Err(StateStoreFailure::Oversize);
        }
        if record.address.bytes != record.bytes_len
            || !addresses.insert(content_address_from_persisted(record.address))
        {
            return Err(StateStoreFailure::Corrupt);
        }
        content_bytes = content_bytes
            .checked_add(record.bytes_len)
            .ok_or(StateStoreFailure::Oversize)?;
        if content_bytes > limits.max_content_bytes {
            return Err(StateStoreFailure::Oversize);
        }
    }
    Ok(())
}

fn state_from_persisted(
    wire: PersistedStateWire,
    mut blobs: BTreeMap<ContentAddress, Vec<u8>>,
    budget: &OperationBudget,
) -> Result<DurableCheckpointState, StateStoreFailure> {
    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    let mut contents = Vec::with_capacity(wire.contents.len());
    for record in wire.contents {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        let address = content_address_from_persisted(record.address);
        let bytes = blobs.remove(&address).ok_or(StateStoreFailure::Corrupt)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != record.bytes_len {
            return Err(StateStoreFailure::Corrupt);
        }
        contents.push(ContentRecord { address, bytes });
    }
    let checkpoints = wire
        .checkpoints
        .into_iter()
        .map(|checkpoint| {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            let manifest = checkpoint.manifest;
            let objects = manifest
                .objects
                .into_iter()
                .map(|object| {
                    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
                    Ok(CheckpointObject {
                        object: object_from_persisted(object.object),
                        state: object_state_from_persisted(object.state),
                        fingerprint: object_fingerprint(object.fingerprint),
                    })
                })
                .collect::<Result<Vec<_>, StateStoreFailure>>()?;
            Ok(Checkpoint {
                id: checkpoint_id_from_persisted(checkpoint.id),
                manifest: CheckpointManifest {
                    scope: scope_from_persisted(manifest.scope)?,
                    revision: WorkspaceRevision(manifest.revision),
                    context: context_from_persisted(manifest.context),
                    objects,
                    fingerprint: manifest.fingerprint,
                    total_bytes: manifest.total_bytes,
                },
            })
        })
        .collect::<Result<Vec<_>, StateStoreFailure>>()?;
    let plans = wire
        .plans
        .into_iter()
        .map(|record| {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            let completed = record.completed.iter().copied().collect::<BTreeSet<_>>();
            if completed.len() != record.completed.len() {
                return Err(StateStoreFailure::Corrupt);
            }
            let plan = record.plan;
            let operations = plan
                .operations
                .into_iter()
                .map(|operation| {
                    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
                    Ok(RecoveryOperation {
                        object: object_from_persisted(operation.object),
                        expected: object_fingerprint(operation.expected),
                        target: target_from_persisted(operation.target),
                    })
                })
                .collect::<Result<Vec<_>, StateStoreFailure>>()?;
            Ok(PlanRecord {
                plan: RecoveryPlan {
                    id: plan_id_from_persisted(plan.id),
                    checkpoint_id: checkpoint_id_from_persisted(plan.checkpoint_id),
                    scope: scope_from_persisted(plan.scope)?,
                    planned_revision: WorkspaceRevision(plan.planned_revision),
                    operations,
                    fingerprint: plan.fingerprint,
                },
                status: status_from_persisted(record.status),
                completed,
                active_attempt: active_attempt_from_persisted(record.active_attempt),
                issued_approval_nonce: record.issued_approval_nonce,
                tombstoned: record.tombstoned,
            })
        })
        .collect::<Result<Vec<_>, StateStoreFailure>>()?;
    let state = DurableCheckpointState(DurableCheckpointStateWire {
        version: wire.state_version,
        checkpoints,
        contents,
        plans,
    });
    budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
    Ok(state)
}

/// The state image handed to an atomic store.  It is immutable from the
/// registry's point of view and contains only content-addressed blobs.
#[derive(Clone)]
pub struct DurableCheckpointState(DurableCheckpointStateWire);

impl Default for DurableCheckpointState {
    fn default() -> Self {
        Self(DurableCheckpointStateWire {
            version: 0,
            checkpoints: Vec::new(),
            contents: Vec::new(),
            plans: Vec::new(),
        })
    }
}

impl DurableCheckpointState {
    pub fn version(&self) -> u64 {
        self.0.version
    }

    pub fn checkpoint_count(&self) -> usize {
        self.0.checkpoints.len()
    }

    pub fn plan_count(&self) -> usize {
        self.0.plans.len()
    }

    pub fn content_bytes(&self) -> u64 {
        self.0.contents.iter().fold(0u64, |total, record| {
            total.saturating_add(u64::try_from(record.bytes.len()).unwrap_or(u64::MAX))
        })
    }

    pub fn checkpoint(&self, id: CheckpointId) -> Option<Checkpoint> {
        self.0
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id == id)
            .filter(|checkpoint| checkpoint.manifest.objects.len() <= HARD_MAX_FILES)
            .cloned()
    }

    pub fn plan(&self, id: PlanId) -> Option<RecoveryPlan> {
        self.0
            .plans
            .iter()
            .take(MAX_PROJECTION_PLANS)
            .find(|record| record.plan.id == id)
            .filter(|record| record.plan.operations.len() <= HARD_MAX_FILES)
            .map(|record| record.plan.clone())
    }

    /// Returns only bounded recovery status metadata.  Content and
    /// fingerprints never cross this projection boundary.
    pub fn plan_projections(&self) -> Vec<RecoveryPlanProjection> {
        self.0
            .plans
            .iter()
            .take(MAX_PROJECTION_PLANS)
            .map(|record| RecoveryPlanProjection {
                id: record.plan.id,
                checkpoint_id: record.plan.checkpoint_id,
                scope: record.plan.scope,
                status: match record.status {
                    RecoveryPlanStatus::Planned => RecoveryStatusProjection::Planned,
                    RecoveryPlanStatus::Applying => RecoveryStatusProjection::Applying,
                    RecoveryPlanStatus::Conflicted => RecoveryStatusProjection::Conflicted,
                    RecoveryPlanStatus::Applied => RecoveryStatusProjection::Applied,
                },
                completed_operations: record.completed.len(),
                operation_count: record.plan.operations.len(),
                tombstoned: record.tombstoned,
            })
            .collect()
    }

    pub fn metadata_projection(&self) -> DurableCheckpointMetadata {
        DurableCheckpointMetadata {
            version: self.version(),
            checkpoints: self
                .0
                .checkpoints
                .iter()
                .take(HARD_MAX_HISTORY)
                .map(|checkpoint| ExternalCheckpointMetadata {
                    id: checkpoint.id.0.to_string(),
                    scope: ExternalScopeMetadata {
                        generation: checkpoint.manifest.scope.generation,
                        action_epoch: checkpoint.manifest.scope.action_epoch,
                    },
                    object_count: checkpoint.manifest.objects.len(),
                    total_bytes: checkpoint.manifest.total_bytes,
                })
                .collect(),
            content_record_count: self.0.contents.len(),
            content_bytes: self.content_bytes(),
            plans: self
                .0
                .plans
                .iter()
                .take(HARD_MAX_HISTORY)
                .map(|record| ExternalPlanMetadata {
                    id: record.plan.id.0.to_string(),
                    checkpoint_id: record.plan.checkpoint_id.0.to_string(),
                    scope: ExternalScopeMetadata {
                        generation: record.plan.scope.generation,
                        action_epoch: record.plan.scope.action_epoch,
                    },
                    status: match record.status {
                        RecoveryPlanStatus::Planned => ExternalRecoveryStatus::Planned,
                        RecoveryPlanStatus::Applying => ExternalRecoveryStatus::Applying,
                        RecoveryPlanStatus::Conflicted => ExternalRecoveryStatus::Conflicted,
                        RecoveryPlanStatus::Applied => ExternalRecoveryStatus::Applied,
                    },
                    completed_operations: record.completed.len(),
                    operation_count: record.plan.operations.len(),
                    tombstoned: record.tombstoned,
                })
                .collect(),
        }
    }
}

impl fmt::Debug for DurableCheckpointState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableCheckpointState")
            .field("version", &self.0.version)
            .field("checkpoint_count", &self.0.checkpoints.len())
            .field("content_bytes", &self.content_bytes())
            .field("plan_count", &self.0.plans.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateStoreFailure {
    Unavailable,
    Conflict,
    Oversize,
    InvalidVersion,
    Corrupt,
}

/// An atomic state boundary.  A production adapter should serialize the
/// supplied image and replace its durable record atomically.  This core does
/// not select a path or persistence profile.  It is the second half of the
/// production-only adapter union; `SqliteCheckpointStore` is test-only and
/// must not be read as the shipped File/Git/store implementation.
#[allow(private_bounds)]
pub trait AtomicCheckpointStateStore: private::StoreSeal {
    /// Loads a state image under fixed bounds.  Implementations backed by a
    /// serialized store must enforce the bounds during streaming decode
    /// before deserializing or allocating a state image.
    fn load_bounded(
        &self,
        limits: StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure>;

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure>;
}

#[cfg(test)]
#[derive(Clone, Default)]
pub struct InMemoryCheckpointStore {
    state: Arc<Mutex<DurableCheckpointState>>,
}

#[cfg(test)]
impl InMemoryCheckpointStore {
    pub fn snapshot(&self) -> DurableCheckpointState {
        self.state.lock().expect("checkpoint store lock").clone()
    }
}

#[cfg(test)]
fn lock_store_with_budget<'a, T>(
    mutex: &'a Mutex<T>,
    budget: &OperationBudget,
) -> Result<std::sync::MutexGuard<'a, T>, StateStoreFailure> {
    loop {
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        match mutex.try_lock() {
            Ok(guard) => {
                budget
                    .check_control()
                    .map_err(|_| StateStoreFailure::Unavailable)?;
                return Ok(guard);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(StateStoreFailure::Unavailable)
            }
            Err(std::sync::TryLockError::WouldBlock) => std::thread::yield_now(),
        }
    }
}

#[cfg(test)]
impl AtomicCheckpointStateStore for InMemoryCheckpointStore {
    fn load_bounded(
        &self,
        limits: StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        let state = lock_store_with_budget(&self.state, budget)?;
        enforce_state_limits(&state, limits).map_err(|_| StateStoreFailure::Oversize)?;
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        Ok(state.clone())
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        let mut state = lock_store_with_budget(&self.state, budget)?;
        let expected_next = expected_version
            .checked_add(1)
            .ok_or(StateStoreFailure::InvalidVersion)?;
        if state.version() != expected_version {
            return Err(StateStoreFailure::Conflict);
        }
        if next.version() != expected_next {
            return Err(StateStoreFailure::InvalidVersion);
        }
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        *state = next;
        budget
            .check_control()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteCheckpointStoreError {
    Unavailable,
    Corrupt,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteFaultPoint {
    BeforeCommit,
    AfterCommit,
}

#[cfg(test)]
#[derive(Clone)]
struct SqliteBusyContext {
    cancelled: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
    work: Option<Arc<AtomicU64>>,
    started: Instant,
    max_wait: Duration,
}

#[cfg(test)]
thread_local! {
    static SQLITE_BUSY_CONTEXT: RefCell<Option<SqliteBusyContext>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn sqlite_busy_handler(_attempt: i32) -> bool {
    let should_continue = SQLITE_BUSY_CONTEXT.with(|slot| {
        slot.borrow().as_ref().is_some_and(|context| {
            !context
                .cancelled
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
                && !context
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                && context.work.as_ref().is_none_or(|work| {
                    let mut remaining = work.load(Ordering::Acquire);
                    loop {
                        if remaining == 0 {
                            break false;
                        }
                        match work.compare_exchange_weak(
                            remaining,
                            remaining - 1,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break true,
                            Err(current) => remaining = current,
                        }
                    }
                })
                && context.started.elapsed() < context.max_wait
        })
    });
    if should_continue {
        std::thread::sleep(Duration::from_millis(1));
    }
    should_continue
}

#[cfg(test)]
fn install_sqlite_busy_context(budget: &OperationBudget, max_wait: Duration) {
    SQLITE_BUSY_CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(SqliteBusyContext {
            cancelled: budget.cancelled.clone(),
            deadline: budget.deadline,
            work: budget.work.clone(),
            started: Instant::now(),
            max_wait,
        });
    });
}

#[cfg(test)]
fn clear_sqlite_busy_context() {
    SQLITE_BUSY_CONTEXT.with(|slot| *slot.borrow_mut() = None);
}

/// File-backed production-contract state store.  Each operation opens its
/// own SQLite connection, uses WAL/full-sync/BEGIN IMMEDIATE, and commits the
/// payload plus content blobs as one durable CAS transaction.
#[cfg(test)]
#[derive(Clone)]
pub struct SqliteCheckpointStore {
    path: PathBuf,
    busy_timeout: Duration,
    fault: Arc<Mutex<Option<(SqliteFaultPoint, usize)>>>,
    blob_reads: Arc<AtomicUsize>,
}

#[cfg(test)]
impl fmt::Debug for SqliteCheckpointStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SqliteCheckpointStore(REDACTED)")
    }
}

#[cfg(test)]
impl SqliteCheckpointStore {
    const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(250);
    const MAX_BUSY_TIMEOUT: Duration = Duration::from_secs(1);

    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteCheckpointStoreError> {
        Self::open_with_busy_timeout(path, Self::DEFAULT_BUSY_TIMEOUT)
    }

    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, SqliteCheckpointStoreError> {
        let store = Self {
            path: path.as_ref().to_path_buf(),
            busy_timeout: busy_timeout.min(Self::MAX_BUSY_TIMEOUT),
            fault: Arc::new(Mutex::new(None)),
            blob_reads: Arc::new(AtomicUsize::new(0)),
        };
        let bootstrap_budget = OperationBudget::unbounded();
        let mut connection = store.open_connection(&bootstrap_budget)?;
        store.initialize(&mut connection)?;
        Ok(store)
    }

    pub fn busy_timeout(&self) -> Duration {
        self.busy_timeout
    }

    #[cfg(test)]
    fn blob_read_count(&self) -> usize {
        self.blob_reads.load(Ordering::Acquire)
    }

    /// Returns the connection-local durability pragmas used by every store
    /// operation.  This is intentionally metadata-only and contains no path
    /// or persisted state.
    pub fn durability_settings(&self) -> Result<(String, i64), SqliteCheckpointStoreError> {
        let budget = OperationBudget::unbounded();
        let connection = self.open_connection(&budget)?;
        let journal_mode = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        let synchronous = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        Ok((journal_mode, synchronous))
    }

    #[cfg(test)]
    pub fn arm_fault(&self, point: SqliteFaultPoint) {
        self.arm_fault_after(point, 0);
    }

    /// Inject one crash-like failure at a specific atomic replacement
    /// boundary.  The counter is scoped to the requested before/after commit
    /// phase, so tests can cover transition, claim, receipt, and terminal
    /// CASes on real independent SQLite connections.
    #[cfg(test)]
    pub fn arm_fault_after(&self, point: SqliteFaultPoint, replacement_index: usize) {
        if let Ok(mut fault) = self.fault.lock() {
            *fault = Some((point, replacement_index));
        }
    }

    #[cfg(test)]
    fn take_fault(&self, point: SqliteFaultPoint) -> bool {
        self.fault
            .lock()
            .map(|mut fault| {
                let should_fail = match fault.as_mut() {
                    Some((armed_point, remaining)) if *armed_point == point => {
                        if *remaining == 0 {
                            true
                        } else {
                            *remaining -= 1;
                            false
                        }
                    }
                    _ => false,
                };
                if should_fail {
                    *fault = None;
                }
                should_fail
            })
            .unwrap_or(false)
    }

    fn open_connection(
        &self,
        budget: &OperationBudget,
    ) -> Result<Connection, SqliteCheckpointStoreError> {
        clear_sqlite_busy_context();
        budget
            .check()
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        let connection =
            Connection::open(&self.path).map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        budget
            .check()
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        connection
            .busy_timeout(
                budget
                    .remaining_duration()
                    .map_or(self.busy_timeout, |remaining| {
                        remaining.min(self.busy_timeout)
                    }),
            )
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        budget
            .check()
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        let max_wait = budget
            .remaining_duration()
            .map_or(self.busy_timeout, |remaining| {
                remaining.min(self.busy_timeout)
            });
        install_sqlite_busy_context(budget, max_wait);
        connection
            .busy_handler(Some(sqlite_busy_handler))
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        budget
            .check()
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        Ok(connection)
    }

    fn initialize(&self, connection: &mut Connection) -> Result<(), SqliteCheckpointStoreError> {
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS checkpoint_state (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    version INTEGER NOT NULL,
                    payload BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS checkpoint_content (
                    digest BLOB PRIMARY KEY NOT NULL,
                    bytes INTEGER NOT NULL,
                    body BLOB NOT NULL
                 );",
            )
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        let bootstrap_budget = OperationBudget::unbounded();
        let payload = encode_persisted_state(&DurableCheckpointState::default(), &bootstrap_budget)
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO checkpoint_state(singleton, version, payload)
                 VALUES (1, 0, ?1)",
                params![payload],
            )
            .map_err(|_| SqliteCheckpointStoreError::Unavailable)?;
        Ok(())
    }
}

#[cfg(test)]
impl AtomicCheckpointStateStore for SqliteCheckpointStore {
    fn load_bounded(
        &self,
        limits: StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        let connection = self
            .open_connection(budget)
            .map_err(map_sqlite_open_error)?;
        let (stored_version, payload_len): (i64, i64) = connection
            .query_row(
                "SELECT version, length(payload) FROM checkpoint_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if stored_version < 0
            || payload_len < 0
            || usize::try_from(payload_len).unwrap_or(usize::MAX) > HARD_MAX_STATE_WIRE_BYTES
            || usize::try_from(payload_len).unwrap_or(usize::MAX) > limits.max_wire_bytes
        {
            return Err(StateStoreFailure::Oversize);
        }
        let content_rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM checkpoint_content", [], |row| {
                row.get(0)
            })
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if content_rows < 0
            || usize::try_from(content_rows).unwrap_or(usize::MAX) > limits.max_content_records
        {
            return Err(StateStoreFailure::Oversize);
        }
        let payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM checkpoint_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if payload.len() != usize::try_from(payload_len).unwrap_or(usize::MAX) {
            return Err(StateStoreFailure::Corrupt);
        }
        let wire = decode_persisted_state_bounded(&payload, limits, budget)?;
        if u64::try_from(stored_version).unwrap_or(u64::MAX) != wire.state_version {
            return Err(StateStoreFailure::Corrupt);
        }
        if wire.checkpoints.len() > limits.max_checkpoints
            || wire.plans.len() > limits.max_plans
            || wire.contents.len() > limits.max_content_records
        {
            return Err(StateStoreFailure::Oversize);
        }
        if usize::try_from(content_rows).unwrap_or(usize::MAX) != wire.contents.len() {
            return Err(StateStoreFailure::Corrupt);
        }
        validate_persisted_content_records(&wire.contents, limits, budget)?;

        let mut blobs = BTreeMap::new();
        for record in &wire.contents {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            let address = content_address_from_persisted(record.address);
            let (declared_bytes, body_len): (i64, i64) = connection
                .query_row(
                    "SELECT bytes, length(body) FROM checkpoint_content WHERE digest = ?1",
                    params![address.digest.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| StateStoreFailure::Corrupt)?;
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            if declared_bytes < 0 || body_len < 0 {
                return Err(StateStoreFailure::Corrupt);
            }
            let declared_bytes = u64::try_from(declared_bytes).unwrap_or(u64::MAX);
            let body_len = u64::try_from(body_len).unwrap_or(u64::MAX);
            if declared_bytes > limits.max_object_bytes || body_len > limits.max_object_bytes {
                return Err(StateStoreFailure::Oversize);
            }
            if declared_bytes != record.bytes_len || body_len != record.bytes_len {
                return Err(StateStoreFailure::Corrupt);
            }
            self.blob_reads.fetch_add(1, Ordering::AcqRel);
            let bytes: Vec<u8> = connection
                .query_row(
                    "SELECT body FROM checkpoint_content WHERE digest = ?1",
                    params![address.digest.as_slice()],
                    |row| row.get(0),
                )
                .map_err(|_| StateStoreFailure::Corrupt)?;
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != record.bytes_len {
                return Err(StateStoreFailure::Corrupt);
            }
            blobs.insert(address, bytes);
        }

        let state = state_from_persisted(wire, blobs, budget)?;
        enforce_state_limits(&state, limits).map_err(|_| StateStoreFailure::Oversize)?;
        budget_check_state_records(&state, budget).map_err(|_| StateStoreFailure::Unavailable)?;
        validate_state(&state, hard_validation_limits()).map_err(|_| StateStoreFailure::Corrupt)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        Ok(state)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        let payload = encode_persisted_state(&next, budget)?;
        let mut connection = self
            .open_connection(budget)
            .map_err(map_sqlite_open_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        let current_version: i64 = transaction
            .query_row(
                "SELECT version FROM checkpoint_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if current_version < 0
            || u64::try_from(current_version).unwrap_or(u64::MAX) != expected_version
        {
            return Err(StateStoreFailure::Conflict);
        }
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        transaction
            .execute("DELETE FROM checkpoint_content", [])
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        for content in &next.0.contents {
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO checkpoint_content(digest, bytes, body) VALUES (?1, ?2, ?3)",
                    params![
                        content.address.digest.as_slice(),
                        i64::try_from(content.bytes.len())
                            .map_err(|_| StateStoreFailure::Oversize)?,
                        content.bytes.as_slice(),
                    ],
                )
                .map_err(|_| StateStoreFailure::Unavailable)?;
            budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        }
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        transaction
            .execute(
                "UPDATE checkpoint_state SET version = ?1, payload = ?2 WHERE singleton = 1 AND version = ?3",
                params![
                    i64::try_from(next.version()).map_err(|_| StateStoreFailure::InvalidVersion)?,
                    payload,
                    i64::try_from(expected_version)
                        .map_err(|_| StateStoreFailure::InvalidVersion)?,
                ],
            )
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if self.take_fault(SqliteFaultPoint::BeforeCommit) {
            return Err(StateStoreFailure::Unavailable);
        }
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| StateStoreFailure::Unavailable)?;
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        if self.take_fault(SqliteFaultPoint::AfterCommit) {
            return Err(StateStoreFailure::Unavailable);
        }
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        Ok(())
    }
}

#[cfg(test)]
fn map_sqlite_open_error(error: SqliteCheckpointStoreError) -> StateStoreFailure {
    match error {
        SqliteCheckpointStoreError::Unavailable => StateStoreFailure::Unavailable,
        SqliteCheckpointStoreError::Corrupt => StateStoreFailure::Corrupt,
    }
}

fn hard_validation_limits() -> CheckpointLimits {
    CheckpointLimits::new(
        HARD_MAX_FILES,
        HARD_MAX_TOTAL_BYTES,
        HARD_MAX_OBJECT_BYTES,
        HARD_MAX_HISTORY,
    )
}

/// Typed failures intentionally carry no path, command, or provider error
/// text.  Callers can expose these variants safely in user-facing events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointFailure {
    InvalidRequest,
    InvalidLimits,
    ScopeMismatch,
    GenerationMismatch,
    ActionEpochMismatch,
    RevisionMismatch,
    DuplicateObject,
    ObjectLimitExceeded,
    ObjectTooLarge,
    ByteLimitExceeded,
    HistoryLimitExceeded,
    DeadlineExceeded,
    Cancelled,
    WorkLimitExceeded,
    AuthorityUnavailable,
    AuthorityConflict,
    UnsupportedAuthority,
    InvalidAuthorityResponse,
    CheckpointNotFound,
    ObjectNotCheckpointed,
    PlanNotFound,
    Unauthorized,
    AttemptInProgress,
    Superseded,
    StalePlan,
    RecoveryConflict,
    StateUnavailable,
    StateConflict,
    StateTooLarge,
    InvalidStateVersion,
    CorruptState,
}

impl fmt::Display for CheckpointFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRequest => "checkpoint request is invalid",
            Self::InvalidLimits => "checkpoint limits are invalid",
            Self::ScopeMismatch => "checkpoint scope does not match authority",
            Self::GenerationMismatch => "checkpoint generation is stale",
            Self::ActionEpochMismatch => "checkpoint action epoch is stale",
            Self::RevisionMismatch => "workspace revision is stale",
            Self::DuplicateObject => "checkpoint object identity is duplicated",
            Self::ObjectLimitExceeded => "checkpoint object limit exceeded",
            Self::ObjectTooLarge => "checkpoint object byte limit exceeded",
            Self::ByteLimitExceeded => "checkpoint byte limit exceeded",
            Self::HistoryLimitExceeded => "checkpoint history limit exceeded",
            Self::DeadlineExceeded => "checkpoint deadline exceeded",
            Self::Cancelled => "checkpoint capture was cancelled",
            Self::WorkLimitExceeded => "checkpoint work budget exceeded",
            Self::AuthorityUnavailable => "workspace authority unavailable",
            Self::AuthorityConflict => "workspace authority reported a conflict",
            Self::UnsupportedAuthority => "workspace authority operation unsupported",
            Self::InvalidAuthorityResponse => "workspace authority response invalid",
            Self::CheckpointNotFound => "checkpoint not found",
            Self::ObjectNotCheckpointed => "object is not in the checkpoint",
            Self::PlanNotFound => "recovery plan not found",
            Self::Unauthorized => "recovery approval is invalid",
            Self::AttemptInProgress => "recovery attempt is already in progress",
            Self::Superseded => "recovery attempt was superseded",
            Self::StalePlan => "recovery plan precondition is stale",
            Self::RecoveryConflict => "recovery target changed externally",
            Self::StateUnavailable => "checkpoint state unavailable",
            Self::StateConflict => "checkpoint state revision conflict",
            Self::StateTooLarge => "checkpoint state exceeds its fixed bounds",
            Self::InvalidStateVersion => "checkpoint state version is invalid",
            Self::CorruptState => "checkpoint state is corrupt",
        })
    }
}

impl std::error::Error for CheckpointFailure {}

/// Registry facade for capture, preview, and separately approved recovery.
pub struct CheckpointRegistry<S> {
    store: S,
    limits: CheckpointLimits,
}

impl<S: AtomicCheckpointStateStore> CheckpointRegistry<S> {
    pub fn new(store: S, limits: CheckpointLimits) -> Self {
        Self { store, limits }
    }

    pub fn state_snapshot(&self) -> Result<DurableCheckpointState, CheckpointFailure> {
        self.state_snapshot_with_budget(CaptureBudget::unbounded())
    }

    pub fn state_snapshot_with_budget(
        &self,
        budget: CaptureBudget,
    ) -> Result<DurableCheckpointState, CheckpointFailure> {
        self.limits.validate()?;
        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        budget.check()?;
        Ok(state)
    }

    pub fn capture<A: SealedWorkspaceAuthority>(
        &mut self,
        authority: &mut A,
        request: CaptureRequest,
        budget: CaptureBudget,
    ) -> Result<Checkpoint, CheckpointFailure> {
        self.limits.validate()?;
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        let captured_revision = checked_revision(authority, &budget)?;
        if request.objects.is_empty() {
            return Err(CheckpointFailure::InvalidRequest);
        }
        if request.objects.len() > self.limits.max_files {
            return Err(CheckpointFailure::ObjectLimitExceeded);
        }
        ensure_unique(request.objects())?;

        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        if state.0.checkpoints.len() >= self.limits.max_history {
            return Err(CheckpointFailure::HistoryLimitExceeded);
        }

        let mut objects = Vec::with_capacity(request.objects.len());
        let mut contents = BTreeMap::<ContentAddress, Vec<u8>>::new();
        let mut total_bytes = 0u64;

        for object in request.objects() {
            budget.check()?;
            check_scope(request.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != captured_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            let sealed = checked_read(authority, object, self.limits.max_object_bytes, &budget)?;
            check_scope(request.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != captured_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            if sealed.object() != *object {
                return Err(CheckpointFailure::InvalidAuthorityResponse);
            }

            let state = match sealed.state() {
                ObjectState::Absent => CheckpointObjectState::Absent,
                ObjectState::Present(bytes) => {
                    let bytes_len = u64::try_from(bytes.len())
                        .map_err(|_| CheckpointFailure::ObjectTooLarge)?;
                    if bytes_len > self.limits.max_object_bytes {
                        return Err(CheckpointFailure::ObjectTooLarge);
                    }
                    total_bytes = total_bytes
                        .checked_add(bytes_len)
                        .ok_or(CheckpointFailure::ByteLimitExceeded)?;
                    if total_bytes > self.limits.max_total_bytes {
                        return Err(CheckpointFailure::ByteLimitExceeded);
                    }
                    let address = content_address(bytes);
                    contents.entry(address).or_insert_with(|| bytes.clone());
                    CheckpointObjectState::Present(address)
                }
            };
            objects.push(CheckpointObject {
                object: *object,
                state,
                fingerprint: sealed.fingerprint(),
            });
        }
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        if checked_revision(authority, &budget)? != captured_revision {
            return Err(CheckpointFailure::RevisionMismatch);
        }

        objects.sort_by_key(|object| object.object);
        let context = request.context;
        let revision = captured_revision;
        let manifest_fingerprint = manifest_fingerprint(request.scope, revision, context, &objects);
        let checkpoint = Checkpoint {
            id: CheckpointId::new(),
            manifest: CheckpointManifest {
                scope: request.scope,
                revision,
                context,
                objects,
                fingerprint: manifest_fingerprint,
                total_bytes,
            },
        };

        let state_limits = self.limits.state_limits();
        let additional_content_records = contents
            .keys()
            .filter(|address| {
                !state
                    .0
                    .contents
                    .iter()
                    .any(|record| record.address == **address)
            })
            .count();
        let mut additional_content_bytes = 0u64;
        for (address, bytes) in &contents {
            if state
                .0
                .contents
                .iter()
                .any(|record| record.address == *address)
            {
                continue;
            }
            additional_content_bytes = additional_content_bytes
                .checked_add(
                    u64::try_from(bytes.len()).map_err(|_| CheckpointFailure::StateTooLarge)?,
                )
                .ok_or(CheckpointFailure::StateTooLarge)?;
        }
        if state
            .0
            .contents
            .len()
            .checked_add(additional_content_records)
            .map_or(true, |count| count > state_limits.max_content_records)
            || state
                .content_bytes()
                .checked_add(additional_content_bytes)
                .map_or(true, |bytes| bytes > state_limits.max_content_bytes)
        {
            return Err(CheckpointFailure::StateTooLarge);
        }

        let mut next = state.clone();
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        next.0.checkpoints.push(checkpoint.clone());
        for (address, bytes) in contents {
            if !next
                .0
                .contents
                .iter()
                .any(|record| record.address == address)
            {
                next.0.contents.push(ContentRecord { address, bytes });
            }
        }
        enforce_state_limits(&next, self.limits.state_limits())?;
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        if checked_revision(authority, &budget)? != captured_revision {
            return Err(CheckpointFailure::RevisionMismatch);
        }
        checked_store_replace(&self.store, state.version(), next, &budget)?;
        budget.check()?;
        Ok(checkpoint)
    }

    pub fn preview_restore<A: SealedWorkspaceAuthority>(
        &mut self,
        authority: &mut A,
        request: RestoreRequest,
    ) -> Result<RecoveryPlan, CheckpointFailure> {
        self.preview_restore_with_budget(authority, request, CaptureBudget::unbounded())
    }

    pub fn preview_restore_with_budget<A: SealedWorkspaceAuthority>(
        &mut self,
        authority: &mut A,
        request: RestoreRequest,
        budget: CaptureBudget,
    ) -> Result<RecoveryPlan, CheckpointFailure> {
        self.limits.validate()?;
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        let planned_revision = checked_revision(authority, &budget)?;
        if request.objects.is_empty() {
            return Err(CheckpointFailure::InvalidRequest);
        }
        if request.objects.len() > self.limits.max_files {
            return Err(CheckpointFailure::ObjectLimitExceeded);
        }
        ensure_unique(request.objects())?;

        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        if state.0.plans.len() >= self.limits.max_history {
            return Err(CheckpointFailure::HistoryLimitExceeded);
        }
        let checkpoint = state
            .0
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id == request.checkpoint_id)
            .ok_or(CheckpointFailure::CheckpointNotFound)?;
        check_scope(request.scope, checkpoint.manifest.scope)?;

        let mut operations = Vec::with_capacity(request.objects.len());
        for object in request.objects() {
            budget.check()?;
            check_scope(request.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            let target = checkpoint
                .manifest
                .objects
                .iter()
                .find(|entry| entry.object == *object)
                .ok_or(CheckpointFailure::ObjectNotCheckpointed)?;
            let current = checked_read(authority, object, self.limits.max_object_bytes, &budget)?;
            check_scope(request.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            if current.object() != *object {
                return Err(CheckpointFailure::InvalidAuthorityResponse);
            }
            let target_state = match target.state {
                CheckpointObjectState::Absent => RecoveryTarget::Absent,
                CheckpointObjectState::Present(address) => RecoveryTarget::Present(address),
            };
            if current.fingerprint() != target.fingerprint {
                operations.push(RecoveryOperation {
                    object: *object,
                    expected: current.fingerprint(),
                    target: target_state,
                });
            }
        }
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        if checked_revision(authority, &budget)? != planned_revision {
            return Err(CheckpointFailure::RevisionMismatch);
        }
        operations.sort_by_key(|operation| operation.object);
        let plan_id = PlanId(Uuid::now_v7());
        let plan_fingerprint = plan_fingerprint(
            plan_id,
            request.checkpoint_id,
            request.scope,
            planned_revision,
            &operations,
        );
        let plan = RecoveryPlan {
            id: plan_id,
            checkpoint_id: request.checkpoint_id,
            scope: request.scope,
            planned_revision,
            operations,
            fingerprint: plan_fingerprint,
        };
        let mut next = state.clone();
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        next.0.plans.push(PlanRecord {
            plan: plan.clone(),
            status: RecoveryPlanStatus::Planned,
            completed: BTreeSet::new(),
            active_attempt: None,
            issued_approval_nonce: None,
            tombstoned: false,
        });
        enforce_state_limits(&next, self.limits.state_limits())?;
        budget.check()?;
        check_scope(request.scope, checked_scope(authority, &budget)?)?;
        if checked_revision(authority, &budget)? != planned_revision {
            return Err(CheckpointFailure::RevisionMismatch);
        }
        checked_store_replace(&self.store, state.version(), next, &budget)?;
        budget.check()?;
        Ok(plan)
    }

    /// Issues a fresh, opaque host approval for a previously previewed plan.
    /// The approval is not the durable crash receipt: the latter is created
    /// only when the Applying transition is atomically committed.
    pub fn issue_host_approval(
        &mut self,
        plan_id: PlanId,
    ) -> Result<RecoveryApproval, CheckpointFailure> {
        self.issue_host_approval_with_budget(plan_id, CaptureBudget::unbounded())
    }

    pub fn issue_host_approval_with_budget(
        &mut self,
        plan_id: PlanId,
        budget: CaptureBudget,
    ) -> Result<RecoveryApproval, CheckpointFailure> {
        self.limits.validate()?;
        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        let record = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        match record.status {
            // A new host request may observe an already-applied tombstone and
            // receive an idempotent receipt; reusing the exact nonce is
            // rejected by `apply_restore`.
            RecoveryPlanStatus::Applied => {}
            RecoveryPlanStatus::Conflicted => return Err(CheckpointFailure::RecoveryConflict),
            RecoveryPlanStatus::Applying => return Err(CheckpointFailure::AttemptInProgress),
            RecoveryPlanStatus::Planned => {}
        }
        let nonce = opaque_bytes(b"checkpoint-host-approval");
        let mut next = state.clone();
        let stored = next
            .0
            .plans
            .iter_mut()
            .find(|stored| stored.plan.id == plan_id)
            .ok_or(CheckpointFailure::CorruptState)?;
        stored.issued_approval_nonce = Some(nonce);
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        checked_store_replace(&self.store, state.version(), next, &budget)?;
        let reread = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&reread, self.limits)?;
        let issued = reread
            .0
            .plans
            .iter()
            .find(|stored| stored.plan.id == plan_id)
            .is_some_and(|stored| stored.issued_approval_nonce == Some(nonce));
        if !issued {
            return Err(CheckpointFailure::StateConflict);
        }
        let approval = RecoveryApproval {
            plan_id: record.plan.id,
            scope: record.plan.scope,
            fingerprint: record.plan.fingerprint,
            nonce,
            attempt_generation: None,
            expected_state_version: reread.version(),
        };
        budget.check()?;
        Ok(approval)
    }

    /// Returns the durable approval for an Applying attempt after a crash.
    /// Recovery reuses the exact attempt fence; it cannot mint a competing
    /// approval or overwrite an active operation.
    pub fn resume_applying(
        &mut self,
        plan_id: PlanId,
    ) -> Result<RecoveryApproval, CheckpointFailure> {
        self.resume_applying_with_budget(plan_id, CaptureBudget::unbounded())
    }

    pub fn resume_applying_with_budget(
        &mut self,
        plan_id: PlanId,
        budget: CaptureBudget,
    ) -> Result<RecoveryApproval, CheckpointFailure> {
        self.limits.validate()?;
        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        let record = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        let attempt = record.active_attempt.ok_or(CheckpointFailure::Superseded)?;
        if record.status != RecoveryPlanStatus::Applying {
            return match record.status {
                RecoveryPlanStatus::Applied => Err(CheckpointFailure::Superseded),
                RecoveryPlanStatus::Conflicted => Err(CheckpointFailure::RecoveryConflict),
                RecoveryPlanStatus::Planned => Err(CheckpointFailure::Superseded),
                RecoveryPlanStatus::Applying => unreachable!(),
            };
        }
        let approval = RecoveryApproval {
            plan_id: record.plan.id,
            scope: record.plan.scope,
            fingerprint: record.plan.fingerprint,
            nonce: attempt.approval_nonce,
            attempt_generation: Some(attempt.attempt_generation),
            expected_state_version: state.version(),
        };
        budget.check()?;
        Ok(approval)
    }

    /// Atomically retires an Applying attempt so a new host approval may be
    /// issued. This is the only transition that can deliberately supersede a
    /// crashed attempt; ordinary approval issuance never overwrites one.
    pub fn supersede_applying(
        &mut self,
        plan_id: PlanId,
        approval: RecoveryApproval,
    ) -> Result<(), CheckpointFailure> {
        self.supersede_applying_with_budget(plan_id, approval, CaptureBudget::unbounded())
    }

    pub fn supersede_applying_with_budget(
        &mut self,
        plan_id: PlanId,
        approval: RecoveryApproval,
        budget: CaptureBudget,
    ) -> Result<(), CheckpointFailure> {
        self.limits.validate()?;
        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        let current = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        if current.plan.id != approval.plan_id
            || current.plan.scope != approval.scope
            || current.plan.fingerprint != approval.fingerprint
        {
            return Err(CheckpointFailure::Unauthorized);
        }
        if current.status != RecoveryPlanStatus::Applying {
            return Err(match current.status {
                RecoveryPlanStatus::Applied => CheckpointFailure::Superseded,
                RecoveryPlanStatus::Conflicted => CheckpointFailure::RecoveryConflict,
                RecoveryPlanStatus::Planned => CheckpointFailure::Superseded,
                RecoveryPlanStatus::Applying => unreachable!(),
            });
        }
        let active = current
            .active_attempt
            .ok_or(CheckpointFailure::CorruptState)?;
        if approval.attempt_generation != Some(active.attempt_generation)
            || approval.nonce != active.approval_nonce
            || approval.expected_state_version != state.version()
            || active.state_version != state.version()
        {
            return Err(if approval.attempt_generation.is_none() {
                CheckpointFailure::Unauthorized
            } else {
                CheckpointFailure::Superseded
            });
        }
        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::CorruptState)?;
        record.status = RecoveryPlanStatus::Planned;
        record.completed.clear();
        record.active_attempt = None;
        record.issued_approval_nonce = None;
        record.tombstoned = false;
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        checked_store_replace(&self.store, state.version(), next, &budget)?;
        let reread = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&reread, self.limits)?;
        if !reread.0.plans.iter().any(|record| {
            record.plan.id == plan_id
                && record.status == RecoveryPlanStatus::Planned
                && record.active_attempt.is_none()
                && record.issued_approval_nonce.is_none()
                && record.completed.is_empty()
                && !record.tombstoned
        }) {
            return Err(CheckpointFailure::Superseded);
        }
        budget.check()?;
        Ok(())
    }

    pub fn apply_restore<A: SealedWorkspaceAuthority>(
        &mut self,
        authority: &mut A,
        plan_id: PlanId,
        approval: RecoveryApproval,
    ) -> Result<RecoveryApplyOutcome, CheckpointFailure> {
        self.apply_restore_with_budget(authority, plan_id, approval, CaptureBudget::unbounded())
    }

    pub fn apply_restore_with_budget<A: SealedWorkspaceAuthority>(
        &mut self,
        authority: &mut A,
        plan_id: PlanId,
        approval: RecoveryApproval,
        budget: CaptureBudget,
    ) -> Result<RecoveryApplyOutcome, CheckpointFailure> {
        self.limits.validate()?;
        let state = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
        validate_state(&state, self.limits)?;
        let record = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .cloned()
            .ok_or(CheckpointFailure::PlanNotFound)?;
        if record.plan.id != approval.plan_id
            || record.plan.scope != approval.scope
            || record.plan.fingerprint != approval.fingerprint
        {
            return Err(CheckpointFailure::Unauthorized);
        }
        let (attempt, replay) = match record.status {
            RecoveryPlanStatus::Conflicted => return Err(CheckpointFailure::RecoveryConflict),
            RecoveryPlanStatus::Applied => {
                if record.issued_approval_nonce != Some(approval.nonce) {
                    return Err(CheckpointFailure::Unauthorized);
                }
                check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
                if checked_revision(authority, &budget)? != record.plan.planned_revision {
                    return Err(CheckpointFailure::RevisionMismatch);
                }
                if approval.attempt_generation.is_some()
                    || approval.expected_state_version != state.version()
                {
                    return Err(CheckpointFailure::Superseded);
                }
                self.consume_applied_approval(plan_id, &approval, &budget)?;
                budget.check()?;
                return Ok(RecoveryApplyOutcome::AlreadyApplied);
            }
            RecoveryPlanStatus::Applying => {
                let active = record
                    .active_attempt
                    .ok_or(CheckpointFailure::CorruptState)?;
                if active.approval_nonce != approval.nonce
                    || approval.attempt_generation != Some(active.attempt_generation)
                    || approval.expected_state_version != state.version()
                    || active.state_version != state.version()
                {
                    return Err(CheckpointFailure::Superseded);
                }
                check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
                if checked_revision(authority, &budget)? != record.plan.planned_revision {
                    return Err(CheckpointFailure::RevisionMismatch);
                }
                (
                    AttemptOwnership {
                        approval_nonce: active.approval_nonce,
                        attempt_generation: active.attempt_generation,
                    },
                    true,
                )
            }
            RecoveryPlanStatus::Planned => {
                if record.issued_approval_nonce != Some(approval.nonce) {
                    return Err(CheckpointFailure::Unauthorized);
                }
                if approval.attempt_generation.is_some()
                    || approval.expected_state_version != state.version()
                {
                    return Err(CheckpointFailure::Superseded);
                }
                check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
                if checked_revision(authority, &budget)? != record.plan.planned_revision {
                    return Err(CheckpointFailure::RevisionMismatch);
                }
                let attempt_generation = opaque_bytes(b"checkpoint-attempt-generation");
                let mut applying = state.clone();
                let applying_record = applying
                    .0
                    .plans
                    .iter_mut()
                    .find(|stored| stored.plan.id == plan_id)
                    .ok_or(CheckpointFailure::CorruptState)?;
                applying_record.status = RecoveryPlanStatus::Applying;
                applying_record.active_attempt = Some(ActiveAttempt {
                    approval_nonce: approval.nonce,
                    attempt_generation,
                    operation_id: None,
                    state_version: state
                        .version()
                        .checked_add(1)
                        .ok_or(CheckpointFailure::InvalidStateVersion)?,
                });
                applying_record.issued_approval_nonce = None;
                applying_record.tombstoned = false;
                applying.0.version = state
                    .version()
                    .checked_add(1)
                    .ok_or(CheckpointFailure::InvalidStateVersion)?;
                enforce_state_limits(&applying, self.limits.state_limits())?;
                if let Err(error) =
                    checked_store_replace(&self.store, state.version(), applying, &budget)
                {
                    if error == CheckpointFailure::StateConflict {
                        let reread =
                            checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
                        validate_state(&reread, self.limits)?;
                        return match attempt_terminal_outcome(
                            &reread,
                            plan_id,
                            approval.nonce,
                            attempt_generation,
                            None,
                        )? {
                            ConflictTransitionOutcome::AlreadyApplied => {
                                Ok(RecoveryApplyOutcome::AlreadyApplied)
                            }
                            ConflictTransitionOutcome::Conflicted => {
                                Err(CheckpointFailure::RecoveryConflict)
                            }
                            ConflictTransitionOutcome::Superseded => {
                                Err(CheckpointFailure::Superseded)
                            }
                            ConflictTransitionOutcome::Completed => {
                                Err(CheckpointFailure::StateConflict)
                            }
                        };
                    }
                    return Err(error);
                }
                let reread = checked_store_load(&self.store, self.limits.state_limits(), &budget)?;
                validate_state(&reread, self.limits)?;
                let active = reread
                    .0
                    .plans
                    .iter()
                    .find(|stored| stored.plan.id == plan_id)
                    .and_then(|stored| stored.active_attempt)
                    .ok_or(CheckpointFailure::Superseded)?;
                if active.approval_nonce != approval.nonce
                    || active.attempt_generation != attempt_generation
                {
                    return Err(CheckpointFailure::Superseded);
                }
                (
                    AttemptOwnership {
                        approval_nonce: approval.nonce,
                        attempt_generation,
                    },
                    false,
                )
            }
        };

        let mut changed = 0usize;
        for (index, operation) in record.plan.operations.iter().enumerate() {
            // The operation claim is itself a durable CAS.  Revalidate the
            // authority fence immediately before it so a scope/revision drift
            // cannot be hidden behind an otherwise successful ownership
            // transition (the post-claim check below covers the other side
            // of that transition).
            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            let completed = match self.claim_operation(
                plan_id,
                attempt.approval_nonce,
                attempt.attempt_generation,
                index,
                &budget,
            )? {
                OperationClaimOutcome::Claimed { completed } => completed,
                OperationClaimOutcome::AlreadyApplied => {
                    return Ok(RecoveryApplyOutcome::AlreadyApplied)
                }
                OperationClaimOutcome::Conflicted => {
                    return Err(CheckpointFailure::RecoveryConflict)
                }
            };
            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            let target_fingerprint =
                target_fingerprint(operation.object, &operation.target, &state.0.contents)?;
            let current = checked_read(
                authority,
                &operation.object,
                self.limits.max_object_bytes,
                &budget,
            )?;
            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            if current.object() != operation.object {
                return Err(CheckpointFailure::InvalidAuthorityResponse);
            }
            if completed {
                if current.fingerprint() != target_fingerprint {
                    return match self.mark_plan_conflicted(
                        plan_id,
                        attempt.approval_nonce,
                        attempt.attempt_generation,
                        Some(index),
                        &budget,
                    )? {
                        ConflictTransitionOutcome::Completed => {
                            Err(CheckpointFailure::StateConflict)
                        }
                        ConflictTransitionOutcome::Conflicted => {
                            Err(CheckpointFailure::RecoveryConflict)
                        }
                        ConflictTransitionOutcome::AlreadyApplied => {
                            Ok(RecoveryApplyOutcome::AlreadyApplied)
                        }
                        ConflictTransitionOutcome::Superseded => Err(CheckpointFailure::Superseded),
                    };
                }
                match self.mark_operation_complete(
                    plan_id,
                    attempt.approval_nonce,
                    attempt.attempt_generation,
                    index,
                    &budget,
                )? {
                    ConflictTransitionOutcome::Completed => {}
                    ConflictTransitionOutcome::AlreadyApplied => {
                        return Ok(RecoveryApplyOutcome::AlreadyApplied)
                    }
                    ConflictTransitionOutcome::Conflicted => {
                        return Err(CheckpointFailure::RecoveryConflict)
                    }
                    ConflictTransitionOutcome::Superseded => {
                        return Err(CheckpointFailure::Superseded)
                    }
                }
                continue;
            }
            if current.fingerprint() == target_fingerprint {
                match self.mark_operation_complete(
                    plan_id,
                    attempt.approval_nonce,
                    attempt.attempt_generation,
                    index,
                    &budget,
                )? {
                    ConflictTransitionOutcome::Completed => {}
                    ConflictTransitionOutcome::AlreadyApplied => {
                        return Ok(RecoveryApplyOutcome::AlreadyApplied)
                    }
                    ConflictTransitionOutcome::Conflicted => {
                        return Err(CheckpointFailure::RecoveryConflict)
                    }
                    ConflictTransitionOutcome::Superseded => {
                        return Err(CheckpointFailure::Superseded)
                    }
                }
                continue;
            }
            if current.fingerprint() != operation.expected {
                return match self.mark_plan_conflicted(
                    plan_id,
                    attempt.approval_nonce,
                    attempt.attempt_generation,
                    Some(index),
                    &budget,
                )? {
                    ConflictTransitionOutcome::Completed => Err(CheckpointFailure::StateConflict),
                    ConflictTransitionOutcome::Conflicted => {
                        Err(CheckpointFailure::RecoveryConflict)
                    }
                    ConflictTransitionOutcome::AlreadyApplied => {
                        Ok(RecoveryApplyOutcome::AlreadyApplied)
                    }
                    ConflictTransitionOutcome::Superseded => Err(CheckpointFailure::Superseded),
                };
            }

            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            match operation.target {
                RecoveryTarget::Absent => {
                    if let Err(error) =
                        checked_remove(authority, &operation.object, &operation.expected, &budget)?
                    {
                        if error == AuthorityFailure::Conflict {
                            return match self.mark_plan_conflicted(
                                plan_id,
                                attempt.approval_nonce,
                                attempt.attempt_generation,
                                Some(index),
                                &budget,
                            )? {
                                ConflictTransitionOutcome::Completed => {
                                    Err(CheckpointFailure::StateConflict)
                                }
                                ConflictTransitionOutcome::Conflicted => {
                                    Err(CheckpointFailure::RecoveryConflict)
                                }
                                ConflictTransitionOutcome::AlreadyApplied => {
                                    Ok(RecoveryApplyOutcome::AlreadyApplied)
                                }
                                ConflictTransitionOutcome::Superseded => {
                                    Err(CheckpointFailure::Superseded)
                                }
                            };
                        }
                        return Err(map_authority_error(error));
                    }
                }
                RecoveryTarget::Present(address) => {
                    let bytes = state
                        .0
                        .contents
                        .iter()
                        .find(|record| record.address == address)
                        .map(|record| record.bytes.as_slice())
                        .ok_or(CheckpointFailure::CorruptState)?;
                    if bytes.len() as u64 > self.limits.max_object_bytes {
                        return Err(CheckpointFailure::CorruptState);
                    }
                    if let Err(error) = checked_write(
                        authority,
                        &operation.object,
                        bytes,
                        &operation.expected,
                        &budget,
                    )? {
                        if error == AuthorityFailure::Conflict {
                            return match self.mark_plan_conflicted(
                                plan_id,
                                attempt.approval_nonce,
                                attempt.attempt_generation,
                                Some(index),
                                &budget,
                            )? {
                                ConflictTransitionOutcome::Completed => {
                                    Err(CheckpointFailure::StateConflict)
                                }
                                ConflictTransitionOutcome::Conflicted => {
                                    Err(CheckpointFailure::RecoveryConflict)
                                }
                                ConflictTransitionOutcome::AlreadyApplied => {
                                    Ok(RecoveryApplyOutcome::AlreadyApplied)
                                }
                                ConflictTransitionOutcome::Superseded => {
                                    Err(CheckpointFailure::Superseded)
                                }
                            };
                        }
                        return Err(map_authority_error(error));
                    }
                }
            }

            // A successful write is not a receipt.  Re-read the sealed
            // object before recording completion so a hostile/partial edit
            // cannot be hidden behind the durable Applying marker.
            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision {
                return Err(CheckpointFailure::RevisionMismatch);
            }
            let completed_object = checked_read(
                authority,
                &operation.object,
                self.limits.max_object_bytes,
                &budget,
            )?;
            check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
            if checked_revision(authority, &budget)? != record.plan.planned_revision
                || completed_object.object() != operation.object
                || completed_object.fingerprint() != target_fingerprint
            {
                return match self.mark_plan_conflicted(
                    plan_id,
                    attempt.approval_nonce,
                    attempt.attempt_generation,
                    Some(index),
                    &budget,
                )? {
                    ConflictTransitionOutcome::Completed => Err(CheckpointFailure::StateConflict),
                    ConflictTransitionOutcome::Conflicted => {
                        Err(CheckpointFailure::RecoveryConflict)
                    }
                    ConflictTransitionOutcome::AlreadyApplied => {
                        Ok(RecoveryApplyOutcome::AlreadyApplied)
                    }
                    ConflictTransitionOutcome::Superseded => Err(CheckpointFailure::Superseded),
                };
            }
            changed += 1;
            match self.mark_operation_complete(
                plan_id,
                attempt.approval_nonce,
                attempt.attempt_generation,
                index,
                &budget,
            )? {
                ConflictTransitionOutcome::Completed => {}
                ConflictTransitionOutcome::AlreadyApplied => {
                    return Ok(RecoveryApplyOutcome::AlreadyApplied)
                }
                ConflictTransitionOutcome::Conflicted => {
                    return Err(CheckpointFailure::RecoveryConflict)
                }
                ConflictTransitionOutcome::Superseded => return Err(CheckpointFailure::Superseded),
            }
        }

        budget.check()?;
        check_scope(record.plan.scope, checked_scope(authority, &budget)?)?;
        if checked_revision(authority, &budget)? != record.plan.planned_revision {
            return Err(CheckpointFailure::RevisionMismatch);
        }
        let terminal = self.mark_plan_applied(
            plan_id,
            attempt.approval_nonce,
            attempt.attempt_generation,
            &budget,
        )?;
        match terminal {
            ConflictTransitionOutcome::Completed => {}
            ConflictTransitionOutcome::AlreadyApplied => {
                return Ok(RecoveryApplyOutcome::AlreadyApplied)
            }
            ConflictTransitionOutcome::Conflicted => {
                return Err(CheckpointFailure::RecoveryConflict)
            }
            ConflictTransitionOutcome::Superseded => return Err(CheckpointFailure::Superseded),
        }
        budget.check()?;
        if replay {
            Ok(RecoveryApplyOutcome::Replayed { changed })
        } else {
            Ok(RecoveryApplyOutcome::Applied { changed })
        }
    }

    fn consume_applied_approval(
        &mut self,
        plan_id: PlanId,
        approval: &RecoveryApproval,
        budget: &CaptureBudget,
    ) -> Result<(), CheckpointFailure> {
        let state = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&state, self.limits)?;
        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        if record.status != RecoveryPlanStatus::Applied
            || record.issued_approval_nonce != Some(approval.nonce)
            || approval.attempt_generation.is_some()
            || approval.expected_state_version != state.version()
        {
            return Err(CheckpointFailure::Unauthorized);
        }
        record.issued_approval_nonce = None;
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        checked_store_replace(&self.store, state.version(), next, budget)?;
        let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&reread, self.limits)?;
        if !reread
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .is_some_and(|record| {
                record.status == RecoveryPlanStatus::Applied
                    && record.issued_approval_nonce.is_none()
            })
        {
            return Err(CheckpointFailure::StateConflict);
        }
        budget.check()?;
        Ok(())
    }

    fn claim_operation(
        &mut self,
        plan_id: PlanId,
        approval_nonce: [u8; 32],
        attempt_generation: [u8; 32],
        index: usize,
        budget: &CaptureBudget,
    ) -> Result<OperationClaimOutcome, CheckpointFailure> {
        let state = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&state, self.limits)?;
        let current = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        match current.status {
            RecoveryPlanStatus::Applied => return Ok(OperationClaimOutcome::AlreadyApplied),
            RecoveryPlanStatus::Conflicted => return Ok(OperationClaimOutcome::Conflicted),
            RecoveryPlanStatus::Planned => return Err(CheckpointFailure::Superseded),
            RecoveryPlanStatus::Applying => {}
        }
        let active = current
            .active_attempt
            .ok_or(CheckpointFailure::CorruptState)?;
        if active.approval_nonce != approval_nonce
            || active.attempt_generation != attempt_generation
            || active.state_version != state.version()
            || active
                .operation_id
                .is_some_and(|operation_id| operation_id != index)
        {
            return Err(CheckpointFailure::Superseded);
        }
        let completed = current.completed.contains(&index);
        if active.operation_id == Some(index) {
            budget.check()?;
            return Ok(OperationClaimOutcome::Claimed { completed });
        }

        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::CorruptState)?;
        record.active_attempt = Some(ActiveAttempt {
            approval_nonce,
            attempt_generation,
            operation_id: Some(index),
            state_version: state
                .version()
                .checked_add(1)
                .ok_or(CheckpointFailure::InvalidStateVersion)?,
        });
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        if let Err(error) = checked_store_replace(&self.store, state.version(), next, budget) {
            if error != CheckpointFailure::StateConflict {
                return Err(error);
            }
            let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
            validate_state(&reread, self.limits)?;
            return match attempt_terminal_outcome(
                &reread,
                plan_id,
                approval_nonce,
                attempt_generation,
                Some(index),
            )? {
                ConflictTransitionOutcome::AlreadyApplied => {
                    Ok(OperationClaimOutcome::AlreadyApplied)
                }
                ConflictTransitionOutcome::Conflicted => Ok(OperationClaimOutcome::Conflicted),
                ConflictTransitionOutcome::Completed => {
                    Ok(OperationClaimOutcome::Claimed { completed: true })
                }
                ConflictTransitionOutcome::Superseded => Err(CheckpointFailure::Superseded),
            };
        }
        let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&reread, self.limits)?;
        let stored = reread
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        if stored.status != RecoveryPlanStatus::Applying
            || stored.active_attempt
                != Some(ActiveAttempt {
                    approval_nonce,
                    attempt_generation,
                    operation_id: Some(index),
                    state_version: reread.version(),
                })
        {
            return Err(CheckpointFailure::Superseded);
        }
        budget.check()?;
        Ok(OperationClaimOutcome::Claimed { completed })
    }

    fn mark_operation_complete(
        &mut self,
        plan_id: PlanId,
        attempt_nonce: [u8; 32],
        attempt_generation: [u8; 32],
        index: usize,
        budget: &CaptureBudget,
    ) -> Result<ConflictTransitionOutcome, CheckpointFailure> {
        let state = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&state, self.limits)?;
        let current = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        match current.status {
            RecoveryPlanStatus::Applied => return Ok(ConflictTransitionOutcome::AlreadyApplied),
            RecoveryPlanStatus::Conflicted => return Ok(ConflictTransitionOutcome::Conflicted),
            RecoveryPlanStatus::Planned => return Ok(ConflictTransitionOutcome::Superseded),
            RecoveryPlanStatus::Applying => {}
        }
        if current.active_attempt
            != Some(ActiveAttempt {
                approval_nonce: attempt_nonce,
                attempt_generation,
                operation_id: Some(index),
                state_version: state.version(),
            })
        {
            return Ok(ConflictTransitionOutcome::Superseded);
        }
        if index >= current.plan.operations.len() {
            return Err(CheckpointFailure::CorruptState);
        }
        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::CorruptState)?;
        record.completed.insert(index);
        // Completing an individual operation does not retire the attempt. The
        // plan remains Applying and the same attempt owns the next operation
        // (or the terminal transition). Keep the durable ownership tuple with
        // an empty operation slot until the whole plan is terminal.
        record.active_attempt = Some(ActiveAttempt {
            approval_nonce: attempt_nonce,
            attempt_generation,
            operation_id: None,
            state_version: state
                .version()
                .checked_add(1)
                .ok_or(CheckpointFailure::InvalidStateVersion)?,
        });
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        if let Err(error) = checked_store_replace(&self.store, state.version(), next, budget) {
            if error != CheckpointFailure::StateConflict {
                return Err(error);
            }
            let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
            validate_state(&reread, self.limits)?;
            return attempt_terminal_outcome(
                &reread,
                plan_id,
                attempt_nonce,
                attempt_generation,
                Some(index),
            );
        }
        let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&reread, self.limits)?;
        let stored = reread
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        if stored.status == RecoveryPlanStatus::Applying
            && stored.completed.contains(&index)
            && stored.active_attempt
                == Some(ActiveAttempt {
                    approval_nonce: attempt_nonce,
                    attempt_generation,
                    operation_id: None,
                    state_version: reread.version(),
                })
        {
            budget.check()?;
            return Ok(ConflictTransitionOutcome::Completed);
        }
        attempt_terminal_outcome(
            &reread,
            plan_id,
            attempt_nonce,
            attempt_generation,
            Some(index),
        )
    }

    fn mark_plan_conflicted(
        &mut self,
        plan_id: PlanId,
        attempt_nonce: [u8; 32],
        attempt_generation: [u8; 32],
        operation_id: Option<usize>,
        budget: &CaptureBudget,
    ) -> Result<ConflictTransitionOutcome, CheckpointFailure> {
        let state = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&state, self.limits)?;
        let current = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        match current.status {
            RecoveryPlanStatus::Applied => return Ok(ConflictTransitionOutcome::AlreadyApplied),
            RecoveryPlanStatus::Conflicted => return Ok(ConflictTransitionOutcome::Conflicted),
            RecoveryPlanStatus::Planned => return Ok(ConflictTransitionOutcome::Superseded),
            RecoveryPlanStatus::Applying => {}
        }
        if current.active_attempt
            != Some(ActiveAttempt {
                approval_nonce: attempt_nonce,
                attempt_generation,
                operation_id,
                state_version: state.version(),
            })
        {
            return Ok(ConflictTransitionOutcome::Superseded);
        }
        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        record.status = RecoveryPlanStatus::Conflicted;
        record.issued_approval_nonce = None;
        record.active_attempt = None;
        record.tombstoned = true;
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        if let Err(error) = checked_store_replace(&self.store, state.version(), next, budget) {
            if error != CheckpointFailure::StateConflict {
                return Err(error);
            }
            let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
            validate_state(&reread, self.limits)?;
            return attempt_terminal_outcome(
                &reread,
                plan_id,
                attempt_nonce,
                attempt_generation,
                operation_id,
            );
        }
        let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&reread, self.limits)?;
        if reread.0.plans.iter().any(|stored| {
            stored.plan.id == plan_id
                && stored.status == RecoveryPlanStatus::Conflicted
                && stored.active_attempt.is_none()
                && stored.issued_approval_nonce.is_none()
                && stored.tombstoned
        }) {
            budget.check()?;
            return Ok(ConflictTransitionOutcome::Conflicted);
        }
        attempt_terminal_outcome(
            &reread,
            plan_id,
            attempt_nonce,
            attempt_generation,
            operation_id,
        )
    }

    fn mark_plan_applied(
        &mut self,
        plan_id: PlanId,
        attempt_nonce: [u8; 32],
        attempt_generation: [u8; 32],
        budget: &CaptureBudget,
    ) -> Result<ConflictTransitionOutcome, CheckpointFailure> {
        let state = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&state, self.limits)?;
        let current = state
            .0
            .plans
            .iter()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        match current.status {
            RecoveryPlanStatus::Applied => return Ok(ConflictTransitionOutcome::AlreadyApplied),
            RecoveryPlanStatus::Conflicted => return Ok(ConflictTransitionOutcome::Conflicted),
            RecoveryPlanStatus::Planned => return Ok(ConflictTransitionOutcome::Superseded),
            RecoveryPlanStatus::Applying => {}
        }
        if current.active_attempt
            != Some(ActiveAttempt {
                approval_nonce: attempt_nonce,
                attempt_generation,
                operation_id: None,
                state_version: state.version(),
            })
            || current.completed.len() != current.plan.operations.len()
        {
            return Ok(ConflictTransitionOutcome::Superseded);
        }
        let mut next = state.clone();
        let record = next
            .0
            .plans
            .iter_mut()
            .find(|record| record.plan.id == plan_id)
            .ok_or(CheckpointFailure::PlanNotFound)?;
        record.status = RecoveryPlanStatus::Applied;
        record.issued_approval_nonce = None;
        record.active_attempt = None;
        record.tombstoned = true;
        next.0.version = state
            .version()
            .checked_add(1)
            .ok_or(CheckpointFailure::InvalidStateVersion)?;
        enforce_state_limits(&next, self.limits.state_limits())?;
        if let Err(error) = checked_store_replace(&self.store, state.version(), next, budget) {
            if error != CheckpointFailure::StateConflict {
                return Err(error);
            }
            let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
            validate_state(&reread, self.limits)?;
            return attempt_terminal_outcome(
                &reread,
                plan_id,
                attempt_nonce,
                attempt_generation,
                None,
            );
        }
        let reread = checked_store_load(&self.store, self.limits.state_limits(), budget)?;
        validate_state(&reread, self.limits)?;
        if reread.0.plans.iter().any(|stored| {
            stored.plan.id == plan_id
                && stored.status == RecoveryPlanStatus::Applied
                && stored.active_attempt.is_none()
                && stored.issued_approval_nonce.is_none()
                && stored.tombstoned
        }) {
            budget.check()?;
            return Ok(ConflictTransitionOutcome::Completed);
        }
        attempt_terminal_outcome(&reread, plan_id, attempt_nonce, attempt_generation, None)
    }
}

fn attempt_terminal_outcome(
    state: &DurableCheckpointState,
    plan_id: PlanId,
    attempt_nonce: [u8; 32],
    attempt_generation: [u8; 32],
    operation_id: Option<usize>,
) -> Result<ConflictTransitionOutcome, CheckpointFailure> {
    let record = state
        .0
        .plans
        .iter()
        .find(|record| record.plan.id == plan_id)
        .ok_or(CheckpointFailure::PlanNotFound)?;
    match record.status {
        RecoveryPlanStatus::Applied => Ok(ConflictTransitionOutcome::AlreadyApplied),
        RecoveryPlanStatus::Conflicted => Ok(ConflictTransitionOutcome::Conflicted),
        RecoveryPlanStatus::Planned => Ok(ConflictTransitionOutcome::Superseded),
        RecoveryPlanStatus::Applying => {
            let completed_by_attempt = operation_id.is_some_and(|index| {
                record.completed.contains(&index)
                    && (record.active_attempt
                        == Some(ActiveAttempt {
                            approval_nonce: attempt_nonce,
                            attempt_generation,
                            operation_id: None,
                            state_version: state.version(),
                        })
                        || record.active_attempt
                            == Some(ActiveAttempt {
                                approval_nonce: attempt_nonce,
                                attempt_generation,
                                operation_id: Some(index),
                                state_version: state.version(),
                            }))
            });
            if completed_by_attempt {
                return Ok(ConflictTransitionOutcome::Completed);
            }
            if record.active_attempt
                == Some(ActiveAttempt {
                    approval_nonce: attempt_nonce,
                    attempt_generation,
                    operation_id,
                    state_version: state.version(),
                })
            {
                Err(CheckpointFailure::StateConflict)
            } else {
                Ok(ConflictTransitionOutcome::Superseded)
            }
        }
    }
}

fn ensure_unique(objects: &[ObjectRef]) -> Result<(), CheckpointFailure> {
    let mut seen = BTreeSet::new();
    for object in objects {
        if !seen.insert(*object) {
            return Err(CheckpointFailure::DuplicateObject);
        }
    }
    Ok(())
}

fn check_scope(
    expected: CheckpointScope,
    actual: CheckpointScope,
) -> Result<(), CheckpointFailure> {
    if expected.task_id != actual.task_id || expected.workspace != actual.workspace {
        return Err(CheckpointFailure::ScopeMismatch);
    }
    if expected.generation != actual.generation {
        return Err(CheckpointFailure::GenerationMismatch);
    }
    if expected.action_epoch != actual.action_epoch {
        return Err(CheckpointFailure::ActionEpochMismatch);
    }
    Ok(())
}

fn checked_scope<A: SealedWorkspaceAuthority>(
    authority: &A,
    budget: &CaptureBudget,
) -> Result<CheckpointScope, CheckpointFailure> {
    budget.check()?;
    let scope = *authority.scope(budget);
    budget.check()?;
    Ok(scope)
}

fn checked_revision<A: SealedWorkspaceAuthority>(
    authority: &A,
    budget: &CaptureBudget,
) -> Result<WorkspaceRevision, CheckpointFailure> {
    budget.check()?;
    let revision = authority.revision(budget);
    budget.check()?;
    Ok(revision)
}

fn map_authority_error(error: AuthorityFailure) -> CheckpointFailure {
    match error {
        AuthorityFailure::Unavailable => CheckpointFailure::AuthorityUnavailable,
        AuthorityFailure::Conflict => CheckpointFailure::AuthorityConflict,
        AuthorityFailure::Oversize => CheckpointFailure::ObjectTooLarge,
        AuthorityFailure::Unsupported => CheckpointFailure::UnsupportedAuthority,
    }
}

fn map_state_error(error: StateStoreFailure) -> CheckpointFailure {
    match error {
        StateStoreFailure::Unavailable => CheckpointFailure::StateUnavailable,
        StateStoreFailure::Conflict => CheckpointFailure::StateConflict,
        StateStoreFailure::Oversize => CheckpointFailure::StateTooLarge,
        StateStoreFailure::InvalidVersion => CheckpointFailure::InvalidStateVersion,
        StateStoreFailure::Corrupt => CheckpointFailure::CorruptState,
    }
}

fn checked_store_load<S: AtomicCheckpointStateStore>(
    store: &S,
    limits: StateLoadLimits,
    budget: &CaptureBudget,
) -> Result<DurableCheckpointState, CheckpointFailure> {
    budget.check()?;
    let result = store.load_bounded(limits, budget);
    budget.check()?;
    result.map_err(map_state_error)
}

fn checked_store_replace<S: AtomicCheckpointStateStore>(
    store: &S,
    expected_version: u64,
    next: DurableCheckpointState,
    budget: &CaptureBudget,
) -> Result<(), CheckpointFailure> {
    budget.check()?;
    let result = store.replace_atomic(expected_version, next, budget);
    budget.check()?;
    result.map_err(map_state_error)
}

fn checked_read<A: SealedWorkspaceAuthority>(
    authority: &mut A,
    object: &ObjectRef,
    max_bytes: u64,
    budget: &CaptureBudget,
) -> Result<SealedObject, CheckpointFailure> {
    budget.check()?;
    let result = authority.read_bounded(object, max_bytes, budget);
    budget.check()?;
    result.map_err(map_authority_error)
}

fn checked_write<A: SealedWorkspaceAuthority>(
    authority: &mut A,
    object: &ObjectRef,
    bytes: &[u8],
    expected: &ObjectFingerprint,
    budget: &CaptureBudget,
) -> Result<Result<(), AuthorityFailure>, CheckpointFailure> {
    budget.check()?;
    let result = authority.write(object, bytes, expected, budget);
    budget.check()?;
    Ok(result)
}

fn checked_remove<A: SealedWorkspaceAuthority>(
    authority: &mut A,
    object: &ObjectRef,
    expected: &ObjectFingerprint,
    budget: &CaptureBudget,
) -> Result<Result<(), AuthorityFailure>, CheckpointFailure> {
    budget.check()?;
    let result = authority.remove(object, expected, budget);
    budget.check()?;
    Ok(result)
}

fn opaque_bytes(domain: &[u8]) -> [u8; 32] {
    let uuid = Uuid::now_v7();
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(uuid.as_bytes());
    hasher.finalize().into()
}

fn content_address(bytes: &[u8]) -> ContentAddress {
    ContentAddress {
        digest: Sha256::digest(bytes).into(),
        bytes: bytes.len() as u64,
    }
}

fn fingerprint(object: ObjectRef, state: &ObjectState) -> ObjectFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(object.id.0);
    hasher.update([match object.kind {
        ObjectKind::File => 1,
        ObjectKind::Artifact => 2,
    }]);
    match state {
        ObjectState::Absent => {
            hasher.update([0]);
            ObjectFingerprint {
                digest: hasher.finalize().into(),
                bytes: 0,
                present: false,
            }
        }
        ObjectState::Present(bytes) => fingerprint_present_with_hasher(hasher, bytes),
    }
}

fn fingerprint_present(object: ObjectRef, bytes: &[u8]) -> ObjectFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(OBJECT_DOMAIN);
    hasher.update(object.id.0);
    hasher.update([match object.kind {
        ObjectKind::File => 1,
        ObjectKind::Artifact => 2,
    }]);
    fingerprint_present_with_hasher(hasher, bytes)
}

fn fingerprint_present_with_hasher(mut hasher: Sha256, bytes: &[u8]) -> ObjectFingerprint {
    hasher.update([1]);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    ObjectFingerprint {
        digest: hasher.finalize().into(),
        bytes: bytes.len() as u64,
        present: true,
    }
}

fn manifest_fingerprint(
    scope: CheckpointScope,
    revision: WorkspaceRevision,
    context: CaptureContext,
    objects: &[CheckpointObject],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_DOMAIN);
    hasher.update(scope.task_id.to_string().as_bytes());
    hasher.update(scope.workspace.0);
    hasher.update(scope.generation.to_le_bytes());
    hasher.update(scope.action_epoch.to_le_bytes());
    hasher.update(revision.0);
    hasher.update([context.reason as u8]);
    hasher.update(context.agent.0);
    hasher.update(context.turn.to_le_bytes());
    hasher.update(context.captured_at_ms.to_le_bytes());
    for object in objects {
        hasher.update(object.object.id.0);
        hasher.update([match object.object.kind {
            ObjectKind::File => 1,
            ObjectKind::Artifact => 2,
        }]);
        hasher.update(object.fingerprint.digest);
        hasher.update(object.fingerprint.bytes.to_le_bytes());
        hasher.update([object.fingerprint.present as u8]);
    }
    hasher.finalize().into()
}

fn plan_fingerprint(
    plan_id: PlanId,
    checkpoint_id: CheckpointId,
    scope: CheckpointScope,
    revision: WorkspaceRevision,
    operations: &[RecoveryOperation],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PLAN_DOMAIN);
    hasher.update(plan_id.0.as_bytes());
    hasher.update(checkpoint_id.0.as_bytes());
    hasher.update(scope.task_id.to_string().as_bytes());
    hasher.update(scope.workspace.0);
    hasher.update(scope.generation.to_le_bytes());
    hasher.update(scope.action_epoch.to_le_bytes());
    hasher.update(revision.0);
    for operation in operations {
        hasher.update(operation.object.id.0);
        hasher.update([match operation.object.kind {
            ObjectKind::File => 1,
            ObjectKind::Artifact => 2,
        }]);
        hasher.update(operation.expected.digest);
        hasher.update(operation.expected.bytes.to_le_bytes());
        hasher.update([operation.expected.present as u8]);
        match operation.target {
            RecoveryTarget::Absent => hasher.update([0]),
            RecoveryTarget::Present(address) => {
                hasher.update([1]);
                hasher.update(address.digest);
                hasher.update(address.bytes.to_le_bytes());
            }
        }
    }
    hasher.finalize().into()
}

fn enforce_state_limits(
    state: &DurableCheckpointState,
    limits: StateLoadLimits,
) -> Result<(), CheckpointFailure> {
    if state.0.checkpoints.len() > limits.max_checkpoints
        || state.0.plans.len() > limits.max_plans
        || state.0.contents.len() > limits.max_content_records
    {
        return Err(CheckpointFailure::StateTooLarge);
    }

    for checkpoint in &state.0.checkpoints {
        if checkpoint.manifest.objects.len() > limits.max_nested_items {
            return Err(CheckpointFailure::StateTooLarge);
        }
    }
    for record in &state.0.plans {
        if record.plan.operations.len() > limits.max_nested_items
            || record.completed.len() > limits.max_nested_items
        {
            return Err(CheckpointFailure::StateTooLarge);
        }
    }

    let mut content_bytes = 0u64;
    for record in &state.0.contents {
        let bytes =
            u64::try_from(record.bytes.len()).map_err(|_| CheckpointFailure::StateTooLarge)?;
        if bytes > limits.max_object_bytes {
            return Err(CheckpointFailure::StateTooLarge);
        }
        content_bytes = content_bytes
            .checked_add(bytes)
            .ok_or(CheckpointFailure::StateTooLarge)?;
        if content_bytes > limits.max_content_bytes {
            return Err(CheckpointFailure::StateTooLarge);
        }
    }
    Ok(())
}

fn budget_check_state_records(
    state: &DurableCheckpointState,
    budget: &OperationBudget,
) -> Result<(), CheckpointFailure> {
    budget.check()?;
    for _ in &state.0.contents {
        budget.check()?;
    }
    for checkpoint in &state.0.checkpoints {
        budget.check()?;
        for _ in &checkpoint.manifest.objects {
            budget.check()?;
        }
    }
    for record in &state.0.plans {
        budget.check()?;
        for _ in &record.completed {
            budget.check()?;
        }
        for _ in &record.plan.operations {
            budget.check()?;
        }
    }
    budget.check()?;
    Ok(())
}

fn validate_state(
    state: &DurableCheckpointState,
    limits: CheckpointLimits,
) -> Result<(), CheckpointFailure> {
    enforce_state_limits(state, limits.state_limits())?;

    let mut addresses = BTreeSet::new();
    for record in &state.0.contents {
        if record.address != content_address(&record.bytes) || !addresses.insert(record.address) {
            return Err(CheckpointFailure::CorruptState);
        }
    }

    let mut checkpoint_ids = BTreeSet::new();
    let mut referenced_addresses = BTreeSet::new();
    for checkpoint in &state.0.checkpoints {
        if !checkpoint_ids.insert(checkpoint.id) {
            return Err(CheckpointFailure::CorruptState);
        }
        let manifest = &checkpoint.manifest;
        if manifest.objects.len() > limits.max_files {
            return Err(CheckpointFailure::StateTooLarge);
        }
        if manifest
            .objects
            .windows(2)
            .any(|pair| pair[0].object >= pair[1].object)
        {
            return Err(CheckpointFailure::CorruptState);
        }
        if manifest.fingerprint
            != manifest_fingerprint(
                manifest.scope,
                manifest.revision,
                manifest.context,
                &manifest.objects,
            )
        {
            return Err(CheckpointFailure::CorruptState);
        }
        let mut total_bytes = 0u64;
        for object in &manifest.objects {
            if object.fingerprint.bytes > limits.max_object_bytes {
                return Err(CheckpointFailure::StateTooLarge);
            }
            total_bytes = total_bytes
                .checked_add(object.fingerprint.bytes)
                .ok_or(CheckpointFailure::CorruptState)?;
            if total_bytes > limits.max_total_bytes {
                return Err(CheckpointFailure::StateTooLarge);
            }
            let expected = match object.state {
                CheckpointObjectState::Absent => fingerprint(object.object, &ObjectState::Absent),
                CheckpointObjectState::Present(address) => {
                    let record = state
                        .0
                        .contents
                        .iter()
                        .find(|record| record.address == address)
                        .ok_or(CheckpointFailure::CorruptState)?;
                    if address.bytes != u64::try_from(record.bytes.len()).unwrap_or(u64::MAX) {
                        return Err(CheckpointFailure::CorruptState);
                    }
                    referenced_addresses.insert(address);
                    fingerprint_present(object.object, &record.bytes)
                }
            };
            if object.fingerprint != expected {
                return Err(CheckpointFailure::CorruptState);
            }
        }
        if total_bytes != manifest.total_bytes {
            return Err(CheckpointFailure::CorruptState);
        }
    }

    // Orphaned blobs can otherwise grow without bound across bounded history
    // rotations, so every durable content record must remain reachable.
    if referenced_addresses.len() != addresses.len() {
        return Err(CheckpointFailure::CorruptState);
    }

    let mut plan_ids = BTreeSet::new();
    for record in &state.0.plans {
        let plan = &record.plan;
        if !plan_ids.insert(plan.id) || plan.operations.len() > limits.max_files {
            return Err(CheckpointFailure::CorruptState);
        }
        let checkpoint = state
            .0
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id == plan.checkpoint_id)
            .ok_or(CheckpointFailure::CorruptState)?;
        if checkpoint.manifest.scope != plan.scope
            || checkpoint.manifest.revision != plan.planned_revision
            || plan.fingerprint
                != plan_fingerprint(
                    plan.id,
                    plan.checkpoint_id,
                    plan.scope,
                    plan.planned_revision,
                    &plan.operations,
                )
            || record
                .completed
                .iter()
                .any(|index| *index >= plan.operations.len())
        {
            return Err(CheckpointFailure::CorruptState);
        }
        match record.status {
            RecoveryPlanStatus::Planned => {
                if !record.completed.is_empty()
                    || record.active_attempt.is_some()
                    || record.tombstoned
                {
                    return Err(CheckpointFailure::CorruptState);
                }
            }
            RecoveryPlanStatus::Applying => {
                let invalid_active = record.active_attempt.is_none()
                    || record.issued_approval_nonce.is_some()
                    || record.tombstoned
                    || record.active_attempt.is_some_and(|attempt| {
                        attempt.state_version != state.version()
                            || attempt
                                .operation_id
                                .is_some_and(|index| index >= plan.operations.len())
                    });
                if invalid_active {
                    return Err(CheckpointFailure::CorruptState);
                }
            }
            RecoveryPlanStatus::Conflicted => {
                if record.active_attempt.is_some()
                    || record.issued_approval_nonce.is_some()
                    || !record.tombstoned
                {
                    return Err(CheckpointFailure::CorruptState);
                }
            }
            RecoveryPlanStatus::Applied => {
                if record.active_attempt.is_some()
                    || record.completed.len() != plan.operations.len()
                    || !record.tombstoned
                {
                    return Err(CheckpointFailure::CorruptState);
                }
            }
        }
        let mut operation_ids = BTreeSet::new();
        for operation in &plan.operations {
            if !operation_ids.insert(operation.object) {
                return Err(CheckpointFailure::CorruptState);
            }
            let checkpoint_entry = checkpoint
                .manifest
                .objects
                .iter()
                .find(|entry| entry.object == operation.object)
                .ok_or(CheckpointFailure::CorruptState)?;
            match (&checkpoint_entry.state, operation.target) {
                (CheckpointObjectState::Absent, RecoveryTarget::Absent) => {}
                (CheckpointObjectState::Present(expected), RecoveryTarget::Present(actual))
                    if *expected == actual && addresses.contains(&actual) => {}
                _ => {
                    return Err(CheckpointFailure::CorruptState);
                }
            }
        }
        if plan
            .operations
            .windows(2)
            .any(|pair| pair[0].object >= pair[1].object)
        {
            return Err(CheckpointFailure::CorruptState);
        }
    }
    Ok(())
}

fn target_fingerprint(
    object: ObjectRef,
    target: &RecoveryTarget,
    contents: &[ContentRecord],
) -> Result<ObjectFingerprint, CheckpointFailure> {
    match target {
        RecoveryTarget::Absent => Ok(fingerprint(object, &ObjectState::Absent)),
        RecoveryTarget::Present(address) => {
            let bytes = contents
                .iter()
                .find(|record| record.address == *address)
                .map(|record| record.bytes.as_slice())
                .ok_or(CheckpointFailure::CorruptState)?;
            Ok(fingerprint_present(object, bytes))
        }
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use crate as devmanager;

    include!("checkpoint_test_impl.rs");
}
