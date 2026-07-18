# Browser Replay Memory-Only Secrets Design

**Status:** Approved checkpoint within the user-approved Windows v1 browser plan

## Scope

Checkpoint 9 lets a replay that is in `NeedsUserSecret` accept password-like values from a masked DevManager prompt and use them in recipe `Type` steps. It adds no `browser_workflow` MCP group, locator-repair state, repository mutation, or new lifecycle owner. Checkpoint 12 will connect the prompt and execution seams to the final workflow tool and app lifecycle.

The security boundary is stricter than the ordinary browser action path: plaintext must never enter a type that can be serialized or meaningfully formatted with `Debug`. In particular, it must not enter `BrowserAction`, ordinary `BrowserCommand` fields, MCP arguments/results, status projections, resources, approval requests, recording drafts, diagnostics, or journal entries.

## Secret ownership

Add `zeroize` as a direct dependency and introduce a focused `replay_secrets` module.

`BrowserReplaySecretSubmission` is a single-owner, non-`Clone`, non-`Debug`, non-serde map of bounded secret input names to `Zeroizing<String>` values. Its constructor accepts only an internal user-prompt boundary. It rejects empty, oversized, duplicate, missing, and extra values with closed errors that never echo a name or value.

Each active replay owns one `BrowserReplaySecretStore`. The coordinator and execution handle share its authority, not copies of its plaintext. The store is exact-instance scoped and has two states:

- `Open`: accepts one exact submission while the replay is `NeedsUserSecret`.
- `Closed`: refuses all exposure and contains no retained values.

Successful submission atomically verifies the complete unresolved-name set, installs the zeroizing values, and transitions the exact replay to `Running`. The safe projection clears `unresolvedSecretInputs`; it never reports values, lengths, or per-value metadata.

Completion, failure, cancellation, replacement, workspace interruption, coordinator drop, and explicit execution teardown synchronously close the store and zeroize its map. Closing is idempotent. A host lease may retain the store authority while work is queued, but it cannot expose a value after closure; this fences late callbacks and approval resumes without relying on eventual `Arc` destruction.

## Secure command lane

Ordinary text keeps using `BrowserAction::Type`. Secret text does not.

Add a value-free `BrowserCommand::SecretType { tab_id, target, input_name }` marker. `input_name` is a validated safe recipe input name, not a value. The command remains safe to clone, inspect, journal, and serialize. It has a fixed redacted summary such as `type secret input`.

The plaintext travels through a separate private sidecar:

- `BrowserController::request_replay_secret_type` accepts an exact replay-scoped sealed lease plus the value-free marker and Agent invocation context.
- `BrowserCommandEnvelope` and `BrowserCommandRequest` hold an optional private sidecar. Neither envelope nor request implements `Debug` or serde.
- The ordinary `request` and `request_with_context` methods cannot attach a sidecar.
- The host rejects a `SecretType` marker without the matching current sidecar, and rejects a sidecar attached to any other command, workspace, replay instance, or input name.
- Registration, project, workspace, and tab cancellation tickets apply exactly as they do to ordinary queued agent automation.

The sidecar contains only an unforgeable lease to the replay store. It does not clone plaintext into the request. Exposure is callback-scoped and allowed only after the host has validated the route, target, cancellation epoch, replay instance, and approval state.

## Host execution, approvals, recording, and audit

Windows routes `SecretType` through the existing per-tab Agent operation queue. It reserves recording using only the safe marker, inspects the semantic target before exposure, combines declared and runtime risk, and uses the existing approval flow. Account/security targets therefore still require confirmation under the project policy.

If recording is active, the command produces only an unset generated `Secret` input and a recipe `Type` reference. No password value enters recorder state.

After approval, the host exposes the value only inside a closure that builds zeroizing JSON and script buffers, invokes a dedicated injected `typeSecret` function, and drops/zeroizes the buffers immediately after WebView2 accepts the script. The page receives the value because typing requires it; DevManager retains no ordinary `String` copy. The injected function resolves the target, assigns the value, dispatches the normal input/change events, and returns only `{ completedActions: 1 }`.

All denial, timeout, interruption, crash, route loss, stale approval, response mismatch, and callback paths drop the request sidecar. Journal, approval, diagnostics, command summaries, and recording completion see only the value-free marker and fixed result codes. Unsupported platforms reject the secure operation with the existing typed unavailable error and never request exposure.

## Executor behavior

The replay plan keeps secret names unresolved and contains no secret values. For a recipe `Type` action:

- literal or Text input: use the existing `BrowserAction::Type` path;
- Secret input: obtain a sealed lease for that exact input from the execution handle and call the secure controller method;
- missing, closed, stale, or wrong-kind lease: fail with the existing closed step failure before a browser side effect.

Secret inputs remain invalid for navigation, waits, assertions, select values, uploads, CDP, or any action other than validated secret typing. The executor closes its secret store on every terminal return, including errors before the first browser command.

## Masked prompt contract

`BrowserReplaySecretPromptEvent` and the pane projection contain only workspace key, replay instance ID, safe input names, focus, and boolean `isSet` flags. They derive `Debug`/serde because they are intentionally value-free.

The editor vault is separate from `BrowserPaneTransient` and `BrowserPaneModel`. It is non-`Clone`, non-`Debug`, non-serde, exact-route scoped, and stores zeroizing values. Rendering receives only `isSet` and displays a fixed mask; it never receives value text or length. Submit consumes the vault into `BrowserReplaySecretSubmission`; cancel, route switch, replacement, and teardown zeroize it.

Checkpoint 9 supplies and tests this event/vault/projection/pane contract. Checkpoint 12 invokes it from the final workflow lifecycle; there is no temporary MCP operation and no secret-valued wire schema.

## Errors and limits

Use closed replay-secret error variants for invalid submission, stale authority, closed store, missing sidecar, and unsupported platform. Messages contain no caller data.

Limits are fixed and centrally defined:

- at most 32 secret inputs per replay;
- at most 128 bytes per input name, subject to existing safe-name validation;
- at most 16 KiB UTF-8 per value;
- exactly one submission per replay instance.

## Verification

Strict RED-to-GREEN tests must prove:

1. compile-time non-`Debug`/non-serde/non-`Clone` properties for submissions and editor vaults;
2. exact-name submission, one-shot transition, stale/cross-workspace rejection, and no mutation on invalid submissions;
3. synchronous store closure and zeroization probes on every terminal path, including an in-flight lease after cancellation;
4. generic-command sidecar forgery rejection and exact secure-route acceptance;
5. Agent queue ordering, target inspection, approval/denial, cancellation, and late-response fencing;
6. recording creates only an unset Secret input and journal/resources/errors contain no sentinel;
7. executor never places a sentinel in `BrowserAction`, command JSON, context, status, or Debug output;
8. prompt events and pane models expose names/set flags only and render a fixed mask;
9. existing replay, recording, host, gateway, unsupported-platform, and release gates remain green.

