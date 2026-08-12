use uuid::Uuid;

use devmanager::connect::{
    guest_may_perform, ActionId, AuthoritativePermissionContext, ConnectRole, ConnectSession,
    ContentClass, DeviceInput, EphemeralPresence, InviteError, InviteRole, InviteUsePolicy,
    KnownAction, PermissionDecision, PermissionDenyReason, PermissionEvaluator, PermissionRequest,
    PinnedHostPublicId, RedeemedDevicePublicId, ScopedPermissionGrant, SessionAdmitError,
    TaskInviteStore,
};
use devmanager::domain::id::{ClientId, CommandId, OperationId, TaskId};
use devmanager::domain::task::TaskLifecycle;

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

fn host(tail: u8) -> PinnedHostPublicId {
    PinnedHostPublicId::from_bytes(fixed_uuid_v7(tail).into_bytes())
}

fn device(tail: u8) -> RedeemedDevicePublicId {
    RedeemedDevicePublicId::from_bytes(fixed_uuid_v7(tail).into_bytes())
}

#[test]
fn task_invite_is_separate_from_pairing_and_hides_collaboration_until_created() {
    let mut store = TaskInviteStore::new();
    assert!(!store.collaboration_visible());
    let task = task_id(0x11);
    assert_eq!(
        store
            .issue(
                task,
                "guest",
                InviteRole::Watcher,
                InviteUsePolicy::SingleUse,
                1,
                1_000,
                host(0x01),
                b"secret-bytes",
                Some("PAIRCODE"),
            )
            .unwrap_err(),
        InviteError::PairingCodeReuseForbidden
    );
    let issued = store
        .issue(
            task,
            "design-review",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            1,
            1_000,
            host(0x01),
            b"secret-bytes",
            None,
        )
        .expect("issue");
    assert!(store.collaboration_visible());
    assert!(!store.has_reusable_plaintext_secret(issued.invite_id));
    assert_eq!(store.grant(issued.invite_id).unwrap().nickname, "design-review");
}

#[test]
fn nickname_expiry_and_single_use_multi_use_policy() {
    let mut store = TaskInviteStore::new();
    let task = task_id(0x12);
    let host = host(0x02);
    assert_eq!(
        store
            .issue(
                task,
                "   ",
                InviteRole::Watcher,
                InviteUsePolicy::SingleUse,
                1,
                10,
                host,
                b"x",
                None,
            )
            .unwrap_err(),
        InviteError::NicknameRequired
    );
    let single = store
        .issue(
            task,
            "once",
            InviteRole::Collaborator,
            InviteUsePolicy::SingleUse,
            1,
            50,
            host,
            b"one",
            None,
        )
        .unwrap();
    store
        .redeem(
            single.invite_id,
            b"one",
            task,
            TaskLifecycle::Open,
            2,
            host,
            device(0x21),
        )
        .unwrap();
    assert_eq!(
        store
            .redeem(
                single.invite_id,
                b"one",
                task,
                TaskLifecycle::Open,
                3,
                host,
                device(0x21),
            )
            .unwrap_err(),
        InviteError::AlreadyRedeemed
    );
    let multi = store
        .issue(
            task,
            "many",
            InviteRole::Watcher,
            InviteUsePolicy::MultiUse { max_redemptions: 2 },
            1,
            50,
            host,
            b"two",
            None,
        )
        .unwrap();
    store
        .redeem(
            multi.invite_id,
            b"two",
            task,
            TaskLifecycle::Open,
            4,
            host,
            device(0x22),
        )
        .unwrap();
    store
        .redeem(
            multi.invite_id,
            b"two",
            task,
            TaskLifecycle::Open,
            5,
            host,
            device(0x22),
        )
        .unwrap();
    assert_eq!(
        store
            .redeem(
                multi.invite_id,
                b"two",
                task,
                TaskLifecycle::Open,
                6,
                host,
                device(0x22),
            )
            .unwrap_err(),
        InviteError::RedemptionExhausted
    );
    assert_eq!(
        store
            .authorize(
                multi.invite_id,
                task,
                TaskLifecycle::Open,
                51,
                ActionId::READ_TASK,
                ContentClass::TaskMetadata,
            )
            .unwrap_err(),
        InviteError::Expired
    );
}

#[test]
fn watcher_is_read_only_and_collaborator_capabilities_are_task_scoped() {
    assert!(!guest_may_perform(InviteRole::Watcher, KnownAction::MutateTask));
    assert!(guest_may_perform(InviteRole::Collaborator, KnownAction::SendPrompt));
    assert!(!guest_may_perform(
        InviteRole::Collaborator,
        KnownAction::ApproveDangerous
    ));
    assert!(!guest_may_perform(
        InviteRole::Watcher,
        KnownAction::ReadPersonalPrompts
    ));

    let task = task_id(0x13);
    let other = task_id(0x14);
    let evaluator = PermissionEvaluator::default();
    let context = AuthoritativePermissionContext::live(1, 2, 3).expect("epochs");
    let watcher_grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task },
        task,
        ActionId::MUTATE_TASK,
        context,
    )
    .unwrap();
    assert_eq!(
        evaluator.evaluate_with_scoped_grant(
            PermissionRequest {
                role: ConnectRole::Watcher { task_id: task },
                task_id: Some(task),
                action: ActionId::MUTATE_TASK,
                credential: None,
            },
            &watcher_grant,
            context,
        ),
        PermissionDecision::Denied(PermissionDenyReason::WatcherReadOnly)
    );
    let collaborator_grant = ScopedPermissionGrant::issue(
        ConnectRole::Collaborator { task_id: task },
        task,
        ActionId::MUTATE_TASK,
        context,
    )
    .unwrap();
    assert_eq!(
        evaluator.evaluate_with_scoped_grant(
            PermissionRequest {
                role: ConnectRole::Collaborator { task_id: task },
                task_id: Some(task),
                action: ActionId::MUTATE_TASK,
                credential: None,
            },
            &collaborator_grant,
            context,
        ),
        PermissionDecision::Allow
    );
    assert!(ScopedPermissionGrant::issue(
        ConnectRole::Collaborator { task_id: other },
        task,
        ActionId::MUTATE_TASK,
        context,
    )
    .is_none());
}

#[test]
fn owner_only_dangerous_approval_and_personal_prompts_stay_owner_only() {
    let task = task_id(0x15);
    let evaluator = PermissionEvaluator::default();
    let context = AuthoritativePermissionContext::live(8, 9, 10).expect("epochs");
    for action in [ActionId::APPROVE_DANGEROUS, ActionId::READ_PERSONAL_PROMPTS] {
        let grant = ScopedPermissionGrant::issue(
            ConnectRole::Collaborator { task_id: task },
            task,
            action,
            context,
        )
        .unwrap();
        assert_eq!(
            evaluator.evaluate_with_scoped_grant(
                PermissionRequest {
                    role: ConnectRole::Collaborator { task_id: task },
                    task_id: Some(task),
                    action,
                    credential: None,
                },
                &grant,
                context,
            ),
            PermissionDecision::Denied(PermissionDenyReason::OwnerOnly)
        );
    }
}

#[test]
fn revoked_guest_and_closed_task_lose_access_without_touching_owner_devices() {
    let mut store = TaskInviteStore::new();
    let task = task_id(0x16);
    let host = host(0x03);
    let issued = store
        .issue(
            task,
            "temp",
            InviteRole::Collaborator,
            InviteUsePolicy::MultiUse { max_redemptions: 4 },
            1,
            9_000,
            host,
            b"keep",
            None,
        )
        .unwrap();
    store
        .redeem(
            issued.invite_id,
            b"keep",
            task,
            TaskLifecycle::Open,
            2,
            host,
            device(0x31),
        )
        .unwrap();
    store.revoke(issued.invite_id, 3).unwrap();
    assert_eq!(
        store
            .authorize(
                issued.invite_id,
                task,
                TaskLifecycle::Open,
                4,
                ActionId::MUTATE_TASK,
                ContentClass::TaskMetadata,
            )
            .unwrap_err(),
        InviteError::Revoked
    );
    assert!(!store.has_reusable_plaintext_secret(issued.invite_id));
    assert!(store
        .audit_events()
        .iter()
        .any(|event| matches!(event.kind, devmanager::connect::InviteAuditKind::Revoked)));

    let open = store
        .issue(
            task,
            "close-me",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            1,
            9_000,
            host,
            b"closed",
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .authorize(
                open.invite_id,
                task,
                TaskLifecycle::Archived,
                5,
                ActionId::READ_TASK,
                ContentClass::TaskMetadata,
            )
            .unwrap_err(),
        InviteError::TaskClosed
    );
}

#[test]
fn guests_cannot_see_other_tasks_prompts_config_devices_or_secrets() {
    let mut store = TaskInviteStore::new();
    let task = task_id(0x17);
    let other = task_id(0x18);
    let host = host(0x04);
    let issued = store
        .issue(
            task,
            "narrow",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            1,
            9_000,
            host,
            b"narrow",
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .authorize(
                issued.invite_id,
                other,
                TaskLifecycle::Open,
                2,
                ActionId::READ_TASK,
                ContentClass::TaskMetadata,
            )
            .unwrap_err(),
        InviteError::TaskScopeMismatch
    );
    for content in [
        ContentClass::PersonalPrompts,
        ContentClass::Configuration,
        ContentClass::PairedDevices,
        ContentClass::Secrets,
        ContentClass::Transcript,
    ] {
        assert_eq!(
            store
                .authorize(
                    issued.invite_id,
                    task,
                    TaskLifecycle::Open,
                    2,
                    ActionId::READ_TASK,
                    content,
                )
                .unwrap_err(),
            InviteError::TaskScopeMismatch
        );
    }
}

#[test]
fn revocation_invalidates_guest_queue_and_not_owner_session() {
    let task = task_id(0x19);
    let guest = client_id(0x41);
    let owner = client_id(0x42);
    let mut session = ConnectSession::new(task);
    session.connect_client(guest);
    session.connect_client(owner);
    let queued = DeviceInput {
        task_id: task,
        client_id: guest,
        command_id: command_id(0x51),
        operation_id: operation_id(0x61),
        expected_revision: Some(session.revision()),
        resource_id: None,
        input_sequence: 1,
        turn_epoch: session.turn_epoch(),
        focus_epoch: session.focus_epoch(),
        observed_at_ms: 1,
    };
    session.enqueue(queued).unwrap();
    session.invalidate_queued_for_client(guest);
    session.disconnect_client(guest);
    let mut presence = EphemeralPresence::default();
    assert_eq!(
        session.admit(queued, &mut presence),
        Err(SessionAdmitError::ClientDisconnected)
    );
    let owner_ok = session
        .admit(
            DeviceInput {
                task_id: task,
                client_id: owner,
                command_id: command_id(0x52),
                operation_id: operation_id(0x62),
                expected_revision: Some(session.revision()),
                resource_id: None,
                input_sequence: 1,
                turn_epoch: session.turn_epoch(),
                focus_epoch: session.focus_epoch(),
                observed_at_ms: 2,
            },
            &mut presence,
        )
        .expect("owner still lives");
    assert!(!owner_ok.is_settled());
    assert_eq!(session.visible_controller(), None);
}

#[test]
fn host_pin_mismatch_and_wrong_secret_fail_closed() {
    let mut store = TaskInviteStore::new();
    let task = task_id(0x1a);
    let issued = store
        .issue(
            task,
            "pin",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            1,
            9_000,
            host(0x05),
            b"right",
            None,
        )
        .unwrap();
    assert_eq!(
        store
            .redeem(
                issued.invite_id,
                b"right",
                task,
                TaskLifecycle::Open,
                2,
                host(0x06),
                device(0x33),
            )
            .unwrap_err(),
        InviteError::HostMismatch
    );
    assert_eq!(
        store
            .redeem(
                issued.invite_id,
                b"wrong",
                task,
                TaskLifecycle::Open,
                2,
                host(0x05),
                device(0x33),
            )
            .unwrap_err(),
        InviteError::UnknownInvite
    );
}
