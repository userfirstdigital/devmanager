//! Caller-driven initial synchronization and live durable subscription.

use crate::client::connection::UnsolicitedServerMessage;
use crate::client::host_client::HostClient;
use crate::client::model::{ClientModel, ClientModelBuilder, ClientModelError};
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
}

#[derive(Debug)]
pub enum SubscriptionError {
    NotReady,
    NeedsResync,
    Released,
    Model(ClientModelError),
    /// Foreign unsolicited frame preserved for the caller; subscription needs resync.
    ForeignSubscription(UnsolicitedServerMessage),
    InvalidResync,
    Transport(IpcError),
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
            Self::InvalidResync => write!(f, "resync required fields are inconsistent"),
            Self::Transport(error) => write!(f, "subscription transport error: {error}"),
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

    /// Snapshot through N → open replay after N → release snapshot → apply frozen
    /// replay → retain live subscription metadata. Caller-driven only.
    pub async fn synchronize(&mut self, client: &mut HostClient) -> Result<(), SubscriptionError> {
        if matches!(
            self.state,
            ClientSubscriptionState::Ready | ClientSubscriptionState::NeedsResync
        ) {
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

        match self.synchronize_inner(client).await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.best_effort_cleanup(client).await;
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
                    Err(error) => return Err(SubscriptionError::Transport(error)),
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
            Err(error) => return Err(SubscriptionError::Transport(error)),
        };
        self.subscription_id = Some(open.subscription_id);

        if let Some(snapshot_id) = self.snapshot_id.take() {
            match client.release_snapshot(snapshot_id).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(SubscriptionError::Query(error)),
                Err(error) => return Err(SubscriptionError::Transport(error)),
            }
        }

        let mut model = model;
        let mut batch = open;
        loop {
            let next = batch.page.next_cursor.clone();
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
                Err(error) => return Err(SubscriptionError::Transport(error)),
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
        }
    }

    /// Mark Ready → NeedsResync when the unsolicited inbox/transport fails.
    pub fn observe_recv_transport_failure(&mut self) {
        if self.state == ClientSubscriptionState::Ready {
            self.state = ClientSubscriptionState::NeedsResync;
        }
    }

    /// Idempotent explicit release of any retained event-replay subscription.
    pub async fn release(&mut self, client: &mut HostClient) -> Result<(), SubscriptionError> {
        if self.state == ClientSubscriptionState::Released && self.subscription_id.is_none() {
            return Ok(());
        }
        if let Some(subscription_id) = self.subscription_id.take() {
            match client.release_event_replay(subscription_id).await {
                Ok(Ok(())) | Ok(Err(QueryError::NotFound)) => {}
                Ok(Err(error)) => {
                    self.subscription_id = Some(subscription_id);
                    return Err(SubscriptionError::Query(error));
                }
                Err(error) => {
                    self.subscription_id = Some(subscription_id);
                    return Err(SubscriptionError::Transport(error));
                }
            }
        }
        if let Some(snapshot_id) = self.snapshot_id.take() {
            let _ = client.release_snapshot(snapshot_id).await;
        }
        self.state = ClientSubscriptionState::Released;
        Ok(())
    }

    async fn best_effort_cleanup(&mut self, client: &mut HostClient) {
        if let Some(subscription_id) = self.subscription_id.take() {
            let _ = client.release_event_replay(subscription_id).await;
        }
        if let Some(snapshot_id) = self.snapshot_id.take() {
            let _ = client.release_snapshot(snapshot_id).await;
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
        }
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
}
