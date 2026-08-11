//! Pure local authorization rules for Connect callers.

use std::fmt;
use std::num::NonZeroU16;

use crate::domain::id::TaskId;

use super::identity::{
    validate_device_credential, ConnectIdentity, CredentialVault, DeviceCredentialProof,
    MachineBinding,
};

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
    /// A raw DeviceId is never sufficient; mint via `bind_device_credential`.
    /// HOLD: live connection/session/epoch wiring remains outside this slice.
    pub credential: Option<DeviceCredentialProof>,
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
        let Some(action) = request.action.known() else {
            return PermissionDecision::Denied(PermissionDenyReason::UnknownAction);
        };

        match request.role {
            ConnectRole::PairedOwner => match request.credential {
                Some(proof) if proof.session_epoch() != 0 && proof.host_generation() != 0 => {
                    PermissionDecision::Allow
                }
                _ => PermissionDecision::Denied(PermissionDenyReason::DeviceCredentialRequired),
            },
            ConnectRole::Watcher { task_id } => {
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

    /// Evaluate a request only after revalidating a PairedOwner proof against
    /// the authoritative identity, vault, and active session epoch.
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
        }
        self.evaluate(request)
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
}
