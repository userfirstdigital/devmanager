use devmanager::client::action::TaskRenameArguments;
use devmanager::domain::agent::AgentSessionLifecycle;
use devmanager::domain::id::{AgentSessionId, TaskId};
use devmanager::ui::components::ActionRequest;
use devmanager::ui::task_cockpit::header::{
    presentation_text, ActionTarget, AgentObservation, HeaderFieldKey, HeaderHighWaterLedger,
    HeaderObservation, HighWaterDecision, OpaqueProviderSessionRef, PendingHeaderActionOutcome,
    PendingHeaderActionQueue, ProjectedAction, QuotaObservation, RemoteHealth,
    SpecialistProjection, TaskIdentity, TopBarModel, TopBarProjectionInput,
    HEADER_HIGH_WATER_TTL_MS, MAX_HEADER_SPECIALISTS, MAX_SPECIALIST_VIRTUAL_WINDOW,
};

fn task_id(byte: u8) -> TaskId {
    TaskId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        byte,
    ])
    .expect("fixed UUIDv7 task id")
}

fn agent_id(index: u32) -> AgentSessionId {
    let bytes = index.to_be_bytes();
    AgentSessionId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, bytes[0], bytes[1],
        bytes[2], bytes[3],
    ])
    .expect("fixed UUIDv7 agent id")
}

fn observation(
    key: HeaderFieldKey,
    generation: u64,
    revision: u64,
    removed: bool,
) -> HeaderObservation {
    HeaderObservation {
        key,
        generation,
        revision,
        observed_at_ms: 100,
        fingerprint: revision + u64::from(removed),
        removed,
    }
}

#[test]
fn high_water_tombstones_survive_ttl_and_require_authoritative_resync() {
    let key = HeaderFieldKey::Task(task_id(1));
    let mut ledger = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        ledger.observe(key.clone(), 7, 10, 100, 10, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(key.clone(), 7, 11, 101, 11, true),
        HighWaterDecision::Accepted
    );
    ledger.expire(101 + HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(ledger.len(), 0, "only detailed state may expire");
    assert_eq!(ledger.tombstone_count(), 1, "retirement floor is durable");
    assert_eq!(
        ledger.observe(
            key.clone(),
            7,
            1,
            101 + HEADER_HIGH_WATER_TTL_MS + 1,
            1,
            false
        ),
        HighWaterDecision::IgnoredStale
    );
    assert_eq!(
        ledger.observe(key, 7, 12, 101 + HEADER_HIGH_WATER_TTL_MS + 1, 12, false),
        HighWaterDecision::NeedsFullResync
    );

    let resync = observation(HeaderFieldKey::Task(task_id(1)), 8, 1, false);
    assert_eq!(
        ledger.apply_full_resync(1, 8, [resync]),
        HighWaterDecision::Accepted
    );
    assert_eq!(ledger.last_full_resync_epoch(), 1);
}

#[test]
fn one_bounded_ledger_policy_covers_nested_fields_and_equal_conflicts() {
    let task = task_id(2);
    let agent = agent_id(2);
    let session = OpaqueProviderSessionRef::try_from_raw("provider-session").unwrap();
    let keys = [
        HeaderFieldKey::Task(task),
        HeaderFieldKey::Agent {
            task_id: task,
            agent_id: agent,
        },
        HeaderFieldKey::AgentProvider {
            task_id: task,
            agent_id: agent,
        },
        HeaderFieldKey::AgentResource {
            task_id: task,
            agent_id: agent,
            field: devmanager::ui::task_cockpit::header::AgentResourceField::Cpu,
        },
        HeaderFieldKey::Quota {
            provider: "claude".into(),
            provider_session_ref: session,
        },
        HeaderFieldKey::Remote {
            source_id: "remote".into(),
        },
    ];
    let mut ledger = HeaderHighWaterLedger::new(32, 1000);
    for (index, key) in keys.into_iter().enumerate() {
        assert_eq!(
            ledger.observe(key.clone(), 1, 4, 10, index as u64, false),
            HighWaterDecision::Accepted
        );
        assert_eq!(
            ledger.observe(key, 1, 4, 11, index as u64, false),
            HighWaterDecision::IgnoredStale
        );
    }
    let conflict_key = HeaderFieldKey::Task(task_id(3));
    assert_eq!(
        ledger.observe(conflict_key.clone(), 1, 9, 10, 20, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(conflict_key, 1, 9, 11, 21, true),
        HighWaterDecision::RejectedConflict
    );
}

#[test]
fn high_water_capacity_resync_retires_omitted_keys_without_revival() {
    let first_key = HeaderFieldKey::Task(task_id(6));
    let second_key = HeaderFieldKey::Task(task_id(7));
    let mut ledger = HeaderHighWaterLedger::new(1, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        ledger.observe(first_key.clone(), 1, 20, 1, 20, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(second_key.clone(), 1, 1, 2, 1, false),
        HighWaterDecision::NeedsFullResync
    );
    assert_eq!(
        ledger.apply_full_resync(1, 2, [observation(second_key.clone(), 2, 1, false)]),
        HighWaterDecision::Accepted
    );
    assert_eq!(ledger.floor_len(), 1);
    assert_eq!(
        ledger.observe(first_key, 1, 1, 3, 1, false),
        HighWaterDecision::IgnoredStale,
        "retirement floor must protect a key omitted by the bounded resync"
    );
}

#[test]
fn specialists_keep_bounded_stable_id_order_and_keyset_windows() {
    let task = task_id(4);
    let labels = ["original", "relabelled"];
    let mut observations = Vec::with_capacity(100_000);
    for index in 0..100_000_u32 {
        observations.push(AgentObservation {
            id: agent_id(index),
            task_id: task,
            label: labels[(index % 2) as usize],
            provider: "claude",
            provider_session_id: None,
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 1,
            revision: 1,
            removed: false,
        });
    }
    let first = SpecialistProjection::from_observations(&observations);
    assert_eq!(first.scanned(), 100_000);
    assert_eq!(first.retained().len(), MAX_HEADER_SPECIALISTS);
    assert_eq!(first.retained().first().unwrap().id, agent_id(0));
    assert_eq!(first.retained().last().unwrap().id, agent_id(4_999));

    let anchor = first.retained()[1_000].id;
    let before = first.window_after_id(Some(anchor), 64);
    assert_eq!(before.window_after_id, Some(anchor));
    assert!(before.items.len() <= MAX_SPECIALIST_VIRTUAL_WINDOW);

    observations[10].label = "new label";
    observations.insert(
        0,
        AgentObservation {
            id: agent_id(0),
            task_id: task,
            label: "relabelled again",
            provider: "codex",
            provider_session_id: None,
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 1,
            revision: 2,
            removed: false,
        },
    );
    let after = SpecialistProjection::from_observations(&observations);
    let after_window = after.window_after_id(Some(anchor), 64);
    assert_eq!(
        before.items.iter().map(|row| row.id).collect::<Vec<_>>(),
        after_window
            .items
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        "reorder/relabel/add-before-anchor must not change keyset identity"
    );

    observations.push(AgentObservation {
        id: agent_id(3),
        task_id: task,
        label: "removed",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Closed,
        runtime_generation: 1,
        revision: 3,
        removed: true,
    });
    let removed = SpecialistProjection::from_observations(&observations);
    assert!(removed.removed_ids().contains(&agent_id(3)));
}

#[test]
fn action_epochs_and_queue_coalesce_only_safe_equivalents() {
    let identity = TaskIdentity {
        task_id: task_id(5),
        revision: 10,
        resource_generation: 2,
        connection_epoch: 3,
        focus_epoch: 4,
        client_epoch: 5,
        navigation_epoch: 6,
        request_epoch: 7,
        action_epoch: 8,
    };
    let show = ProjectedAction::task_show(identity);
    let mut queue = PendingHeaderActionQueue::new(2);
    assert_eq!(queue.push(show.clone()), PendingHeaderActionOutcome::Queued);
    assert_eq!(queue.push(show), PendingHeaderActionOutcome::Coalesced);

    let rename = ProjectedAction::new(
        ActionRequest::TaskRename(TaskRenameArguments {
            task_id: identity.task_id,
            title: "next".into(),
        }),
        ActionTarget::Task(identity),
    );
    assert_eq!(
        queue.push(rename.clone()),
        PendingHeaderActionOutcome::Queued
    );
    assert_eq!(queue.push(rename), PendingHeaderActionOutcome::Full);
    assert_eq!(
        queue.drain_for_tick(8).len(),
        2,
        "destructive ordering is never discarded"
    );
}

#[test]
fn provider_session_refs_and_presentation_are_bounded_and_redacted() {
    let raw = "session-super-secret-123";
    let opaque = OpaqueProviderSessionRef::try_from_raw(raw).unwrap();
    let debug = format!("{opaque:?}");
    let json = serde_json::to_string(&opaque).unwrap();
    assert!(!debug.contains(raw));
    assert!(!json.contains(raw));
    assert!(OpaqueProviderSessionRef::try_from_raw(&"x".repeat(20_000)).is_err());
    let raw_wire = format!(
        r#"{{"identity":{{"provider":"claude","provider_session_id":"{raw}","observation_id":1}},"detail":"ok","observed_at_ms":1,"generation":1,"revision":1}}"#
    );
    assert!(serde_json::from_str::<QuotaObservation>(&raw_wire).is_err());

    let value = format!("TOKEN={raw} {}", "x".repeat(20_000));
    let presented = presentation_text(&value, 64);
    assert!(presented.chars().count() <= 64);
    assert!(!presented.contains(raw));
}

#[test]
fn top_bar_retains_explicit_remote_and_one_fresh_provider_quota_source() {
    let session = OpaqueProviderSessionRef::try_from_raw("quota-session").unwrap();
    let input = TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        host: None,
        connect: None,
        update: None,
        remote: Some(
            devmanager::ui::task_cockpit::header::RemoteObservation::new(
                "remote-source",
                RemoteHealth::Healthy,
                "Remote workspace",
                9,
                3,
                10_000,
            ),
        ),
        quotas: vec![QuotaObservation::new(
            "claude",
            session,
            9,
            4,
            10_000,
            "20% remaining",
        )],
        resources: None,
    };
    let model = TopBarModel::try_from_input(&input).unwrap();
    assert!(model.remote.is_some());
    assert_eq!(model.quotas.len(), 1);
    assert_eq!(model.quotas[0].provider, "claude");
}

#[test]
fn top_bar_empty_snapshot_exposes_unavailable_semantics() {
    let model = TopBarModel::from_input(&TopBarProjectionInput {
        now_ms: 1,
        generation: 1,
        ..TopBarProjectionInput::default()
    });
    assert!(!model.unavailable.is_empty());
    assert!(model.accessible_description.contains("unavailable"));
}
