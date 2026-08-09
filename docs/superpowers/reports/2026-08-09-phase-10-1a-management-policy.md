# Phase 10.1a management/privacy policy report

Status: GREEN — narrow Phase 10.1 management/privacy policy correction
implemented in the isolated `phase-10-1-management-policy` worktree.

## Owned files

- `src/connect/policy.rs` — closed privacy classes, managed-field allowlist and
  explicit denylist, validated enrollment/grants, role/scope decisions, fixed
  reason codes, and the validated 15-minute active-session interval. Decision
  construction and grant construction are private; callers only inspect
  decisions, and grants are non-copyable/non-cloneable, revoke in place, and
  are borrowed by principals. No grant issuer or `grant_issuer` seam exists.
- `tests/management_policy.rs` — public policy contract tests, including
  negative trait assertions and same-grant post-revocation denial. API privacy
  is compiled by the `ManagementGrant` `compile_fail` doctests instead of
  source-string inspection.
- `docs/superpowers/reports/2026-08-09-phase-10-1a-management-policy.md` —
  phase evidence report.

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
- Production grant issuance is explicitly unavailable until Task 10.7 binds
  signed grants to account/device identity, membership, tenant, task link,
  policy revision, expiry, and allowed classes.

## Verification

- RED evidence: review of `d1e3a58` showed the unauthenticated crate-private
  issuer and a source-string visibility assertion; the correction replaces
  both with the closed boundary and compiled negative cases.
- GREEN: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101-authority cargo test
  --locked --test management_policy -- --nocapture` — 6 passed, 0 failed.
- GREEN internal: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101-authority cargo
  test --locked --lib connect::policy::tests -- --nocapture` — 6 passed, 0
  failed.
- GREEN compiled API gate: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101-authority
  cargo test --locked --doc ManagementGrant -- --nocapture` — 2 compile-fail
  doctests passed. The compiler diagnostics were the intended private
  associated-function `E0624` and private-field `E0451` errors.
- Library: `CARGO_TARGET_DIR=C:\Temp\devmanager-phase101-authority cargo
  check --locked --lib` — exit 0. Seven unrelated `src/kernel`/`src/host`
  warnings remain.
- `rustfmt --edition 2021 --check` passed for the two owned Rust files and
  `git diff --check` passed. Repo-wide `cargo fmt -- --check` still reports
  unrelated pre-existing drift in `tests/connect_session.rs`, which was left
  untouched.
- Final exact-target/worktree residue check found no Cargo, rustc, rustfmt, or
  test-harness process; unrelated concurrent worktree processes were left
  untouched.

## Residual risks and later gates

- This contract is not yet wired into projections, transport, storage, grant
  persistence/revocation, or runtime actions; those remain later phase work.
- Task 10.7 remains the grant-issuance blocker: no signed identity,
  membership/tenant binding, task link, policy revision, expiry, or allowed
  class validation exists here yet.
- Active intervals must be split by the caller at the 15-minute idle boundary.
- No full repository suite was run, per the bounded task scope.
