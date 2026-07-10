# SSH key-or-password authentication + always-visible terminal scrollbar — design

Date: 2026-07-06 (scrollbar section added 2026-07-09)
Status: Approved for planning

This spec covers two independent work items shipped on the same branch:
Part 1 — SSH key-or-password auth. Part 2 — always-visible terminal scrollbar (native app).

# Part 1: SSH key-or-password authentication

## Goal

Let an SSH connection authenticate with **either** a password **or** a private key,
storing room for both in config, and have DevManager automatically use whichever is
configured — with no auth-mode toggle for the user to manage.

**Key input decision:** the user **pastes the private key text** into DevManager (they do
not point at a file). DevManager stores the key material and writes a locked-down key file
for `ssh` at connect time.

## Current state (v0.2.45)

- `SSHConnection` (`src/models/config.rs:78`) stores `password: Option<String>`. No key field.
- Sessions spawn the real `ssh` binary through portable-pty with **no shell**. Args are built
  by `build_ssh_launch_spec` (`src/services/process_manager.rs:2539`, single caller at
  `:878`): `ssh <user>@<host> -p <port>`.
- Password auth already works via **prompt detection + auto-inject**: the app watches PTY
  output for a `…password:` prompt (`src/app/mod.rs:12807`, `ssh_password_prompt`) and types
  the stored password (`maybe_auto_submit_ssh_password`, `src/app/mod.rs:7242`; manual action
  `respond_to_ssh_prompt_action`, `:7307`).
- Editor already supports multi-line paste fields: `FormField::multiline` /
  `multiline_sized` (`src/workspace/editor_ui.rs:121`), gated by `allows_newlines()`
  (`src/workspace/mod.rs:1320`); Ctrl+V paste flows generically through
  `apply_text_key_to_string` (`src/app/mod.rs:6370`).
- Data dir resolver: `app_config_dir()` (`src/persistence/mod.rs:71`).

## Auto-use behavior matrix

| private_key | password | Result |
|-------------|----------|--------|
| set    | empty | Materialize key file → `ssh … -i <file>`; key/agent auth. |
| empty  | set   | Unchanged; prompt detection auto-injects the password. |
| set    | set   | `-i <file>` is primary; if the server rejects the key, ssh falls back to a password prompt and the **existing** auto-inject supplies the saved password. No extra code. |
| empty  | empty | Today's behavior: agent / default keys / interactive. |

Notes:
- Decision: pass **only** `-i <file>` — **no** `-o IdentitiesOnly=yes` — so the ssh agent /
  default keys still work as fallback ("prefer key, allow agent fallback").
- A passphrase-protected key prompts `Enter passphrase for key …:`, which the password
  injector deliberately does **not** match (it matches `password:` only). The login password
  can never leak into a passphrase prompt. Passphrase-protected keys stay the domain of
  ssh-agent and are **out of scope**.

## Changes

### 1. Model — `src/models/config.rs`
Add to `SSHConnection`:
```rust
pub private_key: Option<String>,   // JSON: "privateKey" — the key MATERIAL, not a path
```
`#[serde(default, rename_all = "camelCase")]` makes this backward/forward compatible
(old configs → `None`; old builds ignore the field).

### 2. Key materialization — `src/services/process_manager.rs`
New helper, called from `ensure_ssh_session_for_tab` (`:836`) **before** building the launch
spec so IO errors can be surfaced:
```
fn materialize_ssh_key(connection: &SSHConnection) -> Result<Option<PathBuf>, String>
```
- Returns `Ok(None)` when `private_key` is empty/absent.
- Otherwise:
  1. **Sanitize** the pasted text: normalize `\r\n`/`\r` → `\n`, trim leading/trailing blank
     lines, and ensure exactly one trailing `\n`. (OpenSSH rejects keys with CRLF or a missing
     final newline — a common paste failure.)
  2. Ensure dir `app_config_dir()?/ssh-keys/` exists; on Unix set the dir mode to `0700`.
  3. Write the sanitized key to `ssh-keys/<connection_id>` (stable per-connection path,
     overwritten each connect so it always matches the stored key).
  4. **Lock permissions** so `ssh` accepts the file:
     - Unix: `fs::set_permissions(path, 0o600)`.
     - Windows: run `icacls "<path>" /inheritance:r /grant:r "<USERNAME>:F"` to strip inherited
       ACEs and grant only the current user (Win32-OpenSSH refuses keys readable by others).
  5. Return `Ok(Some(path))`.

`build_ssh_launch_spec` gains a param `key_file: Option<&Path>`; when `Some`, it appends
`-i <path>` to the args. The function stays pure/IO-free and unit-testable.

Failure handling: if `materialize_ssh_key` returns `Err`, set a terminal notice
("Couldn't prepare SSH key: …") and connect **without** `-i` (best-effort — password/agent may
still succeed) rather than hard-failing the session.

### 3. Cleanup
- `remove_ssh_connection` (`src/state/app_state.rs:712`) and the save path when the key is
  cleared: best-effort delete `ssh-keys/<connection_id>`. Leftover files are locked-down, so a
  missed delete is low-risk, but we clean up on the obvious paths.

### 4. UI — `src/workspace/mod.rs` + `editor_ui.rs`
- `SshDraft` (`:1297`): add `pub key_text: String`.
- `SshField` (`:1393`): add `KeyText`.
- Field get/set accessors (`:1060–1064`, `:1148–1152`): add `KeyText` arms.
- `allows_newlines()` (`:1320`): add `Self::Ssh(SshField::KeyText)`.
- `render_ssh_panel` Authentication section (`:4705`): add a `FormField::multiline` "Private
  key" field. Helper text: *"Paste your private key (OpenSSH or PEM). Leave blank to use a
  password or your agent."* Keep the existing Password field.
- Editor summary (`:937`): add a "Key" row showing "Saved" / "Not saved" (never the material).

### 5. Draft ↔ connection — `src/app/mod.rs`
- `open_add_ssh_action` (`:4896`): `key_text: String::new()`.
- `open_edit_ssh_action` (`:4910`): `key_text: connection.private_key.unwrap_or_default()`.
- Save conversion (`:5231`): `private_key: normalize_optional_string(&draft.key_text)`
  (`normalize_optional_string` at `:11836`).
- Literal `SSHConnection` construction sites needing the field (or `..Default::default()`):
  `sample_ssh_connection` (`src/app/mod.rs:13073`), sidebar tests (`src/sidebar/mod.rs:1671`,
  `:1679`).

## Testing

- `sanitize_private_key`: CRLF→LF; missing trailing newline added; surrounding blank lines
  trimmed; already-clean key unchanged.
- `materialize_ssh_key`: writes sanitized content; returns the path; `#[cfg(unix)]` asserts
  mode `0600`; empty key → `Ok(None)`.
- `build_ssh_launch_spec`: `key_file = Some` → args contain `-i` then the path; `None` → no
  `-i`; base args (`user@host`, `-p`, port) unchanged.
- Config round-trip: `privateKey` survives serialize→deserialize; a legacy config without it
  loads as `None`.

## Security notes (surfaced, not silently accepted)

- The private key is now stored **in the config JSON in plaintext**, same exposure model as
  the existing plaintext password but a higher-value secret. The materialized key files live
  under the app config dir with locked-down permissions.
- Encrypting secrets at rest (password + key) is a worthwhile **follow-up**, out of scope here.

## Out of scope (YAGNI)

- Changing the password mechanism (already works).
- Key **passphrase** prompt handling (use ssh-agent).
- Pointing at an existing key **file** by path (rejected in favor of paste-the-text).
- `-o IdentitiesOnly=yes` strict mode (rejected in favor of agent fallback).
- Encryption at rest for stored secrets.

# Part 2: Always-visible terminal scrollbar (native app)

## Diagnosis (confirmed with user, 2026-07-09)

The terminal scrollbar is not a regression: it hides whenever the grid has no scrollback
(`total_lines <= rows`). Claude Code v2 runs its UI in the **alternate screen** (binary
contains `[?1049h`/`[?1049l`), which by definition has no scrollback — so Claude/Codex tabs
show no scrollbar while shell/server tabs still do. User confirmed: scrollbar appears in
shell/server tabs; the "missing" case is Claude tabs in the **native desktop app**.

## Behavior change

When `showTerminalScrollbar` is enabled, always render the scrollbar gutter. With no
scrollback (alt-screen apps, fresh sessions) show a **full-height inert thumb** — matching
Windows Terminal — instead of hiding the bar. With scrollback, behavior is unchanged.

## Changes (both in `src/app/mod.rs`)

1. `terminal_has_scrollbar` (`:6921`): drop the `total_lines > rows.max(1)` condition; gate
   on `settings.show_terminal_scrollbar` alone. Layout is unaffected — `available_width`
   (`:3549`) already reserves the gutter whenever the setting is on, so PTY cols don't change.
2. `terminal_scrollbar_model` (`:6891`): remove the `total_lines <= visible_lines → None`
   early return. Existing math then yields `thumb_height_ratio = 1.0` and a pinned thumb.

Safety, verified: `scrollbar_thumb_top_ratio` (`:12868`) handles `max_offset == 0`;
geometry clamps `max_offset` to ≥ 1; dragging an inert thumb resolves to display offset 0
(no-op). The web UI (xterm.js) surface is untouched.

## Testing

- Unit test: a session view with `total_lines == rows` (no history) yields
  `Some(TerminalScrollbarModel)` with `thumb_height_ratio == 1.0`, not `None` (harness
  patterns exist near `src/app/mod.rs:13580`).
- Unit test: a session with history keeps current proportional thumb behavior.
- Existing `scrollbar_thumb_top_ratio` behavior unchanged.

## Out of scope

- The browser web UI surface (user is on the native app; xterm.js hides its own scrollbar
  in alt screen — possible follow-up).
- Scrollback for alt-screen apps (nothing to scroll; Claude Code manages its own history).
- Disabled/dimmed styling for the inert thumb (keep existing colors).
