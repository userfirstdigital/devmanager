use std::collections::BTreeMap;

use crate::domain::TaskId;

use super::{Allocation, Axis, PaneId, PanePresentation, SplitId, Workspace, WorkspaceNode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskPaneBody {
    Full,
    Compact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneProjection<K = TaskId> {
    pub task_id: K,
    pub title: String,
    pub project_name: String,
    pub provider_label: String,
    pub status_label: String,
    pub latest_snippet: Option<String>,
    pub show_terminal: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneViewModel<K = TaskId> {
    pub pane_id: PaneId,
    pub task_id: K,
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
pub struct TaskWorkspaceViewChild<K = TaskId> {
    pub allocation: Allocation,
    pub node: TaskWorkspaceViewNode<K>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskWorkspaceViewNode<K = TaskId> {
    Pane(TaskPaneViewModel<K>),
    Split {
        split_id: SplitId,
        axis: Axis,
        children: Vec<TaskWorkspaceViewChild<K>>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskWorkspaceViewModel<K = TaskId> {
    pub root: Option<TaskWorkspaceViewNode<K>>,
    pub focused_task: Option<K>,
}

impl<K> Default for TaskWorkspaceViewModel<K> {
    fn default() -> Self {
        Self {
            root: None,
            focused_task: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskWorkspaceViewError<K = TaskId> {
    MissingProjection(K),
}

impl Copy for TaskWorkspaceViewError<TaskId> {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskWorkspaceEvent<K = TaskId> {
    Focus(K),
    SetCompact { task_id: K, compact: bool },
    Close(K),
}

impl Copy for TaskWorkspaceEvent<TaskId> {}

impl<K: Clone + Ord + Eq> TaskWorkspaceViewModel<K> {
    pub fn build(
        workspace: &Workspace<K>,
        projections: &BTreeMap<K, TaskPaneProjection<K>>,
    ) -> Result<Self, TaskWorkspaceViewError<K>> {
        let focused_task = workspace.focused_task();
        let root = workspace
            .root()
            .map(|root| build_node(root, projections, focused_task.as_ref()))
            .transpose()?;
        Ok(Self { root, focused_task })
    }

    pub fn panes(&self) -> Vec<&TaskPaneViewModel<K>> {
        let mut panes = Vec::new();
        if let Some(root) = &self.root {
            collect_panes(root, &mut panes);
        }
        panes
    }
}

fn build_node<K: Clone + Ord + Eq>(
    node: &WorkspaceNode<K>,
    projections: &BTreeMap<K, TaskPaneProjection<K>>,
    focused_task: Option<&K>,
) -> Result<TaskWorkspaceViewNode<K>, TaskWorkspaceViewError<K>> {
    match node {
        WorkspaceNode::Pane(pane) => {
            let projection = projections
                .get(&pane.task_id)
                .ok_or_else(|| TaskWorkspaceViewError::MissingProjection(pane.task_id.clone()))?;
            let body = match pane.presentation {
                PanePresentation::Full => TaskPaneBody::Full,
                PanePresentation::CompactManual | PanePresentation::CompactAutomatic => {
                    TaskPaneBody::Compact
                }
            };
            let focused = focused_task == Some(&pane.task_id);
            Ok(TaskWorkspaceViewNode::Pane(TaskPaneViewModel {
                pane_id: pane.id,
                task_id: pane.task_id.clone(),
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
        WorkspaceNode::Split { id, axis, children } => Ok(TaskWorkspaceViewNode::Split {
            split_id: *id,
            axis: *axis,
            children: children
                .iter()
                .map(|child| {
                    Ok(TaskWorkspaceViewChild {
                        allocation: child.allocation,
                        node: build_node(&child.node, projections, focused_task)?,
                    })
                })
                .collect::<Result<_, TaskWorkspaceViewError<K>>>()?,
        }),
    }
}

fn collect_panes<'a, K>(
    node: &'a TaskWorkspaceViewNode<K>,
    panes: &mut Vec<&'a TaskPaneViewModel<K>>,
) {
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
    use crate::ui::task_workspace::TaskWorkspace;

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

    #[test]
    fn background_full_panes_keep_full_body_while_only_focus_owns_composer() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .expect("second pane");
        workspace
            .insert_after_focused(third, Axis::Horizontal)
            .expect("third pane");
        workspace.focus_task(second).expect("focus second");
        workspace
            .set_manual_compact(third, true)
            .expect("compact third");
        let projections = [first, second, third]
            .into_iter()
            .map(|task_id| (task_id, projection(task_id, Some("live"), false)))
            .collect();

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let panes = model.panes();
        let first_pane = panes.iter().find(|pane| pane.task_id == first).unwrap();
        let second_pane = panes.iter().find(|pane| pane.task_id == second).unwrap();
        let third_pane = panes.iter().find(|pane| pane.task_id == third).unwrap();

        assert_eq!(first_pane.body, TaskPaneBody::Full);
        assert!(!first_pane.build_composer);
        assert_eq!(second_pane.body, TaskPaneBody::Full);
        assert!(second_pane.build_composer);
        assert_eq!(third_pane.body, TaskPaneBody::Compact);
        assert!(!third_pane.build_composer);
    }

    #[test]
    fn host_qualified_view_models_keep_duplicate_raw_ids_distinct() {
        type HostKey = (String, TaskId);
        let shared = TaskId::new();
        let local: HostKey = ("local".into(), shared);
        let remote: HostKey = ("remote".into(), shared);
        let mut workspace = Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Horizontal)
            .expect("remote pane");
        workspace.focus_task(remote.clone()).expect("focus remote");
        let projections = BTreeMap::from([
            (
                local.clone(),
                TaskPaneProjection {
                    task_id: local.clone(),
                    title: "Local".into(),
                    project_name: "DevManager".into(),
                    provider_label: "Codex".into(),
                    status_label: "Idle".into(),
                    latest_snippet: Some("local cache".into()),
                    show_terminal: false,
                },
            ),
            (
                remote.clone(),
                TaskPaneProjection {
                    task_id: remote.clone(),
                    title: "Remote".into(),
                    project_name: "DevManager".into(),
                    provider_label: "Codex".into(),
                    status_label: "Working".into(),
                    latest_snippet: Some("remote cache".into()),
                    show_terminal: true,
                },
            ),
        ]);

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("host model");
        let panes = model.panes();
        assert_eq!(panes.len(), 2);
        let local_pane = panes.iter().find(|pane| pane.task_id == local).unwrap();
        let remote_pane = panes.iter().find(|pane| pane.task_id == remote).unwrap();
        assert_eq!(local_pane.latest_snippet.as_deref(), Some("local cache"));
        assert!(!local_pane.build_composer);
        assert_eq!(remote_pane.latest_snippet.as_deref(), Some("remote cache"));
        assert!(remote_pane.build_composer);
        assert!(remote_pane.paint_terminal);
    }
}
