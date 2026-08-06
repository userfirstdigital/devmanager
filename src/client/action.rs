//! Shared read-only action catalog for CLI and future GPUI clients.
//!
//! This slice exposes only `host.actions` and `host.status`. It is intentionally
//! not a dynamic plugin framework.

use crate::protocol::Capability;

/// Stable id for listing the shared action catalog.
pub const ACTION_HOST_ACTIONS: &str = "host.actions";
/// Stable id for attaching and reporting host status.
pub const ACTION_HOST_STATUS: &str = "host.status";

/// Where an action applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionScope {
    Host,
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

#[cfg(test)]
mod tests {
    use super::{
        catalog, require_unique_ids, ActionRisk, ActionScope, ACTION_HOST_ACTIONS,
        ACTION_HOST_STATUS,
    };

    #[test]
    fn catalog_exposes_unique_host_actions_and_status() {
        let ids: Vec<&str> = catalog().iter().map(|action| action.id).collect();
        assert!(ids.contains(&ACTION_HOST_ACTIONS));
        assert!(ids.contains(&ACTION_HOST_STATUS));
        assert_eq!(ids.len(), 2);
        require_unique_ids().expect("ids must be unique");
        for action in catalog() {
            assert_eq!(action.risk, ActionRisk::ReadOnly);
            assert_eq!(action.scope, ActionScope::Host);
            assert!(action.required_capability.is_none());
        }
    }
}
