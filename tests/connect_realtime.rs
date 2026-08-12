use std::collections::BTreeMap;
use std::collections::BTreeSet;

use uuid::Uuid;

use devmanager::connect::{
    forbidden_push_fields, matrix_covers_direct_and_hosted, project_field, sanitize_push,
    simulate_fault, ActionAnswer, ActionEpoch, AttentionKind, ConnectEnrollment, ConnectSession,
    ContentClass, DeviceInput, EphemeralPresence, FailureClass, OutboundField, PairingContinuity,
    PinnedHostPublicId, ProjectionDenyReason, ProjectionGrant, PushPolicy, SessionAdmitError,
    SessionReceiptKind, SimulatedFaultOutcome, UpdateContinuity, UpdateContinuityError,
    CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR, FAILURE_MATRIX,
};
use devmanager::domain::id::{
    ClientId, CommandId, OperationId, RequestId, ResourceId, TaskId,
};

fn fixed_uuid_v7(tail: u8) -> Uuid {
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

fn task_id(tail: u8) -> TaskId {
    TaskId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("task id")
}

fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("client id")
}

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("command id")
}

fn operation_id(tail: u8) -> OperationId {
    OperationId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("operation id")
}

fn request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("request id")
}

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail).into_bytes()).expect("resource id")
}

fn live_input(
    session: &ConnectSession,
    client: ClientId,
    command: CommandId,
    operation: OperationId,
    at: i64,
) -> DeviceInput {
    DeviceInput {
        task_id: session.task_id(),
        client_id: client,
        command_id: command,
        operation_id: operation,
        expected_revision: Some(session.revision()),
        resource_id: None,
        input_sequence: 1,
        turn_epoch: session.turn_epoch(),
        focus_epoch: session.focus_epoch(),
        observed_at_ms: at,
    }
}

#[test]
fn alternating_desktop_phone_then_desktop_is_invisible() {
    let task = task_id(0x10);
    let desktop = client_id(0x21);
    let phone = client_id(0x22);
    let mut session = ConnectSession::new(task);
    session.connect_client(desktop);
    session.connect_client(phone);
    let mut presence = EphemeralPresence::default();

    let first = session
        .admit(
            live_input(&session, desktop, command_id(0x31), operation_id(0x41), 1_000),
            &mut presence,
        )
        .expect("desktop");
    assert_eq!(first.kind, SessionReceiptKind::AcceptedDurable);
    assert!(!first.is_settled());
    assert_eq!(session.visible_controller(), None);
    assert_eq!(session.owner_badge(), None);

    let later = session
        .admit(
            live_input(
                &session,
                phone,
                command_id(0x32),
                operation_id(0x42),
                1_000 + 5 * 60 * 1000,
            ),
            &mut presence,
        )
        .expect("phone five minutes later");
    assert!(!later.is_settled());
    assert_eq!(presence.last_sender(task).unwrap().client_id, phone);
    assert_eq!(
        presence.last_sender(task).unwrap().turn_epoch,
        session.turn_epoch()
    );
    assert_eq!(
        presence.last_sender(task).unwrap().focus_epoch,
        session.focus_epoch()
    );

    let again = session
        .admit(
            live_input(&session, desktop, command_id(0x33), operation_id(0x43), 400_000),
            &mut presence,
        )
        .expect("desktop again");
    assert_eq!(again.kind, SessionReceiptKind::AcceptedDurable);
    assert_eq!(session.visible_controller(), None);
    assert_eq!(presence.last_sender(task).unwrap().client_id, desktop);
}

#[test]
fn alternating_concurrent_typing_has_no_controller_lease() {
    let mut session = ConnectSession::new(task_id(0x11));
    let desktop = client_id(0x23);
    let phone = client_id(0x24);
    session.connect_client(desktop);
    session.connect_client(phone);
    let mut presence = EphemeralPresence::default();
    let resource = resource_id(0x51);

    let mut desktop_input = live_input(
        &session,
        desktop,
        command_id(0x34),
        operation_id(0x44),
        10,
    );
    desktop_input.resource_id = Some(resource);
    desktop_input.input_sequence = 1;
    session
        .admit(desktop_input, &mut presence)
        .expect("desktop keystroke");

    let mut phone_input = live_input(&session, phone, command_id(0x35), operation_id(0x45), 11);
    phone_input.resource_id = Some(resource);
    phone_input.input_sequence = 2;
    session
        .admit(phone_input, &mut presence)
        .expect("phone keystroke");

    let mut stale = live_input(&session, desktop, command_id(0x36), operation_id(0x46), 12);
    stale.resource_id = Some(resource);
    stale.input_sequence = 2;
    assert_eq!(
        session.admit(stale, &mut presence),
        Err(SessionAdmitError::StaleInputSequence)
    );
    assert_eq!(session.visible_controller(), None);
}

#[test]
fn alternating_first_answer_wins_and_never_falls_through() {
    let mut session = ConnectSession::new(task_id(0x12));
    let desktop = client_id(0x25);
    let phone = client_id(0x26);
    session.connect_client(desktop);
    session.connect_client(phone);
    let request = request_id(0x61);
    let epoch = ActionEpoch::new(3).expect("action epoch");
    assert!(session
        .answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: desktop,
            request_id: request,
            action_epoch: epoch,
            runtime_generation: session.runtime_generation(),
            observed_at_ms: 1,
        })
        .is_ok());
    assert_eq!(
        session.answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: phone,
            request_id: request,
            action_epoch: epoch,
            runtime_generation: session.runtime_generation(),
            observed_at_ms: 2,
        }),
        Err(SessionAdmitError::AlreadyResolved)
    );
}

#[test]
fn alternating_disconnect_mid_command_invalidates_queue() {
    let mut session = ConnectSession::new(task_id(0x13));
    let desktop = client_id(0x27);
    session.connect_client(desktop);
    let queued = live_input(&session, desktop, command_id(0x37), operation_id(0x47), 8);
    session.enqueue(queued).expect("queue");
    session.disconnect_client(desktop);
    let mut presence = EphemeralPresence::default();
    assert_eq!(
        session.admit(queued, &mut presence),
        Err(SessionAdmitError::ClientDisconnected)
    );
    session.connect_client(desktop);
    assert_eq!(
        session.admit(
            live_input(&session, desktop, command_id(0x37), operation_id(0x47), 9),
            &mut presence,
        ),
        Err(SessionAdmitError::QueueInvalidated)
    );
}

#[test]
fn alternating_stale_after_provider_restart() {
    let mut session = ConnectSession::new(task_id(0x14));
    let desktop = client_id(0x28);
    session.connect_client(desktop);
    let prior = session.runtime_generation();
    session.restart_provider();
    assert_eq!(
        session.answer(ActionAnswer {
            task_id: session.task_id(),
            client_id: desktop,
            request_id: request_id(0x62),
            action_epoch: ActionEpoch::new(1).expect("epoch"),
            runtime_generation: prior,
            observed_at_ms: 4,
        }),
        Err(SessionAdmitError::StaleGeneration)
    );
}

#[test]
fn alternating_optimistic_echoes_reconcile_by_command_id() {
    let mut session = ConnectSession::new(task_id(0x15));
    let desktop = client_id(0x29);
    let phone = client_id(0x2a);
    session.connect_client(desktop);
    session.connect_client(phone);
    let mut presence = EphemeralPresence::default();
    let first = command_id(0x38);
    let second = command_id(0x39);
    session
        .admit(
            live_input(&session, desktop, first, operation_id(0x48), 1),
            &mut presence,
        )
        .unwrap();
    session
        .admit(
            live_input(&session, phone, second, operation_id(0x49), 2),
            &mut presence,
        )
        .unwrap();
    assert_eq!(session.reconcile_echo(second), Some(operation_id(0x49)));
    assert_eq!(session.reconcile_echo(first), Some(operation_id(0x48)));
    assert_eq!(session.reconcile_echo(command_id(0x3a)), None);
}

#[test]
fn alternating_stale_focus_is_rejected_without_refresh_contract() {
    let mut session = ConnectSession::new(task_id(0x16));
    let desktop = client_id(0x2b);
    session.connect_client(desktop);
    let mut stale = live_input(&session, desktop, command_id(0x3b), operation_id(0x4b), 3);
    session.switch_focus();
    let mut presence = EphemeralPresence::default();
    assert_eq!(
        session.admit(stale, &mut presence),
        Err(SessionAdmitError::StaleFocus)
    );
    stale.focus_epoch = session.focus_epoch();
    stale.turn_epoch = session.turn_epoch();
    stale.expected_revision = Some(session.revision());
    assert!(session.admit(stale, &mut presence).is_ok());
}

#[test]
fn personal_local_only_until_enrolled_and_unknown_fields_deny() {
    let task = task_id(0x70);
    let enrollment = ConnectEnrollment::default();
    assert_eq!(
        project_field("mystery", "x", task, &enrollment, None),
        Err(ProjectionDenyReason::UnknownField)
    );
    assert_eq!(
        project_field("safe_title", "ok", task, &enrollment, None),
        Err(ProjectionDenyReason::PersonalNotEnrolled)
    );
}

#[test]
fn managed_metadata_projects_without_raw_transcript() {
    let task = task_id(0x71);
    let mut enrollment = ConnectEnrollment::default();
    enrollment.enroll(task);
    let allowed = BTreeSet::from([
        ContentClass::TaskMetadata,
        ContentClass::Presence,
        ContentClass::OperationProgress,
    ]);
    let grant = ProjectionGrant {
        allowed_content: &allowed,
        raw_content: false,
    };
    assert!(project_field("attention_kind", "needs_input", task, &enrollment, Some(grant)).is_ok());
    assert_eq!(
        project_field("transcript", "RAW_TRANSCRIPT", task, &enrollment, Some(grant)),
        Err(ProjectionDenyReason::RawContentDenied)
    );
}

#[test]
fn raw_content_grant_is_explicit_and_still_denies_personal_prompts() {
    let task = task_id(0x72);
    let mut enrollment = ConnectEnrollment::default();
    enrollment.enroll(task);
    let allowed = BTreeSet::from([ContentClass::Transcript, ContentClass::TaskMetadata]);
    let grant = ProjectionGrant {
        allowed_content: &allowed,
        raw_content: true,
    };
    assert!(project_field("transcript", "body", task, &enrollment, Some(grant)).is_ok());
    assert_eq!(
        project_field("personal_prompt", "secret", task, &enrollment, Some(grant)),
        Err(ProjectionDenyReason::LocalOnly)
    );
}

#[test]
fn sanitized_push_never_includes_raw_content() {
    let task = task_id(0x73);
    let mut enrollment = ConnectEnrollment::default();
    enrollment.enroll(task);
    let host = PinnedHostPublicId::from_bytes(*task.as_bytes());
    let push = sanitize_push(
        host,
        task,
        AttentionKind::NeedsInput,
        42,
        "/connect/tasks/opaque",
        None,
        PushPolicy::metadata_only(),
        &enrollment,
    )
    .expect("sanitized");
    assert!(push.safe_title.is_none());
    assert!(forbidden_push_fields().contains(&OutboundField::Transcript));
    assert!(forbidden_push_fields().contains(&OutboundField::PromptBody));
    assert!(sanitize_push(
        host,
        task,
        AttentionKind::Completed,
        43,
        "/connect/tasks/opaque?prompt=RAW",
        Some("diff --git a/file"),
        PushPolicy::allow_safe_title(),
        &enrollment,
    )
    .is_err());
}

#[test]
fn stale_bundle_and_protocol_incompatibility_pause_without_rotating_keys() {
    let pairing = PairingContinuity {
        pairing_code_generation: 4,
        host_identity_fingerprint: "host-fpr".into(),
        device_key_fingerprint: "device-fpr".into(),
    };
    let mut continuity = UpdateContinuity::compatible("bundle-1", pairing.clone());
    continuity.preserve_draft("unsent composer text");
    assert_eq!(
        continuity.observe_peer(CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR, "bundle-2"),
        Err(UpdateContinuityError::BundleStale)
    );
    assert_eq!(
        continuity.admit_mutation(),
        Err(UpdateContinuityError::MutationsPaused)
    );
    assert_eq!(
        continuity.local_draft.as_deref(),
        Some("unsent composer text")
    );
    assert!(continuity.reconnect_same_identity(&pairing));
    assert!(!continuity.rotated_pairing(&pairing));
    assert!(!continuity.rotated_device_key(&pairing));

    let mut protocol = UpdateContinuity::compatible("bundle-1", pairing.clone());
    assert_eq!(
        protocol.observe_peer(CONNECT_PROTOCOL_MAJOR + 1, 0, "bundle-1"),
        Err(UpdateContinuityError::ProtocolIncompatible)
    );
    assert!(protocol.reload_required);
}

#[test]
fn desktop_update_and_host_reconnect_preserve_pairing_and_device_key() {
    let pairing = PairingContinuity {
        pairing_code_generation: 9,
        host_identity_fingerprint: "stable-host".into(),
        device_key_fingerprint: "stable-device".into(),
    };
    let continuity = UpdateContinuity::compatible("bundle-stable", pairing.clone());
    assert!(continuity.reconnect_same_identity(&pairing));
    assert!(!continuity.rotated_pairing(&PairingContinuity {
        pairing_code_generation: 9,
        host_identity_fingerprint: "stable-host".into(),
        device_key_fingerprint: "stable-device".into(),
    }));
}

#[test]
fn explicit_manual_pairing_code_rotation_is_detectable_and_not_implied_by_update() {
    let before = PairingContinuity {
        pairing_code_generation: 1,
        host_identity_fingerprint: "host".into(),
        device_key_fingerprint: "device".into(),
    };
    let continuity = UpdateContinuity::compatible("bundle", before.clone());
    let manual = PairingContinuity {
        pairing_code_generation: 2,
        host_identity_fingerprint: "host".into(),
        device_key_fingerprint: "device".into(),
    };
    assert!(continuity.rotated_pairing(&manual));
    assert!(!continuity.rotated_device_key(&manual));
    assert!(!continuity.rotated_pairing(&before));
}

#[test]
fn failure_matrix_direct_and_hosted_never_reach_kernel_on_tamper_or_replay() {
    assert!(matrix_covers_direct_and_hosted());
    for case in FAILURE_MATRIX {
        match case.fault {
            FailureClass::TamperedFrame | FailureClass::ReplayedFrame => {
                assert_eq!(
                    simulate_fault(case),
                    SimulatedFaultOutcome::RejectedBeforeKernel
                );
            }
            FailureClass::SnapshotChunkInterrupt | FailureClass::RelayRestart => {
                assert_eq!(simulate_fault(case), SimulatedFaultOutcome::HeldForResync);
            }
            _ => {
                let _ = simulate_fault(case);
            }
        }
    }
    let empty: BTreeMap<&str, &str> = BTreeMap::new();
    assert!(empty.is_empty());
}
