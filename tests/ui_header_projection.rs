use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::id::{AgentSessionId, EnvironmentId, ProjectId, TaskId};
use devmanager::domain::snapshot::TaskSnapshot;
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, VisibleTaskStatus, WorkspaceRef,
};
use devmanager::ui::components::{AccessibleRole, ActionRequest};
use devmanager::ui::components::{ActivationSource, KeyboardKey};
use devmanager::ui::native_shell::NativeInteraction;
use devmanager::ui::task_cockpit::header::{
    presentation_text, ActionTarget, AgentObservation, AgentResourceField, ConnectObservation,
    ConnectState, HeaderActionEnvelope, HeaderActionError, HeaderFieldKey, HeaderHighWaterLedger,
    HeaderLayout, HeaderObservation, HighWaterDecision, HostHealth, HostObservation,
    HostObservationIdentity, HostResourceObservation, OpaqueProviderSessionRef,
    PendingHeaderActionOutcome, PendingHeaderActionQueue, ProjectProjection, ProjectedAction,
    QuotaObservation, RemoteHealth, SpecialistProjection, TaskHeaderModel, TaskIdentity,
    TitleLayout, TopBarModel, TopBarProjectionController, TopBarProjectionInput, UpdateObservation,
    UpdateState, WorkspaceProjection, HEADER_HIGH_WATER_TTL_MS, MAX_HEADER_SPECIALISTS,
    MAX_SPECIALIST_VIRTUAL_WINDOW, MAX_TOP_BAR_QUOTA_CACHE, PROVIDER_QUOTA_MAX_AGE_MS,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

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

fn resource_fingerprint_for_test(observation: &HostResourceObservation) -> u64 {
    let mut hasher = Sha256::new();
    match observation.cpu_percent {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_bits().to_be_bytes());
        }
        None => hasher.update([0]),
    }
    match observation.memory_bytes {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hasher.update(observation.revision.to_be_bytes());
    match observation.observed_at_ms {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    match observation.generation {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    u64::from_be_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("fixed digest prefix"),
    )
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
fn high_water_rejects_overbound_nested_keys_before_retention() {
    let huge = "x".repeat(100_000);
    let mut ledger = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        ledger.observe(HeaderFieldKey::Host { source_id: huge }, 1, 1, 1, 1, false,),
        HighWaterDecision::RejectedInvalid
    );
    assert!(ledger.is_empty());
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
fn full_resync_retirement_is_keyed_and_unknown_keys_request_resync() {
    let mut ledger = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    let retired = HeaderFieldKey::Task(task_id(31));
    let replacement = HeaderFieldKey::Task(task_id(32));
    let unrelated = HeaderFieldKey::Task(task_id(33));
    assert_eq!(
        ledger.observe(retired, 1, 100, 100, 100, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.apply_full_resync(1, 1, [observation(replacement, 1, 1, false)]),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(unrelated, 1, 1, 101, 1, false),
        HighWaterDecision::Accepted,
        "a retired key's floor must not suppress an unrelated key"
    );
}

#[test]
fn full_resync_retirement_floor_rejects_older_reintroduced_keys() {
    let retired = HeaderFieldKey::Task(task_id(8));
    let replacement = HeaderFieldKey::Task(task_id(9));
    let mut ledger = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        ledger.observe(retired.clone(), 3, 20, 100, 20, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.apply_full_resync(1, 3, [observation(replacement, 3, 21, false)]),
        HighWaterDecision::Accepted
    );

    assert_eq!(
        ledger.apply_full_resync(2, 3, [observation(retired, 3, 1, false)]),
        HighWaterDecision::IgnoredStale,
        "a newer resync epoch must not reopen a lower revision retired by the prior epoch"
    );
}

#[test]
fn full_resync_duplicates_are_conflict_safe_and_monotonic_in_any_order() {
    let key = HeaderFieldKey::Task(task_id(10));
    let mut conflict = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        conflict.apply_full_resync(
            1,
            3,
            [
                observation(key.clone(), 3, 3, false),
                HeaderObservation {
                    key: key.clone(),
                    generation: 3,
                    revision: 3,
                    observed_at_ms: 100,
                    fingerprint: 999,
                    removed: false,
                },
            ],
        ),
        HighWaterDecision::RejectedConflict
    );

    let mut descending = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        descending.apply_full_resync(
            1,
            3,
            [
                observation(key.clone(), 3, 3, false),
                observation(key.clone(), 3, 2, false),
            ],
        ),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        descending.observe(key.clone(), 3, 2, 200, 2, false),
        HighWaterDecision::IgnoredStale,
        "a lower duplicate must not replace the candidate-state high-water"
    );

    let mut ascending = HeaderHighWaterLedger::new(4, HEADER_HIGH_WATER_TTL_MS);
    assert_eq!(
        ascending.apply_full_resync(
            1,
            3,
            [
                observation(key.clone(), 3, 2, false),
                observation(key.clone(), 3, 3, false),
            ],
        ),
        HighWaterDecision::Accepted
    );
    assert_eq!(descending.floor_len(), ascending.floor_len());
    assert_eq!(
        ascending.observe(key, 3, 2, 200, 2, false),
        HighWaterDecision::IgnoredStale
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
    assert!(first.scanned() <= MAX_HEADER_SPECIALISTS + 1);
    assert!(first.unique_count() <= MAX_HEADER_SPECIALISTS + 1);
    assert!(first.overflowed());
    assert!(!first.source_available());
    assert!(first.requires_full_resync());
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

    observations.insert(
        3,
        AgentObservation {
            id: agent_id(3),
            task_id: task,
            label: "removed",
            provider: "claude",
            provider_session_id: None,
            lifecycle: AgentSessionLifecycle::Closed,
            runtime_generation: 1,
            revision: 3,
            removed: true,
        },
    );
    let removed = SpecialistProjection::from_observations(&observations);
    assert!(removed.removed_ids().contains(&agent_id(3)));
}

#[test]
fn specialist_source_unavailable_and_equal_stamp_conflicts_stay_explicit() {
    let unavailable = SpecialistProjection::from_iter_with_source(
        std::iter::empty::<AgentObservation<'_>>(),
        false,
    );
    assert!(!unavailable.source_available());
    assert_eq!(unavailable.unique_count(), 0);

    let task = task_id(13);
    let base = AgentObservation {
        id: agent_id(13),
        task_id: task,
        label: "first",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 1,
        removed: false,
    };
    let conflict = SpecialistProjection::from_iter([
        base,
        AgentObservation {
            label: "second",
            ..base
        },
    ]);
    assert!(!conflict.source_available());
    assert_eq!(conflict.conflicts_rejected(), 1);
    assert!(conflict.retained().is_empty());
}

#[test]
fn specialist_duplicates_are_order_independent_and_count_unique_ids() {
    let task = task_id(11);
    let older = AgentObservation {
        id: agent_id(11),
        task_id: task,
        label: "older",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 2,
        revision: 1,
        removed: false,
    };
    let newer = AgentObservation {
        label: "newer",
        revision: 2,
        ..older
    };
    let forward = SpecialistProjection::from_iter([older, newer]);
    let reverse = SpecialistProjection::from_iter([newer, older]);
    assert_eq!(forward.total_seen(), 1);
    assert_eq!(forward.retained(), reverse.retained());
    assert_eq!(forward.retained()[0].label, "newer");
}

#[test]
fn top_bar_actions_keep_actual_observation_stamps() {
    let session = OpaqueProviderSessionRef::try_from_raw("stamp-session").unwrap();
    let input = TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        host: Some(HostObservation {
            identity: HostObservationIdentity {
                host_id: "host".into(),
                revision: 7,
            },
            health: HostHealth::Healthy,
            observed_at_ms: Some(9_000),
            generation: Some(9),
        }),
        connect: Some(ConnectObservation {
            host_id: "host".into(),
            state: ConnectState::Connected,
            revision: 8,
            observed_at_ms: Some(9_100),
            generation: Some(9),
        }),
        update: Some(UpdateObservation {
            source_id: "host".into(),
            state: UpdateState::UpToDate,
            revision: 9,
            observed_at_ms: Some(9_200),
            generation: Some(9),
        }),
        remote: None,
        quotas: vec![QuotaObservation::new(
            "claude",
            session,
            9,
            10,
            9_300,
            "20% remaining",
        )],
        resources: None,
    };
    let model = TopBarModel::try_from_input(&input).unwrap();
    let host = model.host.unwrap();
    let ActionTarget::Host(host_stamp) = host.action.target() else {
        panic!("host action target")
    };
    assert_eq!(host_stamp.observed_at_ms, 9_000);
    assert_eq!(host_stamp.generation, 9);
    assert_eq!(host_stamp.revision, 7);
    let quota = &model.quotas[0];
    let ActionTarget::Quota(quota_stamp) = quota.action.target() else {
        panic!("quota action target")
    };
    assert_eq!(quota_stamp.observed_at_ms, 9_300);
    assert_eq!(quota_stamp.generation, 9);
    assert_eq!(quota_stamp.revision, 10);
}

#[test]
fn top_bar_reports_each_missing_or_invalid_observation_and_truthful_state() {
    let session = OpaqueProviderSessionRef::try_from_raw("state-session").unwrap();
    let model = TopBarModel::from_input(&TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        host: Some(HostObservation {
            identity: HostObservationIdentity {
                host_id: "host".into(),
                revision: 1,
            },
            health: HostHealth::Degraded,
            observed_at_ms: Some(10_000),
            generation: Some(9),
        }),
        connect: None,
        update: None,
        remote: None,
        quotas: vec![QuotaObservation::new(
            "claude",
            session,
            9,
            1,
            10_000,
            "20% remaining",
        )],
        resources: Some(HostResourceObservation {
            cpu_percent: Some(101.0),
            memory_bytes: Some(10),
            revision: 1,
            observed_at_ms: Some(10_000),
            generation: Some(9),
        }),
    });
    assert!(model
        .host
        .as_ref()
        .unwrap()
        .description
        .contains("degraded"));
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::ConnectionStatus));
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::UpdateStatus));
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::Remote));
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::Cpu));
    assert!(model.resources.as_ref().unwrap().cpu_percent.is_none());
}

#[test]
fn equal_stamp_changed_remote_or_quota_payload_is_rejected() {
    let session = OpaqueProviderSessionRef::try_from_raw("conflict-session").unwrap();
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        remote: Some(
            devmanager::ui::task_cockpit::header::RemoteObservation::new(
                "remote",
                RemoteHealth::Healthy,
                "Remote",
                9,
                1,
                10_000,
            ),
        ),
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    assert_eq!(
        controller.observe_remote(
            devmanager::ui::task_cockpit::header::RemoteObservation::new(
                "remote",
                RemoteHealth::Degraded,
                "Remote",
                9,
                1,
                10_000,
            )
        ),
        HighWaterDecision::RejectedConflict
    );
    let mut quota_controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        quotas: vec![QuotaObservation::new(
            "claude", session, 9, 1, 10_000, "old",
        )],
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    assert_eq!(
        quota_controller.observe_quota(QuotaObservation::new(
            "claude", session, 9, 1, 10_000, "new",
        )),
        HighWaterDecision::RejectedConflict
    );
}

#[test]
fn controller_constructor_never_retains_over_cap_quota_input() {
    let mut quotas = Vec::new();
    for index in 0..(devmanager::ui::task_cockpit::header::MAX_TOP_BAR_QUOTA_CACHE + 7) {
        let session = OpaqueProviderSessionRef::try_from_raw(&format!("session-{index}")).unwrap();
        quotas.push(QuotaObservation::new(
            &format!("provider-{index}"),
            session,
            9,
            index as u64 + 1,
            10_000,
            "ok",
        ));
    }
    let controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 10_000,
        generation: 9,
        quotas,
        ..TopBarProjectionInput::default()
    });
    assert!(controller.is_err(), "over-cap snapshots must fail closed");
}

#[test]
fn idle_controller_tick_hides_expired_quota_without_new_observations() {
    let session = OpaqueProviderSessionRef::try_from_raw("idle-quota").unwrap();
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        quotas: vec![QuotaObservation::new("claude", session, 1, 1, 100, "fresh")],
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    assert_eq!(controller.model().quotas.len(), 1);

    let expired = controller.model_at(100 + PROVIDER_QUOTA_MAX_AGE_MS + 1);
    assert!(expired.quotas.is_empty());
    assert!(expired
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::Quota));
    assert!(controller.high_water().requires_full_resync());
}

#[test]
fn quota_event_expiry_hides_cached_values_before_resync() {
    let session = OpaqueProviderSessionRef::try_from_raw("event-expiry-quota").unwrap();
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        quotas: vec![QuotaObservation::new("claude", session, 1, 1, 100, "fresh")],
        ..TopBarProjectionInput::default()
    })
    .unwrap();

    assert_eq!(
        controller.observe_quota(QuotaObservation::new(
            "claude",
            session,
            1,
            2,
            100 + PROVIDER_QUOTA_MAX_AGE_MS + 1,
            "new",
        )),
        HighWaterDecision::NeedsFullResync
    );
    let model = controller.model();
    assert!(model.quotas.is_empty());
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::Quota));
}

#[test]
fn full_resync_derives_quota_floor_when_callers_omit_observations() {
    let session = OpaqueProviderSessionRef::try_from_raw("derived-quota-floor").unwrap();
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    let snapshot_quota = QuotaObservation::new("claude", session, 1, 9, 100, "fresh");
    assert_eq!(
        controller
            .apply_full_resync(
                1,
                TopBarProjectionInput {
                    now_ms: 100,
                    generation: 1,
                    quotas: vec![snapshot_quota],
                    ..TopBarProjectionInput::default()
                },
                std::iter::empty(),
            )
            .unwrap(),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        controller.observe_quota(QuotaObservation::new("claude", session, 1, 8, 101, "stale",)),
        HighWaterDecision::IgnoredStale
    );
}

#[test]
fn quota_flood_enters_visible_unavailable_resync_state_instead_of_stale_display() {
    let mut quotas = Vec::new();
    for index in 0..MAX_TOP_BAR_QUOTA_CACHE {
        let session = OpaqueProviderSessionRef::try_from_raw(&format!("flood-{index}")).unwrap();
        quotas.push(QuotaObservation::new(
            &format!("provider-{index}"),
            session,
            1,
            index as u64 + 1,
            100,
            "fresh",
        ));
    }
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        quotas,
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    let new_session = OpaqueProviderSessionRef::try_from_raw("flood-new").unwrap();
    assert_eq!(
        controller.observe_quota(QuotaObservation::new(
            "new-provider",
            new_session,
            1,
            100,
            200,
            "new",
        )),
        HighWaterDecision::NeedsFullResync
    );
    let model = controller.model();
    assert!(
        model.quotas.is_empty(),
        "stale quota must not remain visible"
    );
    assert!(model.quota_overflow_count >= 1);
    assert!(model
        .unavailable
        .contains(&devmanager::ui::task_cockpit::header::TopBarUnavailable::Quota));
    assert!(model.quota_overflow_action.is_some());

    assert_eq!(
        controller
            .apply_full_resync(
                1,
                TopBarProjectionInput {
                    now_ms: 200,
                    generation: 1,
                    quotas: vec![QuotaObservation::new(
                        "new-provider",
                        new_session,
                        1,
                        100,
                        200,
                        "new",
                    )],
                    ..TopBarProjectionInput::default()
                },
                std::iter::empty(),
            )
            .unwrap(),
        HighWaterDecision::Accepted
    );
    assert_eq!(controller.model().quota_overflow_count, 0);
    assert_eq!(controller.model().quotas.len(), 1);
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
    assert_eq!(
        queue.push(show.clone().into_envelope()),
        PendingHeaderActionOutcome::Queued
    );
    assert_eq!(
        queue.push(show.into_envelope()),
        PendingHeaderActionOutcome::Coalesced
    );

    let rename = ProjectedAction::task_rename(identity, "next").unwrap();
    assert_eq!(
        queue.push(rename.clone().into_envelope()),
        PendingHeaderActionOutcome::Queued
    );
    assert_eq!(
        queue.push(rename.into_envelope()),
        PendingHeaderActionOutcome::Full
    );
    assert_eq!(
        queue.drain_for_tick(8).len(),
        2,
        "destructive ordering is never discarded"
    );

    let huge_title = "untrusted ".repeat(10_000);
    let bounded = ProjectedAction::task_rename(identity, huge_title);
    assert!(bounded.is_err());
    let bounded = ProjectedAction::task_rename(identity, "bounded title").unwrap();
    let bounded_envelope = bounded.into_envelope();
    let ActionRequest::TaskRename(arguments) = bounded_envelope
        .into_request_if_current(&ActionTarget::Task(identity))
        .unwrap()
    else {
        panic!("rename action")
    };
    assert!(arguments.title.chars().count() <= 160);

    let stamp = devmanager::ui::task_cockpit::header::ObservationStamp {
        observed_at_ms: 10,
        generation: 2,
        revision: 3,
    };
    let remote = ProjectedAction::remote_status(stamp);
    assert!(matches!(remote.target(), ActionTarget::Remote(captured) if *captured == stamp));
    assert_eq!(
        PendingHeaderActionOutcome::from_high_water(HighWaterDecision::RejectedConflict),
        PendingHeaderActionOutcome::Conflict
    );
    assert_eq!(
        PendingHeaderActionOutcome::from_high_water(HighWaterDecision::NeedsFullResync),
        PendingHeaderActionOutcome::Uncertain
    );

    let mut typed_queue = PendingHeaderActionQueue::new(1);
    typed_queue.push(ProjectedAction::task_show(identity).into_envelope());
    let envelopes = typed_queue.drain_envelopes_for_tick(1);
    assert!(matches!(
        envelopes.first().map(HeaderActionEnvelope::target),
        Some(ActionTarget::Task(captured)) if *captured == identity
    ));
}

#[test]
fn projected_action_exposes_only_a_fenced_typed_envelope() {
    let identity = TaskIdentity {
        task_id: task_id(34),
        revision: 10,
        resource_generation: 2,
        connection_epoch: 3,
        focus_epoch: 4,
        client_epoch: 5,
        navigation_epoch: 6,
        request_epoch: 7,
        action_epoch: 8,
    };
    let envelope: HeaderActionEnvelope = ProjectedAction::task_show(identity).into_envelope();
    assert!(matches!(
        envelope.target(),
        ActionTarget::Task(captured) if *captured == identity
    ));
    assert!(matches!(
        envelope.into_request_if_current(&ActionTarget::Task(identity)),
        Ok(ActionRequest::TaskShow { task_id }) if task_id == identity.task_id
    ));
}

#[test]
fn header_envelope_requires_the_exact_current_target_to_extract_a_request() {
    let identity = TaskIdentity {
        task_id: task_id(37),
        revision: 10,
        resource_generation: 2,
        connection_epoch: 3,
        focus_epoch: 4,
        client_epoch: 5,
        navigation_epoch: 6,
        request_epoch: 7,
        action_epoch: 8,
    };
    let mut stale = identity;
    stale.revision += 1;
    let envelope = ProjectedAction::task_show(identity).into_envelope();
    assert_eq!(
        envelope
            .clone()
            .into_request_if_current(&ActionTarget::Task(stale)),
        Err(HeaderActionError::StaleTarget)
    );
    assert!(matches!(
        envelope.into_request_if_current(&ActionTarget::Task(identity)),
        Ok(ActionRequest::TaskShow { task_id }) if task_id == identity.task_id
    ));
}

#[test]
fn native_shell_header_dispatch_consumes_only_a_current_envelope() {
    let identity = TaskIdentity {
        task_id: task_id(38),
        revision: 10,
        resource_generation: 2,
        connection_epoch: 3,
        focus_epoch: 4,
        client_epoch: 5,
        navigation_epoch: 6,
        request_epoch: 7,
        action_epoch: 8,
    };
    let mut stale = identity;
    stale.focus_epoch += 1;
    let mut interaction = NativeInteraction::new(Some(identity.task_id));
    let envelope = ProjectedAction::task_show(identity).into_envelope();
    assert!(interaction
        .action_from_header_envelope(
            envelope.clone(),
            &ActionTarget::Task(stale),
            ActivationSource::Keyboard {
                key: KeyboardKey::Enter,
            },
        )
        .is_none());
    let record = interaction
        .action_from_header_envelope(
            envelope,
            &ActionTarget::Task(identity),
            ActivationSource::Keyboard {
                key: KeyboardKey::Enter,
            },
        )
        .expect("current header envelope dispatch");
    assert!(matches!(
        record.event.request,
        ActionRequest::TaskShow { task_id } if task_id == identity.task_id
    ));
}

#[test]
fn serialized_header_text_and_quota_sequences_are_bounded() {
    let host_wire = format!(
        r#"{{"identity":{{"host_id":"{}","revision":1}},"health":"healthy","observed_at_ms":1,"generation":1}}"#,
        "x".repeat(10_000)
    );
    assert!(
        serde_json::from_str::<devmanager::ui::task_cockpit::header::HostObservation>(&host_wire)
            .is_err()
    );

    let mut quotas = Vec::new();
    for index in 0..65 {
        quotas.push(QuotaObservation::new(
            "claude",
            OpaqueProviderSessionRef::try_from_raw(&format!("wire-{index}")).unwrap(),
            1,
            index + 1,
            1,
            "ok",
        ));
    }
    let wire = serde_json::to_string(&TopBarProjectionInput {
        now_ms: 1,
        generation: 1,
        quotas,
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    assert!(serde_json::from_str::<TopBarProjectionInput>(&wire).is_err());
}

#[test]
fn task_header_uses_snapshot_identity_filters_nested_agents_and_bounds_workspace_path() {
    let snapshot_task = task_id(14);
    let caller_task = task_id(15);
    let project = ProjectId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x0e,
    ])
    .unwrap();
    let environment = EnvironmentId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x0f,
    ])
    .unwrap();
    let specialist = agent_id(14);
    let wrong_task_agent = agent_id(15);
    let task = TaskFacts {
        id: snapshot_task,
        environment_id: environment,
        title: "Task".into(),
        description: None,
        project_id: project,
        workspace: WorkspaceRef::External {
            path: PathBuf::from("C:/".to_string() + &"workspace/".repeat(200)),
        },
        assignment: TaskAssignment::LocalOwner,
        lifecycle: TaskLifecycle::Open,
        action_epoch: 1,
        revision: 2,
        created_at_ms: 1,
    };
    let agent = |id: AgentSessionId, task_id: TaskId| AgentSessionFacts {
        id,
        task_id,
        role: AgentRole::Primary,
        provider_kind: "claude".into(),
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 1,
    };
    let snapshot = TaskSnapshot {
        task,
        connectivity: TaskConnectivity::Connected,
        attention: TaskAttention::None,
        activity: TaskActivity::Idle,
        review_readiness: ReviewReadiness::NotReady,
        agents: BTreeMap::from([
            (specialist, agent(specialist, snapshot_task)),
            (wrong_task_agent, agent(wrong_task_agent, caller_task)),
        ]),
        primary_agent_id: Some(specialist),
        artifacts: BTreeMap::new(),
        resources: BTreeMap::new(),
    };
    let model = TaskHeaderModel::from_snapshot(caller_task, &snapshot, Default::default());
    assert_eq!(model.identity().task_id, snapshot_task);
    assert_eq!(model.specialists().unique_count(), 1);
    let WorkspaceProjection::External(path) = model.workspace() else {
        panic!("external workspace")
    };
    assert!(path.as_path().to_string_lossy().chars().count() <= 512);
}

#[test]
fn long_unbroken_titles_wrap_and_ellipsis_with_bounded_lines() {
    let task = TaskHeaderModel::new(
        TaskIdentity {
            task_id: task_id(12),
            revision: 1,
            resource_generation: 1,
            connection_epoch: 1,
            focus_epoch: 1,
            client_epoch: 1,
            navigation_epoch: 1,
            request_epoch: 1,
            action_epoch: 1,
        },
        "x".repeat(220),
        ProjectProjection::new(
            ProjectId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x0d,
            ])
            .unwrap(),
            "project",
        ),
        WorkspaceProjection::main(),
        SpecialistProjection::from_iter(std::iter::empty()),
        VisibleTaskStatus::Idle,
        "task",
    );
    let wrapped = HeaderLayout::for_model(&task, 400);
    let TitleLayout::Wrapped(lines) = wrapped.title else {
        panic!("expected wrapped title")
    };
    assert!(lines.iter().all(|line| line.chars().count() <= 28));
    let wide = HeaderLayout::for_model(&task, 640);
    assert!(matches!(wide.title, TitleLayout::Truncated(_)));
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

#[test]
fn specialist_eviction_keeps_a_coarse_floor_and_rejects_stale_reentry() {
    let task = task_id(21);
    let mut observations = Vec::with_capacity(MAX_HEADER_SPECIALISTS + 3);
    for index in 1..=MAX_HEADER_SPECIALISTS as u32 {
        observations.push(AgentObservation {
            id: agent_id(index),
            task_id: task,
            label: "fresh",
            provider: "claude",
            provider_session_id: None,
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 1,
            revision: 10,
            removed: false,
        });
    }
    observations.push(AgentObservation {
        id: agent_id(0),
        task_id: task,
        label: "new lower key",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 10,
        removed: false,
    });
    observations.push(AgentObservation {
        id: agent_id(0),
        task_id: task,
        label: "removed lower key",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Closed,
        runtime_generation: 1,
        revision: 11,
        removed: true,
    });
    observations.push(AgentObservation {
        id: agent_id(MAX_HEADER_SPECIALISTS as u32),
        task_id: task,
        label: "stale replay",
        provider: "claude",
        provider_session_id: None,
        lifecycle: AgentSessionLifecycle::Open,
        runtime_generation: 1,
        revision: 1,
        removed: false,
    });

    let projection = SpecialistProjection::from_iter(observations);
    assert!(
        projection
            .retained()
            .iter()
            .all(|agent| agent.id != agent_id(MAX_HEADER_SPECIALISTS as u32)),
        "an evicted identity must not re-enter with an older revision"
    );
    assert!(projection.overflowed());
    assert!(!projection.source_available());
    assert!(projection.requires_full_resync());
}

#[test]
fn controller_observations_advance_snapshot_epoch_and_clock_atomically() {
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    assert_eq!(
        controller.observe_host(HostObservation {
            identity: HostObservationIdentity {
                host_id: "host".into(),
                revision: 2,
            },
            health: HostHealth::Healthy,
            observed_at_ms: Some(2_000),
            generation: Some(2),
        }),
        HighWaterDecision::Accepted
    );
    let model = controller.model();
    assert!(model.host.is_some(), "new-generation host must be visible");
}

#[test]
fn resource_observation_rejects_atomically_when_memory_stamp_conflicts() {
    let older = HostResourceObservation {
        cpu_percent: Some(1.0),
        memory_bytes: Some(10),
        revision: 1,
        observed_at_ms: Some(100),
        generation: Some(1),
    };
    let newer = HostResourceObservation {
        cpu_percent: Some(2.0),
        memory_bytes: Some(20),
        revision: 2,
        observed_at_ms: Some(200),
        generation: Some(1),
    };
    let newer_fingerprint = resource_fingerprint_for_test(&newer);
    let conflicting_memory_fingerprint = newer_fingerprint ^ 1;
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        ..TopBarProjectionInput::default()
    })
    .unwrap();

    assert_eq!(
        controller
            .apply_full_resync(
                1,
                TopBarProjectionInput {
                    now_ms: 100,
                    generation: 1,
                    resources: Some(older.clone()),
                    ..TopBarProjectionInput::default()
                },
                [
                    HeaderObservation {
                        key: HeaderFieldKey::HostResource {
                            field: AgentResourceField::Cpu,
                        },
                        generation: 1,
                        revision: 1,
                        observed_at_ms: 100,
                        fingerprint: newer_fingerprint,
                        removed: false,
                    },
                    HeaderObservation {
                        key: HeaderFieldKey::HostResource {
                            field: AgentResourceField::Memory,
                        },
                        generation: 1,
                        revision: 2,
                        observed_at_ms: 200,
                        fingerprint: conflicting_memory_fingerprint,
                        removed: false,
                    },
                ],
            )
            .unwrap(),
        HighWaterDecision::Accepted
    );
    let before = controller.high_water().clone();

    assert_eq!(
        controller.observe_resource(newer.clone()),
        HighWaterDecision::RejectedConflict
    );
    assert_eq!(
        controller.input().resources.as_ref().unwrap().revision,
        older.revision,
        "a rejected compound observation must not reach the model"
    );

    let mut before_probe = before.clone();
    let mut after_probe = controller.high_water().clone();
    assert_eq!(
        before_probe.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Cpu,
            },
            1,
            2,
            200,
            newer_fingerprint ^ 2,
            false,
        ),
        HighWaterDecision::Accepted,
        "the baseline CPU mark should still be at revision one"
    );
    assert_eq!(
        after_probe.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Cpu,
            },
            1,
            2,
            200,
            newer_fingerprint ^ 2,
            false,
        ),
        HighWaterDecision::Accepted,
        "the rejected compound observation must not advance the CPU mark"
    );

    let mut before_memory_probe = before;
    let mut after_memory_probe = controller.high_water().clone();
    assert_eq!(
        before_memory_probe.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Memory,
            },
            1,
            2,
            200,
            newer_fingerprint,
            false,
        ),
        HighWaterDecision::RejectedConflict,
        "the baseline memory mark should retain the conflicting fingerprint"
    );
    assert_eq!(
        after_memory_probe.observe(
            HeaderFieldKey::HostResource {
                field: AgentResourceField::Memory,
            },
            1,
            2,
            200,
            newer_fingerprint,
            false,
        ),
        HighWaterDecision::RejectedConflict,
        "the rejected compound observation must not advance the memory mark"
    );
}

#[test]
fn oversize_rename_reports_only_the_bounded_probe() {
    let identity = TaskIdentity {
        task_id: task_id(24),
        revision: 1,
        resource_generation: 1,
        connection_epoch: 1,
        focus_epoch: 1,
        client_epoch: 1,
        navigation_epoch: 1,
        request_epoch: 1,
        action_epoch: 1,
    };
    let error = ProjectedAction::task_rename(identity, "x".repeat(100_000)).unwrap_err();
    assert!(matches!(
        error,
        devmanager::ui::task_cockpit::header::HeaderActionError::TaskTitleTooLong {
            actual: 161,
            max: 160,
        }
    ));
}

#[test]
fn task_header_accessibility_names_every_rendered_sibling() {
    let model = TaskHeaderModel::new(
        TaskIdentity {
            task_id: task_id(22),
            revision: 1,
            resource_generation: 1,
            connection_epoch: 1,
            focus_epoch: 1,
            client_epoch: 1,
            navigation_epoch: 1,
            request_epoch: 1,
            action_epoch: 1,
        },
        "Task title",
        ProjectProjection::new(
            ProjectId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x16,
            ])
            .unwrap(),
            "Project",
        ),
        WorkspaceProjection::external("C:/workspace"),
        SpecialistProjection::from_iter(std::iter::empty()),
        VisibleTaskStatus::Idle,
        "Task title. Project. Workspace. Idle.",
    );
    let tree = model.accessibility_tree();
    let labels: Vec<_> = tree
        .children
        .iter()
        .map(|child| child.label.as_str())
        .collect();
    assert!(labels.iter().any(|label| label.contains("Project")));
    assert!(labels.iter().any(|label| label.contains("workspace")));
    assert!(labels.iter().any(|label| label.contains("Idle")));
}

#[test]
fn accessibility_tree_rebounds_untrusted_public_projection_text() {
    let secret = "TOKEN=header-secret ".to_string() + &"x".repeat(20_000);
    let model = TaskHeaderModel::new(
        TaskIdentity {
            task_id: task_id(35),
            revision: 1,
            resource_generation: 1,
            connection_epoch: 1,
            focus_epoch: 1,
            client_epoch: 1,
            navigation_epoch: 1,
            request_epoch: 1,
            action_epoch: 1,
        },
        secret.clone(),
        ProjectProjection::new(
            ProjectId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x18,
            ])
            .unwrap(),
            secret.clone(),
        ),
        WorkspaceProjection::external(secret.clone()),
        SpecialistProjection::from_iter(std::iter::empty()),
        VisibleTaskStatus::Idle,
        secret,
    );
    let tree = model.accessibility_tree();
    for node in std::iter::once(&tree).chain(tree.children.iter()) {
        assert!(node.label.chars().count() <= 512);
        assert!(node.description.chars().count() <= 512);
        assert!(!node.label.contains("header-secret"));
        assert!(!node.description.contains("header-secret"));
    }
}

#[test]
fn narrow_layout_provides_a_focusable_overflow_control_with_the_task_fence() {
    let identity = TaskIdentity {
        task_id: task_id(36),
        revision: 1,
        resource_generation: 1,
        connection_epoch: 1,
        focus_epoch: 1,
        client_epoch: 1,
        navigation_epoch: 1,
        request_epoch: 1,
        action_epoch: 1,
    };
    let model = TaskHeaderModel::new(
        identity,
        "Task",
        ProjectProjection::new(
            ProjectId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x19,
            ])
            .unwrap(),
            "Project",
        ),
        WorkspaceProjection::main(),
        SpecialistProjection::from_iter(std::iter::empty()),
        VisibleTaskStatus::Idle,
        "Task header",
    );
    let layout = HeaderLayout::for_model(&model, 320);
    let overflow = layout.overflow_control.as_ref().expect("overflow control");
    assert!(overflow.focusable);
    assert_eq!(overflow.action.target(), &ActionTarget::Task(identity));
    assert!(overflow.label.chars().count() <= 256);
    assert_eq!(layout.overflow_items.len(), layout.overflow.len());
    assert!(layout
        .overflow_items
        .iter()
        .all(|item| item.focusable && item.label.chars().count() <= 512));
    assert!(model
        .accessibility_tree()
        .children
        .iter()
        .all(|node| node.focusable));
    let narrow_tree = model.accessibility_tree_at(320);
    let menu = narrow_tree
        .children
        .last()
        .expect("width-aware overflow menu node");
    assert_eq!(menu.role, AccessibleRole::Region);
    assert!(menu.focusable);
    assert_eq!(
        menu.action.as_ref().map(|action| action.target()),
        Some(&ActionTarget::Task(identity))
    );
    assert_eq!(menu.children.len(), layout.overflow_items.len());
    assert!(menu
        .children
        .iter()
        .all(|item| item.focusable && item.action.is_some()));
}

#[test]
fn header_layout_does_not_split_grapheme_clusters_and_accepts_scale() {
    let title = "👩‍💻".repeat(80);
    let model = TaskHeaderModel::new(
        TaskIdentity {
            task_id: task_id(23),
            revision: 1,
            resource_generation: 1,
            connection_epoch: 1,
            focus_epoch: 1,
            client_epoch: 1,
            navigation_epoch: 1,
            request_epoch: 1,
            action_epoch: 1,
        },
        title,
        ProjectProjection::new(
            ProjectId::from_bytes([
                0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x17,
            ])
            .unwrap(),
            "Project",
        ),
        WorkspaceProjection::main(),
        SpecialistProjection::from_iter(std::iter::empty()),
        VisibleTaskStatus::Idle,
        "Task",
    );
    let layout = HeaderLayout::for_model_at_scale(&model, 800, 2.0);
    let TitleLayout::Wrapped(lines) = layout.title else {
        panic!("expected wrapped title")
    };
    assert!(lines
        .iter()
        .all(|line| !line.starts_with('\u{200d}') && !line.ends_with('\u{200d}')));
}

#[test]
fn presentation_text_keeps_zwj_clusters_whole_at_the_scalar_bound() {
    let title = "👩‍💻".repeat(80);
    assert_eq!(presentation_text(&title, 160), "👩‍💻".repeat(53));
}

#[test]
fn raw_observation_ids_are_opaque_in_debug_and_json() {
    let host = HostObservation {
        identity: HostObservationIdentity {
            host_id: "host-secret-123".into(),
            revision: 1,
        },
        health: HostHealth::Healthy,
        observed_at_ms: Some(1),
        generation: Some(1),
    };
    assert!(!format!("{host:?}").contains("host-secret-123"));
    let json = serde_json::to_string(&host).unwrap();
    assert!(!json.contains("host-secret-123"));
}

#[test]
fn controller_full_resync_is_epoch_bound_and_updates_the_visible_snapshot() {
    let mut controller = TopBarProjectionController::try_new(TopBarProjectionInput {
        now_ms: 100,
        generation: 1,
        ..TopBarProjectionInput::default()
    })
    .unwrap();
    let snapshot = TopBarProjectionInput {
        now_ms: 2_000,
        generation: 2,
        host: Some(HostObservation {
            identity: HostObservationIdentity {
                host_id: "resynced-host".into(),
                revision: 4,
            },
            health: HostHealth::Healthy,
            observed_at_ms: Some(2_000),
            generation: Some(2),
        }),
        ..TopBarProjectionInput::default()
    };
    assert_eq!(
        controller
            .apply_full_resync(
                1,
                snapshot,
                [observation(
                    HeaderFieldKey::Host {
                        source_id: "resynced-host".into()
                    },
                    2,
                    4,
                    false
                )]
            )
            .unwrap(),
        HighWaterDecision::Accepted
    );
    assert!(controller.model().host.is_some());
    let second = controller
        .apply_full_resync(
            1,
            TopBarProjectionInput {
                now_ms: 3_000,
                generation: 3,
                ..TopBarProjectionInput::default()
            },
            std::iter::empty(),
        )
        .unwrap();
    assert_eq!(second, HighWaterDecision::RejectedInvalid);
    assert!(controller.model().host.is_some());
}

#[test]
fn typed_cpu_contract_normalizes_core_scaled_samples() {
    assert_eq!(
        devmanager::ui::task_cockpit::header::TaskManagerCpuPercent::from_core_scaled(125.0, 64)
            .value(),
        1.953125
    );
    assert_eq!(
        devmanager::ui::task_cockpit::header::TaskManagerCpuPercent::from_core_scaled(
            12_800.0,
            64,
        )
        .value(),
        100.0
    );
}
