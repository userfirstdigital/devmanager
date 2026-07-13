# PWA Review Remediation Design

## Goal

Close the three remaining Task 7 compatibility and lifecycle gaps without changing ordinary service-worker update semantics: recover automatically when both the web protocol and bundle change, preserve an exact runtime-scoped draft across that forced recovery, and prevent an abandoned attachment read from restoring stale PWA safety state.

## Handshake precedence

The WebSocket remains fail-closed until a valid first `hello`. A differing `webBuildId` takes precedence over a differing `protocolVersion`, because the matching embedded bundle is the only code that can authoritatively interpret the host protocol. The socket still closes immediately and sends no resume, lease, or mutation frames. If the build IDs match but protocol versions differ, the existing protocol diagnostic remains terminal.

This keeps the existing `WsHelloFailure` union small: a simultaneous mismatch reports `buildMismatch`; an equal-build protocol mismatch reports `protocolMismatch`.

## Exact draft handoff

Ordinary PWA updates continue to treat every non-empty draft as unsafe. A build-mismatch failure may instead stage the current Zustand drafts into a separate `sessionStorage` handoff before requesting compatible-bundle recovery.

The handoff payload contains a format version, the exact `runtimeInstanceId`, and all non-empty drafts keyed by stable session key. It is bounded to the existing 32 KiB UTF-8 limit per draft and a 512 KiB serialized payload. Staging succeeds only when every draft fits, the browser accepts the write, and a read-back exactly matches the source payload. Missing runtime identity, quota/security failures, truncation, or invalid read-back fail closed and do not request a reload.

While the stored handoff exactly matches the current runtime and in-memory drafts, the PWA safety reader treats only those drafts as recoverable. Pending mutations, selected attachments, and attachment reads remain blocking. This lets the existing cross-tab nonce gate and local reload gate operate unchanged; normal updates never create a handoff and therefore remain blocked by drafts.

After the compatible bundle loads, `loadDraft` returns and removes the matching handoff entry once. It falls back to the existing bounded `localStorage` draft otherwise. Removing, pruning, or changing runtime also removes corresponding handoff data. The session screen then restores the exact text into Zustand through its existing draft-load path.

## Attachment-read cancellation

Composer unmount cleanup increments the same generation used for scope changes, clears its attachment refs, and publishes an empty safety state. Any pending `arrayBuffer()` continuation then fails the generation check and cannot publish selected/loading state after the parent cleared it. Scope changes during a read retain the same invariant.

## Testing

- WebSocket tests cover a simultaneous build/protocol mismatch, assert `buildMismatch`, and assert no protocol-ready traffic.
- Store and draft-storage tests cover successful build recovery with an in-memory draft, exact one-time restoration, ordinary-update blocking without a staged handoff, and fail-closed storage errors.
- Composer tests hold an attachment read unresolved across both scope change and unmount, then prove resolving it cannot republish stale safety.
- Final verification runs the full web suite, typecheck, audit, two byte-compared production builds, the scoped `remote::web` Rust suite, formatting/diff checks, and a clean-worktree check.
