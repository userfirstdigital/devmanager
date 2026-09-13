use std::sync::Arc;
use std::time::{Duration, Instant};

use devmanager::domain::snapshot::{ProcessAccountingSnapshot, ProcessMetricStatus};
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity};
use devmanager::process::job::{
    collect_exact_job_observations, collect_exact_job_observations_with_budget,
    JobMemberObservation,
};
use devmanager::process::registry::JobMemberInfo;
use devmanager::process::sampler::{
    require_exact_process_identity, AccessibleProcess, InaccessibleProcess,
    ProcessMemberObservation, ProcessSampler, SamplerError, SamplingBudget,
};
use devmanager::state::{ResourceMetricValueState, ResourceSnapshot};

const LOGICAL_PROCESSORS: u32 = 8;
const WALL_TICKS: u64 = 1_000_000;

fn identity(pid: u32, creation_time_100ns: u64) -> ManagedProcessIdentity {
    ManagedProcessIdentity::new(
        ManagedProcessId::new(pid, creation_time_100ns).expect("valid process id"),
        std::env::current_exe().expect("test executable"),
    )
    .expect("test executable must be canonicalizable")
}

fn accessible(
    pid: u32,
    creation_time_100ns: u64,
    cpu_time_100ns: u64,
    private_memory_bytes: u64,
) -> ProcessMemberObservation {
    ProcessMemberObservation::Accessible(AccessibleProcess::new(
        identity(pid, creation_time_100ns),
        cpu_time_100ns,
        private_memory_bytes,
    ))
}

fn inaccessible(pid: u32, creation_time_100ns: u64) -> ProcessMemberObservation {
    ProcessMemberObservation::Inaccessible(InaccessibleProcess::new(pid, Some(creation_time_100ns)))
}

fn sample(
    sampler: &mut ProcessSampler,
    sampled_at: Duration,
    members: impl IntoIterator<Item = ProcessMemberObservation>,
) -> Arc<ProcessAccountingSnapshot> {
    sampler
        .sample_at(sampled_at, LOGICAL_PROCESSORS, members)
        .expect("valid accounting sample")
}

#[test]
fn one_saturated_core_is_twelve_point_five_machine_percent_on_eight_processors() {
    let mut sampler = ProcessSampler::new();
    sample(&mut sampler, Duration::ZERO, [accessible(101, 1, 0, 1_000)]);

    let snapshot = sample(
        &mut sampler,
        Duration::from_millis(100),
        [accessible(101, 1, WALL_TICKS, 1_200)],
    );

    assert_eq!(snapshot.machine_cpu_percent, 12.5);
    assert_eq!(snapshot.core_equivalent_percent, 100.0);
}

#[test]
fn eight_saturated_cores_are_clamped_to_one_hundred_machine_percent() {
    let mut sampler = ProcessSampler::new();
    let initial = (1..=8).map(|pid| accessible(pid, 1, 0, 1_000));
    sample(&mut sampler, Duration::ZERO, initial);

    let saturated = (1..=8).map(|pid| accessible(pid, 1, WALL_TICKS, 1_000));
    let snapshot = sample(&mut sampler, Duration::from_millis(100), saturated);

    assert_eq!(snapshot.machine_cpu_percent, 100.0);
    assert_eq!(snapshot.core_equivalent_percent, 800.0);
}

#[test]
fn zero_and_backwards_intervals_are_rejected() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::from_millis(10),
        [accessible(102, 1, 0, 1_000)],
    );

    assert_eq!(
        sampler
            .sample_at(
                Duration::from_millis(10),
                LOGICAL_PROCESSORS,
                [accessible(102, 1, 1, 1_000)],
            )
            .unwrap_err(),
        SamplerError::InvalidInterval
    );
    assert_eq!(
        sampler
            .sample_at(
                Duration::from_millis(9),
                LOGICAL_PROCESSORS,
                [accessible(102, 1, 1, 1_000)],
            )
            .unwrap_err(),
        SamplerError::InvalidInterval
    );
}

#[test]
fn invalid_logical_processor_count_is_rejected() {
    let mut sampler = ProcessSampler::new();

    assert_eq!(
        sampler
            .sample_at(Duration::ZERO, 0, [accessible(103, 1, 0, 1_000)],)
            .unwrap_err(),
        SamplerError::InvalidLogicalProcessorCount
    );
}

#[test]
fn machine_cpu_percent_is_clamped_for_large_or_non_finite_raw_values() {
    assert_eq!(
        devmanager::process::sampler::machine_cpu_percent(800.0, LOGICAL_PROCESSORS),
        100.0
    );
    assert_eq!(
        devmanager::process::sampler::machine_cpu_percent(f64::INFINITY, LOGICAL_PROCESSORS),
        0.0
    );
    assert_eq!(
        devmanager::process::sampler::machine_cpu_percent(-1.0, LOGICAL_PROCESSORS),
        0.0
    );
}

#[test]
fn pid_reuse_does_not_inherit_the_previous_cpu_baseline() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(104, 11, 0, 1_000)],
    );
    sample(
        &mut sampler,
        Duration::from_millis(100),
        [accessible(104, 11, WALL_TICKS, 1_000)],
    );

    let reused = sample(
        &mut sampler,
        Duration::from_millis(200),
        [accessible(104, 22, WALL_TICKS * 2, 1_000)],
    );

    assert_eq!(reused.process_count, 1);
    assert_eq!(reused.machine_cpu_percent, 0.0);
    assert_eq!(reused.core_equivalent_percent, 0.0);
}

#[test]
fn inaccessible_job_members_are_counted_and_mark_metrics_partial() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(105, 1, 0, 4_000), inaccessible(106, 1)],
    );

    let snapshot = sample(
        &mut sampler,
        Duration::from_millis(100),
        [accessible(105, 1, WALL_TICKS, 5_000), inaccessible(106, 1)],
    );

    assert_eq!(snapshot.process_count, 2);
    assert!(snapshot.metrics_unavailable);
    assert_eq!(snapshot.memory_bytes, 5_000);
}

#[test]
fn partial_sampler_diagnostic_is_a_fixed_opaque_code() {
    let mut sampler = ProcessSampler::new();
    let inaccessible = ProcessMemberObservation::Inaccessible(
        InaccessibleProcess::new(107, Some(1))
            .with_reason(r"access denied at C:\private\project --token=secret"),
    );

    let snapshot = sample(&mut sampler, Duration::ZERO, [inaccessible]);

    assert_eq!(snapshot.status, ProcessMetricStatus::Partial);
    assert_eq!(
        snapshot.error.as_deref(),
        Some("member_metrics_unavailable")
    );
}

#[test]
fn bounded_job_observation_never_materializes_more_than_the_tick_cap() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 512);
    let process_ids = (1..=513).collect::<Vec<_>>();
    let mut inspected = 0usize;

    let result = collect_exact_job_observations_with_budget(
        Ok(process_ids),
        |pid| {
            inspected += 1;
            panic!("PID {pid} should not be inspected after the cap is rejected")
        },
        &mut budget,
    );

    let error = result.expect_err("oversized Job must fail before inspection");
    assert!(error.contains("512"), "unexpected bounded error: {error}");
    assert_eq!(inspected, 0);
    assert!(budget.claimed_members() <= 512);
}

#[test]
fn one_tick_budget_caps_job_members_across_multiple_jobs() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 512);
    let first = collect_exact_job_observations_with_budget(
        Ok((1..=400).collect::<Vec<_>>()),
        |_| Err("inaccessible".to_string()),
        &mut budget,
    )
    .expect("first Job remains bounded");
    assert_eq!(first.len(), 400);
    assert_eq!(budget.claimed_members(), 400);

    let result = collect_exact_job_observations_with_budget(
        Ok((401..=601).collect::<Vec<_>>()),
        |_| panic!("second Job must not inspect past the shared cap"),
        &mut budget,
    );
    let error = result.expect_err("second Job must fail closed at the shared cap");
    assert!(
        error.contains("112"),
        "unexpected shared-cap error: {error}"
    );
    assert_eq!(budget.claimed_members(), 400);
}

#[test]
fn one_tick_budget_deduplicates_the_same_exact_job_identity_before_the_cap() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 1);
    let first = collect_exact_job_observations_with_budget(
        Ok(vec![611]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 7), None)),
        &mut budget,
    )
    .expect("first exact Job identity");
    let duplicate = collect_exact_job_observations_with_budget(
        Ok(vec![611]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 7), None)),
        &mut budget,
    )
    .expect("the same exact identity must not consume a second member slot");

    assert_eq!(first.len(), 1);
    assert_eq!(duplicate.len(), 1);
    assert_eq!(budget.claimed_members(), 1);
    assert_eq!(budget.remaining_members(), 0);
    let work = budget.work_counters();
    assert_eq!(work.job_queries, 2);
    assert_eq!(work.job_candidates, 2);
    assert_eq!(work.identity_inspections, 2);
}

#[test]
fn one_tick_budget_rejects_conflicting_job_identity_for_the_same_pid() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 2);
    collect_exact_job_observations_with_budget(
        Ok(vec![612]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 7), None)),
        &mut budget,
    )
    .expect("first exact Job identity");

    let conflict = collect_exact_job_observations_with_budget(
        Ok(vec![612]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 8), None)),
        &mut budget,
    )
    .expect_err("PID reuse during one tick must fail closed");

    assert!(conflict.contains("conflicting process identities"));
    assert_eq!(budget.claimed_members(), 1);
}

#[test]
fn preadmitted_job_pid_still_rejects_a_conflicting_metric_generation() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 1);
    collect_exact_job_observations_with_budget(
        Ok(vec![614]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 7), None)),
        &mut budget,
    )
    .expect("authoritative Job identity");
    let mut sampler = ProcessSampler::new();

    let error = sampler
        .sample_at_with_budget(
            Duration::ZERO,
            LOGICAL_PROCESSORS,
            [accessible(614, 8, 0, 1_000)],
            &mut budget,
        )
        .expect_err("a preadmitted PID must not bypass exact generation validation");

    assert_eq!(error, SamplerError::ConflictingProcessIdentity { pid: 614 });
    assert_eq!(budget.claimed_members(), 1);
}

#[test]
fn unknown_inaccessible_generation_cannot_merge_with_an_exact_job_identity() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 2);
    collect_exact_job_observations_with_budget(
        Ok(vec![613]),
        |_| Err("access denied".to_string()),
        &mut budget,
    )
    .expect("inaccessible Job member remains authoritative membership");

    let conflict = collect_exact_job_observations_with_budget(
        Ok(vec![613]),
        |pid| Ok(JobMemberInfo::new(identity(pid, 9), None)),
        &mut budget,
    )
    .expect_err("an inaccessible unknown generation must not grant a later exact identity");

    assert!(conflict.contains("conflicting process identities"));
    assert_eq!(budget.claimed_members(), 1);
}

#[test]
fn job_member_inspection_stops_when_the_shared_deadline_expires() {
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_millis(20), 512);
    let mut inspected = 0usize;
    let result = collect_exact_job_observations_with_budget(
        Ok(vec![701, 702]),
        |pid| {
            inspected += 1;
            std::thread::sleep(Duration::from_millis(30));
            Ok(JobMemberInfo::new(identity(pid, 1), None))
        },
        &mut budget,
    );

    let error = result.expect_err("expired member inspection must fail closed");
    assert!(error.contains("sampling work budget exceeded"));
    assert_eq!(inspected, 1, "the second member must not be inspected");
    assert_eq!(budget.claimed_members(), 0);
}

#[test]
fn accounting_projection_redacts_executable_identity() {
    let mut sampler = ProcessSampler::new();
    let snapshot = sample(&mut sampler, Duration::ZERO, [accessible(901, 7, 0, 1024)]);
    let executable = snapshot.members[0]
        .executable
        .as_deref()
        .expect("accessible member has a display executable");
    let canonical = std::env::current_exe().expect("test executable");

    assert!(!executable.contains(canonical.to_string_lossy().as_ref()));
    assert_eq!(
        executable,
        canonical
            .file_name()
            .expect("test executable basename")
            .to_string_lossy()
    );
}

#[test]
fn duplicate_exact_member_observations_are_counted_once() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(107, 1, 0, 2_000), accessible(107, 1, 0, 2_000)],
    );

    let snapshot = sample(
        &mut sampler,
        Duration::from_millis(100),
        [
            accessible(107, 1, WALL_TICKS, 3_000),
            accessible(107, 1, WALL_TICKS, 3_000),
        ],
    );

    assert_eq!(snapshot.process_count, 1);
    assert_eq!(snapshot.memory_bytes, 3_000);
    assert_eq!(snapshot.core_equivalent_percent, 100.0);
}

#[test]
fn exited_member_is_removed_and_does_not_contribute_to_the_next_delta() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(108, 1, 0, 1_000), accessible(109, 1, 0, 1_000)],
    );
    sample(
        &mut sampler,
        Duration::from_millis(100),
        [
            accessible(108, 1, WALL_TICKS, 1_000),
            accessible(109, 1, WALL_TICKS, 1_000),
        ],
    );

    let after_exit = sample(
        &mut sampler,
        Duration::from_millis(200),
        [accessible(108, 1, WALL_TICKS * 2, 1_000)],
    );
    assert_eq!(after_exit.process_count, 1);
    assert_eq!(after_exit.core_equivalent_percent, 100.0);

    let reappeared = sample(
        &mut sampler,
        Duration::from_millis(300),
        [
            accessible(108, 1, WALL_TICKS * 3, 1_000),
            accessible(109, 1, WALL_TICKS * 3, 1_000),
        ],
    );
    assert_eq!(reappeared.process_count, 2);
    assert_eq!(reappeared.core_equivalent_percent, 100.0);
}

#[test]
fn memory_is_summed_once_per_process_identity() {
    let mut sampler = ProcessSampler::new();
    let snapshot = sample(
        &mut sampler,
        Duration::ZERO,
        [
            accessible(110, 1, 0, 4_000),
            accessible(110, 1, 0, 4_000),
            accessible(111, 1, 0, 6_000),
        ],
    );

    assert_eq!(snapshot.memory_bytes, 10_000);
    assert_eq!(snapshot.process_count, 2);
}

#[test]
fn optional_io_counters_are_reported_as_deltas_once_per_member() {
    let mut sampler = ProcessSampler::new();
    sampler
        .sample_at(
            Duration::ZERO,
            LOGICAL_PROCESSORS,
            [ProcessMemberObservation::Accessible(
                AccessibleProcess::new(identity(113, 1), 0, 1_000).with_io_bytes(100, 200),
            )],
        )
        .expect("baseline sample");

    let snapshot = sampler
        .sample_at(
            Duration::from_millis(100),
            LOGICAL_PROCESSORS,
            [ProcessMemberObservation::Accessible(
                AccessibleProcess::new(identity(113, 1), WALL_TICKS, 1_000).with_io_bytes(175, 260),
            )],
        )
        .expect("delta sample");

    assert_eq!(snapshot.io_read_bytes, Some(75));
    assert_eq!(snapshot.io_write_bytes, Some(60));
    assert_eq!(snapshot.members[0].io_read_bytes, Some(75));
    assert_eq!(snapshot.members[0].io_write_bytes, Some(60));
}

#[test]
fn partial_observation_retains_available_io_deltas() {
    let mut sampler = ProcessSampler::new();
    sampler
        .sample_at(
            Duration::ZERO,
            LOGICAL_PROCESSORS,
            [
                ProcessMemberObservation::Accessible(
                    AccessibleProcess::new(identity(114, 1), 0, 1_000).with_io_bytes(100, 200),
                ),
                inaccessible(115, 1),
            ],
        )
        .expect("baseline sample");

    let snapshot = sampler
        .sample_at(
            Duration::from_millis(100),
            LOGICAL_PROCESSORS,
            [
                ProcessMemberObservation::Accessible(
                    AccessibleProcess::new(identity(114, 1), WALL_TICKS, 1_250)
                        .with_io_bytes(175, 260),
                ),
                inaccessible(115, 1),
            ],
        )
        .expect("partial sample");

    assert!(snapshot.metrics_unavailable);
    assert_eq!(snapshot.io_read_bytes, Some(75));
    assert_eq!(snapshot.io_write_bytes, Some(60));
    assert_eq!(snapshot.process_count, 2);
}

#[test]
fn empty_job_observation_is_authoritative_but_unavailable_job_observation_allows_fallback() {
    let mut inspected = false;
    let empty = collect_exact_job_observations(Ok(Vec::new()), |_| {
        inspected = true;
        unreachable!("an empty authoritative Job must not inspect or synthesize members")
    })
    .expect("empty Job observation");
    assert!(empty.is_empty());
    assert!(!inspected);

    let unavailable = collect_exact_job_observations(
        Err::<Vec<u32>, _>("Job membership unavailable".to_string()),
        |_| unreachable!("unavailable Job observations must be handled by the caller"),
    )
    .expect_err("an unavailable Job must remain distinguishable from an empty Job");
    assert_eq!(unavailable, "Job membership unavailable");
}

#[test]
fn exact_job_observation_rejects_a_reused_pid_identity() {
    let expected = identity(116, 10);
    let reused = identity(116, 11);
    assert!(require_exact_process_identity(&expected, &reused).is_err());

    let observations = collect_exact_job_observations(Ok(vec![116]), |_| {
        Err("PID 116 was reused before exact Job inspection".to_string())
    })
    .expect("Job observation");
    assert!(matches!(
        observations.as_slice(),
        [JobMemberObservation::Inaccessible { pid: 116, .. }]
    ));
}

#[test]
fn runtime_resource_snapshot_retains_partial_metrics_and_io_deltas() {
    let snapshot = ResourceSnapshot {
        metrics_unavailable: true,
        io_read_bytes: Some(75),
        io_write_bytes: Some(60),
        ..ResourceSnapshot::default()
    };
    let encoded = serde_json::to_string(&snapshot).expect("resource snapshot JSON");
    let decoded: ResourceSnapshot = serde_json::from_str(&encoded).expect("resource snapshot");

    assert!(decoded.metrics_unavailable);
    assert_eq!(decoded.io_read_bytes, Some(75));
    assert_eq!(decoded.io_write_bytes, Some(60));
}

#[test]
fn resource_snapshot_tracks_cpu_and_memory_confidence_independently() {
    let snapshot = ResourceSnapshot {
        cpu_percent: 0.0,
        memory_bytes: 4_096,
        cpu_value_state: ResourceMetricValueState::Unavailable,
        memory_value_state: ResourceMetricValueState::Observed,
        metric_values: ResourceMetricValueState::Partial,
        ..ResourceSnapshot::default()
    };

    let encoded = serde_json::to_string(&snapshot).expect("resource snapshot JSON");
    let decoded: ResourceSnapshot = serde_json::from_str(&encoded).expect("resource snapshot");
    assert_eq!(
        decoded.cpu_value_state,
        ResourceMetricValueState::Unavailable
    );
    assert_eq!(
        decoded.memory_value_state,
        ResourceMetricValueState::Observed
    );
    assert_eq!(decoded.metric_values, ResourceMetricValueState::Partial);
}

#[test]
fn resource_snapshot_preserves_unclamped_core_equivalent_diagnostics() {
    let snapshot = ResourceSnapshot {
        cpu_percent: 100.0,
        core_equivalent_percent: 1_600.0,
        logical_cpu_count: 8,
        ..ResourceSnapshot::default()
    };

    assert_eq!(snapshot.equivalent_cpu_cores(), 16.0);
    let encoded = serde_json::to_string(&snapshot).expect("resource snapshot JSON");
    let decoded: ResourceSnapshot = serde_json::from_str(&encoded).expect("resource snapshot");
    assert_eq!(decoded.core_equivalent_percent, 1_600.0);
    assert_eq!(decoded.equivalent_cpu_cores(), 16.0);
}

#[test]
fn prior_snapshot_is_unchanged_after_the_sampler_advances() {
    let mut sampler = ProcessSampler::new();
    let first = sample(&mut sampler, Duration::ZERO, [accessible(112, 1, 0, 1_000)]);
    let first_copy = first.clone();

    let _second = sample(
        &mut sampler,
        Duration::from_millis(100),
        [accessible(112, 1, WALL_TICKS, 2_000)],
    );

    assert_eq!(first.memory_bytes, 1_000);
    assert_eq!(first.machine_cpu_percent, 0.0);
    assert!(Arc::ptr_eq(&first, &first_copy));
}

#[test]
fn first_sample_does_not_report_zero_as_a_known_cpu_measurement() {
    let mut sampler = ProcessSampler::new();
    let snapshot = sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(117, 1, 10, 1_000)],
    );

    assert_eq!(snapshot.status, ProcessMetricStatus::Unknown);
    assert_eq!(snapshot.members[0].machine_cpu_percent, None);
    assert_eq!(snapshot.members[0].core_equivalent_percent, None);
}

#[test]
fn a_counter_reset_is_partial_and_does_not_reuse_the_old_cpu_delta() {
    let mut sampler = ProcessSampler::new();
    sample(
        &mut sampler,
        Duration::ZERO,
        [accessible(118, 1, 100, 1_000)],
    );
    sample(
        &mut sampler,
        Duration::from_millis(100),
        [accessible(118, 1, 200, 1_000)],
    );

    let reset = sample(
        &mut sampler,
        Duration::from_millis(200),
        [accessible(118, 1, 50, 1_000)],
    );

    assert_eq!(reset.status, ProcessMetricStatus::Partial);
    assert_eq!(reset.machine_cpu_percent, 0.0);
    assert_eq!(reset.members[0].machine_cpu_percent, None);

    let after_reset = sample(
        &mut sampler,
        Duration::from_millis(300),
        [accessible(118, 1, WALL_TICKS + 50, 1_000)],
    );
    assert_eq!(after_reset.status, ProcessMetricStatus::Complete);
    assert_eq!(after_reset.members[0].core_equivalent_percent, Some(100.0));
}

#[test]
fn conflicting_observations_for_one_pid_are_retained_as_one_inaccessible_member() {
    let unique = devmanager::process::sampler::unique_members([
        accessible(119, 1, 0, 1_000),
        accessible(119, 2, 0, 2_000),
    ]);

    assert_eq!(unique.len(), 1);
    assert!(matches!(
        &unique[0],
        ProcessMemberObservation::Inaccessible(member)
            if member.pid == 119 && member.reason.as_deref() == Some("conflicting process identities")
    ));
}

#[test]
fn inaccessible_pid_with_a_different_generation_cannot_grant_accessible_identity() {
    let unique = devmanager::process::sampler::unique_members([
        inaccessible(121, 1),
        accessible(121, 2, 0, 1_000),
    ]);

    assert!(matches!(
        &unique[..],
        [ProcessMemberObservation::Inaccessible(member)]
            if member.pid == 121
                && member.creation_time_100ns.is_none()
                && member.reason.as_deref() == Some("conflicting process identities")
    ));
}

#[test]
fn inaccessible_duplicate_with_the_same_generation_is_replaced_by_accessible_identity() {
    let unique = devmanager::process::sampler::unique_members([
        inaccessible(122, 1),
        accessible(122, 1, 0, 1_000),
    ]);

    assert!(matches!(
        &unique[..],
        [ProcessMemberObservation::Accessible(member)]
            if member.identity.id().pid() == 122
    ));
}

#[test]
fn sampling_budget_fails_closed_before_unbounded_member_work() {
    let mut sampler = ProcessSampler::new();
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 1);
    let error = sampler
        .sample_at_with_budget(
            Duration::ZERO,
            LOGICAL_PROCESSORS,
            [accessible(120, 1, 0, 1_000), accessible(121, 1, 0, 1_000)],
            &mut budget,
        )
        .unwrap_err();

    assert!(matches!(error, SamplerError::WorkBudgetExceeded { .. }));
}

#[test]
fn zero_member_budget_rejects_any_process_observation() {
    let mut sampler = ProcessSampler::new();
    let mut budget = SamplingBudget::new(Instant::now() + Duration::from_secs(1), 0);
    let error = sampler
        .sample_at_with_budget(
            Duration::ZERO,
            LOGICAL_PROCESSORS,
            [accessible(123, 1, 0, 1_000)],
            &mut budget,
        )
        .unwrap_err();

    assert_eq!(
        error,
        SamplerError::WorkBudgetExceeded {
            attempted: 1,
            max: 0
        }
    );
}
