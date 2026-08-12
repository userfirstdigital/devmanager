use devmanager::domain::id::{ClientId, TaskId};
use devmanager::terminal::protocol::{
    ClientInputGrant, CoalesceReason, ReplicaApplyResult, ReplicaUpdate, TerminalSequence,
    TerminalSessionId, TerminalSize, TerminalSpec, MAX_RETAINED_DELTAS,
};
use devmanager::terminal::replica::TerminalReplica;
use devmanager::terminal::service::TerminalService;
use devmanager::terminal::view::TerminalSelectionSnapshot;

fn create_service() -> (
    TerminalService,
    devmanager::domain::id::TerminalId,
    ClientId,
) {
    let service = TerminalService::new();
    let client = ClientId::new();
    let spec = TerminalSpec {
        session_id: TerminalSessionId::new(),
        size: TerminalSize::new(32, 8).expect("size"),
        max_scrollback_rows: 8,
        max_scrollback_bytes: 2_048,
        title: Some("replica".to_string()),
    }
    .validated()
    .expect("spec");
    let id = service
        .create_fixture(TaskId::new(), spec)
        .expect("create fixture");
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    (service, id, client)
}

fn apply_update(replica: &mut TerminalReplica, update: ReplicaUpdate) {
    match update {
        ReplicaUpdate::Empty => {}
        ReplicaUpdate::Deltas(deltas) => {
            for delta in deltas {
                assert_eq!(replica.apply_delta(delta), ReplicaApplyResult::Applied);
            }
        }
        ReplicaUpdate::Snapshot(snapshot) | ReplicaUpdate::CoalescedSnapshot { snapshot, .. } => {
            replica.apply_snapshot(snapshot);
        }
    }
}

#[test]
fn snapshot_then_contiguous_deltas_match() {
    let (service, id, client) = create_service();
    let mut replica = TerminalReplica::new();
    replica.bind_client(client);
    replica.apply_snapshot(service.snapshot(id).expect("bootstrap snapshot"));

    service
        .admit_reader_bytes(id, b"alpha-line\r\n")
        .expect("alpha");
    let first = service
        .updates_since(
            id,
            client,
            replica.generation().expect("gen"),
            replica.sequence(),
        )
        .expect("first deltas");
    match &first {
        ReplicaUpdate::Deltas(deltas) => {
            assert!(!deltas.is_empty());
            let mut expected = replica.sequence();
            for delta in deltas {
                expected = expected.next().expect("seq");
                assert_eq!(delta.sequence, expected);
                assert_eq!(delta.generation, replica.generation().expect("gen"));
            }
        }
        other => panic!("expected contiguous deltas, got {other:?}"),
    }
    apply_update(&mut replica, first);

    service
        .admit_reader_bytes(id, b"beta-line\r\n")
        .expect("beta");
    let second = service
        .updates_since(
            id,
            client,
            replica.generation().expect("gen"),
            replica.sequence(),
        )
        .expect("second deltas");
    apply_update(&mut replica, second);

    let host = service.snapshot(id).expect("host snapshot");
    assert_eq!(replica.sequence(), host.sequence);
    assert_eq!(replica.generation(), Some(host.generation));
    assert_eq!(replica.rows(), host.rows.as_slice());
    assert!(replica.rows().iter().any(|row| row.contains("alpha-line")));
    assert!(replica.rows().iter().any(|row| row.contains("beta-line")));
    let session = service.session_view(id).expect("session view");
    assert!(
        session.runtime.pid.is_none(),
        "session view must not introduce PID authority"
    );
    assert_eq!(session.runtime.title, host.title);
}

#[test]
fn gap_requests_snapshot() {
    let (service, id, client) = create_service();
    let mut replica = TerminalReplica::new();
    replica.apply_snapshot(service.snapshot(id).expect("snapshot"));
    service.admit_reader_bytes(id, b"one\r\n").expect("one");
    service.admit_reader_bytes(id, b"two\r\n").expect("two");
    service.admit_reader_bytes(id, b"three\r\n").expect("three");

    let gapped = service
        .updates_since(
            id,
            client,
            replica.generation().expect("gen"),
            TerminalSequence::from_raw(0),
        )
        .expect("gap");
    match gapped {
        ReplicaUpdate::Deltas(deltas) if deltas.len() == 3 => {
            let skip = deltas[2].clone();
            assert_eq!(replica.apply_delta(skip), ReplicaApplyResult::NeedSnapshot);
        }
        ReplicaUpdate::CoalescedSnapshot {
            reason: CoalesceReason::SequenceGap | CoalesceReason::SlowClient,
            ..
        } => {}
        other => {
            if let ReplicaUpdate::Deltas(deltas) = other {
                assert_eq!(
                    replica.apply_delta(deltas.last().cloned().expect("delta")),
                    ReplicaApplyResult::NeedSnapshot
                );
            } else {
                panic!("unexpected update {other:?}");
            }
        }
    }
    assert!(replica.needs_snapshot());
}

#[test]
fn generation_mismatch_requests_snapshot() {
    let (service, id, client) = create_service();
    let mut replica = TerminalReplica::new();
    replica.apply_snapshot(service.snapshot(id).expect("snapshot"));
    service
        .admit_reader_bytes(id, b"before\r\n")
        .expect("before");
    let stale_generation = replica.generation().expect("old gen");
    let stale_sequence = replica.sequence();

    let new_generation = service.replace_generation(id).expect("replace");
    assert_ne!(new_generation, stale_generation);
    service.admit_reader_bytes(id, b"after\r\n").expect("after");

    let update = service
        .updates_since(id, client, stale_generation, stale_sequence)
        .expect("mismatch");
    match update {
        ReplicaUpdate::CoalescedSnapshot {
            snapshot,
            reason: CoalesceReason::GenerationMismatch,
        } => {
            assert_eq!(snapshot.generation, new_generation);
            replica.apply_snapshot(snapshot);
        }
        other => panic!("expected generation mismatch snapshot, got {other:?}"),
    }

    let foreign = service.snapshot(id).expect("fresh");
    let mut other = TerminalReplica::new();
    other.apply_snapshot(foreign.clone());
    let mut stale_delta = match service
        .updates_since(id, client, new_generation, TerminalSequence::ZERO)
        .expect("current")
    {
        ReplicaUpdate::Deltas(mut deltas) => deltas.pop().expect("delta"),
        ReplicaUpdate::Snapshot(snapshot) | ReplicaUpdate::CoalescedSnapshot { snapshot, .. } => {
            other.apply_snapshot(snapshot);
            return;
        }
        ReplicaUpdate::Empty => panic!("expected a delta after output"),
    };
    stale_delta.generation = stale_generation;
    assert_eq!(
        other.apply_delta(stale_delta),
        ReplicaApplyResult::NeedSnapshot
    );
}

#[test]
fn slow_client_coalesces_to_snapshot() {
    let (service, id, client) = create_service();
    let origin = service.snapshot(id).expect("origin");
    for index in 0..=MAX_RETAINED_DELTAS {
        let chunk = format!("line-{index}\r\n");
        service
            .admit_reader_bytes(id, chunk.as_bytes())
            .expect("pressure feed");
    }
    let update = service
        .updates_since(id, client, origin.generation, origin.sequence)
        .expect("slow client");
    match update {
        ReplicaUpdate::CoalescedSnapshot {
            snapshot,
            reason: CoalesceReason::SlowClient | CoalesceReason::ScrollbackTruncated,
        } => {
            assert_eq!(snapshot.generation, origin.generation);
            assert!(snapshot.sequence.get() > origin.sequence.get());
        }
        other => panic!("expected coalesced snapshot, got {other:?}"),
    }
}

#[test]
fn two_clients_have_independent_viewport_state() {
    let (service, id, client_a) = create_service();
    let client_b = ClientId::new();
    service
        .grant_client(id, client_b, ClientInputGrant::ReadOnly)
        .expect("grant b");
    service
        .admit_reader_bytes(id, b"shared-grid\r\n")
        .expect("shared");
    let snapshot = service.snapshot(id).expect("shared snapshot");

    let mut replica_a = TerminalReplica::new();
    let mut replica_b = TerminalReplica::new();
    replica_a.bind_client(client_a);
    replica_b.bind_client(client_b);
    replica_a.apply_snapshot(snapshot.clone());
    replica_b.apply_snapshot(snapshot);

    replica_a.set_scroll_offset(4);
    replica_a.set_selection(Some(TerminalSelectionSnapshot {
        start_row: 0,
        start_column: 0,
        end_row: 0,
        end_column: 6,
    }));
    replica_a.set_search(Some("shared".to_string()), Vec::new());
    replica_a.set_hover(Some(0), Some(1));
    replica_b.set_scroll_offset(1);
    replica_b.set_hover(Some(2), Some(3));

    assert_eq!(replica_a.rows(), replica_b.rows());
    assert_ne!(
        replica_a.viewport().scroll_offset,
        replica_b.viewport().scroll_offset
    );
    assert_ne!(
        replica_a.viewport().selection,
        replica_b.viewport().selection
    );
    assert_ne!(
        replica_a.viewport().search_query,
        replica_b.viewport().search_query
    );
    assert_ne!(
        replica_a.viewport().hover_row,
        replica_b.viewport().hover_row
    );
}

#[test]
fn bounded_scrollback_emits_truncation_marker() {
    let service = TerminalService::new();
    let spec = TerminalSpec {
        session_id: TerminalSessionId::new(),
        size: TerminalSize::new(20, 4).expect("size"),
        max_scrollback_rows: 4,
        max_scrollback_bytes: 128,
        title: None,
    }
    .validated()
    .expect("bounded spec");
    let id = service
        .create_fixture(TaskId::new(), spec)
        .expect("create fixture");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadOnly)
        .expect("grant");

    for index in 0..24 {
        service
            .admit_reader_bytes(id, format!("overflow-{index}\r\n").as_bytes())
            .expect("overflow feed");
    }
    let snapshot = service.snapshot(id).expect("truncated snapshot");
    assert!(
        snapshot.truncated,
        "bounded host scrollback must surface a truncation marker"
    );
    assert!(snapshot.rows.len() <= 8);

    let mut replica = TerminalReplica::new();
    replica.apply_snapshot(service.snapshot(id).expect("fresh"));
    // Rebuild pressure from zero so the host must coalesce rather than replay
    // an unbounded delta log.
    let update = service
        .updates_since(id, client, snapshot.generation, TerminalSequence::ZERO)
        .expect("bounded update");
    match update {
        ReplicaUpdate::CoalescedSnapshot {
            snapshot,
            reason: CoalesceReason::ScrollbackTruncated | CoalesceReason::SlowClient,
        } => {
            assert!(snapshot.truncated);
            replica.apply_snapshot(snapshot);
            assert!(replica.truncated());
        }
        ReplicaUpdate::Deltas(deltas) => {
            assert!(
                deltas
                    .iter()
                    .any(|delta| delta.ops.iter().any(|op| matches!(
                        op,
                        devmanager::terminal::protocol::TerminalDeltaOp::Truncated { .. }
                    ))),
                "contiguous path must still carry an explicit truncation marker"
            );
        }
        other => panic!("unexpected bounded update {other:?}"),
    }
}
