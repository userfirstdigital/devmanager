# Phase 3 performance budgets

These are objective, conservative **Provisional engineering budgets** for the
Phase 3 process/terminal work. They are guardrails for the soak harness, not
claims about current measured performance and not release evidence. Each value
must be replaced or confirmed with real host/client evidence from a completed
Phase 3 soak and the conformance corpus before it becomes a release gate; this
requires later real evidence, not an inferred pass from helper-only tests.
The PTY first-output/input acknowledgement pair is intentionally listed as two
separate measurable boundaries below.

| Measurement | Provisional budget | Boundary and evidence required |
| --- | ---: | --- |
| Test-helper close latency | ≤ 500 ms p95 | From the close request reaching the bounded helper until its natural exit; record helper identity, request time, exit time, and whether the close was cooperative or forced. |
| PTY first output | ≤ 500 ms p95 | From an admitted PTY launch until the first non-empty output byte is observed by the host; measure with the real host/client cycle, not the helper-only test. |
| PTY input acknowledgement | ≤ 250 ms p95 | From an admitted input sequence until the matching acknowledgement is observed; preserve the sequence and provider/session identity in sanitized evidence. |
| 10 MB output delivery | ≤ 5 s, zero loss | Deliver exactly 10 MiB through the managed PTY path, with no sequence gap, duplicate, or unbounded queue; record bytes admitted, physically delivered, and terminal settlement. |
| 100-cycle memory/handle growth (memory) | ≤ 16 MiB private-bytes delta | Compare a quiet baseline with the same host after 100 launch/write/resize/detach/reattach/close cycles; exclude Cargo, rustc, test harnesses, and unrelated processes. |
| 100-cycle handle growth | ≤ 32 handles delta | Compare the host's handle count at the same lifecycle point before and after 100 cycles; require the host and every managed Job to have settled before sampling. |

## Cycle evidence contract

The runner accepts only `cycleSchemaVersion=1` evidence. A completed result is
an exact object with these top-level fields: `schemaVersion`, `status`, `cycle`,
`seed`, `host`, `client`, `terminal`, `operations`, `managedRoot`,
`ownedProcessIdentities`, `resources`, and `timing`. Missing fields, unknown
fields, duplicate identities or operation evidence, wrong cycle/seed, partial
results, and inconsistent deltas are failures. A bare object containing only
`status=completed` is never a cycle result.

The host, client, and managed-root identities each carry the exact
`processId`, fully qualified `executablePath`, and `creationDate`; host/client
generations and terminal generation must agree. `operations` must prove launch,
first output, input acknowledgement, and close settlement with either a unique
operation ID or a marker plus timestamp. The managed root must report an
authoritative Job member count of zero. Each cycle must report exact helper,
provider, and host-child identities, observed listener/named-pipe/PTY/Job
resources, empty owned/leaked residue, internally consistent resource deltas,
and bounded `launchMs`, `firstOutputMs`, `inputAckMs`, `closeSettlementMs`, and
`totalMs` values.

The runner checks every emitted exact process identity against the live process
identity (PID, executable path, and creation time) after settlement and repeats
the check at final settlement. It does not infer orphan freedom from an
executable inventory. Persisted evidence is sanitized to process IDs,
executable leaves, UTC start times, bounded safe identifiers, and redacted
errors; raw command lines, paths, secrets, and extension output are not
persisted.

The production baseline is captured before any optional cycle extension is
loaded. Its `config.json` and `remote.json` hashes plus installed DevManager
PID/start identity must remain unchanged after load, during cycles, and at
finalization. Until the real typed host/client cycle API defines
`Invoke-DevManagerProcessSoakCycle`, the 100-cycle command is intentionally
`UNAVAILABLE` (exit code 78) before iterations; helper-only fixtures must not
turn that status into a pass.

## Measurement rules

- Record the seed, iteration count, helper identities, host/client identities,
  and the exact lifecycle boundary for every run.
- Report p50, p95, maximum, and sample count for latency metrics. A missing or
  ambiguous timestamp is a failed measurement, not an interpolated value.
- Keep production baseline evidence separate from soak evidence. The soak must
  verify unchanged production `config.json` and `remote.json` hashes and must
  settle every exact emitted identity/resource at the Job boundary without
  claiming that an executable-inventory difference is an orphan.
- Do not convert a provisional budget to a release claim until a real,
  repeatable host/client cycle API produces the evidence and the remaining
  Phase 3 process, terminal, port, and isolation gates agree with it.
