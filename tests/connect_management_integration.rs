use devmanager::connect::{
    ConnectHostId, EvidenceAccessClass, EvidenceAdapter, EvidenceBundle, LocalActionAdapter,
    LocalActionCatalogEntry, LocalActionKind, LocalActionReconcileState, LocalActionRequest,
    ManagedTaskAdapter, ManagedTaskSnapshot, OrganizationAdapter, OrganizationFact,
    OrganizationPromptAdapter, OrganizationPromptSnapshot, OrganizationSyncState, ReplayPolicy,
    SyncOutcome,
};
use devmanager::domain::id::{ProjectId, TaskId};
use devmanager::domain::org::TaskScope;
use devmanager::org::{
    compute_bundle_hash, ActionRisk, Admission, EnrollmentState, EvidenceMediaRef, EvidenceSegment,
    ExternalAccount, HostMembership, MembershipRole, MembershipStatus, OrgError,
    OrganizationPolicyDocument, OrganizationProjection, PortalAccountId, PortalTenantId,
};
use devmanager::prompts::{ComposerInsertion, OrgPrompt, OrgPromptVersion, PromptLifecycle};
use devmanager::protocol::CapabilitySet;
use sha2::{Digest, Sha256};

fn tenant() -> PortalTenantId {
    PortalTenantId::parse("acme").expect("tenant")
}

fn account() -> ExternalAccount {
    ExternalAccount::new(
        tenant(),
        PortalAccountId::parse("owner-1").expect("account"),
        None,
    )
}

fn policy() -> OrganizationPolicyDocument {
    OrganizationPolicyDocument::deny_minimal(tenant()).expect("policy")
}

fn enroll(projection: &mut OrganizationProjection, host_id: ConnectHostId) -> HostMembership {
    let account = account();
    assert_eq!(projection.sign_in(account.clone()), 0);
    let pending = HostMembership::pending(
        host_id,
        account,
        MembershipRole::Owner,
        &policy(),
        "owner-host",
    )
    .expect("pending");
    projection
        .confirm_enrollment(pending, policy(), 1_000)
        .expect("enrolled")
}

fn snapshot(
    membership: &HostMembership,
    task_id: TaskId,
    revision: u64,
    state: EnrollmentState,
) -> ManagedTaskSnapshot {
    ManagedTaskSnapshot {
        host_id: membership.host_id,
        local_task_id: task_id,
        board_card_id: devmanager::org::BoardCardId::parse("board-card-1").expect("card"),
        enrollment_state: state,
        portal_revision: revision,
        metadata_policy_version: membership.policy_revision,
        linked_by: "portal".to_string(),
        linked_at: 1_000,
        unlinked_at: None,
        link_id: devmanager::org::ManagedLinkId::new(),
        tenant_id: membership.tenant_id.clone(),
        portal_title: Some("Portal title".to_string()),
    }
}

fn hash_body(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[test]
fn standalone_remains_empty_and_fail_closed() {
    let mut adapter = OrganizationAdapter::standalone();
    assert_eq!(
        adapter.projection().sync_state(),
        OrganizationSyncState::Standalone
    );
    assert_eq!(adapter.projection().exported_task_count(), 0);
    assert!(adapter.projection().mode().is_standalone());
    assert_eq!(
        adapter.advertised_capabilities(CapabilitySet::empty()),
        CapabilitySet::empty()
    );
    let err = adapter
        .apply_authoritative_fact(
            OrganizationFact::Membership {
                host_id: ConnectHostId::new(),
                tenant_id: tenant(),
                account_id: PortalAccountId::parse("owner-1").expect("account"),
                status: MembershipStatus::Enrolled,
                revision: 1,
                revoked_at_ms: None,
                expires_at_ms: None,
            },
            1_000,
        )
        .expect_err("standalone rejects portal facts");
    assert_eq!(err, OrgError::StandaloneMode);
}

#[test]
fn sign_in_does_not_enroll_or_export_personal_tasks() {
    let mut projection = OrganizationProjection::standalone();
    let exported = projection.sign_in(account());
    assert_eq!(exported, 0);
    assert_eq!(projection.exported_task_count(), 0);
    assert!(matches!(
        projection.mode(),
        devmanager::org::OperatingMode::ConnectSignedIn { .. }
    ));
    assert_eq!(
        projection.enroll_without_local_confirmation(),
        Err(OrgError::ConnectSignInDoesNotEnroll)
    );
    let task_id = TaskId::new();
    assert!(projection
        .personal_scope_without_link(task_id)
        .is_personal());
    let outcome = projection
        .apply_authoritative_fact(
            OrganizationFact::Membership {
                host_id: ConnectHostId::new(),
                tenant_id: tenant(),
                account_id: PortalAccountId::parse("owner-1").expect("account"),
                status: MembershipStatus::Enrolled,
                revision: 1,
                revoked_at_ms: None,
                expires_at_ms: None,
            },
            1_000,
        )
        .expect("membership fact");
    assert_eq!(outcome, SyncOutcome::Applied);
    assert!(!matches!(
        projection.mode(),
        devmanager::org::OperatingMode::HostEnrolled { .. }
    ));
    assert_eq!(projection.exported_task_count(), 0);
}

#[test]
fn managed_reconciliation_is_identity_checked_idempotent_and_not_last_write_wins() {
    let mut projection = OrganizationProjection::standalone();
    let membership = enroll(&mut projection, ConnectHostId::new());
    let task_id = TaskId::new();
    let personal = TaskId::new();
    assert!(projection
        .personal_scope_without_link(personal)
        .is_personal());

    let first = snapshot(&membership, task_id, 1, EnrollmentState::Enrolled);
    let link_id = first.link_id;
    assert_eq!(
        projection
            .apply_authoritative_fact(OrganizationFact::ManagedTask(first.clone()), 1_000)
            .expect("apply"),
        SyncOutcome::Applied
    );
    assert_eq!(projection.exported_task_count(), 1);
    assert!(matches!(
        projection.personal_scope_without_link(task_id),
        TaskScope::Managed(_)
    ));
    assert!(projection
        .personal_scope_without_link(personal)
        .is_personal());
    assert_eq!(
        projection
            .apply_authoritative_fact(OrganizationFact::ManagedTask(first.clone()), 1_000)
            .expect("duplicate"),
        SyncOutcome::Duplicate
    );

    let mut conflict = first.clone();
    conflict.linked_by = "other-writer".to_string();
    assert_eq!(
        projection.apply_authoritative_fact(OrganizationFact::ManagedTask(conflict), 1_000),
        Err(OrgError::LastWriteWinsForbidden)
    );

    assert_eq!(
        ManagedTaskAdapter::reconcile(
            projection.links().expect("links"),
            &membership,
            ManagedTaskSnapshot {
                portal_revision: 0,
                link_id,
                ..first.clone()
            },
        ),
        Err(OrgError::StalePolicy)
    );

    let mut newer = first;
    newer.portal_revision = 2;
    newer.enrollment_state = EnrollmentState::Unlinked;
    newer.unlinked_at = Some(2_000);
    assert_eq!(
        projection
            .apply_authoritative_fact(OrganizationFact::ManagedTask(newer), 2_000)
            .expect("unlink"),
        SyncOutcome::Applied
    );
    assert_eq!(projection.exported_task_count(), 0);
    assert!(projection
        .personal_scope_without_link(task_id)
        .is_personal());
}

#[test]
fn revoke_and_expiry_are_visible_and_fail_closed() {
    let mut projection = OrganizationProjection::standalone();
    let membership = enroll(&mut projection, ConnectHostId::new());
    assert_eq!(projection.sync_state(), OrganizationSyncState::Enrolled);
    assert_eq!(
        projection
            .apply_authoritative_fact(
                OrganizationFact::Membership {
                    host_id: membership.host_id,
                    tenant_id: membership.tenant_id.clone(),
                    account_id: membership.account_id.clone(),
                    status: MembershipStatus::Revoked,
                    revision: 2,
                    revoked_at_ms: Some(3_000),
                    expires_at_ms: None,
                },
                3_000,
            )
            .expect("revoke"),
        SyncOutcome::Applied
    );
    assert_eq!(projection.sync_state(), OrganizationSyncState::Revoked);
    assert_eq!(projection.exported_task_count(), 0);
    assert_eq!(
        projection.prompts().err(),
        Some(OrgError::MembershipRevoked)
    );

    let mut fresh = OrganizationProjection::standalone();
    let membership = enroll(&mut fresh, ConnectHostId::new());
    assert_eq!(
        fresh
            .apply_authoritative_fact(
                OrganizationFact::Membership {
                    host_id: membership.host_id,
                    tenant_id: membership.tenant_id.clone(),
                    account_id: membership.account_id.clone(),
                    status: MembershipStatus::Enrolled,
                    revision: 3,
                    revoked_at_ms: None,
                    expires_at_ms: Some(500),
                },
                1_000,
            )
            .expect("expired"),
        SyncOutcome::Applied
    );
    assert_eq!(fresh.sync_state(), OrganizationSyncState::Expired);
    assert_eq!(fresh.prompts().err(), Some(OrgError::Expired));
}

#[test]
fn organization_prompt_snapshot_preserves_immutable_versions_and_manual_insert() {
    let mut projection = OrganizationProjection::standalone();
    let membership = enroll(&mut projection, ConnectHostId::new());
    let version_id = devmanager::org::OrgPromptVersionId::new();
    let prompt_id = devmanager::org::OrgPromptId::new();
    let body = "Review the assigned change.";
    let version = OrgPromptVersion {
        prompt_id,
        version_id,
        author: membership.account_id.clone(),
        title: "Review".to_string(),
        tags: vec!["review".to_string()],
        body: body.to_string(),
        content_hash_hex: hash_body(body),
        published_at_ms: 1_000,
    };
    let snapshot = OrganizationPromptSnapshot {
        tenant_id: membership.tenant_id.clone(),
        revision: 1,
        prompts: vec![OrgPrompt {
            prompt_id,
            tenant_id: membership.tenant_id.clone(),
            namespace: "ops".to_string(),
            name: "review".to_string(),
            current_version_id: version_id,
            lifecycle: PromptLifecycle::Published,
        }],
        versions: vec![version.clone()],
        chains: Vec::new(),
    };
    assert_eq!(
        projection
            .apply_prompt_snapshot(snapshot.clone(), 1_000, 10_000)
            .expect("sync"),
        SyncOutcome::Applied
    );
    assert_eq!(
        projection
            .apply_prompt_snapshot(snapshot.clone(), 1_000, 10_000)
            .expect("duplicate"),
        SyncOutcome::Duplicate
    );
    let mut mutated = snapshot;
    mutated.versions[0].body = "mutated".to_string();
    mutated.versions[0].content_hash_hex = hash_body("mutated");
    assert_eq!(
        projection.apply_prompt_snapshot(mutated, 1_000, 10_000),
        Err(OrgError::LastWriteWinsForbidden)
    );
    let insertion = projection
        .prompts()
        .expect("prompts")
        .put_in_composer(version_id, 1_500)
        .expect("insert");
    assert_eq!(
        insertion,
        ComposerInsertion {
            version_id,
            body: body.to_string(),
            sent: false,
            advanced: false,
        }
    );
    let mut adapter = OrganizationPromptAdapter::new();
    let (adapter_snapshot, adapter_version) = prompt_snapshot(&membership, "Exact version only.");
    adapter
        .sync_snapshot(&membership, adapter_snapshot, 1_000, 10_000)
        .expect("adapter sync");
    let insertion = adapter
        .put_in_composer(adapter_version, 1_500)
        .expect("adapter insert");
    assert!(!insertion.sent && !insertion.advanced);
    assert_eq!(
        adapter.mutate_old_version(adapter_version, "nope"),
        Err(OrgError::ImmutableVersion)
    );
}

fn prompt_snapshot(
    membership: &HostMembership,
    body: &str,
) -> (
    OrganizationPromptSnapshot,
    devmanager::org::OrgPromptVersionId,
) {
    let version_id = devmanager::org::OrgPromptVersionId::new();
    let prompt_id = devmanager::org::OrgPromptId::new();
    (
        OrganizationPromptSnapshot {
            tenant_id: membership.tenant_id.clone(),
            revision: 1,
            prompts: vec![OrgPrompt {
                prompt_id,
                tenant_id: membership.tenant_id.clone(),
                namespace: "ops".to_string(),
                name: "exact".to_string(),
                current_version_id: version_id,
                lifecycle: PromptLifecycle::Published,
            }],
            versions: vec![OrgPromptVersion {
                prompt_id,
                version_id,
                author: membership.account_id.clone(),
                title: "Exact".to_string(),
                tags: Vec::new(),
                body: body.to_string(),
                content_hash_hex: hash_body(body),
                published_at_ms: 1_000,
            }],
            chains: Vec::new(),
        },
        version_id,
    )
}

#[test]
fn local_action_adapter_admits_without_faking_dispatch() {
    let mut projection = OrganizationProjection::standalone();
    let membership = enroll(&mut projection, ConnectHostId::new());
    let host_id = membership.host_id.to_string();
    let project = ProjectId::new();
    projection
        .bind_local_action_catalog(vec![LocalActionCatalogEntry {
            kind: LocalActionKind::DbSchemaIntrospect,
            version: 1,
            replay_policy: ReplayPolicy::IdempotentSafe,
            risk: ActionRisk::Low,
        }])
        .expect("catalog");
    let request = LocalActionRequest {
        request_id: devmanager::org::LocalActionId::new(),
        tenant_id: membership.tenant_id.as_str().to_string(),
        host_id: host_id.clone(),
        project_id: project,
        kind: LocalActionKind::DbSchemaIntrospect,
        version: 1,
        payload: "{\"schema\":\"public\"}".to_string(),
        risk: ActionRisk::Low,
        required_approvals: 1,
        expected_target_fingerprint: "fp-1".to_string(),
        expiry_ms: 10_000,
        signature_hex: "ab".to_string(),
        remote_replay_policy_override: None,
    };
    let state = projection
        .admit_local_action(&request, &host_id, project, "fp-1", true, 1_000)
        .expect("admit");
    assert_eq!(state.admission, Admission::Accepted);
    assert_eq!(
        state.reconcile,
        LocalActionReconcileState::AwaitingHostExecution
    );
    assert_eq!(state.outcome, None);

    let mut override_request = request.clone();
    override_request.request_id = devmanager::org::LocalActionId::new();
    override_request.remote_replay_policy_override = Some(ReplayPolicy::IdempotentSafe);
    assert_eq!(
        projection.admit_local_action(&override_request, &host_id, project, "fp-1", true, 1_000),
        Err(OrgError::LastWriteWinsForbidden)
    );

    let mut secret = request.clone();
    secret.request_id = devmanager::org::LocalActionId::new();
    secret.payload = "password=super-secret".to_string();
    assert_eq!(
        projection.admit_local_action(&secret, &host_id, project, "fp-1", true, 1_000),
        Err(OrgError::ProhibitedField)
    );

    let uncertain = projection
        .mark_local_action_uncertain(request.request_id)
        .expect("uncertain");
    assert_eq!(uncertain.reconcile, LocalActionReconcileState::Uncertain);
    let mut adapter = LocalActionAdapter::new();
    adapter
        .bind_catalog(vec![LocalActionCatalogEntry {
            kind: LocalActionKind::EnvDiff,
            version: 1,
            replay_policy: ReplayPolicy::IdempotentSafe,
            risk: ActionRisk::Low,
        }])
        .expect("adapter catalog");
    let mut env = request;
    env.request_id = devmanager::org::LocalActionId::new();
    env.kind = LocalActionKind::EnvDiff;
    let admitted = adapter
        .admit(&membership, &env, &host_id, project, "fp-1", true, 1_000)
        .expect("adapter admit");
    assert_eq!(
        admitted.reconcile,
        LocalActionReconcileState::AwaitingHostExecution
    );
    adapter
        .mark_uncertain(env.request_id)
        .expect("adapter uncertain");
    assert_eq!(
        adapter.retry_uncertain(env.request_id),
        OrgError::UncertainOutcome
    );
}

#[test]
fn evidence_adapter_projects_metadata_only_and_rejects_untrusted() {
    let mut projection = OrganizationProjection::standalone();
    let membership = enroll(&mut projection, ConnectHostId::new());
    projection
        .trust_evidence_signer("trusted-device")
        .expect("trust");
    let mut bundle = EvidenceBundle {
        manifest_version: 1,
        bundle_id: devmanager::org::EvidenceBundleId::new(),
        capture_started_at_ms: 1,
        capture_ended_at_ms: 2,
        timezone: "UTC".to_string(),
        source_device: "laptop".to_string(),
        source_user: "owner-1".to_string(),
        transcript_segments: vec![EvidenceSegment {
            started_at_ms: 1,
            ended_at_ms: 2,
            redacted: true,
            text: Some("redacted transcript".to_string()),
        }],
        media_refs: vec![EvidenceMediaRef {
            label: "frame".to_string(),
            digest_hex: "aa".repeat(32),
        }],
        proposed_title: "Draft".to_string(),
        proposed_summary: "Summary".to_string(),
        acceptance_criteria: vec!["passes".to_string()],
        steps: vec!["open".to_string()],
        privacy_labels: vec!["redacted".to_string()],
        redactions: vec!["transcript".to_string()],
        content_hash_hex: String::new(),
        signature_hex: String::new(),
        signer: "trusted-device".to_string(),
    };
    let hash = compute_bundle_hash(&bundle);
    bundle.content_hash_hex = hash.clone();
    bundle.signature_hex = hash;
    let projected = projection.ingest_evidence(&bundle).expect("ingest");
    assert!(!projected.raw_content_included);
    assert!(!projected.draft.reviewed);
    assert_eq!(projected.draft.title, "Draft");
    assert_eq!(
        projection.evidence_raw_segments(EvidenceAccessClass::MetadataOnly, &bundle),
        Err(OrgError::ProhibitedField)
    );
    projection
        .authorize_evidence_e2e_raw(true)
        .expect("authorize e2e");
    assert_eq!(
        projection
            .evidence_raw_segments(EvidenceAccessClass::AuthorizedE2ERaw, &bundle)
            .expect("raw")
            .len(),
        1
    );

    let mut untrusted = bundle.clone();
    untrusted.bundle_id = devmanager::org::EvidenceBundleId::new();
    untrusted.signer = "stranger".to_string();
    let hash = compute_bundle_hash(&untrusted);
    untrusted.content_hash_hex = hash.clone();
    untrusted.signature_hex = hash;
    assert_eq!(
        projection.ingest_evidence(&untrusted),
        Err(OrgError::UntrustedSigner)
    );

    let mut adapter = EvidenceAdapter::new(["trusted-device".to_string()]);
    adapter.bind_tenant(tenant());
    let other = PortalTenantId::parse("other").expect("other");
    assert_eq!(adapter.ingest(&other, &bundle), Err(OrgError::CrossTenant));
}
