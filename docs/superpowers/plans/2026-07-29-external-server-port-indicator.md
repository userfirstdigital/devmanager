# External Server Port Indicator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a blue sidebar dot when a configured server port is listening outside DevManager without adding port work to the render path.

**Architecture:** Extend the existing sidebar indicator model with an external-listener state, then derive that state from the already cached `PortStatus` ownership snapshot. Expand the existing single background snapshot from live-session ports to all configured ports and use an adaptive one-second active / three-second idle refresh interval.

**Tech Stack:** Rust, GPUI, Windows TCP listener-table snapshot through the existing platform service, Cargo unit tests.

## Global Constraints

- Blue means a configured TCP port has a listener whose PID is outside the corresponding DevManager session's tracked process tree; it is not an HTTP health claim.
- Perform one batched snapshot for all configured ports on GPUI's background executor.
- Do not add operating-system port inspection or process-name lookup to rendering.
- Preserve the existing single-flight scan guard.
- Refresh every one second with a live DevManager server session and every three seconds when all configured server sessions are inactive.
- Preserve orange for `Starting`, text for `Stopping`, and existing behavior for commands without configured ports.
- Run the complete Rust library suite serially without an externally set `DEVMANAGER_PROFILE`.
- Confirm the installed DevManager PID/start time and production `config.json` and `remote.json` hashes are unchanged after verification.

---

### Task 1: Add the blue external-listener presentation

**Files:**
- Modify: `src/theme/mod.rs`
- Modify: `src/sidebar/mod.rs`
- Test: `src/sidebar/mod.rs`

**Interfaces:**
- Produces: `theme::EXTERNAL_TEXT: u32`
- Produces: `sidebar::ServerIndicatorState::External`
- Consumes: existing `server_status_label`, `server_status_indicator`, and `server_status_color`

- [ ] **Step 1: Write the failing sidebar presentation test**

Extend the existing server-indicator test in `src/sidebar/mod.rs` with literal expectations:

```rust
assert_eq!(server_status_label(ServerIndicatorState::External), "");
assert_eq!(
    server_status_color(ServerIndicatorState::External),
    theme::EXTERNAL_TEXT
);
```

The production change this catches is an external listener being rendered with
text or with a non-blue state color.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```powershell
cargo test --lib sidebar::tests::server_indicator_uses_warning_for_unready_and_success_for_ready -- --exact
```

Expected: compilation fails because `ServerIndicatorState::External` and
`theme::EXTERNAL_TEXT` do not exist.

- [ ] **Step 3: Add the minimal external state and blue token**

Add the dedicated theme token in `src/theme/mod.rs`:

```rust
pub const EXTERNAL_TEXT: u32 = 0x60a5fa; // blue-400
```

Add `External` to `ServerIndicatorState` and map it as a dot with no label:

```rust
pub enum ServerIndicatorState {
    Stopped,
    Unready,
    Ready,
    External,
    Stopping,
    Crashed,
    Exited,
    Failed,
}
```

Update the presentation matches:

```rust
ServerIndicatorState::Stopped
| ServerIndicatorState::Unready
| ServerIndicatorState::Ready
| ServerIndicatorState::External => "",
```

```rust
ServerIndicatorState::Stopped
    | ServerIndicatorState::Unready
    | ServerIndicatorState::Ready
    | ServerIndicatorState::External
```

```rust
ServerIndicatorState::External => theme::EXTERNAL_TEXT,
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```powershell
cargo test --lib sidebar::tests::server_indicator_uses_warning_for_unready_and_success_for_ready -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit the presentation change**

```powershell
git add src/theme/mod.rs src/sidebar/mod.rs
git commit -m "feat: add external server indicator style"
```

### Task 2: Track all configured ports with adaptive background cadence

**Files:**
- Modify: `src/app/mod.rs`
- Test: `src/app/mod.rs`

**Interfaces:**
- Consumes: `tracked_server_ports(state: &AppState) -> Vec<u16>`
- Consumes: `live_server_ports(state: &AppState, runtime: &RuntimeState) -> Vec<u16>`
- Produces: `server_port_snapshot_plan(state: &AppState, runtime: &RuntimeState) -> (Vec<u16>, Duration)`
- Preserves: `ServerPortSnapshotState::refresh_in_flight`

- [ ] **Step 1: Write a failing test for snapshot scope and cadence**

Add a test for the production plan consumed by the background sampler:

```rust
#[test]
fn server_port_snapshot_plan_tracks_inactive_ports_once_and_adapts_cadence() {
    let mut state = AppState::default();
    let mut project = sample_project();
    project.folders[0].commands[0].port = Some(5174);
    project.folders[0].commands.push(RunCommand {
        id: "server-cmd-2".to_string(),
        port: Some(5174),
        ..Default::default()
    });
    project.folders[0].commands.push(RunCommand {
        id: "server-cmd-3".to_string(),
        port: Some(4321),
        ..Default::default()
    });
    state.config.projects.push(project);
    let mut runtime = RuntimeState::new(false);

    let (ports, interval) = server_port_snapshot_plan(&state, &runtime);
    assert_eq!(ports, vec![4321, 5174]);
    assert_eq!(interval, Duration::from_secs(3));

    let mut session = SessionRuntimeState::new(
        "server-cmd",
        PathBuf::from("."),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );
    session.status = SessionStatus::Running;
    runtime.sessions.insert("server-cmd".to_string(), session);

    let (ports, interval) = server_port_snapshot_plan(&state, &runtime);
    assert_eq!(ports, vec![4321, 5174]);
    assert_eq!(interval, Duration::from_secs(1));
}
```

The test catches reverting the sampler plan to live-session ports, losing
deduplication, running idle scans at the active cadence, or monitoring a live
server too slowly.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test --lib app::tests::server_port_snapshot_plan_tracks_inactive_ports_once_and_adapts_cadence -- --exact
```

Expected: compilation fails because `server_port_snapshot_plan` does not exist.

- [ ] **Step 3: Expand the snapshot scope and implement adaptive cadence**

Add the pure plan used by `sync_server_port_snapshot`:

```rust
fn server_port_snapshot_plan(
    state: &AppState,
    runtime: &RuntimeState,
) -> (Vec<u16>, std::time::Duration) {
    let refresh_interval = if live_server_ports(state, runtime).is_empty() {
        Duration::from_secs(3)
    } else {
        Duration::from_secs(1)
    };
    (tracked_server_ports(state), refresh_interval)
}
```

At the start of `sync_server_port_snapshot`, consume both values:

```rust
let (tracked_ports, refresh_interval) = server_port_snapshot_plan(&self.state, runtime);
```

Use `refresh_interval` in the elapsed comparison and remove the old
`server_port_refresh_interval` helper. Preserve the existing
`refresh_in_flight` check, background executor call, stale-status pruning, and
cached-map render consumption.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```powershell
cargo test --lib app::tests::server_port_snapshot_plan_tracks_inactive_ports_once_and_adapts_cadence -- --exact
```

Expected: PASS.

- [ ] **Step 5: Commit the snapshot change**

```powershell
git add src/app/mod.rs
git commit -m "perf: monitor configured server ports in background"
```

### Task 3: Derive external ownership with explicit precedence

**Files:**
- Modify: `src/app/mod.rs`
- Test: `src/app/mod.rs`

**Interfaces:**
- Consumes: `runtime_owns_port(session: &SessionRuntimeState, status: &PortStatus) -> bool`
- Produces: `derive_server_indicator(...) -> sidebar::ServerIndicatorState::External`

- [ ] **Step 1: Write failing external-listener derivation tests**

Change the existing foreign-owner assertion in
`derive_server_indicator_uses_managed_port_ownership`:

```rust
assert_eq!(
    indicators.get("server-cmd"),
    Some(&sidebar::ServerIndicatorState::External)
);
```

Add an absent-session test:

```rust
#[test]
fn derive_server_indicator_detects_external_listener_without_session() {
    let statuses = HashMap::from([(
        5174,
        PortStatus {
            port: 5174,
            in_use: true,
            pid: Some(99),
            process_name: None,
        },
    )]);

    assert_eq!(
        derive_server_indicator(None, Some(5174), &statuses),
        sidebar::ServerIndicatorState::External
    );
}
```

Add a precedence test:

```rust
#[test]
fn derive_server_indicator_preserves_active_transitions_over_external_listener() {
    let statuses = HashMap::from([(
        5174,
        PortStatus {
            port: 5174,
            in_use: true,
            pid: Some(99),
            process_name: None,
        },
    )]);
    let mut session = SessionRuntimeState::new(
        "server-cmd",
        PathBuf::from("."),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );

    session.status = SessionStatus::Starting;
    assert_eq!(
        derive_server_indicator(Some(&session), Some(5174), &statuses),
        sidebar::ServerIndicatorState::Unready
    );

    session.status = SessionStatus::Stopping;
    assert_eq!(
        derive_server_indicator(Some(&session), Some(5174), &statuses),
        sidebar::ServerIndicatorState::Stopping
    );
}
```

Add a table-driven inactive-state test with literal expected values:

```rust
#[test]
fn derive_server_indicator_prefers_external_listener_for_inactive_sessions() {
    let statuses = HashMap::from([(
        5174,
        PortStatus {
            port: 5174,
            in_use: true,
            pid: Some(99),
            process_name: None,
        },
    )]);
    let mut session = SessionRuntimeState::new(
        "server-cmd",
        PathBuf::from("."),
        SessionDimensions::default(),
        TerminalBackend::PortablePtyFeedingAlacritty,
    );

    for status in [
        SessionStatus::Stopped,
        SessionStatus::Exited,
        SessionStatus::Crashed,
        SessionStatus::Failed,
    ] {
        session.status = status;
        assert_eq!(
            derive_server_indicator(Some(&session), Some(5174), &statuses),
            sidebar::ServerIndicatorState::External,
            "status {status:?}"
        );
    }
}
```

Extend the existing ownership test with a no-listener assertion:

```rust
port_statuses.insert(
    5174,
    PortStatus {
        port: 5174,
        in_use: false,
        pid: None,
        process_name: None,
    },
);
let indicators = derive_server_indicator_states(&state, &runtime, &port_statuses);
assert_eq!(
    indicators.get("server-cmd"),
    Some(&sidebar::ServerIndicatorState::Unready)
);
```

The tests catch missing external detection, treating a foreign owner as managed
readiness, treating a closed port as external, or allowing blue to replace
active starting/stopping transitions.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
cargo test --lib app::tests::derive_server_indicator_ -- --nocapture
```

Expected: external-listener assertions fail because the current derivation
returns `Stopped` or `Unready`.

- [ ] **Step 3: Implement minimal external-listener derivation**

Compute whether the cached listener is external before matching session state:

```rust
let external_listener = port
    .and_then(|port| port_statuses.get(&port))
    .is_some_and(|status| {
        status.in_use
            && session
                .map(|session| !runtime_owns_port(session, status))
                .unwrap_or(true)
    });
```

For an absent session, return `External` when that value is true and `Stopped`
otherwise. Preserve `Starting` and `Stopping`. For `Running` with a configured
port, return `Ready` for a managed owner, `External` for a foreign listener,
and `Unready` for no listener. For stopped, exited, crashed, and failed
sessions, return `External` when a foreign listener exists and otherwise
preserve their current state.

- [ ] **Step 4: Run derivation and sidebar tests and verify GREEN**

Run:

```powershell
cargo test --lib app::tests::derive_server_indicator_ -- --nocapture
cargo test --lib sidebar::tests::server_indicator_uses_warning_for_unready_and_success_for_ready -- --exact
```

Expected: all focused indicator tests PASS.

- [ ] **Step 5: Commit the ownership derivation**

```powershell
git add src/app/mod.rs
git commit -m "feat: show externally managed server ports"
```

### Task 4: Verify the complete change and installed-app isolation

**Files:**
- Verify: all files changed since `8a43cdf`
- Verify: production `%APPDATA%\com.userfirst.devmanager\config.json`
- Verify: production `%APPDATA%\com.userfirst.devmanager\remote.json`

**Interfaces:**
- Consumes: committed Tasks 1-3
- Produces: verification evidence and a clean isolated branch

- [ ] **Step 1: Review the complete diff**

Run:

```powershell
git diff --check 8a43cdf..HEAD
git diff --stat 8a43cdf..HEAD
git diff 8a43cdf..HEAD -- src/theme/mod.rs src/sidebar/mod.rs src/app/mod.rs
```

Check state precedence, exhaustive matches, no render-time port calls, no
process-name lookup, and preservation of the single-flight guard. Correct any
issue in one focused batch and rerun the affected focused tests.

- [ ] **Step 2: Run formatting and focused verification**

Run:

```powershell
cargo fmt --check
cargo test --lib app::tests::server_port_snapshot_plan_tracks_inactive_ports_once_and_adapts_cadence -- --exact
cargo test --lib app::tests::derive_server_indicator_ -- --nocapture
cargo test --lib sidebar::tests::server_indicator_uses_warning_for_unready_and_success_for_ready -- --exact
```

Expected: formatting and all focused tests PASS.

- [ ] **Step 3: Run the complete serial Rust library suite**

Tell the user the generated `target\debug\deps` test executable is expected,
then run:

```powershell
cargo test --lib -- --test-threads=1
```

Expected: the suite passes with the repository's documented ignored-test count.

- [ ] **Step 4: Confirm no development process remains**

After the suite exits, confirm there are no Cargo, rustc, or worktree test
executables:

```powershell
Get-Process cargo,rustc -ErrorAction SilentlyContinue
Get-Process | Where-Object {
    $_.Path -like '*\external-port-indicator\target\debug\deps\*.exe'
}
```

Expected: no matching processes.

- [ ] **Step 5: Confirm the installed app and production configuration are unchanged**

Compare against the before-verification evidence:

```powershell
Get-Process devmanager | Select-Object Id,StartTime,Path
Get-FileHash -Algorithm SHA256 `
  "$env:APPDATA\com.userfirst.devmanager\config.json", `
  "$env:APPDATA\com.userfirst.devmanager\remote.json"
```

Expected: PID `44880`, start time `2026-07-23 14:20:16` local time, installed
path `C:\Users\micro\AppData\Local\DevManager\devmanager.exe`, and both hashes
match the captured pre-verification values.

- [ ] **Step 6: Commit any verification-only correction**

If review or formatting changed source:

```powershell
git add src/theme/mod.rs src/sidebar/mod.rs src/app/mod.rs
git commit -m "fix: tighten external server indicator"
```

Otherwise leave the existing task commits unchanged.
