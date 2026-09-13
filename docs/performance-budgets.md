# Phase 3.10 process-soak contract

This document defines bounded infrastructure evidence. It does not claim that
the installed DevManager is healthy and it is not a release result. The soak
uses only a temporary, process-unique worktree fixture. It never launches,
installs, restarts, or inspects an installed app; it never reads or hashes a
production profile, `config.json`, `remote.json`, or `session.json`.

## Immutable run input

`Invoke-ProcessSoak.ps1` accepts one manifest path and documented bounded
`-Iterations 100 -Seed 3403` overrides. The fixed Rust supervisor opens the
UTF-8 manifest once under a retained no-reparse root, enforces a maximum of 1
MiB, rejects unknown or missing fields, and hashes the exact bytes it read. The
manifest contains `schemaVersion`, `revision`, pinned `gitRevision` and
`buildId`, SHA-256 plus canonical paths for the supervisor, helper, and cycle
executables, an explicit minimal environment allowlist, an ANSI corpus
revision/hash, a temporary working directory, `seed`, bounded `iterations` (at
most 100), bounded suite/cycle/cleanup deadlines and stdout, stderr, and result
byte caps, and a finite `scenarioCatalog`. Every scenario is an argument array
beginning with the fixed `cycle` protocol and has an expected exit code. The
Rust supervisor repeats canonical identity and hash validation immediately
before launch; PowerShell never reopens or hashes the manifest. The retained
`manifest.json` artifact records the exact input hash and bounded protocol
fields without persisting absolute source paths.

## Fixed Rust supervisor

The PowerShell layer starts the fixed supervisor as a child with an argument
array and an explicit `SystemRoot`/`TEMP`/`TMP`/exact-tool-directory `PATH`
block. Its stdout and stderr pumps are bounded and deadline-limited. It never
dot-sources a callback, imports worktree code, uses `Start-Process`, polls with
sleeps, or terminates a raw PID. On Windows the Rust supervisor creates and
owns one Job Object per cycle, launches the exact cycle executable suspended
with the validated argument array and explicit environment block, assigns it to
the Job before resuming it, and records a live process creation time and
canonical executable path. It reads stdout and stderr concurrently under hard
caps and parses exactly one bounded JSON result. Per-cycle and suite deadlines
use monotonic clocks. On timeout, interruption, malformed/multiple/oversized
output, crash, nonzero exit, or residue it terminates the owned Job, joins both
readers, and independently proves `ACTIVE_PROCESS_ZERO` through the Job
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

Evidence is written by Rust only beneath the validated temporary root:

```text
.devmanager-next/evidence/phase-03-process-soak/runs/<unique-run>/
  manifest.json
  summary.json
  performance.json
  conformance.json
  run.json
```

The root is opened and retained with no-reparse semantics, and the unique run
directory is created exclusively. Each artifact is serialized to a
same-directory temporary file and atomically moved into its previously absent
final name. Existing files are never overwritten. `performance.json` contains
each cycle duration plus p50, p95, maximum, and count, together with raw child
CPU time, monotonic wall interval, logical processors, core-equivalent percent,
and whole-machine percent. `conformance.json` records the manifest
revision/hash, pinned build, ANSI corpus case hashes, scenario outcomes,
exact-one-JSON validation, reader caps, real identities, listener and handle
audits, cleanup deadline, and Job zero proof.

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

Both values are published from the raw measured interval; neither is replaced
with a formula-only estimate or silently capped. The evidence records the
logical-processor count and both values, not an ambiguous “CPU percent.”

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
