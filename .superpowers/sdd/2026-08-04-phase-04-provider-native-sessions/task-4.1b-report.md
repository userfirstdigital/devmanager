# Phase 4 Task 4.1 Batch B report

Status: COMPLETE 4.1b boundary — typed provider identity, strict executable
identity/discovery validation, bounded versioned capability/auth evidence, and
the registry-owned fresh-auth evidence seam. Adapter/process integration
remains intentionally deferred.

## Scope

This batch changes only the owned provider/domain boundary. It does not modify
the Batch A registry or adapter implementation, legacy session/runtime fields,
packaging, callsites, the installed DevManager, or any real provider tool.

## TDD evidence

### RED

The saved dirty diff was continued from its furthest point. Focused red checks
then exposed the remaining boundary gaps: versioned lifecycle fields were not
decoded, executable/debug paths were visible, provider-mismatched auth sources
were accepted, and discovery debug/errors leaked paths. A registry assertion
also still expected the pre-lifecycle four-field evidence wire shape.

### GREEN

The final focused identity run passed:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-final cargo test --test provider_identity -- --nocapture
19 passed; 0 failed
```

The selected identity/cache/auth registry cases passed individually:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-final cargo test --test provider_registry <selected identity test> -- --exact
26 selected tests; 26 passed; 0 failed
```

Final gates passed:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-final cargo check --lib
cargo fmt --all -- --check
git diff --check
```

No broad suite was run. No targeted Cargo, rustc, or test-harness process
remained after verification. Existing unrelated compiler warnings remain; they
do not fail these gates.

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
- Provider versions, executable identities, capability evidence, and capability
  wires are bounded and schema-versioned. Unknown fields/versions fail closed,
  evidence collections are bounded before growth, and Debug/errors redact paths,
  nonces, and raw provider details.
- `ProviderPathSnapshot` owns PATH order/provenance, rejects reparse entries
  before canonicalization, and never accepts a caller-forged `PathEntry`.
  Checked-in foreign `.exe` fixture material was removed; tests use a
  test-created copy of the current native test executable as metadata only and
  never execute it.
- `ProviderCapabilities::stable_projection` removes authentication from the
  stable cache projection. Existing registry identity/cache tests cover exact
  provider+executable/version/revision keys, fresh auth refresh, generation
  ordering, and replacement/version invalidation.
- `ProviderAuthEvidenceRegistry` issues provider-specific subscription-login,
  nonce/generation-bound invocations and accepts only fresh receipts tied to the
  expected provider kind and exact executable identity. Receipts expose source,
  observed/expiry time, and confidence. Replay, wrong identity/provider/source,
  expired, future, reordered, same-timestamp, and fabricated evidence fail
  closed. API-key detection is explicit and never treated as authenticated
  subscription.

## Integration seam and deferred work

Batch A can consume the narrow typed seam by owning one
`ProviderAuthEvidenceRegistry`: begin an invocation for the exact discovered
identity, let the provider-specific adapter choose its supported status
command, and accept the result only through the issued invocation. Cache only
the stable projection; auth must be refreshed through a correlated receipt,
including on cache hits. The existing provider-neutral `[auth,status]` adapter
path remains outside this batch and must be replaced/retired during the Batch A
integration in its owned files.

Corrected Task 3.7/4.1a owns the provider-specific subscription-login command,
probe process lifecycle, timeout/output scrubbing, process-tree containment,
and wiring the typed receipt into the active registry. Those files were not
changed here, and no provider CLI, process probe, terminal, launcher, or app
was run. The existing provider-neutral `[auth,status]` adapter path remains
outside this boundary and must be replaced/retired there.

Batch C packaging and the required callsite/legacy compile migration are also
deferred. No `providerSessionId` is inferred here, and no transcript or rollout
parsing is introduced. Exact resume must continue to use only correlated
current-generation provider session identity and must not fall back to a fresh
conversation after an exact-resume failure.

## Review and commit

The complete owned diff was reviewed after the final gates. The correction
batch added bounded/versioned lifecycle wires, provider-source validation,
redacted discovery diagnostics, pre-canonicalization reparse rejection, and
bounded shim reads. No review subagent was used because the task explicitly
prohibited agents. The handoff commit contains only the allowed source, test,
fixture deletion, and report paths.

Commit: final coherent 4.1b boundary commit containing this report; its SHA is
reported in the handoff below.

## Sharpening the Axe

Worked: preserving the saved dirty diff and keeping the final verification on
the identity/cache/auth surface isolated from concurrent Batch A work. Wasted
time: host-wide `C:` pressure caused repeated cold rebuilds; cleaning only the
task target reclaimed 9.9 GiB. Earlier improvement: check free space before a
cold Cargo run while preserving the single-build/no-duplicate rule. No
persistent guidance update is warranted: the existing project instructions
already cover the 600-second cold build, process-tree checks, typed provider
identity, and exact-resume/no-fallback invariants; the disk pressure was
environment-specific.
