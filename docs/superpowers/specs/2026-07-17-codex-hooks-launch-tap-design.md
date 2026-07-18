# Codex Hooks Launch Tap — Design

Date: 2026-07-17
Status: Draft, pending user approval

## Problem

Launching a Codex terminal from DevManager today rewrites the configured command
(`npx -y @openai/codex@latest --yolo`) into something very different:

```
'\\?\C:\...\npx.cmd' '-y' '@openai/codex@0.144.5' '--yolo' '--remote' 'ws://127.0.0.1:<port>' '--remote-auth-token-env' 'DEVMANAGER_CODEX_BRIDGE_TOKEN'
```

This is the loopback WebSocket bridge (`src/ai/codex_bridge.rs`): DevManager
spawns a hidden second process (`codex app-server --listen stdio://`), puts a
one-client WebSocket bridge in front of it, and forces the visible TUI to
connect through that bridge so DevManager can tee the server→TUI frames into
the semantic journal that powers the remote mobile view.

Costs of this architecture:

- **`codex resume` and the in-TUI session picker do not work** in bridged
  sessions — the TUI is chained to a freshly spawned backend behind a one-shot
  bridge. This is the driving complaint.
- The launched command is unrecognizable versus what the user configured
  (resolved npx path, per-launch version pinning, injected `--remote` flags).
- Two Codex processes per terminal; version pinning and multi-probe launch
  preparation exist only to keep them in sync.

Key facts established during investigation:

- The bridge is **observe-only**. DevManager never writes a protocol message
  through it; remote approvals/replies are delivered as PTY keystrokes. Any
  mechanism that can *observe* the session can replace the bridge with no loss
  of control-path functionality.
- Codex ≥ 0.144 has a Claude-style **hooks system** (`PermissionRequest`,
  `PreToolUse`, `PostToolUse`, `SessionStart`, `UserPromptSubmit`, `Stop`, …).
  Hook command payloads include `session_id`, `cwd`, `tool_name`,
  `tool_input`, `tool_use_id`, and `transcript_path` — the exact rollout file
  for the session.
- Codex writes **rollout files** (`~/.codex/sessions/**/rollout-*.jsonl`)
  incrementally during every session, including bridged ones today.
- The WebView2 companion pane is a separate feature with its own pipeline; it
  does not consume the semantic journal and is unaffected by this design.

## Decision (user-approved direction)

Replace the Codex bridge with **Option 1: hooks + rollout tailing**.

- Launch the user's configured command with only transparent, standard `-c`
  config overrides appended. No `--remote`, no sidecar, no npx path rewriting,
  no version pinning. Resume works.
- Accepted trade-off: assistant text in the remote view updates per completed
  item / delta batch (rollout granularity) instead of token-by-token.
- Claude scope: **audit + align** — keep the `--settings` overlay mechanism,
  verify nothing is degraded, unify both providers on one documented relay
  pattern.

## Architecture

### Launch (Codex)

`prepare_codex_adapter` and the bridge are replaced by a simple launch
preparer:

1. Tokenize the configured command only to validate it and append arguments
   safely (reuse the existing tokenizer/quoting helpers).
2. Run one capability probe (`codex --help`, cached per executable+version as
   today) to confirm hooks support before injecting anything.
3. Append `-c` overrides registering DevManager's hooks, each pointing at
   `devmanager codex-hook-relay --url <loopback endpoint> --nonce <per-launch secret>`:
   - `SessionStart` — binds the terminal to its `transcript_path` and starts
     the rollout tailer.
   - `PermissionRequest` — instant `Question` events / needs-input pushes.
   - `PreToolUse` / `PostToolUse` — tool cards (payload includes `tool_input`,
     richer than the bridge provided).
   - `UserPromptSubmit`, `Stop` — user messages and lifecycle status.

The executable is invoked exactly as the user wrote it (`npx`, `codex`, etc.);
no path resolution or version pinning is needed because there is only one
process.

### Trust

Codex gates unmanaged hooks behind a persisted trust hash. Resolution
(investigated 2026-07-17): pre-registering the trust hash would require
replicating Codex's internal hash computation and writing into the user's
global `CODEX_HOME` state — a private interface and exactly the kind of
global-config mutation this design rejects. Therefore DevManager appends
`--dangerously-bypass-hook-trust` (verified present in 0.144.5, probed via
`--help` before injection). Caveat, documented in the settings UI help text:
this also unblocks the user's own untrusted project hooks for DevManager
launches. Also verified: the `hooks` feature is stable and enabled by default
in 0.144.5, and `-c hooks.<Event>=[...]` is the accepted override key path.

### Capture

Two producers feed the existing semantic journal (`SemanticEventDraft` →
`SemanticJournalStore`), replacing the bridge feed:

- **Hook relay** — new `codex-hook-relay` CLI subcommand plus a registry
  modeled directly on `src/ai/claude_hooks.rs`: loopback-only endpoint,
  per-launch nonce, generation fencing so superseded sessions cannot publish.
  Produces `Question`, `Tool`, `UserMessage`, `Status` events.
- **Rollout tailer** — new component tailing the session's `transcript_path`
  JSONL. Maps response items to `AssistantMessage`, `Reasoning`, `Command`,
  and `Diff` drafts with the same bounded-memory discipline as the current
  reducer (per-item byte caps, total caps, truncation markers). Tolerant of
  malformed/unknown lines (the rollout format is Codex-internal and may
  drift). Handles resume: a resumed session's `SessionStart` hook re-delivers
  the correct `transcript_path`, whether Codex appends to the old file or
  starts a new one.

The remote reply path (keystroke injection into the PTY) is unchanged.

### Claude audit + align

- Add tests asserting `--resume`, `--continue`, and arbitrary user flags pass
  through the overlay untouched.
- Extract the shared relay pattern (loopback endpoint + nonce + injection +
  degraded fallback) so Claude and Codex use one documented code path.
- Document the exact injected arguments for both providers in the settings UI
  help text so the launched command is never a surprise.

### Removal

Deleted: app-server sidecar spawn, WebSocket bridge
(`serve_one_loopback_client*`), `--remote` injection, version pinning, npx
path resolution, bridge activation/degradation machinery, and
`CodexSemanticReducer`'s app-server-protocol mapping (superseded by the
rollout mapping; reusable pieces like text bounding move to shared code).

Unchanged: semantic vocabulary and journal, remote PWA, push/attention logic,
keystroke reply path, companion pane.

## Error handling

A failed tap never blocks or alters the user's launch:

- Probe finds no hooks support (older Codex) → launch verbatim; remote view
  degrades to the raw-terminal fallback. (Best-effort cwd-matched tailing was
  considered and dropped as YAGNI — without the SessionStart hook there is no
  reliable session↔file binding, and the supported path is current Codex.)
- Relay registration or overlay preparation fails → launch verbatim, adapter
  reports Degraded (existing health surface).
- Rollout file missing/rotated/unparseable → keep hook-driven events; surface
  adapter health as degraded rather than failing the session.
- Nonce/generation fencing identical to Claude's so stale relaunches cannot
  publish into a newer session's journal.

## Testing

- Unit: override builder (exact argument strings per shell/quoting), rollout
  item → semantic event mapping (fixture JSONL: streaming deltas, truncation,
  rotation, malformed lines), hook ingestion (nonce fencing, out-of-order and
  duplicate events).
- Integration (behind the existing real-binary probe pattern): launch real
  `codex`; assert the command contains no `--remote`, the session lands in the
  resume picker data, and the journal receives hook + rollout events.
- Manual QA: launch from the Codex button; verify `codex resume` lists and
  resumes the session; verify a permission prompt appears live in the phone
  view and can be answered from there.
