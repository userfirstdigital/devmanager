//! Caller-driven initial synchronization and live durable subscription.

use std::collections::{HashSet, VecDeque};

use crate::client::connection::UnsolicitedServerMessage;
use crate::client::host_client::HostClient;
use crate::client::model::{ClientModel, ClientModelBuilder, ClientModelError, TaskInboxPreview};
use crate::domain::event::DomainEvent;
use crate::domain::id::{SnapshotId, SubscriptionId};
use crate::domain::query::QueryError;
use crate::domain::snapshot::SnapshotSection;
use crate::host::IpcError;
use crate::protocol::{Capability, StreamFrame};

const SNAPSHOT_SECTIONS: [SnapshotSection; 5] = [
    SnapshotSection::Tasks,
    SnapshotSection::AgentSessions,
    SnapshotSection::Artifacts,
    SnapshotSection::Resources,
    SnapshotSection::Operations,
];
const MAX_SEEN_EVENT_IDS: usize = 8_192;
const MAX_PENDING_REPLAY_EVENTS: usize = 8_192;

/// Explicit subscription lifecycle visible to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSubscriptionState {
    Pending,
    Ready,
    NeedsResync,
    Released,
}

/// One drained unsolicited update after successful apply / resync transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionUpdate {
    DurableEvent(DomainEvent),
    ResyncRequired {
        last_delivered_sequence: u64,
        newest_sequence: u64,
    },
    Stream(StreamFrame),
    ConversationDirty {
        subscription_id: SubscriptionId,
        task_id: crate::domain::TaskId,
        high_water: u64,
    },
}

#[derive(Debug)]
pub enum SubscriptionError {
    NotReady,
    NeedsResync,
    Released,
    Model(ClientModelError),
    /// Foreign unsolicited frame preserved for the caller; subscription needs resync.
    ForeignSubscription(UnsolicitedServerMessage),
    /// The caller failed to drain race-closing replay before the bounded
    /// handoff filled. Dropping history would make unread/replay state false,
    /// so synchronization must restart from a fresh authoritative snapshot.
    ReplayOverflow {
        limit: usize,
    },
    InvalidResync,
    Transport(IpcError),
    TransportAt {
        operation: String,
        error: IpcError,
    },
    Query(QueryError),
    IncompleteSnapshot,
    MissingCapabilities,
}

impl std::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady => write!(f, "subscription is not ready"),
            Self::NeedsResync => write!(f, "subscription requires resynchronization"),
            Self::Released => write!(f, "subscription was released"),
            Self::Model(error) => write!(f, "client model error: {error}"),
            Self::ForeignSubscription(_) => {
                write!(f, "unsolicited message for a foreign subscription")
            }
            Self::ReplayOverflow { limit } => {
                write!(f, "replay handoff exceeded bounded limit of {limit} events")
            }
            Self::InvalidResync => write!(f, "resync required fields are inconsistent"),
            Self::Transport(error) => write!(f, "subscription transport error: {error}"),
            Self::TransportAt { operation, error } => {
                write!(f, "subscription {operation} transport error: {error}")
            }
            Self::Query(error) => write!(f, "subscription query error: {error:?}"),
            Self::IncompleteSnapshot => write!(f, "snapshot synchronization was incomplete"),
            Self::MissingCapabilities => {
                write!(
                    f,
                    "host did not grant required snapshot/replay capabilities"
                )
            }
        }
    }
}

impl std::error::Error for SubscriptionError {}

impl From<ClientModelError> for SubscriptionError {
    fn from(error: ClientModelError) -> Self {
        Self::Model(error)
    }
}

/// Caller-driven subscription: no background consumer, queue, or timer.
#[derive(Debug)]
pub struct ClientSubscription {
    state: ClientSubscriptionState,
    subscription_id: Option<SubscriptionId>,
    model: Option<ClientModel>,
    snapshot_id: Option<SnapshotId>,
    seen_event_ids: HashSet<crate::domain::id::EventId>,
    seen_event_order: VecDeque<crate::domain::id::EventId>,
    pending_replay_events: VecDeque<DomainEvent>,
}

impl Default for ClientSubscription {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientSubscription {
    pub fn new() -> Self {
        Self {
            state: ClientSubscriptionState::Pending,
            subscription_id: None,
            model: None,
            snapshot_id: None,
            seen_event_ids: HashSet::new(),
            seen_event_order: VecDeque::new(),
            pending_replay_events: VecDeque::new(),
        }
    }

    pub fn state(&self) -> ClientSubscriptionState {
        self.state
    }

    pub fn subscription_id(&self) -> Option<SubscriptionId> {
        self.subscription_id
    }

    pub fn model(&self) -> Option<&ClientModel> {
        self.model.as_ref()
    }

    /// Drain events delivered by the snapshot race-closing replay. The caller
    /// owns presentation semantics (for example, unread state) and therefore
    /// must consume this bounded handoff after every successful synchronize.
    pub fn take_replay_events(&mut self) -> Vec<DomainEvent> {
        self.pending_replay_events.drain(..).collect()
    }

    /// Fetch a fully paginated Tasks snapshot preview on the one HostClient,
    /// release that exact snapshot, and return a [`TaskInboxPreview`] without
    /// installing it as the subscription's ClientModel.
    pub async fn preview_tasks(
        &mut self,
        client: &mut HostClient,
    ) -> Result<TaskInboxPreview, SubscriptionError> {
        // Fail closed before any release/wipe so a Ready canonical model stays
        // intact when Tasks preview is refused.
        if self.state == ClientSubscriptionState::Ready {
            return Err(SubscriptionError::NotReady);
        }
        if self.state == ClientSubscriptionState::Released {
            return Err(SubscriptionError::Released);
        }
        let granted = client.granted_capabilities();
        if !granted.contains(Capability::PagedSnapshots) {
            return Err(SubscriptionError::MissingCapabilities);
        }

        if let Err(error) = self.release_snapshot_if_owned(client).await {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(error);
        }

        let mut builder = ClientModelBuilder::new();
        let mut snapshot_id: Option<SnapshotId> = None;
        let mut through_sequence: Option<u64> = None;
        let mut resume_cursor = None;
        let mut section_started = false;
        let preview = loop {
            let requested_id = if section_started {
                let Some(id) = snapshot_id else {
                    return Err(SubscriptionError::IncompleteSnapshot);
                };
                Some(id)
            } else {
                snapshot_id
            };
            let page = match client
                .snapshot_page(SnapshotSection::Tasks, requested_id, resume_cursor.clone())
                .await
            {
                Ok(Ok(page)) => page,
                Ok(Err(error)) => {
                    self.state = ClientSubscriptionState::NeedsResync;
                    let _ = self.best_effort_cleanup(client).await;
                    return Err(SubscriptionError::Query(error));
                }
                Err(error) => {
                    self.state = ClientSubscriptionState::NeedsResync;
                    let _ = self.best_effort_cleanup(client).await;
                    return Err(SubscriptionError::Transport(error));
                }
            };
            self.snapshot_id = Some(page.snapshot_id);
            match snapshot_id {
                Some(expected) if expected != page.snapshot_id => {
                    self.state = ClientSubscriptionState::NeedsResync;
                    let _ = self.best_effort_cleanup(client).await;
                    return Err(SubscriptionError::IncompleteSnapshot);
                }
                Some(_) => {}
                None => snapshot_id = Some(page.snapshot_id),
            }
            match through_sequence {
                Some(expected) if expected != page.through_sequence => {
                    self.state = ClientSubscriptionState::NeedsResync;
                    let _ = self.best_effort_cleanup(client).await;
                    return Err(SubscriptionError::IncompleteSnapshot);
                }
                Some(_) => {}
                None => through_sequence = Some(page.through_sequence),
            }
            section_started = true;
            let next = page.next_cursor.clone();
            if let Err(error) = builder.ingest_page(page) {
                self.state = ClientSubscriptionState::NeedsResync;
                let _ = self.best_effort_cleanup(client).await;
                return Err(error.into());
            }
            match next {
                Some(cursor) => resume_cursor = Some(cursor),
                None => {
                    let preview = match builder.finish_tasks_preview() {
                        Ok(preview) => preview,
                        Err(error) => {
                            self.state = ClientSubscriptionState::NeedsResync;
                            let _ = self.best_effort_cleanup(client).await;
                            return Err(error.into());
                        }
                    };
                    break preview;
                }
            }
        };

        if self.snapshot_id.is_some() {
            if let Err(error) = self.release_snapshot_if_owned(client).await {
                self.state = ClientSubscriptionState::NeedsResync;
                return Err(error);
            }
        }
        self.model = None;
        if self.state == ClientSubscriptionState::Ready {
            self.state = ClientSubscriptionState::Pending;
        }
        let through = through_sequence.ok_or(SubscriptionError::IncompleteSnapshot)?;
        if preview.through_sequence() != through {
            return Err(SubscriptionError::IncompleteSnapshot);
        }
        Ok(preview)
    }

    /// Snapshot through N → open replay after N → release snapshot → apply frozen
    /// replay → retain live subscription metadata. Caller-driven only.
    pub async fn synchronize(&mut self, client: &mut HostClient) -> Result<(), SubscriptionError> {
        if self.state == ClientSubscriptionState::Ready {
            return Err(SubscriptionError::NotReady);
        }
        if self.state == ClientSubscriptionState::Released {
            return Err(SubscriptionError::Released);
        }
        let granted = client.granted_capabilities();
        if !granted.contains(Capability::PagedSnapshots)
            || !granted.contains(Capability::EventReplay)
        {
            return Err(SubscriptionError::MissingCapabilities);
        }

        // A transport failure or a server resync request leaves the prior
        // subscription generation unusable. Release each owned remote handle
        // before opening a replacement. Failed release remains visible and
        // retains the exact id so the next caller can retry instead of leaking
        // a live replay/snapshot session.
        self.state = ClientSubscriptionState::Pending;
        if let Err(error) = self.release_event_replay_if_owned(client).await {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(error);
        }
        if let Err(error) = self.release_snapshot_if_owned(client).await {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(error);
        }
        self.seen_event_ids.clear();
        self.seen_event_order.clear();
        self.pending_replay_events.clear();

        match self.synchronize_inner(client).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.state = ClientSubscriptionState::NeedsResync;
                self.pending_replay_events.clear();
                if let Some(cleanup_error) = self.best_effort_cleanup(client).await {
                    eprintln!(
                        "devmanager client subscription cleanup after synchronize failure also failed: {cleanup_error}"
                    );
                }
                Err(error)
            }
        }
    }

    async fn synchronize_inner(
        &mut self,
        client: &mut HostClient,
    ) -> Result<(), SubscriptionError> {
        let mut builder = ClientModelBuilder::new();
        let mut snapshot_id: Option<SnapshotId> = None;
        let mut through_sequence: Option<u64> = None;

        for section in SNAPSHOT_SECTIONS {
            let mut resume_cursor = None;
            let mut section_started = false;
            loop {
                let requested_id = if section_started {
                    let Some(id) = snapshot_id else {
                        return Err(SubscriptionError::IncompleteSnapshot);
                    };
                    Some(id)
                } else {
                    snapshot_id
                };
                // First absolute open uses (None, None). Later sections use
                // (Some(id), None). Continuations use (Some(id), Some(cursor)).
                let page = match client
                    .snapshot_page(section, requested_id, resume_cursor.clone())
                    .await
                {
                    Ok(Ok(page)) => page,
                    Ok(Err(error)) => return Err(SubscriptionError::Query(error)),
                    Err(error) => {
                        return Err(SubscriptionError::TransportAt {
                            operation: format!("snapshot {section:?}"),
                            error,
                        });
                    }
                };
                self.snapshot_id = Some(page.snapshot_id);
                match snapshot_id {
                    Some(expected) if expected != page.snapshot_id => {
                        return Err(SubscriptionError::IncompleteSnapshot);
                    }
                    Some(_) => {}
                    None => snapshot_id = Some(page.snapshot_id),
                }
                match through_sequence {
                    Some(expected) if expected != page.through_sequence => {
                        return Err(SubscriptionError::IncompleteSnapshot);
                    }
                    Some(_) => {}
                    None => through_sequence = Some(page.through_sequence),
                }
                section_started = true;
                let next = page.next_cursor.clone();
                builder.ingest_page(page)?;
                match next {
                    Some(cursor) => resume_cursor = Some(cursor),
                    None => break,
                }
            }
        }

        let model = builder.finish()?;
        let through = through_sequence.ok_or(SubscriptionError::IncompleteSnapshot)?;
        if model.last_applied_sequence() != through {
            return Err(SubscriptionError::IncompleteSnapshot);
        }

        let open = match client.open_event_replay(through).await {
            Ok(Ok(batch)) => batch,
            Ok(Err(error)) => return Err(SubscriptionError::Query(error)),
            Err(error) => {
                return Err(SubscriptionError::TransportAt {
                    operation: "open replay".to_string(),
                    error,
                });
            }
        };
        self.subscription_id = Some(open.subscription_id);

        if self.snapshot_id.is_some() {
            self.release_snapshot_if_owned(client).await?;
        }

        let mut model = model;
        let mut batch = open;
        loop {
            let next = batch.page.next_cursor.clone();
            for event in &batch.page.events {
                self.queue_replay_event(event.clone())?;
            }
            model.apply_replay_page(&batch.page)?;
            let Some(cursor) = next else {
                break;
            };
            batch = match client
                .continue_event_replay(batch.subscription_id, cursor)
                .await
            {
                Ok(Ok(batch)) => batch,
                Ok(Err(error)) => return Err(SubscriptionError::Query(error)),
                Err(error) => {
                    return Err(SubscriptionError::TransportAt {
                        operation: "continue replay".to_string(),
                        error,
                    });
                }
            };
            if Some(batch.subscription_id) != self.subscription_id {
                return Err(SubscriptionError::IncompleteSnapshot);
            }
        }

        self.model = Some(model);
        self.state = ClientSubscriptionState::Ready;
        Ok(())
    }

    /// Drain one unsolicited message and apply it when the subscription is Ready.
    pub async fn recv_and_apply(
        &mut self,
        client: &HostClient,
    ) -> Result<SubscriptionUpdate, SubscriptionError> {
        match self.state {
            ClientSubscriptionState::Ready => {}
            ClientSubscriptionState::NeedsResync => return Err(SubscriptionError::NeedsResync),
            ClientSubscriptionState::Released => return Err(SubscriptionError::Released),
            ClientSubscriptionState::Pending => return Err(SubscriptionError::NotReady),
        }
        let message = match client.recv_unsolicited().await {
            Ok(message) => message,
            Err(error) => {
                self.observe_recv_transport_failure();
                return Err(SubscriptionError::Transport(error));
            }
        };
        self.handle_unsolicited_message(message)
    }

    /// Pure/internal seam: apply one already-received unsolicited message.
    pub fn handle_unsolicited_message(
        &mut self,
        message: UnsolicitedServerMessage,
    ) -> Result<SubscriptionUpdate, SubscriptionError> {
        match self.state {
            ClientSubscriptionState::Ready => {}
            ClientSubscriptionState::NeedsResync => return Err(SubscriptionError::NeedsResync),
            ClientSubscriptionState::Released => return Err(SubscriptionError::Released),
            ClientSubscriptionState::Pending => return Err(SubscriptionError::NotReady),
        }
        let subscription_id = self
            .subscription_id
            .ok_or(SubscriptionError::IncompleteSnapshot)?;
        match message {
            UnsolicitedServerMessage::DurableEvent {
                subscription_id: observed,
                event,
            } => {
                if observed != subscription_id {
                    self.state = ClientSubscriptionState::NeedsResync;
                    return Err(SubscriptionError::ForeignSubscription(
                        UnsolicitedServerMessage::DurableEvent {
                            subscription_id: observed,
                            event,
                        },
                    ));
                }
                if self.seen_event_ids.contains(&event.id) {
                    // An exact replay of a previously delivered event is
                    // idempotent. Keep the event visible to the caller so its
                    // durable cursor can remain monotonic without mutating the
                    // model a second time.
                    return Ok(SubscriptionUpdate::DurableEvent(event));
                }
                if event.sequence
                    <= self
                        .model
                        .as_ref()
                        .ok_or(SubscriptionError::IncompleteSnapshot)?
                        .last_applied_sequence()
                {
                    self.state = ClientSubscriptionState::NeedsResync;
                    return Err(SubscriptionError::Model(
                        ClientModelError::DuplicateOrRegression,
                    ));
                }
                self.remember_event_id(event.id);
                let model = self
                    .model
                    .as_mut()
                    .ok_or(SubscriptionError::IncompleteSnapshot)?;
                if let Err(error) = model.apply_event(&event) {
                    self.state = ClientSubscriptionState::NeedsResync;
                    return Err(SubscriptionError::Model(error));
                }
                Ok(SubscriptionUpdate::DurableEvent(event))
            }
            UnsolicitedServerMessage::ResyncRequired {
                subscription_id: observed,
                last_delivered_sequence,
                newest_sequence,
            } => {
                if observed != subscription_id {
                    self.state = ClientSubscriptionState::NeedsResync;
                    return Err(SubscriptionError::ForeignSubscription(
                        UnsolicitedServerMessage::ResyncRequired {
                            subscription_id: observed,
                            last_delivered_sequence,
                            newest_sequence,
                        },
                    ));
                }
                let model = self
                    .model
                    .as_ref()
                    .ok_or(SubscriptionError::IncompleteSnapshot)?;
                if newest_sequence < last_delivered_sequence
                    || last_delivered_sequence != model.last_applied_sequence()
                {
                    self.state = ClientSubscriptionState::NeedsResync;
                    return Err(SubscriptionError::InvalidResync);
                }
                self.state = ClientSubscriptionState::NeedsResync;
                Ok(SubscriptionUpdate::ResyncRequired {
                    last_delivered_sequence,
                    newest_sequence,
                })
            }
            UnsolicitedServerMessage::Stream(frame) => Ok(SubscriptionUpdate::Stream(frame)),
            UnsolicitedServerMessage::ConversationDirty { subscription_id, task_id, high_water } => {
                // This advisory owns a conversation subscription, not the
                // durable replay subscription or its cursor.
                Ok(SubscriptionUpdate::ConversationDirty { subscription_id, task_id, high_water })
            }
        }
    }

    fn remember_event_id(&mut self, event_id: crate::domain::id::EventId) {
        if self.seen_event_ids.insert(event_id) {
            self.seen_event_order.push_back(event_id);
            if self.seen_event_order.len() > MAX_SEEN_EVENT_IDS {
                if let Some(evicted) = self.seen_event_order.pop_front() {
                    self.seen_event_ids.remove(&evicted);
                }
            }
        }
    }

    fn queue_replay_event(&mut self, event: DomainEvent) -> Result<(), SubscriptionError> {
        if self.pending_replay_events.len() >= MAX_PENDING_REPLAY_EVENTS {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(SubscriptionError::ReplayOverflow {
                limit: MAX_PENDING_REPLAY_EVENTS,
            });
        }
        self.remember_event_id(event.id);
        self.pending_replay_events.push_back(event);
        Ok(())
    }

    /// Mark Ready → NeedsResync when the unsolicited inbox/transport fails.
    pub fn observe_recv_transport_failure(&mut self) {
        if self.state == ClientSubscriptionState::Ready {
            self.state = ClientSubscriptionState::NeedsResync;
        }
    }

    /// Idempotent explicit release of any retained event-replay subscription.
    pub async fn release(&mut self, client: &mut HostClient) -> Result<(), SubscriptionError> {
        if self.state == ClientSubscriptionState::Released
            && self.subscription_id.is_none()
            && self.snapshot_id.is_none()
        {
            return Ok(());
        }
        if let Err(error) = self.release_event_replay_if_owned(client).await {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(error);
        }
        if let Err(error) = self.release_snapshot_if_owned(client).await {
            self.state = ClientSubscriptionState::NeedsResync;
            return Err(error);
        }
        self.state = ClientSubscriptionState::Released;
        Ok(())
    }

    /// Fence this generation after its transport has already disappeared.
    ///
    /// There is no remote release to send in this path, but callers still
    /// retain `Arc` handles to the old subscription while replacing it.  Mark
    /// those handles Released and drop their bounded queues/model so a late
    /// tail can never mutate the replacement generation.
    pub fn retire_without_transport(&mut self) {
        self.subscription_id = None;
        self.snapshot_id = None;
        self.model = None;
        self.seen_event_ids.clear();
        self.seen_event_order.clear();
        self.pending_replay_events.clear();
        self.state = ClientSubscriptionState::Released;
    }

    async fn best_effort_cleanup(&mut self, client: &mut HostClient) -> Option<SubscriptionError> {
        // Cleanup is deliberately best effort for the original synchronization
        // error, but each failed release keeps its id in the object for the
        // next caller-driven retry. Never turn an unknown remote owner into a
        // silently lost local handle.
        let mut first_error = None;
        if let Err(error) = self.release_event_replay_if_owned(client).await {
            first_error = Some(error);
        }
        if let Err(error) = self.release_snapshot_if_owned(client).await {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
        first_error
    }

    async fn release_event_replay_if_owned(
        &mut self,
        client: &mut HostClient,
    ) -> Result<(), SubscriptionError> {
        let Some(subscription_id) = self.subscription_id else {
            return Ok(());
        };
        match client.release_event_replay(subscription_id).await {
            Ok(Ok(())) | Ok(Err(QueryError::NotFound)) => {
                self.subscription_id = None;
                Ok(())
            }
            Ok(Err(error)) => Err(SubscriptionError::Query(error)),
            Err(error) => Err(SubscriptionError::Transport(error)),
        }
    }

    async fn release_snapshot_if_owned(
        &mut self,
        client: &mut HostClient,
    ) -> Result<(), SubscriptionError> {
        let Some(snapshot_id) = self.snapshot_id else {
            return Ok(());
        };
        match client.release_snapshot(snapshot_id).await {
            Ok(Ok(())) | Ok(Err(QueryError::NotFound)) => {
                self.snapshot_id = None;
                Ok(())
            }
            Ok(Err(error)) => Err(SubscriptionError::Query(error)),
            Err(error) => Err(SubscriptionError::Transport(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::model::ClientModelBuilder;
    use crate::domain::event::Event;
    use crate::domain::id::{EnvironmentId, ProjectId, SnapshotId};
    use crate::domain::id::{EventId, SubscriptionId, TaskId};
    use crate::domain::snapshot::{SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem};
    use crate::domain::task::{
        ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
        TaskLifecycle, WorkspaceRef,
    };

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn ready_subscription() -> ClientSubscription {
        let snap = SnapshotId::from_bytes(fixed_uuid_v7(0xb0)).expect("snapshot");
        let task = TaskId::from_bytes(fixed_uuid_v7(0xb1)).expect("task");
        let mut builder = ClientModelBuilder::new();
        for section in [
            SnapshotSection::Tasks,
            SnapshotSection::AgentSessions,
            SnapshotSection::Artifacts,
            SnapshotSection::Resources,
            SnapshotSection::Operations,
        ] {
            let items = if section == SnapshotSection::Tasks {
                vec![SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id: task,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0xb2))
                            .expect("env"),
                        title: "Sub".into(),
                        description: None,
                        project_id: ProjectId::from_bytes(fixed_uuid_v7(0xb3)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 1,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                })]
            } else {
                Vec::new()
            };
            builder
                .ingest_page(SnapshotPage {
                    snapshot_id: snap,
                    through_sequence: 1,
                    section,
                    after_item: None,
                    items,
                    encoded_bytes: 1,
                    next_cursor: None,
                })
                .expect("section");
        }
        let model = builder.finish().expect("model");
        ClientSubscription {
            state: ClientSubscriptionState::Ready,
            subscription_id: Some(SubscriptionId::from_bytes(fixed_uuid_v7(0xb4)).expect("sub")),
            model: Some(model),
            snapshot_id: None,
            seen_event_ids: std::collections::HashSet::new(),
            seen_event_order: std::collections::VecDeque::new(),
            pending_replay_events: std::collections::VecDeque::new(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn release_query_failure_keeps_the_replay_id_for_retry() {
        use crate::client::connection::{ClientConnection, ScriptedDetachBehavior};
        use crate::client::host_client::{HostClient, HostClientConfig};
        use crate::domain::ClientId;
        use crate::protocol::{
            Capability, CapabilitySet, FrameLimits, ProfileFingerprint, ServerHello,
            PROTOCOL_MAJOR, PROTOCOL_MINOR,
        };
        use std::collections::BTreeMap;

        let client_id = ClientId::from_bytes(fixed_uuid_v7(0xd1)).expect("client");
        let connection_id = uuid::Uuid::from_bytes(fixed_uuid_v7(0xd2));
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "subscription-release-test".into(),
            host_boot_id: uuid::Uuid::from_bytes(fixed_uuid_v7(0xd3)),
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("subscription-release"),
            granted: CapabilitySet::from_capabilities([Capability::EventReplay]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let connection = ClientConnection::scripted_for_test(
            client_id,
            hello.clone(),
            ScriptedDetachBehavior::ReleaseQueryError,
        );
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "subscription-release-test".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::EventReplay]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(connection),
            BTreeMap::new(),
        );
        let mut subscription = ready_subscription();
        let expected_id = subscription.subscription_id();

        let error = subscription
            .release(&mut client)
            .await
            .expect_err("release failure must remain visible");

        assert!(matches!(
            error,
            SubscriptionError::Query(QueryError::Unauthorized)
        ));
        assert_eq!(subscription.subscription_id(), expected_id);
        assert_ne!(subscription.state(), ClientSubscriptionState::Released);
    }

    #[test]
    fn transport_failure_marks_ready_subscription_needs_resync() {
        let mut sub = ready_subscription();
        assert_eq!(sub.state(), ClientSubscriptionState::Ready);
        sub.observe_recv_transport_failure();
        assert_eq!(sub.state(), ClientSubscriptionState::NeedsResync);
    }

    #[test]
    fn foreign_unsolicited_message_is_preserved_exactly() {
        let mut sub = ready_subscription();
        let own = sub.subscription_id.expect("own");
        let foreign = SubscriptionId::from_bytes(fixed_uuid_v7(0xb5)).expect("foreign");
        assert_ne!(own, foreign);
        let event = DomainEvent {
            id: EventId::from_bytes(fixed_uuid_v7(0xb6)).expect("event"),
            task_id: None,
            sequence: 2,
            task_revision: None,
            occurred_at_ms: 2,
            payload: Event::TaskReopened,
        };
        let message = UnsolicitedServerMessage::DurableEvent {
            subscription_id: foreign,
            event: event.clone(),
        };
        let err = sub
            .handle_unsolicited_message(message.clone())
            .expect_err("foreign must fail closed");
        match err {
            SubscriptionError::ForeignSubscription(preserved) => {
                assert_eq!(preserved, message);
            }
            other => panic!("expected ForeignSubscription, got {other:?}"),
        }
        assert_eq!(sub.state(), ClientSubscriptionState::NeedsResync);
    }

    #[test]
    fn stream_frame_is_surfaced_without_mutating_durable_model_cursor_or_state() {
        // Catches: ServerMessage::Stream / UnsolicitedServerMessage::Stream must
        // surface via SubscriptionUpdate::Stream without durable model/cursor
        // mutation, durable subscription-id matching, or NeedsResync.
        use crate::domain::id::ResourceId;
        use crate::protocol::{StreamFrame, StreamKey, StreamPayloadKind};

        let mut sub = ready_subscription();
        let own = sub.subscription_id.expect("own");
        let cursor_before = sub.model().expect("model").last_applied_sequence();
        let foreign_sub = SubscriptionId::from_bytes(fixed_uuid_v7(0xc0)).expect("foreign");
        assert_ne!(own, foreign_sub);
        let frame = StreamFrame {
            subscription_id: foreign_sub,
            stream: StreamKey::from(ResourceId::from_bytes(fixed_uuid_v7(0xc1)).expect("resource")),
            generation: 2,
            sequence: 8,
            payload_kind: StreamPayloadKind::new(3).expect("kind"),
            schema_version: 1,
            payload: b"live".to_vec(),
        };
        let update = sub
            .handle_unsolicited_message(UnsolicitedServerMessage::Stream(frame.clone()))
            .expect("stream must surface without fail-closed durable matching");
        assert_eq!(update, SubscriptionUpdate::Stream(frame));
        assert_eq!(sub.state(), ClientSubscriptionState::Ready);
        assert_eq!(
            sub.model().expect("model").last_applied_sequence(),
            cursor_before
        );
    }

    #[test]
    fn inbox_runtime_projects_one_subscription_and_fences_replay_duplicates_and_foreign_events() {
        use crate::ui::task_cockpit::InboxRuntime;

        let mut runtime = InboxRuntime::new();
        runtime.attach_subscription(ready_subscription());
        let own = runtime
            .subscription()
            .and_then(|subscription| subscription.subscription_id())
            .expect("ready subscription id");
        let task = runtime
            .subscription()
            .and_then(|subscription| subscription.model())
            .and_then(|model| model.tasks().keys().next().copied())
            .expect("ready task");
        assert_eq!(
            runtime
                .projection()
                .and_then(|projection| projection.row(task))
                .map(|row| row.title.as_str()),
            Some("Sub")
        );

        let event = DomainEvent {
            id: EventId::from_bytes(fixed_uuid_v7(0xc2)).expect("event"),
            task_id: Some(task),
            sequence: 2,
            task_revision: Some(2),
            occurred_at_ms: 22,
            payload: Event::TaskRenamed {
                title: "Renamed".into(),
            },
        };
        let message = UnsolicitedServerMessage::DurableEvent {
            subscription_id: own,
            event: event.clone(),
        };
        let update = runtime
            .subscription_mut()
            .expect("subscription")
            .handle_unsolicited_message(message.clone())
            .expect("live event");
        assert!(runtime
            .apply_subscription_update(update)
            .expect("projection update"));
        assert_eq!(runtime.unread_cursor().last_seen_sequence(), 2);
        assert_eq!(runtime.unread_cursor().unread_count(task), 1);
        assert_eq!(
            runtime
                .projection()
                .and_then(|projection| projection.row(task))
                .map(|row| (row.title.as_str(), row.occurred_at_ms)),
            Some(("Renamed", 22))
        );
        assert_eq!(
            runtime
                .subscription()
                .and_then(|subscription| subscription.model())
                .map(|model| model.task_projection_index_incremental_updates()),
            Some(1),
            "one durable event updates one keyed index entry"
        );
        assert!(runtime.mark_read(task));
        assert_eq!(runtime.unread_cursor().unread_count(task), 0);
        assert_eq!(
            runtime
                .projection()
                .and_then(|projection| projection.row(task))
                .map(|row| row.unread_event_count),
            Some(0)
        );

        let duplicate = runtime
            .subscription_mut()
            .expect("subscription")
            .handle_unsolicited_message(message)
            .expect("duplicate event is idempotent");
        assert!(!runtime
            .apply_subscription_update(duplicate)
            .expect("duplicate projection update"));
        assert_eq!(
            runtime.unread_cursor().unread_count(task),
            0,
            "a replayed event must not undo the local mark-read cursor"
        );
        assert_eq!(runtime.projection_updates(), 2);

        let lower = DomainEvent {
            id: EventId::from_bytes(fixed_uuid_v7(0xc3)).expect("lower event"),
            task_id: Some(task),
            sequence: 1,
            task_revision: Some(2),
            occurred_at_ms: 11,
            payload: Event::TaskRenamed {
                title: "Out of order".into(),
            },
        };
        let error = runtime
            .subscription_mut()
            .expect("subscription")
            .handle_unsolicited_message(UnsolicitedServerMessage::DurableEvent {
                subscription_id: own,
                event: lower,
            })
            .expect_err("unknown out-of-order event must require resync");
        assert!(matches!(error, SubscriptionError::Model(_)));
        assert_eq!(
            runtime
                .subscription()
                .map(|subscription| subscription.state()),
            Some(ClientSubscriptionState::NeedsResync)
        );

        let mut foreign = ready_subscription();
        let foreign_id = SubscriptionId::from_bytes(fixed_uuid_v7(0xc5)).expect("foreign id");
        assert_ne!(foreign_id, own);
        let foreign_error = foreign
            .handle_unsolicited_message(UnsolicitedServerMessage::DurableEvent {
                subscription_id: foreign_id,
                event: DomainEvent {
                    id: EventId::from_bytes(fixed_uuid_v7(0xc4)).expect("foreign event"),
                    task_id: None,
                    sequence: 2,
                    task_revision: None,
                    occurred_at_ms: 23,
                    payload: Event::TaskReopened,
                },
            })
            .expect_err("foreign event must fail closed");
        assert!(matches!(
            foreign_error,
            SubscriptionError::ForeignSubscription(_)
        ));
    }

    #[test]
    fn inbox_runtime_consumes_snapshot_race_replay_for_unread_cursor_on_attach() {
        use crate::ui::task_cockpit::InboxRuntime;

        let mut subscription = ready_subscription();
        let task = subscription
            .model()
            .and_then(|model| model.tasks().keys().next().copied())
            .expect("ready task");
        subscription.pending_replay_events.push_back(DomainEvent {
            id: EventId::from_bytes(fixed_uuid_v7(0xc6)).expect("replay event"),
            task_id: Some(task),
            sequence: 2,
            task_revision: Some(2),
            occurred_at_ms: 22,
            payload: Event::TaskReopened,
        });

        let mut runtime = InboxRuntime::new();
        runtime.attach_subscription(subscription);
        assert_eq!(runtime.unread_cursor().last_seen_sequence(), 2);
        assert_eq!(runtime.unread_cursor().unread_count(task), 1);
        assert!(runtime
            .projection()
            .and_then(|projection| projection.row(task))
            .is_some());
    }

    #[test]
    fn inbox_runtime_stale_blocks_visible_rows_after_resync_until_authoritative_attach() {
        use crate::ui::task_cockpit::InboxRuntime;

        let mut runtime = InboxRuntime::new();
        runtime.attach_subscription(ready_subscription());
        assert!(runtime.projection().is_some());
        runtime
            .apply_subscription_update(SubscriptionUpdate::ResyncRequired {
                last_delivered_sequence: 1,
                newest_sequence: 2,
            })
            .expect("resync transition is observable");
        assert!(runtime.projection().is_none());
        runtime.attach_subscription(ready_subscription());
        assert!(runtime.projection().is_some());
    }

    #[test]
    fn replay_queue_overflow_is_typed_and_never_silently_evicts_history() {
        let mut sub = ready_subscription();
        for sequence in 0..MAX_PENDING_REPLAY_EVENTS {
            sub.queue_replay_event(DomainEvent {
                id: EventId::new(),
                task_id: None,
                sequence: sequence as u64,
                task_revision: None,
                occurred_at_ms: sequence as i64,
                payload: Event::TaskReopened,
            })
            .expect("queue remains bounded before limit");
        }
        let err = sub
            .queue_replay_event(DomainEvent {
                id: EventId::new(),
                task_id: None,
                sequence: MAX_PENDING_REPLAY_EVENTS as u64,
                task_revision: None,
                occurred_at_ms: MAX_PENDING_REPLAY_EVENTS as i64,
                payload: Event::TaskReopened,
            })
            .expect_err("overflow must force typed resync");
        assert!(
            matches!(err, SubscriptionError::ReplayOverflow { limit } if limit == MAX_PENDING_REPLAY_EVENTS)
        );
        assert_eq!(sub.pending_replay_events.len(), MAX_PENDING_REPLAY_EVENTS);
    }

    #[test]
    fn conversation_wake_preserves_replay_cursor_and_its_own_subscription() {
        let mut sub = ready_subscription();
        let cursor = sub.model().unwrap().last_applied_sequence();
        let subscription_id = SubscriptionId::new();
        let task_id = crate::domain::TaskId::new();
        let update = sub.handle_unsolicited_message(UnsolicitedServerMessage::ConversationDirty {
            subscription_id, task_id, high_water: 19,
        }).unwrap();
        assert_eq!(update, SubscriptionUpdate::ConversationDirty { subscription_id, task_id, high_water: 19 });
        assert_eq!(sub.state(), ClientSubscriptionState::Ready);
        assert_eq!(sub.model().unwrap().last_applied_sequence(), cursor);
    }

    #[test]
    fn paged_tasks_preview_stays_subscription_model_free() {
        let snap = SnapshotId::from_bytes(fixed_uuid_v7(0xf0)).expect("snapshot");
        let first = TaskId::from_bytes(fixed_uuid_v7(0xf1)).expect("task");
        let second = TaskId::from_bytes(fixed_uuid_v7(0xf2)).expect("task");
        let mut builder = ClientModelBuilder::new();
        builder
            .ingest_page(SnapshotPage {
                snapshot_id: snap,
                through_sequence: 4,
                section: SnapshotSection::Tasks,
                after_item: None,
                items: vec![SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id: first,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0xf3))
                            .expect("env"),
                        title: "One".into(),
                        description: None,
                        project_id: ProjectId::from_bytes(fixed_uuid_v7(0xf4)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 1,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                })],
                encoded_bytes: 1,
                next_cursor: Some(vec![9]),
            })
            .expect("page 1");
        builder
            .ingest_page(SnapshotPage {
                snapshot_id: snap,
                through_sequence: 4,
                section: SnapshotSection::Tasks,
                after_item: Some(crate::domain::snapshot::SnapshotItemKey::Task(first)),
                items: vec![SnapshotItem::Task(TaskSnapshotItem {
                    task: TaskFacts {
                        id: second,
                        environment_id: EnvironmentId::from_bytes(fixed_uuid_v7(0xf3))
                            .expect("env"),
                        title: "Two".into(),
                        description: None,
                        project_id: ProjectId::from_bytes(fixed_uuid_v7(0xf4)).expect("project"),
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 2,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                    primary_agent_id: None,
                })],
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("page 2");

        let subscription = ClientSubscription::new();
        let preview = builder.finish_tasks_preview().expect("paged tasks preview");
        assert_eq!(preview.tasks().len(), 2);
        assert!(preview.tasks().contains_key(&first));
        assert!(preview.tasks().contains_key(&second));
        assert_eq!(preview.through_sequence(), 4);
        assert!(subscription.model().is_none());
        assert_eq!(subscription.state(), ClientSubscriptionState::Pending);
        assert!(subscription.snapshot_id.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preview_tasks_rejects_ready_without_mutating_canonical_model() {
        use crate::client::connection::ClientConnection;
        use crate::client::host_client::{HostClient, HostClientConfig};
        use crate::domain::ClientId;
        use crate::protocol::{
            Capability, CapabilitySet, FrameLimits, ProfileFingerprint, ServerHello,
            PROTOCOL_MAJOR, PROTOCOL_MINOR,
        };
        use std::collections::BTreeMap;

        let client_id = ClientId::from_bytes(fixed_uuid_v7(0xc1)).expect("client");
        let connection_id = uuid::Uuid::from_bytes(fixed_uuid_v7(0xc2));
        let hello = ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "subscription-preview-ready-test".into(),
            host_boot_id: uuid::Uuid::from_bytes(fixed_uuid_v7(0xc3)),
            connection_id,
            profile_fingerprint: ProfileFingerprint::hash_normalized("subscription-preview-ready"),
            granted: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        };
        let connection = ClientConnection::scripted_for_test(
            client_id,
            hello.clone(),
            crate::client::connection::ScriptedDetachBehavior::MatchingAck,
        );
        let mut client = HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: "subscription-preview-ready-test".into(),
                client_build: "devmanager/test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([Capability::PagedSnapshots]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(connection),
            BTreeMap::new(),
        );
        let mut subscription = ready_subscription();
        let model_before = subscription.model().cloned();
        let state_before = subscription.state();
        let id_before = subscription.subscription_id();

        let error = subscription
            .preview_tasks(&mut client)
            .await
            .expect_err("Ready subscription must refuse Tasks preview");
        assert!(matches!(error, SubscriptionError::NotReady));
        assert_eq!(subscription.state(), state_before);
        assert_eq!(subscription.subscription_id(), id_before);
        assert_eq!(subscription.model(), model_before.as_ref());
    }
}
