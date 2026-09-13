# Provider smoke and conformance

This document pins the Phase 4 stock-provider contract that
`scripts/native-next/Invoke-ProviderSmoke.ps1` and
`tests/provider_smoke.rs` check against committed fixtures. The runner is
non-invasive by default. It never sends a user prompt, never starts an
interactive provider session, never reads or writes production DevManager
config, and never touches real Claude, Codex, or Cursor conversations.

## Entrypoint and probe matrix

| Provider | Stock entrypoint | Read-only probes | Exact resume | Auth probe | Quota |
| --- | --- | --- | --- | --- | --- |
| Claude | `claude` | `--version`, `--help`, `auth status` | `--resume <id>` when help advertises `--resume` | Subscription only when `loggedIn=true` and `authMethod=claude.ai` | Unsupported; no official probe |
| Codex | `codex` | `--version`, `--help`, `login status`, `resume --help` | `resume <id>` only when resume help proves `SESSION_ID` | Subscription only when both `Logged in using ChatGPT` and `ChatGPT Plus subscription` are present | Unsupported; no official probe |
| Cursor | `cursor-agent` | `--version`, `--help` | Unsupported; interactive bare argv only | Unsupported; `auth_state` stays unknown | Unsupported; no official probe |

The authoritative argument shapes are `ProviderProbeKind::arguments()`:

- version: `--version`
- help: `--help`
- auth status: `auth status`
- login status: `login status`
- resume help: `resume --help`

Forbidden live or launch tokens: `exec`, `--print`, `-p`, `create-chat`,
`--continue`, `--last`. Cursor also rejects `cursor.exe` and wrapper
shells. Live mode never passes a prompt, never uses `--continue`/`--last`
or a resume ID, and never invokes `exec`, `--print`, `-p`, or
`create-chat`.

Committed sample output lives in `tests/fixtures/providers/{claude,codex,cursor}`.
The compact smoke matrix is `tests/fixtures/providers/smoke/matrix.json`.

## Subscription-only auth

API-key, token, or ambiguous login evidence never promotes to
`AuthenticatedSubscription`.

- Claude `auth_api_key.txt` (`authMethod=api-key`) and
  `auth_ambiguous.txt` stay `unknown`.
- Codex `login_status_api_key.txt` and `login_status_logged_in_only.txt`
  stay `unknown`.
- Cursor smoke (`phase4_11_smoke_contract.json`) sets `claims_auth=false`
  and `auth_state=Unknown`. The adapter does not probe Claude/Codex
  `auth status`.

## Exact resume and failure visibility

Exact-resume failure stays visible. The runner must not fall back to a
fresh conversation, `--continue`, `--last`, or any inferred session ID.

- Claude exact resume is `--resume <id>`. Fixture failures
  (`resume_not_found.txt`, `resume_incompatible.txt`,
  `resume_auth_failure.txt`) remain errors.
- Codex exact resume is `resume <id>`. `resume_help.txt` proves
  `SESSION_ID`. `resume_help_last_only.txt` does not prove exact resume.
- Cursor `build_launch` with a session ID is
  `UnsupportedCapability(ExactResume)` and must not open a fresh chat.

## Cursor unsupported capabilities

Cursor remains terminal-only until a later native ID and command are
both proven. Unsupported today: exact resume, semantic events, provider
session ID, cooperative stop, quota, and auth state. Live mode records
those as unsupported and does not invoke `auth status`, `login status`,
or `resume --help`.

## Quota unknown and refresh

Quota is never guessed or scraped from help, about, whoami, or account
text. Unless an official provider probe contract exists, smoke records
`unsupported`. Current Claude, Codex, and Cursor adapters have no such
probe. Stale UI quota still expires after one hour in the host; this
runner does not refresh or invent a remaining-percent value.

## Smoke commands and exit codes

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-ProviderSmoke.ps1
pwsh -NoProfile -File scripts/native-next/Invoke-ProviderSmoke.ps1 -Live -Provider claude_code -IAcknowledgeIsolatedNonproductionProfile
```

`-Authenticated` is accepted as an alias of `-Live` / `-Mode live`.
Default mode is fixture.

| Exit | Disposition | When |
| --- | --- | --- |
| 0 | `pass` | Every check passed |
| 2 | `hold` | Safe HOLD: missing optional CLI or unsupported live capability |
| 1 | `rejected` or `failed` | Policy rejection or a failed check |

The single bounded JSON result always includes `schemaVersion`, `mode`,
`providers`, `checks`, `launchedProviders` (false unless a session were
started; this runner never starts one), `residueCount`, and
`disposition`. Credentials, prompts, response text, absolute user paths,
and session IDs are redacted. Output is capped at 16 KiB.

## Isolation and residue

- Isolated profile defaults to
  `.devmanager-cutover-provider-smoke/profile` under the worktree.
- Explicit profiles must be fully qualified. Relative, drive-relative,
  and ambiguous paths are rejected.
- Production `%APPDATA%\com.userfirst.devmanager`, Chrome/Edge user-data
  roots, and `.claude` / `.codex` / `.cursor` profile roots are
  rejected.
- Live/authenticated mode refuses CI and noninteractive hosts, empty or
  unknown allowlists, and missing operator opt-in.
- Process environment keys including `DEVMANAGER_PROFILE` and provider
  API-key variables are cleared, replaced with an explicit
  non-production triple, and restored in `finally`.
- Each live probe owns one process tree, uses a bounded deadline and
  64 KiB output cap, inherits no shell, and must leave zero residue.

Fixture mode does not spawn providers, Cargo, hook relays, or Job
helpers.

## What live mode does not test

Live/probe mode only runs stock `--version` / `--help` /
`auth status` / `login status` / `resume --help` probes against
allowlisted entrypoints. It does not:

- send a user prompt or start an interactive session
- exact-resume a real conversation or prove resume success
- scrape quota or claim a remaining-percent value
- register host runtimes, attach PTYs, or exercise semantic journals
- read or write production `config.json` / `remote.json` / `session.json`
- launch `cursor.exe`, `exec`, `--print`, `-p`, or `create-chat`

Authenticated Task 4.11 session lifecycle remains a later operator-gated
path. This Phase 4 runner only proves command shape, resume/auth/quota
policy, and bounded read-only presence of optional CLIs.
