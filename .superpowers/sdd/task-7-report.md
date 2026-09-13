# Task 7: Host create-with-primary-agent

## RED

`cargo test --lib client::action::tests::task_create_v2_carries_optional_primary_provider -- --exact --test-threads=1`
failed before implementation because `TaskCreateV2Arguments` had no
`primary_provider` field (`E0560`, `E0609`).

## GREEN

- Focused factory test passed.
- Focused host/kernel binding test passed:
  `host::connection::workspace_security_tests::create_with_primary_provider_binds_generation_one_before_launch`
- `cargo check --locked --lib --bins --tests` passed with
  `CARGO_TARGET_DIR=C:/Temp/devmanager-project-agent-first-run`.
- The required full library suite was started isolated with one test thread,
  but stalled in an existing provider capability test after more than 20
  minutes; its exact Cargo/test process tree was terminated. The last
  observed test was
  `providers::capabilities::auth_timestamp_tests::retired_generation_survives_receipt_body_eviction_and_rejects_older_pending_probe`.

## Scope

Create requests now carry an optional provider, the host rejects Cursor,
binds Claude/Codex agents and task terminals at generation 1, sets the
primary agent, and launches a new conversation using action epoch 0.

## Task 7 review-fix RED/GREEN

### RED

Added executor-level regression coverage:
`host::connection::output_tests::create_with_primary_provider_keeps_bindings_when_launch_fails`.
Before the fix it failed because the no-runtime launch returned
`Err(IpcError::Unavailable)` instead of the accepted create receipt.

### GREEN

- The regression test passed: Claude create returned an Accepted receipt,
  persisted the primary agent and terminal at runtime generation 1, and Cursor
  was rejected without persisting a task.
- The existing
  `host::connection::workspace_security_tests::create_with_primary_provider_binds_generation_one_before_launch`
  test passed.
- `cargo check --locked --lib --bins --tests` passed with
  `CARGO_TARGET_DIR=C:/Temp/devmanager-project-agent-first-run`.

### Task 7 review-fix: update-drain launch gate

Added the existing `stops_new_launches()` check to provider-backed task
creation before normalization/persistence, preserving the keep-task-on-launch-
failure behavior.
