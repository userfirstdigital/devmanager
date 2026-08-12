//! Focused Connect request-lane proofs for HostRequestHandle dispatch.

use devmanager::connect::{
    advertised_connect_capabilities, ChannelBinding, ChannelId, ConnectDispatchSession,
    ConnectEnvelope, ConnectHostRequestSlot, ConnectIdentityLiveState, ConnectLimits,
    ConnectPayload, ConnectPrivacyClass, ConnectSessionDisposition, ConnectionId, HelloPayload,
    SessionId, CONNECT_ERROR_FORBIDDEN, CONNECT_ERROR_PROTOCOL, CONNECT_ERROR_UNAUTHORIZED,
    CONNECT_HOLD_CALLBACK_FRAGMENT,
};
use devmanager::domain::command::{Command, CommandEnvelope};
use devmanager::domain::id::{ClientId, CommandId, OperationId, RequestId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryReply, QueryResult};
use devmanager::domain::snapshot::SnapshotSection;
use devmanager::host::HostRequestExecutor;
use devmanager::kernel::CommandBus;
use devmanager::protocol::Capability;

fn binding() -> ChannelBinding {
    ChannelBinding::new(ConnectionId::new(), SessionId::new(), ChannelId::new())
}

fn envelope(
    binding: ChannelBinding,
    sequence: u64,
    request_id: Option<RequestId>,
    payload: ConnectPayload,
) -> ConnectEnvelope {
    let limits = ConnectLimits::v1_default();
    ConnectEnvelope::new(
        binding,
        payload.channel(),
        sequence,
        request_id,
        None,
        limits,
        ConnectPrivacyClass::LocalOnly,
        payload,
    )
    .expect("envelope")
}

async fn hello(session: &mut ConnectDispatchSession, binding: ChannelBinding) -> ClientId {
    let payload = ConnectPayload::Hello(HelloPayload {
        capabilities: advertised_connect_capabilities(),
        limits: ConnectLimits::v1_default(),
        privacy_class: ConnectPrivacyClass::LocalOnly,
        relay_url: None,
        capability_grant: None,
        client_id: None,
    });
    let env = envelope(binding, 1, None, payload.clone());
    let (reply, disposition) = session.handle_payload(&env, payload, None).await;
    assert_eq!(disposition, ConnectSessionDisposition::Continue);
    let ConnectPayload::Hello(hello) = reply else {
        panic!("expected Hello");
    };
    assert!(!hello.capabilities.contains(Capability::EventReplay));
    let client_id = hello.client_id.expect("Hello reply assigns client_id");
    assert_eq!(session.bound_client_id(), Some(client_id));
    client_id
}

fn refute_hold(payload: &ConnectPayload) {
    if let ConnectPayload::Error(error) = payload {
        assert_ne!(error.code, 503);
        assert!(!error.message.contains(CONNECT_HOLD_CALLBACK_FRAGMENT));
        assert!(!error.message.contains("unavailable until"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn connect_query_and_command_reach_existing_host_request_handle() {
    let directory = tempfile::tempdir().expect("tempdir");
    let bus = CommandBus::open(&directory.path().join("e2e-connect.db")).expect("bus");
    let (handle, executor) = HostRequestExecutor::start(bus);
    let slot = ConnectHostRequestSlot::new();
    slot.attach(handle.clone());
    let binding = binding();
    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    let client_id = hello(&mut session, binding).await;
    let request_id = RequestId::new();
    let query = ConnectPayload::Query(QueryEnvelope {
        request_id,
        client_id,
        task_id: None,
        query: Query::OperationStatus {
            operation_id: OperationId::new(),
        },
    });
    let env = envelope(binding, 2, Some(request_id), query.clone());
    let host = slot.get();
    let (reply, _) = session.handle_payload(&env, query, host.as_deref()).await;
    refute_hold(&reply);
    let ConnectPayload::QueryReply(QueryReply {
        request_id: replied,
        ..
    }) = reply
    else {
        panic!("expected QueryReply {reply:?}");
    };
    assert_eq!(replied, request_id);

    let command_id = CommandId::new();
    let command = ConnectPayload::Command(CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(TaskId::new()),
        issued_at_ms: 1,
        expected_task_revision: None,
        command: Command::BeginCloseTask,
    });
    let env = envelope(binding, 3, Some(RequestId::new()), command.clone());
    let (reply, _) = session.handle_payload(&env, command, host.as_deref()).await;
    refute_hold(&reply);
    let ConnectPayload::CommandReceipt(receipt) = reply else {
        panic!("expected CommandReceipt {reply:?}");
    };
    assert_eq!(receipt.command_id(), command_id);
    drop(handle);
    executor.abort();
    let _ = executor.await;
}

#[tokio::test(flavor = "current_thread")]
async fn connect_fail_closed_cases_do_not_dispatch_or_return_hold() {
    let binding = binding();
    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    let request_id = RequestId::new();
    let query = ConnectPayload::Query(QueryEnvelope {
        request_id,
        client_id: ClientId::new(),
        task_id: None,
        query: Query::TaskSnapshot,
    });
    let env = envelope(binding, 1, Some(request_id), query.clone());
    let (reply, disposition) = session.handle_payload(&env, query, None).await;
    assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
    refute_hold(&reply);
    assert!(matches!(reply, ConnectPayload::Error(error) if error.code == CONNECT_ERROR_PROTOCOL));

    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    let bound = hello(&mut session, binding).await;
    let query = ConnectPayload::Query(QueryEnvelope {
        request_id,
        client_id: bound,
        task_id: None,
        query: Query::OpenEventReplay { after_sequence: 0 },
    });
    let env = envelope(binding, 2, Some(request_id), query.clone());
    let (reply, _) = session.handle_payload(&env, query, None).await;
    refute_hold(&reply);
    assert!(matches!(
        reply,
        ConnectPayload::Error(error)
            if error.code == CONNECT_ERROR_UNAUTHORIZED || error.code == CONNECT_ERROR_FORBIDDEN
    ));
    assert!(session.paired_identity_bound());
    session.disconnect();
    assert!(!session.paired_identity_bound());
}

#[tokio::test(flavor = "current_thread")]
async fn connect_resync_returns_a_fresh_bounded_snapshot_through_the_host_lane() {
    let directory = tempfile::tempdir().expect("tempdir");
    let bus = CommandBus::open(&directory.path().join("e2e-connect-resync.db")).expect("bus");
    let (handle, executor) = HostRequestExecutor::start(bus);
    let binding = binding();
    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    let client_id = hello(&mut session, binding).await;
    let payload = ConnectPayload::Resync(devmanager::connect::ResyncPayload {
        channel_sequence: 1,
        newest_sequence: 3,
        reason: devmanager::connect::ResyncReason::Gap,
    });
    let env = envelope(binding, 2, None, payload.clone());

    let (reply, disposition) = session.handle_payload(&env, payload, Some(&handle)).await;
    assert_eq!(disposition, ConnectSessionDisposition::Continue);
    let ConnectPayload::QueryReply(reply) = reply else {
        panic!("expected a fresh snapshot query reply, got {reply:?}");
    };
    assert!(matches!(
        reply.outcome,
        QueryOutcome::Ok(QueryResult::SnapshotPage { page })
            if page.section == SnapshotSection::Tasks
                && page.items.len() <= ConnectLimits::v1_default().max_page_items as usize
    ));
    assert_eq!(session.bound_client_id(), Some(client_id));

    drop(handle);
    executor.abort();
    let _ = executor.await;
}

#[tokio::test(flavor = "current_thread")]
async fn connect_resync_rejects_an_inverted_cursor_before_host_dispatch() {
    let binding = binding();
    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    hello(&mut session, binding).await;
    let payload = ConnectPayload::Resync(devmanager::connect::ResyncPayload {
        channel_sequence: 4,
        newest_sequence: 3,
        reason: devmanager::connect::ResyncReason::Gap,
    });
    let env = envelope(binding, 2, None, payload.clone());

    let (reply, disposition) = session.handle_payload(&env, payload, None).await;
    assert_eq!(disposition, ConnectSessionDisposition::Continue);
    assert!(matches!(
        reply,
        ConnectPayload::Error(error) if error.code == devmanager::connect::CONNECT_ERROR_CONFLICT
    ));
}

#[test]
fn connect_does_not_advertise_live_resync_writer() {
    assert!(!advertised_connect_capabilities().contains(Capability::EventReplay));
}

#[tokio::test(flavor = "current_thread")]
async fn organization_extension_requires_negotiated_capability_and_request_metadata() {
    let binding = binding();
    let mut session = ConnectDispatchSession::bind_paired(
        "web-paired-org".to_owned(),
        ConnectIdentityLiveState::Live,
    );
    hello(&mut session, binding).await;
    let payload = ConnectPayload::Extension(devmanager::connect::GenericExtensionPayload {
        type_id: devmanager::protocol::organization_extension_type(
            devmanager::protocol::OrganizationExtensionKind::OrganizationPrompt,
        ),
        schema_version: devmanager::protocol::ORGANIZATION_SCHEMA_VERSION,
        payload: serde_json::to_vec(&serde_json::json!({
            "Query": "Snapshot"
        }))
        .expect("organization payload"),
    });
    let env = envelope(binding, 2, Some(RequestId::new()), payload.clone());
    let (reply, disposition) = session.handle_payload(&env, payload, None).await;
    assert_eq!(disposition, ConnectSessionDisposition::Continue);
    assert!(matches!(
        reply,
        ConnectPayload::Error(error)
            if error.code == CONNECT_ERROR_FORBIDDEN
                || error.code == devmanager::connect::CONNECT_ERROR_EXECUTOR_UNATTACHED
    ));
}
