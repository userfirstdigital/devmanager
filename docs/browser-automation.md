# Browser automation

DevManager owns one browser runtime for browser-assisted Claude and Codex work. The
native host remains authoritative for tabs, command ordering, approvals, resources,
and the activity journal. Recording and replay reuse that browser controller; they
do not introduce a second transport, queue, approval path, or journal.

## Recording and portable recipes

Recording starts from the currently selected browser tab and assigns it the logical
alias `tab-1`. Later tabs receive deterministic recipe aliases. Recipes store strict,
typed browser actions, waits, and assertions rather than runtime WebView tab IDs or
arbitrary JavaScript predicates.

The replay compiler validates the complete alias lifecycle before execution. A tab
must be created before it can be selected or closed, an alias cannot be reused, and
closed aliases cannot be referenced. Existing legacy recipes that explicitly create
`tab-1` remain supported: their setup tab stays implicit until that create succeeds.

## Replay lifecycle

A replay is scoped to the exact workspace, coordinator instance, immutable compiled
plan, and cancellation authority returned when it starts. Execution requires an
authenticated canonical local project root. An invalid, missing, remote, or
noncanonical root fails before any browser command is issued.

Every replay uses this setup sequence, awaiting and validating each response before
issuing the next command:

1. Create a fresh blank setup tab.
2. Apply the recipe viewport to the returned runtime tab ID.
3. Navigate that tab to the recipe start URL.

The fresh setup tab normally becomes logical `tab-1`. Ambient tabs are never mapped
to recipe aliases. Create, select, close, viewport, and navigation commands update
replay tab state only after the returned workspace snapshot proves the requested
mutation exactly.

Each recipe step then runs its action, optional wait, and assertions in declaration
order. The coordinator advances only after the entire step succeeds. The first
failure stops execution, and only a replay that advances every step can complete.
The executor awaits exactly one existing `BrowserController` request at a time, so
normal operation-queue serialization, target inspection, approval, cancellation,
resource handling, and agent journaling remain in force.

## Typed waits and assertions

Recipe waits compile to bounded host predicates for duration, URL exact/contains,
document load, network idle, element present/visible/hidden, and text
present/absent. Assertions use the same typed wait command for URL, title, text,
element present/absent/visible/hidden, and exact element value. Replay never emits
an arbitrary JavaScript wait.

An ordinary wait that returns `matched: false` fails the step. An assertion that
returns `matched: false` records `AssertionFailed`. A transport error, host error,
wrong response variant, unresolved alias, or invalid workspace snapshot records the
closed `StepFailed` code. Raw host error text and recipe values do not enter replay
status or serialized errors.

## Cancellation and replacement

The executor checks the exact replay instance and its cancellation authority before
and after every awaited browser request and before coordinator transitions. Cancel,
workspace interruption, replacement, or controller interruption stops the old
instance. A late response from an in-flight command is discarded: it cannot change
aliases, run later work, advance a step, fail the replacement, or complete the old
replay.

## Locator repair

Eligible typed `LocatorNotFound` action failures and element wait/assertion timeouts
pause the exact replay instead of terminating it immediately. Before publishing the
pause, the executor retains a fresh semantic snapshot and viewport screenshot in
dedicated owner-scoped resources. The safe repair projection contains only exact
IDs, the locator slot and resume phase, tab/revision metadata, fixed phases, and
resource handles; selectors, page text, recipe values, paths, secrets, and callback
messages remain behind private state or resource handles.

A replacement candidate must be a semantic `BrowserElementRef` from the current
page revision. Preview uses the existing controller queue and journal to install a
pointer-transparent owned highlight. Its exact generation token provides
compare-and-swap install/clear behavior, it dispatches no focus/click/input event,
and owned overlay mutations do not increment the page revision or enter workflow
recording. Navigation, cancellation, workspace/replay/repair replacement, stale
revision, and late callbacks all fence preview authority.

Saving requires that exact preview plus explicit confirmation. An Agent request
declared `Normal` is raised to the `Destructive` approval floor; a higher declared
risk is preserved. User requests preserve their declared risk. Pre-commit and
post-commit validation travel through a private, serde-skipped, sealed command that
external Rust callers cannot construct, while ordinary external code may still
exhaustively ignore or pattern-match the internal variant with `..`.

The compiled plan privately retains the domain-separated digest of compact,
validated v1 recipe JSON. The execution handle binds the authenticated canonical
recipe root once. Apply reloads the recipe, verifies that digest plus the exact
recipe ID, step index, step ID, locator slot, and old locator, replaces only that
locator, validates the complete recipe, and rechecks at the final write boundary.
It reuses the recipe store's process-global write gate and atomic sibling-file
replacement. A cooperating DevManager save/apply therefore leaves either the old
complete recipe or the new complete recipe and no temporary file; this is not an
OS-wide compare-and-swap guarantee against a non-cooperating external editor.

Cancellation and apply share the coordinator gate. Cancellation wins before a
write, or one `Preparing`/`Committing` reservation installs the complete file,
private runtime override, and `Applied` state coherently before terminalization may
continue. Fallible post-validation context creation and final pre-commit host
authorization both happen before that durable commit. After a write, a page,
token, or revision change returns the committed `Applied` projection without
issuing a browser action. A later no-write resume requires a fresh exact preview
of the already committed locator.

Resume retries only the failed action, action wait, step wait, or assertion phase.
A successful mutating action is not repeated after a later wait/assertion failure,
and progress does not advance before the retried phase succeeds. A later failure
gets a new repair identity. Cancellation, replacement, terminal completion, and
coordinator/resource teardown release evidence pins, overlay cleanup, repair
authority, and memory-only secret state at their established lifecycle boundaries.

## Uploads and downloads

Uploads resolve declared File inputs at execution time. Relative paths are resolved
from the authenticated project root. The existing upload classifier canonicalizes
the candidate, follows symlinks or Windows directory redirects, verifies that it is
a file, and declares either `Normal` or `OutsideWorkspaceFile` risk. An outside-root
file therefore enters the existing approval flow; replay never bypasses approval or
copies a file into the workspace to reduce its risk.

Recipe download actions remain semantic clicks. They do not add a direct filesystem
or download-manager path.

## Exact `browser_workflow` MCP contract

Checkpoint 12 exposes one strict grouped `browser_workflow` tool. Every call requires
a bounded nonblank `intent`, one of the exact risks `normal`, `financial`,
`destructive`, `accountSecurity`, `permissionChange`, `outsideWorkspaceFile`, or
`osPermission`, and exactly one of these operations:

| Operation | Accepted operation fields |
| --- | --- |
| `list` | None |
| `get` | `recipeId` |
| `replay` | `recipeId` and optional `inputs` |
| `status` | Positive `replayInstanceId` |
| `cancel` | Positive `replayInstanceId` |
| `repairPreview` | Positive `replayInstanceId`, positive `repairId`, and `candidate` |
| `repairApply` | Positive `replayInstanceId`, positive `repairId`, `confirm: true`, and explicit `resume` |

Unknown or cross-operation fields fail validation. `intent` is at most 1,024 bytes
and `recipeId` is at most 128 bytes. Public inputs contain exactly `name`, `kind`,
and `value` and are limited to `text`, `url`, and `file`; a call may contain at most
64 inputs, with nonblank names up to 128 bytes and runtime value limits of 65,536
bytes for text, 8,192 bytes for a URL, and 32,768 bytes for a file path. A repair
candidate contains `revision`, `locator`, and optional `backendNodeId`; locator
fallbacks are `accessibilityRole`, `accessibilityName`, `testId`, and
`cssSelectors`.

The authenticated HTTP body is capped at 1 MiB before RMCP processing, including
chunked requests without a usable `Content-Length`. Workflow repository reads are
also capped at 1 MiB per file, 1,024 directory entries, and 256 accepted recipes.
A recipe is capped at 64 inputs and 256 steps, with at most 16 assertions per step
and 256 assertions overall. A semantic locator accepts at most 16 CSS fallbacks;
each role, name, test ID, selector, and repair-candidate string is capped at 2,048
bytes. Runtime execution reuses the same locator validation rather than trusting a
previously persisted recipe.

Every success contains `ok: true`, `version: 1`, and the exact `operation`. `list`
adds sorted `recipes`; `get` adds `recipe` and an owner-scoped `resource` handle for
the complete validated recipe JSON. Replay, status, and cancel add only `replay` and
the current `repair` or `null`; preview returns those same fields. Apply adds
`replay`, `repair`, and `recipeWritten`. Replay projections contain only the replay
and recipe IDs, fixed status, current/total step information, unresolved Secret
input names, and a fixed failure. Repair projections contain only exact replay,
repair, recipe, step, tab, revision and locator-slot metadata, fixed phase, and
snapshot/screenshot handles. Locator slots are `primaryAction`, `optionalAction`,
`dragSource`, `dragDestination`, `actionWait`, `stepWait`, or `assertion` with its
index.
Workspace keys, canonical roots, submitted values, Secret values, candidates,
selectors, page text, paths, and resource bodies are not copied into these compact
inline projections. Full recipe and repair evidence stay behind owner-scoped MCP
resources, and stale registrations or another workspace cannot list or read them.

The fixed replay statuses are `pending`, `running`, `needsUserSecret`,
`pausedLocatorRepair`, `completed`, `failed`, and `cancelled`. Fixed failures are
`stepFailed` and `assertionFailed`; typed MCP errors include `stale_reference`,
`invalid_request`, `missing_file`, and `invalid_recipe` without leaking raw host or
path details.

## Authenticated ownership and provider injection

The gateway listens only on `http://127.0.0.1:<random-port>/mcp`. Each provider
registration receives a fresh 256-bit in-memory bearer token bound to one process
session, one exact `{ project_id, ai_tab_id }` workspace, one registration lease,
and one canonical local project root. Requests require the exact Bearer token and a
loopback `Host` header for the gateway's port. Missing, malformed, stale, replaced,
or cross-workspace credentials are rejected. Every recipe operation revalidates the
bound root before filesystem access or browser effects; aliases, remote/UNC roots,
missing roots, and non-directories fail closed.

Claude receives an ephemeral `--mcp-config` overlay whose authorization header reads
the session-only `DEVMANAGER_BROWSER_TOKEN` child environment variable. Codex
receives session-specific `mcp_servers.devmanager_browser` URL,
`bearer_token_env_var`, `required=false`, and
`default_tools_approval_mode="approve"`. The
token is redacted from diagnostics and is neither persisted nor added to global user
configuration. Registration, overlay, adapter, bridge, gateway, or WebView startup
failure revokes partial browser state, retains the original Claude/Codex launch, and
shows a Browser-unavailable diagnostic; terminal use continues normally.

## Native-only Secret handoff

Recipes may declare Secret inputs, but MCP has no Secret input kind and no
secret-submission operation. A replay reports unresolved Secret names, then the
local native companion pane owns the only value-entry path. Values live in
`Zeroizing<String>`, render with a constant mask, and are handed directly to the
shared replay coordinator. Plaintext never enters MCP results or resources,
serializable pane/persistence/remote state, recordings, journals, or debug output.
Submitting requires all fields; canceling the prompt cancels that exact replay and
clears the vault.

## Repair controls

`repairPreview` validates the exact workspace, replay, repair, tab and live page
revision before using the existing controller queue to preview a semantic
replacement. `repairApply` requires the exact preview and `confirm: true`; an Agent
apply has at least `destructive` risk. `resume: false` saves only, while
`resume: true` saves and retries the failed phase. The native pane provides the same
flow as **Select replacement**, **Save repair**, and **Save and retry**. Stale
evidence, changed page/recipe/root/identity, cancellation, or a superseding repair
fails closed, and the safe result continues to expose only fixed metadata and
resource handles.

## Cancellation ownership

| Trigger | Replay and pending-work scope |
| --- | --- |
| MCP/native Cancel | Exact replay instance |
| Stop tab or close logical browser tab | Owning conversation workspace |
| Stop workspace or reset conversation browser | Exact workspace |
| Direct trusted user input | Exact workspace/tab and pending operation; admission epochs prevent older queued input from canceling a newer replay |
| Conversation or terminal-surface switch | Previous active workspace only |
| Clear project profile or delete project | All workspaces in that project; other projects remain live |
| Disable Browser or replace local browser configuration | All local replays before gateway/host teardown |
| Registration replacement/revocation or provider-process exit | Exact registered workspace and lease |
| Restart or close an AI conversation | Exact provider workspace; reinjection creates a fresh registration and token |
| App shutdown, update, force quit, or local/remote-mode transition | All local replays and pending browser work before host/process mutation |

Cancellation, replacement, and teardown also clear native Secret state, repair
evidence/highlights, and replay UI authority. A response that arrives after its
authority was canceled is fenced as interrupted and cannot advance or complete the
old replay.

## Platform and release boundary

Windows is the only functional v1 browser platform and uses the Wry/WebView2 child
host. Non-Windows builds use a compile-safe unavailable adapter: status reports that
Browser is unavailable and commands return `UnavailablePlatform`. The macOS ARM64
app/DMG release job is therefore a compile/package compatibility gate, not a claim
of browser functionality, and no partial WebKit host is included. The release
matrix remains Windows x64 NSIS+WiX, Windows ARM64 NSIS, and macOS ARM64 app+DMG.

V1 adds no whole-PC control, external-Chrome mode, desktop-control tool surface,
Playwright browser backend, Node sidecar, or second workflow/replay owner. Recipe
CDP markers still accept only the validated method, empty parameter object, and a
fixed value-free rationale; that recipe marker is not a raw CDP or JavaScript escape
hatch and does not change the separate trusted-project `browser_cdp` tool contract.

## Checkpoint boundary

Checkpoint 12 is complete and independently approved through `db8f08e`. The final
hardening moves WebView teardown out of the closing window-owned pump, preserves
liveness while close is vetoed, drains canceled native generations before reopen,
bounds authenticated HTTP and workflow-repository inputs, and removes test-only
replay cursor state from production. The focused final review found 0 Critical,
0 Important, and 0 Minor findings. Local Windows x64 build/package evidence is
complete; Windows ARM64 and macOS ARM64 remain CI release-matrix jobs.

## Verification

The replay integration suite drives the real controller command channel with a fake
host responder. It verifies the fresh-tab setup, every portable action and wait,
ordered assertions, exact response variants and tab snapshots, one-at-a-time
ordering, unique operation IDs, cancellation/replacement fencing, value-free error
surfaces, legacy `tab-1`, and canonical upload containment including redirect
escapes. Browser host tests cover typed wait serialization/injection and the
unsupported-host surface.

Locator-repair coverage additionally exercises evidence order and retention,
highlight behavior, revision/identity fencing, deterministic recipe digests, every
exact locator slot, atomic old-or-new replacement failures, approval denial and
interruption, cancellation/apply linearization, post-write page drift, all four
resume cursors, no duplicate successful mutation, second-generation repair cleanup,
secret-store lifetime, private trait/wire boundaries, and unsupported-host routing.

Checkpoint-12 test-only follow-up `20c4821` (`test(process): prove auto restart
completion before shutdown`) replaces timing inference with a deterministic
completion seam. Before that seam, the single-threaded locked all-target run reached
765/766 library tests and failed only when the new deterministic proof timed out.
After the seam, the exact focused regression and its repeated focused runs passed;
the four detached-worker races passed 4/4; ProcessManager passed 80/80; format and
the source-diff check were green; and the fresh locked all-target test exited 0 in
618.1 seconds with 766/766 library tests plus every integration and example target
green.

At final head `db8f08e`, `cargo fmt --all -- --check` passed;
`cargo check --locked --all-targets -j1` passed in 101.7 seconds without warnings;
and `cargo test --locked --all-targets -j1 -- --test-threads=1` exited 0 in
778.8 seconds with 788/788 library tests plus every integration, binary, and example
target green. `cargo build --release --locked -j1` exited 0 in 352.8 seconds.

`cargo packager --release --formats nsis,wix` exited 0 in 35 seconds and produced:

- `target/release/devmanager.exe` — 43,836,928 bytes, SHA-256
  `3D005567239D106292625642F116FFFD39E8BBBEC398B22D4BA8F671D8E5C3C0`.
- `dist/packager/devmanager_0.3.5_x64_en-US.msi` — 17,809,408 bytes, SHA-256
  `A80C82A041702FCB222F0E44BAB7F75714AEF625C12413B5AAB84A1B2CE46880`.
- `dist/packager/devmanager_0.3.5_x64-setup.exe` — 13,649,949 bytes, SHA-256
  `B3B7B3AE84EBF55EF263E5B9F69DFF39C12A0D28A9E5E2B5D9552A6160027083`.

The exact formerly stuck Quit-again/Quit-anyway initialization path was exercised
with temporary instrumentation and the candidate exited in 32 ms after the browser
lease drained. The clean uninstrumented candidate launched, but Windows Computer
Use returned `foreground window did not report a process id` for the initial bind
and the permitted refresh/retry, so the isolated candidate was stopped and no clean
rerun result is inferred. The installed DevManager instance was not touched.

Earlier native acceptance demonstrated the split companion pane, Claude/Codex
launch and session injection, conversation isolation/restoration, browser tabs,
viewport controls, annotation capture, recording review, and collapse/reopen. It
did not demonstrate a real provider successfully invoking every MCP tool,
upload/download, next-prompt annotation consumption, or a complete recipe
replay/repair flow. Windows ARM64 NSIS and macOS ARM64 app/DMG were not run locally
and remain release-CI gates.
