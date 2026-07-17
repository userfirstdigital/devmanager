use super::{
    BrowserRecipeAction, BrowserRecipeInputKind, BrowserRecipeStep, BrowserRecipeV1,
    BrowserRecipeValue, BrowserRecipeViewport, BrowserWorkspaceKey,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub const MAX_BROWSER_REPLAY_INPUTS: usize = 64;
pub const MAX_BROWSER_REPLAY_STEPS: usize = 256;
pub const MAX_BROWSER_REPLAY_INPUT_NAME_BYTES: usize = 128;
pub const MAX_BROWSER_REPLAY_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_BROWSER_REPLAY_URL_BYTES: usize = 8 * 1024;
pub const MAX_BROWSER_REPLAY_FILE_BYTES: usize = 32 * 1024;

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
    InvalidTransition,
    StepOutOfOrder,
    IncompleteReplay,
    TerminalState,
    InstanceIdExhausted,
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
            Self::InvalidTransition => "browser replay status transition is invalid",
            Self::StepOutOfOrder => "browser replay step is out of order",
            Self::IncompleteReplay => "browser replay has incomplete steps",
            Self::TerminalState => "browser replay instance is terminal",
            Self::InstanceIdExhausted => "browser replay instance identity is exhausted",
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

struct ActiveBrowserReplay {
    instance: BrowserReplayInstance,
    plan: BrowserReplayPlan,
    projection: BrowserReplayProjection,
    lease: BrowserReplayCancellationLease,
}

struct TerminalBrowserReplay {
    instance: BrowserReplayInstance,
    projection: BrowserReplayProjection,
}

struct BrowserReplayCoordinatorState {
    scope: Arc<BrowserReplayCoordinatorScope>,
    next_instance_id: u64,
    active: HashMap<BrowserWorkspaceKey, ActiveBrowserReplay>,
    terminal: VecDeque<TerminalBrowserReplay>,
    terminal_capacity: usize,
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
                next_instance_id: 0,
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

    #[allow(dead_code)] // Reserved as the value-free checkpoint-9 secret-store seam.
    pub(crate) fn secrets_ready(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::NeedsUserSecret {
                return Err(BrowserReplayError::InvalidTransition);
            }
            active.projection.status = BrowserReplayStatus::Running;
            active.projection.unresolved_secret_inputs.clear();
            Ok(active.projection.clone())
        })
    }

    pub fn advance_step(
        &self,
        instance: &BrowserReplayInstance,
        step_index: usize,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::Running {
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

    pub fn pause_locator_repair(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::Running {
                return Err(BrowserReplayError::InvalidTransition);
            }
            active.projection.status = BrowserReplayStatus::PausedLocatorRepair;
            Ok(active.projection.clone())
        })
    }

    pub fn resume_locator_repair(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        self.transition_nonterminal(instance, |active| {
            if active.projection.status != BrowserReplayStatus::PausedLocatorRepair {
                return Err(BrowserReplayError::InvalidTransition);
            }
            active.projection.status = BrowserReplayStatus::Running;
            Ok(active.projection.clone())
        })
    }

    pub fn complete(
        &self,
        instance: &BrowserReplayInstance,
    ) -> Result<BrowserReplayProjection, BrowserReplayError> {
        let mut state = self.lock();
        {
            let active = Self::exact_active_mut(&mut state, instance)?;
            if active.projection.status != BrowserReplayStatus::Running {
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
        let lease = BrowserReplayCancellationLease {
            authority: Arc::new(BrowserReplayCancellationAuthority {
                id: instance_id,
                cancelled: AtomicBool::new(false),
            }),
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
            },
        );
        Ok(BrowserReplayStart {
            instance,
            projection,
            lease,
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
        Self::exact_active_mut(state, instance)?;
        let Some(mut active) = state.active.remove(instance.workspace_key()) else {
            return Err(BrowserReplayError::StaleInstance);
        };
        if status == BrowserReplayStatus::Cancelled {
            active
                .lease
                .authority
                .cancelled
                .store(true, Ordering::Release);
        }
        active.projection.status = status;
        active.projection.failure = failure;
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

pub fn compile_browser_replay(
    recipe: &BrowserRecipeV1,
    public_inputs: Vec<BrowserReplayPublicInput>,
) -> Result<BrowserReplayPlan, BrowserReplayError> {
    recipe
        .validate()
        .map_err(|_| BrowserReplayError::InvalidRecipe)?;
    if recipe.inputs.len() > MAX_BROWSER_REPLAY_INPUTS
        || public_inputs.len() > MAX_BROWSER_REPLAY_INPUTS
        || recipe.steps.len() > MAX_BROWSER_REPLAY_STEPS
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

    fn internal_plan(unresolved_secret_inputs: Vec<String>) -> BrowserReplayPlan {
        BrowserReplayPlan {
            recipe_id: "internal-recipe".to_string(),
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

    #[test]
    fn replay_secret_readiness_is_an_internal_value_free_transition() {
        let coordinator = BrowserReplayCoordinator::with_terminal_capacity(2);
        let workspace_key = BrowserWorkspaceKey::new("project", "conversation").unwrap();
        let plan = internal_plan(vec!["password".to_string()]);
        let started = coordinator.start(workspace_key, plan).unwrap();

        assert_eq!(
            started.projection.status,
            BrowserReplayStatus::NeedsUserSecret
        );
        assert_eq!(
            coordinator.secrets_ready(&started.instance).unwrap().status,
            BrowserReplayStatus::Running
        );
        assert_eq!(
            coordinator.complete(&started.instance).unwrap().status,
            BrowserReplayStatus::Completed
        );
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
