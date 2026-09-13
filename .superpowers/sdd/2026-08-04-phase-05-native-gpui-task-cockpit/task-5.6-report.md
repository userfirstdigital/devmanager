# Task 5.6 truthful task header and top-bar projection report

Date: 2026-08-09
Base: `48a65ab` (`fix(ui): integrate phase 5 UI chain contracts`)

Status: complete for the pure projection slice. No GPUI visual capture, live
host wiring, provider polling, or installed-app integration is claimed.

## Delivered

- Added `TaskHeaderModel` under `src/ui/task_cockpit/header.rs`. It projects
  title, stable `ProjectId` label, Main/worktree/external workspace, Primary,
  bounded specialists, and task-local turn/status facts from one
  `ClientModel` snapshot. Visible status uses the existing semantic
  connectivity/attention/activity/review precedence.
- Captured task `revision`/`action_epoch`, agent ID/runtime generation, and
  correlated `provider_session_id` in status and agent actions. Status actions
  target Command Center; agent actions retain the exact agent identity.
- Added `TopBarProjectionInput` as a narrow optional boundary for current
  host/connect/update/quota/resource observations that are not yet part of
  `ClientModel`. The top bar contains only explicit supplied facts.
- Provider quota projection is provider-deduplicated, hides observations older
  than `PROVIDER_QUOTA_MAX_AGE_MS` (one hour), and omits unavailable observations
  instead of displaying a misleading cached value. Command Center remains the
  diagnostic surface for unavailable quota state.
- Host resource projection preserves optional missing values, clamps normal
  CPU to whole-machine `0..=100`, and exposes raw core-equivalent CPU only as
  the explicitly labelled `Core-equivalent CPU (diagnostic)` field.
- Added deterministic responsive priority/overflow models with an accessible
  text-labelled `More task details` control. No direct colors or host probes
  are present in the projection code.
- Added the `ClientModel::task` accessor and selected-task `Shell::task_header`
  seam without duplicating task facts into shell state.

## TDD evidence

RED was captured before the production header module existed:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase56-header cargo test --test ui_header_projection header_ -- --nocapture
error[E0432]: unresolved imports ... HeaderAction, HeaderField, PrimaryAgentProjection,
TaskHeaderModel, TopBarModel, TopBarProjectionInput, WorkspaceProjection
```

Focused GREEN after implementation and the final correction round:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase56-header cargo test --test ui_header_projection -- --nocapture
5 passed; 0 failed
```

The fixture-backed tests cover task context and worktree projection, Primary
and specialist identity, task status links, missing Primary truthfulness,
selected-task shell projection, fresh/stale/unavailable quota handling,
whole-machine CPU normalization and diagnostic labelling, and narrow-width
accessible overflow.

## Integration dependencies and limits

- `ClientModel` currently has no project-name projection, so the header uses
  the stable project ID rather than inventing a display name.
- `ClientModel` currently has no global host/connect/update/quota/resource
  observation stream. The new `TopBarProjectionInput` is intentionally
  optional and must be populated by an existing bounded runtime/updater/quota
  owner before a production shell renderer can show those facts.
- No full library suite, GPUI window, screenshot, live host, network,
  provider launch, performance, or installed-app claim is made in this slice.

## Verification

- `cargo fmt -- src/ui/task_cockpit/header.rs src/ui/task_cockpit/mod.rs src/ui/shell.rs src/client/model.rs tests/ui_header_projection.rs` — passed.
- Focused test file — 5 passed, 0 failed.
- The existing seven library warnings remain outside the owned paths; the new
  projection code adds no warnings.
- The final process and clean-tree checks are recorded in the handoff after
  commit. Concurrent Cargo processes from other worktrees were not touched.

## Owned files

`src/ui/task_cockpit/header.rs`, `src/ui/task_cockpit/mod.rs`, `src/ui/shell.rs`,
`src/client/model.rs`, `tests/ui_header_projection.rs`,
`tests/fixtures/ui/header-states.json`, and this report.
