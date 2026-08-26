//! Production stock-subscription provider session controller.
//!
//! Host dispatchers start Claude Code, Codex, and Cursor through one
//! provider-owned method: resolve the adapter launch proof, seal the runtime
//! request, and hand it to [`ProviderSessionManager`] / [`ProviderProcessLauncher`].
//! Subscription evidence must already be present on the observation (registry
//! auth receipt). Unknown, API-key, and auth-required snapshots fail closed
//! and never open a fresh conversation.

use crate::domain::{AgentResourceBinding, AgentSessionFacts, ProviderSessionId};
use crate::providers::adapter::{ProviderAdapter, ProviderError, ProviderInput};
use crate::providers::capabilities::{ProviderAuthState, ProviderKind};
use crate::providers::registry::ProviderObservation;
use crate::providers::session::{
    ExactResumeFailure, ProviderLaunchError, ProviderLaunchMode, ProviderProcessLauncher,
    ProviderRuntime, ProviderSessionError, ProviderSessionManager, ProviderSessionStartMode,
    ProviderSessionStateStore,
};
use crate::providers::startup::start_request_from_adapter_with_options;
use crate::providers::startup::{start_request_from_adapter, ProviderBridgeError};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// Provider-neutral production start seam for the three stock CLIs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StockProviderSessionController;

impl StockProviderSessionController {
    pub const fn new() -> Self {
        Self
    }

    /// Resolve adapter launch proof, create the sealed start request, and start
    /// the runtime through the injected [`ProviderSessionManager`].
    ///
    /// Exact resume stays typed: a capability, SessionStart, auth, or adapter
    /// failure is returned and is never retried as a new conversation.
    pub fn start<L, S>(
        &self,
        manager: &mut ProviderSessionManager<L, S>,
        agent: AgentSessionFacts,
        observation: &ProviderObservation,
        adapter: &dyn ProviderAdapter,
        input: Option<ProviderInput>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        mode: ProviderSessionStartMode,
    ) -> Result<ProviderRuntime, StockProviderSessionError>
    where
        L: ProviderProcessLauncher,
        S: ProviderSessionStateStore,
    {
        reject_cursor_exact_resume(observation.kind(), mode, agent.provider_session_id.as_ref())?;
        reject_unauthenticated_observation(observation, mode, agent.provider_session_id.as_ref())?;
        let request =
            start_request_from_adapter(agent, observation, adapter, input, cwd, environment, mode)?;
        manager
            .start(request)
            .map_err(StockProviderSessionError::from)
    }

    /// Start through the exact durable task/resource binding. This is the
    /// production path for a task-owned native terminal; it cannot allocate a
    /// replacement resource or silently advance to a different generation.
    pub fn start_with_resource_binding<L, S>(
        &self,
        manager: &mut ProviderSessionManager<L, S>,
        binding: AgentResourceBinding,
        agent: AgentSessionFacts,
        observation: &ProviderObservation,
        adapter: &dyn ProviderAdapter,
        input: Option<ProviderInput>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        mode: ProviderSessionStartMode,
    ) -> Result<ProviderRuntime, StockProviderSessionError>
    where
        L: ProviderProcessLauncher,
        S: ProviderSessionStateStore,
    {
        self.start_with_resource_binding_options(
            manager,
            binding,
            agent,
            observation,
            adapter,
            input,
            cwd,
            environment,
            mode,
            crate::providers::adapter::ProviderLaunchOptions::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_with_resource_binding_options<L, S>(
        &self,
        manager: &mut ProviderSessionManager<L, S>,
        binding: AgentResourceBinding,
        agent: AgentSessionFacts,
        observation: &ProviderObservation,
        adapter: &dyn ProviderAdapter,
        input: Option<ProviderInput>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        mode: ProviderSessionStartMode,
        launch_options: crate::providers::adapter::ProviderLaunchOptions,
    ) -> Result<ProviderRuntime, StockProviderSessionError>
    where
        L: ProviderProcessLauncher,
        S: ProviderSessionStateStore,
    {
        reject_cursor_exact_resume(observation.kind(), mode, agent.provider_session_id.as_ref())?;
        reject_unauthenticated_observation(observation, mode, agent.provider_session_id.as_ref())?;
        let request = start_request_from_adapter_with_options(
            agent,
            observation,
            adapter,
            input,
            cwd,
            environment,
            mode,
            launch_options,
        )?;
        manager
            .start_with_resource_binding(request, binding)
            .map_err(StockProviderSessionError::from)
    }
}

/// Fail closed when the observation does not carry a subscription receipt.
pub(crate) fn reject_unauthenticated_observation(
    observation: &ProviderObservation,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<&ProviderSessionId>,
) -> Result<(), StockProviderSessionError> {
    if let Some(error) = unauthenticated_stock_session_error(
        observation.kind(),
        observation.capabilities().auth_state(),
        mode,
        provider_session_id,
    ) {
        return Err(error);
    }
    Ok(())
}

pub(crate) fn unauthenticated_stock_launch_error(
    kind: ProviderKind,
    auth: ProviderAuthState,
    mode: &ProviderLaunchMode,
) -> Option<ProviderLaunchError> {
    if auth == ProviderAuthState::AuthenticatedSubscription {
        return match (kind, mode) {
            (ProviderKind::Cursor, ProviderLaunchMode::ResumeExact(_)) => {
                Some(ProviderLaunchError::Unsupported)
            }
            _ => None,
        };
    }
    match mode {
        ProviderLaunchMode::ResumeExact(_) if kind == ProviderKind::Cursor => {
            Some(ProviderLaunchError::Unsupported)
        }
        ProviderLaunchMode::ResumeExact(_) => Some(ProviderLaunchError::ExactResumeFailed(
            ExactResumeFailure::AuthRequired,
        )),
        ProviderLaunchMode::NewConversation => Some(ProviderLaunchError::AuthenticationRequired),
    }
}

fn reject_cursor_exact_resume(
    kind: ProviderKind,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<&ProviderSessionId>,
) -> Result<(), StockProviderSessionError> {
    if kind != ProviderKind::Cursor {
        return Ok(());
    }
    match mode {
        ProviderSessionStartMode::ResumeExact => Err(StockProviderSessionError::Session(
            ProviderSessionError::ExactResumeUnavailable { provider: kind },
        )),
        ProviderSessionStartMode::Open if provider_session_id.is_some() => {
            Err(StockProviderSessionError::Session(
                ProviderSessionError::ExactResumeUnavailable { provider: kind },
            ))
        }
        ProviderSessionStartMode::Open | ProviderSessionStartMode::NewConversation => Ok(()),
    }
}

fn unauthenticated_stock_session_error(
    kind: ProviderKind,
    auth: ProviderAuthState,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<&ProviderSessionId>,
) -> Option<StockProviderSessionError> {
    if auth == ProviderAuthState::AuthenticatedSubscription {
        return None;
    }
    Some(match mode {
        ProviderSessionStartMode::ResumeExact => exact_resume_auth_error(kind, provider_session_id),
        ProviderSessionStartMode::Open if provider_session_id.is_some() => {
            exact_resume_auth_error(kind, provider_session_id)
        }
        ProviderSessionStartMode::Open | ProviderSessionStartMode::NewConversation => {
            StockProviderSessionError::Session(ProviderSessionError::LaunchFailed(
                ProviderLaunchError::AuthenticationRequired,
            ))
        }
    })
}

fn exact_resume_auth_error(
    kind: ProviderKind,
    provider_session_id: Option<&ProviderSessionId>,
) -> StockProviderSessionError {
    if kind == ProviderKind::Cursor {
        return StockProviderSessionError::Session(ProviderSessionError::ExactResumeUnavailable {
            provider: kind,
        });
    }
    match provider_session_id {
        Some(provider_session_id) => {
            StockProviderSessionError::Session(ProviderSessionError::ExactResumeFailed {
                provider_session_id: provider_session_id.clone(),
                failure: ExactResumeFailure::AuthRequired,
            })
        }
        None => StockProviderSessionError::Session(ProviderSessionError::LaunchFailed(
            ProviderLaunchError::ExactResumeFailed(ExactResumeFailure::AuthRequired),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StockProviderSessionError {
    Adapter(ProviderError),
    LaunchSpec(crate::providers::session::ProviderLaunchSpecError),
    Session(ProviderSessionError),
}

impl From<ProviderBridgeError> for StockProviderSessionError {
    fn from(error: ProviderBridgeError) -> Self {
        match error {
            ProviderBridgeError::Adapter(error) => Self::Adapter(error),
            ProviderBridgeError::LaunchSpec(error) => Self::LaunchSpec(error),
        }
    }
}

impl From<ProviderSessionError> for StockProviderSessionError {
    fn from(error: ProviderSessionError) -> Self {
        Self::Session(error)
    }
}

impl fmt::Display for StockProviderSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(error) => write!(formatter, "{error}"),
            Self::LaunchSpec(error) => write!(formatter, "{error:?}"),
            Self::Session(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for StockProviderSessionError {}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
