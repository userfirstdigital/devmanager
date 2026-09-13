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
| 4 | `npm --prefix web test -- --run` | Mandatory when `web/` is present. See [npm launch](#npm-launch) |
| 5 | `npm --prefix web run typecheck` | Mandatory when `web/` is present |
| 6 | `npm --prefix web run build` | Mandatory when `web/` is present |
| 7 | Fixture smokes | See [Safe smokes](#safe-smokes) |
| 8 | Production baseline / assert / residue | Capture before commands; assert in `finally` even after failure |

Every child uses argument arrays (no shell command strings), a fresh isolated environment copy, redirected stdout/stderr evidence logs, and a bounded timeout. On timeout the bounded child Job tree is the only cleanup owner. The gate continues to observe residue only and never kills leftover processes, including the installed app.

## Isolation

- `RepoRoot` must be this script's real Git worktree (`git rev-parse --show-toplevel` / `--is-inside-work-tree`).
- Per-run `CARGO_TARGET_DIR` is `<worktree>\.devmanager-next\target-final-release\<run-id>`. It must stay beneath the worktree, outside the production root, and outside `.scratch`.
- Evidence and logs stay under `<worktree>\.devmanager-next\evidence\final-release\<run-id>\`.
- Parent process environment is snapshotted and restored in `finally`.
- Each child copy clears `DEVMANAGER_PROFILE`, `DEVMANAGER_CONFIG_DIR`, `DEVMANAGER_APP_IDENTITY`, other `DEVMANAGER_*` identity variables, provider API-key variables, inherited Cargo target/flag/wrapper variables, and CI-unsafe Git variables. Secrets are never printed.
- Child `TEMP`/`TMP` are the run-unique evidence `tmp` directory. That root is observed, not used as a kill scope.
- No downloads and no network operations are performed by the gate itself.

## npm launch

`ProcessStartInfo` + `DevManagerPhaseGateJob` uses `CreateProcess` with `ArgumentList` (quoted per argument). A `.cmd` file is not a PE image, so `npm.cmd` cannot be `FileName`.

- Resolve `npm` preferring unique `npm.cmd` then `npm.exe`. Reject installed DevManager paths.
- If the resolved leaf is `npm.exe`, launch it directly with the npm argument array.
- If the resolved leaf is `npm.cmd` (or `.bat`), launch `C:\Windows\System32\cmd.exe` with ArgumentList `/d`, `/c`, the resolved `npm.cmd` path, then the original npm arguments. `/d` disables AutoRun. Each token stays a separate ArgumentList entry; the gate never concatenates a shell command string.
- `npm.exe` does not use `cmd.exe`.

## Safe smokes

Invoked only when present, fixture-only, and when a PowerShell AST parameter contract is discoverable and safe. The gate never passes `-Authenticated`, `-Provider`, `-HostRegistered`, `-IAcknowledgeIsolatedNonproductionProfile`, or any install/publish/kill/start/stop parameter, and never supplies credentials.

The gate's typed-contract predicate is an **explicit ID set**, not `Kind=smoke`.
Required typed JSON (`schemaVersion`, `status`/`disposition`, `pass`, `reason` on
stdout; PASS only when exit code is 0 **and** the typed token is PASS):

- `provider-smoke` and `provider-smoke:*`
- `browser-surface-proof` and `browser-surface-proof:*`
- `browser-provider-e2e` and `browser-provider-e2e:*`
- `prompt-smoke` and `prompt-smoke:*`

`workspace-smoke` is **not** in that set. Cargo check/test and web test/typecheck/build
are also non-typed and keep their exit-code contract.

| Smoke | Script | Contract |
| --- | --- | --- |
| Workspace | `Invoke-WorkspaceSmoke.ps1` | Fixture/fake-host only. Do not pass `-Authenticated`. Not a typed-contract smoke. The script emits explicit `WORKSPACE_SMOKE_OK` / `residue=0` markers; the gate classifies this ID by process exit code (0 → **PASS**, nonzero → **FAIL**) and must not treat that untyped marker output as a missing typed JSON failure. |
| Provider | `Invoke-ProviderSmoke.ps1` | Fixture/non-authenticated only. Pass isolated `-IsolatedProfile` when that parameter exists. **Typed JSON required.** `schemaVersion=1` with `disposition` (preferred) or `status`, parsed case-insensitively: `pass`/`passed`/`success` → **PASS**; `hold` → **HOLD**; `rejected`/`fail` → **FAIL**. `pass: true` without a conflicting token is PASS. Malformed JSON, unknown token, `pass: false` without a token, `disposition=pass` with `pass: false`, typed PASS with nonzero exit, or exit 0 with no typed JSON is FAIL. |
| Browser surface | `Invoke-BrowserSurfaceProof.ps1` | Invoke only if present and safe. Pass isolated `-OutputDir` `<run>/browser-surface-proof-output`. **Typed JSON required.** Portable/default runs set `visibleWebView2Proven=false` and must be typed **HOLD** — a successful portable Rust fixture run alone is never PASS. Typed PASS is allowed only when visible WebView2/GPUI proof was actually performed. Missing or unsafe script → HOLD. |
| Browser provider E2E | `Invoke-BrowserProviderE2E.ps1` | Invoke only if present and safe. Pass isolated `-OutputDir` `<run>/browser-provider-e2e-output`, `-Fixture`, and `-IncludeProjectionFixture`/`-IncludeRecovery` when those parameters exist. Never pass `-Authenticated`. **Typed JSON required.** Missing/unavailable fixture server → typed **HOLD**. Ready-line, health, index, traversal, or leftover fixture-process failures → typed **FAIL**. Authenticated provider launch remains HOLD and is never started. Missing or unsafe script → HOLD. |
| Prompt | `Invoke-PromptLibrarySmoke.ps1` or `Invoke-PromptSmoke.ps1` | Same discovery/safety rules. **Typed JSON required** (`prompt-smoke` and `prompt-smoke:*`). Missing → HOLD. |

If a command *does* emit typed JSON (`disposition`/`status`/`pass`), the gate classifies that token even for cargo/web/workspace. Typed HOLD does not become FAIL merely because the process exited 2. Typed-contract smokes listed above cannot PASS from exit 0 alone. Generic untyped smokes outside that set (today: workspace) and cargo/web use exit code: 0 → PASS, nonzero → FAIL.

Missing, unsafe, skipped, or typed-HOLD smokes make the overall result **HOLD** unless an explicit opt-in skip is already HOLD. They never become PASS.

## Production guard and residue

- Capture `config.json` and `remote.json` hashes plus installed DevManager PID/start time through `Capture-ProductionBaseline.ps1`.
- Never read or hash `session.json`. The running installed app may change that file.
- After every command, observe residue using Isolation/PhaseGate inventory fields (`processId`, `executablePath`, `creationDate`, `parentProcessId`) plus Win32 `CommandLine` when present.
- A process is attributed to this run only when it is **not** an installed DevManager executable and either:
  - its executable path is under this run's `CARGO_TARGET_DIR` or run directory (including `tmp`), or
  - its command line mentions those unique roots **and** the executable leaf is a helper (`cargo`, `rustc`, `rustdoc`, `clippy-driver`, `npm`, `node`, `cmd`, `pwsh`).
- Being associated with the isolated temp root is not enough. Worktree-wide command lines, installed DevManager, and unrelated processes are not attributed. The gate never kills residue; the bounded child Job owns timeout cleanup.
- Inspect residue again at the end.
- Call `Assert-ProductionUnchanged.ps1` in `finally` even after a command failure.
- The installed app and its user state must not be killed or modified.

## Forbidden operations

The gate must not invoke or perform:

- Phase 3 / final soak (`Invoke-ProcessSoak.ps1`, `Invoke-FinalSoak.ps1`, `Invoke-Phase3ProcessSupervisorGate.ps1`)
- Start/stop of native-next (`Start-NativeNext.ps1`, `Stop-NativeNext.ps1`)
- Install, publish, tag, or release
- Broad cleanup or killing leftover processes
- Killing the installed DevManager process
- Authenticated provider/browser API calls (fixtures only; no `-Authenticated`, no real credentials)

## Status and exit codes

| Overall status | Exit | Meaning |
| --- | --- | --- |
| `PASS` | 0 | All mandatory cargo/web commands exited 0, workspace-smoke exited 0 under its marker/exit contract, every typed-contract smoke returned typed PASS with exit 0, no attributed residue, production unchanged, evidence write succeeded |
| `PLAN` | 0 | `-PlanOnly` static validation succeeded. This is **not** a release PASS |
| `HOLD` | 2 | Missing/unsafe/typed-HOLD smoke (including browser surface without visible WebView2 proof, browser provider E2E with a missing fixture server, or authenticated provider HOLD), `-SkipWeb`, or `-SkipSmokes` |
| `FAIL` | 1 | Command failure, typed-contract smoke with missing/malformed typed JSON, typed rejection, typed PASS with nonzero exit, workspace/cargo/web nonzero exit, timeout, attributed residue, production change, baseline/assert failure, isolation breach, or evidence write failure |

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
