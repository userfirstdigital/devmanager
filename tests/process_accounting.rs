use std::sync::Arc;
use std::time::Duration;

use devmanager::domain::snapshot::ProcessAccountingSnapshot;
use devmanager::process::identity::{ManagedProcessId, ManagedProcessIdentity};
use devmanager::process::job::{collect_exact_job_observations, JobMemberObservation};
use devmanager::process::sampler::{
    require_exact_process_identity, AccessibleProcess, InaccessibleProcess,
    ProcessMemberObservation, ProcessSampler, SamplerError,
};
use devmanager::state::ResourceSnapshot;

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
fn duplicate_job_and_ancestry_observations_are_counted_once() {
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
