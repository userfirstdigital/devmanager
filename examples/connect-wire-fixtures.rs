//! Emit native-serialized payloads for the actual-WASM browser contract gate.
//! Usage: cargo run --locked --example connect-wire-fixtures
#[path = "../tests/support/connect_wire_fixtures.rs"]
mod connect_wire_fixtures;

use base64::Engine;
use devmanager::connect::ConnectLimits;

fn main() {
    let fixtures = connect_wire_fixtures::native_browser_wire_fixtures()
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
