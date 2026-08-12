//! Thread-safe NativeShell controller for the single host-owned WebView.
//!
//! This type is the only NativeShell-facing admission path. It binds an exact
//! Task/Agent/Context/Resource identity, a generation/lease, and a gateway
//! process-session ref. WebView2 details stay behind [`BrowserWebViewHost`].

use super::{
    unsupported_platform_error, BrowserBounds, BrowserCommand, BrowserError, BrowserWorkspaceKey,
};
use crate::domain::id::{AgentSessionId, BrowserContextId, ResourceId, TaskId};
use crate::protocol::BrowserSurfaceIdentity;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrowserNativeIdentity {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    context_id: BrowserContextId,
    resource_id: ResourceId,
}

impl BrowserNativeIdentity {
    pub const fn new(
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        context_id: BrowserContextId,
        resource_id: ResourceId,
    ) -> Self {
        Self {
            task_id,
            agent_session_id,
            context_id,
            resource_id,
        }
    }

    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn agent_session_id(&self) -> AgentSessionId {
        self.agent_session_id
    }

    pub fn context_id(&self) -> BrowserContextId {
        self.context_id
    }

    pub fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    pub fn protocol_surface(&self) -> BrowserSurfaceIdentity {
        BrowserSurfaceIdentity {
            task_id: self.task_id,
            context_id: self.context_id,
            resource_id: self.resource_id,
        }
    }

    pub fn gateway_tuple(&self) -> (TaskId, AgentSessionId, BrowserContextId, ResourceId) {
        (
            self.task_id,
            self.agent_session_id,
            self.context_id,
            self.resource_id,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserGatewayBindingRef {
    process_session_id: String,
}

impl BrowserGatewayBindingRef {
    pub fn new(process_session_id: impl Into<String>) -> Self {
        Self {
            process_session_id: process_session_id.into(),
        }
    }

    pub fn process_session_id(&self) -> &str {
        &self.process_session_id
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BrowserNativeLease {
    generation: u64,
    token: u64,
}

impl fmt::Debug for BrowserNativeLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserNativeLease")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl BrowserNativeLease {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn spoil_generation_for_test(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

/// Host-side fence for commands crossing the NativeShell/WebView boundary.
///
/// The controller is the authority that mints a lease, but the host must also
/// remember the lease it admitted.  Without this small second fence a command
/// that was queued before a detach/rebind could still reach the WebView host
/// after a new task surface had become current.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrowserNativeLeaseFence {
    current: Option<BrowserNativeLease>,
}

impl BrowserNativeLeaseFence {
    /// Admit the first lease or the exact lease already owned by the host.
    /// Any other generation/token is stale and must not reach host state.
    pub fn admit(&mut self, lease: BrowserNativeLease) -> Result<(), BrowserNativeControllerError> {
        match self.current {
            None => {
                self.current = Some(lease);
                Ok(())
            }
            Some(current) if current == lease => Ok(()),
            Some(current) if current.generation != lease.generation => {
                Err(BrowserNativeControllerError::StaleGeneration)
            }
            Some(_) => Err(BrowserNativeControllerError::StaleLease),
        }
    }

    /// Retire only the lease currently owned by the host.
    pub fn retire(
        &mut self,
        lease: BrowserNativeLease,
    ) -> Result<(), BrowserNativeControllerError> {
        self.admit(lease)?;
        self.current = None;
        Ok(())
    }

    pub fn current(&self) -> Option<BrowserNativeLease> {
        self.current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserNativeDestination {
    raw: u64,
}

impl BrowserNativeDestination {
    pub fn from_raw(raw: u64) -> Result<Self, BrowserNativeControllerError> {
        if raw == 0 {
            return Err(BrowserNativeControllerError::InvalidRequest);
        }
        Ok(Self { raw })
    }

    pub fn raw(self) -> u64 {
        self.raw
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserNativeCallbackKind {
    NavigationComplete,
    SurfaceResized,
    SurfaceFocused,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserNativeCallback {
    pub generation: u64,
    pub lease: BrowserNativeLease,
    pub kind: BrowserNativeCallbackKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserNativeControllerError {
    StaleGeneration,
    StaleLease,
    IdentityMismatch,
    GatewayMismatch,
    GatewayUnbound,
    StaleCallback,
    UnsupportedPlatform,
    Detached,
    AttachedBindingMustDetach,
    InvalidRequest,
}

impl fmt::Display for BrowserNativeControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::StaleGeneration => "browser native generation does not match",
            Self::StaleLease => "browser native lease does not match",
            Self::IdentityMismatch => "browser native identity does not match",
            Self::GatewayMismatch => "browser gateway binding does not match",
            Self::GatewayUnbound => "browser gateway binding is not attached",
            Self::StaleCallback => "browser native callback is stale",
            Self::UnsupportedPlatform => "embedded browser support is unavailable",
            Self::Detached => "browser native surface is detached",
            Self::AttachedBindingMustDetach => {
                "attached browser binding must detach before a new bind"
            }
            Self::InvalidRequest => "browser native controller request is invalid",
        })
    }
}

impl std::error::Error for BrowserNativeControllerError {}

impl From<BrowserNativeControllerError> for BrowserError {
    fn from(error: BrowserNativeControllerError) -> Self {
        match error {
            BrowserNativeControllerError::UnsupportedPlatform => {
                unsupported_platform_error(std::env::consts::OS)
            }
            BrowserNativeControllerError::StaleGeneration => BrowserError::InvalidInvocation {
                field: "generation".to_string(),
            },
            BrowserNativeControllerError::StaleLease => BrowserError::InvalidInvocation {
                field: "lease".to_string(),
            },
            BrowserNativeControllerError::IdentityMismatch => BrowserError::InvalidInvocation {
                field: "identity".to_string(),
            },
            BrowserNativeControllerError::GatewayMismatch
            | BrowserNativeControllerError::GatewayUnbound => BrowserError::InvalidInvocation {
                field: "gateway".to_string(),
            },
            BrowserNativeControllerError::StaleCallback => BrowserError::InvalidInvocation {
                field: "callback".to_string(),
            },
            BrowserNativeControllerError::Detached => BrowserError::Interrupted,
            BrowserNativeControllerError::AttachedBindingMustDetach => {
                BrowserError::InvalidInvocation {
                    field: "attachedBinding".to_string(),
                }
            }
            BrowserNativeControllerError::InvalidRequest => BrowserError::InvalidInvocation {
                field: "nativeShell".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserNativeHostCommand {
    Attach {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
        destination: BrowserNativeDestination,
        bounds: BrowserBounds,
    },
    Reattach {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
        destination: BrowserNativeDestination,
        bounds: BrowserBounds,
    },
    BindGateway {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
    },
    SubmitCommand {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
        command: BrowserCommand,
    },
    Resize {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        bounds: BrowserBounds,
    },
    Focus {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        focused: bool,
    },
    Detach {
        lease: BrowserNativeLease,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
    },
}

impl BrowserNativeHostCommand {
    pub fn lease(&self) -> BrowserNativeLease {
        match self {
            Self::Attach { lease, .. }
            | Self::Reattach { lease, .. }
            | Self::BindGateway { lease, .. }
            | Self::SubmitCommand { lease, .. }
            | Self::Resize { lease, .. }
            | Self::Focus { lease, .. }
            | Self::Detach { lease, .. } => *lease,
        }
    }

    pub fn identity(&self) -> BrowserNativeIdentity {
        match self {
            Self::Attach { identity, .. }
            | Self::Reattach { identity, .. }
            | Self::BindGateway { identity, .. }
            | Self::SubmitCommand { identity, .. }
            | Self::Resize { identity, .. }
            | Self::Focus { identity, .. }
            | Self::Detach { identity, .. } => *identity,
        }
    }

    pub fn workspace_key(&self) -> Option<&BrowserWorkspaceKey> {
        match self {
            Self::Attach { workspace_key, .. }
            | Self::Reattach { workspace_key, .. }
            | Self::BindGateway { workspace_key, .. }
            | Self::SubmitCommand { workspace_key, .. }
            | Self::Detach { workspace_key, .. } => Some(workspace_key),
            Self::Resize { .. } | Self::Focus { .. } => None,
        }
    }

    pub fn gateway(&self) -> Option<&BrowserGatewayBindingRef> {
        match self {
            Self::Attach { gateway, .. }
            | Self::Reattach { gateway, .. }
            | Self::BindGateway { gateway, .. }
            | Self::SubmitCommand { gateway, .. }
            | Self::Detach { gateway, .. } => Some(gateway),
            Self::Resize { .. } | Self::Focus { .. } => None,
        }
    }

    pub fn browser_command(&self) -> Option<&BrowserCommand> {
        match self {
            Self::SubmitCommand { command, .. } => Some(command),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserNativeHostOutcome {
    Applied,
    Parked,
    CommandHandoff,
    Idempotent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveBinding {
    identity: BrowserNativeIdentity,
    workspace_key: BrowserWorkspaceKey,
    gateway: BrowserGatewayBindingRef,
    lease: BrowserNativeLease,
    attached: bool,
    destination: Option<BrowserNativeDestination>,
    bounds: Option<BrowserBounds>,
    focused: bool,
    last_attach: Option<BrowserNativeHostCommand>,
    last_detach: Option<BrowserNativeHostCommand>,
}

struct ControllerState {
    platform_supported: bool,
    next_generation: u64,
    next_token: u64,
    current: Option<LiveBinding>,
}

#[derive(Clone)]
pub struct BrowserNativeShellController {
    inner: Arc<Mutex<ControllerState>>,
}

impl BrowserNativeShellController {
    pub fn supported() -> Self {
        Self::new(true)
    }

    pub fn unsupported() -> Self {
        Self::new(false)
    }

    pub fn for_current_platform() -> Self {
        if cfg!(target_os = "windows") {
            Self::supported()
        } else {
            Self::unsupported()
        }
    }

    fn new(platform_supported: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ControllerState {
                platform_supported,
                next_generation: 0,
                next_token: 0,
                current: None,
            })),
        }
    }

    pub fn platform_supported(&self) -> bool {
        self.lock().platform_supported
    }

    pub fn bind(
        &self,
        identity: BrowserNativeIdentity,
        workspace_key: BrowserWorkspaceKey,
        gateway: BrowserGatewayBindingRef,
    ) -> Result<BrowserNativeLease, BrowserNativeControllerError> {
        if gateway.process_session_id().trim().is_empty() {
            return Err(BrowserNativeControllerError::InvalidRequest);
        }
        let mut state = self.lock();
        if let Some(current) = state.current.as_ref() {
            if current.identity == identity
                && current.workspace_key == workspace_key
                && current.gateway == gateway
            {
                return Ok(current.lease);
            }
            if current.attached {
                return Err(BrowserNativeControllerError::AttachedBindingMustDetach);
            }
            if current.identity == identity
                && current.workspace_key == workspace_key
                && current.gateway != gateway
            {
                return Err(BrowserNativeControllerError::GatewayMismatch);
            }
        }
        state.next_generation = state
            .next_generation
            .checked_add(1)
            .ok_or(BrowserNativeControllerError::InvalidRequest)?;
        state.next_token = state
            .next_token
            .checked_add(1)
            .ok_or(BrowserNativeControllerError::InvalidRequest)?;
        let lease = BrowserNativeLease {
            generation: state.next_generation,
            token: state.next_token,
        };
        state.current = Some(LiveBinding {
            identity,
            workspace_key,
            gateway,
            lease,
            attached: false,
            destination: None,
            bounds: None,
            focused: false,
            last_attach: None,
            last_detach: None,
        });
        Ok(lease)
    }

    pub fn bind_gateway(
        &self,
        lease: &BrowserNativeLease,
        gateway: &BrowserGatewayBindingRef,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let current = self.require_current(lease)?;
        if current.gateway != *gateway {
            return Err(BrowserNativeControllerError::GatewayMismatch);
        }
        Ok(BrowserNativeHostCommand::BindGateway {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
        })
    }

    pub fn attach(
        &self,
        lease: &BrowserNativeLease,
        destination: BrowserNativeDestination,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let gateway = self.require_current(lease)?.gateway.clone();
        self.attach_with_gateway(lease, &gateway, destination, bounds)
    }

    pub fn attach_with_gateway(
        &self,
        lease: &BrowserNativeLease,
        gateway: &BrowserGatewayBindingRef,
        destination: BrowserNativeDestination,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let platform_supported = state.platform_supported;
        let current = require_current_mut(&mut state, lease)?;
        if current.gateway != *gateway {
            return Err(BrowserNativeControllerError::GatewayMismatch);
        }
        if !platform_supported {
            return Err(BrowserNativeControllerError::UnsupportedPlatform);
        }
        if current.attached
            && current.destination == Some(destination)
            && current.bounds == Some(bounds)
        {
            if let Some(command) = current.last_attach.clone() {
                return Ok(command);
            }
        }
        let command = BrowserNativeHostCommand::Attach {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
            destination,
            bounds,
        };
        current.attached = true;
        current.destination = Some(destination);
        current.bounds = Some(bounds);
        current.last_attach = Some(command.clone());
        current.last_detach = None;
        Ok(command)
    }

    pub fn reattach(
        &self,
        lease: &BrowserNativeLease,
        destination: BrowserNativeDestination,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let platform_supported = state.platform_supported;
        let current = require_current_mut(&mut state, lease)?;
        if !platform_supported {
            return Err(BrowserNativeControllerError::UnsupportedPlatform);
        }
        if !current.attached {
            return Err(BrowserNativeControllerError::Detached);
        }
        let command = BrowserNativeHostCommand::Reattach {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
            destination,
            bounds,
        };
        current.destination = Some(destination);
        current.bounds = Some(bounds);
        current.last_attach = Some(BrowserNativeHostCommand::Attach {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
            destination,
            bounds,
        });
        Ok(command)
    }

    pub fn resize(
        &self,
        lease: &BrowserNativeLease,
        bounds: BrowserBounds,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let platform_supported = state.platform_supported;
        let current = require_current_mut(&mut state, lease)?;
        if !platform_supported {
            return Err(BrowserNativeControllerError::UnsupportedPlatform);
        }
        if !current.attached {
            return Err(BrowserNativeControllerError::Detached);
        }
        current.bounds = Some(bounds);
        Ok(BrowserNativeHostCommand::Resize {
            lease: current.lease,
            identity: current.identity,
            bounds,
        })
    }

    pub fn focus(
        &self,
        lease: &BrowserNativeLease,
        focused: bool,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let platform_supported = state.platform_supported;
        let current = require_current_mut(&mut state, lease)?;
        if !platform_supported {
            return Err(BrowserNativeControllerError::UnsupportedPlatform);
        }
        if !current.attached {
            return Err(BrowserNativeControllerError::Detached);
        }
        current.focused = focused;
        Ok(BrowserNativeHostCommand::Focus {
            lease: current.lease,
            identity: current.identity,
            focused,
        })
    }

    pub fn submit_command(
        &self,
        lease: &BrowserNativeLease,
        command: BrowserCommand,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let platform_supported = state.platform_supported;
        let current = require_current_mut(&mut state, lease)?;
        if !platform_supported {
            return Err(BrowserNativeControllerError::UnsupportedPlatform);
        }
        if !current.attached {
            return Err(BrowserNativeControllerError::Detached);
        }
        Ok(BrowserNativeHostCommand::SubmitCommand {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
            command,
        })
    }

    pub fn detach(
        &self,
        lease: &BrowserNativeLease,
    ) -> Result<BrowserNativeHostCommand, BrowserNativeControllerError> {
        let mut state = self.lock();
        let current = require_current_mut(&mut state, lease)?;
        if !current.attached {
            if let Some(command) = current.last_detach.clone() {
                return Ok(command);
            }
        }
        let command = BrowserNativeHostCommand::Detach {
            lease: current.lease,
            identity: current.identity,
            workspace_key: current.workspace_key.clone(),
            gateway: current.gateway.clone(),
        };
        current.attached = false;
        current.destination = None;
        current.focused = false;
        current.last_detach = Some(command.clone());
        Ok(command)
    }

    /// Retire a detached binding after the host has completed its park/close
    /// join.  Keeping a detached binding in the controller would allow a late
    /// callback carrying the old lease to look current until another Task was
    /// bound.  The host calls this only after it has accepted the matching
    /// [`BrowserNativeHostCommand::Detach`], so the controller and host have a
    /// single close boundary and never leave an orphaned surface lease behind.
    pub fn close(&self, lease: &BrowserNativeLease) -> Result<(), BrowserNativeControllerError> {
        let mut state = self.lock();
        let current = require_current_mut(&mut state, lease)?;
        if current.attached {
            return Err(BrowserNativeControllerError::AttachedBindingMustDetach);
        }
        state.current = None;
        Ok(())
    }

    pub fn require_identity(
        &self,
        lease: &BrowserNativeLease,
        identity: BrowserNativeIdentity,
    ) -> Result<(), BrowserNativeControllerError> {
        let current = self.require_current(lease)?;
        if current.identity != identity {
            return Err(BrowserNativeControllerError::IdentityMismatch);
        }
        Ok(())
    }

    pub fn take_callback(
        &self,
        callback: BrowserNativeCallback,
    ) -> Option<BrowserNativeCallbackKind> {
        let current = self.require_current(&callback.lease).ok()?;
        if current.lease.generation != callback.generation {
            return None;
        }
        Some(callback.kind)
    }

    pub fn current_identity(&self) -> Option<BrowserNativeIdentity> {
        self.lock().current.as_ref().map(|current| current.identity)
    }

    pub fn current_gateway(&self) -> Option<BrowserGatewayBindingRef> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.gateway.clone())
    }

    pub fn current_lease(&self) -> Option<BrowserNativeLease> {
        self.lock().current.as_ref().map(|current| current.lease)
    }

    pub fn is_attached(&self) -> bool {
        self.lock()
            .current
            .as_ref()
            .is_some_and(|current| current.attached)
    }

    fn require_current(
        &self,
        lease: &BrowserNativeLease,
    ) -> Result<LiveBinding, BrowserNativeControllerError> {
        let state = self.lock();
        require_current(&state, lease).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, ControllerState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn require_current<'a>(
    state: &'a ControllerState,
    lease: &BrowserNativeLease,
) -> Result<&'a LiveBinding, BrowserNativeControllerError> {
    let current = state
        .current
        .as_ref()
        .ok_or(BrowserNativeControllerError::StaleLease)?;
    if current.lease.token != lease.token {
        return Err(BrowserNativeControllerError::StaleLease);
    }
    if current.lease.generation != lease.generation {
        return Err(BrowserNativeControllerError::StaleGeneration);
    }
    Ok(current)
}

fn require_current_mut<'a>(
    state: &'a mut ControllerState,
    lease: &BrowserNativeLease,
) -> Result<&'a mut LiveBinding, BrowserNativeControllerError> {
    let current = state
        .current
        .as_mut()
        .ok_or(BrowserNativeControllerError::StaleLease)?;
    if current.lease.token != lease.token {
        return Err(BrowserNativeControllerError::StaleLease);
    }
    if current.lease.generation != lease.generation {
        return Err(BrowserNativeControllerError::StaleGeneration);
    }
    Ok(current)
}
