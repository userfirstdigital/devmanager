//! Interactive native-shell UI acceptance for two trusted remote hosts that
//! deliberately share one raw TaskId.
//!
//! Prepares fixture hosts and enrolls them into an isolated native-client
//! profile via production `RemoteTrustStore`, then mounts the real public
//! `run_native_shell` on the main OS thread so ordinary trusted-host autoload
//! can be exercised once root integration is ready.
//!
//! Build after native trusted-host startup integration:
//! `cargo build --bin devmanager-host --example remote-native-ui-smoke`
//!
//! Host binary layout (examples find host one directory up; native shell finds
//! it beside the current exe). From the isolated target debug directory:
//! ```text
//! Copy-Item -LiteralPath .\devmanager-host.exe -Destination .\examples\devmanager-host.exe -Force
//! ```
//!
//! Default: no live LLM. Optional `--with-codex` starts the first shared task
//! on both fixture hosts; the second task remains a never-started Codex draft.
//! This covers both live identityless reuse and deferred first-send. Never sends
//! prompts autonomously. Closing the native window joins fixture-owned hosts
//! and TempDir cleanup only — no installed app/profile mutations.
//! `--deferred-codex` instead leaves every Codex claim unstarted, including the
//! first task, to exercise cold provider-manager readiness on fresh profiles.
#[path = "support/remote_smoke.rs"]
mod remote_smoke;

use std::path::{Path, PathBuf};

use devmanager::client::{pair_enroll_and_connect, PairEnrollRequest, RemoteTrustStore};
use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};
use devmanager::domain::TaskId;
use devmanager::ui::native_shell::{isolated_dev_profile, run_native_shell};
use remote_smoke::IsolatedHostFixture;
use zeroize::Zeroizing;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    remote_smoke::require_windows_debug()?;
    let (with_codex, start_first_provider) = parse_args()?;
    preflight_host_binaries()?;

    // Owned temp workspace for the native shell profile. Fixtures stay on this
    // stack until the window closes so RAII cleanup cannot race GPUI.
    let native_temp = tempfile::Builder::new()
        .prefix("devmanager-native-ui-")
        .tempdir()?;
    let native_workspace = native_temp.path().join("workspace");
    std::fs::create_dir_all(&native_workspace)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut prepared = runtime.block_on(prepare_fixtures(
        with_codex,
        start_first_provider,
        &native_workspace,
    ))?;
    print_fixture_ready(&prepared, with_codex, start_first_provider);

    // Keep fixtures + runtime alive across the blocking native shell. Separate
    // the UI result so teardown still runs when the shell fails.
    let shell_result = run_native_shell(&native_workspace);
    let teardown_result = runtime.block_on(teardown_fixtures(&mut prepared));

    drop(prepared);
    drop(runtime);
    drop(native_temp);

    if let Err(error) = &teardown_result {
        eprintln!("fixture teardown warning: {error}");
    }
    shell_result?;
    teardown_result?;
    Ok(())
}

fn parse_args() -> Result<(bool, bool), Box<dyn std::error::Error>> {
    let mut with_codex = false;
    let mut start_first_provider = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--with-codex" | "--deferred-codex" if !with_codex => {
                with_codex = true;
                start_first_provider = arg == "--with-codex";
            }
            other => {
                return Err(format!(
                    "unexpected argument: {other} (choose --with-codex or --deferred-codex)"
                )
                .into());
            }
        }
    }
    Ok((with_codex, start_first_provider))
}

/// Native shell resolves `devmanager-host` beside the current exe; the smoke
/// helper resolves it one directory up. Both must already exist as real files.
fn preflight_host_binaries() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let examples_dir = executable
        .parent()
        .ok_or("example executable has no parent directory")?;
    let debug_dir = examples_dir
        .parent()
        .ok_or("example executable is not under target/<profile>/examples")?;
    let helper_host = debug_dir.join("devmanager-host.exe");
    let native_host = examples_dir.join("devmanager-host.exe");
    if helper_host.is_file() && native_host.is_file() {
        return Ok(());
    }
    Err(format!(
        "missing real sibling host binary for native UI smoke.\n\
         Helper expects:  {}\n\
         Native shell expects (beside this example): {}\n\
         From the isolated target debug directory, after `cargo build --bin devmanager-host`:\n\
           Copy-Item -LiteralPath .\\devmanager-host.exe -Destination .\\examples\\devmanager-host.exe -Force\n\
         Do not invent a redirect or weaken native path validation.",
        helper_host.display(),
        native_host.display(),
    )
    .into())
}

struct PreparedFixtures {
    host_a: IsolatedHostFixture,
    host_b: IsolatedHostFixture,
    pid_a: u32,
    pid_b: u32,
    shared_task: TaskId,
    draft_task_a: TaskId,
    draft_task_b: TaskId,
    host_public_id_a: [u8; 16],
    host_public_id_b: [u8; 16],
    native_workspace: PathBuf,
    named_profile: String,
    trust_root: PathBuf,
}

async fn prepare_fixtures(
    with_codex: bool,
    start_first_provider: bool,
    native_workspace: &Path,
) -> Result<PreparedFixtures, Box<dyn std::error::Error>> {
    let shared_task = TaskId::new();
    let mut host_a = IsolatedHostFixture::spawn(
        "devmanager-native-ui-a-",
        "Native UI Smoke A",
        "remote-native-ui-smoke/a",
    )?;
    let mut host_b = IsolatedHostFixture::spawn(
        "devmanager-native-ui-b-",
        "Native UI Smoke B",
        "remote-native-ui-smoke/b",
    )?;
    let pid_a = host_a.owner_pid();
    let pid_b = host_b.owner_pid();

    let mut local_a = host_a.connect_local().await?;
    let (_, draft_task_a) = host_a
        .create_project_and_tasks(
            &mut local_a,
            with_codex,
            start_first_provider,
            shared_task,
            "Native UI A · shared task",
            "Native UI A · never-started draft",
        )
        .await?;
    let port_a = host_a.enable_remote_listening(&mut local_a).await?;
    let pairing_a = host_a.pairing_info(&mut local_a).await?;
    let code_a = match pairing_a {
        devmanager::host::remote_setup::RemoteSetupReply::PairingInfo { code, .. } => {
            Zeroizing::new(code)
        }
        other => return Err(format!("expected pairing info A, got {other:?}").into()),
    };
    drop(local_a);

    let mut local_b = host_b.connect_local().await?;
    let (_, draft_task_b) = host_b
        .create_project_and_tasks(
            &mut local_b,
            with_codex,
            start_first_provider,
            shared_task,
            "Native UI B · shared task",
            "Native UI B · never-started draft",
        )
        .await?;
    let port_b = host_b.enable_remote_listening(&mut local_b).await?;
    let pairing_b = host_b.pairing_info(&mut local_b).await?;
    let code_b = match pairing_b {
        devmanager::host::remote_setup::RemoteSetupReply::PairingInfo { code, .. } => {
            Zeroizing::new(code)
        }
        other => return Err(format!("expected pairing info B, got {other:?}").into()),
    };
    drop(local_b);

    // Exact native profile the public shell will reopen. Trust enrollment must
    // land under this resolved root before GPUI starts — never ambient app dirs.
    let profile = isolated_dev_profile(native_workspace)?;
    let trust_root = resolve_app_paths(
        profile.host_config_base(),
        AppProfile::named(profile.named_profile())?,
        BuildKind::Debug,
    )?
    .root;
    let store = RemoteTrustStore::open(trust_root.clone())?;

    let (client_a, record_a) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port_a}"),
            pairing_code: code_a,
            label: Some("Native UI A".into()),
            ..PairEnrollRequest::default()
        },
    )
    .await?;
    let (client_b, record_b) = pair_enroll_and_connect(
        &store,
        PairEnrollRequest {
            endpoint: format!("http://127.0.0.1:{port_b}"),
            pairing_code: code_b,
            label: Some("Native UI B".into()),
            ..PairEnrollRequest::default()
        },
    )
    .await?;

    if record_a.host_public_id == record_b.host_public_id {
        return Err("fixture hosts must present distinct host public IDs".into());
    }
    // Drop preparation clients before native autoload. Persisted trust records
    // and assigned client identities remain for ordinary shell startup.
    drop(client_a);
    drop(client_b);
    drop(store);

    Ok(PreparedFixtures {
        host_a,
        host_b,
        pid_a,
        pid_b,
        shared_task,
        draft_task_a,
        draft_task_b,
        host_public_id_a: record_a.host_public_id,
        host_public_id_b: record_b.host_public_id,
        native_workspace: native_workspace.to_path_buf(),
        named_profile: profile.named_profile().to_string(),
        trust_root,
    })
}

async fn teardown_fixtures(
    prepared: &mut PreparedFixtures,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut local_a = prepared.host_a.connect_local().await?;
    prepared
        .host_a
        .disable_remote_and_quit(&mut local_a)
        .await?;
    drop(local_a);

    let mut local_b = prepared.host_b.connect_local().await?;
    prepared
        .host_b
        .disable_remote_and_quit(&mut local_b)
        .await?;
    drop(local_b);
    Ok(())
}

fn print_fixture_ready(prepared: &PreparedFixtures, with_codex: bool, start_first_provider: bool) {
    println!("FIXTURE READY (not PASS) — click the actions below manually.");
    println!("fixture host A pid={}", prepared.pid_a);
    println!("fixture host B pid={}", prepared.pid_b);
    println!("native workspace: {}", prepared.native_workspace.display());
    println!("native profile: {}", prepared.named_profile);
    println!("trust root: {}", prepared.trust_root.display());
    println!(
        "host A public id: {}",
        uuid::Uuid::from_bytes(prepared.host_public_id_a)
    );
    println!(
        "host B public id: {}",
        uuid::Uuid::from_bytes(prepared.host_public_id_b)
    );
    println!("shared raw TaskId: {}", prepared.shared_task);
    println!("host A never-started draft: {}", prepared.draft_task_a);
    println!("host B never-started draft: {}", prepared.draft_task_b);
    println!(
        "provider start on both hosts: {}",
        if start_first_provider {
            "yes (--with-codex)"
        } else if with_codex {
            "no (all claims deferred; --deferred-codex)"
        } else {
            "no"
        }
    );
    println!(
        "Manual acceptance:\n\
         1. Confirm two rows/panes for the same TaskId with distinct A/B labels.\n\
         2. Rename / Done / Archive on one owner must not mutate the other.\n\
         3. Keep two independent drafts across the shared TaskId owners.\n\
         4. {}.\n\
         Closing the window joins fixture hosts and deletes only this TempDir.\n\
         This example does not claim physical LAN/WAN/mobile acceptance.",
        if with_codex {
            "Send on each shared task, then each never-started draft"
        } else {
            "Skip live provider send unless you re-run with --with-codex"
        }
    );
}
