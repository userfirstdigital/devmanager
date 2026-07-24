# Persistent Browser Pairing and Storage Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the browser invite code stable until manually changed, make production storage unreachable from development and unit-test defaults, surface remote-state load failures, and recover the running installed identity without restarting DevManager.

**Architecture:** Centralize execution-scoped storage selection in `persistence::app_config_dir()`: installed release builds retain the production directory, unprofiled debug builds use `dev-debug`, and unit tests use a process-unique temporary root. Browser pairing stops consuming the invite token, while explicit regeneration and reset retain their distinct semantics. Startup converts remote-state load failure into a visible disabled-remote diagnostic instead of silently accepting a fresh identity.

**Tech Stack:** Rust 2021, GPUI 0.2.2, Axum 0.7, Serde JSON, Windows `%APPDATA%`, PowerShell verification.

## Global Constraints

- Do not stop, restart, update, focus, click, or otherwise mutate the running installed DevManager while the user is working.
- Before the isolation boundary is green, every Rust test command must set `DEVMANAGER_PROFILE=codex-pairing-isolation-red`.
- Never run the two slash-command route tests before Task 1 is green.
- Never print pairing tokens, cookie-signing keys, client IDs, browser installation IDs, or complete `remote.json` contents.
- A successful pairing must leave `WebConfig.pairing_token` unchanged.
- **Generate new code** changes only the invitation code and does not revoke existing clients.
- **Reset browser access** revokes clients and rotates both the invitation code and cookie-signing key.
- Missing `remote.json` remains first-run behavior; an unreadable existing file must produce a visible diagnostic and must not be overwritten automatically.
- Installed release storage remains `%APPDATA%\com.userfirst.devmanager`.
- Unprofiled debug storage is `%APPDATA%\com.userfirst.devmanager-dev-debug`.
- Unit tests resolve beneath a process-unique temporary root, even when `DEVMANAGER_PROFILE` is absent.
- Keep the existing `dev-watch` profile behavior.
- Preserve all unrelated process-accounting working-tree changes.
- Do not restart the installed app until live identity recovery has succeeded and the user says it is safe.

---

### Task 1: Make production storage unreachable from debug and unit-test defaults

**Files:**

- Modify: `src/persistence/mod.rs`
- Modify: `src/remote/mod.rs`

**Interfaces:**

- Consumes: `DEVMANAGER_PROFILE`, `DEVMANAGER_INSTANCE_LABEL`, `dirs::config_dir()`.
- Produces:
  - `fn app_config_dir_for(base: &Path, profile: Option<&str>) -> PathBuf`
  - `fn default_debug_profile() -> Option<String>`
  - `#[cfg(test)] fn test_config_root() -> &'static Path`
  - `#[cfg(test)] fn ensure_test_config_dir_is_isolated(path: PathBuf) -> Result<PathBuf>`
  - unchanged public `pub fn app_config_dir() -> Result<PathBuf>`
  - unchanged public `pub fn app_instance_profile() -> Option<String>`

- [ ] **Step 1: Create an isolated worktree before any implementation test**

Use `superpowers:using-git-worktrees` from the current repository. Create a branch named `codex/persistent-browser-pairing` from the current `master` commit. Do not copy the dirty process-accounting files into the worktree.

Expected: the worktree contains this plan commit and design commit `3a1f95a`, and
`git status --short` is empty.

Build the untouched branch, then run only the persistence baseline behind the
temporary explicit profile. The full library suite is intentionally deferred
until Task 1 is green because it contains the two known contaminating tests:

```powershell
cargo build
$productionRemote = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$before = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
$env:DEVMANAGER_PROFILE='codex-pairing-isolation-red'
cargo test --lib persistence::tests:: -- --test-threads=1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$after = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
if ($before -ne $after) {
    throw 'Production remote.json changed during the isolated baseline.'
}
```

Expected: build and persistence baseline pass, and the production hash is
unchanged.

- [ ] **Step 2: Record a redacted production-state fingerprint**

Run this read-only command outside the app process:

```powershell
$path = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$item = Get-Item -LiteralPath $path
$json = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
[pscustomobject]@{
  Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash
  Length = $item.Length
  Modified = $item.LastWriteTimeUtc
  IsKnownFixture = ([string]$json.host.serverId -eq 'slash-route')
  HasKnownFixtureBrowser = @($json.host.web.pairedClients).Where({
    [string]$_.browserInstallId -eq 'slash-browser'
  }).Count -gt 0
} | ConvertTo-Json -Compress
```

Expected before recovery: `IsKnownFixture` and `HasKnownFixtureBrowser` are `true`. Do not output any other JSON fields.

Confirm the installed process without focusing its window:

```powershell
$process = Get-CimInstance Win32_Process -Filter "ProcessId = 44880"
$started = (Get-Process -Id 44880).StartTime.ToUniversalTime().ToString('o')
[pscustomobject]@{
  ProcessId = $process.ProcessId
  ExecutablePath = $process.ExecutablePath
  StartedUtc = $started
} | ConvertTo-Json -Compress
```

Expected: PID `44880`, executable
`C:\Users\micro\AppData\Local\DevManager\devmanager.exe`, and start time
`2026-07-23T21:20:16.0081856Z`.

- [ ] **Step 3: Write failing storage-scope tests**

In `src/persistence/mod.rs`, add tests expressing the desired API:

```rust
#[test]
fn unprofiled_unit_tests_never_resolve_the_production_directory() {
    let _profile = TestProfileEnvGuard::without_profile();
    let active = app_config_dir().expect("test config directory");
    let production = dirs::config_dir()
        .expect("production config parent")
        .join(APP_CONFIG_DIR);

    assert!(active.starts_with(std::env::temp_dir()));
    assert!(!active.starts_with(&production));
}

#[test]
fn named_unit_test_profiles_remain_beneath_the_test_root() {
    let _profile = TestProfileEnvGuard::new("pairing-isolation");
    let active = app_config_dir().expect("test config directory");

    assert!(active.starts_with(test_config_root()));
    assert!(active
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains("pairing-isolation")));
}

#[test]
fn production_and_debug_directory_names_are_explicit() {
    let base = Path::new("config-root");
    assert_eq!(
        app_config_dir_for(base, None),
        base.join("com.userfirst.devmanager")
    );
    assert_eq!(
        app_config_dir_for(base, Some("dev-debug")),
        base.join("com.userfirst.devmanager-dev-debug")
    );
}

#[test]
#[should_panic(expected = "unit-test config path must not use installed DevManager storage")]
fn unit_test_path_guard_rejects_the_production_tree() {
    let production = dirs::config_dir()
        .expect("production config parent")
        .join(APP_CONFIG_DIR);

    ensure_test_config_dir_is_isolated(production.join("nested"))
        .expect("unsafe test path must be rejected");
}
```

Remove the obsolete `app_config_dir_name_defaults_without_profile` test. The
production name is now tested through the pure resolver. `TestProfileEnvGuard`
is already visible to this test module, so `src/remote/mod.rs` needs no change
in this task unless the source changes before execution.

- [ ] **Step 4: Run the storage tests and verify RED safely**

Run:

```powershell
$env:DEVMANAGER_PROFILE='codex-pairing-isolation-red'
cargo test --lib persistence::tests::unprofiled_unit_tests_never_resolve_the_production_directory -- --nocapture
```

Expected: compile failure for the missing `app_config_dir_for` / `test_config_root` interfaces, or assertion failure because the active path is still under the normal configuration root. The explicit profile prevents any pre-fix write from using production.

- [ ] **Step 5: Implement execution-scoped path selection**

In `src/persistence/mod.rs`, add:

```rust
#[cfg(test)]
use std::sync::OnceLock;

fn configured_profile() -> Option<String> {
    sanitize_scope_segment(std::env::var(APP_PROFILE_ENV).ok())
}

fn default_debug_profile() -> Option<String> {
    #[cfg(all(debug_assertions, not(test)))]
    {
        return Some("dev-debug".to_string());
    }
    #[cfg(any(not(debug_assertions), test))]
    {
        None
    }
}

pub fn app_instance_profile() -> Option<String> {
    configured_profile().or_else(default_debug_profile)
}

fn app_config_dir_for(base: &Path, profile: Option<&str>) -> PathBuf {
    match profile {
        Some(profile) => base.join(format!("{APP_CONFIG_DIR}-{profile}")),
        None => base.join(APP_CONFIG_DIR),
    }
}

#[cfg(test)]
fn test_config_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "devmanager-unit-tests-{}-{nonce}",
            std::process::id()
        ))
    })
    .as_path()
}

#[cfg(test)]
fn ensure_test_config_dir_is_isolated(path: PathBuf) -> Result<PathBuf> {
    if let Some(production) =
        dirs::config_dir().map(|base| app_config_dir_for(&base, None))
    {
        assert!(
            !path.starts_with(&production),
            "unit-test config path must not use installed DevManager storage"
        );
    }
    Ok(path)
}

pub fn app_config_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        let path = app_config_dir_for(
            test_config_root(),
            configured_profile().as_deref(),
        );
        return ensure_test_config_dir_is_isolated(path);
    }
    #[cfg(not(test))]
    {
        dirs::config_dir()
            .map(|base| app_config_dir_for(&base, app_instance_profile().as_deref()))
            .ok_or(PersistenceError::ConfigDirectoryUnavailable)
    }
}
```

Remove the old `app_config_dir_name()` implementation or retain it only as a thin call to `app_config_dir_for` if another caller needs it. Make `app_display_name()` fall back to the active profile when no explicit instance label exists:

```rust
pub fn app_display_name() -> String {
    let label = app_instance_label().or_else(app_instance_profile);
    match label {
        Some(label) => format!("DevManager [{label}]"),
        None => "DevManager".to_string(),
    }
}
```

- [ ] **Step 6: Run storage and persistence tests and verify GREEN**

Run without any profile override:

```powershell
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --lib persistence:: -- --nocapture
```

Expected: all persistence tests pass, and any temporary directories are beneath `%TEMP%`.

- [ ] **Step 7: Prove the production file was untouched**

Re-run the Step 2 fingerprint and compare `Hash`, `Length`, and `Modified` to the baseline.

Expected: exact match.

- [ ] **Step 8: Commit the storage boundary**

```powershell
git add src/persistence/mod.rs src/remote/mod.rs
git commit -m "fix: isolate development and test storage"
```

### Task 2: Keep the browser invitation code stable

**Files:**

- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/mod.rs`

**Interfaces:**

- Consumes: `WebConfig.pairing_token`, `RemoteHostService::regenerate_web_pairing_token`, `RemoteHostService::reset_browser_access`.
- Produces: pairing success that does not mutate `WebConfig.pairing_token`; existing manual regenerate/reset APIs remain unchanged.

- [ ] **Step 1: Replace the single-use tests with stable-code regressions**

In `src/remote/web/mod.rs`, replace the sequential-consumption test with:

```rust
#[test]
fn pair_handler_reuses_stable_invitation_for_multiple_browsers() {
    let _profile = TestProfileGuard::new("web-pair-stable-sequential");
    let service = test_service("host-a");
    let state = test_state(&service);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    for browser_install_id in ["phone-install", "tablet-install"] {
        let response = runtime.block_on(pair_handler(
            State(state.clone()),
            ConnectInfo(test_addr()),
            test_headers(None),
            Query(PairQuery {
                t: Some("PAIR1234".to_string()),
                label: None,
                browser_install_id: Some(browser_install_id.to_string()),
            }),
        ));
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    let config = service.config();
    assert_eq!(config.web.pairing_token, "PAIR1234");
    assert_eq!(config.web.paired_clients.len(), 2);
}
```

Replace the concurrent-consumption test with:

```rust
#[test]
fn pair_handler_accepts_concurrent_reuse_for_unique_browsers() {
    let _profile = TestProfileGuard::new("web-pair-stable-concurrent");
    let service = test_service("host-a");
    let state = test_state(&service);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");

    let mut statuses = runtime.block_on(async {
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let mut requests = Vec::new();
        for (index, browser_install_id) in
            ["phone-install", "tablet-install"].into_iter().enumerate()
        {
            let state = state.clone();
            let start = start.clone();
            requests.push(tokio::spawn(async move {
                start.wait().await;
                pair_handler(
                    State(state),
                    ConnectInfo(SocketAddr::from((
                        [127, 0, 0, (index + 1) as u8],
                        43872,
                    ))),
                    test_headers(None),
                    Query(PairQuery {
                        t: Some("PAIR1234".to_string()),
                        label: None,
                        browser_install_id: Some(browser_install_id.to_string()),
                    }),
                )
                .await
                .status()
            }));
        }

        let mut statuses = Vec::new();
        for request in requests {
            statuses.push(request.await.expect("pair request task"));
        }
        statuses
    });
    drop(runtime);

    statuses.sort_unstable();
    assert_eq!(statuses, [StatusCode::SEE_OTHER, StatusCode::SEE_OTHER]);
    let config = service.config();
    assert_eq!(config.web.paired_clients.len(), 2);
    assert_eq!(config.web.pairing_token, "PAIR1234");
}
```

Update `pair_handler_persists_paired_client_immediately` to assert:

```rust
assert_eq!(saved.host.web.pairing_token, "PAIR1234");
```

- [ ] **Step 2: Run the stable-code tests and verify RED**

Run:

```powershell
cargo test --lib pair_handler_reuses_stable_invitation_for_multiple_browsers -- --nocapture
cargo test --lib pair_handler_accepts_concurrent_reuse_for_unique_browsers -- --nocapture
cargo test --lib pair_handler_persists_paired_client_immediately -- --nocapture
```

Expected: the second sequential pairing returns `401 Unauthorized`, or the persisted token differs from `PAIR1234`.

- [ ] **Step 3: Stop consuming the token**

In `pair_handler`, remove only:

```rust
config.web.pairing_token = generate_web_pairing_token();
```

Retain the atomic enabled/token predicate, client upsert, random client identity, cookie signing, activity logging, rate-limit success reset, and immediate persistence.

- [ ] **Step 4: Add manual-regeneration invariants**

Extend the existing remote-service tests in `src/remote/mod.rs`:

```rust
#[test]
fn regenerating_browser_invite_preserves_existing_browser_authority() {
    let _profile = TestProfileGuard::new("regenerate-web-invite-preserves-clients");
    let mut config = RemoteHostConfig::default();
    let original_token = config.web.pairing_token.clone();
    config.web.paired_clients.push(PairedWebClient {
        client_id: "web-client-1".to_string(),
        browser_install_id: "browser-install-1".to_string(),
        label: "Phone".to_string(),
        ..PairedWebClient::default()
    });
    let subscription = validate_registration(PushRegistrationRequest {
        mode: PushRegistrationMode::Reconcile,
        endpoint: "https://web.push.apple.com/QM-regenerate".to_string(),
        keys: PushRegistrationKeys {
            p256dh: config.web.push.vapid_public_key_base64.clone(),
            auth: URL_SAFE_NO_PAD.encode([7_u8; 16]),
        },
    })
    .expect("valid push subscription");
    config
        .web
        .push
        .enable_and_replace_subscription("web-client-1", subscription, 1)
        .expect("enable push subscription");
    config.web.activity_log.push(RemoteAccessActivityEvent {
        client_id: "web-client-1".to_string(),
        source: RemoteAccessSource::Browser,
        event_kind: RemoteAccessActivityKind::Connected,
        label: "Phone".to_string(),
        ip_address: Some("127.0.0.1".to_string()),
        event_at_epoch_ms: Some(1),
        browser_family: Some("Safari".to_string()),
        browser_version: Some("18".to_string()),
        os_family: Some("iOS".to_string()),
        device_class: Some("phone".to_string()),
    });
    let original_secret = config.web.cookie_secret_hex.clone();
    let original_push = config.web.push.clone();
    let original_activity = config.web.activity_log.clone();
    let service = RemoteHostService::new(config);

    let new_token = service
        .regenerate_web_pairing_token()
        .expect("regenerate browser invite");
    let saved = service.config();
    let persisted = load_remote_machine_state()
        .expect("load persisted regenerated browser invite");

    assert_ne!(new_token, original_token);
    assert_eq!(saved.web.pairing_token, new_token);
    assert_eq!(saved.web.paired_clients.len(), 1);
    assert_eq!(saved.web.cookie_secret_hex, original_secret);
    assert_eq!(saved.web.push, original_push);
    assert_eq!(saved.web.activity_log, original_activity);
    assert_eq!(persisted.host.web, saved.web);
}
```

Keep the existing reset assertions. After them, add:

```rust
let persisted =
    load_remote_machine_state().expect("load persisted browser reset");
assert_eq!(persisted.host.web, saved.web);
```

- [ ] **Step 5: Run the pairing and reset suites and verify GREEN**

Run:

```powershell
cargo test --lib remote::web::tests::pair_handler_ -- --nocapture
cargo test --lib remote::tests::regenerating_browser_invite_preserves_existing_browser_authority -- --nocapture
cargo test --lib remote::tests::reset_browser_access_rotates_cookie_and_disconnects_live_browsers -- --nocapture
```

Expected: all selected tests pass.

- [ ] **Step 6: Commit stable invitation semantics**

```powershell
git add src/remote/web/mod.rs src/remote/mod.rs
git commit -m "fix: keep browser invite codes stable"
```

### Task 3: Make the formerly contaminating route tests explicitly isolated

**Files:**

- Modify: `src/remote/web/mod.rs`

**Interfaces:**

- Consumes: `TestProfileGuard::new`.
- Produces: route tests whose persistence lifetime is explicitly scoped even though Task 1 already prevents production fallback.

- [ ] **Step 1: Add explicit guards to both slash-command route tests**

Add as the first statement of each test:

```rust
let _profile = TestProfileGuard::new("web-slash-command-route");
```

and:

```rust
let _profile = TestProfileGuard::new("web-slash-command-invalid-route");
```

- [ ] **Step 2: Run the exact formerly contaminating tests behind one hash guard**

Run:

```powershell
$productionRemote = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$before = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --lib slash_command_route_requires_pairing_and_returns_safe_live_provider_metadata -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --lib slash_command_route_rejects_unknown_and_non_ai_sessions -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$after = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
if ($before -ne $after) {
    throw 'Production remote.json changed while running isolated tests.'
}
```

Expected: both tests pass, the hash comparison emits no output, and the command
exits 0.

- [ ] **Step 3: Commit explicit route isolation**

```powershell
git add src/remote/web/mod.rs
git commit -m "test: isolate remote web route persistence"
```

### Task 4: Surface remote-state load failures instead of accepting a new identity

**Files:**

- Modify: `src/app/mod.rs`
- Modify: `src/remote/mod.rs`
- Test: unit tests inside `src/app/mod.rs`
- Test: unit tests inside `src/remote/mod.rs`

**Interfaces:**

- Consumes: `remote::load_remote_machine_state() -> Result<RemoteMachineState, PersistenceError>`.
- Produces:
  - `struct RemoteStateStartup { state: RemoteMachineState, diagnostic: Option<String> }`
  - `fn resolve_remote_state_startup(result: Result<RemoteMachineState, PersistenceError>) -> RemoteStateStartup`

- [ ] **Step 1: Write failing startup-resolution tests**

Add:

```rust
#[test]
fn remote_state_load_failure_disables_remote_and_surfaces_diagnostic() {
    let startup = resolve_remote_state_startup(Err(
        crate::persistence::PersistenceError::ConfigDirectoryUnavailable,
    ));

    assert!(!startup.state.host.enabled);
    assert!(!startup.state.host.web.enabled);
    let diagnostic = startup.diagnostic.expect("load diagnostic");
    assert!(diagnostic.contains("Remote access is disabled"));
    assert!(diagnostic.contains("could not determine the user config directory"));
}

#[test]
fn valid_remote_state_has_no_startup_diagnostic() {
    let mut state = RemoteMachineState::default();
    state.host.web.enabled = true;

    let startup = resolve_remote_state_startup(Ok(state.clone()));

    assert_eq!(startup.state, state);
    assert!(startup.diagnostic.is_none());
}
```

Also add two characterization tests beside the existing remote persistence
tests:

```rust
#[test]
fn missing_remote_state_remains_first_run_without_persisting() {
    let _profile = TestProfileGuard::new("remote-state-missing");
    let path = super::remote_state_path().expect("remote state path");
    assert!(!path.exists());

    let state = load_remote_machine_state().expect("missing state is first run");

    assert!(!state.host.enabled);
    assert!(!state.host.web.enabled);
    assert!(!path.exists(), "first-run load must not persist automatically");
}

#[test]
fn malformed_remote_state_returns_error_without_replacing_bytes() {
    let _profile = TestProfileGuard::new("remote-state-malformed");
    let path = super::remote_state_path().expect("remote state path");
    std::fs::create_dir_all(path.parent().expect("remote state directory"))
        .expect("create remote state directory");
    let malformed = b"{ not valid remote json";
    std::fs::write(&path, malformed).expect("write malformed remote state");

    let error = load_remote_machine_state().expect_err("malformed state must fail");

    assert!(matches!(
        error,
        crate::persistence::PersistenceError::Parse { .. }
    ));
    assert_eq!(
        std::fs::read(&path).expect("read preserved malformed state"),
        malformed
    );
}
```

- [ ] **Step 2: Run the startup test and verify RED; record the existing persistence behavior**

Run:

```powershell
cargo test --lib remote_state_load_failure_disables_remote_and_surfaces_diagnostic -- --nocapture
cargo test --lib missing_remote_state_remains_first_run_without_persisting -- --nocapture
cargo test --lib malformed_remote_state_returns_error_without_replacing_bytes -- --nocapture
```

Expected: the first command fails to compile because `RemoteStateStartup` and
`resolve_remote_state_startup` do not exist. The two characterization tests
pass and prove the lower-level loader already distinguishes missing and
malformed files without changing their bytes.

- [ ] **Step 3: Implement startup resolution**

Add near other startup helpers:

```rust
struct RemoteStateStartup {
    state: RemoteMachineState,
    diagnostic: Option<String>,
}

fn resolve_remote_state_startup(
    result: Result<RemoteMachineState, crate::persistence::PersistenceError>,
) -> RemoteStateStartup {
    match result {
        Ok(state) => RemoteStateStartup {
            state,
            diagnostic: None,
        },
        Err(error) => RemoteStateStartup {
            state: RemoteMachineState::default(),
            diagnostic: Some(format!(
                "Remote access is disabled because its saved identity could not be loaded: {error}. \
                 DevManager did not replace the existing remote state."
            )),
        },
    }
}
```

Replace:

```rust
let remote_machine_state = remote::load_remote_machine_state().unwrap_or_default();
```

with:

```rust
let remote_startup = resolve_remote_state_startup(remote::load_remote_machine_state());
let remote_machine_state = remote_startup.state;
```

Merge `remote_startup.diagnostic` into `startup_notice` using the same newline-combining pattern already used for `browser_config_diagnostic`.

- [ ] **Step 4: Run startup and remote persistence tests and verify GREEN**

Run:

```powershell
cargo test --lib remote_state_load_failure_disables_remote_and_surfaces_diagnostic -- --nocapture
cargo test --lib valid_remote_state_has_no_startup_diagnostic -- --nocapture
cargo test --lib missing_remote_state_remains_first_run_without_persisting -- --nocapture
cargo test --lib malformed_remote_state_returns_error_without_replacing_bytes -- --nocapture
cargo test --lib remote_machine_state_round_trips_web_pairing_fields -- --nocapture
```

Expected: all selected tests pass.

- [ ] **Step 5: Commit startup safety**

```powershell
git add src/app/mod.rs src/remote/mod.rs
git commit -m "fix: preserve unreadable remote identities"
```

### Task 5: Update operator documentation

**Files:**

- Modify: `docs/REMOTE_MOBILE_WEB.md`
- Modify: `docs/superpowers/specs/2026-07-24-persistent-browser-pairing-and-storage-isolation-design.md`

**Interfaces:**

- Consumes: verified code behavior from Tasks 1–4.
- Produces: operator-facing stable invite semantics and completed design status.

- [ ] **Step 1: Update pairing documentation**

Replace the single-use wording in `docs/REMOTE_MOBILE_WEB.md` with:

```markdown
A successful invite pairs the browser, stores a signed host-specific cookie,
and redirects into the app. The browser pair code remains valid for pairing
additional devices until **Generate new code** or **Reset access** changes it.
Generating a new code affects future pairings only; existing paired browsers
remain valid. Reset access revokes every browser and creates new pairing and
cookie-signing secrets.
```

In the proxy checklist, replace “one-time invitation” with “invitation
secret”; the `/pair` query string remains sensitive even though the code is
stable.

- [ ] **Step 2: Document storage isolation**

Add a short development note:

```markdown
Installed release builds use the production profile. Unprofiled debug builds
use `dev-debug`, `dev-watch.ps1` uses `dev-watch`, and Rust unit tests use
process-unique temporary storage. Development and tests must never read or
write the installed profile.
```

- [ ] **Step 3: Mark the design implemented only after verification**

Do not change the design status yet. After Task 6 passes, set:

```markdown
**Status:** Implemented and verified
```

Append:

```markdown
### Implementation evidence (2026-07-24)

- `cargo fmt -- --check` passed.
- Focused persistence, remote-web, remote-service, and app tests passed.
- `cargo test --lib --quiet -- --test-threads=1` passed with 0 failures.
- `cargo clippy --all-targets --all-features --message-format=short` passed.
- `cargo build --all-features` passed.
- The production `remote.json` SHA-256 hash matched before and after the
  formerly contaminating route tests and the complete serial library suite.
- The installed DevManager process remained running with the same PID and
  start time; no GUI interaction or restart was performed.
```

- [ ] **Step 4: Commit documentation after verification**

```powershell
git add docs/REMOTE_MOBILE_WEB.md docs/superpowers/specs/2026-07-24-persistent-browser-pairing-and-storage-isolation-design.md
git commit -m "docs: record persistent browser trust"
```

### Task 6: Complete review and verification without touching the running app

**Files:**

- Review all files changed by Tasks 1–5.

**Interfaces:**

- Consumes: all implementation commits.
- Produces: verified code and proof that production storage remained unchanged.

- [ ] **Step 1: Format and check the diff**

Run:

```powershell
cargo fmt
cargo fmt -- --check
$base = git merge-base HEAD master
git diff --check $base --
```

Expected: all commands exit 0.

- [ ] **Step 2: Run focused suites**

Run:

```powershell
cargo test --lib persistence:: -- --test-threads=1
cargo test --lib remote::web:: -- --test-threads=1
cargo test --lib remote::tests:: -- --test-threads=1
cargo test --lib app::tests:: -- --test-threads=1
```

Expected: all selected tests pass.

- [ ] **Step 3: Run the complete serial library suite**

Capture the production hash before and after:

```powershell
$productionRemote = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$before = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
cargo test --lib --quiet -- --test-threads=1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$after = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
if ($before -ne $after) {
    throw 'Production remote.json changed during the full Rust test suite.'
}
```

Expected: complete suite passes and the production hash is unchanged.

- [ ] **Step 4: Run static and build gates**

Run:

```powershell
cargo clippy --all-targets --all-features --message-format=short
cargo build --all-features
```

Expected: exit 0. Existing repository warning baseline may remain; no new warnings from changed code.

- [ ] **Step 5: Review security and scope**

Inspect the complete diff and verify:

- no secret values are logged or added to tests;
- no test path can equal the production profile;
- stable invite reuse retains rate limiting;
- manual regenerate preserves clients and cookie secret;
- reset revokes clients and rotates both secrets;
- remote load failure cannot overwrite the existing file;
- the running installed process PID and start time are unchanged;
- unrelated process-accounting files are absent from this worktree.

Use this read-only process assertion for the installed-process check:

```powershell
$process = Get-CimInstance Win32_Process -Filter "ProcessId = 44880"
if ($null -eq $process) {
    throw 'The installed DevManager process is no longer running.'
}
$started = (Get-Process -Id 44880).StartTime.ToUniversalTime().ToString('o')
if (
    $process.ExecutablePath -ne 'C:\Users\micro\AppData\Local\DevManager\devmanager.exe' -or
    $started -ne '2026-07-23T21:20:16.0081856Z'
) {
    throw 'The installed DevManager process identity changed.'
}
```

- [ ] **Step 6: Update and commit verification evidence**

Update the design status and verification section, then commit the Task 5 documentation.

### Task 7: Integrate safely and recover only when the user is ready

**Files:**

- Integrate pairing commits into `C:\Code\userfirst\devmanager` without staging or discarding existing process-accounting changes.
- Operationally recover `%APPDATA%\com.userfirst.devmanager\remote.json`.

**Interfaces:**

- Consumes: verified pairing branch; still-running installed DevManager with the pre-contamination identity in memory.
- Produces: source changes in the main working tree and a recovered production identity.

- [ ] **Step 1: Integrate the verified commits**

Use `superpowers:finishing-a-development-branch`. Prefer a local merge or cherry-pick that preserves the dirty process-accounting changes. Before each operation, inspect overlap with:

```powershell
git status --short
git diff --name-only
git show --stat --oneline codex/persistent-browser-pairing
```

Expected: no pairing implementation file overlaps the existing process-accounting modifications except intentionally shared documentation.

- [ ] **Step 2: Re-run a focused test in the main working tree**

Run:

```powershell
$productionRemote = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$before = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --lib persistence:: -- --test-threads=1
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --lib pair_handler_reuses_stable_invitation_for_multiple_browsers -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --lib pair_handler_accepts_concurrent_reuse_for_unique_browsers -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --lib slash_command_route_requires_pairing_and_returns_safe_live_provider_metadata -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo test --lib slash_command_route_rejects_unknown_and_non_ai_sessions -- --nocapture
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$after = (Get-FileHash -Algorithm SHA256 -LiteralPath $productionRemote).Hash
if ($before -ne $after) {
    throw 'Production remote.json changed during main-tree verification.'
}
```

Expected: tests pass and the hash is unchanged.

- [ ] **Step 3: Wait for explicit recovery readiness**

Do not focus or click the running app while the user is working. Ask the user to say when it is safe to perform the recovery action.

- [ ] **Step 4: Persist the running in-memory identity**

When the user says it is safe, use the installed app’s **Generate new browser code** action once. This is the old installed version’s safe full-config persistence path. Do not expose the generated code.

- [ ] **Step 5: Verify recovered disk identity**

Read only redacted properties:

```powershell
$path = Join-Path $env:APPDATA 'com.userfirst.devmanager\remote.json'
$json = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json
if ([string]$json.host.serverId -eq 'slash-route') {
    throw 'Fixture server identity is still present.'
}
if (@($json.host.web.pairedClients).Where({
    [string]$_.browserInstallId -eq 'slash-browser'
}).Count -gt 0) {
    throw 'Fixture browser identity is still present.'
}
if ([string]::IsNullOrWhiteSpace([string]$json.host.web.cookieSecretHex)) {
    throw 'Recovered cookie-signing key is missing.'
}
if ([string]$json.host.web.cookieSecretHex -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'Recovered cookie-signing key has an invalid shape.'
}
if (@($json.host.web.pairedClients).Count -eq 0) {
    throw 'No trusted browser identities were recovered.'
}
icacls $path
```

Expected: no fixture identity, a non-empty paired-client set if the live app retained it, a valid signing-key shape, and current-user-only ACL.

- [ ] **Step 6: Restart only with the user’s approval**

After recovery is verified, ask the user before restarting or installing a new build. Confirm the existing remote browser reconnects without entering the invite code.

- [ ] **Step 7: Sharpening the Axe**

Record one consolidated project rule in the narrowest existing authority:

> Release, debug, and test persistence scopes must be structurally disjoint; tests never prove storage behavior by touching the installed profile. Persisted authentication identity is loaded fail-closed and may rotate only through an explicit user action.

Update the existing design specification rather than creating a duplicate guidance file.
