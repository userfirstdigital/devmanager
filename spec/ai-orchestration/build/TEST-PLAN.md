Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# End-to-end test plan: autonomous software goals

Run status: NOT RUN — generated 2026-09-12. Journeys passed/failed: not measured. Budgets: not measured. Update this paragraph with the actual run date, input/build versions, passed/failed/not-verifiable journey IDs and evidence location after execution. Do not turn the document-generation checks into product acceptance.

## Setup

The system under test is the rebuilt **native GPUI DevManager plus its sibling host**, with actual qualified provider processes and browser/tool access. HTML design targets are comparison inputs. A browser runner cannot prove GPUI interaction; native journeys require Computer Use against the actual rendered app. Run from an owned isolated checkout, obey AGENTS.md, and use the matching client/host/Connect protocol-v2 artifacts produced by phase 01. The test executor is a fresh verifier; builder claims and TRACKING ticks are not evidence.

Use these environment **names**, never literal credentials in this plan or model context:

| Name | Meaning |
| --- | --- |
| `DM_ACCEPTANCE_WORKTREE` | Absolute isolated DevManager source worktree with the implementation under test. |
| `DM_ACCEPTANCE_RUN` | Absolute newly owned evidence/temp root outside the daily checkout. |
| `DM_ACCEPTANCE_WORKSPACE` | Absolute disposable target-project root to create for the fixture corpus; distinct from source checkout and installed profile. |
| `DM_ACCEPTANCE_USER_PASSWORD`, `DM_ACCEPTANCE_OPERATOR_PASSWORD` | Disposable fixture personas, entered through the existing native Secret handoff. Values never printed or passed in goal text. |
| `DM_ACCEPTANCE_COMMAND_ROOT` | Absolute authorized isolated Command workspace with its real independent repo topology. |
| `DM_ACCEPTANCE_COMMAND_FEATURE_DIR` | Existing finished authorized feature handoff within that Command workspace; determines the real feature journey and its actual scope. It is not permission to invent a Command feature. |
| `DM_ACCEPTANCE_COMMAND_MAINTENANCE` | Existing authorized Command maintenance outcome/problem, with its required acceptance and delivery boundary. |
| `DM_ACCEPTANCE_COMMAND_URL` | Actual permitted local production-build route for the Command speed journey. |
| `DM_ACCEPTANCE_DEV_MANAGER_HANDOFF` | Existing authorized DevManager feature/maintenance handoff in an isolated workspace, used for self-work acceptance. |
| `DM_ACCEPTANCE_USER_CREDENTIAL_REF`, `DM_ACCEPTANCE_OPERATOR_CREDENTIAL_REF` | Existing secure credential-reference names for real project personas; resolve with current authorization, never copy production passwords into test files. |

The fixture has a client user and an operator, and starts with a deterministic seeded dataset. Additional empty/populated/error variants are explicit in its manifest. Real project credentials, datasets and database identities come from that project's current verified Context Pack and secure bindings. Missing a real handoff/persona/tool leaves the affected live corpus Not verified, without blocking unrelated implementation. These are runtime acceptance inputs, not open product design choices.

### Isolated build and logs

The following commands are definitions verified from current source/CI, **not commands run during generation**. Set the three absolute nonsecret paths above to owned locations before executing. On Linux, prepare the active isolated worktree and one private target as follows; keep one quiet compiler owner and allow a cold build at least 600 seconds. Commands may yield while that exact process continues; join it before retrying.

```bash
cd "$DM_ACCEPTANCE_WORKTREE"
python3 - <<'PY'
import os, pathlib, subprocess
root = pathlib.Path.cwd().resolve()
git_env = dict(os.environ, GIT_OPTIONAL_LOCKS='0')
def git(*args):
    return pathlib.Path(subprocess.check_output(['git', 'rev-parse', '--path-format=absolute', *args], env=git_env, text=True).strip()).resolve()
assert git('--git-dir') != git('--git-common-dir'), 'Use an isolated linked worktree'
run = pathlib.Path(os.environ['DM_ACCEPTANCE_RUN']).resolve()
assert run != root and root not in run.parents, 'Evidence/temp root must be outside the source worktree'
assert 'DEVMANAGER_PROFILE' not in os.environ, 'The full suite owns its profile variable'
target = (root / 'target').resolve()
assert root in target.parents, 'Target escaped the active worktree'
assert not os.environ.get('CARGO_TARGET_DIR') or pathlib.Path(os.environ['CARGO_TARGET_DIR']).resolve() == target
print('Isolated source:', root)
print('Resolved CARGO_TARGET_DIR:', target)
(run / 'logs').mkdir(parents=True, exist_ok=True)
(run / 'tmp').mkdir(parents=True, exist_ok=True)
PY
export CARGO_TARGET_DIR="$DM_ACCEPTANCE_WORKTREE/target"
export RUNNER_TEMP="$DM_ACCEPTANCE_RUN/tmp"
python3 packaging/prepare-ci-checkout.py
export TMPDIR="$DM_ACCEPTANCE_RUN/tmp"
cargo fmt --all -- --check >"$DM_ACCEPTANCE_RUN/logs/rustfmt.log" 2>&1
cargo check --locked --lib --bins --tests >"$DM_ACCEPTANCE_RUN/logs/compiler.log" 2>&1
cargo build --locked --bin devmanager-process-test-helper >"$DM_ACCEPTANCE_RUN/logs/process-helper-build.log" 2>&1
cargo build --locked --bin devmanager --bin devmanager-host >"$DM_ACCEPTANCE_RUN/logs/app-build.log" 2>&1
```

Run each command with its exit status captured; **stop dependent steps on failure**, rather than assuming the next shell line implies success. `prepare-ci-checkout.py` restores manifest-verified WASM inputs; its environment changes do not propagate to the parent shell outside GitHub Actions, hence the explicit exports. No installed application or production profile is launched. Before process/persistence verification record production `config.json`/`remote.json` hashes and installed PID/start time using the current platform isolation procedure; compare afterward, treating `session.json` separately. Never print file contents or kill unrelated processes.

For the required Windows lane use the current [final verification matrix](../../../docs/final-release-verification-matrix.md), its isolation/baseline/Capture/Assert steps and `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1 -Once` only within the owned worktree. Keep its exact private target/temporary-root and process ownership rules. Windows headless GPUI cases need one Application lifetime per harness process as stated in AGENTS.md. Do not treat a Linux run as Windows acceptance.

### Acceptance fixture command contract

**Phase 08 creates** `scripts/goal-acceptance.py` and owned fixture templates; phase 09 adds measurement fixtures, phase 10 adds the 300-test/inventory fault corpus, and phase 17 adds corpus/evidence reconciliation. This Python-standard-library utility is test infrastructure, not a second product server or workflow engine. These exact commands are **planned deliverables**, unavailable before those phases. They only manage their own fixture processes/files and record every owned PID/start identity.

```bash
python3 scripts/goal-acceptance.py seed --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
python3 scripts/goal-acceptance.py start --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
python3 scripts/goal-acceptance.py status --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
```

`seed` refuses a nonempty unowned directory, makes independent `api` and `web` Git repositories plus a root agreement directory, and writes `fixture-manifest.json`. Fixtures contain actual editable source, executable Python `unittest` checks, and a served HTML/HTTP user/operator app. The finished feature handoff specifies an export request/status/download flow with operator visibility; the empty-project handoff specifies a small private task list with add/complete/persist behavior. These are synthetic acceptance projects, explicitly separate from Command. The speed fixture includes a controlled sequential-request cost; the test campaign has exactly 300 distinct baseline failing identities in three demonstrated shared causes. Fault variants cover a hidden server warning, OOM/no-summary output, coverage holes, discovery exclusion, substituted identities, seeded flake and delayed/uncertain effect. Fault controls live in verifier-owned fixture state, outside builder allowed paths.

`start` binds each needed service to `127.0.0.1` on an OS-assigned free port, records actual URLs, launches argv arrays without shell evaluation, and redirects each service stdout/stderr to **`$DM_ACCEPTANCE_RUN/logs/fixture-api.log`** and **`fixture-web.log`**. It writes `owned-processes.json` with exact PID/start-time/workspace/argv/log-path identities. It never changes a shared database. `status` exits 0 only when those exact services answer their readiness checks; it returns a JSON object `{schema_version:1,ready:true,services:[{name,url,pid,start_identity,log_path}],fixture_manifest_sha256}`. Missing readiness or identity mismatch exits nonzero and keeps ownership information.

The manifest records the finished-spec path, brief path, speed route, test invocation (`python3 -m unittest discover -s tests -v` in the seeded campaign repo), persona environment names, dataset versions, each injected condition and exact source hashes. Use those actual paths/URLs in the journey evidence. No hardcoded port or hidden credentials. Seeding never installs providers, changes model settings or modifies the real Command workspace.

Launch the built debug native application against the owned fixture workspace:

```bash
"$CARGO_TARGET_DIR/debug/devmanager" --dev-workspace "$DM_ACCEPTANCE_WORKSPACE" >"$DM_ACCEPTANCE_RUN/logs/native-app.log" 2>&1 &
DM_NATIVE_PID=$!
export DM_NATIVE_PID
python3 scripts/goal-acceptance.py adopt-native --pid "$DM_NATIVE_PID" --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
```

`adopt-native` is part of the phase-08 fixture utility: validate executable/argv/creation time and workspace against the recorded build, record ownership, and discover only its profile-owned sibling host. Reject a mismatching PID without stopping it. Existing native bootstrap writes **`host-stderr.log`** and **`client-startup.log`** beneath:

`$DM_ACCEPTANCE_WORKSPACE/.devmanager-next/dev-profile/com.userfirst.devmanager-<profile>/logs/`

The existing `<profile>` is `native-next-` plus the first 16 hex characters of SHA-256 over UTF-8 `native-next`, one NUL byte and the canonical workspace path. Resolve it using the app's own profile calculation in the utility; validate the actual directory/lock identity rather than searching unrelated profiles. Record these exact log paths in `run-manifest.json`. Goal-owned provider/tool/service logs and original output remain referenced by their host receipts; write their safe evidence exports beneath `$DM_ACCEPTANCE_RUN/receipts/` and record missing coverage. Do not redirect private auth traces into shared logs.

Wait for startup/idle to settle, record baseline log sizes and noise, then measure process→window and window→first task row separately from `client-startup.log` and actual native observation. The preview-only inbox may appear early but cannot enable mutations before canonical synchronization. Qualify configured Claude and Codex profiles through the phase-02 real handshake/tool/input checks; do not make new billing/account choices merely to complete testing.

### Real Command setup

Refresh the actual Command Context Pack/import chain inside the authorized isolated root. Current source definitions rechecked for this plan include API `npm run dev` (port defaults to 8080), staff-web `npm run build` and `npm start` (production static server with `PORT` override), API `test:unit`/`test:ci` and web `test:run`. Do not use their watch-mode `npm test` as a completed test receipt. The static server binds `0.0.0.0` in current code; apply the project's actual network-isolation policy when preparing its owned environment.

Use its existing dev-safe environment/credential bindings with an actual owned database; no migration/reset is authorized merely by launching these commands. Build the local production assets with the verified API URL and retain all current build gates:

```bash
cd "$DM_ACCEPTANCE_COMMAND_ROOT/web"
VITE_API_BASE_URL=http://127.0.0.1:8080 npm run build >"$DM_ACCEPTANCE_RUN/logs/command-web-build.log" 2>&1
cd "$DM_ACCEPTANCE_COMMAND_ROOT/api"
PORT=8080 npm run dev >"$DM_ACCEPTANCE_RUN/logs/command-api.log" 2>&1 &
DM_COMMAND_API_PID=$!
cd "$DM_ACCEPTANCE_COMMAND_ROOT/web"
PORT=8081 npm start >"$DM_ACCEPTANCE_RUN/logs/command-web.log" 2>&1 &
DM_COMMAND_WEB_PID=$!
```

Reserve/check those ports through the phase-13 resource owner before launch; if occupied, lease available ports and regenerate the build with the exact API origin before measurement. Pin the actual web URL into `DM_ACCEPTANCE_COMMAND_URL` and the run record. Record both wrapper/descendant trees; a watcher PID is not the whole service. For a real feature touching portal/agent/service/updater/extension, start and capture each additional service using its freshly verified per-repo Context Pack command. Its actual command/log path must appear in the run manifest before the feature journey starts; an unavailable surface remains Not verified. Command's Chrome MCP/cold-production/300 ms contract below is authoritative for its speed journey.

## Journeys

Perform in order, with independent work explicitly allowed where the goal graph permits. These journeys cover cross-phase integration; per-phase acceptance remains the detailed scenario/check list. Resolve every visible label to actual current controls and pin routes/TaskIds/GoalKeys/artifact references into the run record and this plan's run notes. Never rely on brittle CSS selectors or invented native routes.

| ID / persona | Screen → action → expected observable result | Required evidence |
| --- | --- | --- |
| J01 — Developer | Existing board → New → New manual task → run a normal provider task, use Terminal/Files/Changes, return to board → existing manual workflow and focus still work. Then New → New goal → hand over the fixture's finished DISCOVERY spec with ready-to-deploy boundary → one Preparing/working goal, no LOCKED or workflow-type question. | Actual native recording, start receipt, source version, manual/task IDs; H/I/U. |
| J02 — Developer | Same goal → resend a lost start receipt/reconnect → attach to one owning run. Open Plan → overview, every phase and TEST-PLAN → complete documents and exact contracts, no NEXT. Repeat as docs-only → final documents delivered with no builder process. | Source/publication lineage, full generated set, exactly one start/ownership; H/G. |
| J03 — Client + operator | Goal conversation/Agents → watch actual cross-repo build with an independent permitted worker → open user export screen, request/download export, inspect same operation as operator → both surfaces work and the lead runs fresh audit/journeys, fixes seeded defects and re-verifies without a human handoff. | Real provider identities, source/diffs, current independent receipts, browser/server logs, fixture output; I/R/A/E/C. |
| J04 — Developer | New goal → submit the empty-project task-list brief, then inspect Plan → lead asks only unresolved product policy, settles ordinary technical calls, creates buildable milestones and implements add/complete/persist. Repeat with a localized diagnose-only problem → bounded answer and no code changes. | Canonical GOAL.md, decisions, new project source/test/build receipts, actual UI behavior; N/G. |
| J05 — Developer | Needs you card → inspect consequence and current nonsecret authority → choose and explicitly submit answer → Sending, accepted answer and resumed dependent work. During a worker turn send “keep migrations manual” → accepted/delivered/applied states stay distinct. Supersede the scope and answer old card → visible supersession, no old effect. | Decision/action/account/version/message fences; REF-DECISION/STEERING/EFFECTIVE-ACCESS; Q/U. |
| J06 — Developer | Plan → inspect source → decision → requirement → work → check; restore an earlier agreement document under the existing amendment authority → new version, coherent downstream update and stale affected checks, preserved independent work. Remove source access → explicit waiting/missing source. | Exact viewer links/versions, CAS/publication receipts, invalidated check IDs; G/N/U19. |
| J07 — Developer | Agents → inspect profile/tool/effective access → force read-only refusal, a scoped rate limit and revoked connection → lead chooses only an eligible already permitted profile or shows precise wait/decision, retaining draft, budget and effect identity. Secure sign-in uses native Secret handoff; takeover/return uses the exact browser lease. | Real qualified profiles, restrictions/intersection, registration/input/cleanup receipts; H/R/Q/V. |
| J08 — Developer + fresh auditor | Checks → open seeded missing behavior, hollow test and false scanner-green → exact required finding and Not verified coverage despite exit 0. Observe fixer claim, fresh re-audit and evidenced dispute → only independent current proof closes items. Open original output with secrets/control characters → safe inert/redacted text, origin retained after summary. | Full input/check/coverage records, actual test inventories, fresh-context provenance, safe viewer capture; A/E/U20. |
| J09 — Developer | Checks/cause detail → exercise two completed failed repairs → fresh independent diagnosis → two materially different repairs → finite precise blocker after four failures while independent work continues. Crash verification before summary → incomplete with unchanged completed-repair count. Keep a productive compile beyond checkpoint → one owned process, no duplicate. | Attempt/cause lineage, real completed/incomplete receipts, exact process/activity evidence; A/D. |
| J10 — Client + operator | Browser → execute user and operator journey while tracking each service's new log lines → hidden WARN/ERROR with successful-looking UI remains a finding. Exercise empty/loading/error/partial failure; missing browser/native access remains Not verified. Set up/migrate/reset owned disposable DB, then retarget shared DB → latter refuses before effect. | Per-action log offsets, baseline comparison, console/network/timings, exact setup/target/cleanup receipts; E/C. |
| J11 — Developer | New goal → “this page is too slow” with fixture URL → lead reproduces, pins target/conditions, repairs highest recoverable cause, repeats measurements independently and verifies functional regressions. Repeat on authorized Command URL with its actual Chrome cold contract. Change conditions or move cost elsewhere → no misleading pass. | Complete samples/waterfall, exact data/tool/build conditions, independent final result; P/J03/REF-SPEED. |
| J12 — Developer | New goal → repair all 300 failing fixture tests → Checks shows original inventory, cause groups, verified resolved identities and new failures. Inject identical failure-count substitution, exclusion/skip, flake and OOM → integrity/incomplete results remain visible. Close only after complete independent final reconciliation. | Actual executed 300-case baseline, identity sets, cause histories, seed/order/repeats and full final summary; T/E/REF-TESTS. |
| J13 — Developer | Knowledge → inspect canonical Command project and user sources → correct/retire one scoped entry → source and a fresh lead's actual retrieval change. Evict view, change procedure, deny unrelated project access → exact-version recovery or explicit unavailable, mandatory rules retained. | Canonical file diffs, source/reader scope, actual new handoff/view, negative findings; M/L09–10. |
| J14 — Developer + independent reviewer | Knowledge/Axe → consolidate repeated same-incident reports, independent incidents, counterexamples and old rationale → one coherent scoped revision or valid no-change. Make concurrent edit → CAS reconciliation; observe later regression → justified retirement. Universal promotion waits for independent review and a second project. | Incident identities, full owning-section diff, review/publication/later-validation state, once-per-delivery proof; L/REF-KNOWLEDGE-STATES. |
| J15 — Developer | Board → run two Command goals with overlap plus DevManager goal → at most two active goals/two top-level runtimes each/four total, truthful resource waits and fair third admission. Switch drafts/details/questions; pause one, answer another, change priority → exact scope. | Resource/queue/assignment events, actual process inventory, native context/focus, separate lead/memory inputs; V/W. |
| J16 — Developer | Same run during active work → close/reopen client, pause/resume, interrupt provider, restart host, cancel during in-flight effect → same durable goal with correct exact-resume or explicit replacement, retained work and no duplicate effects. Force success-before-checkpoint and unqueryable effect → reconcile or show uncertainty. Stop → Stopping until exact trees/registrations/outbox settle. | Complete ordered journal replay, nested invocation occurrence, prior-effect identity, physical delivery cursors, scoped cleanup; D/G08. |
| J17 — Developer | Final conversation/Checks → inspect verified implementation and fresh review while second landing is pending → delivery remains incomplete. Settle final effect under unchanged input fence → actual delivered/ready-to-deploy result, per-repo revisions, manual actions and Axe disposition. Change shared UI/source/env inputs before close → affected proof becomes stale. Follow up → new run retains previous result. | Three obligation classes, final fence, actual repo/effect/cleanup/outcome receipts; C/REF-DELIVERY-PENDING/DONE. |
| J18 — Developer, client, operator | Run the configured real Command maintenance and finished feature handoffs, plus authorized DevManager self-work → complete actual applicable surfaces, target project rules/memory remain scoped, active installed manager stays unchanged until verified activation boundary. | Real handoff/authority/current topology, actual required service/native/user outcomes, per-repo evidence, production isolation comparison; J01–04/V09. |
| J19 — Independent native verifier | Repeat the full native path from New goal through real decision/recovery/inspection/fix/delivery at desktop and narrow geometry → current full-shell references match materially and actual controls work. Physical key text, control-key edit, drag-copy, wheel-scroll/restore work in actual provider terminal. | Every phase Pxx-UI, current paired captures, independent discrepancy disposition, actual Computer Use; all U scenarios. |
| J20 — Independent evaluator | Execute equivalent standard Claude/Codex baseline sessions and DevManager-led corpus with the same handoffs/tools/memory/rules/data/budgets/boundaries → compare verified outcomes and human coordination, retaining failed trials. | Complete corpus manifest, timings/usage coverage/cost unknowns, interventions, false-completion/duplicate-effect counts and actual outcomes; E12/phase17. |

## Non-visual surfaces and exact observations

New IPC/CLI/MCP shapes are fixed in the overview and delivered by their named phases. Use these safe read-only CLI entries after the matching host is running:

```bash
"$CARGO_TARGET_DIR/debug/devmanager-host" ctl actions --json
"$CARGO_TARGET_DIR/debug/devmanager-host" ctl status --profile "$DM_NATIVE_PROFILE" --json
"$CARGO_TARGET_DIR/debug/devmanager-host" ctl tasks --profile "$DM_NATIVE_PROFILE" --json
"$CARGO_TARGET_DIR/debug/devmanager-host" ctl goals --profile "$DM_NATIVE_PROFILE" --json
"$CARGO_TARGET_DIR/debug/devmanager-host" ctl goal-show --profile "$DM_NATIVE_PROFILE" --task "$DM_GOAL_TASK_ID" --section checks --json
```

`DM_NATIVE_PROFILE` is the exact validated workspace-bound name in the run manifest; `DM_GOAL_TASK_ID` is the host-issued Task identity, not a displayed title. First three commands already exist; last two are phase-01 contracts. Expect a JSON list/catalogue/current page with exact current revision/high-water, or a typed explicit error. A v1 client fails handshake explicitly before goal events. Do not convert binary MessagePack cursors to guessed text or reuse another snapshot's cursor.

For mutations use existing `ctl invoke --profile <name> --action <action-id> --arguments-json <json> --expected-task-revision <revision> --json` and the exact overview payload. Tests construct argv as an array with nonsecret JSON; shell interpolation is not JSON escaping. Create omits expected Task revision, existing goal mutations require it plus current GoalFence. Assert accepted operation/command identity and later actual terminal facts separately. Stale answers, forged actor/registration, foreign source root, wrong account, mismatched epoch/generation, tampered receipt and stale projection must refuse before effects. MCP tests hit the real authenticated loopback `/goal/mcp` transport from the assigned runtime and assert its tools' current scoped receipt/error; no raw provider lifecycle trust is inferred.

The acceptance fixture's HTTP contract is intentionally small and test-only: `GET /health` → 200 `{ "ready": true }`; `GET /api/exports` → authenticated current-user array `{ "items": [{"id": "<id>", "state": "queued|ready|failed"}] }`; `POST /api/exports` with `{}` → 202 `{ "id": "<id>", "state": "queued" }`; `GET /api/exports/<id>` → authorized 200 `{ "id": "<id>", "state": "ready", "download_url": "/api/exports/<id>/download" }` once complete; download → authorized 200 CSV with expected seeded rows; `GET /api/operator/exports` → operator-only 200 status/error records, client persona 403. Wrong-owner reads return 404 and unauthenticated calls 401. The synthetic finished SPEC fixes these exact contracts and an in-progress/error state; implementation workers build against them. Real Command API calls come from its current handed-off agreement, not from this test fixture.

Database/process/journal assertions use current typed production store APIs and disposable fault fixtures; never mutate the installed kernel database. Check exact lineage before reading a projection as a resume cursor. Compare full validated live/rebuilt projections after the two-input/delivery/cleanup sequence and nested occurrence replay. Inject journal duplicates/holes/foreign predecessors in an isolated copy; execution must fail closed. Test actual unknown-effect read-back without a fabricated successful checkpoint. Independently verify all receipt producers, complete coverage lists and original output retention; a short viewer page is not all collected evidence.

Every failure is retained with journey/scenario, scope, current input, output/log range and root-cause evidence when available. Missing root cause does not erase a reproduced failure. Unexpected logs/pre-existing failures outside this goal are merged into the project's tracker after recurrence checks; they cannot silently enter or leave required acceptance.

## Sweeps on every visited surface

- Native: populated/empty/preparing/loading/error/partial/paused/recovering/blocked/superseded/delivered states as relevant; current Task identity, retained drafts, keyboard focus, label readability, narrow scrolling and close/return path. Preview-only state cannot enable mutations, stale requests cannot erase unrelated projections, and automatic refresh queries only the active detail.
- Browser: console errors/warnings/unhandled rejections, failed assets, 4xx/5xx/hanging requests, response/body correctness, empty/loading/error/partial states and required user/operator permissions. A visible success with a server warning remains separately highlighted.
- Logs: record end offsets before each action and inspect only newly appended bytes afterward, for every service. Retain startup/idle baseline and untouched-surface comparisons to distinguish caused from pre-existing issues.
- Evidence: actual command completion and complete applicable coverage, current source/tool/env/data/visual versions, independent verifier context, inert/redacted text and preserved origin. Missing or contradictory proof is Not verified.
- Shared behavior: no global process kill, no overlapping writer, no duplicate side effect, no reset of budget/retry/cause history, no user approval inferred from recommendation/silence/evidence text.

## Performance budgets

Generic warm API/requested-data visibility: **500 ms p95**, fixed realistic data/auth/cache/network/CPU/build conditions, **3 warmups + 20 measured samples** for each baseline and after round, and a fresh independent final round with the same 3+20 policy. Retain all samples; nearest-rank p95 of 20 is sorted sample 19. Startup is separate: report **3 individual cold starts**, including process→window and window→first task row for the native app, without claiming a p95 from three samples. Use an explicitly agreed operation-specific budget for bulk/report/upload work; do not pretend 500 ms applies to first compilation or a large transfer.

Command's current canonical speed playbook overrides that default: **cold load below 300 ms**, local production assets, empty HTTP/module cache, valid auth, **Chrome MCP**, **10 Mbps down / 40 ms RTT / 4× CPU**, **5 navigations per round**, median and spread. Its actual measuring/saturation/authority rules apply. A Playwright design render or Vite dev page cannot validate that contract. If remaining levers require product changes, retain a measured FLOOR and the exact decision; an unmet requested budget never becomes a success.

Normal warm goal list/detail queries and useful native update should stay within the generic 500 ms budget with the default concurrent corpus and 300-case test campaign. Measure while provider restores/compilers are still active, not only after all work finishes. Process launch/first model response/bulk evidence export has its own observed duration and is not misreported as a fast data-query result. Warm UI should remain interactive with long output and narrow details; record latency regressions caused by over-refresh or unbounded rendering.

## Unobservable and unavailable behavior

No agreed behavior is intentionally assigned an unobservable pass. Required sources, missing browser/native tooling, unavailable service logs, uncertain external effects, opaque native-child counts, unknown spend or absent real-project acceptance inputs are recorded explicitly with the smallest missing prerequisite. The product may still complete independent work, but that specific required check remains Not verified. The verifier must not invent instrumentation output or manually tick it green. A future production business metric outside the agreed stopping boundary is labelled unmeasured/out of scope; an immediate measurable outcome inside the boundary still blocks completion.

## Final reconciliation and cleanup

Run the full required compiler/build and serial library/integration gates from the Context Pack once against the final current inputs, with exact completed failure identities and resource ownership. New tests, real providers, native interaction, required Windows/Linux platform gates and current visual review remain separate evidence. Verify every primary scenario ID from the overview, every UX row, and all journeys above has its current required evidence or an explicit failing/not-verifiable result. The release is not qualified while any required row is missing.

Use the phase-17 utility contracts to reconcile evidence and stop only owned fixture/native/service trees:

```bash
python3 scripts/goal-acceptance.py report --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
python3 scripts/goal-acceptance.py stop --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
python3 scripts/goal-acceptance.py assert-clean --root "$DM_ACCEPTANCE_WORKSPACE" --evidence "$DM_ACCEPTANCE_RUN"
```

`report` writes `run-summary.json` with `{schema_version:1,input_manifest_sha256,journeys:[{id,status,evidence_refs}],scenarios:[{id,status,evidence_refs}],ux_rows:[{id,status,evidence_refs}],budgets,interventions,false_completions,duplicate_effects,usage_coverage}`; status is `pass/fail/not_verified`, never inferred from a missing record. It exits nonzero for missing/failed required proof. `stop` validates recorded executable/start identity/workspace before shutdown, waits for exact descendants and profile host exit, and preserves a failed cleanup record without killing a PID now owned by someone else. `assert-clean` exits 0 only when the scoped fixture/native/provider/compiler/wrapper trees and temporary registrations/leases are gone or an explicitly authorized preserved-work disposition names them. It also records the unchanged installed PID/start and production config hashes. Evidence/original findings remain retained; no cleanup command deletes the installed profile or unrelated workspace.

Update this plan's run summary, canonical audit findings and actual pinned routes/control labels. Fixers record claims; a fresh verifier adjudicates closure. Keep all failed corpus/baseline trials. The final account states achieved outcomes, precise remaining required/manual actions and Axe disposition without claiming that generated docs or synthetic design captures proved autonomous delivery.
