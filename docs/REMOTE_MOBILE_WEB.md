# DevManager Mobile Web App

DevManager's mobile surface is an installable, iPhone-first projection of the native host. It is designed for long sessions in which the phone is frequently locked, backgrounded, disconnected, or moved between networks while the native DevManager process continues running.

## Runtime model

The native DevManager process owns processes, PTYs, session state, semantic journals, control authority, and notification generation. The browser owns only presentation preferences, same-runtime drafts, its last installed-app route, and pending input awaiting host acknowledgement.

- Backgrounding or closing the PWA never stops a server, shell, SSH, Claude, or Codex process.
- Foregrounding reconnects and reconciles automatically. There is no Resume, Reload, or Take Control action.
- A warm return keeps the current route, scroll position where possible, draft, and host-authoritative session timeline.
- An installed cold launch restores the last route only after the first host snapshot proves that the runtime and stable session still exist.
- A native host restart creates a new runtime identity. The browser drops the old projection and same-runtime drafts instead of resurrecting stale sessions.
- Control follows foreground interaction. A hidden, disconnected, or idle web controller becomes preemptible, and stale input generations are rejected at the host mutation boundary.

The browser stores no durable transcript or terminal journal. History exists in bounded host memory and therefore ends with the native process.

## Native session views

Sessions is the default home and groups items that need attention, are active, or were recently active. Every row includes its project so similarly named sessions remain distinguishable.

- Claude and Codex use conversation text plus compact tool, diff, plan, question, status, and error cards.
- Servers use status and resource summaries, native start/stop/restart controls, and wrapping selectable logs.
- Shell and SSH use command/output groups with wrapping native text.
- Every prompt is entered in a native HTML text area. iOS dictation, selection, autocorrection, paste, undo, and the software keyboard remain available.
- PNG and JPEG attachment controls appear where the session supports images.
- Raw Terminal is a lazy fallback for full-screen programs, mouse reporting, alternate-screen interfaces, or explicit advanced use. Leaving it returns to the native view.

AI presentation density defaults to **Calm**. Settings also offers **Minimal** and **Full** without changing the underlying session state.

## Pairing and addresses

Enable **Settings → Browser Web UI** in the native app. The default listener binds to port `43872` and displays a one-time pairing URL. Opening that URL stores a signed, host-specific, `HttpOnly` cookie and redirects into the app. Revoking a paired browser in the native settings invalidates its access.

Plain LAN HTTP remains useful for diagnostics and control, but modern iPhone platform features require a secure context. Use a trusted HTTPS tunnel or reverse proxy for the installed experience. The proxy must preserve WebSocket upgrades and forward `/api/**`, `/pair`, the app shell, the manifest, and the service worker to the same DevManager listener.

Do not expose port `43872` directly to the public internet. Keep DevManager's cookie and pairing boundaries intact; do not add a proxy cache in front of authenticated API routes.

## Install on iPhone

1. Pair through the trusted HTTPS origin in Safari.
2. Confirm the app loads and Settings reports a secure context.
3. Choose **Share → Add to Home Screen**.
4. Launch the new DevManager icon rather than the original Safari tab.

The manifest starts at `/sessions?source=pwa`. Safe-area insets, standalone display, accessible zoom, light/dark appearance, and installed-only route restoration are handled by the web app.

## Notifications

Notifications are optional and require both HTTPS and an installed Home Screen web app on supported iOS versions. DevManager requests permission only after the user taps **Enable notifications** in Settings.

Notifications are limited to actionable transitions:

- Claude or Codex needs input;
- Claude or Codex completes while not being viewed;
- a server crashes;
- an SSH connection disconnects unexpectedly.

Payloads contain only a generic action, project/session label, runtime identity, badge count, event identity, and stable deep link. Prompt text, terminal output, code, diffs, credentials, tokens, commands, and environment values are never included. A notification focuses an existing app window when possible and otherwise opens the stable session route. Viewing or acknowledging attention updates the aggregate badge. The visibly focused session does not also produce a system notification.

If Settings reports that notifications are unavailable, check that the app is running from its Home Screen icon over HTTPS. Plain HTTP deliberately continues to offer the core UI without claiming installation or push support.

## Provider adapters and fallback

Recognized Claude Code sessions use supported command hooks to enrich the native timeline. Recognized Codex sessions use the documented app-server protocol through a loopback bridge while the normal TUI continues to run in the PTY. Both integrations are capability-detected and fail open.

Custom commands, wrappers, unsupported provider versions, parser errors, or a sidecar failure never prevent the session from launching. DevManager marks the adapter as degraded and continues with its native DOM terminal projector. This fallback still wraps, selects, copies, and composes like a mobile app; only the richness of card classification changes.

## Updates and offline behavior

The service worker precaches only the versioned application shell, icons, and static assets. Pairing, authenticated API data, snapshots, semantic journals, terminal output, and push subscriptions are network-only and are never written to Cache Storage.

A newly installed worker waits while a draft or host mutation is pending. DevManager activates it automatically at a safe visible navigation/foreground point after drafts are empty and mutations are acknowledged. If the embedded browser bundle and native host are incompatible, the app performs the same safe reconciliation and guards against reload loops.

The cached shell can open while the host is unreachable, but it intentionally shows an automatic reconnect state rather than stale session content. Working offline is not supported because the host is the only runtime truth.

## Security boundary

The browser protocol is separate from the native remote protocol and exposes allowlisted, redacted DTOs and actions. It does not serialize SSH passwords or private keys, provider tokens, environment values, startup commands, arbitrary native settings, or unrelated sessions. Mutation payloads, image attachments, replay journals, queues, and HTTP bodies have explicit limits. Push endpoints require the paired browser cookie and subscriptions are associated with that browser installation.

## Develop and verify the web app

Install exact dependencies and run the local Vite surface:

```powershell
npm --prefix web ci
npm --prefix web run dev
```

Run the web gates and rebuild the tracked embedded bundle:

```powershell
npm --prefix web test
npm --prefix web run typecheck
npm --prefix web run build
```

`build.rs` never installs packages or reaches the network. It validates the tracked bundle, source fingerprint, manifest, service worker, icons, and referenced hashed assets. If source and bundle differ, run the exact `npm ci` and build commands above before compiling or packaging the Rust application.

The embedded web server uses no-cache responses for the app entry point, manifest, and service worker; immutable caching for content-hashed assets; SPA fallback for native deep links; NetworkOnly handling for authenticated endpoints; and restrictive content, framing, and MIME-sniffing headers.
