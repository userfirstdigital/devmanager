//! Task-scoped invitation store, distinct from long-lived owner pairing.
//!
//! Pairing codes are never reused or rotated by invite issuance. The local host
//! is the final authorization authority. Revocation/expiry leave audit metadata
//! only; the plaintext secret is never retained.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::canonical;
use crate::domain::id::{TaskId, TaskInviteId};
use crate::domain::task::TaskLifecycle;

use super::identity::hex_encode;
use super::permission::{ActionId, ConnectRole, KnownAction};

pub const MAX_TASK_INVITES: usize = 32;
pub const MAX_INVITE_NICKNAME_BYTES: usize = 64;
pub const INVITE_SECRET_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    NicknameRequired,
    NicknameTooLong,
    SecretRequired,
    LimitExceeded,
    UnknownInvite,
    Expired,
    Revoked,
    AlreadyRedeemed,
    RedemptionExhausted,
    TaskClosed,
    TaskScopeMismatch,
    OwnerRoleForbidden,
    PairingCodeReuseForbidden,
    HostMismatch,
    DeviceAlreadyBound,
}

impl fmt::Display for InviteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NicknameRequired => "invite nickname is required",
            Self::NicknameTooLong => "invite nickname exceeds the bounded size",
            Self::SecretRequired => "invite secret is required",
            Self::LimitExceeded => "task invite store is at capacity",
            Self::UnknownInvite => "unknown task invite",
            Self::Expired => "task invite has expired",
            Self::Revoked => "task invite has been revoked",
            Self::AlreadyRedeemed => "single-use invite was already redeemed",
            Self::RedemptionExhausted => "invite redemption limit was reached",
            Self::TaskClosed => "the granted Task is closed",
            Self::TaskScopeMismatch => "invite is not valid for the requested Task",
            Self::OwnerRoleForbidden => "task invites cannot mint owner-device authority",
            Self::PairingCodeReuseForbidden => "task invites cannot reuse the host pairing code",
            Self::HostMismatch => "invite is pinned to a different host identity",
            Self::DeviceAlreadyBound => "invite is already bound to another device",
        })
    }
}

impl std::error::Error for InviteError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    TaskMetadata,
    Presence,
    OperationProgress,
    Transcript,
    PersonalPrompts,
    Configuration,
    PairedDevices,
    Secrets,
}

impl ContentClass {
    pub const fn is_guest_forbidden(self) -> bool {
        matches!(
            self,
            Self::PersonalPrompts | Self::Configuration | Self::PairedDevices | Self::Secrets
        )
    }

    pub const fn is_raw_transcript(self) -> bool {
        matches!(self, Self::Transcript)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteUsePolicy {
    SingleUse,
    MultiUse { max_redemptions: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteRole {
    Watcher,
    Collaborator,
}

impl InviteRole {
    pub fn as_connect_role(self, task_id: TaskId) -> ConnectRole {
        match self {
            Self::Watcher => ConnectRole::Watcher { task_id },
            Self::Collaborator => ConnectRole::Collaborator { task_id },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PinnedHostPublicId([u8; 16]);

impl PinnedHostPublicId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for PinnedHostPublicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PinnedHostPublicId(redacted)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedeemedDevicePublicId([u8; 16]);

impl RedeemedDevicePublicId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for RedeemedDevicePublicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedeemedDevicePublicId(redacted)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedInvite {
    pub invite_id: TaskInviteId,
    pub plaintext_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteAuditEvent {
    pub invite_id: TaskInviteId,
    pub task_id: TaskId,
    pub kind: InviteAuditKind,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteAuditKind {
    Issued,
    Redeemed,
    Expired,
    Revoked,
}

#[derive(Clone, PartialEq, Eq)]
struct InviteRecord {
    invite_id: TaskInviteId,
    task_id: TaskId,
    nickname: String,
    role: InviteRole,
    allowed_actions: BTreeSet<ActionId>,
    allowed_content: BTreeSet<ContentClass>,
    use_policy: InviteUsePolicy,
    secret_hash: [u8; 32],
    created_at_ms: i64,
    expires_at_ms: i64,
    revoked_at_ms: Option<i64>,
    redemptions: u32,
    bound_device: Option<RedeemedDevicePublicId>,
    pinned_host: PinnedHostPublicId,
}

impl fmt::Debug for InviteRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InviteRecord")
            .field("invite_id", &self.invite_id)
            .field("task_id", &self.task_id)
            .field("role", &self.role)
            .field("revoked", &self.revoked_at_ms.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteGrantView {
    pub invite_id: TaskInviteId,
    pub task_id: TaskId,
    pub nickname: String,
    pub role: InviteRole,
    pub allowed_actions: BTreeSet<ActionId>,
    pub allowed_content: BTreeSet<ContentClass>,
    pub bound_device: Option<RedeemedDevicePublicId>,
}

/// Separate invitation store. It never writes pairing codes or owner devices.
#[derive(Debug, Clone, Default)]
pub struct TaskInviteStore {
    invites: BTreeMap<TaskInviteId, InviteRecord>,
    audit: Vec<InviteAuditEvent>,
}

impl TaskInviteStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.invites.is_empty()
    }

    pub fn collaboration_visible(&self) -> bool {
        !self.invites.is_empty()
    }

    pub fn audit_events(&self) -> &[InviteAuditEvent] {
        &self.audit
    }

    pub fn issue(
        &mut self,
        task_id: TaskId,
        nickname: impl Into<String>,
        role: InviteRole,
        use_policy: InviteUsePolicy,
        created_at_ms: i64,
        expires_at_ms: i64,
        pinned_host: PinnedHostPublicId,
        secret: &[u8],
        pairing_code: Option<&str>,
    ) -> Result<IssuedInvite, InviteError> {
        if pairing_code.is_some() {
            return Err(InviteError::PairingCodeReuseForbidden);
        }
        if self.invites.len() >= MAX_TASK_INVITES {
            return Err(InviteError::LimitExceeded);
        }
        if secret.is_empty() {
            return Err(InviteError::SecretRequired);
        }
        let nickname = canonical::canonicalize(nickname.into()).ok_or(InviteError::NicknameRequired)?;
        if nickname.len() > MAX_INVITE_NICKNAME_BYTES {
            return Err(InviteError::NicknameTooLong);
        }
        let invite_id = TaskInviteId::new();
        let record = InviteRecord {
            invite_id,
            task_id,
            nickname,
            role,
            allowed_actions: default_actions(role),
            allowed_content: default_content(role),
            use_policy,
            secret_hash: hash_secret(secret),
            created_at_ms,
            expires_at_ms,
            revoked_at_ms: None,
            redemptions: 0,
            bound_device: None,
            pinned_host,
        };
        self.invites.insert(invite_id, record);
        self.audit.push(InviteAuditEvent {
            invite_id,
            task_id,
            kind: InviteAuditKind::Issued,
            at_ms: created_at_ms,
        });
        Ok(IssuedInvite {
            invite_id,
            plaintext_secret: hex_encode(secret),
        })
    }

    pub fn redeem(
        &mut self,
        invite_id: TaskInviteId,
        secret: &[u8],
        task_id: TaskId,
        lifecycle: TaskLifecycle,
        now_ms: i64,
        host: PinnedHostPublicId,
        device: RedeemedDevicePublicId,
    ) -> Result<InviteGrantView, InviteError> {
        let record = self
            .invites
            .get_mut(&invite_id)
            .ok_or(InviteError::UnknownInvite)?;
        if record.task_id != task_id {
            return Err(InviteError::TaskScopeMismatch);
        }
        if record.pinned_host != host {
            return Err(InviteError::HostMismatch);
        }
        if record.secret_hash != hash_secret(secret) {
            return Err(InviteError::UnknownInvite);
        }
        check_live(record, lifecycle, now_ms)?;
        match record.use_policy {
            InviteUsePolicy::SingleUse if record.redemptions > 0 => {
                return Err(InviteError::AlreadyRedeemed);
            }
            InviteUsePolicy::MultiUse { max_redemptions }
                if record.redemptions >= max_redemptions =>
            {
                return Err(InviteError::RedemptionExhausted);
            }
            _ => {}
        }
        if let Some(bound) = record.bound_device {
            if bound != device {
                return Err(InviteError::DeviceAlreadyBound);
            }
        } else {
            record.bound_device = Some(device);
        }
        record.redemptions = record.redemptions.saturating_add(1);
        self.audit.push(InviteAuditEvent {
            invite_id,
            task_id,
            kind: InviteAuditKind::Redeemed,
            at_ms: now_ms,
        });
        Ok(view(self.invites.get(&invite_id).expect("just redeemed")))
    }

    pub fn revoke(
        &mut self,
        invite_id: TaskInviteId,
        now_ms: i64,
    ) -> Result<InviteAuditEvent, InviteError> {
        let record = self
            .invites
            .get_mut(&invite_id)
            .ok_or(InviteError::UnknownInvite)?;
        record.revoked_at_ms = Some(now_ms);
        let event = InviteAuditEvent {
            invite_id,
            task_id: record.task_id,
            kind: InviteAuditKind::Revoked,
            at_ms: now_ms,
        };
        self.audit.push(event.clone());
        Ok(event)
    }

    pub fn authorize(
        &self,
        invite_id: TaskInviteId,
        task_id: TaskId,
        lifecycle: TaskLifecycle,
        now_ms: i64,
        action: ActionId,
        content: ContentClass,
    ) -> Result<InviteGrantView, InviteError> {
        let record = self
            .invites
            .get(&invite_id)
            .ok_or(InviteError::UnknownInvite)?;
        if record.task_id != task_id {
            return Err(InviteError::TaskScopeMismatch);
        }
        check_live(record, lifecycle, now_ms)?;
        if content.is_guest_forbidden() || !record.allowed_content.contains(&content) {
            return Err(InviteError::TaskScopeMismatch);
        }
        if !record.allowed_actions.contains(&action) {
            return Err(InviteError::TaskScopeMismatch);
        }
        Ok(view(record))
    }

    pub fn has_reusable_plaintext_secret(&self, invite_id: TaskInviteId) -> bool {
        let _ = invite_id;
        false
    }

    pub fn grant(&self, invite_id: TaskInviteId) -> Option<InviteGrantView> {
        self.invites.get(&invite_id).map(view)
    }
}

fn check_live(
    record: &InviteRecord,
    lifecycle: TaskLifecycle,
    now_ms: i64,
) -> Result<(), InviteError> {
    if record.revoked_at_ms.is_some() {
        return Err(InviteError::Revoked);
    }
    if now_ms > record.expires_at_ms {
        return Err(InviteError::Expired);
    }
    if !matches!(lifecycle, TaskLifecycle::Open) {
        return Err(InviteError::TaskClosed);
    }
    Ok(())
}

fn view(record: &InviteRecord) -> InviteGrantView {
    InviteGrantView {
        invite_id: record.invite_id,
        task_id: record.task_id,
        nickname: record.nickname.clone(),
        role: record.role,
        allowed_actions: record.allowed_actions.clone(),
        allowed_content: record.allowed_content.clone(),
        bound_device: record.bound_device,
    }
}

fn hash_secret(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

fn default_actions(role: InviteRole) -> BTreeSet<ActionId> {
    let mut actions = BTreeSet::from([
        ActionId::READ_TASK,
        ActionId::READ_PRESENCE,
        ActionId::READ_OPERATION,
    ]);
    if matches!(role, InviteRole::Collaborator) {
        actions.insert(ActionId::MUTATE_TASK);
        actions.insert(ActionId::SEND_PROMPT);
        actions.insert(ActionId::ANSWER_REQUEST);
        actions.insert(ActionId::TERMINAL_INPUT);
        actions.insert(ActionId::BROWSER_COMMAND);
    }
    actions
}

fn default_content(role: InviteRole) -> BTreeSet<ContentClass> {
    let _ = role;
    BTreeSet::from([
        ContentClass::TaskMetadata,
        ContentClass::Presence,
        ContentClass::OperationProgress,
    ])
}

pub fn guest_may_perform(role: InviteRole, action: KnownAction) -> bool {
    if action.is_owner_only() {
        return false;
    }
    match role {
        InviteRole::Watcher => !action.is_mutating(),
        InviteRole::Collaborator => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_secret_is_not_retained_and_pairing_code_is_rejected() {
        let mut store = TaskInviteStore::new();
        let task = TaskId::new();
        let host = PinnedHostPublicId::from_bytes([7; 16]);
        assert!(store
            .issue(
                task,
                "review",
                InviteRole::Watcher,
                InviteUsePolicy::SingleUse,
                1,
                100,
                host,
                b"secret",
                Some("ABCD2345"),
            )
            .is_err());
        let issued = store
            .issue(
                task,
                "review",
                InviteRole::Watcher,
                InviteUsePolicy::SingleUse,
                1,
                100,
                host,
                b"secret",
                None,
            )
            .unwrap();
        assert!(!store.has_reusable_plaintext_secret(issued.invite_id));
        assert!(store.collaboration_visible());
        assert!(!issued.plaintext_secret.is_empty());
    }
}
