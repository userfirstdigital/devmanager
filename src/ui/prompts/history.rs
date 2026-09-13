//! Recent delivered-prompt history with local policy and clear semantics.

use crate::domain::id::{AgentSessionId, PromptHistoryId, TaskId};
use sha2::{Digest, Sha256};

pub const DEFAULT_HISTORY_RETENTION_DAYS: u16 = 90;
pub const DEFAULT_HISTORY_MAX_ENTRIES: u32 = 10_000;
pub const MIN_HISTORY_RETENTION_DAYS: u16 = 1;
pub const MAX_HISTORY_RETENTION_DAYS: u16 = 365;
pub const MIN_HISTORY_MAX_ENTRIES: u32 = 100;
pub const MAX_HISTORY_MAX_ENTRIES: u32 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryPolicy {
    pub enabled: bool,
    pub retention_days: u16,
    pub max_entries: u32,
}

impl Default for PromptHistoryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: DEFAULT_HISTORY_RETENTION_DAYS,
            max_entries: DEFAULT_HISTORY_MAX_ENTRIES,
        }
    }
}

impl PromptHistoryPolicy {
    pub fn validate(&self) -> bool {
        (MIN_HISTORY_RETENTION_DAYS..=MAX_HISTORY_RETENTION_DAYS).contains(&self.retention_days)
            && (MIN_HISTORY_MAX_ENTRIES..=MAX_HISTORY_MAX_ENTRIES).contains(&self.max_entries)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentHistoryRecord {
    pub id: PromptHistoryId,
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub provider_kind: String,
    pub body: String,
    pub body_sha256: [u8; 32],
    pub submitted_at_ms: i64,
}

impl RecentHistoryRecord {
    pub fn delivered(
        id: PromptHistoryId,
        task_id: TaskId,
        agent_session_id: AgentSessionId,
        provider_kind: impl Into<String>,
        body: impl Into<String>,
        submitted_at_ms: i64,
    ) -> Self {
        let body = body.into();
        let body_sha256 = Sha256::digest(body.as_bytes()).into();
        Self {
            id,
            task_id,
            agent_session_id,
            provider_kind: provider_kind.into(),
            body,
            body_sha256,
            submitted_at_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryClearResult {
    pub removed_history_rows: usize,
    pub removed_task_facts: usize,
    pub removed_saved_prompts: usize,
}

pub fn apply_history_policy(
    rows: &[RecentHistoryRecord],
    policy: &PromptHistoryPolicy,
    query: &str,
) -> Vec<RecentHistoryRecord> {
    if !policy.enabled {
        return Vec::new();
    }
    let needle = query.trim().to_lowercase();
    let mut filtered: Vec<RecentHistoryRecord> = rows
        .iter()
        .filter(|row| {
            needle.is_empty()
                || row.body.to_lowercase().contains(&needle)
                || row.provider_kind.to_lowercase().contains(&needle)
        })
        .cloned()
        .collect();
    filtered.sort_by(|left, right| right.submitted_at_ms.cmp(&left.submitted_at_ms));
    filtered.truncate(policy.max_entries as usize);
    filtered
}
