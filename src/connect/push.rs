//! Sanitized Connect push payloads.
//!
//! Push may carry only opaque host/task IDs, attention kind, an optional safe
//! title when policy allows, a timestamp, and a route deep link. Prompt,
//! response, terminal, browser, diff, and file content are never included.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::TaskId;

use super::invites::PinnedHostPublicId;
use super::projection::{ConnectEnrollment, OutboundField};

pub const MAX_SAFE_TITLE_BYTES: usize = 64;
pub const MAX_ROUTE_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushSanitizeError {
    TitleNotAllowed,
    TitleTooLong,
    RouteRequired,
    RouteTooLong,
    RawContentForbidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    NeedsInput,
    Completed,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedPush {
    pub host_id: PinnedHostPublicId,
    pub task_id: TaskId,
    pub attention_kind: AttentionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_title: Option<String>,
    pub timestamp_ms: i64,
    pub route: String,
}

impl fmt::Debug for SanitizedPush {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SanitizedPush")
            .field("attention_kind", &self.attention_kind)
            .field("has_safe_title", &self.safe_title.is_some())
            .field("timestamp_ms", &self.timestamp_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushPolicy {
    pub allow_safe_title: bool,
}

impl PushPolicy {
    pub const fn metadata_only() -> Self {
        Self {
            allow_safe_title: false,
        }
    }

    pub const fn allow_safe_title() -> Self {
        Self {
            allow_safe_title: true,
        }
    }
}

pub fn sanitize_push(
    host_id: PinnedHostPublicId,
    task_id: TaskId,
    attention_kind: AttentionKind,
    timestamp_ms: i64,
    route: impl Into<String>,
    candidate_title: Option<&str>,
    policy: PushPolicy,
    enrollment: &ConnectEnrollment,
) -> Result<SanitizedPush, PushSanitizeError> {
    if !enrollment.is_enrolled(task_id) {
        return Err(PushSanitizeError::RawContentForbidden);
    }
    let route = canonical::canonicalize(route.into()).ok_or(PushSanitizeError::RouteRequired)?;
    if route.len() > MAX_ROUTE_BYTES {
        return Err(PushSanitizeError::RouteTooLong);
    }
    if route.contains("prompt=")
        || route.contains("transcript")
        || route.contains("diff")
        || route.contains("file=")
    {
        return Err(PushSanitizeError::RawContentForbidden);
    }
    let safe_title = match candidate_title {
        Some(_) if !policy.allow_safe_title => {
            return Err(PushSanitizeError::TitleNotAllowed);
        }
        Some(title) => {
            let title = canonical::canonicalize(title).ok_or(PushSanitizeError::TitleNotAllowed)?;
            if title.len() > MAX_SAFE_TITLE_BYTES {
                return Err(PushSanitizeError::TitleTooLong);
            }
            if looks_like_raw_content(&title) {
                return Err(PushSanitizeError::RawContentForbidden);
            }
            Some(title)
        }
        None => None,
    };
    Ok(SanitizedPush {
        host_id,
        task_id,
        attention_kind,
        safe_title,
        timestamp_ms,
        route,
    })
}

pub fn forbidden_push_fields() -> &'static [OutboundField] {
    &[
        OutboundField::Transcript,
        OutboundField::PromptBody,
        OutboundField::ResponseBody,
        OutboundField::TerminalBytes,
        OutboundField::BrowserDom,
        OutboundField::DiffHunk,
        OutboundField::FileContent,
        OutboundField::PersonalPrompt,
        OutboundField::PairingCode,
        OutboundField::DeviceSecret,
    ]
}

fn looks_like_raw_content(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.contains("```")
        || lower.contains("diff --git")
        || lower.contains("\n")
        || lower.contains('\u{1b}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_push_omits_title_and_raw_routes() {
        let mut enrollment = ConnectEnrollment::default();
        let task = TaskId::new();
        enrollment.enroll(task);
        let host = PinnedHostPublicId::from_bytes([3; 16]);
        assert!(sanitize_push(
            host,
            task,
            AttentionKind::NeedsInput,
            1,
            "/connect/task/opaque?prompt=HELLO",
            None,
            PushPolicy::metadata_only(),
            &enrollment,
        )
        .is_err());
        let push = sanitize_push(
            host,
            task,
            AttentionKind::NeedsInput,
            1,
            "/connect/tasks/opaque",
            None,
            PushPolicy::metadata_only(),
            &enrollment,
        )
        .unwrap();
        assert!(push.safe_title.is_none());
        assert_eq!(forbidden_push_fields().len(), 10);
    }
}
