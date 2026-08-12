use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::time::{Duration, Instant};

use devmanager::domain::TaskId;
use devmanager::workspace::checkpoint::{
    AgentToken, AtomicCheckpointStateStore, AuthorityFailure, CaptureBudget, CaptureContext,
    CaptureRequest, CheckpointFailure, CheckpointId, CheckpointLimits, CheckpointReason,
    CheckpointRegistry, CheckpointScope, DurableCheckpointMetadata, DurableCheckpointState,
    InMemoryCheckpointStore, ObjectFingerprint, ObjectId, ObjectRef, ObjectState, OperationBudget,
    RecoveryApplyOutcome, RecoveryStatusProjection, RestoreRequest, SealedObject,
    SealedWorkspaceAuthority, SqliteCheckpointStore, SqliteFaultPoint, StateStoreFailure,
    WorkspaceRevision, WorkspaceToken,
};
use rusqlite::Connection;

#[derive(Default)]
struct FakeSealedWorkspace {
    scope: Option<CheckpointScope>,
    revision: Option<WorkspaceRevision>,
    objects: BTreeMap<ObjectRef, ObjectState>,
    revision_on_next_read: Option<WorkspaceRevision>,
    mutate_after_write: Option<Vec<u8>>,
}

impl FakeSealedWorkspace {
    fn new(scope: CheckpointScope, revision: WorkspaceRevision) -> Self {
        Self {
            scope: Some(scope),
            revision: Some(revision),
            objects: BTreeMap::new(),
            revision_on_next_read: None,
            mutate_after_write: None,
        }
    }

    fn present(mut self, object: ObjectRef, bytes: &[u8]) -> Self {
        self.objects
            .insert(object, ObjectState::Present(bytes.to_vec()));
        self
    }

    fn absent(mut self, object: ObjectRef) -> Self {
        self.objects.insert(object, ObjectState::Absent);
        self
    }

    fn mutate(&mut self, object: ObjectRef, bytes: &[u8]) {
        self.objects
            .insert(object, ObjectState::Present(bytes.to_vec()));
    }

    fn object_state(&self, object: ObjectRef) -> ObjectState {
        self.objects
            .get(&object)
            .cloned()
            .unwrap_or(ObjectState::Absent)
    }

    fn set_scope(&mut self, scope: CheckpointScope) {
        self.scope = Some(scope);
    }

    fn drift_revision_on_next_read(&mut self, revision: WorkspaceRevision) {
        self.revision_on_next_read = Some(revision);
    }

    fn mutate_after_write(&mut self, bytes: &[u8]) {
        self.mutate_after_write = Some(bytes.to_vec());
    }
}

impl SealedWorkspaceAuthority for FakeSealedWorkspace {
    fn scope(&self, _budget: &OperationBudget) -> &CheckpointScope {
        self.scope.as_ref().expect("scope configured")
    }

    fn revision(&self, _budget: &OperationBudget) -> WorkspaceRevision {
        self.revision.expect("revision configured")
    }

    fn read_bounded(
        &mut self,
        object: &ObjectRef,
        max_bytes: u64,
        _budget: &OperationBudget,
    ) -> Result<SealedObject, AuthorityFailure> {
        let sealed = SealedObject::new(
            *object,
            self.objects
                .get(object)
                .cloned()
                .unwrap_or(ObjectState::Absent),
        );
        if matches!(sealed.state(), ObjectState::Present(bytes) if bytes.len() as u64 > max_bytes) {
            return Err(AuthorityFailure::Oversize);
        }
        if let Some(revision) = self.revision_on_next_read.take() {
            self.revision = Some(revision);
        }
        Ok(sealed)
    }

    fn write(
        &mut self,
        object: &ObjectRef,
        bytes: &[u8],
        expected: &ObjectFingerprint,
        _budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure> {
        let current = SealedObject::new(
            *object,
            self.objects
                .get(object)
                .cloned()
                .unwrap_or(ObjectState::Absent),
        );
        if &current.fingerprint() != expected {
            return Err(AuthorityFailure::Conflict);
        }
        self.objects
            .insert(*object, ObjectState::Present(bytes.to_vec()));
        if let Some(bytes) = self.mutate_after_write.take() {
            self.objects.insert(*object, ObjectState::Present(bytes));
        }
        Ok(())
    }

    fn remove(
        &mut self,
        object: &ObjectRef,
        expected: &ObjectFingerprint,
        _budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure> {
        let current = SealedObject::new(
            *object,
            self.objects
                .get(object)
                .cloned()
                .unwrap_or(ObjectState::Absent),
        );
        if &current.fingerprint() != expected {
            return Err(AuthorityFailure::Conflict);
        }
        self.objects.insert(*object, ObjectState::Absent);
        Ok(())
    }
}

#[derive(Default)]
struct CrashControl {
    successful_replacements_before_failure: AtomicUsize,
    fail_next: AtomicBool,
    conflict_next: AtomicBool,
}

struct CrashStore {
    inner: InMemoryCheckpointStore,
    control: Arc<CrashControl>,
}

impl CrashStore {
    fn new() -> (Self, Arc<CrashControl>) {
        let control = Arc::new(CrashControl::default());
        (
            Self {
                inner: InMemoryCheckpointStore::default(),
                control: control.clone(),
            },
            control,
        )
    }
}

impl AtomicCheckpointStateStore for CrashStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        self.inner.load_bounded(limits, budget)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        if self.control.conflict_next.swap(false, Ordering::AcqRel) {
            return Err(StateStoreFailure::Conflict);
        }
        let remaining = self
            .control
            .successful_replacements_before_failure
            .load(Ordering::Acquire);
        if remaining > 0 {
            self.control
                .successful_replacements_before_failure
                .fetch_sub(1, Ordering::AcqRel);
        } else if self.control.fail_next.swap(false, Ordering::AcqRel) {
            return Err(StateStoreFailure::Unavailable);
        }
        self.inner.replace_atomic(expected_version, next, budget)
    }
}

struct OversizeStore;

impl AtomicCheckpointStateStore for OversizeStore {
    fn load_bounded(
        &self,
        _limits: devmanager::workspace::checkpoint::StateLoadLimits,
        _budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        Err(StateStoreFailure::Oversize)
    }

    fn replace_atomic(
        &self,
        _expected_version: u64,
        _next: DurableCheckpointState,
        _budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        Err(StateStoreFailure::Unavailable)
    }
}

struct ConcurrentStore {
    inner: InMemoryCheckpointStore,
    gate: Arc<Barrier>,
}

impl AtomicCheckpointStateStore for ConcurrentStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        self.inner.load_bounded(limits, budget)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        self.gate.wait();
        budget.check().map_err(|_| StateStoreFailure::Unavailable)?;
        self.inner.replace_atomic(expected_version, next, budget)
    }
}

struct ConflictRaceControl {
    replace_calls: AtomicUsize,
    pause_on_call: usize,
    entered: Barrier,
    release: Barrier,
}

struct ConflictRaceStore {
    inner: SqliteCheckpointStore,
    control: Arc<ConflictRaceControl>,
}

impl AtomicCheckpointStateStore for ConflictRaceStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        self.inner.load_bounded(limits, budget)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        let call = self.control.replace_calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.control.pause_on_call {
            self.control.entered.wait();
            self.control.release.wait();
        }
        self.inner.replace_atomic(expected_version, next, budget)
    }
}

#[derive(Clone)]
struct ProbeStore {
    inner: InMemoryCheckpointStore,
    loads: Arc<AtomicUsize>,
    replacements: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct CancelAfterReplaceStore {
    inner: InMemoryCheckpointStore,
    cancelled: Arc<AtomicBool>,
}

impl AtomicCheckpointStateStore for CancelAfterReplaceStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        self.inner.load_bounded(limits, budget)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        let result = self.inner.replace_atomic(expected_version, next, budget);
        if result.is_ok() {
            self.cancelled.store(true, Ordering::Release);
        }
        result
    }
}

#[derive(Clone)]
struct CancelAfterLoadStore {
    inner: InMemoryCheckpointStore,
    cancelled: Arc<AtomicBool>,
}

impl AtomicCheckpointStateStore for CancelAfterLoadStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        let result = self.inner.load_bounded(limits, budget);
        if result.is_ok() {
            self.cancelled.store(true, Ordering::Release);
        }
        result
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        self.inner.replace_atomic(expected_version, next, budget)
    }
}

impl ProbeStore {
    fn new(inner: InMemoryCheckpointStore) -> Self {
        Self {
            inner,
            loads: Arc::new(AtomicUsize::new(0)),
            replacements: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl AtomicCheckpointStateStore for ProbeStore {
    fn load_bounded(
        &self,
        limits: devmanager::workspace::checkpoint::StateLoadLimits,
        budget: &OperationBudget,
    ) -> Result<DurableCheckpointState, StateStoreFailure> {
        self.loads.fetch_add(1, Ordering::AcqRel);
        self.inner.load_bounded(limits, budget)
    }

    fn replace_atomic(
        &self,
        expected_version: u64,
        next: DurableCheckpointState,
        budget: &OperationBudget,
    ) -> Result<(), StateStoreFailure> {
        self.replacements.fetch_add(1, Ordering::AcqRel);
        self.inner.replace_atomic(expected_version, next, budget)
    }
}

struct ProbeAuthority {
    inner: FakeSealedWorkspace,
    scope_reads: Arc<AtomicUsize>,
    revision_reads: Arc<AtomicUsize>,
}

impl ProbeAuthority {
    fn new(inner: FakeSealedWorkspace) -> Self {
        Self {
            inner,
            scope_reads: Arc::new(AtomicUsize::new(0)),
            revision_reads: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SealedWorkspaceAuthority for ProbeAuthority {
    fn scope(&self, budget: &OperationBudget) -> &CheckpointScope {
        self.scope_reads.fetch_add(1, Ordering::AcqRel);
        self.inner.scope(budget)
    }

    fn revision(&self, budget: &OperationBudget) -> WorkspaceRevision {
        self.revision_reads.fetch_add(1, Ordering::AcqRel);
        self.inner.revision(budget)
    }

    fn read_bounded(
        &mut self,
        object: &ObjectRef,
        max_bytes: u64,
        budget: &OperationBudget,
    ) -> Result<SealedObject, AuthorityFailure> {
        self.inner.read_bounded(object, max_bytes, budget)
    }

    fn write(
        &mut self,
        object: &ObjectRef,
        bytes: &[u8],
        expected: &ObjectFingerprint,
        budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure> {
        self.inner.write(object, bytes, expected, budget)
    }

    fn remove(
        &mut self,
        object: &ObjectRef,
        expected: &ObjectFingerprint,
        budget: &OperationBudget,
    ) -> Result<(), AuthorityFailure> {
        self.inner.remove(object, expected, budget)
    }
}

fn prepared_plan() -> (
    InMemoryCheckpointStore,
    CheckpointLimits,
    CheckpointScope,
    ObjectRef,
    devmanager::workspace::checkpoint::PlanId,
) {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let limits = CheckpointLimits::new(4, 128, 64, 8);
    let store = InMemoryCheckpointStore::default();
    let mut registry = CheckpointRegistry::new(store.clone(), limits);
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"before");
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 20, 20),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(object, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![object]),
        )
        .expect("preview");
    (store, limits, scope, object, plan.id())
}

fn prepared_sqlite_plan() -> (
    tempfile::TempDir,
    PathBuf,
    CheckpointLimits,
    CheckpointScope,
    ObjectRef,
    CheckpointId,
    devmanager::workspace::checkpoint::PlanId,
) {
    let (memory, limits, scope, object, plan_id) = prepared_plan();
    let state = CheckpointRegistry::new(memory, limits)
        .state_snapshot()
        .expect("prepared state");
    let checkpoint_id = state.plan(plan_id).expect("prepared plan").checkpoint_id();
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let sqlite = SqliteCheckpointStore::open(&path).expect("sqlite store");
    sqlite
        .replace_atomic(0, state, &OperationBudget::unbounded())
        .expect("seed sqlite state");
    (
        directory,
        path,
        limits,
        scope,
        object,
        checkpoint_id,
        plan_id,
    )
}

fn scope() -> CheckpointScope {
    CheckpointScope::new(TaskId::new(), WorkspaceToken::new(), 3, 9)
}

fn test_revision() -> WorkspaceRevision {
    WorkspaceRevision::from_host_digest([0x42; 32])
}

#[test]
fn concurrent_compare_and_swap_publishes_only_one_state_image() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let limits = CheckpointLimits::new(4, 128, 64, 8);
    let shared = InMemoryCheckpointStore::default();
    let gate = Arc::new(Barrier::new(2));
    let left_store = ConcurrentStore {
        inner: shared.clone(),
        gate: gate.clone(),
    };
    let right_store = ConcurrentStore {
        inner: shared.clone(),
        gate,
    };
    let left = std::thread::spawn(move || {
        let mut registry = CheckpointRegistry::new(left_store, limits);
        let mut authority =
            FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"left");
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 1, 1),
            ),
            CaptureBudget::unbounded(),
        )
    });
    let right = std::thread::spawn(move || {
        let mut registry = CheckpointRegistry::new(right_store, limits);
        let mut authority =
            FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"right");
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 2, 2),
            ),
            CaptureBudget::unbounded(),
        )
    });
    let outcomes = [
        left.join().expect("left thread"),
        right.join().expect("right thread"),
    ];
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == Err(CheckpointFailure::StateConflict))
            .count(),
        1
    );
    let registry = CheckpointRegistry::new(shared, limits);
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        1
    );
}

#[test]
fn captures_only_explicit_sealed_object_identities_into_a_bounded_manifest() {
    let scope = scope();
    let revision = WorkspaceRevision::new();
    let file = ObjectRef::file(ObjectId::new());
    let artifact = ObjectRef::artifact(ObjectId::new());
    let authority = FakeSealedWorkspace::new(scope, revision)
        .present(file, b"file body")
        .present(artifact, b"artifact body");
    let mut authority = authority;
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );

    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file, artifact],
                CaptureContext::new(
                    CheckpointReason::Manual,
                    AgentToken::new(),
                    7,
                    1_700_000_000_000,
                ),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture should succeed");

    assert_eq!(checkpoint.manifest().objects().len(), 2);
    let captured = checkpoint
        .manifest()
        .objects()
        .iter()
        .map(|entry| entry.object())
        .collect::<Vec<_>>();
    assert!(captured.contains(&file));
    assert!(captured.contains(&artifact));
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        1
    );
    assert_eq!(
        registry.state_snapshot().expect("state").content_bytes(),
        22
    );
    assert!(!format!("{checkpoint:?}").contains("file body"));
    assert!(!format!("{checkpoint:?}").contains("artifact body"));
    assert_eq!(
        format!("{:?}", checkpoint.manifest().objects()[0].fingerprint()),
        "ObjectFingerprint(REDACTED)"
    );
    let state_wire = serde_json::to_string(
        &registry
            .state_snapshot()
            .expect("state transport")
            .metadata_projection(),
    )
    .expect("metadata transport");
    assert!(!state_wire.contains("fingerprint"));
    assert!(!state_wire.contains("file body"));
}

#[test]
fn rejects_oversize_duplicate_and_cancelled_captures_without_partial_state() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 32, 4, 8),
    );
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"12345");
    let context = CaptureContext::new(
        CheckpointReason::Manual,
        AgentToken::new(),
        1,
        1_700_000_000_000,
    );

    let oversized = registry.capture(
        &mut authority,
        CaptureRequest::new(scope, vec![object], context),
        CaptureBudget::unbounded(),
    );
    assert_eq!(oversized, Err(CheckpointFailure::ObjectTooLarge));
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        0
    );

    let duplicate = registry.capture(
        &mut authority,
        CaptureRequest::new(scope, vec![object, object], context),
        CaptureBudget::unbounded(),
    );
    assert_eq!(duplicate, Err(CheckpointFailure::DuplicateObject));

    let cancelled = Arc::new(AtomicBool::new(true));
    let cancelled_result = registry.capture(
        &mut authority,
        CaptureRequest::new(scope, vec![object], context),
        CaptureBudget::unbounded().with_cancellation(cancelled),
    );
    assert_eq!(cancelled_result, Err(CheckpointFailure::Cancelled));

    let expired = registry.capture(
        &mut authority,
        CaptureRequest::new(scope, vec![object], context),
        CaptureBudget::unbounded().with_deadline(Instant::now() - Duration::from_millis(1)),
    );
    assert_eq!(expired, Err(CheckpointFailure::DeadlineExceeded));
}

#[test]
fn rejects_foreign_task_and_workspace_tokens_without_partial_state() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    let foreign_task = CheckpointScope::new(
        TaskId::new(),
        scope.workspace(),
        scope.generation(),
        scope.action_epoch(),
    );
    let mut authority =
        FakeSealedWorkspace::new(foreign_task, test_revision()).present(object, b"body");
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 30, 30),
            ),
            CaptureBudget::unbounded(),
        ),
        Err(CheckpointFailure::ScopeMismatch)
    );
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        0
    );

    let foreign_workspace = CheckpointScope::new(
        scope.task_id(),
        WorkspaceToken::new(),
        scope.generation(),
        scope.action_epoch(),
    );
    authority.set_scope(foreign_workspace);
    assert_eq!(
        registry.preview_restore(
            &mut authority,
            RestoreRequest::new(scope, CheckpointId::new(), vec![object]),
        ),
        Err(CheckpointFailure::ScopeMismatch)
    );
    assert_eq!(registry.state_snapshot().expect("state").plan_count(), 0);
}

#[test]
fn typed_failures_and_core_identities_never_carry_paths_or_git_reset() {
    let failures = [
        CheckpointFailure::InvalidRequest,
        CheckpointFailure::InvalidLimits,
        CheckpointFailure::ScopeMismatch,
        CheckpointFailure::GenerationMismatch,
        CheckpointFailure::ActionEpochMismatch,
        CheckpointFailure::RevisionMismatch,
        CheckpointFailure::DuplicateObject,
        CheckpointFailure::ObjectLimitExceeded,
        CheckpointFailure::ObjectTooLarge,
        CheckpointFailure::ByteLimitExceeded,
        CheckpointFailure::HistoryLimitExceeded,
        CheckpointFailure::DeadlineExceeded,
        CheckpointFailure::Cancelled,
        CheckpointFailure::WorkLimitExceeded,
        CheckpointFailure::AuthorityUnavailable,
        CheckpointFailure::AuthorityConflict,
        CheckpointFailure::UnsupportedAuthority,
        CheckpointFailure::InvalidAuthorityResponse,
        CheckpointFailure::CheckpointNotFound,
        CheckpointFailure::ObjectNotCheckpointed,
        CheckpointFailure::PlanNotFound,
        CheckpointFailure::Unauthorized,
        CheckpointFailure::AttemptInProgress,
        CheckpointFailure::Superseded,
        CheckpointFailure::StalePlan,
        CheckpointFailure::RecoveryConflict,
        CheckpointFailure::StateUnavailable,
        CheckpointFailure::StateConflict,
        CheckpointFailure::StateTooLarge,
        CheckpointFailure::InvalidStateVersion,
        CheckpointFailure::CorruptState,
    ];
    for failure in failures {
        let text = failure.to_string();
        assert!(!text.contains('\\'), "{text}");
        assert!(!text.contains('/'), "{text}");
        assert!(!text.to_ascii_lowercase().contains("reset"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("clean"), "{text}");
        assert!(!text.contains(".."), "{text}");
    }

    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let contained = directory.path().join("escaped-checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&contained).expect("test store open is caller-owned");
    let leaked = format!(
        "{:?} {:?} {:?} {:?} {:?}",
        scope(),
        ObjectRef::file(ObjectId::new()),
        WorkspaceToken::new(),
        store,
        CheckpointFailure::UnsupportedAuthority
    );
    assert!(!leaked.contains("escaped-checkpoint.sqlite3"));
    assert!(!leaked.contains(":\\"));
    assert!(!format!("{store:?}").contains("sqlite3"));
}

#[test]
fn checkpoint_and_plan_history_stay_bounded() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"one");
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 1),
    );
    let context = CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 10, 10);
    registry
        .capture(
            &mut authority,
            CaptureRequest::new(scope, vec![object], context),
            CaptureBudget::unbounded(),
        )
        .expect("first checkpoint");
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(scope, vec![object], context),
            CaptureBudget::unbounded(),
        ),
        Err(CheckpointFailure::HistoryLimitExceeded)
    );
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        1
    );
}

#[test]
fn preview_then_apply_restores_only_the_explicit_target_and_is_idempotent() {
    let scope = scope();
    let file = ObjectRef::file(ObjectId::new());
    let artifact = ObjectRef::artifact(ObjectId::new());
    let mut authority = FakeSealedWorkspace::new(scope, WorkspaceRevision::new())
        .present(file, b"before")
        .present(artifact, b"artifact-before");
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file, artifact],
                CaptureContext::new(CheckpointReason::BeforeTurn, AgentToken::new(), 2, 2),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(file, b"file-after");
    authority.mutate(artifact, b"artifact-after");

    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("preview");
    assert_eq!(plan.operations().len(), 1);
    assert_eq!(plan.operations()[0].object(), file);

    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    let applied = registry
        .apply_restore(&mut authority, plan.id(), approval)
        .expect("apply");
    assert_eq!(applied, RecoveryApplyOutcome::Applied { changed: 1 });
    assert_eq!(
        authority.object_state(file),
        ObjectState::Present(b"before".to_vec())
    );
    assert_eq!(
        authority.object_state(artifact),
        ObjectState::Present(b"artifact-after".to_vec())
    );
    let replay_approval = registry
        .issue_host_approval(plan.id())
        .expect("replay approval");
    assert_eq!(
        registry
            .apply_restore(&mut authority, plan.id(), replay_approval)
            .expect("replay"),
        RecoveryApplyOutcome::AlreadyApplied
    );
}

#[test]
fn refuses_external_mutation_and_generation_reuse_with_redacted_typed_failures() {
    let scope = scope();
    let file = ObjectRef::file(ObjectId::new());
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(file, b"before");
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 3, 3),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(file, b"changed");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("preview");
    let second_plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("second preview");
    let second_approval = registry
        .issue_host_approval(second_plan.id())
        .expect("second host approval");
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), second_approval),
        Err(CheckpointFailure::Unauthorized)
    );
    authority.mutate(file, b"external-edit");
    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::RecoveryConflict)
    );
    assert_eq!(
        registry.issue_host_approval(plan.id()),
        Err(CheckpointFailure::RecoveryConflict)
    );

    let fresh_scope = CheckpointScope::new(
        scope.task_id(),
        scope.workspace(),
        scope.generation() + 1,
        scope.action_epoch(),
    );
    authority.set_scope(fresh_scope);
    assert_eq!(
        registry.preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        ),
        Err(CheckpointFailure::GenerationMismatch)
    );
    authority.set_scope(CheckpointScope::new(
        scope.task_id(),
        scope.workspace(),
        scope.generation(),
        scope.action_epoch() + 1,
    ));
    assert_eq!(
        registry.preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        ),
        Err(CheckpointFailure::ActionEpochMismatch)
    );
    assert!(!CheckpointFailure::RecoveryConflict
        .to_string()
        .contains("external-edit"));
}

#[test]
fn crash_after_target_write_replays_from_atomic_state_without_rewriting() {
    let scope = scope();
    let file = ObjectRef::file(ObjectId::new());
    let (store, control) = CrashStore::new();
    let mut registry = CheckpointRegistry::new(store, CheckpointLimits::new(4, 128, 64, 8));
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(file, b"before");
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file],
                CaptureContext::new(CheckpointReason::BeforeTurn, AgentToken::new(), 4, 4),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(file, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("preview");

    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    control
        .successful_replacements_before_failure
        .store(2, Ordering::Release);
    control.fail_next.store(true, Ordering::Release);
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::StateUnavailable)
    );
    assert_eq!(
        authority.object_state(file),
        ObjectState::Present(b"before".to_vec())
    );

    let replay_approval = registry
        .resume_applying(plan.id())
        .expect("replay host approval");
    let replayed = registry
        .apply_restore(&mut authority, plan.id(), replay_approval)
        .expect("replay");
    assert_eq!(replayed, RecoveryApplyOutcome::Replayed { changed: 0 });
    assert_eq!(
        authority.object_state(file),
        ObjectState::Present(b"before".to_vec())
    );
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::Unauthorized)
    );
    let idempotent_approval = registry
        .issue_host_approval(plan.id())
        .expect("idempotent host approval");
    assert_eq!(
        registry
            .apply_restore(&mut authority, plan.id(), idempotent_approval)
            .expect("idempotent replay"),
        RecoveryApplyOutcome::AlreadyApplied
    );
}

#[test]
fn replay_rereads_completed_operations_before_accepting_the_receipt() {
    let scope = scope();
    let file = ObjectRef::file(ObjectId::new());
    let (store, control) = CrashStore::new();
    let mut registry = CheckpointRegistry::new(store, CheckpointLimits::new(4, 128, 64, 8));
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(file, b"before");
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file],
                CaptureContext::new(CheckpointReason::BeforeTurn, AgentToken::new(), 4, 4),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(file, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("preview");

    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    // Applying and completion are durable; the final Applied receipt fails.
    control
        .successful_replacements_before_failure
        .store(3, Ordering::Release);
    control.fail_next.store(true, Ordering::Release);
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::StateUnavailable)
    );
    authority.mutate(file, b"hostile-after-crash");

    let replay_approval = registry
        .resume_applying(plan.id())
        .expect("replay host approval");
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), replay_approval),
        Err(CheckpointFailure::RecoveryConflict)
    );
    let projection = registry
        .state_snapshot()
        .expect("state")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan.id())
        .expect("projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Conflicted);
    assert!(projection.tombstoned());
}

#[test]
fn targeted_recovery_can_remove_a_selected_binary_object_without_touching_others() {
    let scope = scope();
    let deleted = ObjectRef::file(ObjectId::new());
    let untouched = ObjectRef::artifact(ObjectId::new());
    let mut authority = FakeSealedWorkspace::new(scope, WorkspaceRevision::new())
        .absent(deleted)
        .present(untouched, &[0, 1, 2, 255]);
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![deleted, untouched],
                CaptureContext::new(CheckpointReason::AfterCompletion, AgentToken::new(), 5, 5),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(deleted, b"new-file");
    authority.mutate(untouched, &[9, 8, 7]);

    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![deleted]),
        )
        .expect("preview");
    assert_eq!(plan.operations().len(), 1);
    assert!(matches!(
        plan.operations()[0].target(),
        devmanager::workspace::checkpoint::RecoveryTarget::Absent
    ));
    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    assert_eq!(
        registry
            .apply_restore(&mut authority, plan.id(), approval)
            .expect("apply"),
        RecoveryApplyOutcome::Applied { changed: 1 }
    );
    assert_eq!(authority.object_state(deleted), ObjectState::Absent);
    assert_eq!(
        authority.object_state(untouched),
        ObjectState::Present(vec![9, 8, 7])
    );
}

#[test]
fn approval_is_host_issued_single_use_and_not_derived_from_the_plan() {
    let scope = scope();
    let file = ObjectRef::file(ObjectId::new());
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(file, b"before");
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![file],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 9, 9),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(file, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![file]),
        )
        .expect("preview");
    let plan_wire = serde_json::to_string(
        &registry
            .state_snapshot()
            .expect("state")
            .metadata_projection(),
    )
    .expect("metadata transport");
    assert!(!plan_wire.contains("fingerprint"));
    assert!(!plan_wire.contains("expected"));
    assert!(!format!("{plan:?}").contains("fingerprint"));
    let superseded_approval = registry
        .issue_host_approval(plan.id())
        .expect("first host approval");
    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), superseded_approval),
        Err(CheckpointFailure::Unauthorized)
    );
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Ok(RecoveryApplyOutcome::Applied { changed: 1 })
    );
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::Unauthorized)
    );
}

#[test]
fn bounded_store_load_and_shared_work_budget_fail_closed_before_capture() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"bounded");
    let mut oversized =
        CheckpointRegistry::new(OversizeStore, CheckpointLimits::new(4, 128, 64, 8));
    assert_eq!(
        oversized.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 10, 10),
            ),
            CaptureBudget::unbounded(),
        ),
        Err(CheckpointFailure::StateTooLarge)
    );

    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 11, 11),
            ),
            CaptureBudget::unbounded().with_work_limit(0),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        0
    );
}

#[test]
fn revision_drift_and_partial_edit_are_rejected_and_projected_as_tombstones() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let revision = WorkspaceRevision::new();
    let drifted_revision = WorkspaceRevision::new();
    let mut authority = FakeSealedWorkspace::new(scope, revision).present(object, b"before");
    let mut registry = CheckpointRegistry::new(
        InMemoryCheckpointStore::default(),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    authority.drift_revision_on_next_read(drifted_revision);
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 12, 12),
            ),
            CaptureBudget::unbounded(),
        ),
        Err(CheckpointFailure::RevisionMismatch)
    );

    authority.revision = Some(revision);
    let checkpoint = registry
        .capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 13, 13),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("capture");
    authority.mutate(object, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![object]),
        )
        .expect("preview");
    authority.mutate_after_write(b"hostile-after-write");
    let approval = registry
        .issue_host_approval(plan.id())
        .expect("host approval");
    assert_eq!(
        registry.apply_restore(&mut authority, plan.id(), approval),
        Err(CheckpointFailure::RecoveryConflict)
    );
    let projection = registry
        .state_snapshot()
        .expect("state")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan.id())
        .expect("projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Conflicted);
    assert!(projection.tombstoned());
}

#[test]
fn compare_and_swap_conflict_does_not_publish_partial_checkpoint() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let (store, control) = CrashStore::new();
    let mut registry = CheckpointRegistry::new(store, CheckpointLimits::new(4, 128, 64, 8));
    let mut authority =
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"before");
    control.conflict_next.store(true, Ordering::Release);
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 14, 14),
            ),
            CaptureBudget::unbounded(),
        ),
        Err(CheckpointFailure::StateConflict)
    );
    assert_eq!(
        registry.state_snapshot().expect("state").checkpoint_count(),
        0
    );
}

#[test]
fn supersede_retires_crashed_attempt_before_conflict_and_reopen() {
    let (shared, limits, scope, object, plan_id) = prepared_plan();
    let mut setup = CheckpointRegistry::new(shared.clone(), limits);
    let mut setup_authority =
        FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");

    // A crashed Applying attempt owns the durable approval.  Issuance is
    // rejected until the host explicitly retires that attempt atomically.
    let first_approval = setup
        .issue_host_approval(plan_id)
        .expect("initial host approval");
    assert_eq!(
        setup.apply_restore_with_budget(
            &mut setup_authority,
            plan_id,
            first_approval,
            CaptureBudget::unbounded().with_work_limit(12),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );
    assert_eq!(
        setup.issue_host_approval(plan_id),
        Err(CheckpointFailure::AttemptInProgress)
    );
    let active_attempt = setup
        .resume_applying(plan_id)
        .expect("active attempt receipt");
    setup
        .supersede_applying(plan_id, active_attempt)
        .expect("retire crashed attempt");

    // The replacement attempt can now be issued, but a hostile current
    // object drives it to the terminal Conflicted state.
    let replacement_approval = setup
        .issue_host_approval(plan_id)
        .expect("replacement host approval");
    let mut hostile = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"hostile");
    assert_eq!(
        setup.apply_restore(&mut hostile, plan_id, replacement_approval),
        Err(CheckpointFailure::RecoveryConflict)
    );

    let mut reopened = CheckpointRegistry::new(shared, limits);
    let projection = reopened
        .state_snapshot()
        .expect("conflict remains valid after reopen")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan_id)
        .expect("plan projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Conflicted);
    assert!(projection.tombstoned());
    assert_eq!(
        reopened.issue_host_approval(plan_id),
        Err(CheckpointFailure::RecoveryConflict)
    );
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    assert_eq!(
        reopened.apply_restore(&mut authority, plan_id, first_approval),
        Err(CheckpointFailure::RecoveryConflict)
    );
}

#[test]
fn applying_attempt_rejects_a_fresh_approval_until_superseded() {
    let (store, limits, scope, object, plan_id) = prepared_plan();
    let mut registry = CheckpointRegistry::new(store, limits);
    let approval = registry
        .issue_host_approval(plan_id)
        .expect("initial host approval");
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    assert_eq!(
        registry.apply_restore_with_budget(
            &mut authority,
            plan_id,
            approval,
            CaptureBudget::unbounded().with_work_limit(12),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );
    assert_eq!(
        registry.issue_host_approval(plan_id),
        Err(CheckpointFailure::AttemptInProgress)
    );
}

#[test]
fn supersede_requires_the_exact_persisted_attempt_receipt() {
    let (store, limits, scope, object, plan_id) = prepared_plan();
    let mut registry = CheckpointRegistry::new(store, limits);
    let initial = registry
        .issue_host_approval(plan_id)
        .expect("initial approval");
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    assert_eq!(
        registry.apply_restore_with_budget(
            &mut authority,
            plan_id,
            initial,
            CaptureBudget::unbounded().with_work_limit(12),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );

    let active = registry
        .resume_applying(plan_id)
        .expect("durable active receipt");
    let mut stale_revision = active;
    stale_revision.expected_state_version = stale_revision
        .expected_state_version
        .checked_sub(1)
        .expect("active state version is nonzero");
    assert_eq!(
        registry.supersede_applying(plan_id, stale_revision),
        Err(CheckpointFailure::Superseded)
    );
    assert_eq!(
        registry
            .state_snapshot()
            .expect("stale receipt leaves state unchanged")
            .plan_projections()
            .into_iter()
            .find(|projection| projection.id() == plan_id)
            .expect("active plan projection")
            .status(),
        RecoveryStatusProjection::Applying
    );
    assert_eq!(
        registry.supersede_applying(plan_id, initial),
        Err(CheckpointFailure::Unauthorized)
    );
    registry
        .supersede_applying(plan_id, active)
        .expect("exact receipt retires the attempt");
}

#[test]
fn independent_sqlite_attempt_cannot_tombstone_a_newer_applied_attempt() {
    let (_directory, path, limits, scope, object, _checkpoint_id, plan_id) = prepared_sqlite_plan();
    let control = Arc::new(ConflictRaceControl {
        replace_calls: AtomicUsize::new(0),
        pause_on_call: 3,
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let a_store = ConflictRaceStore {
        inner: SqliteCheckpointStore::open(&path).expect("connection A"),
        control: control.clone(),
    };
    let mut a_registry = CheckpointRegistry::new(a_store, limits);
    let approval_a = a_registry.issue_host_approval(plan_id).expect("approval A");
    let apply_a = std::thread::spawn(move || {
        let mut authority =
            FakeSealedWorkspace::new(scope, test_revision()).present(object, b"hostile");
        a_registry.apply_restore(&mut authority, plan_id, approval_a)
    });

    // A has reread the hostile object and is paused immediately before its
    // attempt-owned conflict CAS. Connection B is now allowed to finish a
    // fresh approval and apply the plan.
    control.entered.wait();
    let mut b_registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("connection B"),
        limits,
    );
    let active_attempt_b = b_registry
        .resume_applying(plan_id)
        .expect("active attempt receipt");
    b_registry
        .supersede_applying(plan_id, active_attempt_b)
        .expect("retire attempt A");
    let approval_b = b_registry.issue_host_approval(plan_id).expect("approval B");
    let mut authority_b =
        FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    assert_eq!(
        b_registry.apply_restore(&mut authority_b, plan_id, approval_b),
        Ok(RecoveryApplyOutcome::Applied { changed: 1 })
    );
    control.release.wait();

    assert_eq!(
        apply_a.join().expect("apply A thread"),
        Ok(RecoveryApplyOutcome::AlreadyApplied)
    );
    let reopened = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen connection"),
        limits,
    );
    let projection = reopened
        .state_snapshot()
        .expect("durable terminal state")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan_id)
        .expect("plan projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Applied);
    assert!(projection.tombstoned());
}

#[test]
fn superseded_sqlite_attempt_cannot_complete_or_apply_after_new_attempt() {
    let (_directory, path, limits, scope, object, _checkpoint_id, plan_id) = prepared_sqlite_plan();
    let control = Arc::new(ConflictRaceControl {
        replace_calls: AtomicUsize::new(0),
        pause_on_call: 3,
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    let a_store = ConflictRaceStore {
        inner: SqliteCheckpointStore::open(&path).expect("connection A"),
        control: control.clone(),
    };
    let mut a_registry = CheckpointRegistry::new(a_store, limits);
    let approval_a = a_registry.issue_host_approval(plan_id).expect("approval A");
    let apply_a = std::thread::spawn(move || {
        let mut authority =
            FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
        a_registry.apply_restore(&mut authority, plan_id, approval_a)
    });

    control.entered.wait();
    let mut b_registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("connection B"),
        limits,
    );
    let active_attempt_b = b_registry
        .resume_applying(plan_id)
        .expect("active attempt receipt");
    b_registry
        .supersede_applying(plan_id, active_attempt_b)
        .expect("retire attempt A");
    let approval_b = b_registry.issue_host_approval(plan_id).expect("approval B");
    control.release.wait();

    assert_eq!(
        apply_a.join().expect("apply A thread"),
        Err(CheckpointFailure::Superseded)
    );
    let mut authority_b =
        FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    assert_eq!(
        b_registry.apply_restore(&mut authority_b, plan_id, approval_b),
        Ok(RecoveryApplyOutcome::Applied { changed: 1 })
    );
    let reopened = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen connection"),
        limits,
    );
    let projection = reopened
        .state_snapshot()
        .expect("durable terminal state")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan_id)
        .expect("plan projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Applied);
    assert!(projection.tombstoned());
}

#[test]
fn sqlite_conflict_wins_and_blocks_a_fresh_approval_after_reopen() {
    let (_directory, path, limits, scope, object, _checkpoint_id, plan_id) = prepared_sqlite_plan();
    let mut first = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("connection A"),
        limits,
    );
    let approval = first.issue_host_approval(plan_id).expect("approval A");
    let mut hostile = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"hostile");
    assert_eq!(
        first.apply_restore(&mut hostile, plan_id, approval),
        Err(CheckpointFailure::RecoveryConflict)
    );
    drop(first);

    let mut reopened = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("connection B"),
        limits,
    );
    let projection = reopened
        .state_snapshot()
        .expect("conflict remains valid after reopen")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan_id)
        .expect("plan projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Conflicted);
    assert!(projection.tombstoned());
    assert_eq!(
        reopened.issue_host_approval(plan_id),
        Err(CheckpointFailure::RecoveryConflict)
    );
}

#[test]
fn conflict_wins_before_new_approval_and_reopen_rejects_issuance() {
    let (shared, limits, scope, object, plan_id) = prepared_plan();
    let mut registry = CheckpointRegistry::new(shared.clone(), limits);
    let approval = registry
        .issue_host_approval(plan_id)
        .expect("host approval");
    let mut authority =
        FakeSealedWorkspace::new(scope, test_revision()).present(object, b"hostile");
    assert_eq!(
        registry.apply_restore(&mut authority, plan_id, approval),
        Err(CheckpointFailure::RecoveryConflict)
    );

    let mut reopened = CheckpointRegistry::new(shared, limits);
    let projection = reopened
        .state_snapshot()
        .expect("conflict remains valid after reopen")
        .plan_projections()
        .into_iter()
        .find(|projection| projection.id() == plan_id)
        .expect("plan projection");
    assert_eq!(projection.status(), RecoveryStatusProjection::Conflicted);
    assert!(projection.tombstoned());
    assert_eq!(
        reopened.issue_host_approval(plan_id),
        Err(CheckpointFailure::RecoveryConflict)
    );
}

#[test]
fn external_metadata_ingress_rejects_a_multi_megabyte_nested_content_body() {
    let body = std::iter::repeat("0")
        .take(2 * 1024 * 1024)
        .collect::<Vec<_>>()
        .join(",");
    let payload = format!(
        r#"{{"version":0,"checkpoints":[],"contents":[{{"address":{{}},"bytes":[{}]}}],"plans":[]}}"#,
        body
    );
    let result = DurableCheckpointMetadata::decode_json(payload.as_bytes());
    assert!(
        result.is_err(),
        "public state ingress must reject an unbounded nested body"
    );
}

#[test]
fn external_metadata_ingress_rejects_an_oversized_nested_operation_sequence() {
    let (store, limits, scope, object, plan_id) = prepared_plan();
    let registry = CheckpointRegistry::new(store, limits);
    let metadata = registry
        .state_snapshot()
        .expect("state")
        .metadata_projection();
    let mut payload = serde_json::to_value(metadata).expect("metadata wire");
    let plans = payload["plans"].as_array_mut().expect("plans");
    let plan = plans[0].clone();
    plans.clear();
    for _ in 0..4097 {
        plans.push(plan.clone());
    }
    let result = DurableCheckpointMetadata::decode_json(
        serde_json::to_vec(&payload)
            .expect("metadata payload")
            .as_slice(),
    );
    assert!(
        result.is_err(),
        "nested operation count must be rejected before allocation"
    );
    let _ = (scope, object, plan_id);
}

#[test]
fn external_metadata_rejects_private_fields_at_every_nested_level() {
    let (store, limits, _scope, _object, _plan_id) = prepared_plan();
    let registry = CheckpointRegistry::new(store, limits);
    let metadata = registry
        .state_snapshot()
        .expect("state")
        .metadata_projection();
    let mut top_level = serde_json::to_value(&metadata).expect("metadata");
    top_level["fingerprint"] = serde_json::json!("private");
    assert!(DurableCheckpointMetadata::decode_json(
        serde_json::to_vec(&top_level)
            .expect("top-level payload")
            .as_slice(),
    )
    .is_err());

    let mut nested = serde_json::to_value(&metadata).expect("metadata");
    nested["plans"][0]["scope"]["workspace"] = serde_json::json!("private");
    assert!(DurableCheckpointMetadata::decode_json(
        serde_json::to_vec(&nested)
            .expect("nested payload")
            .as_slice(),
    )
    .is_err());
}

#[test]
fn external_metadata_projection_contains_no_private_content_or_fingerprints() {
    let (store, limits, _scope, _object, _plan_id) = prepared_plan();
    let registry = CheckpointRegistry::new(store, limits);
    let wire = serde_json::to_string(
        &registry
            .state_snapshot()
            .expect("state")
            .metadata_projection(),
    )
    .expect("metadata transport");
    for private_name in ["digest", "fingerprint", "expected", "operations", "body"] {
        assert!(
            !wire.contains(&format!("\"{private_name}\"")),
            "leaked {private_name}"
        );
    }
}

#[test]
fn sqlite_private_codec_rejects_deep_nested_wire_before_deserialization() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let mut payload = vec![0x91u8; 40];
    payload.push(0xc0);
    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace adversarial payload");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_private_codec_rejects_a_multi_megabyte_declared_payload_before_copy() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let payload = [0xc6u8, 0x04, 0x00, 0x00, 0x00];
    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload.as_slice()],
        )
        .expect("replace adversarial payload");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_store_rejects_a_caller_oversize_wire_before_payload_copy() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let payload = vec![0xc0u8; super::state_wire_limit(1, 1, 1) + 1];
    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace oversize wire");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(1, 128, 64, 1),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_store_rejects_content_cap_plus_one_before_deserializing_a_malformed_row() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    // The second content element is intentionally malformed. A bounded
    // preflight must reject the cap+1 row count before the codec attempts to
    // deserialize either element.
    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 5).expect("state map");
    rmp::encode::write_str(&mut payload, "codec_version").expect("codec key");
    rmp::encode::write_uint(&mut payload, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("codec value");
    rmp::encode::write_str(&mut payload, "state_version").expect("state key");
    rmp::encode::write_uint(&mut payload, 0).expect("state value");
    rmp::encode::write_str(&mut payload, "checkpoints").expect("checkpoint key");
    rmp::encode::write_array_len(&mut payload, 0).expect("checkpoint array");
    rmp::encode::write_str(&mut payload, "contents").expect("content key");
    rmp::encode::write_array_len(&mut payload, 2).expect("content array");
    payload.push(0xc1);
    payload.push(0xc1);
    rmp::encode::write_str(&mut payload, "plans").expect("plan key");
    rmp::encode::write_array_len(&mut payload, 0).expect("plan array");

    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace adversarial payload");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(1, 128, 64, 1),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_store_rejects_duplicate_content_before_copying_any_blob_after_reopen() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let body = b"duplicate-content";
    let address = super::content_address(body);
    let record = super::PersistedContentRecord {
        address: super::persisted_address(address),
        bytes_len: body.len() as u64,
    };
    let wire = super::PersistedStateWire {
        codec_version: super::PERSISTED_CODEC_VERSION,
        state_version: 0,
        checkpoints: Vec::new(),
        contents: vec![record.clone(), record],
        plans: Vec::new(),
    };
    let payload = rmp_serde::to_vec_named(&wire).expect("duplicate content wire");

    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace duplicate payload");
    connection
        .execute(
            "INSERT INTO checkpoint_content(digest, bytes, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                address.digest.as_slice(),
                body.len() as i64,
                body.as_slice()
            ],
        )
        .expect("seed referenced content");
    drop(connection);

    let reopened = SqliteCheckpointStore::open(&path).expect("reopen sqlite store");
    let registry = CheckpointRegistry::new(reopened.clone(), CheckpointLimits::DEFAULT);
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::CorruptState)
    ));
    assert_eq!(
        reopened.blob_read_count(),
        0,
        "duplicate index must fail before copying the referenced body"
    );
}

#[test]
fn sqlite_store_rejects_nested_object_cap_plus_one_before_deserializing() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 5).expect("state map");
    rmp::encode::write_str(&mut payload, "codec_version").expect("codec key");
    rmp::encode::write_uint(&mut payload, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("codec value");
    rmp::encode::write_str(&mut payload, "state_version").expect("state key");
    rmp::encode::write_uint(&mut payload, 0).expect("state value");
    rmp::encode::write_str(&mut payload, "checkpoints").expect("checkpoint key");
    rmp::encode::write_array_len(&mut payload, 1).expect("checkpoint array");
    rmp::encode::write_map_len(&mut payload, 1).expect("checkpoint map");
    rmp::encode::write_str(&mut payload, "objects").expect("objects key");
    rmp::encode::write_array_len(&mut payload, 2).expect("objects array");
    payload.push(0xc1);
    payload.push(0xc1);
    rmp::encode::write_str(&mut payload, "contents").expect("content key");
    rmp::encode::write_array_len(&mut payload, 0).expect("content array");
    rmp::encode::write_str(&mut payload, "plans").expect("plan key");
    rmp::encode::write_array_len(&mut payload, 0).expect("plan array");

    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace adversarial payload");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(1, 128, 64, 1),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_store_rejects_unnamed_nested_array_cap_plus_one_before_deserializing() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 5).expect("state map");
    rmp::encode::write_str(&mut payload, "codec_version").expect("codec key");
    rmp::encode::write_uint(&mut payload, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("codec value");
    rmp::encode::write_str(&mut payload, "state_version").expect("state key");
    rmp::encode::write_uint(&mut payload, 0).expect("state value");
    rmp::encode::write_str(&mut payload, "checkpoints").expect("checkpoint key");
    rmp::encode::write_array_len(&mut payload, 1).expect("checkpoint array");
    rmp::encode::write_map_len(&mut payload, 1).expect("checkpoint map");
    rmp::encode::write_str(&mut payload, "junk").expect("junk key");
    rmp::encode::write_array_len(&mut payload, 2).expect("junk array");
    payload.push(0xc1);
    payload.push(0xc1);
    rmp::encode::write_str(&mut payload, "contents").expect("content key");
    rmp::encode::write_array_len(&mut payload, 0).expect("content array");
    rmp::encode::write_str(&mut payload, "plans").expect("plan key");
    rmp::encode::write_array_len(&mut payload, 0).expect("plan array");

    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET payload = ?1 WHERE singleton = 1",
            rusqlite::params![payload],
        )
        .expect("replace adversarial payload");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(1, 128, 64, 1),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::StateTooLarge)
    ));
}

#[test]
fn sqlite_store_rejects_payload_and_version_column_drift() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    drop(store);

    let connection = Connection::open(&path).expect("raw sqlite connection");
    connection
        .execute(
            "UPDATE checkpoint_state SET version = 1 WHERE singleton = 1",
            [],
        )
        .expect("drift version column");
    drop(connection);

    let registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
        CheckpointLimits::new(4, 128, 64, 8),
    );
    assert!(matches!(
        registry.state_snapshot(),
        Err(CheckpointFailure::CorruptState)
    ));
}

#[test]
fn sqlite_store_reopen_hydrates_content_and_fingerprints_for_apply() {
    let (_directory, path, limits, scope, object, checkpoint_id) = {
        let (memory, limits, scope, object, plan_id) = prepared_plan();
        let state = CheckpointRegistry::new(memory, limits)
            .state_snapshot()
            .expect("state");
        let checkpoint_id = state.plan(plan_id).expect("prepared plan").checkpoint_id();
        let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
        let path = directory.path().join("checkpoint.sqlite3");
        let sqlite = SqliteCheckpointStore::open(&path).expect("sqlite store");
        sqlite
            .replace_atomic(0, state, &OperationBudget::unbounded())
            .expect("seed sqlite state");
        (directory, path, limits, scope, object, checkpoint_id)
    };
    let mut registry = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("first connection"),
        limits,
    );
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
    let plan = registry
        .preview_restore(
            &mut authority,
            RestoreRequest::new(scope, checkpoint_id, vec![object]),
        )
        .expect("preview");
    let approval = registry.issue_host_approval(plan.id()).expect("approval");
    drop(registry);

    let mut reopened = CheckpointRegistry::new(
        SqliteCheckpointStore::open(&path).expect("reopened connection"),
        limits,
    );
    assert_eq!(
        reopened.apply_restore(&mut authority, plan.id(), approval),
        Ok(RecoveryApplyOutcome::Applied { changed: 1 })
    );
}

#[test]
fn sqlite_store_uses_full_sync_and_bounded_busy_timeout() {
    let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
    let path = directory.path().join("checkpoint.sqlite3");
    let store = SqliteCheckpointStore::open_with_busy_timeout(&path, Duration::from_secs(60))
        .expect("sqlite store");
    assert_eq!(store.busy_timeout(), Duration::from_secs(1));
    let (journal_mode, synchronous) = store.durability_settings().expect("durability settings");
    assert_eq!(journal_mode.to_ascii_uppercase(), "WAL");
    assert_eq!(synchronous, 2, "SQLite FULL synchronous mode");
}

#[test]
fn capture_and_preview_do_not_report_success_after_store_expiry() {
    let scope = scope();
    let object = ObjectRef::file(ObjectId::new());
    let limits = CheckpointLimits::new(4, 128, 64, 8);
    let cancelled = Arc::new(AtomicBool::new(false));
    let store = CancelAfterReplaceStore {
        inner: InMemoryCheckpointStore::default(),
        cancelled: cancelled.clone(),
    };
    let mut authority = FakeSealedWorkspace::new(scope, test_revision()).present(object, b"before");
    let mut registry = CheckpointRegistry::new(store, limits);
    assert_eq!(
        registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 92, 92),
            ),
            CaptureBudget::unbounded().with_cancellation(cancelled.clone()),
        ),
        Err(CheckpointFailure::Cancelled)
    );
    assert_eq!(
        registry
            .state_snapshot()
            .expect("captured receipt remains visible")
            .checkpoint_count(),
        1
    );

    let base = InMemoryCheckpointStore::default();
    let mut setup = CheckpointRegistry::new(base.clone(), limits);
    let mut setup_authority =
        FakeSealedWorkspace::new(scope, test_revision()).present(object, b"before");
    let checkpoint = setup
        .capture(
            &mut setup_authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 93, 93),
            ),
            CaptureBudget::unbounded(),
        )
        .expect("setup capture");
    setup_authority.mutate(object, b"after");
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut preview = CheckpointRegistry::new(
        CancelAfterReplaceStore {
            inner: base,
            cancelled: cancelled.clone(),
        },
        limits,
    );
    assert_eq!(
        preview.preview_restore_with_budget(
            &mut setup_authority,
            RestoreRequest::new(scope, checkpoint.id(), vec![object]),
            CaptureBudget::unbounded().with_cancellation(cancelled.clone()),
        ),
        Err(CheckpointFailure::Cancelled)
    );
    assert_eq!(
        preview
            .state_snapshot()
            .expect("planned receipt remains visible")
            .plan_count(),
        1
    );
}

#[test]
fn snapshot_does_not_report_success_after_load_expiry() {
    let (base, limits, scope, object, _plan_id) = prepared_plan();
    let cancelled = Arc::new(AtomicBool::new(false));
    let registry = CheckpointRegistry::new(
        CancelAfterLoadStore {
            inner: base,
            cancelled: cancelled.clone(),
        },
        limits,
    );
    let _ = (scope, object);
    assert!(matches!(
        registry
            .state_snapshot_with_budget(CaptureBudget::unbounded().with_cancellation(cancelled)),
        Err(CheckpointFailure::Cancelled)
    ));
}

#[test]
fn sqlite_fault_boundaries_leave_only_the_old_or_new_state_after_reopen() {
    for fault in [
        SqliteFaultPoint::BeforeCommit,
        SqliteFaultPoint::AfterCommit,
    ] {
        let directory = tempfile::tempdir().expect("checkpoint sqlite directory");
        let path = directory.path().join("checkpoint.sqlite3");
        let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
        store.arm_fault(fault);
        let limits = CheckpointLimits::new(4, 128, 64, 8);
        let scope = scope();
        let object = ObjectRef::file(ObjectId::new());
        let mut registry = CheckpointRegistry::new(store, limits);
        let mut authority =
            FakeSealedWorkspace::new(scope, test_revision()).present(object, b"body");
        let result = registry.capture(
            &mut authority,
            CaptureRequest::new(
                scope,
                vec![object],
                CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 91, 91),
            ),
            CaptureBudget::unbounded(),
        );
        assert_eq!(result, Err(CheckpointFailure::StateUnavailable));
        drop(registry);
        let reopened =
            CheckpointRegistry::new(SqliteCheckpointStore::open(&path).expect("reopen"), limits);
        let count = reopened
            .state_snapshot()
            .expect("atomic reopen")
            .checkpoint_count();
        assert_eq!(count, usize::from(fault == SqliteFaultPoint::AfterCommit));
    }
}

#[test]
fn sqlite_fault_injection_covers_attempt_claim_receipt_and_terminal_cases() {
    for fault in [
        SqliteFaultPoint::BeforeCommit,
        SqliteFaultPoint::AfterCommit,
    ] {
        for replacement_index in 0..4 {
            let (_directory, path, limits, scope, object, _checkpoint_id, plan_id) =
                prepared_sqlite_plan();
            let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
            let fault_store = store.clone();
            let mut registry = CheckpointRegistry::new(store, limits);
            let approval = registry
                .issue_host_approval(plan_id)
                .expect("host approval");
            fault_store.arm_fault_after(fault, replacement_index);

            let mut authority =
                FakeSealedWorkspace::new(scope, test_revision()).present(object, b"after");
            assert_eq!(
                registry.apply_restore(&mut authority, plan_id, approval),
                Err(CheckpointFailure::StateUnavailable),
                "fault {fault:?} at replacement {replacement_index}"
            );
            drop(registry);

            let mut reopened = CheckpointRegistry::new(
                SqliteCheckpointStore::open(&path).expect("reopen sqlite store"),
                limits,
            );
            if fault == SqliteFaultPoint::AfterCommit && replacement_index == 3 {
                let receipt = reopened
                    .issue_host_approval(plan_id)
                    .expect("idempotent receipt after terminal commit");
                assert_eq!(
                    reopened.apply_restore(&mut authority, plan_id, receipt),
                    Ok(RecoveryApplyOutcome::AlreadyApplied)
                );
            } else {
                let recovery_approval =
                    if fault == SqliteFaultPoint::BeforeCommit && replacement_index == 0 {
                        approval
                    } else {
                        reopened
                            .resume_applying(plan_id)
                            .expect("durable applying receipt")
                    };
                assert!(matches!(
                    reopened.apply_restore(&mut authority, plan_id, recovery_approval),
                    Ok(RecoveryApplyOutcome::Applied { .. })
                        | Ok(RecoveryApplyOutcome::Replayed { .. })
                ));
            }
            let projection = reopened
                .state_snapshot()
                .expect("atomic terminal state")
                .plan_projections()
                .into_iter()
                .find(|projection| projection.id() == plan_id)
                .expect("plan projection");
            assert_eq!(projection.status(), RecoveryStatusProjection::Applied);
            assert!(projection.tombstoned());
        }
    }
}

#[test]
fn preview_budget_checks_expiry_and_work_before_authority_or_store_io() {
    let (store, limits, scope, object, checkpoint_id) = {
        let scope = scope();
        let object = ObjectRef::file(ObjectId::new());
        let limits = CheckpointLimits::new(4, 128, 64, 8);
        let store = InMemoryCheckpointStore::default();
        let mut registry = CheckpointRegistry::new(store.clone(), limits);
        let mut authority =
            FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"before");
        let checkpoint = registry
            .capture(
                &mut authority,
                CaptureRequest::new(
                    scope,
                    vec![object],
                    CaptureContext::new(CheckpointReason::Manual, AgentToken::new(), 21, 21),
                ),
                CaptureBudget::unbounded(),
            )
            .expect("capture");
        (store, limits, scope, object, checkpoint.id())
    };
    let mut authority = ProbeAuthority::new(
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"after"),
    );
    let store = ProbeStore::new(store);
    let loads = store.loads.clone();
    let mut registry = CheckpointRegistry::new(store, limits);
    let request = RestoreRequest::new(scope, checkpoint_id, vec![object]);
    assert_eq!(
        registry.preview_restore_with_budget(
            &mut authority,
            request,
            CaptureBudget::unbounded().with_deadline(Instant::now() - Duration::from_secs(1)),
        ),
        Err(CheckpointFailure::DeadlineExceeded)
    );
    assert_eq!(authority.scope_reads.load(Ordering::Acquire), 0);
    assert_eq!(authority.revision_reads.load(Ordering::Acquire), 0);
    assert_eq!(loads.load(Ordering::Acquire), 0);

    let mut authority = ProbeAuthority::new(
        FakeSealedWorkspace::new(scope, WorkspaceRevision::new()).present(object, b"after"),
    );
    assert_eq!(
        registry.preview_restore_with_budget(
            &mut authority,
            RestoreRequest::new(scope, checkpoint_id, vec![object]),
            CaptureBudget::unbounded().with_work_limit(1),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );
    assert_eq!(authority.scope_reads.load(Ordering::Acquire), 0);
    assert_eq!(authority.revision_reads.load(Ordering::Acquire), 0);
}

#[test]
fn approval_budget_checks_expiry_and_work_before_state_store_io() {
    let (store, limits, _scope, _object, plan_id) = prepared_plan();
    let store = ProbeStore::new(store);
    let loads = store.loads.clone();
    let mut registry = CheckpointRegistry::new(store, limits);
    assert_eq!(
        registry.issue_host_approval_with_budget(
            plan_id,
            CaptureBudget::unbounded().with_deadline(Instant::now() - Duration::from_secs(1)),
        ),
        Err(CheckpointFailure::DeadlineExceeded)
    );
    assert_eq!(loads.load(Ordering::Acquire), 0);
    assert_eq!(
        registry.issue_host_approval_with_budget(
            plan_id,
            CaptureBudget::unbounded().with_work_limit(0),
        ),
        Err(CheckpointFailure::WorkLimitExceeded)
    );
    assert_eq!(loads.load(Ordering::Acquire), 0);
}

#[test]
fn snapshot_budget_checks_expiry_and_work_before_state_store_io() {
    let store = ProbeStore::new(InMemoryCheckpointStore::default());
    let loads = store.loads.clone();
    let registry = CheckpointRegistry::new(store, CheckpointLimits::new(4, 128, 64, 8));
    assert!(matches!(
        registry.state_snapshot_with_budget(
            CaptureBudget::unbounded().with_deadline(Instant::now() - Duration::from_secs(1)),
        ),
        Err(CheckpointFailure::DeadlineExceeded)
    ));
    assert_eq!(loads.load(Ordering::Acquire), 0);
    assert!(matches!(
        registry.state_snapshot_with_budget(CaptureBudget::unbounded().with_work_limit(0)),
        Err(CheckpointFailure::WorkLimitExceeded)
    ));
    assert_eq!(loads.load(Ordering::Acquire), 0);
}

#[test]
fn private_codec_rejects_an_unsupported_schema_version_before_decode() {
    let wire = super::PersistedStateWire {
        codec_version: super::PERSISTED_CODEC_VERSION + 1,
        state_version: 0,
        checkpoints: Vec::new(),
        contents: Vec::new(),
        plans: Vec::new(),
    };
    let bytes = rmp_serde::to_vec_named(&wire).expect("wire encoding");
    assert!(matches!(
        super::decode_persisted_state(&bytes, &CaptureBudget::unbounded()),
        Err(StateStoreFailure::InvalidVersion)
    ));
}

#[test]
fn private_codec_rejects_trailing_msgpack_before_deserialization() {
    let state = DurableCheckpointState::default();
    let mut bytes =
        super::encode_persisted_state(&state, &CaptureBudget::unbounded()).expect("wire encoding");
    bytes.push(0xc0);
    assert!(matches!(
        super::decode_persisted_state(&bytes, &CaptureBudget::unbounded()),
        Err(StateStoreFailure::Corrupt)
    ));
}

#[test]
fn private_codec_rejects_duplicate_codec_field_before_deserialization() {
    let mut bytes = Vec::new();
    rmp::encode::write_map_len(&mut bytes, 6).expect("map header");
    rmp::encode::write_str(&mut bytes, "codec_version").expect("codec key");
    rmp::encode::write_uint(&mut bytes, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("codec value");
    rmp::encode::write_str(&mut bytes, "codec_version").expect("duplicate codec key");
    rmp::encode::write_uint(&mut bytes, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("duplicate codec value");
    rmp::encode::write_str(&mut bytes, "state_version").expect("state key");
    rmp::encode::write_uint(&mut bytes, 0).expect("state value");
    for (key, value) in [("checkpoints", 0u32), ("contents", 0), ("plans", 0)] {
        rmp::encode::write_str(&mut bytes, key).expect("collection key");
        rmp::encode::write_array_len(&mut bytes, value).expect("collection value");
    }
    assert!(matches!(
        super::decode_persisted_state(&bytes, &CaptureBudget::unbounded()),
        Err(StateStoreFailure::Corrupt)
    ));
}

#[test]
fn private_codec_rejects_duplicate_non_codec_field_before_deserialization() {
    let mut bytes = Vec::new();
    rmp::encode::write_map_len(&mut bytes, 6).expect("map header");
    rmp::encode::write_str(&mut bytes, "codec_version").expect("codec key");
    rmp::encode::write_uint(&mut bytes, u64::from(super::PERSISTED_CODEC_VERSION))
        .expect("codec value");
    rmp::encode::write_str(&mut bytes, "state_version").expect("state key");
    rmp::encode::write_uint(&mut bytes, 0).expect("state value");
    rmp::encode::write_str(&mut bytes, "state_version").expect("duplicate state key");
    rmp::encode::write_uint(&mut bytes, 0).expect("duplicate state value");
    for (key, value) in [("checkpoints", 0u32), ("contents", 0), ("plans", 0)] {
        rmp::encode::write_str(&mut bytes, key).expect("collection key");
        rmp::encode::write_array_len(&mut bytes, value).expect("collection value");
    }
    assert!(matches!(
        super::scan_persisted_codec_version(&bytes, &CaptureBudget::unbounded()),
        Err(StateStoreFailure::Corrupt)
    ));
}

#[test]
fn in_memory_store_lock_wait_honors_cancellation_before_guard_acquisition() {
    let store = InMemoryCheckpointStore::default();
    let guard = store.state.lock().expect("hold store lock");
    let cancelled = Arc::new(AtomicBool::new(false));
    let budget = OperationBudget::unbounded().with_cancellation(cancelled.clone());
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker_store = store.clone();
    let worker = std::thread::spawn(move || {
        let result = worker_store.load_bounded(super::hard_state_load_limits(), &budget);
        sender.send(result).expect("send lock wait result");
    });

    std::thread::sleep(Duration::from_millis(20));
    cancelled.store(true, Ordering::Release);
    assert!(
        receiver.recv_timeout(Duration::from_millis(250)).is_ok(),
        "cancelled lock wait must return before the guard is released"
    );
    drop(guard);
    worker.join().expect("join lock wait worker");
}

#[test]
fn sqlite_store_budget_cancellation_short_circuits_before_connection_work() {
    let directory = tempfile::tempdir().expect("sqlite directory");
    let store =
        SqliteCheckpointStore::open(directory.path().join("cancel.sqlite3")).expect("sqlite store");
    let cancelled = Arc::new(AtomicBool::new(true));
    let budget = OperationBudget::unbounded().with_cancellation(cancelled);
    assert!(matches!(
        store.load_bounded(super::hard_state_load_limits(), &budget),
        Err(StateStoreFailure::Unavailable)
    ));
}

#[test]
fn sqlite_busy_wait_observes_cancellation_before_the_bounded_timeout() {
    let directory = tempfile::tempdir().expect("sqlite directory");
    let path = directory.path().join("busy-cancel.sqlite3");
    let store = SqliteCheckpointStore::open(&path).expect("sqlite store");
    let blocker = Connection::open(&path).expect("blocker connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold sqlite write lock");

    let cancelled = Arc::new(AtomicBool::new(false));
    let budget = OperationBudget::unbounded().with_cancellation(cancelled.clone());
    let worker_store = store.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = worker_store.load_bounded(super::hard_state_load_limits(), &budget);
        sender.send(result).expect("send sqlite wait result");
    });

    std::thread::sleep(Duration::from_millis(20));
    cancelled.store(true, Ordering::Release);
    assert!(
        receiver.recv_timeout(Duration::from_millis(150)).is_ok(),
        "sqlite busy wait must observe cancellation before the configured timeout"
    );
    blocker
        .execute_batch("ROLLBACK")
        .expect("release sqlite write lock");
    worker.join().expect("join sqlite wait worker");
}
