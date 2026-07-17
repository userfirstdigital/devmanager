use super::{
    redact_browser_text, validate_browser_url, BrowserError, BrowserRecipeAction,
    BrowserRecipeAssertion, BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeLocator,
    BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeValue, BrowserRecipeViewport,
    BrowserRecipeWait, BrowserRisk, BrowserWorkspaceKey, BROWSER_RECIPE_SCHEMA_VERSION,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

const DEFAULT_RECORDING_CAPACITY: usize = 256;
const MAX_RECORDING_PERCENT_DECODE_PASSES: usize = 8;
pub const MAX_BROWSER_RECORDING_INPUTS: usize = 64;
pub const MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION: usize = 16;
pub const MAX_BROWSER_RECORDING_ASSERTIONS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRecordingActor {
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRecordingStatus {
    Inactive,
    Recording,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRecordingCommit {
    Buffered,
    Recorded,
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserRecordingError {
    AlreadyActive,
    StaleInstance,
    StaleReservation,
    CapacityExceeded,
    InvalidAction,
    InvalidMutation,
}

impl fmt::Display for BrowserRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyActive => formatter.write_str("browser recording is already active"),
            Self::StaleInstance => formatter.write_str("browser recording instance is stale"),
            Self::StaleReservation => formatter.write_str("browser recording reservation is stale"),
            Self::CapacityExceeded => formatter.write_str("browser recording capacity was reached"),
            Self::InvalidAction => formatter.write_str("browser recording action is invalid"),
            Self::InvalidMutation => formatter.write_str("browser recording mutation is invalid"),
        }
    }
}

impl std::error::Error for BrowserRecordingError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRecordingInstance {
    workspace_key: BrowserWorkspaceKey,
    id: u64,
}

impl BrowserRecordingInstance {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserRecordingReview {
    instance: BrowserRecordingInstance,
    recipe: BrowserRecipeV1,
    generated_inputs: BTreeSet<String>,
}

/// Mutable review metadata is accepted by value and sanitized before it is
/// copied into recorder state. It intentionally has no `Debug`/`Serialize`
/// implementation so rejected credential-like text cannot be formatted by the
/// recording domain.
pub struct BrowserRecordingMetadata {
    pub id: String,
    pub name: String,
    pub description: String,
    pub start_url: String,
    pub viewport: BrowserRecipeViewport,
}

impl BrowserRecordingReview {
    pub fn instance(&self) -> &BrowserRecordingInstance {
        &self.instance
    }

    pub fn recipe(&self) -> &BrowserRecipeV1 {
        &self.recipe
    }
}

/// An ephemeral capture value. It deliberately implements neither `Debug` nor
/// `Serialize`; constructors replace sensitive text with an unset input marker
/// before the value can enter recorder state.
pub struct BrowserRecordingAction {
    action: PendingRecordingAction,
    wait: Option<BrowserRecipeWait>,
    assertions: Vec<BrowserRecipeAssertion>,
}

enum PendingRecordingAction {
    Recipe(BrowserRecipeAction),
    SecretType(BrowserRecipeLocator),
    FileUpload(BrowserRecipeLocator),
}

impl BrowserRecordingAction {
    pub fn navigate(url: &str) -> Result<Self, BrowserRecordingError> {
        let url = sanitize_recording_url(url)?;
        Self::recipe(BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value: url },
        })
    }

    pub fn type_text(
        locator: BrowserRecipeLocator,
        text: &str,
    ) -> Result<Self, BrowserRecordingError> {
        validate_locator(&locator)?;
        if redact_browser_text(text) != text {
            return Ok(Self {
                action: PendingRecordingAction::SecretType(locator),
                wait: None,
                assertions: Vec::new(),
            });
        }
        let action = if text.is_empty() {
            BrowserRecipeAction::Clear { locator }
        } else {
            BrowserRecipeAction::Type {
                locator,
                value: BrowserRecipeValue::Literal {
                    value: text.to_string(),
                },
            }
        };
        Self::recipe(action)
    }

    pub fn type_password(locator: BrowserRecipeLocator) -> Result<Self, BrowserRecordingError> {
        validate_locator(&locator)?;
        Ok(Self {
            action: PendingRecordingAction::SecretType(locator),
            wait: None,
            assertions: Vec::new(),
        })
    }

    pub fn type_clipboard(locator: BrowserRecipeLocator) -> Result<Self, BrowserRecordingError> {
        // Clipboard contents never cross this API boundary. Replay receives an
        // unset secret input in the same way as other sensitive interactions.
        Self::type_password(locator)
    }

    pub fn upload(locator: BrowserRecipeLocator) -> Result<Self, BrowserRecordingError> {
        validate_locator(&locator)?;
        Ok(Self {
            action: PendingRecordingAction::FileUpload(locator),
            wait: None,
            assertions: Vec::new(),
        })
    }

    pub fn recipe(action: BrowserRecipeAction) -> Result<Self, BrowserRecordingError> {
        let action = sanitize_recipe_action(action)?;
        Ok(Self {
            action,
            wait: None,
            assertions: Vec::new(),
        })
    }

    pub fn with_wait(mut self, wait: BrowserRecipeWait) -> Result<Self, BrowserRecordingError> {
        if wait_has_input_reference(&wait) {
            return Err(BrowserRecordingError::InvalidAction);
        }
        validate_wire_node(&wait)?;
        self.wait = Some(wait);
        Ok(self)
    }

    pub fn with_assertions(
        mut self,
        assertions: Vec<BrowserRecipeAssertion>,
    ) -> Result<Self, BrowserRecordingError> {
        if assertions.len() > MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        for assertion in &assertions {
            if assertion_has_input_reference(assertion) {
                return Err(BrowserRecordingError::InvalidAction);
            }
            validate_wire_node(assertion)?;
        }
        self.assertions = assertions;
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRecordingReservation {
    workspace_key: BrowserWorkspaceKey,
    instance_id: u64,
    sequence: u64,
}

struct ReservationContext {
    actor: BrowserRecordingActor,
    tab_id: String,
    risk: BrowserRisk,
}

enum ReservationState {
    Pending,
    Ready(BrowserRecordingAction),
    Cancelled,
}

struct ReservationSlot {
    context: ReservationContext,
    state: ReservationState,
}

struct RecordedStep {
    actor: BrowserRecordingActor,
    tab_id: String,
    risk: BrowserRisk,
    step: BrowserRecipeStep,
}

struct ActiveRecording {
    instance: BrowserRecordingInstance,
    next_sequence: u64,
    next_to_drain: u64,
    reservations: BTreeMap<u64, ReservationSlot>,
    inputs: Vec<BrowserRecipeInput>,
    generated_inputs: BTreeSet<String>,
    steps: Vec<RecordedStep>,
}

enum WorkspaceRecordingState {
    Recording(ActiveRecording),
    Review(BrowserRecordingReview),
}

pub struct BrowserWorkflowRecorder {
    capacity: usize,
    next_instance_id: u64,
    workspaces: HashMap<BrowserWorkspaceKey, WorkspaceRecordingState>,
}

impl Default for BrowserWorkflowRecorder {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_RECORDING_CAPACITY)
    }
}

impl BrowserWorkflowRecorder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            next_instance_id: 0,
            workspaces: HashMap::new(),
        }
    }

    pub fn status(&self, workspace_key: &BrowserWorkspaceKey) -> BrowserRecordingStatus {
        match self.workspaces.get(workspace_key) {
            Some(WorkspaceRecordingState::Recording(_)) => BrowserRecordingStatus::Recording,
            Some(WorkspaceRecordingState::Review(_)) => BrowserRecordingStatus::Review,
            None => BrowserRecordingStatus::Inactive,
        }
    }

    pub fn start(
        &mut self,
        workspace_key: BrowserWorkspaceKey,
    ) -> Result<BrowserRecordingInstance, BrowserRecordingError> {
        if self.workspaces.contains_key(&workspace_key) {
            return Err(BrowserRecordingError::AlreadyActive);
        }
        self.next_instance_id = self.next_instance_id.saturating_add(1);
        let instance = BrowserRecordingInstance {
            workspace_key: workspace_key.clone(),
            id: self.next_instance_id,
        };
        self.workspaces.insert(
            workspace_key,
            WorkspaceRecordingState::Recording(ActiveRecording {
                instance: instance.clone(),
                next_sequence: 0,
                next_to_drain: 0,
                reservations: BTreeMap::new(),
                inputs: Vec::new(),
                generated_inputs: BTreeSet::new(),
                steps: Vec::new(),
            }),
        );
        Ok(instance)
    }

    pub fn reserve(
        &mut self,
        instance: &BrowserRecordingInstance,
        actor: BrowserRecordingActor,
    ) -> Result<BrowserRecordingReservation, BrowserRecordingError> {
        let tab_id = instance.workspace_key.ai_tab_id.clone();
        self.reserve_on(instance, actor, tab_id, BrowserRisk::Normal)
    }

    pub fn reserve_on(
        &mut self,
        instance: &BrowserRecordingInstance,
        actor: BrowserRecordingActor,
        tab_id: impl Into<String>,
        risk: BrowserRisk,
    ) -> Result<BrowserRecordingReservation, BrowserRecordingError> {
        let tab_id = tab_id.into();
        if tab_id.trim().is_empty() {
            return Err(BrowserRecordingError::InvalidAction);
        }
        let capacity = self.capacity;
        let active = self.active_mut(instance)?;
        if active.steps.len().saturating_add(active.reservations.len()) >= capacity {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        let sequence = active.next_sequence;
        active.next_sequence = active.next_sequence.saturating_add(1);
        active.reservations.insert(
            sequence,
            ReservationSlot {
                context: ReservationContext {
                    actor,
                    tab_id,
                    risk,
                },
                state: ReservationState::Pending,
            },
        );
        Ok(BrowserRecordingReservation {
            workspace_key: instance.workspace_key.clone(),
            instance_id: instance.id,
            sequence,
        })
    }

    pub fn commit(
        &mut self,
        reservation: BrowserRecordingReservation,
        action: BrowserRecordingAction,
    ) -> Result<BrowserRecordingCommit, BrowserRecordingError> {
        let instance = BrowserRecordingInstance {
            workspace_key: reservation.workspace_key,
            id: reservation.instance_id,
        };
        let Ok(active) = self.active_mut(&instance) else {
            return Ok(BrowserRecordingCommit::Ignored);
        };
        let slot = active
            .reservations
            .get(&reservation.sequence)
            .ok_or(BrowserRecordingError::StaleReservation)?;
        if !matches!(slot.state, ReservationState::Pending) {
            return Err(BrowserRecordingError::StaleReservation);
        }
        let exceeds_capacity = retained_assertion_count(active)
            .saturating_add(action.assertions.len())
            > MAX_BROWSER_RECORDING_ASSERTIONS
            || projected_generated_input_count(active)
                .saturating_add(action_generated_input_count(&action))
                > MAX_BROWSER_RECORDING_INPUTS;
        if exceeds_capacity {
            let Some(slot) = active.reservations.get_mut(&reservation.sequence) else {
                return Err(BrowserRecordingError::StaleReservation);
            };
            slot.state = ReservationState::Cancelled;
            drain_ready(active);
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        let Some(slot) = active.reservations.get_mut(&reservation.sequence) else {
            return Err(BrowserRecordingError::StaleReservation);
        };
        slot.state = ReservationState::Ready(action);
        let previous_next = active.next_to_drain;
        drain_ready(active);
        Ok(commit_result(previous_next, active.next_to_drain))
    }

    pub fn cancel(
        &mut self,
        reservation: BrowserRecordingReservation,
    ) -> Result<BrowserRecordingCommit, BrowserRecordingError> {
        let instance = BrowserRecordingInstance {
            workspace_key: reservation.workspace_key,
            id: reservation.instance_id,
        };
        let Ok(active) = self.active_mut(&instance) else {
            return Ok(BrowserRecordingCommit::Ignored);
        };
        let slot = active
            .reservations
            .get_mut(&reservation.sequence)
            .ok_or(BrowserRecordingError::StaleReservation)?;
        if !matches!(slot.state, ReservationState::Pending) {
            return Err(BrowserRecordingError::StaleReservation);
        }
        slot.state = ReservationState::Cancelled;
        let previous_next = active.next_to_drain;
        drain_ready(active);
        Ok(commit_result(previous_next, active.next_to_drain))
    }

    pub fn active_step_count(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<usize, BrowserRecordingError> {
        match self.workspaces.get(&instance.workspace_key) {
            Some(WorkspaceRecordingState::Recording(active))
                if active.instance.id == instance.id =>
            {
                Ok(active.steps.len())
            }
            _ => Err(BrowserRecordingError::StaleInstance),
        }
    }

    pub fn stop(
        &mut self,
        instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let state = self
            .workspaces
            .remove(&instance.workspace_key)
            .ok_or(BrowserRecordingError::StaleInstance)?;
        let WorkspaceRecordingState::Recording(mut active) = state else {
            self.workspaces
                .insert(instance.workspace_key.clone(), state);
            return Err(BrowserRecordingError::StaleInstance);
        };
        if active.instance.id != instance.id {
            self.workspaces.insert(
                instance.workspace_key.clone(),
                WorkspaceRecordingState::Recording(active),
            );
            return Err(BrowserRecordingError::StaleInstance);
        }
        for slot in active.reservations.values_mut() {
            if matches!(slot.state, ReservationState::Pending) {
                slot.state = ReservationState::Cancelled;
            }
        }
        drain_ready(&mut active);
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("recording-{}", instance.id),
            name: format!("Recorded workflow {}", instance.id),
            description: String::new(),
            start_url: "about:blank".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: active.inputs,
            steps: active
                .steps
                .into_iter()
                .map(|recorded| recorded.step)
                .collect(),
        };
        let review = BrowserRecordingReview {
            instance: instance.clone(),
            recipe,
            generated_inputs: active.generated_inputs,
        };
        self.workspaces.insert(
            instance.workspace_key.clone(),
            WorkspaceRecordingState::Review(review.clone()),
        );
        Ok(review)
    }

    pub fn review(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        Ok(self.review_ref(instance)?.clone())
    }

    pub fn set_metadata(
        &mut self,
        instance: &BrowserRecordingInstance,
        metadata: BrowserRecordingMetadata,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        if redact_browser_text(&metadata.id) != metadata.id
            || redact_browser_text(&metadata.name) != metadata.name
            || redact_browser_text(&metadata.description) != metadata.description
        {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        let start_url = sanitize_recording_url(&metadata.start_url)
            .map_err(|_| BrowserRecordingError::InvalidMutation)?;
        let review = self.review_mut(instance)?;
        review.recipe.id = metadata.id;
        review.recipe.name = metadata.name;
        review.recipe.description = metadata.description;
        review.recipe.start_url = start_url;
        review.recipe.viewport = metadata.viewport;
        Ok(review.clone())
    }

    pub fn delete_step(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        let index = review
            .recipe
            .steps
            .iter()
            .position(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        review.recipe.steps.remove(index);
        garbage_collect_generated_inputs(review);
        Ok(review.clone())
    }

    pub fn move_step(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
        new_index: usize,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        if new_index >= review.recipe.steps.len() {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        let index = review
            .recipe
            .steps
            .iter()
            .position(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        let step = review.recipe.steps.remove(index);
        review.recipe.steps.insert(new_index, step);
        Ok(review.clone())
    }

    pub fn convert_action_value_to_input(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
        input_name: &str,
        input_kind: BrowserRecipeInputKind,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let (value, required_kind) = {
            let review = self.review_ref(instance)?;
            let step = review
                .recipe
                .steps
                .iter()
                .find(|step| step.id == step_id)
                .ok_or(BrowserRecordingError::InvalidMutation)?;
            action_value_and_kind(&step.action).ok_or(BrowserRecordingError::InvalidMutation)?
        };
        if required_kind != input_kind {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        if let BrowserRecipeValue::Input { name } = value {
            if name == input_name {
                return self.review(instance);
            }
            return self.rename_input(instance, &name, input_name);
        }
        let BrowserRecipeValue::Literal { value } = value else {
            return Err(BrowserRecordingError::InvalidMutation);
        };
        let default_value = match input_kind {
            BrowserRecipeInputKind::Text => {
                if redact_browser_text(&value) != value {
                    return Err(BrowserRecordingError::InvalidMutation);
                }
                Some(value)
            }
            BrowserRecipeInputKind::Url => {
                let sanitized = sanitize_recording_url(&value)
                    .map_err(|_| BrowserRecordingError::InvalidMutation)?;
                Some(sanitized)
            }
            BrowserRecipeInputKind::File | BrowserRecipeInputKind::Secret => {
                return Err(BrowserRecordingError::InvalidMutation);
            }
        };
        let input = BrowserRecipeInput {
            name: input_name.to_string(),
            kind: input_kind,
            default_value,
        };
        validate_wire_node(&input).map_err(|_| BrowserRecordingError::InvalidMutation)?;

        let review = self.review_mut(instance)?;
        if review
            .recipe
            .inputs
            .iter()
            .any(|existing| existing.name == input.name)
        {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        if review.recipe.inputs.len() >= MAX_BROWSER_RECORDING_INPUTS {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        let step = review
            .recipe
            .steps
            .iter_mut()
            .find(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        *action_value_mut(&mut step.action).ok_or(BrowserRecordingError::InvalidMutation)? =
            BrowserRecipeValue::Input {
                name: input.name.clone(),
            };
        review.generated_inputs.insert(input.name.clone());
        review.recipe.inputs.push(input);
        Ok(review.clone())
    }

    pub fn add_input(
        &mut self,
        instance: &BrowserRecordingInstance,
        input: BrowserRecipeInput,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        validate_wire_node(&input).map_err(|_| BrowserRecordingError::InvalidMutation)?;
        let review = self.review_mut(instance)?;
        if review
            .recipe
            .inputs
            .iter()
            .any(|existing| existing.name == input.name)
        {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        if review.recipe.inputs.len() >= MAX_BROWSER_RECORDING_INPUTS {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        review.recipe.inputs.push(input);
        Ok(review.clone())
    }

    pub fn rename_input(
        &mut self,
        instance: &BrowserRecordingInstance,
        previous_name: &str,
        new_name: &str,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        if previous_name != new_name
            && review
                .recipe
                .inputs
                .iter()
                .any(|input| input.name == new_name)
        {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        let index = review
            .recipe
            .inputs
            .iter()
            .position(|input| input.name == previous_name)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        let candidate = BrowserRecipeInput {
            name: new_name.to_string(),
            kind: review.recipe.inputs[index].kind,
            default_value: review.recipe.inputs[index].default_value.clone(),
        };
        validate_wire_node(&candidate).map_err(|_| BrowserRecordingError::InvalidMutation)?;
        let was_generated = review.generated_inputs.contains(previous_name);
        review.recipe.inputs[index] = candidate;
        rename_value_references(&mut review.recipe, previous_name, new_name);
        if was_generated {
            review.generated_inputs.remove(previous_name);
            review.generated_inputs.insert(new_name.to_string());
        }
        Ok(review.clone())
    }

    pub fn set_input_default(
        &mut self,
        instance: &BrowserRecordingInstance,
        input_name: &str,
        default_value: Option<String>,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        let index = review
            .recipe
            .inputs
            .iter()
            .position(|input| input.name == input_name)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        let candidate = BrowserRecipeInput {
            name: review.recipe.inputs[index].name.clone(),
            kind: review.recipe.inputs[index].kind,
            default_value,
        };
        validate_wire_node(&candidate).map_err(|_| BrowserRecordingError::InvalidMutation)?;
        review.recipe.inputs[index] = candidate;
        Ok(review.clone())
    }

    pub fn remove_input(
        &mut self,
        instance: &BrowserRecordingInstance,
        input_name: &str,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        if recipe_references_input(&review.recipe, input_name) {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        let index = review
            .recipe
            .inputs
            .iter()
            .position(|input| input.name == input_name)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        review.recipe.inputs.remove(index);
        review.generated_inputs.remove(input_name);
        Ok(review.clone())
    }

    pub fn set_step_wait(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
        wait: Option<BrowserRecipeWait>,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        if let Some(wait) = &wait {
            validate_wire_node(wait).map_err(|_| BrowserRecordingError::InvalidMutation)?;
        }
        let review = self.review_mut(instance)?;
        let step = review
            .recipe
            .steps
            .iter_mut()
            .find(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        step.wait = wait;
        Ok(review.clone())
    }

    pub fn add_step_assertion(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
        assertion: BrowserRecipeAssertion,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        validate_wire_node(&assertion).map_err(|_| BrowserRecordingError::InvalidMutation)?;
        let review = self.review_mut(instance)?;
        let step_index = review
            .recipe
            .steps
            .iter()
            .position(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        let total_assertions = review
            .recipe
            .steps
            .iter()
            .map(|step| step.assertions.len())
            .sum::<usize>();
        if review.recipe.steps[step_index].assertions.len()
            >= MAX_BROWSER_RECORDING_ASSERTIONS_PER_ACTION
            || total_assertions >= MAX_BROWSER_RECORDING_ASSERTIONS
        {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        review.recipe.steps[step_index].assertions.push(assertion);
        Ok(review.clone())
    }

    pub fn remove_step_assertion(
        &mut self,
        instance: &BrowserRecordingInstance,
        step_id: &str,
        assertion_index: usize,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let review = self.review_mut(instance)?;
        let step = review
            .recipe
            .steps
            .iter_mut()
            .find(|step| step.id == step_id)
            .ok_or(BrowserRecordingError::InvalidMutation)?;
        if assertion_index >= step.assertions.len() {
            return Err(BrowserRecordingError::InvalidMutation);
        }
        step.assertions.remove(assertion_index);
        Ok(review.clone())
    }

    pub fn recipe_for_save(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecipeV1, BrowserError> {
        let review = self
            .review_ref(instance)
            .map_err(|_| BrowserError::InvalidRecipe {
                message: "recording review instance is not active".to_string(),
            })?;
        review.recipe.validate()?;
        Ok(review.recipe.clone())
    }

    pub fn discard(
        &mut self,
        instance: &BrowserRecordingInstance,
    ) -> Result<(), BrowserRecordingError> {
        let matches = self.workspaces.get(&instance.workspace_key).is_some_and(|state| {
            matches!(state, WorkspaceRecordingState::Recording(active) if active.instance.id == instance.id)
                || matches!(state, WorkspaceRecordingState::Review(review) if review.instance.id == instance.id)
        });
        if !matches {
            return Err(BrowserRecordingError::StaleInstance);
        }
        self.workspaces.remove(&instance.workspace_key);
        Ok(())
    }

    fn active_mut(
        &mut self,
        instance: &BrowserRecordingInstance,
    ) -> Result<&mut ActiveRecording, BrowserRecordingError> {
        match self.workspaces.get_mut(&instance.workspace_key) {
            Some(WorkspaceRecordingState::Recording(active))
                if active.instance.id == instance.id =>
            {
                Ok(active)
            }
            _ => Err(BrowserRecordingError::StaleInstance),
        }
    }

    fn review_ref(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<&BrowserRecordingReview, BrowserRecordingError> {
        match self.workspaces.get(&instance.workspace_key) {
            Some(WorkspaceRecordingState::Review(review)) if review.instance.id == instance.id => {
                Ok(review)
            }
            _ => Err(BrowserRecordingError::StaleInstance),
        }
    }

    fn review_mut(
        &mut self,
        instance: &BrowserRecordingInstance,
    ) -> Result<&mut BrowserRecordingReview, BrowserRecordingError> {
        match self.workspaces.get_mut(&instance.workspace_key) {
            Some(WorkspaceRecordingState::Review(review)) if review.instance.id == instance.id => {
                Ok(review)
            }
            _ => Err(BrowserRecordingError::StaleInstance),
        }
    }
}

fn commit_result(previous_next: u64, next_to_drain: u64) -> BrowserRecordingCommit {
    if previous_next == next_to_drain {
        BrowserRecordingCommit::Buffered
    } else {
        BrowserRecordingCommit::Recorded
    }
}

fn retained_assertion_count(active: &ActiveRecording) -> usize {
    let recorded = active
        .steps
        .iter()
        .map(|step| step.step.assertions.len())
        .sum::<usize>();
    active.reservations.values().fold(recorded, |count, slot| {
        count.saturating_add(match &slot.state {
            ReservationState::Ready(action) => action.assertions.len(),
            ReservationState::Pending | ReservationState::Cancelled => 0,
        })
    })
}

fn projected_generated_input_count(active: &ActiveRecording) -> usize {
    active
        .reservations
        .values()
        .fold(active.inputs.len(), |count, slot| {
            count.saturating_add(match &slot.state {
                ReservationState::Ready(action) => action_generated_input_count(action),
                ReservationState::Pending | ReservationState::Cancelled => 0,
            })
        })
}

fn action_generated_input_count(action: &BrowserRecordingAction) -> usize {
    usize::from(matches!(
        &action.action,
        PendingRecordingAction::SecretType(_) | PendingRecordingAction::FileUpload(_)
    ))
}

fn drain_ready(active: &mut ActiveRecording) {
    loop {
        let Some(slot) = active.reservations.remove(&active.next_to_drain) else {
            break;
        };
        match slot.state {
            ReservationState::Ready(action) => {
                let sequence = active.next_to_drain;
                if coalesce_pending_sensitive(&active.steps, &active.inputs, &action, &slot.context)
                {
                    active.next_to_drain = active.next_to_drain.saturating_add(1);
                    continue;
                }
                let recorded = materialize_action(
                    action,
                    slot.context,
                    sequence,
                    &mut active.inputs,
                    &mut active.generated_inputs,
                );
                if !coalesce_step(&mut active.steps, &recorded) {
                    active.steps.push(recorded);
                }
                active.next_to_drain = active.next_to_drain.saturating_add(1);
            }
            ReservationState::Cancelled => {
                active.next_to_drain = active.next_to_drain.saturating_add(1);
            }
            ReservationState::Pending => {
                active.reservations.insert(active.next_to_drain, slot);
                break;
            }
        }
    }
}

fn coalesce_pending_sensitive(
    steps: &[RecordedStep],
    inputs: &[BrowserRecipeInput],
    next: &BrowserRecordingAction,
    context: &ReservationContext,
) -> bool {
    let Some(previous) = steps.last() else {
        return false;
    };
    if previous.actor != context.actor
        || previous.tab_id != context.tab_id
        || previous.risk != context.risk
        || previous.step.wait.is_some()
        || next.wait.is_some()
        || !previous.step.assertions.is_empty()
        || !next.assertions.is_empty()
    {
        return false;
    }
    let PendingRecordingAction::SecretType(next_locator) = &next.action else {
        return false;
    };
    let BrowserRecipeAction::Type {
        locator: previous_locator,
        value: BrowserRecipeValue::Input { name },
    } = &previous.step.action
    else {
        return false;
    };
    previous_locator == next_locator
        && inputs
            .iter()
            .any(|input| input.name == *name && input.kind == BrowserRecipeInputKind::Secret)
}

fn materialize_action(
    action: BrowserRecordingAction,
    context: ReservationContext,
    sequence: u64,
    inputs: &mut Vec<BrowserRecipeInput>,
    generated_inputs: &mut BTreeSet<String>,
) -> RecordedStep {
    let BrowserRecordingAction {
        action,
        wait,
        assertions,
    } = action;
    let action = match action {
        PendingRecordingAction::Recipe(action) => action,
        PendingRecordingAction::SecretType(locator) => {
            let name = next_input_name("secret", inputs);
            inputs.push(BrowserRecipeInput {
                name: name.clone(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            });
            generated_inputs.insert(name.clone());
            BrowserRecipeAction::Type {
                locator,
                value: BrowserRecipeValue::Input { name },
            }
        }
        PendingRecordingAction::FileUpload(locator) => {
            let name = next_input_name("file", inputs);
            inputs.push(BrowserRecipeInput {
                name: name.clone(),
                kind: BrowserRecipeInputKind::File,
                default_value: None,
            });
            generated_inputs.insert(name.clone());
            BrowserRecipeAction::Upload {
                locator,
                file: BrowserRecipeValue::Input { name },
            }
        }
    };
    RecordedStep {
        actor: context.actor,
        tab_id: context.tab_id,
        risk: context.risk,
        step: BrowserRecipeStep {
            id: format!("step-{}", sequence.saturating_add(1)),
            action,
            wait,
            assertions,
        },
    }
}

fn next_input_name(base: &str, inputs: &[BrowserRecipeInput]) -> String {
    if !inputs.iter().any(|input| input.name == base) {
        return base.to_string();
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = format!("{base}_{suffix}");
        if !inputs.iter().any(|input| input.name == candidate) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn coalesce_step(steps: &mut [RecordedStep], next: &RecordedStep) -> bool {
    let Some(previous) = steps.last_mut() else {
        return false;
    };
    if previous.actor != next.actor
        || previous.tab_id != next.tab_id
        || previous.risk != next.risk
        || previous.step.wait.is_some()
        || next.step.wait.is_some()
        || !previous.step.assertions.is_empty()
        || !next.step.assertions.is_empty()
    {
        return false;
    }

    let replace_text_state = match (&previous.step.action, &next.step.action) {
        (
            BrowserRecipeAction::Type {
                locator: previous_locator,
                value: BrowserRecipeValue::Literal { .. },
            },
            BrowserRecipeAction::Clear {
                locator: next_locator,
            },
        )
        | (
            BrowserRecipeAction::Clear {
                locator: previous_locator,
            },
            BrowserRecipeAction::Type {
                locator: next_locator,
                value: BrowserRecipeValue::Literal { .. },
            },
        )
        | (
            BrowserRecipeAction::Clear {
                locator: previous_locator,
            },
            BrowserRecipeAction::Clear {
                locator: next_locator,
            },
        ) => previous_locator == next_locator,
        _ => false,
    };
    if replace_text_state {
        previous.step.action = next.step.action.clone();
        return true;
    }

    match (&mut previous.step.action, &next.step.action) {
        (
            BrowserRecipeAction::Type {
                locator: previous_locator,
                value:
                    BrowserRecipeValue::Literal {
                        value: previous_value,
                    },
            },
            BrowserRecipeAction::Type {
                locator: next_locator,
                value: BrowserRecipeValue::Literal { value: next_value },
            },
        ) if previous_locator == next_locator => {
            *previous_value = next_value.clone();
            true
        }
        (
            BrowserRecipeAction::Navigate { url: previous_url },
            BrowserRecipeAction::Navigate { url: next_url },
        ) if previous_url == next_url => true,
        (
            BrowserRecipeAction::Select {
                locator: previous_locator,
                values: previous_values,
            },
            BrowserRecipeAction::Select {
                locator: next_locator,
                values: next_values,
            },
        ) if previous_locator == next_locator => {
            *previous_values = next_values.clone();
            true
        }
        _ => false,
    }
}

fn action_value_and_kind(
    action: &BrowserRecipeAction,
) -> Option<(BrowserRecipeValue, BrowserRecipeInputKind)> {
    match action {
        BrowserRecipeAction::Navigate { url } => Some((url.clone(), BrowserRecipeInputKind::Url)),
        BrowserRecipeAction::Type { value, .. }
        | BrowserRecipeAction::Keypress { key: value, .. } => {
            Some((value.clone(), BrowserRecipeInputKind::Text))
        }
        BrowserRecipeAction::Upload { file, .. } => {
            Some((file.clone(), BrowserRecipeInputKind::File))
        }
        _ => None,
    }
}

fn action_value_mut(action: &mut BrowserRecipeAction) -> Option<&mut BrowserRecipeValue> {
    match action {
        BrowserRecipeAction::Navigate { url } => Some(url),
        BrowserRecipeAction::Type { value, .. } => Some(value),
        BrowserRecipeAction::Keypress { key, .. } => Some(key),
        BrowserRecipeAction::Upload { file, .. } => Some(file),
        _ => None,
    }
}

fn rename_value_references(recipe: &mut BrowserRecipeV1, previous_name: &str, new_name: &str) {
    for step in &mut recipe.steps {
        visit_action_values_mut(&mut step.action, &mut |value| {
            rename_value(value, previous_name, new_name)
        });
        if let Some(wait) = &mut step.wait {
            visit_wait_values_mut(wait, &mut |value| {
                rename_value(value, previous_name, new_name)
            });
        }
        for assertion in &mut step.assertions {
            visit_assertion_values_mut(assertion, &mut |value| {
                rename_value(value, previous_name, new_name)
            });
        }
    }
}

fn rename_value(value: &mut BrowserRecipeValue, previous_name: &str, new_name: &str) {
    if let BrowserRecipeValue::Input { name } = value {
        if name == previous_name {
            *name = new_name.to_string();
        }
    }
}

fn visit_action_values_mut(
    action: &mut BrowserRecipeAction,
    visitor: &mut impl FnMut(&mut BrowserRecipeValue),
) {
    match action {
        BrowserRecipeAction::Navigate { url } => visitor(url),
        BrowserRecipeAction::Type { value, .. } => visitor(value),
        BrowserRecipeAction::Select { values, .. } => {
            for value in values {
                visitor(value);
            }
        }
        BrowserRecipeAction::Keypress { key, .. } => visitor(key),
        BrowserRecipeAction::Upload { file, .. } => visitor(file),
        BrowserRecipeAction::Wait { condition } => visit_wait_values_mut(condition, visitor),
        BrowserRecipeAction::Click { .. }
        | BrowserRecipeAction::Hover { .. }
        | BrowserRecipeAction::Focus { .. }
        | BrowserRecipeAction::Clear { .. }
        | BrowserRecipeAction::Scroll { .. }
        | BrowserRecipeAction::DragDrop { .. }
        | BrowserRecipeAction::Download { .. }
        | BrowserRecipeAction::Screenshot { .. } => {}
    }
}

fn visit_wait_values_mut(
    wait: &mut BrowserRecipeWait,
    visitor: &mut impl FnMut(&mut BrowserRecipeValue),
) {
    match wait {
        BrowserRecipeWait::Url { value, .. }
        | BrowserRecipeWait::TextPresent { value, .. }
        | BrowserRecipeWait::TextAbsent { value, .. } => visitor(value),
        BrowserRecipeWait::Duration { .. }
        | BrowserRecipeWait::Load { .. }
        | BrowserRecipeWait::NetworkIdle { .. }
        | BrowserRecipeWait::ElementPresent { .. }
        | BrowserRecipeWait::ElementVisible { .. }
        | BrowserRecipeWait::ElementHidden { .. } => {}
    }
}

fn visit_assertion_values_mut(
    assertion: &mut BrowserRecipeAssertion,
    visitor: &mut impl FnMut(&mut BrowserRecipeValue),
) {
    match assertion {
        BrowserRecipeAssertion::Url { value, .. }
        | BrowserRecipeAssertion::Title { value, .. }
        | BrowserRecipeAssertion::Text { value, .. }
        | BrowserRecipeAssertion::Value { value, .. } => visitor(value),
        BrowserRecipeAssertion::Element { .. } => {}
    }
}

fn recipe_references_input(recipe: &BrowserRecipeV1, input_name: &str) -> bool {
    recipe.steps.iter().any(|step| {
        action_references_input(&step.action, input_name)
            || step
                .wait
                .as_ref()
                .is_some_and(|wait| wait_references_input(wait, input_name))
            || step
                .assertions
                .iter()
                .any(|assertion| assertion_references_input(assertion, input_name))
    })
}

fn garbage_collect_generated_inputs(review: &mut BrowserRecordingReview) {
    let unreferenced = review
        .generated_inputs
        .iter()
        .filter(|name| !recipe_references_input(&review.recipe, name))
        .cloned()
        .collect::<BTreeSet<_>>();
    if unreferenced.is_empty() {
        return;
    }
    review
        .recipe
        .inputs
        .retain(|input| !unreferenced.contains(&input.name));
    review
        .generated_inputs
        .retain(|name| !unreferenced.contains(name));
}

fn action_references_input(action: &BrowserRecipeAction, input_name: &str) -> bool {
    match action {
        BrowserRecipeAction::Navigate { url } => value_references_input(url, input_name),
        BrowserRecipeAction::Type { value, .. } => value_references_input(value, input_name),
        BrowserRecipeAction::Select { values, .. } => values
            .iter()
            .any(|value| value_references_input(value, input_name)),
        BrowserRecipeAction::Keypress { key, .. } => value_references_input(key, input_name),
        BrowserRecipeAction::Upload { file, .. } => value_references_input(file, input_name),
        BrowserRecipeAction::Wait { condition } => wait_references_input(condition, input_name),
        BrowserRecipeAction::Click { .. }
        | BrowserRecipeAction::Hover { .. }
        | BrowserRecipeAction::Focus { .. }
        | BrowserRecipeAction::Clear { .. }
        | BrowserRecipeAction::Scroll { .. }
        | BrowserRecipeAction::DragDrop { .. }
        | BrowserRecipeAction::Download { .. }
        | BrowserRecipeAction::Screenshot { .. } => false,
    }
}

fn wait_references_input(wait: &BrowserRecipeWait, input_name: &str) -> bool {
    match wait {
        BrowserRecipeWait::Url { value, .. }
        | BrowserRecipeWait::TextPresent { value, .. }
        | BrowserRecipeWait::TextAbsent { value, .. } => value_references_input(value, input_name),
        BrowserRecipeWait::Duration { .. }
        | BrowserRecipeWait::Load { .. }
        | BrowserRecipeWait::NetworkIdle { .. }
        | BrowserRecipeWait::ElementPresent { .. }
        | BrowserRecipeWait::ElementVisible { .. }
        | BrowserRecipeWait::ElementHidden { .. } => false,
    }
}

fn assertion_references_input(assertion: &BrowserRecipeAssertion, input_name: &str) -> bool {
    match assertion {
        BrowserRecipeAssertion::Url { value, .. }
        | BrowserRecipeAssertion::Title { value, .. }
        | BrowserRecipeAssertion::Text { value, .. }
        | BrowserRecipeAssertion::Value { value, .. } => value_references_input(value, input_name),
        BrowserRecipeAssertion::Element { .. } => false,
    }
}

fn value_references_input(value: &BrowserRecipeValue, input_name: &str) -> bool {
    matches!(value, BrowserRecipeValue::Input { name } if name == input_name)
}

fn validate_locator(locator: &BrowserRecipeLocator) -> Result<(), BrowserRecordingError> {
    validate_wire_node(locator)
}

fn validate_wire_node<T>(value: &T) -> Result<(), BrowserRecordingError>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let value = serde_json::to_value(value).map_err(|_| BrowserRecordingError::InvalidAction)?;
    serde_json::from_value::<T>(value)
        .map(|_| ())
        .map_err(|_| BrowserRecordingError::InvalidAction)
}

fn sanitize_recipe_action(
    action: BrowserRecipeAction,
) -> Result<PendingRecordingAction, BrowserRecordingError> {
    if action_has_input_reference(&action) {
        return Err(BrowserRecordingError::InvalidAction);
    }
    match action {
        BrowserRecipeAction::Navigate {
            url: BrowserRecipeValue::Literal { value },
        } => Ok(PendingRecordingAction::Recipe(
            BrowserRecipeAction::Navigate {
                url: BrowserRecipeValue::Literal {
                    value: sanitize_recording_url(&value)?,
                },
            },
        )),
        BrowserRecipeAction::Type {
            locator,
            value: BrowserRecipeValue::Literal { value },
        } if redact_browser_text(&value) != value => {
            validate_locator(&locator)?;
            Ok(PendingRecordingAction::SecretType(locator))
        }
        BrowserRecipeAction::Type {
            locator,
            value: BrowserRecipeValue::Literal { .. },
        } if locator_looks_sensitive(&locator) => {
            validate_locator(&locator)?;
            Ok(PendingRecordingAction::SecretType(locator))
        }
        BrowserRecipeAction::Upload { locator, .. } => {
            validate_locator(&locator)?;
            Ok(PendingRecordingAction::FileUpload(locator))
        }
        action => {
            let json =
                serde_json::to_string(&action).map_err(|_| BrowserRecordingError::InvalidAction)?;
            if redact_browser_text(&json) != json {
                return Err(BrowserRecordingError::InvalidAction);
            }
            validate_wire_node(&action)?;
            Ok(PendingRecordingAction::Recipe(action))
        }
    }
}

fn action_has_input_reference(action: &BrowserRecipeAction) -> bool {
    match action {
        BrowserRecipeAction::Navigate { url } => value_has_input_reference(url),
        BrowserRecipeAction::Type { value, .. } => value_has_input_reference(value),
        BrowserRecipeAction::Select { values, .. } => values.iter().any(value_has_input_reference),
        BrowserRecipeAction::Keypress { key, .. } => value_has_input_reference(key),
        BrowserRecipeAction::Upload { file, .. } => value_has_input_reference(file),
        BrowserRecipeAction::Wait { condition } => wait_has_input_reference(condition),
        BrowserRecipeAction::Click { .. }
        | BrowserRecipeAction::Hover { .. }
        | BrowserRecipeAction::Focus { .. }
        | BrowserRecipeAction::Clear { .. }
        | BrowserRecipeAction::Scroll { .. }
        | BrowserRecipeAction::DragDrop { .. }
        | BrowserRecipeAction::Download { .. }
        | BrowserRecipeAction::Screenshot { .. } => false,
    }
}

fn wait_has_input_reference(wait: &BrowserRecipeWait) -> bool {
    match wait {
        BrowserRecipeWait::Url { value, .. }
        | BrowserRecipeWait::TextPresent { value, .. }
        | BrowserRecipeWait::TextAbsent { value, .. } => value_has_input_reference(value),
        BrowserRecipeWait::Duration { .. }
        | BrowserRecipeWait::Load { .. }
        | BrowserRecipeWait::NetworkIdle { .. }
        | BrowserRecipeWait::ElementPresent { .. }
        | BrowserRecipeWait::ElementVisible { .. }
        | BrowserRecipeWait::ElementHidden { .. } => false,
    }
}

fn assertion_has_input_reference(assertion: &BrowserRecipeAssertion) -> bool {
    match assertion {
        BrowserRecipeAssertion::Url { value, .. }
        | BrowserRecipeAssertion::Title { value, .. }
        | BrowserRecipeAssertion::Text { value, .. }
        | BrowserRecipeAssertion::Value { value, .. } => value_has_input_reference(value),
        BrowserRecipeAssertion::Element { .. } => false,
    }
}

fn value_has_input_reference(value: &BrowserRecipeValue) -> bool {
    matches!(value, BrowserRecipeValue::Input { .. })
}

fn locator_looks_sensitive(locator: &BrowserRecipeLocator) -> bool {
    locator
        .accessibility_name
        .iter()
        .chain(locator.test_id.iter())
        .chain(locator.css_selectors.iter())
        .any(|value| sensitive_name(value))
}

fn sensitive_name(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "authorization",
        "apikey",
        "privatekey",
        "cookie",
        "session",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn sanitize_recording_url(value: &str) -> Result<String, BrowserRecordingError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BrowserRecordingError::InvalidAction);
    }
    let (without_fragment, fragment) = value
        .split_once('#')
        .map_or((value, None), |(base, fragment)| (base, Some(fragment)));
    let (base, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(base, query)| {
            (base, Some(query))
        });
    let authority = base
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default();
    if authority.contains('@') || redact_browser_text(base) != base {
        return Err(BrowserRecordingError::InvalidAction);
    }

    let mut safe_query = Vec::new();
    if let Some(query) = query {
        for pair in query.split('&') {
            let decoded_pair = percent_decode_for_inspection(pair)?;
            let key = decoded_pair.split('=').next().unwrap_or_default();
            if !sensitive_name(key) && redact_browser_text(&decoded_pair) == decoded_pair {
                safe_query.push(pair);
            }
        }
    }
    let safe_fragment = match fragment {
        Some(fragment) => {
            let decoded = percent_decode_for_inspection(fragment)?;
            (!fragment_has_sensitive_key(&decoded) && redact_browser_text(&decoded) == decoded)
                .then_some(fragment)
        }
        None => None,
    };
    let mut sanitized = base.to_string();
    if !safe_query.is_empty() {
        sanitized.push('?');
        sanitized.push_str(&safe_query.join("&"));
    }
    if let Some(fragment) = safe_fragment {
        sanitized.push('#');
        sanitized.push_str(fragment);
    }
    validate_browser_url(&sanitized).map_err(|_| BrowserRecordingError::InvalidAction)?;
    if redact_browser_text(&sanitized) != sanitized {
        return Err(BrowserRecordingError::InvalidAction);
    }
    Ok(sanitized)
}

fn percent_decode_for_inspection(value: &str) -> Result<String, BrowserRecordingError> {
    validate_percent_encoding(value)?;
    let mut decoded = value.to_string();
    for _ in 0..MAX_RECORDING_PERCENT_DECODE_PASSES {
        let next = percent_decode_once(&decoded)?;
        if next == decoded {
            return Ok(decoded);
        }
        decoded = next;
    }
    if has_percent_triplet(&decoded) {
        return Err(BrowserRecordingError::InvalidAction);
    }
    Ok(decoded)
}

fn validate_percent_encoding(value: &str) -> Result<(), BrowserRecordingError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || hex_value(bytes[index + 1]).is_none()
                || hex_value(bytes[index + 2]).is_none()
            {
                return Err(BrowserRecordingError::InvalidAction);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn percent_decode_once(value: &str) -> Result<String, BrowserRecordingError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).map_err(|_| BrowserRecordingError::InvalidAction)
}

fn has_percent_triplet(value: &str) -> bool {
    value.as_bytes().windows(3).any(|window| {
        window[0] == b'%' && hex_value(window[1]).is_some() && hex_value(window[2]).is_some()
    })
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn fragment_has_sensitive_key(fragment: &str) -> bool {
    fragment.split(['?', '&', ';']).any(|component| {
        component
            .split_once(['=', ':'])
            .is_some_and(|(key, _)| sensitive_name(key))
    })
}
