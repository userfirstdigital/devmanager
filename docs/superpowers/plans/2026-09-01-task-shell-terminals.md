# Task Shell Terminals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a task own one or more plain shell terminals beside its provider terminal, recorded durably (title, launch cwd/program/args, geometry, live cwd, exit state, timestamps, strip order and focus) and shown as a chip strip in the cockpit Terminal tab.

**Architecture:** Extend `ResourceRecipe::Terminal` with an optional launch recipe; add terminal fact events and a per-task strip event to the event-sourced kernel with a V16 projection migration; key `TerminalService` by `ResourceId`; add `resource_id` to the cockpit terminal queries with the provider terminal as the default; spawn shells through the existing suspended-in-Job launcher; render a chip strip above the terminal grid.

**Tech Stack:** Rust 2021, rusqlite (kernel), serde/rmp_serde (wire), portable-pty + alacritty_terminal (PTY/grid), GPUI (client), tokio (host).

**Spec:** `docs/superpowers/specs/2026-09-01-task-shell-terminals-design.md`

## Global Constraints

- All Cargo commands use an isolated target: `$env:CARGO_TARGET_DIR = "C:\Temp\devmanager-<worktree-slug>"` (AGENTS.md). Never share the daily `target-live-dev`.
- Run focused tests while iterating; one `cargo check --locked --lib --bins --tests` plus the listed focused suites before handing back. Full `cargo test --lib -- --test-threads=1` is the final gate only.
- `ResourceId` is the only terminal identity. Cwd, PID, title and timestamps are recorded facts, never identity.
- Provider terminal fences (agent session id, runtime generation, action epoch) are unchanged. `resource_id: None` on an existing terminal query means the provider terminal.
- At most 8 plain shells per task. Title 1..=64 chars after trim. Live cwd debounce 2 s. Activity coalescing 30 s.
- New migration is V16; migrations are immutable compiled constants with sha256 manifest entries; never `CREATE TABLE IF NOT EXISTS`.
- Every new `Event` variant needs: payload struct, `EventBody` variant with wire rename, `event_type()` arm, encode arm, decode arm, `apply_into` arm, projector arm.
- Files are LF; commit with the repository `.gitattributes` in force. Each task ends in a commit with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.

## File map

- Modify `src/domain/resource.rs` (recipe + `TerminalLaunch`)
- Create `src/domain/terminal_facts.rs` (`TerminalFacts`, `TaskTerminalStrip`, validation)
- Modify `src/domain/snapshot.rs` (`terminal_facts`, `terminal_strip` on `TaskSnapshot`)
- Modify `src/domain/event.rs` (5 events)
- Modify `src/domain/command.rs` (4 client commands, 3 host-only fact commands)
- Modify `src/kernel/schema.rs` (V16), `src/kernel/projector.rs`, `src/kernel/store.rs` (snapshot loader)
- Modify `src/terminal/service.rs` (resource keying, runtime state, plain-shell attach)
- Modify `src/services/process_manager.rs` (`spawn_task_shell_session`)
- Modify `src/host/connection.rs` (OpenShellTerminal execution, cwd/exit/activity fact pump)
- Modify `src/domain/cockpit.rs`, `src/host/cockpit.rs` (`resource_id`, `TaskTerminals`)
- Modify `src/client/action.rs` (actions + factories)
- Modify `src/ui/task_workspace/surfaces.rs`, `src/ui/task_cockpit/dock.rs`, `src/ui/native_shell.rs`, `src/ui/actions.rs` (strip, chips, keys)
- Tests: unit tests beside each file; `tests/terminal_service.rs`; new `tests/task_shell_terminals.rs`

---

### Task 1: Launch recipe and title on the Terminal resource

**Files:**
- Modify: `src/domain/resource.rs:61-144`
- Test: `src/domain/resource.rs` (tests module at file end)

**Interfaces:**
- Produces: `pub struct TerminalLaunch { pub cwd: PathBuf, pub program: PathBuf, pub args: Vec<String> }`; `ResourceRecipe::Terminal { cols, rows, launch: Option<TerminalLaunch>, title: Option<String> }`; `ResourceRecipe::terminal(cols, rows)` constructor for the provider shape; `ResourceRecipe::is_plain_shell(&self) -> bool`; `ResourceValidationError::{InvalidTerminalLaunch, InvalidTerminalTitle}`.

- [ ] **Step 1: Write the failing tests**

Append to the tests module in `src/domain/resource.rs`:

```rust
    #[test]
    fn legacy_terminal_recipe_decodes_with_no_launch_or_title() {
        // msgpack of the pre-V16 shape: {"terminal": {"cols": 120, "rows": 40}}
        let legacy = rmp_serde::to_vec(&serde_json::json!({
            "terminal": { "cols": 120, "rows": 40 }
        }))
        .expect("legacy encode");
        let decoded: ResourceRecipe = rmp_serde::from_slice(&legacy).expect("legacy decode");
        assert_eq!(decoded, ResourceRecipe::terminal(120, 40));
        assert!(!decoded.is_plain_shell());
    }

    #[test]
    fn plain_shell_recipe_round_trips_and_is_detected() {
        let recipe = ResourceRecipe::Terminal {
            cols: 100,
            rows: 30,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from(r"C:\Code\demo"),
                program: std::path::PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
                args: vec!["-NoLogo".to_string()],
            }),
            title: Some("build".to_string()),
        };
        let bytes = rmp_serde::to_vec(&recipe).expect("encode");
        let decoded: ResourceRecipe = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, recipe);
        assert!(decoded.is_plain_shell());
    }

    #[test]
    fn provider_terminal_encoding_is_byte_stable_without_new_fields() {
        let before = rmp_serde::to_vec(&serde_json::json!({
            "terminal": { "cols": 120, "rows": 40 }
        }))
        .expect("legacy encode");
        let now = rmp_serde::to_vec(&ResourceRecipe::terminal(120, 40)).expect("encode");
        assert_eq!(before, now, "absent launch/title must not change the encoding");
    }

    #[test]
    fn terminal_launch_rejects_relative_cwd_and_empty_program() {
        let relative = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from("relative"),
                program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                args: Vec::new(),
            }),
            title: None,
        };
        assert_eq!(
            relative.validate(),
            Err(ResourceValidationError::InvalidTerminalLaunch)
        );
        let empty_program = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from(r"C:\Code"),
                program: std::path::PathBuf::new(),
                args: Vec::new(),
            }),
            title: None,
        };
        assert_eq!(
            empty_program.validate(),
            Err(ResourceValidationError::InvalidTerminalLaunch)
        );
    }

    #[test]
    fn terminal_title_is_trimmed_and_bounded() {
        let recipe = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: None,
            title: Some("  build  ".to_string()),
        }
        .canonicalize()
        .expect("canonical");
        assert_eq!(
            recipe,
            ResourceRecipe::Terminal { cols: 80, rows: 24, launch: None, title: Some("build".to_string()) }
        );
        let too_long = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: None,
            title: Some("x".repeat(65)),
        };
        assert_eq!(too_long.validate(), Err(ResourceValidationError::InvalidTerminalTitle));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib --locked domain::resource:: -- --test-threads=1`
Expected: compile errors (`TerminalLaunch`, `terminal`, `is_plain_shell` undefined; struct variant has no field `launch`).

- [ ] **Step 3: Implement the recipe change**

In `src/domain/resource.rs`, replace the `ResourceRecipe` enum head and add the launch type and errors:

```rust
use std::path::PathBuf;

pub const MAX_TERMINAL_TITLE_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalLaunch {
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl TerminalLaunch {
    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        if !self.cwd.is_absolute() || self.program.as_os_str().is_empty() {
            return Err(ResourceValidationError::InvalidTerminalLaunch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRecipe {
    Terminal {
        cols: u16,
        rows: u16,
        /// `None`: provider-owned terminal (the pre-V16 shape). `Some`: plain shell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch: Option<TerminalLaunch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Browser { start_url: String },
    Service { command: String },
}
```

Add to `ResourceValidationError`: `InvalidTerminalLaunch`, `InvalidTerminalTitle`.

Add helpers on `ResourceRecipe`:

```rust
    pub fn terminal(cols: u16, rows: u16) -> Self {
        Self::Terminal { cols, rows, launch: None, title: None }
    }

    pub fn is_plain_shell(&self) -> bool {
        matches!(self, Self::Terminal { launch: Some(_), .. })
    }

    fn canonical_title(title: Option<String>) -> Result<Option<String>, ResourceValidationError> {
        match title {
            None => Ok(None),
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.chars().count() > MAX_TERMINAL_TITLE_CHARS {
                    return Err(ResourceValidationError::InvalidTerminalTitle);
                }
                Ok(Some(trimmed.to_string()))
            }
        }
    }
```

Update `canonicalize` and `validate` Terminal arms:

```rust
            Self::Terminal { cols, rows, launch, title } => {
                if cols == 0 || rows == 0 {
                    return Err(ResourceValidationError::InvalidTerminalGeometry);
                }
                if let Some(launch) = launch.as_ref() {
                    launch.validate()?;
                }
                let title = Self::canonical_title(title)?;
                Ok(Self::Terminal { cols, rows, launch, title })
            }
```

```rust
            Self::Terminal { cols, rows, launch, title } => {
                if *cols == 0 || *rows == 0 {
                    return Err(ResourceValidationError::InvalidTerminalGeometry);
                }
                if let Some(launch) = launch.as_ref() {
                    launch.validate()?;
                }
                if let Some(title) = title.as_ref() {
                    if title.trim() != title
                        || title.is_empty()
                        || title.chars().count() > MAX_TERMINAL_TITLE_CHARS
                    {
                        return Err(ResourceValidationError::InvalidTerminalTitle);
                    }
                }
                Ok(())
            }
```

Update the wire enum inside `Deserialize for ResourceRecipe`:

```rust
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum ResourceRecipeWire {
            Terminal {
                cols: u16,
                rows: u16,
                #[serde(default)]
                launch: Option<TerminalLaunch>,
                #[serde(default)]
                title: Option<String>,
            },
            Browser { start_url: String },
            Service { command: String },
        }

        match ResourceRecipeWire::deserialize(deserializer)? {
            ResourceRecipeWire::Terminal { cols, rows, launch, title } => {
                Self::Terminal { cols, rows, launch, title }
                    .canonicalize()
                    .map_err(de::Error::custom)
            }
            // Browser / Service arms unchanged
        }
```

Then fix every existing constructor `ResourceRecipe::Terminal { cols, rows }` across the crate to `ResourceRecipe::terminal(cols, rows)`:

Run: `git grep -n "ResourceRecipe::Terminal {" -- src tests` and edit each site (expected in `src/kernel/command_bus.rs`, `src/host/connection.rs`, `src/host/cockpit.rs`, `src/domain/*`, tests). Pattern matches `ResourceRecipe::Terminal { cols, rows }` become `ResourceRecipe::Terminal { cols, rows, .. }`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib --locked domain::resource:: -- --test-threads=1`
Expected: all pass, including the byte-stability test (skip_serializing_if keeps the old two-field map).

The byte-stability test is load-bearing: the kernel loader `unpack_projection_blob` (`src/kernel/command_bus.rs:10419-10433`) re-encodes every decoded recipe with `projector::pack` and rejects the row with `StoreError::CodecMismatch` if the bytes differ. Without `skip_serializing_if` every pre-existing `resources.recipe` blob would fail to load.

- [ ] **Step 5: Run the crate check and commit**

Run: `cargo check --locked --lib --bins --tests` (expected EXIT 0)

```bash
git add src/domain/resource.rs $(git diff --name-only -- src tests)
git commit -m "feat(domain): add launch recipe and title to Terminal resources

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Terminal facts and strip types on the task snapshot

**Files:**
- Create: `src/domain/terminal_facts.rs`
- Modify: `src/domain/mod.rs` (export), `src/domain/snapshot.rs:25-38`

**Interfaces:**
- Produces:

```rust
pub struct TerminalFacts {
    pub resource_id: ResourceId,
    pub title: Option<String>,
    pub live_cwd: Option<PathBuf>,
    pub exit: Option<TerminalExit>,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}
pub struct TerminalExit { pub code: Option<i32>, pub summary: String, pub at_ms: i64 }
pub struct TaskTerminalStrip { pub order: Vec<ResourceId>, pub focused: Option<ResourceId> }
pub enum TerminalStripError { Duplicate(ResourceId), FocusedNotInOrder(ResourceId), NotATerminal(ResourceId), ForeignTask(ResourceId) }
impl TaskTerminalStrip { pub fn validate(&self, task_id: TaskId, resources: &BTreeMap<ResourceId, ResourceFacts>) -> Result<(), TerminalStripError>; }
pub const MAX_PLAIN_SHELLS_PER_TASK: usize = 8;
pub const TERMINAL_CWD_DEBOUNCE_MS: i64 = 2_000;
pub const TERMINAL_ACTIVITY_COALESCE_MS: i64 = 30_000;
```
- `TaskSnapshot` gains `pub terminal_facts: BTreeMap<ResourceId, TerminalFacts>` and `pub terminal_strip: TaskTerminalStrip` (both `Default` for existing constructors).

- [ ] **Step 1: Write the failing tests**

Create `src/domain/terminal_facts.rs` with only the tests module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe};
    use crate::domain::{ResourceId, TaskId};
    use std::collections::BTreeMap;

    fn terminal_resource(task_id: TaskId) -> ResourceFacts {
        ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::terminal(80, 24),
            1_725_000_000_000,
        )
    }

    #[test]
    fn strip_rejects_duplicates_and_unknown_focus() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let mut resources = BTreeMap::new();
        resources.insert(a.id, a.clone());
        let duplicate = TaskTerminalStrip { order: vec![a.id, a.id], focused: None };
        assert_eq!(duplicate.validate(task_id, &resources), Err(TerminalStripError::Duplicate(a.id)));
        let stranger = ResourceId::new();
        let bad_focus = TaskTerminalStrip { order: vec![a.id], focused: Some(stranger) };
        assert_eq!(
            bad_focus.validate(task_id, &resources),
            Err(TerminalStripError::FocusedNotInOrder(stranger))
        );
    }

    #[test]
    fn strip_rejects_foreign_and_non_terminal_resources() {
        let task_id = TaskId::new();
        let other_task = TaskId::new();
        let foreign = terminal_resource(other_task);
        let browser = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::BrowserContext,
            ResourceRecipe::browser("https://example.test").expect("browser recipe"),
            1_725_000_000_000,
        );
        let mut resources = BTreeMap::new();
        resources.insert(foreign.id, foreign.clone());
        resources.insert(browser.id, browser.clone());
        assert_eq!(
            TaskTerminalStrip { order: vec![foreign.id], focused: None }.validate(task_id, &resources),
            Err(TerminalStripError::ForeignTask(foreign.id))
        );
        assert_eq!(
            TaskTerminalStrip { order: vec![browser.id], focused: None }.validate(task_id, &resources),
            Err(TerminalStripError::NotATerminal(browser.id))
        );
    }

    #[test]
    fn valid_strip_passes() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let b = terminal_resource(task_id);
        let mut resources = BTreeMap::new();
        resources.insert(a.id, a.clone());
        resources.insert(b.id, b.clone());
        let strip = TaskTerminalStrip { order: vec![b.id, a.id], focused: Some(a.id) };
        assert_eq!(strip.validate(task_id, &resources), Ok(()));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib --locked domain::terminal_facts:: -- --test-threads=1`
Expected: compile error, module/types undefined.

- [ ] **Step 3: Implement the types**

Prepend to `src/domain/terminal_facts.rs`:

```rust
//! Durable per-terminal facts and the per-task terminal strip.
//!
//! `ResourceId` is the only identity. Everything here is a recorded fact.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::resource::{ResourceFacts, ResourceKind};
use crate::domain::{ResourceId, TaskId};

pub const MAX_PLAIN_SHELLS_PER_TASK: usize = 8;
pub const TERMINAL_CWD_DEBOUNCE_MS: i64 = 2_000;
pub const TERMINAL_ACTIVITY_COALESCE_MS: i64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalExit {
    pub code: Option<i32>,
    pub summary: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalFacts {
    pub resource_id: ResourceId,
    pub title: Option<String>,
    pub live_cwd: Option<PathBuf>,
    pub exit: Option<TerminalExit>,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}

impl TerminalFacts {
    pub fn registered(resource_id: ResourceId, title: Option<String>, created_at_ms: i64) -> Self {
        Self {
            resource_id,
            title,
            live_cwd: None,
            exit: None,
            created_at_ms,
            last_activity_at_ms: created_at_ms,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalStrip {
    pub order: Vec<ResourceId>,
    pub focused: Option<ResourceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStripError {
    Duplicate(ResourceId),
    FocusedNotInOrder(ResourceId),
    NotATerminal(ResourceId),
    ForeignTask(ResourceId),
}

impl TaskTerminalStrip {
    pub fn validate(
        &self,
        task_id: TaskId,
        resources: &BTreeMap<ResourceId, ResourceFacts>,
    ) -> Result<(), TerminalStripError> {
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.order {
            if !seen.insert(*id) {
                return Err(TerminalStripError::Duplicate(*id));
            }
            let facts = resources.get(id).ok_or(TerminalStripError::ForeignTask(*id))?;
            if facts.task_id != Some(task_id) {
                return Err(TerminalStripError::ForeignTask(*id));
            }
            if facts.resource_kind != ResourceKind::Terminal {
                return Err(TerminalStripError::NotATerminal(*id));
            }
        }
        if let Some(focused) = self.focused {
            if !seen.contains(&focused) {
                return Err(TerminalStripError::FocusedNotInOrder(focused));
            }
        }
        Ok(())
    }

    /// Remove a released terminal; clear focus if it pointed at it.
    pub fn remove(&mut self, resource_id: ResourceId) {
        self.order.retain(|id| *id != resource_id);
        if self.focused == Some(resource_id) {
            self.focused = self.order.last().copied();
        }
    }
}
```

In `src/domain/mod.rs` add `pub mod terminal_facts;` beside the other domain modules, and `pub use terminal_facts::{TaskTerminalStrip, TerminalExit, TerminalFacts};` if the module re-exports other fact types the same way (match the neighbouring `pub use` lines).

In `src/domain/snapshot.rs` add to `TaskSnapshot`:

```rust
    pub terminal_facts: BTreeMap<ResourceId, crate::domain::terminal_facts::TerminalFacts>,
    pub terminal_strip: crate::domain::terminal_facts::TaskTerminalStrip,
```

and initialize both with `Default::default()` wherever `TaskSnapshot { .. }` is constructed (`git grep -n "TaskSnapshot {" -- src tests`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib --locked domain::terminal_facts:: -- --test-threads=1` then `cargo check --locked --lib --bins --tests`
Expected: PASS; check EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/domain/terminal_facts.rs src/domain/mod.rs src/domain/snapshot.rs $(git diff --name-only -- src tests)
git commit -m "feat(domain): add terminal facts and task terminal strip

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: Terminal events

**Files:**
- Modify: `src/domain/event.rs` (payload structs ~528-610, `Event` enum ~996-1076, `event_type` ~1147-1185, `EventBody` ~1202-1271, encode ~1403-1421, decode ~1551-1678, `is_task_mutation` ~1187-1199, `apply` ~2049-2136, `apply_into` ~2529-2589 and the `unreachable!` arm ~2909)

**Interfaces:**
- Produces `Event::{TerminalRenamed, TerminalCwdReported, TerminalExited, TerminalActivity, TaskTerminalStripSet}` with wire names `terminal.renamed`, `terminal.cwd_reported`, `terminal.exited`, `terminal.activity`, `task.terminal_strip_set`.
- `TerminalCwdReported`, `TerminalExited`, `TerminalActivity` are host facts: they apply without consuming a task revision (same handling as `ProviderInputDelivered`). `TerminalRenamed` and `TaskTerminalStripSet` are user mutations and consume a revision.
- `ResourceRegistered` for a Terminal now also inserts `TerminalFacts::registered(id, title, occurred_at_ms)`; `ResourceReleased` removes the facts entry and calls `terminal_strip.remove(id)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod durable_workspace_serde_tests` in `src/domain/event.rs`:

```rust
    #[test]
    fn terminal_events_round_trip_json_and_msgpack() {
        let resource_id = ResourceId::new();
        let events = vec![
            Event::TerminalRenamed { resource_id, title: "build".to_string() },
            Event::TerminalCwdReported { resource_id, cwd: std::path::PathBuf::from(r"C:\Code\demo") },
            Event::TerminalExited { resource_id, code: Some(0), summary: "Shell exited with code 0".to_string() },
            Event::TerminalActivity { resource_id },
            Event::TaskTerminalStripSet {
                strip: crate::domain::terminal_facts::TaskTerminalStrip {
                    order: vec![resource_id],
                    focused: Some(resource_id),
                },
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("event json");
            let replayed: Event = serde_json::from_str(&json).expect("event replay");
            assert_eq!(replayed, event);
            let packed = rmp_serde::to_vec(&event).expect("event msgpack");
            let unpacked: Event = rmp_serde::from_slice(&packed).expect("event unpack");
            assert_eq!(unpacked, event);
        }
    }

    #[test]
    fn terminal_event_types_are_stable() {
        let resource_id = ResourceId::new();
        assert_eq!(Event::TerminalRenamed { resource_id, title: "x".into() }.event_type(), "terminal.renamed");
        assert_eq!(Event::TerminalCwdReported { resource_id, cwd: "C:\\".into() }.event_type(), "terminal.cwd_reported");
        assert_eq!(Event::TerminalExited { resource_id, code: None, summary: "s".into() }.event_type(), "terminal.exited");
        assert_eq!(Event::TerminalActivity { resource_id }.event_type(), "terminal.activity");
        assert_eq!(
            Event::TaskTerminalStripSet { strip: Default::default() }.event_type(),
            "task.terminal_strip_set"
        );
    }

    #[test]
    fn host_terminal_facts_do_not_consume_a_task_revision() {
        let resource_id = ResourceId::new();
        assert!(!Event::TerminalCwdReported { resource_id, cwd: "C:\\".into() }.is_task_mutation());
        assert!(!Event::TerminalExited { resource_id, code: None, summary: "s".into() }.is_task_mutation());
        assert!(!Event::TerminalActivity { resource_id }.is_task_mutation());
        assert!(Event::TerminalRenamed { resource_id, title: "x".into() }.is_task_mutation());
        assert!(Event::TaskTerminalStripSet { strip: Default::default() }.is_task_mutation());
    }
```

Add an apply test beside the existing `apply_into` resource tests (find the test that registers a Terminal resource on a snapshot and copy its setup helper; call it `snapshot_with_terminal_resource` if none exists):

```rust
    #[test]
    fn terminal_facts_follow_resource_lifecycle_and_events() {
        let (mut snapshot, resource_id) = snapshot_with_terminal_resource();
        let facts = snapshot.terminal_facts.get(&resource_id).expect("facts on registration");
        assert_eq!(facts.title, None);
        assert_eq!(facts.exit, None);

        apply_into(
            &mut snapshot,
            &Event::TerminalRenamed { resource_id, title: "build".to_string() },
            1_725_000_000_500,
        )
        .expect("rename");
        assert_eq!(snapshot.terminal_facts[&resource_id].title.as_deref(), Some("build"));

        apply_into(
            &mut snapshot,
            &Event::TerminalCwdReported { resource_id, cwd: std::path::PathBuf::from(r"C:\Code\demo") },
            1_725_000_000_600,
        )
        .expect("cwd");
        assert_eq!(
            snapshot.terminal_facts[&resource_id].live_cwd,
            Some(std::path::PathBuf::from(r"C:\Code\demo"))
        );

        apply_into(&mut snapshot, &Event::TerminalActivity { resource_id }, 1_725_000_000_700).expect("activity");
        assert_eq!(snapshot.terminal_facts[&resource_id].last_activity_at_ms, 1_725_000_000_700);

        apply_into(
            &mut snapshot,
            &Event::TerminalExited { resource_id, code: Some(1), summary: "Shell exited with code 1".to_string() },
            1_725_000_000_800,
        )
        .expect("exit");
        assert_eq!(snapshot.terminal_facts[&resource_id].exit.as_ref().map(|e| e.code), Some(Some(1)));

        let generation = snapshot.resources[&resource_id].runtime_generation;
        apply_into(
            &mut snapshot,
            &Event::ResourceReleaseBegun { resource_id, runtime_generation: generation },
            1_725_000_000_900,
        )
        .expect("release begun");
        apply_into(
            &mut snapshot,
            &Event::ResourceReleased { resource_id, runtime_generation: generation },
            1_725_000_001_000,
        )
        .expect("released");
        assert!(!snapshot.terminal_facts.contains_key(&resource_id));
        assert!(!snapshot.terminal_strip.order.contains(&resource_id));
    }

    #[test]
    fn strip_set_rejects_unknown_resource() {
        let (mut snapshot, _resource_id) = snapshot_with_terminal_resource();
        let result = apply_into(
            &mut snapshot,
            &Event::TaskTerminalStripSet {
                strip: crate::domain::terminal_facts::TaskTerminalStrip {
                    order: vec![ResourceId::new()],
                    focused: None,
                },
            },
            1_725_000_000_500,
        );
        assert_eq!(result, Err(ApplyError::NotFound));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib --locked domain::event::durable_workspace_serde_tests -- --test-threads=1`
Expected: compile errors, variants undefined.

- [ ] **Step 3: Implement the events**

Payload structs (next to `ResourceReleasedPayload`):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRenamedPayload {
    pub resource_id: ResourceId,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCwdReportedPayload {
    pub resource_id: ResourceId,
    pub cwd: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalExitedPayload {
    pub resource_id: ResourceId,
    pub code: Option<i32>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalActivityPayload {
    pub resource_id: ResourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalStripSetPayload {
    pub strip: crate::domain::terminal_facts::TaskTerminalStrip,
}
```

`Event` variants (after `ResourceReleased`):

```rust
    TerminalRenamed {
        resource_id: ResourceId,
        title: String,
    },
    TerminalCwdReported {
        resource_id: ResourceId,
        cwd: std::path::PathBuf,
    },
    TerminalExited {
        resource_id: ResourceId,
        code: Option<i32>,
        summary: String,
    },
    TerminalActivity {
        resource_id: ResourceId,
    },
    TaskTerminalStripSet {
        strip: crate::domain::terminal_facts::TaskTerminalStrip,
    },
```

`event_type()` arms:

```rust
            Self::TerminalRenamed { .. } => "terminal.renamed",
            Self::TerminalCwdReported { .. } => "terminal.cwd_reported",
            Self::TerminalExited { .. } => "terminal.exited",
            Self::TerminalActivity { .. } => "terminal.activity",
            Self::TaskTerminalStripSet { .. } => "task.terminal_strip_set",
```

`EventBody` variants:

```rust
    #[serde(rename = "terminal.renamed")]
    TerminalRenamed(TerminalRenamedPayload),
    #[serde(rename = "terminal.cwd_reported")]
    TerminalCwdReported(TerminalCwdReportedPayload),
    #[serde(rename = "terminal.exited")]
    TerminalExited(TerminalExitedPayload),
    #[serde(rename = "terminal.activity")]
    TerminalActivity(TerminalActivityPayload),
    #[serde(rename = "task.terminal_strip_set")]
    TaskTerminalStripSet(TaskTerminalStripSetPayload),
```

Encode arms in `From<&Event> for EventDocument`:

```rust
            Event::TerminalRenamed { resource_id, title } => EventBody::TerminalRenamed(
                TerminalRenamedPayload { resource_id: *resource_id, title: title.clone() },
            ),
            Event::TerminalCwdReported { resource_id, cwd } => EventBody::TerminalCwdReported(
                TerminalCwdReportedPayload { resource_id: *resource_id, cwd: cwd.clone() },
            ),
            Event::TerminalExited { resource_id, code, summary } => EventBody::TerminalExited(
                TerminalExitedPayload { resource_id: *resource_id, code: *code, summary: summary.clone() },
            ),
            Event::TerminalActivity { resource_id } => {
                EventBody::TerminalActivity(TerminalActivityPayload { resource_id: *resource_id })
            }
            Event::TaskTerminalStripSet { strip } => {
                EventBody::TaskTerminalStripSet(TaskTerminalStripSetPayload { strip: strip.clone() })
            }
```

Decode arms in `TryFrom<EventDocument> for Event`:

```rust
            EventBody::TerminalRenamed(p) => {
                let trimmed = p.title.trim();
                if trimmed.is_empty()
                    || trimmed != p.title
                    || trimmed.chars().count() > crate::domain::resource::MAX_TERMINAL_TITLE_CHARS
                {
                    return Err(EventSerdeError::Payload);
                }
                Event::TerminalRenamed { resource_id: p.resource_id, title: p.title }
            }
            EventBody::TerminalCwdReported(p) => {
                if !p.cwd.is_absolute() {
                    return Err(EventSerdeError::Payload);
                }
                Event::TerminalCwdReported { resource_id: p.resource_id, cwd: p.cwd }
            }
            EventBody::TerminalExited(p) => Event::TerminalExited {
                resource_id: p.resource_id,
                code: p.code,
                summary: p.summary,
            },
            EventBody::TerminalActivity(p) => Event::TerminalActivity { resource_id: p.resource_id },
            EventBody::TaskTerminalStripSet(p) => Event::TaskTerminalStripSet { strip: p.strip },
```

`is_task_mutation()`: add `Self::TerminalCwdReported { .. } | Self::TerminalExited { .. } | Self::TerminalActivity { .. }` to the list of events that return `false` (the same list `ProviderInputDelivered` is in).

`apply()`: add an explicit arm next to the `ProviderInputDelivered` arm that applies without `require_next_revision`:

```rust
        Event::TerminalCwdReported { .. }
        | Event::TerminalExited { .. }
        | Event::TerminalActivity { .. } => {
            let mut snap = snapshot.ok_or(ApplyError::MissingSnapshot)?;
            require_matching_task_id(&snap, event)?;
            // Host terminal facts are durable projection changes that do not
            // consume a task revision, exactly like ProviderInputDelivered.
            apply_into(&mut snap, &event.payload, event.occurred_at_ms)?;
            Ok(snap)
        }
```

This mirrors the `ProviderInputDelivered` arm at `event.rs:2118-2126` (same `require_matching_task_id`, same `Ok(snap)`); the catch-all `other =>` arm would otherwise demand `event.task_revision == snap.task.revision + 1`.

`apply_into` arms:

```rust
        Event::TerminalRenamed { resource_id, title } => {
            let new_title = Some(title.clone());
            let facts = snap.terminal_facts.get_mut(resource_id).ok_or(ApplyError::NotFound)?;
            facts.title = new_title.clone();
            if let Some(ResourceRecipe::Terminal { title: recipe_title, .. }) =
                snap.resources.get_mut(resource_id).map(|r| &mut r.recipe)
            {
                *recipe_title = new_title;
            }
        }
        Event::TerminalCwdReported { resource_id, cwd } => {
            let facts = snap.terminal_facts.get_mut(resource_id).ok_or(ApplyError::NotFound)?;
            facts.live_cwd = Some(cwd.clone());
            facts.last_activity_at_ms = occurred_at_ms;
        }
        Event::TerminalExited { resource_id, code, summary } => {
            let facts = snap.terminal_facts.get_mut(resource_id).ok_or(ApplyError::NotFound)?;
            facts.exit = Some(crate::domain::terminal_facts::TerminalExit {
                code: *code,
                summary: summary.clone(),
                at_ms: occurred_at_ms,
            });
        }
        Event::TerminalActivity { resource_id } => {
            let facts = snap.terminal_facts.get_mut(resource_id).ok_or(ApplyError::NotFound)?;
            facts.last_activity_at_ms = occurred_at_ms;
        }
        Event::TaskTerminalStripSet { strip } => {
            strip
                .validate(snap.task.id, &snap.resources)
                .map_err(|_| ApplyError::NotFound)?;
            snap.terminal_strip = strip.clone();
        }
```

In the existing `ResourceRegistered` arm, after inserting the resource:

```rust
            if resource.resource_kind == ResourceKind::Terminal {
                let title = match &resource.recipe {
                    ResourceRecipe::Terminal { title, .. } => title.clone(),
                    _ => None,
                };
                snap.terminal_facts.insert(
                    resource.id,
                    crate::domain::terminal_facts::TerminalFacts::registered(resource.id, title, occurred_at_ms),
                );
                if resource.recipe.is_plain_shell() {
                    snap.terminal_strip.order.push(resource.id);
                    if snap.terminal_strip.focused.is_none() {
                        snap.terminal_strip.focused = Some(resource.id);
                    }
                }
            }
```

In the existing `ResourceReleased` arm, after setting lifecycle `Released`:

```rust
            snap.terminal_facts.remove(&resource_id);
            snap.terminal_strip.remove(resource_id);
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib --locked domain::event -- --test-threads=1`
Expected: PASS (the exhaustive `unreachable!` arm compiles only once every new variant has an `apply_into` arm).

- [ ] **Step 5: Commit**

```bash
git add src/domain/event.rs
git commit -m "feat(domain): add terminal fact and strip events

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: Commands for shells

**Files:**
- Modify: `src/domain/command.rs` (`Command` ~1202-1278, `decide` ~1376+, helpers ~2025-2086), `src/kernel/command_bus.rs:209-230` (host-only list)

**Interfaces:**
- Produces client commands:

```rust
Command::OpenShellTerminal(OpenShellTerminalIntent { resource: ResourceFacts })   // resource carries the resolved launch recipe
Command::CloseTerminal { resource_id: ResourceId }
Command::RenameTerminal { resource_id: ResourceId, title: String }
Command::SetTerminalStrip(TaskTerminalStrip)
```
and host-only fact commands (rejected with `StoreError::HostAuthorityRequired` on the client `execute` path, like `StartProviderSession`):

```rust
Command::RecordTerminalCwd { resource_id: ResourceId, cwd: PathBuf }
Command::RecordTerminalExit { resource_id: ResourceId, code: Option<i32>, summary: String }
Command::RecordTerminalActivity { resource_id: ResourceId }
```
- `RejectionCode::TooManyTerminals` (new) when the task already has 8 plain shells.

- [ ] **Step 1: Write the failing tests**

Add to the tests module in `src/kernel/command_bus.rs`, reusing the helpers `accepted_revision`, `task_envelope`, `host_execute` that live there:

```rust
    fn plain_shell_facts(task_id: TaskId, title: Option<&str>) -> ResourceFacts {
        ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
                launch: Some(crate::domain::resource::TerminalLaunch {
                    cwd: std::path::PathBuf::from(r"C:\Code\demo"),
                    program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                    args: Vec::new(),
                }),
                title: title.map(str::to_string),
            },
            1_725_000_000_100,
        )
    }

    #[test]
    fn open_rename_strip_and_close_shell_terminal() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&directory.path().join("tasks.sqlite")).expect("bus");
        let client_id = ClientId::new();
        let (task_id, revision) = create_open_task(&mut bus, client_id); // existing helper in this module; if named differently, use that name

        let shell = plain_shell_facts(task_id, None);
        let receipt = bus
            .execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::OpenShellTerminal(OpenShellTerminalIntent { resource: shell.clone() }),
            ))
            .expect("open shell");
        let revision = accepted_revision(&receipt);
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert!(snapshot.resources[&shell.id].recipe.is_plain_shell());
        assert_eq!(snapshot.terminal_strip.order, vec![shell.id]);
        assert_eq!(snapshot.terminal_strip.focused, Some(shell.id));

        let receipt = bus
            .execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::RenameTerminal { resource_id: shell.id, title: "  build ".to_string() },
            ))
            .expect("rename");
        let revision = accepted_revision(&receipt);
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert_eq!(snapshot.terminal_facts[&shell.id].title.as_deref(), Some("build"));

        let second = plain_shell_facts(task_id, Some("tests"));
        let receipt = bus
            .execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::OpenShellTerminal(OpenShellTerminalIntent { resource: second.clone() }),
            ))
            .expect("open second");
        let revision = accepted_revision(&receipt);
        let receipt = bus
            .execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::SetTerminalStrip(crate::domain::terminal_facts::TaskTerminalStrip {
                    order: vec![second.id, shell.id],
                    focused: Some(second.id),
                }),
            ))
            .expect("strip");
        let revision = accepted_revision(&receipt);
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert_eq!(snapshot.terminal_strip.order, vec![second.id, shell.id]);

        // Host facts do not need a revision fence and do not bump the revision.
        host_execute(
            &mut bus,
            task_id,
            Command::RecordTerminalCwd { resource_id: shell.id, cwd: std::path::PathBuf::from(r"C:\Code\demo\src") },
        );
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert_eq!(snapshot.task.revision, revision);
        assert_eq!(
            snapshot.terminal_facts[&shell.id].live_cwd,
            Some(std::path::PathBuf::from(r"C:\Code\demo\src"))
        );

        let receipt = bus
            .execute(task_envelope(client_id, task_id, revision, Command::CloseTerminal { resource_id: shell.id }))
            .expect("close");
        let _ = accepted_revision(&receipt);
        let snapshot = bus.task_snapshot(task_id).expect("snapshot").expect("task");
        assert_ne!(snapshot.resources[&shell.id].lifecycle, ResourceLifecycle::Active);
    }

    #[test]
    fn ninth_shell_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&directory.path().join("tasks.sqlite")).expect("bus");
        let client_id = ClientId::new();
        let (task_id, mut revision) = create_open_task(&mut bus, client_id);
        for _ in 0..8 {
            let receipt = bus
                .execute(task_envelope(
                    client_id,
                    task_id,
                    revision,
                    Command::OpenShellTerminal(OpenShellTerminalIntent { resource: plain_shell_facts(task_id, None) }),
                ))
                .expect("open");
            revision = accepted_revision(&receipt);
        }
        let receipt = bus
            .execute(task_envelope(
                client_id,
                task_id,
                revision,
                Command::OpenShellTerminal(OpenShellTerminalIntent { resource: plain_shell_facts(task_id, None) }),
            ))
            .expect("ninth executes");
        assert!(matches!(receipt, CommandReceipt::Rejected { code: RejectionCode::TooManyTerminals, .. }));
    }

    #[test]
    fn client_cannot_record_terminal_facts() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut bus = CommandBus::open(&directory.path().join("tasks.sqlite")).expect("bus");
        let client_id = ClientId::new();
        let (task_id, revision) = create_open_task(&mut bus, client_id);
        let result = bus.execute(task_envelope(
            client_id,
            task_id,
            revision,
            Command::RecordTerminalActivity { resource_id: ResourceId::new() },
        ));
        assert!(matches!(result, Err(StoreError::HostAuthorityRequired)));
    }
```

If no `create_open_task` helper exists in that module, add one that runs `Command::CreateTask(CreateTaskIntent { .. })` through `execute_for_test` exactly as `correlated_provider_binding_is_exact_write_once_and_restorable` does, and returns `(task_id, accepted_revision(&receipt))`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib --locked kernel::command_bus::tests::open_rename_strip -- --test-threads=1`
Expected: compile errors, commands undefined.

- [ ] **Step 3: Implement the commands**

In `src/domain/command.rs` add the intent and variants:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OpenShellTerminalIntent {
    pub resource: ResourceFacts,
}
```

```rust
    OpenShellTerminal(OpenShellTerminalIntent),
    CloseTerminal {
        resource_id: ResourceId,
    },
    RenameTerminal {
        resource_id: ResourceId,
        title: String,
    },
    SetTerminalStrip(crate::domain::terminal_facts::TaskTerminalStrip),
    RecordTerminalCwd {
        resource_id: ResourceId,
        cwd: std::path::PathBuf,
    },
    RecordTerminalExit {
        resource_id: ResourceId,
        code: Option<i32>,
        summary: String,
    },
    RecordTerminalActivity {
        resource_id: ResourceId,
    },
```

Add `TooManyTerminals` to `RejectionCode` (and its wire name where the enum is serialized, matching neighbours).

`decide` arms (model each on the `RegisterResource` arm, which already does `require_runtime_capable_task` + `require_expected_revision` + `validate_for_registration` + task/kind checks):

```rust
        Command::OpenShellTerminal(intent) => {
            let snap = require_runtime_capable_task(snapshot)?;
            require_expected_revision(snap, envelope)?;
            let resource = intent.resource.clone();
            resource.validate_for_registration().map_err(|_| RejectionCode::InvalidResource)?;
            if resource.task_id != Some(snap.task.id)
                || resource.resource_kind != ResourceKind::Terminal
                || !resource.recipe.is_plain_shell()
            {
                return Err(RejectionCode::InvalidResource);
            }
            let plain_shells = snap
                .resources
                .values()
                .filter(|r| r.recipe.is_plain_shell() && r.lifecycle == ResourceLifecycle::Active)
                .count();
            if plain_shells >= crate::domain::terminal_facts::MAX_PLAIN_SHELLS_PER_TASK {
                return Err(RejectionCode::TooManyTerminals);
            }
            Ok(vec![Event::ResourceRegistered { resource }])
        }
        Command::CloseTerminal { resource_id } => {
            let snap = require_open_or_closing_task(snapshot)?;
            require_expected_revision(snap, envelope)?;
            let resource = snap.resources.get(resource_id).ok_or(RejectionCode::NotFound)?;
            if resource.resource_kind != ResourceKind::Terminal || !resource.recipe.is_plain_shell() {
                return Err(RejectionCode::InvalidResource);
            }
            if resource.lifecycle != ResourceLifecycle::Active {
                return Err(RejectionCode::InvalidTransition);
            }
            Ok(vec![Event::ResourceReleaseBegun {
                resource_id: *resource_id,
                runtime_generation: resource.runtime_generation,
            }])
        }
        Command::RenameTerminal { resource_id, title } => {
            let snap = require_open_or_closing_task(snapshot)?;
            require_expected_revision(snap, envelope)?;
            snap.terminal_facts.get(resource_id).ok_or(RejectionCode::NotFound)?;
            let trimmed = title.trim();
            if trimmed.is_empty() || trimmed.chars().count() > crate::domain::resource::MAX_TERMINAL_TITLE_CHARS {
                return Err(RejectionCode::InvalidTitle);
            }
            Ok(vec![Event::TerminalRenamed { resource_id: *resource_id, title: trimmed.to_string() }])
        }
        Command::SetTerminalStrip(strip) => {
            let snap = require_open_or_closing_task(snapshot)?;
            require_expected_revision(snap, envelope)?;
            strip.validate(snap.task.id, &snap.resources).map_err(|_| RejectionCode::InvalidResource)?;
            Ok(vec![Event::TaskTerminalStripSet { strip: strip.clone() }])
        }
        Command::RecordTerminalCwd { resource_id, cwd } => {
            let snap = require_open_or_closing_task(snapshot)?;
            let facts = snap.terminal_facts.get(resource_id).ok_or(RejectionCode::NotFound)?;
            if !cwd.is_absolute() {
                return Err(RejectionCode::InvalidResource);
            }
            if facts.live_cwd.as_ref() == Some(cwd) {
                return Ok(Vec::new());
            }
            Ok(vec![Event::TerminalCwdReported { resource_id: *resource_id, cwd: cwd.clone() }])
        }
        Command::RecordTerminalExit { resource_id, code, summary } => {
            let snap = require_open_or_closing_task(snapshot)?;
            let facts = snap.terminal_facts.get(resource_id).ok_or(RejectionCode::NotFound)?;
            if facts.exit.is_some() {
                return Ok(Vec::new());
            }
            Ok(vec![Event::TerminalExited { resource_id: *resource_id, code: *code, summary: summary.clone() }])
        }
        Command::RecordTerminalActivity { resource_id } => {
            let snap = require_open_or_closing_task(snapshot)?;
            let facts = snap.terminal_facts.get(resource_id).ok_or(RejectionCode::NotFound)?;
            if envelope.issued_at_ms - facts.last_activity_at_ms
                < crate::domain::terminal_facts::TERMINAL_ACTIVITY_COALESCE_MS
            {
                return Ok(Vec::new());
            }
            Ok(vec![Event::TerminalActivity { resource_id: *resource_id }])
        }
```

Use the existing `RejectionCode` names for not-found / invalid-title / invalid-resource if they already exist under other names (`git grep -n "enum RejectionCode" -A40 src/domain/command.rs`); add only `TooManyTerminals`.

In `src/kernel/command_bus.rs::execute` (client path, ~209-230) add `Command::RecordTerminalCwd { .. } | Command::RecordTerminalExit { .. } | Command::RecordTerminalActivity { .. }` to the match that returns `Err(StoreError::HostAuthorityRequired)`. The host path `execute_host_authorized` already accepts every command.

Note for the `terminal_facts.get(...)` lookups in `decide`: `snapshot` is the durable `TaskSnapshot`, populated by Task 5's loader. Until Task 5 lands the kernel test above will fail on the rename step; that is expected ordering, run it green after Task 5.

- [ ] **Step 4: Run the tests to verify they pass (after Task 5)**

Run: `cargo check --locked --lib --bins --tests` now (EXIT 0); run the three new tests after Task 5.

- [ ] **Step 5: Commit**

```bash
git add src/domain/command.rs src/kernel/command_bus.rs
git commit -m "feat(domain): add shell terminal commands and host terminal facts

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: V16 projection, projector arms, and snapshot loading

**Files:**
- Modify: `src/kernel/schema.rs` (V16 constant, manifest entry, `PROJECTION_TABLES` at ~1816-1825, tests `provider_resource_v15_is_contiguous_and_hash_locked` ~2717-2730 and the applied-history assertions ~2019-2024), `src/kernel/projector.rs` (arms), `src/kernel/store.rs` (rebuild batches ~1213-1232, `canonical_table_dump` allowlist ~1258-1269), `src/kernel/command_bus.rs` (`load_task_snapshot` ~8747-8839, new `load_terminal_facts` / `load_terminal_strip` beside `load_resources` ~9253-9283)
- Test: `src/kernel/schema.rs` tests, `src/kernel/command_bus.rs` tests from Task 4

**Interfaces:**
- Produces tables:

```sql
CREATE TABLE terminal_facts (
  resource_id BLOB PRIMARY KEY CHECK(length(resource_id) = 16) REFERENCES resources(resource_id),
  task_id BLOB NOT NULL REFERENCES tasks(task_id),
  title TEXT,
  live_cwd TEXT,
  exit_code INTEGER,
  exit_summary TEXT,
  exited_at_ms INTEGER,
  created_at_ms INTEGER NOT NULL,
  last_activity_at_ms INTEGER NOT NULL
);
CREATE TABLE task_terminal_strip (
  task_id BLOB PRIMARY KEY CHECK(length(task_id) = 16) REFERENCES tasks(task_id),
  order_msgpack BLOB NOT NULL,
  focused_resource_id BLOB CHECK(focused_resource_id IS NULL OR length(focused_resource_id) = 16)
);
```
plus shadow twins, which `rebuild_projection_tables_tx` (`store.rs:1136-1143`) creates automatically for every name in `PROJECTION_TABLES` via `CREATE TEMP TABLE shadow_<table> AS SELECT * FROM <table> WHERE 0`.

- [ ] **Step 1: Write the failing test**

In `src/kernel/schema.rs` tests, update the manifest assertions and add:

```rust
    #[test]
    fn v16_adds_terminal_facts_and_strip_tables() {
        let migrations = migrations();
        assert_eq!(migrations.len(), 16);
        assert_eq!(migrations[15].version, 16);
        assert_eq!(migrations[15].name, "v16_terminal_facts_and_strip");
        assert!(V16_SQL.contains("CREATE TABLE terminal_facts"));
        assert!(V16_SQL.contains("CREATE TABLE task_terminal_strip"));
    }
```

Update `provider_resource_v15_is_contiguous_and_hash_locked` (`schema.rs:2717-2730`): `latest_migration_version()` → 16, `migrations.len()` → 16, and append
```rust
        assert_eq!(migrations[15].version, 16);
        assert_eq!(migrations[15].name, "v16_terminal_facts_and_strip");
        assert_eq!(migrations[15].sha256, sha256_bytes(V16_SQL));
```
Append to the applied-history assertions (`schema.rs:2019-2024`):
```rust
        assert_eq!(history[15].0, 16);
        assert_eq!(history[15].1, "v16_terminal_facts_and_strip");
        assert_eq!(history[15].2, sha256_bytes(V16_SQL).to_vec());
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked kernel::schema:: -- --test-threads=1`
Expected: FAIL, `V16_SQL` undefined / length 15.

- [ ] **Step 3: Implement**

`src/kernel/schema.rs`:

```rust
const V16_SQL: &str = "\
CREATE TABLE terminal_facts (\n\
  resource_id BLOB PRIMARY KEY CHECK(length(resource_id) = 16) REFERENCES resources(resource_id),\n\
  task_id BLOB NOT NULL REFERENCES tasks(task_id),\n\
  title TEXT,\n\
  live_cwd TEXT,\n\
  exit_code INTEGER,\n\
  exit_summary TEXT,\n\
  exited_at_ms INTEGER,\n\
  created_at_ms INTEGER NOT NULL,\n\
  last_activity_at_ms INTEGER NOT NULL\n\
);\n\
CREATE INDEX idx_terminal_facts_task ON terminal_facts(task_id);\n\
CREATE TABLE task_terminal_strip (\n\
  task_id BLOB PRIMARY KEY CHECK(length(task_id) = 16) REFERENCES tasks(task_id),\n\
  order_msgpack BLOB NOT NULL,\n\
  focused_resource_id BLOB CHECK(focused_resource_id IS NULL OR length(focused_resource_id) = 16)\n\
);\n\
";
```

Manifest entry after V15:

```rust
                Migration {
                    version: 16,
                    name: "v16_terminal_facts_and_strip",
                    sql: V16_SQL,
                    sha256: sha256_bytes(V16_SQL),
                },
```

Shadow twins and rebuild: append `"terminal_facts"` and `"task_terminal_strip"` to `PROJECTION_TABLES` (`schema.rs:1816-1825`). In `store.rs:1213-1232` extend both batches: prepend `DELETE FROM task_terminal_strip;\n DELETE FROM terminal_facts;\n` before `DELETE FROM resources;` (children before parents) and append `INSERT INTO terminal_facts SELECT * FROM shadow_terminal_facts;\n INSERT INTO task_terminal_strip SELECT * FROM shadow_task_terminal_strip;` after the `resources` insert (parents first). In `canonical_table_dump` (`store.rs:1258-1269`) add four allowlisted arms:
```rust
            ("terminal_facts", false) => "SELECT * FROM terminal_facts ORDER BY resource_id",
            ("terminal_facts", true) => "SELECT * FROM shadow_terminal_facts ORDER BY resource_id",
            ("task_terminal_strip", false) => "SELECT * FROM task_terminal_strip ORDER BY task_id",
            ("task_terminal_strip", true) => "SELECT * FROM shadow_task_terminal_strip ORDER BY task_id",
```
Never interpolate table names there; the comment at `store.rs:1263` forbids it.

`src/kernel/projector.rs` arms, following the `ResourceRegistered` arm's style (`table_name(base, shadow)`, `params!`, `bump_task_revision` only for revision-consuming events):

```rust
        Event::ResourceRegistered { resource } if resource.resource_kind == ResourceKind::Terminal => {
            // existing INSERT into resources stays; then:
            let title = match &resource.recipe {
                ResourceRecipe::Terminal { title, .. } => title.clone(),
                _ => None,
            };
            tx.execute(
                &format!(
                    "INSERT INTO {} (resource_id, task_id, title, live_cwd, exit_code, exit_summary, exited_at_ms, created_at_ms, last_activity_at_ms) VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, ?4, ?4)",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![
                    resource.id.as_bytes().as_slice(),
                    task_id.as_bytes().as_slice(),
                    title,
                    event.occurred_at_ms,
                ],
            )?;
            if resource.recipe.is_plain_shell() {
                append_to_terminal_strip(tx, shadow, task_id, resource.id)?;
            }
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::TerminalRenamed { resource_id, title } => {
            let task_id = require_task_id(event)?;
            tx.execute(
                &format!("UPDATE {} SET title = ?1 WHERE resource_id = ?2", table_name("terminal_facts", shadow)),
                rusqlite::params![title, resource_id.as_bytes().as_slice()],
            )?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
        Event::TerminalCwdReported { resource_id, cwd } => {
            tx.execute(
                &format!(
                    "UPDATE {} SET live_cwd = ?1, last_activity_at_ms = ?2 WHERE resource_id = ?3",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![cwd.to_string_lossy().into_owned(), event.occurred_at_ms, resource_id.as_bytes().as_slice()],
            )?;
        }
        Event::TerminalExited { resource_id, code, summary } => {
            tx.execute(
                &format!(
                    "UPDATE {} SET exit_code = ?1, exit_summary = ?2, exited_at_ms = ?3 WHERE resource_id = ?4",
                    table_name("terminal_facts", shadow)
                ),
                rusqlite::params![code, summary, event.occurred_at_ms, resource_id.as_bytes().as_slice()],
            )?;
        }
        Event::TerminalActivity { resource_id } => {
            tx.execute(
                &format!("UPDATE {} SET last_activity_at_ms = ?1 WHERE resource_id = ?2", table_name("terminal_facts", shadow)),
                rusqlite::params![event.occurred_at_ms, resource_id.as_bytes().as_slice()],
            )?;
        }
        Event::TaskTerminalStripSet { strip } => {
            let task_id = require_task_id(event)?;
            write_terminal_strip(tx, shadow, task_id, strip)?;
            bump_task_revision(tx, shadow, task_id, event)?;
        }
```

In the existing `ResourceReleased` arm add, after `update_resource_lifecycle`:

```rust
            tx.execute(
                &format!("DELETE FROM {} WHERE resource_id = ?1", table_name("terminal_facts", shadow)),
                rusqlite::params![resource_id.as_bytes().as_slice()],
            )?;
            remove_from_terminal_strip(tx, shadow, task_id, *resource_id)?;
```

Helpers at the bottom of `projector.rs`:

```rust
fn read_terminal_strip(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
) -> Result<crate::domain::terminal_facts::TaskTerminalStrip, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = tx
        .query_row(
            &format!(
                "SELECT order_msgpack, focused_resource_id FROM {} WHERE task_id = ?1",
                table_name("task_terminal_strip", shadow)
            ),
            rusqlite::params![task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((order_bytes, focused)) = row else {
        return Ok(Default::default());
    };
    let order: Vec<ResourceId> = rmp_serde::from_slice(&order_bytes).map_err(|_| StoreError::Corruption)?;
    let focused = match focused {
        Some(bytes) => Some(ResourceId::from_bytes(
            bytes.as_slice().try_into().map_err(|_| StoreError::Corruption)?,
        )),
        None => None,
    };
    Ok(crate::domain::terminal_facts::TaskTerminalStrip { order, focused })
}

fn write_terminal_strip(
    tx: &Transaction<'_>,
    shadow: bool,
    task_id: TaskId,
    strip: &crate::domain::terminal_facts::TaskTerminalStrip,
) -> Result<(), StoreError> {
    tx.execute(
        &format!(
            "INSERT INTO {} (task_id, order_msgpack, focused_resource_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT(task_id) DO UPDATE SET order_msgpack = excluded.order_msgpack, focused_resource_id = excluded.focused_resource_id",
            table_name("task_terminal_strip", shadow)
        ),
        rusqlite::params![
            task_id.as_bytes().as_slice(),
            pack(&strip.order)?,
            strip.focused.map(|id| id.as_bytes().to_vec()),
        ],
    )?;
    Ok(())
}

fn append_to_terminal_strip(tx: &Transaction<'_>, shadow: bool, task_id: TaskId, resource_id: ResourceId) -> Result<(), StoreError> {
    let mut strip = read_terminal_strip(tx, shadow, task_id)?;
    if !strip.order.contains(&resource_id) {
        strip.order.push(resource_id);
    }
    if strip.focused.is_none() {
        strip.focused = Some(resource_id);
    }
    write_terminal_strip(tx, shadow, task_id, &strip)
}

fn remove_from_terminal_strip(tx: &Transaction<'_>, shadow: bool, task_id: TaskId, resource_id: ResourceId) -> Result<(), StoreError> {
    let mut strip = read_terminal_strip(tx, shadow, task_id)?;
    strip.remove(resource_id);
    write_terminal_strip(tx, shadow, task_id, &strip)
}
```

Snapshot loading in `src/kernel/command_bus.rs`: add two loaders beside `load_resources` (`command_bus.rs:9253-9283`), then in `load_task_snapshot` (`command_bus.rs:8747-8839`) call them with the same `StoreError::Projection(format!("task {task_id} terminal projection is invalid: {error}"))` wrapping the other loaders use, and set `terminal_facts` / `terminal_strip` on the returned `TaskSnapshot`:

```rust
fn load_terminal_facts(
    conn: &Connection,
    task_id: TaskId,
) -> Result<BTreeMap<ResourceId, crate::domain::terminal_facts::TerminalFacts>, StoreError> {
        let mut terminal_facts = BTreeMap::new();
        let mut statement = conn.prepare(
            "SELECT resource_id, title, live_cwd, exit_code, exit_summary, exited_at_ms, created_at_ms, last_activity_at_ms \
             FROM terminal_facts WHERE task_id = ?1 ORDER BY resource_id ASC",
        )?;
        let rows = statement.query_map([task_id.as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i32>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        for row in rows {
            let (id_bytes, title, live_cwd, exit_code, exit_summary, exited_at_ms, created_at_ms, last_activity_at_ms) = row?;
            let resource_id = id16::<ResourceId>("terminal_facts.resource_id", &id_bytes)?;
            let exit = match (exit_summary, exited_at_ms) {
                (Some(summary), Some(at_ms)) => Some(crate::domain::terminal_facts::TerminalExit { code: exit_code, summary, at_ms }),
                _ => None,
            };
            terminal_facts.insert(
                resource_id,
                crate::domain::terminal_facts::TerminalFacts {
                    resource_id,
                    title,
                    live_cwd: live_cwd.map(std::path::PathBuf::from),
                    exit,
                    created_at_ms,
                    last_activity_at_ms,
                },
            );
        }
        Ok(terminal_facts)
}

fn load_terminal_strip(
    conn: &Connection,
    task_id: TaskId,
) -> Result<crate::domain::terminal_facts::TaskTerminalStrip, StoreError> {
    let row: Option<(Vec<u8>, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT order_msgpack, focused_resource_id FROM task_terminal_strip WHERE task_id = ?1",
            [task_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((order_bytes, focused)) = row else {
        return Ok(Default::default());
    };
    let order: Vec<ResourceId> = unpack_projection_blob("task_terminal_strip.order_msgpack", &order_bytes)?;
    let focused = match focused {
        Some(bytes) => Some(id16::<ResourceId>("task_terminal_strip.focused_resource_id", &bytes)?),
        None => None,
    };
    Ok(crate::domain::terminal_facts::TaskTerminalStrip { order, focused })
}
```

`id16` and `unpack_projection_blob` are the existing helpers in this file (`command_bus.rs:10395-10433`). Because `unpack_projection_blob` requires a byte-lossless re-encode, the projector must write `order_msgpack` with the same `projector::pack(&strip.order)` used here (it does, via `write_terminal_strip`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib --locked kernel::schema:: kernel::command_bus::tests::open_rename_strip kernel::command_bus::tests::ninth_shell kernel::command_bus::tests::client_cannot_record -- --test-threads=1`
Expected: PASS.

Also run the existing projector/replay suites: `cargo test --lib --locked kernel:: -- --test-threads=1` and `cargo test --locked --test host_recovery` (a fresh store must migrate V1..V16 and a replayed store must produce identical snapshots).

- [ ] **Step 5: Commit**

```bash
git add src/kernel/schema.rs src/kernel/projector.rs src/kernel/store.rs src/kernel/command_bus.rs
git commit -m "feat(kernel): project terminal facts and strip (V16)

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: TerminalService keyed by resource

**Files:**
- Modify: `src/terminal/service.rs:193-224` (`HostedTerminal`), `:1085-1115` (`attach_bound_task_runtime`), `:1505-1605` (view/scroll/resize), plus new methods
- Test: `tests/terminal_service.rs`

**Interfaces:**
- Produces:

```rust
pub enum TerminalRuntimeState { Running, Exited { summary: String }, Unknown }
impl TerminalService {
    pub fn attach_plain_shell(&self, owner: TaskId, resource_id: ResourceId, resource_generation: u64, spec: TerminalSpec, runtime: Arc<dyn AttachedTerminalRuntime>) -> Result<TerminalId, TerminalError>;
    pub fn task_terminal_view_for(&self, task_id: TaskId, resource_id: Option<ResourceId>) -> Result<Option<TaskTerminalView>, TerminalError>;
    pub fn scroll_task_terminal_for(&self, task_id: TaskId, resource_id: Option<ResourceId>, delta_lines: i32) -> Result<(), TerminalError>;
    pub fn resize_task_terminal_for(&self, task_id: TaskId, resource_id: Option<ResourceId>, size: TerminalSize) -> Result<(), TerminalError>;
    pub fn task_terminal_summaries(&self, task_id: TaskId) -> Result<Vec<TerminalRuntimeSummary>, TerminalError>;
    pub fn mark_unknown(&self, owner: TaskId, resource_id: ResourceId, resource_generation: u64) -> Result<(), TerminalError>; // used by spec 2
}
pub struct TerminalRuntimeSummary { pub resource_id: ResourceId, pub terminal_id: Option<TerminalId>, pub state: TerminalRuntimeState, pub is_provider: bool, pub sequence: u64 }
```
- `task_terminal_view(task_id)` becomes `task_terminal_view_for(task_id, None)` and keeps its exact behaviour when only a provider terminal exists. `task_terminal_view_for(task, None)` selects the terminal whose `agent_session_id.is_some()`; with `Some(id)` it selects by `resource_id`.
- `TaskTerminalView` gains `pub is_provider: bool` and `pub runtime_state: TerminalRuntimeState`; `agent_session_id`, `runtime_generation`, `action_epoch` become `Option<_>` (None for plain shells). Every current reader unwraps them only on the provider path (`host/cockpit.rs`, `host/connection.rs`).

- [ ] **Step 1: Write the failing test**

Append to `tests/terminal_service.rs` (reuse its existing fixture helpers for a fake `AttachedTerminalRuntime`; the file already has a runtime with `inject_reader_eof`):

```rust
#[test]
fn provider_and_two_shells_coexist_on_one_task() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let provider_runtime = fixture_runtime(); // existing helper name in this file
    let provider_spec = TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24).unwrap()).unwrap();
    let provider_id = service
        .attach_bound_task_runtime(task_id, provider_spec, provider_runtime, AgentSessionId::new(), 1, 1)
        .expect("provider attach");

    let shell_a = ResourceId::new();
    let shell_b = ResourceId::new();
    let a_id = service
        .attach_plain_shell(task_id, shell_a, 1, TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24).unwrap()).unwrap(), fixture_runtime())
        .expect("shell a");
    let b_id = service
        .attach_plain_shell(task_id, shell_b, 1, TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24).unwrap()).unwrap(), fixture_runtime())
        .expect("shell b");
    assert_ne!(a_id, b_id);

    // Default selection still returns the provider terminal.
    let view = service.task_terminal_view_for(task_id, None).expect("view").expect("present");
    assert_eq!(view.terminal_id, provider_id);
    assert!(view.is_provider);

    let view_a = service.task_terminal_view_for(task_id, Some(shell_a)).expect("view").expect("present");
    assert_eq!(view_a.terminal_id, a_id);
    assert!(!view_a.is_provider);
    assert_eq!(view_a.runtime_state, TerminalRuntimeState::Running);

    let summaries = service.task_terminal_summaries(task_id).expect("summaries");
    assert_eq!(summaries.len(), 3);
    assert_eq!(summaries.iter().filter(|s| s.is_provider).count(), 1);

    service.scroll_task_terminal_for(task_id, Some(shell_b), 3).expect("scroll b");
    service.resize_task_terminal_for(task_id, Some(shell_b), TerminalSize::new(100, 30).unwrap()).expect("resize b");
    let view_b = service.task_terminal_view_for(task_id, Some(shell_b)).expect("view").expect("present");
    assert_eq!(view_b.view.screen.cols, 100);
}

#[test]
fn second_plain_shell_for_same_resource_is_rejected() {
    let service = TerminalService::default();
    let task_id = TaskId::new();
    let resource_id = ResourceId::new();
    let spec = || TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24).unwrap()).unwrap();
    service.attach_plain_shell(task_id, resource_id, 1, spec(), fixture_runtime()).expect("first");
    let second = service.attach_plain_shell(task_id, resource_id, 1, spec(), fixture_runtime());
    assert!(matches!(second, Err(TerminalError::InvalidFence)));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --locked --test terminal_service provider_and_two_shells -- --nocapture`
Expected: compile error (`attach_plain_shell`, `task_terminal_view_for` undefined).

- [ ] **Step 3: Implement**

In `src/terminal/service.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalRuntimeState {
    Running,
    Exited { summary: String },
    /// Boot window only: durable terminal not yet reconciled with a runtime.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRuntimeSummary {
    pub resource_id: ResourceId,
    pub terminal_id: Option<TerminalId>,
    pub state: TerminalRuntimeState,
    pub is_provider: bool,
    pub sequence: u64,
}
```

Add to `HostedTerminal`: `unknown: bool` (default false). Add to `TaskTerminalView`: `pub is_provider: bool, pub runtime_state: TerminalRuntimeState`, and change `agent_session_id: Option<AgentSessionId>`, `runtime_generation: Option<u64>`, `action_epoch: Option<u64>`.

Replace the duplicate-owner guard in `attach_bound_task_runtime`:

```rust
        if terminals
            .values()
            .any(|current| current.task_id == owner && !current.closed && current.agent_session_id.is_some())
        {
            return Err(TerminalError::InvalidFence);
        }
```

Add:

```rust
    pub fn attach_plain_shell(
        &self,
        owner: TaskId,
        resource_id: ResourceId,
        resource_generation: u64,
        spec: TerminalSpec,
        runtime: Arc<dyn AttachedTerminalRuntime>,
    ) -> Result<TerminalId, TerminalError> {
        let spec = spec.validated()?;
        let terminal_id = TerminalId::new();
        let mut hosted = HostedTerminal::open_attached(owner, spec, terminal_id, runtime)?;
        hosted.resource_id = resource_id;
        hosted.generation = TerminalGeneration::from_raw(resource_generation)?;
        let mut terminals = self.lock()?;
        if terminals
            .values()
            .any(|current| current.resource_id == resource_id && !current.closed)
        {
            return Err(TerminalError::InvalidFence);
        }
        terminals.insert(terminal_id, hosted);
        Ok(terminal_id)
    }

    fn select_terminal(
        terminals: &HashMap<TerminalId, HostedTerminal>,
        task_id: TaskId,
        resource_id: Option<ResourceId>,
    ) -> Result<Option<TerminalId>, TerminalError> {
        let matching = terminals
            .iter()
            .filter(|(_, hosted)| hosted.task_id == task_id && !hosted.closed)
            .filter(|(_, hosted)| match resource_id {
                Some(id) => hosted.resource_id == id,
                None => hosted.agent_session_id.is_some(),
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            _ => Err(TerminalError::InvalidFence),
        }
    }

    pub fn task_terminal_view_for(
        &self,
        task_id: TaskId,
        resource_id: Option<ResourceId>,
    ) -> Result<Option<TaskTerminalView>, TerminalError> {
        let mut terminals = self.lock()?;
        let Some(terminal_id) = Self::select_terminal(&terminals, task_id, resource_id)? else {
            return Ok(None);
        };
        let hosted = terminals.get_mut(&terminal_id).ok_or(TerminalError::NotFound)?;
        hosted.ensure_open()?;
        hosted.drain_attached_output(terminal_id)?;
        if hosted.sequence == TerminalSequence::ZERO {
            hosted.bump_sequence()?;
        }
        let is_provider = hosted.agent_session_id.is_some();
        if is_provider {
            hosted.runtime_generation.ok_or(TerminalError::InvalidFence)?;
            hosted.action_epoch.ok_or(TerminalError::InvalidFence)?;
        }
        let runtime_state = if hosted.unknown {
            TerminalRuntimeState::Unknown
        } else if let Some(summary) = hosted.exit_summary.clone() {
            TerminalRuntimeState::Exited { summary }
        } else {
            TerminalRuntimeState::Running
        };
        let view = hosted.session_view()?;
        Ok(Some(TaskTerminalView {
            task_id,
            terminal_id,
            session_id: hosted.session_id,
            agent_session_id: hosted.agent_session_id,
            runtime_generation: hosted.runtime_generation,
            action_epoch: hosted.action_epoch,
            focus_epoch: hosted.focus_epoch,
            resource_id: hosted.resource_id,
            resource_generation: hosted.generation.get(),
            accepted_input_sequence: hosted.accepted_input_sequence,
            sequence: hosted.sequence.get(),
            is_provider,
            runtime_state,
            view,
        }))
    }

    pub fn task_terminal_view(&self, task_id: TaskId) -> Result<Option<TaskTerminalView>, TerminalError> {
        self.task_terminal_view_for(task_id, None)
    }

    pub fn task_terminal_summaries(&self, task_id: TaskId) -> Result<Vec<TerminalRuntimeSummary>, TerminalError> {
        let terminals = self.lock()?;
        let mut out = terminals
            .iter()
            .filter(|(_, hosted)| hosted.task_id == task_id && !hosted.closed)
            .map(|(id, hosted)| TerminalRuntimeSummary {
                resource_id: hosted.resource_id,
                terminal_id: Some(*id),
                state: if hosted.unknown {
                    TerminalRuntimeState::Unknown
                } else if let Some(summary) = hosted.exit_summary.clone() {
                    TerminalRuntimeState::Exited { summary }
                } else {
                    TerminalRuntimeState::Running
                },
                is_provider: hosted.agent_session_id.is_some(),
                sequence: hosted.sequence.get(),
            })
            .collect::<Vec<_>>();
        out.sort_by_key(|s| (!s.is_provider, s.resource_id));
        Ok(out)
    }
```

Rewrite `scroll_task_terminal` and `resize_task_terminal` as thin wrappers over `scroll_task_terminal_for(task_id, None, ..)` / `resize_task_terminal_for(task_id, None, ..)`, whose bodies replace the `matching`/`let [terminal_id]` block with `Self::select_terminal(&terminals, task_id, resource_id)?.ok_or(TerminalError::NotFound)?` and keep the rest verbatim.

Fix the provider readers: in `src/host/cockpit.rs::serve_task_terminal` and `src/host/connection.rs::attach_provider_terminal`, replace direct field reads with `terminal.agent_session_id.ok_or(...)`-style unwraps on the provider path only (these paths call `task_terminal_view`, which still selects the provider terminal). Use `TerminalError::InvalidFence` / `denied(.., StaleFence)` for a `None` there.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --locked --test terminal_service` and `cargo check --locked --lib --bins --tests`
Expected: all terminal_service tests pass (7 existing + 2 new); EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/terminal/service.rs src/host/cockpit.rs src/host/connection.rs tests/terminal_service.rs
git commit -m "feat(terminal): key TerminalService by resource and add plain-shell attach

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: Host shell spawn and cwd/exit/activity fact pump

**Files:**
- Modify: `src/services/process_manager.rs` (near `spawn_shell_session` ~3062 and `issue_exact_provider_terminal_authority` ~3258), `src/host/connection.rs` (execution of `OpenShellTerminal`, reaper tick pump)
- Test: `src/host/connection.rs` tests (unit, with the in-process bus) and `tests/task_shell_terminals.rs` (process)

**Interfaces:**
- Produces in `ProcessManager`:

```rust
pub fn spawn_task_shell_session(
    &self,
    task_id: TaskId,
    resource_id: ResourceId,
    resource_generation: u64,
    action_epoch: u64,
    launch: &crate::domain::resource::TerminalLaunch,
    dimensions: SessionDimensions,
) -> Result<TerminalSessionId, String>;                       // session id is "shell-<TerminalSessionId>"
pub fn task_shell_runtime(&self, session_id: TerminalSessionId) -> Result<Arc<TerminalSession>, String>;
pub fn shell_session_cwd(&self, session_id: TerminalSessionId) -> Option<PathBuf>;   // from SessionRuntimeState.cwd
pub fn shell_session_exit(&self, session_id: TerminalSessionId) -> Option<(Option<i32>, String)>;
```
- Produces in `HostRequestExecutor`: `fn open_shell_terminal_after_accept(&mut self, task_id: TaskId, resource_id: ResourceId)` called from the command-acceptance path when `Command::OpenShellTerminal` is accepted; `shell_sessions: HashMap<ResourceId, ShellSessionLink { task_id, session_id, last_cwd: Option<PathBuf>, last_cwd_at: Instant, exit_recorded: bool }>`; `fn pump_shell_terminal_facts(&mut self)` called from both reaper ticks beside `queue_one_provider_restore()`.

- [ ] **Step 1: Write the failing tests**

Add a Windows process test `tests/task_shell_terminals.rs` that drives the manager directly (no host):

```rust
#![cfg(windows)]

use devmanager::domain::resource::TerminalLaunch;
use devmanager::domain::{ResourceId, TaskId};
use devmanager::services::ProcessManager;
use devmanager::state::runtime_state::SessionDimensions;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn cmd_launch(cwd: PathBuf) -> TerminalLaunch {
    TerminalLaunch { cwd, program: PathBuf::from(r"C:\Windows\System32\cmd.exe"), args: vec!["/Q".to_string()] }
}

#[test]
fn task_shell_spawns_reports_cwd_and_exit() {
    let manager = ProcessManager::for_test(); // existing test constructor used by tests/process_supervisor.rs; use that exact name
    let workdir = tempfile::tempdir().expect("workdir");
    let task_id = TaskId::new();
    let resource_id = ResourceId::new();
    let session_id = manager
        .spawn_task_shell_session(task_id, resource_id, 1, 1, &cmd_launch(workdir.path().to_path_buf()),
            SessionDimensions { cols: 100, rows: 30, cell_width: 8, cell_height: 16 })
        .expect("spawn");

    let runtime = manager.task_shell_runtime(session_id).expect("runtime");
    let sub = workdir.path().join("sub");
    std::fs::create_dir(&sub).expect("mkdir");
    runtime.write_input(format!("cd sub\r\n").as_bytes()).expect("cd"); // use the session's existing input method name
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if manager.shell_session_cwd(session_id).as_deref() == Some(sub.as_path()) { break; }
        assert!(Instant::now() < deadline, "cwd never reported: {:?}", manager.shell_session_cwd(session_id));
        std::thread::sleep(Duration::from_millis(100));
    }

    runtime.write_input(b"exit 3\r\n").expect("exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((code, _summary)) = manager.shell_session_exit(session_id) { assert_eq!(code, Some(3)); break; }
        assert!(Instant::now() < deadline, "exit never observed");
        std::thread::sleep(Duration::from_millis(100));
    }
}
```

Read `tests/process_supervisor.rs` for the manager constructor and the session input method actually exported (`write_input` / `send_input`) and use those names.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --locked --test task_shell_terminals -- --nocapture`
Expected: compile error.

- [ ] **Step 3: Implement the manager side**

In `src/services/process_manager.rs`, beside `issue_exact_provider_terminal_authority`, add a task-owned authority issuer with the same body but `ProcessOwner::Task(task_id)` and the given `resource_id`/`generation`/`action_epoch`:

```rust
    fn issue_task_shell_terminal_authority(
        &self,
        session_id: &str,
        task_id: TaskId,
        resource_id: ResourceId,
        generation: u64,
        action_epoch: u64,
    ) -> Result<TerminalLaunchAuthority, String> {
        // identical to issue_exact_provider_terminal_authority except the owner
        // is ProcessOwner::Task(task_id) and ports are empty
        ...
    }

    pub fn spawn_task_shell_session(
        &self,
        task_id: TaskId,
        resource_id: ResourceId,
        resource_generation: u64,
        action_epoch: u64,
        launch: &crate::domain::resource::TerminalLaunch,
        dimensions: SessionDimensions,
    ) -> Result<TerminalSessionId, String> {
        let terminal_session_id = TerminalSessionId::new();
        let session_id = format!("shell-{terminal_session_id}");
        ensure_prior_session_teardown_settled(&self.inner, &session_id, Duration::from_secs(2))?;
        let authority = self.issue_task_shell_terminal_authority(&session_id, task_id, resource_id, resource_generation, action_epoch)?;
        let session = TerminalSession::spawn_command(
            session_id.clone(),
            launch.cwd.clone(),
            dimensions,
            launch.program.to_string_lossy().into_owned(),
            launch.args.clone(),
            HashMap::new(),
            self.log_buffer_size(),
            None,
            self.inner.runtime_state.clone(),
            self.inner.debug_enabled,
            Some(session_change_notifier(self.inner.clone(), session_id.clone())),
            Some(session_output_notifier(self.inner.clone(), session_id.clone())),
            authority,
        )?;
        self.inner
            .sessions
            .lock()
            .map_err(|_| "Session store poisoned".to_string())?
            .insert(session_id, Arc::new(session));
        Ok(terminal_session_id)
    }

    pub fn task_shell_runtime(&self, session_id: TerminalSessionId) -> Result<Arc<TerminalSession>, String> {
        let key = format!("shell-{session_id}");
        self.inner.sessions.lock().map_err(|_| "Session store poisoned".to_string())?
            .get(&key).cloned().ok_or_else(|| format!("shell session {key} is not live"))
    }

    pub fn shell_session_cwd(&self, session_id: TerminalSessionId) -> Option<PathBuf> {
        let key = format!("shell-{session_id}");
        self.inner.runtime_state.read().ok()?.sessions.get(&key)?.cwd.clone()
    }

    pub fn shell_session_exit(&self, session_id: TerminalSessionId) -> Option<(Option<i32>, String)> {
        let key = format!("shell-{session_id}");
        let runtime = self.inner.runtime_state.read().ok()?;
        let session = runtime.sessions.get(&key)?;
        session.exit.as_ref().map(|exit| (exit.code, exit.summary.clone()))
    }
```

`SessionRuntimeState.cwd` is already maintained by the shell-sequence parser (`ShellSequenceParser` in `apply_terminal_output_chunk`, OSC 7 / 9;9). Confirm with `git grep -n "cwd" src/terminal/session.rs | grep -i "osc\|shell_sequences\|reported"`; if the parser sets a different field, read that field here. If PowerShell candidates do not already receive the shell-integration prompt hook when `shell_integration_enabled`, add it in `shell_candidates` following the existing pwsh args pattern (this is a separate commit if needed).

Windows PEB fallback for `cmd.exe` (rung 2): add `pub fn root_process_cwd(pid: u32) -> Option<PathBuf>` in `src/services/platform_service.rs` that opens the process with `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`, calls `NtQueryInformationProcess(ProcessBasicInformation)` for the PEB address, reads `ProcessParameters->CurrentDirectory.DosPath` (`UNICODE_STRING`), and returns it only if `is_absolute() && is_dir()`. Guard every read with `ReadProcessMemory` length checks; return `None` on any failure. `shell_session_cwd` falls back to `root_process_cwd(session.pid)` when the parser has not reported.

- [ ] **Step 4: Implement the host side**

In `src/host/connection.rs`:

```rust
struct ShellSessionLink {
    task_id: TaskId,
    session_id: crate::terminal::protocol::TerminalSessionId,
    last_cwd: Option<std::path::PathBuf>,
    last_cwd_change: Option<Instant>,
    exit_recorded: bool,
}
```

Add `shell_sessions: HashMap<ResourceId, ShellSessionLink>` to `HostRequestExecutor` and initialize `HashMap::new()` in both constructors.

Where the executor turns an accepted `CommandEnvelope` into effects (the same place that handles `Command::StartProviderSession` acceptance), add:

```rust
            Command::OpenShellTerminal(intent) => {
                self.open_shell_terminal_after_accept(intent.resource.task_id.expect("task-owned"), intent.resource.id);
            }
            Command::CloseTerminal { resource_id } => {
                self.close_shell_terminal(*resource_id);
            }
```

```rust
    fn open_shell_terminal_after_accept(&mut self, task_id: TaskId, resource_id: ResourceId) {
        let Some(runtime) = self.configured_service_runtime.as_ref() else { return; };
        let Ok(Some(snapshot)) = self.bus.task_snapshot(task_id) else { return; };
        let Some(resource) = snapshot.resources.get(&resource_id) else { return; };
        let (cols, rows, launch) = match &resource.recipe {
            ResourceRecipe::Terminal { cols, rows, launch: Some(launch), .. } => (*cols, *rows, launch.clone()),
            _ => return,
        };
        let dimensions = SessionDimensions { cols, rows, cell_width: 8, cell_height: 16 };
        match runtime.manager.spawn_task_shell_session(
            task_id, resource_id, resource.runtime_generation, snapshot.task.action_epoch, &launch, dimensions,
        ) {
            Ok(session_id) => {
                let attached = match runtime.manager.task_shell_runtime(session_id) {
                    Ok(session) => session,
                    Err(error) => { self.record_shell_exit(task_id, resource_id, None, format!("spawn failed: {error}")); return; }
                };
                let spec = match TerminalSpec::new(session_id, TerminalSize::new(cols, rows).unwrap_or_default()) {
                    Ok(spec) => spec,
                    Err(error) => { self.record_shell_exit(task_id, resource_id, None, format!("spec invalid: {error}")); return; }
                };
                if let Err(error) = self.terminal_service.attach_plain_shell(task_id, resource_id, resource.runtime_generation, spec, attached) {
                    self.record_shell_exit(task_id, resource_id, None, format!("attach failed: {error}"));
                    return;
                }
                self.shell_sessions.insert(resource_id, ShellSessionLink {
                    task_id, session_id, last_cwd: None, last_cwd_change: None, exit_recorded: false,
                });
            }
            Err(error) => self.record_shell_exit(task_id, resource_id, None, format!("spawn failed: {error}")),
        }
    }

    fn record_shell_exit(&mut self, task_id: TaskId, resource_id: ResourceId, code: Option<i32>, summary: String) {
        let _ = self.execute_host_fact(task_id, Command::RecordTerminalExit { resource_id, code, summary });
    }

    /// Host-authorized fact write with no revision fence.
    fn execute_host_fact(&mut self, task_id: TaskId, command: Command) -> Result<(), String> {
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id: self.host_client_id(),          // existing host-side client id accessor
            task_id: Some(task_id),
            issued_at_ms: crate::domain::now_ms(),      // existing clock helper used elsewhere in this file
            expected_task_revision: None,
            command,
        };
        self.bus
            .execute_host_authorized(envelope, None, RequestId::new(), Uuid::nil())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn pump_shell_terminal_facts(&mut self) {
        let Some(runtime) = self.configured_service_runtime.as_ref().map(|r| r.manager.clone()) else { return; };
        let now = Instant::now();
        let resource_ids = self.shell_sessions.keys().copied().collect::<Vec<_>>();
        for resource_id in resource_ids {
            let Some(link) = self.shell_sessions.get(&resource_id) else { continue; };
            let task_id = link.task_id;
            let session_id = link.session_id;
            if !link.exit_recorded {
                if let Some((code, summary)) = runtime.shell_session_exit(session_id) {
                    self.record_shell_exit(task_id, resource_id, code, summary);
                    if let Some(link) = self.shell_sessions.get_mut(&resource_id) { link.exit_recorded = true; }
                    continue;
                }
            }
            let observed = runtime.shell_session_cwd(session_id);
            let Some(link) = self.shell_sessions.get_mut(&resource_id) else { continue; };
            if observed != link.last_cwd {
                link.last_cwd = observed.clone();
                link.last_cwd_change = Some(now);
            }
            let debounced = link.last_cwd_change.is_some_and(|at| now.duration_since(at) >= Duration::from_millis(2_000));
            if debounced {
                link.last_cwd_change = None;
                if let Some(cwd) = observed {
                    let _ = self.execute_host_fact(task_id, Command::RecordTerminalCwd { resource_id, cwd });
                }
            }
            let _ = self.execute_host_fact(task_id, Command::RecordTerminalActivity { resource_id });
        }
    }

    fn close_shell_terminal(&mut self, resource_id: ResourceId) {
        if let Some(link) = self.shell_sessions.remove(&resource_id) {
            if let Some(runtime) = self.configured_service_runtime.as_ref() {
                let _ = runtime.manager.close_session(&format!("shell-{}", link.session_id)); // existing close by session id
            }
        }
    }
```

`RecordTerminalActivity` coalescing happens in `decide` (30 s), so calling it every tick is cheap; the pump runs at the reaper cadence. Add `self.pump_shell_terminal_facts();` immediately after `self.queue_one_provider_restore();` in both reaper-tick arms (~3143 and ~3225).

The `ResourceReleased` completion for a closed shell already flows through the existing teardown journal; `close_shell_terminal` only asks the manager to close, the release facts land via the existing resource release path.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --locked --test task_shell_terminals -- --nocapture` and `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 6: Commit**

```bash
git add src/services/process_manager.rs src/services/platform_service.rs src/host/connection.rs tests/task_shell_terminals.rs
git commit -m "feat(host): spawn task shells and record cwd, exit, and activity facts

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: Cockpit queries with `resource_id` and the `TaskTerminals` strip query

**Files:**
- Modify: `src/domain/cockpit.rs:316-331` (queries), `:641-668` (`TaskTerminalProjection`), `TaskCockpitResult` (new variant), `src/host/cockpit.rs:246-280,564-727`, `src/client/action.rs:675-678` (`cockpit_query_action_id`)
- Test: `src/host/cockpit.rs` tests

**Interfaces:**
- `TaskCockpitQuery::Terminal { #[serde(default)] resource_id: Option<ResourceId> }` (was unit), `TerminalScroll { delta_lines, #[serde(default)] resource_id }`, `TerminalResize { cols, rows, #[serde(default)] resource_id }`, `TerminalReadiness { #[serde(default)] resource_id }`, new `TaskTerminals`.
- `TaskTerminalProjection` gains `#[serde(default)] pub is_provider: bool` and `#[serde(default)] pub runtime_state: TerminalRuntimeStateWire` (`Running | Exited { summary } | Unknown`, default Running); `agent_session_id`, `runtime_generation`, `action_epoch` stay non-optional for the wire: plain shells send `AgentSessionId::nil()`-style zero values only if the wire type forbids Option; prefer making them `Option<_>` with `#[serde(default)]` and update the client readers (`native_shell.rs:2753-2783` `terminal_input_request` must reject `None` on the provider path and use zeros for shells).
- New `TaskCockpitResult::TaskTerminals(TaskTerminalsProjection)`:

```rust
pub struct TaskTerminalsProjection {
    pub task_id: TaskId,
    pub terminals: Vec<TaskTerminalChip>,
    pub order: Vec<ResourceId>,
    pub focused: Option<ResourceId>,
}
pub struct TaskTerminalChip {
    pub resource_id: ResourceId,
    pub is_provider: bool,
    pub title: Option<String>,
    pub label: String,
    pub runtime_state: TerminalRuntimeStateWire,
    pub live_cwd: Option<String>,
    pub exit: Option<crate::domain::terminal_facts::TerminalExit>,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}
```

- [ ] **Step 1: Write the failing test**

In `src/host/cockpit.rs` tests (there is an existing fixture that builds a `TaskCockpitDispatch` with a `TerminalService`; reuse it):

```rust
    #[test]
    fn task_terminals_query_lists_provider_first_then_strip_order() {
        let (dispatch, task_id, snapshot, service) = cockpit_fixture_with_terminal_service(); // existing fixture, adapt name
        let shell_a = ResourceId::new();
        let shell_b = ResourceId::new();
        // register two plain shells into the snapshot and attach them to the service
        let mut snapshot = snapshot;
        for id in [shell_a, shell_b] {
            snapshot.terminal_facts.insert(id, TerminalFacts::registered(id, None, 1));
            snapshot.terminal_strip.order.push(id);
            service
                .attach_plain_shell(task_id, id, 1, TerminalSpec::new(TerminalSessionId::new(), TerminalSize::new(80, 24).unwrap()).unwrap(), fixture_runtime())
                .unwrap();
        }
        snapshot.terminal_strip.order = vec![shell_b, shell_a];
        snapshot.terminal_strip.focused = Some(shell_a);
        let outcome = serve_task_terminals(&dispatch, task_id, &snapshot);
        let QueryOutcome::Result(TaskCockpitResult::TaskTerminals(projection)) = outcome else { panic!("{outcome:?}") };
        assert!(projection.terminals[0].is_provider);
        assert_eq!(projection.terminals[1].resource_id, shell_b);
        assert_eq!(projection.terminals[2].resource_id, shell_a);
        assert_eq!(projection.focused, Some(shell_a));
    }

    #[test]
    fn terminal_query_without_resource_id_still_targets_provider_terminal() {
        let json = r#"{"terminal":{}}"#;
        let query: TaskCockpitQuery = serde_json::from_str(json).expect("legacy query shape");
        assert_eq!(query, TaskCockpitQuery::Terminal { resource_id: None });
        let legacy_unit = r#""terminal""#;
        assert!(serde_json::from_str::<TaskCockpitQuery>(legacy_unit).is_ok(), "unit form must still decode");
    }
```

If the unit form cannot decode after the change, keep the `Terminal` variant unit and add a new `TerminalFor { resource_id }` variant instead; then the same applies to scroll/resize/readiness (`TerminalScrollFor`, `TerminalResizeFor`, `TerminalReadinessFor`). Choose whichever keeps the legacy wire decoding and update the test accordingly.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked host::cockpit::tests::task_terminals -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

Domain: add the fields/variants above. Wire enum for runtime state:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalRuntimeStateWire {
    #[default]
    Running,
    Exited { summary: String },
    Unknown,
}
```

Host `src/host/cockpit.rs`: thread `resource_id` into `serve_task_terminal` and the scroll/resize arms (`service.scroll_task_terminal_for(task_id, resource_id, delta)`, `resize_task_terminal_for`), call `service.task_terminal_view_for(task_id, resource_id)`. On the provider path (view.is_provider) keep every existing fence check; on the shell path require only `resource.lifecycle == Active && resource.runtime_generation == view.resource_generation`, and skip `Capability::ProviderInput` (grant `ReadWrite` on the terminal directly). Fill `is_provider` and `runtime_state`.

Add:

```rust
fn serve_task_terminals(
    dispatch: &TaskCockpitDispatch<'_>,
    task_id: TaskId,
    snapshot: &crate::domain::TaskSnapshot,
) -> QueryOutcome {
    let Some(service) = dispatch.terminal_service else {
        return unavailable(TaskCockpitSurface::Terminal, TaskCockpitUnavailableReason::TerminalUnavailable);
    };
    let summaries = match service.task_terminal_summaries(task_id) {
        Ok(s) => s,
        Err(_) => return denied(TaskCockpitSurface::Terminal, TaskCockpitDeniedReason::StaleFence),
    };
    let live_by_resource = summaries.iter().map(|s| (s.resource_id, s)).collect::<HashMap<_, _>>();
    let mut chips = Vec::new();
    // provider first: the Active Terminal resource bound to the primary agent
    let provider_resource = snapshot.primary_agent_id
        .and_then(|agent_id| snapshot.agents.get(&agent_id))
        .and_then(|agent| snapshot.resources.values().find(|r| r.resource_kind == ResourceKind::Terminal && !r.recipe.is_plain_shell() && r.lifecycle == ResourceLifecycle::Active && r.runtime_generation == agent.runtime_generation))
        .map(|r| r.id);
    let chip_for = |resource_id: ResourceId, is_provider: bool| -> Option<TaskTerminalChip> {
        let facts = snapshot.terminal_facts.get(&resource_id);
        let live = live_by_resource.get(&resource_id);
        let label = dispatch.terminal_label(resource_id).unwrap_or_else(|| shell_label_for(snapshot, resource_id));
        Some(TaskTerminalChip {
            resource_id,
            is_provider,
            title: facts.and_then(|f| f.title.clone()),
            label,
            runtime_state: match live.map(|s| &s.state) {
                Some(TerminalRuntimeState::Running) => TerminalRuntimeStateWire::Running,
                Some(TerminalRuntimeState::Exited { summary }) => TerminalRuntimeStateWire::Exited { summary: summary.clone() },
                Some(TerminalRuntimeState::Unknown) | None => TerminalRuntimeStateWire::Unknown,
            },
            live_cwd: facts.and_then(|f| f.live_cwd.as_ref()).map(|p| p.to_string_lossy().into_owned()),
            exit: facts.and_then(|f| f.exit.clone()),
            created_at_ms: facts.map(|f| f.created_at_ms).unwrap_or_default(),
            last_activity_at_ms: facts.map(|f| f.last_activity_at_ms).unwrap_or_default(),
        })
    };
    if let Some(provider) = provider_resource {
        chips.extend(chip_for(provider, true));
    }
    for id in &snapshot.terminal_strip.order {
        chips.extend(chip_for(*id, false));
    }
    QueryOutcome::Result(TaskCockpitResult::TaskTerminals(TaskTerminalsProjection {
        task_id,
        terminals: chips,
        order: snapshot.terminal_strip.order.clone(),
        focused: snapshot.terminal_strip.focused,
    }))
}

fn shell_label_for(snapshot: &crate::domain::TaskSnapshot, resource_id: ResourceId) -> String {
    match snapshot.resources.get(&resource_id).map(|r| &r.recipe) {
        Some(ResourceRecipe::Terminal { launch: Some(launch), .. }) => launch
            .program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shell".to_string()),
        _ => "terminal".to_string(),
    }
}
```

`dispatch.terminal_label(resource_id)` is a new optional closure on `TaskCockpitDispatch` (`Option<&dyn Fn(ResourceId) -> Option<String>>`) that the host wires to the manager's child-command label from the shared process snapshot (the existing foreground label used for the sidebar; if none exists yet for shells, wire `None` now and return the shell stem, and leave the child-command label to the port-ideas item 20). Route `TaskCockpitQuery::TaskTerminals => serve_task_terminals(...)` in the dispatcher and add it to `cockpit_query_action_id` mapping to `ACTION_PROVIDER_TERMINAL_INPUT`'s surface (`TaskCockpitSurface::Terminal`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked host::cockpit:: domain::cockpit:: -- --test-threads=1` and `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0. Also run `cargo test --locked --test host_admission --test cli_client` (wire-shape consumers).

- [ ] **Step 5: Commit**

```bash
git add src/domain/cockpit.rs src/host/cockpit.rs src/client/action.rs src/ui/native_shell.rs
git commit -m "feat(cockpit): address terminals by resource and add TaskTerminals strip query

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: Client actions for shells

**Files:**
- Modify: `src/client/action.rs` (ids ~72-78, catalog ~334-372, `ActionRequest` ~529-566, `id()` ~569-594, factories ~1476-1512, `ActionArgumentSchema`)
- Test: `src/client/action.rs` tests

**Interfaces:**
- Produces ids `terminal.open_shell`, `terminal.close`, `terminal.rename`, `terminal.set_strip`; `ActionRequest::{TerminalOpenShell { task_id, cwd: Option<String> }, TerminalClose { task_id, resource_id }, TerminalRename(TerminalRenameArguments { task_id, resource_id, title }), TerminalSetStrip { task_id, strip: TaskTerminalStrip }}`; factories:

```rust
pub fn terminal_close_command(command_id, client_id, issued_at_ms, expected_task_revision: u64, task_id: TaskId, resource_id: ResourceId) -> CommandEnvelope
pub fn terminal_rename_command(command_id, client_id, issued_at_ms, expected_task_revision: u64, args: TerminalRenameArguments) -> Result<CommandEnvelope, ResourceValidationError>
pub fn terminal_set_strip_command(command_id, client_id, issued_at_ms, expected_task_revision: u64, task_id: TaskId, strip: TaskTerminalStrip) -> CommandEnvelope
```
- `TerminalOpenShell` is not a plain factory: the host resolves cwd and program. It maps to a new `NativeHostCommand::OpenShellTerminal { request_id, task_id, cwd }` handled in the host executor by building `ResourceFacts` (cwd resolution: given, else `load_task_runtime(...).workspace.runtime_working_directory()`; validate `is_dir`; program from `shell_candidates(settings.default_terminal, ..)` first existing candidate resolved with `resolve_terminal_executable`) and executing `Command::OpenShellTerminal` on the client's behalf with the client's `expected_task_revision`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn terminal_actions_are_registered_and_mutating() {
        for id in [ACTION_TERMINAL_OPEN_SHELL, ACTION_TERMINAL_CLOSE, ACTION_TERMINAL_RENAME, ACTION_TERMINAL_SET_STRIP] {
            let descriptor = ActionCatalog::descriptor(id).unwrap_or_else(|| panic!("{id} registered"));
            assert_eq!(descriptor.scope, ActionScope::Task);
            assert_eq!(descriptor.risk, ActionRisk::Mutating);
        }
    }

    #[test]
    fn terminal_rename_command_canonicalizes_title() {
        let envelope = terminal_rename_command(
            CommandId::new(), ClientId::new(), 1, 7,
            TerminalRenameArguments { task_id: TaskId::new(), resource_id: ResourceId::new(), title: "  build ".into() },
        ).expect("rename");
        assert!(matches!(envelope.command, Command::RenameTerminal { ref title, .. } if title == "build"));
        assert_eq!(envelope.expected_task_revision, Some(7));
        assert!(terminal_rename_command(CommandId::new(), ClientId::new(), 1, 7,
            TerminalRenameArguments { task_id: TaskId::new(), resource_id: ResourceId::new(), title: "   ".into() }).is_err());
    }
```

Use the existing catalog lookup name if it is not `ActionCatalog::descriptor` (`git grep -n "fn descriptor" src/client/action.rs`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked client::action::tests::terminal_ -- --test-threads=1`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
pub const ACTION_TERMINAL_OPEN_SHELL: &str = "terminal.open_shell";
pub const ACTION_TERMINAL_CLOSE: &str = "terminal.close";
pub const ACTION_TERMINAL_RENAME: &str = "terminal.rename";
pub const ACTION_TERMINAL_SET_STRIP: &str = "terminal.set_strip";
```

Catalog entries (copy the `ACTION_TASK_RENAME` descriptor shape): titles "Open shell terminal", "Close terminal", "Rename terminal", "Arrange terminals"; keywords `["terminal", "shell", ...]`; `argument_schema` new variants `TerminalOpenShellV1`, `TerminalIdV1`, `TerminalRenameV1`, `TerminalStripV1`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRenameArguments {
    pub task_id: TaskId,
    pub resource_id: ResourceId,
    pub title: String,
}

    // ActionRequest variants
    TerminalOpenShell { task_id: TaskId, cwd: Option<String> },
    TerminalClose { task_id: TaskId, resource_id: ResourceId },
    TerminalRename(TerminalRenameArguments),
    TerminalSetStrip { task_id: TaskId, strip: crate::domain::terminal_facts::TaskTerminalStrip },
```

`id()` arms map each to its constant. Factories:

```rust
pub fn terminal_close_command(command_id: CommandId, client_id: ClientId, issued_at_ms: i64, expected_task_revision: u64, task_id: TaskId, resource_id: ResourceId) -> CommandEnvelope {
    CommandEnvelope { command_id, client_id, task_id: Some(task_id), issued_at_ms, expected_task_revision: Some(expected_task_revision), command: Command::CloseTerminal { resource_id } }
}

pub fn terminal_rename_command(command_id: CommandId, client_id: ClientId, issued_at_ms: i64, expected_task_revision: u64, args: TerminalRenameArguments) -> Result<CommandEnvelope, ResourceValidationError> {
    let trimmed = args.title.trim();
    if trimmed.is_empty() || trimmed.chars().count() > crate::domain::resource::MAX_TERMINAL_TITLE_CHARS {
        return Err(ResourceValidationError::InvalidTerminalTitle);
    }
    Ok(CommandEnvelope { command_id, client_id, task_id: Some(args.task_id), issued_at_ms, expected_task_revision: Some(expected_task_revision),
        command: Command::RenameTerminal { resource_id: args.resource_id, title: trimmed.to_string() } })
}

pub fn terminal_set_strip_command(command_id: CommandId, client_id: ClientId, issued_at_ms: i64, expected_task_revision: u64, task_id: TaskId, strip: crate::domain::terminal_facts::TaskTerminalStrip) -> CommandEnvelope {
    CommandEnvelope { command_id, client_id, task_id: Some(task_id), issued_at_ms, expected_task_revision: Some(expected_task_revision), command: Command::SetTerminalStrip(strip) }
}
```

In `src/ui/native_shell.rs:8760-8800` map the three factory-backed requests to `NativeHostCommand::Mutation(envelope)` (the same path `TaskRename` uses) and `TerminalOpenShell` to a new `NativeHostCommand::OpenShellTerminal { request_id, task_id, cwd, expected_task_revision }`. In `src/host/connection.rs` handle that command: resolve cwd and program as described in Interfaces, build `ResourceFacts::new(Some(task_id), OwnerKind::Task, ResourceKind::Terminal, ResourceRecipe::Terminal { cols: 120, rows: 40, launch: Some(launch), title: None }, now_ms)`, wrap it in `Command::OpenShellTerminal`, and execute it with the client's `expected_task_revision` via the normal mutation path so acceptance triggers `open_shell_terminal_after_accept`. Reject with a named message when cwd is not a directory (`"cwd is not a directory: <path>"`) or no shell candidate resolves (`"no shell found; tried: pwsh, powershell, cmd"`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked client::action:: -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/client/action.rs src/ui/native_shell.rs src/host/connection.rs
git commit -m "feat(client): add shell terminal actions and host open-shell command

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 10: Per-resource terminal surfaces in the client registry and dock

**Files:**
- Modify: `src/ui/task_workspace/surfaces.rs:267-275,712-740`, `src/ui/task_cockpit/dock.rs:580-618,1063-1112,1681-1694`, `src/ui/native_shell.rs` (`admit_owner_terminal_projection` 16145-16230, `pending_terminal_*` maps 10318-10340, `dispatch_provider_terminal_text` 20282-20385, `terminal_input_request` 2753-2783, `request_terminal_resize_for_owner` 25230-25269)
- Test: `src/ui/task_workspace/surfaces.rs` tests, `src/ui/task_cockpit/dock.rs` tests

**Interfaces:**
- `TaskSurfaceState` gains `pub terminals: BTreeMap<ResourceId, TaskTerminalProjection>` and `pub strip: Option<TaskTerminalsProjection>`; `latest_terminal` becomes a method `fn latest_terminal(&self) -> Option<&TaskTerminalProjection>` returning the focused terminal's projection (the strip's `focused`, else the provider). `admit_terminal` inserts by `projection.resource_id`; new `admit_terminals(task_id, &TaskTerminalsProjection)`.
- `RememberedDockState` keeps its single terminal memory but the dock's `admit_task_terminal_projection` only admits the projection whose `resource_id` equals the current strip focus (so the grid shows the focused chip). `ContextDock::set_focused_terminal(resource_id: Option<ResourceId>)` resets the memory when focus changes.
- All `HostTaskKey`-keyed pending maps for terminals become keyed by `(HostTaskKey, ResourceId)`; introduce `type TerminalKey = (HostTaskKey, ResourceId);`.

- [ ] **Step 1: Write the failing tests**

In `src/ui/task_workspace/surfaces.rs` tests:

```rust
    #[test]
    fn registry_holds_one_projection_per_terminal_resource() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let a = terminal_projection_fixture(task_id, ResourceId::new()); // existing fixture builder, adapt
        let b = terminal_projection_fixture(task_id, ResourceId::new());
        registry.admit_terminal(task_id, &a).unwrap();
        registry.admit_terminal(task_id, &b).unwrap();
        let state = registry.state(task_id).unwrap();
        assert_eq!(state.terminals.len(), 2);
        assert!(state.terminals.contains_key(&a.resource_id));
        assert!(state.terminals.contains_key(&b.resource_id));
        // Focused terminal wins latest_terminal(); provider is the default.
        assert_eq!(state.latest_terminal().map(|t| t.resource_id), Some(a.resource_id).filter(|_| a.is_provider).or(Some(b.resource_id)).or(Some(a.resource_id)));
    }
```

Replace the last assertion with the exact rule: with no strip, `latest_terminal()` returns the provider projection if any, else the first by resource id. Write it as two explicit asserts once the fixture states which is the provider.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked ui::task_workspace::surfaces::tests::registry_holds_one -- --test-threads=1`
Expected: compile error (`terminals` field).

- [ ] **Step 3: Implement**

`surfaces.rs`:

```rust
pub struct TaskSurfaceState {
    // existing fields ...
    pub terminals: BTreeMap<ResourceId, TaskTerminalProjection>,
    pub strip: Option<TaskTerminalsProjection>,
    // remove: pub latest_terminal: Option<TaskTerminalProjection>,
}

impl TaskSurfaceState {
    pub fn focused_resource(&self) -> Option<ResourceId> {
        self.strip.as_ref().and_then(|s| s.focused)
            .or_else(|| self.terminals.values().find(|t| t.is_provider).map(|t| t.resource_id))
            .or_else(|| self.terminals.keys().next().copied())
    }
    pub fn latest_terminal(&self) -> Option<&TaskTerminalProjection> {
        self.focused_resource().and_then(|id| self.terminals.get(&id))
    }
}

    pub fn admit_terminal(&mut self, task_id: K, projection: &TaskTerminalProjection) -> Result<(), SurfaceAdmissionError>
    where K: SurfaceTaskKey,
    {
        if task_id.domain_task_id() != projection.task_id { return Err(SurfaceAdmissionError::WrongTask); }
        let state = self.ensure_task(task_id);
        state.terminals.insert(projection.resource_id, projection.clone());
        state.terminal_attachment = TerminalAttachmentState::Live;
        state.terminal_query_in_flight = false;
        Ok(())
    }

    pub fn admit_terminals(&mut self, task_id: K, projection: &TaskTerminalsProjection) -> Result<(), SurfaceAdmissionError>
    where K: SurfaceTaskKey,
    {
        if task_id.domain_task_id() != projection.task_id { return Err(SurfaceAdmissionError::WrongTask); }
        let state = self.ensure_task(task_id);
        state.terminals.retain(|id, _| projection.terminals.iter().any(|c| c.resource_id == *id));
        state.strip = Some(projection.clone());
        Ok(())
    }
```

Replace every `state.latest_terminal.as_ref()` / `.latest_terminal.clone()` reader in `native_shell.rs` with `state.latest_terminal()` (grep `latest_terminal`). Change `pending_terminal_input_cursors`, `pending_terminal_resizes`, `pending_terminal_echoes`, `pending_terminal_requeries` to `HashMap<TerminalKey, _>` and key them with `(owner.clone(), terminal.resource_id)` at every use. In `admit_owner_terminal_projection` add `TaskCockpitQuery::TaskTerminals` handling: on `TaskCockpitResult::TaskTerminals(p)` call `self.task_surfaces.admit_terminals(owner, &p)` and, if the focused resource changed, `dock_mut().set_focused_terminal(p.focused)` then re-query `Terminal { resource_id: p.focused }`. For `Terminal*` replies, admit into the dock only when `projection.resource_id == state.focused_resource()`.

`dock.rs`: add `focused_terminal: Option<ResourceId>` to `RememberedDockState`; `set_focused_terminal` clears `replica_view`, `last_valid_view`, `last_sequence`, `identity` when the value changes; `admit_task_terminal_projection` returns `Ok(false)` early when `projection.resource_id != memory.focused_terminal.unwrap_or(projection.resource_id)`. For plain shells `HostTerminalBinding::from_client_model(model, task_id)` does not apply: skip the identity comparison when `!projection.is_provider` and build `TerminalRuntimeIdentity` from the projection alone.

`terminal_input_request`: for `!terminal.is_provider`, fill `agent_session_id` with `AgentSessionId::nil()` (add `nil()` to `define_id!` if absent: `Self(Uuid::nil())`) and `runtime_generation: 0`, `action_epoch: terminal.action_epoch.unwrap_or(0)`; extend `TerminalInputContext::validate` to accept the zero fences only when the host-side terminal is a plain shell (host checks `hosted.agent_session_id.is_none()`).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked ui::task_workspace::surfaces ui::task_cockpit::dock -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_workspace/surfaces.rs src/ui/task_cockpit/dock.rs src/ui/native_shell.rs src/terminal/protocol.rs src/domain/id.rs
git commit -m "feat(ui): track one terminal projection per resource in surfaces and dock

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 11: Terminal strip UI, chip menu, and keyboard

**Files:**
- Modify: `src/ui/native_shell.rs:26741-26806` (`terminal_dock_surface`), `src/ui/actions.rs:163-183,389-441`
- Test: `src/ui/actions.rs` tests; `src/ui/native_shell.rs` tests (pure helpers)

**Interfaces:**
- Pure helper `fn terminal_chip_rows(strip: &TaskTerminalsProjection) -> Vec<TerminalChipRow>` with `struct TerminalChipRow { resource_id, label: String, selected: bool, state: TerminalRuntimeStateWire, is_provider: bool }` (provider first, then `strip.order`, label = title else label).
- Keyboard: `KeyboardAction::OpenShellTerminal` on `Ctrl+Shift+Backtick`; `KeyboardAction::CycleTerminal { backwards: bool }` on `Ctrl+Tab` / `Ctrl+Shift+Tab` when the terminal has focus.
- Reorder in this slice uses chip-menu "Move left" / "Move right" (the GPUI shell has no `on_drop` idiom yet); drag reorder is deferred and noted in the spec's decisions log by the implementer.

- [ ] **Step 1: Write the failing tests**

`src/ui/actions.rs` tests:

```rust
    #[test]
    fn shell_terminal_chords_resolve() {
        let model = KeyboardModel::default();
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Backtick)),
            Some(KeyboardAction::OpenShellTerminal)
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Tab)),
            Some(KeyboardAction::CycleTerminal { backwards: false })
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Tab)),
            Some(KeyboardAction::CycleTerminal { backwards: true })
        );
    }
```

`native_shell.rs` tests:

```rust
    #[test]
    fn terminal_chip_rows_put_provider_first_and_use_titles() {
        let task_id = TaskId::new();
        let provider = ResourceId::new();
        let shell = ResourceId::new();
        let strip = TaskTerminalsProjection {
            task_id,
            terminals: vec![
                TaskTerminalChip { resource_id: shell, is_provider: false, title: Some("build".into()), label: "pwsh".into(), runtime_state: TerminalRuntimeStateWire::Running, live_cwd: None, exit: None, created_at_ms: 1, last_activity_at_ms: 1 },
                TaskTerminalChip { resource_id: provider, is_provider: true, title: None, label: "Claude".into(), runtime_state: TerminalRuntimeStateWire::Running, live_cwd: None, exit: None, created_at_ms: 1, last_activity_at_ms: 1 },
            ],
            order: vec![shell],
            focused: Some(shell),
        };
        let rows = terminal_chip_rows(&strip);
        assert_eq!(rows[0].resource_id, provider);
        assert_eq!(rows[1].label, "build");
        assert!(rows[1].selected);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib --locked ui::actions::tests::shell_terminal_chords ui::native_shell::tests::terminal_chip_rows -- --test-threads=1`
Expected: compile errors.

- [ ] **Step 3: Implement**

`actions.rs`: add `ShortcutKey::Tab`, `KeyboardShortcut::ctrl_shift` (if absent), `KeyboardAction::OpenShellTerminal`, `KeyboardAction::CycleTerminal { backwards: bool }`; add GPUI actions:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_shell_terminal")]
pub struct NativeOpenShellTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.cycle_terminal")]
pub struct NativeCycleTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.cycle_terminal_back")]
pub struct NativeCycleTerminalBack;
```

Bindings in `register_native_keyboard_bindings`: `KeyBinding::new("ctrl-shift-`", NativeOpenShellTerminal, None)`, `KeyBinding::new("ctrl-tab", NativeCycleTerminal, Some("TerminalFocused"))`, `KeyBinding::new("ctrl-shift-tab", NativeCycleTerminalBack, Some("TerminalFocused"))` (the terminal surface sets `key_context("TerminalFocused")`). Add the matching `KeyboardBinding` entries to `Default for KeyboardModel`.

`native_shell.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalChipRow {
    pub resource_id: ResourceId,
    pub label: String,
    pub selected: bool,
    pub state: TerminalRuntimeStateWire,
    pub is_provider: bool,
}

pub(crate) fn terminal_chip_rows(strip: &TaskTerminalsProjection) -> Vec<TerminalChipRow> {
    let by_id = strip.terminals.iter().map(|c| (c.resource_id, c)).collect::<HashMap<_, _>>();
    let mut rows = Vec::new();
    if let Some(provider) = strip.terminals.iter().find(|c| c.is_provider) {
        rows.push(TerminalChipRow {
            resource_id: provider.resource_id,
            label: provider.title.clone().unwrap_or_else(|| provider.label.clone()),
            selected: strip.focused.is_none() || strip.focused == Some(provider.resource_id),
            state: provider.runtime_state.clone(),
            is_provider: true,
        });
    }
    for id in &strip.order {
        let Some(chip) = by_id.get(id) else { continue };
        rows.push(TerminalChipRow {
            resource_id: *id,
            label: chip.title.clone().unwrap_or_else(|| chip.label.clone()),
            selected: strip.focused == Some(*id),
            state: chip.runtime_state.clone(),
            is_provider: false,
        });
    }
    rows
}
```

Strip element, inserted as the first child of the `surface` div in `terminal_dock_surface` (idioms from the dock tab bar at `native_shell.rs:40106-40184`):

```rust
        let strip = self.task_surfaces.state(owner.clone()).and_then(|s| s.strip.clone());
        let mut chips = div()
            .id("native-shell-terminal-strip")
            .w_full().flex().flex_none().items_center()
            .gap(px(2.0)).px(px(4.0)).pb(px(4.0)).overflow_hidden();
        if let Some(strip) = strip.as_ref() {
            for row in terminal_chip_rows(strip) {
                let key = stable_resource_element_key(row.resource_id, "chip");
                let resource_id = row.resource_id;
                let owner_for_click = owner.clone();
                let mut chip = div()
                    .id(("native-terminal-chip", key))
                    .tab_stop(true)
                    .flex().flex_none().items_center().gap(px(4.0))
                    .h(px(24.0)).px(px(6.0))
                    .rounded(px(tokens.density.radii.pill))
                    .text_size(px(tokens.density.typography.caption))
                    .line_height(px(tokens.density.typography.caption_line_height))
                    .cursor_pointer()
                    .child(row.label.clone());
                chip = match row.state {
                    TerminalRuntimeStateWire::Exited { .. } => chip.text_color(tokens.text.disabled.to_gpui()),
                    TerminalRuntimeStateWire::Unknown => chip.text_color(tokens.text.muted.to_gpui()).child("?"),
                    TerminalRuntimeStateWire::Running => chip,
                };
                chip = if row.selected {
                    chip.bg(tokens.actions.primary.default.background.to_gpui())
                        .text_color(tokens.actions.primary.default.foreground.to_gpui())
                } else {
                    chip.text_color(tokens.text.muted.to_gpui())
                        .hover(|style| style.bg(tokens.surfaces.raised.to_gpui()))
                };
                let chip = chip
                    .on_mouse_down(MouseButton::Left, cx.listener(move |shell, _e: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        shell.focus_terminal_chip(&owner_for_click, resource_id);
                        cx.notify();
                    }))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |shell, e: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        shell.terminal_chip_menu.open(resource_id, e.position);
                        cx.notify();
                    }));
                chips = chips.child(chip);
            }
        }
        let owner_for_add = owner.clone();
        chips = chips.child(
            div()
                .id("native-terminal-chip-add")
                .tab_stop(true)
                .flex().flex_none().items_center()
                .h(px(24.0)).px(px(6.0))
                .rounded(px(tokens.density.radii.pill))
                .text_color(tokens.text.muted.to_gpui())
                .hover(|style| style.bg(tokens.surfaces.raised.to_gpui()))
                .cursor_pointer()
                .child("+")
                .on_mouse_down(MouseButton::Left, cx.listener(move |shell, _e: &MouseDownEvent, _w, cx| {
                    cx.stop_propagation();
                    shell.open_shell_terminal_for_owner(&owner_for_add, None);
                    cx.notify();
                })),
        );
```

Add `fn stable_resource_element_key(resource_id: ResourceId, suffix: &str) -> u64` modelled on `stable_service_element_key` (`native_shell.rs:1380`). Add:

```rust
    fn focus_terminal_chip(&mut self, owner: &HostTaskKey, resource_id: ResourceId) {
        let Some(strip) = self.task_surfaces.state(owner.clone()).and_then(|s| s.strip.clone()) else { return };
        let strip = crate::domain::terminal_facts::TaskTerminalStrip { order: strip.order.clone(), focused: Some(resource_id) };
        let _ = self.dispatch_action_recorded_for_owner(&owner.host, ActionRequest::TerminalSetStrip { task_id: owner.task_id, strip });
        if let Some(slot) = self.host_slot_mut(&owner.host) {
            slot.cockpit.dock_mut().set_focused_terminal(Some(resource_id));
        }
        let _ = self.dispatch_action_recorded_for_owner(&owner.host, ActionRequest::TaskCockpit { task_id: owner.task_id, query: TaskCockpitQuery::Terminal { resource_id: Some(resource_id) } });
    }

    fn open_shell_terminal_for_owner(&mut self, owner: &HostTaskKey, cwd: Option<String>) {
        let _ = self.dispatch_action_recorded_for_owner(&owner.host, ActionRequest::TerminalOpenShell { task_id: owner.task_id, cwd });
        self.pending_terminal_requeries.insert((owner.clone(), ResourceId::nil()), Instant::now()); // triggers TaskTerminals re-query on next tick
    }

    fn cycle_terminal(&mut self, backwards: bool) {
        let Some(owner) = self.selected_task_key.clone() else { return };
        let Some(strip) = self.task_surfaces.state(owner.clone()).and_then(|s| s.strip.clone()) else { return };
        let rows = terminal_chip_rows(&strip);
        if rows.is_empty() { return; }
        let current = rows.iter().position(|r| r.selected).unwrap_or(0);
        let next = if backwards { (current + rows.len() - 1) % rows.len() } else { (current + 1) % rows.len() };
        self.focus_terminal_chip(&owner, rows[next].resource_id);
    }
```

Chip menu state `terminal_chip_menu: TerminalChipMenuState { open_for: Option<ResourceId>, position: Point<Pixels> }` rendered with the `deferred(anchored()...)` idiom from `native_shell.rs:35748-35830`, rows: Rename (opens an inline text field bound to `ActionRequest::TerminalRename`), Move left, Move right (both compute a new `order` and dispatch `TerminalSetStrip`), Open in project root (`TerminalOpenShell { cwd: Some(project_root) }`), Close (dispatch `TerminalClose`; if the chip's label differs from the shell stem, meaning a child command is running, show a confirm row first). Selecting the `TaskTerminals` query: after any `Terminal*` reply or every 2 s while the Terminal tab is visible, dispatch `TaskCockpitQuery::TaskTerminals` for the selected owner if none is in flight.

Wire `KeyboardAction::OpenShellTerminal => self.open_shell_terminal_for_owner(&owner, None)` and `CycleTerminal { backwards } => self.cycle_terminal(backwards)` in `apply_keyboard_shell_effects` beside the `OpenTerminal` arm, and register the three GPUI action listeners beside `open_terminal` at `native_shell.rs:40012` / `40680`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --lib --locked ui::actions:: ui::native_shell::tests::terminal_chip_rows -- --test-threads=1`; `cargo check --locked --lib --bins --tests`
Expected: PASS; EXIT 0.

- [ ] **Step 5: Manual verification in the dev build**

Run `launch-dev.bat`, open a task, press Ctrl+Shift+Backtick: a `pwsh` chip appears after the provider chip and shows a prompt; `cd` into a subdirectory, wait 3 s, hover the chip: tooltip shows the new directory; type `exit`: chip greys with "exited"; right-click: Rename / Move / Close work. Record the observations in the commit body.

- [ ] **Step 6: Commit**

```bash
git add src/ui/actions.rs src/ui/native_shell.rs
git commit -m "feat(ui): terminal chip strip with open, focus, rename, reorder, close

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 12: Docs, sabotage checks, and final gates

**Files:**
- Modify: `docs/architecture.md` (Host lifetime section: one paragraph on plain shells), `docs/superpowers/specs/2026-09-01-task-shell-terminals-design.md` (decisions log: menu-based reorder in slice 1)

- [ ] **Step 1: Sabotage the cap and the debounce**

Temporarily change `MAX_PLAIN_SHELLS_PER_TASK` to `usize::MAX` and run `cargo test --lib --locked kernel::command_bus::tests::ninth_shell_is_rejected`: expected FAIL. Revert. Temporarily change `TERMINAL_ACTIVITY_COALESCE_MS` to `0` and confirm `open_rename_strip_and_close_shell_terminal` still passes but a new quick assertion (two `RecordTerminalActivity` within 1 s produce two events) would fail: write that assertion into the test, confirm it fails under the sabotage, revert the constant, confirm it passes.

- [ ] **Step 2: Docs**

Add to `docs/architecture.md` under "Host lifetime and terminal survivability":

```markdown
Plain shell terminals are Terminal resources with a launch recipe (cwd, program, args). The host records their live cwd, exit and activity as durable facts, keeps the per-task strip order and focus, and addresses every terminal by `ResourceId`; the provider terminal remains the default target of the legacy terminal queries.
```

Add to the spec's decisions log: "Slice 1 reorders via chip menu (Move left / Move right); drag reorder waits for an `on_drop` idiom in the GPUI shell."

- [ ] **Step 3: Final gates**

Run, in the isolated target:

```powershell
cargo check --locked --lib --bins --tests
cargo test --lib --locked -- --test-threads=1 domain:: kernel:: terminal:: client::action ui::actions ui::task_workspace ui::task_cockpit
cargo test --locked --test terminal_service --test process_supervisor --test task_shell_terminals --test host_recovery
```

Expected: all EXIT 0. Read the `EXIT=` value of each command, never the wrapper's.

- [ ] **Step 4: Commit**

```bash
git add docs/architecture.md docs/superpowers/specs/2026-09-01-task-shell-terminals-design.md src/kernel/command_bus.rs
git commit -m "docs: record plain shell terminal model and slice-1 reorder decision

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

## Self-review notes

- Spec §3 durable model → Tasks 1, 2, 3, 5. Spec §4 commands and cwd ladder → Tasks 4, 7, 9. Spec §5 service and queries → Tasks 6, 8. Spec §6 UX → Tasks 10, 11 (drag reorder deferred, recorded). Spec §7 errors → named rejections in Task 4, spawn failures recorded as `TerminalExited` in Task 7. Spec §8 tests → each task's Step 1 plus Task 12 sabotage.
- Type names used across tasks: `TerminalLaunch`, `TerminalFacts`, `TerminalExit`, `TaskTerminalStrip`, `TerminalRuntimeState` (service), `TerminalRuntimeStateWire` (wire), `TaskTerminalsProjection`, `TaskTerminalChip`, `TerminalChipRow`, `OpenShellTerminalIntent`, `TerminalRenameArguments`, `ShellSessionLink`.
- Known dependency: Task 4's kernel tests go green only after Task 5's loader; the plan says so in Task 4 Step 4.
