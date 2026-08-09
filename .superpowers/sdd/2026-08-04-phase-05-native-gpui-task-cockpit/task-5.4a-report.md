# Task 5.4a pure Task Cockpit shell/task-list/focus contract

Date: 2026-08-09
Base: `fe6997d`
Status: complete for the pure contract slice; no GPUI/live-host integration is claimed.

## Delivered

- `src/ui/actions.rs` decorates only `client::action::catalog()` entries with
  presentation kind, shortcut, local selection-based availability, disabled
  reason, and independent accessibility metadata. It adds no action IDs,
  factories, callbacks, or host capability reads.
- `src/ui/task_cockpit/inbox.rs` projects only ordered `TaskId` values from
  `&ClientModel`, retains a local viewport, caps retained rows at 5,000, and
  exposes explicit overflow counts. `VirtualWindow` uses fixed overscan 32,
  bounded below the required 80-row ceiling.
- `src/ui/shell.rs` owns optional selected task, checked navigation epoch,
  transient priority, and one exact pointer owner. Navigation mouse-down is
  consumed and commits only at the current epoch; mouse-up is always consumed.
  Terminal release authorization requires exact pointer, task, generation,
  button, and epoch. View/focus/deactivate/resync boundaries and mismatches
  clear ownership and reject stale releases without synthesizing input.
- `src/ui/mod.rs` adds only the narrow module exports. No 5.3 component or
  callback API is used.

## TDD evidence

RED was captured before production modules existed:

```text
cargo test --locked --test ui_actions -- --nocapture
error[E0432]: unresolved imports `devmanager::ui::actions`
```

GREEN focused coverage:

```text
cargo test --locked --test ui_actions --test ui_focus --test ui_task_list -- --nocapture
9 passed; 0 failed
```

The tests cover shared action identity and accessibility, navigation epoch
commit/rejection and consumption, exact pointer ownership, lifecycle
invalidation, local priority, deterministic 5,000-task projection, explicit
5,001-task overflow, fixed overscan, viewport clamping, and invalid-viewport
zero effects.

## Verification

All Rust commands used:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase54a-shell-20260809
```

- `cargo check --locked --lib` — passed.
- `cargo fmt --all -- --check` — passed.
- Focused tests — 9 passed, 0 failed.
- The existing seven library warnings remain outside the owned paths; the new
  modules add no warnings.
- No full suite, GPUI window, live host, terminal, installed app, production
  AppData, network, merge, push, or performance claim was made.

## Owned files

`src/ui/actions.rs`, `src/ui/shell.rs`,
`src/ui/task_cockpit/{mod,inbox}.rs`, the narrow `src/ui/mod.rs` export,
`tests/ui_{actions,focus,task_list}.rs`,
`tests/fixtures/ui/task-list-{states,5000}.json`, and this report.
