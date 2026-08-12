# Release checklist

Use this checklist for packaged DevManager candidates. Compile/tests alone do not complete a release.

## Package identity

- [ ] `packaging/package-contract.json` matches `Cargo.toml` packager metadata
- [ ] Release build produces sibling `devmanager` and `devmanager-host` binaries once via `before-packaging-command`
- [ ] Package contains no `devmanager-next`, process-test helper, or other legacy/development binary
- [ ] Windows metadata: product `DevManager` for both; file descriptions `DevManager` / `DevManager Host`; distinct original filenames
- [ ] Resources are limited to declared roots (`assets`, `third_party/ghostty`); exclusions cover worktrees, target/evidence, fixtures, `session.json`, dev profiles, Portal trees, and secrets
- [ ] `packaging/Assert-PackageContract.ps1` passes against `target/release` and `dist/packager`
- [ ] `THIRD_PARTY_NOTICES.md` covers shipped GPUI/`gpui-component` and SQLite/`rusqlite` usage

## Signed updater metadata

- [ ] Installers and `.sig` files exist before `latest.json` is generated
- [ ] `latest.json` version matches the prepared release version and points at the uploaded artifacts
- [ ] Public key / signing secrets are present without printing private key material
- [ ] Existing updater shape (`version`, `notes`, `pub_date`, `platforms.*.format|signature|url`) remains intact

## Isolated validation

- [ ] Clean install in a disposable profile/VM launches client + host with matching version/protocol identity
- [ ] Update detection uses signed release metadata against the installed build identity, not checkout files or stale PWA assets
- [ ] `config.json` / `remote.json` hashes and pairing/device identity remain stable across update; `session.json` is ignored
- [ ] Production installed DevManager PID/start time and production config/remote hashes stay unchanged until an explicitly approved install

## Publication gate

- [ ] `.github/workflows/release.yml` verify + every platform build + release job are green
- [ ] Draft release asset set and digests match the staged contract before publish
- [ ] Explicit human approval precedes merge/tag/publish/install to the daily machine

Historical phase plans under `docs/superpowers/plans/` remain design history; this checklist is the operator-facing gate summary.
