# Phase 5: Native GPUI Task Cockpit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the terminal-tab-centric desktop with a polished native GPUI Task Cockpit whose default surface is a semantic conversation, while retaining a first-class raw terminal and making every task's files, changes, browser, services, artifacts, and review state immediately reachable.

**Architecture:** `devmanager-next` renders an immutable client projection supplied by `HostClient`; UI actions issue typed commands and never mutate kernel/runtime truth directly. A small tokenized component system built on pinned GPUI/gpui-component primitives supports dark/light themes, density, keyboard navigation, contrast, and deterministic preview fixtures. The central timeline uses one provider-neutral semantic renderer registry plus a safe generic fallback; Rust contracts/golden fixtures remain authoritative for later Connect web renderers. The shell has a Task Inbox, task header, central timeline/composer, and one context dock—no arbitrary pane canvas and no embedded web UI.

**Tech Stack:** GPUI 0.2.2, gpui-component 0.5.1 pinned exactly, Rust, existing terminal renderer, Markdown/code rendering using audited native crates, Windows screenshot/accessibility tooling.

## Global Constraints

- Desktop UI remains native GPUI throughout. Do not add Tauri, WebView/HTML for application chrome, React, or CSS-to-native translation.
- `gpui-component` is an Apache-2.0 dependency, pinned to `=0.5.1`; verify its v0.5.1 manifest still targets GPUI 0.2.2 before implementation. Record the source and license in `THIRD_PARTY_NOTICES.md`.
- Use gpui-component selectively for audited primitives, not as a second design system. DevManager owns tokens, task anatomy, action semantics, and performance budgets.
- UI models are projections. All task/provider/terminal/process/browser/service facts come from the host; client-local state is limited to navigation, focus, selection, draft, viewport, and preferences.
- Default navigation unit is Task, not terminal. One Task can have one Primary and optional agent/session/resource children.
- The context dock has one active tool at a time: Changes, Files, Terminal, Browser, Services, Artifacts, or Review.
- Raw terminal view subscribes to the same `AgentSessionId`/runtime generation as the semantic timeline; switching views launches nothing.
- Every action is defined once in `client::action::ActionCatalog`; GPUI contributes presentation bindings/shortcuts and the command palette shows disabled reasons from capabilities/current state.
- Navigation clicks are consumed and focus epochs from Phase 3 prevent click-through into questions, approvals, or terminal mouse reporting.
- Provider text, Markdown, tool labels, paths, URLs, terminal titles/links, browser titles, and remote display names are untrusted input: bound them, sanitize them, never execute embedded markup, and require confirmation for external links/actions.
- Unknown or malformed optional provider events render a bounded generic card with source/type/schema/status/redacted details. They never disappear, crash the Task, or become an approval/question/settlement/task transition.
- Renderer semantics live in Rust protocol/domain fixtures. TypeScript clients consume generated or fixture-verified decoders rather than redefining event meaning by hand.
- UI preview/visual tests use only the isolated `native-next-dev` host/profile and never attach to the installed app.

---

## File map

- Create: `src/ui/mod.rs`
- Create: `src/ui/tokens.rs`
- Create: `src/ui/actions.rs`
- Create: `src/ui/preview.rs`
- Create: `src/ui/shell.rs`
- Create: `src/ui/components/mod.rs`
- Create: `src/ui/components/{button,icon_button,badge,status_light,text_field,menu,tooltip,dialog,toast,splitter,virtual_list,empty_state,error_boundary}.rs`
- Create: `src/ui/task_cockpit/mod.rs`
- Create: `src/ui/task_cockpit/{inbox,header,timeline,message,tool_card,question,approval,composer,context_dock,terminal_panel}.rs`
- Create: `src/ui/renderers/{mod,registry,generic,message,tool,question,approval,operation,plan,artifact,agent}.rs`
- Create: `src/ui/command_center/mod.rs`
- Create: `src/ui/configuration/mod.rs`
- Modify: `src/bin/devmanager-next.rs`
- Modify: `src/client/{model,subscription}.rs`
- Modify: `src/terminal/view.rs`
- Modify: `src/theme/mod.rs`
- Modify: `src/icons.rs`
- Modify: `Cargo.toml`
- Create: `tests/ui_tokens.rs`
- Create: `tests/ui_actions.rs`
- Create: `tests/ui_projection.rs`
- Create: `tests/ui_focus.rs`
- Create: `tests/ui_accessibility.rs`
- Create: `tests/renderer_registry.rs`
- Create: `tests/fixtures/semantic/v1/*`
- Create: `tests/fixtures/ui/*.json`
- Create: `scripts/native-next/Capture-UiPreviews.ps1`
- Create: `docs/ui/task-cockpit.md`
- Modify: `THIRD_PARTY_NOTICES.md`

### Task 5.1: Pin and wrap the native component dependency

**Files:** `Cargo.toml`, `Cargo.lock`, `src/ui/{mod,preview}.rs`, `src/bin/devmanager-next.rs`, `THIRD_PARTY_NOTICES.md`, `tests/ui_projection.rs`

**Upstream evidence:** `https://github.com/longbridge/gpui-component/releases/tag/v0.5.1`, `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/Cargo.toml`, and `https://raw.githubusercontent.com/longbridge/gpui-component/v0.5.1/crates/ui/Cargo.toml`.

- [ ] **Step 1: Verify upstream v0.5.1 evidence** from the release/tag manifests: license, GPUI version, Rust/MSRV requirements, feature set, and transitive licenses. Save direct URLs and the reviewed commit/tag in `THIRD_PARTY_NOTICES.md`.
- [ ] **Step 2: Write a failing smoke test** that constructs a headless preview application, initializes gpui-component exactly once, registers DevManager assets/fonts/actions, and renders a minimal root without a production profile.
- [ ] **Step 3: Run** `cargo test --test ui_projection component_init_ -- --nocapture` through Phase 0 isolation and retain the red result.
- [ ] **Step 4: Add** `gpui-component = "=0.5.1"` and only directly used supporting crates. Wrap initialization in `ui::init(cx)`; no feature module calls third-party global initialization itself.
- [ ] **Step 5: Add `devmanager-next --ui-preview tests/fixtures/ui/theme-gallery.json --output .devmanager-next/evidence/phase-05/screenshots/theme-gallery.png`** as the concrete deterministic preview path; the arguments accept other validated fixture/output paths, never auto-start the real host, and refuse production paths.
- [ ] **Step 6: Run** the smoke test and one empty preview render; commit as `feat(ui): establish native gpui component foundation`.

### Task 5.2: Define visual tokens, themes, density, and contrast

**Files:** `src/ui/tokens.rs`, `src/theme/mod.rs`, `tests/ui_tokens.rs`, `tests/fixtures/ui/theme-gallery.json`

**Token groups:** semantic colors, text hierarchy, surfaces/elevation, borders/focus, spacing, radii, typography, icon sizes, control heights, motion durations, terminal palette, status meanings.

- [ ] **Step 1: Write failing tests** that enumerate every semantic token in dark/light themes, require no transparent foregrounds, enforce WCAG contrast (`4.5:1` normal text, `3:1` large text/UI indicators), and verify compact/comfortable density invariants.
- [ ] **Step 2: Run** `cargo test --test ui_tokens -- --nocapture` and record the red result.
- [ ] **Step 3: Define tokens by meaning** (`text_primary`, `surface_raised`, `status_external`, `focus_ring`) rather than component-specific literals. Ban direct hex/RGBA literals outside `tokens.rs` with a source scan test.
- [ ] **Step 4: Implement dark and light themes** with the same semantic contract. Use blue only for externally running resources, orange for starting/attention, green for healthy/complete, red for failed/destructive, and neutral tones for inactive.
- [ ] **Step 5: Define density metrics** so resizing does not clip labels, icons, question choices, or terminal text at 100%, 125%, 150%, and 200% Windows scale.
- [ ] **Step 6: Render the theme gallery** at both themes/densities and inspect text, disabled states, hover, focus, selection, and status lights; commit as `feat(ui): add accessible visual tokens and themes`.

### Task 5.3: Build and preview the reusable component vocabulary

**Files:** `src/ui/components/*.rs`, `src/ui/preview.rs`, `tests/{ui_projection,ui_accessibility}.rs`, `tests/fixtures/ui/component-gallery.json`

- [ ] **Step 1: Write failing component-state tests** for default/hover/pressed/focused/disabled/loading/destructive variants, tooltip delay, menu dismissal, dialog focus trap, toast lifetime, splitter bounds, and virtual-list row reuse.
- [ ] **Step 2: Run** `cargo test --test ui_projection components_ -- --nocapture` and save the red result.
- [ ] **Step 3: Implement the minimum primitives** listed in the file map using tokens and one interaction-state model. Prefer audited gpui-component internals where they meet behavior/accessibility; wrap them behind DevManager APIs.
- [ ] **Step 4: Give each interactive primitive** a stable action, accessible name/description/role/state, keyboard activation, focus ring, pointer capture, and disabled reason. Icon-only controls require tooltips and accessible labels.
- [ ] **Step 5: Build a component gallery fixture** containing long text, Unicode, missing data, errors, loading, empty, overflow, high scaling, and both themes.
- [ ] **Step 6: Capture and inspect gallery screenshots** with `Capture-UiPreviews.ps1`; correct clipping, contrast, alignment, and focus before committing as `feat(ui): add native cockpit component vocabulary`.

### Task 5.4: Create one shell, action registry, and keyboard model

**Files:** `src/ui/{actions,shell}.rs`, `src/bin/devmanager-next.rs`, `src/client/model.rs`, `tests/{ui_actions,ui_focus}.rs`

**Shell regions:** top bar, Task Inbox sidebar, task header, main content, context dock, global overlays.

- [ ] **Step 1: Write failing tests** that GPUI exposes the same ActionIds/command envelopes as `devmanager-host ctl`, plus palette search, shortcut conflicts, disabled reasons, navigation history, restore of client-local selected task, Escape priority, and focus return after dialogs.
- [ ] **Step 2: Run** `cargo test --test ui_actions -- --nocapture` and retain the red output.
- [ ] **Step 3: Bind the shared `ActionCatalog` into typed GPUI actions** and add only UI-specific default shortcuts/icon/grouping there. Menus, buttons, palette, context menus, and CLI reference the same ActionIds and pure command factories.
- [ ] **Step 4: Implement shell navigation** as client-local selection over host task projections. On task switch, increment focus epoch, consume the initiating pointer sequence, preserve per-task dock/view state, and never synthesize provider/terminal input.
- [ ] **Step 5: Set keyboard rules:** Ctrl+K palette; Ctrl+P task switcher; Ctrl+Shift+P command palette alias only if conflict-free; Alt+1…7 dock tools; Ctrl+backtick terminal; Escape dismisses the topmost transient layer before changing selection.
- [ ] **Step 6: Render shell fixtures** at 1024×700, 1440×900, and 4K/high-DPI; commit as `feat(ui): add task cockpit shell and actions`.

### Task 5.5: Build the Task Inbox and attention model

**Files:** `src/ui/task_cockpit/inbox.rs`, `src/client/model.rs`, `tests/ui_projection.rs`, `tests/fixtures/ui/task-inbox.json`

- [ ] **Step 1: Write failing projection tests** for Needs Me/Running/Ready/Recent sections, archived-task search, disconnected/failed/uncertain-outcome/approval/answer/working/settling/ready/idle precedence, Primary provider icon, unread semantic event count, external service status, long project names, missing provider, and 5,000 virtualized tasks.
- [ ] **Step 2: Run** `cargo test --test ui_projection inbox_ -- --nocapture` and save the red result.
- [ ] **Step 3: Derive `TaskRowModel`** entirely from the separate lifecycle/connectivity/attention/activity/review axes plus client-local unread cursor. Sections are Needs Me, Running, Ready, and Recent; Archived is reachable through search/history. Sorting is deterministic within a section: recent activity, title, TaskId.
- [ ] **Step 4: Render compact rows** with title, project/worktree, Primary/provider state, one semantic status, and optional resource indicators. Do not expose verbose ownership language or terminal IDs in normal rows.
- [ ] **Step 5: Make row activation pointer-safe** using Phase 3 focus fencing. Context menu actions target captured `TaskId`, not whichever row is selected after asynchronous UI work.
- [ ] **Step 6: Prove virtualized scrolling and incremental updates** stay responsive with 5,000 fixture tasks; commit as `feat(ui): add attention-first task inbox`.

### Task 5.6: Build the task header and truthful top bar

**Files:** `src/ui/task_cockpit/header.rs`, `src/ui/shell.rs`, `src/client/model.rs`, `tests/ui_projection.rs`, `tests/fixtures/ui/header-states.json`

- [ ] **Step 1: Write failing tests** for title/project/worktree, Primary/specialists, turn state, host/connect health, update state, fresh/stale/unavailable quota, aggregate CPU/memory, narrow-width overflow, and accessible status descriptions.
- [ ] **Step 2: Run** `cargo test --test ui_projection header_ -- --nocapture` and retain the red result.
- [ ] **Step 3: Put task-local facts in the task header** and global host/Connect/update/provider quota in the top bar. Status values link to their operational detail surface.
- [ ] **Step 4: Hide quota observations at one-hour staleness**; show an unavailable diagnostic only in Command Center, not as a misleading cached number.
- [ ] **Step 5: Use whole-machine `0..=100` CPU** in normal UI. Put raw core-equivalent percentage behind a diagnostics disclosure with explicit label.
- [ ] **Step 6: Use responsive priority rules** to collapse labels into accessible icon/menu items without clipping; commit as `feat(ui): add truthful task header and top bar`.

### Task 5.7: Build the semantic renderer registry and virtualized timeline

**Files:** `src/ui/renderers/{mod,registry,generic,message,tool,question,approval,operation,plan,artifact,agent}.rs`, `src/ui/task_cockpit/{timeline,message,tool_card,question,approval}.rs`, `src/client/model.rs`, `tests/{renderer_registry,ui_projection,ui_accessibility}.rs`, `tests/fixtures/semantic/v1/*`, `tests/fixtures/ui/timeline-*.json`

- [ ] **Step 1: Write failing tests** for user/assistant Markdown, code blocks, tool calls/results, progress, plans, errors, operation pending/success/failure/cancellation/uncertainty, question choices, approvals, specialist lineage, artifacts, known renderer registration, duplicate kind rejection, unknown extension events, malformed known events, bounded generic payload, Rust semantic golden fixtures, 20,000 events, streaming updates, and anchored scroll.
- [ ] **Step 2: Run** `cargo test --test renderer_registry --test ui_projection timeline_ -- --nocapture` and save the red output.
- [ ] **Step 3: Define the registry boundary**

```rust
pub trait SemanticRenderer: Send + Sync {
    fn kind(&self) -> SemanticKind;
    fn project(&self, event: &SemanticEvent) -> Result<TimelineItemModel, RenderModelError>;
}

pub struct GenericSemanticCard {
    pub event_id: EventId,
    pub provider: ProviderKind,
    pub source_type: String,
    pub schema_version: u16,
    pub status: GenericStatus,
    pub title: String,
    pub redacted_fields: Vec<(String, String)>,
    pub raw_terminal_available: bool,
}
```

Register exactly one specialized renderer for known message/tool/question/approval/operation/plan/artifact/agent kinds. Convert journal events into stable `TimelineItemModel`s keyed by event ID and group only by explicit turn/tool relationships.
- [ ] **Step 4: Route unknown/malformed optional events through `GenericSemanticCard`.** Limit title to 160 Unicode scalar values, 32 fields, keys to 64, values to 512, and total encoded card data to 16 KiB. Never expose secret-class fields or interpret generic data as an interactive control/domain transition. Always offer the same-generation raw terminal when available.
- [ ] **Step 5: Render `OperationUncertain` as a known Needs Me warning**, not as success, ordinary failure, or generic data. Show the operation/effect evidence and an inspect/reconcile path; any explicit new attempt warns that the earlier effect may already have happened and creates a new CommandId. Never provide an implicit resend.
- [ ] **Step 6: Render Markdown/code natively** with bounded parsing, selectable/copyable text, wrapped prose, horizontally scrollable code, link confirmation, and no HTML execution.
- [ ] **Step 7: Virtualize variable-height items** while preserving the visible anchor when earlier events load or streaming items grow. Auto-follow only when the user is at the bottom; show a `Jump to latest` control otherwise.
- [ ] **Step 8: Make questions/approvals explicit controls** with request ID, action epoch, runtime generation, capability, and first-answer-wins settlement. Switching into the task never activates a choice.
- [ ] **Step 9: Generate/verify cross-client semantic fixtures** from the Rust schema and make a fixture check fail if a later TypeScript decoder changes a discriminant or interactive meaning by hand.
- [ ] **Step 10: Run shared conformance baseline/variant arms** across every known, unknown, and malformed semantic fixture. Record renderer selection, generic fallback, interaction eligibility, update latency, and bounded output size; never record raw provider content or score model output.
- [ ] **Step 11: Capture fixtures for all states/themes/scales**, measure update latency and memory, and commit as `feat(ui): add shared semantic renderer registry`.

### Task 5.8: Build the composer, attachments, and turn-mode controls

**Files:** `src/ui/task_cockpit/composer.rs`, `src/ui/actions.rs`, `src/client/model.rs`, `tests/{ui_actions,ui_focus}.rs`

- [ ] **Step 1: Write failing tests** for multiline input, Send Now, Steer, Queue Follow-up, slash/action search, file/image attachments, paste, draft preservation per task, reconnect retry with same command ID, disabled capability reason, and stop-turn.
- [ ] **Step 2: Run** `cargo test --test ui_actions composer_ -- --nocapture` and retain the red output.
- [ ] **Step 3: Keep drafts client-local** keyed by Task/Agent until the host accepts the command. Persist draft text/attachment references in the isolated client preference store, never `session.json`.
- [ ] **Step 4: Resolve Enter behavior explicitly:** Enter sends by user preference; Shift+Enter inserts newline; IME composition never sends; every send button labels the active mode.
- [ ] **Step 5: Upload/import attachments as Task artifacts first**, then reference typed ArtifactIds in provider commands. Validate size/type/path and show progress/cancel/failure.
- [ ] **Step 6: Disable unsupported provider modes** with a specific reason; do not approximate steer/follow-up with hidden terminal keystrokes when the adapter cannot guarantee semantics.
- [ ] **Step 7: Run** composer/focus tests and commit as `feat(ui): add capability-aware task composer`.

### Task 5.9: Add the single context dock and raw terminal view

**Files:** `src/ui/task_cockpit/{context_dock,terminal_panel}.rs`, `src/terminal/view.rs`, `src/ui/actions.rs`, `tests/{ui_projection,ui_focus}.rs`

- [ ] **Step 1: Write failing tests** for seven dock tools, one active tool, remembered tool/size per task, collapse/reopen, unavailable tool state, raw terminal generation match, independent scroll/selection, and zero provider launch on view switch.
- [ ] **Step 2: Run** `cargo test --test ui_projection dock_ -- --nocapture` and save the red result.
- [ ] **Step 3: Implement a bounded resizable dock** at right or bottom according to window aspect/preference, with tabs for Changes, Files, Terminal, Browser, Services, Artifacts, and Review. Do not add freeform nesting or arbitrary pane splitting.
- [ ] **Step 4: Embed the existing native terminal renderer** against the Phase 3 `TerminalReplica`; remove direct `TerminalSession` ownership from the view. Show reconnect/resync/exit states over the last valid grid.
- [ ] **Step 5: On semantic/terminal switch**, preserve the same AgentSession/generation and verify provider root/PTY reader counts remain unchanged.
- [ ] **Step 6: Apply focus epochs and pointer capture** across dock resizing/tab clicks so terminal mouse reports cannot leak; commit as `feat(ui): add task context dock and shared raw terminal`.

### Task 5.10: Complete accessibility, resilience, and visual/performance gates

**Files:** `src/ui/components/error_boundary.rs`, `src/ui/preview.rs`, `tests/{ui_accessibility,ui_focus,ui_projection}.rs`, `scripts/native-next/Capture-UiPreviews.ps1`, `docs/ui/task-cockpit.md`

- [ ] **Step 1: Add accessibility tests** for reachable focus order, named controls, roles/states, contrast, no keyboard trap, reduced motion, screen-reader status text, 200% scaling, and color-independent status meaning.
- [ ] **Step 2: Add resilience tests** for host disconnect, incompatible host, resync, malformed optional projection, provider terminal-only mode, empty database, 5,000 tasks, and 20,000 timeline items.
- [ ] **Step 3: Add UI error boundaries** at shell, task content, and dock-tool levels so one renderer failure becomes a diagnostic card rather than closing the app or host.
- [ ] **Step 4: Define performance budgets:** host event to visible update p95 ≤100 ms locally; navigation cached projection p95 ≤50 ms; typing p95 ≤16 ms; scroll maintains 60 fps target; idle UI CPU below 1% on the reference machine; no synchronous filesystem/process/port/network work on render/input paths.
- [ ] **Step 5: Capture the approved fixture matrix** at dark/light, compact/comfortable, 100%/150%/200%, narrow/standard/wide, empty/loading/error/large-data. Inspect every image and record approved baselines.
- [ ] **Step 6: Use Windows accessibility inspection and keyboard-only walkthrough** on the isolated binary. Fix every unnamed/clipped/unreachable control.
- [ ] **Step 7: Document anatomy, tokens, action rules, and preview commands**; commit as `test(ui): gate task cockpit quality and accessibility`.

## Phase 5 verification gate

- [ ] Capture production baseline and start only `devmanager-next`/isolated host with `native-next-dev`.
- [ ] Run `cargo test --test ui_tokens --test ui_actions --test renderer_registry --test ui_projection --test ui_focus --test ui_accessibility -- --nocapture`.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Run `pwsh scripts/native-next/Capture-UiPreviews.ps1 -AllFixtures -AllThemes -AllScales` and visually inspect every generated image.
- [ ] Perform keyboard-only and 200% Windows-scale walkthrough of Task Inbox, timeline, question, approval, composer, dock, terminal, menus, and dialogs.
- [ ] Record performance traces for large inbox/timeline and prove no OS/filesystem/network probe occurs on UI paint/input threads.
- [ ] Switch semantic/raw views 100 times and assert provider process count and PTY reader count never change.
- [ ] Feed every known/unknown/malformed semantic fixture through the registry and verify GPUI meaning matches the Rust golden contract; generic fallback must remain bounded and non-interactive.
- [ ] Rebuild the conformance query index and compare semantic-renderer baseline/variant arms.
- [ ] Confirm all preview/client/host/test processes are closed; compare production hashes and installed PID/start time.
- [ ] Review the Phase 5 diff and deletion ledger.

## Phase 5 exit criteria

- The native desktop reads as a Task Cockpit rather than a pile of terminals and remains usable at all approved sizes/scales/themes.
- Semantic conversation is the default, raw terminal is one click/shortcut away, and both views share one live provider session.
- Known semantic events use specialized accessible renderers; unknown/malformed optional events remain visible through a bounded safe generic card and never invent task state.
- Task navigation, questions, approvals, and terminal input cannot receive click-through.
- Every operation is reachable by mouse and keyboard, has an accessible name/state, and uses contrast-compliant tokens.
- Large task/timeline fixtures meet the defined latency/idle budgets without hot-path probes.
- The component preview gallery makes future UI work visually reviewable before integration.
