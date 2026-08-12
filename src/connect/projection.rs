//! Outbound Connect projection privacy.
//!
//! Every field is classified before serialization. Unknown fields are denied.
//! Personal Tasks stay local-only until deliberately enrolled. Raw transcript
//! is never projected by default.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::id::TaskId;

use super::envelope::ConnectPrivacyClass;
use super::invites::ContentClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionDenyReason {
    UnknownField,
    PersonalNotEnrolled,
    GrantMissing,
    RawContentDenied,
    LocalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundField {
    TaskId,
    HostId,
    AttentionKind,
    SafeTitle,
    Revision,
    Lifecycle,
    PresenceHint,
    OperationProgress,
    Transcript,
    PromptBody,
    ResponseBody,
    TerminalBytes,
    BrowserDom,
    DiffHunk,
    FileContent,
    PersonalPrompt,
    PairingCode,
    DeviceSecret,
}

impl OutboundField {
    pub fn from_wire_name(name: &str) -> Option<Self> {
        Some(match name {
            "task_id" => Self::TaskId,
            "host_id" => Self::HostId,
            "attention_kind" => Self::AttentionKind,
            "safe_title" => Self::SafeTitle,
            "revision" => Self::Revision,
            "lifecycle" => Self::Lifecycle,
            "presence_hint" => Self::PresenceHint,
            "operation_progress" => Self::OperationProgress,
            "transcript" => Self::Transcript,
            "prompt_body" => Self::PromptBody,
            "response_body" => Self::ResponseBody,
            "terminal_bytes" => Self::TerminalBytes,
            "browser_dom" => Self::BrowserDom,
            "diff_hunk" => Self::DiffHunk,
            "file_content" => Self::FileContent,
            "personal_prompt" => Self::PersonalPrompt,
            "pairing_code" => Self::PairingCode,
            "device_secret" => Self::DeviceSecret,
            _ => return None,
        })
    }

    pub const fn privacy_class(self) -> ConnectPrivacyClass {
        match self {
            Self::TaskId
            | Self::HostId
            | Self::AttentionKind
            | Self::Revision
            | Self::Lifecycle
            | Self::PresenceHint
            | Self::OperationProgress => ConnectPrivacyClass::ManagedMetadata,
            Self::SafeTitle => ConnectPrivacyClass::ManagedMetadata,
            Self::Transcript
            | Self::PromptBody
            | Self::ResponseBody
            | Self::TerminalBytes
            | Self::BrowserDom
            | Self::DiffHunk
            | Self::FileContent => ConnectPrivacyClass::RawContent,
            Self::PersonalPrompt | Self::PairingCode | Self::DeviceSecret => {
                ConnectPrivacyClass::LocalOnly
            }
        }
    }

    pub const fn content_class(self) -> ContentClass {
        match self {
            Self::TaskId
            | Self::HostId
            | Self::AttentionKind
            | Self::SafeTitle
            | Self::Revision
            | Self::Lifecycle => ContentClass::TaskMetadata,
            Self::PresenceHint => ContentClass::Presence,
            Self::OperationProgress => ContentClass::OperationProgress,
            Self::Transcript
            | Self::PromptBody
            | Self::ResponseBody
            | Self::TerminalBytes
            | Self::BrowserDom
            | Self::DiffHunk
            | Self::FileContent => ContentClass::Transcript,
            Self::PersonalPrompt => ContentClass::PersonalPrompts,
            Self::PairingCode => ContentClass::PairedDevices,
            Self::DeviceSecret => ContentClass::Secrets,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectEnrollment {
    enrolled: BTreeSet<TaskId>,
}

impl ConnectEnrollment {
    pub fn enroll(&mut self, task_id: TaskId) {
        self.enrolled.insert(task_id);
    }

    pub fn is_enrolled(&self, task_id: TaskId) -> bool {
        self.enrolled.contains(&task_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionGrant<'a> {
    pub allowed_content: &'a BTreeSet<ContentClass>,
    pub raw_content: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectedObject {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

pub fn project_field(
    field_name: &str,
    value: &str,
    task_id: TaskId,
    enrollment: &ConnectEnrollment,
    grant: Option<ProjectionGrant<'_>>,
) -> Result<(OutboundField, String), ProjectionDenyReason> {
    let field = OutboundField::from_wire_name(field_name)
        .ok_or(ProjectionDenyReason::UnknownField)?;
    if matches!(field.privacy_class(), ConnectPrivacyClass::LocalOnly) {
        return Err(ProjectionDenyReason::LocalOnly);
    }
    if !enrollment.is_enrolled(task_id)
        && !matches!(field, OutboundField::TaskId | OutboundField::HostId)
    {
        return Err(ProjectionDenyReason::PersonalNotEnrolled);
    }
    let Some(grant) = grant else {
        return Err(ProjectionDenyReason::GrantMissing);
    };
    if !grant.allowed_content.contains(&field.content_class()) {
        return Err(ProjectionDenyReason::GrantMissing);
    }
    if matches!(field.privacy_class(), ConnectPrivacyClass::RawContent) && !grant.raw_content {
        return Err(ProjectionDenyReason::RawContentDenied);
    }
    Ok((field, value.to_string()))
}

pub fn project_object(
    source: &BTreeMap<&str, &str>,
    task_id: TaskId,
    enrollment: &ConnectEnrollment,
    grant: Option<ProjectionGrant<'_>>,
) -> Result<ProjectedObject, ProjectionDenyReason> {
    let mut fields = BTreeMap::new();
    for (name, value) in source {
        let (field, projected) = project_field(name, value, task_id, enrollment, grant)?;
        fields.insert(wire_name(field).to_string(), projected);
    }
    Ok(ProjectedObject { fields })
}

const fn wire_name(field: OutboundField) -> &'static str {
    match field {
        OutboundField::TaskId => "task_id",
        OutboundField::HostId => "host_id",
        OutboundField::AttentionKind => "attention_kind",
        OutboundField::SafeTitle => "safe_title",
        OutboundField::Revision => "revision",
        OutboundField::Lifecycle => "lifecycle",
        OutboundField::PresenceHint => "presence_hint",
        OutboundField::OperationProgress => "operation_progress",
        OutboundField::Transcript => "transcript",
        OutboundField::PromptBody => "prompt_body",
        OutboundField::ResponseBody => "response_body",
        OutboundField::TerminalBytes => "terminal_bytes",
        OutboundField::BrowserDom => "browser_dom",
        OutboundField::DiffHunk => "diff_hunk",
        OutboundField::FileContent => "file_content",
        OutboundField::PersonalPrompt => "personal_prompt",
        OutboundField::PairingCode => "pairing_code",
        OutboundField::DeviceSecret => "device_secret",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_raw_transcript_default_deny() {
        let task = TaskId::new();
        let enrollment = ConnectEnrollment::default();
        assert_eq!(
            project_field("mystery", "x", task, &enrollment, None),
            Err(ProjectionDenyReason::UnknownField)
        );
        assert_eq!(
            project_field("transcript", "raw", task, &enrollment, None),
            Err(ProjectionDenyReason::PersonalNotEnrolled)
        );
    }
}
