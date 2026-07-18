# Browser Replay Locator Repair Design

## Scope

Checkpoints 10 and 11 add one exact, route-bound locator-repair lifecycle to the existing replay coordinator. They do not add the `browser_workflow` MCP group, provider lifecycle wiring, or a second replay owner; those remain checkpoint 12.

The repair path must:

- distinguish a missing semantic target from a crashed WebView;
- pause the exact replay at the exact locator-bearing step position;
- retain a fresh semantic snapshot and screenshot as pinned resources while repair is unresolved;
- accept a replacement only from the current page revision and exact repair instance;
- preview by highlighting only, without clicking, typing, focusing, recording, or changing page revision;
- require explicit confirmation before a repository write;
- compare the current recipe and exact old locator before an atomic replacement;
- optionally wake the same executor and retry the same step with the replacement; and
- release evidence, highlights, overrides, secrets, and waiters on every terminal lifecycle path.

## One lifecycle owner

`BrowserReplayCoordinator` remains the sole replay lifecycle owner. Each active replay gains at most one private repair state and one value-free change signal. No separate repair coordinator may independently decide whether a replay is active, paused, cancelled, or terminal.

`BrowserReplayExecutionHandle` keeps the immutable plan, cancellation authority, secret store, locator-override map, and a value-free change receiver alive while the executor waits. A repair pause does not return from `execute_browser_replay`; the executor waits for either an exact confirmed resume or a terminal signal. This preserves unresolved public inputs and memory-only secrets without minting a second execution authority.

The existing seven replay statuses remain unchanged. `PausedLocatorRepair` is enriched by an exact `BrowserReplayRepairInstance` and safe `BrowserReplayRepairProjection`, not by adding another top-level status machine.

## Typed locator failure

Add `BrowserError::LocatorNotFound { target }`, where `target` is a fixed enum containing only `Primary`, `Source`, or `Destination`. It is safe to serialize and format. The Windows injected action boundary returns only fixed missing-target codes; it never returns a selector, accessible name, URL, page text, or arbitrary exception message. Secret target disappearance and target-change fencing map to `Primary`.

The executor maps the host target kind to an exact recipe locator slot:

- primary action locator;
- drag source or destination;
- optional keypress/scroll locator;
- nested action wait locator;
- step wait locator; or
- assertion locator plus assertion index.

An element-present, element-visible, or element-value wait/assertion that reaches its page-condition timeout is repairable at its known locator slot. Element-absent and element-hidden semantics are not converted into missing-locator failures merely because the target is absent.

All direct MCP browser actions also receive the typed host error, but only recipe execution creates repair state.

## Repair identity and evidence

`BrowserReplayRepairInstance` contains the replay instance, a checked monotonic repair ID, and an unforgeable coordinator scope. Equality requires pointer-identical scope plus exact workspace, replay, and repair IDs. The safe projection contains only:

- workspace, replay, and repair IDs;
- recipe and step IDs;
- step index and locator slot;
- runtime tab ID and captured `BrowserRevision`;
- pinned semantic-snapshot and screenshot resource handles; and
- fixed phase (`awaitingPreview`, `previewed`, or `applied`).

The failed locator and candidate locator stay private. They are validated recipe metadata, but do not need to be copied into status, errors, journals, or inline MCP results.

On a typed failure the executor captures, in order, a fresh semantic snapshot and viewport screenshot through the existing controller queue. It verifies exact response kinds, tab ID, owner, resource kinds, and snapshot revision. It pins each resource immediately; a failure rolls back earlier pins. The coordinator owns a non-serializable pin lease and unpins on cancel, replacement, successful resume, terminalization, or drop. The resource bodies remain available only through their existing owner-scoped handles.

If either capture or pin step fails, replay terminates with the existing fixed `StepFailed` code rather than exposing a partial repair instance.

## Waiting and interruption

Each active replay owns a Tokio watch generation. Pause, preview/apply state changes, resume, cancel, replace, workspace interruption, and terminalization advance it. The executor subscribes before waiting and always rechecks exact coordinator state after wakeup, so a signal cannot be lost or rearm a cancelled authority.

While paused, Stop, direct user input, route loss, registration revocation, process loss, reset, profile clear, and shutdown will use checkpoint 12's lifecycle bridge to call the existing coordinator cancellation path. Checkpoints 10 and 11 prove the coordinator/executor side using cancel, replace, and workspace interruption directly.

## Preview

`BrowserReplayRepairCandidate` contains an exact `BrowserElementRef`; its revision must equal both the captured repair revision and the host's current workspace revision. The locator must convert to a valid `BrowserRecipeLocator` and must not be coordinates-only.

A dedicated internal repair-highlight command resolves the candidate and draws one DevManager-owned, pointer-transparent overlay. Its DOM nodes are excluded from mutation revision tracking. Preview does not focus or dispatch page events and is never sent to the recipe recorder. Navigation, another preview, cancel, or apply clears the prior overlay.

Preview first validates the exact repair instance, then asks the real host to validate and highlight the element reference. Only an exact acknowledged response stores the candidate. A late callback after page, workspace, replay, or repair replacement is ignored and cleared.

The same preview API accepts `User` or `Agent`; checkpoint 12 exposes the Agent path through MCP and connects the native user surface.

## Atomic recipe apply

The compiled replay plan stores a private SHA-256 digest of the validated canonical recipe JSON. Repair state also stores the exact original step ID, index, locator slot, and locator.

Apply requires:

1. the exact active repair instance in `PausedLocatorRepair`;
2. a successfully previewed candidate from the same current revision;
3. explicit `confirm: true`;
4. a second host validation of that exact element reference;
5. existing approval authorization for an Agent repository overwrite, with a `Destructive` risk floor; and
6. the canonical authenticated project root.

Under one process-local recipe-write gate, the repository helper reloads the strict v1 recipe, compares its canonical digest, exact step ID/index, locator slot, and old locator, clones it, replaces only that locator, validates the complete recipe, and uses the existing hardened sibling-temp atomic replacement. The destination is re-read and compared again at the final boundary. Failure leaves the old complete file and paused repair intact; no partial JSON is accepted.

Apply is idempotent only for the same active repair and same already-applied candidate. A changed recipe, page revision, workspace, replay, repair, step, slot, old locator, or candidate returns a fixed typed repair error and performs no write.

## Same-step resume

After a successful write, the replacement is installed into the execution handle's private override map under `(step_index, locator_slot)`. If `resume` is true, the coordinator clears the highlight, removes the repair pin lease, changes the replay back to `Running`, advances the watch generation, and the existing executor retries the same step without advancing progress first.

Every action, wait, and assertion obtains its effective locator from the override map immediately before constructing a command. A repaired drag source does not replace its destination; an assertion repair does not affect another assertion. The original immutable plan is never mutated in memory.

If `resume` is false, the repair remains paused and reports `applied`; a later exact apply call may request resume without writing again. A second locator failure at the same step creates a new repair ID and fresh evidence.

## Safety and platform behavior

- Repair state, pin leases, write reservations, and override maps are non-`Debug` and non-serde.
- Safe projections and errors contain no selector, page text, path, public input value, secret, or arbitrary exception.
- Preview is Normal risk and non-mutating; Agent apply has a Destructive floor while higher runtime/declared risks still win.
- Secret stores remain open during a pause and close on every terminal executor return.
- Unsupported/macOS builds expose the domain and return `UnavailablePlatform` for highlight/authorization commands without importing Wry/WebView2.
- Recording never captures evidence, highlights, authorization, or repair bookkeeping.
- Checkpoint 12 remains responsible for MCP schemas, native repair controls, shared external lifecycle cancellation, and provider-session loss.

## Test strategy

Checkpoint 10 covers typed Windows callback mapping, target-kind safety, exact repair identity, pin rollback/release, fresh evidence validation, pause/wait/cancel/replace races, and absence of repair payloads from safe replay surfaces.

Checkpoint 11 covers current-revision preview, overlay mutation exclusion, User/Agent authority, explicit confirmation, approval risk, strict recipe compare-and-replace, atomic failure preservation, concurrent/stale apply rejection, exact locator-slot mutation, applied-without-resume, same-step retry, second failure, and all terminal cleanup paths.
