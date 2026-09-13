//! Two-host transport fleet acceptance. Unattended by default.
//!
//! Build: `cargo build --bin devmanager-host --example remote-fleet-smoke`
//! (requires HostFleet + remote-trust APIs integrated by root).
//!
//! Spawns two real `devmanager-host` binaries under distinct TempDirs/profiles
//! that deliberately share the same first raw TaskId. Native clients pair via
//! production RemoteTrustStore, then exercise HostFleet install/sync/command/
//! reconnect/remove. Proves transport fleet ownership only — not native UI or
//! physical LAN.
#[path = "support/remote_smoke.rs"]
mod remote_smoke;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use devmanager::client::action::{task_rename_command, TaskRenameArguments};
use devmanager::client::{
    connect_trusted_host, forget_trusted_host, list_trusted_hosts, pair_enroll_and_connect,
    ConnectTrustedOptions, FleetError, FleetRetainedCommand, HostFleet, HostId, HostTaskKey,
    PairEnrollRequest, RemoteTrustError, RemoteTrustStore,
};
use devmanager::domain::{CommandId, RequestId, TaskId};
use devmanager::host::IpcError;
use remote_smoke::IsolatedHostFixture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    remote_smoke::require_windows_debug()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_fleet_acceptance());
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

async fn run_fleet_acceptance() -> Result<(), Box<dyn std::error::Error>> {
    let shared_task = TaskId::new();
    let mut host_a = IsolatedHostFixture::spawn(
        "devmanager-fleet-a-",
        "Fleet Smoke A",
        "remote-fleet-smoke/a",
    )?;
    let mut host_b = IsolatedHostFixture::spawn(
        "devmanager-fleet-b-",
        "Fleet Smoke B",
        "remote-fleet-smoke/b",
    )?;
    println!(
        "Spawned hosts A pid={} B pid={} shared_task={}",
        host_a.owner_pid(),
        host_b.owner_pid(),
        shared_task
    );

    let mut local_a = host_a.connect_local().await?;
    host_a
        .create_project_and_tasks(
            &mut local_a,
            false,
            false,
            shared_task,
            "Fleet A · shared task",
            "Fleet A · second task",
        )
        .await?;
    let port_a = host_a.enable_remote_listening(&mut local_a).await?;
    let pairing_a = host_a.pairing_info(&mut local_a).await?;
    let code_a = match &pairing_a {
        devmanager::host::remote_setup::RemoteSetupReply::PairingInfo { code, .. } => code.clone(),
        other => return Err(format!("expected pairing info A, got {other:?}").into()),
    };
    step(true, "host A project/tasks/remote listening")?;

    let mut local_b = host_b.connect_local().await?;
    host_b
        .create_project_and_tasks(
            &mut local_b,
            false,
            false,
            shared_task,
            "Fleet B · shared task",
            "Fleet B · second task",
        )
        .await?;
    let port_b = host_b.enable_remote_listening(&mut local_b).await?;
    let pairing_b = host_b.pairing_info(&mut local_b).await?;
    let code_b = match &pairing_b {
        devmanager::host::remote_setup::RemoteSetupReply::PairingInfo { code, .. } => code.clone(),
        other => return Err(format!("expected pairing info B, got {other:?}").into()),
    };
    step(true, "host B project/tasks/remote listening")?;

    // Pairing codes stay in process memory only (not printed) for this example.
    let trust_root = host_a.fixture_root().join("fleet-native-trust");
    let store = RemoteTrustStore::open(trust_root.clone())?;
    let (client_a, record_a) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port_a}"),
            pairing_code: zeroize::Zeroizing::new(code_a),
            label: Some("Fleet native A".into()),
            ..PairEnrollRequest::default()
        },
    )
    .await?;
    let (client_b, record_b) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port_b}"),
            pairing_code: zeroize::Zeroizing::new(code_b),
            label: Some("Fleet native B".into()),
            ..PairEnrollRequest::default()
        },
    )
    .await?;
    step(
        record_a.host_public_id != record_b.host_public_id,
        "paired distinct authenticated host IDs",
    )?;
    let trusted = list_trusted_hosts(&store, Duration::from_secs(10)).await?;
    step(
        trusted.len() == 2 && trusted.contains(&record_a) && trusted.contains(&record_b),
        "Settings roster reloads both exact authenticated trust records",
    )?;

    let fleet = HostFleet::new();
    let host_id_a = HostId::remote(record_a.host_public_id)?;
    let host_id_b = HostId::remote(record_b.host_public_id)?;
    fleet.install(host_id_a.clone(), client_a)?;
    fleet.install(host_id_b.clone(), client_b)?;
    fleet.synchronize(&host_id_a).await?;
    fleet.synchronize(&host_id_b).await?;
    step(true, "fleet install + synchronize A and B")?;

    let merged = fleet.merged_task_keys()?;
    let shared_keys: Vec<_> = merged
        .iter()
        .filter(|key| key.task_id == shared_task)
        .cloned()
        .collect();
    step(
        shared_keys.len() == 2
            && shared_keys.iter().any(|key| key.host == host_id_a)
            && shared_keys.iter().any(|key| key.host == host_id_b),
        "merged_task_keys has exactly two HostTaskKeys for shared raw TaskId",
    )?;

    let title_a = task_title(&fleet, &host_id_a, shared_task)?;
    let title_b = task_title(&fleet, &host_id_b, shared_task)?;
    step(
        title_a == "Fleet A · shared task" && title_b == "Fleet B · shared task",
        "presentation titles correct per host for shared TaskId",
    )?;

    let admission_a = fleet.admit_action(HostTaskKey::new(host_id_a.clone(), shared_task))?;
    let revision_a = task_revision(&fleet, &host_id_a, shared_task)?;
    // Capture A, then use B before executing the captured action. There is no
    // mutable active-host fallback that can retarget this command to B.
    let admission_b = fleet.admit_read(HostTaskKey::new(host_id_b.clone(), shared_task))?;
    let b_read = fleet
        .query(
            &admission_b,
            devmanager::client::action::task_show_query(
                RequestId::new(),
                admission_b.client_id,
                shared_task,
            ),
        )
        .await?;
    step(b_read.host == host_id_b, "B read carries B owner")?;
    let rename = task_rename_command(
        CommandId::new(),
        admission_a.client_id,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64,
        revision_a,
        TaskRenameArguments {
            task_id: shared_task,
            title: "Fleet A · renamed".into(),
        },
    )?;
    let owned = fleet.execute_command(&admission_a, rename).await?;
    step(
        owned.host == host_id_a
            && owned.generation == admission_a.generation
            && matches!(
                owned.value,
                devmanager::domain::CommandReceipt::Accepted { .. }
            ),
        "captured A command accepted on A after using B",
    )?;
    fleet.acknowledge_retained_owned(
        &host_id_a,
        owned.generation,
        owned.client_id,
        owned.value.command_id(),
    )?;
    fleet.synchronize(&host_id_a).await?;
    fleet.synchronize(&host_id_b).await?;
    await_task_title(&fleet, &host_id_a, shared_task, "Fleet A · renamed").await?;
    step(
        task_title(&fleet, &host_id_a, shared_task)? == "Fleet A · renamed"
            && task_title(&fleet, &host_id_b, shared_task)? == "Fleet B · shared task",
        "rename on A leaves B title unchanged",
    )?;

    let admission_b = fleet.admit_read(HostTaskKey::new(host_id_b.clone(), shared_task))?;
    drop(local_a);
    host_a.terminate_owned_job()?;
    step(true, "stopped owned host A job")?;

    let b_ok = tokio::time::timeout(Duration::from_secs(15), async {
        fleet.synchronize(&host_id_b).await?;
        let title = task_title(&fleet, &host_id_b, shared_task)?;
        fleet.validate_admission(&admission_b)?;
        let _ = fleet
            .query(
                &admission_b,
                devmanager::client::action::task_show_query(
                    RequestId::new(),
                    admission_b.client_id,
                    shared_task,
                ),
            )
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(title)
    })
    .await
    .map_err(|_| "B query/sync timed out after A stop")??;
    step(
        b_ok == "Fleet B · shared task",
        "B remains usable after A stop (bounded timeout)",
    )?;

    host_a.restart_same_profile()?;
    local_a = host_a.connect_local().await?;
    // Same listen port as first enroll so the persisted trust endpoint stays valid.
    let port_restart = host_a
        .enable_remote_listening_on(&mut local_a, Some(port_a))
        .await?;
    step(
        port_restart == port_a,
        "A restart rebound the same loopback listen port",
    )?;

    let stale = admission_a.clone();
    // Production path: connect_trusted_host from the saved trust record, then
    // fleet reconnect_with_factory (no re-pair, no resend of prior CommandIds).
    fleet
        .reconnect_with_factory(
            &host_id_a,
            trusted_reconnect_factory(trust_root.clone(), record_a.host_public_id),
        )
        .await
        .map_err(|error| format!("reconnect_with_factory: {error}"))?;
    step(
        true,
        "connect_trusted_host + reconnect_with_factory restored A",
    )?;

    let stale_rejected = matches!(
        fleet.validate_admission(&stale),
        Err(FleetError::StaleGeneration) | Err(FleetError::StaleClientId)
    );
    step(stale_rejected, "old A admission rejected after reconnect")?;

    fleet.synchronize(&host_id_a).await?;
    fleet.synchronize(&host_id_b).await?;
    step(
        task_title(&fleet, &host_id_a, shared_task)? == "Fleet A · renamed"
            && task_title(&fleet, &host_id_b, shared_task)? == "Fleet B · shared task",
        "new A snapshot keeps rename; B unaffected",
    )?;

    let removal = fleet.remove(&host_id_a).await?;
    ack_removal_retained(&fleet, &removal)?;
    step(
        !fleet.contains(&host_id_a) && fleet.contains(&host_id_b),
        "remove A leaves B present",
    )?;
    let mut stale_record_a = record_a.clone();
    stale_record_a.assigned_client_id = devmanager::domain::ClientId::new();
    step(
        matches!(
            forget_trusted_host(&store, stale_record_a, Duration::from_secs(10)).await,
            Err(RemoteTrustError::PinChanged)
        ),
        "stale Settings forget cannot remove current paired identity",
    )?;
    forget_trusted_host(&store, record_a.clone(), Duration::from_secs(10)).await?;
    forget_trusted_host(&store, record_a, Duration::from_secs(10)).await?;
    let remaining = list_trusted_hosts(&store, Duration::from_secs(10)).await?;
    step(
        remaining == vec![record_b],
        "exact A forget is idempotent and leaves B trust intact",
    )?;
    fleet.synchronize(&host_id_b).await?;
    step(
        task_title(&fleet, &host_id_b, shared_task)? == "Fleet B · shared task",
        "B still synchronized after A remove",
    )?;

    let removal_b = fleet.remove(&host_id_b).await?;
    ack_removal_retained(&fleet, &removal_b)?;
    host_b.disable_remote_and_quit(&mut local_b).await?;
    drop(local_b);
    drop(host_b);
    host_a.disable_remote_and_quit(&mut local_a).await?;
    drop(local_a);
    drop(host_a);
    Ok(())
}

fn trusted_reconnect_factory(
    trust_root: PathBuf,
    host_public_id: [u8; 16],
) -> devmanager::client::HostClientFactory {
    Box::new(move || {
        Box::pin(async move {
            let store = RemoteTrustStore::open(trust_root).map_err(|_| IpcError::Unavailable)?;
            connect_trusted_host(&store, host_public_id, ConnectTrustedOptions::default())
                .await
                .map_err(|_| IpcError::Unavailable)
        })
    })
}

async fn await_task_title(
    fleet: &HostFleet,
    host: &HostId,
    task: TaskId,
    expected: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if task_title(fleet, host, task)? == expected {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
            let event = fleet.recv_subscription_update(host).await?;
            step(event.host == *host, "live event retains exact host owner")?;
        }
    })
    .await
    .map_err(|_| "live task projection did not update before deadline")?
}

fn task_title(
    fleet: &HostFleet,
    host: &HostId,
    task_id: TaskId,
) -> Result<String, Box<dyn std::error::Error>> {
    let model = fleet
        .presentation_model(host)?
        .ok_or("missing presentation model")?
        .value;
    let snapshot = model.task(task_id).ok_or("missing task in model")?;
    Ok(snapshot.task.title.clone())
}

fn task_revision(
    fleet: &HostFleet,
    host: &HostId,
    task_id: TaskId,
) -> Result<u64, Box<dyn std::error::Error>> {
    let model = fleet
        .presentation_model(host)?
        .ok_or("missing presentation model")?
        .value;
    let snapshot = model.task(task_id).ok_or("missing task in model")?;
    Ok(snapshot.task.revision)
}

fn ack_removal_retained(
    fleet: &HostFleet,
    removal: &devmanager::client::FleetRemoval,
) -> Result<(), Box<dyn std::error::Error>> {
    for owned in &removal.retained {
        match &owned.value {
            FleetRetainedCommand::Receipt(receipt) => {
                println!(
                    "ACK retained receipt command_id={} gen={} (not resent)",
                    receipt.command_id(),
                    owned.generation
                );
            }
            FleetRetainedCommand::Uncertain(uncertain) => {
                println!(
                    "ACK uncertain command_id={} (not resent)",
                    uncertain.command_id
                );
            }
        }
    }
    for uncertain in &removal.uncertain {
        println!(
            "ACK removal uncertain command_id={} (not resent)",
            uncertain.command_id
        );
    }
    fleet.acknowledge_removal_ledger(&removal.host, removal.generation)?;
    Ok(())
}
