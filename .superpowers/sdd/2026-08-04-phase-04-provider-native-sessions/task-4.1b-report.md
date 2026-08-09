# Phase 4 Task 4.1 Batch B report

Status: PARTIAL 4.1b — typed provider identity, strict executable identity and
discovery validation, and the registry-owned fresh-auth evidence seam.

## Scope

This batch changes only the owned provider/domain boundary. It does not modify
the Batch A registry or adapter implementation, legacy session/runtime fields,
packaging, callsites, the installed DevManager, or any real provider tool.

## TDD evidence

### RED

Before the implementation, the new `provider_identity` test target was run with:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-20260809 cargo test --test provider_identity -- --nocapture
```

It failed to compile as intended. The tests exposed the missing
`ProviderAuthEvidenceRegistry`, typed auth/discovery seam, strict executable
constructor/current-validation API, and `ProviderSessionId` `FromSql` support.

### GREEN

The final focused run passed:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-20260809 cargo test --test provider_identity -- --nocapture
11 passed; 0 failed
```

Additional gates passed:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-20260809 cargo check --lib
cargo fmt --all -- --check
git diff --check
```

No Batch B Cargo/Rust/test process remained after verification. Existing
unrelated compiler warnings remain; they do not fail these gates.

## Delivered

- `ProviderSessionId` preserves exact provider-issued bytes, rejects empty,
  bounded/unsafe values, and has checked serde plus SQLite `ToSql`/`FromSql`.
  `AgentSessionFacts` remains typed without migrating legacy UI/runtime string
  fields.
- `ProviderExecutable` now carries canonical path, platform file identity, and
  SHA-256 evidence. Construction, serde, and current-file validation fail
  closed for non-files, symlink/reparse paths, ambiguous hardlinks where the
  platform exposes link counts, forged metadata, path replacement, and
  same-path replacement between validation snapshots.
- `ProviderDiscoveryContract` preserves candidate order and provenance,
  resolves PATH candidates before trust, enforces provider entrypoint/type
  allowlists, rejects the desktop Cursor executable for Cursor Agent, and
  accepts Windows `.cmd` only through an explicit, bounded, exact controlled
  shim proof targeting the validated native executable.
- `ProviderCapabilities::stable_projection` removes authentication from the
  stable cache projection.
- `ProviderAuthEvidenceRegistry` issues nonce/generation-bound invocations and
  accepts only fresh receipts tied to the expected provider kind and exact
  executable identity. Replay, wrong identity/provider, expired, future,
  reordered, same-timestamp, and fabricated evidence fail closed. API-key
  detection is explicit and never treated as authenticated subscription.

## Integration seam and deferred work

Batch A can consume the narrow typed seam by owning one
`ProviderAuthEvidenceRegistry`: begin an invocation for the exact discovered
identity, let the provider-specific adapter choose its supported status
command, and accept the result only through the issued invocation. Cache only
the stable projection; auth must be refreshed through a correlated receipt,
including on cache hits. The existing provider-neutral `[auth,status]` adapter
path remains outside this batch and must be replaced/retired during the Batch A
integration in its owned files.

Batch A registry integration is intentionally deferred. Batch C packaging and
the required callsite/legacy compile migration are also deferred. Exact resume
must continue to use only correlated current-generation provider session
identity and must not fall back to a fresh conversation after an exact-resume
failure.

## Review and commit

The complete diff was reviewed in one correction batch. The correction added
dedicated hardlink ambiguity failure and canonical PATH-origin validation. No
review subagent was used because the task explicitly prohibited agents.

Commit: final coherent PARTIAL 4.1b commit containing this report; its SHA is
reported in the handoff below.

## Sharpening the Axe

Worked: a bounded red/green pass kept the new identity/evidence contract
isolated from concurrent Batch A work. Wasted time: the first RED pass included
a test-only JSON macro typo. Earlier improvement: add a tiny fixture/test
compile smoke check before capturing RED so test-authoring mistakes are
separated from missing-production-API failures. No persistent guidance update
is warranted; the existing project instructions already cover typed provider
identity and exact-resume/no-fallback invariants.
