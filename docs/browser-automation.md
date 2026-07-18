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

The replay executor deliberately exposes no UI or MCP method. Secret-value entry and
storage, locator repair, workflow-list lifecycle controls, and replay management UI
belong to later checkpoints. CDP markers accept only the recipe's validated method,
an empty parameter object, and a fixed value-free rationale; this is not a raw CDP
or JavaScript escape hatch.

## Verification

The replay integration suite drives the real controller command channel with a fake
host responder. It verifies the fresh-tab setup, every portable action and wait,
ordered assertions, exact response variants and tab snapshots, one-at-a-time
ordering, unique operation IDs, cancellation/replacement fencing, value-free error
surfaces, legacy `tab-1`, and canonical upload containment including redirect
escapes. Browser host tests cover typed wait serialization/injection and the
unsupported-host surface.
