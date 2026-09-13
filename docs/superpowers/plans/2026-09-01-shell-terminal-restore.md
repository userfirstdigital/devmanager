# Shell Terminal Restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After `devmanager-host` restarts, recreate every plain shell terminal of an open task in its last known directory once a client attaches, with the same resource id, title, strip position and geometry; make boot reconcile stale terminal resources honestly.

**Architecture:** Boot loads stale Active/Releasing Terminal resources, marks them `Unknown` in the terminal service and finalizes exited or releasing ones. A kernel enumerator `restorable_shell_terminals` produces fenced intents; a restore lane beside the provider lane pumps them through one idempotent verb, `ensure_running`, once the first client output registers. Failures are per-resource `TerminalExited` facts.

**Tech Stack:** Rust 2021, rusqlite, tokio, portable-pty, existing `HostRequestExecutor` lanes.

**Spec:** `docs/superpowers/specs/2026-09-01-shell-terminal-restore-design.md`
**Depends on:** plan `2026-09-01-task-shell-terminals.md` fully landed (all its types and commands are used by name here).

## Global Constraints

- Isolated `CARGO_TARGET_DIR` under `C:\Temp\devmanager-*`; focused tests while iterating; `cargo check --locked --lib --bins --tests` before handing back.
- `ensure_running(resource_id, seed)` is the only code path that spawns a plain shell. `OpenShellTerminal` acceptance and restore both call it.
- Restore spawns at the recorded cols/rows, never a default size.
- Cwd ladder order is fixed: last reported cwd → nearest existing ancestor inside the task workspace root → task runtime working directory → project configured root.
- Shared restore budget `MAX_CONCURRENT_RESTORES = 2`; providers dequeue first. Dedupe by `resource_id`. Negative cache keyed `(resource_id, action_epoch)`.
- The lane pumps only after the first client output has registered on this host boot.
- Boot never emits `ResourceReleased` for an Active plain shell without an exit fact; it never resurrects an exited shell.
- Kill-on-close Jobs stay as they are.

## File map

- Modify `src/domain/event.rs` (`ResourceReleased` gains `#[serde(default)] reason: Option<ResourceReleaseReason>`; new `TerminalRestored`)
- Modify `src/domain/resource.rs` (`ResourceReleaseReason`, `RestoreFallback`)
- Modify `src/kernel/command_bus.rs` (`restorable_shell_terminals`, `stale_terminal_resources`, `RestoreShellIntent`)
- Modify `src/domain/command.rs` (`RecordTerminalRestored`, `FinalizeStaleResource` host-only)
- Modify `src/terminal/service.rs` (`mark_unknown`, `clear_unknown`)
- Modify `src/services/process_manager.rs` (`ensure_running` core: `spawn_task_shell_session` reuse with geometry)
- Modify `src/host/connection.rs` (boot reconciliation, restore lane, first-client trigger, `inspect_host_quit` shells)
- Modify `src/host/cockpit.rs` (readiness `Unknown` hint for shells), `src/ui/native_shell.rs` (restoring/unknown chip states, tooltip)
- Tests: `src/kernel/command_bus.rs`, `src/host/connection.rs` unit tests; `tests/shell_restore.rs` process test

---

### Task 1: Release reason and restored event

**Files:**
- Modify: `src/domain/resource.rs`, `src/domain/event.rs`
- Test: `src/domain/event.rs` serde tests

**Interfaces:**
- Produces:

```rust
pub enum ResourceReleaseReason { UserClose, TaskClose, HostRestart, RestoreFailed }
pub enum RestoreFallback { None, AncestorOfReported, TaskRuntimeDir, ProjectRoot }
Event::ResourceReleased { resource_id, runtime_generation, reason: Option<ResourceReleaseReason> }  // reason None = legacy
Event::TerminalRestored { resource_id, cwd_used: PathBuf, fallback: RestoreFallback }              // wire "terminal.restored", host fact, no revision
```

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn resource_released_without_reason_still_decodes() {
        let json = format!(
            r#"{{"schema_version":1,"event_type":"resource.released","payload":{{"resource_id":"{}","runtime_generation":3}}}}"#,
            ResourceId::new()
        );
        let event: Event = serde_json::from_str(&json).expect("legacy released decodes");
        assert!(matches!(event, Event::ResourceReleased { reason: None, .. }));
    }

    #[test]
    fn terminal_restored_round_trips_and_is_not_a_task_mutation() {
        let event = Event::TerminalRestored {
            resource_id: ResourceId::new(),
            cwd_used: std::path::PathBuf::from(r"C:\Code\demo"),
            fallback: RestoreFallback::TaskRuntimeDir,
        };
        let json = serde_json::to_string(&event).expect("json");
        let replayed: Event = serde_json::from_str(&json).expect("replay");
        assert_eq!(replayed, event);
        assert_eq!(event.event_type(), "terminal.restored");
        assert!(!event.is_task_mutation());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked domain::event -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

`resource.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceReleaseReason { UserClose, TaskClose, HostRestart, RestoreFailed }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreFallback { None, AncestorOfReported, TaskRuntimeDir, ProjectRoot }
```

`event.rs`: `ResourceReleasedPayload` gains `#[serde(default)] pub reason: Option<ResourceReleaseReason>`; the `Event::ResourceReleased` variant gains `reason: Option<ResourceReleaseReason>`; encode copies it, decode passes it through; every existing constructor site adds `reason: None` (`git grep -n "Event::ResourceReleased {" -- src tests`), and `Command::ReleaseResource`'s decide arm emits `reason: Some(ResourceReleaseReason::UserClose)`; `Command::CloseTerminal` (plan 1) emits `UserClose` on its released event path (the release completion is produced by the existing teardown completion code — find where `Event::ResourceReleased` is emitted after teardown and pass the reason recorded on the release-begun path; if none is recorded, `None`).

Add `TerminalRestoredPayload { resource_id, cwd_used: PathBuf, fallback: RestoreFallback }`, variant, `event_type` `"terminal.restored"`, `EventBody` rename, encode/decode, exclusion in `is_task_mutation`, an `apply` arm in the `ProviderInputDelivered` shape, and `apply_into`:

```rust
        Event::TerminalRestored { resource_id, cwd_used, .. } => {
            let facts = snap.terminal_facts.get_mut(resource_id).ok_or(ApplyError::NotFound)?;
            facts.live_cwd = Some(cwd_used.clone());
            facts.exit = None;
            facts.last_activity_at_ms = occurred_at_ms;
        }
```

Projector arm: `UPDATE terminal_facts SET live_cwd = ?1, exit_code = NULL, exit_summary = NULL, exited_at_ms = NULL, last_activity_at_ms = ?2 WHERE resource_id = ?3` with no revision bump.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked domain:: kernel:: -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/domain/resource.rs src/domain/event.rs src/domain/command.rs src/kernel/projector.rs $(git diff --name-only -- src tests)
git commit -m "feat(domain): add release reasons and TerminalRestored fact

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Kernel enumeration and stale-resource finalization

**Files:**
- Modify: `src/kernel/command_bus.rs` (beside `restorable_provider_starts` ~481-541), `src/domain/command.rs`
- Test: `src/kernel/command_bus.rs` tests

**Interfaces:**
- Produces:

```rust
pub struct RestoreShellIntent {
    pub task_id: TaskId,
    pub resource_id: ResourceId,
    pub launch: TerminalLaunch,
    pub last_cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub expected_task_revision: u64,
    pub expected_action_epoch: u64,
}
impl CommandBus {
    pub fn restorable_shell_terminals(&self, limit: usize) -> Result<Vec<RestoreShellIntent>, StoreError>;
    pub fn stale_terminal_resources(&self, current_boot_generation_floor: u64) -> Result<Vec<StaleTerminalResource>, StoreError>;
}
pub struct StaleTerminalResource { pub task_id: TaskId, pub resource_id: ResourceId, pub runtime_generation: u64, pub lifecycle: ResourceLifecycle, pub has_exit: bool, pub is_plain_shell: bool }
Command::FinalizeStaleResource { resource_id: ResourceId, runtime_generation: u64 }   // host-only; emits ResourceReleased { reason: HostRestart } from Active or Releasing
Command::RecordTerminalRestored { resource_id, cwd_used: PathBuf, fallback: RestoreFallback } // host-only
```

Runtime generations: plan 1 registers each shell with `runtime_generation` equal to the host boot generation counter already used for provider resources (`agent.runtime_generation`). Boot passes the previous boot's generation as the floor; every Terminal resource with `runtime_generation < floor` is stale. If the host does not yet persist a boot generation, derive the floor from `host_boot_id` ordering: add `host_boots (boot_id BLOB PRIMARY KEY, generation INTEGER NOT NULL, started_at_ms INTEGER NOT NULL)` to V16 in plan 1's migration if this plan lands in the same release, otherwise as V17 here with the same manifest discipline.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn restorable_shell_terminals_selects_open_task_plain_shells_in_strip_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&directory.path().join("tasks.sqlite")).expect("bus");
        let client_id = ClientId::new();
        let (task_id, mut revision) = create_open_task(&mut bus, client_id);
        let a = plain_shell_facts(task_id, Some("a"));
        let b = plain_shell_facts(task_id, Some("b"));
        for shell in [&a, &b] {
            let receipt = bus.execute(task_envelope(client_id, task_id, revision,
                Command::OpenShellTerminal(OpenShellTerminalIntent { resource: shell.clone() }))).expect("open");
            revision = accepted_revision(&receipt);
        }
        let receipt = bus.execute(task_envelope(client_id, task_id, revision,
            Command::SetTerminalStrip(TaskTerminalStrip { order: vec![b.id, a.id], focused: Some(b.id) }))).expect("strip");
        revision = accepted_revision(&receipt);
        host_execute(&mut bus, task_id, Command::RecordTerminalCwd { resource_id: a.id, cwd: std::path::PathBuf::from(r"C:\Code\demo\src") });
        // exited shell must be skipped
        let c = plain_shell_facts(task_id, Some("c"));
        let receipt = bus.execute(task_envelope(client_id, task_id, revision,
            Command::OpenShellTerminal(OpenShellTerminalIntent { resource: c.clone() }))).expect("open c");
        revision = accepted_revision(&receipt);
        host_execute(&mut bus, task_id, Command::RecordTerminalExit { resource_id: c.id, code: Some(0), summary: "done".into() });

        let intents = bus.restorable_shell_terminals(64).expect("enumerate");
        assert_eq!(intents.iter().map(|i| i.resource_id).collect::<Vec<_>>(), vec![b.id, a.id]);
        let a_intent = intents.iter().find(|i| i.resource_id == a.id).unwrap();
        assert_eq!(a_intent.last_cwd, Some(std::path::PathBuf::from(r"C:\Code\demo\src")));
        assert_eq!((a_intent.cols, a_intent.rows), (120, 40));
        assert_eq!(a_intent.expected_task_revision, revision);
        let snapshot = bus.task_snapshot(task_id).unwrap().unwrap();
        assert_eq!(a_intent.expected_action_epoch, snapshot.task.action_epoch);
    }

    #[test]
    fn provider_terminals_and_closed_tasks_are_not_enumerated() {
        // reuse create_open_task + register a provider Terminal resource (ResourceRecipe::terminal) and
        // a plain shell on a second task that is then archived (Command::BeginCloseTask ... existing close flow);
        // assert restorable_shell_terminals returns only the open task's plain shell.
    }

    #[test]
    fn finalize_stale_resource_releases_with_host_restart_reason() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&directory.path().join("tasks.sqlite")).expect("bus");
        let client_id = ClientId::new();
        let (task_id, revision) = create_open_task(&mut bus, client_id);
        let shell = plain_shell_facts(task_id, None);
        bus.execute(task_envelope(client_id, task_id, revision,
            Command::OpenShellTerminal(OpenShellTerminalIntent { resource: shell.clone() }))).expect("open");
        host_execute(&mut bus, task_id, Command::RecordTerminalExit { resource_id: shell.id, code: Some(0), summary: "done".into() });
        host_execute(&mut bus, task_id, Command::FinalizeStaleResource { resource_id: shell.id, runtime_generation: shell.runtime_generation });
        let snapshot = bus.task_snapshot(task_id).unwrap().unwrap();
        assert_eq!(snapshot.resources[&shell.id].lifecycle, ResourceLifecycle::Released);
        assert!(!snapshot.terminal_facts.contains_key(&shell.id));
        // idempotent
        host_execute(&mut bus, task_id, Command::FinalizeStaleResource { resource_id: shell.id, runtime_generation: shell.runtime_generation });
    }
```

Fill in the second test body fully before running (register the provider terminal exactly as `correlated_provider_binding_is_exact_write_once_and_restorable` does).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked kernel::command_bus::tests::restorable_shell -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

`RestoreShellIntent` in `src/domain/command.rs` beside `StartProviderSessionIntent` (same derives). Enumerator in `command_bus.rs`, modelled on `restorable_provider_starts`:

```rust
    pub fn restorable_shell_terminals(&self, limit: usize) -> Result<Vec<RestoreShellIntent>, StoreError> {
        let limit = limit.min(64);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.store.open_query_connection()?;
        let mut statement = conn.prepare(
            "SELECT r.task_id, r.resource_id, r.recipe, t.revision, t.action_epoch, f.live_cwd, s.order_msgpack
             FROM resources r
             JOIN tasks t ON t.task_id = r.task_id
             LEFT JOIN terminal_facts f ON f.resource_id = r.resource_id
             LEFT JOIN task_terminal_strip s ON s.task_id = r.task_id
             WHERE r.resource_kind = 'terminal'
               AND r.lifecycle = 'active'
               AND t.lifecycle = 'open'
               AND (f.exited_at_ms IS NULL)
               AND NOT EXISTS (SELECT 1 FROM agent_sessions a WHERE a.provider_resource_id = r.resource_id)
             ORDER BY t.updated_at_ms DESC, r.task_id ASC, r.resource_id ASC
             LIMIT ?1",
        )?;
        let mut rows = statement.query(rusqlite::params![i64::try_from(limit).map_err(|_| StoreError::Corruption)?])?;
        let mut by_task: Vec<(TaskId, Vec<RestoreShellIntent>, Vec<ResourceId>)> = Vec::new();
        while let Some(row) = rows.next()? {
            let task_bytes: Vec<u8> = row.get(0)?;
            let resource_bytes: Vec<u8> = row.get(1)?;
            let recipe_bytes: Vec<u8> = row.get(2)?;
            let revision = u64_from_nonnegative_i64("tasks.revision", row.get(3)?)?;
            let action_epoch = u64_from_nonnegative_i64("tasks.action_epoch", row.get(4)?)?;
            let live_cwd: Option<String> = row.get(5)?;
            let order_bytes: Option<Vec<u8>> = row.get(6)?;
            let task_id = id16::<TaskId>("tasks.task_id", &task_bytes)?;
            let resource_id = id16::<ResourceId>("resources.resource_id", &resource_bytes)?;
            let recipe: ResourceRecipe = unpack_projection_blob("resources.recipe", &recipe_bytes)?;
            let ResourceRecipe::Terminal { cols, rows, launch: Some(launch), .. } = recipe else { continue };
            let intent = RestoreShellIntent {
                task_id,
                resource_id,
                launch,
                last_cwd: live_cwd.map(std::path::PathBuf::from),
                cols,
                rows,
                expected_task_revision: revision,
                expected_action_epoch: action_epoch,
            };
            let order: Vec<ResourceId> = match order_bytes {
                Some(bytes) => unpack_projection_blob("task_terminal_strip.order_msgpack", &bytes)?,
                None => Vec::new(),
            };
            match by_task.iter_mut().find(|(id, _, _)| *id == task_id) {
                Some((_, intents, _)) => intents.push(intent),
                None => by_task.push((task_id, vec![intent], order)),
            }
        }
        let mut out = Vec::new();
        for (_, mut intents, order) in by_task {
            intents.sort_by_key(|intent| {
                order.iter().position(|id| *id == intent.resource_id).unwrap_or(usize::MAX)
            });
            out.extend(intents);
        }
        Ok(out)
    }
```

`stale_terminal_resources(floor)`:

```rust
    pub fn stale_terminal_resources(&self, floor: u64) -> Result<Vec<StaleTerminalResource>, StoreError> {
        let conn = self.store.open_query_connection()?;
        let mut statement = conn.prepare(
            "SELECT r.task_id, r.resource_id, r.runtime_generation, r.lifecycle, r.recipe, f.exited_at_ms
             FROM resources r LEFT JOIN terminal_facts f ON f.resource_id = r.resource_id
             WHERE r.resource_kind = 'terminal' AND r.lifecycle IN ('active', 'releasing') AND r.runtime_generation < ?1",
        )?;
        let floor = i64::try_from(floor).map_err(|_| StoreError::Corruption)?;
        let rows = statement.query_map(rusqlite::params![floor], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (task_bytes, resource_bytes, generation, lifecycle, recipe_bytes, exited_at) = row?;
            let recipe: ResourceRecipe = unpack_projection_blob("resources.recipe", &recipe_bytes)?;
            out.push(StaleTerminalResource {
                task_id: id16::<TaskId>("resources.task_id", &task_bytes)?,
                resource_id: id16::<ResourceId>("resources.resource_id", &resource_bytes)?,
                runtime_generation: u64_from_nonnegative_i64("resources.runtime_generation", generation)?,
                lifecycle: parse_resource_lifecycle(&lifecycle)?,
                has_exit: exited_at.is_some(),
                is_plain_shell: recipe.is_plain_shell(),
            });
        }
        Ok(out)
    }
```

`decide` arms for the host-only commands:

```rust
        Command::FinalizeStaleResource { resource_id, runtime_generation } => {
            let snap = require_open_or_closing_task(snapshot)?;
            let Some(resource) = snap.resources.get(resource_id) else { return Ok(Vec::new()); };
            if resource.lifecycle == ResourceLifecycle::Released || resource.runtime_generation != *runtime_generation {
                return Ok(Vec::new());
            }
            let mut events = Vec::new();
            if resource.lifecycle == ResourceLifecycle::Active {
                events.push(Event::ResourceReleaseBegun { resource_id: *resource_id, runtime_generation: *runtime_generation });
            }
            events.push(Event::ResourceReleased {
                resource_id: *resource_id,
                runtime_generation: *runtime_generation,
                reason: Some(ResourceReleaseReason::HostRestart),
            });
            Ok(events)
        }
        Command::RecordTerminalRestored { resource_id, cwd_used, fallback } => {
            let snap = require_open_or_closing_task(snapshot)?;
            snap.terminal_facts.get(resource_id).ok_or(RejectionCode::NotFound)?;
            Ok(vec![Event::TerminalRestored { resource_id: *resource_id, cwd_used: cwd_used.clone(), fallback: *fallback }])
        }
```

Both are added to the client-path `HostAuthorityRequired` list in `CommandBus::execute`. Check that `apply_into` accepts `ResourceReleaseBegun` followed by `ResourceReleased` in one decision batch (the existing release path does the same two-step; if the batch applier requires separate envelopes, emit only `ResourceReleased` when lifecycle is `Releasing` and issue two host commands from the boot code instead).

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked kernel::command_bus::tests -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/kernel/command_bus.rs src/domain/command.rs
git commit -m "feat(kernel): enumerate restorable shells and finalize stale terminal resources

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: `ensure_running` and the cwd ladder

**Files:**
- Modify: `src/host/connection.rs` (new `ShellEnsureOutcome`, `ensure_running`), `src/terminal/service.rs` (`mark_unknown`, `clear_unknown`)
- Create: `src/host/shell_restore.rs` (pure cwd ladder + intent bookkeeping, unit-testable without processes)
- Test: `src/host/shell_restore.rs` tests

**Interfaces:**
- Produces in `src/host/shell_restore.rs`:

```rust
pub struct CwdLadderInput<'a> { pub last_reported: Option<&'a Path>, pub workspace_root: &'a Path, pub task_runtime_dir: &'a Path, pub project_root: &'a Path }
pub fn choose_restore_cwd(input: CwdLadderInput<'_>, exists: &dyn Fn(&Path) -> bool) -> Option<(PathBuf, RestoreFallback)>;
pub struct ShellRestoreLane { queue: VecDeque<RestoreShellIntent>, in_flight: HashSet<ResourceId>, failed: HashMap<(ResourceId, u64), ()>, pending_enumeration: bool, first_client_seen: bool }
impl ShellRestoreLane {
    pub fn new() -> Self;
    pub fn note_first_client(&mut self);
    pub fn push_unique(&mut self, intent: RestoreShellIntent) -> bool;
    pub fn pop_ready(&mut self, provider_in_flight: usize, max_concurrent: usize) -> Option<RestoreShellIntent>;
    pub fn mark_failed(&mut self, resource_id: ResourceId, action_epoch: u64);
    pub fn finish(&mut self, resource_id: ResourceId);
    pub fn is_failed(&self, resource_id: ResourceId, action_epoch: u64) -> bool;
}
```
- Produces in `HostRequestExecutor`:

```rust
fn ensure_running(&mut self, resource_id: ResourceId, seed: Option<Vec<u8>>) -> Result<(), String>;   // seed used by plan 3
```
- `TerminalService::mark_unknown(owner: TaskId, resource_id: ResourceId, resource_generation: u64) -> Result<(), TerminalError>` inserts a closed-less placeholder `HostedTerminal` with `unknown = true` and a fixture projection so `task_terminal_summaries` reports `Unknown`; `clear_unknown(resource_id)` removes the placeholder before `attach_plain_shell`.

- [ ] **Step 1: Write the failing tests**

`src/host/shell_restore.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn exists_in(set: &[&str]) -> impl Fn(&Path) -> bool + '_ {
        move |p| set.iter().any(|s| Path::new(s) == p)
    }

    #[test]
    fn ladder_prefers_last_reported_when_it_exists() {
        let out = choose_restore_cwd(
            CwdLadderInput {
                last_reported: Some(Path::new(r"C:\Code\demo\src")),
                workspace_root: Path::new(r"C:\Code\demo"),
                task_runtime_dir: Path::new(r"C:\Code\demo"),
                project_root: Path::new(r"C:\Code"),
            },
            &exists_in(&[r"C:\Code\demo\src", r"C:\Code\demo", r"C:\Code"]),
        );
        assert_eq!(out, Some((PathBuf::from(r"C:\Code\demo\src"), RestoreFallback::None)));
    }

    #[test]
    fn ladder_walks_up_within_workspace_then_runtime_then_project() {
        let input = |reported: Option<&'static str>| CwdLadderInput {
            last_reported: reported.map(Path::new),
            workspace_root: Path::new(r"C:\Code\demo"),
            task_runtime_dir: Path::new(r"C:\Code\demo\.worktrees\t1"),
            project_root: Path::new(r"C:\Code\demo"),
        };
        // deleted leaf, existing parent inside workspace
        assert_eq!(
            choose_restore_cwd(input(Some(r"C:\Code\demo\src\gone")), &exists_in(&[r"C:\Code\demo\src", r"C:\Code\demo"])),
            Some((PathBuf::from(r"C:\Code\demo\src"), RestoreFallback::AncestorOfReported))
        );
        // reported path outside the workspace root: skip to runtime dir
        assert_eq!(
            choose_restore_cwd(input(Some(r"D:\elsewhere\x")), &exists_in(&[r"D:\elsewhere", r"C:\Code\demo\.worktrees\t1", r"C:\Code\demo"])),
            Some((PathBuf::from(r"C:\Code\demo\.worktrees\t1"), RestoreFallback::TaskRuntimeDir))
        );
        // runtime dir gone too
        assert_eq!(
            choose_restore_cwd(input(None), &exists_in(&[r"C:\Code\demo"])),
            Some((PathBuf::from(r"C:\Code\demo"), RestoreFallback::ProjectRoot))
        );
        assert_eq!(choose_restore_cwd(input(None), &exists_in(&[])), None);
    }

    #[test]
    fn lane_dedupes_gates_on_first_client_and_shares_budget() {
        let mut lane = ShellRestoreLane::new();
        let intent = |id: ResourceId| RestoreShellIntent {
            task_id: TaskId::new(), resource_id: id,
            launch: TerminalLaunch { cwd: PathBuf::from(r"C:\"), program: PathBuf::from("cmd.exe"), args: vec![] },
            last_cwd: None, cols: 80, rows: 24, expected_task_revision: 1, expected_action_epoch: 1,
        };
        let a = ResourceId::new();
        assert!(lane.push_unique(intent(a)));
        assert!(!lane.push_unique(intent(a)));
        assert!(lane.pop_ready(0, 2).is_none(), "no client yet");
        lane.note_first_client();
        assert!(lane.pop_ready(2, 2).is_none(), "budget consumed by providers");
        let popped = lane.pop_ready(1, 2).expect("one slot left");
        assert_eq!(popped.resource_id, a);
        assert!(!lane.push_unique(intent(a)), "in flight blocks re-queue");
        lane.mark_failed(a, 1);
        lane.finish(a);
        assert!(lane.is_failed(a, 1));
        assert!(!lane.is_failed(a, 2));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked host::shell_restore -- --test-threads=1`
Expected: compile error (module missing). Add `pub(crate) mod shell_restore;` to `src/host/mod.rs`.

- [ ] **Step 3: Implement**

```rust
//! Pure shell-restore bookkeeping: cwd ladder and lane state. No processes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::domain::command::RestoreShellIntent;
use crate::domain::resource::RestoreFallback;
use crate::domain::ResourceId;

pub struct CwdLadderInput<'a> {
    pub last_reported: Option<&'a Path>,
    pub workspace_root: &'a Path,
    pub task_runtime_dir: &'a Path,
    pub project_root: &'a Path,
}

pub fn choose_restore_cwd(
    input: CwdLadderInput<'_>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<(PathBuf, RestoreFallback)> {
    if let Some(reported) = input.last_reported {
        if reported.is_absolute() && exists(reported) {
            return Some((reported.to_path_buf(), RestoreFallback::None));
        }
        if reported.starts_with(input.workspace_root) {
            let mut cursor = reported.parent();
            while let Some(dir) = cursor {
                if !dir.starts_with(input.workspace_root) {
                    break;
                }
                if exists(dir) {
                    return Some((dir.to_path_buf(), RestoreFallback::AncestorOfReported));
                }
                cursor = dir.parent();
            }
        }
    }
    if exists(input.task_runtime_dir) {
        return Some((input.task_runtime_dir.to_path_buf(), RestoreFallback::TaskRuntimeDir));
    }
    if exists(input.project_root) {
        return Some((input.project_root.to_path_buf(), RestoreFallback::ProjectRoot));
    }
    None
}

#[derive(Default)]
pub struct ShellRestoreLane {
    queue: VecDeque<RestoreShellIntent>,
    in_flight: HashSet<ResourceId>,
    failed: HashMap<(ResourceId, u64), ()>,
    pub pending_enumeration: bool,
    first_client_seen: bool,
}

impl ShellRestoreLane {
    pub fn new() -> Self {
        Self { pending_enumeration: true, ..Default::default() }
    }
    pub fn note_first_client(&mut self) {
        self.first_client_seen = true;
    }
    pub fn first_client_seen(&self) -> bool {
        self.first_client_seen
    }
    pub fn push_unique(&mut self, intent: RestoreShellIntent) -> bool {
        if self.in_flight.contains(&intent.resource_id)
            || self.queue.iter().any(|q| q.resource_id == intent.resource_id)
            || self.is_failed(intent.resource_id, intent.expected_action_epoch)
        {
            return false;
        }
        self.queue.push_back(intent);
        true
    }
    pub fn pop_ready(&mut self, provider_in_flight: usize, max_concurrent: usize) -> Option<RestoreShellIntent> {
        if !self.first_client_seen || provider_in_flight + self.in_flight.len() >= max_concurrent {
            return None;
        }
        let intent = self.queue.pop_front()?;
        self.in_flight.insert(intent.resource_id);
        Some(intent)
    }
    pub fn mark_failed(&mut self, resource_id: ResourceId, action_epoch: u64) {
        self.failed.insert((resource_id, action_epoch), ());
    }
    pub fn finish(&mut self, resource_id: ResourceId) {
        self.in_flight.remove(&resource_id);
    }
    pub fn is_failed(&self, resource_id: ResourceId, action_epoch: u64) -> bool {
        self.failed.contains_key(&(resource_id, action_epoch))
    }
}
```

`TerminalService` additions:

```rust
    pub fn mark_unknown(&self, owner: TaskId, resource_id: ResourceId, resource_generation: u64) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        if terminals.values().any(|t| t.resource_id == resource_id && !t.closed) {
            return Ok(());
        }
        let spec = TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24)?)?;
        let terminal_id = TerminalId::new();
        let mut hosted = HostedTerminal::open_fixture(owner, spec, terminal_id)?; // the existing fixture constructor used by tests; expose it pub(crate) if private
        hosted.resource_id = resource_id;
        hosted.generation = TerminalGeneration::from_raw(resource_generation)?;
        hosted.unknown = true;
        terminals.insert(terminal_id, hosted);
        Ok(())
    }

    pub fn clear_unknown(&self, resource_id: ResourceId) -> Result<(), TerminalError> {
        let mut terminals = self.lock()?;
        terminals.retain(|_, t| !(t.resource_id == resource_id && t.unknown));
        Ok(())
    }
```

`ensure_running` in `src/host/connection.rs`:

```rust
    fn ensure_running(&mut self, resource_id: ResourceId, seed: Option<Vec<u8>>) -> Result<(), String> {
        if self.shell_sessions.contains_key(&resource_id) {
            return Ok(());
        }
        let Some(runtime) = self.configured_service_runtime.as_ref().map(|r| r.manager.clone()) else {
            return Err("configured runtime unavailable".to_string());
        };
        let (task_id, resource, facts) = {
            let (task_id, resource) = self.bus.load_resource_with_task(resource_id).map_err(|e| e.to_string())?
                .ok_or_else(|| "terminal resource not found".to_string())?;   // add this small kernel accessor beside load_resource: returns (TaskId, ResourceFacts)
            let snapshot = self.bus.task_snapshot(task_id).map_err(|e| e.to_string())?
                .ok_or_else(|| "task missing".to_string())?;
            let facts = snapshot.terminal_facts.get(&resource_id).cloned();
            (task_id, resource, facts)
        };
        let ResourceRecipe::Terminal { cols, rows, launch: Some(launch), .. } = resource.recipe.clone() else {
            return Err("not a plain shell".to_string());
        };
        if resource.lifecycle != ResourceLifecycle::Active {
            return Err("resource is not active".to_string());
        }
        let loaded = self.bus.load_task_runtime(task_id, &self.workspace_projects).map_err(|e| e.to_string())?
            .ok_or_else(|| "task runtime missing".to_string())?;
        let runtime_dir = loaded.workspace.runtime_working_directory().map_err(|e| e.to_string())?;
        let workspace_root = loaded.workspace.workspace_root().to_path_buf();      // existing accessor; if named differently use that
        let project_root = loaded.workspace.canonical_configured_project_root().map_err(|e| e.to_string())?;
        let (cwd, fallback) = super::shell_restore::choose_restore_cwd(
            super::shell_restore::CwdLadderInput {
                last_reported: facts.as_ref().and_then(|f| f.live_cwd.as_deref()),
                workspace_root: &workspace_root,
                task_runtime_dir: &runtime_dir,
                project_root: &project_root,
            },
            &|p| p.is_dir(),
        )
        .ok_or_else(|| "no usable directory".to_string())?;
        let launch = TerminalLaunch { cwd: cwd.clone(), program: launch.program, args: launch.args };
        let dims = SessionDimensions { cols, rows, cell_width: 8, cell_height: 16 };
        let snapshot = self.bus.task_snapshot(task_id).map_err(|e| e.to_string())?.ok_or("task missing")?;
        let session_id = runtime.spawn_task_shell_session(task_id, resource_id, resource.runtime_generation, snapshot.task.action_epoch, &launch, dims)?;
        let attached = runtime.task_shell_runtime(session_id)?;
        if let Some(seed) = seed {
            attached.seed_history_before_prompt(&seed);   // plan 3 adds this; until then the argument is always None
        }
        self.terminal_service.clear_unknown(resource_id).map_err(|e| e.to_string())?;
        let spec = TerminalSpec::new(session_id, TerminalSize::new(cols, rows).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        self.terminal_service
            .attach_plain_shell(task_id, resource_id, resource.runtime_generation, spec, attached)
            .map_err(|e| e.to_string())?;
        self.shell_sessions.insert(resource_id, ShellSessionLink { task_id, session_id, last_cwd: Some(cwd.clone()), last_cwd_change: None, exit_recorded: false });
        let _ = self.execute_host_fact(task_id, Command::RecordTerminalRestored { resource_id, cwd_used: cwd, fallback });
        Ok(())
    }
```

Change plan 1's `open_shell_terminal_after_accept` to call `self.ensure_running(resource_id, None)` and record a `TerminalExited { summary: format!("spawn failed: {error}") }` on `Err`. Seeding order for plan 3: `spawn_task_shell_session` must expose a pre-spawn hook; plan 3 changes the signature to accept `seed: Option<&[u8]>` and feeds the grid before the PTY child is created. Here the call passes `None`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked host::shell_restore terminal:: -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/host/shell_restore.rs src/host/mod.rs src/host/connection.rs src/terminal/service.rs src/kernel/command_bus.rs
git commit -m "feat(host): add ensure_running with the cwd ladder and shell restore lane state

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: Boot reconciliation and the restore lane pump

**Files:**
- Modify: `src/host/connection.rs` (constructors ~2909-3045, reaper ticks ~3135-3230, `handle_control` RegisterOutput ~3310-3336), `src/bin/devmanager-host.rs` (boot generation floor)
- Test: `src/host/connection.rs` unit tests with an in-process bus

**Interfaces:**
- `HostRequestExecutor` gains `shell_restore: ShellRestoreLane`, `shell_restore_jobs: FuturesUnordered<ShellRestoreFuture>`, `boot_generation: u64`.
- `fn reconcile_stale_terminals_at_boot(&mut self)` runs once before the request loop: for each `stale_terminal_resources(boot_generation)` row: if `has_exit || lifecycle == Releasing || !is_plain_shell` → `FinalizeStaleResource`; else `terminal_service.mark_unknown(task_id, resource_id, runtime_generation)`.
- `fn queue_one_shell_restore(&mut self)` called after `queue_one_provider_restore()` in both reaper arms; first-client detection in `handle_control` `RegisterOutput` when `self.outputs.is_empty()` → `self.shell_restore.note_first_client()`.
- Readiness: while `shell_restore.pending_enumeration` is true the cockpit readiness for shells reports `Unknown` (already how `TerminalRuntimeState::Unknown` surfaces through `task_terminal_summaries`).

- [ ] **Step 1: Write the failing test**

Using the existing in-process executor test harness in `src/host/connection.rs` tests (the one that builds a `HostRequestExecutor` over a temp bus without spawning real processes; grep `fn executor_for_test`), add:

```rust
    #[test]
    fn boot_marks_live_shells_unknown_and_finalizes_exited_ones() {
        let mut harness = executor_for_test();
        let (task_id, revision) = harness.create_open_task();
        let live = harness.open_plain_shell(task_id, revision);       // helper wrapping Command::OpenShellTerminal with runtime_generation = 1
        let exited = harness.open_plain_shell(task_id, revision + 1);
        harness.record_exit(exited, Some(0));
        harness.executor.boot_generation = 2;                         // previous boot wrote generation 1
        harness.executor.reconcile_stale_terminals_at_boot();
        let summaries = harness.executor.terminal_service.task_terminal_summaries(task_id).unwrap();
        assert!(summaries.iter().any(|s| s.resource_id == live && s.state == TerminalRuntimeState::Unknown));
        assert!(!summaries.iter().any(|s| s.resource_id == exited));
        let snapshot = harness.executor.bus.task_snapshot(task_id).unwrap().unwrap();
        assert_eq!(snapshot.resources[&exited].lifecycle, ResourceLifecycle::Released);
        assert_eq!(snapshot.resources[&live].lifecycle, ResourceLifecycle::Active);
    }

    #[test]
    fn shell_restore_lane_waits_for_first_client_then_dedupes() {
        let mut harness = executor_for_test();
        let (task_id, revision) = harness.create_open_task();
        let shell = harness.open_plain_shell(task_id, revision);
        harness.executor.boot_generation = 2;
        harness.executor.reconcile_stale_terminals_at_boot();
        harness.executor.queue_one_shell_restore();
        assert!(!harness.executor.shell_restore.pending_enumeration, "enumeration ran");
        assert_eq!(harness.executor.shell_restore_jobs.len(), 0, "no client, no spawn");
        harness.register_fake_output();                                // drives handle_control RegisterOutput
        harness.executor.queue_one_shell_restore();
        assert_eq!(harness.executor.shell_restore_jobs.len(), 1);
        harness.executor.queue_one_shell_restore();
        assert_eq!(harness.executor.shell_restore_jobs.len(), 1, "same resource not queued twice");
        let _ = shell;
    }
```

If the harness lacks `open_plain_shell` / `record_exit` / `register_fake_output`, add them in the test module using `execute_host_authorized` and the existing `ExecutorControl::RegisterOutput` fixture used by the slow-reader tests.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib --locked host::connection::tests::boot_marks -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

Struct fields and constructors:

```rust
    shell_restore: super::shell_restore::ShellRestoreLane,
    shell_restore_jobs: FuturesUnordered<ShellRestoreFuture>,
    boot_generation: u64,
```

```rust
struct ShellRestoreOutcome { task_id: TaskId, resource_id: ResourceId, action_epoch: u64, result: Result<(), String> }
type ShellRestoreFuture = Pin<Box<dyn Future<Output = ShellRestoreOutcome> + Send + 'static>>;
```

Because `ensure_running` needs `&mut self` and touches the terminal service synchronously, run it inline rather than as a boxed future: `queue_one_shell_restore` pops one intent, checks the fence (`task_snapshot(...).task.revision == expected_task_revision && action_epoch == expected_action_epoch`, else drop silently and mark for re-enumeration), calls `self.ensure_running(intent.resource_id, None)`, and handles the outcome immediately:

```rust
    fn queue_one_shell_restore(&mut self) {
        if self.shell_restore.pending_enumeration {
            match self.bus.restorable_shell_terminals(64) {
                Ok(intents) => {
                    self.shell_restore.pending_enumeration = false;
                    for intent in intents {
                        self.shell_restore.push_unique(intent);
                    }
                }
                Err(error) => {
                    eprintln!("devmanager-host: shell restore enumeration failed: {error}");
                    return;
                }
            }
        }
        let provider_in_flight = self.provider_restore_in_flight.len();
        let Some(intent) = self.shell_restore.pop_ready(provider_in_flight, MAX_CONCURRENT_PROVIDER_RESTORES) else {
            return;
        };
        let fence_ok = matches!(
            self.bus.task_snapshot(intent.task_id),
            Ok(Some(snapshot))
                if snapshot.task.revision == intent.expected_task_revision
                    && snapshot.task.action_epoch == intent.expected_action_epoch
        );
        if !fence_ok {
            self.shell_restore.finish(intent.resource_id);
            self.shell_restore.pending_enumeration = true;
            return;
        }
        let outcome = self.ensure_running(intent.resource_id, None);
        self.shell_restore.finish(intent.resource_id);
        if let Err(error) = outcome {
            self.shell_restore.mark_failed(intent.resource_id, intent.expected_action_epoch);
            let _ = self.execute_host_fact(
                intent.task_id,
                Command::RecordTerminalExit { resource_id: intent.resource_id, code: None, summary: format!("restore failed: {error}") },
            );
        }
    }
```

(Keep `shell_restore_jobs` only if `ensure_running` later moves the blocking spawn into `spawn_blocking`; if it stays inline, drop the field and the test assertions about `shell_restore_jobs.len()` become assertions on `shell_sessions.len()`.)

```rust
    fn reconcile_stale_terminals_at_boot(&mut self) {
        let stale = match self.bus.stale_terminal_resources(self.boot_generation) {
            Ok(rows) => rows,
            Err(error) => { eprintln!("devmanager-host: stale terminal scan failed: {error}"); return; }
        };
        for row in stale {
            if row.has_exit || row.lifecycle == ResourceLifecycle::Releasing || !row.is_plain_shell {
                let _ = self.execute_host_fact(row.task_id, Command::FinalizeStaleResource { resource_id: row.resource_id, runtime_generation: row.runtime_generation });
                continue;
            }
            if let Err(error) = self.terminal_service.mark_unknown(row.task_id, row.resource_id, row.runtime_generation) {
                eprintln!("devmanager-host: could not mark terminal {} unknown: {error}", row.resource_id);
            }
        }
    }
```

Call `reconcile_stale_terminals_at_boot()` once in `serve_foreground_host` right after the executor is constructed and before `HelloListener::bind`. Boot generation: the executor reads `bus.next_boot_generation()` (a small kernel method that inserts into `host_boots` and returns the new generation; add the table in V16 per Task 2's note) and uses it both as the stale floor and as the `runtime_generation` for new shell resources registered this boot (plan 1's `OpenShellTerminal` host handler sets `resource.runtime_generation = self.boot_generation`).

In `handle_control` for `ExecutorControl::RegisterOutput`, before inserting into `self.outputs`: `if self.outputs.is_empty() { self.shell_restore.note_first_client(); }`.

Add `self.queue_one_shell_restore();` after `self.queue_one_provider_restore();` in both reaper arms.

`inspect_host_quit`: add `shells: Vec<HostQuitShell { resource_id, task_id, title, label }>` built from `shell_sessions` joined with `task_snapshot(...).terminal_facts` titles; include it in the inspection wire struct with `#[serde(default)]` and render it in the client's quit summary next to agents.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --lib --locked host::connection::tests -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/host/connection.rs src/bin/devmanager-host.rs src/kernel/command_bus.rs src/kernel/schema.rs src/ui/native_shell.rs
git commit -m "feat(host): reconcile stale terminals at boot and restore shells after first client attach

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: Chip states, tooltips, Restart action

**Files:**
- Modify: `src/ui/native_shell.rs` (chip rendering from plan 1 Task 11), `src/domain/cockpit.rs` (`TaskTerminalChip` gains `restored: Option<TerminalRestoredInfo { at_ms, cwd_used: String, fallback: RestoreFallback }>`), `src/host/cockpit.rs` (fill it from `terminal_facts` + the last `TerminalRestored` fact: store `restored_at_ms`, `restore_fallback` columns on `terminal_facts` in the same V16/V17 migration and update them in the `TerminalRestored` projector arm)
- Test: `src/ui/native_shell.rs` `terminal_chip_rows` tests

**Interfaces:**
- `TerminalChipRow` gains `pub restoring: bool` (true when `state == Unknown` and the restore lane has not failed it) and `pub tooltip: String`.
- `ActionRequest::TerminalRestart { task_id, resource_id }` → `NativeHostCommand::EnsureShellRunning { request_id, task_id, resource_id }` → executor calls `ensure_running(resource_id, None)`; on error the chip shows the summary.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn chip_rows_expose_restoring_and_restored_tooltips() {
        let task_id = TaskId::new();
        let shell = ResourceId::new();
        let mut chip = TaskTerminalChip {
            resource_id: shell, is_provider: false, title: None, label: "pwsh".into(),
            runtime_state: TerminalRuntimeStateWire::Unknown, live_cwd: None, exit: None,
            created_at_ms: 1, last_activity_at_ms: 1, restored: None,
        };
        let strip = TaskTerminalsProjection { task_id, terminals: vec![chip.clone()], order: vec![shell], focused: Some(shell) };
        let rows = terminal_chip_rows(&strip);
        assert!(rows[0].restoring);
        chip.runtime_state = TerminalRuntimeStateWire::Running;
        chip.restored = Some(TerminalRestoredInfo { at_ms: 1_725_000_000_000, cwd_used: r"C:\Code\demo".into(), fallback: RestoreFallback::TaskRuntimeDir });
        let strip = TaskTerminalsProjection { task_id, terminals: vec![chip], order: vec![shell], focused: Some(shell) };
        let rows = terminal_chip_rows(&strip);
        assert!(!rows[0].restoring);
        assert!(rows[0].tooltip.contains("restored"));
        assert!(rows[0].tooltip.contains(r"C:\Code\demo"));
        assert!(rows[0].tooltip.contains("task directory"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked ui::native_shell::tests::chip_rows_expose -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
fn restore_tooltip(info: &TerminalRestoredInfo) -> String {
    let when = format_local_time_ms(info.at_ms);   // existing formatter used for task timestamps in native_shell.rs
    match info.fallback {
        RestoreFallback::None => format!("restored {when}"),
        RestoreFallback::AncestorOfReported => format!("restored {when} · opened in {} because the previous directory is gone", info.cwd_used),
        RestoreFallback::TaskRuntimeDir => format!("restored {when} · opened in the task directory {}", info.cwd_used),
        RestoreFallback::ProjectRoot => format!("restored {when} · opened in the project root {}", info.cwd_used),
    }
}
```

In `terminal_chip_rows`: `restoring: matches!(chip.runtime_state, TerminalRuntimeStateWire::Unknown) && chip.exit.is_none()`, `tooltip: chip.restored.as_ref().map(restore_tooltip).unwrap_or_default()`. Render: restoring chips show a dim "…" suffix; exited chips add a Restart row to the chip menu dispatching `ActionRequest::TerminalRestart`. Tooltip via the repo's existing hover-tooltip idiom (`.tooltip(...)` if present, else the `meta_row` popover used for task status).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked ui::native_shell::tests -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/ui/native_shell.rs src/domain/cockpit.rs src/host/cockpit.rs src/kernel/projector.rs src/kernel/schema.rs src/client/action.rs
git commit -m "feat(ui): restoring and restored chip states with a Restart action

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: Process test, sabotage, docs

**Files:**
- Create: `tests/shell_restore.rs`
- Modify: `docs/architecture.md`

- [ ] **Step 1: Write the process test**

Model on `tests/host_lifecycle.rs` (`host_command`, `host_exe`, `connect_bounded`, isolated profile/config base):

```rust
#![cfg(windows)]
// 1. start host A with --parent-pid, connect a client, create a task, TerminalOpenShell via the action path,
//    wait until TaskTerminals reports Running, send "cd <tempdir>\sub\r\n" and wait for live_cwd == sub.
// 2. kill host A (child.kill()), wait for exit.
// 3. start host B on the same profile; connect a client; assert TaskTerminals shows the same resource id
//    first as Unknown (before subscribing) then Running after the first subscription; assert live_cwd == sub,
//    same title, same strip order; assert the shell answers `echo restored` in its grid.
```

Write the three phases fully with the helpers the lifecycle test exposes. Keep timeouts bounded (10 s per wait) and assert with messages naming the observed state.

- [ ] **Step 2: Run it**

Run: `cargo build --locked --bin devmanager-process-test-helper` then `cargo test --locked --test shell_restore -- --nocapture`
Expected: PASS. Note: `tests/host_lifecycle.rs` was red on this tree for a pre-existing `Unavailable` at the first command (2026-09-01); if the same failure reproduces here, stop and report it rather than working around it.

- [ ] **Step 3: Sabotage checks**

Remove the `first_client_seen` gate in `pop_ready` → `lane_dedupes_gates_on_first_client_and_shares_budget` must fail. Remove the `exited_at_ms IS NULL` predicate in `restorable_shell_terminals` → the exited-shell assertion in Task 2's test must fail. Revert both.

- [ ] **Step 4: Docs**

Append to `docs/architecture.md` "Host lifetime and terminal survivability":

```markdown
After a host restart, plain shells of open tasks are restored once the first client attaches: boot marks their resources unknown, finalizes exited or releasing ones with a `host_restart` release reason, and `ensure_running` respawns each shell at its recorded geometry in the best directory of the ladder (last reported cwd, nearest existing ancestor inside the workspace, task runtime directory, project root), recording a `terminal.restored` fact that names the fallback used.
```

- [ ] **Step 5: Final gates and commit**

```powershell
cargo check --locked --lib --bins --tests
cargo test --lib --locked -- --test-threads=1 domain:: kernel:: host::shell_restore host::connection terminal::
cargo test --locked --test terminal_service --test task_shell_terminals --test shell_restore --test host_recovery
```

```bash
git add tests/shell_restore.rs docs/architecture.md
git commit -m "test(host): shell restore across a host restart; document the restore path

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Self-review notes

- Spec §3 boot reconciliation → Task 4 (`reconcile_stale_terminals_at_boot`) with Task 2's `stale_terminal_resources` and `FinalizeStaleResource`. §4 enumeration → Task 2. §5 `ensure_running` and ladder → Task 3. §6 lane → Tasks 3 (state) and 4 (pump, first-client trigger, shared budget). §7 quit inspection → Task 4. §8 UX → Task 5. §9 errors → per-resource `TerminalExited` in Task 4, enumeration retry via `pending_enumeration`. §10 tests → each task plus Task 6.
- Open implementation choice recorded: inline `ensure_running` on the executor thread versus a `spawn_blocking` future; the plan prefers inline and says how the test assertions change if a future is used.
- Boot generation persistence (`host_boots`) is required by Tasks 2 and 4; it is called out in Task 2 and must land in V16 or V17 before Task 4.
