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

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
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

/// Which view a task pane shows (spec 6.2). `TABS` are the five visible tabs
/// in order; `MORE` live behind the panel menu. Serialised per pane; an
/// unknown value fails closed to `Conversation`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneView {
    #[default]
    Conversation,
    Terminal,
    Files,
    Changes,
    Browser,
    Review,
    Artifacts,
    Services,
}

impl PaneView {
    pub const TABS: [PaneView; 5] = [
        PaneView::Conversation,
        PaneView::Terminal,
        PaneView::Files,
        PaneView::Changes,
        PaneView::Browser,
    ];
    pub const MORE: [PaneView; 3] = [PaneView::Review, PaneView::Artifacts, PaneView::Services];

    pub fn label(self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::Terminal => "Terminal",
            Self::Files => "Files",
            Self::Changes => "Changes",
            Self::Browser => "Browser",
            Self::Review => "Review",
            Self::Artifacts => "Artifacts",
            Self::Services => "Services",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanePresentation {
    Full,
    CompactManual,
    CompactAutomatic,
}

/// Recursive pane workspace keyed by task identity `K`.
///
/// Local callers keep [`TaskWorkspace`] / [`TaskPane`] aliases (`K = TaskId`).
/// Future host-qualified keys (non-`Copy` enums) plug in without remapping UUIDs
/// or spawning a separate workspace per host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
pub struct TaskPane<K = TaskId> {
    pub id: PaneId,
    pub task_id: K,
    pub presentation: PanePresentation,
    pub last_focused_at: u64,
    /// Which surface this pane shows. Absent in files written before the pane
    /// owned its own view, so it defaults rather than failing the load.
    #[serde(default)]
    pub view: PaneView,
}

impl<K> TaskPane<K> {
    fn new(task_id: K, last_focused_at: u64) -> Self {
        Self {
            id: PaneId::new(),
            task_id,
            presentation: PanePresentation::Full,
            last_focused_at,
            view: PaneView::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
pub struct SplitChild<K = TaskId> {
    pub node: WorkspaceNode<K>,
    pub allocation: Allocation,
}

impl<K> SplitChild<K> {
    fn auto(node: WorkspaceNode<K>) -> Self {
        Self {
            node,
            allocation: Allocation::auto(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
pub enum WorkspaceNode<K = TaskId> {
    Pane(TaskPane<K>),
    Split {
        id: SplitId,
        axis: Axis,
        children: Vec<SplitChild<K>>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de>"))]
pub struct Workspace<K = TaskId> {
    root: Option<WorkspaceNode<K>>,
    focused: Option<PaneId>,
    previous_focus: Option<PaneId>,
    focus_clock: u64,
}

/// Local TaskId-keyed workspace (existing public API).
pub type TaskWorkspace = Workspace<TaskId>;

impl<K> Default for Workspace<K> {
    fn default() -> Self {
        Self {
            root: None,
            focused: None,
            previous_focus: None,
            focus_clock: 0,
        }
    }
}

impl<K: Clone + Ord + Eq> Workspace<K> {
    pub fn single(task_id: K) -> Self {
        let focus_clock = 1;
        let pane = TaskPane::new(task_id, focus_clock);
        Self {
            focused: Some(pane.id),
            root: Some(WorkspaceNode::Pane(pane)),
            previous_focus: None,
            focus_clock,
        }
    }

    pub fn root(&self) -> Option<&WorkspaceNode<K>> {
        self.root.as_ref()
    }

    pub(crate) fn root_mut(&mut self) -> Option<&mut WorkspaceNode<K>> {
        self.root.as_mut()
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.focused
    }

    pub fn previous_focus(&self) -> Option<PaneId> {
        self.previous_focus
    }

    pub fn focused_task(&self) -> Option<K> {
        self.focused
            .and_then(|pane_id| self.pane(pane_id))
            .map(|pane| pane.task_id.clone())
    }

    pub fn pane_count(&self) -> usize {
        self.root.as_ref().map(count_panes).unwrap_or(0)
    }

    pub fn pane(&self, pane_id: PaneId) -> Option<&TaskPane<K>> {
        self.root.as_ref().and_then(|root| find_pane(root, pane_id))
    }

    pub fn pane_for_task(&self, task_id: K) -> Option<&TaskPane<K>> {
        self.root
            .as_ref()
            .and_then(|root| find_pane_for_task(root, &task_id))
    }

    pub fn task_ids(&self) -> Vec<K> {
        let mut task_ids = Vec::with_capacity(self.pane_count());
        if let Some(root) = &self.root {
            collect_task_ids(root, &mut task_ids);
        }
        task_ids
    }

    pub fn contains_task(&self, task_id: K) -> bool {
        self.pane_for_task(task_id).is_some()
    }

    pub fn presentation(&self, task_id: K) -> Option<PanePresentation> {
        self.pane_for_task(task_id).map(|pane| pane.presentation)
    }

    pub fn view_of(&self, task_id: K) -> Option<PaneView> {
        self.pane_for_task(task_id).map(|pane| pane.view)
    }

    pub fn set_view(&mut self, task_id: K, view: PaneView) -> Result<(), WorkspaceError> {
        let pane = self
            .pane_for_task_mut(task_id)
            .ok_or(WorkspaceError::MissingPane)?;
        pane.view = view;
        Ok(())
    }

    pub(crate) fn pane_for_task_mut(&mut self, task_id: K) -> Option<&mut TaskPane<K>> {
        self.root
            .as_mut()
            .and_then(|root| find_pane_for_task_mut(root, &task_id))
    }

    pub fn insert_after_focused(
        &mut self,
        task_id: K,
        axis: Axis,
    ) -> Result<PaneId, WorkspaceError> {
        if self.contains_task(task_id.clone()) {
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

    pub fn focus_task(&mut self, task_id: K) -> Result<(), WorkspaceError> {
        let pane_id = self
            .pane_for_task(task_id)
            .map(|pane| pane.id)
            .ok_or(WorkspaceError::MissingPane)?;
        self.focus_pane(pane_id)
    }

    /// Open a different task in the focused slot without discarding other panes
    /// or their manually sized split allocations. Compact presentation and the
    /// pane identity stay put so geometry/pins transfer with the slot.
    pub fn replace_focused_task(&mut self, task_id: K) -> Result<(), WorkspaceError> {
        if self.contains_task(task_id.clone()) {
            return self.focus_task(task_id);
        }
        let pane_id = self.focused.ok_or(WorkspaceError::MissingPane)?;
        self.focus_clock = self.focus_clock.saturating_add(1).max(1);
        let clock = self.focus_clock;
        let pane = self.pane_mut(pane_id).ok_or(WorkspaceError::MissingPane)?;
        pane.task_id = task_id;
        pane.last_focused_at = clock;
        Ok(())
    }

    pub fn set_manual_compact(&mut self, task_id: K, compact: bool) -> Result<(), WorkspaceError> {
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

    /// Copy the exact pane tree while remapping task keys.
    ///
    /// Preserves pane IDs, split IDs, allocations, presentation, focus clocks,
    /// focus/previous-focus pane IDs, and tree order. Mapping two distinct
    /// source keys onto the same destination key is rejected (`DuplicateTask`)
    /// rather than merging panes.
    pub fn map_task_keys<U, F>(&self, mut map_key: F) -> Result<Workspace<U>, WorkspaceError>
    where
        U: Clone + Ord + Eq,
        F: FnMut(&K) -> U,
    {
        let mut seen = BTreeSet::new();
        let root = match &self.root {
            Some(node) => Some(map_workspace_node(node, &mut map_key, &mut seen)?),
            None => None,
        };
        let mapped = Workspace {
            root,
            focused: self.focused,
            previous_focus: self.previous_focus,
            focus_clock: self.focus_clock,
        };
        mapped.validate()?;
        Ok(mapped)
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

    pub(crate) fn pane_mut(&mut self, pane_id: PaneId) -> Option<&mut TaskPane<K>> {
        self.root
            .as_mut()
            .and_then(|root| find_pane_mut(root, pane_id))
    }

    pub fn pin_task_axis_size(
        &mut self,
        task_id: K,
        logical_px: f32,
    ) -> Result<(), WorkspaceError> {
        if !logical_px.is_finite() || logical_px <= 0.0 {
            return Err(WorkspaceError::InvalidTree);
        }
        let root = self.root_mut().ok_or(WorkspaceError::MissingPane)?;
        if set_task_allocation(root, &task_id, Allocation::Pinned { logical_px }) {
            Ok(())
        } else {
            Err(WorkspaceError::MissingPane)
        }
    }

    pub fn reset_task_axis_size(&mut self, task_id: K) -> Result<(), WorkspaceError> {
        let root = self.root_mut().ok_or(WorkspaceError::MissingPane)?;
        if set_task_allocation(root, &task_id, Allocation::auto()) {
            Ok(())
        } else {
            Err(WorkspaceError::MissingPane)
        }
    }

    pub fn split_child_allocation(
        &self,
        split_id: SplitId,
        child_index: usize,
    ) -> Option<Allocation> {
        self.root
            .as_ref()
            .and_then(|root| find_split(root, split_id))
            .and_then(|children| children.get(child_index))
            .map(|child| child.allocation)
    }

    pub fn resize_split_child(
        &mut self,
        split_id: SplitId,
        child_index: usize,
        logical_px: f32,
    ) -> Result<(), WorkspaceError> {
        self.resize_split_child_with_parent_extent(split_id, child_index, logical_px, None)
    }

    /// Persist only explicit user intent; viewport-dependent peer adjustment is
    /// owned by the allocator and must never overwrite another custom pin.
    pub fn resize_split_child_with_parent_extent(
        &mut self,
        split_id: SplitId,
        child_index: usize,
        logical_px: f32,
        _parent_extent: Option<(f32, f32)>,
    ) -> Result<(), WorkspaceError> {
        if !logical_px.is_finite() || logical_px <= 0.0 {
            return Err(WorkspaceError::InvalidTree);
        }
        let mut candidate = self.clone();
        let root = candidate.root_mut().ok_or(WorkspaceError::MissingPane)?;
        let children = find_split_mut(root, split_id).ok_or(WorkspaceError::MissingPane)?;
        if child_index + 1 >= children.len() {
            return Err(WorkspaceError::MissingPane);
        }
        // Only the dragged pin is persisted. The allocator lets Auto peers yield,
        // then the least-recently-focused custom peers when necessary. Mutating
        // peer pins here makes saturated forward/back drags drift saved sizes.
        children[child_index].allocation = Allocation::Pinned { logical_px };
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub fn reset_split_child(
        &mut self,
        split_id: SplitId,
        child_index: usize,
    ) -> Result<(), WorkspaceError> {
        let mut candidate = self.clone();
        let root = candidate.root_mut().ok_or(WorkspaceError::MissingPane)?;
        let children = find_split_mut(root, split_id).ok_or(WorkspaceError::MissingPane)?;
        if child_index + 1 >= children.len() {
            return Err(WorkspaceError::MissingPane);
        }
        children[child_index].allocation = Allocation::auto();
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn task_is_unpinned(&self, task_id: K) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| task_path_is_auto(root, &task_id, true))
    }

    pub(crate) fn set_presentation(
        &mut self,
        task_id: K,
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

fn map_workspace_node<K, U, F>(
    node: &WorkspaceNode<K>,
    map_key: &mut F,
    seen: &mut BTreeSet<U>,
) -> Result<WorkspaceNode<U>, WorkspaceError>
where
    U: Clone + Ord + Eq,
    F: FnMut(&K) -> U,
{
    match node {
        WorkspaceNode::Pane(pane) => {
            let mapped = map_key(&pane.task_id);
            if !seen.insert(mapped.clone()) {
                return Err(WorkspaceError::DuplicateTask);
            }
            Ok(WorkspaceNode::Pane(TaskPane {
                id: pane.id,
                task_id: mapped,
                presentation: pane.presentation,
                last_focused_at: pane.last_focused_at,
                view: pane.view,
            }))
        }
        WorkspaceNode::Split { id, axis, children } => {
            let mut mapped_children = Vec::with_capacity(children.len());
            for child in children {
                mapped_children.push(SplitChild {
                    node: map_workspace_node(&child.node, map_key, seen)?,
                    allocation: child.allocation,
                });
            }
            Ok(WorkspaceNode::Split {
                id: *id,
                axis: *axis,
                children: mapped_children,
            })
        }
    }
}

fn count_panes<K>(node: &WorkspaceNode<K>) -> usize {
    match node {
        WorkspaceNode::Pane(_) => 1,
        WorkspaceNode::Split { children, .. } => {
            children.iter().map(|child| count_panes(&child.node)).sum()
        }
    }
}

fn first_pane_id<K>(node: &WorkspaceNode<K>) -> Option<PaneId> {
    match node {
        WorkspaceNode::Pane(pane) => Some(pane.id),
        WorkspaceNode::Split { children, .. } => children
            .first()
            .and_then(|child| first_pane_id(&child.node)),
    }
}

fn most_recent_focus_in_node<K>(node: &WorkspaceNode<K>) -> Option<u64> {
    match node {
        WorkspaceNode::Pane(pane) => Some(pane.last_focused_at),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .filter_map(|child| most_recent_focus_in_node(&child.node))
            .max(),
    }
}

fn find_pane<K>(node: &WorkspaceNode<K>, pane_id: PaneId) -> Option<&TaskPane<K>> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.id == pane_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .find_map(|child| find_pane(&child.node, pane_id)),
    }
}

fn find_pane_for_task<'a, K: PartialEq>(
    node: &'a WorkspaceNode<K>,
    task_id: &K,
) -> Option<&'a TaskPane<K>> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.task_id == *task_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .find_map(|child| find_pane_for_task(&child.node, task_id)),
    }
}

fn find_pane_for_task_mut<'a, K: PartialEq>(
    node: &'a mut WorkspaceNode<K>,
    task_id: &K,
) -> Option<&'a mut TaskPane<K>> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.task_id == *task_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter_mut()
            .find_map(|child| find_pane_for_task_mut(&mut child.node, task_id)),
    }
}

fn find_pane_mut<K>(node: &mut WorkspaceNode<K>, pane_id: PaneId) -> Option<&mut TaskPane<K>> {
    match node {
        WorkspaceNode::Pane(pane) => (pane.id == pane_id).then_some(pane),
        WorkspaceNode::Split { children, .. } => children
            .iter_mut()
            .find_map(|child| find_pane_mut(&mut child.node, pane_id)),
    }
}

fn find_split<K>(node: &WorkspaceNode<K>, split_id: SplitId) -> Option<&[SplitChild<K>]> {
    match node {
        WorkspaceNode::Pane(_) => None,
        WorkspaceNode::Split { id, children, .. } if *id == split_id => Some(children),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .find_map(|child| find_split(&child.node, split_id)),
    }
}

fn find_split_mut<K>(
    node: &mut WorkspaceNode<K>,
    split_id: SplitId,
) -> Option<&mut Vec<SplitChild<K>>> {
    match node {
        WorkspaceNode::Pane(_) => None,
        WorkspaceNode::Split { id, children, .. } => {
            if *id == split_id {
                Some(children)
            } else {
                children
                    .iter_mut()
                    .find_map(|child| find_split_mut(&mut child.node, split_id))
            }
        }
    }
}

fn collect_task_ids<K: Clone>(node: &WorkspaceNode<K>, task_ids: &mut Vec<K>) {
    match node {
        WorkspaceNode::Pane(pane) => task_ids.push(pane.task_id.clone()),
        WorkspaceNode::Split { children, .. } => {
            for child in children {
                collect_task_ids(&child.node, task_ids);
            }
        }
    }
}

fn set_task_allocation<K: PartialEq>(
    node: &mut WorkspaceNode<K>,
    task_id: &K,
    allocation: Allocation,
) -> bool {
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

fn contains_task<K: PartialEq>(node: &WorkspaceNode<K>, task_id: &K) -> bool {
    find_pane_for_task(node, task_id).is_some()
}

fn task_path_is_auto<K: PartialEq>(
    node: &WorkspaceNode<K>,
    task_id: &K,
    path_is_auto: bool,
) -> bool {
    match node {
        WorkspaceNode::Pane(pane) => pane.task_id == *task_id && path_is_auto,
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

fn insert_pane_near<K>(
    node: WorkspaceNode<K>,
    target: PaneId,
    pane: TaskPane<K>,
    axis: Axis,
    insert_after: bool,
) -> (WorkspaceNode<K>, bool) {
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

fn contains_pane<K>(node: &WorkspaceNode<K>, target: PaneId) -> bool {
    find_pane(node, target).is_some()
}

fn remove_pane_node<K>(
    node: WorkspaceNode<K>,
    target: PaneId,
) -> (Option<WorkspaceNode<K>>, Option<TaskPane<K>>) {
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

fn validate_node<K: Clone + Ord + Eq>(
    node: &WorkspaceNode<K>,
    pane_ids: &mut BTreeSet<PaneId>,
    task_ids: &mut BTreeSet<K>,
    split_ids: &mut BTreeSet<SplitId>,
) -> Result<(), WorkspaceError> {
    match node {
        WorkspaceNode::Pane(pane) => {
            if pane.last_focused_at == 0
                || !pane_ids.insert(pane.id)
                || !task_ids.insert(pane.task_id.clone())
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
    fn a_pane_defaults_to_the_conversation_view_and_remembers_a_set_view() {
        let mut workspace = Workspace::single(1u32);
        assert_eq!(workspace.view_of(1), Some(PaneView::Conversation));
        workspace.set_view(1, PaneView::Terminal).expect("set");
        assert_eq!(workspace.view_of(1), Some(PaneView::Terminal));
        assert_eq!(
            workspace.set_view(9, PaneView::Files),
            Err(WorkspaceError::MissingPane)
        );
    }

    #[test]
    fn a_serialized_pane_without_a_view_field_loads_as_conversation() {
        let workspace = Workspace::single(1u32);
        let mut json = serde_json::to_value(&workspace).expect("json");
        fn strip(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("view");
                    for nested in map.values_mut() {
                        strip(nested);
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
                _ => {}
            }
        }
        strip(&mut json);
        let restored: Workspace<u32> = serde_json::from_value(json).expect("old file loads");
        assert_eq!(restored.view_of(1), Some(PaneView::Conversation));
    }

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

    #[test]
    fn resizing_a_divider_pins_only_the_manually_adjusted_child() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        workspace
            .insert_after_focused(third, Axis::Horizontal)
            .unwrap();
        let WorkspaceNode::Split { id, .. } = workspace.root().unwrap() else {
            panic!("three horizontal tasks must share one split")
        };
        let split_id = *id;

        workspace.resize_split_child(split_id, 0, 320.0).unwrap();

        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 320.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::auto())
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 2),
            Some(Allocation::auto())
        );

        workspace.reset_split_child(split_id, 0).unwrap();
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::auto())
        );
    }

    #[test]
    fn edge_move_reuses_the_transactional_tree_and_keeps_focus() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        let first_pane = workspace.focused_pane_id().unwrap();
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let third_pane = workspace
            .insert_after_focused(third, Axis::Horizontal)
            .unwrap();

        workspace
            .move_pane(
                third_pane,
                DropTarget::Edge {
                    pane: first_pane,
                    edge: Edge::Top,
                },
            )
            .unwrap();

        assert_eq!(workspace.focused_pane_id(), Some(third_pane));
        assert_eq!(workspace.task_ids().len(), 3);
        assert!(workspace.validate().is_ok());
    }

    #[test]
    fn plain_replace_preserves_focused_pane_id_compact_and_sibling() {
        let first = TaskId::new();
        let second = TaskId::new();
        let next = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let focused_slot = workspace.focused_pane_id().unwrap();
        workspace.set_manual_compact(second, true).unwrap();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        // Pin the first child via resize; pin the focused (last) pane via the
        // task-axis API because resize_split_child rejects the final index.
        workspace.resize_split_child(split_id, 0, 420.0).unwrap();
        workspace.pin_task_axis_size(second, 280.0).unwrap();
        let before_first_alloc = workspace.split_child_allocation(split_id, 0);
        let before_second_alloc = workspace.split_child_allocation(split_id, 1);

        workspace.replace_focused_task(next).unwrap();

        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.focused_pane_id(), Some(focused_slot));
        assert_eq!(workspace.focused_task(), Some(next));
        assert!(workspace.contains_task(first));
        assert!(!workspace.contains_task(second));
        assert_eq!(
            workspace.presentation(next),
            Some(PanePresentation::CompactManual)
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            before_first_alloc
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            before_second_alloc
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::Pinned { logical_px: 280.0 })
        );
    }

    #[test]
    fn resize_split_child_keeps_auto_neighbors_and_falls_back_to_lrf() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        workspace
            .insert_after_focused(third, Axis::Horizontal)
            .unwrap();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.pin_task_axis_size(first, 220.0).unwrap();
        workspace.pin_task_axis_size(second, 220.0).unwrap();
        workspace.pin_task_axis_size(third, 220.0).unwrap();

        workspace.resize_split_child(split_id, 0, 360.0).unwrap();
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 360.0 })
        );
        assert!(
            matches!(
                workspace.split_child_allocation(split_id, 1),
                Some(Allocation::Pinned { logical_px: 220.0 })
            ) || matches!(
                workspace.split_child_allocation(split_id, 2),
                Some(Allocation::Pinned { logical_px: 220.0 })
            ),
            "LRF rendering may yield, but saved custom pins must remain unchanged"
        );
        assert!(
            matches!(
                workspace.split_child_allocation(split_id, 1),
                Some(Allocation::Pinned { .. })
            ) && matches!(
                workspace.split_child_allocation(split_id, 2),
                Some(Allocation::Pinned { .. })
            ),
            "all-pinned resize must not demote peers to Auto"
        );

        workspace.resize_split_child(split_id, 0, 300.0).unwrap();
        workspace.resize_split_child(split_id, 0, 400.0).unwrap();
        workspace.resize_split_child(split_id, 0, 300.0).unwrap();
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 300.0 })
        );
        assert!(workspace.validate().is_ok());
    }

    type HostTaskKey = (String, TaskId);

    fn host_key(host: &str, task: TaskId) -> HostTaskKey {
        (host.to_string(), task)
    }

    #[test]
    fn same_raw_task_id_on_two_hosts_are_distinct_panes() {
        let shared = TaskId::new();
        let local = host_key("local", shared);
        let remote = host_key("remote", shared);
        let mut workspace = Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Horizontal)
            .unwrap();

        assert_eq!(workspace.pane_count(), 2);
        assert!(workspace.contains_task(local.clone()));
        assert!(workspace.contains_task(remote.clone()));
        assert_eq!(workspace.focused_task(), Some(remote));
        assert!(workspace.validate().is_ok());
        assert_eq!(
            workspace.insert_after_focused(local, Axis::Vertical),
            Err(WorkspaceError::DuplicateTask)
        );
    }

    #[test]
    fn host_qualified_focus_replace_and_pins_preserve_geometry_slots() {
        let shared = TaskId::new();
        let other = TaskId::new();
        let local = host_key("alpha", shared);
        let remote = host_key("beta", shared);
        let replacement = host_key("beta", other);
        let mut workspace = Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Horizontal)
            .unwrap();
        let focused_slot = workspace.focused_pane_id().unwrap();
        workspace.set_manual_compact(remote.clone(), true).unwrap();
        workspace.pin_task_axis_size(remote.clone(), 280.0).unwrap();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        let pinned_before = workspace.split_child_allocation(split_id, 1);

        workspace.replace_focused_task(replacement.clone()).unwrap();

        assert_eq!(workspace.focused_pane_id(), Some(focused_slot));
        assert_eq!(workspace.focused_task(), Some(replacement.clone()));
        assert!(workspace.contains_task(local));
        assert!(!workspace.contains_task(remote));
        assert_eq!(
            workspace.presentation(replacement.clone()),
            Some(PanePresentation::CompactManual)
        );
        assert_eq!(workspace.split_child_allocation(split_id, 1), pinned_before);
        workspace.focus_task(replacement).unwrap();
        assert!(workspace.validate().is_ok());
    }

    #[test]
    fn host_qualified_workspace_serde_roundtrip_preserves_keys() {
        let shared = TaskId::new();
        let local = host_key("desk", shared);
        let remote = host_key("laptop", shared);
        let mut workspace = Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Vertical)
            .unwrap();
        workspace.pin_task_axis_size(local.clone(), 240.0).unwrap();

        let encoded = serde_json::to_value(&workspace).expect("serialize host workspace");
        let decoded: Workspace<HostTaskKey> =
            serde_json::from_value(encoded).expect("deserialize host workspace");

        assert_eq!(decoded.task_ids(), vec![local.clone(), remote.clone()]);
        assert_eq!(decoded.focused_task(), Some(remote));
        assert!(!decoded.task_is_unpinned(local));
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn legacy_task_id_workspace_serde_shape_is_unchanged() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let value = serde_json::to_value(&workspace).expect("serialize legacy workspace");
        let root = value.get("root").expect("root");
        assert!(root.get("Pane").is_some() || root.get("Split").is_some());
        if let Some(pane) = root.get("Pane") {
            assert!(pane.get("task_id").and_then(|id| id.as_str()).is_some());
        } else if let Some(split) = root.get("Split") {
            let children = split.get("children").and_then(|c| c.as_array()).unwrap();
            let task_id = &children[0]["node"]["Pane"]["task_id"];
            assert!(task_id.as_str().is_some(), "TaskId remains a UUID string");
        }
        let roundtrip: TaskWorkspace =
            serde_json::from_value(value).expect("deserialize legacy workspace");
        assert_eq!(roundtrip.task_ids(), workspace.task_ids());
    }

    #[test]
    fn map_task_keys_preserves_geometry_and_rejects_collisions() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let focused = workspace.focused_pane_id();
        let previous = workspace.previous_focus();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.pin_task_axis_size(first, 240.0).unwrap();
        workspace.set_manual_compact(second, true).unwrap();
        let first_pane = workspace.pane_for_task(first).unwrap().id;
        let second_pane = workspace.pane_for_task(second).unwrap().id;

        let mapped = workspace
            .map_task_keys(|task| ("local".to_string(), *task))
            .expect("map keys");
        assert_eq!(mapped.focused_pane_id(), focused);
        assert_eq!(mapped.previous_focus(), previous);
        assert_eq!(mapped.pane(first_pane).unwrap().id, first_pane);
        assert_eq!(mapped.pane(second_pane).unwrap().id, second_pane);
        assert_eq!(
            mapped.presentation(("local".into(), second)),
            Some(PanePresentation::CompactManual)
        );
        assert_eq!(
            mapped.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 240.0 })
        );

        assert_eq!(
            workspace.map_task_keys(|_| "same-owner".to_string()),
            Err(WorkspaceError::DuplicateTask)
        );
    }
}
