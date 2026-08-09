# Phase 10.1a management/privacy policy report

Status: GREEN — pure transport-independent policy contract implemented in the
isolated `phase-10-1-management-policy` worktree.

## Owned files

- `src/connect/policy.rs` — closed privacy classes, managed-field allowlist and
  explicit denylist, validated enrollment/grants, role/scope decisions, fixed
  reason codes, and the validated 15-minute active-session interval.
- `src/connect/mod.rs` — policy module and public contract export only.
- `tests/management_policy.rs` — RED/GREEN contract tests.

The policy type is named `ManagementPrivacyClass` because Phase 9 already
exports a wire `connect::PrivacyClass` alias. The policy has no serde derives,
wire shape, transport, persistence, outbox, remote-store, UI, Portal, legacy
remote, or provider-runtime dependency.

## Contract coverage

- Allowlist covers task state/attention/assignment reference, provider kind and
  state, source/observed timestamps, provider-reported usage, human
  message/turn counts, bounded active-session intervals, Git summary, host
  health, and approved artifact references.
- Denylist names quota, cost, estimate, prompts, responses, terminal, browser,
  recordings, file bodies, full diffs, credentials, and environment values.
- Unmanaged tasks, absent/not-yet-valid/stale/revoked/wrong-task grants, raw
  content, personal tasks without enrollment and consent, unknown fields,
  watcher mutation, and non-owner dangerous approval deny with fixed codes.
- Collaborator mutation is allowed only through a valid matching task grant.
  Exactly 15 minutes is accepted for an active interval; one millisecond over
  the boundary is rejected.

## Verification

- RED: the new integration test first failed with unresolved policy imports
  before any policy production code existed.
- GREEN: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101a-policy cargo test
  --test management_policy -- --nocapture` — 10 passed, 0 failed.
- Library: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101a-policy cargo check
  --lib` — exit 0. It retains seven pre-existing warnings in unrelated
  `src/kernel`/`src/host` code.
- `rustfmt --edition 2021` passed for the three owned Rust files.
- `git diff --check` passed.
- The exact policy Cargo/rustc/test-harness process tree was empty after the
  checks; unrelated concurrent worktree Cargo processes were left untouched.

## Residual risks and later gates

- This contract is not yet wired into projections, transport, storage, grant
  persistence/revocation, or runtime actions; those remain later phase work.
- Active intervals must be split by the caller at the 15-minute idle boundary.
- No full repository suite was run, per the bounded task scope.
