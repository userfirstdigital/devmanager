# Native Mobile Web Interface Design

**Date:** 2026-07-13  
**Status:** Approved for implementation  
**Scope:** DevManager's embedded remote web client on iPhone first, responsive through desktop  
**Product decision:** Build one coordinated native-feeling interface for Claude, Codex, servers, shell, and SSH. Keep the PTY as the host execution substrate and expose the raw terminal only when terminal semantics are genuinely required.

## 1. Product intent

The installed DevManager web app should feel like an iPhone work app that happens to control development sessions. It must not feel like a desktop terminal squeezed into Safari.

The primary experience is semantic:

- Claude and Codex read like native conversations with compact tool, diff, status, and question cards.
- Server sessions read like live service consoles with status, controls, and searchable log rows.
- Shell and SSH sessions read like command timelines with separate commands, output, exit state, and a native composer.
- Full-screen terminal programs automatically open the existing xterm surface for as long as terminal-grid interaction is needed.

The native DevManager process remains the only source of runtime truth. The browser is a projection and controller, never a second owner of processes or transcripts.

## 2. Success criteria

The release is successful when all of the following are true:

1. An iPhone user can install DevManager to the Home Screen and spend an extended work session in it without needing the desktop UI.
2. Returning after app switching, locking the phone, or a network interruption reconnects automatically and restores the same session without a resume or take-control button.
3. Closing and reopening the installed PWA restores the last valid host session. If the native host restarted, the PWA accepts the host's new blank runtime instead of resurrecting browser state.
4. Every normal prompt is entered through a native HTML textarea, so iOS dictation, selection, autocorrection, paste, and the software keyboard work naturally.
5. Project identity is prominent anywhere sessions can be confused, especially the active/recent list and session navigation title.
6. Claude and Codex are readable without understanding terminal escape sequences or terminal layout.
7. Server, shell, and SSH output wraps to the viewport and supports native selection and copy.
8. The existing raw terminal remains available automatically for alternate-screen or mouse-reporting applications and as a resilience fallback.
9. No SSH password, private key, GitHub token, command environment secret, or other unnecessary desktop configuration is serialized to the browser.
10. Actionable background events can notify an installed iPhone PWA after the user explicitly enables notifications.

## 3. Non-goals

- A Swift/SwiftUI App Store application.
- Replacing portable-pty or changing how DevManager owns child processes.
- Persisting AI transcripts or terminal journals across a native DevManager restart.
- Recreating every desktop configuration editor on mobile in this release.
- Rendering arbitrary curses/TUI applications as semantic native controls.
- Synchronizing independent browser drafts or cursors between multiple simultaneous writers.
- Making insecure LAN HTTP support installability, service workers, or Web Push. Those platform features require a secure context.

## 4. Experience model

### 4.1 App shell

The PWA uses a single-column iPhone shell with three bottom destinations:

- **Sessions**: active and recent work, sorted by host-observed activity.
- **Projects**: project-oriented launcher and complete configured session inventory.
- **Settings**: appearance density, notifications, installation/security diagnostics, and an advanced terminal preference.

On wider viewports, the destinations become a persistent sidebar and the selected content uses the remaining pane. The information architecture and route identity remain the same at every width.

The shell uses:

- iOS system font stack and Dynamic Type-friendly rem sizing;
- 44-point minimum interactive targets;
- `env(safe-area-inset-*)` padding;
- restrained separators and grouped surfaces instead of desktop bordered panels;
- native momentum scrolling and overscroll containment inside the active timeline;
- no hover-only affordances;
- high-contrast light and dark themes following the system by default;
- visible focus styles and reduced-motion support.

### 4.2 Sessions home

The Sessions destination is the default cold-launch surface when no restorable session exists.

Each row shows, in priority order:

1. session label;
2. project name;
3. kind icon and concise state such as Thinking, Ready, Running, Crashed, or Disconnected;
4. relative last activity;
5. an unread/action-required indicator when applicable.

Sections are:

- **Needs attention**, only when non-empty;
- **Active**, for live or thinking sessions;
- **Recent**, for other sessions that existed during the current host runtime.

Recent ordering exists only in host memory. A host restart creates a new runtime instance and therefore a blank recent list.

### 4.3 Projects

Projects are grouped native list sections. A project detail screen shows:

- open Claude and Codex tabs;
- configured server commands with live state and start/stop/restart actions;
- available SSH connections by redacted label/host metadata;
- launch actions for a new Claude or Codex session.

Project name is always included in the navigation title or subtitle when a session is open.

### 4.4 Session screen

Every session screen has the same structural anatomy:

- compact navigation bar with Back, session label, project subtitle, and an overflow menu;
- connection/activity state presented unobtrusively in the title area;
- scrollable semantic timeline;
- sticky native composer or context-appropriate server controls above the safe-area inset;
- a short reconnect overlay only when the browser has no current host snapshot.

There is no permanent status bar, desktop drawer, terminal key strip, or explicit Take Control control in the primary interface.

### 4.5 Composer

The composer is a real `<textarea>` with `font-size: 16px` or larger to avoid iOS focus zoom. It supports:

- iOS keyboard dictation automatically;
- multiple lines;
- native selection, paste, undo, and autocorrection;
- image attachment from Photos, Files, camera, and clipboard for supported AI sessions;
- Return to send when the user chooses that setting, with a visible send button as the unambiguous default;
- an interrupt/stop affordance while an AI is actively running;
- growing height capped so the timeline remains visible.

Drafts are stored locally under `(runtimeInstanceId, stableSessionKey)`. They survive page suspension and app switching, expire after seven days, and are discarded when the host runtime instance changes or the target session no longer exists. Draft text is the only session content intentionally stored by the browser.

Image attachments reuse the existing authenticated WebSocket image-paste path. The browser accepts PNG/JPEG up to 5 MiB, sends the bytes only after the target AI session and writer lease are validated, and waits for host acknowledgement. The host stages the file beneath the target workspace's `.devmanager/pasted-images` directory (or its existing temp fallback), inserts only that session's `@path` reference, and removes staged files older than 24 hours. Attachment bytes and paths never enter the semantic journal or browser cache.

### 4.6 AI density

Default density is **Calm**:

- user and assistant prose is fully readable;
- thinking/reasoning, tool calls, command output, diffs, and token details are compact cards;
- successful cards are collapsed by default;
- questions, permission requests, and errors are expanded;
- streaming prose updates in place without terminal cursor artifacts.

Settings also offers:

- **Minimal**: prose and action-required cards only;
- **Full detail**: all supported semantic events expanded.

This preference is presentation-only and can be browser-local.

## 5. Stable identity and navigation

Browser navigation must never depend on an ephemeral PTY session ID.

Stable session keys are:

- `server:<commandId>` for configured servers;
- `tab:<tabId>` for Claude, Codex, and SSH tabs;

An interactive shell currently belongs to its configured server tab and therefore retains `server:<commandId>` identity while the renderer changes from server controls to command-composer mode. A future standalone-shell model would require its own persisted tab ID, but is not needed to provide the coordinated shell experience in this release.

Canonical routes use the SPA fallback already provided by the embedded web server:

- `/sessions`
- `/projects`
- `/projects/:projectId`
- `/session/:kind/:stableId`
- `/settings`

The installed manifest starts at `/sessions?source=pwa`. Navigation uses the History API and updates the URL, allowing notification deep links and normal back gestures.

The browser stores the last route only for installed-app mode (`display-mode: standalone` or `navigator.standalone`). A normal browser tab always starts at Sessions. On an installed cold launch, the stored session route is restored only after the first host snapshot proves that:

1. the saved runtime instance ID equals the current runtime instance ID; and
2. the stable session key still resolves to a host session.

Otherwise the app replaces the route with `/sessions` without showing an error.

## 6. Runtime and reconnection contract

### 6.1 Host authority

The native host owns:

- process lifecycle;
- PTY input/output and screen state;
- open tabs and configured projects;
- semantic journals;
- active/recent ordering;
- action-required state;
- controller lease;
- notification generation.

The browser owns only ephemeral presentation state such as scroll position, expanded cards, appearance density, and a draft tied to the current runtime instance.

### 6.2 Runtime instance

`RemoteHostInner` creates a new random `runtime_instance_id` once per native process start. It is never persisted. Every web hello, snapshot, and push deep link includes or is scoped to this ID.

`server_id` continues to identify the paired DevManager installation. It is not used to infer that sessions survived a restart.

### 6.3 Warm return

When the PWA is suspended and resumed:

1. `visibilitychange`, `focus`, `pageshow`, and `online` call the WebSocket wake path immediately.
2. A socket that appears open but has not received a frame within the stale threshold is replaced.
3. The host sends a fresh redacted snapshot and journal cursors.
4. The browser reconciles by stable key and event sequence, preserving the mounted route, draft, and scroll anchor.
5. Missed semantic events are replayed from the host's in-memory bounded journal.
6. The composer becomes usable as soon as the socket and automatic writer lease are ready; there is no resume button.

### 6.4 Host restart

If the host runtime ID differs from the browser's remembered ID:

- clear cached journals, unread state, drafts, and last-session restoration data from the prior runtime;
- accept the new snapshot as complete truth;
- show Sessions, which may be blank;
- never render stale transcript content while reconnecting.

### 6.5 Offline presentation

The service worker caches only the application shell and static assets. Authenticated API data, snapshots, journals, and terminal output are never stored in Cache Storage or IndexedDB. LocalStorage intentionally holds only runtime-scoped drafts, the installed-app last route/runtime pair, and presentation preferences; runtime-change and expiry rules clear stale entries before they can render.

While disconnected, an already mounted timeline may remain visually present but is covered by a concise `Reconnecting…` state and its composer is disabled. On a cold offline launch the shell shows that DevManager is unreachable; it does not display an old transcript.

## 7. Automatic writer lease

The current global explicit control model is replaced in the web UX by an automatic foreground writer lease while preserving one-writer safety.

Rules:

1. A visible web client requests the lease on connection, foreground return, composer focus, or an attempted mutating action.
2. The current web lease is renewed by heartbeat and input activity.
3. A hidden or disconnected web client becomes immediately preemptible; expiry remains only a crash/network-loss guarantee.
4. A foreground interaction transfers the lease atomically from a hidden, disconnected, or idle client. A very short active-input guard prevents two simultaneously typing clients from interleaving bytes, but phone-to-desktop and desktop-to-phone handoff does not wait for the normal expiry.
5. A currently active writer is shown passively as `Active on another device`; focusing the composer completes the automatic handoff as soon as the active-input guard is clear.
6. Mutating messages include a lease generation. Stale writers are rejected without replaying input.
7. Read-only subscriptions never require the lease.

No normal user path contains Take Control, Resume, or Reconnect buttons. A diagnostics-only force-release action may live under Settings for exceptional recovery.

## 8. Web-safe protocol boundary

The browser protocol becomes explicitly web-specific instead of serializing `RemoteWorkspaceSnapshot` and `RemoteWorkspaceDelta` directly.

### 8.1 Redacted configuration DTOs

`WebWorkspaceSnapshot` includes only fields required by the mobile operating UI:

- runtime instance and server identity;
- sanitized projects: IDs, names, colors, folder IDs/names, and command IDs/labels/ports;
- sanitized open tabs: stable IDs, kind, project, command/connection references, and label;
- sanitized SSH references: ID, label, host, port, and username;
- sanitized settings required for display;
- session summaries, port status, controller lease state, and journal cursors.

It never includes:

- SSH passwords or private key text;
- GitHub tokens;
- environment maps or env-file content;
- full command lines or startup commands unless explicitly required by a log card and redacted;
- arbitrary desktop window state;
- authentication tokens.

The same projection is used for snapshots and deltas. Serialization tests must prove forbidden sentinel values cannot appear in JSON.

### 8.2 Versioning

The web JSON protocol has its own integer `webProtocolVersion`, independent of the native MessagePack remote protocol. Incompatible browser bundles receive a reload-required message. Additive message fields remain optional in TypeScript.

The inbound side uses a separate allowlisted `WebAction` enum and converts each accepted variant to an internal `RemoteAction` on the host. The browser cannot submit full `saveProject`, `saveSsh`, `saveSettings`, or another arbitrary native action. Mobile configuration changes introduced later use narrow patch actions that preserve host-only secret fields.

### 8.3 Semantic event envelope

The host maintains a bounded in-memory journal per stable session key. Each entry uses:

```text
SemanticEvent {
  runtime_instance_id
  stable_session_key
  sequence
  occurred_at_epoch_ms
  source: claude | codex | shell | server | ssh | system
  kind
  payload
}
```

The common event kinds are:

- `userMessage`
- `assistantMessage` with streaming/replacement identity
- `reasoning`
- `toolCall`
- `toolResult`
- `diff`
- `command`
- `output`
- `question`
- `permission`
- `status`
- `error`
- `sessionEnded`
- `terminalMode`

Provider-specific data may be retained in an optional diagnostics payload, but the React renderer consumes the normalized fields above.

Journal retention is tiered. User/assistant prose, commands, questions, final tool summaries, errors, and state transitions are retained for the full host runtime under a generous per-session safety ceiling (target 50,000 events or 64 MiB). Verbose server logs, streaming deltas, and tool output use separate rolling budgets and collapse into explicit truncation markers before canonical conversation events are considered for eviction. When any safety ceiling rolls over, the host emits the oldest available sequence so the browser can replace, rather than incorrectly append to, an incomplete view. Ordinary all-day phone use must not lose the conversation timeline.

### 8.4 WebSocket additions

The web wire adds:

- `subscribeSemantic { stableSessionKey, afterSequence }`
- `unsubscribeSemantic { stableSessionKey }`
- `semanticBootstrap { stableSessionKey, oldestSequence, latestSequence, events }`
- `semanticEvent { event }`
- `composerSubmit { stableSessionKey, text, attachments, expectedLeaseGeneration }`
- `interruptSession { stableSessionKey, expectedLeaseGeneration }`
- `acquireWriterLease`
- `writerLeaseState`

PTY subscription and input messages remain for raw mode and compatibility.

## 9. Projection pipeline

### 9.1 One execution stream, multiple projections

Every session continues to execute through the existing PTY. The native UI and raw web terminal consume the existing terminal model. A new presentation service observes the same runtime/output stream and emits semantic events.

This avoids splitting process ownership or allowing a provider adapter to become the execution authority.

### 9.2 Claude adapter

For recognized Claude Code launch commands:

1. Generate a session-scoped Claude settings file and a random relay channel tied to the DevManager tab.
2. Add Claude's supported `--settings` argument without changing the user's working directory, approval flags, or PTY behavior.
3. Register official command hooks for SessionStart, UserPromptSubmit, PreToolUse, PermissionRequest, PermissionDenied, PostToolUse, PostToolUseFailure, PostToolBatch, Notification, MessageDisplay, Elicitation/ElicitationResult, subagent/task lifecycle, compact lifecycle, Stop/StopFailure, and SessionEnd.
4. Run the current DevManager executable in an early `claude-hook-relay` mode. The relay reads one hook JSON payload from stdin, forwards it over bounded local IPC, writes nothing to the terminal, always exits successfully, and never returns a permission decision.
5. Normalize user/assistant display content, parallel tool lifecycles keyed by `tool_use_id`, errors, questions, and lifecycle entries into the ephemeral semantic journal.
6. Treat the PTY child-exit path as authoritative if SessionEnd is missing.

If the command is customized such that safe settings injection or relay installation is unavailable, mark adapter health as `degraded` and use the native DOM terminal projector. Do not prevent the Claude session from launching.

Claude's transcript path may be retained only as session-scoped diagnostic metadata. Its undocumented JSONL entry format is not a semantic contract and is not tailed as the primary event source. Hook backpressure drops optional MessageDisplay diagnostics before lifecycle events and must never block Claude.

### 9.3 Codex adapter

For recognized Codex launch commands, prefer Codex's structured app-server protocol when the installed CLI exposes the required compatible methods:

1. Resolve and record one exact Codex executable/version for the tab so the TUI and sidecar cannot straddle an `@latest` upgrade.
2. Start `codex app-server --listen stdio://` as a supervised sidecar and an ephemeral loopback WebSocket listener in DevManager.
3. Launch the normal Codex TUI inside the existing PTY with `--remote ws://127.0.0.1:<port>`.
4. Transparently proxy every TUI WebSocket JSON-RPC frame to/from the app-server JSONL stream. Unknown messages pass through unchanged, and semantic decoding is never on the forwarding critical path.
5. Observe the proxied thread, turn, item, tool, approval, diff, plan, usage, and error notifications. Completed items are authoritative; deltas update transient display state.
6. Normalize observed events into the shared semantic journal without answering approvals on the TUI's behalf.
7. Stop the sidecar and bridge with the tab's process tree.

Compatibility is capability-detected at launch; no assumption is made from version text alone. If app-server startup, bridge binding, or protocol negotiation fails, terminate only the sidecar and launch the user's normal Codex command unchanged. Mark the adapter `degraded` and use the native DOM terminal projector. Rollout JSONL is an optional recovery diagnostic, not a public or primary contract; if exact correlation cannot be proven, DevManager never attaches to it.

### 9.4 Shell and SSH projector

Ghostty OSC 133 prompt marks already parsed by `terminal/session.rs` define command boundaries:

- Prompt/InputReady opens composer state.
- CommandStart turns submitted input into a command card.
- CommandFinished attaches exit status and closes its output block.
- Reported CWD updates the navigation subtitle.

ANSI control sequences are removed from semantic output while preserving line breaks and plain Unicode text. Long output is appended in bounded chunks and rendered with a monospaced native DOM style, line wrapping enabled by default.

When shell integration is unavailable, output is still shown as log blocks and submitted prompts are shown as command cards, but exit grouping is marked best-effort.

SSH uses the same projector, with connection/disconnection events and redacted host identity added as status cards.

### 9.5 Server projector

Server sessions emit:

- starting/running/stopping/crashed/exited status events;
- start, stop, and restart actions;
- port and process resource summary;
- ANSI-cleaned line-oriented logs;
- crash/exit cards expanded by default;
- auto-restart transitions without duplicating the prior journal.

The mobile server view does not show a prompt composer unless the underlying command is explicitly interactive.

### 9.6 Native terminal projection and raw fallback

The guaranteed degradation path is still native DOM. DevManager projects the host terminal's plain-text screen/scrollback into selectable, wrapping rows with a native composer even when Claude/Codex structured integration is unavailable. This path preserves a mobile-readable experience across provider version drift and custom commands, although its cards are less richly classified.

The existing xterm view remains lazy-loaded and uses the existing PTY bootstrap/replay path.

Raw mode activates automatically when:

- a non-AI shell/SSH screen enters alternate-screen mode;
- mouse reporting is enabled;
- the projector reports that the current interaction cannot be operated without cursor-grid or mouse semantics; or
- the user chooses `Open Terminal` from the session overflow menu.

When alternate-screen and mouse-reporting modes end, the UI returns to semantic mode automatically unless the user manually pinned Terminal for that session.

For Claude/Codex, both a healthy structured adapter and the native terminal projector take precedence over the TUI's own alternate-screen mode. Adapter degradation changes card richness, not the overall mobile-native layout. Only an interaction that genuinely needs cursor-grid semantics causes a clear but non-blocking raw Terminal transition.

## 10. PWA and iPhone integration

### 10.1 Installable shell

Use `vite-plugin-pwa` to generate:

- a manifest with `display: standalone`, portrait-friendly behavior, app name/short name, theme/background colors, and maskable icons;
- an app-shell service worker;
- explicit cache exclusion for `/api/**`, WebSockets, and any authenticated response;
- an update flow that activates on a safe navigation/cold start and never reloads in the middle of prompt entry.

The HTML head includes Apple touch icons, status-bar appearance metadata, theme colors for light/dark, and an accessible viewport that does not disable pinch zoom.

### 10.2 Secure context

On iPhone, installability, service workers, camera access, and Web Push require HTTPS. DevManager supports two modes:

- **Secure remote URL**: full PWA, attachment, and notification capability through the user's HTTPS tunnel or trusted reverse proxy.
- **Plain LAN HTTP**: operating UI and WebSocket control only; Settings explains which native capabilities are unavailable and why.

The product must feature-detect each capability and never claim installation or push support on an insecure origin.

### 10.3 Notifications

iOS requires notification permission to be requested from a user gesture, so Settings contains one explicit **Enable notifications** action. This is setup, not a reconnect/resume control.

After permission is granted, the service worker subscribes to Push and sends the subscription to an authenticated host endpoint. The host persists the subscription and VAPID material as remote-web configuration, separate from runtime journals.

Actionable notification classes are:

- Claude/Codex needs input;
- Claude/Codex completed while the app was not visible;
- server crashed or exhausted auto-restart;
- SSH disconnected unexpectedly.

Notifications include the runtime ID and stable route. A click focuses an existing PWA window or opens the route. If the host runtime changed, normal route validation lands on Sessions. The app icon badge equals the current action-required count where the Badging API is available.

Foreground sessions do not generate duplicate system notifications. Normal log output never notifies.

## 11. React structure

The current monolithic terminal-first route is replaced with focused units:

```text
web/src/
  app/
    AppShell.tsx
    router.ts
    restore.ts
  sessions/
    sessionKey.ts
    sessionModel.ts
    SessionsScreen.tsx
    SessionScreen.tsx
    Composer.tsx
    timeline/
      Timeline.tsx
      eventRenderers.tsx
    views/
      AiSessionView.tsx
      ServerSessionView.tsx
      CommandSessionView.tsx
      RawTerminalView.tsx
  projects/
    ProjectsScreen.tsx
    ProjectScreen.tsx
  settings/
    SettingsScreen.tsx
  pwa/
    register.ts
    notifications.ts
  api/
    types.ts
    ws.ts
  store/
    index.ts
```

Existing xterm and image-paste utilities are retained behind the new session views. State selectors derive view models so components do not traverse raw wire DTOs.

## 12. Error and edge behavior

- **Temporary network loss:** retain the current route, disable input, reconnect, reconcile sequences, and restore the scroll anchor.
- **Host unreachable on cold launch:** show a native empty/error state with automatic retry and current host label; no stale session content.
- **Session ended while away:** show its final status and journal if still present in the current runtime, with context-appropriate restart/reopen action.
- **Session removed while away:** return to Sessions with a brief non-blocking notice.
- **Adapter parser error:** record diagnostics, show a one-time compact fallback card, and continue through the native DOM terminal projector; use raw terminal only if the interaction needs cursor-grid semantics.
- **Journal rollover:** replace with the available bounded bootstrap and show `Earlier activity is no longer in host memory` once.
- **Writer conflict:** queue no input; wait for automatic lease resolution and preserve the local draft.
- **PWA update:** do not reload while composer text is non-empty or a mutation is in flight.
- **Push endpoint expires:** remove it after the push service returns a terminal gone/not-found response.

## 13. Security and privacy

- Pairing and authenticated cookies remain required for all snapshots, WebSockets, push registration, and icons/badge state that reveal activity.
- Cookies remain HttpOnly, SameSite, and Secure whenever served over HTTPS.
- WebSocket origin validation is retained and tested for tunnel/reverse-proxy deployments.
- Push payload text is privacy-minimal by default: project/session label and action category, not prompt or source-code content.
- Semantic event HTML is always rendered as text/React elements; ANSI and Markdown content never enters `dangerouslySetInnerHTML`.
- Attachment staging remains scoped to the selected AI session and enforces existing MIME/size limits.
- Provider transcript adapters verify the expected path/session identity before reading and never expose unrelated provider sessions.
- Browser caches contain only versioned static assets.

## 14. Deterministic web embedding

The embedded bundle is part of the Rust binary and must never be silently stale or incomplete.

- The complete production bundle, including hashed assets, manifest, service worker, icons, and a source fingerprint, is tracked so an ordinary clean Rust checkout remains offline-buildable.
- `build.rs` validates that the tracked bundle is internally complete; the presence of `index.html` alone is insufficient. It does not install Node dependencies or access the network.
- CI/release and explicit frontend workflows perform `npm ci` and `npm run build` before Rust packaging, then fail if the generated fingerprint or references are inconsistent.
- A Rust build fails if the tracked index references a missing hashed asset, manifest, service worker, or required icon, and explains the exact frontend build command when regeneration is needed.
- One generated web build ID is embedded in both the bundle and the web welcome frame.
- CI verifies a clean checkout by removing generated bundle files, performing the supported build, and probing the root, every referenced static asset, the manifest, the service worker, and a deep-link SPA fallback.
- Versioned hashed assets use immutable caching; HTML, manifest, and service worker use revalidation/no-cache headers.

## 15. Delivery strategy

The user-visible launch is coordinated, but implementation is divided into independently testable foundations:

1. web-safe DTO and runtime instance boundary;
2. semantic journal and shell/server projection;
3. Claude and Codex structured adapters with degradation fallback;
4. native React shell, routing, session views, composer, and raw fallback;
5. PWA install/restore/update behavior;
6. automatic writer lease;
7. notifications and badges;
8. end-to-end iPhone-sized validation and accessibility polish.

No intermediate foundation is advertised as the completed mobile redesign. The old terminal-first UI remains usable on the implementation branch until the new shell is ready.

## 16. Test and acceptance matrix

### Rust unit/integration tests

- Redacted DTO serialization excludes password, private key, token, environment sentinel, and startup command sentinel.
- Runtime ID is stable during one host instance and changes for a new instance.
- Stable session key resolution covers server, Claude, Codex, and SSH tabs.
- Journals order, deduplicate, replay after a cursor, and report rollover correctly.
- ANSI/log projection handles chunk boundaries, carriage returns, Unicode, OSC 133 boundaries, and bounded retention.
- Claude and Codex fixtures normalize messages, tools, diffs, errors, and streaming replacement identities.
- Provider mismatch cannot attach to another session.
- Writer lease acquisition, renewal, expiry, foreground reclaim, stale-generation rejection, and desktop priority are deterministic under a fake clock.
- Push subscriptions are authenticated, validated, and removed after terminal endpoint errors.

### React/Vitest tests

- Session grouping and project labels remain unambiguous.
- Installed-only last-route restore validates runtime ID and session existence.
- Host restart clears draft and transcript state.
- WebSocket wake replaces stale sockets and resubscribes from the last sequence.
- Composer preserves drafts through remount, submits through the semantic action, and exposes native attachment inputs.
- Calm/minimal/full density renders the expected event classes.
- Raw fallback enters/exits from terminal mode events and respects a manual pin.
- PWA update defers while a draft or mutation is active.
- Notification click routes to the stable session key.

### Browser/mobile acceptance

Validate at minimum at 390 x 844 and 430 x 932 CSS pixels, plus a desktop viewport:

1. Pair and install from a secure URL.
2. Launch from Home Screen with standalone chrome and correct safe areas.
3. Open each session kind and confirm all normal content is native DOM, selectable, wrapping, and readable.
4. Dictate into the composer, attach an image to an AI session, send, interrupt, and continue.
5. Switch apps for 30 seconds and 10 minutes; return directly to the same session with no buttons.
6. Drop connectivity during streaming output, restore it, and confirm no duplicate/missing journal events within retained history.
7. Close the PWA, continue work on desktop, relaunch, and confirm the host's latest session state appears.
8. Restart native DevManager and confirm the PWA returns to a blank/new host runtime without stale content.
9. Enter and leave a full-screen program over shell/SSH and confirm automatic raw/native transitions.
10. Enable notifications from Settings, background the PWA, trigger each actionable class, and validate deep links and badges.
11. Inspect browser network payloads and prove secret fields are absent.
12. Run keyboard-only, VoiceOver-oriented landmark/label, reduced-motion, light/dark, and large-text checks.

## 17. Platform references

- Claude Code Remote Control establishes the expected local-process, remote-client, synchronized, auto-reconnecting interaction model: https://code.claude.com/docs/en/remote-control
- Claude Code hooks provide structured session and lifecycle integration points: https://code.claude.com/docs/en/hooks
- Apple Web Push documentation: https://developer.apple.com/documentation/usernotifications/sending-web-push-notifications-in-web-apps-and-browsers
- WebKit's iOS/iPadOS Home Screen Web Push requirements: https://webkit.org/blog/13878/web-push-for-web-apps-on-ios-and-ipados/
