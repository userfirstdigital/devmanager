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

- The root crate version in `Cargo.toml` is the source of truth.
- Each push derives a CI release version like `0.2.0-dev.<run_number>` from that base version without committing a version bump back to the repo.
- Windows builds publish signed `nsis` installers.
- macOS builds publish signed updater bundles (`app`) plus `dmg` artifacts.
- The workflow uploads packaged artifacts and a generated `latest.json` manifest to the GitHub Release for the current push.

## Required GitHub Secrets And Variables

Set these before relying on the release workflow or the packaged updater:

- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY`
- Secret: `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD`
- Variable: `DEVMANAGER_UPDATE_PUBKEY`

Generate the signing pair with the packager signer:

```powershell
cargo packager signer generate
```

Use the generated private key for release signing and embed the public key into packaged builds via `DEVMANAGER_UPDATE_PUBKEY`.

## Notes

- The archived Tauri release path is intentionally not used anymore.
- Full Apple notarization and broader release hardening are still out of scope for this slice.
