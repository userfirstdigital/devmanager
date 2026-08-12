//! Focused Connect invitation authority proofs.

use devmanager::connect::ActionId;
use devmanager::connect::{
    ContentClass, InviteError, InviteRole, InviteUsePolicy, PinnedHostPublicId,
    RedeemedDevicePublicId, TaskInviteStore, INVITE_SECRET_BYTES, MAX_INVITE_LIFETIME_MS,
};
use devmanager::domain::id::TaskId;
use devmanager::domain::task::TaskLifecycle;

fn secret() -> [u8; INVITE_SECRET_BYTES] {
    [0x5A; INVITE_SECRET_BYTES]
}

#[test]
fn short_secret_and_lifetime_boundaries_reject() {
    let mut store = TaskInviteStore::new();
    let task = TaskId::new();
    let host = PinnedHostPublicId::from_bytes([1; 16]);
    assert_eq!(
        store.issue(
            task,
            "guest",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            10,
            20,
            host,
            b"too-short",
            None,
        ),
        Err(InviteError::SecretTooShort)
    );
    assert_eq!(
        store.issue(
            task,
            "guest",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            0,
            20,
            host,
            &secret(),
            None,
        ),
        Err(InviteError::InvalidLifetime)
    );
    assert_eq!(
        store.issue(
            task,
            "guest",
            InviteRole::Watcher,
            InviteUsePolicy::SingleUse,
            10,
            10 + MAX_INVITE_LIFETIME_MS + 1,
            host,
            &secret(),
            None,
        ),
        Err(InviteError::InvalidLifetime)
    );
}

#[test]
fn expiry_revocation_and_cross_device_authorization_fail_closed() {
    let mut store = TaskInviteStore::new();
    let task = TaskId::new();
    let host = PinnedHostPublicId::from_bytes([2; 16]);
    let device_a = RedeemedDevicePublicId::from_bytes([3; 16]);
    let device_b = RedeemedDevicePublicId::from_bytes([4; 16]);
    let issued = store
        .issue(
            task,
            "review",
            InviteRole::Collaborator,
            InviteUsePolicy::SingleUse,
            100,
            200,
            host,
            &secret(),
            None,
        )
        .unwrap();
    let grant = store
        .redeem(
            issued.invite_id,
            &secret(),
            task,
            TaskLifecycle::Open,
            150,
            host,
            device_a,
        )
        .unwrap();
    assert_eq!(grant.bound_device, Some(device_a));
    assert!(store
        .authorize(
            issued.invite_id,
            task,
            TaskLifecycle::Open,
            150,
            ActionId::READ_TASK,
            ContentClass::TaskMetadata,
            device_a,
        )
        .is_ok());
    assert!(store
        .grant(issued.invite_id, TaskLifecycle::Open, 150, device_a)
        .is_some());
    assert!(store
        .grant(issued.invite_id, TaskLifecycle::Open, 150, device_b)
        .is_none());
    assert!(store
        .grant(issued.invite_id, TaskLifecycle::Open, 200, device_a)
        .is_none());
    assert_eq!(
        store.authorize(
            issued.invite_id,
            task,
            TaskLifecycle::Open,
            150,
            ActionId::READ_TASK,
            ContentClass::TaskMetadata,
            device_b,
        ),
        Err(InviteError::DeviceAlreadyBound)
    );
    assert_eq!(
        store.authorize(
            issued.invite_id,
            task,
            TaskLifecycle::Open,
            201,
            ActionId::READ_TASK,
            ContentClass::TaskMetadata,
            device_a,
        ),
        Err(InviteError::Expired)
    );
    store.revoke(issued.invite_id, 160).unwrap();
    assert_eq!(
        store.authorize(
            issued.invite_id,
            task,
            TaskLifecycle::Open,
            160,
            ActionId::READ_TASK,
            ContentClass::TaskMetadata,
            device_a,
        ),
        Err(InviteError::Revoked)
    );
    assert!(store
        .grant(issued.invite_id, TaskLifecycle::Open, 160, device_a)
        .is_none());
    assert!(!store.has_reusable_plaintext_secret(issued.invite_id));
}
