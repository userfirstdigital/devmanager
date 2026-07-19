use super::commands::verified_authenticated_local_project_root;
use super::recipes::{
    canonical_browser_recipe_digest, recipe_step_locator_at, replace_recipe_locator_atomic,
    BrowserRecipeDigestV1, BrowserRecipeLocatorReplaceError,
};
use super::replay_repair::{
    BrowserReplayRecipeLocatorTarget, BrowserReplayRepairApplyAcknowledgement,
    BrowserReplayRepairApplyAuthority, BrowserReplayRepairApplyReceipt,
    BrowserReplayRepairApplyStage, BrowserReplayRepairAuthorityScope,
    BrowserReplayRepairHighlightCleanup, BrowserReplayRepairHighlightToken,
    BrowserReplayRepairPreviewAbortDisposition, BrowserReplayRepairPreviewAcknowledgement,
    BrowserReplayRepairPreviewAuthority, BrowserReplayRepairPreviewReceipt,
    BrowserReplayRepairResumeCursor, BrowserReplayRepairRetentionAuthority,
};
use super::resources::BrowserReplayRepairRetentionLease;
use super::{
    effective_browser_risk, BrowserError, BrowserInvocationActor, BrowserInvocationContext,
    BrowserRecipeAction, BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep,
    BrowserRecipeV1, BrowserRecipeValue, BrowserRecipeViewport, BrowserReplayLocatorSlot,
    BrowserReplayRepairCandidate, BrowserReplayRepairInstance, BrowserReplayRepairPhase,
    BrowserReplayRepairProjection, BrowserReplaySecretError, BrowserReplaySecretLease,
    BrowserReplaySecretStore, BrowserReplaySecretSubmission, BrowserResourceHandle,
    BrowserResourceKind, BrowserResourceStore, BrowserRevision, BrowserRisk, BrowserWorkspaceKey,
    MAX_BROWSER_REPLAY_SECRET_INPUTS,
};
#[cfg(test)]
use super::{BrowserRecipeAssertion, BrowserRecipeElementState, BrowserRecipeWait};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokio::sync::watch;

pub const MAX_BROWSER_REPLAY_INPUTS: usize = 64;
pub const MAX_BROWSER_REPLAY_STEPS: usize = 256;
pub const MAX_BROWSER_REPLAY_INPUT_NAME_BYTES: usize = 128;
pub const MAX_BROWSER_REPLAY_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_BROWSER_REPLAY_URL_BYTES: usize = 8 * 1024;
pub const MAX_BROWSER_REPLAY_FILE_BYTES: usize = 32 * 1024;
const MAX_BROWSER_REPLAY_REPAIR_TAB_ID_BYTES: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserReplayError {
    InvalidRecipe,
    CapacityExceeded,
    InvalidPublicInputName,
    DuplicatePublicInput,
    UnknownPublicInput,
    MissingPublicInput,
    PublicSecretRejected,
    InputKindMismatch,
    InvalidTextInput,
    InvalidUrlInput,
    InvalidFileInput,
    AlreadyActive,
    StaleInstance,
    InvalidExecutionAuthority,
    RecipeRootUnavailable,
    RecipeRootAlreadyBound,
    InvalidTransition,
    StepOutOfOrder,
    IncompleteReplay,
    TerminalState,
    InstanceIdExhausted,
    RepairInstanceIdExhausted,
    RepairPreviewIdExhausted,
    RepairApplyIdExhausted,
    RepairConfirmationRequired,
    RepairRecipeChanged,
    RepairCandidateInvalid,
    RepairWriteFailed,
    InvalidRepairSlot,
    InvalidRepairEvidence,
    RepairEvidenceUnavailable,
}

impl fmt::Display for BrowserReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRecipe => "browser replay recipe is invalid",
            Self::CapacityExceeded => "browser replay capacity was reached",
            Self::InvalidPublicInputName => "browser replay public input name is invalid",
            Self::DuplicatePublicInput => "browser replay public input is duplicated",
            Self::UnknownPublicInput => "browser replay public input is not declared",
            Self::MissingPublicInput => "browser replay required public input is missing",
            Self::PublicSecretRejected => "browser replay public secret input is forbidden",
            Self::InputKindMismatch => "browser replay public input kind does not match",
            Self::InvalidTextInput => "browser replay text input is invalid",
            Self::InvalidUrlInput => "browser replay URL input is invalid",
            Self::InvalidFileInput => "browser replay file input is invalid",
            Self::AlreadyActive => "browser replay workspace already has an active instance",
            Self::StaleInstance => "browser replay instance is stale",
            Self::InvalidExecutionAuthority => "browser replay execution authority is invalid",
            Self::RecipeRootUnavailable => "browser replay recipe root is unavailable",
            Self::RecipeRootAlreadyBound => "browser replay recipe root is already bound",
            Self::InvalidTransition => "browser replay status transition is invalid",
            Self::StepOutOfOrder => "browser replay step is out of order",
            Self::IncompleteReplay => "browser replay has incomplete steps",
            Self::TerminalState => "browser replay instance is terminal",
            Self::InstanceIdExhausted => "browser replay instance identity is exhausted",
            Self::RepairInstanceIdExhausted => "browser replay repair identity is exhausted",
            Self::RepairPreviewIdExhausted => "browser replay repair preview identity is exhausted",
            Self::RepairApplyIdExhausted => "browser replay repair apply identity is exhausted",
            Self::RepairConfirmationRequired => {
                "browser replay repair requires explicit confirmation"
            }
            Self::RepairRecipeChanged => "browser replay repair recipe changed",
            Self::RepairCandidateInvalid => "browser replay repair candidate is invalid",
            Self::RepairWriteFailed => "browser replay repair could not be written",
            Self::InvalidRepairSlot => "browser replay repair locator slot is invalid",
            Self::InvalidRepairEvidence => "browser replay repair evidence is invalid",
            Self::RepairEvidenceUnavailable => "browser replay repair evidence is unavailable",
        })
    }
}

impl std::error::Error for BrowserReplayError {}

pub struct BrowserReplayPublicInput {
    name: String,
    kind: BrowserRecipeInputKind,
    value: String,
}

impl BrowserReplayPublicInput {
    pub fn new(
        name: impl Into<String>,
        kind: BrowserRecipeInputKind,
        value: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            value: value.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

struct BrowserReplayBoundValue {
    name: String,
    kind: BrowserRecipeInputKind,
    value: String,
}

pub struct BrowserReplayPlan {
    recipe_id: String,
    #[allow(dead_code)] // Task 7 consumes the private digest during exact repair apply.
    recipe_digest: BrowserRecipeDigestV1,
    start_url: String,
    viewport: BrowserRecipeViewport,
    steps: Vec<BrowserRecipeStep>,
    bindings: Vec<BrowserReplayBoundValue>,
    unresolved_secret_inputs: Vec<String>,
}

impl BrowserReplayPlan {
    pub fn recipe_id(&self) -> &str {
        &self.recipe_id
    }

    pub fn start_url(&self) -> &str {
        &self.start_url
    }

    pub fn viewport(&self) -> BrowserRecipeViewport {
        self.viewport
    }

    pub fn steps(&self) -> &[BrowserRecipeStep] {
        &self.steps
    }

    pub fn unresolved_secret_input_names(&self) -> &[String] {
        &self.unresolved_secret_inputs
    }

    pub fn resolve_input(&self, name: &str) -> Option<&str> {
        self.bindings
            .iter()
            .find(|binding| binding.name == name)
            .map(|binding| binding.value.as_str())
    }

    pub fn input_kind(&self, name: &str) -> Option<BrowserRecipeInputKind> {
        self.bindings
            .iter()
            .find(|binding| binding.name == name)
            .map(|binding| binding.kind)
            .or_else(|| {
                self.unresolved_secret_inputs
                    .iter()
                    .any(|input_name| input_name == name)
                    .then_some(BrowserRecipeInputKind::Secret)
            })
    }

    pub fn bound_input_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.bindings.iter().map(|binding| binding.name.as_str())
    }

    pub fn resolve_value<'a>(&'a self, value: &'a BrowserRecipeValue) -> Option<&'a str> {
        match value {
            BrowserRecipeValue::Literal { value } => Some(value),
            BrowserRecipeValue::Input { name } => self.resolve_input(name),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserReplayStatus {
    Pending,
    Running,
    NeedsUserSecret,
    PausedLocatorRepair,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserReplayFailureCode {
    StepFailed,
    AssertionFailed,
}

struct BrowserReplayCoordinatorScope;

#[derive(Clone)]
pub struct BrowserReplayInstance {
    workspace_key: BrowserWorkspaceKey,
    id: u64,
    scope: Arc<BrowserReplayCoordinatorScope>,
}

impl fmt::Debug for BrowserReplayInstance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserReplayInstance")
            .field("workspace_key", &self.workspace_key)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for BrowserReplayInstance {
    fn eq(&self, other: &Self) -> bool {
        self.workspace_key == other.workspace_key
            && self.id == other.id
            && Arc::ptr_eq(&self.scope, &other.scope)
    }
}

impl Eq for BrowserReplayInstance {}

impl BrowserReplayInstance {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn id(&self) -> u64 {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReplayProjection {
    pub workspace_key: BrowserWorkspaceKey,
    pub instance_id: u64,
    pub recipe_id: String,
    pub status: BrowserReplayStatus,
    pub current_step_index: usize,
    pub total_steps: usize,
    pub current_step_id: Option<String>,
    pub unresolved_secret_inputs: Vec<String>,
    pub failure: Option<BrowserReplayFailureCode>,
}

pub struct BrowserReplayStart {
    pub instance: BrowserReplayInstance,
    pub projection: BrowserReplayProjection,
    pub lease: BrowserReplayCancellationLease,
    pub execution: BrowserReplayExecutionHandle,
}

struct BrowserReplayCancellationAuthority {
    id: u64,
    cancelled: AtomicBool,
}

#[derive(Clone)]
pub struct BrowserReplayCancellationLease {
    authority: Arc<BrowserReplayCancellationAuthority>,
}

impl BrowserReplayCancellationLease {
    pub fn authority_id(&self) -> u64 {
        self.authority.id
    }

    pub fn same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.authority, &other.authority)
    }

    pub fn is_cancelled(&self) -> bool {
        self.authority.cancelled.load(Ordering::Acquire)
    }
}

pub struct BrowserReplayExecutionHandle {
    instance: BrowserReplayInstance,
    plan: Arc<BrowserReplayPlan>,
    lease: BrowserReplayCancellationLease,
    secret_store: BrowserReplaySecretStore,
    repair_watch: watch::Receiver<u64>,
    locator_overrides: Arc<Mutex<HashMap<(usize, BrowserReplayLocatorSlot), BrowserRecipeLocator>>>,
    #[allow(dead_code)] // Task 7 binds this in the executor and reads it during coordinator apply.
    canonical_recipe_root: Arc<OnceLock<PathBuf>>,
}

impl BrowserReplayExecutionHandle {
    pub fn same_instance(&self, instance: &BrowserReplayInstance) -> bool {
        self.instance == *instance
    }

    pub fn same_authority(&self, lease: &BrowserReplayCancellationLease) -> bool {
        self.lease.same_authority(lease)
    }

    pub fn is_cancelled(&self) -> bool {
        self.lease.is_cancelled()
    }

    pub fn secret_lease(
        &self,
        input_name: &str,
    ) -> Result<BrowserReplaySecretLease, BrowserReplaySecretError> {
        self.secret_store.lease(input_name)
    }

    #[cfg(test)]
    fn observe_memory_clear_for_test(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.secret_store.observe_memory_clear_for_test()
    }

    pub(crate) fn plan(&self) -> &BrowserReplayPlan {
        &self.plan
    }

    pub(crate) fn close_secret_store(&self) {
        self.secret_store.close();
    }

    pub(crate) fn repair_watch(&self) -> watch::Receiver<u64> {
        self.repair_watch.clone()
    }

    pub(crate) fn locator_override(
        &self,
        step_index: usize,
        locator_slot: BrowserReplayLocatorSlot,
    ) -> Option<BrowserRecipeLocator> {
        self.locator_overrides
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(step_index, locator_slot))
            .cloned()
    }

    #[allow(dead_code)] // Wired before the first command by Task 7's executor change.
    pub(crate) fn bind_canonical_recipe_root(
        &self,
        authenticated_canonical_root: &Path,
    ) -> Result<(), BrowserReplayError> {
        let canonical = verified_authenticated_local_project_root(authenticated_canonical_root)
            .map_err(|_| BrowserReplayError::RecipeRootUnavailable)?;
        self.canonical_recipe_root
            .set(canonical)
            .map_err(|_| BrowserReplayError::RecipeRootAlreadyBound)
    }

    #[allow(dead_code)] // Task 7 consumes the exact shared binding.
    pub(crate) fn bound_canonical_recipe_root(&self) -> Result<&Path, BrowserReplayError> {
        self.canonical_recipe_root
            .get()
            .map(PathBuf::as_path)
            .ok_or(BrowserReplayError::RecipeRootUnavailable)
    }

    #[allow(dead_code)] // Task 7 passes this private digest to the atomic primitive.
    pub(crate) fn recipe_digest(&self) -> &BrowserRecipeDigestV1 {
        &self.plan.recipe_digest
    }
}

struct BrowserReplayRepairLeaseSlot {
    lease: Mutex<Option<BrowserReplayRepairRetentionLease>>,
}

impl BrowserReplayRepairLeaseSlot {
    fn new(lease: BrowserReplayRepairRetentionLease) -> Self {
        Self {
            lease: Mutex::new(Some(lease)),
        }
    }

    fn with_lease<T>(
        &self,
        operation: impl FnOnce(&mut BrowserReplayRepairRetentionLease) -> Result<T, BrowserError>,
    ) -> Result<T, BrowserError> {
        let mut lease = self
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        operation(
            lease
                .as_mut()
                .ok_or_else(|| BrowserError::BlockedPermission {
                    permission: "repair retention".to_string(),
                })?,
        )
    }

    fn close(&self) {
        self.lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    fn is_live(&self) -> bool {
        self.lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }
}

enum BrowserReplayPrivateRepairPhase {
    Capturing,
    Paused(BrowserReplayPrivatePausedRepair),
    Previewing(BrowserReplayPrivatePreviewReservation),
    Preparing(BrowserReplayPrivateApplyReservation),
    Committing(BrowserReplayPrivateApplyReservation),
}

struct BrowserReplayPrivatePausedRepair {
    projection: BrowserReplayRepairProjection,
    candidate: Option<BrowserReplayRepairCandidate>,
    recipe_locator: Option<BrowserRecipeLocator>,
    highlight: Option<BrowserReplayPrivateInstalledHighlight>,
    applied_preview_fresh: bool,
}

struct BrowserReplayPrivateInstalledHighlight {
    token: BrowserReplayRepairHighlightToken,
    cleanup: BrowserReplayRepairHighlightCleanup,
}

struct BrowserReplayPrivatePreviewReservation {
    previous: BrowserReplayPrivatePausedRepair,
    authority: BrowserReplayRepairPreviewAuthority,
}

struct BrowserReplayPrivateApplyReservation {
    previous: BrowserReplayPrivatePausedRepair,
    authority: BrowserReplayRepairApplyAuthority,
}

pub(crate) struct BrowserReplayRepairApplyCommit {
    pub(crate) repair: BrowserReplayRepairProjection,
    pub(crate) replay: BrowserReplayProjection,
    pub(crate) recipe_written: bool,
    _released_repair: Option<BrowserReplayPrivateRepairState>,
}

struct BrowserReplayPrivateRepairState {
    instance: BrowserReplayRepairInstance,
    resource_store: BrowserResourceStore,
    lease: Arc<BrowserReplayRepairLeaseSlot>,
    recipe_target: BrowserReplayRecipeLocatorTarget,
    tab_id: String,
    revision: BrowserRevision,
    resume_cursor: BrowserReplayRepairResumeCursor,
    snapshot: Option<BrowserResourceHandle>,
    screenshot: Option<BrowserResourceHandle>,
    snapshot_reserved: bool,
    screenshot_reserved: bool,
    phase: BrowserReplayPrivateRepairPhase,
}

impl Drop for BrowserReplayPrivateRepairState {
    fn drop(&mut self) {
        if let BrowserReplayPrivateRepairPhase::Previewing(reservation) = &self.phase {
            reservation.authority.close();
        }
        if let BrowserReplayPrivateRepairPhase::Preparing(reservation) = &self.phase {
            reservation.authority.close();
        }
        if let BrowserReplayPrivateRepairPhase::Committing(reservation) = &self.phase {
            reservation.authority.close();
        }
        self.lease.close();
    }
}

pub(crate) struct BrowserReplayRepairCaptureAuthority {
    repair: BrowserReplayRepairInstance,
    resource_store: BrowserResourceStore,
    lease: Arc<BrowserReplayRepairLeaseSlot>,
    receipt: Arc<Mutex<BrowserReplayRepairCaptureReceiptState>>,
    kind: BrowserResourceKind,
    tab_id: String,
    revision: BrowserRevision,
}

struct BrowserReplayRepairCaptureReceiptState {
    repair: BrowserReplayRepairInstance,
    kind: BrowserResourceKind,
    handle: Option<BrowserResourceHandle>,
    consumed: bool,
}

pub(crate) struct BrowserReplayRepairCaptureReceipt {
    lease: Arc<BrowserReplayRepairLeaseSlot>,
    state: Arc<Mutex<BrowserReplayRepairCaptureReceiptState>>,
}

pub(crate) struct BrowserReplayRepairCapturedEvidence {
    repair: BrowserReplayRepairInstance,
    kind: BrowserResourceKind,
    handle: BrowserResourceHandle,
}

impl BrowserReplayRepairCaptureReceipt {
    pub(crate) fn consume_exact(
        &self,
        repair: &BrowserReplayRepairInstance,
        kind: BrowserResourceKind,
        handle: &BrowserResourceHandle,
    ) -> Option<BrowserReplayRepairCapturedEvidence> {
        let live = self.lease.is_live();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.consumed {
            return None;
        }
        state.consumed = true;
        let exact = live
            && state.repair == *repair
            && state.kind == kind
            && state.handle.take().as_ref() == Some(handle);
        exact.then(|| BrowserReplayRepairCapturedEvidence {
            repair: repair.clone(),
            kind,
            handle: handle.clone(),
        })
    }
}

impl BrowserReplayRepairCaptureAuthority {
    pub(crate) fn repair(&self) -> &BrowserReplayRepairInstance {
        &self.repair
    }

    pub(crate) fn kind(&self) -> BrowserResourceKind {
        self.kind
    }

    pub(crate) fn tab_id(&self) -> &str {
        &self.tab_id
    }

    pub(crate) fn revision(&self) -> BrowserRevision {
        self.revision
    }

    pub(crate) fn is_live(&self) -> bool {
        self.lease.is_live()
    }

    pub(crate) fn retain(
        &self,
        store: &BrowserResourceStore,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        kind: BrowserResourceKind,
        mime_type: &str,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let expected_mime =
            repair_resource_mime(self.kind).ok_or_else(invalid_repair_retention_sidecar)?;
        if workspace_key != self.repair.workspace_key()
            || tab_id != self.tab_id
            || kind != self.kind
            || mime_type != expected_mime
            || !self.resource_store.same_runtime(store)
        {
            return Err(invalid_repair_retention_sidecar());
        }
        let mut receipt = self
            .receipt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if receipt.repair != self.repair
            || receipt.kind != self.kind
            || receipt.consumed
            || receipt.handle.is_some()
        {
            return Err(invalid_repair_retention_sidecar());
        }
        let handle = self
            .lease
            .with_lease(|lease| store.put_repair_retained(lease, kind, mime_type, bytes))
            .map_err(|error| match error {
                BrowserError::ResourceTooLarge { .. } => error,
                BrowserError::BlockedPermission { .. } => invalid_repair_retention_sidecar(),
                _ => BrowserError::ResourceRootUnavailable,
            })?;
        receipt.handle = Some(handle.clone());
        Ok(handle)
    }
}

fn invalid_repair_retention_sidecar() -> BrowserError {
    BrowserError::InvalidInvocation {
        field: "repairSidecar".to_string(),
    }
}

fn repair_resource_mime(kind: BrowserResourceKind) -> Option<&'static str> {
    match kind {
        BrowserResourceKind::ReplayRepairSnapshot => Some("application/json"),
        BrowserResourceKind::ReplayRepairScreenshot => Some("image/png"),
        _ => None,
    }
}

struct ActiveBrowserReplay {
    instance: BrowserReplayInstance,
    plan: Arc<BrowserReplayPlan>,
    projection: BrowserReplayProjection,
    lease: BrowserReplayCancellationLease,
    secret_store: BrowserReplaySecretStore,
    repair: Option<BrowserReplayPrivateRepairState>,
    repair_signal: watch::Sender<u64>,
    repair_generation: u64,
    _locator_overrides:
        Arc<Mutex<HashMap<(usize, BrowserReplayLocatorSlot), BrowserRecipeLocator>>>,
    #[allow(dead_code)] // Shared with the execution handle for Task 7 coordinator-side apply.
    canonical_recipe_root: Arc<OnceLock<PathBuf>>,
}

struct TerminalBrowserReplay {
    instance: BrowserReplayInstance,
    projection: BrowserReplayProjection,
}

fn signal_repair_state(active: &mut ActiveBrowserReplay) {
    active.repair_generation = active.repair_generation.wrapping_add(1);
    active.repair_signal.send_replace(active.repair_generation);
}

struct BrowserReplayCoordinatorState {
    scope: Arc<BrowserReplayCoordinatorScope>,
    repair_scope: Arc<BrowserReplayRepairAuthorityScope>,
    next_instance_id: u64,
    next_repair_id: u64,
    next_preview_id: u64,
    next_apply_id: u64,
    active: HashMap<BrowserWorkspaceKey, ActiveBrowserReplay>,
    terminal: VecDeque<TerminalBrowserReplay>,
    terminal_capacity: usize,
}

impl Drop for BrowserReplayCoordinatorState {
    fn drop(&mut self) {
        for active in self.active.values_mut() {
            active
                .lease
                .authority
                .cancelled
                .store(true, Ordering::Release);
            active.secret_store.close();
            signal_repair_state(active);
        }
    }
}

#[derive(Clone)]
pub struct BrowserReplayCoordinator {
    inner: Arc<Mutex<BrowserReplayCoordinatorState>>,
}

impl Default for BrowserReplayCoordinator {
    fn default() -> Self {
        Self::with_terminal_capacity(128)
    }
}

impl BrowserReplayCoordinator {
    pub fn with_terminal_capacity(terminal_capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BrowserReplayCoordinatorState {
                scope: Arc::new(BrowserReplayCoordinatorScope),
                repair_scope: Arc::new(BrowserReplayRepairAuthorityScope),
                next_instance_id: 0,
                next_repair_id: 0,
                next_preview_id: 0,
                next_apply_id: 0,
                active: HashMap::new(),
                terminal: VecDeque::new(),
                terminal_capacity: terminal_capacity.max(1),
            })),
        }
    }

    pub fn start(
        &self,
        workspace_key: BrowserWorkspaceKey,
        plan: BrowserReplayPlan,
    ) -> Result<BrowserReplayStart, BrowserReplayError> {
        let mut state = self.lock();
        Self::start_locked(&mut state, workspace_key, plan)
    }

    pub fn replace(
        &self,
        workspace_key: BrowserWorkspaceKey,
        plan: BrowserReplayPlan,
    ) -> Result<BrowserReplayStart, BrowserReplayError> {
        let mut state = self.lock();
        if let Some(instance) = state
            .active
            .get(&workspace_key)
            .map(|active| active.instance.clone())
        {
            Self::terminalize(&mut state, &instance, BrowserReplayStatus::Cancelled, None)?;
        }
        Self::start_locked(&mut state, workspace_key, plan)
    }

    pub fn status(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let state = self.lock();
        if let Some(active) = state.active.get(instance.workspace_key()) {
            if active.instance == *instance {
                return Ok(active.projection.clone());
            }
        }
        state
            .terminal
            .iter()
            .rev()
            .find(|terminal| terminal.instance == *instance)
            .map(|terminal| terminal.projection.clone())
            .ok_or(BrowserReplayError::StaleInstance)
    }

    pub fn begin(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::Pending {
                return Err(BrowserReplayError::InvalidTransition);
            }
            active.projection.status = BrowserReplayStatus::Running;
            Ok(active.projection.clone())
        })
    }

    pub fn submit_secrets(
        &self,
        instance: &BrowserReplayInstance,
        submission: BrowserReplaySecretSubmission,
    ) -> Result<BrowserReplayProjection, BrowserReplaySecretError> {
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, instance)
            .map_err(|_| BrowserReplaySecretError::StaleAuthority)?;
        if active.projection.status != BrowserReplayStatus::NeedsUserSecret {
            return Err(active.secret_store.submission_error());
        }

        active
            .secret_store
            .install(&active.projection.unresolved_secret_inputs, submission)?;
        active.projection.status = BrowserReplayStatus::Running;
        active.projection.unresolved_secret_inputs.clear();
        Ok(active.projection.clone())
    }

    pub fn advance_step(
        &self,
        instance: &BrowserReplayInstance,
        step_index: usize,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::Running || active.repair.is_some() {
                return Err(BrowserReplayError::InvalidTransition);
            }
            if step_index != active.projection.current_step_index
                || step_index >= active.projection.total_steps
            {
                return Err(BrowserReplayError::StepOutOfOrder);
            }
            active.projection.current_step_index += 1;
            active.projection.current_step_id = active
                .plan
                .steps
                .get(active.projection.current_step_index)
                .map(|step| step.id.clone());
            Ok(active.projection.clone())
        })
    }

    pub(crate) fn reserve_locator_repair_capture(
        &self,
        instance: &BrowserReplayInstance,
        resource_store: &BrowserResourceStore,
        step_index: usize,
        locator_slot: BrowserReplayLocatorSlot,
        tab_id: impl Into<String>,
        revision: BrowserRevision,
        resume_cursor: BrowserReplayRepairResumeCursor,
    ) -> Result<BrowserReplayRepairInstance, BrowserReplayError> {
        let tab_id = tab_id.into();
        if tab_id.trim().is_empty()
            || tab_id.len() > MAX_BROWSER_REPLAY_REPAIR_TAB_ID_BYTES
            || tab_id.chars().any(char::is_control)
        {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }

        let mut state = self.lock();
        let (step_id, old_locator) = {
            let active = Self::exact_active_mut(&mut state, instance)?;
            if active.projection.status != BrowserReplayStatus::Running
                || active.repair.is_some()
                || active.projection.current_step_index != step_index
            {
                return Err(BrowserReplayError::InvalidTransition);
            }
            let step = active
                .plan
                .steps
                .get(step_index)
                .ok_or(BrowserReplayError::InvalidRepairSlot)?;
            validate_repair_cursor(locator_slot, resume_cursor)?;
            let plan_locator = recipe_step_locator_at(step, locator_slot)
                .map_err(|_| BrowserReplayError::InvalidRepairSlot)?
                .ok_or(BrowserReplayError::InvalidRepairSlot)?
                .clone();
            let old_locator = active
                ._locator_overrides
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(step_index, locator_slot))
                .cloned()
                .unwrap_or(plan_locator);
            (step.id.clone(), old_locator)
        };
        let repair_id = state
            .next_repair_id
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(BrowserReplayError::RepairInstanceIdExhausted)?;
        let repair = BrowserReplayRepairInstance::new(
            instance.clone(),
            repair_id,
            Arc::clone(&state.repair_scope),
        );
        let authority = BrowserReplayRepairRetentionAuthority::for_repair(&repair);
        let lease = resource_store
            .begin_repair_retention(&authority)
            .map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
        let active = Self::exact_active_mut(&mut state, instance)?;
        active.repair = Some(BrowserReplayPrivateRepairState {
            instance: repair.clone(),
            resource_store: resource_store.clone(),
            lease: Arc::new(BrowserReplayRepairLeaseSlot::new(lease)),
            recipe_target: BrowserReplayRecipeLocatorTarget::new(
                step_index,
                step_id,
                locator_slot,
                old_locator,
            ),
            tab_id,
            revision,
            resume_cursor,
            snapshot: None,
            screenshot: None,
            snapshot_reserved: false,
            screenshot_reserved: false,
            phase: BrowserReplayPrivateRepairPhase::Capturing,
        });
        state.next_repair_id = repair_id.get();
        Ok(repair)
    }

    pub(crate) fn issue_locator_repair_capture_authority(
        &self,
        repair: &BrowserReplayRepairInstance,
        kind: BrowserResourceKind,
    ) -> Result<
        (
            BrowserReplayRepairCaptureAuthority,
            BrowserReplayRepairCaptureReceipt,
        ),
        BrowserReplayError,
    > {
        if repair_resource_mime(kind).is_none() {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if active.projection.status != BrowserReplayStatus::Running
            || repair_state.instance != *repair
            || !matches!(
                repair_state.phase,
                BrowserReplayPrivateRepairPhase::Capturing
            )
        {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let reserved = match kind {
            BrowserResourceKind::ReplayRepairSnapshot => &mut repair_state.snapshot_reserved,
            BrowserResourceKind::ReplayRepairScreenshot => &mut repair_state.screenshot_reserved,
            _ => unreachable!("dedicated repair kind was checked"),
        };
        let captured = match kind {
            BrowserResourceKind::ReplayRepairSnapshot => repair_state.snapshot.is_some(),
            BrowserResourceKind::ReplayRepairScreenshot => repair_state.screenshot.is_some(),
            _ => unreachable!("dedicated repair kind was checked"),
        };
        if *reserved
            || captured
            || (kind == BrowserResourceKind::ReplayRepairScreenshot
                && repair_state.snapshot.is_none())
        {
            return Err(BrowserReplayError::InvalidTransition);
        }
        *reserved = true;
        let receipt = Arc::new(Mutex::new(BrowserReplayRepairCaptureReceiptState {
            repair: repair.clone(),
            kind,
            handle: None,
            consumed: false,
        }));
        Ok((
            BrowserReplayRepairCaptureAuthority {
                repair: repair.clone(),
                resource_store: repair_state.resource_store.clone(),
                lease: Arc::clone(&repair_state.lease),
                receipt: Arc::clone(&receipt),
                kind,
                tab_id: repair_state.tab_id.clone(),
                revision: repair_state.revision,
            },
            BrowserReplayRepairCaptureReceipt {
                lease: Arc::clone(&repair_state.lease),
                state: receipt,
            },
        ))
    }

    pub(crate) fn record_locator_repair_evidence(
        &self,
        evidence: BrowserReplayRepairCapturedEvidence,
    ) -> Result<(), BrowserReplayError> {
        let BrowserReplayRepairCapturedEvidence {
            repair,
            kind,
            handle,
        } = evidence;
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if active.projection.status != BrowserReplayStatus::Running
            || repair_state.instance != repair
            || !matches!(
                repair_state.phase,
                BrowserReplayPrivateRepairPhase::Capturing
            )
            || handle.kind != kind
            || handle.mime_type != repair_resource_mime(kind).unwrap_or_default()
            || !handle.pinned
        {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let exact = repair_state
            .resource_store
            .handle(repair.workspace_key(), &handle.id)
            .map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
        if exact != handle {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        match kind {
            BrowserResourceKind::ReplayRepairSnapshot
                if repair_state.snapshot_reserved && repair_state.snapshot.is_none() =>
            {
                repair_state.snapshot = Some(handle);
                repair_state.snapshot_reserved = false;
            }
            BrowserResourceKind::ReplayRepairScreenshot
                if repair_state.screenshot_reserved && repair_state.screenshot.is_none() =>
            {
                repair_state.screenshot = Some(handle);
                repair_state.screenshot_reserved = false;
            }
            _ => return Err(BrowserReplayError::InvalidRepairEvidence),
        }
        Ok(())
    }

    pub(crate) fn abort_locator_repair_capture(&self, repair: &BrowserReplayRepairInstance) {
        let removed = {
            let mut state = self.lock();
            let Ok(active) = Self::exact_active_mut(&mut state, repair.replay()) else {
                return;
            };
            if active
                .repair
                .as_ref()
                .is_some_and(|candidate| candidate.instance == *repair)
                && active.projection.status == BrowserReplayStatus::Running
            {
                active.repair.take()
            } else {
                None
            }
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(super) fn retain_locator_repair_evidence_for_test(
        &self,
        repair: &BrowserReplayRepairInstance,
        kind: BrowserResourceKind,
        mime_type: impl Into<String>,
        bytes: impl AsRef<[u8]>,
    ) -> Result<BrowserResourceHandle, BrowserError> {
        let mut state = self.lock();
        let mime_type = mime_type.into();
        let expected_mime_type = match kind {
            BrowserResourceKind::ReplayRepairSnapshot => "application/json",
            BrowserResourceKind::ReplayRepairScreenshot => "image/png",
            _ => {
                return Err(BrowserError::InvalidInvocation {
                    field: "resourceKind".to_string(),
                });
            }
        };
        if mime_type != expected_mime_type {
            if let Ok(active) = Self::exact_active_mut(&mut state, repair.replay()) {
                if active
                    .repair
                    .as_ref()
                    .is_some_and(|candidate| candidate.instance == *repair)
                {
                    active.repair.take();
                }
            }
            return Err(BrowserError::InvalidInvocation {
                field: "mimeType".to_string(),
            });
        }
        let (resource_store, lease) = {
            let active = Self::exact_active_mut(&mut state, repair.replay()).map_err(|_| {
                BrowserError::BlockedPermission {
                    permission: "repair retention".to_string(),
                }
            })?;
            let repair_state =
                active
                    .repair
                    .as_ref()
                    .ok_or_else(|| BrowserError::BlockedPermission {
                        permission: "repair retention".to_string(),
                    })?;
            let kind_available = match kind {
                BrowserResourceKind::ReplayRepairSnapshot => repair_state.snapshot.is_none(),
                BrowserResourceKind::ReplayRepairScreenshot => repair_state.screenshot.is_none(),
                _ => false,
            };
            if active.projection.status != BrowserReplayStatus::Running
                || repair_state.instance != *repair
                || !matches!(
                    repair_state.phase,
                    BrowserReplayPrivateRepairPhase::Capturing
                )
                || !kind_available
            {
                return Err(BrowserError::BlockedPermission {
                    permission: "repair retention".to_string(),
                });
            }
            (
                repair_state.resource_store.clone(),
                Arc::clone(&repair_state.lease),
            )
        };

        let retained = lease
            .with_lease(|lease| resource_store.put_repair_retained(lease, kind, mime_type, bytes));
        let handle = match retained {
            Ok(handle) => handle,
            Err(error) => {
                if let Ok(active) = Self::exact_active_mut(&mut state, repair.replay()) {
                    if active
                        .repair
                        .as_ref()
                        .is_some_and(|candidate| candidate.instance == *repair)
                    {
                        active.repair.take();
                    }
                }
                return Err(error);
            }
        };
        let active = Self::exact_active_mut(&mut state, repair.replay()).map_err(|_| {
            BrowserError::BlockedPermission {
                permission: "repair retention".to_string(),
            }
        })?;
        let repair_state =
            active
                .repair
                .as_mut()
                .ok_or_else(|| BrowserError::BlockedPermission {
                    permission: "repair retention".to_string(),
                })?;
        match kind {
            BrowserResourceKind::ReplayRepairSnapshot => {
                repair_state.snapshot = Some(handle.clone());
            }
            BrowserResourceKind::ReplayRepairScreenshot => {
                repair_state.screenshot = Some(handle.clone());
            }
            _ => unreachable!("repair resource kind was validated before retention"),
        }
        Ok(handle)
    }

    pub(crate) fn reserve_locator_repair_preview(
        &self,
        repair: &BrowserReplayRepairInstance,
        candidate: BrowserReplayRepairCandidate,
    ) -> Result<
        (
            BrowserReplayRepairPreviewAuthority,
            BrowserReplayRepairPreviewReceipt,
        ),
        BrowserReplayError,
    > {
        let recipe_locator = candidate
            .validated_recipe_locator()
            .map_err(|_| BrowserReplayError::InvalidRepairEvidence)?;
        let wire = random_repair_preview_wire_token()?;
        let mut state = self.lock();
        let preview_id = state
            .next_preview_id
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(BrowserReplayError::RepairPreviewIdExhausted)?;
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if repair_state.instance != *repair {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let applied_locator = match &repair_state.phase {
            BrowserReplayPrivateRepairPhase::Paused(paused) => (paused.projection.phase
                == BrowserReplayRepairPhase::Applied)
                .then_some(paused.recipe_locator.as_ref())
                .flatten(),
            BrowserReplayPrivateRepairPhase::Previewing(reservation) => {
                (reservation.previous.projection.phase == BrowserReplayRepairPhase::Applied)
                    .then_some(reservation.previous.recipe_locator.as_ref())
                    .flatten()
            }
            _ => None,
        };
        if let Some(committed) = applied_locator {
            if committed != &recipe_locator {
                return Err(BrowserReplayError::InvalidRepairEvidence);
            }
        } else if candidate.element_ref().revision != repair_state.revision {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let phase = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        );
        let previous = match phase {
            BrowserReplayPrivateRepairPhase::Paused(paused) => paused,
            BrowserReplayPrivateRepairPhase::Previewing(reservation) => {
                reservation.authority.close();
                reservation.previous
            }
            BrowserReplayPrivateRepairPhase::Capturing => {
                repair_state.phase = BrowserReplayPrivateRepairPhase::Capturing;
                return Err(BrowserReplayError::InvalidTransition);
            }
            BrowserReplayPrivateRepairPhase::Preparing(reservation) => {
                repair_state.phase = BrowserReplayPrivateRepairPhase::Preparing(reservation);
                return Err(BrowserReplayError::InvalidTransition);
            }
            BrowserReplayPrivateRepairPhase::Committing(reservation) => {
                repair_state.phase = BrowserReplayPrivateRepairPhase::Committing(reservation);
                return Err(BrowserReplayError::InvalidTransition);
            }
        };
        let token = BrowserReplayRepairHighlightToken::new(
            repair.clone(),
            preview_id,
            repair_state.tab_id.clone(),
            wire,
        );
        let expected_previous_token = previous
            .highlight
            .as_ref()
            .map(|highlight| highlight.token.clone());
        let (authority, receipt) = BrowserReplayRepairPreviewAuthority::issue(
            repair.clone(),
            preview_id,
            repair_state.tab_id.clone(),
            candidate.element_ref().revision,
            candidate,
            recipe_locator,
            token,
            expected_previous_token,
        );
        repair_state.phase =
            BrowserReplayPrivateRepairPhase::Previewing(BrowserReplayPrivatePreviewReservation {
                previous,
                authority: authority.clone(),
            });
        signal_repair_state(active);
        state.next_preview_id = preview_id.get();
        Ok((authority, receipt))
    }

    pub(crate) fn abort_locator_repair_preview(
        &self,
        authority: &BrowserReplayRepairPreviewAuthority,
    ) -> BrowserReplayRepairPreviewAbortDisposition {
        let mut state = self.lock();
        let Ok(active) = Self::exact_active_mut(&mut state, authority.repair().replay()) else {
            authority.close();
            return BrowserReplayRepairPreviewAbortDisposition::ClearExactOnly;
        };
        let Some(repair_state) = active.repair.as_mut() else {
            authority.close();
            return BrowserReplayRepairPreviewAbortDisposition::ClearExactOnly;
        };
        let exact = repair_state.instance == *authority.repair()
            && matches!(
                &repair_state.phase,
                BrowserReplayPrivateRepairPhase::Previewing(reservation)
                    if reservation.authority.token() == authority.token()
                        && reservation.authority.preview_id() == authority.preview_id()
            );
        if !exact {
            authority.close();
            return BrowserReplayRepairPreviewAbortDisposition::ClearExactOnly;
        }
        let BrowserReplayPrivateRepairPhase::Previewing(reservation) = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        ) else {
            unreachable!("exact preview reservation was checked")
        };
        reservation.authority.close();
        repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
        signal_repair_state(active);
        BrowserReplayRepairPreviewAbortDisposition::RestorePrevious
    }

    pub(crate) fn commit_locator_repair_preview<F>(
        &self,
        acknowledgement: BrowserReplayRepairPreviewAcknowledgement,
        cleanup: F,
    ) -> Result<BrowserReplayRepairProjection, BrowserReplayError>
    where
        F: FnOnce() -> BrowserReplayRepairHighlightCleanup,
    {
        let BrowserReplayRepairPreviewAcknowledgement {
            repair,
            preview_id,
            candidate,
            recipe_locator,
            token,
            ..
        } = &acknowledgement;
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if repair_state.instance != *repair {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let exact = matches!(
            &repair_state.phase,
            BrowserReplayPrivateRepairPhase::Previewing(reservation)
                if reservation.authority.preview_id() == preview_id.get()
                    && reservation.authority.token() == token
                    && reservation.authority.same_lease(&acknowledgement)
        );
        if !exact {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let BrowserReplayPrivateRepairPhase::Previewing(mut reservation) = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        ) else {
            unreachable!("exact preview reservation was checked")
        };
        reservation.authority.close();
        if let Some(previous) = reservation.previous.highlight.as_mut() {
            previous.cleanup.disarm();
        }
        let was_applied =
            reservation.previous.projection.phase == BrowserReplayRepairPhase::Applied;
        if !was_applied {
            reservation.previous.projection.phase = BrowserReplayRepairPhase::Previewed;
        }
        reservation.previous.applied_preview_fresh = was_applied;
        reservation.previous.candidate = Some(candidate.clone());
        reservation.previous.recipe_locator = Some(recipe_locator.clone());
        reservation.previous.highlight = Some(BrowserReplayPrivateInstalledHighlight {
            token: token.clone(),
            cleanup: cleanup(),
        });
        let projection = reservation.previous.projection.clone();
        repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
        signal_repair_state(active);
        Ok(projection)
    }

    pub(crate) fn reserve_locator_repair_apply(
        &self,
        repair: &BrowserReplayRepairInstance,
        confirmed: bool,
        context: &BrowserInvocationContext,
    ) -> Result<
        (
            BrowserReplayRepairApplyAuthority,
            BrowserReplayRepairApplyReceipt,
        ),
        BrowserReplayError,
    > {
        if !confirmed {
            return Err(BrowserReplayError::RepairConfirmationRequired);
        }
        context
            .validate()
            .map_err(|_| BrowserReplayError::InvalidRepairEvidence)?;
        if !matches!(
            context.actor,
            BrowserInvocationActor::User | BrowserInvocationActor::Agent
        ) {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let effective_risk = if context.actor == BrowserInvocationActor::Agent {
            effective_browser_risk(context.declared_risk, None, Some(BrowserRisk::Destructive))
        } else {
            context.declared_risk
        };
        let mut state = self.lock();
        let apply_id = state
            .next_apply_id
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(BrowserReplayError::RepairApplyIdExhausted)?;
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if repair_state.instance != *repair {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let phase = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        );
        let paused = match phase {
            BrowserReplayPrivateRepairPhase::Paused(paused)
                if paused.projection.phase == BrowserReplayRepairPhase::Previewed
                    || (paused.projection.phase == BrowserReplayRepairPhase::Applied
                        && paused.applied_preview_fresh) =>
            {
                paused
            }
            other => {
                repair_state.phase = other;
                return Err(BrowserReplayError::InvalidTransition);
            }
        };
        let Some(candidate) = paused.candidate.clone() else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let Some(recipe_locator) = paused.recipe_locator.clone() else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let Some(token) = paused
            .highlight
            .as_ref()
            .map(|highlight| highlight.token.clone())
        else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let (authority, receipt) = BrowserReplayRepairApplyAuthority::issue(
            repair.clone(),
            apply_id,
            BrowserReplayRepairApplyStage::PreCommit,
            context.actor,
            context.operation_id.clone(),
            effective_risk,
            candidate.element_ref().revision,
            candidate,
            recipe_locator,
            token,
        );
        repair_state.phase =
            BrowserReplayPrivateRepairPhase::Preparing(BrowserReplayPrivateApplyReservation {
                previous: paused,
                authority: authority.clone(),
            });
        signal_repair_state(active);
        state.next_apply_id = apply_id.get();
        Ok((authority, receipt))
    }

    pub(crate) fn abort_locator_repair_apply(&self, authority: &BrowserReplayRepairApplyAuthority) {
        let mut state = self.lock();
        let Ok(active) = Self::exact_active_mut(&mut state, authority.repair().replay()) else {
            authority.close();
            return;
        };
        let Some(repair_state) = active.repair.as_mut() else {
            authority.close();
            return;
        };
        let exact = repair_state.instance == *authority.repair()
            && matches!(
                &repair_state.phase,
                BrowserReplayPrivateRepairPhase::Preparing(reservation)
                    if reservation.authority.apply_id() == authority.apply_id()
                        && reservation.authority.is_live()
            );
        if !exact {
            authority.close();
            return;
        }
        let BrowserReplayPrivateRepairPhase::Preparing(reservation) = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        ) else {
            unreachable!("exact repair apply reservation was checked")
        };
        reservation.authority.close();
        repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
        signal_repair_state(active);
    }

    pub(crate) fn commit_locator_repair_apply(
        &self,
        acknowledgement: BrowserReplayRepairApplyAcknowledgement,
    ) -> Result<BrowserReplayRepairApplyCommit, BrowserReplayError> {
        let mut state = self.lock();
        Self::commit_locator_repair_apply_locked(&mut state, acknowledgement)
    }

    fn commit_locator_repair_apply_locked(
        state: &mut BrowserReplayCoordinatorState,
        acknowledgement: BrowserReplayRepairApplyAcknowledgement,
    ) -> Result<BrowserReplayRepairApplyCommit, BrowserReplayError> {
        let repair = acknowledgement.repair.clone();
        let apply_id = acknowledgement.apply_id;
        let candidate = acknowledgement.candidate.clone();
        let recipe_locator = acknowledgement.recipe_locator.clone();
        let token = acknowledgement.token.clone();
        let (canonical_root, recipe_id, recipe_digest, recipe_target, already_applied) = {
            let active = Self::exact_active_mut(state, repair.replay())?;
            if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
                return Err(BrowserReplayError::InvalidTransition);
            }
            let canonical_root = active.canonical_recipe_root.get().cloned();
            let recipe_id = active.plan.recipe_id.clone();
            let recipe_digest = active.plan.recipe_digest.clone();
            let repair_state = active
                .repair
                .as_mut()
                .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
            let exact = repair_state.instance == repair
                && matches!(
                    &repair_state.phase,
                    BrowserReplayPrivateRepairPhase::Preparing(reservation)
                        if reservation.authority.apply_id() == apply_id.get()
                            && reservation.authority.stage()
                                == BrowserReplayRepairApplyStage::PreCommit
                            && acknowledgement.stage
                                == BrowserReplayRepairApplyStage::PreCommit
                            && reservation.authority.same_lease(&acknowledgement)
                            && reservation.authority.candidate() == &candidate
                            && reservation.authority.token() == &token
                            && reservation.authority.revision()
                                == candidate.element_ref().revision
                            && reservation.authority.is_live()
                );
            if !exact {
                return Err(BrowserReplayError::InvalidRepairEvidence);
            }
            let BrowserReplayPrivateRepairPhase::Preparing(reservation) = std::mem::replace(
                &mut repair_state.phase,
                BrowserReplayPrivateRepairPhase::Capturing,
            ) else {
                unreachable!("exact preparing apply reservation was checked")
            };
            let already_applied =
                reservation.previous.projection.phase == BrowserReplayRepairPhase::Applied;
            if !already_applied && canonical_root.is_none() {
                reservation.authority.close();
                repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
                signal_repair_state(active);
                return Err(BrowserReplayError::RecipeRootUnavailable);
            }
            repair_state.phase = BrowserReplayPrivateRepairPhase::Committing(reservation);
            (
                canonical_root,
                recipe_id,
                recipe_digest,
                repair_state.recipe_target.clone(),
                already_applied,
            )
        };

        let write_result = if already_applied {
            Ok(())
        } else {
            replace_recipe_locator_atomic(
                canonical_root
                    .as_deref()
                    .expect("non-applied commit validated its canonical recipe root"),
                &recipe_id,
                &recipe_digest,
                &recipe_target,
                &recipe_locator,
            )
            .map(|_| ())
        };

        let active = Self::exact_active_mut(state, repair.replay())?;
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        let BrowserReplayPrivateRepairPhase::Committing(mut reservation) = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        ) else {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        if reservation.authority.apply_id() != apply_id.get()
            || reservation.authority.stage() != BrowserReplayRepairApplyStage::PreCommit
            || acknowledgement.stage != BrowserReplayRepairApplyStage::PreCommit
            || !reservation.authority.same_lease(&acknowledgement)
        {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Committing(reservation);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        if let Err(error) = write_result {
            reservation.authority.close();
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
            signal_repair_state(active);
            return Err(map_repair_locator_replace_error(error));
        }

        if already_applied {
            let exact_override = active
                ._locator_overrides
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&(recipe_target.step_index(), recipe_target.locator_slot()))
                .is_some_and(|locator| locator == &recipe_locator);
            if !exact_override {
                reservation.authority.close();
                repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
                signal_repair_state(active);
                return Err(BrowserReplayError::InvalidRepairEvidence);
            }
            reservation.authority.close();
            reservation.previous.applied_preview_fresh = false;
            let repair_projection = reservation.previous.projection.clone();
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
            let released_repair = active
                .repair
                .take()
                .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
            active.projection.status = BrowserReplayStatus::Running;
            let replay_projection = active.projection.clone();
            signal_repair_state(active);
            return Ok(BrowserReplayRepairApplyCommit {
                repair: repair_projection,
                replay: replay_projection,
                recipe_written: false,
                _released_repair: Some(released_repair),
            });
        }

        active
            ._locator_overrides
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (recipe_target.step_index(), recipe_target.locator_slot()),
                recipe_locator,
            );
        reservation.authority.close();
        reservation.previous.projection.phase = BrowserReplayRepairPhase::Applied;
        reservation.previous.applied_preview_fresh = false;
        let repair_projection = reservation.previous.projection.clone();
        repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(reservation.previous);
        let replay_projection = active.projection.clone();
        signal_repair_state(active);
        Ok(BrowserReplayRepairApplyCommit {
            repair: repair_projection,
            replay: replay_projection,
            recipe_written: true,
            _released_repair: None,
        })
    }

    pub(crate) fn reserve_locator_repair_post_commit_validation(
        &self,
        repair: &BrowserReplayRepairInstance,
        context: &BrowserInvocationContext,
    ) -> Result<
        (
            BrowserReplayRepairApplyAuthority,
            BrowserReplayRepairApplyReceipt,
        ),
        BrowserReplayError,
    > {
        context
            .validate()
            .map_err(|_| BrowserReplayError::InvalidRepairEvidence)?;
        if !matches!(
            context.actor,
            BrowserInvocationActor::User | BrowserInvocationActor::Agent
        ) {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let mut state = self.lock();
        let apply_id = state
            .next_apply_id
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or(BrowserReplayError::RepairApplyIdExhausted)?;
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if repair_state.instance != *repair {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let phase = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        );
        let paused = match phase {
            BrowserReplayPrivateRepairPhase::Paused(paused)
                if paused.projection.phase == BrowserReplayRepairPhase::Applied
                    && !paused.applied_preview_fresh =>
            {
                paused
            }
            other => {
                repair_state.phase = other;
                return Err(BrowserReplayError::InvalidTransition);
            }
        };
        let Some(candidate) = paused.candidate.clone() else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let Some(recipe_locator) = paused.recipe_locator.clone() else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let Some(token) = paused
            .highlight
            .as_ref()
            .map(|highlight| highlight.token.clone())
        else {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(paused);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        };
        let (authority, receipt) = BrowserReplayRepairApplyAuthority::issue(
            repair.clone(),
            apply_id,
            BrowserReplayRepairApplyStage::PostCommit,
            context.actor,
            context.operation_id.clone(),
            BrowserRisk::Normal,
            candidate.element_ref().revision,
            candidate,
            recipe_locator,
            token,
        );
        repair_state.phase =
            BrowserReplayPrivateRepairPhase::Preparing(BrowserReplayPrivateApplyReservation {
                previous: paused,
                authority: authority.clone(),
            });
        signal_repair_state(active);
        state.next_apply_id = apply_id.get();
        Ok((authority, receipt))
    }

    pub(crate) fn complete_locator_repair_post_commit_validation(
        &self,
        acknowledgement: BrowserReplayRepairApplyAcknowledgement,
        commit: &mut BrowserReplayRepairApplyCommit,
        resume: bool,
    ) -> Result<(), BrowserReplayError> {
        if acknowledgement.stage != BrowserReplayRepairApplyStage::PostCommit
            || !commit.recipe_written
            || commit.repair.phase != BrowserReplayRepairPhase::Applied
        {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let repair = acknowledgement.repair.clone();
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let repair_state = active
            .repair
            .as_mut()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        let exact = repair_state.instance == repair
            && matches!(
                &repair_state.phase,
                BrowserReplayPrivateRepairPhase::Preparing(reservation)
                    if reservation.authority.apply_id() == acknowledgement.apply_id.get()
                        && reservation.authority.stage()
                            == BrowserReplayRepairApplyStage::PostCommit
                        && reservation.authority.same_lease(&acknowledgement)
                        && reservation.authority.candidate() == &acknowledgement.candidate
                        && reservation.authority.token() == &acknowledgement.token
                        && reservation.authority.is_live()
            );
        if !exact {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let BrowserReplayPrivateRepairPhase::Preparing(reservation) = std::mem::replace(
            &mut repair_state.phase,
            BrowserReplayPrivateRepairPhase::Capturing,
        ) else {
            unreachable!("exact post-commit validation reservation was checked")
        };
        if reservation.previous.projection != commit.repair
            || reservation.previous.projection.phase != BrowserReplayRepairPhase::Applied
            || reservation.previous.applied_preview_fresh
        {
            repair_state.phase = BrowserReplayPrivateRepairPhase::Preparing(reservation);
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        reservation.authority.close();
        let mut previous = reservation.previous;
        previous.applied_preview_fresh = false;
        repair_state.phase = BrowserReplayPrivateRepairPhase::Paused(previous);
        if !resume {
            commit.replay = active.projection.clone();
            signal_repair_state(active);
            return Ok(());
        }
        let released_repair = active
            .repair
            .take()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        active.projection.status = BrowserReplayStatus::Running;
        commit.replay = active.projection.clone();
        commit._released_repair = Some(released_repair);
        signal_repair_state(active);
        Ok(())
    }

    pub(crate) fn locator_repair_status(
        &self,
        repair: &BrowserReplayRepairInstance,
    ) -> Result<BrowserReplayRepairProjection, BrowserReplayError> {
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        let repair_state = active
            .repair
            .as_ref()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        if repair_state.instance != *repair {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        match &repair_state.phase {
            BrowserReplayPrivateRepairPhase::Capturing => {
                Err(BrowserReplayError::InvalidTransition)
            }
            BrowserReplayPrivateRepairPhase::Paused(paused) => Ok(paused.projection.clone()),
            BrowserReplayPrivateRepairPhase::Previewing(reservation) => {
                Ok(reservation.previous.projection.clone())
            }
            BrowserReplayPrivateRepairPhase::Preparing(reservation) => {
                Ok(reservation.previous.projection.clone())
            }
            BrowserReplayPrivateRepairPhase::Committing(reservation) => {
                Ok(reservation.previous.projection.clone())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn active_locator_repair_capture_for_test(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<
        (
            BrowserReplayRepairInstance,
            BrowserReplayLocatorSlot,
            BrowserReplayRepairResumeCursor,
        ),
        BrowserReplayError,
    > {
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, instance)?;
        let repair = active
            .repair
            .as_ref()
            .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
        Ok((
            repair.instance.clone(),
            repair.recipe_target.locator_slot(),
            repair.resume_cursor,
        ))
    }

    #[cfg(test)]
    pub(crate) fn resume_locator_repair_for_executor_test(
        &self,
        repair: &BrowserReplayRepairInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let (removed, projection) = {
            let mut state = self.lock();
            let active = Self::exact_active_mut(&mut state, repair.replay())?;
            if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
                return Err(BrowserReplayError::InvalidTransition);
            }
            let active_repair = active
                .repair
                .as_ref()
                .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
            if active_repair.instance != *repair
                || !matches!(
                    &active_repair.phase,
                    BrowserReplayPrivateRepairPhase::Paused(_)
                )
            {
                return Err(BrowserReplayError::InvalidRepairEvidence);
            }
            let removed = active
                .repair
                .take()
                .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
            active.projection.status = BrowserReplayStatus::Running;
            signal_repair_state(active);
            (removed, active.projection.clone())
        };
        drop(removed);
        Ok(projection)
    }

    pub(crate) fn publish_locator_repair(
        &self,
        repair: &BrowserReplayRepairInstance,
        snapshot: &BrowserResourceHandle,
        screenshot: &BrowserResourceHandle,
    ) -> Result<BrowserReplayRepairProjection, BrowserReplayError> {
        let mut state = self.lock();
        let active = Self::exact_active_mut(&mut state, repair.replay())?;
        if active.projection.status != BrowserReplayStatus::Running {
            return Err(BrowserReplayError::InvalidTransition);
        }
        let (resource_store, recipe_target, tab_id, revision) = {
            let repair_state = active
                .repair
                .as_ref()
                .ok_or(BrowserReplayError::InvalidRepairEvidence)?;
            if repair_state.instance != *repair
                || !matches!(
                    repair_state.phase,
                    BrowserReplayPrivateRepairPhase::Capturing
                )
                || repair_state.snapshot.as_ref() != Some(snapshot)
                || repair_state.screenshot.as_ref() != Some(screenshot)
                || snapshot.kind != BrowserResourceKind::ReplayRepairSnapshot
                || screenshot.kind != BrowserResourceKind::ReplayRepairScreenshot
            {
                return Err(BrowserReplayError::InvalidRepairEvidence);
            }
            (
                repair_state.resource_store.clone(),
                repair_state.recipe_target.clone(),
                repair_state.tab_id.clone(),
                repair_state.revision,
            )
        };
        let exact_snapshot = resource_store
            .handle(repair.workspace_key(), &snapshot.id)
            .map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
        let exact_screenshot = resource_store
            .handle(repair.workspace_key(), &screenshot.id)
            .map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
        if exact_snapshot != *snapshot || exact_screenshot != *screenshot {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let step = active
            .plan
            .steps
            .get(recipe_target.step_index())
            .ok_or(BrowserReplayError::InvalidRepairSlot)?;
        let plan_old_locator = recipe_step_locator_at(step, recipe_target.locator_slot())
            .map_err(|_| BrowserReplayError::InvalidRepairSlot)?
            .cloned();
        let exact_old_locator = active
            ._locator_overrides
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(recipe_target.step_index(), recipe_target.locator_slot()))
            .cloned()
            .or(plan_old_locator);
        if active.projection.current_step_index != recipe_target.step_index()
            || active.projection.current_step_id.as_deref() != Some(recipe_target.step_id())
            || step.id != recipe_target.step_id()
            || exact_old_locator.as_ref() != Some(recipe_target.old_locator())
        {
            return Err(BrowserReplayError::InvalidRepairEvidence);
        }
        let projection = BrowserReplayRepairProjection {
            workspace_key: repair.workspace_key().clone(),
            replay_instance_id: repair.replay_instance_id(),
            repair_id: repair.repair_id(),
            recipe_id: active.plan.recipe_id.clone(),
            step_id: recipe_target.step_id().to_string(),
            step_index: recipe_target.step_index(),
            locator_slot: recipe_target.locator_slot(),
            tab_id,
            revision,
            snapshot: snapshot.clone(),
            screenshot: screenshot.clone(),
            phase: BrowserReplayRepairPhase::AwaitingPreview,
        };
        active
            .repair
            .as_mut()
            .expect("exact capturing repair was checked under the coordinator lock")
            .phase = BrowserReplayPrivateRepairPhase::Paused(BrowserReplayPrivatePausedRepair {
            projection: projection.clone(),
            candidate: None,
            recipe_locator: None,
            highlight: None,
            applied_preview_fresh: false,
        });
        active.projection.status = BrowserReplayStatus::PausedLocatorRepair;
        signal_repair_state(active);
        Ok(projection)
    }

    pub fn complete(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let mut state = self.lock();
        {
            let active = Self::exact_active_mut(&mut state, instance)?;
            if active.projection.status != BrowserReplayStatus::Running || active.repair.is_some() {
                return Err(BrowserReplayError::InvalidTransition);
            }
            if active.projection.current_step_index != active.projection.total_steps {
                return Err(BrowserReplayError::IncompleteReplay);
            }
        }
        Self::terminalize(&mut state, instance, BrowserReplayStatus::Completed, None)
    }

    pub fn fail(
        &self,
        instance: &BrowserReplayInstance,
        failure: BrowserReplayFailureCode,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let mut state = self.lock();
        {
            let active = Self::exact_active_mut(&mut state, instance)?;
            if !matches!(
                active.projection.status,
                BrowserReplayStatus::Running | BrowserReplayStatus::PausedLocatorRepair
            ) {
                return Err(BrowserReplayError::InvalidTransition);
            }
        }
        Self::terminalize(
            &mut state,
            instance,
            BrowserReplayStatus::Failed,
            Some(failure),
        )
    }

    pub fn cancel(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let mut state = self.lock();
        {
            let active = Self::exact_active_mut(&mut state, instance)?;
            if !matches!(
                active.projection.status,
                BrowserReplayStatus::Pending
                    | BrowserReplayStatus::Running
                    | BrowserReplayStatus::NeedsUserSecret
                    | BrowserReplayStatus::PausedLocatorRepair
            ) {
                return Err(BrowserReplayError::InvalidTransition);
            }
        }
        Self::terminalize(&mut state, instance, BrowserReplayStatus::Cancelled, None)
    }

    pub fn interrupt_workspace(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserReplayProjection> {
        let mut state = self.lock();
        let instance = state
            .active
            .get(workspace_key)
            .map(|active| active.instance.clone())?;
        Self::terminalize(&mut state, &instance, BrowserReplayStatus::Cancelled, None).ok()
    }

    pub fn retained_terminal_count(&self) -> usize {
        self.lock().terminal.len()
    }

    fn lock(&self) -> MutexGuard<'_, BrowserReplayCoordinatorState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn start_locked(
        state: &mut BrowserReplayCoordinatorState,
        workspace_key: BrowserWorkspaceKey,
        plan: BrowserReplayPlan,
    ) -> Result<BrowserReplayStart, BrowserReplayError> {
        if state.active.contains_key(&workspace_key) {
            return Err(BrowserReplayError::AlreadyActive);
        }
        let instance_id = state
            .next_instance_id
            .checked_add(1)
            .ok_or(BrowserReplayError::InstanceIdExhausted)?;
        state.next_instance_id = instance_id;
        let instance = BrowserReplayInstance {
            workspace_key: workspace_key.clone(),
            id: instance_id,
            scope: state.scope.clone(),
        };
        let plan = Arc::new(plan);
        let lease = BrowserReplayCancellationLease {
            authority: Arc::new(BrowserReplayCancellationAuthority {
                id: instance_id,
                cancelled: AtomicBool::new(false),
            }),
        };
        let secret_store = BrowserReplaySecretStore::new(instance.clone());
        let (repair_signal, repair_watch) = watch::channel(0_u64);
        let locator_overrides = Arc::new(Mutex::new(HashMap::new()));
        let canonical_recipe_root = Arc::new(OnceLock::new());
        let execution = BrowserReplayExecutionHandle {
            instance: instance.clone(),
            plan: Arc::clone(&plan),
            lease: lease.clone(),
            secret_store: secret_store.share_authority(),
            repair_watch,
            locator_overrides: Arc::clone(&locator_overrides),
            canonical_recipe_root: Arc::clone(&canonical_recipe_root),
        };
        let projection = BrowserReplayProjection {
            workspace_key: workspace_key.clone(),
            instance_id,
            recipe_id: plan.recipe_id.clone(),
            status: if plan.unresolved_secret_inputs.is_empty() {
                BrowserReplayStatus::Pending
            } else {
                BrowserReplayStatus::NeedsUserSecret
            },
            current_step_index: 0,
            total_steps: plan.steps.len(),
            current_step_id: plan.steps.first().map(|step| step.id.clone()),
            unresolved_secret_inputs: plan.unresolved_secret_inputs.clone(),
            failure: None,
        };
        state.active.insert(
            workspace_key,
            ActiveBrowserReplay {
                instance: instance.clone(),
                plan,
                projection: projection.clone(),
                lease: lease.clone(),
                secret_store,
                repair: None,
                repair_signal,
                repair_generation: 0,
                _locator_overrides: locator_overrides,
                canonical_recipe_root,
            },
        );
        Ok(BrowserReplayStart {
            instance,
            projection,
            lease,
            execution,
        })
    }

    fn transition_nonterminal(
        &self,
        instance: &BrowserReplayInstance,
        transition: impl FnOnce(
            &mut ActiveBrowserReplay,
        ) -> Result<BrowserReplayProjection, BrowserReplayError>,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let mut state = self.lock();
        transition(Self::exact_active_mut(&mut state, instance)?)
    }

    fn exact_active_mut<'a>(
        state: &'a mut BrowserReplayCoordinatorState,
        instance: &BrowserReplayInstance,
    ) -> Result<&'a mut ActiveBrowserReplay, BrowserReplayError> {
        let is_exact_active = state
            .active
            .get(instance.workspace_key())
            .is_some_and(|active| active.instance == *instance);
        if is_exact_active {
            return state
                .active
                .get_mut(instance.workspace_key())
                .ok_or(BrowserReplayError::StaleInstance);
        }
        if state
            .terminal
            .iter()
            .any(|terminal| terminal.instance == *instance)
        {
            return Err(BrowserReplayError::TerminalState);
        }
        Err(BrowserReplayError::StaleInstance)
    }

    fn terminalize(
        state: &mut BrowserReplayCoordinatorState,
        instance: &BrowserReplayInstance,
        status: BrowserReplayStatus,
        failure: Option<BrowserReplayFailureCode>,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        {
            let active = Self::exact_active_mut(state, instance)?;
            if status == BrowserReplayStatus::Cancelled {
                active
                    .lease
                    .authority
                    .cancelled
                    .store(true, Ordering::Release);
            }
            active.projection.status = status;
            active.projection.failure = failure;
            active.secret_store.close();
            signal_repair_state(active);
        }
        let Some(active) = state.active.remove(instance.workspace_key()) else {
            return Err(BrowserReplayError::StaleInstance);
        };
        let projection = active.projection;
        state.terminal.push_back(TerminalBrowserReplay {
            instance: active.instance,
            projection: projection.clone(),
        });
        while state.terminal.len() > state.terminal_capacity {
            state.terminal.pop_front();
        }
        Ok(projection)
    }
}

fn random_repair_preview_wire_token() -> Result<String, BrowserReplayError> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        write!(&mut token, "{byte:02x}")
            .map_err(|_| BrowserReplayError::RepairEvidenceUnavailable)?;
    }
    Ok(token)
}

fn map_repair_locator_replace_error(error: BrowserRecipeLocatorReplaceError) -> BrowserReplayError {
    match error {
        BrowserRecipeLocatorReplaceError::InvalidCandidate
        | BrowserRecipeLocatorReplaceError::UnchangedCandidate => {
            BrowserReplayError::RepairCandidateInvalid
        }
        BrowserRecipeLocatorReplaceError::RecipeChanged
        | BrowserRecipeLocatorReplaceError::StepIndexChanged
        | BrowserRecipeLocatorReplaceError::StepIdChanged
        | BrowserRecipeLocatorReplaceError::LocatorSlotChanged
        | BrowserRecipeLocatorReplaceError::TargetlessLocator
        | BrowserRecipeLocatorReplaceError::OldLocatorChanged => {
            BrowserReplayError::RepairRecipeChanged
        }
        BrowserRecipeLocatorReplaceError::Store(_) => BrowserReplayError::RepairWriteFailed,
    }
}

fn validate_repair_cursor(
    locator_slot: BrowserReplayLocatorSlot,
    resume_cursor: BrowserReplayRepairResumeCursor,
) -> Result<(), BrowserReplayError> {
    let matches = match locator_slot {
        BrowserReplayLocatorSlot::PrimaryAction
        | BrowserReplayLocatorSlot::OptionalAction
        | BrowserReplayLocatorSlot::DragSource
        | BrowserReplayLocatorSlot::DragDestination => {
            resume_cursor == BrowserReplayRepairResumeCursor::Action
        }
        BrowserReplayLocatorSlot::ActionWait => {
            resume_cursor == BrowserReplayRepairResumeCursor::ActionWait
        }
        BrowserReplayLocatorSlot::StepWait => {
            resume_cursor == BrowserReplayRepairResumeCursor::StepWait
        }
        BrowserReplayLocatorSlot::Assertion { index } => {
            resume_cursor == BrowserReplayRepairResumeCursor::Assertion(index)
        }
    };
    matches
        .then_some(())
        .ok_or(BrowserReplayError::InvalidRepairSlot)
}

pub fn compile_browser_replay(
    recipe: &BrowserRecipeV1,
    public_inputs: Vec<BrowserReplayPublicInput>,
) -> Result<BrowserReplayPlan, BrowserReplayError> {
    recipe
        .validate()
        .map_err(|_| BrowserReplayError::InvalidRecipe)?;
    let recipe_digest =
        canonical_browser_recipe_digest(recipe).map_err(|_| BrowserReplayError::InvalidRecipe)?;
    if recipe.inputs.len() > MAX_BROWSER_REPLAY_INPUTS
        || public_inputs.len() > MAX_BROWSER_REPLAY_INPUTS
        || recipe.steps.len() > MAX_BROWSER_REPLAY_STEPS
        || recipe
            .inputs
            .iter()
            .filter(|input| input.kind == BrowserRecipeInputKind::Secret)
            .count()
            > MAX_BROWSER_REPLAY_SECRET_INPUTS
    {
        return Err(BrowserReplayError::CapacityExceeded);
    }
    validate_tab_alias_lifecycle(&recipe.steps)?;
    if recipe.inputs.iter().any(|input| {
        input.name.len() > MAX_BROWSER_REPLAY_INPUT_NAME_BYTES
            || input.name.chars().any(char::is_control)
            || super::automation::browser_text_contains_secret(&input.name)
    }) {
        return Err(BrowserReplayError::InvalidRecipe);
    }

    let declared = recipe
        .inputs
        .iter()
        .map(|input| (input.name.as_str(), input))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut bindings = HashMap::new();
    for supplied in public_inputs {
        if supplied.kind == BrowserRecipeInputKind::Secret {
            return Err(BrowserReplayError::PublicSecretRejected);
        }
        if supplied.name.is_empty()
            || supplied.name.len() > MAX_BROWSER_REPLAY_INPUT_NAME_BYTES
            || supplied.name.chars().any(char::is_control)
            || super::automation::browser_text_contains_secret(&supplied.name)
        {
            return Err(BrowserReplayError::InvalidPublicInputName);
        }
        if !seen.insert(supplied.name.clone()) {
            return Err(BrowserReplayError::DuplicatePublicInput);
        }
        let Some(input) = declared.get(supplied.name.as_str()) else {
            return Err(BrowserReplayError::UnknownPublicInput);
        };
        if input.kind == BrowserRecipeInputKind::Secret {
            return Err(BrowserReplayError::PublicSecretRejected);
        }
        if input.kind != supplied.kind {
            return Err(BrowserReplayError::InputKindMismatch);
        }
        validate_public_value(supplied.kind, &supplied.value)?;
        bindings.insert(
            supplied.name,
            BrowserReplayBoundValue {
                name: input.name.clone(),
                kind: supplied.kind,
                value: supplied.value,
            },
        );
    }

    let mut unresolved_secret_inputs = Vec::new();
    for input in &recipe.inputs {
        if input.kind == BrowserRecipeInputKind::Secret {
            unresolved_secret_inputs.push(input.name.clone());
            continue;
        }
        if bindings.contains_key(&input.name) {
            continue;
        }
        let Some(default_value) = input.default_value.clone() else {
            return Err(BrowserReplayError::MissingPublicInput);
        };
        validate_public_value(input.kind, &default_value)?;
        bindings.insert(
            input.name.clone(),
            BrowserReplayBoundValue {
                name: input.name.clone(),
                kind: input.kind,
                value: default_value,
            },
        );
    }

    let ordered_bindings = recipe
        .inputs
        .iter()
        .filter(|input| input.kind != BrowserRecipeInputKind::Secret)
        .map(|input| {
            let binding = bindings
                .remove(&input.name)
                .ok_or(BrowserReplayError::MissingPublicInput)?;
            Ok(BrowserReplayBoundValue {
                name: input.name.clone(),
                kind: binding.kind,
                value: binding.value,
            })
        })
        .collect::<Result<Vec<_>, BrowserReplayError>>()?;

    Ok(BrowserReplayPlan {
        recipe_id: recipe.id.clone(),
        recipe_digest,
        start_url: recipe.start_url.clone(),
        viewport: recipe.viewport,
        steps: recipe.steps.clone(),
        bindings: ordered_bindings,
        unresolved_secret_inputs,
    })
}

fn validate_tab_alias_lifecycle(steps: &[BrowserRecipeStep]) -> Result<(), BrowserReplayError> {
    let legacy_creates_tab_one = steps.iter().any(|step| {
        matches!(
            &step.action,
            BrowserRecipeAction::CreateTab { tab, .. } if tab == "tab-1"
        )
    });
    let mut active = HashSet::new();
    let mut seen = HashSet::new();
    if !legacy_creates_tab_one {
        active.insert("tab-1".to_string());
        seen.insert("tab-1".to_string());
    }

    for step in steps {
        match &step.action {
            BrowserRecipeAction::CreateTab { tab, .. } => {
                if !seen.insert(tab.clone()) {
                    return Err(BrowserReplayError::InvalidRecipe);
                }
                active.insert(tab.clone());
            }
            BrowserRecipeAction::SelectTab { tab } => {
                if !active.contains(tab) {
                    return Err(BrowserReplayError::InvalidRecipe);
                }
            }
            BrowserRecipeAction::CloseTab { tab } => {
                if !active.remove(tab) {
                    return Err(BrowserReplayError::InvalidRecipe);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_public_value(
    kind: BrowserRecipeInputKind,
    value: &str,
) -> Result<(), BrowserReplayError> {
    match kind {
        BrowserRecipeInputKind::Text => {
            if value.len() > MAX_BROWSER_REPLAY_TEXT_BYTES
                || value.contains('\0')
                || super::automation::browser_text_contains_secret(value)
            {
                return Err(BrowserReplayError::InvalidTextInput);
            }
        }
        BrowserRecipeInputKind::Url => {
            if value.len() > MAX_BROWSER_REPLAY_URL_BYTES
                || super::recipes::validate_safe_url(value, "replay URL input").is_err()
            {
                return Err(BrowserReplayError::InvalidUrlInput);
            }
        }
        BrowserRecipeInputKind::File => {
            if value.len() > MAX_BROWSER_REPLAY_FILE_BYTES
                || value.trim().is_empty()
                || value.chars().any(char::is_control)
            {
                return Err(BrowserReplayError::InvalidFileInput);
            }
        }
        BrowserRecipeInputKind::Secret => {
            return Err(BrowserReplayError::PublicSecretRejected);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{
        BrowserElementRef, BrowserError, BrowserInvocationActor, BrowserInvocationContext,
        BrowserLocator, BrowserRecipeLocator, BrowserReplayLocatorSlot,
        BrowserReplayRepairCandidate, BrowserReplayRepairPhase, BrowserReplayRepairResumeCursor,
        BrowserResourceKind, BrowserResourceLimits, BrowserResourceStore, BrowserRevision,
        BrowserRisk, MAX_BROWSER_REPLAY_SECRET_INPUTS, MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES,
        MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES,
    };
    use std::sync::atomic::AtomicUsize;

    const SECRET_SENTINEL: &str = "value-sentinel-secret-store";

    fn internal_plan(unresolved_secret_inputs: Vec<String>) -> BrowserReplayPlan {
        BrowserReplayPlan {
            recipe_id: "internal-recipe".to_string(),
            recipe_digest: BrowserRecipeDigestV1::placeholder_for_test(),
            start_url: "https://example.test".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            steps: Vec::new(),
            bindings: Vec::new(),
            unresolved_secret_inputs,
        }
    }

    fn click_repair_plan(locator: BrowserRecipeLocator) -> BrowserReplayPlan {
        let mut plan = internal_plan(Vec::new());
        plan.steps.push(BrowserRecipeStep {
            id: "click-submit".to_string(),
            action: BrowserRecipeAction::Click { locator },
            wait: None,
            assertions: Vec::new(),
        });
        plan
    }

    #[test]
    fn canonical_recipe_root_binding_is_exact_once_and_shared_with_active_replay() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "devmanager-replay-root-binding-{}-{nonce:x}",
            std::process::id()
        ));
        let other = std::env::temp_dir().join(format!(
            "devmanager-replay-root-binding-other-{}-{nonce:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let root = root.canonicalize().unwrap();
        let other = other.canonicalize().unwrap();

        let coordinator = BrowserReplayCoordinator::default();
        let owner = BrowserWorkspaceKey::new("root-binding", "tab-1").unwrap();
        let started = coordinator
            .start(
                owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        {
            let state = coordinator.lock();
            let active = state.active.get(&owner).unwrap();
            assert!(Arc::ptr_eq(
                &active.canonical_recipe_root,
                &started.execution.canonical_recipe_root
            ));
            assert!(active.canonical_recipe_root.get().is_none());
        }

        let unavailable = root.join("missing-root");
        assert_eq!(
            started.execution.bind_canonical_recipe_root(&unavailable),
            Err(BrowserReplayError::RecipeRootUnavailable)
        );
        assert!(started.execution.canonical_recipe_root.get().is_none());
        started.execution.bind_canonical_recipe_root(&root).unwrap();
        {
            let state = coordinator.lock();
            let active = state.active.get(&owner).unwrap();
            assert_eq!(active.canonical_recipe_root.get(), Some(&root));
        }
        assert_eq!(
            started.execution.bind_canonical_recipe_root(&root),
            Err(BrowserReplayError::RecipeRootAlreadyBound)
        );
        assert_eq!(
            started.execution.bind_canonical_recipe_root(&other),
            Err(BrowserReplayError::RecipeRootAlreadyBound)
        );
        assert_eq!(
            started.execution.bound_canonical_recipe_root().unwrap(),
            root.as_path()
        );

        coordinator.cancel(&started.instance).unwrap();
        assert!(!coordinator.lock().active.contains_key(&owner));
        drop(started);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn compiled_plan_retains_the_validated_canonical_recipe_digest_privately() {
        let recipe = BrowserRecipeV1 {
            schema_version: crate::browser::BROWSER_RECIPE_SCHEMA_VERSION,
            id: "digest-plan".to_string(),
            name: "Digest plan".to_string(),
            description: "Validated canonical digest".to_string(),
            start_url: "https://example.test/".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: BrowserRecipeLocator {
                        test_id: Some("submit".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let expected = canonical_browser_recipe_digest(&recipe).unwrap();
        let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                BrowserWorkspaceKey::new("digest-plan", "tab-1").unwrap(),
                plan,
            )
            .unwrap();
        assert!(started.execution.recipe_digest() == &expected);
    }

    #[cfg(target_os = "windows")]
    fn repair_store(
        label: &str,
        max_resource_bytes: u64,
    ) -> (std::path::PathBuf, BrowserResourceStore) {
        let root = std::env::temp_dir().join(format!(
            "devmanager-replay-repair-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes,
            },
        )
        .unwrap();
        (root, store)
    }

    #[cfg(target_os = "windows")]
    fn paused_repair(
        coordinator: &BrowserReplayCoordinator,
        store: &BrowserResourceStore,
        owner: BrowserWorkspaceKey,
        revision: BrowserRevision,
    ) -> (BrowserReplayInstance, BrowserReplayRepairInstance) {
        let started = coordinator
            .start(
                owner,
                click_repair_plan(BrowserRecipeLocator {
                    test_id: Some("old-submit".to_string()),
                    ..BrowserRecipeLocator::default()
                }),
            )
            .unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                revision,
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();
        (started.instance, repair)
    }

    fn repair_candidate(revision: BrowserRevision, test_id: &str) -> BrowserReplayRepairCandidate {
        BrowserReplayRepairCandidate::new(BrowserElementRef {
            revision,
            locator: BrowserLocator {
                test_id: Some(test_id.to_string()),
                ..BrowserLocator::default()
            },
            backend_node_id: Some(71),
        })
    }

    #[cfg(target_os = "windows")]
    fn saved_repair_recipe(recipe_id: &str) -> BrowserRecipeV1 {
        BrowserRecipeV1 {
            schema_version: crate::browser::BROWSER_RECIPE_SCHEMA_VERSION,
            id: recipe_id.to_string(),
            name: "Saved locator repair".to_string(),
            description: "Exact atomic apply fixture".to_string(),
            start_url: "https://example.test/".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            inputs: Vec::new(),
            steps: vec![
                BrowserRecipeStep {
                    id: "click-submit".to_string(),
                    action: BrowserRecipeAction::Click {
                        locator: BrowserRecipeLocator {
                            test_id: Some("old-submit".to_string()),
                            ..BrowserRecipeLocator::default()
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "click-neighbor".to_string(),
                    action: BrowserRecipeAction::Click {
                        locator: BrowserRecipeLocator {
                            test_id: Some("neighbor".to_string()),
                            ..BrowserRecipeLocator::default()
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
            ],
        }
    }

    #[cfg(target_os = "windows")]
    fn saved_paused_repair(
        label: &str,
        revision: BrowserRevision,
    ) -> (
        PathBuf,
        PathBuf,
        BrowserResourceStore,
        BrowserReplayCoordinator,
        BrowserReplayStart,
        BrowserReplayRepairInstance,
    ) {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let project_root = std::env::temp_dir().join(format!(
            "devmanager-saved-repair-{label}-{}-{nonce:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        let recipe = saved_repair_recipe(&format!("repair-{label}"));
        crate::browser::save_recipe(&project_root, &recipe).unwrap();
        let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
        let (resource_root, store) = repair_store(label, 1024 * 1024);
        assert_ne!(resource_root, project_root);
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                BrowserWorkspaceKey::new("saved-repair", label).unwrap(),
                plan,
            )
            .unwrap();
        started
            .execution
            .bind_canonical_recipe_root(&project_root)
            .unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                revision,
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();
        let (preview, receipt) = coordinator
            .reserve_locator_repair_preview(&repair, repair_candidate(revision, "replacement"))
            .unwrap();
        assert!(preview.acknowledge_for_test());
        coordinator
            .commit_locator_repair_preview(receipt.consume_exact(&repair).unwrap(), || {
                BrowserReplayRepairHighlightCleanup::new(|| {})
            })
            .unwrap();
        (
            project_root,
            resource_root,
            store,
            coordinator,
            started,
            repair,
        )
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_apply_requires_exact_preview_confirmation_and_destructive_agent_authorization() {
        let (root, store) = repair_store("apply-confirmation", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::default();
        let owner = BrowserWorkspaceKey::new("repair-apply", "confirmation").unwrap();
        let (instance, repair) = paused_repair(&coordinator, &store, owner, BrowserRevision(41));
        let cleanup = Arc::new(AtomicUsize::new(0));
        let (preview_authority, preview_receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(41), "replacement"),
            )
            .unwrap();
        assert!(preview_authority.acknowledge_for_test());
        coordinator
            .commit_locator_repair_preview(preview_receipt.consume_exact(&repair).unwrap(), || {
                BrowserReplayRepairHighlightCleanup::for_test(Arc::clone(&cleanup))
            })
            .unwrap();

        let agent = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save the reviewed locator repair",
            BrowserRisk::Normal,
            "repair-apply-confirmation",
        )
        .unwrap();
        assert!(matches!(
            coordinator.reserve_locator_repair_apply(&repair, false, &agent),
            Err(BrowserReplayError::RepairConfirmationRequired)
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Previewed
        );

        let (authority, _receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &agent)
            .unwrap();
        assert_eq!(
            authority.effective_risk_for_test(),
            BrowserRisk::Destructive
        );
        coordinator.abort_locator_repair_apply(&authority);

        let higher_risk = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save the reviewed account locator repair",
            BrowserRisk::AccountSecurity,
            "repair-apply-higher-risk",
        )
        .unwrap();
        let (authority, _receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &higher_risk)
            .unwrap();
        assert_eq!(
            authority.effective_risk_for_test(),
            BrowserRisk::AccountSecurity
        );
        coordinator.abort_locator_repair_apply(&authority);

        coordinator.cancel(&instance).unwrap();
        assert_eq!(cleanup.load(Ordering::Acquire), 1);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_apply_commits_exact_file_override_and_applied_state_only_after_acknowledgement() {
        let revision = BrowserRevision(51);
        let (project_root, resource_root, store, coordinator, started, repair) =
            saved_paused_repair("atomic-commit", revision);
        let recipe_path =
            crate::browser::recipe_path(&project_root, started.execution.plan().recipe_id())
                .unwrap();
        let before = std::fs::read(&recipe_path).unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save the exact reviewed locator repair",
            BrowserRisk::Normal,
            "repair-atomic-commit",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert_eq!(std::fs::read(&recipe_path).unwrap(), before);
        assert_eq!(
            started
                .execution
                .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction),
            None
        );

        assert!(authority.acknowledge_for_test());
        let acknowledgement = receipt.consume_exact(&repair).unwrap();
        let committed = coordinator
            .commit_locator_repair_apply(acknowledgement)
            .unwrap();
        assert!(committed.recipe_written);
        assert_eq!(committed.repair.phase, BrowserReplayRepairPhase::Applied);
        assert_eq!(
            committed.replay.status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        let replacement = BrowserRecipeLocator {
            test_id: Some("replacement".to_string()),
            ..BrowserRecipeLocator::default()
        };
        assert_eq!(
            started
                .execution
                .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction),
            Some(replacement.clone())
        );
        assert_eq!(
            started
                .execution
                .locator_override(1, BrowserReplayLocatorSlot::PrimaryAction),
            None
        );
        let saved =
            crate::browser::load_recipe(&project_root, started.execution.plan().recipe_id())
                .unwrap();
        assert_eq!(
            recipe_step_locator_at(&saved.steps[0], BrowserReplayLocatorSlot::PrimaryAction)
                .unwrap(),
            Some(&replacement)
        );
        assert_eq!(
            recipe_step_locator_at(&saved.steps[1], BrowserReplayLocatorSlot::PrimaryAction)
                .unwrap()
                .and_then(|locator| locator.test_id.as_deref()),
            Some("neighbor")
        );

        coordinator.cancel(&started.instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(project_root).unwrap();
        std::fs::remove_dir_all(resource_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_apply_unbound_root_restores_preview_and_closes_the_failed_reservation() {
        let (root, store) = repair_store("apply-unbound-root", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::default();
        let (instance, repair) = paused_repair(
            &coordinator,
            &store,
            BrowserWorkspaceKey::new("repair-apply", "unbound-root").unwrap(),
            BrowserRevision(61),
        );
        let (preview, receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(61), "replacement"),
            )
            .unwrap();
        assert!(preview.acknowledge_for_test());
        coordinator
            .commit_locator_repair_preview(receipt.consume_exact(&repair).unwrap(), || {
                BrowserReplayRepairHighlightCleanup::new(|| {})
            })
            .unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save reviewed locator repair",
            BrowserRisk::Normal,
            "repair-unbound-root",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        assert!(matches!(
            coordinator.commit_locator_repair_apply(receipt.consume_exact(&repair).unwrap()),
            Err(BrowserReplayError::RecipeRootUnavailable)
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Previewed
        );
        let (replacement, _receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .expect("failed pre-write reservation must restore an exact retryable preview");
        coordinator.abort_locator_repair_apply(&replacement);

        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_apply_cancellation_replacement_interrupt_and_denial_never_write() {
        enum TerminalPath {
            Cancel,
            Replace,
            Interrupt,
        }

        for (label, path) in [
            ("cancel-before-commit", TerminalPath::Cancel),
            ("replace-before-commit", TerminalPath::Replace),
            ("interrupt-before-commit", TerminalPath::Interrupt),
        ] {
            let (project_root, resource_root, store, coordinator, started, repair) =
                saved_paused_repair(label, BrowserRevision(71));
            let recipe_path =
                crate::browser::recipe_path(&project_root, started.execution.plan().recipe_id())
                    .unwrap();
            let before = std::fs::read(&recipe_path).unwrap();
            let context = BrowserInvocationContext::new(
                BrowserInvocationActor::Agent,
                "save reviewed locator repair",
                BrowserRisk::Normal,
                format!("repair-{label}"),
            )
            .unwrap();
            let (authority, receipt) = coordinator
                .reserve_locator_repair_apply(&repair, true, &context)
                .unwrap();
            assert!(authority.acknowledge_for_test());
            let acknowledgement = receipt.consume_exact(&repair).unwrap();
            match path {
                TerminalPath::Cancel => {
                    coordinator.cancel(&started.instance).unwrap();
                }
                TerminalPath::Replace => {
                    coordinator
                        .replace(
                            started.instance.workspace_key().clone(),
                            click_repair_plan(BrowserRecipeLocator::default()),
                        )
                        .unwrap();
                }
                TerminalPath::Interrupt => {
                    coordinator
                        .interrupt_workspace(started.instance.workspace_key())
                        .unwrap();
                }
            }
            assert!(matches!(
                coordinator.commit_locator_repair_apply(acknowledgement),
                Err(BrowserReplayError::TerminalState)
            ));
            assert_eq!(std::fs::read(&recipe_path).unwrap(), before);
            assert_eq!(
                started
                    .execution
                    .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction),
                None
            );
            drop(store);
            std::fs::remove_dir_all(project_root).unwrap();
            std::fs::remove_dir_all(resource_root).unwrap();
        }

        let (project_root, resource_root, store, coordinator, started, repair) =
            saved_paused_repair("denied-before-commit", BrowserRevision(72));
        let recipe_path =
            crate::browser::recipe_path(&project_root, started.execution.plan().recipe_id())
                .unwrap();
        let before = std::fs::read(&recipe_path).unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save reviewed locator repair",
            BrowserRisk::Normal,
            "repair-denied-before-commit",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        coordinator.abort_locator_repair_apply(&authority);
        assert!(!authority.acknowledge_for_test());
        assert!(receipt.consume_exact(&repair).is_none());
        assert_eq!(std::fs::read(&recipe_path).unwrap(), before);
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Previewed
        );
        coordinator.cancel(&started.instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(project_root).unwrap();
        std::fs::remove_dir_all(resource_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_apply_coordinator_gate_deterministically_linearizes_both_lock_orderings() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (project_root, resource_root, store, coordinator, started, repair) =
            saved_paused_repair("gate-terminal-first", BrowserRevision(81));
        let recipe_path =
            crate::browser::recipe_path(&project_root, started.execution.plan().recipe_id())
                .unwrap();
        let before = std::fs::read(&recipe_path).unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save reviewed locator repair",
            BrowserRisk::Normal,
            "repair-gate-terminal-first",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        let acknowledgement = receipt.consume_exact(&repair).unwrap();
        let mut state = coordinator.lock();
        let (started_tx, started_rx) = mpsc::channel();
        let commit_coordinator = coordinator.clone();
        let commit = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            commit_coordinator.commit_locator_repair_apply(acknowledgement)
        });
        started_rx.recv().unwrap();
        BrowserReplayCoordinator::terminalize(
            &mut state,
            &started.instance,
            BrowserReplayStatus::Cancelled,
            None,
        )
        .unwrap();
        drop(state);
        assert!(matches!(
            commit.join().unwrap(),
            Err(BrowserReplayError::TerminalState)
        ));
        assert_eq!(std::fs::read(&recipe_path).unwrap(), before);
        drop(store);
        std::fs::remove_dir_all(project_root).unwrap();
        std::fs::remove_dir_all(resource_root).unwrap();

        let (project_root, resource_root, store, coordinator, started, repair) =
            saved_paused_repair("gate-commit-first", BrowserRevision(82));
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save reviewed locator repair",
            BrowserRisk::Normal,
            "repair-gate-commit-first",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        let acknowledgement = receipt.consume_exact(&repair).unwrap();
        let mut state = coordinator.lock();
        let cancel_coordinator = coordinator.clone();
        let cancel_instance = started.instance.clone();
        let (cancel_started_tx, cancel_started_rx) = mpsc::channel();
        let (cancel_done_tx, cancel_done_rx) = mpsc::channel();
        let cancel = std::thread::spawn(move || {
            cancel_started_tx.send(()).unwrap();
            let result = cancel_coordinator.cancel(&cancel_instance);
            cancel_done_tx.send(result).unwrap();
        });
        cancel_started_rx.recv().unwrap();
        assert!(matches!(
            cancel_done_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        let committed = BrowserReplayCoordinator::commit_locator_repair_apply_locked(
            &mut state,
            acknowledgement,
        )
        .unwrap();
        assert_eq!(committed.repair.phase, BrowserReplayRepairPhase::Applied);
        assert!(committed.recipe_written);
        assert!(cancel_done_rx.try_recv().is_err());
        assert_eq!(
            started
                .execution
                .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction)
                .and_then(|locator| locator.test_id),
            Some("replacement".to_string())
        );
        drop(state);
        assert_eq!(
            cancel_done_rx.recv().unwrap().unwrap().status,
            BrowserReplayStatus::Cancelled
        );
        cancel.join().unwrap();
        drop(store);
        std::fs::remove_dir_all(project_root).unwrap();
        std::fs::remove_dir_all(resource_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn applied_repair_requires_fresh_exact_preview_and_resumes_without_rewriting() {
        let (project_root, resource_root, store, coordinator, started, repair) =
            saved_paused_repair("fresh-no-write-resume", BrowserRevision(91));
        let recipe_path =
            crate::browser::recipe_path(&project_root, started.execution.plan().recipe_id())
                .unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "save reviewed locator repair",
            BrowserRisk::Normal,
            "repair-initial-write",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        let committed = coordinator
            .commit_locator_repair_apply(receipt.consume_exact(&repair).unwrap())
            .unwrap();
        assert!(committed.recipe_written);
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Applied
        );
        assert!(matches!(
            coordinator.reserve_locator_repair_apply(&repair, true, &context),
            Err(BrowserReplayError::InvalidTransition)
        ));

        assert!(matches!(
            coordinator.reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(92), "different")
            ),
            Err(BrowserReplayError::InvalidRepairEvidence)
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Applied
        );

        let (preview, receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(92), "replacement"),
            )
            .expect("a fresh exact preview may revalidate the committed locator");
        assert!(preview.acknowledge_for_test());
        coordinator
            .commit_locator_repair_preview(receipt.consume_exact(&repair).unwrap(), || {
                BrowserReplayRepairHighlightCleanup::new(|| {})
            })
            .unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Applied
        );

        let mut permissions = std::fs::metadata(&recipe_path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&recipe_path, permissions).unwrap();
        let context = BrowserInvocationContext::new(
            BrowserInvocationActor::Agent,
            "resume the exact committed locator repair",
            BrowserRisk::Normal,
            "repair-no-write-resume",
        )
        .unwrap();
        let (authority, receipt) = coordinator
            .reserve_locator_repair_apply(&repair, true, &context)
            .unwrap();
        assert!(authority.acknowledge_for_test());
        let resumed = coordinator
            .commit_locator_repair_apply(receipt.consume_exact(&repair).unwrap())
            .unwrap();
        assert!(!resumed.recipe_written);
        assert_eq!(resumed.repair.phase, BrowserReplayRepairPhase::Applied);
        assert_eq!(resumed.replay.status, BrowserReplayStatus::Running);
        assert!(matches!(
            coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::InvalidRepairEvidence)
        ));
        drop(resumed);

        coordinator.cancel(&started.instance).unwrap();
        drop(store);
        let mut permissions = std::fs::metadata(&recipe_path).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&recipe_path, permissions).unwrap();
        std::fs::remove_dir_all(project_root).unwrap();
        std::fs::remove_dir_all(resource_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn preview_reservation_requires_exact_repair_revision_and_valid_semantic_locator() {
        let (root, store) = repair_store("preview-authority", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::default();
        let owner = BrowserWorkspaceKey::new("repair-preview", "authority").unwrap();
        let (instance, repair) =
            paused_repair(&coordinator, &store, owner.clone(), BrowserRevision(17));

        assert!(matches!(
            coordinator.reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(16), "candidate")
            ),
            Err(BrowserReplayError::InvalidRepairEvidence)
        ));
        for locator in [
            BrowserLocator::default(),
            BrowserLocator {
                accessibility_role: Some("button".to_string()),
                ..BrowserLocator::default()
            },
            BrowserLocator {
                test_id: Some(" candidate ".to_string()),
                ..BrowserLocator::default()
            },
            BrowserLocator {
                css_selectors: vec!["button".to_string(), "button".to_string()],
                ..BrowserLocator::default()
            },
            BrowserLocator {
                test_id: Some("sk-proj-abcdefghijklmnopqrstuvwxyz012345".to_string()),
                ..BrowserLocator::default()
            },
        ] {
            let candidate = BrowserReplayRepairCandidate::new(BrowserElementRef {
                revision: BrowserRevision(17),
                locator,
                backend_node_id: None,
            });
            assert!(matches!(
                coordinator.reserve_locator_repair_preview(&repair, candidate),
                Err(BrowserReplayError::InvalidRepairEvidence)
            ));
        }

        let foreign = BrowserReplayCoordinator::default();
        assert!(matches!(
            foreign.reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(17), "candidate")
            ),
            Err(BrowserReplayError::StaleInstance)
        ));
        coordinator.lock().next_preview_id = u64::MAX;
        assert!(matches!(
            coordinator.reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(17), "candidate")
            ),
            Err(BrowserReplayError::RepairPreviewIdExhausted)
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::AwaitingPreview
        );

        coordinator.cancel(&instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn preview_receipt_cas_preserves_old_preview_and_only_current_cleanup_survives() {
        let (root, store) = repair_store("preview-cas", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::default();
        let owner = BrowserWorkspaceKey::new("repair-preview", "cas").unwrap();
        let (instance, repair) = paused_repair(&coordinator, &store, owner, BrowserRevision(23));
        let first_cleanup = Arc::new(AtomicUsize::new(0));
        let second_cleanup = Arc::new(AtomicUsize::new(0));

        let (first_authority, first_receipt) = coordinator
            .reserve_locator_repair_preview(&repair, repair_candidate(BrowserRevision(23), "first"))
            .unwrap();
        assert!(first_authority.acknowledge_for_test());
        let first = first_receipt.consume_exact(&repair).unwrap();
        coordinator
            .commit_locator_repair_preview(first, || {
                BrowserReplayRepairHighlightCleanup::for_test(Arc::clone(&first_cleanup))
            })
            .unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Previewed
        );

        let (late_authority, late_receipt) = coordinator
            .reserve_locator_repair_preview(&repair, repair_candidate(BrowserRevision(23), "late"))
            .unwrap();
        let (current_authority, _current_receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(23), "current"),
            )
            .unwrap();
        assert!(late_authority.expected_previous_token_is_some_for_test());
        assert!(current_authority.expected_previous_token_is_some_for_test());
        assert!(!late_authority.acknowledge_for_test());
        assert!(late_receipt.consume_exact(&repair).is_none());
        assert_eq!(first_cleanup.load(Ordering::Acquire), 0);

        assert!(matches!(
            coordinator.abort_locator_repair_preview(&current_authority),
            BrowserReplayRepairPreviewAbortDisposition::RestorePrevious
        ));
        assert_eq!(
            coordinator.locator_repair_status(&repair).unwrap().phase,
            BrowserReplayRepairPhase::Previewed,
            "failed superseding preview retains the old acknowledged preview"
        );
        assert_eq!(first_cleanup.load(Ordering::Acquire), 0);

        let (current_authority, current_receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(23), "current"),
            )
            .unwrap();
        assert!(current_authority.acknowledge_for_test());
        let current = current_receipt.consume_exact(&repair).unwrap();
        coordinator
            .commit_locator_repair_preview(current, || {
                BrowserReplayRepairHighlightCleanup::for_test(Arc::clone(&second_cleanup))
            })
            .unwrap();
        assert_eq!(first_cleanup.load(Ordering::Acquire), 0);
        let (terminal_authority, _terminal_receipt) = coordinator
            .reserve_locator_repair_preview(
                &repair,
                repair_candidate(BrowserRevision(23), "terminal"),
            )
            .unwrap();
        coordinator.cancel(&instance).unwrap();
        assert!(matches!(
            coordinator.abort_locator_repair_preview(&terminal_authority),
            BrowserReplayRepairPreviewAbortDisposition::ClearExactOnly
        ));
        assert_eq!(first_cleanup.load(Ordering::Acquire), 0);
        assert_eq!(second_cleanup.load(Ordering::Acquire), 1);

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn executor_test_resume_rejects_a_stale_repair_generation() {
        fn reserve_and_publish(
            coordinator: &BrowserReplayCoordinator,
            instance: &BrowserReplayInstance,
            store: &BrowserResourceStore,
            revision: u64,
        ) -> BrowserReplayRepairInstance {
            let repair = coordinator
                .reserve_locator_repair_capture(
                    instance,
                    store,
                    0,
                    BrowserReplayLocatorSlot::PrimaryAction,
                    "runtime-tab-1",
                    BrowserRevision(revision),
                    BrowserReplayRepairResumeCursor::Action,
                )
                .unwrap();
            let snapshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairSnapshot,
                    "application/json",
                    b"{}",
                )
                .unwrap();
            let screenshot = coordinator
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairScreenshot,
                    "image/png",
                    b"png",
                )
                .unwrap();
            coordinator
                .publish_locator_repair(&repair, &snapshot, &screenshot)
                .unwrap();
            repair
        }

        let (root, store) = repair_store("stale-executor-resume", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::default();
        let owner = BrowserWorkspaceKey::new("repair-resume", "stale-generation").unwrap();
        let started = coordinator
            .start(owner, click_repair_plan(BrowserRecipeLocator::default()))
            .unwrap();
        coordinator.begin(&started.instance).unwrap();

        let stale_repair = reserve_and_publish(&coordinator, &started.instance, &store, 7);
        coordinator
            .resume_locator_repair_for_executor_test(&stale_repair)
            .unwrap();
        let active_repair = reserve_and_publish(&coordinator, &started.instance, &store, 8);

        let stale_resume = coordinator.resume_locator_repair_for_executor_test(&stale_repair);
        assert!(matches!(
            stale_resume,
            Err(BrowserReplayError::InvalidRepairEvidence)
        ));
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert_eq!(
            coordinator
                .locator_repair_status(&active_repair)
                .unwrap()
                .repair_id,
            active_repair.repair_id()
        );
        assert_ne!(stale_repair.repair_id(), active_repair.repair_id());

        coordinator.cancel(&started.instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_capture_receipt_is_exact_one_shot_and_rejects_late_use() {
        let (root, store) = repair_store("capture-receipt", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let owner = BrowserWorkspaceKey::new("repair-receipt", "repair-tab").unwrap();
        let started = coordinator
            .start(
                owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(7),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let (authority, receipt) = coordinator
            .issue_locator_repair_capture_authority(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
            )
            .unwrap();
        let handle = authority
            .retain(
                &store,
                &owner,
                "runtime-tab-1",
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        assert!(receipt
            .consume_exact(&repair, BrowserResourceKind::ReplayRepairSnapshot, &handle,)
            .is_some());
        assert!(receipt
            .consume_exact(&repair, BrowserResourceKind::ReplayRepairSnapshot, &handle,)
            .is_none());
        coordinator.cancel(&started.instance).unwrap();

        let late_started = coordinator
            .start(
                owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        coordinator.begin(&late_started.instance).unwrap();
        let late_repair = coordinator
            .reserve_locator_repair_capture(
                &late_started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(8),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let (late_authority, late_receipt) = coordinator
            .issue_locator_repair_capture_authority(
                &late_repair,
                BrowserResourceKind::ReplayRepairSnapshot,
            )
            .unwrap();
        let late_handle = late_authority
            .retain(
                &store,
                &owner,
                "runtime-tab-1",
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"late",
            )
            .unwrap();
        coordinator.cancel(&late_started.instance).unwrap();
        assert!(late_receipt
            .consume_exact(
                &late_repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                &late_handle,
            )
            .is_none());
        assert!(matches!(
            store.handle(&owner, &late_handle.id),
            Err(BrowserError::MissingResource { .. })
        ));

        drop(late_authority);
        drop(late_receipt);
        drop(authority);
        drop(receipt);
        drop(coordinator);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn locator_repair_capture_stays_running_until_exact_evidence_is_atomically_published() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-replay-repair-red-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let owner = BrowserWorkspaceKey::new("repair-project", "repair-tab").unwrap();
        let mut plan = internal_plan(Vec::new());
        plan.steps.push(BrowserRecipeStep {
            id: "click-submit".to_string(),
            action: BrowserRecipeAction::Click {
                locator: BrowserRecipeLocator {
                    test_id: Some("submit".to_string()),
                    ..BrowserRecipeLocator::default()
                },
            },
            wait: None,
            assertions: Vec::new(),
        });
        let started = coordinator.start(owner.clone(), plan).unwrap();
        let cancellation_lease = started.lease.clone();
        let cancellation_authority_id = started.lease.authority_id();
        coordinator.begin(&started.instance).unwrap();

        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(7),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        assert_eq!(
            coordinator.advance_step(&started.instance, 0),
            Err(BrowserReplayError::InvalidTransition)
        );
        assert_eq!(
            coordinator.complete(&started.instance),
            Err(BrowserReplayError::InvalidTransition)
        );
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::Running
        );
        assert!(matches!(
            coordinator.reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(7),
                BrowserReplayRepairResumeCursor::Action,
            ),
            Err(BrowserReplayError::InvalidTransition)
        ));

        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::Running
        );
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::Running
        );

        {
            let mut state = coordinator.lock();
            let active = state.active.get_mut(&owner).unwrap();
            active.projection.current_step_index = 1;
            active.projection.current_step_id = None;
        }
        assert_eq!(
            coordinator.publish_locator_repair(&repair, &snapshot, &screenshot),
            Err(BrowserReplayError::InvalidRepairEvidence)
        );
        {
            let mut state = coordinator.lock();
            let active = state.active.get_mut(&owner).unwrap();
            active.projection.current_step_index = 0;
            active.projection.current_step_id = Some("click-submit".to_string());
        }

        let published = coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();
        assert_eq!(published.phase, BrowserReplayRepairPhase::AwaitingPreview);
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert_eq!(started.lease.authority_id(), cancellation_authority_id);
        assert!(started.lease.same_authority(&cancellation_lease));
        assert!(!started.lease.is_cancelled());

        coordinator.cancel(&started.instance).unwrap();
        assert!(started.lease.is_cancelled());
        assert!(cancellation_lease.is_cancelled());
        assert!(matches!(
            store.handle(&owner, &snapshot.id),
            Err(BrowserError::MissingResource { .. })
        ));
        assert!(matches!(
            store.handle(&owner, &screenshot.id),
            Err(BrowserError::MissingResource { .. })
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn every_terminal_path_releases_partial_repair_evidence_while_token_survives() {
        #[derive(Clone, Copy)]
        enum TerminalPath {
            Cancel,
            Fail,
            Replace,
            Interrupt,
            CoordinatorDrop,
        }

        fn click_plan() -> BrowserReplayPlan {
            let mut plan = internal_plan(Vec::new());
            plan.steps.push(BrowserRecipeStep {
                id: "click-submit".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: BrowserRecipeLocator {
                        test_id: Some("submit".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                },
                wait: None,
                assertions: Vec::new(),
            });
            plan
        }

        for (index, terminal_path) in [
            TerminalPath::Cancel,
            TerminalPath::Fail,
            TerminalPath::Replace,
            TerminalPath::Interrupt,
            TerminalPath::CoordinatorDrop,
        ]
        .into_iter()
        .enumerate()
        {
            let root = std::env::temp_dir().join(format!(
                "devmanager-replay-repair-terminal-{}-{index}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let store = BrowserResourceStore::open(
                &root,
                BrowserResourceLimits {
                    max_temporary_count: 0,
                    max_temporary_bytes: 1024 * 1024,
                    max_resource_bytes: 1024 * 1024,
                },
            )
            .unwrap();
            let owner =
                BrowserWorkspaceKey::new(format!("repair-terminal-{index}"), "repair-tab").unwrap();
            let mut coordinator = Some(BrowserReplayCoordinator::with_terminal_capacity(2));
            let started = coordinator
                .as_ref()
                .unwrap()
                .start(owner.clone(), click_plan())
                .unwrap();
            let mut repair_watch = started.execution.repair_watch();
            coordinator
                .as_ref()
                .unwrap()
                .begin(&started.instance)
                .unwrap();
            let repair = coordinator
                .as_ref()
                .unwrap()
                .reserve_locator_repair_capture(
                    &started.instance,
                    &store,
                    0,
                    BrowserReplayLocatorSlot::PrimaryAction,
                    "runtime-tab-1",
                    BrowserRevision(7),
                    BrowserReplayRepairResumeCursor::Action,
                )
                .unwrap();
            let snapshot = coordinator
                .as_ref()
                .unwrap()
                .retain_locator_repair_evidence_for_test(
                    &repair,
                    BrowserResourceKind::ReplayRepairSnapshot,
                    "application/json",
                    b"{}",
                )
                .unwrap();
            assert_eq!(
                coordinator
                    .as_ref()
                    .unwrap()
                    .status(&started.instance)
                    .unwrap()
                    .status,
                BrowserReplayStatus::Running
            );
            assert!(!repair_watch.has_changed().unwrap());

            match terminal_path {
                TerminalPath::Cancel => {
                    coordinator
                        .as_ref()
                        .unwrap()
                        .cancel(&started.instance)
                        .unwrap();
                }
                TerminalPath::Fail => {
                    coordinator
                        .as_ref()
                        .unwrap()
                        .fail(&started.instance, BrowserReplayFailureCode::StepFailed)
                        .unwrap();
                }
                TerminalPath::Replace => {
                    coordinator
                        .as_ref()
                        .unwrap()
                        .replace(owner.clone(), click_plan())
                        .unwrap();
                }
                TerminalPath::Interrupt => {
                    coordinator
                        .as_ref()
                        .unwrap()
                        .interrupt_workspace(&owner)
                        .unwrap();
                }
                TerminalPath::CoordinatorDrop => drop(coordinator.take()),
            }

            assert_eq!(repair.replay_instance_id(), started.instance.id());
            match terminal_path {
                TerminalPath::Fail => {
                    assert_eq!(
                        coordinator
                            .as_ref()
                            .unwrap()
                            .status(&started.instance)
                            .unwrap()
                            .status,
                        BrowserReplayStatus::Failed
                    );
                    assert!(!started.execution.is_cancelled());
                }
                TerminalPath::CoordinatorDrop => {
                    assert!(started.lease.is_cancelled());
                    assert!(started.execution.is_cancelled());
                }
                TerminalPath::Cancel | TerminalPath::Replace | TerminalPath::Interrupt => {
                    assert_eq!(
                        coordinator
                            .as_ref()
                            .unwrap()
                            .status(&started.instance)
                            .unwrap()
                            .status,
                        BrowserReplayStatus::Cancelled
                    );
                    assert!(started.execution.is_cancelled());
                }
            }
            // The terminal/cancellation state above must be installed before this wake is visible.
            assert_eq!(*repair_watch.borrow_and_update(), 1);
            assert!(repair_watch.has_changed().is_err());
            assert!(matches!(
                store.handle(&owner, &snapshot.id),
                Err(BrowserError::MissingResource { .. })
            ));
            drop(coordinator);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_transitions_use_one_value_free_watch_and_private_override_map() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-replay-repair-watch-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let owner = BrowserWorkspaceKey::new("repair-watch", "repair-tab").unwrap();
        let mut plan = internal_plan(Vec::new());
        plan.steps.push(BrowserRecipeStep {
            id: "click-submit".to_string(),
            action: BrowserRecipeAction::Click {
                locator: BrowserRecipeLocator {
                    test_id: Some("submit".to_string()),
                    ..BrowserRecipeLocator::default()
                },
            },
            wait: None,
            assertions: Vec::new(),
        });
        let started = coordinator.start(owner, plan).unwrap();
        let mut repair_watch: tokio::sync::watch::Receiver<u64> = started.execution.repair_watch();
        assert_eq!(*repair_watch.borrow(), 0);
        assert_eq!(
            started
                .execution
                .locator_override(0, BrowserReplayLocatorSlot::PrimaryAction),
            None
        );
        coordinator.begin(&started.instance).unwrap();

        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(7),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        assert!(!repair_watch.has_changed().unwrap());

        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        assert!(!repair_watch.has_changed().unwrap());

        coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();
        assert!(repair_watch.has_changed().unwrap());
        assert_eq!(*repair_watch.borrow_and_update(), 1);

        coordinator.cancel(&started.instance).unwrap();
        assert_eq!(*repair_watch.borrow_and_update(), 2);
        assert!(repair_watch.has_changed().is_err());

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn locator_repair_status_is_exact_safe_checked_and_terminal_immutable() {
        const FORBIDDEN: &str = "locator-value-sentinel-do-not-project";
        let (root, store) = repair_store("status", 1024 * 1024);
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let owner = BrowserWorkspaceKey::new("repair-status", "repair-tab").unwrap();
        let started = coordinator
            .start(
                owner.clone(),
                click_repair_plan(BrowserRecipeLocator {
                    test_id: Some(FORBIDDEN.to_string()),
                    ..BrowserRecipeLocator::default()
                }),
            )
            .unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(11),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::InvalidTransition)
        );

        let same_workspace_other_coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let other_started = same_workspace_other_coordinator
            .start(
                owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        same_workspace_other_coordinator
            .begin(&other_started.instance)
            .unwrap();
        assert_eq!(
            same_workspace_other_coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::StaleInstance)
        );
        let other_workspace_coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let other_workspace_started = other_workspace_coordinator
            .start(
                BrowserWorkspaceKey::new("repair-other", "repair-tab").unwrap(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        other_workspace_coordinator
            .begin(&other_workspace_started.instance)
            .unwrap();
        assert_eq!(
            other_workspace_coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::StaleInstance)
        );

        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                FORBIDDEN.as_bytes(),
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                FORBIDDEN.as_bytes(),
            )
            .unwrap();
        let projection = coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair),
            Ok(projection.clone())
        );
        for rendered in [
            serde_json::to_string(&projection).unwrap(),
            format!("{projection:?}"),
        ] {
            assert!(!rendered.contains(FORBIDDEN));
        }

        coordinator.cancel(&started.instance).unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::TerminalState)
        );
        let replacement = coordinator
            .start(owner, click_repair_plan(BrowserRecipeLocator::default()))
            .unwrap();
        coordinator.cancel(&replacement.instance).unwrap();
        assert_eq!(
            coordinator.locator_repair_status(&repair),
            Err(BrowserReplayError::StaleInstance)
        );

        let exhausted_owner = BrowserWorkspaceKey::new("repair-exhausted", "repair-tab").unwrap();
        let exhausted = coordinator
            .start(
                exhausted_owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        coordinator.begin(&exhausted.instance).unwrap();
        coordinator.lock().next_repair_id = u64::MAX;
        assert!(matches!(
            coordinator.reserve_locator_repair_capture(
                &exhausted.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-2",
                BrowserRevision(12),
                BrowserReplayRepairResumeCursor::Action,
            ),
            Err(BrowserReplayError::RepairInstanceIdExhausted)
        ));
        assert_eq!(
            coordinator.status(&exhausted.instance).unwrap().status,
            BrowserReplayStatus::Running
        );
        assert!(store.list(&exhausted_owner).unwrap().is_empty());
        coordinator.cancel(&exhausted.instance).unwrap();

        drop(other_workspace_coordinator);
        drop(same_workspace_other_coordinator);
        drop(coordinator);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn repair_evidence_rejects_mime_handle_substitution_and_rolls_back_second_write() {
        let (mime_root, mime_store) = repair_store("mime", 1024 * 1024);
        let mime_coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let mime_owner = BrowserWorkspaceKey::new("repair-mime", "repair-tab").unwrap();
        let mime_started = mime_coordinator
            .start(
                mime_owner,
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        mime_coordinator.begin(&mime_started.instance).unwrap();
        let mime_repair = mime_coordinator
            .reserve_locator_repair_capture(
                &mime_started.instance,
                &mime_store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(1),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        assert!(matches!(
            mime_coordinator.retain_locator_repair_evidence_for_test(
                &mime_repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "text/plain",
                b"{}",
            ),
            Err(BrowserError::InvalidInvocation { ref field }) if field == "mimeType"
        ));
        assert_eq!(
            mime_coordinator.locator_repair_status(&mime_repair),
            Err(BrowserReplayError::InvalidRepairEvidence)
        );
        drop(mime_coordinator);
        drop(mime_store);
        std::fs::remove_dir_all(mime_root).unwrap();

        let (handle_root, handle_store) = repair_store("handles", 1024 * 1024);
        let handle_coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let handle_owner = BrowserWorkspaceKey::new("repair-handles", "repair-tab").unwrap();
        let handle_started = handle_coordinator
            .start(
                handle_owner,
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        handle_coordinator.begin(&handle_started.instance).unwrap();
        let handle_repair = handle_coordinator
            .reserve_locator_repair_capture(
                &handle_started.instance,
                &handle_store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(2),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let snapshot = handle_coordinator
            .retain_locator_repair_evidence_for_test(
                &handle_repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = handle_coordinator
            .retain_locator_repair_evidence_for_test(
                &handle_repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        let mut wrong = snapshot.clone();
        wrong.uri.push_str("-wrong");
        assert_eq!(
            handle_coordinator.publish_locator_repair(&handle_repair, &wrong, &screenshot),
            Err(BrowserReplayError::InvalidRepairEvidence)
        );
        assert_eq!(
            handle_coordinator.publish_locator_repair(&handle_repair, &screenshot, &snapshot),
            Err(BrowserReplayError::InvalidRepairEvidence)
        );
        std::fs::remove_file(handle_root.join(format!("{}.bin", screenshot.id.0))).unwrap();
        assert_eq!(
            handle_coordinator.publish_locator_repair(&handle_repair, &snapshot, &screenshot),
            Err(BrowserReplayError::RepairEvidenceUnavailable)
        );
        assert_eq!(
            handle_coordinator
                .status(&handle_started.instance)
                .unwrap()
                .status,
            BrowserReplayStatus::Running
        );
        drop(handle_coordinator);
        drop(handle_store);
        std::fs::remove_dir_all(handle_root).unwrap();

        let (rollback_root, rollback_store) = repair_store("rollback", 4);
        let rollback_coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
        let rollback_owner = BrowserWorkspaceKey::new("repair-rollback", "repair-tab").unwrap();
        let rollback_started = rollback_coordinator
            .start(
                rollback_owner.clone(),
                click_repair_plan(BrowserRecipeLocator::default()),
            )
            .unwrap();
        rollback_coordinator
            .begin(&rollback_started.instance)
            .unwrap();
        let rollback_repair = rollback_coordinator
            .reserve_locator_repair_capture(
                &rollback_started.instance,
                &rollback_store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(3),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let retained_snapshot = rollback_coordinator
            .retain_locator_repair_evidence_for_test(
                &rollback_repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        assert!(matches!(
            rollback_coordinator.retain_locator_repair_evidence_for_test(
                &rollback_repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"too-large",
            ),
            Err(BrowserError::ResourceTooLarge { .. })
        ));
        assert!(matches!(
            rollback_store.handle(&rollback_owner, &retained_snapshot.id),
            Err(BrowserError::MissingResource { .. })
        ));
        assert_eq!(
            rollback_coordinator
                .status(&rollback_started.instance)
                .unwrap()
                .status,
            BrowserReplayStatus::Running
        );
        assert_eq!(
            rollback_coordinator.locator_repair_status(&rollback_repair),
            Err(BrowserReplayError::InvalidRepairEvidence)
        );
        let retry = rollback_coordinator
            .reserve_locator_repair_capture(
                &rollback_started.instance,
                &rollback_store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "runtime-tab-1",
                BrowserRevision(3),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        assert!(retry.repair_id() > rollback_repair.repair_id());
        drop(rollback_coordinator);
        drop(rollback_store);
        std::fs::remove_dir_all(rollback_root).unwrap();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn locator_slot_cursor_table_is_exact_safe_and_resource_free_on_rejection() {
        fn literal(value: &str) -> BrowserRecipeValue {
            BrowserRecipeValue::Literal {
                value: value.to_string(),
            }
        }

        fn step(
            id: &str,
            action: BrowserRecipeAction,
            wait: Option<BrowserRecipeWait>,
            assertions: Vec<BrowserRecipeAssertion>,
        ) -> BrowserRecipeStep {
            BrowserRecipeStep {
                id: id.to_string(),
                action,
                wait,
                assertions,
            }
        }

        let locator = BrowserRecipeLocator {
            test_id: Some("target".to_string()),
            ..BrowserRecipeLocator::default()
        };
        let cases = vec![
            (
                "primary",
                step(
                    "primary",
                    BrowserRecipeAction::Click {
                        locator: locator.clone(),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::PrimaryAction,
                BrowserReplayRepairResumeCursor::Action,
                true,
            ),
            (
                "optional-some",
                step(
                    "optional-some",
                    BrowserRecipeAction::Keypress {
                        locator: Some(locator.clone()),
                        key: literal("Enter"),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::OptionalAction,
                BrowserReplayRepairResumeCursor::Action,
                true,
            ),
            (
                "optional-none",
                step(
                    "optional-none",
                    BrowserRecipeAction::Keypress {
                        locator: None,
                        key: literal("Enter"),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::OptionalAction,
                BrowserReplayRepairResumeCursor::Action,
                false,
            ),
            (
                "drag-source",
                step(
                    "drag-source",
                    BrowserRecipeAction::DragDrop {
                        source: locator.clone(),
                        destination: locator.clone(),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::DragSource,
                BrowserReplayRepairResumeCursor::Action,
                true,
            ),
            (
                "drag-destination",
                step(
                    "drag-destination",
                    BrowserRecipeAction::DragDrop {
                        source: locator.clone(),
                        destination: locator.clone(),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::DragDestination,
                BrowserReplayRepairResumeCursor::Action,
                true,
            ),
            (
                "action-wait",
                step(
                    "action-wait",
                    BrowserRecipeAction::Wait {
                        condition: BrowserRecipeWait::ElementPresent {
                            locator: locator.clone(),
                            timeout_ms: 10,
                        },
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::ActionWait,
                BrowserReplayRepairResumeCursor::ActionWait,
                true,
            ),
            (
                "action-hidden",
                step(
                    "action-hidden",
                    BrowserRecipeAction::Wait {
                        condition: BrowserRecipeWait::ElementHidden {
                            locator: locator.clone(),
                            timeout_ms: 10,
                        },
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::ActionWait,
                BrowserReplayRepairResumeCursor::ActionWait,
                false,
            ),
            (
                "step-wait",
                step(
                    "step-wait",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    Some(BrowserRecipeWait::ElementVisible {
                        locator: locator.clone(),
                        timeout_ms: 10,
                    }),
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::StepWait,
                BrowserReplayRepairResumeCursor::StepWait,
                true,
            ),
            (
                "step-hidden",
                step(
                    "step-hidden",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    Some(BrowserRecipeWait::ElementHidden {
                        locator: locator.clone(),
                        timeout_ms: 10,
                    }),
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::StepWait,
                BrowserReplayRepairResumeCursor::StepWait,
                false,
            ),
            (
                "assert-present",
                step(
                    "assert-present",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Element {
                        locator: locator.clone(),
                        state: BrowserRecipeElementState::Present,
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(0),
                true,
            ),
            (
                "assert-visible",
                step(
                    "assert-visible",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Element {
                        locator: locator.clone(),
                        state: BrowserRecipeElementState::Visible,
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(0),
                true,
            ),
            (
                "assert-value",
                step(
                    "assert-value",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Value {
                        locator: locator.clone(),
                        value: literal("expected"),
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(0),
                true,
            ),
            (
                "assert-absent",
                step(
                    "assert-absent",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Element {
                        locator: locator.clone(),
                        state: BrowserRecipeElementState::Absent,
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(0),
                false,
            ),
            (
                "assert-hidden",
                step(
                    "assert-hidden",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Element {
                        locator: locator.clone(),
                        state: BrowserRecipeElementState::Hidden,
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(0),
                false,
            ),
            (
                "cursor-mismatch",
                step(
                    "cursor-mismatch",
                    BrowserRecipeAction::Click {
                        locator: locator.clone(),
                    },
                    None,
                    Vec::new(),
                ),
                BrowserReplayLocatorSlot::PrimaryAction,
                BrowserReplayRepairResumeCursor::StepWait,
                false,
            ),
            (
                "assert-index-mismatch",
                step(
                    "assert-index-mismatch",
                    BrowserRecipeAction::Screenshot { full_page: false },
                    None,
                    vec![BrowserRecipeAssertion::Value {
                        locator: locator.clone(),
                        value: literal("expected"),
                    }],
                ),
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                BrowserReplayRepairResumeCursor::Assertion(1),
                false,
            ),
        ];

        let (root, store) = repair_store("slot-table", 1024 * 1024);
        for (label, recipe_step, locator_slot, resume_cursor, accepted) in cases {
            let coordinator = BrowserReplayCoordinator::with_terminal_capacity(1);
            let owner = BrowserWorkspaceKey::new(format!("repair-{label}"), "repair-tab").unwrap();
            let mut plan = internal_plan(Vec::new());
            plan.steps.push(recipe_step);
            let started = coordinator.start(owner.clone(), plan).unwrap();
            coordinator.begin(&started.instance).unwrap();
            let reserved = coordinator.reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                locator_slot,
                "runtime-tab-1",
                BrowserRevision(1),
                resume_cursor,
            );
            if accepted {
                assert!(reserved.is_ok(), "{label} should be repairable");
            } else {
                assert!(matches!(
                    reserved,
                    Err(BrowserReplayError::InvalidRepairSlot)
                ));
                assert!(store.list(&owner).unwrap().is_empty());
                assert_eq!(
                    coordinator.status(&started.instance).unwrap().status,
                    BrowserReplayStatus::Running
                );
            }
            drop(coordinator);
            assert!(store.list(&owner).unwrap().is_empty());
        }

        assert_eq!(
            serde_json::to_value(BrowserReplayLocatorSlot::PrimaryAction).unwrap(),
            serde_json::json!("primaryAction")
        );
        assert_eq!(
            serde_json::to_value(BrowserReplayLocatorSlot::Assertion { index: 4 }).unwrap(),
            serde_json::json!({ "assertion": { "index": 4 } })
        );
        assert_eq!(
            serde_json::to_value(BrowserReplayRepairPhase::AwaitingPreview).unwrap(),
            serde_json::json!("awaitingPreview")
        );
        assert_eq!(
            serde_json::to_value(BrowserReplayRepairPhase::Previewed).unwrap(),
            serde_json::json!("previewed")
        );
        assert_eq!(
            serde_json::to_value(BrowserReplayRepairPhase::Applied).unwrap(),
            serde_json::json!("applied")
        );

        let bounded = BrowserReplayCoordinator::with_terminal_capacity(1);
        let bounded_owner = BrowserWorkspaceKey::new("repair-tab-bound", "repair-tab").unwrap();
        let bounded_started = bounded
            .start(bounded_owner.clone(), click_repair_plan(locator.clone()))
            .unwrap();
        bounded.begin(&bounded_started.instance).unwrap();
        assert!(matches!(
            bounded.reserve_locator_repair_capture(
                &bounded_started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "x".repeat(1_025),
                BrowserRevision(1),
                BrowserReplayRepairResumeCursor::Action,
            ),
            Err(BrowserReplayError::InvalidRepairEvidence)
        ));
        assert!(store.list(&bounded_owner).unwrap().is_empty());
        drop(bounded);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn secret_submission(values: Vec<(&str, String)>) -> BrowserReplaySecretSubmission {
        BrowserReplaySecretSubmission::from_user_prompt(
            values
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        )
    }

    fn one_secret_submission() -> BrowserReplaySecretSubmission {
        secret_submission(vec![("password", SECRET_SENTINEL.to_string())])
    }

    fn started_secret_replay(
        coordinator: &BrowserReplayCoordinator,
        workspace_key: BrowserWorkspaceKey,
    ) -> BrowserReplayStart {
        coordinator
            .start(workspace_key, internal_plan(vec!["password".to_string()]))
            .unwrap()
    }

    #[test]
    fn replay_secret_submission_is_exact_complete_and_one_shot() {
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let workspace_key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        let started = started_secret_replay(&coordinator, workspace_key);

        assert_eq!(
            started.projection.status,
            BrowserReplayStatus::NeedsUserSecret
        );
        let projection = coordinator
            .submit_secrets(&started.instance, one_secret_submission())
            .unwrap();
        assert_eq!(projection.status, BrowserReplayStatus::Running);
        assert!(projection.unresolved_secret_inputs.is_empty());
        assert!(!format!("{projection:?}").contains(SECRET_SENTINEL));
        assert!(!serde_json::to_string(&projection)
            .unwrap()
            .contains(SECRET_SENTINEL));

        let lease = started.execution.secret_lease("password").unwrap();
        assert_eq!(
            lease.expose(|value| value == SECRET_SENTINEL).unwrap(),
            true
        );
        assert_eq!(
            coordinator.submit_secrets(&started.instance, one_secret_submission()),
            Err(BrowserReplaySecretError::AlreadySubmitted)
        );
    }

    #[test]
    fn replay_secret_submission_rejects_invalid_sets_without_mutating_the_store() {
        let invalid = vec![
            secret_submission(vec![("", SECRET_SENTINEL.to_string())]),
            secret_submission(vec![(
                &"n".repeat(MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES + 1),
                SECRET_SENTINEL.to_string(),
            )]),
            secret_submission(vec![("password", String::new())]),
            secret_submission(vec![(
                "password",
                "x".repeat(MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES + 1),
            )]),
            secret_submission(vec![
                ("password", SECRET_SENTINEL.to_string()),
                ("password", "other-value".to_string()),
            ]),
            secret_submission(Vec::new()),
            secret_submission(vec![
                ("password", SECRET_SENTINEL.to_string()),
                ("extra", "other-value".to_string()),
            ]),
        ];

        for (index, submission) in invalid.into_iter().enumerate() {
            let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
            let workspace_key =
                BrowserWorkspaceKey::new("project", format!("invalid-{index}")).unwrap();
            let started = started_secret_replay(&coordinator, workspace_key);

            assert_eq!(
                coordinator.submit_secrets(&started.instance, submission),
                Err(BrowserReplaySecretError::InvalidSubmission)
            );
            assert_eq!(
                coordinator.status(&started.instance).unwrap().status,
                BrowserReplayStatus::NeedsUserSecret
            );
            assert!(started.execution.secret_lease("password").is_err());

            coordinator
                .submit_secrets(&started.instance, one_secret_submission())
                .unwrap();
            assert!(started.execution.secret_lease("password").is_ok());
        }
    }

    #[test]
    fn replay_secret_submission_rejects_stale_and_foreign_authority_without_mutation() {
        let left = BrowserReplayCoordinator::with_terminal_capacity(4);
        let right = BrowserReplayCoordinator::with_terminal_capacity(4);
        let workspace_key = BrowserWorkspaceKey::new("project", "shared-name").unwrap();
        let left_started = started_secret_replay(&left, workspace_key.clone());
        let right_started = started_secret_replay(&right, workspace_key.clone());

        assert_eq!(
            left.submit_secrets(&right_started.instance, one_secret_submission()),
            Err(BrowserReplaySecretError::StaleAuthority)
        );
        assert_eq!(
            left.status(&left_started.instance).unwrap().status,
            BrowserReplayStatus::NeedsUserSecret
        );

        let replacement = left
            .replace(workspace_key, internal_plan(vec!["password".to_string()]))
            .unwrap();
        assert_eq!(
            left.submit_secrets(&left_started.instance, one_secret_submission()),
            Err(BrowserReplaySecretError::StaleAuthority)
        );
        assert_eq!(
            left.status(&replacement.instance).unwrap().status,
            BrowserReplayStatus::NeedsUserSecret
        );
        left.submit_secrets(&replacement.instance, one_secret_submission())
            .unwrap();
    }

    #[test]
    fn replay_secret_submission_enforces_the_exact_input_count_limit() {
        let names = |count: usize| {
            (0..count)
                .map(|index| format!("secret_{index}"))
                .collect::<Vec<_>>()
        };
        let submission = |count: usize| {
            BrowserReplaySecretSubmission::from_user_prompt(
                (0..count)
                    .map(|index| (format!("secret_{index}"), format!("value-{index}")))
                    .collect(),
            )
        };

        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(4);
        let accepted = coordinator
            .start(
                BrowserWorkspaceKey::new("project", "thirty-two-secrets").unwrap(),
                internal_plan(names(MAX_BROWSER_REPLAY_SECRET_INPUTS)),
            )
            .unwrap();
        assert_eq!(
            coordinator
                .submit_secrets(
                    &accepted.instance,
                    submission(MAX_BROWSER_REPLAY_SECRET_INPUTS),
                )
                .unwrap()
                .status,
            BrowserReplayStatus::Running
        );

        let rejected = coordinator
            .start(
                BrowserWorkspaceKey::new("project", "thirty-three-secrets").unwrap(),
                internal_plan(names(MAX_BROWSER_REPLAY_SECRET_INPUTS + 1)),
            )
            .unwrap();
        assert_eq!(
            coordinator.submit_secrets(
                &rejected.instance,
                submission(MAX_BROWSER_REPLAY_SECRET_INPUTS + 1),
            ),
            Err(BrowserReplaySecretError::InvalidSubmission)
        );
        assert_eq!(
            coordinator.status(&rejected.instance).unwrap().status,
            BrowserReplayStatus::NeedsUserSecret
        );
    }

    #[test]
    fn replay_secret_submission_rejects_credential_shaped_input_names() {
        let credential_name = "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let started = coordinator
            .start(
                BrowserWorkspaceKey::new("project", "unsafe-secret-name").unwrap(),
                internal_plan(vec![credential_name.to_string()]),
            )
            .unwrap();

        assert_eq!(
            coordinator.submit_secrets(
                &started.instance,
                secret_submission(vec![(credential_name, SECRET_SENTINEL.to_string())]),
            ),
            Err(BrowserReplaySecretError::InvalidSubmission)
        );
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::NeedsUserSecret
        );
    }

    #[test]
    fn retained_secret_leases_close_and_owned_bytes_clear_on_every_terminal_path() {
        enum TerminalPath {
            Complete,
            Fail,
            Cancel,
            Replace,
            Interrupt,
            CoordinatorDrop,
        }

        for (index, terminal_path) in [
            TerminalPath::Complete,
            TerminalPath::Fail,
            TerminalPath::Cancel,
            TerminalPath::Replace,
            TerminalPath::Interrupt,
            TerminalPath::CoordinatorDrop,
        ]
        .into_iter()
        .enumerate()
        {
            let coordinator = BrowserReplayCoordinator::with_terminal_capacity(4);
            let workspace_key =
                BrowserWorkspaceKey::new("project", format!("terminal-{index}")).unwrap();
            let started = started_secret_replay(&coordinator, workspace_key.clone());
            coordinator
                .submit_secrets(&started.instance, one_secret_submission())
                .unwrap();
            let cleared: Arc<AtomicUsize> = started.execution.observe_memory_clear_for_test();
            let lease = started.execution.secret_lease("password").unwrap();
            assert!(lease.expose(|value| value == SECRET_SENTINEL).unwrap());

            match terminal_path {
                TerminalPath::Complete => {
                    coordinator.complete(&started.instance).unwrap();
                }
                TerminalPath::Fail => {
                    coordinator
                        .fail(&started.instance, BrowserReplayFailureCode::StepFailed)
                        .unwrap();
                }
                TerminalPath::Cancel => {
                    coordinator.cancel(&started.instance).unwrap();
                }
                TerminalPath::Replace => {
                    coordinator
                        .replace(workspace_key, internal_plan(Vec::new()))
                        .unwrap();
                }
                TerminalPath::Interrupt => {
                    coordinator.interrupt_workspace(&workspace_key).unwrap();
                }
                TerminalPath::CoordinatorDrop => drop(coordinator),
            }

            assert_eq!(
                lease.expose(|_| ()),
                Err(BrowserReplaySecretError::ClosedStore)
            );
            assert_eq!(cleared.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn replay_instance_identity_overflow_fails_closed_without_installing_a_plan() {
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        coordinator.lock().next_instance_id = u64::MAX;
        let workspace_key = BrowserWorkspaceKey::new("project", "overflow").unwrap();

        let error = match coordinator.start(workspace_key.clone(), internal_plan(Vec::new())) {
            Ok(_) => panic!("overflow unexpectedly installed a replay"),
            Err(error) => error,
        };
        assert_eq!(error, BrowserReplayError::InstanceIdExhausted);
        assert!(!coordinator.lock().active.contains_key(&workspace_key));
    }

    #[test]
    fn retained_cancellation_lease_does_not_retain_terminal_plan() {
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let workspace_key = BrowserWorkspaceKey::new("project", "plan-drop").unwrap();
        let started = coordinator
            .start(workspace_key, internal_plan(Vec::new()))
            .unwrap();
        let BrowserReplayStart {
            instance,
            projection: _,
            lease,
            execution,
        } = started;
        let plan = Arc::downgrade(&execution.plan);

        drop(execution);
        coordinator.cancel(&instance).unwrap();

        assert!(lease.is_cancelled());
        assert!(plan.upgrade().is_none());
    }

    #[test]
    fn replay_coordinator_recovers_a_poisoned_lock_without_panicking_or_leaking_values() {
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let inner = coordinator.inner.clone();
        let poisoned = std::panic::catch_unwind(move || {
            let _guard = inner.lock().unwrap();
            panic!("intentional replay coordinator poison");
        });
        assert!(poisoned.is_err());

        let workspace_key = BrowserWorkspaceKey::new("project", "recovered").unwrap();
        let started = coordinator
            .start(workspace_key, internal_plan(Vec::new()))
            .unwrap();
        assert_eq!(started.projection.status, BrowserReplayStatus::Pending);
    }
}
