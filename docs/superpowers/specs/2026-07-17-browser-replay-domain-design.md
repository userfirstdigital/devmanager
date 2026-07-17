# Browser Replay Domain Design

## Goal and scope

Checkpoint 7 adds only the platform-neutral workflow replay compiler, value-free status state machine, exact workspace/instance fencing, bounded terminal cleanup, and one replay-lifetime cancellation lease. It does not execute browser actions or touch the host, controller, operation queue, approvals, journal, filesystem, UI, MCP, secret values, or locator-repair payloads.

## Chosen architecture

Create one focused `src/browser/replay.rs` module with two independent units:

- A pure `compile_browser_replay` function validates a strict `BrowserRecipeV1` and caller-supplied public non-secret inputs, then returns an immutable `BrowserReplayPlan`.
- A cloneable `BrowserReplayCoordinator` owns one active replay per `BrowserWorkspaceKey`, exact replay identities, explicit status transitions, one cancellation authority per replay, and a bounded deque of safe terminal projections.

Keeping both units in one module makes their value-safety and lifecycle invariants reviewable without introducing any checkpoint-8 execution coupling.

## Compiler and value boundaries

`BrowserReplayPublicInput` contains an exact declared name, `BrowserRecipeInputKind`, and value. It implements neither `Debug` nor `Serialize`. The compiler:

1. Runs `BrowserRecipeV1::validate` before reading inputs.
2. Rejects recipes above 64 inputs or 256 ordered steps and rejects unsafe input names that are unbounded or contain controls.
3. Rejects duplicate public names, unknown names, missing required public inputs, kind mismatches, and every attempt to submit a `Secret` value through the public API.
4. Applies only validated Text/URL defaults. File and Secret defaults remain forbidden by the recipe contract.
5. Accepts bounded Text values only when they contain no credential-like material. URL values use the existing safe browser-URL and credential checks.
6. Treats File values as bounded, nonblank, NUL/control-free opaque path candidates. It does not require absolute or relative form, normalize, canonicalize, inspect existence, or inspect the filesystem.
7. Leaves declared Secret inputs as unresolved names and reports their presence through safe metadata only.

`BrowserReplayPlan` owns cloned start metadata, ordered recipe steps, non-secret bindings, and unresolved Secret names. It implements neither `Debug` nor `Serialize`. Deliberate accessors expose the start URL, viewport, ordered steps, and resolved values for checkpoint-8 execution; no status, error, or diagnostic projection contains those values.

## Replay identity, status, and transitions

`BrowserReplayStatus` has exactly `Pending`, `Running`, `NeedsUserSecret`, `PausedLocatorRepair`, `Completed`, `Failed`, and `Cancelled`.

Starting a plan creates a monotonically increasing `BrowserReplayInstance` bound to one exact workspace. A plan without unresolved secrets starts `Pending`; a plan with unresolved Secret names starts `NeedsUserSecret`. A second ordinary start for the same workspace fails. Explicit replacement cancels and archives the previous instance before installing the new instance.

The coordinator exposes narrow transition methods with these legal edges:

- `Pending -> Running`
- `NeedsUserSecret -> Running` only through an internal value-free `secrets_ready` seam reserved for checkpoint 9
- `Running -> PausedLocatorRepair | Completed | Failed | Cancelled`
- `PausedLocatorRepair -> Running | Failed | Cancelled`

A checked step-advance method accepts only the current Running instance and exact next index. Completion requires every compiled step to have advanced. `Completed`, `Failed`, and `Cancelled` are immutable. Stale, replaced, cross-workspace, out-of-order, and illegal-transition calls return closed typed errors with fixed messages and no caller value.

`BrowserReplayProjection` is safe to `Debug` and serialize. It includes only exact identity, workspace, recipe/step safe slugs, step counts/index, status, unresolved-secret names, and an optional closed `BrowserReplayFailureCode`; it never includes recipe literals, public values, file paths, or arbitrary error messages.

## Cancellation lease and cleanup

Each replay creates one immutable authority ID and one shared atomic cancellation state. Every clone of `BrowserReplayCancellationLease` references that same authority through one `Arc`; no transition or step boundary creates or rearms an epoch. Lease handles implement neither `Debug` nor `Serialize` and expose only authority identity comparison and cancellation observation.

Cancel, replacement, and workspace interruption synchronously invalidate the shared lease and transition the exact nonterminal replay to `Cancelled`. A caller holding a lease observes cancellation immediately between simulated step calls and across Pending, Running, NeedsUserSecret, and PausedLocatorRepair gaps.

Active plans are removed on terminal transition. Only safe terminal projections enter a configurable bounded deque; oldest terminal projections are evicted first. Evicted identities become stale. Capacity is always at least one.

## Errors and verification

`BrowserReplayError` and `BrowserReplayFailureCode` are closed enums with fixed value-free formatting. No variant carries arbitrary text, input values, paths, or recipe values.

Focused integration tests in `tests/browser_replay.rs` cover compiler defaults and every input rejection, public Secret rejection plus `NeedsUserSecret`, exact transitions, step ordering, cancellation across gaps and pauses, replacement/stale fencing, terminal immutability, bounded cleanup, value-free formatting/serialization, non-serialization of value-bearing carriers, and `Send + Sync` platform-neutral behavior. Broader browser, compile, build, formatting, and diff gates remain required before completion.
