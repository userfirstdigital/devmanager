//! Unattended TLS cross-origin admission smoke on an isolated host fixture.
//!
//! Build (root): `cargo build --bin devmanager-host --example remote-cross-origin-smoke`
//! Run after source integration. Proves real rustls HTTPS/WSS + Noise on loopback
//! with an ephemeral test CA trusted only by this process — never OS-installed,
//! never verification-bypassed. No LLM / provider calls.
#[path = "support/remote_smoke.rs"]
mod remote_smoke;

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use zeroize::Zeroizing;

use devmanager::client::{
    fetch_published_host_identity, hex_encode, pair_enroll_and_connect, parse_host_public_id,
    validate_remote_endpoint, ClientSubscription, ConnectClientConfig, HostClient,
    PairEnrollRequest, RemoteTrustStore,
};
use devmanager::connect::{ConnectNoiseCustody, ConnectNoiseStaticPublicKey};
use devmanager::domain::{ClientId, TaskId};
use devmanager::host::remote_setup::RemoteSetupReply;
use remote_smoke::{
    generate_ephemeral_loopback_tls, https_json_post_with_origin_until, open_wss_with_origin_until,
    tls_options_trusting_ca, IsolatedHostFixture,
};

const PHONE_ORIGIN: &str = "https://phone-owner.example";
const WRONG_ORIGIN: &str = "https://wrong-origin.example";
const CROSS_ORIGIN_PATH: &str = "/api/connect/cross-origin";
const ABSOLUTE_DEADLINE: Duration = Duration::from_secs(120);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    remote_smoke::require_windows_debug()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(ABSOLUTE_DEADLINE, run_cross_origin_smoke())
            .await
            .map_err(|_| Box::<dyn std::error::Error>::from("absolute smoke deadline"))?
    });
    match &result {
        Ok(()) => println!("RESULT: PASS"),
        Err(error) => println!("RESULT: FAIL ({error})"),
    }
    result
}

fn step(ok: bool, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    if ok {
        println!("PASS: {label}");
        Ok(())
    } else {
        println!("FAIL: {label}");
        Err(format!("step failed: {label}").into())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantJson {
    grant: String,
    origin: String,
    #[allow(dead_code)]
    expires_at_epoch_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairJson {
    attach_ticket: String,
    #[allow(dead_code)]
    expires_at_epoch_ms: u64,
    host_public_id: String,
    client_id: String,
}

async fn run_cross_origin_smoke() -> Result<(), Box<dyn std::error::Error>> {
    let deadline_at = Instant::now() + ABSOLUTE_DEADLINE;
    let mut fixture = IsolatedHostFixture::spawn(
        "devmanager-cross-origin-",
        "Cross Origin Smoke",
        "remote-cross-origin-smoke/1",
    )?;
    let host_pid = fixture.owner_pid();
    println!("PASS: spawned isolated host pid={host_pid}");

    let tls_dir = fixture.fixture_root().join("tls");
    let tls_files = generate_ephemeral_loopback_tls(&tls_dir)?;
    let tls = tls_options_trusting_ca(&tls_files.ca_pem);
    step(true, "ephemeral CA + 127.0.0.1 leaf under fixture temp")?;

    let mut local = fixture.connect_local().await?;
    let owner_task = TaskId::new();
    let (_project_id, _second) = fixture
        .create_project_and_tasks(
            &mut local,
            false,
            false,
            owner_task,
            "Cross-origin owner · first task",
            "Cross-origin owner · second task",
        )
        .await?;
    step(true, "deferred no-provider tasks created")?;

    let (port, host_origin) = fixture
        .enable_remote_listening_tls(&mut local, &tls_files, None)
        .await?;
    step(
        true,
        &format!("HTTPS listener enabled port={port} origin_authority=127.0.0.1"),
    )?;

    let pairing = fixture.pairing_info(&mut local).await?;
    let code = match pairing {
        RemoteSetupReply::PairingInfo { code, .. } => code,
        other => return Err(format!("expected pairing info, got {other:?}").into()),
    };
    remaining(deadline_at)?;

    let https_base = format!("https://127.0.0.1:{port}");
    let endpoint =
        validate_remote_endpoint(&https_base).map_err(|error| format!("endpoint: {error:?}"))?;
    let published = fetch_published_host_identity(&endpoint, &tls, remaining(deadline_at)?)
        .await
        .map_err(|error| format!("host marker: {error:?}"))?;
    let host_uuid = uuid::Uuid::from_bytes(published.host_public_id);
    step(
        true,
        &format!("published host marker hostPublicId={host_uuid}"),
    )?;

    let trust_root = fixture.fixture_root().join("owner-trust");
    let store = RemoteTrustStore::open(trust_root)?;
    let (mut owner_client, owner_record) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: https_base.clone(),
            pairing_code: Zeroizing::new(code),
            label: Some("Cross-origin owner".into()),
            additional_ca_pem: Some(tls_files.ca_pem.clone()),
            ..PairEnrollRequest::default()
        },
    )
    .await
    .map_err(|error| format!("owner pair: {error:?}"))?;
    if owner_record.host_public_id != published.host_public_id {
        return Err("owner pair host id mismatch vs published marker".into());
    }
    if owner_record.host_key_pin.as_bytes() != published.host_public_key {
        return Err("owner pair host pin mismatch vs published marker".into());
    }
    let (_record, owner_cookie) = store
        .load_trusted_host(published.host_public_id)
        .map_err(|error| format!("load owner cookie: {error:?}"))?;
    drop(owner_client);
    step(
        true,
        &format!(
            "owner paired over TLS assignedClientId={}",
            owner_record.assigned_client_id
        ),
    )?;
    remaining(deadline_at)?;

    let grant_body = serde_json::json!({ "origin": PHONE_ORIGIN });
    let grant_bytes = serde_json::to_vec(&grant_body)?;
    let grant_response = https_json_post_with_origin_until(
        &endpoint,
        "/api/connect/cross-origin-grants",
        &host_origin,
        Some(owner_cookie.as_str()),
        &grant_bytes,
        &tls,
        deadline_at,
    )
    .await?;
    if !grant_response.status.is_success() {
        return Err(format!("grant HTTP {}", grant_response.status.as_u16()).into());
    }
    let grant_json: GrantJson = serde_json::from_slice(&grant_response.body)?;
    if grant_json.origin != PHONE_ORIGIN {
        return Err("grant origin mismatch".into());
    }
    let grant = Zeroizing::new(grant_json.grant);
    step(true, "owner minted cross-origin grant for phone origin")?;
    remaining(deadline_at)?;

    let phone_custody =
        ConnectNoiseCustody::generate().map_err(|error| format!("phone custody: {error:?}"))?;
    let phone_public = phone_custody.public().as_bytes();
    let phone_public_hex = hex_encode(&phone_public);
    let browser_install_id = format!("phone-{}", uuid::Uuid::new_v4().simple());
    let pair_body = serde_json::json!({
        "grant": grant.as_str(),
        "browserInstallId": browser_install_id,
        "label": "Phone owner smoke",
        "publicKey": phone_public_hex,
    });
    drop(grant);
    let pair_bytes = serde_json::to_vec(&pair_body)?;
    let pair_response = https_json_post_with_origin_until(
        &endpoint,
        "/api/connect/cross-origin-pair",
        PHONE_ORIGIN,
        None,
        &pair_bytes,
        &tls,
        deadline_at,
    )
    .await?;
    if !pair_response.status.is_success() {
        return Err(format!("pair HTTP {}", pair_response.status.as_u16()).into());
    }
    let pair_json: PairJson = serde_json::from_slice(&pair_response.body)?;
    let attach_ticket = Zeroizing::new(pair_json.attach_ticket);
    let paired_host_id = parse_host_public_id(&pair_json.host_public_id)
        .map_err(|error| format!("pair hostPublicId: {error:?}"))?;
    if paired_host_id != published.host_public_id {
        return Err("pair hostPublicId mismatch vs verified marker".into());
    }
    let paired_client_id = ClientId::parse(&pair_json.client_id)?;
    step(
        true,
        &format!(
            "cross-origin pair assignedClientId={paired_client_id} hostPublicId={}",
            uuid::Uuid::from_bytes(paired_host_id)
        ),
    )?;
    remaining(deadline_at)?;

    let wss_endpoint = endpoint
        .with_connect_path(CROSS_ORIGIN_PATH)
        .map_err(|error| format!("wss path: {error:?}"))?;
    let host_pin = ConnectNoiseStaticPublicKey::from_bytes(published.host_public_key)
        .map_err(|error| format!("host pin: {error:?}"))?;

    let mut phone = connect_cross_origin_ticket(
        &wss_endpoint,
        PHONE_ORIGIN,
        attach_ticket.as_str(),
        &phone_custody,
        published.host_public_id,
        host_pin,
        None,
        &tls,
        deadline_at,
    )
    .await?;
    if phone.client_id() != paired_client_id {
        return Err("ticket connect assigned ClientId mismatch".into());
    }
    let mut subscription = ClientSubscription::new();
    subscription.synchronize(&mut phone).await?;
    let model = subscription.model().ok_or("canonical model missing")?;
    let has_owner_task = model.task(owner_task).is_some();
    subscription.release(&mut phone).await?;
    step(
        has_owner_task,
        "ticket WSS Noise/Hello synchronized owner task",
    )?;
    let first_client_id = phone.client_id();
    drop(phone);
    remaining(deadline_at)?;

    let mut phone_resumed = connect_cross_origin_resume(
        &wss_endpoint,
        PHONE_ORIGIN,
        &phone_custody,
        published.host_public_id,
        host_pin,
        Some(first_client_id),
        &tls,
        deadline_at,
    )
    .await?;
    if phone_resumed.client_id() != first_client_id {
        return Err("resume changed assigned ClientId".into());
    }
    let mut subscription = ClientSubscription::new();
    subscription.synchronize(&mut phone_resumed).await?;
    let model = subscription.model().ok_or("resume model missing")?;
    let has_owner_task = model.task(owner_task).is_some();
    subscription.release(&mut phone_resumed).await?;
    step(
        has_owner_task,
        &format!("resume WSS preserved ClientId={first_client_id}"),
    )?;
    drop(phone_resumed);
    remaining(deadline_at)?;

    let reused = connect_cross_origin_ticket(
        &wss_endpoint,
        PHONE_ORIGIN,
        attach_ticket.as_str(),
        &phone_custody,
        published.host_public_id,
        host_pin,
        None,
        &tls,
        deadline_at,
    )
    .await;
    step(
        reused.is_err(),
        "reused attach ticket rejected under absolute deadline",
    )?;
    drop(attach_ticket);

    let wrong_origin = connect_cross_origin_resume(
        &wss_endpoint,
        WRONG_ORIGIN,
        &phone_custody,
        published.host_public_id,
        host_pin,
        Some(first_client_id),
        &tls,
        deadline_at,
    )
    .await;
    step(
        wrong_origin.is_err(),
        "wrong Origin resume cannot acquire channel",
    )?;

    // RemoteSetupRequest exposes Snapshot/Enable/Disable/Retry/PairingInfo only —
    // no paired-client revoke. Report residual; do not invent an API.
    println!(
        "RESIDUAL: RemoteSetupRequest has no paired-client revoke; stream-close/resume-after-revoke not asserted"
    );

    fixture.disable_remote_and_quit(&mut local).await?;
    drop(local);
    step(true, "fixture remote disabled and host quit")?;
    Ok(())
}

fn remaining(deadline_at: Instant) -> Result<Duration, Box<dyn std::error::Error>> {
    let left = deadline_at.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err("absolute smoke deadline".into())
    } else {
        Ok(left)
    }
}

async fn send_dmcx1_prelude(
    socket: &mut tokio_tungstenite::WebSocketStream<devmanager::client::RemoteIo>,
    json: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    socket
        .send(WsMessage::Binary(b"DMCX1".to_vec()))
        .await
        .map_err(|error| format!("DMCX1 send: {error}"))?;
    let bytes = serde_json::to_vec(&json)?;
    socket
        .send(WsMessage::Binary(bytes))
        .await
        .map_err(|error| format!("prelude json send: {error}"))?;
    Ok(())
}

async fn connect_cross_origin_ticket(
    endpoint: &devmanager::client::RemoteEndpoint,
    origin: &str,
    ticket: &str,
    custody: &ConnectNoiseCustody,
    host_public_id: [u8; 16],
    host_pin: ConnectNoiseStaticPublicKey,
    requested_client_id: Option<ClientId>,
    tls: &devmanager::client::RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<HostClient, Box<dyn std::error::Error>> {
    remaining(deadline_at)?;
    let mut socket = open_wss_with_origin_until(endpoint, origin, tls, deadline_at).await?;
    send_dmcx1_prelude(
        &mut socket,
        serde_json::json!({ "type": "ticket", "ticket": ticket }),
    )
    .await?;
    let (sink, stream) = socket.split();
    let mut config = ConnectClientConfig::for_browser_fleet(host_public_id, host_pin, None);
    config.requested_client_id = requested_client_id;
    let left = remaining(deadline_at)?;
    tokio::time::timeout(
        left,
        HostClient::connect_connect(config, custody, sink, stream),
    )
    .await
    .map_err(|_| Box::<dyn std::error::Error>::from("ticket connect deadline"))?
    .map_err(|error| format!("ticket Noise/Hello: {error:?}").into())
}

async fn connect_cross_origin_resume(
    endpoint: &devmanager::client::RemoteEndpoint,
    origin: &str,
    custody: &ConnectNoiseCustody,
    host_public_id: [u8; 16],
    host_pin: ConnectNoiseStaticPublicKey,
    requested_client_id: Option<ClientId>,
    tls: &devmanager::client::RemoteTlsOptions,
    deadline_at: Instant,
) -> Result<HostClient, Box<dyn std::error::Error>> {
    remaining(deadline_at)?;
    let mut socket = open_wss_with_origin_until(endpoint, origin, tls, deadline_at).await?;
    send_dmcx1_prelude(&mut socket, serde_json::json!({ "type": "resume" })).await?;
    let (sink, stream) = socket.split();
    let mut config = ConnectClientConfig::for_browser_fleet(host_public_id, host_pin, None);
    config.requested_client_id = requested_client_id;
    let left = remaining(deadline_at)?;
    tokio::time::timeout(
        left,
        HostClient::connect_connect(config, custody, sink, stream),
    )
    .await
    .map_err(|_| Box::<dyn std::error::Error>::from("resume connect deadline"))?
    .map_err(|error| format!("resume Noise/Hello: {error:?}").into())
}
