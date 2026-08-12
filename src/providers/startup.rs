//! Provider-owned stock adapter factory and launch-proof bridge.
//!
//! This module registers one adapter per [`ProviderKind`] and seals
//! observation + adapter `build_launch` into durable session start proofs.
//! It is **not** invoked by app/native_shell/process_manager today; hosts must
//! call these seams explicitly. Until that host wiring exists, treat this as a
//! provider-owned construction boundary, not live production startup.

use crate::domain::{AgentSessionFacts, ProviderSessionId};
use crate::providers::adapter::{
    LaunchProviderRequest, ProviderAdapter, ProviderError, ProviderInput,
    ProviderLaunchSpec as AdapterLaunchSpec, WindowsProviderProbeRunner,
};
use crate::providers::capabilities::{ProviderExecutablePolicy, ProviderKind};
use crate::providers::claude::ClaudeCodeAdapter;
use crate::providers::codex::CodexAdapter;
use crate::providers::cursor::CursorAdapter;
use crate::providers::registry::{ProviderObservation, ProviderRegistry};
use crate::providers::session::{
    ProviderAdapterLaunchProof, ProviderAdapterLaunchSpec, ProviderLaunchSpecError,
    ProviderSessionStartMode, StartProviderSessionRequest,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// Deterministic stock adapter registration order (matches [`ProviderKind`] ord).
pub const STOCK_PROVIDER_REGISTRATION_ORDER: [ProviderKind; 3] = [
    ProviderKind::ClaudeCode,
    ProviderKind::Codex,
    ProviderKind::Cursor,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderBridgeError {
    Adapter(ProviderError),
    LaunchSpec(ProviderLaunchSpecError),
}

impl From<ProviderError> for ProviderBridgeError {
    fn from(error: ProviderError) -> Self {
        Self::Adapter(error)
    }
}

impl From<ProviderLaunchSpecError> for ProviderBridgeError {
    fn from(error: ProviderLaunchSpecError) -> Self {
        Self::LaunchSpec(error)
    }
}

/// Build an empty registry and register the three stock adapters exactly once.
///
/// Host/session wiring must call this; it is not auto-invoked by app startup.
pub fn stock_provider_registry() -> Result<ProviderRegistry, ProviderError> {
    let mut registry = ProviderRegistry::new();
    register_stock_adapters(&mut registry)?;
    Ok(registry)
}

/// Register Claude, Codex, then Cursor. Rejects duplicates via the registry.
pub fn register_stock_adapters(registry: &mut ProviderRegistry) -> Result<(), ProviderError> {
    registry.register(Arc::new(ClaudeCodeAdapter::new()) as Arc<dyn ProviderAdapter>)?;
    registry.register(Arc::new(CodexAdapter::new(Arc::new(
        WindowsProviderProbeRunner::new(
            ProviderExecutablePolicy::new(["codex"]).expect("codex entrypoint"),
        ),
    ))) as Arc<dyn ProviderAdapter>)?;
    registry.register(Arc::new(CursorAdapter::new()) as Arc<dyn ProviderAdapter>)?;
    Ok(())
}

/// Registered kinds in deterministic ascending order.
pub fn registered_stock_kinds(registry: &ProviderRegistry) -> Vec<ProviderKind> {
    registry.registered_kinds()
}

/// Seal only an adapter-produced launch spec that already matches the
/// observation handle. Callers must obtain `adapter_launch` from
/// [`ProviderAdapter::build_launch`], not synthesize args/paths.
pub(crate) fn prove_adapter_launch(
    observation: &ProviderObservation,
    adapter_launch: &AdapterLaunchSpec,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<ProviderSessionId>,
) -> Result<(ProviderAdapterLaunchSpec, ProviderAdapterLaunchProof), ProviderLaunchSpecError> {
    observation
        .validate()
        .map_err(|_| ProviderLaunchSpecError::InvalidCapabilities)?;
    if adapter_launch.executable() != observation.executable_handle() {
        return Err(ProviderLaunchSpecError::InvalidCapabilities);
    }
    match mode {
        ProviderSessionStartMode::NewConversation if provider_session_id.is_some() => {
            return Err(ProviderLaunchSpecError::ResumeIntentMismatch);
        }
        ProviderSessionStartMode::ResumeExact if provider_session_id.is_none() => {
            return Err(ProviderLaunchSpecError::ResumeIntentMismatch);
        }
        ProviderSessionStartMode::ResumeExact
            if !observation.capabilities().exact_resume.is_supported() =>
        {
            return Err(ProviderLaunchSpecError::ResumeIntentMismatch);
        }
        _ => {}
    }
    let arguments = adapter_launch
        .arguments()
        .map(OsString::from)
        .collect::<Vec<_>>();
    let spec = ProviderAdapterLaunchSpec::from_registry(
        observation.executable().clone(),
        arguments,
        cwd,
        environment,
        observation.capabilities().clone(),
    )?;
    let proof = ProviderAdapterLaunchProof::from_registry(spec.clone(), mode, provider_session_id)?;
    Ok((spec, proof))
}

/// Production provider-owned start seam: calls `adapter.build_launch` against
/// the registry-issued executable handle. Callers cannot supply raw launch args
/// or bypass exact-resume capability/visibility checks.
pub fn start_request_from_adapter(
    agent: AgentSessionFacts,
    observation: &ProviderObservation,
    adapter: &dyn ProviderAdapter,
    input: Option<ProviderInput>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    mode: ProviderSessionStartMode,
) -> Result<StartProviderSessionRequest, ProviderBridgeError> {
    if agent.provider_kind != observation.kind() || adapter.kind() != observation.kind() {
        return Err(ProviderLaunchSpecError::InvalidCapabilities.into());
    }
    let provider_session_id = match mode {
        ProviderSessionStartMode::NewConversation => None,
        ProviderSessionStartMode::ResumeExact => {
            let Some(session_id) = agent.provider_session_id.clone() else {
                return Err(ProviderLaunchSpecError::ResumeIntentMismatch.into());
            };
            Some(session_id)
        }
        ProviderSessionStartMode::Open => agent.provider_session_id.clone(),
    };
    let request = LaunchProviderRequest::new(
        observation.executable_handle().clone(),
        input,
        provider_session_id.clone(),
    );
    // Exact resume unsupported/mismatch fails here visibly with no fresh fallback.
    let adapter_launch = adapter.build_launch(request)?;
    let (_spec, proof) = prove_adapter_launch(
        observation,
        &adapter_launch,
        cwd,
        environment,
        mode,
        provider_session_id,
    )?;
    Ok(StartProviderSessionRequest::from_registry(
        agent, proof, mode,
    ))
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
