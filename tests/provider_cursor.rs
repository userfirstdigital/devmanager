use devmanager::domain::ProviderSessionId;
use devmanager::providers::adapter::{
    LaunchProviderRequest, ProviderAdapter, ProviderRuntime, StopStrategy,
};
use devmanager::providers::capabilities::{ProviderCapability, ProviderExecutable, ProviderKind};
use devmanager::providers::cursor::CursorAdapter;
use devmanager::providers::registry::{ProviderDiscoveryConfig, ProviderRegistry};
use serde_json::Value;
use std::sync::Arc;
use tempfile::tempdir;

const PHASE4_11_SMOKE_CONTRACT: &str =
    include_str!("fixtures/providers/cursor/phase4_11_smoke_contract.json");

fn test_executable() -> ProviderExecutable {
    // Capability refusal does not execute the image, but the pinned executable
    // contract still requires a real, inspectable identity rather than a fake path.
    ProviderExecutable::from_path(std::env::current_exe().expect("test executable path"))
        .expect("inspect test executable")
}

#[test]
fn cursor_public_constructor_is_live_runner_not_pinned_bytes() {
    let adapter = CursorAdapter::new();
    assert_eq!(adapter.kind(), ProviderKind::Cursor);
}

#[tokio::test]
async fn cursor_stays_unsupported_until_registry_registration() {
    let registry = ProviderRegistry::new();
    let observed = registry
        .observe(ProviderKind::Cursor, &ProviderDiscoveryConfig::default())
        .await;
    assert!(matches!(
        observed,
        Err(devmanager::providers::ProviderError::ProviderNotRegistered(
            ProviderKind::Cursor
        ))
    ));
}

#[test]
fn cursor_public_adapter_cannot_launch_without_exact_capability() {
    let adapter = CursorAdapter::new();
    let executable = test_executable();
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            executable.open_for_launch().unwrap(),
            None,
            None,
        )),
        Err(devmanager::providers::ProviderError::UnsupportedCapability(
            ProviderCapability::BuildLaunch
        ))
    ));
}

#[tokio::test]
async fn cursor_signals_stop_and_quota_stay_explicitly_unsupported() {
    let adapter = CursorAdapter::new();
    assert_eq!(
        adapter.cooperative_stop(&ProviderRuntime),
        StopStrategy::Unsupported
    );
    let executable = test_executable().open_for_launch().unwrap();
    assert!(matches!(
        adapter.observe_quota(&executable).await,
        Err(devmanager::providers::ProviderError::UnsupportedCapability(
            ProviderCapability::ObserveQuota
        ))
    ));
}

#[tokio::test]
async fn cursor_adapter_does_not_accept_desktop_cursor_exe() {
    let temp = tempdir().unwrap();
    let desktop = temp.path().join("cursor.exe");
    std::fs::write(&desktop, b"desktop-cursor").unwrap();

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CursorAdapter::new())).unwrap();

    let rejected = registry
        .resolve_executable(
            ProviderKind::Cursor,
            &ProviderDiscoveryConfig {
                executable_override: Some(desktop),
                path: None,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        rejected,
        Err(devmanager::providers::ProviderError::WrapperCommandNotAllowed { .. })
            | Err(devmanager::providers::ProviderError::ExecutableNotAllowed { .. })
    ));
}

#[test]
fn cursor_phase4_11_smoke_contract_is_fixture_only_and_does_not_claim_auth() {
    let contract: Value =
        serde_json::from_str(PHASE4_11_SMOKE_CONTRACT).expect("cursor smoke contract json");

    assert_eq!(contract["contract_id"], "phase4.11.cursor.smoke");
    assert_eq!(contract["mode"], "fixture_only");
    assert_eq!(contract["launches_provider"], false);
    assert_eq!(contract["claims_auth"], false);
    assert_eq!(contract["host_registration_required"], true);
    assert_eq!(
        contract["environment"]["required"]["DEVMANAGER_PROFILE"],
        "provider-smoke-cursor-dev"
    );
    assert_eq!(
        contract["environment"]["forbidden_config_roots"][0],
        "%APPDATA%\\com.userfirst.devmanager"
    );
    assert_eq!(
        contract["executable"]["allowed_entrypoints"][0],
        "cursor-agent"
    );
    assert!(contract["executable"]["forbidden_entrypoints"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "cursor.exe"));
    assert_eq!(contract["capabilities"]["terminal_only"], true);
    assert_eq!(contract["capabilities"]["exact_resume"], "Unsupported");
    assert_eq!(contract["capabilities"]["semantic_events"], "Unsupported");
    assert_eq!(contract["capabilities"]["auth_state"], "Unknown");
    assert_eq!(
        contract["assertions"]["exact_resume_error"],
        "UnsupportedCapability(ExactResume)"
    );
    assert_eq!(contract["assertions"]["no_fresh_conversation"], true);
    assert_eq!(contract["assertions"]["one_root_pty"], true);
    assert_eq!(contract["assertions"]["zero_post_close_residue"], true);
    assert_eq!(
        contract["later_runner"]["authenticated"]["argv"],
        serde_json::json!(["-Authenticated", "-Provider", "cursor"])
    );
    assert_eq!(
        contract["later_runner"]["authenticated"]["requires_operator_opt_in"],
        true
    );
    assert_eq!(
        contract["later_runner"]["authenticated"]["refuse_production_profile"],
        true
    );

    let adapter = CursorAdapter::new();
    let executable = test_executable();
    let executable_handle = executable.open_for_launch().unwrap();
    let session = ProviderSessionId::new("chat-id-must-not-be-inferred").unwrap();
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            executable_handle.clone(),
            None,
            Some(session)
        )),
        Err(devmanager::providers::ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));
    assert!(
        matches!(
            adapter.build_launch(LaunchProviderRequest::new(executable_handle, None, None)),
            Err(devmanager::providers::ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch
            ))
        ),
        "fixture contract must not mint a fresh conversation when resume is unsupported"
    );
    assert_eq!(
        adapter.cooperative_stop(&ProviderRuntime),
        StopStrategy::Unsupported
    );
    assert_eq!(adapter.kind(), ProviderKind::Cursor);
}
