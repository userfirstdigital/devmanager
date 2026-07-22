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
cargo install cargo-packager --version 0.11.8 --locked
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
$env:DEVMANAGER_UPDATE_ENDPOINTS = "https://github.com/<owner>/<repo>/releases/latest/download/latest.json"
$env:DEVMANAGER_UPDATE_PUBKEY = "<public key>"
$env:CARGO_PACKAGER_SIGN_PRIVATE_KEY = "<private key>"
$env:CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD = "<key password>"
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

`.github/workflows/release.yml` packages the native crate and publishes a public GitHub Release on every non-`[skip ci]` push to `master`.

- A Windows verification job runs the complete locked Rust test suite plus the web tests, typecheck, audit, production build, embedded-bundle check, and Rust formatting check in parallel with release preparation and packaging. Installer artifacts are still built when verification fails, but publication remains blocked until verification and every platform build succeed.
- Release builds use the supported Node `24` LTS line and pin Rust `1.94.0`, `cargo-packager` `0.11.8`, NSIS `3.12.0`, and WiX `3.14.1.20250415`; manual dispatches outside `master` are refused and all release runs share one concurrency lock.
- The workflow uses `Cargo.toml` when it is newer than the latest stable `vX.Y.Z` tag; otherwise it selects the next patch version.
- The prepare job writes the release version into `Cargo.toml` and `Cargo.lock`, then commits that bump back to `master` with `[skip ci]`.
- Every platform checks out that exact prepared commit, and the release tag is explicitly pinned to the same commit rather than the moving branch head.
- Windows builds publish updater-signed `nsis` installers (plus `wix` on x64). macOS builds publish updater-signed `app` bundles plus `dmg` artifacts.
- The workflow creates a new draft without updating an existing release, requires the exact 11-file platform/signature/manifest contract, and verifies every uploaded size and SHA-256 digest before publication.
- A push to `master` can therefore publish immediately when the required secrets and variables are configured. Treat the push as the production approval point.

## Required GitHub Secrets And Variables

Set these before relying on the release workflow or the packaged updater:

- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY`
- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD`
- Variable: `DEVMANAGER_UPDATE_PUBKEY`

Also make sure `master` allows the workflow bot to push the automated version-bump commit back to the branch.

Generate the signing pair with the packager signer:

```powershell
cargo packager signer generate
```

Use the generated private key for release signing and embed the public key into packaged builds via `DEVMANAGER_UPDATE_PUBKEY`.

## Release Smoke Check

After pushing `master`, do not consider the release complete until all of these checks pass:

- the `verify`, `prepare`, all three platform `build` jobs, and `release` job succeed; the release remains a draft until its exact asset set and tag commit pass the final check
- the new tag points to the workflow's reported prepared commit, not merely the latest `master` commit
- the GitHub Release contains Windows x64/ARM64, macOS ARM64, matching updater `.sig` files, and `latest.json`
- every URL and platform key in `latest.json` resolves to the uploaded asset for the same version
- a clean Windows install launches and the existing app detects, verifies, downloads, and offers the update
- the mobile web health endpoint, HTTPS app shell, pairing, WebSocket reconnect, and one real prompt all work through the production proxy
- backgrounding and reopening the installed iPhone app resumes the same host session without a button, while restarting the native host produces a new blank runtime

If packaging fails before draft creation, fix forward and push again; no release exists to roll back. If a late check leaves an unpublished draft and orphan tag, delete both before retrying only after confirming that version was never public. If the public release is bad, keep the native host running and remove the bad GitHub Release so `releases/latest` returns to the prior updater manifest, but retain the bad version's tag so it can never be reused. Fix forward from `master` as the next higher version. A successful authoritative check replaces or discards a downloaded-but-uninstalled recalled update; clients that have not checked again must not click **Restart to update**. Do not delete or replace the user's persisted DevManager profile during rollback.

## Platform Signing Status

The `.sig` files authenticate updates to DevManager itself; they are not operating-system publisher signatures. Current Windows installers are not Authenticode-signed, so SmartScreen can warn. Current macOS artifacts have neither an Apple Developer ID signature nor notarization, so Gatekeeper can block them. Public low-friction distribution requires adding those platform credentials to the packaging workflow; until then, this repository's releases are suitable only for users who explicitly accept the warning/workaround.

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

## Mobile Web App

DevManager includes an iPhone-first web app for working with the same live sessions managed by the native desktop process. Claude, Codex, servers, shell, and SSH are rendered as wrapping, selectable native web views; the terminal grid is loaded only for interactions that genuinely require terminal cursor semantics.

The native DevManager process remains the source of truth. App switching, phone locking, browser suspension, and ordinary network loss reconnect automatically and return to the current host state without a Resume, Reload, or Take Control button. Closing the web app does not close sessions. Restarting the native host intentionally starts a new blank web runtime.

### Connect and install

1. In the desktop app, open **Settings → Remote → Host → Browser Access** and enable it.
2. For the installed experience, copy the displayed one-time invite but open its `/pair?...` path through the final trusted HTTPS hostname. Pairing rotates the invitation and sets a long-lived, host-specific `Secure`, `HttpOnly` browser cookie for that public origin.
3. For LAN diagnostics, the displayed `http://<lan-ip>:43872` URL can run the control UI directly.
4. For Home Screen installation, attachments that need secure browser APIs, and notifications, expose DevManager through a trusted HTTPS tunnel or reverse proxy. Never publish the raw listener directly to the internet.
5. In iPhone Safari, open the HTTPS address, choose **Share → Add to Home Screen**, then launch DevManager from its Home Screen icon.

The installed app opens on Sessions, highlights work needing attention, and labels every item with its project. It restores the last valid session only after the host confirms that the same runtime and session still exist. Normal prompts use a real multiline text area, so iOS dictation, paste, selection, autocorrection, and the software keyboard work normally.

See [Mobile Web App operations](docs/REMOTE_MOBILE_WEB.md) for lifecycle guarantees, HTTPS setup, notification behavior, adapter fallback, security boundaries, and development commands.

## Notes

- The archived Tauri release path is intentionally not used anymore.
