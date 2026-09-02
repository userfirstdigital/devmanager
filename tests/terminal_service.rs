use std::sync::Arc;

use devmanager::domain::id::{AgentSessionId, ClientId, ResourceId, TaskId};
use devmanager::terminal::protocol::{
    ClientInputGrant, CloseReason, InputAck, InputEnvelope, InputId, InputRejectReason,
    ResizeFence, TerminalError, TerminalSessionId, TerminalSize, TerminalSpec, ViewKind,
};
use devmanager::terminal::service::{MockAttachedRuntime, TerminalRuntimeState, TerminalService};

fn create_fixture_terminal(
    service: &TerminalService,
) -> (
    devmanager::domain::id::TerminalId,
    TaskId,
    TerminalSessionId,
) {
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let spec = TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec");
    let id = service.create_fixture(task, spec).expect("create fixture");
    (id, task, session)
}

fn screen_contains(
    service: &TerminalService,
    id: devmanager::domain::id::TerminalId,
    needle: &str,
) -> bool {
    let raw = service.raw_view(id).expect("raw view");
    let raw_text: String = raw
        .lines
        .iter()
        .map(|line| line.iter().map(|cell| cell.character).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let session = service.session_view(id).expect("session view");
    let session_text: String = session
        .screen
        .lines
        .iter()
        .map(|line| line.iter().map(|cell| cell.character).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    raw_text.contains(needle) && session_text.contains(needle)
}

#[test]
fn one_canonical_reader_feeds_both_views() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let id = service
        .attach(
            TaskId::new(),
            TerminalSpec::new(
                TerminalSessionId::new(),
                TerminalSize::new(40, 8).expect("size"),
            )
            .expect("spec"),
            runtime.clone(),
        )
        .expect("attach");
    assert!(service.is_attached(id).expect("attached"));
    assert_eq!(service.canonical_reader_count(id).expect("reader"), 1);

    let raw = service.open_view(id, ViewKind::Raw).expect("open raw");
    let session = service
        .open_view(id, ViewKind::Session)
        .expect("open session");
    assert_eq!(raw.terminal_id, session.terminal_id);
    assert_eq!(raw.generation, session.generation);
    assert_eq!(service.canonical_reader_count(id).expect("reader"), 1);
    assert_eq!(service.view_count(id).expect("views"), 2);

    runtime.inject_reader_output("canonical-one");
    service.pump_attached_output(id).expect("pump one");
    assert!(
        screen_contains(&service, id, "canonical-one"),
        "both projections must see the first reader chunk"
    );

    runtime.inject_reader_output("canonical-two");
    service.pump_attached_output(id).expect("pump two");
    assert!(
        screen_contains(&service, id, "canonical-two"),
        "both projections must see the second reader chunk"
    );
    assert_eq!(
        service.canonical_reader_count(id).expect("reader"),
        1,
        "opening a second view must not create a second reader"
    );

    let raw_after = service.raw_view(id).expect("raw after");
    let session_after = service.session_view(id).expect("session after");
    assert_eq!(raw_after.rows, session_after.screen.rows);
    assert_eq!(raw_after.cols, session_after.screen.cols);
    assert_eq!(raw_after.cursor, session_after.screen.cursor);
}

#[test]
fn disconnect_does_not_close_the_session() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let id = service
        .attach(
            task,
            TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
            runtime.clone(),
        )
        .expect("attach");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    runtime.inject_reader_output("still-live");
    service.pump_attached_output(id).expect("pump");

    service.disconnect_view(id, client).expect("disconnect");
    assert!(service.is_open(id).expect("open after disconnect"));
    assert!(
        screen_contains(&service, id, "still-live"),
        "disconnect must keep the canonical session grid"
    );

    let write = service
        .write(
            id,
            InputEnvelope {
                client_id: ClientId::new(),
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: service.current_generation(id).expect("gen"),
                focus_epoch: service.current_focus(id).expect("focus"),
                bytes: b"x".to_vec(),
            },
        )
        .expect("write after disconnect");
    assert_eq!(
        write,
        InputAck::Rejected {
            reason: InputRejectReason::ReadOnly
        }
    );
    let snapshot = service.snapshot(id).expect("snapshot after disconnect");
    assert_eq!(snapshot.terminal_id, id);
}

#[test]
fn generation_and_session_fence_rejects_stale_operations() {
    let service = TerminalService::new();
    let (id, task, session) = create_fixture_terminal(&service);
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    let generation = service.current_generation(id).expect("generation");
    let focus = service.current_focus(id).expect("focus");

    service
        .set_provider_session_id(id, "provider-conversation-keep")
        .expect("provider identity");
    let replaced = service.replace_generation(id).expect("new pty generation");
    assert_ne!(replaced, generation);
    assert_eq!(
        service.provider_session_id(id).expect("provider"),
        Some("provider-conversation-keep".to_string())
    );

    let stale_write = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: generation,
                focus_epoch: focus,
                bytes: b"stale-gen".to_vec(),
            },
        )
        .expect("stale generation write");
    assert_eq!(
        stale_write,
        InputAck::Rejected {
            reason: InputRejectReason::StaleGeneration
        }
    );
    assert!(service.accepted_input_bytes(id).expect("bytes").is_empty());

    let stale_resize = service.resize(
        id,
        TerminalSize::new(20, 8).expect("size"),
        Some(ResizeFence {
            generation,
            client_id: client,
            view_sequence: 1,
        }),
    );
    assert_eq!(stale_resize, Err(TerminalError::StaleGeneration));

    service
        .retarget_session(id, TerminalSessionId::new())
        .expect("retarget");
    let current_generation = service.current_generation(id).expect("current gen");
    let stale_session = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: current_generation,
                focus_epoch: service.current_focus(id).expect("focus"),
                bytes: b"stale-session".to_vec(),
            },
        )
        .expect("stale session write");
    assert_eq!(
        stale_session,
        InputAck::Rejected {
            reason: InputRejectReason::StaleSession
        }
    );
    assert!(service.accepted_input_bytes(id).expect("bytes").is_empty());
}

#[test]
fn close_semantics_stay_explicit() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let id = service
        .attach(
            task,
            TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
            runtime.clone(),
        )
        .expect("attach");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    service.disconnect_view(id, client).expect("disconnect");
    assert!(service.is_open(id).expect("still open"));

    runtime.set_missing_fence(true);
    assert_eq!(
        service.close(id, CloseReason::ExplicitServiceClose),
        Err(TerminalError::TeardownFenceMissing)
    );
    assert!(service.is_open(id).expect("fail closed"));
    runtime.set_missing_fence(false);

    let report = service
        .close(id, CloseReason::ExplicitServiceClose)
        .expect("explicit close");
    assert!(report.closed);
    assert!(report.explicit);
    assert_eq!(report.reason, CloseReason::ExplicitServiceClose);
    assert!(!service.is_open(id).expect("closed"));

    assert_eq!(service.snapshot(id), Err(TerminalError::Closed));
    let closed_write = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: report.generation,
                focus_epoch: devmanager::terminal::protocol::FocusEpoch::initial(),
                bytes: b"nope".to_vec(),
            },
        )
        .expect("write against closed");
    assert_eq!(
        closed_write,
        InputAck::Rejected {
            reason: InputRejectReason::Closed
        }
    );

    let repeat = service
        .close(id, CloseReason::TaskClose)
        .expect("close is idempotent");
    assert!(repeat.closed);
    assert!(repeat.explicit);
}

#[test]
fn attached_write_resize_forward_and_reject_runtime_failure() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let attached_runtime: Arc<dyn devmanager::terminal::service::AttachedTerminalRuntime> =
        runtime.clone();
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let id = service
        .attach(
            task,
            TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
            attached_runtime,
        )
        .expect("attach");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");

    let ack = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: service.current_generation(id).expect("gen"),
                focus_epoch: service.current_focus(id).expect("focus"),
                bytes: b"exact-bytes\x1b[A".to_vec(),
            },
        )
        .expect("write");
    assert_eq!(ack, InputAck::Accepted { sequence: 1 });
    assert_eq!(runtime.written_bytes(), b"exact-bytes\x1b[A");

    runtime.set_fail_write(true);
    let rejected = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: service.current_generation(id).expect("gen"),
                focus_epoch: service.current_focus(id).expect("focus"),
                bytes: b"should-not-record".to_vec(),
            },
        )
        .expect("failed forward");
    assert_eq!(
        rejected,
        InputAck::Rejected {
            reason: InputRejectReason::RuntimeForwardFailed
        }
    );
    assert_eq!(runtime.written_bytes(), b"exact-bytes\x1b[A");
    assert_eq!(
        service.accepted_input_bytes(id).expect("accepted"),
        b"exact-bytes\x1b[A"
    );

    runtime.set_fail_write(false);
    runtime.set_fail_resize(true);
    assert_eq!(
        service.resize(
            id,
            TerminalSize::new(12, 8).expect("size"),
            Some(ResizeFence {
                generation: service.current_generation(id).expect("gen"),
                client_id: client,
                view_sequence: 1,
            }),
        ),
        Err(TerminalError::RuntimeIo)
    );
    assert_eq!(runtime.current_size().cols, 40);

    runtime.set_fail_resize(false);
    service
        .resize(
            id,
            TerminalSize::new(12, 8).expect("size"),
            Some(ResizeFence {
                generation: service.current_generation(id).expect("gen"),
                client_id: client,
                view_sequence: 2,
            }),
        )
        .expect("resize");
    assert_eq!(runtime.current_size().cols, 12);
}

#[test]
fn attached_restart_rebinds_only_the_published_generation() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let id = service
        .attach(
            task,
            TerminalSpec::new(session, TerminalSize::new(40, 8).expect("size")).expect("spec"),
            runtime.clone(),
        )
        .expect("attach");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    let old_generation = service.current_generation(id).expect("old generation");
    let focus = service.current_focus(id).expect("focus");

    runtime.bump_attachment_generation();
    let stale = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: old_generation,
                focus_epoch: focus,
                bytes: b"old-generation".to_vec(),
            },
        )
        .expect("stale write");
    assert_eq!(
        stale,
        InputAck::Rejected {
            reason: InputRejectReason::StaleGeneration
        }
    );

    let rebound = service.replace_generation(id).expect("rebind restart");
    assert_ne!(rebound, old_generation);
    let accepted = service
        .write(
            id,
            InputEnvelope {
                client_id: client,
                input_id: InputId::new(),
                task_id: task,
                session_id: session,
                terminal_id: id,
                terminal_generation: rebound,
                focus_epoch: focus,
                bytes: b"new-generation".to_vec(),
            },
        )
        .expect("new generation write");
    assert_eq!(accepted, InputAck::Accepted { sequence: 1 });
    assert_eq!(runtime.written_bytes(), b"new-generation");
}

#[test]
fn attached_exit_settlement_survives_retired_runtime_fence() {
    let service = TerminalService::new();
    let runtime = MockAttachedRuntime::new(TerminalSize::new(40, 8).expect("size"));
    let id = service
        .attach(
            TaskId::new(),
            TerminalSpec::new(
                TerminalSessionId::new(),
                TerminalSize::new(40, 8).expect("size"),
            )
            .expect("spec"),
            runtime.clone(),
        )
        .expect("attach");

    runtime.inject_reader_eof();
    runtime.set_missing_fence(true);
    service.pump_attached_output(id).expect("settle EOF");
    assert_eq!(
        service.exit_summary(id).expect("exit summary"),
        Some("PTY reader reached EOF".to_string())
    );
    assert!(
        service.snapshot(id).is_ok(),
        "final screen remains readable"
    );
}

fn shell_runtime(resource_id: ResourceId) -> Arc<MockAttachedRuntime> {
    MockAttachedRuntime::with_resource_fence(TerminalSize::new(80, 24).expect("size"), resource_id)
}

fn shell_spec() -> TerminalSpec {
    TerminalSpec::new(
        TerminalSessionId::new(),
        TerminalSize::new(80, 24).expect("size"),
    )
    .expect("spec")
}

#[test]
fn provider_and_two_shells_coexist_on_one_task() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let provider_runtime = MockAttachedRuntime::new(TerminalSize::new(80, 24).expect("size"));
    let provider_spec = shell_spec();
    let provider_id = service
        .attach_bound_task_runtime(
            task_id,
            provider_spec,
            provider_runtime,
            AgentSessionId::new(),
            1,
            1,
        )
        .expect("provider attach");

    let shell_a = ResourceId::new();
    let shell_b = ResourceId::new();
    let a_id = service
        .attach_plain_shell(task_id, shell_a, 1, shell_spec(), shell_runtime(shell_a))
        .expect("shell a");
    let b_id = service
        .attach_plain_shell(task_id, shell_b, 1, shell_spec(), shell_runtime(shell_b))
        .expect("shell b");
    assert_ne!(a_id, b_id);

    // Default selection still returns the provider terminal.
    let view = service
        .task_terminal_view_for(task_id, None)
        .expect("view")
        .expect("present");
    assert_eq!(view.terminal_id, provider_id);
    assert!(view.is_provider);

    let view_a = service
        .task_terminal_view_for(task_id, Some(shell_a))
        .expect("view")
        .expect("present");
    assert_eq!(view_a.terminal_id, a_id);
    assert!(!view_a.is_provider);
    assert_eq!(view_a.runtime_state, TerminalRuntimeState::Running);
    assert_eq!(view_a.agent_session_id, None);
    assert_eq!(view_a.runtime_generation, None);
    assert_eq!(view_a.action_epoch, None);

    let summaries = service.task_terminal_summaries(task_id).expect("summaries");
    assert_eq!(summaries.len(), 3);
    assert_eq!(summaries.iter().filter(|s| s.is_provider).count(), 1);
    assert!(
        summaries[0].is_provider,
        "the provider terminal sorts before the plain shells"
    );

    service
        .scroll_task_terminal_for(task_id, Some(shell_b), 3)
        .expect("scroll b");
    service
        .resize_task_terminal_for(
            task_id,
            Some(shell_b),
            TerminalSize::new(100, 30).expect("size"),
        )
        .expect("resize b");
    let view_b = service
        .task_terminal_view_for(task_id, Some(shell_b))
        .expect("view")
        .expect("present");
    assert_eq!(view_b.view.screen.cols, 100);
    assert_eq!(view_b.terminal_id, b_id);

    // The provider terminal is untouched by the shell-scoped resize.
    let provider_after = service
        .task_terminal_view_for(task_id, None)
        .expect("view")
        .expect("present");
    assert_eq!(provider_after.view.screen.cols, 80);
}

#[test]
fn second_plain_shell_for_same_resource_is_rejected() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let resource_id = ResourceId::new();
    service
        .attach_plain_shell(
            task_id,
            resource_id,
            1,
            shell_spec(),
            shell_runtime(resource_id),
        )
        .expect("first");
    let second = service.attach_plain_shell(
        task_id,
        resource_id,
        1,
        shell_spec(),
        shell_runtime(resource_id),
    );
    assert!(matches!(second, Err(TerminalError::InvalidFence)));
}

#[test]
fn plain_shell_attach_requires_the_exact_runtime_fence() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let resource_id = ResourceId::new();
    let foreign = service.attach_plain_shell(
        task_id,
        resource_id,
        1,
        shell_spec(),
        shell_runtime(ResourceId::new()),
    );
    assert!(matches!(foreign, Err(TerminalError::InvalidFence)));

    let stale_generation = service.attach_plain_shell(
        task_id,
        resource_id,
        2,
        shell_spec(),
        shell_runtime(resource_id),
    );
    assert!(matches!(stale_generation, Err(TerminalError::InvalidFence)));
}

#[test]
fn plain_shells_do_not_block_the_provider_terminal() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let shell = ResourceId::new();
    service
        .attach_plain_shell(task_id, shell, 1, shell_spec(), shell_runtime(shell))
        .expect("shell");
    service
        .attach_bound_task_runtime(
            task_id,
            shell_spec(),
            MockAttachedRuntime::new(TerminalSize::new(80, 24).expect("size")),
            AgentSessionId::new(),
            1,
            1,
        )
        .expect("provider attaches beside a plain shell");

    // The legacy provider-only accessors keep resolving the provider terminal.
    let view = service
        .task_terminal_view(task_id)
        .expect("view")
        .expect("present");
    assert!(view.is_provider);
    service.scroll_task_terminal(task_id, 1).expect("scroll");
    service
        .resize_task_terminal(task_id, TerminalSize::new(90, 20).expect("size"))
        .expect("resize");
}

#[test]
fn remove_closed_retires_only_a_closed_hosted_shell() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let shell = ResourceId::new();
    let terminal_id = service
        .attach_plain_shell(task_id, shell, 1, shell_spec(), shell_runtime(shell))
        .expect("shell");

    // A live entry is never retired: the answer distinguishes "refused" from
    // "nothing to do" by leaving the terminal queryable.
    assert!(!service.remove_closed(shell).expect("live entry"));
    assert!(service
        .task_terminal_view_for(task_id, Some(shell))
        .expect("view")
        .is_some());
    assert_eq!(
        service
            .task_terminal_summaries(task_id)
            .expect("summaries")
            .len(),
        1
    );

    service
        .close(terminal_id, CloseReason::ExplicitServiceClose)
        .expect("close");
    assert!(service.remove_closed(shell).expect("closed entry"));
    assert!(service
        .task_terminal_summaries(task_id)
        .expect("summaries")
        .is_empty());
    assert!(service
        .task_terminal_view_for(task_id, Some(shell))
        .expect("view")
        .is_none());
    // Retiring twice is a no-op rather than an error.
    assert!(!service.remove_closed(shell).expect("already retired"));
    assert!(matches!(
        service.snapshot(terminal_id),
        Err(TerminalError::NotFound)
    ));
}
