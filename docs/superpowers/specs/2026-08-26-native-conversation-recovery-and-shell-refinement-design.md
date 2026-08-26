# Native Conversation Recovery and Shell Refinement

## Outcome

DevManager accepts a message immediately, displays locally persisted conversation history across
restarts, restores the exact provider conversation when possible, and keeps runtime attachment
errors separate from task and transcript state. The native rail, composer selectors, terminal,
activity indicators, and notification sound expose real state and behave consistently.

## Confirmed root causes

- Provider conversation identity already exists as `providerSessionId`/`ProviderSessionId` and is
  captured from correlated Claude and Codex `SessionStart` hooks. The repair must prove that this
  exact value persists and is supplied to restart restore; it must never synthesize or infer one.
- A provider reattachment error currently calls `mark_provider_restore_failed`, which promotes a
  runtime attachment problem into durable task failure. The cockpit then replaces locally stored
  history with an agent-start failure surface.
- Conversation facts are already persisted in the profile-scoped semantic journal. The client
  polls conversation pages, merges them with a linear duplicate scan and sort, clones the full
  cache, and reprojects the full timeline even when no fact changed.
- Terminal rendering is present and proven, but it requires one current active terminal resource.
  Failed restore leaves no such resource while the UI can still claim the terminal is live.
- The v0.4.1 idle reconciliation and notification sound implementation remains in the repository,
  but the native configured host runtime does not apply the admitted sound setting.
- Composer model, reasoning, and access controls click-cycle values. The task rail manually applies
  wheel delta with the wrong sign for the requested interaction and overlays its scrollbar on rows.

## Architecture

### 1. Persist and restore exact provider identity

`ProviderSessionId` remains distinct from PTY identity and is durable provider-session state. A
correlated current-generation `SessionStart` binds it to the exact agent session and active
resource. Snapshot, journal, and profile reload tests must prove the bound value round-trips and
that `ResumeExact` receives that value after restart.

No fallback may silently start a new conversation when a stored provider session ID exists. A
provider that explicitly lacks exact-resume support exposes a detached/unavailable attachment
state and permits a new conversation only through an explicit user action. A first turn without a
provider session ID may use the existing new-conversation path and bind the official ID when the
correlated hook arrives.

### 2. Separate durable task state, transcript state, and runtime attachment

A failed restore no longer marks an existing task failed. The exact task surface records a bounded
runtime attachment state: `Starting`, `Reconnecting`, `Live`, `Unavailable`, or `Exited`, plus a
safe diagnostic message. Durable task failure remains reserved for failure of the task operation
itself.

Persisted conversation facts remain visible under every attachment state. The composer stays
available when the host can safely enqueue a new turn or explicit reconnect; attachment state is
shown as a banner/status, not as a replacement transcript. Late restore results remain fenced by
task, agent, runtime generation, action epoch, and provider session identity.

### 3. Incremental conversation delivery and rendering

The semantic journal remains conversation truth. Add a bounded task-scoped change notification
carrying only the task identity and admitted journal high-water. It is separate from the PTY byte
stream. A client that sees a newer high-water fetches only facts after its admitted sequence.

`TaskConversationCache` admits monotonic pages without scanning every existing fact, exposes
whether facts actually changed, and retains a stable page/cursor view without cloning or sorting
the complete history for an empty delta. Timeline projection runs only for changed tasks and
preserves stable row identity and scroll anchoring while assistant output streams.

Submitting a message paints a pending user row immediately after local admission, then reconciles
it with the durable journal fact. A rejected command visibly marks or removes the pending row. No
UI path waits for provider discovery or exact restore before acknowledging the local gesture.

### 4. Terminal recovery

Terminal remains the provider's real PTY, never reconstructed from semantic conversation text.
Each task surface keeps the last admitted terminal snapshot. When the exact terminal resource is
live it is interactive; while reconnecting it is visibly read-only and labelled stale; when no
snapshot exists it shows an accurate reconnecting, unavailable, or exited state instead of
claiming a live terminal is waiting for output.

Task focus triggers a bounded current-resource terminal query without waiting for unrelated
provider restores. A successful exact restore immediately rebinds the matching terminal resource
and resumes delta delivery.

### 5. Real activity and notification sound

Task activity is derived from current-generation provider/session facts and process reconciliation:
starting or active work maps to Thinking/Working, a completed turn maps to Idle, a blocked question
or approval maps to Waiting, and an actual task/runtime error maps to Failed. Settled/Done remains
a separate user-controlled lifecycle; provider idleness never settles a task.

The native host applies the configured notification sound to `ProcessManager`. Sound plays once
for a validated current-session Working-to-Idle completion, using the existing v0.4.1 debounce and
foreground/background rules. Replayed history, restart reconciliation, stale generations, and
ordinary task selection do not play a sound.

### 6. Compact explicit composer selectors

Model, thinking/reasoning, and access use small popover dropdowns with explicit labelled choices,
selection state, keyboard navigation, click-away/Escape dismissal, and accessibility semantics.
They no longer cycle values. Selection continues to persist as the last-used choice and respects
project defaults when present.

### 7. Task rail and archived navigation

- Mouse-wheel input advances the task list in the user's expected direction.
- A fixed scrollbar gutter sits beside rows and never covers row hit targets or text.
- The Search row has no add button.
- A compact New task button sits beside All projects.
- The footer action is a distinct folder-plus New project button.
- Archived rows use the same task selection/focus path as active and settled rows. Opening one
  shows its persisted conversation and terminal snapshot; restore and permanent delete remain
  explicit separate actions.

## Performance and safety boundaries

- Local gesture acknowledgement does not depend on provider startup.
- Unchanged conversation high-water causes no transcript reprojection.
- Work for one task cannot block task-list queries, task focus, or another task surface.
- All page sizes, notifications, terminal snapshots, and diagnostics remain bounded.
- Installed DevManager process/profile are untouched; development watch mode remains in the
  current checkout.
- Existing multi-folder project, recursive workspace, settlement, archive, delete, and exact
  provider-identity invariants remain intact.

## Verification

Implementation uses focused RED/GREEN cycles and one broad Rust verification train at the end.
Acceptance includes:

1. A captured Codex and Claude provider session ID survives profile restart and is used for exact
   resume; a mismatch fails closed without creating a new conversation.
2. Restore failure leaves persisted history visible and reports Reconnecting/Unavailable rather
   than marking the task Failed.
3. Sending paints immediately, an unchanged high-water causes zero reprojection, and streamed
   deltas retain stable rows and follow behavior.
4. Terminal states accurately distinguish live, stale/reconnecting, unavailable, and exited, and
   a restored resource resumes real PTY output.
5. Current-generation Thinking-to-Idle updates the rail and plays exactly one configured sound;
   replay and stale generations are silent.
6. Dropdowns select explicit model/reasoning/access values and persist them.
7. Wheel direction, scrollbar gutter, New task/New project actions, and archived selection work
   through real mouse/keyboard interaction.
8. A live watch-mode restart and feature-by-feature sweep verifies real provider conversation,
   conversation history, task switching, terminal output, multi-pane focus, settled tasks,
   archived tasks, selectors, status, sound, and layout persistence.
9. Final formatting, serial library suite, all-target compiler check, process cleanup, installed
   PID/start-time, and production config/remote hash checks pass once after edits freeze.

## Out of scope

- Replacing the semantic journal with provider-owned history.
- Rendering conversation text as terminal output.
- Replacing DevManager's multi-folder project model.
- Silently creating a fresh provider conversation when an exact stored identity cannot resume.

