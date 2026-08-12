//! Capability-driven Cursor CLI adapter.
//!
//! Supported launch is proven only from the pinned interactive default surface.
//! Exact resume, semantic events, cooperative stop, quota, and auth stay
//! typed Unsupported unless a provider-native ID and command are both proven.
//! This adapter never probes Claude/Codex `auth status`, never scrapes local
//! history, and never infers a conversation id.
//!
//! Production registers this adapter through `startup::register_stock_adapters`.
//! Launch still requires a successful `cursor-agent` probe that records
//! `CapabilitySupport::Supported`. Unsupported resume/session IDs/events/quota
//! remain typed failures.

use crate::providers::adapter::{
    AdapterDeliveryPermit, AdapterIngressUnavailable, JournalNormalizeError, LaunchProviderRequest,
    NormalizedAdapterDelivery, ProviderAdapter, ProviderError, ProviderLaunchSpec,
    ProviderProbeError, ProviderProbeFailureCode, ProviderProbeIoError, ProviderProbeRequest,
    ProviderProbeRequestError, ProviderProbeRunner, ProviderProbeStatus, ProviderRuntime,
    QuotaObservation, StopStrategy, WindowsProviderProbeRunner,
};
use crate::providers::capabilities::{
    CapabilityEvidence, CapabilitySupport, EvidenceSourceId, EvidenceStatus, ProviderAuthState,
    ProviderCapabilities, ProviderCapabilitiesError, ProviderCapability, ProviderExecutableHandle,
    ProviderExecutablePolicy, ProviderKind, ProviderVersion, ProviderVersionError,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const PINNED_INTERACTIVE_SURFACE: &str =
    "When no command is specified, starts an interactive agent session in the terminal.";

struct PinnedProbes {
    version: Vec<u8>,
    help: Vec<u8>,
}

/// Release and integration crates cannot name fake-evidence constructors.
///
/// ```compile_fail
/// let _ = devmanager::providers::CursorAdapter::from_pinned_probes;
/// ```
///
/// ```compile_fail
/// let _ = devmanager::providers::CursorAdapter::from_test_runner;
/// ```
pub struct CursorAdapter {
    pinned: Option<PinnedProbes>,
    runner: Option<Arc<dyn ProviderProbeRunner>>,
    observed_at: u64,
    observed_build_launch: Mutex<Option<CapabilitySupport>>,
}

impl CursorAdapter {
    pub fn new() -> Self {
        let policy = ProviderExecutablePolicy::new(["cursor-agent"])
            .expect("cursor-agent is a valid provider entrypoint");
        Self {
            pinned: None,
            runner: Some(Arc::new(WindowsProviderProbeRunner::new(policy))),
            observed_at: now_ms(),
            observed_build_launch: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_pinned_probes(version: &[u8], help: &[u8], observed_at: u64) -> Self {
        Self {
            pinned: Some(PinnedProbes {
                version: version.to_vec(),
                help: help.to_vec(),
            }),
            runner: None,
            observed_at,
            observed_build_launch: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_test_runner(runner: Arc<dyn ProviderProbeRunner>) -> Self {
        Self {
            pinned: None,
            runner: Some(runner),
            observed_at: now_ms(),
            observed_build_launch: Mutex::new(None),
        }
    }

    async fn version_and_help(
        &self,
        executable: &ProviderExecutableHandle,
    ) -> Result<(Vec<u8>, Vec<u8>), ProviderError> {
        if let Some(pinned) = &self.pinned {
            return Ok((pinned.version.clone(), pinned.help.clone()));
        }
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ProviderError::MissingCli {
                kind: ProviderKind::Cursor,
                requested: Some(executable.canonical_path().to_path_buf()),
            })?;
        let version = run_text_probe(runner.as_ref(), executable, |handle| {
            ProviderProbeRequest::version(handle)
        })
        .await?;
        let help = run_text_probe(runner.as_ref(), executable, |handle| {
            ProviderProbeRequest::help(handle)
        })
        .await?;
        Ok((version, help))
    }

    fn record_build_launch(&self, support: Option<CapabilitySupport>) {
        *self
            .observed_build_launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = support;
    }
}

#[async_trait]
impl ProviderAdapter for CursorAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Cursor
    }

    async fn probe(
        &self,
        executable: &ProviderExecutableHandle,
    ) -> Result<ProviderCapabilities, ProviderError> {
        let observed_at = if self.pinned.is_some() {
            self.observed_at
        } else {
            now_ms()
        };
        let result = match self.version_and_help(executable).await {
            Ok((version_output, help_output)) => {
                capabilities_from_cursor_probes(&version_output, &help_output, observed_at)
            }
            Err(error) => {
                self.record_build_launch(None);
                return Err(error);
            }
        };
        match result {
            Ok(capabilities) => {
                self.record_build_launch(Some(capabilities.build_launch));
                Ok(capabilities)
            }
            Err(error) => {
                self.record_build_launch(None);
                Err(error)
            }
        }
    }

    fn build_launch(
        &self,
        request: LaunchProviderRequest,
    ) -> Result<ProviderLaunchSpec, ProviderError> {
        if request.provider_session_id().is_some() {
            return Err(ProviderError::UnsupportedCapability(
                ProviderCapability::ExactResume,
            ));
        }
        let observed = *self
            .observed_build_launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if observed != Some(CapabilitySupport::Supported) {
            return Err(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch,
            ));
        }
        ProviderLaunchSpec::new(request.executable().clone(), Vec::new())
            .map_err(|_| ProviderError::UnsupportedCapability(ProviderCapability::BuildLaunch))
    }

    fn normalize_delivery(
        &self,
        _permit: &AdapterDeliveryPermit,
        _bytes: &[u8],
    ) -> Result<NormalizedAdapterDelivery, JournalNormalizeError> {
        Err(JournalNormalizeError::Unavailable(
            AdapterIngressUnavailable,
        ))
    }

    fn cooperative_stop(&self, _session: &ProviderRuntime) -> StopStrategy {
        StopStrategy::Unsupported
    }

    async fn observe_quota(
        &self,
        _executable: &ProviderExecutableHandle,
    ) -> Result<Option<QuotaObservation>, ProviderError> {
        Err(ProviderError::UnsupportedCapability(
            ProviderCapability::ObserveQuota,
        ))
    }
}

fn capabilities_from_cursor_probes(
    version_output: &[u8],
    help_output: &[u8],
    observed_at: u64,
) -> Result<ProviderCapabilities, ProviderError> {
    let version = ProviderVersion::from_probe_output(version_output)?;
    let help = std::str::from_utf8(help_output)
        .map_err(|_| ProviderError::MalformedVersion(ProviderVersionError::InvalidUtf8))?;
    let build_launch = if help_proves_interactive_terminal(help) {
        CapabilitySupport::Supported
    } else {
        CapabilitySupport::Unknown
    };
    let capabilities = ProviderCapabilities {
        kind: ProviderKind::Cursor,
        version,
        auth_state: ProviderAuthState::Unknown,
        exact_resume: CapabilitySupport::Unsupported,
        semantic_events: CapabilitySupport::Unsupported,
        provider_session_id: CapabilitySupport::Unsupported,
        build_launch,
        parse_signal: CapabilitySupport::Unsupported,
        cooperative_stop: CapabilitySupport::Unsupported,
        observe_quota: CapabilitySupport::Unsupported,
        evidence: vec![
            evidence(
                EvidenceSourceId::ExecutableVersion,
                observed_at,
                EvidenceStatus::Supported,
            )?,
            evidence(
                EvidenceSourceId::CapabilityProbe,
                observed_at,
                if build_launch.is_supported() {
                    EvidenceStatus::Supported
                } else {
                    EvidenceStatus::Unknown
                },
            )?,
        ],
    };
    capabilities.validate()?;
    Ok(capabilities)
}

fn evidence(
    source: EvidenceSourceId,
    observed_at: u64,
    status: EvidenceStatus,
) -> Result<CapabilityEvidence, ProviderError> {
    CapabilityEvidence::new(source, observed_at, status, None).map_err(|error| {
        ProviderError::InvalidCapabilities(ProviderCapabilitiesError::InvalidEvidence(error))
    })
}

fn help_proves_interactive_terminal(help: &str) -> bool {
    normalize_whitespace(help).contains(PINNED_INTERACTIVE_SURFACE)
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn run_text_probe(
    runner: &dyn ProviderProbeRunner,
    executable: &ProviderExecutableHandle,
    request: impl FnOnce(
        ProviderExecutableHandle,
    ) -> Result<ProviderProbeRequest, ProviderProbeRequestError>,
) -> Result<Vec<u8>, ProviderError> {
    let request = request(executable.clone()).map_err(ProviderProbeError::InvalidRequest)?;
    let result = runner
        .run(request)
        .await
        .map_err(|error| map_probe_error(error, executable))?;
    match result.status() {
        ProviderProbeStatus::Completed => Ok(result.stdout().to_vec()),
        ProviderProbeStatus::NonZeroExit => {
            Err(ProviderError::Probe(ProviderProbeError::NonZeroExit(None)))
        }
        ProviderProbeStatus::TimedOut => Err(ProviderError::Probe(ProviderProbeError::TimedOut)),
        ProviderProbeStatus::OutputTooLarge => {
            Err(ProviderError::Probe(ProviderProbeError::OutputTooLarge))
        }
        ProviderProbeStatus::Failed(code) => {
            Err(ProviderError::Probe(ProviderProbeError::Io(match code {
                ProviderProbeFailureCode::ExecutableMissing => {
                    ProviderProbeIoError::ExecutableMissing
                }
                ProviderProbeFailureCode::PermissionDenied => {
                    ProviderProbeIoError::PermissionDenied
                }
                ProviderProbeFailureCode::SpawnFailed => ProviderProbeIoError::SpawnFailed,
                ProviderProbeFailureCode::WaitFailed => ProviderProbeIoError::WaitFailed,
                ProviderProbeFailureCode::DescendantCleanupFailed => {
                    ProviderProbeIoError::DescendantCleanupFailed
                }
            })))
        }
    }
}

fn map_probe_error(
    error: ProviderProbeError,
    executable: &ProviderExecutableHandle,
) -> ProviderError {
    match error {
        ProviderProbeError::Io(ProviderProbeIoError::ExecutableMissing) => {
            ProviderError::MissingCli {
                kind: ProviderKind::Cursor,
                requested: Some(executable.canonical_path().to_path_buf()),
            }
        }
        other => ProviderError::Probe(other),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .filter(|millis| *millis > 0)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ProviderSessionId;
    use crate::providers::adapter::{
        LaunchProviderRequest, ProviderProbeRequest, ProviderProbeResult,
    };
    use crate::providers::capabilities::{ProviderExecutable, ProviderExecutableHandle};
    use std::path::{Path, PathBuf};

    const PINNED_VERSION: &[u8] =
        include_bytes!("../../tests/fixtures/providers/cursor/version.txt");
    const PINNED_HELP: &[u8] = include_bytes!("../../tests/fixtures/providers/cursor/help.txt");
    const OBSERVED_AT: u64 = 1_700_000_000_400;

    fn pinned_adapter() -> CursorAdapter {
        CursorAdapter::from_pinned_probes(PINNED_VERSION, PINNED_HELP, OBSERVED_AT)
    }

    fn cursor_executable() -> ProviderExecutable {
        ProviderExecutable::new(PathBuf::from("C:/bin/cursor-agent"), [0x44; 32]).unwrap()
    }

    fn cursor_handle() -> ProviderExecutableHandle {
        cursor_executable().open_for_launch().unwrap()
    }

    #[tokio::test]
    async fn cursor_probe_from_pinned_fixtures_keeps_interactive_terminal_first_class() {
        let adapter = pinned_adapter();
        assert_eq!(adapter.kind(), ProviderKind::Cursor);

        let executable = cursor_handle();
        let capabilities = adapter.probe(&executable).await.unwrap();

        assert_eq!(capabilities.kind, ProviderKind::Cursor);
        assert_eq!(capabilities.version.as_str(), "2026.08.09-docs-pinned");
        assert_eq!(capabilities.build_launch, CapabilitySupport::Supported);
        assert_eq!(capabilities.exact_resume, CapabilitySupport::Unsupported);
        assert_eq!(capabilities.semantic_events, CapabilitySupport::Unsupported);
        assert_eq!(
            capabilities.provider_session_id,
            CapabilitySupport::Unsupported
        );
        assert_eq!(capabilities.parse_signal, CapabilitySupport::Unsupported);
        assert_eq!(
            capabilities.cooperative_stop,
            CapabilitySupport::Unsupported
        );
        assert_eq!(capabilities.observe_quota, CapabilitySupport::Unsupported);
        assert!(matches!(
            adapter.observe_quota(&executable).await,
            Err(ProviderError::UnsupportedCapability(
                ProviderCapability::ObserveQuota
            ))
        ));
        assert_eq!(capabilities.auth_state, ProviderAuthState::Unknown);
        assert!(capabilities
            .evidence
            .iter()
            .all(|evidence| evidence.source() != EvidenceSourceId::AuthStatusProbe));
        capabilities.validate().unwrap();
    }

    #[test]
    fn cursor_build_launch_requires_supported_observation() {
        let adapter = pinned_adapter();
        let before_probe =
            adapter.build_launch(LaunchProviderRequest::new(cursor_handle(), None, None));
        assert!(matches!(
            before_probe,
            Err(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch
            ))
        ));
    }

    #[tokio::test]
    async fn cursor_build_launch_is_bare_interactive_and_resume_is_typed_failure() {
        let adapter = pinned_adapter();
        let executable = cursor_handle();
        let probe_executable = cursor_handle();
        adapter.probe(&probe_executable).await.unwrap();

        let fresh = adapter
            .build_launch(LaunchProviderRequest::new(executable.clone(), None, None))
            .unwrap();
        assert_eq!(
            fresh.executable().canonical_path(),
            Path::new("C:/bin/cursor-agent")
        );
        assert_eq!(fresh.arguments().count(), 0);

        let session = ProviderSessionId::new("chat-id-must-not-be-inferred").unwrap();
        let resume =
            adapter.build_launch(LaunchProviderRequest::new(executable, None, Some(session)));
        assert!(matches!(
            resume,
            Err(ProviderError::UnsupportedCapability(
                ProviderCapability::ExactResume
            ))
        ));
    }

    #[tokio::test]
    async fn cursor_non_interactive_help_does_not_prove_terminal_launch() {
        let adapter = CursorAdapter::from_pinned_probes(
            PINNED_VERSION,
            b"Usage: agent --print\nPrint responses (non-interactive; not a TTY session)\n",
            OBSERVED_AT,
        );
        let executable = cursor_handle();
        let capabilities = adapter.probe(&executable).await.unwrap();
        assert_eq!(capabilities.build_launch, CapabilitySupport::Unknown);
        assert_eq!(capabilities.exact_resume, CapabilitySupport::Unsupported);
        assert_eq!(capabilities.auth_state, ProviderAuthState::Unknown);
        assert!(matches!(
            adapter.build_launch(LaunchProviderRequest::new(cursor_handle(), None, None)),
            Err(ProviderError::UnsupportedCapability(
                ProviderCapability::BuildLaunch
            ))
        ));
    }

    #[tokio::test]
    async fn cursor_loose_help_words_do_not_prove_interactive_launch() {
        let adapter = CursorAdapter::from_pinned_probes(
            PINNED_VERSION,
            b"agent session interactive words without the pinned default surface\n",
            OBSERVED_AT,
        );
        let executable = cursor_handle();
        let capabilities = adapter.probe(&executable).await.unwrap();
        assert_eq!(capabilities.build_launch, CapabilitySupport::Unknown);
    }

    struct NonZeroProbeRunner;

    #[async_trait]
    impl ProviderProbeRunner for NonZeroProbeRunner {
        async fn run(
            &self,
            request: ProviderProbeRequest,
        ) -> Result<ProviderProbeResult, ProviderProbeError> {
            ProviderProbeResult::completed(&request, 1, 0, 0)
        }
    }

    #[tokio::test]
    async fn cursor_nonzero_probe_status_is_rejected() {
        let adapter = CursorAdapter::from_test_runner(Arc::new(NonZeroProbeRunner));
        let executable = cursor_handle();
        let result = adapter.probe(&executable).await;
        assert!(matches!(
            result,
            Err(ProviderError::Probe(ProviderProbeError::NonZeroExit(_)))
        ));
    }

    struct OversizeProbeRunner;

    #[async_trait]
    impl ProviderProbeRunner for OversizeProbeRunner {
        async fn run(
            &self,
            _request: ProviderProbeRequest,
        ) -> Result<ProviderProbeResult, ProviderProbeError> {
            Err(ProviderProbeError::OutputTooLarge)
        }
    }

    #[tokio::test]
    async fn cursor_oversize_probe_is_rejected() {
        let adapter = CursorAdapter::from_test_runner(Arc::new(OversizeProbeRunner));
        let executable = cursor_handle();
        let result = adapter.probe(&executable).await;
        assert!(matches!(
            result,
            Err(ProviderError::Probe(ProviderProbeError::OutputTooLarge))
        ));
    }

    #[tokio::test]
    async fn cursor_lossy_help_utf8_is_rejected() {
        let adapter =
            CursorAdapter::from_pinned_probes(PINNED_VERSION, b"\xff\xfe not utf-8", OBSERVED_AT);
        let executable = cursor_handle();
        let result = adapter.probe(&executable).await;
        assert!(matches!(
            result,
            Err(ProviderError::MalformedVersion(
                ProviderVersionError::InvalidUtf8
            ))
        ));
    }
}
