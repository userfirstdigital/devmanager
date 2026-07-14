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

Enable **Settings → Remote → Host → Browser Access** in the native app. The listener defaults to `0.0.0.0:43872` for direct LAN use. Set **Browser bind address** to `127.0.0.1` when the trusted HTTPS proxy runs on the same computer. A successful invite is atomically single-use: it pairs one browser, rotates the future-pairing token, stores a signed host-specific cookie, and redirects into the app. Existing paired browsers remain valid until **Reset access** or an individual revoke invalidates them.

Plain LAN HTTP remains useful for diagnostics and control, but modern iPhone platform features require a secure context. Use a trusted HTTPS tunnel or reverse proxy for the installed experience. The proxy must preserve WebSocket upgrades and forward `/api/**`, `/pair`, the app shell, the manifest, and the service worker to the same DevManager listener.

Pair through the public origin itself. Copy the invite, retain its `/pair?t=...` path and query, replace the displayed local scheme/authority with the final `https://<public-host>`, and open that URL once in Safari. Pairing over the local HTTP URL and then navigating to the public hostname does not work: browser cookies are scoped to the authority that issued them.

The trusted proxy must remove any client-supplied forwarding headers and set exactly one value for each of these headers on every forwarded HTTP and WebSocket request:

```text
X-Forwarded-Proto: https
X-Forwarded-Host: <the public host, including a non-default port>
```

`X-Forwarded-Host` may be omitted only when the proxy preserves that same public authority in `Host`. Do not append forwarding values or send comma-separated lists. DevManager uses this exact public scheme and authority to issue `Secure` cookies and reject cross-origin WebSocket and push mutations.

Do not expose port `43872` directly to the public internet. Keep DevManager's cookie and pairing boundaries intact; do not add a proxy cache in front of authenticated API routes.

## Production proxy and network checklist

Use one of these topologies:

- **Same-host proxy (recommended):** set **Browser bind address** to `127.0.0.1`; expose only the proxy's TCP `443` listener. No firewall rule is needed for `43872`.
- **Proxy on another trusted machine:** bind to the specific private interface where possible, allow TCP `43872` only from the proxy's private IP, and deny every other source. Do not use a broad public-network or UDP rule.

Before pairing a phone, verify all of the following:

- the public hostname has a trusted, current TLS certificate and redirects HTTP to HTTPS without putting the invite token into an intermediate host
- the proxy removes client-supplied `Forwarded`, `X-Forwarded-Proto`, `X-Forwarded-Host`, and related forwarding headers, then supplies the single canonical values documented above
- WebSocket upgrades and long-lived connections are enabled; idle timeouts are long enough that ordinary phone backgrounding is handled by DevManager's reconnect path rather than a rapid proxy reconnect loop
- `/api/**`, `/pair`, and authenticated responses are never cached; the proxy does not rewrite the service worker, manifest, hashed assets, cookies, or CSP headers
- access logs omit or redact the `/pair` query string because it contains the one-time invitation; application/error logs must not record cookies, authorization values, request bodies, prompt text, or attachment content
- the origin is dedicated to this DevManager host; do not multiplex another app under the same authority and path space

The native app's `remote.json` contains cookie, pairing, TLS, and push credentials. DevManager writes it with current-user-only file permissions. Backups and diagnostic bundles must preserve that confidentiality and must not publish the file.

## Install on iPhone

1. Pair through the trusted HTTPS origin in Safari.
2. Confirm the app loads and Settings does not show **Requires a secure HTTPS address** in the notification row.
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

## Go-live smoke test

A push to `master` starts release packaging. The release stays private in draft state until its complete asset-name set and exact tag commit pass the final check. Confirm the GitHub Actions `verify` job, version preparation, all platform builds, and release job are green. Confirm the published release tag points to the exact prepared commit, `latest.json` has the same version and expected platform URLs, and each updater asset has a non-empty `.sig` file.

Do not restart the workstation's active DevManager host merely to test the artifact. Install on a clean/secondary Windows profile first and check:

1. `https://<public-host>/api/health` returns `{"ok":true}` without a cache hit.
2. The HTTPS app shell and manifest load, while an unpaired `/api/me` returns `401`.
3. One public-origin invitation succeeds, immediate reuse of that same invitation returns `401`, and the new invite shown by the host is different.
4. The installed Home Screen app connects its WebSocket, lists the expected projects/sessions, and sends one real prompt.
5. Lock/background the phone, change state from the desktop, and return. The phone reconnects to the same runtime and current state without a Resume or Reconnect action.
6. Drop and restore the phone network. Pending acknowledged input is not duplicated and current output catches up automatically.
7. Reset browser access. Existing browser connections close and the old invite/cookie no longer authorize access.
8. On the secondary host only, restart DevManager. The phone accepts the new blank runtime without resurrecting the old transcript or draft.
9. Confirm the native updater verifies and offers the release before scheduling the real host restart at a point where losing the current in-memory web runtime is acceptable.

## Release rollback

- **Failure before GitHub Release publication:** leave the running host untouched and fix forward. If the workflow created an unpublished draft and orphan tag, delete both before retrying only after confirming that version was never public.
- **Bad public artifact before installation:** immediately remove the bad GitHub Release so `releases/latest` falls back to the last good manifest, but retain its tag so that version can never be reused. Fix forward and publish the next higher version. A subsequent successful authoritative check replaces or discards a downloaded-but-uninstalled recalled update; until that check completes, tell affected clients not to choose **Restart to update**. Keep a copy of the failed workflow/artifact evidence for diagnosis.
- **Bad artifact already installed:** stop further distribution, preserve the user's profile, and fix forward. Do not install an older binary over a profile written by a newer version unless backward compatibility has been explicitly verified.
- **Proxy-only failure:** remove public routing or close TCP `443`; do not stop DevManager or delete `remote.json`. Existing native sessions continue under the desktop host while the web surface is unavailable.

Installing an update restarts the native host and therefore intentionally creates a new web runtime. Schedule that restart; pushing and publishing alone do not disturb the currently running process.

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
