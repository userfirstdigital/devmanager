use std::collections::BTreeMap;

use crate::domain::TaskId;

use super::layout::{Allocation, Axis, PanePresentation, TaskWorkspace, WorkspaceNode};

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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AllocatedWorkspace {
    panes: BTreeMap<TaskId, PaneRect>,
}

impl AllocatedWorkspace {
    pub fn rect(&self, task_id: TaskId) -> Option<PaneRect> {
        self.panes.get(&task_id).copied()
    }

    pub fn width(&self, task_id: TaskId) -> Option<f32> {
        self.rect(task_id).map(|rect| rect.width)
    }

    pub fn height(&self, task_id: TaskId) -> Option<f32> {
        self.rect(task_id).map(|rect| rect.height)
    }
}

impl TaskWorkspace {
    pub fn allocate(
        &mut self,
        viewport: Viewport,
        metrics: AllocationMetrics,
    ) -> AllocatedWorkspace {
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
            if self.presentation(task_id) == Some(PanePresentation::CompactAutomatic) {
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

fn minimum_size(node: &WorkspaceNode, metrics: AllocationMetrics) -> MinimumSize {
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

fn allocate_node(
    node: &WorkspaceNode,
    rect: PaneRect,
    metrics: AllocationMetrics,
    panes: &mut BTreeMap<TaskId, PaneRect>,
) {
    match node {
        WorkspaceNode::Pane(pane) => {
            panes.insert(pane.task_id, rect);
        }
        WorkspaceNode::Split { axis, children, .. } => {
            let divider_total = metrics.divider * children.len().saturating_sub(1) as f32;
            let available = match axis {
                Axis::Horizontal => rect.width,
                Axis::Vertical => rect.height,
            };
            let available = (available - divider_total).max(0.0);
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
            let sizes = allocate_axis_sizes(children, &minimums, available, fallback_resize_index);
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
                    cursor += metrics.divider;
                }
            }
        }
    }
}

fn allocate_axis_sizes(
    children: &[super::layout::SplitChild],
    minimums: &[f32],
    available: f32,
    fallback_resize_index: Option<usize>,
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
    if remaining_for_auto + f32::EPSILON >= auto_min_total && auto_weight > 0.0 {
        // Keep pinned at requested sizes; Auto siblings absorb surplus (and
        // any prior oversize shrink by starting from mins then taking remainder).
        let auto_extra = (remaining_for_auto - auto_min_total).max(0.0);
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
            if weight > 0.0 {
                sizes[index] = minimums[index] + auto_extra * weight / auto_weight;
            }
        }
        return sizes;
    }

    // Deficit: Auto already at mins. Shrink pinned via LRF before proportional
    // compression, preserving other pinned when an Auto path cannot absorb.
    for (index, child) in children.iter().enumerate() {
        if matches!(child.allocation, Allocation::Auto { .. }) {
            sizes[index] = minimums[index];
        }
    }
    let mut deficit = sizes.iter().sum::<f32>() - available;
    if deficit > 0.0 {
        if let Some(index) = fallback_resize_index.filter(|index| *index < sizes.len()) {
            let reducible = (sizes[index] - minimums[index]).max(0.0);
            let take = deficit.min(reducible);
            sizes[index] -= take;
            deficit -= take;
        }
        if deficit > 0.0 {
            // Remaining deficit: shrink other children toward mins, LRF order.
            let mut order: Vec<_> = (0..children.len()).collect();
            order.sort_by_key(|index| most_recent_focus(&children[*index].node).unwrap_or(0));
            for index in order {
                if deficit <= 0.0 {
                    break;
                }
                let reducible = (sizes[index] - minimums[index]).max(0.0);
                let take = deficit.min(reducible);
                sizes[index] -= take;
                deficit -= take;
            }
        }
        if deficit > 0.0 {
            // Physical mins still exceed available: proportional compress.
            let total: f32 = sizes.iter().sum();
            if total > 0.0 {
                let scale = available / total;
                for size in &mut sizes {
                    *size *= scale;
                }
            }
        }
    } else if deficit < 0.0 {
        // Floating error / all-pinned surplus with no Auto weight.
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
fn least_recent_focus_child_index(children: &[super::layout::SplitChild]) -> Option<usize> {
    children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| {
            most_recent_focus(&child.node).map(|last_focused_at| (index, last_focused_at))
        })
        .min_by_key(|(_, last_focused_at)| *last_focused_at)
        .map(|(index, _)| index)
}

fn most_recent_focus(node: &WorkspaceNode) -> Option<u64> {
    match node {
        WorkspaceNode::Pane(pane) => Some(pane.last_focused_at),
        WorkspaceNode::Split { children, .. } => children
            .iter()
            .filter_map(|child| most_recent_focus(&child.node))
            .max(),
    }
}

fn contains_full_pane(node: &WorkspaceNode) -> bool {
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
        assert_eq!(smaller.width(first), Some(220.0));
        assert_eq!(smaller.width(second), Some(280.0));
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
}
