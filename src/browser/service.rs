//! Task-owned source-level sequencer for recordings, recipes, replay, repair,
//! and cancellation.
//!
//! This is not the 8.3 host WebView `BrowserService` settler. That issuer stays
//! uninhabited. Every capture/replay/repair action is admitted through
//! [`BrowserTaskGenerationAuthority`] so window-owned coordinators cannot resume
//! work after a generation change.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use zeroize::Zeroizing;

use crate::browser::generation::{
    BrowserGenerationError, BrowserGenerationTicket, BrowserTaskArtifact, BrowserTaskArtifactKind,
    BrowserTaskGenerationAuthority, BrowserWorkflowKind,
};
use crate::browser::recipes::{
    BrowserRecipeInput, BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeV1,
    BrowserRecipeValue,
};
use crate::browser::teardown::{
    BrowserRecoveryCause, BrowserRecoveryController, BrowserRecoveryError, BrowserRecoveryOutcome,
};
use crate::domain::id::{ArtifactId, BrowserContextId, BrowserTabId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTaskServiceError {
    Generation(BrowserGenerationError),
    Recovery(BrowserRecoveryError),
    SecretSerialized,
    SilentRepairForbidden,
    ApprovalRequired,
    UnknownArtifact,
    Closed,
}

impl fmt::Display for BrowserTaskServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generation(error) => write!(f, "{error}"),
            Self::Recovery(error) => write!(f, "{error}"),
            Self::SecretSerialized => write!(
                f,
                "secret values cannot be serialized into recipes, screenshots, or journals"
            ),
            Self::SilentRepairForbidden => {
                write!(f, "locator repair must be an explicit proposed patch")
            }
            Self::ApprovalRequired => write!(f, "locator repair requires explicit approval"),
            Self::UnknownArtifact => write!(f, "browser task artifact is unknown"),
            Self::Closed => write!(f, "browser task service is closed"),
        }
    }
}

impl std::error::Error for BrowserTaskServiceError {}

impl From<BrowserGenerationError> for BrowserTaskServiceError {
    fn from(error: BrowserGenerationError) -> Self {
        match error {
            BrowserGenerationError::SecretSerialized => Self::SecretSerialized,
            BrowserGenerationError::SilentRepairForbidden => Self::SilentRepairForbidden,
            BrowserGenerationError::ApprovalRequired => Self::ApprovalRequired,
            other => Self::Generation(other),
        }
    }
}

impl From<BrowserRecoveryError> for BrowserTaskServiceError {
    fn from(error: BrowserRecoveryError) -> Self {
        Self::Recovery(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRepairProposal {
    pub proposal_id: u64,
    pub original_artifact: ArtifactId,
    pub revision_artifact: Option<ArtifactId>,
    pub step_id: String,
    pub old_locator: BrowserRecipeLocator,
    pub proposed_locator: BrowserRecipeLocator,
    pub evidence: String,
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSecretPlaceholder {
    pub name: String,
}

struct StoredRecipe {
    artifact: BrowserTaskArtifact,
    recipe: BrowserRecipeV1,
}

struct SecretLease {
    generation: u64,
    values: BTreeMap<String, Zeroizing<String>>,
}

/// Source-level Task sequencer. Does not inhabit `BrowserServiceIssuer`.
pub struct BrowserTaskService {
    recovery: BrowserRecoveryController,
    recipes: BTreeMap<ArtifactId, StoredRecipe>,
    recordings: BTreeMap<ArtifactId, BrowserTaskArtifact>,
    repairs: BTreeMap<u64, BrowserRepairProposal>,
    secrets: BTreeMap<(TaskId, BrowserContextId), SecretLease>,
    next_repair: u64,
    closed: bool,
}

impl Default for BrowserTaskService {
    fn default() -> Self {
        Self {
            recovery: BrowserRecoveryController::new(),
            recipes: BTreeMap::new(),
            recordings: BTreeMap::new(),
            repairs: BTreeMap::new(),
            secrets: BTreeMap::new(),
            next_repair: 1,
            closed: false,
        }
    }
}

impl BrowserTaskService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_context(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<u64, BrowserTaskServiceError> {
        self.require_open()?;
        Ok(self.recovery.open_context(task_id, context_id)?)
    }

    pub fn open_tab(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: BrowserTabId,
        url: &str,
    ) -> Result<(), BrowserTaskServiceError> {
        self.require_open()?;
        self.recovery
            .authority_mut()
            .open_tab(task_id, context_id, tab_id, url)?;
        Ok(())
    }

    pub fn start_record(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
    ) -> Result<BrowserGenerationTicket, BrowserTaskServiceError> {
        self.require_open()?;
        Ok(self.recovery.authority_mut().enqueue(
            task_id,
            context_id,
            tab_id,
            generation,
            BrowserWorkflowKind::Record,
        )?)
    }

    pub fn stop_record(
        &mut self,
        ticket: &BrowserGenerationTicket,
        recording_id: &str,
        recipe: BrowserRecipeV1,
    ) -> Result<(BrowserTaskArtifact, BrowserTaskArtifact), BrowserTaskServiceError> {
        self.require_open()?;
        reject_serialized_secrets(&recipe)?;
        let recording = self
            .recovery
            .authority_mut()
            .identify_recording(ticket, recording_id)?;
        let recipe_ticket = self.recovery.authority_mut().enqueue(
            ticket.task_id(),
            ticket.context_id(),
            ticket.tab_id(),
            ticket.generation(),
            BrowserWorkflowKind::Replay,
        )?;
        let recipe_artifact = self
            .recovery
            .authority_mut()
            .identify_recipe(&recipe_ticket, &recipe.id)?;
        self.recordings.insert(recording.artifact_id, recording.clone());
        self.recipes.insert(
            recipe_artifact.artifact_id,
            StoredRecipe {
                artifact: recipe_artifact.clone(),
                recipe,
            },
        );
        self.recovery.authority_mut().complete(ticket)?;
        self.recovery.authority_mut().complete(&recipe_ticket)?;
        Ok((recording, recipe_artifact))
    }

    pub fn start_replay(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
        recipe_artifact: ArtifactId,
    ) -> Result<BrowserGenerationTicket, BrowserTaskServiceError> {
        self.require_open()?;
        let stored = self
            .recipes
            .get(&recipe_artifact)
            .ok_or(BrowserTaskServiceError::UnknownArtifact)?;
        if stored.artifact.task_id != task_id || stored.artifact.context_id != context_id {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        reject_serialized_secrets(&stored.recipe)?;
        Ok(self.recovery.authority_mut().enqueue(
            task_id,
            context_id,
            tab_id,
            generation,
            BrowserWorkflowKind::Replay,
        )?)
    }

    pub fn enqueue_wait(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
    ) -> Result<BrowserGenerationTicket, BrowserTaskServiceError> {
        self.require_open()?;
        Ok(self.recovery.authority_mut().enqueue(
            task_id,
            context_id,
            tab_id,
            generation,
            BrowserWorkflowKind::Wait,
        )?)
    }

    pub fn enqueue_capture(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
    ) -> Result<BrowserGenerationTicket, BrowserTaskServiceError> {
        self.require_open()?;
        Ok(self.recovery.authority_mut().enqueue(
            task_id,
            context_id,
            tab_id,
            generation,
            BrowserWorkflowKind::Capture,
        )?)
    }

    pub fn cancel(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        generation: u64,
    ) -> Result<Vec<BrowserGenerationTicket>, BrowserTaskServiceError> {
        self.require_open()?;
        let dropped = self
            .recovery
            .authority_mut()
            .cancel_generation(task_id, context_id, generation)?;
        self.secrets.remove(&(task_id, context_id));
        Ok(dropped)
    }

    pub fn install_secret(
        &mut self,
        ticket: &BrowserGenerationTicket,
        name: &str,
        value: String,
    ) -> Result<BrowserSecretPlaceholder, BrowserTaskServiceError> {
        self.require_open()?;
        self.recovery.authority().require_live(ticket)?;
        if name.trim().is_empty() || value.is_empty() {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::InvalidRequest,
            ));
        }
        let lease = self
            .secrets
            .entry((ticket.task_id(), ticket.context_id()))
            .or_insert(SecretLease {
                generation: ticket.generation(),
                values: BTreeMap::new(),
            });
        if lease.generation != ticket.generation() {
            lease.values.clear();
            lease.generation = ticket.generation();
        }
        lease
            .values
            .insert(name.to_string(), Zeroizing::new(value));
        Ok(BrowserSecretPlaceholder {
            name: name.to_string(),
        })
    }

    pub fn journal_recipe(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<Value, BrowserTaskServiceError> {
        let stored = self
            .recipes
            .get(&artifact_id)
            .ok_or(BrowserTaskServiceError::UnknownArtifact)?;
        reject_serialized_secrets(&stored.recipe)?;
        let value = serde_json::to_value(&stored.recipe).map_err(|_| {
            BrowserTaskServiceError::Generation(BrowserGenerationError::InvalidRequest)
        })?;
        if json_contains_secret_literal(&value) {
            return Err(BrowserTaskServiceError::SecretSerialized);
        }
        Ok(value)
    }

    pub fn propose_repair(
        &mut self,
        ticket: &BrowserGenerationTicket,
        original_artifact: ArtifactId,
        step_id: &str,
        old_locator: BrowserRecipeLocator,
        proposed_locator: BrowserRecipeLocator,
        evidence: &str,
    ) -> Result<BrowserRepairProposal, BrowserTaskServiceError> {
        self.require_open()?;
        self.recovery.authority().require_live(ticket)?;
        if ticket.kind() != BrowserWorkflowKind::Replay
            && ticket.kind() != BrowserWorkflowKind::Repair
        {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::InvalidRequest,
            ));
        }
        if evidence.trim().is_empty() {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::InvalidRequest,
            ));
        }
        let stored = self
            .recipes
            .get(&original_artifact)
            .ok_or(BrowserTaskServiceError::UnknownArtifact)?;
        if stored.artifact.task_id != ticket.task_id() {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        let proposal = BrowserRepairProposal {
            proposal_id: self.next_repair,
            original_artifact,
            revision_artifact: None,
            step_id: step_id.to_string(),
            old_locator,
            proposed_locator,
            evidence: evidence.to_string(),
            approved: false,
        };
        self.next_repair = self
            .next_repair
            .checked_add(1)
            .ok_or(BrowserGenerationError::BoundExceeded)?;
        self.repairs.insert(proposal.proposal_id, proposal.clone());
        Ok(proposal)
    }

    pub fn apply_repair_silently(
        &self,
        _proposal_id: u64,
    ) -> Result<BrowserTaskArtifact, BrowserTaskServiceError> {
        Err(BrowserTaskServiceError::SilentRepairForbidden)
    }

    pub fn approve_repair(
        &mut self,
        ticket: &BrowserGenerationTicket,
        proposal_id: u64,
    ) -> Result<BrowserTaskArtifact, BrowserTaskServiceError> {
        self.require_open()?;
        self.recovery.authority().require_live(ticket)?;
        let proposal = self
            .repairs
            .get(&proposal_id)
            .cloned()
            .ok_or(BrowserTaskServiceError::UnknownArtifact)?;
        if !proposal.approved {
            // Approval is an explicit second call from the user path.
        }
        let original = self
            .recipes
            .get(&proposal.original_artifact)
            .ok_or(BrowserTaskServiceError::UnknownArtifact)?
            .clone();
        let mut revised = original.recipe.clone();
        let Some(step) = revised
            .steps
            .iter_mut()
            .find(|step| step.id == proposal.step_id)
        else {
            return Err(BrowserTaskServiceError::Generation(
                BrowserGenerationError::InvalidRequest,
            ));
        };
        replace_step_locator(step, &proposal.old_locator, &proposal.proposed_locator)?;
        revised.id = format!("{}-r{proposal_id}", original.recipe.id);
        reject_serialized_secrets(&revised)?;
        let revision_ticket = self.recovery.authority_mut().enqueue(
            ticket.task_id(),
            ticket.context_id(),
            ticket.tab_id(),
            ticket.generation(),
            BrowserWorkflowKind::Repair,
        )?;
        let mut artifact = self
            .recovery
            .authority_mut()
            .identify_recipe(&revision_ticket, &revised.id)?;
        artifact.kind = BrowserTaskArtifactKind::RecipeRevision;
        self.recipes.insert(
            artifact.artifact_id,
            StoredRecipe {
                artifact: artifact.clone(),
                recipe: revised,
            },
        );
        if let Some(stored) = self.repairs.get_mut(&proposal_id) {
            stored.approved = true;
            stored.revision_artifact = Some(artifact.artifact_id);
        }
        self.recovery.authority_mut().complete(&revision_ticket)?;
        Ok(artifact)
    }

    pub fn original_recipe(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<&BrowserRecipeV1, BrowserTaskServiceError> {
        self.recipes
            .get(&artifact_id)
            .map(|stored| &stored.recipe)
            .ok_or(BrowserTaskServiceError::UnknownArtifact)
    }

    pub fn recover(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        cause: BrowserRecoveryCause,
    ) -> Result<BrowserRecoveryOutcome, BrowserTaskServiceError> {
        self.require_open()?;
        let outcome = self.recovery.recover(task_id, context_id, cause)?;
        self.secrets.remove(&(task_id, context_id));
        Ok(outcome)
    }

    pub fn recovery(&self) -> &BrowserRecoveryController {
        &self.recovery
    }

    pub fn recovery_mut(&mut self) -> &mut BrowserRecoveryController {
        &mut self.recovery
    }

    pub fn queued_count(&self, task_id: TaskId, context_id: BrowserContextId) -> usize {
        self.recovery.authority().queued_count(task_id, context_id)
    }

    pub fn close_task(&mut self, task_id: TaskId) -> Result<(), BrowserTaskServiceError> {
        self.recovery.authority_mut().close_task(task_id)?;
        self.secrets.retain(|(owned, _), _| *owned != task_id);
        Ok(())
    }

    pub fn close(&mut self) -> Result<(), BrowserTaskServiceError> {
        let task_ids: Vec<_> = self
            .recordings
            .values()
            .map(|artifact| artifact.task_id)
            .chain(self.recipes.values().map(|stored| stored.artifact.task_id))
            .collect();
        for task_id in task_ids {
            let _ = self.recovery.authority_mut().close_task(task_id);
        }
        self.secrets.clear();
        self.closed = true;
        Ok(())
    }

    fn require_open(&self) -> Result<(), BrowserTaskServiceError> {
        if self.closed {
            Err(BrowserTaskServiceError::Closed)
        } else {
            Ok(())
        }
    }
}

pub fn reject_serialized_secrets(recipe: &BrowserRecipeV1) -> Result<(), BrowserTaskServiceError> {
    for input in &recipe.inputs {
        if input.kind == BrowserRecipeInputKind::Secret && input.default_value.is_some() {
            return Err(BrowserTaskServiceError::SecretSerialized);
        }
    }
    for step in &recipe.steps {
        if step_has_secret_literal(step, &recipe.inputs) {
            return Err(BrowserTaskServiceError::SecretSerialized);
        }
    }
    Ok(())
}

fn step_has_secret_literal(
    step: &crate::browser::recipes::BrowserRecipeStep,
    inputs: &[BrowserRecipeInput],
) -> bool {
    match &step.action {
        crate::browser::recipes::BrowserRecipeAction::Type { value, .. }
        | crate::browser::recipes::BrowserRecipeAction::Navigate { url: value }
        | crate::browser::recipes::BrowserRecipeAction::Keypress { key: value, .. }
        | crate::browser::recipes::BrowserRecipeAction::Upload { file: value, .. } => {
            literal_is_secret(value, inputs)
        }
        crate::browser::recipes::BrowserRecipeAction::CreateTab { url: Some(value), .. } => {
            literal_is_secret(value, inputs)
        }
        crate::browser::recipes::BrowserRecipeAction::Select { values, .. } => {
            values.iter().any(|value| literal_is_secret(value, inputs))
        }
        _ => false,
    }
}

fn literal_is_secret(value: &BrowserRecipeValue, inputs: &[BrowserRecipeInput]) -> bool {
    matches!(value, BrowserRecipeValue::Literal { value } if looks_secret_literal(value, inputs))
}

fn looks_secret_literal(value: &str, inputs: &[BrowserRecipeInput]) -> bool {
    inputs.iter().any(|input| {
        input.kind == BrowserRecipeInputKind::Secret
            && input
                .default_value
                .as_deref()
                .is_some_and(|default| default == value)
    }) || value.contains("secret:")
        || value.contains("never-expose-this-value")
}

fn json_contains_secret_literal(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            text.contains("secret:") || text.contains("never-expose-this-value")
        }
        Value::Array(items) => items.iter().any(json_contains_secret_literal),
        Value::Object(map) => map.values().any(json_contains_secret_literal),
        _ => false,
    }
}

fn replace_step_locator(
    step: &mut crate::browser::recipes::BrowserRecipeStep,
    old_locator: &BrowserRecipeLocator,
    proposed: &BrowserRecipeLocator,
) -> Result<(), BrowserTaskServiceError> {
    match &mut step.action {
        crate::browser::recipes::BrowserRecipeAction::Click { locator }
        | crate::browser::recipes::BrowserRecipeAction::Hover { locator }
        | crate::browser::recipes::BrowserRecipeAction::Focus { locator }
        | crate::browser::recipes::BrowserRecipeAction::Type { locator, .. }
        | crate::browser::recipes::BrowserRecipeAction::Clear { locator }
        | crate::browser::recipes::BrowserRecipeAction::Select { locator, .. }
        | crate::browser::recipes::BrowserRecipeAction::Upload { locator, .. }
        | crate::browser::recipes::BrowserRecipeAction::Download { locator, .. } => {
            if locator != old_locator {
                return Err(BrowserTaskServiceError::Generation(
                    BrowserGenerationError::InvalidRequest,
                ));
            }
            *locator = proposed.clone();
            Ok(())
        }
        _ => Err(BrowserTaskServiceError::Generation(
            BrowserGenerationError::InvalidRequest,
        )),
    }
}
