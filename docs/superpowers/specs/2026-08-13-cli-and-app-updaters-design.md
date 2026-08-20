# Silent CLI Refresh and Native DevManager Updater

## Problem

DevManager often fails to **find** stock Claude Code and Codex even when they
are installed and on PATH. npm hardlinked natives, a sibling `claude.ps1` /
`codex.ps1`, and the global-prefix `codex.cmd` layout (`node_modules\@openai\...`
rather than `.bin\..\@openai\...`) were treated as CheckFailed / not found.
Until Refresh shows those CLIs, a CLI updater and “always latest” are
pointless.

People also want those CLIs on **latest** without wrapping the agent in
`npx -y @latest`. DevManager must still launch an attested `claude` / `codex`
on PATH so identity, auth, and exact-resume stay honest. Sub-agents look up
the other CLI on PATH, so a private DevManager-only install folder would
break `claude` calling `codex` and the reverse.

Separately, the installed **0.4.1** app updater works: it shows the current
version, checks and downloads in the background, and a clickable control
installs and restarts. The native shell starts `UpdaterService` but never
paints that chrome or wires the click. Dev/smoke builds without embedded
update endpoints stay disabled; that is expected. Production must look and
act like 0.4.1 again.

## Goals

- **First:** Refresh finds stock PATH Claude Code and Codex (npm global or
  `.bin` shims, native `claude.exe`, sibling `.ps1` ignored). This is a
  merge gate for the rest of the spec. The running binary the user actually
  opens must include it; the installed 0.4.1 build does not.
- Keep the global/PATH Claude and Codex installs current without interrupting
  a live PTY.
- Every **new process** (new chat, user-restarted chat, DevManager restart)
  launches the latest attested CLI on PATH.
- Sub-agents keep resolving `claude` and `codex` through PATH. Do not
  hardcode NVM or a hidden DevManager bin as the source of truth.
- Restore the 0.4.1 DevManager **app** updater in the native header: show
  version, auto-check, auto-download, clickable update, install and restart.
- Claude/Codex CLI updates are **silent**. They never use the app-updater
  chrome and never produce “Could not check agents.”

## Non-goals

- Launching the agent as `npx`, `npm`, or any other forbidden runner.
- Auto-stopping a live Claude/Codex process because a newer CLI exists.
- Recreating 0.4.1’s old `src/app` window. The native shell is the product
  chrome; it must host the same updater **behavior**.
- A first-run wizard that installs Node, NVM, or the CLIs. Missing CLIs stay
  “not found / install then Refresh.”
- Changing signed-release identity (`latest.json`, pubkey, packager
  installer, host drain handoff). Reuse 0.4.1’s updater machine.
- Showing Claude/Codex version toasts, connect-canvas “updating…”, or
  Settings noise for CLI fetches.

## Terminology

| Word | Meaning |
| --- | --- |
| **CLI updater** | Background refresh of Claude Code and Codex on PATH. |
| **App updater** | Signed DevManager release check/download/install (0.4.1). |
| **Live PTY** | The currently running Claude or Codex process for a tab. |
| **New process** | A newly spawned CLI: new task, user restart of that chat, or a new generation after DevManager itself restarts. |
| **PATH install** | The attested `claude` / `codex` discovery already uses: whatever PATH entry the vendor put on the machine (official Node `%APPDATA%\npm`, NVM prefix, native `claude.exe`, etc.). |

## Design

### Slice 0: PATH discovery (must ship first)

Stock Windows npm installs must resolve from PATH:

- Native `claude.exe` may be hardlinked (npm/pnpm). Identity is volume/file
  index, not `link_count == 1`.
- `claude.cmd` / `codex.cmd` may sit beside a stock npm `*.ps1`. A sibling
  that fails shim proof is skipped; it does not abort the whole PATH scan.
- Codex global prefix shims launch
  `"%dp0%\node_modules\@openai\codex\bin\codex.js"`. Attest and parse that
  layout as well as the `.bin` `..\@openai\...` layout.
- Discovery never assumes NVM. Tests must not require `C:\nvm4w\nodejs`.

Connect/Settings after this slice: Claude and Codex are NotFound only when
PATH has no attested candidate; CheckFailed is not used for the npm layouts
above. Auth still requires `AuthenticatedSubscription` for “signed in.”

The implementation train is: discovery in the smoke/live binary the user
launches → then CLI updater → then native app-updater chrome. Do not start
the two-hour CLI fetch until Refresh can see both tools on a machine that
has them.

### Two updaters

CLI updater and app updater share no UI and no schedule. A CLI fetch must not
block Refresh, connect, or +Claude / +Codex. An app update **does** restart
DevManager; after that, tabs exact-resume with `providerSessionId` on whatever
CLI is now latest on PATH.

### CLI updater (silent)

**When.** Once shortly after the host is up, then every **two hours** while
DevManager is open. Refresh does not wait on it.

**What.** Update the install already discovered on PATH, with that install’s
own updater:

- Claude: `claude update` on that same binary (npm or native).
- Codex: `npm install -g @openai/codex@latest` using the `npm`/`node` that
  belong to that install (`npm prefix -g` for that Node). If there is no npm,
  skip Codex until a PATH Codex exists.

Never hardcode `C:\nvm4w\nodejs`, `%APPDATA%\npm`, or a DevManager-owned
package tree as the canonical home. Discovery stays PATH-first.

**Live PTY.** Do not stop it, do not rewrite its environment, do not
exact-resume it for a CLI bump. Overnight loops and a waiting question stay
put.

**New process.** Rediscover PATH (and any side-by-side prefix from a finished
fetch) and launch that attested latest. Exact-resume the tab’s
`providerSessionId` when this is the same conversation. If SessionStart does
not match, fail visibly; do not open a different conversation.

**Locked file.** If Windows holds `claude.exe` / `codex.exe` because a session
is running, leave that file alone and retry later. For **new** launches only,
if latest can be fetched without replacing the locked file, prepend **that
copy’s directory** to the new process PATH and keep the rest of PATH so the
other agent still resolves wherever it already lives. Do not require both
tools in one folder.

**Failure.** Keep the last good PATH install. Do not surface CLI fetch errors
as CheckFailed / “Could not check agents.”

**Sub-agents.** Children inherit the launched process PATH. They must find
`claude` and `codex` the same way a terminal would. A private directory that
is not on that PATH is invalid.

### App updater (exactly 0.4.1)

Reuse `UpdaterService`: embedded or env `DEVMANAGER_UPDATE_ENDPOINTS` +
`DEVMANAGER_UPDATE_PUBKEY`, background check (~30 minutes), auto-download of
the signed artifact, host drain, packager install, quit.

Native header chrome (missing today):

| Stage | What the user sees |
| --- | --- |
| Up to date / idle (configured) | Current version, e.g. `v0.4.1` |
| Checking | `Checking for updates` |
| Update found, download starting | `Update {version} found. Starting download...` |
| Downloading | `Downloading update {percent}%` |
| Ready | Clickable **Restart to update {version}** |
| Installing | `Installing update...` |
| Error | `Update failed` |
| Disabled / not configured | No updater chrome (dev/smoke without endpoints) |

The clickable control is the ready state. Click runs the existing install
handoff: admit, drain host/resources, launch the verified installer, exit.
That is “clickable update available, then auto install and restart.” Download
stays automatic; the click is the install/restart gate, as in 0.4.1.

Do not invent a second updater FSM. Wire `UpdaterSnapshot` into the native
header the way `src/app/chrome.rs` already maps stages to labels and the
ready-state click to install.

### Data flow

```
CLI (silent)
  host up → wait 2h loop
    → discover PATH claude/codex
    → vendor update (skip if locked)
    → next new process rediscovers PATH

App (0.4.1)
  configured? no → hide chrome
  yes → show v{current}
    → background check ~30m
    → auto-download
    → click Restart to update
    → drain → installer → quit
    → relaunch exact-resumes tabs on latest CLI
```

### Error handling

- CLI update errors stay in logs / updater-internal detail only. Agent
  connection mapping is unchanged: missing CLI is NotFound; probe/auth
  failures are not “update failed.”
- App updater errors use the 0.4.1 `Update failed` label. They do not disable
  agent connect.
- Exact-resume after an app restart uses `providerSessionId`. A CLI that
  cannot resume that id fails visibly.

### Testing

- Native header shows `v{version}` when the app updater is up to date;
  ReadyToInstall is clickable and dispatches the existing install action.
- Unconfigured updater paints no version/update control.
- CLI updater tests: PATH discovery is prefix-agnostic (global npm layout and
  `.bin` layout); a locked-file skip does not recycle a live session; a new
  launch after a successful fetch sees the new identity; sibling `.ps1`
  wrappers still do not fail the PATH scan.
- No test may require NVM or `C:\nvm4w\nodejs`.

## Implementation notes

The native shell already constructs `UpdaterService`, calls
`start_background_checks`, and copies snapshots into `TopBarModel.update`.
`header_bar` does not render `model.update` and does not bind install. That
paint/click gap is the app-updater slice.

The CLI updater is new host-owned work. It must not use `npx` and must not
share the app updater’s 30-minute thread.
