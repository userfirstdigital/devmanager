# Phase 10.1a management/privacy policy report

Status: GREEN — Phase 9 → 10 policy boundary hardening completed in the
isolated `phase-9-10-policy-integration` worktree.

## Owned files

- `src/connect/policy.rs` — closed privacy classes, managed-field allowlist and
  explicit denylist, crate-only host admission through the canonical
  `PermissionEvaluator`, provenance-carrying action/content evidence, exact
  connection/session/client/task/action-epoch and task/resource-generation
  binding, fixed reason codes, one-shot grants, and the validated 15-minute
  active-session interval. Decision, context, principal, operation, and grant
  construction are private; callers only inspect decisions. Grants carry an
  opaque id/nonce, are non-copyable/non-cloneable, revoke in place, and are
  consumed once by an allowed decision.
- `tests/management_policy.rs` — public descriptive contract tests, including
  negative trait assertions, exhaustive field classes, fixed reason codes, and
  the active-session boundary. API privacy and the removed caller-labelled
  constructors are compiled by `compile_fail` doctests instead of
  source-string inspection. Runtime authority tests stay in the crate-private
  policy module because external callers must have no issuance path.
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
  Grants are exact connection/session/client/task/action-epoch authorities with
  opaque id/nonce, expiry, revocation, and one-shot replay tracking. Exact
  task/resource generations are checked on every evidence record. Exactly 15
  minutes is accepted for an active interval; one millisecond over the
  boundary is rejected.
- The host bridge fails closed unless the deferred signed external authority is
  present. It calls the canonical `PermissionEvaluator` before issuing any
  current authority, derives action evidence from validated `CommandEnvelope`
  values or host reducers, and derives field/content pairs atomically so raw
  evidence cannot be relabelled as safe metadata.

## Verification

- RED evidence: the pre-correction public integration test compiled and then
  allowed a caller-labelled `PolicyOperation::MutateTask`; the same borrowed
  grant was also accepted twice. Compile-fail probes additionally showed that
  `PolicyPrincipal::Owner`, arbitrary `TaskContext::enrolled`, and the
  caller-labelled operation constructors were all reachable. The correction
  removes those paths and adds runtime foreign/stale/relabel/replay proofs.
- GREEN runtime and compile evidence used the fresh target
  `CARGO_TARGET_DIR=C:\Temp\devmanager-phase910-policy-correction2` with
  `CARGO_BUILD_JOBS=1`:
  - `cargo test --locked --lib connect::policy::tests -- --nocapture` — 8
    passed, including foreign/stale/relabel/replay/revocation/expiry proofs.
  - `cargo test --locked --test management_policy -- --nocapture` — 3 passed,
    including the public field, interval, reason-code, and non-copy/non-clone
    contract.
  - Compile-fail doctests passed for `TaskContext` (1), `ManagementGrant` (2),
    and `PolicyOperation` (1); they reject public context/principal/grant and
    caller-labelled operation construction.
  - `cargo test --locked --test connect_contract --test connect_session
    --test protocol_contract -- --nocapture` — 28 contract, 8 session, and 50
    protocol tests passed.
  - `cargo test --locked --lib connect -- --test-threads=1` — 94 tests passed.
  - `cargo check --locked --lib` — exit 0. Seven unrelated
    `src/kernel`/`src/host` warnings remain; the corrected policy module adds
    none.
- `cargo fmt --all -- --check` and `git diff --check` passed. The accepted
  Connect transport/envelope/permission/presence/schema files are byte-identical
  to `b1b65ba`; `git merge-base --is-ancestor b1b65ba HEAD` passed.
- The final exact-target residue check found no Cargo, rustc, rustfmt, or test
  harness process. Concurrent processes belonging to other worktrees were not
  touched.

## Residual risks and later gates

- This contract is not yet wired into projections, transport, storage, grant
  persistence/revocation, or runtime actions; those remain later phase work.
- Signed external identity/tenant/membership issuance remains deferred. Until
  that issuer exists, the crate-only host bridge fails closed rather than
  accepting caller-asserted organization class, consent, or membership.
- Persisted grants and persisted revocation, durable replay/idempotency
  tracking, and the connection/session binding issuer remain deferred. The
  current in-memory grant is bounded to one opaque authority and one allowed
  decision only.
- Projection/runtime wiring, host reducer integration, and transport/session
  lifecycle revocation remain deferred; this module does not imply current
  enforcement outside the policy decision boundary.
- Active intervals must be split by the caller at the 15-minute idle boundary.
- The complete repository-wide library suite was not required for this bounded
  correction; policy, management, Connect contract/session/protocol, serial
  `lib connect`, formatting, locked library check, ancestry, and transport
  byte-diff gates all passed above.
