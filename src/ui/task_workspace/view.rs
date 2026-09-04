use std::collections::BTreeMap;

use crate::domain::TaskId;

use super::{
    Allocation, Axis, PaneId, PanePresentation, PaneView, SplitId, Workspace, WorkspaceNode,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneProjection<K = TaskId> {
    pub task_id: K,
    pub title: String,
    pub project_name: String,
    pub provider_label: String,
    pub status_label: String,
    /// A debug preview conversation is installed, which replaces whatever
    /// surface the pane would otherwise paint. It is an input to the model so
    /// that the terminal arm and the composer are decided together, in one
    /// place, rather than the painter carrying half of the rule.
    pub preview_conversation_installed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskPaneViewModel<K = TaskId> {
    pub pane_id: PaneId,
    pub task_id: K,
    pub title: String,
    pub project_name: String,
    pub provider_label: String,
    pub status_label: String,
    /// Which surface this pane paints. There is no separate compact body any
    /// more, so the view is the only thing that decides what is drawn.
    pub view: PaneView,
    /// Paint the raw terminal rather than a conversation. This is the one
    /// terminal predicate: `view == Terminal` unless a preview conversation
    /// has displaced it.
    pub paint_terminal: bool,
    /// Too little room for content: paint the title strip alone.
    pub minimised: bool,
    /// This pane is filling the canvas.
    pub zoomed: bool,
    pub focused: bool,
    pub build_composer: bool,
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
        let zoomed = workspace.zoomed();
        // A zoomed pane is the whole canvas, so the root the shell renders is
        // that pane alone. The tree underneath is untouched and comes straight
        // back when zoom is released.
        let root = match zoomed.and_then(|pane_id| workspace.pane(pane_id)) {
            Some(pane) => Some(build_node(
                &WorkspaceNode::Pane(pane.clone()),
                projections,
                focused_task.as_ref(),
                zoomed,
            )?),
            None => workspace
                .root()
                .map(|root| build_node(root, projections, focused_task.as_ref(), zoomed))
                .transpose()?,
        };
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
    zoomed: Option<PaneId>,
) -> Result<TaskWorkspaceViewNode<K>, TaskWorkspaceViewError<K>> {
    match node {
        WorkspaceNode::Pane(pane) => {
            let projection = projections
                .get(&pane.task_id)
                .ok_or_else(|| TaskWorkspaceViewError::MissingProjection(pane.task_id.clone()))?;
            let minimised = pane.presentation == PanePresentation::Minimised;
            let focused = focused_task == Some(&pane.task_id);
            let paint_terminal =
                pane.view == PaneView::Terminal && !projection.preview_conversation_installed;
            Ok(TaskWorkspaceViewNode::Pane(TaskPaneViewModel {
                pane_id: pane.id,
                task_id: pane.task_id.clone(),
                title: projection.title.clone(),
                project_name: projection.project_name.clone(),
                provider_label: projection.provider_label.clone(),
                status_label: projection.status_label.clone(),
                view: pane.view,
                paint_terminal,
                minimised,
                zoomed: zoomed == Some(pane.id),
                focused,
                // A pane painting the terminal has no conversation to put a
                // composer under, so the two are one decision: whatever turns
                // the terminal arm off gives the composer back.
                build_composer: focused && !minimised && !paint_terminal,
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
                        node: build_node(&child.node, projections, focused_task, zoomed)?,
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

    fn projection(task_id: TaskId) -> TaskPaneProjection {
        TaskPaneProjection {
            task_id,
            title: format!("Task {task_id}"),
            project_name: "DevManager".into(),
            provider_label: "Codex".into(),
            status_label: "Working".into(),
            preview_conversation_installed: false,
        }
    }

    #[test]
    fn view_model_carries_view_minimised_and_zoomed_flags() {
        let task_id = TaskId::new();
        let other = TaskId::new();
        let mut workspace = TaskWorkspace::single(task_id);
        workspace
            .insert_after_focused(other, Axis::Horizontal)
            .expect("second pane");
        workspace.set_view(task_id, PaneView::Files).expect("view");
        let pane = workspace.pane_for_task(task_id).expect("pane").id;
        workspace.zoom(pane).expect("zoom");
        let projections = [task_id, other]
            .into_iter()
            .map(|id| (id, projection(id)))
            .collect();

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let panes = model.panes();

        assert_eq!(panes.len(), 1, "zoom shows only the zoomed pane");
        assert_eq!(panes[0].task_id, task_id);
        assert_eq!(panes[0].view, PaneView::Files);
        assert!(panes[0].zoomed);
        assert!(!panes[0].minimised);
        assert!(panes[0].build_composer, "the zoomed pane owns the composer");
    }

    #[test]
    fn a_previewed_conversation_turns_off_the_terminal_arm_in_one_place() {
        let task_id = TaskId::new();
        let mut workspace = TaskWorkspace::single(task_id);
        workspace
            .set_view(task_id, PaneView::Terminal)
            .expect("terminal view");

        // Preview off: the pane paints the terminal, so it owns no composer.
        let live = BTreeMap::from([(task_id, projection(task_id))]);
        let pane = TaskWorkspaceViewModel::build(&workspace, &live)
            .expect("workspace")
            .panes()[0]
            .clone();
        assert!(pane.paint_terminal);
        assert!(!pane.build_composer);

        // Preview on: the same pane paints the previewed conversation instead,
        // so the terminal arm is off and the focused pane owns the composer
        // again. Both facts are decided here, not in the painter.
        let previewed = BTreeMap::from([(
            task_id,
            TaskPaneProjection {
                preview_conversation_installed: true,
                ..projection(task_id)
            },
        )]);
        let pane = TaskWorkspaceViewModel::build(&workspace, &previewed)
            .expect("workspace")
            .panes()[0]
            .clone();
        assert!(
            !pane.paint_terminal,
            "a previewed conversation is not the terminal"
        );
        assert!(
            pane.build_composer,
            "a focused pane painting a conversation owns the composer"
        );
        assert_eq!(
            pane.view,
            PaneView::Terminal,
            "the pane's own view is untouched; only what it paints changed"
        );
    }

    #[test]
    fn zooming_a_strip_gives_it_its_content_back() {
        let strip = TaskId::new();
        let other = TaskId::new();
        let mut workspace = TaskWorkspace::single(strip);
        workspace
            .insert_after_focused(other, Axis::Horizontal)
            .expect("second pane");
        workspace
            .set_presentation(strip, PanePresentation::Minimised)
            .expect("minimise");
        let pane = workspace.pane_for_task(strip).expect("pane").id;
        let projections = [strip, other]
            .into_iter()
            .map(|id| (id, projection(id)))
            .collect();

        workspace.zoom(pane).expect("zoom");

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let panes = model.panes();
        assert_eq!(panes.len(), 1);
        assert!(
            !panes[0].minimised,
            "a pane filling the canvas is never a title strip"
        );
        assert!(panes[0].zoomed);
        assert!(
            panes[0].build_composer,
            "and it owns the composer, which a strip never does"
        );
    }

    #[test]
    fn a_minimised_pane_reports_itself_and_owns_no_composer() {
        let task_id = TaskId::new();
        let mut workspace = TaskWorkspace::single(task_id);
        workspace
            .set_presentation(task_id, PanePresentation::Minimised)
            .expect("minimise task");
        let projections = BTreeMap::from([(task_id, projection(task_id))]);

        let model = TaskWorkspaceViewModel::build(&workspace, &projections)
            .expect("workspace model")
            .panes()
            .into_iter()
            .next()
            .expect("pane")
            .clone();
        assert_eq!(model.project_name, "DevManager");
        assert_eq!(model.status_label, "Working");
        assert!(model.minimised);
        assert!(!model.zoomed);
        assert!(!model.build_composer);
    }

    #[test]
    fn a_focused_terminal_pane_paints_the_terminal_not_the_composer() {
        let task_id = TaskId::new();
        let mut workspace = TaskWorkspace::single(task_id);
        workspace
            .set_view(task_id, PaneView::Terminal)
            .expect("terminal view");
        let projections = BTreeMap::from([(task_id, projection(task_id))]);

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let panes = model.panes();
        assert!(panes[0].focused);
        assert_eq!(panes[0].view, PaneView::Terminal);
        assert!(
            !panes[0].build_composer,
            "a Terminal pane paints the terminal; the composer is a Conversation control"
        );

        workspace
            .set_view(task_id, PaneView::Conversation)
            .expect("conversation view");
        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        assert!(
            model.panes()[0].build_composer,
            "the composer comes back with the conversation"
        );
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
            .map(|task_id| (task_id, projection(task_id)))
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
    fn background_full_panes_keep_their_view_while_only_focus_owns_composer() {
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
            .set_presentation(third, PanePresentation::Minimised)
            .expect("minimise third");
        let projections: BTreeMap<_, _> = [first, second, third]
            .into_iter()
            .map(|task_id| (task_id, projection(task_id)))
            .collect();

        // On Conversation, the focused Full pane owns the composer. Asserted
        // before the view is flipped so the negative below is a measured
        // change rather than a property this test never saw hold.
        let before = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let before_panes = before.panes();
        let second_before = before_panes
            .iter()
            .find(|pane| pane.task_id == second)
            .unwrap();
        assert_eq!(second_before.view, PaneView::Conversation);
        assert!(
            second_before.build_composer,
            "the focused Full pane on Conversation owns the composer"
        );

        workspace
            .set_view(second, PaneView::Terminal)
            .expect("terminal view");

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("workspace");
        let panes = model.panes();
        let first_pane = panes.iter().find(|pane| pane.task_id == first).unwrap();
        let second_pane = panes.iter().find(|pane| pane.task_id == second).unwrap();
        let third_pane = panes.iter().find(|pane| pane.task_id == third).unwrap();

        assert!(!first_pane.minimised);
        assert_eq!(first_pane.view, PaneView::Conversation);
        assert!(!first_pane.build_composer);
        assert!(!second_pane.minimised);
        assert_eq!(
            second_pane.view,
            PaneView::Terminal,
            "each pane carries its own view, not the focused pane's"
        );
        assert!(
            !second_pane.build_composer,
            "the focused pane is on Terminal, so it paints the terminal, not the composer"
        );
        assert!(third_pane.minimised);
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
                    preview_conversation_installed: false,
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
                    preview_conversation_installed: false,
                },
            ),
        ]);

        let model = TaskWorkspaceViewModel::build(&workspace, &projections).expect("host model");
        let panes = model.panes();
        assert_eq!(panes.len(), 2);
        let local_pane = panes.iter().find(|pane| pane.task_id == local).unwrap();
        let remote_pane = panes.iter().find(|pane| pane.task_id == remote).unwrap();
        assert_eq!(local_pane.status_label, "Idle");
        assert!(!local_pane.build_composer);
        assert_eq!(remote_pane.status_label, "Working");
        assert!(remote_pane.build_composer);
    }
}
