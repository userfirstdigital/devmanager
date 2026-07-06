# SSH key-or-password authentication — design

Date: 2026-07-06
Status: Approved for planning

## Goal

Let an SSH connection authenticate with **either** a password **or** a private key,
storing room for both in config, and have DevManager automatically use whichever is
configured — with no mode toggle for the user to manage.

## Current state (as of v0.2.45)

- `SSHConnection` (`src/models/config.rs:78`) already stores `password: Option<String>`.
  There is **no** key field.
- Sessions spawn the real `ssh` binary through portable-pty (no shell), with args built
  by `build_ssh_launch_spec` (`src/services/process_manager.rs:2539`):
  `ssh <user>@<host> -p <port>`. The password is **not** on the command line.
- Password auth already works by **prompt detection + auto-inject**: the app watches PTY
  output for a `…password:` prompt (`src/app/mod.rs:12807`, `ssh_password_prompt`) and
  types the stored password via `maybe_auto_submit_ssh_password`
  (`src/app/mod.rs:7242`). A manual "send password" action exists too
  (`respond_to_ssh_prompt_action`, `src/app/mod.rs:7307`).
- SSH editor UI: `SshDraft` (`src/workspace/mod.rs:1297`), `SshField`
  (`src/workspace/mod.rs:1393`), `render_ssh_panel` (`src/workspace/mod.rs:4705`).

## Key realization

"Auto-use the correct method" is *mostly emergent*, not new machinery:

1. Adding `-i <keypath>` to the args is the only new launch logic.
2. Password injection already fires only on a `password:` prompt, so it needs no change.
3. `ssh`'s own auth fallback + the existing prompt detection produce the right behavior
   for every combination.

## Behavior matrix

| key_path | password | Result |
|----------|----------|--------|
| set      | empty    | `ssh … -i <key>`; key/agent auth; no password prompt expected. |
| empty    | set      | Unchanged today's behavior; prompt detection auto-injects password. |
| set      | set      | `ssh … -i <key>` is primary. If the server rejects the key, ssh falls back to a password prompt and the **existing** auto-inject supplies the saved password. Zero extra code. |
| empty    | empty    | Today's behavior: agent / default keys / interactive. |

Notes:
- Decision: when a key is set we pass **only** `-i <key>` (NO `-o IdentitiesOnly=yes`),
  so ssh may still try the agent/default keys — "prefer key, allow agent fallback."
- A passphrase-protected key prompts `Enter passphrase for key …:`, which the password
  injector deliberately does **not** match (it matches `password:` only). The saved
  login password therefore can never leak into a passphrase prompt. Passphrase-protected
  keys remain the domain of ssh-agent and are **out of scope**.

## Changes

### 1. Model — `src/models/config.rs`
Add to `SSHConnection`:
```rust
pub key_path: Option<String>,   // JSON: "keyPath"
```
Struct is `#[serde(default, rename_all = "camelCase")]`, so old configs load as `None`
and new configs are ignored by old builds — fully backward/forward compatible.

### 2. Launch — `src/services/process_manager.rs` (`build_ssh_launch_spec`, line 2539)
When `connection.key_path` is present and non-empty after trimming, append:
```
-i <expanded_path>
```
Path expansion: because `ssh` is spawned with no shell, a leading `~` or `~/` will not be
expanded by the shell. Add a small helper `expand_ssh_key_path(&str) -> String` that
replaces a leading `~` with `dirs::home_dir()` (the `dirs` crate is already a dependency),
and leaves everything else untouched. Absolute paths pass through unchanged. This is
deterministic across platforms (notably Windows, where Win32-OpenSSH's own `~` handling is
version-dependent).

### 3. UI — `src/workspace/mod.rs`
- `SshDraft`: add `pub key_path: String`.
- `SshField`: add `KeyPath`.
- Field get/set accessors (~lines 1060–1064 and 1148–1152): add `KeyPath` arms.
- `render_ssh_panel` Authentication section (line 4705): add a "Private key" text field
  above/below Password. Helper text: *"Path to a private key (e.g. ~/.ssh/id_ed25519).
  Leave blank to use a password or your agent."*
- Editor summary (~lines 937–944): add a "Key" row showing the path or "Not set".
- No change to the numeric-field predicate (line 1337) — key path is text.

### 4. Draft ↔ connection — `src/app/mod.rs`
- `open_add_ssh_action` (line 4896): `key_path: String::new()`.
- `open_edit_ssh_action` (line 4910): `key_path: connection.key_path.unwrap_or_default()`.
- Save conversion (line 5231): `key_path: normalize_optional_string(&draft.key_path)`.
- Literal construction sites that must gain the field (or switch to
  `..Default::default()`): `sample_ssh_connection` (`src/app/mod.rs:13073`), sidebar tests
  (`src/sidebar/mod.rs:1671`, `1679`).

## Testing

- Unit test `build_ssh_launch_spec` (module already has `#[cfg(test)]` tests in
  `process_manager.rs`): key set → args contain `-i` followed by the key path; key unset →
  no `-i`; `~/…` → expands to the home directory.
- Unit test `expand_ssh_key_path`: `~/foo` → `<home>/foo`; absolute path unchanged; empty
  unchanged; a bare `~` → home.
- Config round-trip: a fixture / assertion that `keyPath` survives serialize→deserialize and
  that a legacy config without `keyPath` loads as `None`.

## Out of scope (YAGNI)

- Changing the password mechanism (already works).
- Handling key **passphrase** prompts (use ssh-agent).
- An `rfd` file-picker button for the key path (text field is enough for v1; trivial
  follow-up).
- Encrypting the stored password at rest (pre-existing; unchanged by this work).
- `IdentitiesOnly=yes` strict mode (explicitly rejected in favor of agent fallback).
