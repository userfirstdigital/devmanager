//! Optional HTTP/JSON boundary for the proprietary Portal management API.
//!
//! This module deliberately does not participate in the MessagePack Connect
//! protocol.  Connect remains the local/open-source transport; this adapter is
//! an explicit, opt-in client for the Portal organization-management service.
//! Portal ids remain opaque strings at this boundary even though the current
//! Portal database happens to use UUID columns.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

pub const OPAQUE_ID_MAX_BYTES: usize = 256;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const PROMPT_BODY_MAX_BYTES: usize = 256 * 1024;

/// A wire id is intentionally not a UUID newtype.  Portal is the issuer and
/// may migrate its identifier format without requiring a DevManager release.
pub type PortalId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalAdapterError {
    InvalidBaseUrl(String),
    InvalidId {
        field: String,
        reason: &'static str,
    },
    InvalidTimestamp {
        field: String,
        value: String,
    },
    InvalidValue {
        field: String,
        reason: String,
    },
    UnknownEnum {
        field: String,
        value: String,
    },
    InvalidRawDefault,
    UnitOverflow {
        field: &'static str,
    },
    RequestTooLarge {
        bytes: usize,
    },
    ResponseTooLarge,
    Http {
        status: u16,
        code: Option<String>,
        message: String,
    },
    Transport(String),
    Serialization(String),
}

impl fmt::Display for PortalAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl(value) => write!(f, "invalid Portal base URL: {value}"),
            Self::InvalidId { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::InvalidTimestamp { field, .. } => write!(f, "invalid ISO timestamp in {field}"),
            Self::InvalidValue { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::UnknownEnum { field, value } => write!(f, "unknown {field}: {value}"),
            Self::InvalidRawDefault => write!(f, "raw sharing must remain disabled by default"),
            Self::UnitOverflow { field } => write!(f, "{field} is outside the local unit range"),
            Self::RequestTooLarge { bytes } => {
                write!(f, "Portal request is too large ({bytes} bytes)")
            }
            Self::ResponseTooLarge => write!(f, "Portal response is too large"),
            Self::Http {
                status,
                code,
                message,
            } => {
                if let Some(code) = code {
                    write!(f, "Portal HTTP {status} ({code}): {message}")
                } else {
                    write!(f, "Portal HTTP {status}: {message}")
                }
            }
            Self::Transport(message) => write!(f, "Portal transport failed: {message}"),
            Self::Serialization(message) => write!(f, "Portal JSON failed: {message}"),
        }
    }
}

impl std::error::Error for PortalAdapterError {}

pub fn validate_opaque_id(value: &str, field: &str) -> Result<(), PortalAdapterError> {
    if value.is_empty() || value.trim().is_empty() {
        return Err(PortalAdapterError::InvalidId {
            field: field.to_string(),
            reason: "must be non-empty",
        });
    }
    if value.len() > OPAQUE_ID_MAX_BYTES {
        return Err(PortalAdapterError::InvalidId {
            field: field.to_string(),
            reason: "exceeds the 256-byte bound",
        });
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(PortalAdapterError::InvalidId {
            field: field.to_string(),
            reason: "contains a control character",
        });
    }
    Ok(())
}

/// Validate an RFC3339-shaped timestamp without normalizing it.  Keeping the
/// original string is important for audit and signature verification.  The
/// Portal server performs the full calendar validation; this client rejects
/// malformed/control-bearing values before they enter local state.
pub fn validate_iso_timestamp(value: &str, field: &str) -> Result<(), PortalAdapterError> {
    let bytes = value.as_bytes();
    let digits = |slice: &[u8]| slice.iter().all(|byte| byte.is_ascii_digit());
    let date_time = bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && digits(&bytes[..4])
        && digits(&bytes[5..7])
        && digits(&bytes[8..10])
        && digits(&bytes[11..13])
        && digits(&bytes[14..16])
        && digits(&bytes[17..19]);
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        if index == fraction_start || index - fraction_start > 9 {
            return Err(PortalAdapterError::InvalidTimestamp {
                field: field.to_string(),
                value: value.to_string(),
            });
        }
    }
    let timezone = match bytes.get(index) {
        Some(b'Z') => index + 1 == bytes.len(),
        Some(b'+') | Some(b'-') => {
            index + 6 == bytes.len()
                && bytes[index + 3] == b':'
                && digits(&bytes[index + 1..index + 3])
                && digits(&bytes[index + 4..index + 6])
        }
        _ => false,
    };
    if !date_time || !timezone || value.chars().any(|character| character.is_control()) {
        return Err(PortalAdapterError::InvalidTimestamp {
            field: field.to_string(),
            value: value.to_string(),
        });
    }
    Ok(())
}

fn deserialize_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_opaque_id(&value, "Portal id").map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_optional_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            validate_opaque_id(&value, "Portal id").map_err(serde::de::Error::custom)?;
            Ok(value)
        })
        .transpose()
}

fn deserialize_iso<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_iso_timestamp(&value, "timestamp").map_err(serde::de::Error::custom)?;
    Ok(value)
}

fn deserialize_optional_iso<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            validate_iso_timestamp(&value, "timestamp").map_err(serde::de::Error::custom)?;
            Ok(value)
        })
        .transpose()
}

fn deserialize_false<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    if value {
        Err(serde::de::Error::custom("rawDefault must be false"))
    } else {
        Ok(false)
    }
}

fn deserialize_raw_off<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value != "off" {
        return Err(serde::de::Error::custom("rawDefault must be off"));
    }
    Ok(value)
}

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl std::str::FromStr for $name {
            type Err = PortalAdapterError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value { $($wire => Ok(Self::$variant),)+ _ => Err(PortalAdapterError::UnknownEnum { field: stringify!($name).to_string(), value: value.to_string() }) }
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where S: Serializer {
                serializer.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: serde::Deserializer<'de> {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
        impl $name { pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire,)+ } } }
    };
}

string_enum!(PortalOrgRole {
    Owner => "owner", Admin => "admin", Manager => "manager", Member => "member", PromptMaintainer => "prompt_maintainer"
});
string_enum!(PortalMembershipStatus {
    Pending => "pending", Enrolled => "enrolled", Revoked => "revoked", Unenrolled => "unenrolled"
});
string_enum!(PortalEnrollmentState {
    Personal => "personal", PendingHostConfirm => "pending_host_confirm", PendingOwnerAccept => "pending_owner_accept", Enrolled => "enrolled", Unenrolled => "unenrolled", Tombstoned => "tombstoned"
});
string_enum!(PortalGrantAccess { Watcher => "watcher", Collaborator => "collaborator", Owner => "owner" });
string_enum!(PortalRawSharingCeiling { None => "none", Metadata => "metadata", ApprovedRaw => "approved_raw" });
string_enum!(PortalUsageSource { ProviderReported => "provider_reported", ProviderQuoted => "provider_quoted", LocalEstimate => "local_estimate" });
string_enum!(PortalActionKind { DbSchemaInspect => "dbflow.schema.inspect", DbChangeApply => "dbflow.change.apply", EnvDiff => "env.diff", EnvApply => "env.apply" });
string_enum!(PortalActionRisk { Low => "low", Standard => "standard", Production => "production" });
string_enum!(PortalAdmissionStatus { Pending => "pending", Accepted => "accepted", Rejected => "rejected" });
string_enum!(PortalOutcomeStatus { Settled => "settled", Failed => "failed", Cancelled => "cancelled", Uncertain => "uncertain" });
string_enum!(PortalPromptStatus { Published => "published", Deprecated => "deprecated" });

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationGrant {
    #[serde(deserialize_with = "deserialize_id")]
    pub account_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub device_public_id: String,
    pub membership_role: PortalOrgRole,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub task_link_id: Option<String>,
    pub policy_revision: u32,
    #[serde(deserialize_with = "deserialize_iso")]
    pub expires_at: String,
    pub content_classes: Vec<String>,
    pub action_classes: Vec<String>,
    pub access: PortalGrantAccess,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedTaskLink {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub local_task_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub board_card_id: String,
    pub enrollment_state: PortalEnrollmentState,
    pub local_revision: u64,
    pub portal_revision: u64,
    pub metadata_policy_version: u64,
    #[serde(deserialize_with = "deserialize_id")]
    pub linked_by: String,
    #[serde(deserialize_with = "deserialize_iso")]
    pub linked_at: String,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub unlinked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMembershipDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub tenant_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub user_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    pub role: PortalOrgRole,
    pub status: PortalMembershipStatus,
    pub display_label: Option<String>,
    pub policy_revision: u32,
    pub capabilities: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub enrolled_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub last_seen_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub revoked_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub grant_expires_at: Option<String>,
    pub audit: MembershipAudit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MembershipAudit {
    pub last_event: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub last_event_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectHostDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub device_public_id: String,
    pub display_label: String,
    pub status: PortalMembershipStatus,
    pub policy_revision: u32,
    pub signed_policy_revision: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub enrolled_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub last_seen_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub revoked_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub host_confirmed_at: Option<String>,
    #[serde(default)]
    pub temporary_seam: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedTaskDto {
    #[serde(flatten)]
    pub link: ManagedTaskLink,
    pub accepted_at: Option<String>,
    pub title_conflict: Option<TitleConflict>,
    pub local_projection: LocalTaskProjection,
    pub board_card: Option<BoardCardProjection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleConflict {
    pub field: String,
    pub local_value: String,
    pub portal_value: String,
    pub local_revision: u64,
    pub portal_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalTaskProjection {
    pub title: Option<String>,
    pub status: Option<String>,
    pub attention: Option<String>,
    pub provider_kind: Option<String>,
    pub provider_state: Option<String>,
    pub local_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardCardProjection {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    pub title: String,
    pub column_key: String,
    pub assigned_to: Option<String>,
    pub target_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLiveViewDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub link_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub managed_task_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    pub fields: serde_json::Map<String, Value>,
    pub grant: LiveViewGrant,
    pub granted_content_classes: Vec<String>,
    #[serde(deserialize_with = "deserialize_false")]
    pub raw_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveViewGrant {
    pub access: PortalGrantAccess,
    #[serde(deserialize_with = "deserialize_iso")]
    pub expires_at: String,
    pub policy_revision: u32,
    pub content_classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetHostDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    pub display_label: String,
    pub attention: String,
    pub host_online: bool,
    pub host_stale: bool,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub last_activity_at: Option<String>,
    pub usage_labels: FleetUsageLabels,
    pub active_session_rule: String,
    pub labels: FleetLabels,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetUsageLabels {
    pub tokens: Option<PortalUsageSource>,
    pub quota: Option<PortalUsageSource>,
    pub quoted_cost: Option<PortalUsageSource>,
    pub local_estimate: Option<PortalUsageSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetLabels {
    pub active_session_time: String,
    pub not_hours_worked: bool,
    pub not_payroll: bool,
    pub not_productivity: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPromptDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    pub namespace: String,
    pub name: String,
    pub status: PortalPromptStatus,
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub current_version_id: Option<String>,
    pub tags: Vec<String>,
    #[serde(deserialize_with = "deserialize_id")]
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPromptVersionDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub prompt_id: String,
    pub version: u32,
    #[serde(deserialize_with = "deserialize_id")]
    pub author_user_id: String,
    pub title: String,
    pub body: String,
    pub content_hash: String,
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub supersedes_version_id: Option<String>,
    #[serde(deserialize_with = "deserialize_iso")]
    pub published_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPromptDiffDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub from_version_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub to_version_id: String,
    pub title_changed: bool,
    pub body_changed: bool,
    pub from_hash: String,
    pub to_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPromptChainLinkDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub chain_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub prompt_version_id: String,
    pub position: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgPromptChainDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    pub name: String,
    pub revision: u64,
    #[serde(deserialize_with = "deserialize_id")]
    pub created_by: String,
    pub links: Vec<OrgPromptChainLinkDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalActionDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub request_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    pub project_binding: String,
    pub action_kind: PortalActionKind,
    pub action_version: u16,
    pub payload: serde_json::Map<String, Value>,
    pub risk: PortalActionRisk,
    pub required_approvals: Vec<String>,
    pub approvals: Vec<String>,
    pub expected_fingerprint: Option<String>,
    pub expected_revision: Option<String>,
    #[serde(deserialize_with = "deserialize_iso")]
    pub expires_at: String,
    pub signature: String,
    pub admission_status: PortalAdmissionStatus,
    pub outcome_status: Option<PortalOutcomeStatus>,
    pub operation_id: Option<String>,
    pub local_actor: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub target_fingerprint: Option<String>,
    pub redacted_result: Option<serde_json::Map<String, Value>>,
    pub artifact_refs: Vec<String>,
    pub error_class: Option<String>,
    #[serde(deserialize_with = "deserialize_id")]
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalActionReceiptDto {
    #[serde(deserialize_with = "deserialize_id")]
    pub request_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub operation_id: String,
    pub admission: PortalAdmissionStatus,
    pub outcome: Option<PortalOutcomeStatus>,
    pub local_actor: String,
    #[serde(deserialize_with = "deserialize_iso")]
    pub started_at: String,
    #[serde(default, deserialize_with = "deserialize_optional_iso")]
    pub ended_at: Option<String>,
    pub target_fingerprint: Option<String>,
    pub redacted_result: Option<serde_json::Map<String, Value>>,
    pub artifact_refs: Vec<String>,
    pub error_class: Option<String>,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentPreviewDto {
    pub metadata_classes: Vec<String>,
    pub raw_content_classes: Vec<String>,
    #[serde(deserialize_with = "deserialize_raw_off")]
    pub raw_default: String,
    pub retention_days: u32,
    pub idle_interval_minutes: u32,
    pub viewers: Vec<EnrollmentViewer>,
    pub manager_visibility: String,
    pub raw_sharing_ceiling: PortalRawSharingCeiling,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentViewer {
    #[serde(deserialize_with = "deserialize_id")]
    pub user_id: String,
    pub role: PortalOrgRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationPolicyDto {
    pub revision: String,
    pub enrollment_required: bool,
    pub metadata_fields: Vec<String>,
    pub retention_days: u32,
    pub idle_interval_minutes: u32,
    pub raw_sharing_ceiling: PortalRawSharingCeiling,
    pub local_action_approval_required: bool,
    pub auto_accept_managed_tasks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPolicyUnits {
    pub revision: u32,
    pub retention_ms: u64,
    pub idle_interval_ms: u64,
    pub enrollment_required: bool,
    pub metadata_fields: Vec<String>,
    pub raw_sharing_ceiling: PortalRawSharingCeiling,
    pub local_action_approval_required: bool,
    pub auto_accept_managed_tasks: bool,
}

impl OrganizationPolicyDto {
    pub fn to_local_units(&self) -> Result<LocalPolicyUnits, PortalAdapterError> {
        let revision =
            self.revision
                .parse::<u32>()
                .map_err(|_| PortalAdapterError::InvalidValue {
                    field: "revision".into(),
                    reason: "must fit u32".into(),
                })?;
        Ok(LocalPolicyUnits {
            revision,
            retention_ms: days_to_ms(self.retention_days)?,
            idle_interval_ms: minutes_to_ms(self.idle_interval_minutes)?,
            enrollment_required: self.enrollment_required,
            metadata_fields: self.metadata_fields.clone(),
            raw_sharing_ceiling: self.raw_sharing_ceiling,
            local_action_approval_required: self.local_action_approval_required,
            auto_accept_managed_tasks: self.auto_accept_managed_tasks,
        })
    }
}

pub fn days_to_ms(days: u32) -> Result<u64, PortalAdapterError> {
    u64::from(days)
        .checked_mul(86_400_000)
        .ok_or(PortalAdapterError::UnitOverflow {
            field: "retentionDays",
        })
}

pub fn minutes_to_ms(minutes: u32) -> Result<u64, PortalAdapterError> {
    u64::from(minutes)
        .checked_mul(60_000)
        .ok_or(PortalAdapterError::UnitOverflow {
            field: "idleIntervalMinutes",
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentPreviewRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_card_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishPromptRequest {
    pub namespace: String,
    pub name: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_current_version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePromptVersionRequest {
    pub title: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_current_version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptChainRequest {
    pub name: String,
    pub version_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalActionRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub host_id: String,
    pub project_binding: String,
    pub action_kind: PortalActionKind,
    pub action_version: u16,
    pub payload: serde_json::Map<String, Value>,
    pub risk: PortalActionRisk,
    pub expected_fingerprint: String,
    pub expected_revision: Option<String>,
    pub expires_at: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalActionReceiptRequest {
    pub request_id: String,
    pub operation_id: String,
    pub admission: PortalAdmissionStatus,
    pub outcome: Option<PortalOutcomeStatus>,
    pub local_actor: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub target_fingerprint: String,
    pub redacted_result: Option<serde_json::Map<String, Value>>,
    pub artifact_refs: Vec<String>,
    pub error_class: Option<String>,
    pub signature: String,
}

/// Metadata attached to an explicit Portal request.  The body digest and
/// optional HMAC cover canonical JSON bytes, not raw content or credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalRequestMetadata {
    pub request_id: String,
    pub idempotency_key: String,
    pub client_version: String,
    pub issued_at: String,
    pub body_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl PortalRequestMetadata {
    pub fn for_body(
        request_id: &str,
        idempotency_key: &str,
        client_version: &str,
        issued_at: &str,
        body: &[u8],
    ) -> Result<Self, PortalAdapterError> {
        validate_opaque_id(request_id, "requestId")?;
        validate_opaque_id(idempotency_key, "idempotencyKey")?;
        validate_iso_timestamp(issued_at, "issuedAt")?;
        Ok(Self {
            request_id: request_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            client_version: client_version.to_string(),
            issued_at: issued_at.to_string(),
            body_sha256: hex_digest(body),
            signature: None,
        })
    }

    pub fn with_hmac(
        mut self,
        secret: &[u8],
        canonical_body: &[u8],
    ) -> Result<Self, PortalAdapterError> {
        self.signature = Some(hmac_signature(secret, canonical_body)?);
        Ok(self)
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hmac_signature(secret: &[u8], canonical_bytes: &[u8]) -> Result<String, PortalAdapterError> {
    let mut mac =
        HmacSha256::new_from_slice(secret).map_err(|_| PortalAdapterError::InvalidValue {
            field: "signatureSecret".into(),
            reason: "invalid HMAC key".into(),
        })?;
    mac.update(canonical_bytes);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

const PROHIBITED_TOKENS: &[&str] = &[
    "credentials",
    "env_values",
    "password",
    "secret",
    "token",
    "apikey",
    "connectionstring",
    "raw_prompt",
    "raw_response",
    "terminal",
    "browser",
    "recording",
    "file_body",
    "full_diff",
    "screenshot",
    "screenshots",
];

pub fn reject_prohibited_fields(value: &Value) -> Result<(), PortalAdapterError> {
    fn walk(value: &Value, path: &str) -> Result<(), PortalAdapterError> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let normalized = key.to_ascii_lowercase().replace(['-', ' '], "_");
                    if PROHIBITED_TOKENS
                        .iter()
                        .any(|token| normalized == *token || normalized.contains(token))
                    {
                        return Err(PortalAdapterError::InvalidValue {
                            field: format!("{path}{key}"),
                            reason: "raw or secret content is not accepted by the metadata adapter"
                                .into(),
                        });
                    }
                    walk(child, &format!("{path}{key}."))?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}{index}."))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, "")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceMetadataBundle {
    #[serde(deserialize_with = "deserialize_id")]
    pub evidence_bundle_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub organization_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub host_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub source_device_id: String,
    pub created_at: String,
    pub content_hash: String,
    pub artifact_refs: Vec<String>,
    pub metadata: serde_json::Map<String, Value>,
    pub signature: String,
}

impl EvidenceMetadataBundle {
    pub fn validate(&self) -> Result<(), PortalAdapterError> {
        validate_iso_timestamp(&self.created_at, "createdAt")?;
        reject_prohibited_fields(&Value::Object(self.metadata.clone()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceImportRequest {
    pub bundle: EvidenceMetadataBundle,
    #[serde(default)]
    pub media_bindings: Vec<MediaBinding>,
    #[serde(default)]
    pub review_receipt: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaBinding {
    #[serde(deserialize_with = "deserialize_id")]
    pub media_id: String,
    #[serde(deserialize_with = "deserialize_id")]
    pub server_object_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PortalEnvelope<T> {
    data: T,
}

#[derive(Debug, Clone, Deserialize)]
struct PortalErrorBody {
    code: Option<String>,
    message: Option<String>,
}

/// Synchronous, bounded, bearer-authenticated Portal client.  It is never
/// invoked by the internal Connect transport and performs no implicit login.
pub struct PortalManagementClient {
    endpoint: Url,
    token: String,
    agent: ureq::Agent,
}

impl fmt::Debug for PortalManagementClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalManagementClient")
            .field("endpoint", &self.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl PortalManagementClient {
    pub fn new(
        base_url: &str,
        bearer_token: impl Into<String>,
    ) -> Result<Self, PortalAdapterError> {
        let mut endpoint = Url::parse(base_url)
            .map_err(|_| PortalAdapterError::InvalidBaseUrl(base_url.to_string()))?;
        if endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host_str().is_none()
        {
            return Err(PortalAdapterError::InvalidBaseUrl(base_url.to_string()));
        }
        let path = endpoint.path().trim_end_matches('/');
        let path = if path.ends_with("/api/devmanager") {
            path.to_string()
        } else if path.ends_with("/api") {
            format!("{path}/devmanager")
        } else {
            format!("{path}/api/devmanager")
        };
        endpoint.set_path(&path);
        let token = bearer_token.into();
        if token.trim().is_empty() {
            return Err(PortalAdapterError::InvalidValue {
                field: "bearerToken".into(),
                reason: "must be non-empty".into(),
            });
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(20)))
            .max_redirects(0)
            .proxy(None)
            .build()
            .into();
        Ok(Self {
            endpoint,
            token,
            agent,
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Url, PortalAdapterError> {
        let mut url = self.endpoint.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| PortalAdapterError::InvalidBaseUrl(url.to_string()))?;
            path.pop_if_empty();
            path.extend(segments.iter().copied());
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            pairs.clear().extend_pairs(query.iter().copied());
        }
        Ok(url)
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<T, PortalAdapterError> {
        let url = self.url(segments, query)?.to_string();
        let authorization = format!("Bearer {}", self.token);
        let result = match method {
            "GET" => self
                .agent
                .get(&url)
                .header("Accept", "application/json")
                .header("Authorization", authorization.as_str())
                .call(),
            "POST" | "PUT" => {
                let body = body.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                let encoded = serde_json::to_vec(&body)
                    .map_err(|error| PortalAdapterError::Serialization(error.to_string()))?;
                if encoded.len() > MAX_REQUEST_BYTES {
                    return Err(PortalAdapterError::RequestTooLarge {
                        bytes: encoded.len(),
                    });
                }
                if method == "POST" {
                    self.agent
                        .post(&url)
                        .header("Accept", "application/json")
                        .header("Authorization", authorization.as_str())
                        .header("Content-Type", "application/json")
                        .send(encoded)
                } else {
                    self.agent
                        .put(&url)
                        .header("Accept", "application/json")
                        .header("Authorization", authorization.as_str())
                        .header("Content-Type", "application/json")
                        .send(encoded)
                }
            }
            _ => unreachable!(),
        };
        let mut response =
            result.map_err(|error| PortalAdapterError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => PortalAdapterError::ResponseTooLarge,
                other => PortalAdapterError::Transport(other.to_string()),
            })?;
        if !(200..300).contains(&status) {
            let parsed = serde_json::from_slice::<PortalErrorBody>(&bytes).ok();
            return Err(PortalAdapterError::Http {
                status,
                code: parsed.as_ref().and_then(|value| value.code.clone()),
                message: parsed
                    .and_then(|value| value.message)
                    .unwrap_or_else(|| "Portal request failed".into()),
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| PortalAdapterError::Serialization(error.to_string()))
    }

    fn get_data<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, PortalAdapterError> {
        self.request::<PortalEnvelope<T>>("GET", segments, query, None)
            .map(|value| value.data)
    }
    fn post_data<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        body: Value,
    ) -> Result<T, PortalAdapterError> {
        self.request::<PortalEnvelope<T>>("POST", segments, &[], Some(body))
            .map(|value| value.data)
    }
    fn put_data<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        body: Value,
    ) -> Result<T, PortalAdapterError> {
        self.request::<PortalEnvelope<T>>("PUT", segments, &[], Some(body))
            .map(|value| value.data)
    }

    pub fn get_policy(&self) -> Result<OrganizationPolicyDto, PortalAdapterError> {
        self.get_data(&["policy"], &[])
    }
    pub fn update_policy(
        &self,
        policy: &OrganizationPolicyDto,
    ) -> Result<OrganizationPolicyDto, PortalAdapterError> {
        self.put_data(
            &["policy"],
            serde_json::to_value(policy)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn enrollment_preview(
        &self,
        request: &EnrollmentPreviewRequest,
    ) -> Result<EnrollmentPreviewDto, PortalAdapterError> {
        self.post_data(
            &["tasks", "enrollment-preview"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn list_hosts(&self) -> Result<Vec<HostMembershipDto>, PortalAdapterError> {
        self.get_data(&["hosts"], &[])
    }
    pub fn get_host(&self, host_id: &str) -> Result<Value, PortalAdapterError> {
        validate_opaque_id(host_id, "hostId")?;
        self.request("GET", &["hosts", host_id], &[], None)
    }
    pub fn list_tasks(&self) -> Result<Vec<ManagedTaskDto>, PortalAdapterError> {
        self.get_data(&["tasks"], &[])
    }
    pub fn get_task(&self, link_id: &str) -> Result<ManagedTaskDto, PortalAdapterError> {
        validate_opaque_id(link_id, "linkId")?;
        self.get_data(&["tasks", link_id], &[])
    }
    pub fn get_task_live_view(&self, link_id: &str) -> Result<TaskLiveViewDto, PortalAdapterError> {
        validate_opaque_id(link_id, "linkId")?;
        self.get_data(&["tasks", link_id, "live"], &[])
    }
    pub fn list_fleet(&self) -> Result<Vec<FleetHostDto>, PortalAdapterError> {
        self.get_data(&["fleet"], &[])
    }
    pub fn list_prompts(
        &self,
        query: &[(&str, &str)],
    ) -> Result<Vec<OrgPromptDto>, PortalAdapterError> {
        self.get_data(&["prompts"], query)
    }
    pub fn publish_prompt(
        &self,
        request: &PublishPromptRequest,
    ) -> Result<Value, PortalAdapterError> {
        self.post_data(
            &["prompts"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn list_prompt_versions(
        &self,
        prompt_id: &str,
    ) -> Result<Vec<OrgPromptVersionDto>, PortalAdapterError> {
        validate_opaque_id(prompt_id, "promptId")?;
        self.get_data(&["prompts", prompt_id, "versions"], &[])
    }
    pub fn create_prompt_version(
        &self,
        prompt_id: &str,
        request: &CreatePromptVersionRequest,
    ) -> Result<OrgPromptVersionDto, PortalAdapterError> {
        validate_opaque_id(prompt_id, "promptId")?;
        self.post_data(
            &["prompts", prompt_id, "versions"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn diff_prompt(
        &self,
        from_version_id: &str,
        to_version_id: &str,
    ) -> Result<OrgPromptDiffDto, PortalAdapterError> {
        validate_opaque_id(from_version_id, "fromVersionId")?;
        validate_opaque_id(to_version_id, "toVersionId")?;
        self.get_data(&["prompts", "diff", from_version_id, to_version_id], &[])
    }
    pub fn list_prompt_chains(&self) -> Result<Vec<OrgPromptChainDto>, PortalAdapterError> {
        self.get_data(&["prompt-chains"], &[])
    }
    pub fn get_prompt_chain(
        &self,
        chain_id: &str,
    ) -> Result<OrgPromptChainDto, PortalAdapterError> {
        validate_opaque_id(chain_id, "chainId")?;
        self.get_data(&["prompt-chains", chain_id], &[])
    }
    pub fn create_prompt_chain(
        &self,
        request: &PromptChainRequest,
    ) -> Result<OrgPromptChainDto, PortalAdapterError> {
        self.post_data(
            &["prompt-chains"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn update_prompt_chain(
        &self,
        chain_id: &str,
        request: &PromptChainRequest,
    ) -> Result<OrgPromptChainDto, PortalAdapterError> {
        validate_opaque_id(chain_id, "chainId")?;
        self.put_data(
            &["prompt-chains", chain_id],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn list_actions(
        &self,
        host_id: Option<&str>,
    ) -> Result<Vec<LocalActionDto>, PortalAdapterError> {
        let query = host_id
            .map(|id| {
                validate_opaque_id(id, "hostId")?;
                Ok::<_, PortalAdapterError>(vec![("hostId", id)])
            })
            .transpose()?
            .unwrap_or_default();
        self.get_data(&["actions"], &query)
    }
    pub fn create_action(
        &self,
        request: &LocalActionRequestDto,
    ) -> Result<LocalActionDto, PortalAdapterError> {
        self.post_data(
            &["actions"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn approve_action(&self, id: &str) -> Result<LocalActionDto, PortalAdapterError> {
        validate_opaque_id(id, "requestId")?;
        self.post_data(
            &["actions", id, "approve"],
            Value::Object(serde_json::Map::new()),
        )
    }
    pub fn cancel_action(
        &self,
        id: &str,
        reason: Option<&str>,
    ) -> Result<Value, PortalAdapterError> {
        validate_opaque_id(id, "requestId")?;
        let mut body = serde_json::Map::new();
        if let Some(reason) = reason {
            body.insert("reason".into(), Value::String(reason.into()));
        }
        self.post_data(&["actions", id, "cancel"], Value::Object(body))
    }
    pub fn receive_action_receipt(
        &self,
        request_id: &str,
        request: &LocalActionReceiptRequest,
    ) -> Result<LocalActionReceiptDto, PortalAdapterError> {
        validate_opaque_id(request_id, "requestId")?;
        self.post_data(
            &["actions", request_id, "receipt"],
            serde_json::to_value(request)
                .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?,
        )
    }
    pub fn import_evidence(
        &self,
        request: &EvidenceImportRequest,
    ) -> Result<Value, PortalAdapterError> {
        request.bundle.validate()?;
        let value = serde_json::to_value(request)
            .map_err(|e| PortalAdapterError::Serialization(e.to_string()))?;
        reject_prohibited_fields(&value)?;
        self.post_data(&["evidence"], value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_deserializes_portal_camel_case_and_converts_units() {
        let policy: OrganizationPolicyDto = serde_json::from_value(serde_json::json!({
            "revision": "7", "enrollmentRequired": true, "metadataFields": ["status"],
            "retentionDays": 90, "idleIntervalMinutes": 15, "rawSharingCeiling": "none",
            "localActionApprovalRequired": true, "autoAcceptManagedTasks": false
        }))
        .unwrap();
        let local = policy.to_local_units().unwrap();
        assert_eq!(local.revision, 7);
        assert_eq!(local.retention_ms, 90 * 86_400_000);
        assert_eq!(local.idle_interval_ms, 15 * 60_000);
    }

    #[test]
    fn live_view_rejects_raw_default() {
        let value = serde_json::json!({"linkId":"l","managedTaskId":"m","organizationId":"o","fields":{},"grant":{"access":"watcher","expiresAt":"2026-08-12T00:00:00Z","policyRevision":1,"contentClasses":["metadata"]},"grantedContentClasses":["metadata"],"rawDefault":true});
        assert!(serde_json::from_value::<TaskLiveViewDto>(value).is_err());
    }

    #[test]
    fn metadata_digest_and_hmac_are_deterministic() {
        let body = br#"{"safe":true}"#;
        let metadata = PortalRequestMetadata::for_body(
            "request-1",
            "idem-1",
            "0.4.2",
            "2026-08-12T00:00:00Z",
            body,
        )
        .unwrap();
        assert_eq!(metadata.body_sha256, hex_digest(body));
        assert_eq!(
            hmac_signature(b"secret", body).unwrap(),
            hmac_signature(b"secret", body).unwrap()
        );
    }

    #[test]
    fn evidence_rejects_raw_nested_fields() {
        assert!(
            reject_prohibited_fields(&serde_json::json!({"metadata":{"terminal":"raw"}})).is_err()
        );
    }
}
