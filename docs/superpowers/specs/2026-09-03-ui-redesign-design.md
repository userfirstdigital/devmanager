# UI Redesign Design

**Date:** 2026-09-03
**Status:** Design validated in the visual companion, awaiting user review of this document
**Related:** `2026-08-26-native-multi-task-recursive-workspace-design.md` (the layout tree this design re-skins and adjusts), `2026-09-01-task-shell-terminals-design.md`, `.superpowers/sdd/2026-09-02-startup-latency/brief-scrollbars-everywhere.md`, `.superpowers/sdd/2026-09-02-startup-latency/brief-terminal-scroll-locality.md`
**Mockups:** `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/` holds the six reference screens as PNG (rendered at 1.5x) with the chosen variant at full brightness and tagged CHOSEN, the rejected variants dimmed for context, plus the standalone HTML each was rendered from. They are the acceptance reference, not an illustration: see section 12.

## 1. Problem

The main screen's job is supervising many agents at once: rarely more than eight tasks, each routinely with three or more subagents, across several projects, popping between conversations all day. The current cockpit does not serve that job:

- Task rows spend three lines on project, branch, agent and status, identical on every row, while the title, the only identifying content, is clipped mid-word.
- Panes centre one sentence in a 2,000 px column and pin the real content to the bottom.
- The action strip has no hierarchy: view switches and state-changing verbs sit at equal weight, with Delete beside Archive.
- Colour carries no meaning. Status is a dot you read rather than see.
- Nothing on a row says whether the task needs you, why, what it is doing now, how long it has been like that, or how far along it is.

This design replaces the board, the panel chrome, the state vocabulary and the colour system. It keeps the recursive split workspace from the 2026-08-26 design and adjusts its compact-pane rule. It does not touch the kernel, the host, or the terminal engine except where a new fact must be read from a provider (section 8).

## 2. Goals and non-goals

Goals:

- A board that answers, per task and at a glance: needs me and why; doing now; how long in this state; progress, when the agent has a plan.
- Panels whose chrome costs two rows, so the stream gets the height. Text is not wide; height is precious.
- One colour language: colour means "needs you". Everything else is near-monochrome, with a muted hue per project on the edge.
- Zoom one panel to the whole space with one key and no separate screen.
- Subagents of any provider presented the same way native Claude Code subagents are.
- A Done section that keeps finished tasks out of the way and brings them back only on purpose.

Non-goals:

- New kernel tables or migrations. Every fact shown is either already durable or is a live provider fact held in memory.
- Changing the recursive layout tree's data model beyond what section 6 names.
- The Connect mobile/web client. It keeps its current presentation.
- Theming beyond the dark theme. Tokens are named so a light theme can follow; no light values are designed here.
- Reading Codex plan events. That is a probe in sub-project 3, not a promise.

## 3. Composition

Chosen: composition A (`composition.html`).

The window is a left board column and a panel grid.

- **Board column**: 236 px wide at default density, resizable by dragging its right edge, collapsible to a 36 px rail showing only the state dots and counts. Never hidden entirely.
- **Panel grid**: the existing recursive split workspace. Panels are the task surfaces. There is no other task screen.
- **Zoom**: `Z` or the ⤢ control on a panel shows only that panel in the grid's full area, with identical chrome so nothing moves under your hands. `Esc` or `Z` returns. Zoom is the existing per-workspace `zoomed` idea from herdr, stored as a transient `Option<PaneId>` beside `focused`, never persisted as a layout change.

## 4. The board

Chosen: two-line rows in full-width boxes (`boxed-two-line.html` A), project stripe on the column's outer edge (`project-colour.html` 1), segments only where a plan exists (`stripe-progress.html`), grey provider mark on the second line (`provider-logos.html` 1).

### 4.1 Groups

Rows are grouped by state, in this fixed order, each group with an uppercase 10.5 px label and a count:

1. **Needs you**: a question awaiting an answer, a permission awaiting a decision, or the provider failed or refused (blocked).
2. **Working**: the provider is inside a turn.
3. **Idle**: the provider is between turns and the last message was the assistant's, or the task has never been started.
4. **Done**: collapsed by default, showing only the count and a disclosure chevron. Expanding it lists Done rows in one-line density (title and age only). Archived tasks are not on the board; they live behind the board header's ⋯ menu as "Archived…".

Within a group, rows sort by state age ascending for Needs you (oldest ask first, since it has waited longest) and by last activity descending for the others.

### 4.2 Row anatomy

Each row is a full-width box: a 1 px line above and below, no side margins, 3 px gap between rows. The box background is one step lighter than the column. The selected row is two steps lighter with a lighter border.

```
| 3px stripe | ● | title …………………………………………………………… | age |
|            |   | why / doing now …… ▮▮▮▯ 3/4 …………… ✱ |
```

- **Stripe**: 3 px on the row's left edge, full row height, in the project's colour (section 5.3).
- **State dot**: 7 px. Amber with a soft halo for Needs you (question or permission); red with a halo for blocked; mid-grey for Working; dark grey for Idle and Done.
- **Title**: 12.5 px, primary text, single line, ellipsis. White (`text.emphasis`) on question and permission rows; blocked rows keep primary text, as the reference image measures (`05-provider-mark-1.png`).
- **Age**: 10.5 px muted, right-aligned on the title line. Time in the current state: since the question was asked, since the turn started, since the last message. Formatted `12s`, `4m`, `2h`, `3d`.
- **Second line**: 10.5 px muted. Left: the *why* on Needs you rows ("Asked a question", "Permission: Bash", "Provider rejected", "Failed: exit 1"), or the *doing now* on Working rows (the current tool or command, bounded to 40 chars: "cargo test", "Editing purge.rs", "Thinking", "Reading host-stderr.log", "2 subagents"), or "Last reply 18m" on Idle rows. Right: progress segments and count when a plan exists (4.3), then the provider mark (4.4).

The project name is not written on the row; the stripe carries it, and the panel title carries it in full. Hovering a row shows a tooltip with project, branch, provider and the full title.

### 4.3 Progress segments

Shown only when the task has at least one plan step (the existing `ActivityKind::PlanStep` entries). One segment per step, 9 by 4 px, 2 px gaps: completed segments mid-grey, the in-progress segment light grey, pending segments dark. Beside them the count `3/5` in 10 px muted. A list that grows mid-run simply gains segments; nothing animates backwards. Rows with no plan show nothing in that space. There is never a smooth bar and never a percent.

On rows narrower than 200 px the count is dropped first, then the segments.

### 4.4 Provider mark

An 11 px monochrome glyph at the far right of the second line, in the muted text colour, one per `PrimaryProviderIcon` variant: Claude, Codex, Cursor, Other. The same glyph appears before the panel title and on subagent tabs. The repo has no such assets today (`assets/icons/` holds generic glyphs and the enum carries labels only), so sub-project 1 adds four SVGs under `assets/icons/provider-*.svg` with a stated origin for each: the vendor's published mark where its licence allows, otherwise a neutral stand-in shape. Tinted brand colours were rejected.

### 4.5 Board header

"Board" in 13 px semibold, a `+ New` button on the right, and a ⋯ menu holding Archived…, Collapse to rail, and Density (Comfortable / Compact, the latter shrinking row padding by 2 px per side).

`+ New` opens the existing new-task flow in a new panel beside the focused one.

## 5. Colour and type

### 5.1 The rule

Colour is reserved for state that needs you. Two hues carry it: amber `#f2b441` for "waiting on you" (question, permission) and red `#e5484d` for "broken" (provider failed or refused, task failed). They appear on the board dot and halo, the row's box border, the panel border and its glow, and the title-row status text. Nowhere else.

Everything else is a grey ramp:

| Token | Dark value | Use |
|---|---|---|
| `surface.column` | `#101013` | board column |
| `surface.row` | `#151518` | row box, panel body |
| `surface.row_selected` | `#1e1e23` | selected row, focused panel title row — selected row: filled slab, 5 px stripe, strong rules, white title (ruled `#26262b`; that drops `text_disabled_on_selection` to 4.118:1, so the fill sits at the 4.5:1 ceiling `#1e1e23` and `surface.disabled` moved down to keep the two distinct) |
| `surface.stream` | `#111114` | conversation stream background |
| `surface.terminal` | `#0b0b0d` | terminal background |
| `border.subtle` | `#26262b` | row lines, panel border |
| `border.strong` | `#34343c` | selected row, focused panel |
| `text.primary` | `#e6e6ea` | titles, stream text |
| `text.secondary` | `#9a9aa3` | status line |
| `text.muted` | `#86868f` | second line, ages, marks (spec drafted `#6b6b74`; the repo's 4.5:1 floor on raised forced `#86868f`, landed 2026-09-03) |
| `text.disabled` | `#85858e` | one step below muted; the disabled-text floor is also 4.5:1, so it cannot sit lower |
| `surface.disabled` | `#1b1b20` | disabled controls — one step below `surface.row_selected` so a selected row never reads as a disabled one; disabled text is 4.691:1 here |
| `status.inactive` | `#86868f` | inactive chips |
| `status.attention` | `#f2b441` | needs you |
| `status.blocked` | `#e5484d` | broken |
| `status.success` | `#7fb07f` | inline only: test passed, subagent live dot |
| `action.primary` | `#e6e6ea`, text `#101013` | the one loud control: New task, New project, primary confirm. Loud by inversion, never by hue. Hover `#ffffff`, pressed `#d0d0d6`, disabled `#606876` with `#f8fafc` text |
| `border.focus` | `#86868f` | focus ring on the workspace pane and every other control — a grey, not the warning yellow it used to borrow |

These map onto the existing `ThemeTokens` groups (`surfaces`, `borders`, `text`, `status`); the redesign changes values and adds `surface.stream`, `surface.terminal`, `border.strong`. Destructive actions are not red buttons; Delete is a menu item that opens a confirmation, and the confirmation's primary button is the only red button in the app.

### 5.2 Type

Segoe UI (system UI font on Windows) for chrome and conversation prose; Cascadia Mono for terminal, commands and paths. Sizes: 13 px panel titles (semibold), 12.5 px row titles and stream prose, 11.5 px status and tabs, 10.5 px second lines and group labels, 10 px counts. Line height 1.4 in chrome, 1.5 in the stream. Terminal line height stays the measured cell pitch from the terminal view, not a chrome constant.

### 5.3 Project colours

Each project gets one hue from a fixed palette of eight muted, cool colours of similar lightness (teal `#5aa3a0`, slate `#7a86c4`, sand `#a78a5c`, mauve `#8c6fa8`, moss `#7a9a6a`, dusk `#9a7a8a`, steel `#6f8fa8`, clay `#a8806f`), assigned at first sight in project creation order and persisted in the profile's workspace layout store as `project_colours: BTreeMap<ProjectId, u8>`. A project can be re-coloured from the project menu. Hues are chosen so amber and red remain the only saturated colours on screen.

## 6. Panels

Chosen: chrome option 2, status folded into the title row (`panel-chrome.html` 2), stripe on the panel's left edge (`densities-striped.html`).

### 6.1 Chrome

Two rows, then the view, then the composer:

```
| 3px stripe | ✱ Title ……………………… | ▶ cargo test · 12s ▮▮▮▮▮▯ 5/6 | ⤢ | Done | ⋯ |
|            | Conversation | Terminal | Files | Changes | Browser      | • code-reviewer | Explore |
|            | view …                                                                          |
|            | Message Claude…                                                    ⏎ send        |
```

- **Title row**: provider mark, title (semibold, ellipsis, takes the slack), inline status (secondary text; amber when needs-you; shows the state icon, the doing-now text, the age, and the progress segments), the zoom control, the one primary action, the ⋯ menu.
- **Tab row**: views on the left, subagent tabs on the right. The active tab has the row-selected background and primary text. Subagent tabs carry a 6 px dot: green while the subagent is live, dark once it has stopped.
- The status truncates before the title does when the panel is narrow; below 320 px the segments go, then the doing-now text, leaving the state icon and age.

### 6.2 Views

Tabs, in order: Conversation, Terminal, Files, Changes, Browser. Review, Artifacts and Services move to the ⋯ menu under "More views" and open as the panel's view when chosen. The task's chosen view persists per pane as it does today (`task_center_terminal` becomes a `view: PaneView` field, section 6.5).

Conversation is the existing timeline with the existing composer. The composer has one placeholder ("Message Claude…", with the provider's display name) and a right-aligned key hint; it never appears more than once per panel.

### 6.3 Primary action and menu

The one visible action is **Done** for open tasks and **Reopen** for Done tasks. Everything else is behind ⋯:

```
Add action          A
Commit              C
──────────────
Zoom                Z
Pin size            P
Move  ← ↑ ↓ →       ⇧⌘ arrows
Swap with…          S
──────────────
More views  ▸       Review · Artifacts · Services
Rename
Archive
Delete…                 (confirms)
```

The keys work when the panel is focused, without opening the menu.

### 6.4 Needs-you states

Chosen: question as a card in the stream (`needs-you.html` 1); permission as a docked one-liner (`needs-you.html`, lower row).

- **Question** (an `ConversationRow::Question` with no settled choice): the panel border turns amber with a 1 px glow, the title-row status reads "? Asked a question · 4m" in amber, and the question renders as a card at the end of the stream: an amber "Question" label, the prompt, the choices as numbered rows with the recommended one outlined more strongly, and a footer hint "1-3 pick · ⏎ send". Choices carry their full description text; the card scrolls with the stream, which is why it lives there and not in a dock. Number keys pick, the composer takes free text, and either settles the row. When the card is scrolled out of view the amber status is the reminder.
- **Permission** (a pending approval item): the composer is replaced by a docked card: "Allow?", the command or the file with its line delta in monospace, and three buttons: Allow (Enter), Always for this task, Deny (Esc). `D` opens the diff in the Changes view without answering. "Always for this task" grants the same tool for the remainder of the task through the existing per-task permission path; it never writes to the provider's global settings.
- **Blocked** (provider failed, refused, or exited non-zero): red border, red status text naming the cause from `TaskCockpitResult::Unavailable.detail`, and a "Retry" secondary action in the status line.

### 6.5 Layout adjustments to the 2026-08-26 design

The recursive tree, `Allocation::{Auto, Pinned}`, drag-to-pin, double-click-to-reset, collapse-on-close, edge moves and swaps all stand. Three changes:

1. **No distilled compact presentation.** Every panel shows its live view. `PanePresentation::CompactManual` is removed. `CompactAutomatic` remains only as the minimised strip: when allocation would give a panel less than its minimum (320 by 160 px), the least-recently-focused unpinned panel renders as its title row alone (mark, title, status, ⤢), 28 px tall, and expands again when room returns exactly as the 2026-08-26 design describes. The strip is the same title row, so nothing is redesigned for it.
2. **Zoom** is added as transient state (section 3).
3. **`TaskPane` gains `view: PaneView`** (`Conversation | Terminal | Files | Changes | Browser | Review | Artifacts | Services`) replacing the per-task `task_center_terminal` bool; schema v5 of `workspace_layout.rs` migrates `true` to `Terminal` and everything else to `Conversation`. Unknown values fail closed to `Conversation`.

Adding a task still inserts beside the focused pane into the nearest Auto split. Panels that were pinned keep their size; the rest reflow.

### 6.6 Done and lifecycle

- **Done** (button or `⌘D`): the task moves to the board's Done group and its panel closes; the split collapses so neighbours absorb the space. Its layout position is not remembered.
- A Done task returns to the board's live groups and gets a panel again only when reopened from the Done group, or when a message is sent to it (the existing restore-then-submit path). Nothing the provider does on its own reopens it.
- **Archive** removes the task from the board into the Archived list; **Delete…** confirms, then removes the task entirely (the purge that already exists).
- Closing a panel from the ⋯ menu is a local layout action; the task stays where it is on the board.

## 7. Subagents

Chosen: one tab per running subagent in the parent's panel (decision 10), same chrome, provider mark on the tab.

### 7.1 What a subagent tab shows

A subagent tab is a Conversation view scoped to that subagent's events: its tool calls, outputs and final message, with the same row rendering as the parent. The tab label is the subagent's type or name ("code-reviewer", "Explore"), the dot is green while live and dark when stopped, and stopped tabs stay until the parent's turn ends, then fold into a single "3 subagents · done" tab that expands on click. The parent's Conversation view shows a one-line "▸ code-reviewer · 40s" entry where the subagent ran, which jumps to its tab.

### 7.2 Parity contract

For a subagent to be first-class the provider adapter must deliver these facts, each with a stable subagent identity:

| Fact | Claude Code | Cursor | Codex |
|---|---|---|---|
| Start, with type/name | `SubagentStart` hook: `agent_id`, `agent_type` (verified) | unverified | unverified |
| Stop, with last message | `SubagentStop` hook: `agent_id`, `last_assistant_message`, `transcript_path` (verified) | unverified | unverified |
| Attribution of tool events to the subagent | `parent_tool_use_id` on hook payloads (verified) | unverified | unverified |
| Per-subagent transcript | file at `transcript_path` (verified) | unverified | unverified |

Today the semantic stream is flat per task and a subagent's work arrives as unattributed tool events in the parent's stream. Sub-project 4 adds `subagent_id: Option<String>` to `SemanticEventKind::Tool` events and to `ActivityEntry`, populated from `parent_tool_use_id` for Claude, and probes Cursor and Codex with the same script used for the progress probe. A provider that cannot attribute still gets the start/stop tab with the final message; its tool events stay in the parent's stream. The UI must render both cases without a separate code path.

## 8. Progress

Chosen: segments only where a plan exists; injection is a setting (decision 14).

### 8.1 Facts from the probe (2026-09-03, scratchpad `probe-*`)

- No agent makes a task list unprompted for a small task.
- Claude Code 2.1.259 has `TaskCreate`, `TaskUpdate`, `TaskList` but they are not in the default tool set for `-p` sessions; with them enabled and a standing instruction it created one task per step, moved each through in-progress to completed, and fired `TaskCreated` and `TaskCompleted` hooks with `task_id` and `task_subject`, the vocabulary the app already turns into plan steps. In-progress arrives only as `PostToolUse` for `TaskUpdate` with `status: in_progress`. Enabling the tools by replacing the tool list removed `Write` and cost a five-minute detour; the tools must be added, not substituted.
- Cursor agent, told, emitted `updateTodosToolCall` in its JSON stream with ids, content and pending / in-progress / completed states, re-sent whole on each change. The app does not read it.
- Codex 0.153, told, narrated "Task 1 completed" in prose and emitted nothing structured in `exec --json`. Whether `update_plan` reaches the app's Codex `PostToolUse` hook is unverified.

### 8.2 Settings

Under provider settings, per provider:

- **Keep a task list**: on / off. Default on for Claude Code and Cursor, off for Codex until 8.3 verifies it.
- **Instruction text**: editable, seeded with: "Before starting multi-step work, create a task list with one entry per step using your task tool, mark each entry in progress when you start it and completed when it is done, and add entries when you discover new steps."
- For Claude Code, "on" also adds `TaskCreate`, `TaskUpdate` and `TaskList` to the session's tool set through the existing launch-argument path, additively.

The instruction rides the existing `--append-system-prompt` path for Claude; for Cursor and Codex it goes wherever their adapters already place per-session instructions, and the settings page states which mechanism each uses.

### 8.3 Readers

- Claude: existing `TaskCreated` / `TaskCompleted` ingestion, plus `PostToolUse` for `TaskUpdate` to move a step to `Active`.
- Cursor: a new reader for `updateTodosToolCall` mapping `TODO_STATUS_PENDING / IN_PROGRESS / COMPLETED` to `PlanStepStatus::{Pending, Active, Completed}` keyed by the todo `id`; the whole list is replaced on each call.
- Codex: a probe through the app's own hook channel first. If `update_plan` arrives as `PostToolUse`, map its steps the same way; if not, the setting stays off and the row shows no segments.

## 9. Scrollbars, scrolling and loading

- Every scrollable surface uses the shared gpui-component `Scrollbar` styled per `brief-scrollbars-everywhere.md`: 4 px idle track that is visible, 10 px on hover or drag, no stepper arrows, the terminal included.
- Terminal scrolling is served from a client-side retained window per `brief-terminal-scroll-locality.md`; a redesigned panel that still paints at 10 fps under the wheel is not done.
- The startup pill and the per-task loading states from `brief-loading-ux.md` keep their behaviour; their visuals adopt the tokens in 5.1. A loading panel fills its whole area with the stream surface colour and a centred single-line status.

## 10. Keyboard model

Board: `↑ ↓` move the selection, `⏎` opens or focuses the panel, `N` new task, `/` search. Panel (when focused): `Z` zoom, `⌘D` done, `A` add action, `C` commit, `P` pin, `S` swap, `⇧⌘ arrows` move, `⌘ arrows` directional focus, `1-9` answer a question, `⏎` allow and `Esc` deny a permission, `D` view the diff, `⌘1..5` switch view tabs, `⌘[ ]` cycle subagent tabs. `Esc` also leaves zoom. No key does anything destructive without a confirmation.

## 11. Sub-projects and order

Each produces working, testable software on its own and gets its own plan.

1. **Board and tokens**: colour tokens, type, project colours, provider glyphs, the row model (`BoardRow { state, why, doing_now, age, progress: Option<(usize, usize)>, provider, project_colour }` derived from the existing snapshot), groups, rail, header. Replaces the inbox list. Measured by: a task in each state rendered from a fixture snapshot, pixel tests for the four states.
2. **Panel chrome and needs-you**: title row, tabs, menu, primary action, question card, permission dock, blocked state, zoom, minimised strip, `PaneView` and schema v5, Done lifecycle. Measured by: a question answered by number key from a panel; a permission allowed by Enter; zoom in and out leaving the tree unchanged.
3. **Progress**: settings, additive tool enablement for Claude, `TaskUpdate` in-progress reader, Cursor todo reader, Codex hook probe with a written verdict. Measured by: the probe script run against each provider through the app, segments appearing on the board within one second of the hook.
4. **Subagent parity**: `subagent_id` attribution, per-subagent tabs, fold-on-turn-end, Cursor and Codex probes with written verdicts. Depends on 2.
5. **Scrollbars and scroll locality**: the two existing briefs, unchanged, sequenced after 2 so they style the new surfaces once.

Order: 1, 2, 3, 5, 4. Sub-projects 3 and 5 are independent of each other and can run in parallel worktrees.

## 12. Verification

**The reference images are the acceptance criteria for appearance.** The risk this guards against is the one the user named: the same widgets as today in a different order. So every sub-project that changes what is on screen ends with a capture of the built UI, taken through the existing native preview capture (`src/ui/preview_capture.rs`, the path that produced `task-cockpit-ux-review-latest.png`), placed side by side with the matching reference PNG:

| Sub-project | Reference | What must match |
|---|---|---|
| 1 Board | `03-board-rows-boxed-A.png`, `04-project-stripe-1.png`, `05-provider-mark-1.png` | row anatomy, group labels, stripe on the column edge, segments only where a plan exists, grey provider mark, colours from 5.1 |
| 2 Panels | `01-composition-A.png`, `02-panel-chrome-2.png`, `06-needs-you-question-1-permission.png` | two-row chrome, tabs, one primary action, question card in the stream, permission dock, amber and red only on needs-you |

The reviewer of that sub-project's final diff receives both images and answers one question in writing: name every visible difference between the capture and the reference, and for each say whether the spec allows it. A capture that "has the same parts" but a different arrangement fails. Differences the spec explicitly allows: real data in place of the mock text, font hinting, and widths that follow the live window.


- Fixture snapshots covering every board state and every needs-you shape, rendered through the real projection code, with screenshot tests where the project already has them and element-tree assertions where it does not.
- The layout tree's existing property tests stay green; new tests cover zoom as a no-op on the tree, the minimised strip's threshold, and the v4 to v5 migration in both directions of the old bool.
- Each provider probe is a script under `scripts/probes/` that the plan runs and whose output is quoted in the report, not summarised.
- Before any sub-project is called done, the user launches the dev build themselves (never the agent) and confirms the behaviour the sub-project claims, per the standing rule.

## 13. Open questions

None blocking. Two facts are marked unverified in the tables above (Cursor and Codex subagent facts; Codex plan events) and each has a named probe in its sub-project rather than an assumption.
