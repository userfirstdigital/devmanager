# Phase 9 Batch2 slice B: canonical protocol chunk

## Scope

Implemented only the Phase 1 protocol chunk primitive. Connect schema, envelope,
and transport remain untouched.

- `src/protocol/chunk.rs`: strict named MessagePack `ChunkFrame`, negotiated
  `ChunkLimits`, and incremental poisoned `ChunkContext`.
- `src/protocol/mod.rs`: public protocol exports only.
- `tests/protocol_contract.rs`: six focused chunk contract tests.

## Proof

RED was observed first: the focused test target failed because the three new
protocol exports did not yet exist.

GREEN command:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase91-chunk cargo test --test protocol_contract chunk_ -- --nocapture
```

Result: 6 passed, 0 failed, 42 filtered out.

The tests cover the six-field canonical map and binary fields, unknown and
positional rejection, limit negotiation/fail-closed validation, cumulative
SHA-256, contiguous/duplicate/out-of-order identity, transfer/cursor binding,
final/post-final behavior, permanent poisoning, per-chunk/cumulative/overflow
bounds, and the single cursor limit.

Additional checks: rustfmt on all owned Rust files and `git diff --check` both
pass. No Cargo, rustc, rustdoc, or protocol test harness process remains for
the isolated target/worktree.

## Residual risk

The Connect layer intentionally still has its pre-existing chunk shape and is
not integrated in this slice. The full repository suite was not run because
the requested gate is the focused protocol contract; unrelated pre-existing
library warnings remain visible in that run.
