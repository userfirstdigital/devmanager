# LLM Conversation Resume Design

## Summary

DevManager currently restores Claude and Codex tabs as terminal workspaces, but
it deliberately clears the saved PTY session ID. Selecting one of those tabs
therefore starts a new provider conversation. DevManager will instead persist
the provider's own conversation/session ID separately from the disposable PTY
ID and use that provider ID to resume the exact conversation represented by the
open tab.

## Goals

- An open Claude or Codex tab resumes its exact provider conversation after
  DevManager restarts.
- A newly created Claude or Codex tab still starts a fresh conversation.
- Closing an LLM tab forgets its provider-conversation association.
- Existing saved tabs created before this feature remain usable.
- Resume failures remain visible and never silently create a different
  conversation.
- Provider identity capture and persistence stay outside terminal rendering and
  other hot paths.

## Non-goals

- Transcript-directory scanning or "latest conversation in this folder"
  inference.
- Resuming conversations whose DevManager tab was closed.
- Adding a user-visible restart action for LLM tabs.
- Changing server restart behavior.
- Reintroducing the removed Codex remote bridge.

## Identity Model

`SessionTab` gains an optional `provider_session_id`, serialized as
`providerSessionId` in `session.json`.

The identities have different lifetimes:

- `SessionTab.id` identifies the durable DevManager tab.
- `SessionTab.pty_session_id` identifies one disposable terminal runtime.
- `SessionTab.provider_session_id` identifies the durable Claude or Codex
  conversation.

Startup restoration continues clearing `pty_session_id` for Claude and Codex
tabs, but preserves `provider_session_id`. Removing the tab removes all three
identities together through the existing tab-removal path.

## Provider-ID Capture

Provider hooks are the authority. DevManager does not infer IDs from the
working directory, output text, timestamps, or transcript filenames.

- Claude's accepted, current-generation `SessionStart` hook publishes the
  official bounded `session_id`.
- Codex's existing `SessionStarted(CodexSessionBinding)` event supplies the
  official bounded `session_id`.
- `ProcessManager` binds the provider ID to the matching internal PTY runtime
  only after the existing nonce/generation correlation accepts the hook.
- `SessionRuntimeState` exposes the optional provider ID and bumps the normal
  runtime revision only when the value changes.
- The existing background maintenance pass projects changed runtime provider
  IDs into their matching `SessionTab` entries and saves `session.json`.

This keeps hook threads independent from `AppState` and keeps disk persistence
off the hook callback and render paths.

## Launch Semantics

Launch mode is derived from the saved tab state:

| Tab state | Claude launch | Codex launch |
| --- | --- | --- |
| New tab: provider ID absent, PTY ID present | configured command unchanged | configured command unchanged |
| Restored tab: provider ID present | append `--resume <id>` | append `resume <id>` |
| Legacy restored tab: both IDs absent | append `--resume` | append `resume` |

The launch adapter validates provider IDs as bounded, single-line command
arguments and shell-quotes them for the configured interactive shell. Unsafe or
malformed persisted IDs produce a launch error instead of running a modified
command.

The normal Claude settings-overlay injection and Codex hooks injection continue
after resume arguments are added, so provider hooks capture the authoritative
ID for fresh, exact-resume, and picker-resume launches.

## Legacy Migration

Older `session.json` files deserialize with `providerSessionId = null`.
A restored LLM tab is distinguishable from a new tab because startup
restoration has cleared its PTY ID. On first selection DevManager opens the
provider's normal resume picker. The selected conversation's `SessionStart`
hook then captures and persists its exact ID, so subsequent launches resume
directly.

No version marker or one-time migration file is required.

## Failure Behavior

- If an exact provider ID no longer exists or the provider rejects resume, the
  provider's error remains visible in that tab's terminal.
- DevManager retains the saved provider ID and never retries as a fresh
  conversation.
- If hook capture is unavailable, the terminal still works, but the tab cannot
  gain or update a durable provider ID; existing adapter-health diagnostics
  continue reporting the hook degradation.
- Closing the failed tab and creating a new one is the explicit way to start a
  fresh conversation.

## Verification

Focused tests will cover:

- backward-compatible `SessionTab` serialization;
- provider-ID projection into only the matching LLM tab;
- PTY cleanup preserving provider identity;
- fresh, exact-resume, legacy-picker, and malformed-ID launch commands for both
  Claude and Codex;
- current-generation Claude and Codex hook events binding the provider ID to
  the correct runtime without duplicate revision churn;
- persisted session state keeping provider identity while stripping PTY
  identity.

The final gate is `cargo test --lib -- --test-threads=1`, followed by checks that
no Cargo, Rust compiler, or test harness remains and that the installed
DevManager PID/start time plus production `config.json` and `remote.json`
fingerprints are unchanged.
