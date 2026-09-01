//! Fleet presentation merge and selection helpers for the multi-owner shell.
//!
//! Commands always run against the captured [`HostTaskKey`] owner — never an
//! implicit "currently selected host" fallback.
//!
//! Rail selection reuses [`crate::ui::task_workspace::surfaces::apply_workspace_selection`]
//! so Plain/Toggle preserve pane tree, PaneId, and pins. Done/Archived clicks
//! open the chat without restoring lifecycle.

use crate::client::{HostId, HostTaskKey};
use crate::domain::id::{ProjectId, TaskId};
use crate::domain::task::TaskLifecycle;
use crate::ui::native_host_state::{FleetInboxProjection, FleetTaskRow};
use crate::ui::task_cockpit::{ConfigSidebarProjection, Inbox, TaskRowModel};
use crate::ui::task_workspace::surfaces::{apply_workspace_selection, WorkspaceSelectionGesture};
use crate::ui::task_workspace::{Workspace, WorkspaceError};

/// Visible host label for the rail. Actual project names stay on each row.
///
/// Remote UUIDv7 ids share a time-based prefix, so the fallback uses the full
/// key's entropy tail — never the first four bytes alone.
pub fn host_display_label(host: &HostId) -> String {
    match host {
        HostId::LocalProfile(name) => name.clone(),
        HostId::Remote(bytes) => discriminating_remote_host_label(bytes),
    }
}

/// Prefer a hostname from an already-loaded trusted/connected endpoint; fall
/// back to a discriminating immutable encoding of the full remote host key.
/// When a port is present it is included so shared hostnames/loopback siblings
/// stay distinguishable. No production special-casing of synthetic hostnames.
pub fn host_label_from_endpoint_or_key(endpoint: &str, host: &HostId) -> String {
    if let Ok(url) = url::Url::parse(endpoint) {
        if let Some(host_str) = url.host_str() {
            let trimmed = host_str.trim();
            if !trimmed.is_empty() {
                let is_loopback =
                    trimmed == "127.0.0.1" || trimmed == "localhost" || trimmed == "::1";
                let port = url
                    .port()
                    .or_else(|| is_loopback.then(|| url.port_or_known_default()).flatten());
                if let Some(port) = port {
                    return format!("{trimmed}:{port}");
                }
                if !is_loopback {
                    return trimmed.to_string();
                }
            }
        }
    }
    host_display_label(host)
}

fn discriminating_remote_host_label(bytes: &[u8; 16]) -> String {
    let full: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    // Last 12 hex chars (6 bytes) discriminate UUIDv7 siblings that share a
    // timestamp prefix such as `01a04553…`.
    let tail = full
        .get(full.len().saturating_sub(12)..)
        .unwrap_or(full.as_str());
    format!("remote-{tail}")
}

/// Build fleet rows for one host's inbox without touching other owners.
pub fn fleet_rows_from_inbox(
    host: &HostId,
    host_label: &str,
    inbox: &Inbox,
) -> FleetInboxProjection {
    let active = inbox
        .active_rows()
        .iter()
        .map(|row| fleet_row_from_task_row(host, host_label, row, false))
        .collect();
    let done = inbox
        .settled_rows()
        .iter()
        .map(|row| fleet_row_from_task_row(host, host_label, row, false))
        .collect();
    let archived = inbox
        .history_rows()
        .iter()
        .map(|row| fleet_row_from_task_row(host, host_label, row, true))
        .collect();
    FleetInboxProjection {
        active,
        done,
        archived,
    }
}

fn fleet_row_from_task_row(
    host: &HostId,
    host_label: &str,
    row: &TaskRowModel,
    archived: bool,
) -> FleetTaskRow {
    FleetTaskRow {
        key: HostTaskKey::new(host.clone(), row.task_id),
        host_label: host_label.to_string(),
        title: row.title.clone(),
        project_label: row.display.project.clone(),
        project_id: Some(row.project_id),
        done: matches!(row.lifecycle, TaskLifecycle::Settled),
        archived,
        unread_event_count: row.unread_event_count,
        occurred_at_ms: row.occurred_at_ms,
    }
}

/// Overlay this host's configured project labels onto fleet rows. Same raw
/// [`ProjectId`] on another host must not be consulted.
pub fn overlay_owner_config_project_labels(
    projection: &mut FleetInboxProjection,
    config_sidebar: &ConfigSidebarProjection,
) {
    let labels: std::collections::BTreeMap<ProjectId, String> = config_sidebar
        .projects
        .iter()
        .filter_map(|project| {
            let project_id = ProjectId::parse(&project.workspace_id).ok()?;
            let label = project.label.trim();
            (!label.is_empty()).then(|| (project_id, project.label.clone()))
        })
        .collect();
    if labels.is_empty() {
        return;
    }
    for row in projection
        .active
        .iter_mut()
        .chain(projection.done.iter_mut())
        .chain(projection.archived.iter_mut())
    {
        let Some(project_id) = row.project_id else {
            continue;
        };
        if let Some(label) = labels.get(&project_id) {
            row.project_label = label.clone();
        }
    }
}

/// Merge per-host projections: active hosts first (stable host order), Done
/// compact at the bottom of the ordinary rail, archives separate.
pub fn merge_fleet_inbox(
    per_host: impl IntoIterator<Item = FleetInboxProjection>,
) -> FleetInboxProjection {
    let mut merged = FleetInboxProjection::default();
    for part in per_host {
        merged.active.extend(part.active);
        merged.done.extend(part.done);
        merged.archived.extend(part.archived);
    }
    merged
}

/// Plain click replaces the focused pane slot; Shift toggles membership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FleetSelectMode {
    Replace,
    Toggle,
}

impl From<FleetSelectMode> for WorkspaceSelectionGesture {
    fn from(mode: FleetSelectMode) -> Self {
        match mode {
            FleetSelectMode::Replace => WorkspaceSelectionGesture::Plain,
            FleetSelectMode::Toggle => WorkspaceSelectionGesture::Toggle,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FleetSelectionOutcome {
    pub selected: Option<HostTaskKey>,
    pub open_keys: Vec<HostTaskKey>,
    /// Always false: Done/Archived open without restoring lifecycle; selection
    /// is never a Restore refusal.
    pub refused_done_restore: bool,
}

/// Apply rail selection via the canonical workspace gesture implementation.
/// Done and Archived chats open when clicked; lifecycle is never mutated here.
///
/// Prefer [`apply_fleet_workspace_selection`] against the live shell workspace.
/// This helper exists for pure unit tests that only have an open-key vector.
pub fn apply_fleet_rail_selection(
    mode: FleetSelectMode,
    clicked: &FleetTaskRow,
    focused: Option<&HostTaskKey>,
    open_keys: &[HostTaskKey],
) -> FleetSelectionOutcome {
    let mut workspace: Option<Workspace<HostTaskKey>> = None;
    if let Some(first) = open_keys.first() {
        let _ = apply_workspace_selection(
            &mut workspace,
            first.clone(),
            WorkspaceSelectionGesture::Plain,
        );
        for key in open_keys.iter().skip(1) {
            let _ = apply_workspace_selection(
                &mut workspace,
                key.clone(),
                WorkspaceSelectionGesture::Toggle,
            );
        }
        if let Some(focused_key) = focused {
            if let Some(ws) = workspace.as_mut() {
                let _ = ws.focus_task(focused_key.clone());
            }
        }
    }
    let _ = apply_fleet_workspace_selection(&mut workspace, clicked.key.clone(), mode);
    let open_keys = workspace
        .as_ref()
        .map(|ws| ws.task_ids())
        .unwrap_or_default();
    let selected = workspace
        .as_ref()
        .and_then(|ws| ws.focused_task())
        .or_else(|| open_keys.first().cloned());
    FleetSelectionOutcome {
        selected,
        open_keys,
        refused_done_restore: false,
    }
}

/// Apply selection directly against the live workspace (preferred shell path).
pub fn apply_fleet_workspace_selection(
    workspace: &mut Option<Workspace<HostTaskKey>>,
    key: HostTaskKey,
    mode: FleetSelectMode,
) -> Result<(), WorkspaceError> {
    apply_workspace_selection(workspace, key, WorkspaceSelectionGesture::from(mode))
}

/// Forget one host: drop only its open panes; never retarget survivors to local.
pub fn forget_host_open_keys(
    open_keys: &[HostTaskKey],
    selected: Option<&HostTaskKey>,
    removed: &HostId,
) -> (Vec<HostTaskKey>, Option<HostTaskKey>) {
    let remaining: Vec<HostTaskKey> = open_keys
        .iter()
        .filter(|key| &key.host != removed)
        .cloned()
        .collect();
    let selected = selected
        .filter(|key| &key.host != removed)
        .cloned()
        .or_else(|| remaining.first().cloned());
    (remaining, selected)
}

/// Remote raw terminal is never offered for a remote owner, even when a local
/// task shares the same raw [`TaskId`].
pub fn remote_raw_terminal_allowed(host: &HostId) -> bool {
    host.as_local_profile().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(host: HostId, task: TaskId, done: bool) -> FleetTaskRow {
        FleetTaskRow {
            key: HostTaskKey::new(host, task),
            host_label: "h".into(),
            title: "t".into(),
            project_label: "p".into(),
            project_id: None,
            done,
            archived: false,
            unread_event_count: 0,
            occurred_at_ms: 0,
        }
    }

    #[test]
    fn plain_select_reuses_workspace_focus_existing() {
        let local = HostId::local_profile("dev").expect("local");
        let remote = HostId::Remote([3; 16]);
        let t1 = TaskId::new();
        let t2 = TaskId::new();
        let open = vec![
            HostTaskKey::new(local.clone(), t1),
            HostTaskKey::new(remote.clone(), t2),
        ];
        let focused = HostTaskKey::new(local.clone(), t1);
        // Click already-open remote: focus it, keep both panes.
        let clicked = row(remote.clone(), t2, false);
        let outcome =
            apply_fleet_rail_selection(FleetSelectMode::Replace, &clicked, Some(&focused), &open);
        assert_eq!(outcome.selected, Some(clicked.key.clone()));
        assert_eq!(outcome.open_keys.len(), 2);
        assert!(outcome
            .open_keys
            .iter()
            .any(|key| key.host == local && key.task_id == t1));
        assert!(!outcome.refused_done_restore);
    }

    #[test]
    fn done_and_archived_open_without_restore_refusal() {
        let local = HostId::local_profile("dev").expect("local");
        let task = TaskId::new();
        let focused = HostTaskKey::new(local.clone(), TaskId::new());
        let done = row(local.clone(), task, true);
        let outcome =
            apply_fleet_rail_selection(FleetSelectMode::Replace, &done, Some(&focused), &[]);
        assert!(!outcome.refused_done_restore);
        assert_eq!(outcome.selected, Some(done.key.clone()));
        assert!(outcome.open_keys.iter().any(|key| key == &done.key));

        let archived = FleetTaskRow {
            archived: true,
            done: false,
            ..row(local, TaskId::new(), false)
        };
        let outcome =
            apply_fleet_rail_selection(FleetSelectMode::Replace, &archived, Some(&focused), &[]);
        assert!(!outcome.refused_done_restore);
        assert_eq!(outcome.selected, Some(archived.key.clone()));
    }

    #[test]
    fn forget_host_leaves_other_owner_panes() {
        let local = HostId::local_profile("dev").expect("local");
        let remote = HostId::Remote([4; 16]);
        let open = vec![
            HostTaskKey::new(local.clone(), TaskId::new()),
            HostTaskKey::new(remote.clone(), TaskId::new()),
        ];
        let selected = open[0].clone();
        let (remaining, next) = forget_host_open_keys(&open, Some(&selected), &local);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].host, remote);
        assert_eq!(next.as_ref().map(|key| &key.host), Some(&remote));
    }

    #[test]
    fn remote_raw_terminal_refused() {
        assert!(remote_raw_terminal_allowed(
            &HostId::local_profile("dev").expect("local")
        ));
        assert!(!remote_raw_terminal_allowed(&HostId::Remote([5; 16])));
    }

    #[test]
    fn fleet_done_comes_from_settled_rows_and_keeps_same_uuid_hosts_independent() {
        use crate::client::ClientModelBuilder;
        use crate::domain::id::{EnvironmentId, ProjectId, SnapshotId};
        use crate::domain::snapshot::{
            SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem,
        };
        use crate::domain::task::{
            ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity,
            TaskFacts, TaskLifecycle, WorkspaceRef,
        };
        use crate::ui::task_cockpit::Inbox;

        let shared = TaskId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf1,
        ])
        .expect("task");
        let snap = SnapshotId::from_bytes([
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0xf0,
        ])
        .expect("snapshot");
        let page = |lifecycle: TaskLifecycle, title: &str, occurred: i64| {
            let mut builder = ClientModelBuilder::new();
            let items = vec![SnapshotItem::Task(TaskSnapshotItem {
                task: TaskFacts {
                    id: shared,
                    environment_id: EnvironmentId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x11,
                    ])
                    .expect("env"),
                    title: title.into(),
                    description: None,
                    project_id: ProjectId::from_bytes([
                        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x12,
                    ])
                    .expect("project"),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    lifecycle,
                    action_epoch: 0,
                    revision: 1,
                    created_at_ms: occurred,
                },
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
                primary_agent_id: None,
            })];
            for (section, section_items) in [
                (SnapshotSection::Tasks, items),
                (SnapshotSection::AgentSessions, Vec::new()),
                (SnapshotSection::Artifacts, Vec::new()),
                (SnapshotSection::Resources, Vec::new()),
                (SnapshotSection::Operations, Vec::new()),
            ] {
                builder
                    .ingest_page(SnapshotPage {
                        snapshot_id: snap,
                        through_sequence: 1,
                        section,
                        after_item: None,
                        items: section_items,
                        encoded_bytes: 1,
                        next_cursor: None,
                    })
                    .expect("page");
            }
            builder.finish().expect("model")
        };

        let local = HostId::local_profile("dev").expect("local");
        let remote = HostId::Remote([0x77; 16]);
        let local_inbox = Inbox::from_model(&page(TaskLifecycle::Settled, "local-done", 500));
        let remote_inbox = Inbox::from_model(&page(TaskLifecycle::Open, "remote-open", 600));
        let local_proj = fleet_rows_from_inbox(&local, "dev", &local_inbox);
        let remote_proj = fleet_rows_from_inbox(&remote, "phone", &remote_inbox);

        assert!(local_proj.active.is_empty());
        assert_eq!(local_proj.done.len(), 1);
        assert_eq!(local_proj.done[0].key.task_id, shared);
        assert_eq!(local_proj.done[0].occurred_at_ms, 500);
        assert_eq!(remote_proj.active.len(), 1);
        assert!(remote_proj.done.is_empty());
        assert_eq!(remote_proj.active[0].key.task_id, shared);
        assert_ne!(local_proj.done[0].key, remote_proj.active[0].key);

        let merged = merge_fleet_inbox([local_proj, remote_proj]);
        assert_eq!(merged.done.len(), 1);
        assert_eq!(merged.active.len(), 1);
        assert_eq!(merged.done[0].key.host, local);
        assert_eq!(merged.active[0].key.host, remote);
    }
}
