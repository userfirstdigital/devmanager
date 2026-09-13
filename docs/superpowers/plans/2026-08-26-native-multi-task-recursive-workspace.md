# Native Multi-Task Recursive Workspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users Shift-click multiple tasks into a persistent native recursive workspace with live full panes, compact monitoring cards, pinned resizing, automatic LRU compaction, and tmux-style pane movement.

**Architecture:** A pure Rust recursive split tree owns only client-local layout and focus state. `NativeShell` keeps host-owned task/runtime truth and binds its existing interactive composer, cockpit, and terminal to exactly one focused task while per-task surface caches feed background monitoring panes. GPUI renders the tree recursively and uses its native pointer, drag, and drop APIs; layout schema v4 persists the validated tree in the existing profile-scoped atomic store.

**Tech Stack:** Rust 2021, GPUI 0.2.2, serde/serde_json, existing DevManager native host/client projections, Windows native acceptance harnesses.

**Spec:** `docs/superpowers/specs/2026-08-26-native-multi-task-recursive-workspace-design.md`

## Global Constraints

- Reuse/adapt Traycer MIT tree/drop algorithms and Herdr Apache-2.0 Rust layout invariants before writing new algorithms.
- Reuse GPUI 0.2.2 `on_drag`/`on_drop` public APIs; do not copy Zed's GPL workspace implementation.
- Layout remains client-local presentation state; task, provider session, PTY, runtime generation, action epoch, repositories, branches, worktrees, and cwd remain host-owned.
- Every pane and every admitted result is keyed by exact `TaskId`; never infer provider or terminal identity from pane position.
- Exactly one focused full pane owns interactive composer, approval/question, terminal input, and context dock controls.
- Open panes may cross project boundaries and must show the actual project name without narrowing DevManager's multi-folder/repository project model.
- Preserve the user's existing uncommitted `AGENTS.md`; stage only files named by each task.
- Use the current checkout so the user's dev app can hot reload. All Rust verification uses the exact isolated target `C:\Temp\devmanager-native-multi-task-VisualDevManager` and never the repository target.
- Focused RED/GREEN tests run per task. Broad Rust/native suites run once in Task 8.
- Do not set `DEVMANAGER_PROFILE` for the full library suite.
- Before and after final verification, record production `config.json` and `remote.json` hashes plus installed DevManager PID/start time.

---

## File structure

- Create `src/ui/task_workspace/mod.rs`: public module boundary and re-exports.
- Create `src/ui/task_workspace/layout.rs`: serializable recursive tree, stable IDs, focus history, validated transactional mutations.
- Create `src/ui/task_workspace/allocation.rs`: viewport allocation, minimum sizes, pinned children, automatic compaction/re-expansion.
- Create `src/ui/task_workspace/surfaces.rs`: exact-TaskId monitoring cache and per-task in-flight admission state.
- Create `src/ui/task_workspace/view.rs`: pane view models, drop geometry, drag payloads, and content-agnostic recursive GPUI rendering.
- Modify `src/ui/mod.rs`: export `task_workspace`.
- Modify `src/ui/workspace_layout.rs`: schema v4 persistence, v3 migration, corrupt-tree fail-closed behavior.
- Modify `src/ui/native_shell.rs`: Shift-click selection, focused task binding, pane view-model projection, query scheduling, lifecycle reconciliation, and workspace event callbacks.
- Modify `src/ui/task_cockpit/dock.rs`: expose bounded read-only projections needed by unfocused full panes without adding a second interactive owner.
- Modify the existing native preview/acceptance tests inside `src/ui/native_shell.rs`: add one fresh-Application multi-pane scenario using the existing harness rather than creating another Windows application lifetime.

---

### Task 1: Pure recursive task workspace

**Files:**
- Create: `src/ui/task_workspace/mod.rs`
- Create: `src/ui/task_workspace/layout.rs`
- Modify: `src/ui/mod.rs`

**Interfaces:**
- Consumes: `crate::domain::TaskId`, `serde::{Serialize, Deserialize}`, `uuid::Uuid`.
- Produces: `PaneId`, `SplitId`, `Axis`, `Allocation`, `PanePresentation`, `TaskPane`, `WorkspaceNode`, `DropTarget`, `WorkspaceError`, and `TaskWorkspace` methods used by every later task.

- [ ] **Step 1: Write the failing layout tests**

Add tests inside `layout.rs` first, calling the intended API:

```rust
#[test]
fn inserting_tasks_preserves_unique_identity_and_focus_history() {
    let first = TaskId::new();
    let second = TaskId::new();
    let mut workspace = TaskWorkspace::single(first);
    let first_pane = workspace.focused_pane_id().unwrap();

    let second_pane = workspace.insert_after_focused(second, Axis::Horizontal).unwrap();

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
        workspace.move_pane(pane, DropTarget::Edge { pane, edge: Edge::Left }),
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
    let second_pane = workspace.insert_after_focused(second, Axis::Vertical).unwrap();

    workspace.remove_pane(second_pane).unwrap();

    assert_eq!(workspace.pane_count(), 1);
    assert_eq!(workspace.focused_pane_id(), Some(first_pane));
    assert!(matches!(workspace.root(), Some(WorkspaceNode::Pane(_))));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run in PowerShell with `CARGO_TARGET_DIR=C:\Temp\devmanager-native-multi-task-VisualDevManager`:

```powershell
cargo test --lib ui::task_workspace::layout::tests -- --test-threads=1
```

Expected: compilation fails because `task_workspace` and its types do not exist.

- [ ] **Step 3: Port the minimal validated tree model**

Adapt Herdr's `src/layout.rs` mutation/rollback/focus rules and Traycer's
`clients/gui-app/src/stores/epics/canvas/tile-tree.ts` stable identity and normalization rules into:

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PaneId(Uuid);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Axis { Horizontal, Vertical }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Allocation {
    Auto { weight: f32 },
    Pinned { logical_px: f32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanePresentation { Full, CompactManual, CompactAutomatic }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkspaceNode {
    Pane(TaskPane),
    Split { id: SplitId, axis: Axis, children: Vec<SplitChild> },
}
```

Implement candidate-clone validation for `insert_after_focused`, `remove_pane`, `swap_panes`, and
`move_pane`. Reject duplicate task IDs, duplicate node IDs, empty/single-child persisted splits,
non-finite/non-positive allocations, invalid focus, and self-drops.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run the Task 1 command. Expected: all `task_workspace::layout` tests pass.

- [ ] **Step 5: Commit Task 1**

```powershell
git add -- src/ui/mod.rs src/ui/task_workspace/mod.rs src/ui/task_workspace/layout.rs
git commit -m "feat(native): add recursive task workspace model"
```

---

### Task 2: Pinned allocation and LRU compaction

**Files:**
- Create: `src/ui/task_workspace/allocation.rs`
- Modify: `src/ui/task_workspace/mod.rs`
- Modify: `src/ui/task_workspace/layout.rs`

**Interfaces:**
- Consumes: `TaskWorkspace`, `WorkspaceNode`, `Allocation`, `PanePresentation`, `Axis` from Task 1.
- Produces: `Viewport`, `PaneRect`, `AllocationMetrics`, `AllocatedWorkspace`, `TaskWorkspace::allocate`, `TaskWorkspace::resize_adjacent`, and `TaskWorkspace::reset_adjacent`.

- [ ] **Step 1: Write the failing allocation tests**

```rust
#[test]
fn pinned_children_keep_requested_pixels_while_auto_children_share_the_remainder() {
    let mut workspace = three_horizontal_tasks();
    workspace.pin_first_child(300.0).unwrap();

    let allocated = workspace.allocate(Viewport::new(1_000.0, 700.0), metrics());

    assert_eq!(allocated.width(first_task()), 300.0);
    assert_eq!(allocated.width(second_task()), 350.0);
    assert_eq!(allocated.width(third_task()), 350.0);
}

#[test]
fn pressure_compacts_the_least_recent_unpinned_pane_and_room_reexpands_it() {
    let mut workspace = three_horizontal_tasks();
    workspace.focus_task(third_task()).unwrap();
    workspace.focus_task(second_task()).unwrap();

    workspace.allocate(Viewport::new(650.0, 700.0), metrics());
    assert_eq!(workspace.presentation(first_task()), Some(PanePresentation::CompactAutomatic));

    workspace.allocate(Viewport::new(1_400.0, 700.0), metrics());
    assert_eq!(workspace.presentation(first_task()), Some(PanePresentation::Full));
}

#[test]
fn manual_compaction_never_auto_expands() {
    let mut workspace = three_horizontal_tasks();
    workspace.set_manual_compact(first_task(), true).unwrap();
    workspace.allocate(Viewport::new(1_800.0, 900.0), metrics());
    assert_eq!(workspace.presentation(first_task()), Some(PanePresentation::CompactManual));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

```powershell
cargo test --lib ui::task_workspace::allocation::tests -- --test-threads=1
```

Expected: failure because allocation APIs are absent.

- [ ] **Step 3: Implement allocation by adapting proven algorithms**

Use Traycer's `sizesForGroup`/minimum-pane logic and Herdr's 0.1–0.9 resize clamping as behavioral
references. Implement recursive axis allocation: subtract Pinned logical pixels, divide remaining
pixels by normalized Auto weights, then compact eligible Full panes by ascending
`last_focused_at` until minimums fit. Track requested pinned size separately from rendered clamps.
`resize_adjacent(split, divider, delta_px)` converts the two adjacent allocations to Pinned;
`reset_adjacent` converts them to equal Auto weights.

- [ ] **Step 4: Run Task 1 and Task 2 focused tests and verify GREEN**

```powershell
cargo test --lib ui::task_workspace -- --test-threads=1
```

Expected: all task-workspace pure tests pass.

- [ ] **Step 5: Commit Task 2**

```powershell
git add -- src/ui/task_workspace/mod.rs src/ui/task_workspace/layout.rs src/ui/task_workspace/allocation.rs
git commit -m "feat(native): allocate pinned and compact task panes"
```

---

### Task 3: Workspace layout schema v4

**Files:**
- Modify: `src/ui/workspace_layout.rs`

**Interfaces:**
- Consumes: serializable `TaskWorkspace` from Task 1.
- Produces: `WorkspaceLayout.task_workspace: Option<TaskWorkspace>`, schema v4 load/save, v3 migration, and `reconcile_task_workspace(&[TaskId])`.

- [ ] **Step 1: Add failing persistence and migration tests**

```rust
#[test]
fn v3_selected_task_migrates_to_a_single_pane_v4_workspace() {
    let selected = TaskId::new();
    let store = store_with_json(v3_json_with_selected(selected));
    let layout = store.load();
    let workspace = layout.task_workspace.expect("migrated workspace");
    assert_eq!(workspace.pane_count(), 1);
    assert_eq!(workspace.focused_task(), Some(selected));
}

#[test]
fn corrupt_v4_tree_fails_closed_without_losing_valid_shell_geometry() {
    let store = store_with_json(v4_json_with_duplicate_task_ids());
    let layout = store.load();
    assert!(layout.task_workspace.is_none());
    assert_eq!(layout.sidebar_width, 280.0);
}
```

- [ ] **Step 2: Run the focused persistence tests and verify RED**

```powershell
cargo test --lib ui::workspace_layout::tests -- --test-threads=1
```

Expected: failure because schema v4 and `task_workspace` do not exist.

- [ ] **Step 3: Implement v4 and backward migration**

Rename the current schema constant to `LAYOUT_SCHEMA_V3`, set
`LAYOUT_SCHEMA = "devmanager.workspace-layout/v4"`, and add:

```rust
#[serde(default)]
pub task_workspace: Option<TaskWorkspace>,
```

On v3/v2/v1 load, create `TaskWorkspace::single(selected_task)` when selected_task exists. On v4
load, keep shell geometry but discard only an invalid task tree. Reconciliation prunes TaskIds not
present in the canonical `ClientModel`, normalizes the tree, and synchronizes `selected_task` to
the focused pane for backward compatibility.

- [ ] **Step 4: Run the focused persistence tests and verify GREEN**

Run the Task 3 command. Expected: workspace-layout tests pass.

- [ ] **Step 5: Commit Task 3**

```powershell
git add -- src/ui/workspace_layout.rs
git commit -m "feat(native): persist multi-task workspace layout"
```

---

### Task 4: Exact-task surface registry and workspace interaction

**Files:**
- Create: `src/ui/task_workspace/surfaces.rs`
- Modify: `src/ui/task_workspace/mod.rs`
- Modify: `src/ui/native_shell.rs`

**Interfaces:**
- Consumes: `TaskWorkspace`, `TaskId`, current `TaskConversationCache`, `TaskCockpitResult`, request generation, and task-list lifecycle projections.
- Produces: `TaskSurfaceRegistry`, `TaskSurfaceState`, `WorkspaceSelectionGesture`, `NativeShell::apply_workspace_selection`, and exact-task result admission.

- [ ] **Step 1: Write failing surface and gesture tests**

```rust
#[test]
fn shift_click_toggles_membership_while_plain_click_only_focuses_an_open_task() {
    let mut shell = connected_shell_with_tasks(3);
    let [first, second, third] = three_task_ids(&shell);
    shell.apply_workspace_selection(first, WorkspaceSelectionGesture::Plain).unwrap();
    shell.apply_workspace_selection(second, WorkspaceSelectionGesture::Toggle).unwrap();
    shell.apply_workspace_selection(third, WorkspaceSelectionGesture::Toggle).unwrap();
    shell.apply_workspace_selection(first, WorkspaceSelectionGesture::Plain).unwrap();
    assert_eq!(shell.workspace_task_ids().len(), 3);
    assert_eq!(shell.focused_workspace_task(), Some(first));
}

#[test]
fn late_conversation_result_is_admitted_only_to_its_exact_task_surface() {
    let mut registry = TaskSurfaceRegistry::default();
    registry.begin_conversation(first_task(), 7);
    registry.begin_conversation(second_task(), 9);
    assert!(registry.admit_conversation(first_task(), 7, page("first")).is_ok());
    assert_eq!(registry.latest_snippet(first_task()), Some("first"));
    assert_eq!(registry.latest_snippet(second_task()), None);
    assert_eq!(
        registry.admit_conversation(first_task(), 6, page("stale")),
        Err(SurfaceAdmissionError::StaleGeneration)
    );
}
```

- [ ] **Step 2: Run focused tests and verify RED**

```powershell
cargo test --lib task_workspace_surface -- --test-threads=1
```

Expected: failure because registry and gesture APIs do not exist.

- [ ] **Step 3: Implement the per-task registry and replace single in-flight truth**

Move conversation cache/in-flight ownership behind:

```rust
pub struct TaskSurfaceState {
    pub conversation: TaskConversationCache,
    pub conversation_generation: u64,
    pub conversation_in_flight: bool,
    pub latest_snippet: Option<String>,
    pub latest_terminal: Option<TaskTerminalProjection>,
}

#[derive(Default)]
pub struct TaskSurfaceRegistry {
    surfaces: BTreeMap<TaskId, TaskSurfaceState>,
}
```

Keep the existing `CockpitDock` and `TerminalDockAdapter` single-owner. On pane focus, call the
existing fenced `sync_selected_task`, then bind only that task's cached projection to the
interactive objects. Update query dispatch/result admission to clear only the matching task's
in-flight flag.

- [ ] **Step 4: Wire Shift-click without changing right-click behavior**

In the existing task-row `MouseDownEvent`, map `event.modifiers.shift` to Toggle and plain left
click to Plain. Keep right-click rename/delete unchanged. Settled/archived task restore remains a
host command; do not infer restored state locally.

- [ ] **Step 5: Run the focused native interaction tests and verify GREEN**

Run the Task 4 command plus:

```powershell
cargo test --lib shift_click -- --test-threads=1
```

Expected: all new gesture and admission tests pass.

- [ ] **Step 6: Commit Task 4**

```powershell
git add -- src/ui/task_workspace/mod.rs src/ui/task_workspace/surfaces.rs src/ui/native_shell.rs
git commit -m "feat(native): bind task surfaces to recursive workspace"
```

---

### Task 5: Full and compact native task panes

**Files:**
- Create: `src/ui/task_workspace/view.rs`
- Modify: `src/ui/task_workspace/mod.rs`
- Modify: `src/ui/native_shell.rs`
- Modify: `src/ui/task_cockpit/dock.rs`

**Interfaces:**
- Consumes: allocated pane rectangles, exact task surface states, current theme tokens, project/task labels, conversation rows, and focused task controls.
- Produces: `TaskPaneViewModel`, `TaskWorkspaceViewModel`, `TaskWorkspaceEvent`, and recursive `render_workspace`.

- [ ] **Step 1: Write failing view-model tests**

```rust
#[test]
fn compact_view_model_contains_status_and_snippet_but_no_heavy_surface() {
    let model = TaskPaneViewModel::from_surface(compact_pane(), projected_task(), surface());
    assert_eq!(model.project_name, "DevManager");
    assert_eq!(model.status_label, "Working");
    assert_eq!(model.latest_snippet.as_deref(), Some("Editing layout.rs"));
    assert_eq!(model.body, TaskPaneBody::Compact);
    assert!(!model.build_composer);
    assert!(!model.paint_terminal);
}

#[test]
fn only_the_focused_full_pane_builds_interactive_controls() {
    let models = workspace_view_models(four_tasks(), second_task());
    assert_eq!(models.iter().filter(|pane| pane.build_composer).count(), 1);
    assert_eq!(models.iter().find(|pane| pane.build_composer).unwrap().task_id, second_task());
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

```powershell
cargo test --lib task_pane_view_model -- --test-threads=1
```

Expected: failure because view models do not exist.

- [ ] **Step 3: Implement content-agnostic recursive rendering**

Adapt Traycer's `split-container.tsx` recursive structure into GPUI `div().flex()` nodes. Render
split nodes only from tree/allocation state and leaf nodes from `TaskPaneViewModel`. Full focused
panes reuse the existing conversation/terminal/composer renderers. Full unfocused panes render the
bounded cached conversation tail or read-only terminal snapshot with no input handlers. Compact
panes render task name, actual project name, status, latest snippet, Expand, and Close controls.

- [ ] **Step 4: Mount the workspace in place of the single center canvas**

When the tree is empty, preserve the idle random-image surface. With one pane, preserve current
geometry and behavior. With multiple panes, render the recursive workspace while the existing
right context dock follows only the focused task.

- [ ] **Step 5: Run focused view-model and native canvas tests and verify GREEN**

```powershell
cargo test --lib task_pane_view_model -- --test-threads=1
cargo test --lib main_conversation_canvas -- --test-threads=1
```

Expected: view-model tests pass and existing one-task canvas tests remain green.

- [ ] **Step 6: Commit Task 5**

```powershell
git add -- src/ui/task_workspace/mod.rs src/ui/task_workspace/view.rs src/ui/native_shell.rs src/ui/task_cockpit/dock.rs
git commit -m "feat(native): render live full and compact task panes"
```

---

### Task 6: Divider resizing and pane drag/drop

**Files:**
- Modify: `src/ui/task_workspace/view.rs`
- Modify: `src/ui/task_workspace/layout.rs`
- Modify: `src/ui/task_workspace/allocation.rs`
- Modify: `src/ui/native_shell.rs`

**Interfaces:**
- Consumes: GPUI `MouseDownEvent`, `MouseMoveEvent`, `MouseUpEvent`, `on_drag`, `on_drop`, allocated pane bounds, split IDs, and layout mutations.
- Produces: `DraggedTaskPane`, `DropZone`, `resolve_drop_zone`, transient divider drag state, and one-commit layout persistence callbacks.

- [ ] **Step 1: Write failing drop-geometry and resize-commit tests**

```rust
#[test]
fn pane_center_is_swap_and_edges_are_directional_splits() {
    let bounds = TestRect::new(0.0, 0.0, 400.0, 300.0);
    assert_eq!(resolve_drop_zone(bounds, point(200.0, 150.0)), DropZone::Center);
    assert_eq!(resolve_drop_zone(bounds, point(5.0, 150.0)), DropZone::Left);
    assert_eq!(resolve_drop_zone(bounds, point(395.0, 150.0)), DropZone::Right);
}

#[test]
fn divider_drag_is_transient_until_mouse_release() {
    let mut harness = resize_harness();
    harness.mouse_down(divider(), 300.0);
    harness.mouse_move(360.0);
    assert_eq!(harness.persistence_writes(), 0);
    harness.mouse_up(360.0);
    assert_eq!(harness.persistence_writes(), 1);
    assert!(harness.left_allocation().is_pinned());
    assert!(harness.right_allocation().is_pinned());
}
```

- [ ] **Step 2: Run focused tests and verify RED**

```powershell
cargo test --lib workspace_drop -- --test-threads=1
cargo test --lib workspace_resize -- --test-threads=1
```

Expected: failure because drop zones and transient resize state are absent.

- [ ] **Step 3: Implement GPUI drag/drop using the installed Apache GPUI example**

Follow `gpui-0.2.2/src/interactive.rs` and the Apache GPUI `examples/drag_drop.rs` API shape:

```rust
.on_drag(DraggedTaskPane { pane_id }, |payload, position, _, cx| {
    cx.new(|_| payload.at(position))
})
.on_drop(cx.listener(|shell, payload: &DraggedTaskPane, window, cx| {
    shell.commit_workspace_drop(*payload, window, cx);
}))
```

Port Traycer's edge-band geometry from `pane-drop-geometry.ts`; use center=swap and edge=move/split.
Reject self-drop before cloning the candidate tree.

- [ ] **Step 4: Implement divider drag and reset**

Use GPUI element state for live delta, update only the two adjacent rendered allocations on move,
commit `resize_adjacent` on release, and save layout once. Double-click calls `reset_adjacent` and
saves once.

- [ ] **Step 5: Run focused drop/resize tests and verify GREEN**

Run both Task 6 commands. Expected: all drop and resize tests pass.

- [ ] **Step 6: Commit Task 6**

```powershell
git add -- src/ui/task_workspace/view.rs src/ui/task_workspace/layout.rs src/ui/task_workspace/allocation.rs src/ui/native_shell.rs
git commit -m "feat(native): resize and move recursive task panes"
```

---

### Task 7: Lifecycle reconciliation and bounded background monitoring

**Files:**
- Modify: `src/ui/task_workspace/surfaces.rs`
- Modify: `src/ui/native_shell.rs`
- Test: `src/ui/native_shell.rs` (`#[cfg(test)]` native GPUI scenarios)

**Interfaces:**
- Consumes: canonical `ClientModel`, settled/archived task projections, action results, pane presentation/focus, and task cockpit queries.
- Produces: lifecycle reconciliation, bounded visible-pane scheduler, and one end-to-end GPUI Application scenario.

- [ ] **Step 1: Confirm the existing single-Application acceptance seam**

Run:

```powershell
rg -n "Application::new|headless|preview|first.paint|capture" src/ui tests scripts/native-next
```

Select the existing harness that already installs an in-memory idle image and runs multiple native
gestures in one Application lifetime inside `src/ui/native_shell.rs`. Extend that test module and
do not create a second process-global GPUI test application.

- [ ] **Step 2: Write failing lifecycle and scheduling tests**

```rust
#[test]
fn settle_keeps_the_pane_done_while_archive_removes_it() {
    let mut shell = shell_with_open_tasks(2);
    shell.apply_settled_snapshot(first_task());
    assert_eq!(shell.pane_status(first_task()), Some("Done"));
    assert!(shell.workspace_contains(first_task()));
    shell.apply_archived_snapshot(first_task());
    assert!(!shell.workspace_contains(first_task()));
}

#[test]
fn query_scheduler_prioritizes_focused_full_then_background_full_and_skips_compact() {
    let schedule = schedule_for_mixed_workspace();
    assert_eq!(schedule[0].task_id, focused_task());
    assert_eq!(schedule[0].priority, QueryPriority::Interactive);
    assert!(schedule.iter().all(|item| item.task_id != compact_task()));
}
```

- [ ] **Step 3: Run focused tests and verify RED**

```powershell
cargo test --lib workspace_lifecycle -- --test-threads=1
cargo test --lib workspace_query_scheduler -- --test-threads=1
```

Expected: failure because reconciliation and the scheduler are absent.

- [ ] **Step 4: Implement lifecycle and bounded scheduling**

Reconcile open pane TaskIds after each canonical snapshot. Settled remains with Done status;
archived/deleted/missing is removed and the tree normalized. Schedule at most one interactive
focused query plus two background full-pane tail queries; compact panes use task-list status and
cached snippets only. Cancellation or failure clears only that task's in-flight state.

- [ ] **Step 5: Add the single-Application native interaction scenario**

In the selected existing harness, drive: plain select, two Shift-click additions, focus change,
manual compact/expand, divider drag/reset, center swap, edge move, settle, archive removal, save,
reload, and exact focused-task composer ownership. Install the idle image in memory before the
shell can render it and end all spawned work before Application teardown.

- [ ] **Step 6: Run focused lifecycle/scheduler/headless tests and verify GREEN**

Run the two Task 7 commands plus
`cargo test --lib native_multi_task_workspace_headless -- --test-threads=1`. Expected: all focused
tests pass with no detached GPUI/network work.

- [ ] **Step 7: Commit Task 7**

```powershell
git add -- src/ui/task_workspace/surfaces.rs src/ui/native_shell.rs
git commit -m "feat(native): reconcile and monitor tiled tasks"
```

---

### Task 8: Visible acceptance and final gates

**Files:**
- Modify only files required by failures that receive a new failing regression test first.

**Interfaces:**
- Consumes: all Tasks 1–7.
- Produces: verified native feature, clean scoped diff, and final commit(s).

- [ ] **Step 1: Capture pre-verification isolation evidence**

Record SHA-256 for production `config.json` and `remote.json`, plus installed DevManager PID,
start time, and executable path. Record the isolated target path and verify it is exactly beneath
`C:\Temp\devmanager-native-multi-task-VisualDevManager` before Cargo starts.

- [ ] **Step 2: Run format and complete compiler reconciliation once**

```powershell
cargo fmt --all -- --check
cargo check --locked --lib --bins --tests
```

Use the isolated `CARGO_TARGET_DIR`. Expected: both commands exit 0.

- [ ] **Step 3: Run the complete Rust library suite once**

Tell the user before starting the long run, then execute:

```powershell
cargo test --lib -- --test-threads=1
```

Give the first cold build at least 600 seconds. If the wrapper times out, join the exact Cargo
process tree; do not start a duplicate build. Expected: zero failed tests.

- [ ] **Step 4: Run native feature acceptance**

Run the exact fresh-process headless scenario from Task 7 and the repository's existing native
preview/smoke command. In the user's hot-reload dev app, verify Shift-click, focus, live background
updates, Full/Compact, automatic LRU compaction, automatic re-expansion, pinned resize, divider
reset, center swap, edge move, Done/reactivate, archive/delete removal, cross-project labels, and
restart persistence at laptop and large-monitor dimensions.

- [ ] **Step 5: Measure one versus fifteen panes**

Use existing native counters/timing seams to compare one Full pane with fifteen mixed Full/Compact
panes. Confirm compact panes do not construct composers, paint terminals, or request conversation
pagination, and focusing a pane remains responsive while another provider query is in flight.

- [ ] **Step 6: Verify post-run isolation and process cleanup**

Recompute production config hashes and installed PID/start time. Confirm hashes and installed
process identity are unchanged. Confirm no descendant test harness, Cargo, rustc, linker, or
Cursor wrapper process remains for this checkout/target.

- [ ] **Step 7: Review the complete diff and commit corrections**

Run `git diff --check`, inspect every changed file, verify only intended files are staged, and keep
the user's `AGENTS.md` unstaged. Any correction begins with a failing focused regression test.
Commit bounded corrections with a message naming the corrected behavior.

- [ ] **Step 8: Sharpening the Axe closeout**

Search project and global guidance for overlap. Record at most one consolidated durable update
only if evidence shows a severe, repeated, or broadly reusable improvement; otherwise report that
no persistent update was warranted. Name the authority updated in the final response.
