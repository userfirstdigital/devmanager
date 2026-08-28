//! Host-qualified presentation types and drain helpers for the multi-owner shell.
//!
//! Domain models stay per-host inside [`crate::ui::native_shell::HostUiState`].
//! This module merges rows at the presentation boundary only.

use crate::client::{HostId, HostTaskKey, MAX_FLEET_HOSTS};
use crate::domain::id::{ProjectId, TaskId};

// Re-export the single authority from the client fleet module.
pub use crate::client::MAX_FLEET_HOSTS as FLEET_HOST_CAP;

/// Per-host drain quota used by the controller round-robin.
pub const FLEET_DRAIN_PER_HOST: usize = 4;

/// Compile-time reminder: UI capacity must match the fleet install cap.
const _: () = assert!(MAX_FLEET_HOSTS == 16);

/// Host-qualified rail row. Presentation merge only — never concatenates models.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FleetTaskRow {
    pub key: HostTaskKey,
    pub host_label: String,
    pub title: String,
    pub project_label: String,
    pub project_id: Option<ProjectId>,
    pub done: bool,
    pub archived: bool,
    pub unread_event_count: u64,
    /// Last durable occurrence time for compact age labels (not synthetic).
    pub occurred_at_ms: i64,
}

/// Merged inbox projection across attached hosts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FleetInboxProjection {
    pub active: Vec<FleetTaskRow>,
    pub done: Vec<FleetTaskRow>,
    pub archived: Vec<FleetTaskRow>,
}

impl FleetInboxProjection {
    pub fn rail_rows(&self) -> impl Iterator<Item = &FleetTaskRow> {
        self.active.iter().chain(self.done.iter())
    }

    pub fn find(&self, key: &HostTaskKey) -> Option<&FleetTaskRow> {
        self.rail_rows()
            .chain(self.archived.iter())
            .find(|row| &row.key == key)
    }
}

/// Project identity scoped to its owning host.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct HostProjectKey {
    pub host: HostId,
    pub project_id: ProjectId,
}

impl HostProjectKey {
    pub fn new(host: HostId, project_id: ProjectId) -> Self {
        Self { host, project_id }
    }
}

/// Round-robin drain cursor across installed host runtimes.
#[derive(Clone, Debug, Default)]
pub struct HostDrainCursor {
    order: Vec<HostId>,
    next: usize,
}

impl HostDrainCursor {
    pub fn sync_hosts<I>(&mut self, hosts: I)
    where
        I: IntoIterator<Item = HostId>,
    {
        let mut next_order: Vec<HostId> = hosts.into_iter().collect();
        next_order.sort();
        if next_order != self.order {
            self.order = next_order;
            self.next = 0;
        }
    }

    /// Returns `(host, quota)` starting at the rotating head so no host starves.
    pub fn take_round(&mut self, per_host_quota: usize) -> Vec<(HostId, usize)> {
        if self.order.is_empty() || per_host_quota == 0 {
            return Vec::new();
        }
        let mut plan = Vec::with_capacity(self.order.len());
        let start = self.next % self.order.len();
        for offset in 0..self.order.len() {
            let index = (start + offset) % self.order.len();
            plan.push((self.order[index].clone(), per_host_quota));
        }
        self.next = (start + 1) % self.order.len();
        plan
    }
}

/// Explicit local-profile mapper for legacy layout/draft keys.
pub fn local_host_task_key(profile_name: &str, task_id: TaskId) -> Result<HostTaskKey, String> {
    let host = HostId::local_profile(profile_name).map_err(|error| error.to_string())?;
    Ok(HostTaskKey::new(host, task_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_cursor_round_robins_without_starvation() {
        let a = HostId::local_profile("a").expect("a");
        let b = HostId::Remote([1; 16]);
        let mut cursor = HostDrainCursor::default();
        cursor.sync_hosts([a.clone(), b.clone()]);
        let first = cursor.take_round(2);
        let second = cursor.take_round(2);
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_ne!(first[0].0, second[0].0);
        assert_eq!(first[0].1, 2);
    }

    #[test]
    fn fleet_rows_keep_same_raw_task_independent() {
        let task = TaskId::new();
        let local = HostTaskKey::new(HostId::local_profile("dev").expect("local"), task);
        let remote = HostTaskKey::new(HostId::Remote([2; 16]), task);
        assert_ne!(local, remote);
        let projection = FleetInboxProjection {
            active: vec![
                FleetTaskRow {
                    key: local.clone(),
                    host_label: "dev".into(),
                    title: "Same".into(),
                    project_label: "Apps".into(),
                    project_id: None,
                    done: false,
                    archived: false,
                    unread_event_count: 0,
                    occurred_at_ms: 1,
                },
                FleetTaskRow {
                    key: remote.clone(),
                    host_label: "phone".into(),
                    title: "Same".into(),
                    project_label: "Apps".into(),
                    project_id: None,
                    done: false,
                    archived: false,
                    unread_event_count: 0,
                    occurred_at_ms: 2,
                },
            ],
            done: Vec::new(),
            archived: Vec::new(),
        };
        assert_eq!(projection.rail_rows().count(), 2);
        assert!(projection.find(&local).is_some());
        assert!(projection.find(&remote).is_some());
    }

    #[test]
    fn local_mapper_never_uses_selected_host() {
        let task = TaskId::new();
        let key = local_host_task_key("dev", task).expect("local");
        assert_eq!(key.host.as_local_profile(), Some("dev"));
        assert_eq!(key.task_id, task);
    }

    #[test]
    fn ui_capacity_matches_client_fleet_cap() {
        assert_eq!(MAX_FLEET_HOSTS, 16);
        assert_eq!(FLEET_HOST_CAP, MAX_FLEET_HOSTS);
    }
}
