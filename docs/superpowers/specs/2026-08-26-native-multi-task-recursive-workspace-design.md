# Native Multi-Task Recursive Workspace

## Context

DevManager currently renders one selected task in one native conversation/terminal canvas. The
user needs to Shift-click several tasks and monitor them simultaneously in a tmux-like workspace
whose panes can be moved, resized, compacted, and restored. This must extend DevManager's
multi-folder project model rather than reducing a project to one repository or one working
directory.

The approved interaction model is:

- full live task panes by default;
- one focused pane owns the composer and richer task controls;
- manually resized panes retain their size while untouched panes share remaining space;
- panes can be manually reduced to compact monitoring cards;
- if full panes no longer fit, the least-recently-focused unpinned pane compacts automatically;
- settled tasks stay visible with a Done state and reactivate when messaged;
- archive and delete remain separate operations and remove affected panes;
- the workspace arrangement persists locally.

The approved architecture is a recursive split workspace, not a fixed grid or an overlapping
free-form window canvas.

## Reuse survey

The implementation must adapt proven code before adding new machinery. All application roots
under `C:\Code\userfirst` were searched for pane, split, tile, layout, resize, drag/drop, and
workspace implementations.

| Source | License | Reuse |
|---|---|---|
| `traycer-main` | MIT | Adapt the N-ary recursive tile tree, stable node identity, edge/center drop geometry, single-commit resize, persistence shape, and tree invariant tests. |
| `herdr-master` | Apache-2.0 | Port the Rust BSP mutation primitives, focus history, split/close/swap/resize rules, clamped ratios, rollback behavior, and layout tests. |
| `zed-main/crates/gpui` | Apache-2.0 | Reuse GPUI's public drag/drop event patterns from the example and the GPUI APIs DevManager already depends on. |
| `zed-main/crates/workspace` | GPL-3.0-or-later | Study behavior only. Do not copy this implementation into Apache-2.0 DevManager. |
| `t3code-main` | MIT | Preserve the already-adopted task-list semantics and visual hierarchy; it has no equivalent multi-task tiling engine to port. |
| `connect` and `template` | project-local | No stronger reusable tiling implementation was found. |

The resulting Rust types will be DevManager-shaped adaptations, not framework translations of
Traycer's React components. Copyright notices required by copied substantial MIT or Apache source
will be retained.

## Architecture

### 1. Pure recursive layout model

Add `src/ui/task_workspace/layout.rs`, with no GPUI or host imports. It owns the presentation-only
tree:

```rust
TaskWorkspace {
    root: Option<WorkspaceNode>,
    focused: Option<PaneId>,
    previous_focus: Option<PaneId>,
    focus_clock: u64,
}

WorkspaceNode::Pane(TaskPane)
WorkspaceNode::Split {
    id: SplitId,
    axis: Axis,
    children: Vec<SplitChild>,
}

SplitChild {
    node: WorkspaceNode,
    allocation: Allocation,
}

Allocation::Auto { weight: f32 }
Allocation::Pinned { logical_px: f32 }
```

`TaskPane` has a stable `PaneId`, exact `TaskId`, `PanePresentation`, and `last_focused_at`.
`PanePresentation` is `Full`, `CompactManual`, or `CompactAutomatic`. Presentation identity is
never used as task, provider-session, PTY, runtime-generation, or action-epoch identity.

The tree exposes bounded pure operations adapted from Traycer and Herdr:

- insert a task beside the focused pane;
- remove a pane and collapse redundant split parents;
- move a pane to a target edge;
- swap two panes on a center drop;
- resize adjacent children;
- reset adjacent allocations to automatic;
- focus a pane and update monotonic recency;
- compact or expand a pane;
- find the least-recently-focused eligible full pane;
- validate unique node IDs, unique visible task IDs, finite positive sizes, valid focus, and a
  non-empty split child set.

Mutations are transactional at the model boundary: compute a candidate tree, validate it, then
replace the live tree. A failed move or malformed drop leaves the previous tree unchanged.

### 2. Allocation and automatic compaction

`src/ui/task_workspace/allocation.rs` computes pane rectangles from the recursive tree and current
canvas bounds.

- Auto children share remaining axis space by normalized weight.
- Dragging a divider changes only the two adjacent allocations and pins both results.
- Double-clicking a divider changes the adjacent allocations back to Auto.
- Pinned logical pixels survive adding/removing other tasks and normal window resizing.
- Full and compact panes have separate minimum sizes. Minimums are theme/density metrics rather
  than durable data.
- Before producing an unreadable rectangle, allocation automatically compacts the
  least-recently-focused pane that is Full, Auto, and not focused.
- Automatic compaction is reversible. When room returns, automatically compacted panes expand in
  most-recent-focus order. Manually compacted panes never auto-expand.
- If pinned panes alone cannot fit the viewport, they are clamped proportionally as the last
  resort. The model retains their requested logical sizes so they recover when room returns.

Adding a task chooses the focused pane's nearest split with an Auto allocation. If none exists,
it creates a sibling split at the focused pane and runs automatic compaction. It does not silently
rewrite manually pinned allocations elsewhere.

### 3. Native workspace surface

Add `src/ui/task_workspace/{mod.rs,pane.rs,render.rs,drag.rs}` and mount it in
`src/ui/native_shell.rs` where the single center conversation canvas is rendered today.

The recursive renderer is content-agnostic at the split level:

- split nodes render flex children and GPUI divider hit targets;
- pane nodes render a task-pane header and either Full or Compact content;
- divider drag updates a transient visual allocation during pointer movement and commits one
  model mutation plus one persistence write on release;
- pane-header drag uses GPUI `on_drag`, drag-over, and `on_drop` events;
- the target center means swap; edge bands mean move/split left, right, up, or down;
- invalid self-drops and descendant cycles are rejected before mutation.

The task header contains task name, actual project name, provider/activity state, Done state, Full
or Compact toggle, and Close-from-workspace. Close-from-workspace does not settle, archive, delete,
or stop the task.

Shift-click in the task list toggles workspace membership. Plain click focuses an already-open
pane. In a one-pane workspace, plain selection retains today's behavior. A task selected without
Shift while a multi-pane workspace exists becomes focused without removing the other panes.

The sidebar's project filter controls task discovery only. Open panes may belong to different
projects, and each header keeps its project label. A DevManager project continues to contain any
number of folders and repositories; no layout operation chooses or changes a repository,
worktree, branch, or cwd.

### 4. Task surface registry and exact ownership

The current `NativeInteraction`, `TaskCockpitShell`, `CockpitDock`, terminal adapter, and query
in-flight flags assume one selected task. Multi-task presentation must not make those single-owner
objects accept results from several tasks.

Add a task-surface registry keyed by exact `TaskId`:

```rust
TaskSurfaceRegistry {
    surfaces: BTreeMap<TaskId, TaskSurfaceState>,
    focused_task: Option<TaskId>,
}
```

Each `TaskSurfaceState` owns its conversation cache, requested/physical conversation cursors,
current center mode, bounded latest status/snippet, and any admitted read-only terminal snapshot.
Host results are admitted only when the response's TaskId and request generation match that
surface. Provider session IDs, terminal owners, runtime generations, composer fences, action
epochs, and request generations are never shared or inferred across panes.

Only the focused full pane owns interactive composer, approval/question controls, raw terminal
mouse/keyboard input, and the context dock. Changing focus rebinds these existing interactive
objects to the exact task through the current fenced selection path. Background panes are live
monitoring surfaces, not alternate input owners.

Query priority is proportional to visible work:

1. focused Full pane: current complete conversation paging, active terminal, and task cockpit;
2. unfocused Full panes: bounded conversation tail and selected read-only terminal snapshot;
3. Compact panes: task-list activity plus the last admitted cached snippet, with no terminal
   painting or background conversation pagination.

The scheduler keeps per-task in-flight state and bounded concurrency. A slow provider restore or
conversation query for one pane cannot monopolize local request execution or block focusing a
different pane.

### 5. Compact presentation

A compact pane remains a node in the recursive tree; it is not moved to a detached sidebar and
does not lose task identity. It renders:

- task and project names;
- provider/activity state, including blocked/waiting/working/done;
- latest bounded conversation or terminal activity summary;
- elapsed activity indicator where available;
- one-click Expand and Close-from-workspace controls.

Compact mode performs no full transcript layout, raw terminal cell painting, context-dock query,
or composer construction. This is both the information-density behavior approved by the user and
the performance boundary needed when monitoring many tasks.

### 6. Lifecycle behavior

- Settle/Done moves the task into the sidebar's Done section but leaves an open pane visible with
  a Done state.
- Sending a message from that pane uses the existing restore/reactivate command before submission.
- Restoring from the sidebar focuses or adds the same exact task pane.
- Archiving or deleting a task removes all corresponding workspace panes after the host confirms
  the operation.
- A task missing from a fresh canonical host snapshot is removed from the presentation tree; the
  tree is normalized and persisted.
- Closing a pane affects only local presentation state.

### 7. Persistence and migration

Extend `src/ui/workspace_layout.rs` to schema v4 with an optional/defaulted serialized task
workspace containing the tree, focused pane, recency order, allocations, presentation modes, and
per-task center-canvas preference.

- A v3 layout with `selected_task` migrates to a one-pane workspace.
- Existing `task_center_terminal` values seed the matching task surface modes.
- Unknown task IDs are pruned after the canonical task snapshot loads.
- Duplicate TaskIds, duplicate node IDs, non-finite allocations, invalid focus, and malformed
  split shapes fail closed to a one-pane layout for the selected canonical task.
- Persistence continues through the existing profile-scoped, atomic workspace-layout path. No
  task layout is stored by an LLM provider or fetched from the LLM on load.
- The installed profile and test profile remain isolated under the existing persistence rules.

## Error handling and recovery

- Layout errors are local presentation errors and never mutate task/runtime state.
- A failed drag/drop or persistence write leaves the last valid in-memory tree visible and reports
  a bounded non-modal error.
- A per-pane projection failure appears inside that exact pane; other panes remain interactive.
- Focus rebinding uses the existing navigation/focus epochs. Late responses from the previously
  focused task cannot rewrite the current composer, terminal, or cockpit.
- Restoring a layout never synthesizes provider conversation identity or resumes a provider solely
  because a pane exists.
- Canonical snapshot/replay remains the only authority that enables task mutations after startup;
  preview data may populate pane shells but not composers or terminal input.

## Performance boundaries

- Split layout calculation is pure and linear in visible node count.
- Tree mutations clone/validate only presentation state; they do not clone transcripts or terminal
  buffers.
- Each pane subscribes to its own task surface state so one task's output does not rebuild every
  conversation pane.
- Divider movement uses transient GPUI element state and one durable commit on release.
- Compact panes avoid expensive presentation work by construction.
- Acceptance measures one pane versus fifteen mixed Full/Compact panes for render count, query
  count, focus latency, and idle CPU. Background panes must not multiply provider restore work.

## Verification

Implementation follows focused RED/GREEN slices, but broad suites run once at the end as requested.

1. Port/adapt pure layout tests from Herdr and Traycer for split, insert, remove, normalize, swap,
   edge move, failed-move rollback, resize clamping, unique identities, focus history, and
   serialization round trips.
2. Add allocation tests for mixed Auto/Pinned children, LRU automatic compaction, automatic
   re-expansion, manual-compaction stability, viewport pressure, and divider reset.
3. Add workspace-layout v3-to-v4 migration and corrupt-tree fail-closed tests.
4. Add native interaction tests for Shift-click membership, plain-click focus, exact-task result
   admission, background query isolation, settle/restore, archive/delete removal, and composer/
   terminal focus ownership.
5. Run one fresh-process GPUI headless scenario covering add, focus, resize, compact, drag/swap,
   edge move, settle, expand, and persistence restore in a single Application lifetime.
6. Run a visible dev-app acceptance sweep at representative window sizes, including fifteen mixed
   panes and cross-project tasks.
7. Run final format, complete Rust library suite serially, `cargo check --locked --lib --bins
   --tests`, and applicable native UI gates in one isolated `CARGO_TARGET_DIR`, then verify no test
   harness, Cargo, rustc, or linker processes remain.
8. Confirm production `config.json` and `remote.json` hashes and installed DevManager PID/start
   time are unchanged by verification; treat `session.json` separately.

## Out of scope

- Overlapping free-floating child windows and z-order.
- Hidden tab stacks in the first implementation; every selected task remains visible.
- Host protocol, provider-session, task-journal, repository, worktree, branch, or cwd changes.
- Automatic stopping, settling, archiving, or deleting when a pane closes.
- Replacing DevManager's multi-folder project model with a single-repository model.

## Acceptance criteria

- Shift-clicking several tasks shows all of them in one native recursive workspace.
- Full panes stay live, and exactly one focused pane owns interactive input.
- Manual resize persists; untouched panes auto-size around pinned panes.
- Viewport pressure compacts the least-recently-focused eligible pane and reverses automatic
  compaction when room returns.
- Users can manually compact/expand, resize/reset, swap, edge-move, focus, and close panes.
- Settled tasks stay visible as Done; a new message restores them; archive/delete removes them.
- Cross-project panes preserve the actual project label and do not alter multi-folder/repository
  scope.
- Layout and pane modes survive restart without asking an LLM or weakening canonical host truth.
- Slow or failed work in one pane does not block or corrupt another pane.
- The final native feature sweep, Rust suite, compiler check, persistence-hash check, and process
  cleanup checks pass.
