# Project Language, First-Run Agent Gate, and Inbox Agent Actions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First run is connect Claude or Codex, then add a project with a small +, then start work with +Claude or +Codex — using the word **project** everywhere except the OS folder picker.

**Architecture:** A pure `AgentConnectionSnapshot` (Claude Code and Codex only) is the gate. The host observes those CLIs on an async path next to `dispatch_provider_start`, and the shell stores the snapshot. Settings and the connect canvas read that snapshot. `+Claude` / `+Codex` send `task.create.v2` with a placeholder title and `primary_provider`; the host then registers the primary agent, a terminal resource, and starts a new conversation. Do not change `TaskFacts::validate_for_create` (`action_epoch` stays 0 on create). Launch uses agent `runtime_generation: 1`, not the task epoch.

**Tech Stack:** Rust, GPUI native shell, stock Claude Code / Codex probes (`AuthenticatedSubscription`), existing `task.create.v2` / `RegisterAgentSession` / `SetPrimaryAgent` / `StartProviderSession` commands.

## Global Constraints

- User-facing copy says **project**, except the OS directory picker prompt **Choose folder**.
- **Signed in** means `ProviderAuthState::AuthenticatedSubscription`. API-key and unknown auth are not signed in.
- **Connected** means at least one of Claude Code or Codex is signed in. Cursor is out of this slice.
- Do not seed projects or copy the workspace path into an empty profile.
- Do not rustfmt all of `src/ui/native_shell.rs`; edit only the regions this plan names.
- GPUI headless: one `Application::headless().run()` per process, `HEADLESS_SHELL_TEST_LOCK`, `--exact --test-threads=1`.
- Isolated Cargo target: `C:\Temp\devmanager-project-agent-first-run` (must print and fail if the target is not under the active worktree or `C:\Temp\devmanager-*`).
- Do not set `DEVMANAGER_PROFILE` for the complete lib suite. Do not touch the installed DevManager PID or `%APPDATA%\com.userfirst.devmanager`.
- Agent auth failures must not paint host transport **Can't connect**.

## File map

- Create: `src/ui/agent_connection.rs` — presence copy, `connected()`, placeholder titles, which inbox + actions to show
- Modify: `src/ui/mod.rs` — declare the module
- Modify: `src/domain/cockpit.rs` — `AgentPresence`, `AgentConnectionSnapshot`, `TaskCockpitQuery::AgentConnection`, `TaskCockpitResult::AgentConnection`
- Modify: `src/host/connection.rs` — async agent-connection query; after create, bind+start primary
- Modify: `src/client/host_client.rs` — `query_agent_connection()`
- Modify: `src/client/action.rs` — `TaskCreateV2Arguments.primary_provider`
- Modify: `src/domain/command.rs` — `CreateTaskRequestIntent.primary_provider` (serde default; kernel create ignores it)
- Modify: `src/ui/native_shell.rs` — copy, chrome, Settings, stages, +Claude/+Codex dispatch
- Modify: `src/ui/task_cockpit/config_sidebar.rs` — **Projects** heading, heading +, hide **LLM providers**
- Modify: `src/ui/task_cockpit/shell.rs` — hold copy only if primary is still missing after a failed start

---

### Task 1: Pure agent-connection projection

**Files:**
- Create: `src/ui/agent_connection.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/domain/cockpit.rs`

**Interfaces:**
- Produces: `crate::domain::cockpit::{AgentPresence, AgentConnectionRow, AgentConnectionSnapshot}`
- Produces: `crate::ui::agent_connection::{placeholder_task_title, inbox_agent_actions, settings_row_copy, connect_canvas_copy, snapshot_connected}`
- Consumes: `ConfigSidebarProviderKind`, `ProviderKind`

- [ ] **Step 1: Write the failing domain + UI projection tests**

Add to `src/domain/cockpit.rs` tests:

```rust
#[test]
fn agent_connection_snapshot_is_connected_when_one_agent_is_signed_in() {
    let snapshot = AgentConnectionSnapshot {
        agents: vec![
            AgentConnectionRow {
                provider: ConfigSidebarProviderKind::Claude,
                presence: AgentPresence::NotFound,
            },
            AgentConnectionRow {
                provider: ConfigSidebarProviderKind::Codex,
                presence: AgentPresence::SignedIn,
            },
        ],
    };
    assert!(snapshot.connected());
    assert!(!AgentConnectionSnapshot { agents: vec![] }.connected());
}
```

Add `src/ui/agent_connection.rs` tests (file can start as tests + unimplemented fns):

```rust
#[test]
fn placeholder_titles_are_non_empty_and_provider_specific() {
    assert_eq!(
        placeholder_task_title(ProviderKind::ClaudeCode),
        "New Claude task"
    );
    assert_eq!(placeholder_task_title(ProviderKind::Codex), "New Codex task");
}

#[test]
fn inbox_actions_list_only_signed_in_claude_and_codex() {
    let snapshot = AgentConnectionSnapshot {
        agents: vec![
            AgentConnectionRow {
                provider: ConfigSidebarProviderKind::Claude,
                presence: AgentPresence::SignedIn,
            },
            AgentConnectionRow {
                provider: ConfigSidebarProviderKind::Codex,
                presence: AgentPresence::NotSignedIn,
            },
        ],
    };
    assert_eq!(
        inbox_agent_actions(&snapshot),
        vec![InboxAgentAction {
            provider: ProviderKind::ClaudeCode,
            label: "+Claude",
        }]
    );
}

#[test]
fn settings_copy_does_not_claim_signed_out_on_check_failed() {
    let copy = settings_row_copy(ConfigSidebarProviderKind::Claude, AgentPresence::CheckFailed);
    assert!(copy.to_ascii_lowercase().contains("could not check"));
    assert!(!copy.to_ascii_lowercase().contains("signed out"));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
printf '%s\n' "$CARGO_TARGET_DIR"
cargo test --lib domain::cockpit::tests::agent_connection_snapshot_is_connected_when_one_agent_is_signed_in -- --exact --test-threads=1
```

Expected: compile fail; `AgentConnectionSnapshot` does not exist.

- [ ] **Step 3: Add the types and copy helpers**

In `src/domain/cockpit.rs` next to `ConfigSidebarProvider`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPresence {
    Checking,
    SignedIn,
    NotSignedIn,
    NotFound,
    CheckFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConnectionRow {
    pub provider: ConfigSidebarProviderKind,
    pub presence: AgentPresence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConnectionSnapshot {
    pub agents: Vec<AgentConnectionRow>,
}

impl AgentConnectionSnapshot {
    pub fn connected(&self) -> bool {
        self.agents
            .iter()
            .any(|row| row.presence == AgentPresence::SignedIn)
    }
}
```

Re-export from `src/domain/mod.rs` the same way `ConfigSidebarSnapshot` is exported.

`src/ui/agent_connection.rs`:

```rust
use crate::domain::{AgentConnectionRow, AgentConnectionSnapshot, AgentPresence, ConfigSidebarProviderKind};
use crate::providers::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxAgentAction {
    pub provider: ProviderKind,
    pub label: &'static str,
}

pub fn snapshot_connected(snapshot: Option<&AgentConnectionSnapshot>) -> bool {
    snapshot.is_some_and(AgentConnectionSnapshot::connected)
}

pub fn placeholder_task_title(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::ClaudeCode => "New Claude task",
        ProviderKind::Codex => "New Codex task",
        ProviderKind::Cursor => "New Cursor task",
    }
}

pub fn inbox_agent_actions(snapshot: &AgentConnectionSnapshot) -> Vec<InboxAgentAction> {
    snapshot
        .agents
        .iter()
        .filter(|row| row.presence == AgentPresence::SignedIn)
        .filter_map(|row| match row.provider {
            ConfigSidebarProviderKind::Claude => Some(InboxAgentAction {
                provider: ProviderKind::ClaudeCode,
                label: "+Claude",
            }),
            ConfigSidebarProviderKind::Codex => Some(InboxAgentAction {
                provider: ProviderKind::Codex,
                label: "+Codex",
            }),
        })
        .collect()
}

pub fn settings_row_copy(provider: ConfigSidebarProviderKind, presence: AgentPresence) -> String {
    let name = match provider {
        ConfigSidebarProviderKind::Claude => "Claude Code",
        ConfigSidebarProviderKind::Codex => "Codex",
    };
    match presence {
        AgentPresence::Checking => format!("Checking {name}…"),
        AgentPresence::SignedIn => format!("{name} is signed in."),
        AgentPresence::NotSignedIn => {
            format!("Sign in with {name}, then Refresh.")
        }
        AgentPresence::NotFound => {
            format!("{name} was not found on this machine. Install it, then Refresh.")
        }
        AgentPresence::CheckFailed => {
            format!("Could not check {name}. Retry.")
        }
    }
}

pub fn connect_canvas_copy(snapshot: Option<&AgentConnectionSnapshot>) -> (&'static str, String) {
    if snapshot_connected(snapshot) {
        (
            "Add a project",
            "Use + in the project list to add one.".into(),
        )
    } else {
        (
            "Connect an agent",
            "Sign in with Claude Code or Codex on this machine, then Refresh. You can add a project after one of them is connected.".into(),
        )
    }
}
```

Declare `pub mod agent_connection;` in `src/ui/mod.rs`.

- [ ] **Step 4: Run the tests and verify GREEN**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib domain::cockpit::tests::agent_connection_snapshot_is_connected_when_one_agent_is_signed_in -- --exact --test-threads=1
cargo test --lib ui::agent_connection -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/domain/cockpit.rs src/domain/mod.rs src/ui/agent_connection.rs src/ui/mod.rs
git commit -m "$(cat <<'EOF'
Add Claude/Codex connection projection for first-run gating.

EOF
)"
```

---

### Task 2: User-facing language is Project

**Files:**
- Modify: `src/ui/task_cockpit/config_sidebar.rs` (`folder_section_title`, test `a_single_folder_is_listed_once_under_folders`)
- Modify: `src/ui/native_shell.rs` (palette titles, overlay copy, `open_add_project` field name, accessibility setup strings; keep `prompt: Some("Choose folder".into())`)

**Interfaces:**
- Produces: `ConfigSidebarProjection::project_section_title() -> "Projects"` (replace `folder_section_title`)
- Consumes: existing overlay/palette helpers

- [ ] **Step 1: Write the failing copy tests**

In `config_sidebar.rs` rename the test to `a_single_project_is_listed_once_under_projects` and expect `"Projects"`.

In `native_shell.rs` tests, add (non-GPUI) assertions next to palette tests if they exist; otherwise add:

```rust
#[test]
fn palette_add_project_says_project_not_folder() {
    assert_eq!(PaletteItem::AddProject.title(), "Add project");
    assert_eq!(
        PaletteItem::AddProject.hint(),
        "Choose a folder on this computer"
    );
}
```

Keep the hint as folder (OS picker). Overlay tests that look for `"Add this folder?"` must expect `"Add this project?"`. Path caption may stay `"Folder"`. Confirm body may say the chosen folder will be added as a project.

- [ ] **Step 2: Run focused tests and verify RED**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib ui::task_cockpit::config_sidebar::tests::a_single_project_is_listed_once_under_projects -- --exact --test-threads=1
```

Expected: FAIL (`Folders` or missing renamed test).

- [ ] **Step 3: Change user-visible strings**

Exact replacements in `native_shell.rs` (do not change `prompt: Some("Choose folder".into())`):

| Current | New |
| --- | --- |
| Palette title `Add folder` | `Add project` |
| Overlay heading `Add this folder?` / `Add a folder` | `Add this project?` / `Add a project` |
| Overlay field `TextField::new("Folder name")` | `TextField::new("Project name")` |
| Sidebar `folder_section_title` `"Folders"` | `project_section_title` `"Projects"` |
| Button labels `Add folder` | remove in Task 5; for this task, if a label remains, `Add project` |

Leave welcome **Choose a folder** CTA until Task 5 (that button goes away). Update any test whose assertion string you changed in this task.

`config_sidebar.rs` `surface` must call `project_section_title()`.

- [ ] **Step 4: Run the tests and verify GREEN**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib ui::task_cockpit::config_sidebar::tests::a_single_project_is_listed_once_under_projects -- --exact --test-threads=1
cargo test --lib ui::native_shell::tests::palette_add_project_says_project_not_folder -- --exact --test-threads=1
```

Expected: PASS. Also re-run any overlay GPUI test you touched, **alone**.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_cockpit/config_sidebar.rs src/ui/native_shell.rs
git commit -m "$(cat <<'EOF'
Say project in the shell; keep folder only for the OS picker.

EOF
)"
```

---

### Task 3: Host agent-connection query

**Files:**
- Modify: `src/domain/cockpit.rs` — `TaskCockpitQuery::AgentConnection`, `TaskCockpitResult::AgentConnection`, `cockpit_surface`
- Modify: `src/host/connection.rs` — async fork beside `dispatch_provider_start`
- Modify: `src/client/host_client.rs` — `query_agent_connection`
- Modify: `src/ui/native_shell.rs` — `NativeHostCommand::AgentConnectionQuery`, `NativeHostQueryBody::AgentConnection`, store `agent_connection: Option<AgentConnectionSnapshot>` on the shell
- Test: `src/host/connection.rs` or a focused `src/host/agent_connection.rs` mapper test

**Interfaces:**
- Produces: `HostClient::query_agent_connection() -> Result<Result<AgentConnectionSnapshot, QueryError>, IpcError>`
- Produces: `map_provider_observe(kind, result) -> AgentConnectionRow`
- Consumes: `ProviderRegistry::observe`, `ProviderAuthState::AuthenticatedSubscription`

- [ ] **Step 1: Write the failing mapper test**

Put `map_provider_observe` in `src/host/agent_connection.rs` (new small module, `pub(crate)` from `host/mod.rs`):

```rust
#[test]
fn missing_cli_is_not_found_and_subscription_is_signed_in() {
    assert_eq!(
        map_provider_observe(
            ConfigSidebarProviderKind::Claude,
            Err(&ProviderError::MissingCli {
                kind: ProviderKind::ClaudeCode,
                requested: None,
            })
        )
        .presence,
        AgentPresence::NotFound
    );
    // Signed-in row: construct a stub Ok observation in the test with
    // auth_state AuthenticatedSubscription, or map from ProviderAuthState directly.
    assert_eq!(
        presence_from_auth(ProviderAuthState::AuthenticatedSubscription),
        AgentPresence::SignedIn
    );
    assert_eq!(
        presence_from_auth(ProviderAuthState::AuthRequired),
        AgentPresence::NotSignedIn
    );
    assert_eq!(
        presence_from_auth(ProviderAuthState::Unknown),
        AgentPresence::CheckFailed
    );
}
```

If `ProviderError::MissingCli` fields differ, match the real variant in `src/providers/adapter.rs`.

- [ ] **Step 2: Run the test and verify RED**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib host::agent_connection -- --test-threads=1
```

Expected: compile fail.

- [ ] **Step 3: Implement mapping, query variant, async host path, client method, shell storage**

`presence_from_auth`:

```rust
pub(crate) fn presence_from_auth(auth: ProviderAuthState) -> AgentPresence {
    match auth {
        ProviderAuthState::AuthenticatedSubscription => AgentPresence::SignedIn,
        ProviderAuthState::AuthRequired => AgentPresence::NotSignedIn,
        ProviderAuthState::Unknown => AgentPresence::CheckFailed,
    }
}
```

`TaskCockpitQuery::AgentConnection` — handle **before** task lookup, same as `ConfigSnapshot`, but **do not** serve it from sync `serve_task_cockpit` (return Unavailable if reached). Instead, in `HostRequestExecutor::run` (both loops around `connection.rs:2206` and `2258`):

```rust
let result = if is_agent_connection_query(&job.request) {
    self.dispatch_agent_connection(job.negotiated, job.request, job.output_id).await
} else if is_provider_start_request(&job.request) {
    self.dispatch_provider_start(job.negotiated, job.request, job.output_id).await
} else {
    self.dispatch_job(job.negotiated, job.request, job.output_id, job.routing)
};
```

`dispatch_agent_connection` observes `ProviderKind::ClaudeCode` then `ProviderKind::Codex` through `configured_service_runtime.manager.provider_host().registry().observe(kind, &ProviderDiscoveryConfig::default())`, maps each with `map_provider_observe`, returns `QueryReply` with `TaskCockpitResult::AgentConnection`. Never observe Cursor. Never send a prompt.

`HostClient::query_agent_connection` copies `query_config_sidebar` but sends `TaskCockpitQuery::AgentConnection` and expects `TaskCockpitResult::AgentConnection`.

In the native host worker (`native_shell.rs` `HostStatusQuery` neighbor), add `NativeHostCommand::AgentConnectionQuery`. On success, apply snapshot onto `NativeShell.agent_connection` and `cx.notify()`. On attach/connected, dispatch this query. Settings Refresh (Task 4) re-dispatches it.

Wire `NativeHostQueryBody::AgentConnection` so a failure does **not** call `set_transport_failure`. Follow the existing stale-query rule: do not flip `NativeHostState::Error`.

Default `agent_connection` on a new shell: `AgentConnectionSnapshot` with both rows `Checking`, `connected() == false`.

- [ ] **Step 4: Run mapper tests GREEN**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib host::agent_connection -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/domain/cockpit.rs src/domain/mod.rs src/host/agent_connection.rs src/host/mod.rs src/host/connection.rs src/client/host_client.rs src/ui/native_shell.rs
git commit -m "$(cat <<'EOF'
Query Claude and Codex sign-in without using the host transport error path.

EOF
)"
```

---

### Task 4: Settings in the header

**Files:**
- Modify: `src/ui/native_shell.rs` — `header_bar`, overlay, `OpenSettings` / `settings_open`
- Test: GPUI headless in `native_shell.rs`

**Interfaces:**
- Produces: `NativeShell::settings_open_for_test() -> bool`
- Consumes: `settings_row_copy`, `AgentConnectionSnapshot`, Refresh → `AgentConnectionQuery`

- [ ] **Step 1: Write the failing headless test**

```rust
#[test]
fn header_settings_opens_on_welcome_and_cockpit() {
    // HEADLESS_SHELL_TEST_LOCK + Application::headless().run
    // with_test_shell_in_app: assert a test helper
    // NativeShell::header_shows_settings_for_test() is true on Welcome (empty projects)
    // and after install_named_folder_for_test (FirstTask/Cockpit).
    // dispatch_named_accessibility_action("native-header-settings")
    // => settings_open_for_test()
}
```

- [ ] **Step 2: Run it RED (alone)**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib ui::native_shell::tests::header_settings_opens_on_welcome_and_cockpit -- --exact --test-threads=1
```

Expected: FAIL / compile fail.

- [ ] **Step 3: Implement Settings chrome**

Always include a ghost `Button::new("native-header-settings").label("Settings")` in `header_bar` (Connecting and Recovery too). Click sets `self.settings_open = true`.

Overlay (same pattern as add-project: `deferred(anchored())`):
- Title: `Settings`
- One row per Claude Code and Codex using `settings_row_copy`
- Button `Refresh` → dispatch `AgentConnectionQuery`
- Caption: DevManager does not log you in; sign in with that app, then Refresh
- Cancel / backdrop click closes

Replace `OpenSettings => "Configuration settings selected"` with opening this overlay.

Accessibility action `"native-header-settings"`.

- [ ] **Step 4: Run the test GREEN (alone)**

Same cargo command as Step 2. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/native_shell.rs
git commit -m "$(cat <<'EOF'
Put Settings in the header so agent sign-in is always reachable.

EOF
)"
```

---

### Task 5: Connect-first canvas and small project +

**Files:**
- Modify: `src/ui/native_shell.rs` — `shell_stage`, `shows_add_project_plus`, setup copy, remove large Add folder / Choose a folder buttons, palette gate
- Modify: `src/ui/task_cockpit/config_sidebar.rs` — `surface(..., projects_heading_action: Option<AnyElement>)`, hide LLM providers section
- Test: GPUI + unit tests replacing `first_task_does_not_show_add_folder`

**Interfaces:**
- Produces: `NativeShell::shows_add_project_plus(&self) -> bool` = `snapshot_connected(self.agent_connection.as_ref())`
- Produces: connect canvas when `!shows_add_project_plus()` even if projects exist
- Consumes: Task 3 snapshot, Task 1 `connect_canvas_copy`

- [ ] **Step 1: Write failing tests**

Replace `first_task_does_not_show_add_folder` with:

```rust
#[test]
fn project_plus_is_hidden_until_an_agent_is_connected() {
    assert!(!NativeShell::plus_visible_for_state(false, true)); // connected=false, has_project=true
    assert!(NativeShell::plus_visible_for_state(true, false));
    assert!(NativeShell::plus_visible_for_state(true, true));
}
```

GPUI (alone): empty shell with `agent_connection` both `NotFound` → setup title `Connect an agent`; no `native-setup-add-project`; sidebar has no heading +. Inject SignedIn Claude → plus available (`native-projects-add` exists via accessibility id).

Palette: `PaletteItem::for_stage` must not include `AddProject` when not connected. Remove `NewTask` from the palette in this or Task 6; this task should stop advertising Add folder/project when disconnected.

- [ ] **Step 2: Run RED**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib ui::native_shell::tests::project_plus_is_hidden_until_an_agent_is_connected -- --exact --test-threads=1
```

- [ ] **Step 3: Implement the gate and chrome**

`shell_stage`: if recovery/connecting unchanged; else if `!snapshot_connected(...)` → treat as Welcome **but show the sidebar** when `!projects.is_empty()` (same layout as current FirstTask: sidebar + intro). Intro uses `connect_canvas_copy`. No primary **Choose a folder** button. Optional **Refresh** on the canvas (Settings remains in the header).

If connected and `projects.is_empty()` → Welcome/add-project canvas with **no** large CTA; the only add control is the sidebar **+**.

`shows_add_folder_chrome` becomes `shows_add_project_plus` using connected state, not `stage == Cockpit`.

Remove:
- `native-header-add-project` **Add folder** button
- `native-sidebar-add-project` large ghost button
- `native-inbox-add-project`
- `native-setup-add-project` **Choose a folder** primary button

Add: `config_sidebar.surface(tokens, plus)` where `plus` is a small `Button::new("native-projects-add").label("+")` with accessibility name **Add project**, only when `shows_add_project_plus()`. Put it in the Projects heading row (change `section()` to accept `Option<AnyElement>` trailing on the heading for section 0 only).

`begin_choose_folder` / palette `AddProject` must no-op when not connected (open Settings instead is acceptable; do not open the OS picker).

Update `setup_welcome_steps` to: connect an agent; add a project; start Claude or Codex from the inbox. Only show those steps on the add-project canvas (connected, no project), not on the connect canvas.

Hide the **LLM providers** sidebar section (Settings owns that copy).

Keep OS picker prompt **Choose folder**. Overlay confirm may say the chosen folder will be added as a project.

- [ ] **Step 4: Run GREEN (unit, then each GPUI test alone)**

- [ ] **Step 5: Commit**

```bash
git add src/ui/native_shell.rs src/ui/task_cockpit/config_sidebar.rs
git commit -m "$(cat <<'EOF'
Require a signed-in agent before adding a project, using a small list +.

EOF
)"
```

---

### Task 6: Inbox +Claude / +Codex instead of Name this task

**Files:**
- Modify: `src/ui/native_shell.rs` — `offer_first_task_if_needed`, `begin_new_task`, header **New task**, inbox empty actions, `stacked_panel_grow` trailing, palette `NewTask`, accessibility FirstTask tree
- Modify: `src/ui/task_cockpit/config_sidebar.rs` only if needed
- Test: replace `first_task_stage_opens_the_name_overlay_once`

**Interfaces:**
- Produces: inbox header trailing from `inbox_agent_actions`
- Consumes: connected snapshot, selected project
- `begin_new_task` is removed from user chrome (keep function until Task 7 if tests still call it; stop auto-offering)

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn first_task_does_not_open_a_name_overlay() {
    // install_named_folder_for_test + inject SignedIn Claude
    // offer_first_task_if_needed
    // assert!(!shell.new_task_overlay_open_for_test())
}

#[test]
fn inbox_header_lists_plus_claude_when_claude_is_signed_in() {
    assert_eq!(
        inbox_agent_actions(&signed_in_claude_only()),
        vec![InboxAgentAction { provider: ProviderKind::ClaudeCode, label: "+Claude" }]
    );
}
```

Replace FirstTask accessibility CTA `Create a task` / `Give this work a name.` Once a project exists and an agent is connected, `shell_stage` should be **Cockpit** even with zero tasks (empty inbox is the first-task UI). That inverts `_ if self.task_list.task_ids().is_empty() => FirstTask`.

Update `first_task_does_not_repeat_the_folder_name` if the stage is now Cockpit: header still must not restamp the project name.

- [ ] **Step 2: Run RED (each GPUI test alone)**

- [ ] **Step 3: Implement**

`shell_stage`: connected + has project → `Cockpit` even when the task list is empty. `FirstTask` remains only if you still need it for disconnected+has-project; prefer Connect canvas (Welcome layout with sidebar) when disconnected.

Remove auto `begin_new_task` from `offer_first_task_if_needed` (delete the offer or make it a no-op).

Remove header **New task**, inbox **New task**, setup **Create a task**.

`stacked_panel_grow("native-shell-task-inbox", "Task inbox", ...)` already has `panel_header(..., trailing)`. Pass +Claude/+Codex buttons (`native-inbox-plus-claude` / `native-inbox-plus-codex`) when `inbox_agent_actions` is non-empty **and** `first_workspace_project_id().is_some()`. Clicks call `start_task_with_agent(kind)`, which in this task only stores `pending_inbox_agent: Option<ProviderKind>` for tests (no create yet). Task 8 replaces that body with `TaskCreateV2`. Empty inbox copy: `No tasks yet` / `Use +Claude or +Codex to start.`

Remove `PaletteItem::NewTask` from `ALL` / `for_stage`.

- [ ] **Step 4: Run GREEN**

- [ ] **Step 5: Commit**

```bash
git add src/ui/native_shell.rs
git commit -m "$(cat <<'EOF'
Replace Name this task with +Claude and +Codex on the inbox.

EOF
)"
```

---

### Task 7: Host create-with-primary-agent

**Files:**
- Modify: `src/client/action.rs` — `TaskCreateV2Arguments.primary_provider`
- Modify: `src/domain/command.rs` — `CreateTaskRequestIntent.primary_provider` with `#[serde(default)]`
- Modify: `src/host/connection.rs` — after successful create, bind+start
- Test: host/kernel test that create with `primary_provider: ClaudeCode` leaves `primary_agent_id` set

**Interfaces:**
- Consumes: existing `RegisterAgentSession`, `RegisterResource` (Terminal), `SetPrimaryAgent`, `dispatch_provider_start`
- Produces: create follow-through that does not require the client `provider_start_command` (that factory still rejects `action_epoch == 0`)
- Agent registration uses `runtime_generation: 1`, `revision: 0`, `role: Primary`
- `StartProviderSessionIntent.expected_action_epoch` is the created task epoch (`0`)
- Launch correlation / binding generation must be **nonzero** (`runtime_generation: 1`). If `start_production_stock_provider_session` currently copies task `action_epoch` into the launch correlation, change that mapping to the agent generation and prove it with a test. Do **not** change `TaskFacts::validate_for_create`.

- [ ] **Step 1: Write the failing factory/host test**

```rust
#[test]
fn task_create_v2_carries_optional_primary_provider() {
    let args = TaskCreateV2Arguments {
        task_id: TaskId::new(),
        environment_id: EnvironmentId::new(),
        title: "New Claude task".into(),
        description: None,
        project_id: ProjectId::new(),
        workspace: WorkspaceRequest::main(),
        primary_provider: Some(ProviderKind::ClaudeCode),
    };
    assert_eq!(args.primary_provider, Some(ProviderKind::ClaudeCode));
}
```

Add a host-side test (follow existing `connection.rs` create+register agent fixtures) that after create-with-provider the task snapshot has `primary_agent_id.is_some()` and the registered agent `runtime_generation == 1`. If launch cannot run in that test harness, assert the `StartProviderSessionIntent` the host would send has `expected_action_epoch == 0` and the binding generation is 1.

Reject `primary_provider: Some(Cursor)` at the host boundary (`IpcError::Unavailable` or security).

- [ ] **Step 2: Run RED**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
cargo test --lib client::action::tests::task_create_v2_carries_optional_primary_provider -- --exact --test-threads=1
```

(Adjust module path to wherever you place the test.)

- [ ] **Step 3: Implement host follow-through**

Add to both structs:

```rust
#[serde(default)]
pub primary_provider: Option<ProviderKind>,
```

Every existing `TaskCreateV2Arguments { ... }` literal in the crate must set `primary_provider: None` (or rely on struct update if they use `..` — they likely do not). Compile will list them.

In `dispatch` after `execute_host_authorized` for a successful create, if `primary_provider` is Some(ClaudeCode|Codex):
1. `RegisterAgentSession` Primary, that `provider_kind`, `runtime_generation: 1`
2. `RegisterResource` task-owned Terminal (cols/rows > 0; match an existing kernel fixture)
3. `SetPrimaryAgent`
4. `dispatch_provider_start` with `ProviderStartMode::NewConversation`

If step 4 fails, keep the task and primary bind; surface the launch error on the command receipt / query path. Do not delete the task. Do not pretend a PTY started.

Capture `primary_provider` **before** `normalize_task_create_at_host` so it is not lost if normalize drops unknown fields.

- [ ] **Step 4: Run GREEN**

- [ ] **Step 5: Commit**

```bash
git add src/client/action.rs src/domain/command.rs src/host/connection.rs
git commit -m "$(cat <<'EOF'
Bind and start a primary agent when a new task names Claude or Codex.

EOF
)"
```

---

### Task 8: Wire +Claude / +Codex to create-with-provider

**Files:**
- Modify: `src/ui/native_shell.rs` — click / accessibility `native-inbox-plus-claude` / `native-inbox-plus-codex`
- Modify: `src/ui/task_cockpit/shell.rs` — if start failed, hold must not say generic missing-field; use the host error
- Test: extend `created_task_is_listed_without_a_disconnect_or_missing_field` or add `plus_claude_creates_a_bound_task`

**Interfaces:**
- Consumes: `placeholder_task_title`, `TaskCreateV2Arguments { primary_provider: Some(kind), .. }`
- `dispatch_named_accessibility_action` for the two inbox ids

- [ ] **Step 1: Write the failing test**

GPUI, **alone**:

```rust
#[test]
fn plus_claude_creates_a_task_with_placeholder_title() {
    // connected Claude, named project, Cockpit, empty inbox
    // dispatch_named_accessibility_action("native-inbox-plus-claude")
    // last dispatched ActionRequest::TaskCreateV2 has
    // title == "New Claude task" and primary_provider == Some(ClaudeCode)
}
```

Add a test helper `last_create_v2_for_test() -> Option<TaskCreateV2Arguments>` if the shell does not already record the last request.

Also assert `new_task_overlay_open_for_test() == false`.

If you can apply a client_model with `primary_agent_id: Some(...)`, assert conversation hold is **not** `missing field agent_session_id` and **not** host **Can't connect**.

- [ ] **Step 2: Run RED (alone)**

- [ ] **Step 3: Implement dispatch**

```rust
fn start_task_with_agent(&mut self, kind: ProviderKind) {
    let Some(project_id) = self.first_workspace_project_id() else {
        return;
    };
    if !inbox_agent_actions(self.agent_connection.as_ref().unwrap_or(&empty_snapshot()))
        .iter()
        .any(|action| action.provider == kind)
    {
        return;
    }
    let _ = self.dispatch_action(ActionRequest::TaskCreateV2(TaskCreateV2Arguments {
        task_id: TaskId::new(),
        environment_id: EnvironmentId::new(),
        title: placeholder_task_title(kind).to_string(),
        description: None,
        project_id,
        workspace: WorkspaceRequest::main(),
        primary_provider: Some(kind),
    }));
}
```

Wire buttons and accessibility. Do not open `new_task` overlay.

Existing sticky semantic title from the first substantive user message remains the rename-from-conversation path. No new name overlay. Inbox may show the placeholder until that title arrives.

- [ ] **Step 4: Run GREEN (alone)**

Re-run `created_task_is_listed_without_a_disconnect_or_missing_field` **alone** so the unbound-task hold still works for tasks without `primary_provider`.

- [ ] **Step 5: Commit**

```bash
git add src/ui/native_shell.rs src/ui/task_cockpit/shell.rs src/client/action.rs
git commit -m "$(cat <<'EOF'
Start a bound Claude or Codex task from the inbox plus actions.

EOF
)"
```

---

### Task 9: Isolation check and leftover copy

**Files:**
- Grep leftovers: user-visible `Add folder`, `Folders`, `Name this task`, `New task`, `LLM providers`, `Choose a folder` (allowed only in OS prompt / overlay path caption / picker hint)
- Modify any missed strings
- Test: `cargo check --locked --lib --bins --tests` in the isolated target

- [ ] **Step 1: Grep user-facing leftovers**

```bash
rg -n "Add folder|Name this task|\"Folders\"|LLM providers" src/ui --glob '*.rs'
```

Expected: no user-visible hits except comments, OS prompt `Choose folder`, overlay path label `Folder`, palette hint `Choose a folder on this computer`.

- [ ] **Step 2: Fix stragglers with tests if a string is user-visible**

- [ ] **Step 3: Compiler check in the isolated target**

```bash
export CARGO_TARGET_DIR=/c/Temp/devmanager-project-agent-first-run
printf '%s\n' "$CARGO_TARGET_DIR"
cargo check --locked --lib --bins --tests
```

Expected: success. Confirm no leftover Cargo/rustc from this target after the check. Do not start a second check while this one is running.

- [ ] **Step 4: Confirm installed app untouched**

Compare installed DevManager PID/start time and production `config.json` / `remote.json` hashes if this machine has an installed app; they must be unchanged. Isolated profile under `.devmanager-next` may change.

- [ ] **Step 5: Commit** only if Step 2 produced diffs

```bash
git add src/ui
git commit -m "$(cat <<'EOF'
Remove leftover folder-first copy from the native shell.

EOF
)"
```

---

## Spec coverage

| Spec requirement | Task |
| --- | --- |
| Project language except OS picker | 2, 5, 9 |
| Small + on project list; no large Add folder | 5 |
| Connect before add project; existing projects stay listed | 5 |
| Settings always in header; Refresh; no in-app login | 4 |
| Signed in = AuthenticatedSubscription; Cursor out | 1, 3 |
| +Claude/+Codex; no Name this task / New task | 6, 8 |
| Placeholder title; rename later via existing task.rename | 1, 8 |
| Host bind+start on create; no unbound success path | 7, 8 |
| Auth failure ≠ Can't connect | 3 |
| Tests listed in the spec | 2, 5, 6, 8 |
