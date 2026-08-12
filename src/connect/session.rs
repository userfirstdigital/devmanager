//! Invisible multi-device input admission.
//!
//! Each mutation is independent. An accepted receipt means only that the
//! operation is durable, not that its effect has settled. Last-sender state is
//! ephemeral presence metadata and never a controller lease.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::id::{
    ClientId, CommandId, OperationId, RequestId, ResourceId, TaskId,
};

use super::epoch::{ActionEpoch, FocusEpoch, RuntimeGeneration, TurnEpoch};
use super::presence::{LastSenderHint, PresenceSink};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAdmitError {
    ZeroEpoch,
    TaskMismatch,
    DuplicateCommand,
    StaleTurn,
    StaleFocus,
    StaleGeneration,
    StaleInputSequence,
    RevisionConflict,
    ClientDisconnected,
    QueueInvalidated,
    AlreadyResolved,
    StaleAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionReceiptKind {
    AcceptedDurable,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionReceipt {
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub kind: SessionReceiptKind,
    pub settled: bool,
}

impl SessionReceipt {
    pub const fn is_settled(self) -> bool {
        self.settled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInput {
    pub task_id: TaskId,
    pub client_id: ClientId,
    pub command_id: CommandId,
    pub operation_id: OperationId,
    pub expected_revision: Option<u64>,
    pub resource_id: Option<ResourceId>,
    pub input_sequence: u64,
    pub turn_epoch: TurnEpoch,
    pub focus_epoch: FocusEpoch,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionAnswer {
    pub task_id: TaskId,
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub action_epoch: ActionEpoch,
    pub runtime_generation: RuntimeGeneration,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettledAnswer {
    client_id: ClientId,
    action_epoch: ActionEpoch,
    runtime_generation: RuntimeGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedMutation {
    client_id: ClientId,
    generation: u64,
}

/// Per-task Connect session. No solo controller lease exists.
#[derive(Debug, Clone)]
pub struct ConnectSession {
    task_id: TaskId,
    revision: u64,
    turn_epoch: TurnEpoch,
    focus_epoch: FocusEpoch,
    runtime_generation: RuntimeGeneration,
    queue_generation: u64,
    accepted_commands: BTreeMap<CommandId, OperationId>,
    resource_sequences: BTreeMap<ResourceId, u64>,
    settled_requests: BTreeMap<RequestId, SettledAnswer>,
    queued: BTreeMap<CommandId, QueuedMutation>,
    invalidated: BTreeSet<CommandId>,
    connected: BTreeSet<ClientId>,
    last_client: Option<ClientId>,
}

impl ConnectSession {
    pub fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            revision: 1,
            turn_epoch: TurnEpoch::new(1).expect("nonzero"),
            focus_epoch: FocusEpoch::new(1).expect("nonzero"),
            runtime_generation: RuntimeGeneration::new(1).expect("nonzero"),
            queue_generation: 1,
            accepted_commands: BTreeMap::new(),
            resource_sequences: BTreeMap::new(),
            settled_requests: BTreeMap::new(),
            queued: BTreeMap::new(),
            invalidated: BTreeSet::new(),
            connected: BTreeSet::new(),
            last_client: None,
        }
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn turn_epoch(&self) -> TurnEpoch {
        self.turn_epoch
    }

    pub fn focus_epoch(&self) -> FocusEpoch {
        self.focus_epoch
    }

    pub fn runtime_generation(&self) -> RuntimeGeneration {
        self.runtime_generation
    }

    /// Connect never exposes a durable controller or owner badge.
    pub const fn visible_controller(&self) -> Option<ClientId> {
        None
    }

    pub const fn owner_badge(&self) -> Option<ClientId> {
        None
    }

    pub fn connect_client(&mut self, client_id: ClientId) {
        self.connected.insert(client_id);
    }

    pub fn disconnect_client(&mut self, client_id: ClientId) {
        self.connected.remove(&client_id);
        self.queue_generation = self.queue_generation.saturating_add(1);
        self.invalidate_queued_for_client(client_id);
    }

    pub fn invalidate_queued_for_client(&mut self, client_id: ClientId) {
        let condemned: Vec<CommandId> = self
            .queued
            .iter()
            .filter(|(_, queued)| queued.client_id == client_id)
            .map(|(command_id, _)| *command_id)
            .collect();
        for command_id in condemned {
            self.queued.remove(&command_id);
            self.invalidated.insert(command_id);
        }
    }

    pub fn restart_provider(&mut self) {
        self.runtime_generation = self.runtime_generation.saturating_next();
        self.settled_requests.clear();
    }

    pub fn switch_focus(&mut self) -> FocusEpoch {
        self.focus_epoch = self.focus_epoch.saturating_next();
        self.focus_epoch
    }

    pub fn enqueue(&mut self, input: DeviceInput) -> Result<(), SessionAdmitError> {
        self.validate_epochs(input)?;
        if !self.connected.contains(&input.client_id) {
            return Err(SessionAdmitError::ClientDisconnected);
        }
        if self.invalidated.contains(&input.command_id) {
            return Err(SessionAdmitError::QueueInvalidated);
        }
        self.queued.insert(
            input.command_id,
            QueuedMutation {
                client_id: input.client_id,
                generation: self.queue_generation,
            },
        );
        Ok(())
    }

    pub fn admit(
        &mut self,
        input: DeviceInput,
        presence: &mut impl PresenceSink,
    ) -> Result<SessionReceipt, SessionAdmitError> {
        self.validate_epochs(input)?;
        if !self.connected.contains(&input.client_id) {
            return Err(SessionAdmitError::ClientDisconnected);
        }
        if self.invalidated.contains(&input.command_id) {
            return Err(SessionAdmitError::QueueInvalidated);
        }
        if let Some(queued) = self.queued.get(&input.command_id) {
            if queued.generation != self.queue_generation {
                self.queued.remove(&input.command_id);
                return Err(SessionAdmitError::QueueInvalidated);
            }
        }
        if let Some(&operation_id) = self.accepted_commands.get(&input.command_id) {
            return Ok(SessionReceipt {
                command_id: input.command_id,
                operation_id,
                kind: SessionReceiptKind::Duplicate,
                settled: false,
            });
        }
        if let Some(expected) = input.expected_revision {
            if expected != self.revision {
                return Err(SessionAdmitError::RevisionConflict);
            }
        }
        if let Some(resource_id) = input.resource_id {
            let last = self.resource_sequences.get(&resource_id).copied().unwrap_or(0);
            if input.input_sequence <= last {
                return Err(SessionAdmitError::StaleInputSequence);
            }
            self.resource_sequences
                .insert(resource_id, input.input_sequence);
        }

        self.queued.remove(&input.command_id);
        self.accepted_commands
            .insert(input.command_id, input.operation_id);
        if self.last_client != Some(input.client_id) {
            self.turn_epoch = self.turn_epoch.saturating_next();
            self.last_client = Some(input.client_id);
        }
        self.revision = self.revision.saturating_add(1);
        presence.record(LastSenderHint::new(
            self.task_id,
            input.client_id,
            input.observed_at_ms,
            self.turn_epoch,
            self.focus_epoch,
        ));
        Ok(SessionReceipt {
            command_id: input.command_id,
            operation_id: input.operation_id,
            kind: SessionReceiptKind::AcceptedDurable,
            settled: false,
        })
    }

    pub fn answer(&mut self, answer: ActionAnswer) -> Result<RequestId, SessionAdmitError> {
        if answer.task_id != self.task_id {
            return Err(SessionAdmitError::TaskMismatch);
        }
        if answer.runtime_generation != self.runtime_generation {
            return Err(SessionAdmitError::StaleGeneration);
        }
        if let Some(settled) = self.settled_requests.get(&answer.request_id) {
            if settled.action_epoch != answer.action_epoch {
                return Err(SessionAdmitError::StaleAction);
            }
            return Err(SessionAdmitError::AlreadyResolved);
        }
        self.settled_requests.insert(
            answer.request_id,
            SettledAnswer {
                client_id: answer.client_id,
                action_epoch: answer.action_epoch,
                runtime_generation: answer.runtime_generation,
            },
        );
        Ok(answer.request_id)
    }

    /// Optimistic echoes reconcile by command identity, never arrival order.
    pub fn reconcile_echo(&self, command_id: CommandId) -> Option<OperationId> {
        self.accepted_commands.get(&command_id).copied()
    }

    fn validate_epochs(&self, input: DeviceInput) -> Result<(), SessionAdmitError> {
        if input.task_id != self.task_id {
            return Err(SessionAdmitError::TaskMismatch);
        }
        if input.turn_epoch.get() == 0 || input.focus_epoch.get() == 0 {
            return Err(SessionAdmitError::ZeroEpoch);
        }
        if input.turn_epoch != self.turn_epoch {
            return Err(SessionAdmitError::StaleTurn);
        }
        if input.focus_epoch != self.focus_epoch {
            return Err(SessionAdmitError::StaleFocus);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::presence::EphemeralPresence;
    use crate::domain::id::ResourceId;
    use uuid::Uuid;

    fn fixed_uuid(tail: u8) -> Uuid {
        let mut bytes = [0_u8; 16];
        bytes[0] = 0x01;
        bytes[1] = 0x23;
        bytes[2] = 0x45;
        bytes[3] = 0x67;
        bytes[4] = 0x89;
        bytes[5] = 0xab;
        bytes[6] = 0x70;
        bytes[7] = 0xcd;
        bytes[8] = 0x80;
        bytes[9] = 0xef;
        bytes[15] = tail;
        Uuid::from_bytes(bytes)
    }

    fn task(tail: u8) -> TaskId {
        TaskId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn client(tail: u8) -> ClientId {
        ClientId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn command(tail: u8) -> CommandId {
        CommandId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn operation(tail: u8) -> OperationId {
        OperationId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn request(tail: u8) -> RequestId {
        RequestId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn resource(tail: u8) -> ResourceId {
        ResourceId::from_bytes(fixed_uuid(tail).into_bytes()).unwrap()
    }

    fn input(
        session: &ConnectSession,
        client_id: ClientId,
        command_id: CommandId,
        operation_id: OperationId,
        at: i64,
    ) -> DeviceInput {
        DeviceInput {
            task_id: session.task_id(),
            client_id,
            command_id,
            operation_id,
            expected_revision: Some(session.revision()),
            resource_id: None,
            input_sequence: 1,
            turn_epoch: session.turn_epoch(),
            focus_epoch: session.focus_epoch(),
            observed_at_ms: at,
        }
    }

    #[test]
    fn accepted_receipt_is_not_settlement_and_has_no_lease() {
        let task_id = task(0x11);
        let desktop = client(0x21);
        let mut session = ConnectSession::new(task_id);
        session.connect_client(desktop);
        let mut presence = EphemeralPresence::default();
        let receipt = session
            .admit(
                input(&session, desktop, command(0x31), operation(0x41), 100),
                &mut presence,
            )
            .unwrap();
        assert_eq!(receipt.kind, SessionReceiptKind::AcceptedDurable);
        assert!(!receipt.is_settled());
        assert_eq!(session.visible_controller(), None);
        assert_eq!(session.owner_badge(), None);
        assert_eq!(presence.last_sender(task_id).unwrap().client_id, desktop);
        assert_eq!(
            presence.last_sender(task_id).unwrap().turn_epoch,
            session.turn_epoch()
        );
    }

    #[test]
    fn first_answer_wins_and_stale_generation_is_rejected() {
        let mut session = ConnectSession::new(task(0x12));
        let desktop = client(0x22);
        let phone = client(0x23);
        session.connect_client(desktop);
        session.connect_client(phone);
        let request_id = request(0x51);
        let epoch = ActionEpoch::new(1).unwrap();
        assert!(session
            .answer(ActionAnswer {
                task_id: session.task_id(),
                client_id: desktop,
                request_id,
                action_epoch: epoch,
                runtime_generation: session.runtime_generation(),
                observed_at_ms: 10,
            })
            .is_ok());
        assert_eq!(
            session.answer(ActionAnswer {
                task_id: session.task_id(),
                client_id: phone,
                request_id,
                action_epoch: epoch,
                runtime_generation: session.runtime_generation(),
                observed_at_ms: 11,
            }),
            Err(SessionAdmitError::AlreadyResolved)
        );
        session.restart_provider();
        assert_eq!(
            session.answer(ActionAnswer {
                task_id: session.task_id(),
                client_id: desktop,
                request_id: request(0x52),
                action_epoch: epoch,
                runtime_generation: RuntimeGeneration::new(1).unwrap(),
                observed_at_ms: 12,
            }),
            Err(SessionAdmitError::StaleGeneration)
        );
        let mut presence = EphemeralPresence::default();
        let mut concurrent = input(&session, desktop, command(0x33), operation(0x43), 20);
        concurrent.resource_id = Some(resource(0x61));
        concurrent.input_sequence = 2;
        session.connect_client(desktop);
        assert!(session.admit(concurrent, &mut presence).is_ok());
    }
}
