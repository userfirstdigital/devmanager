# Browser Replay Executor Design

## Goal and checkpoint boundary

Checkpoint 8 executes one compiled checkpoint-7 replay through the existing `BrowserController`. It adds no parallel host path, operation queue, approval system, journal, lifecycle owner, MCP/UI surface, secret store, locator repair, or raw JavaScript escape hatch.

The executor receives the exact `BrowserReplayCoordinator`, `BrowserReplayInstance`, one non-`Debug`/non-`Serialize` execution handle containing the immutable plan and cancellation authority, a `BrowserInvocationActor`, and an authenticated canonical local project root. It awaits exactly one existing controller command at a time.

## Chosen architecture

Add a focused `src/browser/replay_executor.rs` module. Keep compilation and lifecycle transitions in `replay.rs`; the new module owns only value resolution, command mapping, response fencing, tab aliases, and sequential orchestration.

The coordinator stores the plan in one `Arc<BrowserReplayPlan>` and gives the starter a `BrowserReplayExecutionHandle` that shares that immutable plan and the replay-lifetime cancellation authority. The handle is exact-instance data, implements neither `Debug` nor `Serialize`, and exposes no values publicly. Terminal transition removes the coordinator's active reference; an executing task may retain its handle only until it observes cancellation or finishes unwinding.

Tests drive the real `BrowserController` through `browser_command_channel` with a fake inbox responder. This proves ordering and response fencing without adding an executor-specific transport or host queue.

## Root preflight and setup

The executor begins the exact Pending instance, verifies the supplied local project root with the existing authenticated-root canonicalization, and converts any failure to the fixed `StepFailed` terminal code. Root verification happens before the first browser setup command, so an invalid, remote, noncanonical, missing, or nondirectory root causes no browser side effect and no path appears in replay output.

Every replay then creates a fresh setup tab with `CreateTab { url: None }`. The runtime tab ID is accepted only from the returned `Workspace` snapshot: the selected ID must be nonblank, present exactly once in the returned tab list, and identify the newly returned selected tab. The executor applies the recipe viewport to that tab and then navigates it to `startUrl`, checking the exact `Workspace` response after each command, before executing any recipe step.

This fresh tab makes setup independent of ambient tab order and gives every replay a known current tab. Existing ambient tabs are never silently assigned recipe aliases.

## Portable tab aliases

New recordings seed their currently selected tab as logical `tab-1` when recording starts. The replay compiler validates alias lifecycle in recipe order:

- Normally the fresh setup tab begins as active `tab-1`.
- For legacy recipes that explicitly introduce `tab-1` with `CreateTab`, the setup tab remains implicit and unaliased until that create succeeds.
- `CreateTab` introduces one previously unseen active alias only after a successful `Workspace` response proves the returned selected tab.
- `SelectTab` and `CloseTab` require an active alias. Selection succeeds only when the returned selected ID exactly matches the mapped runtime ID. Close succeeds only when the returned snapshot no longer contains that runtime ID, then removes the active alias.
- Alias count is bounded, aliases are never inferred from ambient tabs, and a closed or otherwise unresolved alias is not rebound.

Statically impossible lifecycle sequences fail compilation as `InvalidRecipe`. Any runtime snapshot mismatch or unresolved alias fails the current replay as `StepFailed`. This preserves old `CreateTab tab-1` workflows while making newly recorded initial-tab workflows portable.

## Sequential command compilation

Each setup operation, action, optional wait, and assertion gets a fresh `BrowserInvocationContext` with the caller's actor, a unique operation ID, a fixed bounded value-free intent, and declared `Normal` risk except a classified upload. No intent includes recipe values, URLs, paths, locator strings, CDP methods, or host error text.

Recipe actions map to existing commands and exact responses:

- tab creation, selection, close, navigation, and viewport changes use `Workspace` responses;
- back, forward, and reload use `Acknowledged`;
- click, hover, focus, type, clear, select, keypress, scroll, drag/drop, and semantic download use one-action `Act` commands and `Action` responses;
- screenshot uses `Screenshot`;
- upload uses `Upload`;
- `CdpMarker` uses its already validated method, an empty object, a fixed rationale intent, and requires `Cdp`;
- action-level and step-level waits use `Wait` and require `Wait`.

Download remains the existing semantic click path rather than a filesystem or download-manager shortcut. Existing host target inspection remains the only runtime risk elevation for financial, destructive, account-security, and permission targets.

## Waits and assertions

Extend `BrowserWaitCondition` and the injected host wait implementation only with the typed predicates needed by recipes: `NetworkIdle`, `Title`, `ElementAbsent`, and `ElementValue`. Network idle is bounded and observes the existing injected fetch/XHR instrumentation; the other predicates inspect title, semantic locator absence, or exact element value. Recipe execution never emits `JavaScript` waits.

For each step the executor runs the action, its optional wait, and every assertion in declared order. Assertions compile to short bounded typed waits:

- URL and title preserve exact-versus-contains semantics;
- text preserves present-versus-absent semantics;
- element checks preserve present, absent, visible, and hidden states;
- value checks require exact element value.

A successful command with `matched: false` during an ordinary recipe wait is `StepFailed`. During an assertion it is `AssertionFailed`. A transport, host, response-variant, alias-proof, mapping, or value-resolution failure is `StepFailed`. The coordinator advances only after the entire step succeeds, stops on the first failure, and completes only after all steps advance.

## Cancellation and stale-response fencing

The executor checks the shared cancellation authority and exact coordinator instance before and after every awaited controller call and immediately before every advance, fail, or complete transition. Cancel, replacement, workspace interruption, controller interruption, or a stale instance ends execution as Cancelled/stale. A response that arrives after cancellation or replacement is discarded and can never update aliases or advance the coordinator.

No command is spawned concurrently and no late task is left running by the executor. Existing controller and host cancellation remain authoritative for in-flight transport; the replay lease supplies the additional exact-instance fence between calls and after late responses.

## File containment, approvals, and value safety

An upload value must resolve from a declared File input at execution time. Relative candidates are joined to the verified authenticated root; absolute candidates remain absolute. The existing `classify_upload_path` canonicalizes both root and candidate, resolves symlinks, verifies existence, and returns either `Normal` or `OutsideWorkspaceFile`. The canonical path is passed only in the existing upload command, using the authenticated-root request path. Outside-workspace files therefore enter the existing approval flow through declared risk.

Executor state, handles, outcomes, and errors carry only closed enums, indices, safe recipe/step aliases, and safe projections. They never derive `Debug` or `Serialize` over plans, bindings, resolved values, canonical paths, or raw host errors. Host errors are collapsed to fixed replay failure codes; existing browser journaling receives only fixed redacted command summaries and value-free replay intents.

## Verification strategy

Implementation proceeds in strict test-driven slices: compiler alias lifecycle and recording seed; setup and all action/wait/assertion mappings; fake-bridge one-at-a-time ordering and response fencing; cancellation/replacement before, between, and after late calls; fixed contexts, risk, and journal-safe summaries; assertion failure and first-failure stop; file containment/symlink/outside-workspace risk; typed injected waits; and unsupported/macOS compilation. Focused replay/recipe/recording/host tests, the aggregate browser suite, `cargo check`, release build, formatting, and an exact diff review gate the checkpoint.
