//! Real host + browser smoke fixture. Never opens an installed profile.
//! Build with `cargo build --bin devmanager-host --example remote-host-smoke`.
//! Run the example, visit the printed loopback URL, then press Enter to stop.
//! Type `restart` to restart only its owned host/provider tree in the same profile.
#[path = "support/remote_smoke.rs"]
mod remote_smoke;

use std::io;

use remote_smoke::{exercise_native_client, IsolatedHostFixture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    remote_smoke::require_windows_debug()?;
    // Optional real CLI; the default fixture never launches a provider.
    let with_codex = std::env::args().skip(1).any(|arg| arg == "--with-codex");
    let mut fixture =
        IsolatedHostFixture::spawn("devmanager-remote-smoke-", "Remote Smoke", "remote-smoke/1")?;
    let owner_pid = fixture.owner_pid();
    let profile = fixture.profile().to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut client = runtime.block_on(fixture.connect_local())?;
    eprintln!("Smoke: connected to isolated host {owner_pid}");
    runtime.block_on(async {
        let first_task = devmanager::domain::TaskId::new();
        fixture
            .create_project_and_tasks(
                &mut client,
                with_codex,
                with_codex,
                first_task,
                "Remote owner · first task",
                "Remote owner · second task",
            )
            .await?;
        eprintln!("Smoke: project created");
        eprintln!("Smoke: task shells created");
        let port = fixture.enable_remote_listening(&mut client).await?;
        let pairing = fixture.pairing_info(&mut client).await?;
        println!("{}", serde_json::to_string(&pairing)?);
        if let devmanager::host::remote_setup::RemoteSetupReply::PairingInfo { code, .. } = &pairing
        {
            // Exercise the real HTTP admission + Noise + Hello + native query
            // path, not a pre-authenticated in-memory pair.
            exercise_native_client(port, code, fixture.fixture_root()).await?;
        }
        println!(
            "Isolated host PID {} / profile {}. Enter stops; restart restarts this exact isolated owner.",
            owner_pid, profile
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    loop {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if line.trim() != "restart" {
            break;
        }
        // The guard joins/kills only this fixture's Job; no installed or watch
        // app is discovered or stopped. Preserve profile, identity and journal.
        fixture.restart_same_profile()?;
        client = runtime.block_on(fixture.connect_local())?;
        println!(
            "Restarted isolated owner PID {} with the same profile and keys.",
            fixture.owner_pid()
        );
    }
    runtime.block_on(fixture.disable_remote_and_quit(&mut client))?;
    drop(client);
    Ok(())
}
