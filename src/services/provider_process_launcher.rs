//! Production [`ProviderProcessLauncher`] backed by ProcessManager Job/PTY ownership.

use crate::providers::input::{
    ProviderInputDeliveryError, ProviderInputDeliveryIdentity, ProviderRuntimeByteWriter,
    ProviderRuntimeWriteHandle,
};
use crate::providers::session::{
    sealed, ActiveProcessZeroSettlement, ProviderLaunchError, ProviderLaunchOutcome,
    ProviderProcessLauncher, ProviderProcessLease, ProviderRecoveryHandoffFailure,
    ProviderRuntimeLaunchRequest, ProviderSessionState,
};
use crate::services::process_manager::ProcessManager;
use uuid::Uuid;

/// One ProcessManager-owned launcher for exact stock Claude/Codex CLI runtimes.
#[derive(Clone)]
pub struct ProcessManagerProviderLauncher {
    manager: ProcessManager,
}

impl ProcessManagerProviderLauncher {
    pub(crate) fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }

    pub fn write_handle(
        &self,
        identity: ProviderInputDeliveryIdentity,
        lease: &ProviderProcessLease,
    ) -> Result<ProviderRuntimeWriteHandle, ProviderInputDeliveryError> {
        ProviderRuntimeWriteHandle::bind(
            identity,
            lease.fence().clone(),
            Box::new(ProcessManagerByteWriter {
                manager: self.manager.clone(),
            }),
        )
    }

    /// Bind a write capability to the currently live stock runtime. The
    /// identity is looked up by the ProcessManager-owned session authority;
    /// callers cannot manufacture a fence from durable metadata alone.
    pub(crate) fn write_handle_for_identity(
        &self,
        identity: &ProviderInputDeliveryIdentity,
    ) -> Result<ProviderRuntimeWriteHandle, ProviderInputDeliveryError> {
        let fence = self.manager.live_provider_write_fence(identity)?;
        ProviderRuntimeWriteHandle::bind(
            identity.clone(),
            fence,
            Box::new(ProcessManagerByteWriter {
                manager: self.manager.clone(),
            }),
        )
    }
}

struct ProcessManagerByteWriter {
    manager: ProcessManager,
}

impl ProviderRuntimeByteWriter for ProcessManagerByteWriter {
    fn write_exact(
        &self,
        fence: &crate::process::registry::ManagedProcessFence,
        identity: &ProviderInputDeliveryIdentity,
        bytes: &[u8],
    ) -> Result<(), ProviderInputDeliveryError> {
        self.manager
            .write_sealed_provider_bytes(fence, identity, bytes)
    }
}

impl sealed::ProviderProcessLauncher for ProcessManagerProviderLauncher {}

impl ProviderProcessLauncher for ProcessManagerProviderLauncher {
    fn launch(&mut self, request: &ProviderRuntimeLaunchRequest) -> ProviderLaunchOutcome {
        match self.manager.launch_sealed_provider_runtime(request) {
            Ok(lease) => ProviderLaunchOutcome::Started(lease),
            Err(error) => ProviderLaunchOutcome::Rejected(error),
        }
    }

    fn stop_and_join(
        &mut self,
        lease: &mut ProviderProcessLease,
    ) -> Result<ActiveProcessZeroSettlement, ProviderLaunchError> {
        self.manager.stop_sealed_provider_runtime(lease)
    }

    fn observe_root_exit(
        &mut self,
        lease: &ProviderProcessLease,
    ) -> Result<Option<ActiveProcessZeroSettlement>, ProviderLaunchError> {
        self.manager.observe_sealed_provider_zero(lease)
    }

    fn retain_for_recovery(
        &mut self,
        state: &ProviderSessionState,
        lease: ProviderProcessLease,
    ) -> Result<(), ProviderRecoveryHandoffFailure> {
        self.manager.retain_sealed_provider_runtime(state, lease)
    }

    fn retain_for_recovery_with_receipt(
        &mut self,
        state: &ProviderSessionState,
        lease: ProviderProcessLease,
        handoff_receipt: Uuid,
    ) -> Result<Uuid, ProviderRecoveryHandoffFailure> {
        self.manager.retain_sealed_provider_runtime(state, lease)?;
        Ok(handoff_receipt)
    }

    fn recover(
        &mut self,
        state: &ProviderSessionState,
    ) -> Result<Option<ProviderProcessLease>, ProviderLaunchError> {
        self.manager.recover_sealed_provider_runtime(state)
    }
}

impl std::fmt::Debug for ProcessManagerProviderLauncher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessManagerProviderLauncher")
            .field("process_manager", &true)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AgentRole, AgentSessionFacts, AgentSessionId, AgentSessionLifecycle, ProviderKind,
        ProviderSessionId, ResourceId, TaskId, TerminalId,
    };
    use crate::process::identity::ProcessOwner;
    use crate::providers::capabilities::{
        CapabilitySupport, ProviderAuthState, ProviderCapabilities, ProviderExecutable,
        ProviderVersion,
    };
    use crate::providers::session::{
        LaunchNonce, ProviderLaunchMode, ProviderLaunchSpec, ProviderRuntimeLaunchRequest,
        RuntimeCorrelation,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn test_capabilities(
        kind: ProviderKind,
        exact_resume: CapabilitySupport,
    ) -> ProviderCapabilities {
        test_capabilities_with_auth(
            kind,
            exact_resume,
            ProviderAuthState::AuthenticatedSubscription,
        )
    }

    fn test_capabilities_with_auth(
        kind: ProviderKind,
        exact_resume: CapabilitySupport,
        auth_state: ProviderAuthState,
    ) -> ProviderCapabilities {
        ProviderCapabilities {
            kind,
            version: ProviderVersion::new("1.0.0-test").expect("version"),
            auth_state,
            exact_resume,
            semantic_events: CapabilitySupport::Unsupported,
            provider_session_id: exact_resume,
            build_launch: CapabilitySupport::Supported,
            parse_signal: CapabilitySupport::Unsupported,
            cooperative_stop: CapabilitySupport::Unsupported,
            observe_quota: CapabilitySupport::Unsupported,
            evidence: vec![],
        }
    }

    fn launch_request(
        kind: ProviderKind,
        exact_resume: CapabilitySupport,
        mode: ProviderLaunchMode,
        arguments: Vec<OsString>,
    ) -> ProviderRuntimeLaunchRequest {
        let executable = ProviderExecutable::from_path(if cfg!(windows) {
            PathBuf::from(r"C:\Windows\System32\cmd.exe")
        } else {
            std::env::current_exe().expect("exe")
        })
        .expect("identity");
        let generation = 3;
        let task_id = TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task");
        let resource_id =
            ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").expect("resource");
        let agent_session_id =
            AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent");
        let launch_nonce = LaunchNonce::new();
        let launch_spec = ProviderLaunchSpec::sealed(
            kind,
            executable,
            mode,
            arguments,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            BTreeMap::new(),
            test_capabilities(kind, exact_resume),
            task_id,
            resource_id,
            TerminalId::new(),
            generation,
            launch_nonce,
        );
        ProviderRuntimeLaunchRequest::sealed(
            RuntimeCorrelation::sealed(
                task_id,
                agent_session_id,
                kind,
                generation,
                4,
                launch_nonce,
            ),
            launch_spec,
        )
    }

    fn launch_request_with_auth(
        kind: ProviderKind,
        exact_resume: CapabilitySupport,
        auth_state: ProviderAuthState,
        mode: ProviderLaunchMode,
        arguments: Vec<OsString>,
    ) -> ProviderRuntimeLaunchRequest {
        let executable = ProviderExecutable::from_path(if cfg!(windows) {
            PathBuf::from(r"C:\Windows\System32\cmd.exe")
        } else {
            std::env::current_exe().expect("exe")
        })
        .expect("identity");
        let generation = 3;
        let task_id = TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task");
        let resource_id =
            ResourceId::parse("018f60b0-9c1a-7001-8000-000000000057").expect("resource");
        let agent_session_id =
            AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent");
        let launch_nonce = LaunchNonce::new();
        let launch_spec = ProviderLaunchSpec::sealed(
            kind,
            executable,
            mode,
            arguments,
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            BTreeMap::new(),
            test_capabilities_with_auth(kind, exact_resume, auth_state),
            task_id,
            resource_id,
            TerminalId::new(),
            generation,
            launch_nonce,
        );
        ProviderRuntimeLaunchRequest::sealed(
            RuntimeCorrelation::sealed(
                task_id,
                agent_session_id,
                kind,
                generation,
                4,
                launch_nonce,
            ),
            launch_spec,
        )
    }

    #[cfg(windows)]
    #[test]
    fn production_launcher_issues_registry_permit_with_nonzero_fence() {
        let manager = ProcessManager::new();
        let mut launcher = manager.provider_process_launcher();
        let request = launch_request(
            ProviderKind::Codex,
            CapabilitySupport::Supported,
            ProviderLaunchMode::NewConversation,
            Vec::new(),
        );
        let ProviderLaunchOutcome::Started(mut lease) = launcher.launch(&request) else {
            panic!("expected registry-issued permit");
        };
        assert_ne!(lease.process_id().pid(), 0);
        assert_ne!(lease.process_id().creation_time_100ns(), 0);
        assert_eq!(
            lease.fence().resource().runtime_generation,
            request.launch_spec().generation()
        );
        assert_eq!(
            lease.fence().owner(),
            ProcessOwner::Task(request.launch_spec().task_id())
        );
        assert_eq!(
            lease.fence().root().canonical_executable(),
            request.launch_spec().executable().canonical_path()
        );
        let proof = launcher
            .stop_and_join(&mut lease)
            .expect("exact ACTIVE_PROCESS_ZERO");
        assert!(proof.matches_permit(&lease));
    }

    #[test]
    fn cursor_and_exact_resume_failure_do_not_open_fresh() {
        let manager = ProcessManager::new();
        let mut launcher = manager.provider_process_launcher();
        let cursor = launch_request(
            ProviderKind::Cursor,
            CapabilitySupport::Unsupported,
            ProviderLaunchMode::ResumeExact(
                ProviderSessionId::new("cursor-session").expect("session"),
            ),
            vec![OsString::from("--resume"), OsString::from("cursor-session")],
        );
        assert!(matches!(
            launcher.launch(&cursor),
            ProviderLaunchOutcome::Rejected(ProviderLaunchError::Unsupported)
        ));

        let missing = launch_request(
            ProviderKind::ClaudeCode,
            CapabilitySupport::Supported,
            ProviderLaunchMode::ResumeExact(
                ProviderSessionId::new("missing-session").expect("session"),
            ),
            vec![
                OsString::from("--resume"),
                OsString::from("missing-session"),
                OsString::from("--definitely-not-a-real-flag"),
            ],
        );
        match launcher.launch(&missing) {
            ProviderLaunchOutcome::Rejected(ProviderLaunchError::ExactResumeFailed(_))
            | ProviderLaunchOutcome::Rejected(ProviderLaunchError::SpawnFailed)
            | ProviderLaunchOutcome::Started(_) => {}
            other => panic!("exact resume must not invent a fresh conversation: {other:?}"),
        }
        let _facts = AgentSessionFacts {
            id: AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000021").expect("agent"),
            task_id: TaskId::parse("018f60b0-9c1a-7001-8000-00000000000b").expect("task"),
            role: AgentRole::Primary,
            provider_kind: ProviderKind::ClaudeCode,
            provider_session_id: Some(ProviderSessionId::new("missing-session").expect("session")),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 3,
            revision: 0,
        };
    }

    #[test]
    fn stock_claude_and_codex_launches_require_subscription_auth() {
        let manager = ProcessManager::new();
        let mut launcher = manager.provider_process_launcher();
        for kind in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            let fresh = launch_request_with_auth(
                kind,
                CapabilitySupport::Supported,
                ProviderAuthState::Unknown,
                ProviderLaunchMode::NewConversation,
                Vec::new(),
            );
            assert!(matches!(
                launcher.launch(&fresh),
                ProviderLaunchOutcome::Rejected(ProviderLaunchError::AuthenticationRequired)
            ));

            let exact = launch_request_with_auth(
                kind,
                CapabilitySupport::Supported,
                ProviderAuthState::Unknown,
                ProviderLaunchMode::ResumeExact(
                    ProviderSessionId::new("exact-session").expect("session"),
                ),
                vec![OsString::from("--resume"), OsString::from("exact-session")],
            );
            assert!(matches!(
                launcher.launch(&exact),
                ProviderLaunchOutcome::Rejected(ProviderLaunchError::ExactResumeFailed(
                    crate::providers::session::ExactResumeFailure::AuthRequired
                ))
            ));
        }
    }
}
