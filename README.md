# DevManager Native

DevManager is the native GPUI rewrite of the archived Tauri + React app. The active code lives at the repo root and the archived reference app remains in `zz-archive/tauri-react-v0.1.11`.

The native stack currently centers on:

- `gpui`
- `alacritty_terminal`
- `portable-pty`
- `cargo-packager`
- `cargo-packager-updater`

## Run

```powershell
cargo run
```

The updater is disabled by default for ad-hoc local runs unless you provide updater env vars at runtime or compile them into the build.

## Hot Watch

For local UI iteration on Windows, use the included watcher instead of running the executable directly from Cargo's output directory:

```powershell
powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1
```

Or use the wrapper:

```bat
watch.bat
```

The watcher listens to `src/`, `assets/`, `Cargo.toml`, and `Cargo.lock`.
Each successful rebuild goes to `target-watch/`, then the script copies the fresh binary to `target-live/` and relaunches from there.
That split avoids Windows locking the compiler output while the app is running, and a failed build leaves the last good app window untouched.

If you only want a single rebuild-and-launch cycle, run:

```powershell
powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1 -Once
```

## Local Packaging

Install the packager CLI once:

```powershell
cargo install cargo-packager --locked
```

Package a signed Windows build:

```powershell
$env:CARGO_PACKAGER_SIGN_PRIVATE_KEY = "<private key>"
$env:CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD = "<key password>"
$env:DEVMANAGER_UPDATE_ENDPOINTS = "https://github.com/<owner>/<repo>/releases/latest/download/latest.json"
$env:DEVMANAGER_UPDATE_PUBKEY = "<public key>"
cargo packager --release --formats nsis
```

Package a signed macOS build with both updater and end-user artifacts:

```powershell
DEVMANAGER_UPDATE_ENDPOINTS="https://github.com/<owner>/<repo>/releases/latest/download/latest.json" \
DEVMANAGER_UPDATE_PUBKEY="<public key>" \
CARGO_PACKAGER_SIGN_PRIVATE_KEY="<private key>" \
CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD="<key password>" \
cargo packager --release --formats app,dmg
```

Generated artifacts are written to `dist/packager`. Temporary replaceable installer icons live in `packaging/icons`.

## Native Updater

The app now contains a native updater module in `src/updater/mod.rs`.

- It reads updater endpoints and the public verification key from runtime env vars first, then from build-time embedded env vars.
- It checks for updates in the background on startup when updater configuration is available.
- It supports `check`, `download`, and `restart to update` through the native settings surface.
- It surfaces updater state in the shell header so availability and download progress are visible outside settings.

The updater expects a GitHub-hosted manifest at:

```text
https://github.com/<owner>/<repo>/releases/latest/download/latest.json
```

The manifest shape matches the `cargo-packager-updater` multi-platform format and includes:

- `version`
- `notes`
- `pub_date`
- `platforms.<target>.format`
- `platforms.<target>.signature`
- `platforms.<target>.url`

The GitHub-hosted updater flow assumes public release assets. If releases are private, the native updater will need an authenticated distribution endpoint instead of raw GitHub asset URLs.

## GitHub Release Workflow

`.github/workflows/release.yml` now packages the native crate directly and publishes GitHub Releases on every push to `native`.

- The workflow looks at the latest stable `vX.Y.Z` tag (or falls back to `Cargo.toml` when there is no tag yet) and releases the next patch version.
- The prepare job writes that release version back into `Cargo.toml` and `Cargo.lock`, then commits the bump back to `native` with `[skip ci]` before the platform builds run.
- Windows builds publish signed `nsis` installers.
- macOS builds publish signed updater bundles (`app`) plus `dmg` artifacts.
- The workflow uploads packaged artifacts and a generated `latest.json` manifest to the GitHub Release for the current push.
- The first push to `native` can create a real public release immediately if the required secrets and variables are already configured.

## Required GitHub Secrets And Variables

Set these before relying on the release workflow or the packaged updater:

- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY`
- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD`
- Variable: `DEVMANAGER_UPDATE_PUBKEY`

Also make sure the `native` branch allows the workflow bot to push the automated version-bump commit back to the branch.

Generate the signing pair with the packager signer:

```powershell
cargo packager signer generate
```

Use the generated private key for release signing and embed the public key into packaged builds via `DEVMANAGER_UPDATE_PUBKEY`.

## First Release Smoke Check

After the first push to `native`, verify:

- a new `vX.Y.Z` tag was created at the workflow-produced version
- the GitHub Release contains Windows installer assets, macOS updater assets, and `latest.json`
- the updater-ready assets each have matching `.sig` files
- `latest.json` points at the uploaded asset names and includes the expected platform entries
- the app detects the release, downloads it, and shows the restart/install prompt

## macOS Installation

The macOS build is not yet signed with an Apple Developer ID certificate or notarized, so macOS Gatekeeper may show **"DevManager is damaged and can't be opened"** when you try to open the app from the DMG.

To work around this, open Terminal and remove the quarantine attribute:

```bash
xattr -cr /Applications/DevManager.app
```

If the DMG itself won't open, run this first:

```bash
xattr -cr ~/Downloads/DevManager*.dmg
```

Alternatively, you can right-click the app in Finder, choose **Open**, and click **Open** in the confirmation dialog. This bypasses Gatekeeper for that specific app.

This workaround is needed until proper Apple code signing and notarization are added to the release workflow.

## Browser Web UI

DevManager can host a second client over plain HTTP so any browser on the same LAN — phones, tablets, another laptop — can see terminals and drive servers without installing the native app.

### Enable

1. In the desktop app, open **Settings → Browser Web UI**.
2. Flip **Enable web UI**. The panel shows the listener URL (defaults to `http://<your-lan-ip>:43872`) and the web pair token.
3. On another device, visit `http://<your-lan-ip>:43872/pair?t=<web-pair-token>`. The host sets a long-lived `HttpOnly` cookie for that DevManager instance and redirects to `/`.
4. Subsequent visits to that same DevManager from the paired browser load directly — no token required. If you run multiple DevManager web listeners on the same host, each instance now keeps its own remembered browser auth.

### Usage

- The sidebar mirrors the desktop app's project tree. Tap a command row to open its terminal. Tap **Start** (hover to reveal) to launch a server.
- The top-right **View / Control** toggle claims keyboard control. While the browser has control, the desktop app runs in viewer mode; toggle back to hand control over.
- Below the terminal on mobile, a helper row surfaces Esc, Tab, Ctrl (sticky), arrow keys, and common shell chars that a phone keyboard lacks.
- The **Running ports** section on the empty screen lists every dev server that has `inUse = true` and links to each with the browser's current hostname substituted — so `http://<lan-ip>:5173` just works when the dev server is bound to `0.0.0.0`.

### Remote access beyond the LAN

The web listener ships as plain HTTP only. For access from outside the LAN, front it with a tunnel that terminates TLS upstream — Tailscale Funnel and Cloudflare Tunnel are both known good. Do **not** port-forward the raw `:43872` listener to the public internet.

### Architecture notes

- The listener is an Axum + Tokio server embedded inside `RemoteHostService` (`src/remote/web/`). It runs alongside — or instead of — the TCP remote-host listener on `:43871`.
- WebSocket frames at `/api/ws` carry a simplified JSON protocol that reuses `RemoteAction` / `RemoteWorkspaceSnapshot` / `RemoteWorkspaceDelta` verbatim. Session output rides binary frames for zero-overhead streaming.
- The React + Vite + Tailwind SPA lives under `web/`. It is embedded into the packaged binary via `rust-embed`, so a single `devmanager.exe` ships the entire web client.
- `build.rs` auto-runs `npm install && npm run build` in `web/` on a fresh clone when the committed bundle stub is detected. CI explicitly runs `npm ci && npm run build` as a workflow step before `cargo packager`.

### Developing the web UI

Iterating on the SPA outside the packaged flow:

```powershell
cd web
npm install
npm run dev
```

Vite's dev server (port 5199) renders the SPA in isolation. Point it at a running devmanager instance by manually opening `http://localhost:5199/?` and the embedded WS client connects back to the devmanager on port 43872. Rebuild the embedded bundle with `npm run build` when you want the packaged binary to pick up your changes — or let `build.rs` rebuild on the next clean `cargo build` if you've only got the stub.

## Notes

- The archived Tauri release path is intentionally not used anymore.
