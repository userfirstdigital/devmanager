# Build-Bound PWA Draft Handoff Design

## Goal

Preserve exact drafts through forced compatible-bundle recovery without allowing the old incompatible page to consume the handoff, and never return handoff text unless its one-time storage removal or rewrite is verified.

## Chosen approach

The handoff payload gains the received host build ID as `targetBuildId`. Staging and exact-safety verification require that target together with the existing runtime ID and exact draft record. Zustand stores the target build ID, rather than a separate readiness boolean, so one value identifies both why the draft is temporarily recoverable and which stored handoff must exist.

The other considered approaches were an activation marker written by the service worker and a separate recovery nonce. An activation marker adds worker/page coordination even though every bundle already has an immutable compiled build ID. A separate nonce still needs build binding and creates another lifecycle to reconcile. Binding directly to the existing build ID is smaller and authoritative.

## Data flow

On `buildMismatch`, the old page stages `{ version, targetBuildId, runtimeInstanceId, drafts }`, verifies the exact serialized write, stores `targetBuildId` in Zustand, and requests that compatible build. PWA safety treats drafts as recoverable only while the stored payload exactly matches that target, runtime, and all current non-empty drafts. Ordinary updates have no target and remain blocked.

`loadDraft` compares the payload target with the running bundle's compiled `CLIENT_WEB_BUILD_ID`. A remount or navigation in the old page may load the ordinary local draft, but it cannot alter the build-bound handoff, so recovery remains ready. A different non-target build also leaves the handoff untouched. Only the matching bundle may attempt one-time consumption.

## Storage integrity

Removing the final handoff entry is successful only if a read-back confirms the storage key is absent. Rewriting a remaining multi-draft payload is successful only if a read-back exactly matches the serialized replacement. Consumption has three outcomes: no consumable matching-build entry, verified consumption with text, or integrity failure. The integrity-failure outcome returns `null` from `loadDraft` and does not fall through to local storage, preventing a stale prompt from being resurrected while the handoff remains unconsumed.

Lifecycle pruning and removal preserve `targetBuildId` on rewrites. Invalid, mismatched, or inaccessible storage never grants the PWA draft exemption.

## Tests

- An old-build remount returns only the ordinary local draft and leaves the target-build handoff exactly ready.
- A wrong build cannot consume or mutate the handoff.
- The matching build consumes the handoff entry once.
- A failed final removal and a failed multi-entry rewrite each return no text and retain the original handoff.
- Store tests prove the received host build is staged before recovery and retained as the safety target.
- PWA safety tests prove target/runtime/draft exactness remains mandatory.
- Final verification covers the full web suite, typecheck, production audit, two byte-compared builds, the Rust web suite, formatting, and diff checks.
