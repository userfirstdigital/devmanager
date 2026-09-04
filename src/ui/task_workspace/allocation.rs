use std::collections::BTreeMap;

use crate::domain::TaskId;

use super::layout::{Allocation, Axis, PanePresentation, Workspace, WorkspaceNode};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: finite_non_negative(width),
            height: finite_non_negative(height),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AllocationMetrics {
    pub full_min_width: f32,
    pub full_min_height: f32,
    pub compact_min_width: f32,
    pub compact_min_height: f32,
    pub divider: f32,
}

impl AllocationMetrics {
    /// Spec 6.4: a pane is Full at 320x160 or it is a 320x28 title strip.
    /// One source for the live shell and for every test that asserts what the
    /// shipped minimums actually do — a fixture that invents its own numbers
    /// cannot see a rule that only bites when the two widths are equal.
    pub const fn production() -> Self {
        Self {
            full_min_width: 320.0,
            full_min_height: 160.0,
            compact_min_width: 320.0,
            compact_min_height: 28.0,
            divider: 4.0,
        }
    }

    fn sanitized(self) -> Self {
        Self {
            full_min_width: finite_non_negative(self.full_min_width),
            full_min_height: finite_non_negative(self.full_min_height),
            compact_min_width: finite_non_negative(self.compact_min_width),
            compact_min_height: finite_non_negative(self.compact_min_height),
            divider: finite_non_negative(self.divider),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AllocatedWorkspace<K = TaskId> {
    panes: BTreeMap<K, PaneRect>,
}

impl<K> Default for AllocatedWorkspace<K> {
    fn default() -> Self {
        Self {
            panes: BTreeMap::new(),
        }
    }
}

impl<K: Clone + Ord + Eq> AllocatedWorkspace<K> {
    pub fn rect(&self, task_id: K) -> Option<PaneRect> {
        self.panes.get(&task_id).copied()
    }

    pub fn width(&self, task_id: K) -> Option<f32> {
        self.rect(task_id).map(|rect| rect.width)
    }

    pub fn height(&self, task_id: K) -> Option<f32> {
        self.rect(task_id).map(|rect| rect.height)
    }
}

impl<K: Clone + Ord + Eq> Workspace<K> {
    pub fn allocate(
        &mut self,
        viewport: Viewport,
        metrics: AllocationMetrics,
    ) -> AllocatedWorkspace<K> {
        let metrics = metrics.sanitized();
        // A zoomed pane is the whole canvas, so there is nothing to divide and
        // nothing to minimise: its peers are not on screen to be measured.
        if let Some(zoomed) = self.zoomed() {
            let mut allocated = AllocatedWorkspace::default();
            if let Some(pane) = self.pane(zoomed) {
                allocated.panes.insert(
                    pane.task_id.clone(),
                    PaneRect {
                        x: 0.0,
                        y: 0.0,
                        width: viewport.width,
                        height: viewport.height,
                    },
                );
                return allocated;
            }
        }
        self.minimise_to_fit(viewport, metrics);

        let mut allocated = AllocatedWorkspace::default();
        if let Some(root) = self.root() {
            allocate_node(
                root,
                PaneRect {
                    x: 0.0,
                    y: 0.0,
                    width: viewport.width,
                    height: viewport.height,
                },
                metrics,
                &mut allocated.panes,
            );
        }
        allocated
    }

    /// Decide which panes are strips for this viewport, from scratch.
    ///
    /// Minimisation is never a user choice, so it is re-derived rather than
    /// remembered: every pane starts Full, and the least-recently-focused
    /// unpinned one becomes a 28 px strip for as long as the tree cannot hold
    /// them all at the full minimum. Restoring first is what makes a widened
    /// window give the strips their content back.
    ///
    /// The focused pane is never a candidate — it is the one the user is
    /// looking at, and minimising it would also let a single-pane workspace
    /// collapse to a title row it could never grow out of.
    ///
    /// A strip keeps the full *width* minimum at the production tuple (320 for
    /// both), so on a horizontal split minimisation recovers nothing on the
    /// axis under pressure. Every minimisation therefore has to pay for itself:
    /// it must strictly lower the tree minimum on an axis that is over budget.
    ///
    /// One that does not is **skipped, not fatal**. The candidate order is
    /// global least-recently-focused, but the over-budget axis is governed by
    /// one branch, so the oldest pane is routinely in the branch that does not
    /// set the maximum: ending the pass there would leave a tree that a later
    /// candidate could still have made fit. The unhelpful candidate goes back
    /// to Full, is remembered as tried, and the next one is measured. Only when
    /// no untried candidate helps does the pass stop and leave the rest to the
    /// physical floor.
    ///
    /// A skip is **provisional**, so `tried` is cleared on every acceptance: a
    /// pane on the second-highest branch buys nothing while another branch is
    /// the maximum, and buys the fit itself once that branch has been lowered
    /// past it. Clearing cannot loop, because an acceptance always turns one
    /// Full pane into a strip and the candidate set is drawn from the Full
    /// ones; within a single round `tried` only grows.
    fn minimise_to_fit(&mut self, viewport: Viewport, metrics: AllocationMetrics) {
        for task_id in self.task_ids() {
            if self.presentation(task_id.clone()) == Some(PanePresentation::Minimised) {
                let _ = self.set_presentation(task_id, PanePresentation::Full);
            }
        }
        let focused = self.focused_task();
        // Candidates that were measured and bought nothing. A skipped pane is
        // restored to Full, so without this it would be picked again forever.
        let mut tried: Vec<K> = Vec::new();
        loop {
            let Some(root) = self.root() else {
                return;
            };
            let minimum = minimum_size(root, metrics);
            let over_width = minimum.width > viewport.width;
            let over_height = minimum.height > viewport.height;
            if !over_width && !over_height {
                return;
            }
            let Some(candidate) =
                self.least_recently_focused_full_unpinned(focused.as_ref(), &tried)
            else {
                return;
            };
            if self
                .set_presentation(candidate.clone(), PanePresentation::Minimised)
                .is_err()
            {
                return;
            }
            let Some(root) = self.root() else {
                return;
            };
            let relieved = minimum_size(root, metrics);
            let recovered = (over_width && relieved.width < minimum.width)
                || (over_height && relieved.height < minimum.height);
            if recovered {
                // The tree changed shape, so an earlier skip may now be the
                // candidate that fits it. Measure them all again.
                tried.clear();
            } else {
                let _ = self.set_presentation(candidate.clone(), PanePresentation::Full);
                tried.push(candidate);
            }
        }
    }

    fn least_recently_focused_full_unpinned(&self, focused: Option<&K>, tried: &[K]) -> Option<K> {
        let mut candidates: Vec<_> = self
            .task_ids()
            .into_iter()
            .filter(|task_id| Some(task_id) != focused)
            .filter(|task_id| !tried.contains(task_id))
            .filter(|task_id| self.presentation(task_id.clone()) == Some(PanePresentation::Full))
            .filter(|task_id| self.task_is_unpinned(task_id.clone()))
            .filter_map(|task_id| {
                self.pane_for_task(task_id.clone())
                    .map(|pane| (pane.last_focused_at, task_id))
            })
            .collect();
        candidates.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        candidates.into_iter().next().map(|(_, task_id)| task_id)
    }
}

#[derive(Clone, Copy)]
struct MinimumSize {
    width: f32,
    height: f32,
}

fn minimum_size<K>(node: &WorkspaceNode<K>, metrics: AllocationMetrics) -> MinimumSize {
    match node {
        WorkspaceNode::Pane(pane) => match pane.presentation {
            PanePresentation::Full => MinimumSize {
                width: metrics.full_min_width,
                height: metrics.full_min_height,
            },
            PanePresentation::Minimised => MinimumSize {
                width: metrics.compact_min_width,
                height: metrics.compact_min_height,
            },
        },
        WorkspaceNode::Split { axis, children, .. } => {
            let children_min: Vec<_> = children
                .iter()
                .map(|child| minimum_size(&child.node, metrics))
                .collect();
            let dividers = metrics.divider * children.len().saturating_sub(1) as f32;
            match axis {
                Axis::Horizontal => MinimumSize {
                    width: children_min.iter().map(|size| size.width).sum::<f32>() + dividers,
                    height: children_min
                        .iter()
                        .map(|size| size.height)
                        .fold(0.0, f32::max),
                },
                Axis::Vertical => MinimumSize {
                    width: children_min
                        .iter()
                        .map(|size| size.width)
                        .fold(0.0, f32::max),
                    height: children_min.iter().map(|size| size.height).sum::<f32>() + dividers,
                },
            }
        }
    }
}

fn allocate_node<K: Clone + Ord + Eq>(
    node: &WorkspaceNode<K>,
    rect: PaneRect,
    metrics: AllocationMetrics,
    panes: &mut BTreeMap<K, PaneRect>,
) {
    match node {
        WorkspaceNode::Pane(pane) => {
            panes.insert(pane.task_id.clone(), rect);
        }
        WorkspaceNode::Split { axis, children, .. } => {
            let extent = match axis {
                Axis::Horizontal => rect.width,
                Axis::Vertical => rect.height,
            };
            let divider_count = children.len().saturating_sub(1) as f32;
            let divider = if divider_count > 0.0 {
                metrics.divider.min(extent / divider_count)
            } else {
                0.0
            };
            let available = (extent - divider * divider_count).max(0.0);
            let minimums: Vec<f32> = children
                .iter()
                .map(|child| {
                    let preferred = minimum_size(&child.node, metrics);
                    let preferred = match axis {
                        Axis::Horizontal => preferred.width,
                        Axis::Vertical => preferred.height,
                    };
                    match child.allocation {
                        Allocation::Pinned { logical_px } => preferred.min(logical_px),
                        Allocation::Auto { .. } => preferred,
                    }
                })
                .collect();
            let fallback_resize_index = least_recent_focus_child_index(children);
            let sizes = allocate_axis_sizes(
                children,
                &minimums,
                available,
                fallback_resize_index,
                *axis,
                metrics,
            );
            let mut cursor = match axis {
                Axis::Horizontal => rect.x,
                Axis::Vertical => rect.y,
            };
            for (index, (child, size)) in children.iter().zip(sizes).enumerate() {
                let child_rect = match axis {
                    Axis::Horizontal => PaneRect {
                        x: cursor,
                        y: rect.y,
                        width: size,
                        height: rect.height,
                    },
                    Axis::Vertical => PaneRect {
                        x: rect.x,
                        y: cursor,
                        width: rect.width,
                        height: size,
                    },
                };
                allocate_node(&child.node, child_rect, metrics, panes);
                cursor += size;
                if index + 1 < children.len() {
                    cursor += divider;
                }
            }
        }
    }
}

/// Visible physical floor for an axis share under user drag pressure. Preferred
/// content mins (e.g. 360) must not steal a requested pin; Auto peers yield to
/// this floor first. Nested splits recurse so two Full panes need 64+64+divider.
const PHYSICAL_AXIS_FLOOR: f32 = 64.0;

fn nested_physical_floor<K>(
    node: &WorkspaceNode<K>,
    axis: Axis,
    metrics: AllocationMetrics,
) -> f32 {
    match node {
        WorkspaceNode::Pane(_) => PHYSICAL_AXIS_FLOOR,
        WorkspaceNode::Split {
            axis: split_axis,
            children,
            ..
        } => {
            let child_floors: Vec<f32> = children
                .iter()
                .map(|child| {
                    let descendant_floor = nested_physical_floor(&child.node, axis, metrics);
                    if *split_axis == axis {
                        match child.allocation {
                            Allocation::Auto { .. } => descendant_floor,
                            // A user pin below the physical floor is valid and
                            // must remain the lower bound for that branch. A
                            // larger pin still yields to its descendants' Auto
                            // floors if the parent is under pressure.
                            Allocation::Pinned { logical_px } => logical_px.min(descendant_floor),
                        }
                    } else {
                        // This allocation belongs to the other axis. Do not let
                        // a vertical pin constrain horizontal width (or vice
                        // versa) while finding a recursive floor.
                        descendant_floor
                    }
                })
                .collect();
            if *split_axis == axis {
                let dividers = metrics.divider * children.len().saturating_sub(1) as f32;
                child_floors.iter().sum::<f32>() + dividers
            } else {
                child_floors.into_iter().fold(PHYSICAL_AXIS_FLOOR, f32::max)
            }
        }
    }
}

fn allocate_axis_sizes<K>(
    children: &[super::layout::SplitChild<K>],
    minimums: &[f32],
    available: f32,
    fallback_resize_index: Option<usize>,
    axis: Axis,
    metrics: AllocationMetrics,
) -> Vec<f32> {
    if children.is_empty() {
        return Vec::new();
    }

    let mut sizes: Vec<f32> = children
        .iter()
        .zip(minimums)
        .map(|(child, minimum)| match child.allocation {
            Allocation::Auto { .. } => *minimum,
            Allocation::Pinned { logical_px } => logical_px,
        })
        .collect();

    let pinned_total: f32 = children
        .iter()
        .zip(&sizes)
        .map(|(child, size)| match child.allocation {
            Allocation::Pinned { .. } => *size,
            Allocation::Auto { .. } => 0.0,
        })
        .sum();
    let auto_min_total: f32 = children
        .iter()
        .zip(minimums)
        .map(|(child, minimum)| match child.allocation {
            Allocation::Auto { .. } => *minimum,
            Allocation::Pinned { .. } => 0.0,
        })
        .sum();
    let auto_floors: Vec<f32> = children
        .iter()
        .map(|child| match child.allocation {
            Allocation::Auto { .. } => nested_physical_floor(&child.node, axis, metrics),
            Allocation::Pinned { .. } => 0.0,
        })
        .collect();
    let auto_floor_total: f32 = auto_floors.iter().sum();

    let full_auto_weight: f32 = children
        .iter()
        .map(|child| match child.allocation {
            Allocation::Auto { weight } if contains_full_pane(&child.node) => weight,
            _ => 0.0,
        })
        .sum();
    let auto_weight: f32 = if full_auto_weight > 0.0 {
        full_auto_weight
    } else {
        children
            .iter()
            .map(|child| match child.allocation {
                Allocation::Auto { weight } => weight,
                Allocation::Pinned { .. } => 0.0,
            })
            .sum()
    };

    let remaining_for_auto = available - pinned_total;
    let distribute_auto = |sizes: &mut [f32], auto_base: &[f32], auto_base_total: f32| {
        let auto_extra = (remaining_for_auto - auto_base_total).max(0.0);
        for (index, child) in children.iter().enumerate() {
            let weight = match child.allocation {
                Allocation::Auto { weight }
                    if full_auto_weight > 0.0 && contains_full_pane(&child.node) =>
                {
                    weight
                }
                Allocation::Auto { weight } if full_auto_weight == 0.0 => weight,
                _ => 0.0,
            };
            if matches!(child.allocation, Allocation::Auto { .. }) {
                sizes[index] = auto_base[index] + auto_extra * weight / auto_weight;
            }
        }
    };
    if remaining_for_auto + f32::EPSILON >= auto_min_total && auto_weight > 0.0 {
        distribute_auto(&mut sizes, minimums, auto_min_total);
        return sizes;
    }
    if remaining_for_auto + f32::EPSILON >= auto_floor_total && auto_weight > 0.0 {
        distribute_auto(&mut sizes, &auto_floors, auto_floor_total);
        return sizes;
    }

    for (index, child) in children.iter().enumerate() {
        if matches!(child.allocation, Allocation::Auto { .. }) {
            sizes[index] = auto_floors[index];
        }
    }
    let mut deficit = sizes.iter().sum::<f32>() - available;
    if deficit > 0.0 {
        // Oversized pins may compress so nested Auto floors (e.g.
        // 64+64+divider) can claim space before residual scale. Preserve a
        // nested branch's recursive Auto floor while there is room for it, and
        // never raise a custom pin that is already below 64px.
        let compression_floor = |index: usize| match children[index].allocation {
            Allocation::Auto { .. } => auto_floors[index],
            Allocation::Pinned { logical_px } => {
                logical_px.min(nested_physical_floor(&children[index].node, axis, metrics))
            }
        };
        if let Some(index) = fallback_resize_index.filter(|index| *index < sizes.len()) {
            let floor = compression_floor(index);
            let reducible = (sizes[index] - floor).max(0.0);
            let take = deficit.min(reducible);
            sizes[index] -= take;
            deficit -= take;
        }
        if deficit > 0.0 {
            let mut order: Vec<_> = (0..children.len()).collect();
            order.sort_by_key(|index| most_recent_focus(&children[*index].node).unwrap_or(0));
            for index in order {
                if deficit <= 0.0 {
                    break;
                }
                let floor = compression_floor(index);
                let reducible = (sizes[index] - floor).max(0.0);
                let take = deficit.min(reducible);
                sizes[index] -= take;
                deficit -= take;
            }
        }
        if deficit > 0.0 {
            let total: f32 = sizes.iter().sum();
            if total > 0.0 {
                let scale = available / total;
                for size in &mut sizes {
                    *size *= scale;
                }
            }
        }
    } else if deficit < 0.0 {
        let surplus = -deficit;
        if let Some(index) = fallback_resize_index.filter(|index| *index < sizes.len()) {
            sizes[index] += surplus;
        } else if let Some(last) = sizes.last_mut() {
            *last += surplus;
        }
    }

    sizes
}

/// Rank each split child by its most-recent focus among descendants, then pick
/// the least-recently-focused branch so an active nested pane is not resized
/// when an older wholly-unfocused sibling branch exists.
fn least_recent_focus_child_index<K>(children: &[super::layout::SplitChild<K>]) -> Option<usize> {
    children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| {
            most_recent_focus(&child.node).map(|last_focused_at| (index, last_focused_at))
        })
        .min_by_key(|(_, last_focused_at)| *last_focused_at)
        .map(|(index, _)| index)
}

fn most_recent_focus<K>(node: &WorkspaceNode<K>) -> Option<u64> {
    match node {
        WorkspaceNode::Pane(pane) => Some(pane.last_focused_at),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .filter_map(|child| most_recent_focus(&child.node))
            .max(),
    }
}

fn contains_full_pane<K>(node: &WorkspaceNode<K>) -> bool {
    match node {
        WorkspaceNode::Pane(pane) => pane.presentation == PanePresentation::Full,
        WorkspaceNode::Split { children, .. } => {
            children.iter().any(|child| contains_full_pane(&child.node))
        }
    }
}

fn finite_non_negative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskId;
    use crate::ui::task_workspace::{Axis, PanePresentation, TaskWorkspace};

    fn metrics() -> AllocationMetrics {
        AllocationMetrics {
            full_min_width: 220.0,
            full_min_height: 180.0,
            compact_min_width: 80.0,
            compact_min_height: 64.0,
            divider: 0.0,
        }
    }

    fn three_horizontal_tasks() -> (TaskWorkspace, [TaskId; 3]) {
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
        (workspace, [first, second, third])
    }

    fn legacy_fixture_metrics() -> AllocationMetrics {
        AllocationMetrics {
            full_min_width: 360.0,
            full_min_height: 300.0,
            compact_min_width: 210.0,
            compact_min_height: 116.0,
            divider: 4.0,
        }
    }

    // Build an H branch below a V branch below the root H split. This lets the
    // tests distinguish floors on the requested axis from pins on the other
    // axis without reaching into production-only tree construction helpers.
    fn nested_horizontal_branch() -> (TaskWorkspace, TaskId, TaskId, TaskId, TaskId) {
        let outer = TaskId::new();
        let inner_left = TaskId::new();
        let cross_axis = TaskId::new();
        let inner_right = TaskId::new();
        let mut workspace = TaskWorkspace::single(outer);
        workspace
            .insert_after_focused(inner_left, Axis::Horizontal)
            .unwrap();
        workspace.focus_task(inner_left).unwrap();
        workspace
            .insert_after_focused(cross_axis, Axis::Vertical)
            .unwrap();
        workspace.focus_task(inner_left).unwrap();
        workspace
            .insert_after_focused(inner_right, Axis::Horizontal)
            .unwrap();
        (workspace, outer, inner_left, inner_right, cross_axis)
    }

    fn set_root_child_allocation(
        workspace: &mut TaskWorkspace,
        child_index: usize,
        allocation: Allocation,
    ) {
        let WorkspaceNode::Split { children, .. } = workspace.root_mut().expect("root") else {
            panic!("expected split root")
        };
        children[child_index].allocation = allocation;
    }

    #[test]
    fn pinned_children_keep_requested_pixels_while_auto_children_share_the_remainder() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(first, 300.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());

        assert_eq!(allocated.width(first), Some(300.0));
        assert_eq!(allocated.width(second), Some(350.0));
        assert_eq!(allocated.width(third), Some(350.0));
    }

    #[test]
    fn narrow_pressure_minimises_the_oldest_pane_and_still_fills_parent() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();

        // Three Full panes need 660 of the 550 available, so the oldest yields.
        let allocated = workspace.allocate(Viewport::new(550.0, 700.0), metrics());
        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(third), Some(PanePresentation::Full));
        assert_eq!(
            allocated.width(first),
            Some(80.0),
            "the strip keeps its min"
        );
        assert_eq!(
            allocated.width(first).unwrap()
                + allocated.width(second).unwrap()
                + allocated.width(third).unwrap(),
            550.0,
            "minimising must not leave an unowned gap"
        );
    }

    #[test]
    fn a_horizontal_split_stays_full_when_minimising_cannot_recover_width() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .expect("second pane");

        // Two Full panes need 320+320+4 = 644 of the 600 available, so the
        // width axis is over budget. A strip keeps the full 320 width by
        // design, so minimising `first` would leave the tree minimum at 644 —
        // no width recovered and a pane stripped for nothing. Both stay Full
        // and the physical floor squeezes them instead.
        let allocated =
            workspace.allocate(Viewport::new(600.0, 700.0), AllocationMetrics::production());

        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
        assert_eq!(allocated.width(first), Some(298.0));
        assert_eq!(allocated.width(second), Some(298.0));
        assert_eq!(
            allocated.width(first).unwrap() + 4.0 + allocated.width(second).unwrap(),
            600.0,
            "leaving both Full must not leave an unowned gap"
        );
    }

    #[test]
    fn a_vertical_split_minimises_the_oldest_until_the_tree_fits() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Vertical)
            .expect("second pane");
        workspace
            .insert_after_focused(third, Axis::Vertical)
            .expect("third pane");

        // Height is the divided axis here, and a strip is 28 rather than 160,
        // so each minimisation does recover room. Three Full panes need
        // 160*3 + 4*2 = 488 of 300: stripping `first` leaves 356, still short,
        // so `second` follows and the pass stops at 224.
        let allocated =
            workspace.allocate(Viewport::new(400.0, 300.0), AllocationMetrics::production());

        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(
            workspace.presentation(second),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(
            workspace.presentation(third),
            Some(PanePresentation::Full),
            "the focused pane keeps its content"
        );
        assert_eq!(allocated.height(first), Some(28.0));
        assert_eq!(allocated.height(second), Some(28.0));
        assert_eq!(allocated.height(third), Some(236.0));
        assert_eq!(
            allocated.height(first).unwrap()
                + 4.0
                + allocated.height(second).unwrap()
                + 4.0
                + allocated.height(third).unwrap(),
            300.0
        );
    }

    #[test]
    fn an_unhelpful_candidate_is_skipped_rather_than_ending_the_pass() {
        let a = TaskId::new();
        let b = TaskId::new();
        let c = TaskId::new();
        let mut workspace = TaskWorkspace::single(a);
        workspace
            .insert_after_focused(c, Axis::Horizontal)
            .expect("c beside a");
        workspace
            .insert_after_focused(b, Axis::Vertical)
            .expect("b under c");
        workspace.focus_task(c).expect("focus c");

        // H[a, V[c, b]] at 800x300. Height is over budget: max(160, 160+4+160)
        // = 324. The globally least-recently-focused candidate is `a`, and `a`
        // is in the branch that is NOT the max, so minimising it leaves the
        // root at 324 - it buys nothing. `a` must therefore be skipped rather
        // than ending the pass: `b` is next, and it lowers the V branch to
        // 28+4+160 = 192, so max(160, 192) = 192 and the tree fits.
        let allocated =
            workspace.allocate(Viewport::new(800.0, 300.0), AllocationMetrics::production());

        assert_eq!(
            workspace.presentation(b),
            Some(PanePresentation::Minimised),
            "the candidate that governs the over-budget axis is the one that yields"
        );
        assert_eq!(
            workspace.presentation(a),
            Some(PanePresentation::Full),
            "a skipped candidate is left Full, not stripped for nothing"
        );
        assert_eq!(workspace.presentation(c), Some(PanePresentation::Full));
        assert_eq!(allocated.height(b), Some(28.0));
        assert_eq!(
            allocated.height(c),
            Some(268.0),
            "c keeps more than its 160 full minimum, so nothing is squeezed"
        );
        assert_eq!(allocated.height(a), Some(300.0));
        assert_eq!(
            allocated.height(c).unwrap() + 4.0 + allocated.height(b).unwrap(),
            300.0
        );
    }

    #[test]
    fn a_skipped_candidate_is_retried_once_the_governing_branch_is_lowered() {
        let p = TaskId::new();
        let p2 = TaskId::new();
        let q = TaskId::new();
        let q2 = TaskId::new();
        let q3 = TaskId::new();
        let mut workspace = TaskWorkspace::single(p);
        workspace
            .insert_after_focused(q, Axis::Horizontal)
            .expect("second branch");
        workspace.focus_task(p).expect("focus p");
        workspace
            .insert_after_focused(p2, Axis::Vertical)
            .expect("p2 under p");
        workspace.focus_task(q).expect("focus q");
        workspace
            .insert_after_focused(q2, Axis::Vertical)
            .expect("q2 under q");
        workspace
            .insert_after_focused(q3, Axis::Vertical)
            .expect("q3 under q2");
        workspace.focus_task(q3).expect("focus q3");

        // H[ A=V[p, p2], B=V[q, q2, q3] ] at 800x300, and A's panes are the
        // OLDEST, so they are measured first. A is 324 and B is 488, so B
        // governs the height: `p` and `p2` are both skipped, then `q` (488 ->
        // 356) is accepted, which clears the skip list; `p` and `p2` are
        // skipped again (A's 324 is still under B's 356), then `q2` (356 ->
        // 324) is accepted. At that point B is 224 and A's 324 is the maximum,
        // so `p` -- skipped twice -- is the only candidate left that can help.
        // Without re-trying it the pass stops at 324 against a 300 px
        // viewport; re-trying it reaches 224 and fits.
        let allocated =
            workspace.allocate(Viewport::new(800.0, 300.0), AllocationMetrics::production());

        assert_eq!(
            workspace.presentation(p),
            Some(PanePresentation::Minimised),
            "a skip is provisional: once B stopped governing, p had to be measured again"
        );
        assert_eq!(workspace.presentation(q), Some(PanePresentation::Minimised));
        assert_eq!(
            workspace.presentation(q2),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(
            workspace.presentation(p2),
            Some(PanePresentation::Full),
            "and no more than needed: p2 is left alone once the tree fits"
        );
        assert_eq!(workspace.presentation(q3), Some(PanePresentation::Full));

        // 224 <= 300, so every pane is at or above its own minimum and nothing
        // is squeezed under the physical floor.
        assert_eq!(allocated.height(p), Some(28.0));
        assert_eq!(allocated.height(p2), Some(268.0));
        assert_eq!(allocated.height(q), Some(28.0));
        assert_eq!(allocated.height(q2), Some(28.0));
        assert_eq!(allocated.height(q3), Some(236.0));
        assert_eq!(
            allocated.height(q).unwrap()
                + 4.0
                + allocated.height(q2).unwrap()
                + 4.0
                + allocated.height(q3).unwrap(),
            300.0
        );
    }

    #[test]
    fn every_candidate_skipped_leaves_the_tree_to_the_physical_floor() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(third).expect("focus third");

        // Three Full panes need 320*3 + 4*2 = 968 of 600, and a strip keeps the
        // full 320 width, so neither `first` nor `second` lowers the width by a
        // pixel. Both are tried, both are put back, the candidate set is
        // exhausted, and the 64 px physical floor absorbs the pressure instead.
        let allocated =
            workspace.allocate(Viewport::new(600.0, 700.0), AllocationMetrics::production());

        for task in [first, second, third] {
            assert_eq!(
                workspace.presentation(task),
                Some(PanePresentation::Full),
                "no pane is stripped when stripping recovers nothing"
            );
            let width = allocated.width(task).expect("rect");
            assert!(
                width < 320.0,
                "the floor, not minimisation, is what squeezes: {width}"
            );
        }
        let total = allocated.width(first).unwrap()
            + 4.0
            + allocated.width(second).unwrap()
            + 4.0
            + allocated.width(third).unwrap();
        assert!(
            (total - 600.0).abs() < 0.01,
            "the children still own the whole parent: {total}"
        );
    }

    #[test]
    fn a_zoomed_workspace_allocates_one_full_canvas_rectangle() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .expect("pane");
        let pane = workspace.pane_for_task(second).expect("pane").id;
        workspace.zoom(pane).expect("zoom");

        let allocated =
            workspace.allocate(Viewport::new(500.0, 400.0), AllocationMetrics::production());

        assert_eq!(
            allocated.rect(first),
            None,
            "only the zoomed pane is placed"
        );
        assert_eq!(
            allocated.rect(second).map(|rect| (rect.width, rect.height)),
            Some((500.0, 400.0))
        );
        assert!(
            workspace
                .task_ids()
                .into_iter()
                .all(|task| workspace.presentation(task) == Some(PanePresentation::Full)),
            "zoom never minimises"
        );
    }

    #[test]
    fn a_pane_under_320_by_160_is_minimised_to_a_28px_strip_and_restored_when_room_returns() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Vertical)
            .expect("second pane");

        // 250 px tall: two Full panes need 160+4+160 = 324, so one must
        // become a strip; 28+4+160 = 192 then fits.
        let allocated =
            workspace.allocate(Viewport::new(400.0, 250.0), AllocationMetrics::production());

        let minimised: Vec<_> = workspace
            .task_ids()
            .into_iter()
            .filter(|task| workspace.presentation(*task) == Some(PanePresentation::Minimised))
            .collect();
        assert_eq!(
            minimised,
            vec![first],
            "the least-recently-focused unpinned pane yields, never the focused one"
        );
        assert_eq!(allocated.height(first), Some(28.0), "the strip is 28 px");
        assert_eq!(
            allocated.height(second),
            Some(218.0),
            "the surviving Full pane takes the rest of the 246 divided px"
        );

        let restored =
            workspace.allocate(Viewport::new(400.0, 400.0), AllocationMetrics::production());

        assert!(
            workspace
                .task_ids()
                .into_iter()
                .all(|task| workspace.presentation(task) == Some(PanePresentation::Full)),
            "returning room restores every strip"
        );
        assert_eq!(restored.height(first), Some(198.0));
        assert_eq!(restored.height(second), Some(198.0));
    }

    #[test]
    fn a_minimised_pane_is_restored_to_full_when_the_viewport_can_hold_it() {
        let (mut workspace, [first, second, _third]) = three_horizontal_tasks();
        workspace
            .set_presentation(first, PanePresentation::Minimised)
            .unwrap();
        workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
    }

    #[test]
    fn the_focused_pane_is_never_minimised_however_narrow_the_viewport_gets() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();

        workspace.allocate(Viewport::new(60.0, 60.0), metrics());

        assert_eq!(
            workspace.presentation(second),
            Some(PanePresentation::Full),
            "the pane the user is looking at keeps its content"
        );
        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(
            workspace.presentation(third),
            Some(PanePresentation::Minimised)
        );
    }

    #[test]
    fn a_strip_beside_a_full_pane_fills_the_parent_without_an_unowned_gap() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Vertical)
            .unwrap();

        // Presentation is re-derived from the viewport on every pass, so a
        // strip is set up by giving the tree too little room, never by calling
        // set_presentation: 500 px cannot hold 300+4+300, and 116+4+300 fits.
        // Two strips is not a state this allocator can reach at all, because
        // the focused pane is never a candidate.
        let allocated = workspace.allocate(Viewport::new(700.0, 500.0), legacy_fixture_metrics());

        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
        assert_eq!(
            allocated.height(first),
            Some(116.0),
            "the strip keeps its min"
        );
        assert_eq!(allocated.height(second), Some(380.0));
        assert_eq!(
            allocated.height(first).unwrap() + 4.0 + allocated.height(second).unwrap(),
            500.0
        );
    }

    #[test]
    fn nested_splits_conserve_parent_extent_on_both_axes() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        workspace.focus_task(second).unwrap();
        workspace
            .insert_after_focused(third, Axis::Vertical)
            .unwrap();
        workspace.focus_task(first).unwrap();

        // 500 px cannot hold the nested branch's two Full panes (300+4+300),
        // and `second` is the oldest unfocused candidate, so the strip in this
        // allocation is one the pass actually produced and one that sticks.
        let allocated = workspace.allocate(Viewport::new(1_000.0, 500.0), legacy_fixture_metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(
            workspace.presentation(second),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(workspace.presentation(third), Some(PanePresentation::Full));
        let first_rect = allocated.rect(first).expect("first");
        let second_rect = allocated.rect(second).expect("second");
        let third_rect = allocated.rect(third).expect("third");

        assert_eq!(first_rect.width + 4.0 + second_rect.width, 1_000.0);
        assert_eq!(first_rect.height, 500.0);
        assert_eq!(second_rect.height + 4.0 + third_rect.height, 500.0);
        assert_eq!(second_rect.width, third_rect.width);
    }

    #[test]
    fn all_pinned_children_resize_the_least_recently_focused_to_fill() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();
        workspace.focus_task(third).unwrap();
        workspace.pin_task_axis_size(first, 200.0).unwrap();
        workspace.pin_task_axis_size(second, 200.0).unwrap();
        workspace.pin_task_axis_size(third, 200.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());
        let widths = [
            allocated.width(first).unwrap(),
            allocated.width(second).unwrap(),
            allocated.width(third).unwrap(),
        ];
        assert_eq!(widths.iter().sum::<f32>(), 1_000.0);
        assert!(
            widths[0] > 200.0,
            "least-recently-focused pinned child absorbs surplus: {widths:?}"
        );
        assert_eq!(widths[1], 200.0);
        assert_eq!(widths[2], 200.0);
    }

    #[test]
    fn pinned_deficit_is_absorbed_by_auto_siblings_before_pinned_shrink() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(first, 300.0).unwrap();
        workspace.pin_task_axis_size(second, 250.0).unwrap();

        let wide = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());
        assert_eq!(wide.width(first), Some(300.0));
        assert_eq!(wide.width(second), Some(250.0));
        assert_eq!(wide.width(third), Some(450.0));

        let narrow = workspace.allocate(Viewport::new(900.0, 700.0), metrics());
        assert_eq!(narrow.width(first), Some(300.0));
        assert_eq!(narrow.width(second), Some(250.0));
        assert_eq!(
            narrow.width(third),
            Some(350.0),
            "auto sibling absorbs the 100px deficit while pinned sizes stay put"
        );
    }

    #[test]
    fn all_pinned_deficit_shrinks_least_recent_branch_first() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();
        workspace.focus_task(third).unwrap();
        workspace.pin_task_axis_size(first, 400.0).unwrap();
        workspace.pin_task_axis_size(second, 400.0).unwrap();
        workspace.pin_task_axis_size(third, 400.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_050.0, 700.0), metrics());
        let widths = [
            allocated.width(first).unwrap(),
            allocated.width(second).unwrap(),
            allocated.width(third).unwrap(),
        ];
        assert!((widths.iter().sum::<f32>() - 1_050.0).abs() < 0.01);
        assert!(
            widths[0] < 400.0,
            "LRF pinned branch absorbs deficit first: {widths:?}"
        );
        assert_eq!(widths[1], 400.0);
        assert_eq!(widths[2], 400.0);
        let smaller = workspace.allocate(Viewport::new(900.0, 700.0), metrics());
        assert_eq!(
            smaller.width(first),
            Some(100.0),
            "LRF pins may yield to the visible physical floor before other pins"
        );
        assert_eq!(smaller.width(second), Some(400.0));
        assert_eq!(smaller.width(third), Some(400.0));
    }

    #[test]
    fn nested_lrf_uses_most_recent_focus_per_branch() {
        let left_old = TaskId::new();
        let left_older = TaskId::new();
        let focused = TaskId::new();
        let mut workspace = TaskWorkspace::single(left_old);
        workspace
            .insert_after_focused(focused, Axis::Horizontal)
            .unwrap();
        workspace.focus_task(left_old).unwrap();
        workspace
            .insert_after_focused(left_older, Axis::Vertical)
            .unwrap();
        workspace.focus_task(focused).unwrap();
        let horizontal_id = match workspace.root().expect("root") {
            crate::ui::task_workspace::WorkspaceNode::Split {
                id,
                axis: Axis::Horizontal,
                ..
            } => *id,
            _ => panic!("expected horizontal root after insert"),
        };
        workspace
            .resize_split_child(horizontal_id, 0, 500.0)
            .unwrap();
        workspace.pin_task_axis_size(focused, 500.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), metrics());
        let left_w = allocated.width(left_old).unwrap();
        let right_w = allocated.width(focused).unwrap();
        assert!((left_w + right_w - 800.0).abs() < 0.01);
        assert_eq!(right_w, 500.0, "active right branch keeps preferred size");
        assert_eq!(
            left_w, 300.0,
            "older left branch (lower max focus) absorbs horizontal deficit"
        );
    }

    #[test]
    fn pinned_sizes_stay_put_when_an_auto_sibling_can_absorb_resize() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(first, 300.0).unwrap();
        workspace.pin_task_axis_size(second, 250.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());
        assert_eq!(allocated.width(first), Some(300.0));
        assert_eq!(allocated.width(second), Some(250.0));
        assert_eq!(allocated.width(third), Some(450.0));
    }

    #[test]
    fn narrow_pressure_minimises_oldest_first_only_until_the_tree_fits() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(third).unwrap();

        // 1088 needed of 900: minimising `first` leaves 938, still short, so
        // `second` follows and the pass then stops at 788 rather than stripping
        // every pane it is allowed to.
        let allocated = workspace.allocate(Viewport::new(900.0, 700.0), legacy_fixture_metrics());
        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(
            workspace.presentation(second),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(workspace.presentation(third), Some(PanePresentation::Full));
        let total = allocated.width(first).unwrap()
            + 4.0
            + allocated.width(second).unwrap()
            + 4.0
            + allocated.width(third).unwrap();
        assert_eq!(total, 900.0);
    }

    #[test]
    fn requested_pin_keeps_size_while_auto_yields_below_preferred_min() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let split_id = match workspace.root().expect("root") {
            crate::ui::task_workspace::WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.resize_split_child(split_id, 0, 500.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), legacy_fixture_metrics());
        assert_eq!(
            allocated.width(first),
            Some(500.0),
            "requested pin must survive preferred Auto min pressure"
        );
        assert_eq!(
            allocated.width(second),
            Some(296.0),
            "Auto peer yields residual under physical floor, not preferred 360: {:?}",
            allocated.width(second)
        );
        assert!(matches!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::Auto { .. })
        ));
    }

    #[test]
    fn mixed_full_and_minimised_auto_peers_conserve_extent_under_pin_pressure() {
        let (mut workspace, [pinned, full_auto, focused_auto]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(pinned, 500.0).unwrap();
        let metrics = legacy_fixture_metrics();

        // Three Full panes need 1088 of 800. The pinned pane is not a
        // candidate and the third is focused, so `full_auto` is the strip.
        let at_500 = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(
            workspace.presentation(full_auto),
            Some(PanePresentation::Minimised)
        );
        assert_eq!(at_500.width(pinned), Some(500.0));
        assert_eq!(at_500.width(full_auto), Some(64.0));
        assert_eq!(at_500.width(focused_auto), Some(228.0));
        assert_eq!(
            at_500.width(pinned).unwrap()
                + metrics.divider
                + at_500.width(full_auto).unwrap()
                + metrics.divider
                + at_500.width(focused_auto).unwrap(),
            800.0
        );

        workspace.pin_task_axis_size(pinned, 700.0).unwrap();
        let at_700 = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(at_700.width(pinned), Some(664.0));
        assert_eq!(at_700.width(full_auto), Some(64.0));
        assert_eq!(at_700.width(focused_auto), Some(64.0));
        assert_eq!(
            at_700.width(pinned).unwrap()
                + metrics.divider
                + at_700.width(full_auto).unwrap()
                + metrics.divider
                + at_700.width(focused_auto).unwrap(),
            800.0
        );
    }

    #[test]
    fn all_pinned_resize_keeps_lrf_peer_pinned_with_residual() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(first, 220.0).unwrap();
        workspace.pin_task_axis_size(second, 220.0).unwrap();
        workspace.pin_task_axis_size(third, 220.0).unwrap();
        let split_id = match workspace.root().expect("root") {
            crate::ui::task_workspace::WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.focus_task(first).unwrap();
        workspace.resize_split_child(split_id, 0, 360.0).unwrap();
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 360.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 2),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
    }

    #[test]
    fn nested_splits_conserve_parent_extent_after_forward_and_back_resize() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        workspace.focus_task(second).unwrap();
        workspace
            .insert_after_focused(third, Axis::Vertical)
            .unwrap();
        let horizontal_id = match workspace.root().expect("root") {
            crate::ui::task_workspace::WorkspaceNode::Split {
                id,
                axis: Axis::Horizontal,
                ..
            } => *id,
            _ => panic!("expected horizontal root"),
        };
        workspace
            .resize_split_child(horizontal_id, 0, 420.0)
            .unwrap();
        workspace
            .resize_split_child(horizontal_id, 0, 380.0)
            .unwrap();
        workspace
            .resize_split_child(horizontal_id, 0, 460.0)
            .unwrap();
        let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), legacy_fixture_metrics());
        let first_rect = allocated.rect(first).expect("first");
        let second_rect = allocated.rect(second).expect("second");
        let third_rect = allocated.rect(third).expect("third");
        assert!((first_rect.width + 4.0 + second_rect.width - 1_000.0).abs() < 0.01);
        assert!((second_rect.height + 4.0 + third_rect.height - 700.0).abs() < 0.01);
        assert_eq!(second_rect.width, third_rect.width);
    }

    #[test]
    fn nested_auto_physical_floor_claims_space_before_oversized_pin() {
        // outer Pinned(700) + Auto nested H(Full,Full) @ 800 with divider 4:
        // nested floor = 64+64+4 = 132; pin must yield rather than leave 46/46.
        let (mut workspace, outer, nested_a, nested_b, cross_axis) = nested_horizontal_branch();
        workspace.pin_task_axis_size(outer, 700.0).unwrap();
        // This pin is vertical at the intermediate V split and must not
        // inflate the horizontal floor of the nested branch.
        workspace.pin_task_axis_size(cross_axis, 700.0).unwrap();
        let metrics = legacy_fixture_metrics();
        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        let nested_w = allocated.width(nested_a).unwrap()
            + metrics.divider
            + allocated.width(nested_b).unwrap();
        assert!(
            nested_w + 0.01 >= 132.0,
            "nested Auto branch must receive recursive floor 132, got {nested_w}"
        );
        assert_eq!(allocated.width(nested_a), Some(64.0));
        assert_eq!(allocated.width(nested_b), Some(64.0));
        assert_eq!(allocated.width(outer), Some(664.0));
        let total = allocated.width(outer).unwrap() + metrics.divider + nested_w;
        assert!((total - 800.0).abs() < 0.01, "extent conserved: {total}");
    }

    #[test]
    fn pinned_nested_branch_preserves_descendant_auto_floor_when_compressed() {
        let (mut workspace, outer, nested_a, nested_b, _cross_axis) = nested_horizontal_branch();
        // The root's second child is the nested V branch. Pin that branch at
        // the root H axis while keeping its inner H children automatic.
        workspace.pin_task_axis_size(outer, 700.0).unwrap();
        set_root_child_allocation(&mut workspace, 1, Allocation::Pinned { logical_px: 500.0 });
        workspace.focus_task(outer).unwrap();

        let metrics = legacy_fixture_metrics();
        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(allocated.width(outer), Some(664.0));
        assert_eq!(allocated.width(nested_a), Some(64.0));
        assert_eq!(allocated.width(nested_b), Some(64.0));
        assert_eq!(
            allocated.width(nested_a).unwrap()
                + metrics.divider
                + allocated.width(nested_b).unwrap(),
            132.0
        );
    }

    #[test]
    fn nested_physical_floor_honors_custom_pin_below_64_on_matching_axis() {
        let (mut workspace, outer, nested_a, nested_b, _cross_axis) = nested_horizontal_branch();
        workspace.pin_task_axis_size(outer, 700.0).unwrap();
        // nested_a is a child of the inner H split, so this is a horizontal pin.
        workspace.pin_task_axis_size(nested_a, 32.0).unwrap();

        let metrics = legacy_fixture_metrics();
        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(allocated.width(nested_a), Some(32.0));
        assert_eq!(allocated.width(nested_b), Some(64.0));
        assert_eq!(allocated.width(outer), Some(696.0));
        assert_eq!(
            allocated.width(nested_a).unwrap()
                + metrics.divider
                + allocated.width(nested_b).unwrap(),
            100.0
        );
    }

    #[test]
    fn custom_pin_below_physical_floor_is_not_raised() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        workspace.pin_task_axis_size(first, 32.0).unwrap();
        workspace.pin_task_axis_size(second, 32.0).unwrap();
        // In an all-pinned split the LRF peer must absorb spare viewport space.
        // Focus the pin under test so the assertion checks its floor, not the
        // deliberate LRF fill behavior.
        workspace.focus_task(first).unwrap();
        let pinned_workspace = workspace.clone();
        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), legacy_fixture_metrics());
        assert_eq!(
            allocated.width(first),
            Some(32.0),
            "valid custom pin 32 must not clamp up to physical floor 64"
        );
        assert_eq!(allocated.width(second), Some(764.0));
        assert_eq!(
            workspace, pinned_workspace,
            "fill must not mutate stored pins"
        );
    }

    #[test]
    fn tiny_viewport_conserves_divider_without_overflow() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let metrics = legacy_fixture_metrics();
        let allocated = workspace.allocate(Viewport::new(2.0, 100.0), metrics);
        let last = allocated.rect(second).unwrap();
        let total = last.x + last.width;
        assert!(
            total <= 2.0 + 0.01,
            "parent width 2 with divider 4 must not emit overflow total {total}"
        );

        let first = TaskId::new();
        let second = TaskId::new();
        let mut vertical = TaskWorkspace::single(first);
        vertical
            .insert_after_focused(second, Axis::Vertical)
            .unwrap();
        let allocated = vertical.allocate(Viewport::new(100.0, 2.0), metrics);
        let last = allocated.rect(second).unwrap();
        let total = last.y + last.height;
        assert!(
            total <= 2.0 + 0.01,
            "parent height 2 with divider 4 must not emit overflow total {total}"
        );
    }

    #[test]
    fn gesture_parent_extent_keeps_all_pinned_residual_stable_across_reverse() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.pin_task_axis_size(first, 220.0).unwrap();
        workspace.pin_task_axis_size(second, 220.0).unwrap();
        workspace.pin_task_axis_size(third, 220.0).unwrap();
        let split_id = match workspace.root().expect("root") {
            crate::ui::task_workspace::WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.focus_task(first).unwrap();
        let metrics = legacy_fixture_metrics();
        let viewport = Viewport::new(800.0, 700.0);
        let parent_extent = 800.0;
        let divider_total = metrics.divider * 2.0;
        workspace
            .resize_split_child_with_parent_extent(
                split_id,
                0,
                500.0,
                Some((parent_extent, divider_total)),
            )
            .unwrap();
        workspace
            .resize_split_child_with_parent_extent(
                split_id,
                0,
                700.0,
                Some((parent_extent, divider_total)),
            )
            .unwrap();
        workspace
            .resize_split_child_with_parent_extent(
                split_id,
                0,
                500.0,
                Some((parent_extent, divider_total)),
            )
            .unwrap();
        workspace
            .resize_split_child_with_parent_extent(
                split_id,
                0,
                220.0,
                Some((parent_extent, divider_total)),
            )
            .unwrap();
        let allocated = workspace.allocate(viewport, metrics);
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 2),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(allocated.width(first), Some(220.0));
        assert_eq!(allocated.width(second), Some(352.0));
        assert_eq!(allocated.width(third), Some(220.0));
        let total = allocated.width(first).unwrap()
            + metrics.divider
            + allocated.width(second).unwrap()
            + metrics.divider
            + allocated.width(third).unwrap();
        assert!((total - 800.0).abs() < 0.01);

        // Changing focus changes only the LRF recipient; it must not mutate
        // any stored custom pin.
        workspace.focus_task(second).unwrap();
        workspace.focus_task(third).unwrap();
        let changed_focus = workspace.allocate(viewport, metrics);
        assert_eq!(changed_focus.width(first), Some(352.0));
        assert_eq!(changed_focus.width(second), Some(220.0));
        assert_eq!(changed_focus.width(third), Some(220.0));
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 2),
            Some(Allocation::Pinned { logical_px: 220.0 })
        );
    }

    #[test]
    fn host_qualified_keys_allocate_distinct_rects_for_shared_raw_task_id() {
        let shared = TaskId::new();
        let local = ("local".to_string(), shared);
        let remote = ("remote".to_string(), shared);
        let mut workspace = crate::ui::task_workspace::Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Horizontal)
            .unwrap();
        workspace.pin_task_axis_size(local.clone(), 300.0).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());

        assert_eq!(allocated.width(local), Some(300.0));
        assert_eq!(allocated.width(remote), Some(700.0));
    }
}
