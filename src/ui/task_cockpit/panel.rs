//! Shared bounded presentation contracts for Task Cockpit panels.
//!
//! Panels are deliberately projections.  They carry the exact task identity,
//! the task revision observed by the caller, and an existing catalog
//! [`ActionRequest`].  They never create a second command vocabulary or read
//! workspace/configuration state themselves.

use gpui::{div, px, rgb, AnyElement, IntoElement, ParentElement, Styled};

use crate::client::action::ActionRequest;
use crate::domain::id::TaskId;
use crate::ui::tokens::ThemeTokens;
use sha2::{Digest, Sha256};

/// Hard cap for all panel rows.  The host already bounds the source payload;
/// keeping the UI cap here prevents a stale or malicious projection from
/// turning one render into an unbounded allocation.
pub const MAX_PANEL_ROWS: usize = 64;
pub const MAX_PANEL_LABEL_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelDisabledReason {
    NoTaskSelected,
    HostProjectionMissing,
    ProjectionLoading,
    SecretPath,
    Directory,
    NotReviewable,
    Unsupported,
}

impl PanelDisabledReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoTaskSelected => "Select a task first",
            Self::HostProjectionMissing => "Host projection unavailable",
            Self::ProjectionLoading => "Waiting for host projection",
            Self::SecretPath => "Secret paths are not readable",
            Self::Directory => "Directories are opened from the file list",
            Self::NotReviewable => "This task is not ready for review",
            Self::Unsupported => "This action is not available yet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelIdentity {
    pub task_id: TaskId,
    /// Revision is an optional UI fence.  Host cockpit queries currently
    /// carry the task id only; callers with a durable snapshot should provide
    /// the observed revision so stale row actions can be rejected upstream.
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelAction {
    pub identity: PanelIdentity,
    pub action_id: &'static str,
    pub request: ActionRequest,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl PanelAction {
    /// Stable GPUI key scoped to the task, revision, catalog action, and row
    /// target.  The target is hashed so secret/path text never appears in the
    /// element identity or diagnostics.
    pub fn element_key(&self, target: &str) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(self.identity.task_id.as_bytes());
        hasher.update([u8::from(self.identity.revision.is_some())]);
        hasher.update(self.identity.revision.unwrap_or_default().to_le_bytes());
        hasher.update(self.action_id.as_bytes());
        hasher.update(target.as_bytes());
        u64::from_le_bytes(hasher.finalize()[..8].try_into().expect("digest prefix"))
    }
}

impl PanelAction {
    pub fn enabled(identity: PanelIdentity, request: ActionRequest) -> Self {
        let action_id = request.id();
        Self {
            identity,
            action_id,
            request,
            disabled_reason: None,
        }
    }

    pub fn disabled(
        identity: PanelIdentity,
        request: ActionRequest,
        reason: PanelDisabledReason,
    ) -> Self {
        let action_id = request.id();
        Self {
            identity,
            action_id,
            request,
            disabled_reason: Some(reason),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

pub fn task_identity(task_id: TaskId, revision: Option<u64>) -> PanelIdentity {
    PanelIdentity { task_id, revision }
}

/// Render a small action affordance.  The action request stays on the typed
/// projection for the owning shell to dispatch; this renderer intentionally
/// does not invent a click handler or bypass the host action boundary.
pub fn render_panel_action(action: &PanelAction, target: &str, tokens: ThemeTokens) -> AnyElement {
    let label = action.disabled_reason.map_or_else(
        || action_label(action.action_id),
        PanelDisabledReason::label,
    );
    let mut element = div()
        .id(("task-cockpit-panel-action", action.element_key(target)))
        .px(px(tokens.density.spacing.sm))
        .py(px(tokens.density.spacing.xs))
        .border_1()
        .border_color(rgb(tokens.borders.subtle.to_u32()))
        .text_xs()
        .child(label);
    if action.is_enabled() {
        element = element
            .cursor_pointer()
            .text_color(rgb(tokens.text.primary.to_u32()));
    } else {
        element = element.text_color(rgb(tokens.text.disabled.to_u32()));
    }
    element.into_any_element()
}

pub fn render_panel_frame(
    id: &'static str,
    title: &'static str,
    summary: impl Into<String>,
    actions: impl IntoIterator<Item = PanelAction>,
    body: impl IntoElement,
    tokens: ThemeTokens,
) -> AnyElement {
    let controls = actions
        .into_iter()
        .map(|action| render_panel_action(&action, "panel", tokens));
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.xs))
        .p(px(tokens.density.physical().control_padding as f32))
        .bg(rgb(tokens.surfaces.sunken.to_u32()))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(tokens.density.spacing.sm))
                .text_color(rgb(tokens.text.primary.to_u32()))
                .child(title)
                .child(summary.into()),
        )
        .child(
            div()
                .flex()
                .gap(px(tokens.density.spacing.xs))
                .children(controls),
        )
        .child(body)
        .into_any_element()
}

pub fn action_label(action_id: &str) -> &'static str {
    match action_id {
        crate::client::action::ACTION_WORKSPACE_STATUS => "Refresh workspace",
        crate::client::action::ACTION_GIT_STATUS => "Refresh changes",
        crate::client::action::ACTION_FILES_LIST => "Refresh files",
        crate::client::action::ACTION_FILES_READ => "Read file",
        crate::client::action::ACTION_TASK_SHOW => "Refresh task",
        _ => "Open",
    }
}

/// Accept an action only while its captured task and optional revision remain
/// the selected durable snapshot.  This is the UI capture fence; the host
/// still performs its own task/capability/path admission.
pub fn action_is_current(
    action: &PanelAction,
    selected_task: Option<TaskId>,
    current_revision: Option<u64>,
) -> bool {
    selected_task == Some(action.identity.task_id) && action.identity.revision == current_revision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::action;
    use crate::domain::cockpit::TaskCockpitQuery;

    #[test]
    fn action_keeps_exact_task_revision_and_catalog_identity() {
        let identity = task_identity(TaskId::new(), Some(17));
        let request = ActionRequest::TaskCockpit {
            task_id: identity.task_id,
            query: TaskCockpitQuery::GitStatus,
        };
        let action = PanelAction::enabled(identity, request.clone());
        assert_eq!(action.identity, identity);
        assert_eq!(action.action_id, action::ACTION_GIT_STATUS);
        assert_eq!(action.request, request);
        assert!(action.is_enabled());
    }

    #[test]
    fn disabled_reason_is_typed_and_truthful() {
        let identity = task_identity(TaskId::new(), None);
        let request = ActionRequest::TaskCockpit {
            task_id: identity.task_id,
            query: TaskCockpitQuery::WorkspaceStatus,
        };
        let action = PanelAction::disabled(
            identity,
            request,
            PanelDisabledReason::HostProjectionMissing,
        );
        assert_eq!(
            action.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
        assert!(!action.is_enabled());
        assert_eq!(
            action.disabled_reason.map(PanelDisabledReason::label),
            Some("Host projection unavailable")
        );
    }

    #[test]
    fn element_key_is_unique_for_task_revision_action_and_row_target() {
        let task_id = TaskId::new();
        let first = PanelAction::enabled(
            task_identity(task_id, Some(1)),
            ActionRequest::TaskCockpit {
                task_id,
                query: TaskCockpitQuery::FilesRead {
                    relative_path: "src/a.rs".into(),
                    max_bytes: 16,
                },
            },
        );
        let second = PanelAction::enabled(
            task_identity(task_id, Some(1)),
            ActionRequest::TaskCockpit {
                task_id,
                query: TaskCockpitQuery::FilesRead {
                    relative_path: "src/b.rs".into(),
                    max_bytes: 16,
                },
            },
        );
        assert_ne!(
            first.element_key("src/a.rs"),
            second.element_key("src/b.rs")
        );
        assert_ne!(first.element_key("src/a.rs"), first.element_key("src/b.rs"));
    }

    #[test]
    fn current_action_rejects_task_or_revision_drift() {
        let task_id = TaskId::new();
        let action = PanelAction::enabled(
            task_identity(task_id, Some(4)),
            ActionRequest::TaskShow { task_id },
        );
        assert!(action_is_current(&action, Some(task_id), Some(4)));
        assert!(!action_is_current(&action, Some(TaskId::new()), Some(4)));
        assert!(!action_is_current(&action, Some(task_id), Some(5)));
        let unfenced = PanelAction::enabled(
            task_identity(task_id, None),
            ActionRequest::TaskShow { task_id },
        );
        assert!(action_is_current(&unfenced, Some(task_id), None));
        assert!(!action_is_current(&unfenced, Some(task_id), Some(4)));
    }
}
