# Phase 3.10 process-soak contract

This document defines bounded infrastructure evidence. It does not claim that
the installed DevManager is healthy and it is not a release result. The soak
uses only a temporary, process-unique worktree fixture. It never launches,
installs, restarts, or inspects an installed app; it never reads or hashes a
production profile, `config.json`, `remote.json`, or `session.json`.

## Immutable run input

`Invoke-ProcessSoak.ps1` accepts one UTF-8 JSON manifest (maximum 1 MiB) and
rejects unknown or missing fields. The manifest contains `schemaVersion`, an
immutable `revision`, SHA-256 plus canonical paths for the supervisor, helper,
and cycle executables, a temporary working directory, `seed`, bounded
`iterations` (at most 100), bounded suite/cycle/cleanup deadlines and stdout,
stderr, and result byte caps, and a finite `scenarioCatalog`. Every scenario
is an argument array beginning with the fixed `cycle` protocol and has an
expected exit code. The script hashes every executable before dispatch and the
Rust supervisor repeats canonical identity and hash validation before launch.
The run records bounded manifest content, and the original input hash is
verified unchanged after execution; the retained `manifest.json` artifact records
the revision, seed, budgets, scenario catalog, binary hashes, byte count, and
source name without persisting an absolute source path.

## Fixed Rust supervisor

The PowerShell layer starts the allowlisted supervisor as a child with an
argument array. It never dot-sources a callback, imports worktree code, uses
`Start-Process`, polls with sleeps, or terminates a raw PID. On Windows the
Rust supervisor creates and owns one Job Object per cycle, launches the exact
cycle executable suspended with the validated argument array, assigns it to
the Job before resuming it, and records a live process creation time and
canonical executable path. It reads stdout and stderr concurrently under hard
caps and parses exactly one bounded JSON result. Per-cycle and suite deadlines
use monotonic clocks. On timeout, interruption, malformed/multiple/oversized
output, crash, nonzero exit, or residue it terminates the owned Job, joins
both readers, and independently proves `ACTIVE_PROCESS_ZERO` through the Job
completion port and active-member query. A raw PID is never a termination
authority. Every reported member identity is obtained by opening the live Job
member and querying its creation time and executable path; self-reported PIDs
from cycle output are informational only.

The result itself is one bounded JSON line. A passing result requires every
cycle to have the expected exit, one valid result, exact scenario/iteration
identity, live root identity, and `activeProcessZero=true`. Rejected manifests,
missing tools, malformed protocol output, nonzero exits, crashes, deadlines,
reader cap violations, and incomplete cleanup are failures or `UNAVAILABLE`,
never false passes.

## Evidence publication

Evidence is written only beneath the validated temporary root:

```text
.devmanager-next/evidence/phase-03-process-soak/runs/<unique-run>/
  manifest.json
  summary.json
  performance.json
  conformance.json
  run.json
```

The root and run name are canonicalized and checked for reparse points. Each
artifact is serialized to a same-directory temporary file and atomically moved
into its previously absent final name. Run IDs are unique and append-only;
existing files are never overwritten. `performance.json` contains each cycle
duration plus p50, p95, maximum, and count. `conformance.json` records the
manifest revision/hash, scenario outcomes, exact-one-JSON validation, reader
caps, real identities, cleanup deadline, and Job zero proof.

## Budgets and measurements

These are provisional infrastructure budgets for focused validation:

| Measurement | Budget | Evidence |
| --- | ---: | --- |
| Cycle/supervisor close settlement | ≤ 500 ms p95 | Monotonic cycle duration and `ACTIVE_PROCESS_ZERO` |
| First bounded result | ≤ 500 ms p95 | One exact result line and byte counts |
| 10 MiB bounded output fixture | ≤ 5 s, zero loss | Capped stdout/stderr totals and failure on overflow |
| 100-cycle memory/handle review | ≤ 16 MiB / 32 handles delta | Future real soak only; never inferred from this dry run |

Report p50 and p95 from sorted monotonic durations: p50 is the ceiling of
`count × 0.50`, p95 the ceiling of `count × 0.95`, both clamped to the last
sample. Missing or ambiguous timestamps fail the measurement.

CPU percentages use the same denominator as Windows Task Manager. For a
sample interval of `sampleMs`, process CPU time `processMs`, and `logical`
logical processors:

```text
whole-machine % = processMs / (sampleMs × logical) × 100
core-equivalent % = processMs / sampleMs × 100
```

The first is capped at 100% for a single process on a whole-machine display;
the second can exceed 100% when multiple logical processors are consumed.
The evidence records the logical-processor count and both values, not an
ambiguous “CPU percent.”

## Conformance corpus

ANSI/VT handling is referenced by the versioned corpus at
`tests/fixtures/ansi/phase3-v1.json`. It includes clear-line, color, cursor,
and escape sequences split across reader chunks. The fixed cycle fixture also
records this corpus revision. Timeout tree cleanup, occupied external
listeners, helper/cycle hash mismatch, malformed/multiple/oversized output,
nonzero exit, crash, restart/resume, interruption, and no-false-pass paths
are covered by the focused infrastructure tests. Only those tests and an
isolated two-cycle dry run are allowed before the final Phase 3 union; do not
run the 100-cycle soak on a partial union.
