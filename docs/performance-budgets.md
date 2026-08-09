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

## Measurement rules

- Record the seed, iteration count, helper identities, host/client identities,
  and the exact lifecycle boundary for every run.
- Report p50, p95, maximum, and sample count for latency metrics. A missing or
  ambiguous timestamp is a failed measurement, not an interpolated value.
- Keep production baseline evidence separate from soak evidence. The soak must
  verify unchanged production `config.json` and `remote.json` hashes and must
  report any new process identity as orphan residue without killing it.
- Do not convert a provisional budget to a release claim until a real,
  repeatable host/client cycle API produces the evidence and the remaining
  Phase 3 process, terminal, port, and isolation gates agree with it.
