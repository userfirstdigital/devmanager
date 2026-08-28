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
        // CompactAutomatic was a focus/width hide; restore to Full so only
        // explicit CompactManual remains condensed. Geometry shrinks panes
        // instead of swapping full content for snippets.
        self.restore_automatic_to_full();

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

    fn restore_automatic_to_full(&mut self) {
        for task_id in self.task_ids() {
            if self.presentation(task_id.clone()) == Some(PanePresentation::CompactAutomatic) {
                let _ = self.set_presentation(task_id, PanePresentation::Full);
            }
        }
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
            PanePresentation::CompactManual | PanePresentation::CompactAutomatic => MinimumSize {
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

    fn production_like_metrics() -> AllocationMetrics {
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
    fn narrow_pressure_keeps_full_presentation_and_fills_parent() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();

        let allocated = workspace.allocate(Viewport::new(550.0, 700.0), metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(third), Some(PanePresentation::Full));
        assert_eq!(
            allocated.width(first).unwrap()
                + allocated.width(second).unwrap()
                + allocated.width(third).unwrap(),
            550.0
        );
    }

    #[test]
    fn automatic_compact_migrates_to_full_on_allocate() {
        let (mut workspace, [first, second, _third]) = three_horizontal_tasks();
        workspace
            .set_presentation(first, PanePresentation::CompactAutomatic)
            .unwrap();
        workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(workspace.presentation(second), Some(PanePresentation::Full));
    }

    #[test]
    fn manual_compaction_never_auto_expands() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.set_manual_compact(first, true).unwrap();

        let allocated = workspace.allocate(Viewport::new(1_000.0, 900.0), metrics());

        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::CompactManual)
        );
        assert_eq!(allocated.width(first), Some(80.0));
        assert_eq!(allocated.width(second), Some(460.0));
        assert_eq!(allocated.width(third), Some(460.0));
    }

    #[test]
    fn compact_only_vertical_split_fills_parent_without_unowned_gap() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Vertical)
            .unwrap();
        workspace.set_manual_compact(first, true).unwrap();
        workspace.set_manual_compact(second, true).unwrap();

        let allocated = workspace.allocate(Viewport::new(700.0, 700.0), production_like_metrics());

        assert_eq!(allocated.height(first), Some(348.0));
        assert_eq!(allocated.height(second), Some(348.0));
        assert_eq!(
            allocated.height(first).unwrap() + 4.0 + allocated.height(second).unwrap(),
            700.0
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
        workspace.set_manual_compact(third, true).unwrap();

        let allocated =
            workspace.allocate(Viewport::new(1_000.0, 700.0), production_like_metrics());
        let first_rect = allocated.rect(first).expect("first");
        let second_rect = allocated.rect(second).expect("second");
        let third_rect = allocated.rect(third).expect("third");

        assert_eq!(first_rect.width + 4.0 + second_rect.width, 1_000.0);
        assert_eq!(first_rect.height, 700.0);
        assert_eq!(second_rect.height + 4.0 + third_rect.height, 700.0);
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
    fn full_content_survives_narrow_pressure_until_manual_compact() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(third).unwrap();
        workspace.set_manual_compact(second, true).unwrap();

        let allocated = workspace.allocate(Viewport::new(900.0, 700.0), production_like_metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
        assert_eq!(
            workspace.presentation(second),
            Some(PanePresentation::CompactManual)
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

        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), production_like_metrics());
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
    fn mixed_full_and_compact_auto_peers_conserve_extent_under_pin_pressure() {
        let (mut workspace, [pinned, full_auto, compact_auto]) = three_horizontal_tasks();
        workspace.set_manual_compact(compact_auto, true).unwrap();
        workspace.pin_task_axis_size(pinned, 500.0).unwrap();
        let metrics = production_like_metrics();

        let at_500 = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(at_500.width(pinned), Some(500.0));
        assert_eq!(at_500.width(full_auto), Some(228.0));
        assert_eq!(at_500.width(compact_auto), Some(64.0));
        assert_eq!(
            at_500.width(pinned).unwrap()
                + metrics.divider
                + at_500.width(full_auto).unwrap()
                + metrics.divider
                + at_500.width(compact_auto).unwrap(),
            800.0
        );

        workspace.pin_task_axis_size(pinned, 700.0).unwrap();
        let at_700 = workspace.allocate(Viewport::new(800.0, 700.0), metrics);
        assert_eq!(at_700.width(pinned), Some(664.0));
        assert_eq!(at_700.width(full_auto), Some(64.0));
        assert_eq!(at_700.width(compact_auto), Some(64.0));
        assert_eq!(
            at_700.width(pinned).unwrap()
                + metrics.divider
                + at_700.width(full_auto).unwrap()
                + metrics.divider
                + at_700.width(compact_auto).unwrap(),
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
        let allocated =
            workspace.allocate(Viewport::new(1_000.0, 700.0), production_like_metrics());
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
        let metrics = production_like_metrics();
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

        let metrics = production_like_metrics();
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

        let metrics = production_like_metrics();
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
        let allocated = workspace.allocate(Viewport::new(800.0, 700.0), production_like_metrics());
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
        let metrics = production_like_metrics();
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
        let metrics = production_like_metrics();
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
