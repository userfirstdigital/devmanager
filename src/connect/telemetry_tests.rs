use std::time::Duration;

use super::policy::HostTimeAuthority;
use super::telemetry::reject_encoded_page_bytes;
use super::{
    encode_observation, ManagedField, ManagementGrant, ManagementPolicy, ObservationAuthority,
    ObservationCompleteness, ObservationConfidence, ObservationDependency, ObservationError,
    ObservationFreshness, ObservationId, ObservationMessageClass, ObservationPage,
    ObservationRecord, ObservationReducer, ObservationSchema, PageBudget, ProviderObservation,
    QualifyingActivity, ReduceOutcome, RestrictiveGitSummary, TaskObservationFacts, UsageKind,
    UsageMeasure, UsageProvenance, ACTIVE_SESSION_IDLE_LIMIT_MS, ACTIVE_SESSION_TIME_LABEL,
    MAX_ACTIVITIES_PER_TASK, MAX_CONNECT_PAGE_ENCODED_BYTES, MAX_OBSERVATION_DOCUMENT_BYTES,
    MAX_OBSERVATION_RETENTION_MS, MAX_READY_INTERVALS, MAX_SPECIALISTS,
    OBSERVATION_SCHEMA_REVISION, OBSERVATION_STALE_AFTER_MS,
};
use crate::domain::agent::{AgentRole, AgentSessionLifecycle};
use crate::domain::id::{ClientId, EventId, TaskId};
use crate::domain::task::{TaskActivity, TaskAttention, TaskConnectivity, TaskLifecycle};
use uuid::Uuid;

fn uuid_v7(tail: u32) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[12..16].copy_from_slice(&tail.to_be_bytes());
    Uuid::from_bytes(bytes)
}

fn task_id(tail: u32) -> TaskId {
    TaskId::from_bytes(uuid_v7(tail).into_bytes()).expect("task id")
}

fn event_id(tail: u32) -> EventId {
    EventId::from_bytes(uuid_v7(tail).into_bytes()).expect("event id")
}

fn client_id(tail: u32) -> ClientId {
    ClientId::from_bytes(uuid_v7(tail).into_bytes()).expect("client id")
}

fn human_command(task: TaskId, client: ClientId, event: EventId) -> QualifyingActivity {
    QualifyingActivity::AcceptedHumanCommand {
        task_id: task,
        client_id: client,
        event_id: event,
    }
}

fn foreground(task: TaskId, client: ClientId, event: EventId) -> QualifyingActivity {
    QualifyingActivity::ForegroundTaskInteraction {
        task_id: task,
        client_id: client,
        event_id: event,
    }
}

fn provider(kind: &str, role: AgentRole, activity: TaskActivity) -> ProviderObservation {
    ProviderObservation::try_new(kind, role, AgentSessionLifecycle::Open, activity)
        .expect("canonical provider")
}

fn facts(now_ms: u64, revision: u64) -> TaskObservationFacts {
    TaskObservationFacts::try_new(
        task_id(1),
        TaskLifecycle::Open,
        TaskAttention::NeedsAnswer,
        TaskConnectivity::Connected,
        Some(provider(
            "claude",
            AgentRole::Primary,
            TaskActivity::Working,
        )),
        vec![provider(
            "codex",
            AgentRole::specialist("reviewer").expect("specialist"),
            TaskActivity::Idle,
        )],
        now_ms,
        revision,
    )
    .expect("task facts")
}

fn live_grant() -> ManagementGrant {
    ManagementGrant::issued_for_test(task_id(1))
}

fn bind(grant: &ManagementGrant) -> ObservationAuthority {
    ObservationAuthority::from_grant(grant).expect("live grant")
}

fn seeded(now_ms: u64) -> (ManagementGrant, ObservationReducer) {
    let grant = live_grant();
    let mut reducer = ObservationReducer::from_host_time(now_ms, bind(&grant)).expect("host bind");
    reducer
        .record_task_facts(&grant, facts(now_ms, 1))
        .expect("facts");
    (grant, reducer)
}

fn page_all() -> PageBudget {
    PageBudget {
        max_items: 32,
        max_work: 256,
    }
}

#[test]
fn fifteen_minute_idle_rule_closes_from_accepted_human_actions() {
    let task = task_id(1);
    let desktop = client_id(1);
    let (grant, mut reducer) = seeded(0);
    assert_eq!(
        reducer
            .record_activity(&grant, human_command(task, desktop, event_id(10)))
            .expect("command"),
        ReduceOutcome::Accepted
    );
    reducer
        .advance(Duration::from_millis(5 * 60 * 1_000))
        .expect("clock");
    assert_eq!(
        reducer
            .record_non_qualifying_provider_cpu(&grant, task, event_id(11))
            .expect("cpu"),
        ReduceOutcome::Ignored
    );
    reducer
        .advance(Duration::from_millis(
            ACTIVE_SESSION_IDLE_LIMIT_MS - 1 - 5 * 60 * 1_000,
        ))
        .expect("clock");
    reducer
        .record_activity(&grant, foreground(task, desktop, event_id(12)))
        .expect("foreground");
    assert!(reducer
        .ready_page(page_all(), None)
        .expect("open")
        .items()
        .is_empty());

    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let page = reducer.ready_page(page_all(), None).expect("closed");
    let union_ms: u64 = page
        .items()
        .iter()
        .map(|item| {
            let interval = item.active_session().expect("interval");
            assert!(
                interval.ended_at_ms() - interval.started_at_ms() <= ACTIVE_SESSION_IDLE_LIMIT_MS
            );
            interval.ended_at_ms() - interval.started_at_ms()
        })
        .sum();
    assert_eq!(
        union_ms,
        (ACTIVE_SESSION_IDLE_LIMIT_MS - 1) + ACTIVE_SESSION_IDLE_LIMIT_MS
    );
    assert_eq!(
        page.items()[0].active_time_label(),
        ACTIVE_SESSION_TIME_LABEL
    );
}

#[test]
fn overlapping_desktop_and_phone_activity_is_unioned_not_summed() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(&grant, human_command(task, client_id(1), event_id(20)))
        .expect("desktop");
    reducer
        .advance(Duration::from_millis(5 * 60 * 1_000))
        .expect("clock");
    reducer
        .record_activity(&grant, foreground(task, client_id(2), event_id(21)))
        .expect("phone");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let page = reducer.ready_page(page_all(), None).expect("ready");
    let union_ms: u64 = page
        .items()
        .iter()
        .map(|item| {
            let interval = item.active_session().expect("interval");
            interval.ended_at_ms() - interval.started_at_ms()
        })
        .sum();
    assert_eq!(union_ms, 5 * 60 * 1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS);
    assert_ne!(union_ms, 2 * ACTIVE_SESSION_IDLE_LIMIT_MS);
}

#[test]
fn same_timestamp_events_order_by_event_id() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(50);
    reducer
        .record_activity(&grant, human_command(task, client_id(2), event_id(92)))
        .expect("later id");
    reducer
        .record_activity(&grant, foreground(task, client_id(1), event_id(91)))
        .expect("earlier id");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let page = reducer.ready_page(page_all(), None).expect("ready");
    assert_eq!(page.items()[0].source_event_id(), event_id(91));
}

#[test]
fn long_cluster_splits_and_emits_each_interval_once() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    for step in 0..8 {
        reducer
            .record_activity(
                &grant,
                human_command(task, client_id(1), event_id(200 + step)),
            )
            .expect("pulse");
        reducer
            .advance(Duration::from_millis(5 * 60 * 1_000))
            .expect("clock");
    }
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let first = reducer.ready_page(page_all(), None).expect("first inspect");
    let second = reducer
        .ready_page(page_all(), None)
        .expect("second inspect");
    assert!(!first.items().is_empty());
    assert_eq!(first.items().len(), second.items().len());
    assert_eq!(
        first
            .items()
            .iter()
            .map(|item| item.observation_id())
            .collect::<Vec<_>>(),
        second
            .items()
            .iter()
            .map(|item| item.observation_id())
            .collect::<Vec<_>>()
    );
    let union_ms: u64 = first
        .items()
        .iter()
        .map(|item| {
            let interval = item.active_session().expect("split");
            interval.ended_at_ms() - interval.started_at_ms()
        })
        .sum();
    assert_eq!(union_ms, 7 * 5 * 60 * 1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS);
}

#[test]
fn exact_replay_is_duplicate_and_payload_mismatch_is_conflict() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    let first = human_command(task, client_id(1), event_id(30));
    assert_eq!(
        reducer.record_activity(&grant, first).expect("first"),
        ReduceOutcome::Accepted
    );
    assert_eq!(
        reducer.record_activity(&grant, first).expect("replay"),
        ReduceOutcome::Duplicate
    );
    let conflicted = foreground(task, client_id(2), event_id(30));
    assert_eq!(
        reducer
            .record_activity(&grant, conflicted)
            .expect_err("conflict"),
        ObservationError::Conflict
    );
}

#[test]
fn cpu_event_does_not_consume_a_later_human_event_id() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    assert_eq!(
        reducer
            .record_non_qualifying_provider_cpu(&grant, task, event_id(77))
            .expect("cpu"),
        ReduceOutcome::Ignored
    );
    assert_eq!(
        reducer
            .record_activity(&grant, human_command(task, client_id(1), event_id(77)))
            .expect("human reuses the unused id"),
        ReduceOutcome::Accepted
    );
}

#[test]
fn stale_task_facts_and_usage_do_not_overwrite_newer_evidence() {
    let (grant, mut reducer) = seeded(1_000);
    assert_eq!(
        reducer
            .record_task_facts(&grant, facts(500, 0))
            .expect_err("older"),
        ObservationError::StaleRevision
    );
    let newer = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(10),
        "tokens",
        None,
        1_000,
        2,
    )
    .expect("newer usage");
    reducer
        .record_usage(&grant, task_id(1), newer)
        .expect("newer usage");
    let older = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(1),
        "tokens",
        None,
        500,
        1,
    )
    .expect("older usage");
    assert_eq!(
        reducer
            .record_usage(&grant, task_id(1), older)
            .expect_err("stale"),
        ObservationError::StaleRevision
    );
}

#[test]
fn provider_usage_labels_stay_distinct_and_reject_invalid_windows() {
    let tokens = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(1_024),
        "tokens",
        None,
        1_000,
        1,
    )
    .expect("tokens");
    assert_eq!(tokens.provenance_label(), "provider_reported");
    assert_eq!(
        UsageMeasure::try_new(
            "claude",
            "cli.status",
            UsageKind::MonetaryQuote,
            UsageProvenance::ProviderReported,
            Some(1),
            "usd_cents",
            None,
            1_000,
            1,
        )
        .expect_err("kind mismatch"),
        ObservationError::InvalidUsage
    );
    assert_eq!(
        UsageMeasure::try_new(
            "claude",
            "cli.status",
            UsageKind::QuotaRemaining,
            UsageProvenance::ProviderReported,
            Some(1),
            "percent",
            Some((2_000, 1_000)),
            1_000,
            1,
        )
        .expect_err("reversed window"),
        ObservationError::InvalidWindow
    );
    assert_eq!(
        UsageMeasure::try_new(
            "claude",
            "cli.status",
            UsageKind::Tokens,
            UsageProvenance::ProviderReported,
            Some(1),
            "tokens",
            None,
            9_999,
            1,
        )
        .and_then(|measure| {
            let (grant, mut reducer) = seeded(1_000);
            reducer.record_usage(&grant, task_id(1), measure)
        })
        .expect_err("future observed_at"),
        ObservationError::FutureTimestamp
    );

    let (grant, mut reducer) = seeded(1_000);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(40)),
        )
        .expect("source");
    reducer
        .record_usage(&grant, task_id(1), tokens)
        .expect("record");
    let snapshot = reducer.current_observation(task_id(1)).expect("current");
    assert_eq!(snapshot.task_id(), task_id(1));
    assert_eq!(snapshot.lifecycle(), TaskLifecycle::Open);
    assert_eq!(snapshot.attention(), TaskAttention::NeedsAnswer);
    assert_eq!(snapshot.connectivity(), TaskConnectivity::Connected);
    assert_eq!(
        snapshot.usage().tokens().and_then(|item| item.value()),
        Some(1_024)
    );
    assert!(snapshot.usage().local_cost_estimate().is_none());
    assert_eq!(snapshot.schema_revision(), OBSERVATION_SCHEMA_REVISION);
    assert_eq!(snapshot.completeness(), ObservationCompleteness::Partial);
    assert_eq!(snapshot.confidence(), ObservationConfidence::High);
}

#[test]
fn only_qualifying_human_messages_increment_counts() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    assert_eq!(
        reducer
            .record_message(
                &grant,
                task,
                event_id(40),
                ObservationMessageClass::Human,
                true,
            )
            .expect("human"),
        ReduceOutcome::Accepted
    );
    for (tail, class) in [
        (41, ObservationMessageClass::Synthetic),
        (42, ObservationMessageClass::StatusNotice),
        (43, ObservationMessageClass::ProviderInternal),
        (44, ObservationMessageClass::Replay),
        (45, ObservationMessageClass::CopiedPrompt),
        (46, ObservationMessageClass::InheritedContext),
        (47, ObservationMessageClass::SpecialistToPrimaryTransfer),
    ] {
        assert_eq!(
            reducer
                .record_message(&grant, task, event_id(tail), class, true)
                .expect("excluded"),
            ReduceOutcome::Ignored
        );
    }
    let snapshot = reducer.current_observation(task).expect("snapshot");
    assert_eq!(snapshot.human_message_count(), 1);
    assert_eq!(snapshot.human_turn_count(), 1);
}

#[test]
fn restrictive_git_summary_is_structurally_capped() {
    let allowed =
        RestrictiveGitSummary::try_new(Some("codex/cursor-auto-10-05"), Some("abc1234"), 3, 10, 2)
            .expect("ok");
    assert_eq!(allowed.files_changed(), 3);
    assert!(RestrictiveGitSummary::try_new(Some("main"), Some("XYZ"), 1, 1, 0).is_err());
    assert!(
        RestrictiveGitSummary::try_new(Some(&"a".repeat(300)), Some("abc1234"), 1, 1, 0).is_err()
    );
    assert!(RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), u32::MAX, 1, 0).is_err());
}

#[test]
fn current_observation_without_source_is_unavailable() {
    let (grant, reducer) = seeded(0);
    assert_eq!(
        reducer
            .current_observation(task_id(1))
            .expect_err("no source"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );
}

#[test]
fn delivery_and_ack_are_unavailable_until_outbox_exists() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(50)),
        )
        .expect("activity");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let page = reducer.ready_page(page_all(), None).expect("pending");
    assert_eq!(
        page.items()[0].freshness(reducer.now_ms()),
        ObservationFreshness::Current
    );
    let id = page.items()[0].observation_id();
    assert_eq!(
        reducer.request_delivery(id).expect_err("no outbox"),
        ObservationError::Unavailable(ObservationDependency::DurableOutbox)
    );
    assert_eq!(
        reducer.acknowledge(id).expect_err("no ack channel"),
        ObservationError::Unavailable(ObservationDependency::DurableOutbox)
    );
    let again = reducer.ready_page(page_all(), None).expect("still pending");
    assert_eq!(again.items()[0].observation_id(), id);
    reducer
        .advance(Duration::from_millis(OBSERVATION_STALE_AFTER_MS))
        .expect("clock");
    assert_eq!(
        again.items()[0].freshness(reducer.now_ms()),
        ObservationFreshness::Stale
    );
}

#[test]
fn offline_overflow_fails_closed_and_keeps_pending_identity() {
    let task = task_id(1);
    let (grant, mut reducer) = seeded(0);
    for step in 0..MAX_READY_INTERVALS {
        reducer
            .record_activity(
                &grant,
                human_command(task, client_id(1), event_id(300 + step as u32)),
            )
            .expect("activity");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("clock");
    }
    let pending = reducer
        .ready_page(
            PageBudget {
                max_items: MAX_READY_INTERVALS as u16,
                max_work: 256,
            },
            None,
        )
        .expect("at cap");
    assert_eq!(pending.items().len(), MAX_READY_INTERVALS);
    let first_id = pending.items()[0].observation_id();
    reducer
        .record_activity(
            &grant,
            human_command(
                task,
                client_id(1),
                event_id(300 + MAX_READY_INTERVALS as u32),
            ),
        )
        .expect("next activity after closed windows");
    assert_eq!(
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect_err("overflow"),
        ObservationError::Backpressure
    );
    let still = reducer
        .inspect_pending(
            PageBudget {
                max_items: MAX_READY_INTERVALS as u16,
                max_work: 256,
            },
            None,
        )
        .expect("overflow must not drop pending");
    assert_eq!(still.ids().len(), MAX_READY_INTERVALS);
    assert_eq!(still.ids()[0], first_id);
}

#[test]
fn cap_plus_one_and_100k_adversarial_inputs_stop_predictably() {
    let grant = live_grant();
    let mut reducer = ObservationReducer::from_host_time(0, bind(&grant)).expect("host bind");
    reducer
        .record_task_facts(&grant, facts(0, 1))
        .expect("facts");
    assert_eq!(
        reducer
            .record_task_facts(
                &grant,
                TaskObservationFacts::try_new(
                    task_id(10),
                    TaskLifecycle::Open,
                    TaskAttention::None,
                    TaskConnectivity::Connected,
                    None,
                    Vec::new(),
                    0,
                    1,
                )
                .expect("foreign task"),
            )
            .expect_err("cross-task facts"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );

    let (activity_grant, mut activity_reducer) = seeded(0);
    let mut accepted = 0usize;
    let mut stopped = false;
    for step in 0..100_000u32 {
        match activity_reducer.record_activity(
            &activity_grant,
            human_command(task_id(1), client_id(1), event_id(10_000 + step)),
        ) {
            Ok(ReduceOutcome::Accepted) => accepted += 1,
            Err(ObservationError::BoundExceeded) => {
                stopped = true;
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(accepted, MAX_ACTIVITIES_PER_TASK);
    assert!(stopped);

    let too_many = vec![
        provider("a", AgentRole::specialist("a").unwrap(), TaskActivity::Idle);
        MAX_SPECIALISTS + 1
    ];
    assert_eq!(
        TaskObservationFacts::try_new(
            task_id(1),
            TaskLifecycle::Open,
            TaskAttention::None,
            TaskConnectivity::Connected,
            None,
            too_many,
            0,
            1,
        )
        .expect_err("specialist cap"),
        ObservationError::BoundExceeded
    );
}

#[test]
fn ready_page_obeys_work_budget_without_collecting_the_ledger() {
    let (grant, mut reducer) = seeded(0);
    for step in 0..8 {
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(80 + step)),
            )
            .expect("pulse");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("clock");
    }
    let page = reducer
        .ready_page(
            PageBudget {
                max_items: 2,
                max_work: 2,
            },
            None,
        )
        .expect("budgeted");
    assert_eq!(page.items().len(), 2);
    assert!(page.more());
    assert!(page.work_used() <= 2);
}

#[test]
fn observation_codec_denies_unknown_and_surveillance_fields() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(60)),
        )
        .expect("source");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let record = reducer.ready_page(page_all(), None).expect("ready").items()[0].clone();
    assert_eq!(record.policy_revision(), ManagementPolicy::REVISION);
    assert!(record.git().is_none());
    assert_eq!(record.primary().map(|item| item.kind()), Some("claude"));
    let encoded = encode_observation(&record).expect("encode");
    let decoded = ObservationSchema::decode(&encoded).expect("decode");
    assert_eq!(decoded, record);
    assert!(ObservationSchema::decode(
        br#"{"observation_id":"00","sentiment":"angry","prompt":"secret","path_count":1}"#
    )
    .is_err());
    assert!(ObservationSchema::current().allows_aggregate("files_changed"));
    assert!(!ObservationSchema::current().allows_aggregate("sentiment"));
    assert!(!ObservationSchema::current().allows_aggregate("path"));
}

fn all_usage_slots(at_ms: u64) -> [UsageMeasure; 6] {
    [
        UsageMeasure::try_new(
            "claude",
            "cli.status",
            UsageKind::Tokens,
            UsageProvenance::ProviderReported,
            Some(11),
            "tokens",
            None,
            at_ms,
            3,
        )
        .expect("tokens"),
        UsageMeasure::try_new(
            "claude",
            "cli.quota",
            UsageKind::QuotaRemaining,
            UsageProvenance::ProviderReported,
            Some(40),
            "percent",
            Some((at_ms.saturating_sub(10), at_ms)),
            at_ms,
            3,
        )
        .expect("quota remaining"),
        UsageMeasure::try_new(
            "claude",
            "cli.quota",
            UsageKind::QuotaReset,
            UsageProvenance::ProviderReported,
            Some(at_ms + 60),
            "epoch_ms",
            Some((at_ms.saturating_sub(10), at_ms)),
            at_ms,
            3,
        )
        .expect("quota reset"),
        UsageMeasure::try_new(
            "claude",
            "cli.invoice",
            UsageKind::MonetaryQuote,
            UsageProvenance::ProviderQuoted,
            Some(77),
            "usd_cents",
            None,
            at_ms,
            3,
        )
        .expect("quoted"),
        UsageMeasure::try_new(
            "claude",
            "local.estimator",
            UsageKind::Tokens,
            UsageProvenance::LocalEstimate,
            Some(9),
            "tokens",
            None,
            at_ms,
            3,
        )
        .expect("local tokens"),
        UsageMeasure::try_new(
            "claude",
            "local.estimator",
            UsageKind::MonetaryQuote,
            UsageProvenance::LocalEstimate,
            Some(3),
            "usd_cents",
            None,
            at_ms,
            3,
        )
        .expect("local cost"),
    ]
}

#[test]
fn codec_round_trips_every_usage_slot_and_rejects_forged_identity() {
    let (grant, mut reducer) = seeded(1_000);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(70)),
        )
        .expect("source");
    for measure in all_usage_slots(1_000) {
        reducer
            .record_usage(&grant, task_id(1), measure)
            .expect("usage");
    }
    reducer
        .record_git(
            &grant,
            task_id(1),
            RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), 2, 4, 1).expect("git"),
            event_id(71),
            1_000,
            1,
        )
        .expect("git");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("clock");
    let record = reducer.ready_page(page_all(), None).expect("ready").items()[0].clone();
    assert_eq!(
        record.usage().tokens().and_then(UsageMeasure::value),
        Some(11)
    );
    assert_eq!(
        record
            .usage()
            .quota_remaining()
            .and_then(UsageMeasure::value),
        Some(40)
    );
    assert_eq!(
        record.usage().quoted_cost().and_then(UsageMeasure::value),
        Some(77)
    );
    let encoded = encode_observation(&record).expect("encode");
    assert!(encoded.len() <= MAX_OBSERVATION_DOCUMENT_BYTES);
    let decoded = ObservationSchema::decode(&encoded).expect("round-trip");
    assert_eq!(decoded, record);
    assert_eq!(
        decoded.usage().tokens().map(UsageMeasure::source),
        Some("cli.status")
    );
    assert_eq!(
        decoded
            .usage()
            .local_cost_estimate()
            .map(UsageMeasure::revision),
        Some(3)
    );

    let mut forged = encoded.clone();
    let needle = br#""commit":"abc1234""#;
    let pos = forged
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("commit field is present");
    let commit_at = pos + br#""commit":""#.len();
    forged[commit_at] = b'd';
    assert_eq!(
        ObservationSchema::decode(&forged).expect_err("content/id mismatch"),
        ObservationError::Conflict
    );
    assert_eq!(
        ObservationSchema::decode(&vec![b'{'; MAX_OBSERVATION_DOCUMENT_BYTES + 1])
            .expect_err("physical cap"),
        ObservationError::BoundExceeded
    );
}

#[test]
fn closed_interval_identity_does_not_change_after_later_updates() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(80)),
        )
        .expect("open");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    let before = reducer
        .ready_page(page_all(), None)
        .expect("frozen")
        .items()[0]
        .clone();
    reducer
        .record_message(
            &grant,
            task_id(1),
            event_id(81),
            ObservationMessageClass::Human,
            true,
        )
        .expect("later message");
    reducer
        .record_usage(
            &grant,
            task_id(1),
            UsageMeasure::try_new(
                "claude",
                "cli.status",
                UsageKind::Tokens,
                UsageProvenance::ProviderReported,
                Some(99),
                "tokens",
                None,
                ACTIVE_SESSION_IDLE_LIMIT_MS,
                4,
            )
            .expect("later usage"),
        )
        .expect("later usage");
    let after = reducer
        .ready_page(page_all(), None)
        .expect("still frozen")
        .items()[0]
        .clone();
    assert_eq!(before.observation_id(), after.observation_id());
    assert_eq!(before, after);
    assert!(after.usage().tokens().is_none());
    let live = reducer.current_observation(task_id(1)).expect("live");
    assert_eq!(
        live.usage().tokens().and_then(UsageMeasure::value),
        Some(99)
    );
    assert_eq!(live.human_message_count(), 1);
    assert_ne!(live.observation_id(), before.observation_id());
}

#[test]
fn same_revision_and_timestamp_with_different_payload_is_conflict() {
    let (grant, mut reducer) = seeded(1_000);
    let changed = TaskObservationFacts::try_new(
        task_id(1),
        TaskLifecycle::Open,
        TaskAttention::Failed,
        TaskConnectivity::Connected,
        Some(provider("claude", AgentRole::Primary, TaskActivity::Idle)),
        Vec::new(),
        1_000,
        1,
    )
    .expect("same rev");
    assert_eq!(
        reducer
            .record_task_facts(&grant, changed)
            .expect_err("facts conflict"),
        ObservationError::Conflict
    );
    let first = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(5),
        "tokens",
        None,
        1_000,
        2,
    )
    .expect("first");
    reducer
        .record_usage(&grant, task_id(1), first)
        .expect("first usage");
    let different = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(6),
        "tokens",
        None,
        1_000,
        2,
    )
    .expect("different");
    assert_eq!(
        reducer
            .record_usage(&grant, task_id(1), different)
            .expect_err("usage conflict"),
        ObservationError::Conflict
    );
    reducer
        .record_git(
            &grant,
            task_id(1),
            RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), 1, 1, 0).expect("git"),
            event_id(90),
            1_000,
            1,
        )
        .expect("git");
    assert_eq!(
        reducer
            .record_git(
                &grant,
                task_id(1),
                RestrictiveGitSummary::try_new(Some("dev"), Some("def5678"), 1, 1, 0).expect("git"),
                event_id(91),
                1_000,
                1,
            )
            .expect_err("git conflict"),
        ObservationError::Conflict
    );
}

#[test]
fn current_observation_does_not_refresh_stale_facts_on_query() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(100)),
        )
        .expect("source");
    reducer
        .advance(Duration::from_millis(OBSERVATION_STALE_AFTER_MS))
        .expect("age");
    let snapshot = reducer
        .current_observation(task_id(1))
        .expect("stale current");
    assert_eq!(snapshot.observed_at_ms(), 0);
    assert_eq!(
        snapshot.freshness(reducer.now_ms()),
        ObservationFreshness::Stale
    );
    assert_eq!(snapshot.completeness(), ObservationCompleteness::Partial);
}

#[test]
fn ready_page_charges_work_on_the_first_inspected_unit() {
    let (grant, mut reducer) = seeded(0);
    for step in 0..8 {
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(400 + step)),
            )
            .expect("pulse");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("clock");
    }
    let page = reducer
        .ready_page(
            PageBudget {
                max_items: 1_000,
                max_work: 2,
            },
            None,
        )
        .expect("budgeted");
    assert_eq!(page.work_used(), 2);
    assert!(page.items().len() <= 2);
    assert!(page.more());
}

#[test]
fn pending_cursor_replays_each_interval_identity_once() {
    let (grant, mut reducer) = seeded(0);
    for step in 0..3 {
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(500 + step)),
            )
            .expect("pulse");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("clock");
    }
    let first = reducer
        .inspect_pending(
            PageBudget {
                max_items: 1,
                max_work: 1,
            },
            None,
        )
        .expect("first page");
    assert_eq!(first.ids().len(), 1);
    let second = reducer
        .inspect_pending(
            PageBudget {
                max_items: 1,
                max_work: 1,
            },
            first.next_cursor(),
        )
        .expect("resume");
    assert_eq!(second.ids().len(), 1);
    assert_ne!(first.ids()[0], second.ids()[0]);
    let again = reducer
        .inspect_pending(
            PageBudget {
                max_items: 1,
                max_work: 1,
            },
            None,
        )
        .expect("replay first");
    assert_eq!(again.ids()[0], first.ids()[0]);
}

#[test]
fn host_time_is_monotonic_and_fake_clock_is_not_public_entry() {
    let (grant, mut reducer) = seeded(1_000);
    assert_eq!(reducer.now_ms(), 1_000);
    assert_eq!(
        reducer.observe_at(999).expect_err("backward host time"),
        ObservationError::StaleRevision
    );
    reducer.observe_at(1_000).expect("same tick");
    reducer
        .observe_at(1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS)
        .expect("forward host time");
    assert_eq!(reducer.now_ms(), 1_000 + ACTIVE_SESSION_IDLE_LIMIT_MS);
}

#[test]
fn retention_evicts_expired_intervals_and_keeps_frozen_map_bounded() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(800)),
        )
        .expect("open");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    assert_eq!(
        reducer
            .inspect_pending(page_all(), None)
            .expect("frozen")
            .ids()
            .len(),
        1
    );
    reducer
        .observe_at(ACTIVE_SESSION_IDLE_LIMIT_MS + MAX_OBSERVATION_RETENTION_MS)
        .expect("expire");
    assert!(reducer
        .inspect_pending(page_all(), None)
        .expect("evicted")
        .ids()
        .is_empty());
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(801)),
        )
        .expect("reopen");
    reducer
        .observe_at(
            ACTIVE_SESSION_IDLE_LIMIT_MS
                + MAX_OBSERVATION_RETENTION_MS
                + ACTIVE_SESSION_IDLE_LIMIT_MS,
        )
        .expect("close successor");
    let pending = reducer
        .inspect_pending(page_all(), None)
        .expect("successor");
    assert_eq!(pending.ids().len(), 1);
    assert!(pending.ids().len() <= MAX_READY_INTERVALS);
}

#[test]
fn unsupported_managed_fields_are_unavailable_and_raw_fields_are_denied() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(810)),
        )
        .expect("source");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    let record: ObservationRecord =
        reducer.ready_page(page_all(), None).expect("page").items()[0].clone();
    let _id: ObservationId = record.observation_id();
    let _page: ObservationPage = reducer.ready_page(page_all(), None).expect("named page");
    assert_eq!(
        record.export_managed_field(ManagedField::HostHealth),
        Err(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource
        ))
    );
    assert_eq!(
        record.export_managed_field(ManagedField::TaskAssignmentReference),
        Err(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource
        ))
    );
    assert_eq!(
        record.export_managed_field(ManagedField::ApprovedArtifactReference),
        Err(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource
        ))
    );
    assert_eq!(
        record.export_managed_field(ManagedField::Prompt),
        Err(ObservationError::ProhibitedContent)
    );
    assert_eq!(
        record.export_managed_field(ManagedField::ProviderQuota),
        Err(ObservationError::Unavailable(
            ObservationDependency::AuthoritativeSource
        ))
    );
    record
        .export_managed_field(ManagedField::TaskState)
        .expect("task state is on the record");
    record
        .export_managed_field(ManagedField::ActiveSessionInterval)
        .expect("interval is on the record");
}

#[test]
fn revoked_grant_cannot_bind_observation_authority() {
    let grant = live_grant();
    grant.revoke();
    assert_eq!(
        ObservationAuthority::from_grant(&grant).expect_err("revoked"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );
}

#[test]
fn instant_expired_grant_cannot_bind_observation_authority() {
    let grant = ManagementGrant::issued_at_for_test(
        task_id(1),
        HostTimeAuthority::at_test_millis(0),
        HostTimeAuthority::at_test_millis(10),
    );
    assert_eq!(
        ObservationAuthority::from_grant_at(&grant, HostTimeAuthority::at_test_millis(10))
            .expect_err("expired"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );
    let live = ManagementGrant::issued_at_for_test(
        task_id(1),
        HostTimeAuthority::at_test_millis(0),
        HostTimeAuthority::at_test_millis(50),
    );
    ObservationAuthority::from_grant_at(&live, HostTimeAuthority::at_test_millis(10))
        .expect("inside validity interval");
}

#[test]
fn revoke_after_bind_stops_observe_and_record() {
    let (grant, mut reducer) = seeded(0);
    grant.revoke();
    assert_eq!(
        reducer.observe_at(1).expect_err("revoked lease"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );
    assert_eq!(
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(900))
            )
            .expect_err("revoked record"),
        ObservationError::Unavailable(ObservationDependency::AuthoritativeSource)
    );
}

#[test]
fn observe_at_backpressure_does_not_drop_already_frozen_intervals() {
    let (grant, mut reducer) = seeded(0);
    for step in 0..MAX_READY_INTERVALS {
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(400 + step as u32)),
            )
            .expect("fill");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("close");
    }
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(500)),
        )
        .expect("one more open interval");
    let cap_page = PageBudget {
        max_items: MAX_READY_INTERVALS as u16,
        max_work: 256,
    };
    let before = reducer
        .inspect_pending(cap_page, None)
        .expect("before")
        .ids()
        .to_vec();
    assert_eq!(before.len(), MAX_READY_INTERVALS);
    let would_close = reducer
        .now_ms()
        .checked_add(ACTIVE_SESSION_IDLE_LIMIT_MS)
        .expect("close one more");
    assert_eq!(
        reducer
            .observe_at(would_close)
            .expect_err("one more closed interval would exceed the ready cap"),
        ObservationError::Backpressure
    );
    let after = reducer
        .inspect_pending(cap_page, None)
        .expect("unchanged")
        .ids()
        .to_vec();
    assert_eq!(after, before);
}

#[test]
fn delivery_stays_unavailable_after_host_time_bind() {
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(820)),
        )
        .expect("source");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    let id = reducer.ready_page(page_all(), None).expect("page").items()[0].observation_id();
    assert_eq!(
        reducer.request_delivery(id).expect_err("no outbox"),
        ObservationError::Unavailable(ObservationDependency::DurableOutbox)
    );
    assert_eq!(
        reducer.acknowledge(id).expect_err("no ack"),
        ObservationError::Unavailable(ObservationDependency::DurableOutbox)
    );
    assert_eq!(
        reducer
            .request_organization_publication(id)
            .expect_err("no portal"),
        ObservationError::Unavailable(ObservationDependency::PortalObservationEffect)
    );
}

#[test]
fn record_star_rejects_foreign_grant_task_id() {
    let (grant, mut reducer) = seeded(0);
    let foreign = task_id(2);
    let denied = ObservationError::Unavailable(ObservationDependency::AuthoritativeSource);
    assert_eq!(
        reducer
            .record_task_facts(
                &grant,
                TaskObservationFacts::try_new(
                    foreign,
                    TaskLifecycle::Open,
                    TaskAttention::None,
                    TaskConnectivity::Connected,
                    None,
                    Vec::new(),
                    0,
                    1,
                )
                .expect("facts"),
            )
            .expect_err("facts"),
        denied
    );
    assert_eq!(
        reducer
            .record_activity(&grant, human_command(foreign, client_id(2), event_id(910)))
            .expect_err("activity"),
        denied
    );
    assert_eq!(
        reducer
            .record_non_qualifying_provider_cpu(&grant, foreign, event_id(911))
            .expect_err("cpu"),
        denied
    );
    assert_eq!(
        reducer
            .record_message(
                &grant,
                foreign,
                event_id(912),
                ObservationMessageClass::Human,
                true
            )
            .expect_err("message"),
        denied
    );
    let measure = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(1),
        "tokens",
        None,
        0,
        1,
    )
    .expect("usage");
    assert_eq!(
        reducer
            .record_usage(&grant, foreign, measure)
            .expect_err("usage"),
        denied
    );
    assert_eq!(
        reducer
            .record_git(
                &grant,
                foreign,
                RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), 1, 1, 0)
                    .expect("git"),
                event_id(913),
                0,
                1,
            )
            .expect_err("git"),
        denied
    );
    assert_eq!(
        reducer.current_observation(foreign).expect_err("current"),
        denied
    );
}

#[test]
fn ready_page_preflights_encoded_connect_page_bound() {
    assert_eq!(
        reject_encoded_page_bytes(MAX_CONNECT_PAGE_ENCODED_BYTES as usize + 1)
            .expect_err("over page cap"),
        ObservationError::BoundExceeded
    );
    reject_encoded_page_bytes(MAX_CONNECT_PAGE_ENCODED_BYTES as usize).expect("at page cap");
    reject_encoded_page_bytes(0).expect("empty page");

    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(920)),
        )
        .expect("source");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    let page = reducer.ready_page(page_all(), None).expect("publish");
    let mut total = 0usize;
    for item in page.items() {
        let encoded = encode_observation(item).expect("encode");
        total = total.checked_add(encoded.len()).expect("sum");
        assert!(encoded.len() <= MAX_OBSERVATION_DOCUMENT_BYTES);
    }
    assert!(total <= MAX_CONNECT_PAGE_ENCODED_BYTES as usize);
    assert!(!page.items().is_empty());
}

#[test]
fn reducer_and_state_debug_redact_usage_git_and_evidence_bodies() {
    let (grant, mut reducer) = seeded(1_000);
    let usage = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(424_242),
        "tokens",
        None,
        1_000,
        1,
    )
    .expect("usage");
    let git = RestrictiveGitSummary::try_new(Some("secret-branch-zz"), Some("deadbee"), 7, 9, 3)
        .expect("git");
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(930)),
        )
        .expect("source");
    reducer
        .record_usage(&grant, task_id(1), usage.clone())
        .expect("usage");
    reducer
        .record_git(&grant, task_id(1), git.clone(), event_id(931), 1_000, 1)
        .expect("git");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("close");
    let page = reducer.ready_page(page_all(), None).expect("page");
    let reducer_debug = format!("{reducer:?}");
    let record_debug = format!("{:?}", page.items()[0]);
    assert_eq!(format!("{usage:?}"), "<redacted>");
    assert!(!format!("{git:?}").contains("secret-branch-zz"));
    assert!(!format!("{git:?}").contains("deadbee"));
    for needle in ["424242", "secret-branch-zz", "deadbee"] {
        assert!(
            !reducer_debug.contains(needle),
            "reducer debug leaked {needle} via {reducer_debug}"
        );
    }
    assert!(record_debug.contains("<redacted>"));
    assert!(!record_debug.contains("secret-branch-zz"));
}

#[test]
fn inspect_pending_uses_actual_encoded_page_bounds_not_n_times_constants() {
    let (grant, mut reducer) = seeded(0);
    let closed = 40usize;
    assert!(
        closed
            .checked_mul(MAX_OBSERVATION_DOCUMENT_BYTES)
            .expect("coarse")
            > MAX_CONNECT_PAGE_ENCODED_BYTES as usize,
        "ledger-wide n*document must exceed the encoded page cap"
    );
    for step in 0..closed {
        reducer
            .record_activity(
                &grant,
                human_command(task_id(1), client_id(1), event_id(940 + step as u32)),
            )
            .expect("pulse");
        reducer
            .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
            .expect("close");
    }
    let ids = reducer
        .inspect_pending(page_all(), None)
        .expect("inspect uses actual encoded sizes");
    let page = reducer
        .ready_page(page_all(), None)
        .expect("ready uses actual encoded sizes");
    assert_eq!(ids.ids().len(), page.items().len());
    assert!(!ids.ids().is_empty());
    let mut total = 0usize;
    for item in page.items() {
        let encoded = encode_observation(item).expect("encode");
        total = total.checked_add(encoded.len()).expect("sum");
        assert!(encoded.len() <= MAX_OBSERVATION_DOCUMENT_BYTES);
    }
    assert!(total <= MAX_CONNECT_PAGE_ENCODED_BYTES as usize);
    assert_eq!(ids.more(), page.more());
}

#[test]
fn record_star_rejects_foreign_grant_identity_via_matches_grant() {
    let (grant, mut reducer) = seeded(0);
    let foreign_grant = ManagementGrant::issued_for_test(task_id(1));
    let denied = ObservationError::Unavailable(ObservationDependency::AuthoritativeSource);
    assert_eq!(
        reducer
            .record_task_facts(&foreign_grant, facts(0, 2))
            .expect_err("facts"),
        denied
    );
    assert_eq!(
        reducer
            .record_activity(
                &foreign_grant,
                human_command(task_id(1), client_id(1), event_id(950)),
            )
            .expect_err("activity"),
        denied
    );
    assert_eq!(
        reducer
            .record_non_qualifying_provider_cpu(&foreign_grant, task_id(1), event_id(951))
            .expect_err("cpu"),
        denied
    );
    assert_eq!(
        reducer
            .record_message(
                &foreign_grant,
                task_id(1),
                event_id(952),
                ObservationMessageClass::Human,
                true,
            )
            .expect_err("message"),
        denied
    );
    let measure = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(1),
        "tokens",
        None,
        0,
        1,
    )
    .expect("usage");
    assert_eq!(
        reducer
            .record_usage(&foreign_grant, task_id(1), measure)
            .expect_err("usage"),
        denied
    );
    assert_eq!(
        reducer
            .record_git(
                &foreign_grant,
                task_id(1),
                RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), 1, 1, 0)
                    .expect("git"),
                event_id(953),
                0,
                1,
            )
            .expect_err("git"),
        denied
    );
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(954)),
        )
        .expect("bound grant still records");
}

#[test]
fn revoke_during_record_does_not_commit_state() {
    let denied = ObservationError::Unavailable(ObservationDependency::AuthoritativeSource);
    let task = task_id(1);

    let (grant, mut reducer) = seeded(0);
    assert_eq!(reducer.recorded_facts_revision(task), Some(1));
    reducer.arm_revoke_before_commit();
    assert_eq!(
        reducer
            .record_task_facts(&grant, facts(0, 2))
            .expect_err("facts"),
        denied
    );
    assert_eq!(reducer.recorded_facts_revision(task), Some(1));

    let (grant, mut reducer) = seeded(0);
    reducer.arm_revoke_before_commit();
    assert_eq!(
        reducer
            .record_activity(&grant, human_command(task, client_id(1), event_id(960)),)
            .expect_err("activity"),
        denied
    );
    assert_eq!(reducer.recorded_activity_len(task), 0);

    let (grant, mut reducer) = seeded(0);
    reducer.arm_revoke_before_commit();
    assert_eq!(
        reducer
            .record_non_qualifying_provider_cpu(&grant, task, event_id(961))
            .expect_err("cpu"),
        denied
    );

    let (grant, mut reducer) = seeded(0);
    reducer.arm_revoke_before_commit();
    assert_eq!(
        reducer
            .record_message(
                &grant,
                task,
                event_id(962),
                ObservationMessageClass::Human,
                true,
            )
            .expect_err("message"),
        denied
    );
    assert_eq!(reducer.recorded_human_message_count(task), 0);

    let (grant, mut reducer) = seeded(1_000);
    reducer.arm_revoke_before_commit();
    let measure = UsageMeasure::try_new(
        "claude",
        "cli.status",
        UsageKind::Tokens,
        UsageProvenance::ProviderReported,
        Some(77),
        "tokens",
        None,
        1_000,
        1,
    )
    .expect("usage");
    assert_eq!(
        reducer
            .record_usage(&grant, task, measure)
            .expect_err("usage"),
        denied
    );
    assert!(!reducer.recorded_has_usage_tokens(task));

    let (grant, mut reducer) = seeded(0);
    reducer.arm_revoke_before_commit();
    assert_eq!(
        reducer
            .record_git(
                &grant,
                task,
                RestrictiveGitSummary::try_new(Some("main"), Some("abc1234"), 1, 1, 0)
                    .expect("git"),
                event_id(963),
                0,
                1,
            )
            .expect_err("git"),
        denied
    );
    assert!(!reducer.recorded_has_git(task));
}

#[test]
fn revoke_during_observe_at_does_not_settle_evict_or_publish() {
    let denied = ObservationError::Unavailable(ObservationDependency::AuthoritativeSource);
    let (grant, mut reducer) = seeded(0);
    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(970)),
        )
        .expect("open");
    reducer
        .advance(Duration::from_millis(ACTIVE_SESSION_IDLE_LIMIT_MS))
        .expect("first settle");
    let settled = reducer.ready_page(page_all(), None).expect("published");
    assert_eq!(settled.items().len(), 1);
    let settled_id = settled.items()[0].observation_id();
    assert_eq!(reducer.recorded_frozen_ids(), vec![settled_id]);

    reducer
        .record_activity(
            &grant,
            human_command(task_id(1), client_id(1), event_id(971)),
        )
        .expect("second open");
    reducer.arm_revoke_before_observe_commit();
    assert_eq!(
        reducer
            .advance(Duration::from_millis(MAX_OBSERVATION_RETENTION_MS))
            .expect_err("revoked during freeze/evict"),
        denied
    );
    assert_eq!(reducer.now_ms(), ACTIVE_SESSION_IDLE_LIMIT_MS);
    assert_eq!(reducer.recorded_frozen_ids(), vec![settled_id]);
    assert_eq!(
        reducer.ready_page(page_all(), None).expect_err("no page"),
        denied
    );
    assert_eq!(
        reducer
            .inspect_pending(page_all(), None)
            .expect_err("no inspect"),
        denied
    );
    assert_eq!(
        reducer
            .current_observation(task_id(1))
            .expect_err("no reconstruct from current state"),
        denied
    );
}
