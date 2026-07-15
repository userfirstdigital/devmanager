# Native Transcript and Process Monitor Design

**Date:** 2026-07-14
**Status:** Approved product direction; written checkpoint awaiting review
**Scope:** Complete the native-first web session experience and repair/redesign the desktop process monitor without changing native DevManager's ownership of processes.

## 1. Context

The first native-mobile implementation established the right shell, stable session identity, automatic restoration, host-authoritative state, semantic journals, and raw-terminal fallback. Live testing exposed two unfinished interaction layers:

1. Claude and Codex sessions still present routine lifecycle and terminal output as a stack of expandable cards. A successful response can be buried inside a large terminal-redraw dump because degraded provider output is appended as plain text without applying cursor movement.
2. The desktop process monitor closes when a row is clicked. The row click propagates through the modal frame to the backdrop and then reaches the underlying sidebar. Its rows also omit the project name and do not make the session kind sufficiently explicit.

The product direction remains provider-first and native-first. Terminal bytes are an execution substrate and diagnostic fallback, not the primary information architecture.

## 2. Goals

- Make Claude and Codex read like a native conversation, with one continuously updating assistant response rather than terminal-shaped output cards.
- Make tool activity useful but visually secondary to the conversation.
- Make server, shell, and SSH output readable as continuous native logs or command timelines.
- Keep raw terminal access available as an explicit advanced mode.
- Recover silently from ordinary phone suspension and short network interruptions.
- Make the desktop process monitor stable, compact, project-aware, and explicit about Server, SSH, Claude, Codex, or Shell identity.
- Preserve seamless restoration from native-host state and discard browser state when the native host runtime changes.

## 3. Non-goals

- Replacing the PTY or moving process ownership into the browser.
- Persisting session history after native DevManager restarts.
- Inferring a complete semantic transcript from arbitrary full-screen TUI bytes when the provider adapter is unavailable.
- Removing the raw terminal.
- Adding a second process-state store for the process monitor.
- Rebuilding unrelated desktop navigation or project configuration screens.

## 4. AI session experience

### 4.1 Conversation-first timeline

The default AI view is a single vertical transcript:

- User prompts use compact, right-aligned message bubbles.
- Assistant prose uses the available width, native typography, and Markdown rendering.
- Streaming deltas update one assistant message in place using its stable message identity. They do not create new cards or duplicate already-rendered text.
- Assistant Markdown supports paragraphs, headings, lists, task lists, links, inline code, fenced code, tables, and block quotes. Code and tables may scroll horizontally inside their own bounded surface; the whole page must not scroll horizontally.
- Markdown is rendered through React elements with raw HTML disabled. Links opened outside the app use safe target and rel attributes.
- Questions, permission requests, and failures remain expanded and prominent because they require user action.

The renderer must not show routine `starting`, `running`, `ready`, `native view restored`, or equivalent terminal-mode messages as timeline cards. Current state belongs in the compact session header.

### 4.2 Activity disclosure

Tool calls, command executions, diffs, reasoning summaries, and tool results between two conversational messages form one activity group.

The collapsed form is a single line such as `4 actions · Read 2 files · Edited 1 file`. Expanding it shows compact rows with:

- action icon and concise label;
- running, succeeded, failed, or approval-needed state;
- elapsed time when known;
- bounded detail for commands, output, and diffs.

Successful activity is collapsed by default. Running, failed, permission, and question rows are visible without an extra tap. Verbose terminal output is never promoted above assistant prose.

### 4.3 Provider authority and adapter health

Structured provider events are authoritative for AI conversation content:

- Codex uses the supervised app-server bridge and normalizes thread, turn, item, delta, tool, approval, diff, plan, usage, and error notifications.
- Claude uses the existing supported hook relay and normalizes message, tool, permission, question, and lifecycle events.
- The host continues to own the PTY and process lifecycle. Provider integration observes and projects; it does not become process authority.

Adapter health is observable in diagnostics and tests. A bridge startup or negotiation failure must record a concise reason before falling back. The ordinary transcript may show one compact `Limited transcript detail` notice, but must not expose repeated startup/status cards.

The Codex launch path must be tested against the installed CLI's supported `app-server --listen`, `--remote`, and remote-auth behavior. Capability detection remains runtime-based rather than version-string-based, and a failed sidecar never prevents the user's original Codex command from launching.

### 4.4 Degraded projection

Raw TUI chunks must not be concatenated into the native timeline. ANSI stripping alone is insufficient because full-screen TUIs redraw content with cursor movement.

When a provider adapter is degraded, the host uses terminal-screen reconciliation:

1. Apply PTY output to the existing terminal model, which already understands cursor movement and screen replacement.
2. Project stable visible content from screen/scrollback snapshots rather than directly from each byte chunk.
3. Replace the current in-progress fallback block when the terminal screen changes; append only lines proven to have entered stable scrollback.
4. Remove recognized persistent TUI chrome and repeated identical frames from the native projection.
5. Bound fallback content and retain an explicit truncation marker.

This fallback provides readable best-effort text, not provider-level semantics. Raw Terminal remains one tap away for any interaction the reconciler cannot represent.

## 5. Server, shell, and SSH experience

### 5.1 Server sessions

A server session uses a compact sticky header with status, port/resource summary, and start/stop/restart controls. Below it, output is a continuous wrapping log surface rather than an `Output` accordion.

- New log lines stream at the bottom.
- ANSI styling is normalized to safe text styling.
- stdout and stderr remain distinguishable without placing each chunk in a separate box.
- The view follows new output only when the user is already near the bottom.
- A visible `New output` affordance returns to the bottom after the user has scrolled up.
- Crash and exit summaries remain prominent; routine state transitions remain in the header.
- Separate runs have clear boundaries so a restarted server does not visually merge with its previous process.

### 5.2 Shell and SSH sessions

Shell and SSH use the same continuous output surface, enhanced by OSC 133 command boundaries when available. Commands appear as concise command rows with associated output and exit state. SSH connection identity is redacted but clear, and disconnect/error events remain visible.

Alternate-screen or mouse-dependent applications can automatically require Raw Terminal for shell/SSH. Claude and Codex do not switch to Raw Terminal merely because their own TUI enables those capabilities.

## 6. Reconnection and restoration

The currently rendered route and timeline remain visible during ordinary reconnect attempts. The app must not flash a large warning during normal WebSocket establishment or a brief phone suspension.

- `connecting` is silent.
- A closed socket starts a seven-second offline timer while automatic wake/retry continues.
- If connectivity returns before the timer expires, no reconnect message is shown.
- After seven seconds, show a small non-blocking `Offline · reconnecting` indicator in the session header or app chrome and disable mutations.
- Clear the indicator immediately after the authoritative snapshot and journal reconciliation complete.
- No Resume, Reconnect, or Take Control button is introduced.

The native host remains authoritative. Warm returns reconcile by runtime ID, stable session key, and journal sequence. A changed runtime ID clears stale browser journals, drafts, and restoration state and shows the host's new state, including an empty Sessions screen when appropriate.

## 7. Desktop process monitor

### 7.1 Interaction correction

The modal backdrop closes the monitor only when the pointer-down originates outside the panel. Pointer-down inside the panel must call GPUI propagation stop before any row or button action runs.

- Clicking a collapsed session row toggles its details and leaves the monitor open.
- Clicking an expanded session row collapses it and leaves the monitor open.
- Clicking Stop performs only the stop action and must not also toggle the row or close the monitor.
- Clicking the actual backdrop closes the monitor.
- A click inside the monitor must never activate a sidebar item or other control underneath it.

### 7.2 Project-aware view model

The renderer consumes a pure `ProcessMonitorEntry` view model derived from the current `AppState` project configuration plus `RuntimeState`. It does not duplicate runtime state.

Each entry contains:

- stable session ID;
- primary session label;
- project ID and resolved project name, with `Unknown project` only when configuration no longer resolves;
- normalized type: `Server`, `SSH`, `Claude`, `Codex`, or `Shell`;
- concise runtime status;
- PID, process count, CPU, and memory;
- ordered child-process details;
- stop capability.

Type derivation uses explicit session/tab/command metadata first. Command-name guessing is only a compatibility fallback and is covered by tests.

### 7.3 Compact layout

The monitor is a dense native sheet, not a collection of large cards:

- header contains title, session count, total memory, and close action in one compact row;
- collapsed rows target 48-56 logical pixels;
- first row line shows session label, type badge, and status;
- second line shows project name, PID/process count, CPU, and memory in a compact metadata run;
- Stop is a small explicit trailing action with an accessible label;
- expansion adds child-process rows directly below the parent without repeating the parent summary;
- the list scrolls inside the sheet while header and totals remain visible;
- typography and contrast remain readable at high density.

Sorting is deterministic: active/problem states first, then project name, type, session label, and stable ID. The current aggregate session count and memory total remain visible in the desktop status bar.

## 8. Implementation boundaries

### 8.1 Host presentation

- Preserve the existing semantic journal and stable event envelope.
- Complete Codex bridge diagnostics and structured-event coverage before relying on fallback projection.
- Replace byte-chunk `PlainTextProjector` behavior for full-screen AI output with terminal-model snapshot reconciliation.
- Keep server/shell line projection separate from AI screen reconciliation because ordinary line-oriented streams should append rather than replace.

### 8.2 Web presentation

- Add a pure timeline presentation model that groups normalized semantic events into user messages, assistant messages, activity groups, actionable events, and continuous log runs.
- Keep grouping logic out of React components so it can be fixture-tested.
- Render AI and server views with dedicated layouts rather than passing every event through the generic compact-card renderer.
- Add a safe Markdown renderer based on `react-markdown` and `remark-gfm`, with raw HTML intentionally unsupported.
- Move routine state and adapter indicators into session header selectors.
- Replace immediate reconnect-banner logic with a timer-driven connection presentation state using injectable/fake time in tests.

### 8.3 Desktop monitor

- Build and unit-test `ProcessMonitorEntry` derivation separately from GPUI rendering.
- Pass project configuration into the monitor derivation at the existing render boundary.
- Stop event propagation at the modal panel and at nested action buttons where required.
- Preserve existing process-tree collection and stop actions.

## 9. Error handling

- Provider bridge failure: record diagnostic reason, mark adapter degraded, launch the original command, and present one compact fallback notice.
- Malformed structured event: skip only that event, retain bridge forwarding, and record diagnostics.
- Fallback screen reconciliation failure: retain last good native snapshot and keep Raw Terminal available.
- Journal rollover: replace from the oldest available sequence and show one history-truncation notice.
- Server log rollover: preserve run boundaries and show one explicit earlier-log truncation line.
- Missing project configuration in the process monitor: show `Unknown project` without dropping the live process entry.
- Stop failure: keep the monitor open and display the failure on that row.

## 10. Test strategy

Implementation follows test-first slices.

### Rust tests

- Codex launch and bridge fixtures cover healthy startup, supported current flags, notification normalization, malformed messages, sidecar failure, diagnostic reason, and unchanged-command fallback.
- Terminal reconciliation fixtures cover cursor-up redraw, clear-screen, carriage-return progress, repeated frames, scrollback commitment, Unicode, TUI chrome filtering, and bounded retention.
- Existing line-oriented projector fixtures continue to cover ANSI chunk boundaries and server logs.
- Process-monitor view-model tests cover every type, duplicate labels in different projects, missing projects, deterministic sorting, totals, expansion identity, and child ordering.
- A focused GPUI interaction regression verifies or structurally guarantees that panel and Stop pointer events consume propagation while backdrop pointer events close.

### React/Vitest tests

- AI fixtures render one streaming assistant response in place and never an `Output`, `Starting`, `Running`, or `Native view restored` card.
- Activity fixtures group consecutive tools, keep failures/action requests visible, and retain bounded detail.
- Markdown fixtures cover code, links, tables, task lists, raw-HTML suppression, and long-content overflow.
- Server fixtures render continuous stdout/stderr logs with run boundaries and follow-output behavior.
- Reconnect tests use fake timers to prove no indicator before seven seconds, one indicator afterward, immediate clearing after reconciliation, and no manual resume control.
- Host-runtime-change tests prove stale timeline and draft state are discarded.

### Live acceptance

Run the hot-load DevManager build on a separate port and validate at an iPhone viewport plus the native desktop app:

1. Send a real Codex prompt and confirm user bubble, one streaming Markdown response, grouped actions, no terminal redraw dump, and healthy adapter diagnostics.
2. Exercise Claude structured events when account availability permits; separately verify its rate-limit/error presentation.
3. Start, stream, stop, and restart a real server; confirm continuous logs, run boundaries, compact controls, and no output accordion.
4. Open shell and SSH output, confirm native wrapping/copying, and verify explicit Raw Terminal fallback.
5. Reload, background/foreground, and interrupt connectivity; confirm exact session restoration and silent short reconnects.
6. Restart the native host; confirm the browser follows the new blank/current host state without resurrecting stale history.
7. Open the desktop process monitor; click every parent row and Stop control; confirm the modal remains open, details toggle, and no underlying control activates.
8. Confirm each monitor entry names its project and shows an explicit Server, SSH, Claude, Codex, or Shell badge at compact density.

## 11. Acceptance criteria

The work is complete when:

- A successful AI turn is readable from prompt through final response without opening an output card or reading terminal artifacts.
- Assistant streaming updates one message and renders safe Markdown.
- Routine status cards no longer dominate the timeline.
- Server output is a continuous native log rather than boxed accordions.
- Brief phone reconnects are silent and sustained offline state is compact and automatic.
- Raw Terminal remains available and functional.
- The desktop process monitor cannot click through or close from an internal row click.
- Every process-monitor row clearly identifies project and type while fitting more sessions on screen.
- Automated tests pass, the production web bundle is regenerated and embedded, and the hot-load browser/native acceptance matrix passes before merge.
