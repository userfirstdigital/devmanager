use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::TaskId;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PaneId(Uuid);

impl PaneId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for PaneId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SplitId(Uuid);

impl SplitId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SplitId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Allocation {
    Auto { weight: f32 },
    Pinned { logical_px: f32 },
}

impl Allocation {
    pub const fn auto() -> Self {
        Self::Auto { weight: 1.0 }
    }

    pub fn is_valid(self) -> bool {
        match self {
            Self::Auto { weight } => weight.is_finite() && weight > 0.0,
            Self::Pinned { logical_px } => logical_px.is_finite() && logical_px > 0.0,
        }
    }

    pub const fn is_pinned(self) -> bool {
        matches!(self, Self::Pinned { .. })
    }
}

impl Default for Allocation {
    fn default() -> Self {
        Self::auto()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanePresentation {
    Full,
    CompactManual,
    CompactAutomatic,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskPane {
    pub id: PaneId,
    pub task_id: TaskId,
    pub presentation: PanePresentation,
    pub last_focused_at: u64,
}

impl TaskPane {
    fn new(task_id: TaskId, last_focused_at: u64) -> Self {
        Self {
            id: PaneId::new(),
            task_id,
            presentation: PanePresentation::Full,
            last_focused_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SplitChild {
    pub node: WorkspaceNode,
    pub allocation: Allocation,
}

impl SplitChild {
    fn auto(node: WorkspaceNode) -> Self {
        Self {
            node,
            allocation: Allocation::auto(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceNode {
    Pane(TaskPane),
    Split {
        id: SplitId,
        axis: Axis,
        children: Vec<SplitChild>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Top | Self::Bottom => Axis::Vertical,
        }
    }

    const fn inserts_after(self) -> bool {
        matches!(self, Self::Right | Self::Bottom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropTarget {
    Center { pane: PaneId },
    Edge { pane: PaneId, edge: Edge },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceError {
    DuplicateTask,
    InvalidTree,
    MissingPane,
    SelfDrop,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskWorkspace {
    root: Option<WorkspaceNode>,
    focused: Option<PaneId>,
    previous_focus: Option<PaneId>,
    focus_clock: u64,
}

impl TaskWorkspace {
    pub fn single(task_id: TaskId) -> Self {
        let focus_clock = 1;
        let pane = TaskPane::new(task_id, focus_clock);
        Self {
            focused: Some(pane.id),
            root: Some(WorkspaceNode::Pane(pane)),
            previous_focus: None,
            focus_clock,
        }
    }

    pub fn root(&self) -> Option<&WorkspaceNode> {
        self.root.as_ref()
    }

    pub(crate) fn root_mut(&mut self) -> Option<&mut WorkspaceNode> {
        self.root.as_mut()
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.focused
    }

    pub fn previous_focus(&self) -> Option<PaneId> {
        self.previous_focus
    }

    pub fn focused_task(&self) -> Option<TaskId> {
        self.focused
            .and_then(|pane_id| self.pane(pane_id))
            .map(|pane| pane.task_id)
    }

    pub fn pane_count(&self) -> usize {
        self.root.as_ref().map(count_panes).unwrap_or(0)
    }

    pub fn pane(&self, pane_id: PaneId) -> Option<&TaskPane> {
        self.root.as_ref().and_then(|root| find_pane(root, pane_id))
    }

    pub fn pane_for_task(&self, task_id: TaskId) -> Option<&TaskPane> {
        self.root
            .as_ref()
            .and_then(|root| find_pane_for_task(root, task_id))
    }

    pub fn task_ids(&self) -> Vec<TaskId> {
        let mut task_ids = Vec::with_capacity(self.pane_count());
        if let Some(root) = &self.root {
            collect_task_ids(root, &mut task_ids);
        }
        task_ids
    }

    pub fn contains_task(&self, task_id: TaskId) -> bool {
        self.pane_for_task(task_id).is_some()
    }

    pub fn presentation(&self, task_id: TaskId) -> Option<PanePresentation> {
        self.pane_for_task(task_id).map(|pane| pane.presentation)
    }

    pub fn insert_after_focused(
        &mut self,
        task_id: TaskId,
        axis: Axis,
    ) -> Result<PaneId, WorkspaceError> {
        if self.contains_task(task_id) {
            return Err(WorkspaceError::DuplicateTask);
        }
        if self.root.is_none() {
            *self = Self::single(task_id);
            return self.focused.ok_or(WorkspaceError::InvalidTree);
        }
        let target = self.focused.ok_or(WorkspaceError::InvalidTree)?;
        let mut candidate = self.clone();
        candidate.focus_clock = candidate.focus_clock.saturating_add(1).max(1);
        let pane = TaskPane::new(task_id, candidate.focus_clock);
        let pane_id = pane.id;
        let root = candidate.root.take().ok_or(WorkspaceError::InvalidTree)?;
        let (next_root, inserted) = insert_pane_near(root, target, pane, axis, true);
        if !inserted {
            return Err(WorkspaceError::MissingPane);
        }
        candidate.root = Some(next_root);
        candidate.previous_focus = candidate.focused;
        candidate.focused = Some(pane_id);
        candidate.validate()?;
        *self = candidate;
        Ok(pane_id)
    }

    pub fn focus_pane(&mut self, pane_id: PaneId) -> Result<(), WorkspaceError> {
        if self.pane(pane_id).is_none() {
            return Err(WorkspaceError::MissingPane);
        }
        if self.focused == Some(pane_id) {
            return Ok(());
        }
        self.focus_clock = self.focus_clock.saturating_add(1).max(1);
        self.previous_focus = self.focused;
        self.focused = Some(pane_id);
        let clock = self.focus_clock;
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.last_focused_at = clock;
        }
        Ok(())
    }

    pub fn focus_task(&mut self, task_id: TaskId) -> Result<(), WorkspaceError> {
        let pane_id = self
            .pane_for_task(task_id)
            .map(|pane| pane.id)
            .ok_or(WorkspaceError::MissingPane)?;
        self.focus_pane(pane_id)
    }

    pub fn set_manual_compact(
        &mut self,
        task_id: TaskId,
        compact: bool,
    ) -> Result<(), WorkspaceError> {
        let pane_id = self
            .pane_for_task(task_id)
            .map(|pane| pane.id)
            .ok_or(WorkspaceError::MissingPane)?;
        let pane = self.pane_mut(pane_id).ok_or(WorkspaceError::MissingPane)?;
        pane.presentation = if compact {
            PanePresentation::CompactManual
        } else {
            PanePresentation::Full
        };
        Ok(())
    }

    pub fn remove_pane(&mut self, pane_id: PaneId) -> Result<(), WorkspaceError> {
        if self.pane(pane_id).is_none() {
            return Err(WorkspaceError::MissingPane);
        }
        let mut candidate = self.clone();
        let root = candidate.root.take().ok_or(WorkspaceError::MissingPane)?;
        let (next_root, removed) = remove_pane_node(root, pane_id);
        if removed.is_none() {
            return Err(WorkspaceError::MissingPane);
        }
        candidate.root = next_root;
        candidate.repair_focus_after_removal(pane_id);
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> Result<(), WorkspaceError> {
        if first == second {
            return Err(WorkspaceError::SelfDrop);
        }
        if self.pane(first).is_none() || self.pane(second).is_none() {
            return Err(WorkspaceError::MissingPane);
        }
        let mut candidate = self.clone();
        let first_pane = candidate
            .pane(first)
            .cloned()
            .ok_or(WorkspaceError::MissingPane)?;
        let second_pane = candidate
            .pane(second)
            .cloned()
            .ok_or(WorkspaceError::MissingPane)?;
        let mut first_replacement = second_pane;
        first_replacement.id = first;
        let mut second_replacement = first_pane;
        second_replacement.id = second;
        *candidate
            .pane_mut(first)
            .ok_or(WorkspaceError::MissingPane)? = first_replacement;
        *candidate
            .pane_mut(second)
            .ok_or(WorkspaceError::MissingPane)? = second_replacement;
        if candidate.focused == Some(first) {
            candidate.focused = Some(second);
        } else if candidate.focused == Some(second) {
            candidate.focused = Some(first);
        }
        if candidate.previous_focus == Some(first) {
            candidate.previous_focus = Some(second);
        } else if candidate.previous_focus == Some(second) {
            candidate.previous_focus = Some(first);
        }
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub fn move_pane(&mut self, source: PaneId, target: DropTarget) -> Result<(), WorkspaceError> {
        let target_pane = match target {
            DropTarget::Center { pane } | DropTarget::Edge { pane, .. } => pane,
        };
        if source == target_pane {
            return Err(WorkspaceError::SelfDrop);
        }
        if self.pane(source).is_none() || self.pane(target_pane).is_none() {
            return Err(WorkspaceError::MissingPane);
        }
        if matches!(target, DropTarget::Center { .. }) {
            return self.swap_panes(source, target_pane);
        }

        let DropTarget::Edge { edge, .. } = target else {
            unreachable!();
        };
        let mut candidate = self.clone();
        let root = candidate.root.take().ok_or(WorkspaceError::MissingPane)?;
        let (without_source, moved) = remove_pane_node(root, source);
        let moved = moved.ok_or(WorkspaceError::MissingPane)?;
        let without_source = without_source.ok_or(WorkspaceError::InvalidTree)?;
        let (next_root, inserted) = insert_pane_near(
            without_source,
            target_pane,
            moved,
            edge.axis(),
            edge.inserts_after(),
        );
        if !inserted {
            return Err(WorkspaceError::MissingPane);
        }
        candidate.root = Some(next_root);
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), WorkspaceError> {
        let Some(root) = &self.root else {
            return if self.focused.is_none() && self.previous_focus.is_none() {
                Ok(())
            } else {
                Err(WorkspaceError::InvalidTree)
            };
        };
        let mut pane_ids = BTreeSet::new();
        let mut task_ids = BTreeSet::new();
        let mut split_ids = BTreeSet::new();
        validate_node(root, &mut pane_ids, &mut task_ids, &mut split_ids)?;
        let focused = self.focused.ok_or(WorkspaceError::InvalidTree)?;
        if !pane_ids.contains(&focused) {
            return Err(WorkspaceError::InvalidTree);
        }
        if self
            .previous_focus
            .is_some_and(|previous| previous == focused || !pane_ids.contains(&previous))
        {
            return Err(WorkspaceError::InvalidTree);
        }
        Ok(())
    }

    pub(crate) fn pane_mut(&mut self, pane_id: PaneId) -> Option<&mut TaskPane> {
        self.root
            .as_mut()
            .and_then(|root| find_pane_mut(root, pane_id))
    }

    pub fn pin_task_axis_size(
        &mut self,
        task_id: TaskId,
        logical_px: f32,
    ) -> Result<(), WorkspaceError> {
        if !logical_px.is_finite() || logical_px <= 0.0 {
            return Err(WorkspaceError::InvalidTree);
        }
        let root = self.root_mut().ok_or(WorkspaceError::MissingPane)?;
        if set_task_allocation(root, task_id, Allocation::Pinned { logical_px }) {
            Ok(())
        } else {
            Err(WorkspaceError::MissingPane)
        }
    }

    pub fn reset_task_axis_size(&mut self, task_id: TaskId) -> Result<(), WorkspaceError> {
        let root = self.root_mut().ok_or(WorkspaceError::MissingPane)?;
        if set_task_allocation(root, task_id, Allocation::auto()) {
            Ok(())
        } else {
            Err(WorkspaceError::MissingPane)
        }
    }

    pub(crate) fn task_is_unpinned(&self, task_id: TaskId) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| task_path_is_auto(root, task_id, true))
    }

    pub(crate) fn set_presentation(
        &mut self,
        task_id: TaskId,
        presentation: PanePresentation,
    ) -> Result<(), WorkspaceError> {
        let pane_id = self
            .pane_for_task(task_id)
            .map(|pane| pane.id)
            .ok_or(WorkspaceError::MissingPane)?;
        self.pane_mut(pane_id)
            .ok_or(WorkspaceError::MissingPane)?
            .presentation = presentation;
        Ok(())
    }

    fn repair_focus_after_removal(&mut self, removed: PaneId) {
        if self.root.is_none() {
            self.focused = None;
            self.previous_focus = None;
            return;
        }
        let previous = self
            .previous_focus
            .filter(|pane_id| *pane_id != removed && self.pane(*pane_id).is_some());
        if self.focused == Some(removed) {
            self.focused = previous.or_else(|| self.root.as_ref().and_then(first_pane_id));
            self.previous_focus = None;
        } else if self.previous_focus == Some(removed) {
            self.previous_focus = None;
        }
    }
}

fn count_panes(node: &WorkspaceNode) -> usize {
    match node {
        WorkspaceNode::Pane(_) => 1,
        WorkspaceNode::Split { children, .. } => {
            children.iter().map(|child| count_panes(&child.node)).sum()
        }
    }
}

fn first_pane_id(node: &WorkspaceNode) -> Option<PaneId> {
    match node {
        WorkspaceNode::Pane(pane) => Some(pane.id),
        WorkspaceNode::Split { children, .. } => children
            .first()
            .and_then(|child| first_pane_id(&child.node)),
    }
}

fn find_pane(node: &WorkspaceNode, pane_id: PaneId) -> Option<&TaskPane> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.id == pane_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .find_map(|child| find_pane(&child.node, pane_id)),
    }
}

fn find_pane_for_task(node: &WorkspaceNode, task_id: TaskId) -> Option<&TaskPane> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.task_id == task_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .find_map(|child| find_pane_for_task(&child.node, task_id)),
    }
}

fn find_pane_mut(node: &mut WorkspaceNode, pane_id: PaneId) -> Option<&mut TaskPane> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.id == pane_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter_mut()
            .find_map(|child| find_pane_mut(&mut child.node, pane_id)),
    }
}

fn collect_task_ids(node: &WorkspaceNode, task_ids: &mut Vec<TaskId>) {
    match node {
        WorkspaceNode::Pane(pane) => task_ids.push(pane.task_id),
        WorkspaceNode::Split { children, .. } => {
            for child in children {
                collect_task_ids(&child.node, task_ids);
            }
        }
    }
}

fn set_task_allocation(node: &mut WorkspaceNode, task_id: TaskId, allocation: Allocation) -> bool {
    let WorkspaceNode::Split { children, .. } = node else {
        return false;
    };
    for child in children {
        if !contains_task(&child.node, task_id) {
            continue;
        }
        if set_task_allocation(&mut child.node, task_id, allocation) {
            return true;
        }
        child.allocation = allocation;
        return true;
    }
    false
}

fn contains_task(node: &WorkspaceNode, task_id: TaskId) -> bool {
    find_pane_for_task(node, task_id).is_some()
}

fn task_path_is_auto(node: &WorkspaceNode, task_id: TaskId, path_is_auto: bool) -> bool {
    match node {
        WorkspaceNode::Pane(pane) => pane.task_id == task_id && path_is_auto,
        WorkspaceNode::Split { children, .. } => children.iter().any(|child| {
            contains_task(&child.node, task_id)
                && task_path_is_auto(
                    &child.node,
                    task_id,
                    path_is_auto && !child.allocation.is_pinned(),
                )
        }),
    }
}

fn insert_pane_near(
    node: WorkspaceNode,
    target: PaneId,
    pane: TaskPane,
    axis: Axis,
    insert_after: bool,
) -> (WorkspaceNode, bool) {
    match node {
        WorkspaceNode::Pane(existing) if existing.id == target => {
            let (first, second) = if insert_after {
                (WorkspaceNode::Pane(existing), WorkspaceNode::Pane(pane))
            } else {
                (WorkspaceNode::Pane(pane), WorkspaceNode::Pane(existing))
            };
            (
                WorkspaceNode::Split {
                    id: SplitId::new(),
                    axis,
                    children: vec![SplitChild::auto(first), SplitChild::auto(second)],
                },
                true,
            )
        }
        WorkspaceNode::Pane(existing) => (WorkspaceNode::Pane(existing), false),
        WorkspaceNode::Split {
            id,
            axis: split_axis,
            mut children,
        } => {
            if split_axis == axis {
                if let Some(index) = children
                    .iter()
                    .position(|child| matches!(&child.node, WorkspaceNode::Pane(existing) if existing.id == target))
                {
                    let insert_index = if insert_after { index + 1 } else { index };
                    children.insert(
                        insert_index,
                        SplitChild::auto(WorkspaceNode::Pane(pane)),
                    );
                    return (
                        WorkspaceNode::Split {
                            id,
                            axis: split_axis,
                            children,
                        },
                        true,
                    );
                }
            }
            if let Some(index) = children
                .iter()
                .position(|child| contains_pane(&child.node, target))
            {
                let child = children.remove(index);
                let (next_node, inserted) =
                    insert_pane_near(child.node, target, pane, axis, insert_after);
                children.insert(
                    index,
                    SplitChild {
                        node: next_node,
                        allocation: child.allocation,
                    },
                );
                return (
                    WorkspaceNode::Split {
                        id,
                        axis: split_axis,
                        children,
                    },
                    inserted,
                );
            }
            (
                WorkspaceNode::Split {
                    id,
                    axis: split_axis,
                    children,
                },
                false,
            )
        }
    }
}

fn contains_pane(node: &WorkspaceNode, target: PaneId) -> bool {
    find_pane(node, target).is_some()
}

fn remove_pane_node(
    node: WorkspaceNode,
    target: PaneId,
) -> (Option<WorkspaceNode>, Option<TaskPane>) {
    match node {
        WorkspaceNode::Pane(pane) if pane.id == target => (None, Some(pane)),
        WorkspaceNode::Pane(pane) => (Some(WorkspaceNode::Pane(pane)), None),
        WorkspaceNode::Split { id, axis, children } => {
            let mut next_children = Vec::with_capacity(children.len());
            let mut removed = None;
            for child in children {
                if removed.is_some() {
                    next_children.push(child);
                    continue;
                }
                let allocation = child.allocation;
                let (next_node, found) = remove_pane_node(child.node, target);
                if let Some(found) = found {
                    removed = Some(found);
                }
                if let Some(next_node) = next_node {
                    next_children.push(SplitChild {
                        node: next_node,
                        allocation,
                    });
                }
            }
            if removed.is_none() {
                return (
                    Some(WorkspaceNode::Split {
                        id,
                        axis,
                        children: next_children,
                    }),
                    None,
                );
            }
            let normalized = match next_children.len() {
                0 => None,
                1 => Some(next_children.remove(0).node),
                _ => Some(WorkspaceNode::Split {
                    id,
                    axis,
                    children: next_children,
                }),
            };
            (normalized, removed)
        }
    }
}

fn validate_node(
    node: &WorkspaceNode,
    pane_ids: &mut BTreeSet<PaneId>,
    task_ids: &mut BTreeSet<TaskId>,
    split_ids: &mut BTreeSet<SplitId>,
) -> Result<(), WorkspaceError> {
    match node {
        WorkspaceNode::Pane(pane) => {
            if pane.last_focused_at == 0
                || !pane_ids.insert(pane.id)
                || !task_ids.insert(pane.task_id)
            {
                return Err(WorkspaceError::InvalidTree);
            }
        }
        WorkspaceNode::Split { id, children, .. } => {
            if children.len() < 2 || !split_ids.insert(*id) {
                return Err(WorkspaceError::InvalidTree);
            }
            for child in children {
                if !child.allocation.is_valid() {
                    return Err(WorkspaceError::InvalidTree);
                }
                validate_node(&child.node, pane_ids, task_ids, split_ids)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskId;

    #[test]
    fn inserting_tasks_preserves_unique_identity_and_focus_history() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        let first_pane = workspace.focused_pane_id().unwrap();

        let second_pane = workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();

        assert_eq!(workspace.focused_task(), Some(second));
        assert_eq!(workspace.previous_focus(), Some(first_pane));
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.pane(second_pane).unwrap().task_id, second);
        assert!(workspace.validate().is_ok());
    }

    #[test]
    fn failed_edge_move_keeps_the_original_tree() {
        let task = TaskId::new();
        let mut workspace = TaskWorkspace::single(task);
        let pane = workspace.focused_pane_id().unwrap();
        let before = workspace.clone();

        assert_eq!(
            workspace.move_pane(
                pane,
                DropTarget::Edge {
                    pane,
                    edge: Edge::Left,
                },
            ),
            Err(WorkspaceError::SelfDrop)
        );
        assert_eq!(workspace, before);
    }

    #[test]
    fn removing_a_pane_collapses_redundant_splits_and_restores_previous_focus() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        let first_pane = workspace.focused_pane_id().unwrap();
        let second_pane = workspace
            .insert_after_focused(second, Axis::Vertical)
            .unwrap();

        workspace.remove_pane(second_pane).unwrap();

        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.focused_pane_id(), Some(first_pane));
        assert!(matches!(workspace.root(), Some(WorkspaceNode::Pane(_))));
    }
}
