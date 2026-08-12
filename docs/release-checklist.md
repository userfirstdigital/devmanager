# Release checklist

Use this checklist for packaged DevManager candidates. Compile/tests alone do not complete a release. Packaging/staging is independent from public publication.

## Package identity

- [ ] `packaging/package-contract.json` matches `Cargo.toml` packager metadata (`binaries-dir = "target/release"`, explicit `devmanager` + `devmanager-host`)
- [ ] Shipping identity is final `devmanager/<version>`, matching host (`devmanager-host/<version>`), ctl (`devmanager-host-ctl/<version>`), build.rs env stamps, and protocol major/minor
- [ ] Release build produces sibling `devmanager` and `devmanager-host` binaries once via `before-packaging-command`
- [ ] Package/extracted payload contains no `devmanager-next`, process-test helper, or other legacy/development binary
- [ ] Windows metadata: product `DevManager` for both; file descriptions `DevManager` / `DevManager Host`; distinct original filenames
- [ ] Resources are limited to declared roots (`assets`, `third_party/ghostty`); exclusions cover `.worktrees`, target/evidence, fixtures, `session.json`, dev profiles, Portal trees, secrets, and `zz-archive`
- [ ] WebView2 evergreen runtime expectation is declared for Windows browser surfaces
- [ ] `packaging/Assert-PackageContract.ps1` passes against `target/release`, `dist/packager`, and extracted installer payload; host `ctl actions --json` succeeds under disposable profile state
- [ ] `THIRD_PARTY_NOTICES.md` covers GPUI/`gpui-component`, SQLite/`rusqlite`, `similar` 3.1.2, and selected crypto

## Signed updater metadata

- [ ] Installers and `.sig` files exist before `latest.json` is generated
- [ ] Signatures cryptographically verify with configured `DEVMANAGER_UPDATE_PUBKEY` (minisign) before staging and again before publish
- [ ] `latest.json` includes `version`, `identity` (`devmanager/<version>`), `protocol`, and per-platform `format`/`signature`/`url`/`sha256`
- [ ] Public key / signing secrets are present without printing private key material
- [ ] Existing updater shape remains compatible; immutable hashes/identity are additive metadata for release contract enforcement

## Isolated validation

- [ ] Clean install in a disposable profile/VM launches client + host with matching version/protocol identity
- [ ] Update detection uses signed release metadata against the installed build identity, not checkout files or stale PWA assets
- [ ] `config.json` / `remote.json` hashes and pairing/device identity remain stable across update; `session.json` is ignored
- [ ] Production installed DevManager PID/start time and production config/remote hashes stay unchanged until an explicitly approved install

## Publication gate

- [ ] `.github/workflows/release.yml` verify + every platform build + stage job are green
- [ ] Draft release asset set and digests match the staged contract; release remains draft
- [ ] `cargo metadata --format-version 1 --locked` passed in verify
- [ ] Phase 11 stale-reference scan and Cargo.lock provenance machine-check passed
- [ ] Public publish uses `workflow_dispatch` (`publish=true`) with an explicit existing draft `tag_name` through protected Environment `release-publish` — never auto-publish from push and never compute a new patch during publish
- [ ] Publish recomputes every downloaded artifact hash and compares `latest.json` before leaving draft
- [ ] Explicit human approval precedes merge/tag/publish/install to the daily machine

Historical phase plans under `docs/superpowers/plans/` remain design history; this checklist is the operator-facing gate summary.
