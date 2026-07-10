# SSH Key-or-Password Auth + Always-Visible Terminal Scrollbar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an SSH connection authenticate with a pasted private key or a saved password (auto-using whichever is set, key first with password fallback), and always show the terminal scrollbar gutter when the setting is on (full-height inert thumb when there is no scrollback).

**Architecture:** Part 1 stores pasted key material in `SSHConnection.private_key`, materializes it to a permission-locked file under the app config dir at connect time, and appends `-i <file>` to the `ssh` args. The existing password prompt auto-inject is untouched and becomes the fallback. Part 2 relaxes two gates in `src/app/mod.rs` so the scrollbar renders even when `total_lines <= rows`, extracting the model math into a pure function for testability.

**Tech Stack:** Rust, GPUI, serde, portable-pty. Tests: `cargo test` (built-in harness).

**Spec:** `docs/superpowers/specs/2026-07-06-ssh-key-or-password-auth-design.md`

## Global Constraints

- Config serde: `SSHConnection` is `#[serde(default, rename_all = "camelCase")]` — the new field serializes as `privateKey` and must load as `None` from legacy configs.
- When a key is set, pass **only** `-i <file>` — NO `-o IdentitiesOnly=yes` (user decision: prefer key, allow agent fallback).
- Key materialization failure must NOT fail the session — connect without `-i` and surface a notice in the terminal.
- Windows subprocesses must not flash console windows: use `creation_flags(0x08000000)` (CREATE_NO_WINDOW), matching `src/git/git_service.rs:9`.
- Never display key material in UI summaries — only "Saved" / "Not saved".
- All work on branch `feature/ssh-key-or-password-auth`. Run `cargo test` before each commit.
- Ignore `zz-archive/` and `third_party/` entirely.

---

### Task 1: Add `private_key` to the SSHConnection model

**Files:**
- Modify: `src/models/config.rs:78-85` (SSHConnection struct)
- Modify: `src/app/mod.rs:5231-5241` (save conversion literal — temporary `None`)
- Modify: `src/app/mod.rs:13072-13081` (test fixture `sample_ssh_connection`)
- Modify: `src/sidebar/mod.rs:1671-1678` (test literal)
- Create: `tests/ssh_private_key.rs`

**Interfaces:**
- Produces: `SSHConnection.private_key: Option<String>` (JSON key `privateKey`) — consumed by Tasks 2, 3, 4.

- [ ] **Step 1: Write the failing round-trip test**

Create `tests/ssh_private_key.rs`:

```rust
use devmanager::models::SSHConnection;

#[test]
fn private_key_serializes_camel_case_and_round_trips() {
    let connection = SSHConnection {
        id: "ssh-1".to_string(),
        label: "Prod".to_string(),
        host: "example.com".to_string(),
        port: 22,
        username: "deploy".to_string(),
        password: Some("pw".to_string()),
        private_key: Some(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
                .to_string(),
        ),
    };

    let json = serde_json::to_string(&connection).expect("serialize");
    assert!(json.contains("\"privateKey\""));

    let back: SSHConnection = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, connection);
}

#[test]
fn legacy_connection_without_private_key_loads_as_none() {
    let json = r#"{"id":"ssh-1","label":"Prod","host":"example.com","port":22,"username":"deploy"}"#;

    let connection: SSHConnection = serde_json::from_str(json).expect("deserialize legacy");

    assert_eq!(connection.private_key, None);
    assert_eq!(connection.password, None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test ssh_private_key`
Expected: COMPILE ERROR — `struct SSHConnection has no field named private_key`.

- [ ] **Step 3: Add the field and fix all struct literals**

In `src/models/config.rs`, add one field to `SSHConnection` (after `password`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct SSHConnection {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub private_key: Option<String>,
}
```

Fix the three literals that now fail to compile:

1. `src/app/mod.rs:5231` (the save conversion inside the SSH editor save arm) — add a temporary line; Task 4 replaces it with the draft value:

```rust
                let connection = SSHConnection {
                    id: draft
                        .existing_id
                        .clone()
                        .unwrap_or_else(|| next_entity_id("ssh")),
                    label: draft.label.trim().to_string(),
                    host: draft.host.trim().to_string(),
                    port,
                    username: draft.username.trim().to_string(),
                    password: normalize_optional_string(&draft.password),
                    private_key: None,
                };
```

2. `src/app/mod.rs:13072` test fixture — add `private_key: None,` after `password: None,`.

3. `src/sidebar/mod.rs:1671` test literal — add `private_key: None,` after `password: None,`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test ssh_private_key`
Expected: `test result: ok. 2 passed`

Run: `cargo test`
Expected: all existing tests still pass (serde `default` keeps fixtures loading).

- [ ] **Step 5: Commit**

```bash
git add src/models/config.rs src/app/mod.rs src/sidebar/mod.rs tests/ssh_private_key.rs
git commit -m "feat: add private_key field to SSHConnection model"
```

---

### Task 2: Key sanitization and materialization helpers

**Files:**
- Modify: `src/services/process_manager.rs` (new free functions near `build_ssh_launch_spec` at `:2539`, new `impl ProcessManager` methods, tests in the existing `mod tests` at `:3008`)

**Interfaces:**
- Consumes: `SSHConnection.private_key` from Task 1; `crate::persistence::app_config_dir()` (`src/persistence/mod.rs:71`, returns `persistence::Result<PathBuf>` whose error implements `Display`); test helper `temp_test_dir` (`src/services/process_manager.rs:3309`).
- Produces (all in `process_manager.rs`):
  - `fn sanitize_private_key(text: &str) -> String` (private free fn)
  - `fn safe_key_file_name(connection_id: &str) -> String` (private free fn)
  - `fn materialize_ssh_key_in(dir: &Path, connection: &SSHConnection) -> Result<Option<PathBuf>, String>` (private free fn)
  - `impl ProcessManager { fn materialize_ssh_key(&self, connection: &SSHConnection) -> Result<Option<PathBuf>, String> }` (private method)
  - `impl ProcessManager { pub fn remove_materialized_ssh_key(connection_id: &str) }` (pub associated fn, best-effort, used by Task 4)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/services/process_manager.rs` (after the existing tests; `use super::*;` is already at the top of the module):

```rust
    #[test]
    fn sanitize_private_key_normalizes_line_endings_and_trailing_newline() {
        let pasted = "-----BEGIN OPENSSH PRIVATE KEY-----\r\nabc\r\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn sanitize_private_key_leaves_clean_key_unchanged() {
        let clean = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n";
        assert_eq!(sanitize_private_key(clean), clean);
    }

    #[test]
    fn sanitize_private_key_trims_surrounding_blank_lines() {
        let pasted = "\n\n  -----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n\n\n";
        assert_eq!(
            sanitize_private_key(pasted),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn safe_key_file_name_replaces_path_hostile_characters() {
        assert_eq!(safe_key_file_name("ssh-1a2b-3"), "ssh-1a2b-3");
        assert_eq!(safe_key_file_name("ssh/../evil"), "ssh____evil");
    }

    #[test]
    fn materialize_ssh_key_writes_sanitized_key_file() {
        let dir = temp_test_dir("materialize-ssh-key");
        let connection = SSHConnection {
            id: "ssh-test".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: None,
            private_key: Some("-----BEGIN KEY-----\r\nabc\r\n-----END KEY-----".to_string()),
        };

        let path = materialize_ssh_key_in(&dir, &connection)
            .expect("materialize")
            .expect("path");

        assert_eq!(path, dir.join("ssh-test"));
        assert_eq!(
            fs::read_to_string(&path).expect("read key"),
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn materialize_ssh_key_returns_none_without_key_material() {
        let dir = temp_test_dir("materialize-ssh-key-empty");
        let connection = SSHConnection {
            id: "ssh-empty".to_string(),
            label: "Test".to_string(),
            host: "example.com".to_string(),
            port: 22,
            username: "deploy".to_string(),
            password: Some("pw".to_string()),
            private_key: Some("   \n".to_string()),
        };

        assert_eq!(materialize_ssh_key_in(&dir, &connection), Ok(None));
        assert!(!dir.join("ssh-empty").exists());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib services::process_manager`
Expected: COMPILE ERROR — `cannot find function sanitize_private_key`.

- [ ] **Step 3: Implement the helpers**

Add as free functions directly above `fn build_ssh_launch_spec` (`src/services/process_manager.rs:2539`). `Path`, `PathBuf`, and `SSHConnection` are already imported at the top of the file.

```rust
/// OpenSSH rejects key files with CRLF line endings or a missing final
/// newline — both are common artifacts of pasting a key into a text field.
fn sanitize_private_key(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{}\n", normalized.trim())
}

fn safe_key_file_name(connection_id: &str) -> String {
    connection_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn materialize_ssh_key_in(
    dir: &Path,
    connection: &SSHConnection,
) -> Result<Option<PathBuf>, String> {
    let Some(key) = connection
        .private_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };

    std::fs::create_dir_all(dir)
        .map_err(|error| format!("create {}: {error}", dir.display()))?;
    let path = dir.join(safe_key_file_name(&connection.id));
    std::fs::write(&path, sanitize_private_key(key))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    lock_key_file_permissions(&path)?;
    Ok(Some(path))
}

#[cfg(unix)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set permissions on {}: {error}", path.display()))
}

#[cfg(windows)]
fn lock_key_file_permissions(path: &Path) -> Result<(), String> {
    // Win32-OpenSSH refuses private keys readable by other accounts. Strip
    // inherited ACEs and grant only the current user.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let username =
        std::env::var("USERNAME").map_err(|_| "resolve current user name".to_string())?;
    let output = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{username}:F"))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("run icacls: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "icacls failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn lock_key_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}
```

Add the `ProcessManager` methods. Put them inside the existing `impl ProcessManager` block, directly after `spawn_ssh_session` (which ends at `src/services/process_manager.rs:1903`):

```rust
    fn materialize_ssh_key(&self, connection: &SSHConnection) -> Result<Option<PathBuf>, String> {
        let dir = crate::persistence::app_config_dir()
            .map_err(|error| format!("resolve config dir: {error}"))?
            .join("ssh-keys");
        materialize_ssh_key_in(&dir, connection)
    }

    /// Best-effort cleanup when a connection is deleted or its key cleared.
    /// Materialized files are permission-locked, so a missed delete is low risk.
    pub fn remove_materialized_ssh_key(connection_id: &str) {
        let Ok(dir) = crate::persistence::app_config_dir() else {
            return;
        };
        let _ = std::fs::remove_file(
            dir.join("ssh-keys").join(safe_key_file_name(connection_id)),
        );
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib services::process_manager`
Expected: PASS, including the 6 new tests. (`remove_materialized_ssh_key` and `materialize_ssh_key` will warn as unused until Tasks 3-4 — that is expected; if the build treats these as errors, add `#[allow(dead_code)]` on both and remove it in the task that wires them.)

- [ ] **Step 5: Commit**

```bash
git add src/services/process_manager.rs
git commit -m "feat: materialize pasted SSH keys to locked-down files"
```

---

### Task 3: Wire the key into the ssh launch args

**Files:**
- Modify: `src/services/process_manager.rs:2539-2562` (`build_ssh_launch_spec`)
- Modify: `src/services/process_manager.rs:836-900` (`ensure_ssh_session_for_tab`)
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: `materialize_ssh_key` / `sanitize_private_key` from Task 2; `AppState::default()` (public `config` field) and `SessionTab` (derives `Default`) for tests.
- Produces: `fn build_ssh_launch_spec(app_state: &AppState, tab: &SessionTab, connection: &SSHConnection, key_file: Option<&Path>) -> SshLaunchSpec`. Behavior later tasks rely on: args are `[user@host, "-p", port]` plus `["-i", <path>]` when a key file exists.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/services/process_manager.rs`:

```rust
    fn ssh_test_connection() -> SSHConnection {
        SSHConnection {
            id: "ssh-1".to_string(),
            label: "Prod".to_string(),
            host: "example.com".to_string(),
            port: 2222,
            username: "deploy".to_string(),
            password: None,
            private_key: None,
        }
    }

    fn ssh_test_tab() -> SessionTab {
        SessionTab {
            id: "ssh-tab-1".to_string(),
            tab_type: TabType::Ssh,
            project_id: "project-1".to_string(),
            ssh_connection_id: Some("ssh-1".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn build_ssh_launch_spec_without_key_omits_identity_flag() {
        let state = AppState::default();

        let launch = build_ssh_launch_spec(&state, &ssh_test_tab(), &ssh_test_connection(), None);

        assert_eq!(launch.program, "ssh");
        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_launch_spec_with_key_appends_identity_flag() {
        let state = AppState::default();
        let key_file = PathBuf::from("/keys/ssh-1");

        let launch = build_ssh_launch_spec(
            &state,
            &ssh_test_tab(),
            &ssh_test_connection(),
            Some(key_file.as_path()),
        );

        assert_eq!(
            launch.args,
            vec![
                "deploy@example.com".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-i".to_string(),
                key_file.display().to_string(),
            ]
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib services::process_manager`
Expected: COMPILE ERROR — `build_ssh_launch_spec` takes 3 arguments but 4 were supplied.

- [ ] **Step 3: Update `build_ssh_launch_spec` and its caller**

Replace `build_ssh_launch_spec` (`:2539`):

```rust
fn build_ssh_launch_spec(
    app_state: &AppState,
    tab: &SessionTab,
    connection: &SSHConnection,
    key_file: Option<&Path>,
) -> SshLaunchSpec {
    let cwd = app_state
        .find_project(&tab.project_id)
        .map(|project| PathBuf::from(&project.root_path))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

    let mut args = vec![
        format!("{}@{}", connection.username.trim(), connection.host.trim()),
        "-p".to_string(),
        connection.port.to_string(),
    ];
    if let Some(key_file) = key_file {
        // No `-o IdentitiesOnly=yes` on purpose: the user prefers the saved
        // key but still wants agent/default keys as fallback.
        args.push("-i".to_string());
        args.push(key_file.display().to_string());
    }

    SshLaunchSpec {
        tab_id: tab.id.clone(),
        ssh_connection_id: connection.id.clone(),
        project_id: tab.project_id.clone(),
        cwd,
        args,
        program: "ssh".to_string(),
    }
}
```

In `ensure_ssh_session_for_tab`, replace the single line at `:877-878`:

```rust
        let session_id = next_ssh_session_id(&connection_id);
        let launch = build_ssh_launch_spec(app_state, &tab, &connection);
```

with:

```rust
        let session_id = next_ssh_session_id(&connection_id);
        let (key_file, key_error) = match self.materialize_ssh_key(&connection) {
            Ok(path) => (path, None),
            Err(error) => (None, Some(error)),
        };
        let launch = build_ssh_launch_spec(app_state, &tab, &connection, key_file.as_deref());
```

and after the successful spawn (the existing `self.spawn_ssh_session(&launch, &session_id, dimensions)?;` at `:895`), surface the best-effort notice inside the terminal:

```rust
        self.spawn_ssh_session(&launch, &session_id, dimensions)?;
        if let Some(error) = key_error {
            let _ = self.write_virtual_text(
                &session_id,
                &format!(
                    "[devmanager] Couldn't prepare the saved SSH key ({error}); trying password/agent auth instead.\r\n"
                ),
            );
        }
```

If Task 2 added `#[allow(dead_code)]` to `materialize_ssh_key`, remove it now.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib services::process_manager`
Expected: PASS including both new tests.

Run: `cargo test`
Expected: full suite passes (the `tests/ssh_restore.rs` fixtures construct `SshLaunchSpec` directly with `args`, unaffected by the new parameter).

- [ ] **Step 5: Commit**

```bash
git add src/services/process_manager.rs
git commit -m "feat: pass materialized SSH key via -i in launch args"
```

---

### Task 4: Private-key field in the SSH editor UI

**Files:**
- Modify: `src/workspace/mod.rs:1297-1304` (`SshDraft`), `:1393-1399` (`SshField`), `:1060-1064` and `:1148-1152` (text accessors), `:1320-1325` (`allows_newlines`), `:920-945` (SSH summary rows), `:3725-3733` (`sample_ssh_draft`), `:4705-4710` (`render_ssh_panel` Authentication section)
- Modify: `src/app/mod.rs:4896-4908` (`open_add_ssh_action`), `:4910-4927` (`open_edit_ssh_action`), `:5231-5241` (save conversion), `:5630-5674` (`delete_ssh_action`)

**Interfaces:**
- Consumes: `SSHConnection.private_key` (Task 1), `ProcessManager::remove_materialized_ssh_key` (Task 2), `normalize_optional_string` (`src/app/mod.rs:11836` — note it trims, which is fine: materialization re-adds the trailing newline), `FormField::multiline(label, hint, value, field)` (`src/workspace/editor_ui.rs:121`).
- Produces: `SshDraft.key_text: String`, `SshField::KeyText`.

- [ ] **Step 1: Add the draft field and enum variant**

In `src/workspace/mod.rs`:

`SshDraft` (`:1297`) — add `key_text` after `password`:

```rust
pub struct SshDraft {
    pub existing_id: Option<String>,
    pub label: String,
    pub host: String,
    pub port_text: String,
    pub username: String,
    pub password: String,
    pub key_text: String,
}
```

`SshField` (`:1393`) — add the variant:

```rust
pub enum SshField {
    Label,
    Host,
    Port,
    Username,
    Password,
    KeyText,
}
```

- [ ] **Step 2: Wire accessors, newline support, form field, and summary**

All in `src/workspace/mod.rs`:

1. In the read accessor match (`:1060-1064`), add after the `Password` arm:

```rust
            (Self::Ssh(draft), EditorField::Ssh(SshField::KeyText)) => Some(&draft.key_text),
```

2. In the mutable accessor match (`:1148-1152`), add after the `Password` arm:

```rust
            (Self::Ssh(draft), EditorField::Ssh(SshField::KeyText)) => Some(&mut draft.key_text),
```

3. `allows_newlines` (`:1320`) — pasted keys are multi-line:

```rust
    pub fn allows_newlines(self) -> bool {
        matches!(
            self,
            Self::Project(ProjectField::Notes)
                | Self::Folder(FolderField::EnvContents)
                | Self::Ssh(SshField::KeyText)
        )
    }
```

4. `render_ssh_panel` (`:4705`) — replace the single-field Authentication section:

```rust
            FormSection::new("Authentication").fields(vec![
                FormField::text(
                    "Password",
                    "Leave blank if you use keys or an agent.",
                    draft.password.clone(),
                    EditorField::Ssh(SshField::Password),
                ),
                FormField::multiline(
                    "Private key",
                    "Paste your private key (OpenSSH or PEM). Used before the password when set.",
                    draft.key_text.clone(),
                    EditorField::Ssh(SshField::KeyText),
                ),
            ]),
```

5. SSH summary rows (`:937-944`) — add a "Key" row after the "Password" row (never show material):

```rust
                (
                    "Key".to_string(),
                    if draft.key_text.trim().is_empty() {
                        "Not saved".to_string()
                    } else {
                        "Saved".to_string()
                    },
                ),
```

6. `sample_ssh_draft` (`:3725`) — add `key_text: String::new(),` after `password: String::new(),`.

- [ ] **Step 3: Wire the app-side draft conversions and cleanup**

All in `src/app/mod.rs`:

1. `open_add_ssh_action` (`:4896`) — add `key_text: String::new(),` after `password: String::new(),`.

2. `open_edit_ssh_action` (`:4910`) — add after the `password:` line:

```rust
                    key_text: connection.private_key.unwrap_or_default(),
```

3. Save conversion (`:5231`) — replace the Task 1 placeholder `private_key: None,` with:

```rust
                    private_key: normalize_optional_string(&draft.key_text),
```

and immediately after the `let connection = SSHConnection { ... };` literal, add best-effort cleanup for a cleared key:

```rust
                if connection.private_key.is_none() {
                    ProcessManager::remove_materialized_ssh_key(&connection.id);
                }
```

(`ProcessManager` is already imported in `app/mod.rs`.)

4. `delete_ssh_action` (`:5630`) — in the local branch, right after `self.state.remove_ssh_connection(connection_id);` (`:5668`), add:

```rust
        ProcessManager::remove_materialized_ssh_key(connection_id);
```

If Task 2 added `#[allow(dead_code)]` to `remove_materialized_ssh_key`, remove it now.

- [ ] **Step 4: Build and run the full suite**

Run: `cargo test`
Expected: PASS. The UI layer has no direct unit tests for form fields; coverage comes from the compile-time exhaustive matches plus the round-trip/materialization/launch tests from Tasks 1-3.

- [ ] **Step 5: Commit**

```bash
git add src/workspace/mod.rs src/app/mod.rs
git commit -m "feat: private key paste field in SSH connection editor"
```

---

### Task 5: Always-visible terminal scrollbar

**Files:**
- Modify: `src/app/mod.rs:6891-6927` (`terminal_scrollbar_model`, `terminal_has_scrollbar`), `:6929-6934` (`terminal_scrollbar_geometry` gate)
- Test: existing `mod tests` in `src/app/mod.rs` (helpers `screen_from_lines` at `:13535`)

**Interfaces:**
- Consumes: `scrollbar_thumb_top_ratio(display_offset, max_offset)` (`src/app/mod.rs:12868`, already handles `max_offset == 0`), `view::TerminalScrollbarModel { thumb_top_ratio, thumb_height_ratio }` (`src/terminal/view.rs:147`), test helper `screen_from_lines`.
- Produces: `fn scrollbar_model_for_screen(screen: &crate::terminal::session::TerminalScreenSnapshot, drag_thumb_top_ratio: Option<f32>, enabled: bool) -> Option<view::TerminalScrollbarModel>` (free fn). `terminal_has_scrollbar` is deleted.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app/mod.rs` (near the other scrollbar-adjacent tests around `:13580`):

```rust
    #[test]
    fn scrollbar_model_shows_full_height_thumb_without_history() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 2;
        screen.history_size = 0;
        screen.display_offset = 0;

        let model = scrollbar_model_for_screen(&screen, None, true).expect("model");

        assert_eq!(model.thumb_height_ratio, 1.0);
    }

    #[test]
    fn scrollbar_model_hidden_when_setting_disabled() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 20;
        screen.history_size = 18;

        assert!(scrollbar_model_for_screen(&screen, None, false).is_none());
    }

    #[test]
    fn scrollbar_model_keeps_proportional_thumb_with_history() {
        let mut screen = screen_from_lines(&["one", "two"]);
        screen.total_lines = 8;
        screen.history_size = 6;
        screen.display_offset = 0;

        let model = scrollbar_model_for_screen(&screen, None, true).expect("model");

        assert_eq!(model.thumb_height_ratio, 0.25);
        assert_eq!(model.thumb_top_ratio, 1.0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib scrollbar_model`
Expected: COMPILE ERROR — `cannot find function scrollbar_model_for_screen`.

- [ ] **Step 3: Extract the pure function and relax the gates**

In `src/app/mod.rs`, replace `terminal_scrollbar_model` (`:6891-6919`) and DELETE `terminal_has_scrollbar` (`:6921-6927`):

```rust
    fn terminal_scrollbar_model(
        &self,
        session: Option<&crate::terminal::session::TerminalSessionView>,
    ) -> Option<view::TerminalScrollbarModel> {
        let session = session?;
        scrollbar_model_for_screen(
            &session.screen,
            self.terminal_scrollbar_drag.map(|drag| drag.thumb_top_ratio),
            self.state.settings().show_terminal_scrollbar,
        )
    }
```

Add the free function next to `scrollbar_thumb_top_ratio` (`:12868`):

```rust
/// Pure scrollbar math shared by render and tests. With no scrollback
/// (alt-screen apps, fresh sessions) this intentionally returns a
/// full-height inert thumb instead of `None`, so the gutter stays visible
/// whenever the setting is on — matching Windows Terminal.
fn scrollbar_model_for_screen(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    drag_thumb_top_ratio: Option<f32>,
    enabled: bool,
) -> Option<view::TerminalScrollbarModel> {
    if !enabled {
        return None;
    }

    let total_lines = screen.total_lines.max(screen.rows.max(1));
    let visible_lines = screen.rows.max(1);
    let max_offset = screen.history_size.max(1);
    let thumb_height_ratio = visible_lines as f32 / total_lines as f32;
    let thumb_top_ratio = drag_thumb_top_ratio
        .unwrap_or_else(|| scrollbar_thumb_top_ratio(screen.display_offset, max_offset));

    Some(view::TerminalScrollbarModel {
        thumb_top_ratio: thumb_top_ratio.clamp(0.0, 1.0),
        thumb_height_ratio,
    })
}
```

In `terminal_scrollbar_geometry` (`:6929`), replace the deleted-function gate:

```rust
        if !self.terminal_has_scrollbar(session) {
            return None;
        }
```

with:

```rust
        if !self.state.settings().show_terminal_scrollbar {
            return None;
        }
```

If the `session` parameter of `terminal_scrollbar_geometry` is still used elsewhere in its body (it is — for history/offset math), leave the signature alone.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib scrollbar_model`
Expected: 3 new tests PASS.

Run: `cargo test`
Expected: full suite passes; no remaining references to `terminal_has_scrollbar` (`grep -n terminal_has_scrollbar src/ -r` returns nothing).

- [ ] **Step 5: Commit**

```bash
git add src/app/mod.rs
git commit -m "feat: keep terminal scrollbar visible with inert thumb when no scrollback"
```

---

### Task 6: Full verification and manual QA

**Files:** none new — verification only.

- [ ] **Step 1: Run the full test suite and lint**

Run: `cargo test`
Expected: all tests pass.

Run: `cargo clippy --all-targets 2>&1 | tail -20`
Expected: no NEW warnings in the files this branch touched (`src/models/config.rs`, `src/services/process_manager.rs`, `src/workspace/mod.rs`, `src/app/mod.rs`). Pre-existing warnings elsewhere are out of scope (see `docs/superpowers/specs/2026-04-16-rust-lint-baseline-design.md`).

- [ ] **Step 2: Manual QA in the running app**

The user runs DevManager continuously; a debug build can run side by side but shares the config dir, so prefer the watcher build only when the user is ready, or coordinate with the user to test in their instance after release. Manual checks:

1. **Scrollbar**: open a Claude tab (alt-screen) — scrollbar gutter shows a full-height thumb. Open a shell tab, generate output (`ls -R` a few times) — thumb shrinks and scrolls as before. Toggle Settings → "show terminal scrollbar" off — gutter disappears.
2. **SSH key**: edit an SSH connection, paste a throwaway private key, save. Reopen the editor — key field shows the pasted text; summary says "Saved". Connect — `%APPDATA%/com.userfirst.devmanager/ssh-keys/<id>` exists with LF endings and a trailing newline; on a host with that key installed, login proceeds with no password prompt.
3. **Fallback**: point a connection with both key and password at a host that rejects the key — the password prompt appears and the saved password auto-injects (existing behavior).
4. **Cleanup**: delete the connection — the `ssh-keys/<id>` file is gone.

- [ ] **Step 3: Update the memory index if the terminal-loss investigation was affected**

Not expected — this branch does not touch the PTY read loop. Skip unless something surfaced.

- [ ] **Step 4: Final commit (if QA produced fixes)**

```bash
git add -A
git commit -m "fix: address manual QA findings for ssh key auth and scrollbar"
```

Otherwise nothing to commit — the branch is ready for the finishing-a-development-branch flow.
