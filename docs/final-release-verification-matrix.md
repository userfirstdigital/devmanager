# Final release verification matrix

Operator matrix for `scripts/native-next/Invoke-FinalReleaseGate.ps1`. This gate proves isolated compile/test/fixture readiness in one worktree. It does **not** replace platform packaging, signing, update metadata, disposable-VM install matrices, or public release publication.

Long Rust verification creates expected Cargo, rustc, and test-harness executables under the run-unique `CARGO_TARGET_DIR`. Warn the operator before a full run. The gate must prove those processes are gone afterward and must never kill or modify the installed DevManager app or its user state.

## Ownership

| Item | Owner |
| --- | --- |
| Gate script | `scripts/native-next/Invoke-FinalReleaseGate.ps1` |
| Isolation / evidence / production guards | `scripts/native-next/Isolation.ps1`, `Capture-ProductionBaseline.ps1`, `Assert-ProductionUnchanged.ps1` |
| Bounded child process tree | `scripts/native-next/PhaseGate.ps1` (`DevManagerPhaseGateJob`) |
| Worktree / profile / target isolation | This worktree only; unique `CARGO_TARGET_DIR`; no production `DEVMANAGER_PROFILE` |
| Evidence | `.devmanager-next/evidence/final-release/<run-id>/` |
| Public packaging / signing / publish / tag | Existing release workflow and `docs/release-checklist.md` — **out of scope** |

## Sequence

| Step | Command | Notes |
| --- | --- | --- |
| 1 | `cargo check --locked --lib --bins --tests` | Isolated target; fail closed on nonzero |
| 2 | `cargo test --locked --lib -- --test-threads=1` | Serial unit tests. Inherited `DEVMANAGER_PROFILE` is cleared; tests own process-unique profiles |
| 3 | `cargo test --locked --tests -- --test-threads=1` | Serial integration tests |
| 4 | `npm --prefix web test -- --run` | Mandatory when `web/` is present |
| 5 | `npm --prefix web run typecheck` | Mandatory when `web/` is present |
| 6 | `npm --prefix web run build` | Mandatory when `web/` is present |
| 7 | Fixture smokes | See [Safe smokes](#safe-smokes) |
| 8 | Production baseline / assert / residue | Capture before commands; assert in `finally` even after failure |

Every child uses argument arrays (no shell command strings), a fresh isolated environment copy, redirected stdout/stderr evidence logs, and a bounded timeout. On timeout the gate kills only its owned Job tree. Pre-existing processes, including the installed app, are never touched.

## Isolation

- `RepoRoot` must be this script's real Git worktree (`git rev-parse --show-toplevel` / `--is-inside-work-tree`).
- Per-run `CARGO_TARGET_DIR` is `<worktree>\.devmanager-next\target-final-release\<run-id>`. It must stay beneath the worktree, outside the production root, and outside `.scratch`.
- Evidence and logs stay under `<worktree>\.devmanager-next\evidence\final-release\<run-id>\`.
- Parent process environment is snapshotted and restored in `finally`.
- Each child copy clears `DEVMANAGER_PROFILE`, `DEVMANAGER_CONFIG_DIR`, `DEVMANAGER_APP_IDENTITY`, other `DEVMANAGER_*` identity variables, provider API-key variables, inherited Cargo target/flag/wrapper variables, and CI-unsafe Git variables. Secrets are never printed.
- No downloads and no network operations are performed by the gate itself.

## Safe smokes

Invoked only when present, fixture-only, and when a PowerShell AST parameter contract is discoverable and safe:

| Smoke | Script | Contract |
| --- | --- | --- |
| Workspace | `Invoke-WorkspaceSmoke.ps1` | Fixture/fake-host only. Do not pass `-Authenticated`. |
| Provider | `Invoke-ProviderSmoke.ps1` | Fixture/non-authenticated only. A typed `HOLD` (JSON `disposition=hold`) is an explicit dependency result, **not** PASS. Unexpected success is FAIL. |
| Browser | `Invoke-BrowserSmoke.ps1` or `Invoke-BrowserFixtureSmoke.ps1` | Invoke only if the file exists and its parameter contract is discoverable and safe. Missing → HOLD. |
| Prompt | `Invoke-PromptLibrarySmoke.ps1` or `Invoke-PromptSmoke.ps1` | Same as browser. Missing → HOLD. |

Missing, unsafe, skipped, or typed-HOLD smokes make the overall result **HOLD** unless an explicit opt-in skip is already HOLD. They never become PASS.

## Production guard and residue

- Capture `config.json` and `remote.json` hashes plus installed DevManager PID/start time through `Capture-ProductionBaseline.ps1`.
- Never read or hash `session.json`. The running installed app may change that file.
- After every command, assert no newly attributed gate-owned process remains (executable under this run's `CARGO_TARGET_DIR` or run directory).
- Inspect residue again at the end.
- Call `Assert-ProductionUnchanged.ps1` in `finally` even after a command failure.
- The installed app and its user state must not be killed or modified.

## Forbidden operations

The gate must not invoke or perform:

- Phase 3 / final soak (`Invoke-ProcessSoak.ps1`, `Invoke-FinalSoak.ps1`, `Invoke-Phase3ProcessSupervisorGate.ps1`)
- Start/stop of native-next (`Start-NativeNext.ps1`, `Stop-NativeNext.ps1`)
- Install, publish, tag, or release
- Broad cleanup
- Killing the installed DevManager process
- Unauthenticated provider API calls (fixtures only; no `-Authenticated`)

## Status and exit codes

| Overall status | Exit | Meaning |
| --- | --- | --- |
| `PASS` | 0 | All mandatory commands exited 0, web present and successful, every available safe smoke successful, no residue, production unchanged, evidence write succeeded |
| `PLAN` | 0 | `-PlanOnly` static validation succeeded. This is **not** a release PASS |
| `HOLD` | 2 | Missing/unsafe/typed-HOLD smoke, `-SkipWeb`, or `-SkipSmokes` |
| `FAIL` | 1 | Command failure, timeout, residue, production change, baseline/assert failure, isolation breach, or evidence write failure |

`-SkipWeb` and `-SkipSmokes` are explicit opt-in only and cannot yield PASS.

## Operator commands

Plan-only (static validation, no cargo/npm/smoke/baseline execution):

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-FinalReleaseGate.ps1 -PlanOnly
```

Real gate (long Rust run; warn the operator first):

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-FinalReleaseGate.ps1
```

Optional documented escapes (overall HOLD, never PASS):

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-FinalReleaseGate.ps1 -SkipWeb
pwsh -NoProfile -File scripts/native-next/Invoke-FinalReleaseGate.ps1 -SkipSmokes
```

Evidence is written to `.devmanager-next/evidence/final-release/<run-id>/plan.json` (plan-only) or `verification.json` (real run), including each command, exit code, duration, status, isolation roots, residue observations, baseline/assert result, and the final status.
