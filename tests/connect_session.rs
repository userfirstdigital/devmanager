//! Focused Connect session admission / answer proofs.

use devmanager::connect::{
    ActionAnswer, ActionEpoch, ConnectSession, DeviceInput, EphemeralPresence, RuntimeGeneration,
    SessionAdmitError, SessionReceiptKind, MAX_SESSION_CONNECTED,
};
use devmanager::domain::id::{ClientId, CommandId, OperationId, RequestId, TaskId};
use uuid::Uuid;

fn uuid(tail: u8) -> Uuid {
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
    TaskId::from_bytes(uuid(tail).into_bytes()).unwrap()
}

fn client(tail: u8) -> ClientId {
    ClientId::from_bytes(uuid(tail).into_bytes()).unwrap()
}

fn command(tail: u8) -> CommandId {
    CommandId::from_bytes(uuid(tail).into_bytes()).unwrap()
}

fn operation(tail: u8) -> OperationId {
    OperationId::from_bytes(uuid(tail).into_bytes()).unwrap()
}

fn request(tail: u8) -> RequestId {
    RequestId::from_bytes(uuid(tail).into_bytes()).unwrap()
}

#[test]
fn alternating_clients_preserve_durable_ids_without_leases() {
    let mut session = ConnectSession::new(task(1));
    let desktop = client(2);
    let phone = client(3);
    session.connect_client(desktop).unwrap();
    session.connect_client(phone).unwrap();
    let mut presence = EphemeralPresence::default();
    let first = session
        .admit(
            DeviceInput {
                task_id: session.task_id(),
                client_id: desktop,
                command_id: command(4),
                operation_id: operation(5),
                expected_revision: Some(session.revision()),
                resource_id: None,
                input_sequence: 1,
                turn_epoch: session.turn_epoch(),
                focus_epoch: session.focus_epoch(),
                observed_at_ms: 10,
            },
            &mut presence,
        )
        .unwrap();
    assert_eq!(first.kind, SessionReceiptKind::AcceptedDurable);
    let second = session
        .admit(
            DeviceInput {
                task_id: session.task_id(),
                client_id: phone,
                command_id: command(6),
                operation_id: operation(7),
                expected_revision: Some(session.revision()),
                resource_id: None,
                input_sequence: 1,
                turn_epoch: session.turn_epoch(),
                focus_epoch: session.focus_epoch(),
                observed_at_ms: 20,
            },
            &mut presence,
        )
        .unwrap();
    assert_eq!(second.kind, SessionReceiptKind::AcceptedDurable);
    assert_eq!(session.visible_controller(), None);
    assert_eq!(session.last_sender_lease(), None);
    assert_eq!(session.reconcile_echo(command(4)), Some(operation(5)));
}

#[test]
fn stale_and_duplicate_answers_remain_explicit_denials() {
    let mut session = ConnectSession::new(task(8));
    let desktop = client(9);
    session.connect_client(desktop).unwrap();
    let request_id = request(10);
    let epoch = ActionEpoch::new(3).unwrap();
    session.open_request(request_id, epoch).unwrap();
    assert!(session
        .answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: desktop,
            request_id,
            action_epoch: epoch,
            runtime_generation: session.runtime_generation(),
            observed_at_ms: 1,
        })
        .is_ok());
    assert_eq!(
        session.answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: desktop,
            request_id,
            action_epoch: epoch,
            runtime_generation: session.runtime_generation(),
            observed_at_ms: 2,
        }),
        Err(SessionAdmitError::AlreadyResolved)
    );
    session.restart_provider();
    session.open_request(request(11), epoch).unwrap();
    assert_eq!(
        session.answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: desktop,
            request_id: request(11),
            action_epoch: epoch,
            runtime_generation: RuntimeGeneration::new(1).unwrap(),
            observed_at_ms: 3,
        }),
        Err(SessionAdmitError::StaleGeneration)
    );
}

#[test]
fn reconnect_resync_and_connected_bound() {
    let mut session = ConnectSession::new(task(12));
    let desktop = client(13);
    session.connect_client(desktop).unwrap();
    session.disconnect_client(desktop);
    assert_eq!(
        session.enqueue(DeviceInput {
            task_id: session.task_id(),
            client_id: desktop,
            command_id: command(14),
            operation_id: operation(15),
            expected_revision: Some(session.revision()),
            resource_id: None,
            input_sequence: 1,
            turn_epoch: session.turn_epoch(),
            focus_epoch: session.focus_epoch(),
            observed_at_ms: 1,
        }),
        Err(SessionAdmitError::ClientDisconnected)
    );
    session.connect_client(desktop).unwrap();
    for i in 0..MAX_SESSION_CONNECTED {
        let extra = client((20 + i) as u8);
        let _ = session.connect_client(extra);
    }
    assert_eq!(
        session.connect_client(client(0xff)),
        Err(SessionAdmitError::StateBoundExceeded)
    );
}
