//! Manager fleet and Task Watcher projections. Coordination only: no ranking,
//! attendance, payroll, emotion, or productivity scores.

use serde::{Deserialize, Serialize};

use crate::connect::{ACTIVE_SESSION_TIME_LABEL, ConnectHostId};
use crate::domain::id::TaskId;
use crate::domain::task::{TaskAttention, TaskLifecycle};
use crate::org::error::OrgError;
use crate::org::identity::BoardCardId;
use crate::org::managed::ManagedTaskLink;
use crate::org::membership::{HostMembership, MembershipRole};

pub const ACTIVE_SESSION_RULE: &str =
    "Active session time closes after 15 minutes without qualifying human activity. It is not hours worked, attendance, payroll, or productivity.";

pub const FORBIDDEN_WATCHER_LABELS: &[&str] = &[
    "ranking",
    "productivity",
    "score",
    "sentiment",
    "emotion",
    "distress",
    "yelling",
    "profanity",
    "blame",
    "behavior",
    "attendance",
    "hours worked",
    "payroll",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostReachability {
    Online,
    Stale,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetWatcherView {
    pub host_id: ConnectHostId,
    pub reachability: HostReachability,
    pub assigned: u32,
    pub in_progress: u32,
    pub waiting: u32,
    pub blocked: u32,
    pub review: u32,
    pub last_activity_ms: Option<i64>,
    pub active_session_time_label: &'static str,
    pub active_session_rule: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWatcherView {
    pub task_id: TaskId,
    pub board_card_id: BoardCardId,
    pub lifecycle: TaskLifecycle,
    pub attention: TaskAttention,
    pub host_reachability: HostReachability,
    pub usage_source_label: Option<String>,
    pub git_summary: Option<String>,
    pub freshness: &'static str,
    pub raw_content: &'static str,
    pub mutation_allowed: bool,
}

pub struct WatcherProjection;

impl WatcherProjection {
    pub fn fleet(
        membership: &HostMembership,
        reachability: HostReachability,
        counts: [u32; 5],
        last_activity_ms: Option<i64>,
    ) -> Result<FleetWatcherView, OrgError> {
        Self::authorize_watch(membership)?;
        Ok(FleetWatcherView {
            host_id: membership.host_id,
            reachability,
            assigned: counts[0],
            in_progress: counts[1],
            waiting: counts[2],
            blocked: counts[3],
            review: counts[4],
            last_activity_ms,
            active_session_time_label: ACTIVE_SESSION_TIME_LABEL,
            active_session_rule: ACTIVE_SESSION_RULE,
        })
    }

    pub fn task(
        membership: &HostMembership,
        link: &ManagedTaskLink,
        lifecycle: TaskLifecycle,
        attention: TaskAttention,
        host_reachability: HostReachability,
        usage_source_label: Option<String>,
        git_summary: Option<String>,
    ) -> Result<TaskWatcherView, OrgError> {
        Self::authorize_watch(membership)?;
        if membership.tenant_id != link.tenant_id {
            return Err(OrgError::CrossTenant);
        }
        if let Some(label) = usage_source_label.as_deref() {
            reject_forbidden_label(label)?;
        }
        Ok(TaskWatcherView {
            task_id: link.local_task_id,
            board_card_id: link.board_card_id.clone(),
            lifecycle,
            attention,
            host_reachability,
            usage_source_label,
            git_summary,
            freshness: "observed_at plus completeness/confidence; unavailable stays unavailable",
            raw_content: "absent unless an explicit E2E raw-content grant exists",
            mutation_allowed: false,
        })
    }

    fn authorize_watch(membership: &HostMembership) -> Result<(), OrgError> {
        if !membership.is_enrolled() {
            return Err(OrgError::HostUnenrolled);
        }
        if membership.role.is_disabled() {
            return Err(OrgError::DisabledMember);
        }
        if !membership.role.can_watch() && membership.role != MembershipRole::Member {
            return Err(OrgError::RoleDenied);
        }
        if !membership.role.can_watch() {
            return Err(OrgError::RoleDenied);
        }
        Ok(())
    }
}

pub fn reject_forbidden_label(label: &str) -> Result<(), OrgError> {
    let lowered = label.to_ascii_lowercase();
    if FORBIDDEN_WATCHER_LABELS
        .iter()
        .any(|forbidden| lowered.contains(forbidden))
    {
        return Err(OrgError::ProhibitedLabel);
    }
    Ok(())
}

pub fn reject_forbidden_fields<'a>(
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<(), OrgError> {
    for field in fields {
        reject_forbidden_label(field)?;
    }
    Ok(())
}
