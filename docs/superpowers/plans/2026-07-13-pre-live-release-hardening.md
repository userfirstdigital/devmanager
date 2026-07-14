# Pre-Live Release Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the release, security, lifecycle, and operations gaps found in the final pre-live audit, then produce a reproducible release artifact and an evidence-backed go-live handoff without pushing or interrupting the currently running DevManager host.

**Status:** Completed locally on 2026-07-13. The source tree and unsigned Windows package are verified; pushing, publishing, firewall changes, and restarting the active host remain explicit live operations.

**Architecture:** Keep the native DevManager process as the single runtime owner. Make cloned remote-service handles non-owning, make browser-pairing invitations atomically single-use, gate packaging on the complete Rust/web test suite, pin the GitHub Release tag to the exact verified source commit, and document a loopback-first HTTPS reverse-proxy deployment with an explicit smoke test and rollback path.

**Tech Stack:** Rust, Axum, GitHub Actions, PowerShell, cargo-packager, React, Vite/PWA

## Global Constraints

- Do not push, publish a GitHub Release, stop the running DevManager process, or change host firewall rules without explicit user authorization.
- Preserve the running host and its active sessions while preparing the source tree and release artifacts.
- Use strict RED/GREEN tests for behavior changes and fresh verification evidence before claiming readiness.
- Keep browser state a projection of the native host; do not introduce a second durable runtime truth.
- Treat OS installer signing/notarization as a documented distribution limitation until the required Microsoft/Apple identities are available.

---

### Task 1: Correct remote-host service lifetime ownership

**Files:**
- Modify: `src/remote/mod.rs`
- Modify only if required by the owner-token field: `src/remote/web/bridge.rs`

- [x] Add a regression test proving that a temporary/cloned service facade does not set the shared stop flag or shut down browser/native listeners.
- [x] Run the focused test and capture the expected failure.
- [x] Give only the root owner a shutdown token; cloned and temporary service handles must be non-owning.
- [x] Preserve shutdown when the actual root owner is dropped.
- [x] Prove root drop closes the Axum browser listener and releases its bound port even while callback clones/internal references remain.
- [x] Run the focused lifecycle tests and related remote-host tests to green.

### Task 2: Make browser invitations atomically single-use

**Files:**
- Modify: `src/remote/web/mod.rs`
- Modify if shared config mutation is needed: `src/remote/mod.rs`

- [x] Add tests proving a successful pairing consumes the invitation and that concurrent reuse yields exactly one success.
- [x] Add a test proving Reset Access rotates both the cookie secret and the future-pairing invitation.
- [x] Run the focused tests and capture the expected failures.
- [x] Validate and rotate the invitation inside the existing serialized configuration mutation boundary.
- [x] Do not invalidate already-paired browsers when only a successful invitation is consumed.
- [x] Run pairing, authentication, reset-access, and configuration rollback tests to green.
- [x] Replace full stale app-side host snapshots with serialized field-level service mutations so a native toggle/token action cannot resurrect a consumed invitation.
- [x] Serialize host and known-host persistence as locked read/modify/write transactions; test pair-vs-known-host races and crash/reload truth.

### Task 3: Protect persisted remote access secrets

**Files:**
- Modify: `src/remote/mod.rs`

- [x] Add a temp-profile regression test proving `remote.json` is private to the current user (Unix mode `0600`; restrictive owner ACL on Windows).
- [x] Run the focused test and capture the expected failure without reading or modifying the live user profile.
- [x] Harden the temporary file before the atomic rename and verify permissions again on the final file.
- [x] Harden legacy `remote.json` files before reading them on upgrade.
- [x] On Windows, resolve the current process-token SID and system `icacls.exe` robustly instead of trusting mutable username/domain text or `PATH`.
- [x] Treat permission-hardening failure as a persistence failure instead of silently writing broadly readable credentials.
- [x] Run persistence, rollback, and configuration-concurrency tests to green on Windows.

### Task 4: Gate releases and pin tags to verified source

**Files:**
- Modify: `.github/workflows/release.yml`
- Test: `.github/workflows/release.yml`

- [x] Add a Windows `verify` job that runs before `prepare` and is skipped for the workflow's own `[skip ci]` version-bump commit.
- [x] In `verify`, install locked web dependencies, run web tests/typecheck/build, verify the embedded bundle is clean, run `cargo fmt --all -- --check`, and run `cargo test --locked --all-targets -- --test-threads=1`.
- [x] Make `prepare` depend on `verify`, so no version bump or packaging occurs after a failed gate.
- [x] Create and verify the release tag at `${{ needs.prepare.outputs.commit_sha }}`, then use create-only draft publication so a concurrent `master` push cannot move or replace the release.
- [x] Reject manual dispatch outside `master`, serialize all release refs through one concurrency group, and atomically create/verify the release tag at the prepared commit before uploading assets.
- [x] Pin the Rust and cargo-packager versions used for release builds.
- [x] Parse and inspect the workflow locally, and verify every build/release dependency still receives the exact prepared commit SHA.

### Task 5: Align the coordinated release version and operator documentation

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`
- Modify: `docs/REMOTE_MOBILE_WEB.md`
- Modify: `src/workspace/mod.rs`
- Modify: `src/app/mod.rs`

- [x] Set the coordinated native-mobile release line to `0.3.0` so it is published as a feature release rather than the next `0.2.x` patch.
- [x] Expose the existing browser listener bind address in native Settings, retaining `0.0.0.0` for direct LAN access and supporting `127.0.0.1` for a same-host proxy.
- [x] Replace stale release-branch references with `master` and describe the full pre-package verification gate.
- [x] Document public-origin pairing: open the one-time invitation through the final trusted HTTPS authority so the secure cookie is issued for that origin.
- [x] Document loopback binding for a same-host reverse proxy, strict replacement of forwarding headers, WebSocket upgrades, no authenticated caching, query-string redaction, idle-timeout requirements, and minimal TCP-only firewall exposure.
- [x] Add exact post-push smoke checks and rollback steps, including host/session preservation and updater verification.
- [x] State the current Windows Authenticode and macOS Developer ID/notarization limitations plainly.
- [x] On an authoritative updater recall, discard or replace downloaded-but-uninstalled bytes; retain them only across transient check errors or an equal offered version.

### Task 6: Produce a reproducible release-profile build

**Files:**
- Verify: generated web bundle and Rust release output
- Verify: platform package metadata/artifacts

- [x] Point `GPUI_FXC_PATH` at the installed Windows SDK shader compiler (`10.0.22621.0\\x64\\fxc.exe`) for local release commands.
- [x] Run `npm ci`, the full web tests, typecheck, and two clean production builds; compare generated bundle manifests and content fingerprints.
- [x] Run the full locked Rust suite serially, formatting check, all-target check, and `cargo build --release` with the SDK override.
- [x] Run an unsigned Windows NSIS package dry run without publishing; leave updater signing to CI because its private signing key is not present locally.
- [x] Inspect artifact version, embedded web assets, updater metadata, and signature state.

### Task 7: Final review and go-live handoff

**Files:**
- Review: all files changed since `origin/master`

- [x] Request an independent code/security review of the lifecycle, pairing, workflow, and operations changes.
- [x] Resolve every blocking finding and rerun affected focused tests.
- [x] Run one fresh end-to-end verification pass after all edits are complete.
- [x] Confirm the worktree contains only intended changes, create local commits, and report the exact branch/commit/artifact state.
- [x] Present the remaining authorized live actions separately: preserving/stopping the existing host, correcting firewall/runtime configuration, pushing `master`, observing the release workflow, and executing smoke/rollback checks.
