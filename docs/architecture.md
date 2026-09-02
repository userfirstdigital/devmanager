# Architecture

DevManager is one product with two shipping binaries that share semantic version, build identity, and local protocol identity:

- `devmanager` / `devmanager.exe` — native GPUI desktop client (`devmanager/<version>`)
- `devmanager-host` / `devmanager-host.exe` — durable local execution host (`devmanager-host/<version>`)

Installers place both binaries as siblings so the client can attach to the exact signed host path. Development-only identities such as `devmanager-next` are not packaging outputs. cargo-packager loads both from `binaries-dir = "target/release"`.

## Authority boundary

The host owns durable state and local work: tasks, operations, process/Job trees, PTYs, provider runtimes, browser automation surfaces, workspace/Git/services, and Connect/device secrets held in the OS vault. Desktop, CLI, automation, and optional Connect clients are clients of the same typed command/event contract. UI code does not reach around the host to mutate process, task, Git, browser, or provider state.

## Host lifetime and terminal survivability

Terminals live exactly as long as the host process, so the host is built to outlive everything except an explicit full quit:

- Closing the desktop window is a detach, never a quit. Production and isolated debug hosts both keep running; the next client launch attaches to the existing host over the profile pipe. Debug hosts are parent-bound only when a harness launches the binary directly with `--parent-pid`, or when the client runs with `DEVMANAGER_DEBUG_HOST_PARENT_BOUND=1`; otherwise the desktop client launches them with `--detach-from-parent`.
- The client spawns the host with `CREATE_NO_WINDOW` so it never shares the launcher's console, and adds `CREATE_BREAKAWAY_FROM_JOB` when the client itself sits inside a kill-on-close Job that permits breakaway (see `current_process_job_containment`).
- A PTY read error or EOF is never treated as child death. The reader retries with bounded backoff while the wait actor still reports the child alive; only an observed child exit ends a terminal. This is what keeps terminals through Windows sleep/resume, where ConPTY reports transient EOFs.
- A single terminal's teardown miss is reported through diagnostics and its own kill-on-close Job handle; it never aborts the host, because an abort would end every other terminal the host owns.

Every managed PTY child still runs inside a host-owned kill-on-close Job Object, so a host crash, update, or full quit ends every terminal tree. Provider conversations are then restored through exact `--resume`; shell screen contents are not persisted across a host restart.

## Protocol and identity

Local compatibility uses the protocol constants in `src/protocol/capabilities.rs` (`PROTOCOL_MAJOR` / `PROTOCOL_MINOR`, currently `1.0`). Client and host builds must advertise matching package version metadata under the final shipping identity contract above; ctl automation uses `devmanager-host-ctl/<version>` against the same semver/protocol. Exact provider conversation identity (`providerSessionId`) is distinct from disposable PTY identity and is captured only from correlated current-generation Claude/Codex `SessionStart` hooks.

## Configuration and cutover data rules

Supported configuration lives in `config.json` and `remote.json` (project/folder/command/SSH settings; long-lived pairing/device/host identity and other schema-valid remote fields). Updates must not rotate the long-lived pairing code, task invitations, device keys, or host keys, and must not overwrite those files as part of ordinary packaging.

`session.json` and provider rollout/history directories are not a cutover import source. Packaging never embeds `session.json`, development profiles, worktrees, target/evidence trees, test fixtures, Portal proprietary trees, `zz-archive`, or secrets. SQLite task/prompt storage is created and migrated by the application; packages do not ship user prompt databases or organization content.

## Packaging contract

Authoritative packaging expectations live in `packaging/package-contract.json` and are enforced by `tests/package_contract.rs`, `tests/cutover_contract.rs`, `packaging/Assert-PackageContract.ps1`, and `scripts/native-next/Invoke-CutoverAudit.ps1`. Windows file metadata uses product name `DevManager` for both binaries, with distinct file descriptions (`DevManager` vs `DevManager Host`) and original filenames. Windows browser surfaces expect the Evergreen WebView2 runtime via `wry`/`webview2-com`.

Signed updater metadata (`latest.json`) is generated only after cryptographic signature verification. Public publication requires protected manual approval (`release-publish`) and is independent from packaging/staging. Approved design and phase plans under `docs/superpowers/` remain historical design records.
