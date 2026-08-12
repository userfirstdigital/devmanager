use std::process::Command;

use devmanager::browser::{
    browser_fixture_root, hold_authenticated_provider_launch, real_provider_launch_is_forbidden,
    validate_browser_fixture_site, BrowserFixtureAction, BrowserFixtureRecoveryCase,
    BrowserProviderArm, BrowserProviderE2EHold, BROWSER_E2E_VERIFICATION_TOKEN, BROWSER_FIXTURE_CASES,
};
use static_assertions::assert_not_impl_any;

assert_not_impl_any!(BrowserProviderE2EHold: From<bool>);

#[test]
fn fixture_cases_cover_the_deterministic_provider_prompt_matrix() {
    let required = [
        BrowserFixtureAction::Navigate,
        BrowserFixtureAction::InspectValue,
        BrowserFixtureAction::FillNonSecretForm,
        BrowserFixtureAction::ChooseOption,
        BrowserFixtureAction::Submit,
        BrowserFixtureAction::OpenTab,
        BrowserFixtureAction::DownloadArtifact,
        BrowserFixtureAction::UploadArtifact,
        BrowserFixtureAction::HandlePermission,
        BrowserFixtureAction::ReportVerificationToken,
    ];
    for action in required {
        assert!(
            BROWSER_FIXTURE_CASES
                .iter()
                .any(|case| case.actions.contains(&action)),
            "missing fixture action {action:?}"
        );
    }
    for recovery in [
        BrowserFixtureRecoveryCase::NavigationError,
        BrowserFixtureRecoveryCase::RendererCrash,
        BrowserFixtureRecoveryCase::ProviderCrash,
        BrowserFixtureRecoveryCase::HostFullQuit,
        BrowserFixtureRecoveryCase::FailedBrowserLaunch,
    ] {
        assert!(
            BROWSER_FIXTURE_CASES
                .iter()
                .any(|case| case.recovery == Some(recovery)),
            "missing recovery case {recovery:?}"
        );
    }
    for case in BROWSER_FIXTURE_CASES {
        assert_eq!(case.expected_token, BROWSER_E2E_VERIFICATION_TOKEN);
        assert!(!case.prompt.contains("https://"));
        assert!(!case.prompt.contains("sk-"));
    }
}

#[test]
fn fixture_site_validates_without_network_or_launch() {
    let validation =
        validate_browser_fixture_site(&browser_fixture_root()).expect("fixture site");
    assert_eq!(validation.token, BROWSER_E2E_VERIFICATION_TOKEN);
    assert_eq!(validation.network_urls, 0);
    assert_eq!(validation.secrets_in_manifest, 0);
    assert_eq!(validation.cases, BROWSER_FIXTURE_CASES.len());
}

#[test]
fn authenticated_provider_arm_is_a_hold_and_never_launches() {
    assert!(real_provider_launch_is_forbidden());
    for provider in [None, Some("claude"), Some("codex"), Some("cursor")] {
        let hold = hold_authenticated_provider_launch(provider);
        assert_eq!(hold.arm, BrowserProviderArm::AuthenticatedHold);
        assert!(!hold.launched);
        assert_eq!(
            hold.hold,
            BrowserProviderE2EHold::AuthenticatedLaunchRequiresExplicitOptIn
        );
    }
}

#[test]
fn e2e_script_hold_path_does_not_spawn_providers_or_browsers() {
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/native-next/Invoke-BrowserProviderE2E.ps1");
    assert!(script.is_file(), "missing {}", script.display());
    let source = std::fs::read_to_string(&script).expect("read e2e script");
    for forbidden in [
        "Start-Process",
        "Invoke-WebRequest",
        "claude ",
        "codex ",
        "cursor ",
        "WebView2",
        "msedge",
        "cargo test",
        "python -m http.server",
    ] {
        assert!(
            !source.to_ascii_lowercase().contains(&forbidden.to_ascii_lowercase())
                || forbidden == "WebView2" && source.contains("HOLD"),
            "script must not launch {forbidden}"
        );
    }
    assert!(source.contains("AuthenticatedLaunchRequiresExplicitOptIn"));
    assert!(source.contains("-Fixture"));
    assert!(source.contains("-Authenticated"));
    let _ = Command::new("pwsh");
}
