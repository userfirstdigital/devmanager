use std::sync::Arc;

use devmanager::domain::id::{ClientId, TaskId};
use devmanager::terminal::protocol::{
    ClientInputGrant, FocusEpoch, InputAck, InputEnvelope, InputId, InputRejectReason,
    TerminalGeneration, TerminalSessionId, TerminalSize, TerminalSpec,
};
use devmanager::terminal::service::{
    AttachedTerminalRuntime, MockAttachedRuntime, TerminalService,
};

struct Fixture {
    service: TerminalService,
    runtime: Arc<MockAttachedRuntime>,
    id: devmanager::domain::id::TerminalId,
    task: TaskId,
    session: TerminalSessionId,
    client: ClientId,
    generation: TerminalGeneration,
    focus: FocusEpoch,
}

fn fixture() -> Fixture {
    let service = TerminalService::new();
    let task = TaskId::new();
    let session = TerminalSessionId::new();
    let size = TerminalSize::new(40, 8).expect("size");
    let runtime = MockAttachedRuntime::new(size);
    let spec = TerminalSpec::new(session, size).expect("spec");
    let attached: Arc<dyn AttachedTerminalRuntime> = runtime.clone();
    let id = service.attach(task, spec, attached).expect("attach");
    let client = ClientId::new();
    service
        .grant_client(id, client, ClientInputGrant::ReadWrite)
        .expect("grant");
    let generation = service.current_generation(id).expect("generation");
    let focus = service.current_focus(id).expect("focus");
    Fixture {
        service,
        runtime,
        id,
        task,
        session,
        client,
        generation,
        focus,
    }
}

impl Fixture {
    fn envelope(&self, input_id: InputId, bytes: &[u8]) -> InputEnvelope {
        InputEnvelope {
            client_id: self.client,
            input_id,
            task_id: self.task,
            session_id: self.session,
            terminal_id: self.id,
            terminal_generation: self.generation,
            focus_epoch: self.focus,
            bytes: bytes.to_vec(),
        }
    }
}

#[test]
fn accepted_input_sequences_are_monotonic() {
    let fixture = fixture();
    let first = fixture
        .service
        .write(fixture.id, fixture.envelope(InputId::new(), b"one"))
        .expect("first");
    let second = fixture
        .service
        .write(fixture.id, fixture.envelope(InputId::new(), b"two"))
        .expect("second");
    let third = fixture
        .service
        .write(fixture.id, fixture.envelope(InputId::new(), b"three"))
        .expect("third");
    assert_eq!(first, InputAck::Accepted { sequence: 1 });
    assert_eq!(second, InputAck::Accepted { sequence: 2 });
    assert_eq!(third, InputAck::Accepted { sequence: 3 });
    assert_eq!(
        fixture
            .service
            .accepted_input_bytes(fixture.id)
            .expect("bytes"),
        b"onetwothree"
    );
}

#[test]
fn duplicate_input_id_is_idempotent() {
    let fixture = fixture();
    let input_id = InputId::new();
    let envelope = fixture.envelope(input_id, b"retry-me");
    let first = fixture
        .service
        .write(fixture.id, envelope.clone())
        .expect("first accept");
    let second = fixture
        .service
        .write(fixture.id, envelope)
        .expect("duplicate");
    assert_eq!(first, InputAck::Accepted { sequence: 1 });
    assert_eq!(second, InputAck::Duplicate { sequence: 1 });
    assert_eq!(
        fixture
            .service
            .accepted_input_bytes(fixture.id)
            .expect("bytes"),
        b"retry-me"
    );
}

#[test]
fn stale_task_session_generation_focus_is_rejected_and_writes_no_bytes() {
    let fixture = fixture();
    let authorized = fixture
        .service
        .write(fixture.id, fixture.envelope(InputId::new(), b"keep"))
        .expect("seed");
    assert_eq!(authorized, InputAck::Accepted { sequence: 1 });
    let before = fixture
        .service
        .accepted_input_bytes(fixture.id)
        .expect("before");

    let stale_task = fixture
        .service
        .write(
            fixture.id,
            InputEnvelope {
                client_id: fixture.client,
                input_id: InputId::new(),
                task_id: TaskId::new(),
                session_id: fixture.session,
                terminal_id: fixture.id,
                terminal_generation: fixture.generation,
                focus_epoch: fixture.focus,
                bytes: b"task".to_vec(),
            },
        )
        .expect("stale task");
    assert_eq!(
        stale_task,
        InputAck::Rejected {
            reason: InputRejectReason::StaleTask
        }
    );

    fixture
        .service
        .retarget_session(fixture.id, TerminalSessionId::new())
        .expect("retarget session");
    let stale_session = fixture
        .service
        .write(
            fixture.id,
            InputEnvelope {
                client_id: fixture.client,
                input_id: InputId::new(),
                task_id: fixture.task,
                session_id: fixture.session,
                terminal_id: fixture.id,
                terminal_generation: fixture.generation,
                focus_epoch: fixture.focus,
                bytes: b"session".to_vec(),
            },
        )
        .expect("stale session");
    assert_eq!(
        stale_session,
        InputAck::Rejected {
            reason: InputRejectReason::StaleSession
        }
    );
    fixture
        .service
        .retarget_session(fixture.id, fixture.session)
        .expect("restore session");

    let stale_generation = fixture
        .service
        .write(
            fixture.id,
            InputEnvelope {
                client_id: fixture.client,
                input_id: InputId::new(),
                task_id: fixture.task,
                session_id: fixture.session,
                terminal_id: fixture.id,
                terminal_generation: fixture.generation.next().expect("future gen"),
                focus_epoch: fixture.focus,
                bytes: b"generation".to_vec(),
            },
        )
        .expect("stale generation");
    assert_eq!(
        stale_generation,
        InputAck::Rejected {
            reason: InputRejectReason::StaleGeneration
        }
    );

    fixture.service.advance_focus(fixture.id).expect("focus");
    let stale_focus = fixture
        .service
        .write(
            fixture.id,
            InputEnvelope {
                client_id: fixture.client,
                input_id: InputId::new(),
                task_id: fixture.task,
                session_id: fixture.session,
                terminal_id: fixture.id,
                terminal_generation: fixture.generation,
                focus_epoch: fixture.focus,
                bytes: b"focus".to_vec(),
            },
        )
        .expect("stale focus");
    assert_eq!(
        stale_focus,
        InputAck::Rejected {
            reason: InputRejectReason::StaleFocus
        }
    );
    assert_eq!(
        fixture
            .service
            .accepted_input_bytes(fixture.id)
            .expect("after"),
        before
    );
}

#[test]
fn authorized_input_preserves_bytes() {
    let fixture = fixture();
    let keyboard = b"\x1b[A";
    let paste = b"\x1b[200~pasted-bytes\x1b[201~";
    let mouse = b"\x1b[<0;12;8M";
    let ime = "合成".as_bytes();
    for chunk in [keyboard.as_slice(), paste.as_slice(), mouse.as_slice(), ime] {
        let ack = fixture
            .service
            .write(fixture.id, fixture.envelope(InputId::new(), chunk))
            .expect("authorized write");
        assert!(matches!(ack, InputAck::Accepted { .. }));
    }
    let accepted = fixture
        .service
        .accepted_input_bytes(fixture.id)
        .expect("accepted");
    let mut expected = Vec::new();
    expected.extend_from_slice(keyboard);
    expected.extend_from_slice(paste);
    expected.extend_from_slice(mouse);
    expected.extend_from_slice(ime);
    assert_eq!(accepted, expected);
    assert_eq!(fixture.runtime.written_bytes(), expected);
}

#[test]
fn read_only_input_is_rejected() {
    let fixture = fixture();
    let watcher = ClientId::new();
    fixture
        .service
        .grant_client(fixture.id, watcher, ClientInputGrant::ReadOnly)
        .expect("read-only grant");
    let ack = fixture
        .service
        .write(
            fixture.id,
            InputEnvelope {
                client_id: watcher,
                input_id: InputId::new(),
                task_id: fixture.task,
                session_id: fixture.session,
                terminal_id: fixture.id,
                terminal_generation: fixture.generation,
                focus_epoch: fixture.focus,
                bytes: b"watcher".to_vec(),
            },
        )
        .expect("read-only write");
    assert_eq!(
        ack,
        InputAck::Rejected {
            reason: InputRejectReason::ReadOnly
        }
    );
    assert!(fixture
        .service
        .accepted_input_bytes(fixture.id)
        .expect("bytes")
        .is_empty());
    assert!(fixture.runtime.written_bytes().is_empty());
}
