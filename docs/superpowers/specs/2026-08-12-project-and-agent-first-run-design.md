# Project Language, First-Run Agent Gate, and Inbox Agent Actions

## Problem

The native shell mixes **folder** and **project** in user-facing copy, even
though the thing you add is a project. First-run also leads with a large
**Add folder** control and lets you add a project before Claude Code or Codex
is signed in. A new task then sits unbound: there is no primary agent, and
starting work is a separate, easy-to-miss step.

Connecting an agent is a machine setting (CLI present and signed in), not
something you invent after naming a task.

## Goals

- Use **project** in every user-facing string except the operating-system
  directory picker, which still says **folder** because it is choosing a
  directory.
- Replace large **Add folder** / **Add project** buttons with a small **+** on
  the project list.
- Require at least one signed-in Claude Code or Codex agent before the user
  can add a project.
- Keep **Settings** in the window chrome at all times; that is where agents
  are connected and re-checked.
- Replace **New task** / **Name this task** with **+Claude** and **+Codex**
  next to the inbox title. Creating work starts that agent immediately.
- Do not ask for a task title up front. Show a placeholder until the first
  substantive user message; allow rename afterward.

## Non-goals

- Changing how a project stores nested workspace roots internally (config
  still has project folders; the user never sees a **Folders** heading).
- Adding Cursor to the first-run connect gate or inbox **+** actions. Cursor
  auth remains unsupported.
- Building a full settings app (theme, keybindings, SSH, servers). This slice
  only needs agent connection status, refresh, and a path into sign-in help.
- Letting API-key or unknown auth start a session. Launch already requires a
  subscription receipt (`AuthenticatedSubscription`). The UI says **signed
  in**; it does not invent a weaker auth bar.
- Seeding projects or copying the workspace path into an empty profile.

## Terminology

| User-facing word | Meaning |
| --- | --- |
| **Project** | A named work root the user added to DevManager. |
| **Folder** | Only the OS directory picker. |
| **Agent** | Claude Code or Codex on this machine. |
| **Signed in** | The stock provider probe reports `AuthenticatedSubscription`. |
| **Connected** | At least one of Claude Code or Codex is signed in. |

Settings and first-run copy say Claude Code and Codex, not “LLM providers”.

## Design

### Language and chrome

User-visible strings that today say folder (palette **Add folder**, welcome
**Choose a folder**, overlay **Add this folder?**, sidebar **Folders**, header
**Add folder**) become **project**, except:

- the GPUI/OS path prompt, which still asks the user to choose a folder;
- confirm copy may say the chosen folder will be added as a project.

The project list heading is **Projects**. The only add control is a small **+**
on that heading, labeled for accessibility as **Add project**. It is absent
until an agent is connected.

Remove the large welcome/header/sidebar **Add folder** buttons. Palette
**Add project** stays available only when the **+** would be available.

**Settings** is always in the window header (not only on the empty canvas).
Opening it shows Claude Code and Codex: found or not, signed in or not, and
**Refresh**. It does not log the user in inside DevManager; it tells them to
sign in with that CLI and come back.

### First-run order

1. If no agent is connected, the main canvas is the connect screen. The
   project **+** is hidden. Existing projects, if any, still appear in the
   list.
2. Connect means: detect the CLI, probe auth, show **Refresh**. The same
   status appears in Settings.
3. Once at least one agent is signed in, the project **+** appears. If there
   is still no project, the canvas tells the user to add one.
4. After a project exists, the inbox is the main canvas. **+Claude** /
   **+Codex** appear only for signed-in agents.

Logging out later does not delete projects. The **+** actions for that agent
disappear. If no agent remains signed in, adding another project is blocked
again and the canvas returns to the connect screen (project list still
visible).

### Starting work

There is no **New task** button and no **Name this task** overlay on first
task.

The inbox title row has **+Claude** and/or **+Codex**. One click:

1. Creates a task in the selected project.
2. Starts a new conversation with that provider as the primary agent.

The kernel/host must allow that on a newly created task. Today a new task has
no primary agent and `action_epoch == 0`, which cannot launch. This slice
owns that seam: the user gesture must not leave an unbound task.

The domain still requires a non-empty title. The created title is a
placeholder such as **New Claude task** / **New Codex task**. The inbox may
treat that placeholder as untitled. After the first substantive user message,
the existing sticky semantic title replaces it. The user can rename later
with the existing task rename action. This slice does not add a name overlay.

If the selected project is missing, **+Claude** / **+Codex** are hidden (the
user is still on add-project). If a provider is not signed in, its **+** is
hidden, not shown disabled with developer jargon.

### Data flow

```
startup / Settings / Refresh
  → probe Claude Code and Codex (version + auth/login status)
  → connected = any AuthenticatedSubscription

connected?
  no  → connect canvas; hide project +
  yes → show project +

project selected?
  no  → add-project canvas
  yes → inbox with +Claude / +Codex for signed-in agents

+Claude / +Codex
  → task.create (placeholder title, selected project)
  → start provider session (new conversation, that provider)
  → bind as primary agent
```

Probes stay on the existing stock-provider contract. They never send a prompt
or start a session just to check sign-in.

### Errors

| Situation | User-facing result |
| --- | --- |
| CLI not found | Settings/connect: that app was not found on this machine. Install Claude Code or Codex, then Refresh. No in-app installer. |
| CLI found, not signed in | Sign in with Claude Code / Codex, then Refresh. |
| Probe failed | Could not check; Retry. Do not claim signed out. |
| +Claude click fails after create | Surface the failure on that task; do not pretend the agent started. |
| No project selected | Inbox agent **+** controls are not shown. |

Do not paint a signed-in failure as **Can't connect** (host transport). Agent
auth and host transport stay distinct.

### Testing

Cover copy and gates with GPUI headless tests, one `Application::headless().run()`
process at a time (existing `HEADLESS_SHELL_TEST_LOCK` rule):

- user-visible strings say **project**, not **Folders** / **Add folder**, except
  the OS picker prompt;
- project **+** is absent when no agent is connected, present when one is;
- Settings exists in header chrome on connect, first-project, and cockpit;
- inbox has no **New task** / **Name this task**;
- **+Claude** / **+Codex** match signed-in agents only;
- one **+Claude** (or **+Codex**) action creates a task and binds that provider
  as primary agent without a disconnect or missing `agent_session_id`;
- placeholder title is non-empty; first substantive user message can replace it.

Do not set `DEVMANAGER_PROFILE` for the full lib suite. Isolated live smoke
uses the existing `.devmanager-next` profile, not the installed app.

## Implementation notes

- `NativeShell::shows_add_folder_chrome` becomes a project-**+** visibility
  rule driven by connected-agent state, not only `ShellStage::Cockpit`.
- `ConfigSidebar::folder_section_title` becomes **Projects**.
- `OpenSettings` must open a real overlay, not only the current status string
  “Configuration settings selected”.
- Palette `AddProject` labels change from **Add folder** to **Add project**.
- First-task auto-open of **Name this task** is removed.

## Success

An average first run is: open Settings or the connect canvas → sign in to
Claude or Codex → Refresh → **+** a project (OS folder picker) → **+Claude**
or **+Codex** on the inbox → talk. No folder/project mix-up, no large add
button, no empty unbound task.
