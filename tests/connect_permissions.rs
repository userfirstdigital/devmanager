//! Focused Connect role/policy fail-closed proofs.

use devmanager::connect::{
    ActionId, AuthoritativePermissionContext, ConnectRole, PermissionDecision,
    PermissionDenyReason, PermissionEvaluator, PermissionRequest, ScopedPermissionGrant,
    SessionAuthorizer,
};
use devmanager::domain::id::TaskId;

#[test]
fn watcher_and_collaborator_require_scoped_grants_and_deny_owner_only() {
    let task_id = TaskId::new();
    let watcher = SessionAuthorizer::watcher(task_id);
    let collaborator = SessionAuthorizer::collaborator(task_id);
    let request = PermissionRequest {
        role: ConnectRole::Watcher { task_id },
        task_id: Some(task_id),
        action: ActionId::READ_TASK,
        credential: None,
    };
    assert_eq!(
        watcher.authorize(request.clone()),
        PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
    );
    let context = AuthoritativePermissionContext::live(1, 2, 3).unwrap();
    let grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id },
        task_id,
        ActionId::READ_TASK,
        context,
    )
    .unwrap();
    assert_eq!(
        watcher.authorize_with_grant(request, &grant, context),
        PermissionDecision::Allow
    );
    assert_eq!(
        collaborator.authorize(PermissionRequest {
            role: ConnectRole::Collaborator { task_id },
            task_id: Some(task_id),
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
    );
    let dangerous = ScopedPermissionGrant::issue(
        ConnectRole::Collaborator { task_id },
        task_id,
        ActionId::APPROVE_DANGEROUS,
        context,
    )
    .unwrap();
    assert_eq!(
        collaborator.authorize_with_grant(
            PermissionRequest {
                role: ConnectRole::Collaborator { task_id },
                task_id: Some(task_id),
                action: ActionId::APPROVE_DANGEROUS,
                credential: None,
            },
            &dangerous,
            context,
        ),
        PermissionDecision::Denied(PermissionDenyReason::OwnerOnly)
    );
}

#[test]
fn unknown_actions_and_cross_task_grants_deny() {
    let task_a = TaskId::new();
    let task_b = TaskId::new();
    assert_eq!(
        PermissionEvaluator::default().evaluate(PermissionRequest {
            role: ConnectRole::Watcher { task_id: task_a },
            task_id: Some(task_a),
            action: ActionId::new(99).unwrap(),
            credential: None,
        }),
        PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
    );
    let context = AuthoritativePermissionContext::live(4, 5, 6).unwrap();
    let grant = ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task_a },
        task_a,
        ActionId::new(99).unwrap(),
        context,
    )
    .unwrap();
    assert_eq!(
        PermissionEvaluator::default().evaluate_with_scoped_grant(
            PermissionRequest {
                role: ConnectRole::Watcher { task_id: task_a },
                task_id: Some(task_a),
                action: ActionId::new(99).unwrap(),
                credential: None,
            },
            &grant,
            context,
        ),
        PermissionDecision::Denied(PermissionDenyReason::UnknownAction)
    );
    assert!(ScopedPermissionGrant::issue(
        ConnectRole::Watcher { task_id: task_a },
        task_b,
        ActionId::READ_TASK,
        context,
    )
    .is_none());
}
