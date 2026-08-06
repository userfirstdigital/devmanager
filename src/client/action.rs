//! Shared read-only action catalog for CLI and future GPUI clients.
//!
//! This slice exposes `host.actions`, `host.status`, and `task.show`. It is
//! intentionally not a dynamic plugin framework.

use crate::domain::query::{Query, QueryEnvelope};
use crate::domain::{ClientId, RequestId, TaskId};
use crate::protocol::Capability;

/// Stable id for listing the shared action catalog.
pub const ACTION_HOST_ACTIONS: &str = "host.actions";
/// Stable id for attaching and reporting host status.
pub const ACTION_HOST_STATUS: &str = "host.status";
/// Stable id for reading one Task through the host query boundary.
pub const ACTION_TASK_SHOW: &str = "task.show";

/// Where an action applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionScope {
    Host,
    Task,
}

/// Risk classification for catalog entries in this read-only slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionRisk {
    ReadOnly,
}

/// Static metadata for one catalog action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDescriptor {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub keywords: &'static [&'static str],
    pub scope: ActionScope,
    pub required_capability: Option<Capability>,
    pub risk: ActionRisk,
}

const ACTIONS: &[ActionDescriptor] = &[
    ActionDescriptor {
        id: ACTION_HOST_ACTIONS,
        title: "List actions",
        description: "Emit the shared read-only action catalog as versioned JSON.",
        keywords: &["actions", "catalog", "help", "list"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
    },
    ActionDescriptor {
        id: ACTION_HOST_STATUS,
        title: "Host status",
        description: "Attach to a running named-profile host and report ServerHello status fields.",
        keywords: &["status", "host", "hello", "attach"],
        scope: ActionScope::Host,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
    },
    ActionDescriptor {
        id: ACTION_TASK_SHOW,
        title: "Show task",
        description: "Read one Task snapshot through the host query boundary.",
        keywords: &["task", "show", "inspect", "snapshot"],
        scope: ActionScope::Task,
        required_capability: None,
        risk: ActionRisk::ReadOnly,
    },
];

/// Return the closed catalog for this slice.
pub fn catalog() -> &'static [ActionDescriptor] {
    ACTIONS
}

/// Fail when two descriptors share a stable id.
pub fn require_unique_ids() -> Result<(), String> {
    let mut seen = Vec::new();
    for action in catalog() {
        if seen.contains(&action.id) {
            return Err(format!("duplicate action id: {}", action.id));
        }
        seen.push(action.id);
    }
    Ok(())
}

/// Build the shared side-effect-free request for `task.show`.
pub fn task_show_query(
    request_id: RequestId,
    client_id: ClientId,
    task_id: TaskId,
) -> QueryEnvelope {
    QueryEnvelope {
        request_id,
        client_id,
        task_id: Some(task_id),
        query: Query::TaskSnapshot,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        catalog, require_unique_ids, task_show_query, ActionRisk, ActionScope, ACTION_HOST_ACTIONS,
        ACTION_HOST_STATUS, ACTION_TASK_SHOW,
    };
    use crate::domain::query::Query;
    use crate::domain::{ClientId, RequestId, TaskId};

    #[test]
    fn catalog_exposes_three_unique_read_only_actions() {
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_HOST_ACTIONS));
        assert!(ids.contains(&ACTION_HOST_STATUS));
        assert!(ids.contains(&ACTION_TASK_SHOW));
        assert_eq!(ids.len(), 3);
        require_unique_ids().expect("ids must be unique");
        for action in catalog() {
            assert_eq!(action.risk, ActionRisk::ReadOnly);
            let expected_scope = if action.id == ACTION_TASK_SHOW {
                ActionScope::Task
            } else {
                ActionScope::Host
            };
            assert_eq!(action.scope, expected_scope);
            assert!(action.required_capability.is_none());
        }
    }

    #[test]
    fn task_show_factory_binds_client_request_and_task_scope() {
        let request_id = RequestId::new();
        let client_id = ClientId::new();
        let task_id = TaskId::new();
        let query = task_show_query(request_id, client_id, task_id);
        assert_eq!(query.request_id, request_id);
        assert_eq!(query.client_id, client_id);
        assert_eq!(query.task_id, Some(task_id));
        assert_eq!(query.query, Query::TaskSnapshot);
    }
}
