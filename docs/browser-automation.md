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

## Checkpoint boundary

Checkpoint 11 completes the internal locator-repair state, preview, atomic apply,
and same-step resume contract. It deliberately exposes no `browser_workflow` MCP
schema or operation, native repair/replay controls, or provider/process lifecycle
bridge. Those exact list/get/replay/status/cancel/repair-preview/repair-apply and
Stop/direct-input/close/switch/reset/revocation/process-loss/shutdown ownership
surfaces remain checkpoint 12. CDP markers accept only the recipe's validated
method, an empty parameter object, and a fixed value-free rationale; this is not a
raw CDP or JavaScript escape hatch.

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
