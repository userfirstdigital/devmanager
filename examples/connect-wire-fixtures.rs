//! Emit native-serialized payloads for the actual-WASM browser contract gate.
//! Usage: cargo run --locked --example connect-wire-fixtures
use base64::Engine;
use devmanager::connect::{
    native_browser_contract_fixtures, CanonicalSchemaFixture, ConnectLimits, ConnectPayload,
};
use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt};
use devmanager::domain::id::{ClientId, CommandId, EventId, OperationId, RequestId, TaskId};
use devmanager::domain::query::{Query, QueryEnvelope, QueryOutcome, QueryReply, QueryResult};

fn fixture_uuid(tail: u8) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = tail;
    bytes
}

fn receipt_status_fixtures() -> [CanonicalSchemaFixture; 2] {
    let request_id = RequestId::from_bytes(fixture_uuid(0x61)).expect("fixture request id");
    let client_id = ClientId::from_bytes(fixture_uuid(0x62)).expect("fixture client id");
    let task_id = TaskId::from_bytes(fixture_uuid(0x63)).expect("fixture task id");
    let command_id = CommandId::from_bytes(fixture_uuid(0x64)).expect("fixture command id");
    let operation_id = OperationId::from_bytes(fixture_uuid(0x65)).expect("fixture operation id");
    let event_id = EventId::from_bytes(fixture_uuid(0x66)).expect("fixture event id");
    let command = CommandEnvelope {
        command_id,
        client_id,
        task_id: Some(task_id),
        issued_at_ms: 1_725_000_000_777,
        expected_task_revision: Some(3),
        command: Command::BeginCloseTask,
    };
    [
        CanonicalSchemaFixture {
            name: "command_receipt_status_query",
            payload: ConnectPayload::Query(QueryEnvelope {
                request_id,
                client_id,
                task_id: Some(task_id),
                query: Query::CommandReceiptStatus {
                    command: command.clone(),
                },
            }),
        },
        CanonicalSchemaFixture {
            name: "command_receipt_status_result",
            payload: ConnectPayload::QueryReply(QueryReply {
                request_id,
                outcome: QueryOutcome::Ok(QueryResult::CommandReceiptStatus {
                    receipt: Some(CommandReceipt::Accepted {
                        command_id,
                        operation_id,
                        task_revision: Some(4),
                        event_ids: vec![event_id],
                        prompt_mutation: None,
                    }),
                }),
            }),
        },
    ]
}

fn main() {
    let mut source = native_browser_contract_fixtures()
        .into_iter()
        .filter(|fixture| matches!(fixture.payload.kind().get(), 1 | 18 | 19 | 20 | 21 | 22))
        .collect::<Vec<_>>();
    source.extend(receipt_status_fixtures());
    let fixtures = source
        .into_iter()
        .map(|fixture| {
            let bytes = fixture
                .payload
                .encode(ConnectLimits::v1_default())
                .expect("native payload");
            serde_json::json!({
                "name": fixture.name,
                "payloadKind": fixture.payload.kind().get(),
                "channel": fixture.payload.channel(),
                "payloadBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&fixtures).expect("fixture JSON")
    );
}
