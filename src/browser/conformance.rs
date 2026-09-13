//! Deterministic fixture cases and a fail-closed real-provider HOLD.
//!
//! Fixture validation is file/source inspection only. Authenticated Claude,
//! Codex, and Cursor launches stay HOLD: this module never starts a provider,
//! browser, WebView2 helper, or fixture HTTP server.

use std::path::{Path, PathBuf};

use crate::domain::browser::BrowserIntegrationHold;

pub const BROWSER_E2E_VERIFICATION_TOKEN: &str = "DM-BROWSER-E2E-OK";
pub const BROWSER_E2E_SCHEMA_VERSION: u32 = 1;
pub const BROWSER_VISIBLE_WEBVIEW2_OPT_IN_ENV: &str = "DEVMANAGER_BROWSER_WEBVIEW2_E2E";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserVisibleHostProofClass {
    FixtureProtocolOnly,
    VisibleHold,
    VisibleGreen,
}

impl BrowserVisibleHostProofClass {
    pub fn is_visible_green(self) -> bool {
        matches!(self, Self::VisibleGreen)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserVisibleHostProofClaim {
    pub fixture_only: bool,
    pub visible_claimed: bool,
    pub opt_in_marker: bool,
    pub observed_host_owned_webview2: bool,
    pub observed_window_lifecycle: bool,
    pub observed_helper_lifecycle: bool,
}

/// Classify a visible-host proof claim.
///
/// Fixture-only runs may prove protocol/admission. They never become
/// [`BrowserVisibleHostProofClass::VisibleGreen`], even if a caller marks
/// every observation flag. A visible claim without the opt-in marker and a
/// complete host-owned WebView2/window/helper observation is a HOLD.
pub fn classify_visible_host_proof(
    claim: BrowserVisibleHostProofClaim,
) -> BrowserVisibleHostProofClass {
    if claim.fixture_only || !claim.visible_claimed {
        return BrowserVisibleHostProofClass::FixtureProtocolOnly;
    }
    if claim.opt_in_marker
        && claim.observed_host_owned_webview2
        && claim.observed_window_lifecycle
        && claim.observed_helper_lifecycle
    {
        BrowserVisibleHostProofClass::VisibleGreen
    } else {
        BrowserVisibleHostProofClass::VisibleHold
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserProviderArm {
    Fixture,
    AuthenticatedHold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserProviderE2EHold {
    AuthenticatedLaunchRequiresExplicitOptIn,
    BrowserServiceAbsent,
    WebViewSurfaceAbsent,
}

impl From<BrowserProviderE2EHold> for BrowserIntegrationHold {
    fn from(hold: BrowserProviderE2EHold) -> Self {
        match hold {
            BrowserProviderE2EHold::AuthenticatedLaunchRequiresExplicitOptIn
            | BrowserProviderE2EHold::BrowserServiceAbsent => {
                BrowserIntegrationHold::BrowserServiceAbsent
            }
            BrowserProviderE2EHold::WebViewSurfaceAbsent => {
                BrowserIntegrationHold::WebViewSurfaceAbsent
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFixtureAction {
    Navigate,
    InspectValue,
    FillNonSecretForm,
    ChooseOption,
    Submit,
    OpenTab,
    DownloadArtifact,
    UploadArtifact,
    HandlePermission,
    ReportVerificationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserFixtureCase {
    pub id: &'static str,
    pub prompt: &'static str,
    pub expected_token: &'static str,
    pub actions: &'static [BrowserFixtureAction],
    pub recovery: Option<BrowserFixtureRecoveryCase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFixtureRecoveryCase {
    NavigationError,
    RendererCrash,
    ProviderCrash,
    HostFullQuit,
    FailedBrowserLaunch,
}

pub const BROWSER_FIXTURE_CASES: &[BrowserFixtureCase] = &[
    BrowserFixtureCase {
        id: "navigate-inspect-fill-submit",
        prompt: "Open the local fixture, inspect the semantic target, fill the non-secret display name, choose option beta, submit the form, and report the verification token.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[
            BrowserFixtureAction::Navigate,
            BrowserFixtureAction::InspectValue,
            BrowserFixtureAction::FillNonSecretForm,
            BrowserFixtureAction::ChooseOption,
            BrowserFixtureAction::Submit,
            BrowserFixtureAction::ReportVerificationToken,
        ],
        recovery: None,
    },
    BrowserFixtureCase {
        id: "tab-download-upload-permission",
        prompt: "From the local fixture open a second tab, download the fixture artifact, upload it through the file input, handle the camera permission control, and report the verification token.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[
            BrowserFixtureAction::OpenTab,
            BrowserFixtureAction::DownloadArtifact,
            BrowserFixtureAction::UploadArtifact,
            BrowserFixtureAction::HandlePermission,
            BrowserFixtureAction::ReportVerificationToken,
        ],
        recovery: None,
    },
    BrowserFixtureCase {
        id: "recovery-navigation-error",
        prompt: "Attempt the fixture navigation-error page, settle the interruption, and report the verification token only after recovery.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[
            BrowserFixtureAction::Navigate,
            BrowserFixtureAction::ReportVerificationToken,
        ],
        recovery: Some(BrowserFixtureRecoveryCase::NavigationError),
    },
    BrowserFixtureCase {
        id: "recovery-renderer-crash",
        prompt: "Attempt the fixture renderer-crash page, settle the interruption, and report the verification token only after recovery.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[
            BrowserFixtureAction::Navigate,
            BrowserFixtureAction::ReportVerificationToken,
        ],
        recovery: Some(BrowserFixtureRecoveryCase::RendererCrash),
    },
    BrowserFixtureCase {
        id: "recovery-provider-crash",
        prompt: "Simulate a provider crash against the fixture, settle the task, and leave zero helper members.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[BrowserFixtureAction::ReportVerificationToken],
        recovery: Some(BrowserFixtureRecoveryCase::ProviderCrash),
    },
    BrowserFixtureCase {
        id: "recovery-host-full-quit",
        prompt: "Simulate host full quit during the fixture run and settle teardown with zero residue.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[BrowserFixtureAction::ReportVerificationToken],
        recovery: Some(BrowserFixtureRecoveryCase::HostFullQuit),
    },
    BrowserFixtureCase {
        id: "recovery-failed-browser-launch",
        prompt: "Simulate a failed browser launch and settle the failed-create teardown without helpers.",
        expected_token: BROWSER_E2E_VERIFICATION_TOKEN,
        actions: &[BrowserFixtureAction::ReportVerificationToken],
        recovery: Some(BrowserFixtureRecoveryCase::FailedBrowserLaunch),
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserFixtureValidation {
    pub root: PathBuf,
    pub cases: usize,
    pub token: &'static str,
    pub network_urls: usize,
    pub secrets_in_manifest: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProviderHoldRecord {
    pub arm: BrowserProviderArm,
    pub provider: Option<String>,
    pub hold: BrowserProviderE2EHold,
    pub launched: bool,
}

pub fn browser_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/browser-site")
}

pub fn validate_browser_fixture_site(
    root: &Path,
) -> Result<BrowserFixtureValidation, BrowserFixtureValidationError> {
    if !root.is_dir() {
        return Err(BrowserFixtureValidationError::MissingRoot);
    }
    let required = [
        "index.html",
        "redirect.html",
        "destination.html",
        "download.txt",
        "api-success.json",
        "verification.json",
        "navigation-error.html",
        "renderer-crash.html",
    ];
    let mut network_urls = 0usize;
    for name in required {
        let path = root.join(name);
        let body = std::fs::read_to_string(&path)
            .map_err(|_| BrowserFixtureValidationError::MissingFile(name))?;
        if body.contains("https://") || body.contains("http://") {
            network_urls += 1;
        }
        if name == "index.html" {
            for marker in [
                "data-testid=\"semantic-target\"",
                "data-testid=\"fixture-form\"",
                "data-testid=\"fixture-select\"",
                "data-testid=\"submit-form\"",
                "data-testid=\"new-tab-link\"",
                "data-testid=\"fixture-download\"",
                "data-testid=\"fixture-upload\"",
                "data-testid=\"permission-target\"",
                "data-testid=\"verification-token\"",
                BROWSER_E2E_VERIFICATION_TOKEN,
            ] {
                if !body.contains(marker) {
                    return Err(BrowserFixtureValidationError::MissingMarker(marker));
                }
            }
        }
        if name == "verification.json" && !body.contains(BROWSER_E2E_VERIFICATION_TOKEN) {
            return Err(BrowserFixtureValidationError::MissingMarker(
                BROWSER_E2E_VERIFICATION_TOKEN,
            ));
        }
        if body.contains("sk-") || body.contains("secret:") {
            return Err(BrowserFixtureValidationError::SecretInFixture);
        }
    }
    if network_urls != 0 {
        return Err(BrowserFixtureValidationError::ExternalNetwork);
    }
    if BROWSER_FIXTURE_CASES
        .iter()
        .any(|case| case.expected_token != BROWSER_E2E_VERIFICATION_TOKEN)
    {
        return Err(BrowserFixtureValidationError::MissingMarker(
            BROWSER_E2E_VERIFICATION_TOKEN,
        ));
    }
    Ok(BrowserFixtureValidation {
        root: root.to_path_buf(),
        cases: BROWSER_FIXTURE_CASES.len(),
        token: BROWSER_E2E_VERIFICATION_TOKEN,
        network_urls: 0,
        secrets_in_manifest: 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserFixtureValidationError {
    MissingRoot,
    MissingFile(&'static str),
    MissingMarker(&'static str),
    ExternalNetwork,
    SecretInFixture,
}

pub fn hold_authenticated_provider_launch(provider: Option<&str>) -> BrowserProviderHoldRecord {
    let _ = provider;
    BrowserProviderHoldRecord {
        arm: BrowserProviderArm::AuthenticatedHold,
        provider: provider.map(str::to_string),
        hold: BrowserProviderE2EHold::AuthenticatedLaunchRequiresExplicitOptIn,
        launched: false,
    }
}

pub fn real_provider_launch_is_forbidden() -> bool {
    !hold_authenticated_provider_launch(None).launched
        && matches!(
            hold_authenticated_provider_launch(Some("claude")).hold,
            BrowserProviderE2EHold::AuthenticatedLaunchRequiresExplicitOptIn
        )
}
