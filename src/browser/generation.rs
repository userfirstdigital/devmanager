//! Task-owned generation authority for recordings, recipes, replay, repair,
//! and cancellation.
//!
//! Window/workspace coordinators cannot mint tickets. Every queued action is
//! fenced by Task, context, and the live generation. Advancing a generation
//! drops the prior queue so no cancelled wait/capture/replay resumes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::domain::browser::{
    BrowserAction, BrowserBook, BrowserContractError, BrowserHealth, BrowserHostOutcome,
    BrowserRequest, BrowserSettlement, BrowserTabKind,
};
use crate::domain::id::{
    ArtifactId, BrowserContextId, BrowserRequestId, BrowserTabId, TaskId,
};

pub const MAX_BROWSER_GENERATION_QUEUE: usize = 32;
pub const MAX_BROWSER_GENERATION_CONTEXTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserGenerationError {
    CrossTask,
    GenerationMismatch,
    Closed,
    BoundExceeded,
    StaleOperation,
    Cancelled,
    InvalidRequest,
    SecretSerialized,
    SilentRepairForbidden,
    ApprovalRequired,
    QueueOrphan,
    HostEffectUnavailable,
}

impl fmt::Display for BrowserGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CrossTask => write!(f, "browser generation belongs to a different Task"),
            Self::GenerationMismatch => write!(f, "browser generation does not match"),
            Self::Closed => write!(f, "closed Task or context cannot admit browser work"),
            Self::BoundExceeded => write!(f, "browser generation queue bound exceeded"),
            Self::StaleOperation => write!(f, "browser operation is stale for this generation"),
            Self::Cancelled => write!(f, "browser operation was cancelled"),
            Self::InvalidRequest => write!(f, "browser generation request is not admissible"),
            Self::SecretSerialized => {
                write!(f, "secret values cannot be serialized into recipes or journals")
            }
            Self::SilentRepairForbidden => {
                write!(f, "locator repair requires an explicit proposed patch")
            }
            Self::ApprovalRequired => write!(f, "locator repair requires explicit approval"),
            Self::QueueOrphan => write!(f, "browser generation queue retained orphaned work"),
            Self::HostEffectUnavailable => {
                write!(f, "browser host effect remains a typed HOLD")
            }
        }
    }
}

impl std::error::Error for BrowserGenerationError {}

impl From<BrowserContractError> for BrowserGenerationError {
    fn from(error: BrowserContractError) -> Self {
        match error {
            BrowserContractError::CrossTask => Self::CrossTask,
            BrowserContractError::GenerationMismatch => Self::GenerationMismatch,
            BrowserContractError::ClosedTask => Self::Closed,
            BrowserContractError::BoundExceeded => Self::BoundExceeded,
            BrowserContractError::HostEffectUnavailable => Self::HostEffectUnavailable,
            BrowserContractError::IdempotencyConflict | BrowserContractError::InvalidRequest => {
                Self::InvalidRequest
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BrowserWorkflowKind {
    Record,
    Replay,
    Repair,
    Cancel,
    Recover,
    Capture,
    Wait,
    Navigate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserGenerationTicket {
    request_id: BrowserRequestId,
    task_id: TaskId,
    context_id: BrowserContextId,
    tab_id: Option<BrowserTabId>,
    generation: u64,
    kind: BrowserWorkflowKind,
}

impl BrowserGenerationTicket {
    pub fn request_id(&self) -> BrowserRequestId {
        self.request_id
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn context_id(&self) -> BrowserContextId {
        self.context_id
    }

    pub fn tab_id(&self) -> Option<BrowserTabId> {
        self.tab_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn kind(&self) -> BrowserWorkflowKind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserTaskArtifact {
    pub artifact_id: ArtifactId,
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub generation: u64,
    pub identity: String,
    pub kind: BrowserTaskArtifactKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTaskArtifactKind {
    Recording,
    Recipe,
    RecipeRevision,
}

#[derive(Debug, Clone, Default)]
struct GenerationQueue {
    generation: u64,
    active: Option<BrowserGenerationTicket>,
    queued: VecDeque<BrowserGenerationTicket>,
    cancelled: BTreeSet<u64>,
    cancelled_requests: BTreeSet<BrowserRequestId>,
}

impl GenerationQueue {
    fn is_empty(&self) -> bool {
        self.active.is_none() && self.queued.is_empty()
    }

    fn len(&self) -> usize {
        self.queued.len() + usize::from(self.active.is_some())
    }
}

/// Task-owned generation fence. This is the only mint for workflow tickets.
#[derive(Debug)]
pub struct BrowserTaskGenerationAuthority {
    book: BrowserBook,
    queues: BTreeMap<(TaskId, BrowserContextId), GenerationQueue>,
}

impl Default for BrowserTaskGenerationAuthority {
    fn default() -> Self {
        Self {
            book: BrowserBook::new(),
            queues: BTreeMap::new(),
        }
    }
}

impl BrowserTaskGenerationAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_task(&mut self, task_id: TaskId) -> Result<(), BrowserGenerationError> {
        self.book.open_task(task_id).map_err(Into::into)
    }

    pub fn create_context(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<u64, BrowserGenerationError> {
        if self.queues.len() >= MAX_BROWSER_GENERATION_CONTEXTS
            && !self.queues.contains_key(&(task_id, context_id))
        {
            return Err(BrowserGenerationError::BoundExceeded);
        }
        let request = BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            action: BrowserAction::CreateContext,
        };
        self.book.admit(&request)?;
        self.queues.insert(
            (task_id, context_id),
            GenerationQueue {
                generation: 1,
                ..GenerationQueue::default()
            },
        );
        Ok(1)
    }

    pub fn open_tab(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: BrowserTabId,
        url: &str,
    ) -> Result<(), BrowserGenerationError> {
        let generation = self.live_generation(task_id, context_id)?;
        let request = BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation,
            action: BrowserAction::OpenTab {
                url: url.to_string(),
                kind: BrowserTabKind::Page,
            },
        };
        self.book.admit(&request)?;
        Ok(())
    }

    pub fn live_generation(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<u64, BrowserGenerationError> {
        let view = self
            .book
            .context_view(context_id)
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        if view.task_id != task_id {
            return Err(BrowserGenerationError::CrossTask);
        }
        if view.closed {
            return Err(BrowserGenerationError::Closed);
        }
        Ok(view.generation)
    }

    pub fn health(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<BrowserHealth, BrowserGenerationError> {
        let view = self
            .book
            .context_view(context_id)
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        if view.task_id != task_id {
            return Err(BrowserGenerationError::CrossTask);
        }
        Ok(view.health)
    }

    pub fn enqueue(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        tab_id: Option<BrowserTabId>,
        generation: u64,
        kind: BrowserWorkflowKind,
    ) -> Result<BrowserGenerationTicket, BrowserGenerationError> {
        let live = self.live_generation(task_id, context_id)?;
        if generation != live {
            return Err(BrowserGenerationError::GenerationMismatch);
        }
        let queue = self
            .queues
            .get_mut(&(task_id, context_id))
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        if queue.len() >= MAX_BROWSER_GENERATION_QUEUE {
            return Err(BrowserGenerationError::BoundExceeded);
        }
        let ticket = BrowserGenerationTicket {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id,
            generation,
            kind,
        };
        if queue.active.is_none() {
            queue.active = Some(ticket.clone());
        } else {
            queue.queued.push_back(ticket.clone());
        }
        Ok(ticket)
    }

    pub fn require_live(
        &self,
        ticket: &BrowserGenerationTicket,
    ) -> Result<(), BrowserGenerationError> {
        let live = self.live_generation(ticket.task_id, ticket.context_id)?;
        if ticket.generation != live {
            return Err(BrowserGenerationError::StaleOperation);
        }
        let queue = self
            .queues
            .get(&(ticket.task_id, ticket.context_id))
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        if queue.cancelled.contains(&ticket.generation)
            || queue
                .cancelled_requests
                .contains(&ticket.request_id)
        {
            return Err(BrowserGenerationError::Cancelled);
        }
        let known = queue
            .active
            .as_ref()
            .is_some_and(|active| active.request_id == ticket.request_id)
            || queue
                .queued
                .iter()
                .any(|queued| queued.request_id == ticket.request_id);
        if !known {
            return Err(BrowserGenerationError::StaleOperation);
        }
        Ok(())
    }

    pub fn complete(
        &mut self,
        ticket: &BrowserGenerationTicket,
    ) -> Result<Option<BrowserGenerationTicket>, BrowserGenerationError> {
        self.require_live(ticket)?;
        let queue = self
            .queues
            .get_mut(&(ticket.task_id, ticket.context_id))
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        match queue.active.as_ref() {
            Some(active) if active.request_id == ticket.request_id => {
                queue.active = queue.queued.pop_front();
                Ok(queue.active.clone())
            }
            _ => Err(BrowserGenerationError::StaleOperation),
        }
    }

    /// Cancel the live generation. Queued work is dropped and cannot resume
    /// after a later generation is minted.
    pub fn cancel_generation(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        generation: u64,
    ) -> Result<Vec<BrowserGenerationTicket>, BrowserGenerationError> {
        let live = self.live_generation(task_id, context_id)?;
        if generation != live {
            return Err(BrowserGenerationError::GenerationMismatch);
        }
        let request = BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: None,
            generation,
            action: BrowserAction::Cancel,
        };
        self.book.admit(&request)?;
        let dropped = self.drop_queue(task_id, context_id, generation)?;
        Ok(dropped)
    }

    pub fn recover_generation(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        from_generation: u64,
    ) -> Result<u64, BrowserGenerationError> {
        let live = self.live_generation(task_id, context_id)?;
        if from_generation != live {
            return Err(BrowserGenerationError::GenerationMismatch);
        }
        let next = from_generation
            .checked_add(1)
            .ok_or(BrowserGenerationError::BoundExceeded)?;
        self.drop_queue(task_id, context_id, from_generation)?;
        let request = BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: None,
            generation: from_generation,
            action: BrowserAction::Recover,
        };
        self.book.admit(&request)?;
        self.book.settle(&BrowserHostOutcome {
            request_id: request.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: from_generation,
            settlement: BrowserSettlement::Recovered { generation: next },
        })?;
        if let Some(queue) = self.queues.get_mut(&(task_id, context_id)) {
            queue.generation = next;
            queue.cancelled.insert(from_generation);
            queue.cancelled_requests.clear();
            if !queue.is_empty() {
                return Err(BrowserGenerationError::QueueOrphan);
            }
        }
        Ok(next)
    }

    pub fn identify_recording(
        &mut self,
        ticket: &BrowserGenerationTicket,
        recording_id: &str,
    ) -> Result<BrowserTaskArtifact, BrowserGenerationError> {
        self.require_live(ticket)?;
        if ticket.kind != BrowserWorkflowKind::Record {
            return Err(BrowserGenerationError::InvalidRequest);
        }
        self.settle_identity(ticket, BrowserAction::Record, recording_id, BrowserTaskArtifactKind::Recording)
    }

    pub fn identify_recipe(
        &mut self,
        ticket: &BrowserGenerationTicket,
        recipe_id: &str,
    ) -> Result<BrowserTaskArtifact, BrowserGenerationError> {
        self.require_live(ticket)?;
        if ticket.kind != BrowserWorkflowKind::Replay && ticket.kind != BrowserWorkflowKind::Repair
        {
            return Err(BrowserGenerationError::InvalidRequest);
        }
        self.settle_identity(ticket, BrowserAction::Replay, recipe_id, BrowserTaskArtifactKind::Recipe)
    }

    pub fn queued_count(&self, task_id: TaskId, context_id: BrowserContextId) -> usize {
        self.queues
            .get(&(task_id, context_id))
            .map(GenerationQueue::len)
            .unwrap_or(0)
    }

    pub fn has_orphans(&self, task_id: TaskId, context_id: BrowserContextId) -> bool {
        self.queues
            .get(&(task_id, context_id))
            .is_some_and(|queue| !queue.is_empty() && queue.cancelled.contains(&queue.generation))
    }

    pub fn close_task(&mut self, task_id: TaskId) -> Result<(), BrowserGenerationError> {
        let keys: Vec<_> = self
            .queues
            .keys()
            .filter(|(owned, _)| *owned == task_id)
            .cloned()
            .collect();
        for (owned, context_id) in keys {
            if let Ok(generation) = self.live_generation(owned, context_id) {
                self.drop_queue(owned, context_id, generation)?;
            }
            self.queues.remove(&(owned, context_id));
        }
        self.book.close_task(task_id)?;
        if self
            .queues
            .iter()
            .any(|((owned, _), queue)| *owned == task_id && !queue.is_empty())
        {
            return Err(BrowserGenerationError::QueueOrphan);
        }
        Ok(())
    }

    pub fn book(&self) -> &BrowserBook {
        &self.book
    }

    fn drop_queue(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        generation: u64,
    ) -> Result<Vec<BrowserGenerationTicket>, BrowserGenerationError> {
        let queue = self
            .queues
            .get_mut(&(task_id, context_id))
            .ok_or(BrowserGenerationError::InvalidRequest)?;
        if queue.generation != generation {
            return Err(BrowserGenerationError::GenerationMismatch);
        }
        let mut dropped = Vec::new();
        if let Some(active) = queue.active.take() {
            dropped.push(active);
        }
        dropped.extend(queue.queued.drain(..));
        for ticket in &dropped {
            queue.cancelled_requests.insert(ticket.request_id);
        }
        if !queue.is_empty() {
            return Err(BrowserGenerationError::QueueOrphan);
        }
        Ok(dropped)
    }

    fn settle_identity(
        &mut self,
        ticket: &BrowserGenerationTicket,
        action: BrowserAction,
        identity: &str,
        kind: BrowserTaskArtifactKind,
    ) -> Result<BrowserTaskArtifact, BrowserGenerationError> {
        if identity.is_empty() || identity.len() > crate::domain::browser::MAX_BROWSER_IDENTITY_BYTES
        {
            return Err(BrowserGenerationError::InvalidRequest);
        }
        let request = BrowserRequest {
            request_id: ticket.request_id,
            task_id: ticket.task_id,
            context_id: ticket.context_id,
            tab_id: ticket.tab_id,
            generation: ticket.generation,
            action,
        };
        self.book.admit(&request)?;
        let settlement = match kind {
            BrowserTaskArtifactKind::Recording => BrowserSettlement::RecordingIdentified {
                recording_id: identity.to_string(),
            },
            BrowserTaskArtifactKind::Recipe | BrowserTaskArtifactKind::RecipeRevision => {
                BrowserSettlement::RecipeIdentified {
                    recipe_id: identity.to_string(),
                }
            }
        };
        self.book.settle(&BrowserHostOutcome {
            request_id: ticket.request_id,
            task_id: ticket.task_id,
            context_id: ticket.context_id,
            tab_id: ticket.tab_id,
            generation: ticket.generation,
            settlement,
        })?;
        let artifact_id = ArtifactId::new();
        let link = BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id: ticket.task_id,
            context_id: ticket.context_id,
            tab_id: None,
            generation: ticket.generation,
            action: BrowserAction::LinkArtifact { artifact_id },
        };
        self.book.admit(&link)?;
        Ok(BrowserTaskArtifact {
            artifact_id,
            task_id: ticket.task_id,
            context_id: ticket.context_id,
            generation: ticket.generation,
            identity: identity.to_string(),
            kind,
        })
    }
}
