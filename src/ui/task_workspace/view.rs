use std::collections::BTreeMap;

use crate::domain::TaskId;

use super::{Allocation, Axis, PaneId, PanePresentation, TaskWorkspace, WorkspaceNode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskPaneBody {
    Full,
    Compact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneProjection {
    pub task_id: TaskId,
    pub title: String,
    pub project_name: String,
    pub provider_label: String,
    pub status_label: String,
    pub latest_snippet: Option<String>,
    pub show_terminal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneViewModel {
    pub pane_id: PaneId,
    pub task_id: TaskId,
    pub title: String,
    pub project_name: String,
    pub provider_label: String,
    pub status_label: String,
    pub latest_snippet: Option<String>,
    pub body: TaskPaneBody,
    pub focused: bool,
    pub build_composer: bool,
    pub paint_terminal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskWorkspaceViewChild {
    pub allocation: Allocation,
    pub node: TaskWorkspaceViewNode,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskWorkspaceViewNode {
    Pane(TaskPaneViewModel),
    Split {
        axis: Axis,
        children: Vec<TaskWorkspaceViewChild>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskWorkspaceViewModel {
    pub root: Option<TaskWorkspaceViewNode>,
    pub focused_task: Option<TaskId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskWorkspaceViewError {
    MissingProjection(TaskId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskWorkspaceEvent {
    Focus(TaskId),
    SetCompact { task_id: TaskId, compact: bool },
    Close(TaskId),
}

impl TaskWorkspaceViewModel {
    pub fn build(
        workspace: &TaskWorkspace,
        projections: &BTreeMap<TaskId, TaskPaneProjection>,
    ) -> Result<Self, TaskWorkspaceViewError> {
        let focused_task = workspace.focused_task();
        let root = workspace
            .root()
            .map(|root| build_node(root, projections, focused_task))
            .transpose()?;
        Ok(Self { root, focused_task })
    }

    pub fn panes(&self) -> Vec<&TaskPaneViewModel> {
        let mut panes = Vec::new();
        if let Some(root) = &self.root {
            collect_panes(root, &mut panes);
        }
        panes
    }
}

fn build_node(
    node: &WorkspaceNode,
    projections: &BTreeMap<TaskId, TaskPaneProjection>,
    focused_task: Option<TaskId>,
) -> Result<TaskWorkspaceViewNode, TaskWorkspaceViewError> {
    match node {
        WorkspaceNode::Pane(pane) => {
            let projection = projections
                .get(&pane.task_id)
                .ok_or(TaskWorkspaceViewError::MissingProjection(pane.task_id))?;
            let body = match pane.presentation {
                PanePresentation::Full => TaskPaneBody::Full,
                PanePresentation::CompactManual | PanePresentation::CompactAutomatic => {
                    TaskPaneBody::Compact
                }
            };
            let focused = focused_task == Some(pane.task_id);
            Ok(TaskWorkspaceViewNode::Pane(TaskPaneViewModel {
                pane_id: pane.id,
                task_id: pane.task_id,
                title: projection.title.clone(),
                project_name: projection.project_name.clone(),
                provider_label: projection.provider_label.clone(),
                status_label: projection.status_label.clone(),
                latest_snippet: projection.latest_snippet.clone(),
                body,
                focused,
                build_composer: focused && body == TaskPaneBody::Full,
                paint_terminal: body == TaskPaneBody::Full && projection.show_terminal,
            }))
        }
        WorkspaceNode::Split { axis, children, .. } => Ok(TaskWorkspaceViewNode::Split {
            axis: *axis,
            children: children
                .iter()
                .map(|child| {
                    Ok(TaskWorkspaceViewChild {
                        allocation: child.allocation,
                        node: build_node(&child.node, projections, focused_task)?,
                    })
                })
                .collect::<Result<_, TaskWorkspaceViewError>>()?,
        }),
    }
}

fn collect_panes<'a>(node: &'a TaskWorkspaceViewNode, panes: &mut Vec<&'a TaskPaneViewModel>) {
    match node {
        TaskWorkspaceViewNode::Pane(pane) => panes.push(pane),
        TaskWorkspaceViewNode::Split { children, .. } => {
            for child in children {
                collect_panes(&child.node, panes);
            }
        }
    }
}

#[cfg(test)]
mod task_pane_view_model_tests {
    use super::*;

    fn projection(task_id: TaskId, snippet: Option<&str>, terminal: bool) -> TaskPaneProjection {
        TaskPaneProjection {
            task_id,
            title: format!("Task {task_id}"),
            project_name: "DevManager".into(),
            provider_label: "Codex".into(),
            status_label: "Working".into(),
            latest_snippet: snippet.map(str::to_string),
            show_terminal: terminal,
        }
    }

    #[test]
    fn compact_view_model_contains_status_and_snippet_but_no_heavy_surface() {
        let task_id = TaskId::new();
        let mut workspace = TaskWorkspace::single(task_id);
        workspace
            .set_manual_compact(task_id, true)
            .expect("compact task");
        let projections = BTreeMap::from([(
            task_id,
            projection(task_id, Some("Editing layout.rs"), true),
        )]);

        let model = TaskWorkspaceViewModel::build(&workspace, &projections)
            .expect("workspace model")
            .panes()
            .into_iter()
            .next()
            .expect("pane")
            .clone();
        assert_eq!(model.project_name, "DevManager");
        assert_eq!(model.status_label, "Working");
        assert_eq!(model.latest_snippet.as_deref(), Some("Editing layout.rs"));
        assert_eq!(model.body, TaskPaneBody::Compact);
        assert!(!model.build_composer);
        assert!(!model.paint_terminal);
    }

    #[test]
    fn only_the_focused_full_pane_builds_interactive_controls() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .expect("second pane");
        workspace
            .insert_after_focused(third, Axis::Vertical)
            .expect("third pane");
        workspace.focus_task(second).expect("focus second");
        let projections = [first, second, third]
            .into_iter()
            .map(|task_id| (task_id, projection(task_id, None, false)))
            .collect();

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let interactive: Vec<_> = model
            .panes()
            .into_iter()
            .filter(|pane| pane.build_composer)
            .collect();
        assert_eq!(interactive.len(), 1);
        assert_eq!(interactive[0].task_id, second);
    }
}
