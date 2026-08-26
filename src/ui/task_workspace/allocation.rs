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
        self.restore_automatic_panes_that_fit(viewport, metrics);
        self.compact_until_fit(viewport, metrics);

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

    fn restore_automatic_panes_that_fit(&mut self, viewport: Viewport, metrics: AllocationMetrics) {
        let mut candidates: Vec<_> = self
            .task_ids()
            .into_iter()
            .filter_map(|task_id| {
                let pane = self.pane_for_task(task_id)?;
                (pane.presentation == PanePresentation::CompactAutomatic)
                    .then_some((pane.last_focused_at, task_id))
            })
            .collect();
        candidates.sort_by_key(|(last_focused_at, _)| std::cmp::Reverse(*last_focused_at));
        for (_, task_id) in candidates {
            let _ = self.set_presentation(task_id, PanePresentation::Full);
            if !self.minimum_fits(viewport, metrics) {
                let _ = self.set_presentation(task_id, PanePresentation::CompactAutomatic);
            }
        }
    }

    fn compact_until_fit(&mut self, viewport: Viewport, metrics: AllocationMetrics) {
        while !self.minimum_fits(viewport, metrics) {
            let focused = self.focused_task();
            let candidate = self
                .task_ids()
                .into_iter()
                .filter(|task_id| Some(*task_id) != focused && self.task_is_unpinned(*task_id))
                .filter_map(|task_id| {
                    let pane = self.pane_for_task(task_id)?;
                    (pane.presentation == PanePresentation::Full)
                        .then_some((pane.last_focused_at, task_id))
                })
                .min_by_key(|(last_focused_at, _)| *last_focused_at)
                .map(|(_, task_id)| task_id);
            let Some(task_id) = candidate else {
                break;
            };
            let _ = self.set_presentation(task_id, PanePresentation::CompactAutomatic);
        }
    }

    fn minimum_fits(&self, viewport: Viewport, metrics: AllocationMetrics) -> bool {
        let Some(root) = self.root() else {
            return true;
        };
        let minimum = minimum_size(root, metrics);
        minimum.width <= viewport.width && minimum.height <= viewport.height
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
                    let minimum = minimum_size(&child.node, metrics);
                    match axis {
                        Axis::Horizontal => minimum.width,
                        Axis::Vertical => minimum.height,
                    }
                })
                .collect();
            let sizes = allocate_axis_sizes(children, &minimums, available);
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
) -> Vec<f32> {
    if children.is_empty() {
        return Vec::new();
    }
    let bases: Vec<f32> = children
        .iter()
        .zip(minimums)
        .map(|(child, minimum)| match child.allocation {
            Allocation::Auto { .. } => *minimum,
            Allocation::Pinned { logical_px } => logical_px.max(*minimum),
        })
        .collect();
    let base_total: f32 = bases.iter().sum();
    if base_total > available && base_total > 0.0 {
        let scale = available / base_total;
        return bases.into_iter().map(|base| base * scale).collect();
    }

    let extra = (available - base_total).max(0.0);
    let auto_weight: f32 = children
        .iter()
        .map(|child| match child.allocation {
            Allocation::Auto { weight } => weight,
            Allocation::Pinned { .. } => 0.0,
        })
        .sum();
    let fallback_weight = if auto_weight > 0.0 {
        auto_weight
    } else {
        children.len() as f32
    };
    children
        .iter()
        .zip(bases)
        .map(|(child, base)| {
            let weight = if auto_weight > 0.0 {
                match child.allocation {
                    Allocation::Auto { weight } => weight,
                    Allocation::Pinned { .. } => 0.0,
                }
            } else {
                1.0
            };
            base + extra * weight / fallback_weight
        })
        .collect()
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
    fn pressure_compacts_the_least_recent_unpinned_pane_and_room_reexpands_it() {
        let (mut workspace, [first, second, third]) = three_horizontal_tasks();
        workspace.focus_task(second).unwrap();

        workspace.allocate(Viewport::new(550.0, 700.0), metrics());
        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::CompactAutomatic)
        );
        assert_eq!(workspace.presentation(third), Some(PanePresentation::Full));

        workspace.allocate(Viewport::new(1_400.0, 700.0), metrics());
        assert_eq!(workspace.presentation(first), Some(PanePresentation::Full));
    }

    #[test]
    fn manual_compaction_never_auto_expands() {
        let (mut workspace, [first, _, _]) = three_horizontal_tasks();
        workspace.set_manual_compact(first, true).unwrap();

        workspace.allocate(Viewport::new(1_800.0, 900.0), metrics());

        assert_eq!(
            workspace.presentation(first),
            Some(PanePresentation::CompactManual)
        );
    }
}
