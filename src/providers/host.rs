//! Production host facade for stock provider registration and launch admission.
//!
//! ProcessManager owns one instance created at startup. That registers stock
//! Claude/Codex/Cursor adapters, admits exact-resume identity, and supplies
//! the Job/PTY [`ProviderProcessLauncher`] used by [`ProviderSessionManager`].
//! Exact Cursor resume stays unsupported. Input settlement requires a live
//! managed-session write receipt; specialist cancel/write/result use that
//! same exact-fenced process and journal lineage.

use crate::domain::command::SpecialistResult;
use crate::domain::{
    AgentRole, AgentSessionFacts, AgentSessionId, EventId, ProviderSessionId, ResourceFence, TaskId,
};
use crate::process::registry::ManagedProcessFence;
use crate::providers::adapter::{ProviderAdapter, ProviderError, ProviderInput};
use crate::providers::capabilities::ProviderKind;
use crate::providers::input::{
    deliver_through_capability, BoundProviderInputPort, ProviderInputBridgeHold,
    ProviderInputDeliveryError, ProviderInputDeliveryIdentity, ProviderInputDeliveryPlan,
    ProviderInputWriteReceipt, ProviderRuntimeWriteHandle,
};
use crate::providers::journal::JournalEvent;
use crate::providers::orchestrator::{
    ensure_single_primary, specialist_cancel_hold, specialist_native_child_hold,
    specialist_structured_result_hold, specialist_write_hold, validate_specialist_result,
    OrchestrationHold,
};
use crate::providers::registry::{ProviderObservation, ProviderRegistry};
use crate::providers::session::{
    ProviderProcessLauncher, ProviderProcessLease, ProviderSessionManager,
    ProviderSessionStartMode, ProviderSessionStateStore, StartProviderSessionRequest,
    UnavailableProviderProcessLauncher,
};
use crate::providers::startup::{
    register_stock_adapters, start_request_from_adapter, stock_provider_registry,
    ProviderBridgeError, STOCK_PROVIDER_REGISTRATION_ORDER,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// One stock registry plus host admission seams. Not a live session runtime.
pub struct ProviderHost {
    registry: ProviderRegistry,
}

impl fmt::Debug for ProviderHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHost")
            .field("registered_kinds", &self.registry.registered_kinds())
            .finish()
    }
}

impl ProviderHost {
    /// Register Claude, Codex, then Cursor exactly once.
    pub fn stock() -> Result<Self, ProviderError> {
        Ok(Self {
            registry: stock_provider_registry()?,
        })
    }

    pub fn from_registry(registry: ProviderRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    pub fn adapter(&self, kind: ProviderKind) -> Option<Arc<dyn ProviderAdapter>> {
        self.registry.adapter(kind)
    }

    pub fn registered_kinds(&self) -> Vec<ProviderKind> {
        self.registry.registered_kinds()
    }

    pub fn is_registered(&self, kind: ProviderKind) -> bool {
        self.registry.is_registered(kind)
    }

    /// Production AI tab admission: registered adapter required; exact resume
    /// uses the official providerSessionId only and fails visibly for Cursor.
    /// This does not launch via ProviderSessionManager.
    pub fn admit_production_ai_session(
        &self,
        kind: ProviderKind,
        provider_session_id: Option<&str>,
    ) -> Result<HostAiLaunchAdmission, HostLaunchError> {
        let adapter = self
            .adapter(kind)
            .ok_or(HostLaunchError::ProviderNotRegistered(kind))?;
        if adapter.kind() != kind {
            return Err(HostLaunchError::ProviderNotRegistered(kind));
        }
        match provider_session_id {
            None => Ok(HostAiLaunchAdmission {
                kind,
                mode: ProviderSessionStartMode::NewConversation,
                provider_session_id: None,
            }),
            Some(raw) => {
                let provider_session_id = ProviderSessionId::new(raw.to_owned())
                    .map_err(|_| HostLaunchError::InvalidProviderSessionId)?;
                if kind == ProviderKind::Cursor {
                    return Err(HostLaunchError::ExactResumeUnsupported(kind));
                }
                Ok(HostAiLaunchAdmission {
                    kind,
                    mode: ProviderSessionStartMode::ResumeExact,
                    provider_session_id: Some(provider_session_id),
                })
            }
        }
    }

    /// Seal a start request through the registered adapter `build_launch`.
    /// Exact resume never falls back to a fresh conversation. ProcessManager
    /// supplies the production Job/PTY launcher for [`ProviderSessionManager`].
    pub fn start_request_from_registered_adapter(
        &self,
        agent: AgentSessionFacts,
        observation: &ProviderObservation,
        input: Option<ProviderInput>,
        cwd: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        mode: ProviderSessionStartMode,
    ) -> Result<StartProviderSessionRequest, ProviderBridgeError> {
        let adapter = self
            .adapter(agent.provider_kind)
            .ok_or(ProviderError::ProviderNotRegistered(agent.provider_kind))?;
        start_request_from_adapter(
            agent,
            observation,
            adapter.as_ref(),
            input,
            cwd,
            environment,
            mode,
        )
    }

    pub fn session_manager<L, S>(launcher: L, store: S) -> ProviderSessionManager<L, S>
    where
        L: ProviderProcessLauncher,
        S: ProviderSessionStateStore,
    {
        ProviderSessionManager::with_state_store(launcher, store)
    }

    pub fn unavailable_process_launcher() -> UnavailableProviderProcessLauncher {
        UnavailableProviderProcessLauncher
    }

    pub fn session_manager_hold(&self) -> OrchestrationHold {
        OrchestrationHold::ProviderRuntimeAuthorityAbsent
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAiLaunchAdmission {
    kind: ProviderKind,
    mode: ProviderSessionStartMode,
    provider_session_id: Option<ProviderSessionId>,
}

impl HostAiLaunchAdmission {
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    pub const fn mode(&self) -> ProviderSessionStartMode {
        self.mode
    }

    pub fn provider_session_id(&self) -> Option<&ProviderSessionId> {
        self.provider_session_id.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLaunchError {
    ProviderNotRegistered(ProviderKind),
    InvalidProviderSessionId,
    ExactResumeUnsupported(ProviderKind),
}

impl fmt::Display for HostLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderNotRegistered(kind) => {
                write!(formatter, "stock provider {kind} is not registered")
            }
            Self::InvalidProviderSessionId => {
                write!(formatter, "provider session id is invalid")
            }
            Self::ExactResumeUnsupported(kind) => {
                write!(formatter, "exact provider resume is unsupported for {kind}")
            }
        }
    }
}

impl std::error::Error for HostLaunchError {}

/// Correlated specialist facts plus a managed-process fence. Correlation is
/// not cancel, write, or result authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistProcessAuthority {
    task_id: TaskId,
    specialist_id: AgentSessionId,
    provider_kind: ProviderKind,
    provider_session_id: ProviderSessionId,
    runtime_generation: u64,
    resource_fence: ResourceFence,
}

impl SpecialistProcessAuthority {
    pub fn from_managed_process(
        facts: &AgentSessionFacts,
        fence: &ManagedProcessFence,
    ) -> Result<Self, OrchestrationHold> {
        if !matches!(facts.role, AgentRole::Specialist { .. }) {
            return Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent);
        }
        let Some(provider_session_id) = facts.provider_session_id.clone() else {
            return Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent);
        };
        if facts.runtime_generation == 0
            || fence.resource().runtime_generation != facts.runtime_generation
        {
            return Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent);
        }
        Ok(Self {
            task_id: facts.task_id,
            specialist_id: facts.id,
            provider_kind: facts.provider_kind,
            provider_session_id,
            runtime_generation: facts.runtime_generation,
            resource_fence: fence.resource(),
        })
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn specialist_id(&self) -> AgentSessionId {
        self.specialist_id
    }

    pub const fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    pub const fn resource_fence(&self) -> ResourceFence {
        self.resource_fence
    }

    pub fn provider_session_id(&self) -> &ProviderSessionId {
        &self.provider_session_id
    }
}

pub fn correlate_specialist_authority(
    authority: &SpecialistProcessAuthority,
    facts: &AgentSessionFacts,
) -> Result<(), OrchestrationHold> {
    if facts.id != authority.specialist_id
        || facts.task_id != authority.task_id
        || facts.provider_kind != authority.provider_kind
        || facts.runtime_generation != authority.runtime_generation
        || facts.provider_session_id.as_ref() != Some(&authority.provider_session_id)
        || !matches!(facts.role, AgentRole::Specialist { .. })
    {
        return Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent);
    }
    Ok(())
}

/// Exact-fenced specialist lifecycle after a successful Job/PTY operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistLifecycleLineage {
    specialist_id: AgentSessionId,
    task_id: TaskId,
    runtime_generation: u64,
    resource_fence: ResourceFence,
}

impl SpecialistLifecycleLineage {
    pub const fn specialist_id(&self) -> AgentSessionId {
        self.specialist_id
    }

    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub const fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    pub const fn resource_fence(&self) -> ResourceFence {
        self.resource_fence
    }
}

/// Journal-correlated specialist result after exact generation/resource match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecialistResultLineage {
    specialist_id: AgentSessionId,
    task_id: TaskId,
    runtime_generation: u64,
    action_epoch: u64,
    journal_event_id: EventId,
    journal_sequence: u64,
}

impl SpecialistResultLineage {
    pub const fn specialist_id(&self) -> AgentSessionId {
        self.specialist_id
    }

    pub const fn journal_event_id(&self) -> EventId {
        self.journal_event_id
    }

    pub const fn journal_sequence(&self) -> u64 {
        self.journal_sequence
    }
}

/// Correlation plus exact Job/PTY cancel for this specialist generation.
pub fn cancel_specialist_with_authority<L: ProviderProcessLauncher>(
    launcher: &mut L,
    lease: &mut ProviderProcessLease,
    authority: &SpecialistProcessAuthority,
    facts: &AgentSessionFacts,
) -> Result<SpecialistLifecycleLineage, OrchestrationHold> {
    correlate_specialist_authority(authority, facts)?;
    if lease.fence().resource() != authority.resource_fence
        || lease.fence().resource().runtime_generation != authority.runtime_generation
    {
        return Err(specialist_cancel_hold());
    }
    launcher
        .stop_and_join(lease)
        .map_err(|_| specialist_cancel_hold())?;
    Ok(SpecialistLifecycleLineage {
        specialist_id: authority.specialist_id,
        task_id: authority.task_id,
        runtime_generation: authority.runtime_generation,
        resource_fence: authority.resource_fence,
    })
}

pub fn write_specialist_with_authority(
    handle: &ProviderRuntimeWriteHandle,
    authority: &SpecialistProcessAuthority,
    facts: &AgentSessionFacts,
    identity: &ProviderInputDeliveryIdentity,
    action: &crate::domain::provider_input::ProviderInputAction,
    plan: &ProviderInputDeliveryPlan,
) -> Result<ProviderInputWriteReceipt, OrchestrationHold> {
    correlate_specialist_authority(authority, facts)?;
    if handle.fence().resource() != authority.resource_fence
        || identity.agent_session_id != authority.specialist_id
        || identity.runtime_generation != authority.runtime_generation
        || identity.provider_session_id != authority.provider_session_id
    {
        return Err(specialist_write_hold());
    }
    handle
        .write_action(identity, action, plan)
        .map_err(|_| specialist_write_hold())
}

pub fn observe_specialist_native_child(
    authority: &SpecialistProcessAuthority,
    facts: &AgentSessionFacts,
) -> Result<(), OrchestrationHold> {
    correlate_specialist_authority(authority, facts)?;
    Err(specialist_native_child_hold())
}

pub fn accept_specialist_structured_result(
    authority: &SpecialistProcessAuthority,
    facts: &AgentSessionFacts,
    result: &SpecialistResult,
    journal: &JournalEvent,
) -> Result<SpecialistResultLineage, OrchestrationHold> {
    correlate_specialist_authority(authority, facts)?;
    validate_specialist_result(result)?;
    if journal.task_id() != authority.task_id
        || journal.agent_session_id() != authority.specialist_id
        || journal.provider() != authority.provider_kind
        || journal.runtime_generation() != authority.runtime_generation
        || journal.resource_id() != authority.resource_fence.resource_id
        || journal.sequence() == 0
    {
        return Err(specialist_structured_result_hold());
    }
    Ok(SpecialistResultLineage {
        specialist_id: authority.specialist_id,
        task_id: authority.task_id,
        runtime_generation: authority.runtime_generation,
        action_epoch: journal.action_epoch(),
        journal_event_id: journal.id(),
        journal_sequence: journal.sequence(),
    })
}

/// Admit an optional specialist start through the registered adapter. Process
/// spawn stays fail-closed while the Job/PTY permit issuer is unavailable.
pub fn admit_specialist_start(
    host: &ProviderHost,
    existing_primary: bool,
    agent: AgentSessionFacts,
    observation: &ProviderObservation,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    mode: ProviderSessionStartMode,
) -> Result<StartProviderSessionRequest, OrchestrationHold> {
    ensure_single_primary(existing_primary, matches!(agent.role, AgentRole::Primary))?;
    if !matches!(agent.role, AgentRole::Specialist { .. }) {
        return Err(OrchestrationHold::ProviderRuntimeAuthorityAbsent);
    }
    host.start_request_from_registered_adapter(agent, observation, None, cwd, environment, mode)
        .map_err(|_| OrchestrationHold::ProviderRuntimeAuthorityAbsent)
}

/// Bind-only input never settles. Missing owner: provider-owned session write.
pub fn deliver_claimed_provider_input(
    port: &mut BoundProviderInputPort,
    identity: ProviderInputDeliveryIdentity,
    plan: ProviderInputDeliveryPlan,
) -> Result<ProviderInputBridgeHold, ProviderInputDeliveryError> {
    deliver_through_capability(port, identity, plan)
}

/// Proof-bearing live write. Generic PTY, transcript, or identity bind cannot
/// issue this receipt.
pub fn deliver_live_provider_input(
    handle: &ProviderRuntimeWriteHandle,
    identity: ProviderInputDeliveryIdentity,
    action: &crate::domain::provider_input::ProviderInputAction,
    plan: ProviderInputDeliveryPlan,
) -> Result<ProviderInputWriteReceipt, ProviderInputDeliveryError> {
    handle.write_action(&identity, action, &plan)
}

pub fn stock_registration_order() -> [ProviderKind; 3] {
    STOCK_PROVIDER_REGISTRATION_ORDER
}

pub fn register_stock_host_adapters(registry: &mut ProviderRegistry) -> Result<(), ProviderError> {
    register_stock_adapters(registry)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
