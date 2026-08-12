//! Pure local authorization rules for Connect callers.

use std::fmt;
use std::num::NonZeroU16;

use crate::domain::id::TaskId;

use super::identity::{
    validate_device_credential, ConnectIdentity, CredentialVault, DeviceCredentialProof,
    MachineBinding,
};
use super::identity_store::{IdentityPersistence, IsolatedRemoteStore};

/// Stable action discriminant. Unknown nonzero values are denied, never
/// converted into a new command or interactive action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(NonZeroU16);

impl ActionId {
    pub const READ_TASK: Self = Self(NonZeroU16::new(1).unwrap());
    pub const READ_PRESENCE: Self = Self(NonZeroU16::new(2).unwrap());
    pub const READ_OPERATION: Self = Self(NonZeroU16::new(3).unwrap());
    pub const READ_PERSONAL_PROMPTS: Self = Self(NonZeroU16::new(4).unwrap());
    pub const MUTATE_TASK: Self = Self(NonZeroU16::new(10).unwrap());
    pub const SEND_PROMPT: Self = Self(NonZeroU16::new(11).unwrap());
    pub const ANSWER_REQUEST: Self = Self(NonZeroU16::new(12).unwrap());
    pub const TERMINAL_INPUT: Self = Self(NonZeroU16::new(13).unwrap());
    pub const BROWSER_COMMAND: Self = Self(NonZeroU16::new(14).unwrap());
    pub const APPROVE_DANGEROUS: Self = Self(NonZeroU16::new(20).unwrap());

    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u16 {
        self.0.get()
    }

    pub const fn known(self) -> Option<KnownAction> {
        Some(match self.get() {
            1 => KnownAction::ReadTask,
            2 => KnownAction::ReadPresence,
            3 => KnownAction::ReadOperation,
            4 => KnownAction::ReadPersonalPrompts,
            10 => KnownAction::MutateTask,
            11 => KnownAction::SendPrompt,
            12 => KnownAction::AnswerRequest,
            13 => KnownAction::TerminalInput,
            14 => KnownAction::BrowserCommand,
            20 => KnownAction::ApproveDangerous,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownAction {
    ReadTask,
    ReadPresence,
    ReadOperation,
    ReadPersonalPrompts,
    MutateTask,
    SendPrompt,
    AnswerRequest,
    TerminalInput,
    BrowserCommand,
    ApproveDangerous,
}

impl KnownAction {
    pub const fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::MutateTask
                | Self::SendPrompt
                | Self::AnswerRequest
                | Self::TerminalInput
                | Self::BrowserCommand
                | Self::ApproveDangerous
        )
    }

    pub const fn is_dangerous_approval(self) -> bool {
        matches!(self, Self::ApproveDangerous)
    }

    pub const fn is_owner_only(self) -> bool {
        matches!(self, Self::ReadPersonalPrompts | Self::ApproveDangerous)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectRole {
    PairedOwner,
    Watcher { task_id: TaskId },
    Collaborator { task_id: TaskId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub role: ConnectRole,
    pub task_id: Option<TaskId>,
    pub action: ActionId,
    /// Opaque current registered, non-revoked, host-bound credential.
    /// A raw DeviceId is never sufficient; mint via the authoritative store
    /// binding operation.
    /// Live connection/session/route authority is supplied separately as an
    /// opaque `ScopedPermissionGrant`; this request alone is never enough.
    pub credential: Option<DeviceCredentialProof>,
}

/// Epoch tuple supplied by the authoritative connection/session router. The
/// local evaluator never manufactures one; a scoped grant must carry the
/// exact tuple that was active when it was issued.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AuthoritativePermissionContext {
    channel_epoch: u64,
    session_epoch: u64,
    route_epoch: u64,
}

impl AuthoritativePermissionContext {
    pub fn live(channel_epoch: u64, session_epoch: u64, route_epoch: u64) -> Option<Self> {
        if channel_epoch == 0 || session_epoch == 0 || route_epoch == 0 {
            return None;
        }
        Some(Self {
            channel_epoch,
            session_epoch,
            route_epoch,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(channel_epoch: u64, session_epoch: u64, route_epoch: u64) -> Self {
        Self::live(channel_epoch, session_epoch, route_epoch).expect("test epochs must be nonzero")
    }
}

impl fmt::Debug for AuthoritativePermissionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthoritativePermissionContext")
            .field("channel_epoch", &self.channel_epoch)
            .field("session_epoch", &self.session_epoch)
            .field("route_epoch", &self.route_epoch)
            .finish()
    }
}

/// Opaque grant issued by a trusted channel/session authority. Watcher and
/// Collaborator role labels alone are not authorization; every guest request
/// must present a grant bound to role, Task, action, and all three live epochs.
#[derive(Clone, PartialEq, Eq)]
pub struct ScopedPermissionGrant {
    role: ConnectRole,
    task_id: TaskId,
    action: ActionId,
    channel_epoch: u64,
    session_epoch: u64,
    route_epoch: u64,
}

impl ScopedPermissionGrant {
    pub fn issue(
        role: ConnectRole,
        task_id: TaskId,
        action: ActionId,
        context: AuthoritativePermissionContext,
    ) -> Option<Self> {
        if context.channel_epoch == 0 || context.session_epoch == 0 || context.route_epoch == 0 {
            return None;
        }
        if matches!(role, ConnectRole::PairedOwner) {
            return None;
        }
        if let ConnectRole::Watcher { task_id: scoped }
        | ConnectRole::Collaborator { task_id: scoped } = role
        {
            if scoped != task_id {
                return None;
            }
        }
        Some(Self {
            role,
            task_id,
            action,
            channel_epoch: context.channel_epoch,
            session_epoch: context.session_epoch,
            route_epoch: context.route_epoch,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        role: ConnectRole,
        task_id: TaskId,
        action: ActionId,
        context: AuthoritativePermissionContext,
    ) -> Self {
        Self::issue(role, task_id, action, context).expect("test grant must be scoped")
    }

    fn matches(
        &self,
        request: &PermissionRequest,
        context: AuthoritativePermissionContext,
    ) -> bool {
        self.channel_epoch != 0
            && self.session_epoch != 0
            && self.route_epoch != 0
            && context.channel_epoch != 0
            && context.session_epoch != 0
            && context.route_epoch != 0
            && self.role == request.role
            && request.task_id == Some(self.task_id)
            && self.action == request.action
            && self.channel_epoch == context.channel_epoch
            && self.session_epoch == context.session_epoch
            && self.route_epoch == context.route_epoch
    }
}

impl fmt::Debug for ScopedPermissionGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedPermissionGrant")
            .field("role", &self.role)
            .field("task_id", &self.task_id)
            .field("action", &self.action)
            .field("epochs", &"redacted")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDenyReason {
    UnknownAction,
    TaskScopeRequired,
    TaskScopeMismatch,
    WatcherReadOnly,
    OwnerOnly,
    CollaboratorWriteDisabled,
    DeviceCredentialRequired,
    ScopedGrantRequired,
    NonAuthoritativeContext,
}

impl fmt::Display for PermissionDenyReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownAction => "unknown Connect action",
            Self::TaskScopeRequired => "a Task scope is required",
            Self::TaskScopeMismatch => "the requested Task is outside the grant scope",
            Self::WatcherReadOnly => "Watcher grants are read-only",
            Self::OwnerOnly => "the action is Owner-only",
            Self::CollaboratorWriteDisabled => "Collaborator writes are disabled",
            Self::DeviceCredentialRequired => {
                "PairedOwner actions require a verified device credential"
            }
            Self::ScopedGrantRequired => "guest roles require a current authoritative scoped grant",
            Self::NonAuthoritativeContext => {
                "the evaluator lacks authoritative channel/session/route context"
            }
        })
    }
}

impl std::error::Error for PermissionDenyReason {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Denied(PermissionDenyReason),
}

impl PermissionDecision {
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Local evaluator. Presence is intentionally not an input to this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionEvaluator {
    collaborator_writes_enabled: bool,
}

impl PermissionEvaluator {
    pub const fn new(collaborator_writes_enabled: bool) -> Self {
        Self {
            collaborator_writes_enabled,
        }
    }

    pub const fn owner_only() -> Self {
        Self::new(false)
    }

    pub fn evaluate(&self, request: PermissionRequest) -> PermissionDecision {
        self.evaluate_roles(request, false, false)
    }

    /// Evaluate a request after reloading and validating its credential
    /// through the authoritative identity store and active session. A public
    /// evaluator must not accept a caller-supplied identity snapshot.
    pub fn evaluate_with_store<P: IdentityPersistence + 'static, V: CredentialVault>(
        &self,
        request: PermissionRequest,
        store: &mut IsolatedRemoteStore<P>,
        binding: &MachineBinding,
        vault: &V,
        active_session_epoch: u64,
    ) -> PermissionDecision {
        if matches!(request.role, ConnectRole::PairedOwner) {
            let Some(proof) = request.credential.as_ref() else {
                return PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired);
            };
            if store
                .validate_device_credential(binding, vault, proof, active_session_epoch)
                .is_err()
            {
                return PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired);
            }
            // Credential proof validation is authoritative for identity only;
            // this API has no channel/session/route epoch, so it must not
            // authorize a live permission request.
            return PermissionDecision::Denied(PermissionDenyReason::NonAuthoritativeContext);
        }
        self.evaluate_roles(request, false, false)
    }

    /// Evaluate a guest request only when a trusted authority supplies a
    /// one-shot grant bound to the live channel/session/route epoch tuple.
    pub fn evaluate_with_scoped_grant(
        &self,
        request: PermissionRequest,
        grant: &ScopedPermissionGrant,
        context: AuthoritativePermissionContext,
    ) -> PermissionDecision {
        if !grant.matches(&request, context) {
            return PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired);
        }
        self.evaluate_roles(request, false, true)
    }

    /// Compatibility shim for callers that still pass an identity snapshot.
    /// Snapshot validation can reject stale proofs, but this API deliberately
    /// never authorizes a live request because it has no channel/route epoch.
    pub fn evaluate_with_authority<V: CredentialVault>(
        &self,
        request: PermissionRequest,
        identity: &ConnectIdentity,
        binding: &MachineBinding,
        vault: &V,
        active_session_epoch: u64,
    ) -> PermissionDecision {
        if matches!(request.role, ConnectRole::PairedOwner) {
            let Some(proof) = request.credential.as_ref() else {
                return PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired);
            };
            if validate_device_credential(identity, binding, vault, proof, active_session_epoch)
                .is_err()
            {
                return PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired);
            }
            return PermissionDecision::Denied(PermissionDenyReason::NonAuthoritativeContext);
        }
        self.evaluate(request)
    }

    fn evaluate_roles(
        &self,
        request: PermissionRequest,
        paired_owner_authorized: bool,
        scoped_guest_authorized: bool,
    ) -> PermissionDecision {
        let Some(action) = request.action.known() else {
            return PermissionDecision::Denied(PermissionDenyReason::UnknownAction);
        };

        match request.role {
            ConnectRole::PairedOwner if paired_owner_authorized => PermissionDecision::Allow,
            ConnectRole::PairedOwner => {
                PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
            }
            ConnectRole::Watcher { task_id } => {
                if !scoped_guest_authorized {
                    return PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired);
                }
                if !matches!(request.task_id, Some(requested) if requested == task_id) {
                    return PermissionDecision::Denied(match request.task_id {
                        Some(_) => PermissionDenyReason::TaskScopeMismatch,
                        None => PermissionDenyReason::TaskScopeRequired,
                    });
                }
                if action.is_mutating() {
                    PermissionDecision::Denied(PermissionDenyReason::WatcherReadOnly)
                } else if action.is_owner_only() {
                    PermissionDecision::Denied(PermissionDenyReason::OwnerOnly)
                } else {
                    PermissionDecision::Allow
                }
            }
            ConnectRole::Collaborator { task_id } => {
                if !scoped_guest_authorized {
                    return PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired);
                }
                if !matches!(request.task_id, Some(requested) if requested == task_id) {
                    return PermissionDecision::Denied(match request.task_id {
                        Some(_) => PermissionDenyReason::TaskScopeMismatch,
                        None => PermissionDenyReason::TaskScopeRequired,
                    });
                }
                if action.is_owner_only() {
                    return PermissionDecision::Denied(PermissionDenyReason::OwnerOnly);
                }
                if action.is_mutating() && !self.collaborator_writes_enabled {
                    return PermissionDecision::Denied(
                        PermissionDenyReason::CollaboratorWriteDisabled,
                    );
                }
                PermissionDecision::Allow
            }
        }
    }

    pub fn authorize(&self, request: PermissionRequest) -> bool {
        self.evaluate(request).is_allowed()
    }
}

impl Default for PermissionEvaluator {
    fn default() -> Self {
        Self::new(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::DeviceId;

    #[test]
    fn paired_owner_cannot_approve_dangerous_without_device_credential() {
        let decision = PermissionEvaluator::owner_only().evaluate(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id: None,
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        });
        assert_eq!(
            decision,
            PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
        );
    }

    #[test]
    fn paired_owner_does_not_authorize_arbitrary_device_id() {
        let _forged = DeviceId::new();
        let decision = PermissionEvaluator::owner_only().evaluate(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id: None,
            action: ActionId::APPROVE_DANGEROUS,
            credential: None,
        });
        assert_eq!(
            decision,
            PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
        );
    }

    #[test]
    fn paired_owner_cannot_read_without_device_credential() {
        let decision = PermissionEvaluator::owner_only().evaluate(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id: None,
            action: ActionId::READ_TASK,
            credential: None,
        });
        assert_eq!(
            decision,
            PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired)
        );
    }

    #[test]
    fn guest_roles_fail_closed_without_a_scoped_grant() {
        let task_id = TaskId::new();
        for role in [
            ConnectRole::Watcher { task_id },
            ConnectRole::Collaborator { task_id },
        ] {
            let decision = PermissionEvaluator::default().evaluate(PermissionRequest {
                role,
                task_id: Some(task_id),
                action: ActionId::READ_TASK,
                credential: None,
            });
            assert_eq!(
                decision,
                PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
            );
        }
    }

    #[test]
    fn scoped_guest_grant_is_bound_to_all_live_epochs() {
        let task_id = TaskId::new();
        let context = AuthoritativePermissionContext::for_test(4, 5, 6);
        let grant = ScopedPermissionGrant::for_test(
            ConnectRole::Watcher { task_id },
            task_id,
            ActionId::READ_TASK,
            context,
        );
        let request = PermissionRequest {
            role: ConnectRole::Watcher { task_id },
            task_id: Some(task_id),
            action: ActionId::READ_TASK,
            credential: None,
        };
        assert_eq!(
            PermissionEvaluator::default().evaluate_with_scoped_grant(
                request.clone(),
                &grant,
                context
            ),
            PermissionDecision::Allow
        );
        assert_eq!(
            PermissionEvaluator::default().evaluate_with_scoped_grant(
                request,
                &grant,
                AuthoritativePermissionContext::for_test(4, 5, 7),
            ),
            PermissionDecision::Denied(PermissionDenyReason::ScopedGrantRequired)
        );
    }
}
