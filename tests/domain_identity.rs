use std::path::PathBuf;
use std::str::FromStr;

use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
use devmanager::domain::id::{
    AgentSessionId, ArtifactId, BrowserContextId, ClientId, CommandId, EnvironmentId, EventId,
    IdError, OperationId, ProjectId, RequestId, ResourceId, ServiceId, SubscriptionId, TaskId,
    TerminalId, TransferId,
};
use devmanager::domain::operation::{OperationFacts, OperationLifecycle};
use devmanager::domain::resource::{
    OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe,
};
use devmanager::domain::task::{
    TaskAssignment, TaskFacts, TaskLifecycle, TaskValidationError, WorkspaceRef,
};
use static_assertions::assert_not_impl_any;

assert_not_impl_any!(TaskId: From<AgentSessionId>, From<ArtifactId>, From<ResourceId>);
assert_not_impl_any!(AgentSessionId: From<TaskId>, From<ResourceId>, From<ClientId>);
assert_not_impl_any!(ArtifactId: From<TaskId>, From<ResourceId>, From<OperationId>);
assert_not_impl_any!(ResourceId: From<TaskId>, From<ArtifactId>, From<TransferId>);
assert_not_impl_any!(ClientId: From<CommandId>, From<EventId>, From<RequestId>);
assert_not_impl_any!(OperationId: From<TransferId>, From<CommandId>, From<EventId>);
assert_not_impl_any!(EnvironmentId: From<ProjectId>, From<TaskId>);
assert_not_impl_any!(TerminalId: From<BrowserContextId>, From<ServiceId>);
assert_not_impl_any!(RequestId: From<SubscriptionId>, From<EventId>);

fn uuid_version(bytes: &[u8; 16]) -> u8 {
    (bytes[6] >> 4) & 0x0f
}

fn uuid_variant_rfc(bytes: &[u8; 16]) -> bool {
    // RFC 4122 / RFC 9562 variant: the two MSBs of byte 8 must be 0b10.
    (bytes[8] & 0xc0) == 0x80
}

#[test]
fn typed_ids_generate_uuid_version_7() {
    let id = TaskId::new();
    assert_eq!(uuid_version(id.as_bytes()), 7);

    let ids = [
        *EnvironmentId::new().as_bytes(),
        *ProjectId::new().as_bytes(),
        *AgentSessionId::new().as_bytes(),
        *ArtifactId::new().as_bytes(),
        *ResourceId::new().as_bytes(),
        *TerminalId::new().as_bytes(),
        *BrowserContextId::new().as_bytes(),
        *ServiceId::new().as_bytes(),
        *ClientId::new().as_bytes(),
        *CommandId::new().as_bytes(),
        *RequestId::new().as_bytes(),
        *OperationId::new().as_bytes(),
        *TransferId::new().as_bytes(),
        *SubscriptionId::new().as_bytes(),
        *EventId::new().as_bytes(),
    ];
    for bytes in ids {
        assert_eq!(uuid_version(&bytes), 7);
    }
}

#[test]
fn typed_ids_serde_round_trip() {
    let id = TaskId::new();
    let json = serde_json::to_string(&id).expect("serialize TaskId");
    assert!(
        json.starts_with('"') && json.ends_with('"'),
        "serialization must remain a transparent UUID string: {json}"
    );
    let restored: TaskId = serde_json::from_str(&json).expect("deserialize TaskId");
    assert_eq!(id.as_bytes(), restored.as_bytes());
}

#[test]
fn typed_ids_serde_rejects_non_version_7() {
    // RFC 4122 UUID version 4 serialized as a UUID string (current wire shape).
    const UUID_V4_JSON: &str = "\"550e8400-e29b-41d4-a716-446655440000\"";
    let err = serde_json::from_str::<TaskId>(UUID_V4_JSON)
        .expect_err("v4 UUID must not deserialize into TaskId");
    let message = err.to_string();
    assert!(
        message.contains("version") || message.contains("UUID"),
        "unexpected serde error: {message}"
    );
}

#[test]
fn typed_ids_reject_invalid_rfc_variant_even_with_version_7_nibble() {
    // Version nibble 7, but variant bits are Microsoft (0b110x), not RFC 0b10xx.
    // Layout: time_hi_and_version = 0x7c3d, clock_seq_hi = 0xc0 (variant 0b11).
    const BAD_VARIANT_V7: &str = "018f60b0-9c1a-7c3d-c012-3456789abcde";
    let mut bad_bytes = [0u8; 16];
    bad_bytes.copy_from_slice(&[
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x7c, 0x3d, 0xc0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
        0xde,
    ]);
    assert_eq!(uuid_version(&bad_bytes), 7);
    assert!(!uuid_variant_rfc(&bad_bytes));
    assert_eq!(
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            u32::from_be_bytes(bad_bytes[0..4].try_into().unwrap()),
            u16::from_be_bytes(bad_bytes[4..6].try_into().unwrap()),
            u16::from_be_bytes(bad_bytes[6..8].try_into().unwrap()),
            u16::from_be_bytes(bad_bytes[8..10].try_into().unwrap()),
            {
                let mut n = 0u64;
                for b in &bad_bytes[10..16] {
                    n = (n << 8) | u64::from(*b);
                }
                n
            }
        ),
        BAD_VARIANT_V7
    );

    assert!(matches!(
        TaskId::parse(BAD_VARIANT_V7),
        Err(IdError::InvalidVariant)
    ));
    assert!(matches!(
        TaskId::from_bytes(bad_bytes),
        Err(IdError::InvalidVariant)
    ));
    let json = format!("\"{BAD_VARIANT_V7}\"");
    assert!(
        serde_json::from_str::<TaskId>(&json).is_err(),
        "serde must reject invalid RFC variant"
    );
}

#[test]
fn typed_ids_display_parse_round_trip() {
    let id = ArtifactId::new();
    let text = id.to_string();
    let parsed = ArtifactId::parse(&text).expect("parse display text");
    assert_eq!(id.as_bytes(), parsed.as_bytes());

    let from_str = ArtifactId::from_str(&text).expect("FromStr");
    assert_eq!(id.as_bytes(), from_str.as_bytes());
}

#[test]
fn typed_ids_reject_non_version_7() {
    // RFC 4122 UUID version 4 (version nibble = 4).
    const UUID_V4: &str = "550e8400-e29b-41d4-a716-446655440000";
    // UUID version 1 (version nibble = 1).
    const UUID_V1: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";

    assert!(matches!(
        TaskId::parse(UUID_V4),
        Err(IdError::InvalidVersion)
    ));
    assert!(matches!(
        ClientId::parse(UUID_V1),
        Err(IdError::InvalidVersion)
    ));
    assert!(matches!(
        OperationId::parse("not-a-uuid"),
        Err(IdError::InvalidFormat)
    ));
}

#[test]
fn typed_ids_from_bytes_round_trip_and_reject_invalid_version() {
    let id = ResourceId::new();
    let restored = ResourceId::from_bytes(*id.as_bytes()).expect("from_bytes");
    assert_eq!(id.as_bytes(), restored.as_bytes());

    // Craft a UUID v4 byte layout (version nibble = 4).
    let mut v4_bytes = *id.as_bytes();
    v4_bytes[6] = (v4_bytes[6] & 0x0f) | 0x40;
    assert!(matches!(
        ResourceId::from_bytes(v4_bytes),
        Err(IdError::InvalidVersion)
    ));
}

#[test]
fn task_facts_construct_with_validated_fields() {
    let workspace =
        WorkspaceRef::worktree(PathBuf::from(r"C:\code\proj\.worktrees\a"), "feature/x")
            .expect("workspace");
    let assignment =
        TaskAssignment::external_principal("org", "user@example.com").expect("assignment");

    let facts = TaskFacts::new(
        EnvironmentId::new(),
        "Ship kernel slice",
        Some("Typed identities first".into()),
        ProjectId::new(),
        workspace,
        assignment,
        1_725_000_000_000,
    )
    .expect("task facts");

    assert_eq!(facts.title, "Ship kernel slice");
    assert_eq!(facts.description.as_deref(), Some("Typed identities first"));
    assert_eq!(facts.lifecycle, TaskLifecycle::Open);
    assert_eq!(facts.action_epoch, 0);
    assert_eq!(facts.revision, 0);
    assert_eq!(facts.created_at_ms, 1_725_000_000_000);
    assert!(matches!(facts.workspace, WorkspaceRef::Worktree { .. }));
    assert!(matches!(
        facts.assignment,
        TaskAssignment::ExternalPrincipal { .. }
    ));
}

#[test]
fn task_facts_reject_invalid_title_and_description() {
    let workspace = WorkspaceRef::Main;
    let assignment = TaskAssignment::LocalOwner;

    let empty_title = TaskFacts::new(
        EnvironmentId::new(),
        "   ",
        None,
        ProjectId::new(),
        workspace.clone(),
        assignment.clone(),
        0,
    );
    assert!(matches!(empty_title, Err(TaskValidationError::EmptyTitle)));

    let empty_description = TaskFacts::new(
        EnvironmentId::new(),
        "Valid",
        Some("  ".into()),
        ProjectId::new(),
        workspace,
        assignment,
        0,
    );
    assert!(matches!(
        empty_description,
        Err(TaskValidationError::EmptyDescription)
    ));
}

#[test]
fn workspace_and_assignment_validate_paths_and_principals() {
    assert!(matches!(
        WorkspaceRef::worktree("", "main"),
        Err(TaskValidationError::EmptyPath)
    ));
    assert!(matches!(
        WorkspaceRef::worktree(r"C:\code\proj", ""),
        Err(TaskValidationError::EmptyBranch)
    ));
    assert!(matches!(
        WorkspaceRef::external(""),
        Err(TaskValidationError::EmptyPath)
    ));
    assert!(matches!(
        TaskAssignment::external_principal(" ", "subject"),
        Err(TaskValidationError::EmptyPrincipalAuthority)
    ));
    assert!(matches!(
        TaskAssignment::external_principal("auth", ""),
        Err(TaskValidationError::EmptyPrincipalSubject)
    ));

    let main = WorkspaceRef::Main;
    assert!(matches!(main, WorkspaceRef::Main));
    let local = TaskAssignment::LocalOwner;
    assert!(matches!(local, TaskAssignment::LocalOwner));
}

#[test]
fn agent_artifact_resource_and_operation_facts_bind_ownership() {
    let task_id = TaskId::new();

    let agent = AgentSessionFacts::new(
        task_id,
        AgentRole::Primary,
        "claude",
        Some("provider-session-1".into()),
    )
    .expect("agent facts");
    assert_eq!(agent.task_id.as_bytes(), task_id.as_bytes());
    assert_eq!(agent.lifecycle, AgentSessionLifecycle::Open);
    assert_eq!(agent.runtime_generation, 0);
    assert_eq!(agent.revision, 0);

    let artifact = ArtifactFacts::new(
        task_id,
        ArtifactKind::Finding,
        "security-notes",
        ArtifactContentRef::InlineUtf8("note".into()),
        [0u8; 32],
        PrivacyClass::LocalOnly,
        100,
    )
    .expect("artifact facts");
    assert_eq!(artifact.task_id.as_bytes(), task_id.as_bytes());
    assert_eq!(artifact.label, "security-notes");

    let resource = ResourceFacts::new(
        Some(task_id),
        OwnerKind::Task,
        ResourceKind::Terminal,
        ResourceRecipe::Terminal {
            cols: 120,
            rows: 40,
        },
        200,
    )
    .expect("resource facts");
    assert_eq!(
        resource.task_id.as_ref().map(|id| *id.as_bytes()),
        Some(*task_id.as_bytes())
    );
    assert_eq!(resource.lifecycle, ResourceLifecycle::Active);
    assert_eq!(resource.runtime_generation, 0);

    let operation = OperationFacts::accepted(CommandId::new(), Some(task_id), 300);
    assert_eq!(operation.state, OperationLifecycle::Accepted);
    assert_eq!(operation.accepted_at_ms, 300);
}

#[test]
fn agent_artifact_resource_reject_invalid_labels_and_providers() {
    let task_id = TaskId::new();

    assert!(AgentSessionFacts::new(task_id, AgentRole::Primary, "  ", None).is_err());
    assert!(ArtifactFacts::new(
        task_id,
        ArtifactKind::Finding,
        "",
        ArtifactContentRef::InlineUtf8("x".into()),
        [0u8; 32],
        PrivacyClass::LocalOnly,
        0,
    )
    .is_err());
    assert!(ResourceFacts::new(
        None,
        OwnerKind::Host,
        ResourceKind::Terminal,
        ResourceRecipe::Terminal { cols: 0, rows: 0 },
        0,
    )
    .is_err());
}
