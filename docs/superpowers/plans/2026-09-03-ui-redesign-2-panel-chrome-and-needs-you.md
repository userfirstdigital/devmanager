# UI Redesign 2: Panel Chrome and Needs-You Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every task panel the two-row chrome from the spec (title row with status and one primary action, tab row with views), the amber question card and docked permission prompt, a red blocked state, one-key zoom, and a Done that closes the panel; retire the compact-pane summary and the right dock in the process.

**Architecture:** The recursive split workspace stays. `TaskPane` gains a `view` (which tab is showing) and the workspace gains a transient `zoomed` pane. A new pure module `src/ui/panel/` derives a `PanelChrome` model per pane from the same facts plan 1's board uses (one status derivation for both surfaces) and a painter draws it. The shell's `render_task_workspace_pane` is replaced: the body is chosen by the pane's view, with the old dock panel renderers reused per owner. The question card is restyled where conversation rows are painted; the permission prompt replaces the composer when an approval is pending.

**Tech Stack:** Rust, gpui, gpui-component 0.5.1, serde. No new crates.

**Spec:** `docs/superpowers/specs/2026-09-03-ui-redesign-design.md` sections 3, 6, 10, 12. Reference images: `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.png`, `02-panel-chrome-2.png`, `06-needs-you-question-1-permission.png`.

**Depends on:** plan 1 (`2026-09-03-ui-redesign-1-board-and-tokens.md`) landed: `crate::ui::board::{BoardRow, BoardState, board_state_of, activity::board_activity, age::format_age, project_colour::ProjectColourBook}`, `PrimaryProviderIcon::glyph_path()`, the dark tokens, and `NativeShell::board_rows(now_ms)`.

## Global Constraints

- Panel chrome is exactly two rows above the view: title row (provider mark, title, inline status, ⤢, one primary action, ⋯) and tab row (Conversation, Terminal, Files, Changes, Browser on the left; subagent tabs on the right are sub-project 4 and stay empty here). Title row 30 px, tab row 26 px.
- Inline status: state icon, doing-now text, age, progress segments; secondary text colour, amber when needs-you, red when blocked. Truncation order as the panel narrows: segments go first, then doing-now text, leaving icon and age; the status truncates before the title does; below 320 px the segments are gone.
- 3 px project stripe on the panel's left edge, full height, from plan 1's colour book.
- Primary action is **Done** for open tasks and **Reopen** for Done tasks. Everything else is behind ⋯ in this order: Add action (A), Commit (C), separator, Zoom (Z), Pin size (P), Move ← ↑ ↓ → (Shift+Ctrl+arrows), Swap with… (S), separator, More views ▸ (Review, Artifacts, Services), Rename, Archive, Delete… (confirms).
- Letter keys act only while the panel's chrome has focus, never while a text input has focus. Views switch with Ctrl+1..5. Number keys 1-9 answer a pending question, Enter allows and Esc denies a pending permission, D opens the diff without answering.
- Question: amber panel border with a 1 px glow, title-row status "? Asked a question · 4m" in amber, and a card at the end of the stream with the amber label QUESTION, the prompt, numbered choices (the recommended one outlined more strongly), and the footer "1-N pick · ⏎ send". The card scrolls with the stream; it is never docked.
- Permission: the composer is replaced by a docked card: "ALLOW?", the request summary in monospace, buttons Allow (⏎), Always for this task, Deny (Esc), and "D view diff" when the summary names a file. "Always for this task" is client-local and never writes to a provider's settings.
- Blocked: red border, red status naming the cause, a Retry secondary action in the status line.
- Zoom (`Z`, ⤢, Esc back) shows one panel in the grid's whole area with identical chrome; it is transient state, never persisted as a layout change.
- No distilled compact presentation. A panel that would get less than 320 by 160 px renders as its title row alone (28 px) and expands when room returns.
- Done closes the panel and collapses the split; a Done task gets a panel again only when reopened from the board or when a message is sent to it.
- Colours only from `ThemeTokens` and the project palette; amber `status.attention` and red `status.destructive` appear only on needs-you panels.
- Work in your own git worktree off `VisualDevManager`; own `CARGO_TARGET_DIR`; copy `web/src/connect/wasm/` if `build.rs` complains; never write to `<repo>/.devmanager-next/`; never launch the app.
- LF line endings. Gates: `cargo check --locked --lib --bins --tests -j 2` EXIT 0; `cargo fmt --all -- --check` EXIT 0; no new warnings; targeted tests while iterating, one full `cargo test --lib -- --test-threads=4` at the end.
- Commit trailer on every commit: `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` then `Claude-Session: https://claude.ai/code/session_01BJgrNVntfxuTu79ugvwNHm`.
- **Visual fidelity rule (user ruling 2026-09-03: the built UI must MATCH the images, not resemble them).** Every task that paints pixels is a design task: before writing any painter code the implementer loads the `frontend-design` skill (`C:/Users/micro/.claude/plugins/cache/claude-plugins-official/frontend-design/unknown/skills/frontend-design/SKILL.md`) and Reads the reference PNG(s) named in the task with the Read tool, then opens the matching mockup HTML beside it and copies sizes, colours, spacing, weights and radii from that CSS verbatim, never approximating from memory. The reviewer of a painter task also Reads the reference PNG and judges the painter code against it element by element (row height, paddings, font sizes, colours, stripe width, segment geometry, mark size), listing every deviation as an Important finding. The final visual acceptance is a side-by-side of a real capture against the PNGs done by the controller with the design skill loaded, and a reviewer independently; a capture that has the same parts in a different arrangement fails.
- The reference PNGs are the acceptance criterion for appearance (spec 12). Task 10 compares a real capture against them.

---

## File structure

| File | Responsibility |
|---|---|
| `src/ui/task_workspace/layout.rs` (modify) | `PaneView` on `TaskPane`; drop `CompactManual`; `zoomed` |
| `src/ui/task_workspace/allocation.rs` (modify) | Minimised-strip threshold 320×160; zoom gives the whole canvas |
| `src/ui/task_workspace/view.rs` (modify) | View model carries `view`, `minimised`, `zoomed` |
| `src/ui/workspace_layout.rs` (modify) | Seed `view` from the legacy `task_center_terminal` map on load; stop writing the map |
| `src/ui/panel/mod.rs`, `model.rs` (create) | `PanelChrome`, `PanelStatus`, `NeedsYou`, `status_layout(width)` |
| `src/ui/panel/render.rs` (create) | Title row, tab row, stripe, borders, minimised strip |
| `src/ui/panel/menu.rs` (create) | ⋯ menu rows and key letters (pure) |
| `src/ui/conversation/render.rs` (modify) | `question_element` restyled to the card |
| `src/ui/panel/permission.rs` (create) | Docked permission card painter |
| `src/ui/actions.rs` (modify) | `KeyboardAction::SelectView(PaneView)` on Ctrl+1..5; dock bindings removed |
| `src/ui/native_shell.rs` (modify) | Pane renderer replaced; per-owner dock surfaces; Done closes pane; menu overlay; key routing; dock hidden |

---

### Task 1: `PaneView` on the pane, migrated from the terminal preference

**Files:**
- Modify: `src/ui/task_workspace/layout.rs:86-105` (`TaskPane`), add `PaneView`
- Modify: `src/ui/workspace_layout.rs:118-176` (`task_center_terminal` map, its key helpers) and `sanitized()`
- Modify: `src/ui/native_shell.rs:20210-20260` (`task_center_terminal_preference`, `set_task_center_terminal_preference`)
- Test: `src/ui/task_workspace/layout.rs`, `src/ui/workspace_layout.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneView { #[default] Conversation, Terminal, Files, Changes, Browser, Review, Artifacts, Services }
impl PaneView {
    pub const TABS: [PaneView; 5] = [Conversation, Terminal, Files, Changes, Browser];
    pub const MORE: [PaneView; 3] = [Review, Artifacts, Services];
    pub fn label(self) -> &'static str;
}
// on TaskPane:  #[serde(default)] pub view: PaneView,
impl<K> Workspace<K> {
    pub fn view_of(&self, task_id: K) -> Option<PaneView>;
    pub fn set_view(&mut self, task_id: K, view: PaneView) -> Result<(), WorkspaceError>;
}
```

- The shell's `task_center_terminal_preference(owner) -> bool` becomes `pane_view(owner) -> PaneView` and `set_task_center_terminal_preference(owner, bool)` becomes `set_pane_view(owner, PaneView)`; every caller of the old pair is updated (grep both names; there are callers in the terminal focus paths around 20133 and 41330).

- [ ] **Step 1: Write the failing tests**

In `layout.rs` tests:

```rust
#[test]
fn a_pane_defaults_to_the_conversation_view_and_remembers_a_set_view() {
    let mut ws = Workspace::single(1u32);
    assert_eq!(ws.view_of(1), Some(PaneView::Conversation));
    ws.set_view(1, PaneView::Terminal).expect("set");
    assert_eq!(ws.view_of(1), Some(PaneView::Terminal));
    assert_eq!(ws.set_view(9, PaneView::Files), Err(WorkspaceError::MissingPane));
}

#[test]
fn a_serialized_pane_without_a_view_field_loads_as_conversation() {
    let ws = Workspace::single(1u32);
    let mut json = serde_json::to_value(&ws).expect("json");
    // strip "view" from the only pane
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => { map.remove("view"); for v in map.values_mut() { strip(v); } }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut json);
    let restored: Workspace<u32> = serde_json::from_value(json).expect("old file loads");
    assert_eq!(restored.view_of(1), Some(PaneView::Conversation));
}
```

In `workspace_layout.rs` tests:

```rust
#[test]
fn legacy_task_center_terminal_true_seeds_the_terminal_view_once() {
    let task = TaskId::new();
    let mut layout = KeyedWorkspaceLayout::<TaskId>::default();
    layout.task_workspace = Some(Workspace::single(task));
    layout.task_center_terminal.insert(task_center_terminal_preference_key(&task).expect("key"), true);
    let sanitized = layout.sanitized();
    assert_eq!(sanitized.task_workspace.as_ref().and_then(|w| w.view_of(task)), Some(PaneView::Terminal));
    assert!(sanitized.task_center_terminal.is_empty(), "the map is consumed, not kept");
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::task_workspace::layout ui::workspace_layout -- --test-threads=4`
Expected: compile errors (`PaneView`, `view_of`, `set_view`).

- [ ] **Step 3: Implement**

In `layout.rs` add the enum (with `label()` returning "Conversation", "Terminal", "Files", "Changes", "Browser", "Review", "Artifacts", "Services"), the `view` field with `#[serde(default)]` on `TaskPane`, `TaskPane::new` setting `view: PaneView::default()`, and:

```rust
pub fn view_of(&self, task_id: K) -> Option<PaneView> {
    self.pane_for_task(task_id).map(|pane| pane.view)
}

pub fn set_view(&mut self, task_id: K, view: PaneView) -> Result<(), WorkspaceError> {
    let pane = self.pane_for_task_mut(task_id).ok_or(WorkspaceError::MissingPane)?;
    pane.view = view;
    Ok(())
}
```

Add `pane_for_task_mut` beside `pane_for_task` if it does not exist (same traversal, `&mut`).

In `workspace_layout.rs` `sanitized()`: after the workspace is validated, for each `(key, true)` in `task_center_terminal` whose key parses to a task in the workspace, call `set_view(task, PaneView::Terminal)`; then clear the map. Keep the field (still `#[serde(default)]`) so older files load; never write into it again.

In `native_shell.rs` replace the two preference functions:

```rust
fn pane_view(&mut self, owner: &HostTaskKey) -> PaneView {
    self.layout.task_workspace.as_ref().and_then(|w| w.view_of(owner.clone())).unwrap_or_default()
}

fn set_pane_view(&mut self, owner: &HostTaskKey, view: PaneView) {
    let changed = self.layout.task_workspace.as_mut()
        .is_some_and(|w| w.view_of(owner.clone()) != Some(view) && w.set_view(owner.clone(), view).is_ok());
    if changed { self.mark_layout_dirty(); }
}
```

and update callers: `task_center_terminal_preference(&k)` → `self.pane_view(&k) == PaneView::Terminal`; `set_task_center_terminal_preference(&k, true)` → `self.set_pane_view(&k, PaneView::Terminal)`; `(…, false)` → `PaneView::Conversation`. Also `TaskPaneProjection.show_terminal` and `TaskPaneViewModel.paint_terminal` in `view.rs` become `view: PaneView` (Task 3 finishes that).

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::task_workspace ui::workspace_layout -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_workspace/layout.rs src/ui/workspace_layout.rs src/ui/native_shell.rs
git commit -m "feat(workspace): per-pane view replaces the terminal preference map"
```

---

### Task 2: Retire the compact summary; keep the minimised strip

**Files:**
- Modify: `src/ui/task_workspace/layout.rs` (`PanePresentation`, `set_manual_compact`)
- Modify: `src/ui/task_workspace/allocation.rs:80-135` (presentation restore, minimum sizes)
- Modify: `src/ui/task_workspace/surfaces.rs:2078`, `src/ui/workspace_layout.rs` (one `CompactManual` reference each)
- Modify: `src/ui/native_shell.rs:25030-25045` (`set_workspace_task_compact`, the header "Compact/Full" control)
- Test: `allocation.rs`, `layout.rs`

**Interfaces:**
- `PanePresentation` becomes `{ Full, Minimised }` with `#[serde(alias = "CompactAutomatic")]` on `Minimised` and `#[serde(alias = "CompactManual")]` mapping to `Full` via a custom `Deserialize` or a post-load normalisation in `sanitized()`. Choose the post-load normalisation: deserialize `CompactManual` as `Minimised` through the alias, then `sanitized()` sets every `Minimised` pane back to `Full` (automatic minimisation is recomputed by allocation every frame anyway).
- `Workspace::set_manual_compact` is deleted. `set_presentation` stays for allocation's use.
- Minimum sizes: `MinimumSize::full()` = 320 by 160 logical px; `MinimumSize::minimised()` = 320 by 28. Allocation minimises the least-recently-focused unpinned Full pane when a rectangle would fall under full minimum, exactly the existing `CompactAutomatic` rule, and restores when room returns.

- [ ] **Step 1: Write the failing tests**

In `allocation.rs` tests, adapt the existing test that asserts `CompactAutomatic` (around line 581) to `Minimised`, and add:

```rust
#[test]
fn a_pane_under_320_by_160_is_minimised_to_a_28px_strip_and_restored_when_room_returns() {
    let mut ws = Workspace::single(1u32);
    ws.insert_after_focused(2u32, Axis::Horizontal).expect("second pane");
    // 500 px wide canvas: two full panes need 640, so one minimises.
    let rects = allocate(&mut ws, Size { width: 500.0, height: 400.0 });
    let minimised: Vec<_> = ws.task_ids().into_iter().filter(|t| ws.presentation(*t) == Some(PanePresentation::Minimised)).collect();
    assert_eq!(minimised.len(), 1);
    let strip = rects.iter().find(|r| r.task_id == minimised[0]).expect("strip rect");
    assert_eq!(strip.rect.height, 28.0);
    let _ = allocate(&mut ws, Size { width: 900.0, height: 400.0 });
    assert!(ws.task_ids().into_iter().all(|t| ws.presentation(t) == Some(PanePresentation::Full)));
}
```

Use the real names of the allocation entry point and rect type from `allocation.rs` (`PaneRect` at line 44; find the function that returns `Vec<PaneRect>`).

In `layout.rs` tests, add:

```rust
#[test]
fn compact_manual_from_an_older_file_loads_and_normalises_to_full() {
    let json = r#"{"root":{"Pane":{"id":"00000000-0000-7000-8000-000000000001","task_id":1,"presentation":"CompactManual","last_focused_at":1,"view":"conversation"}},"focused":"00000000-0000-7000-8000-000000000001","previous_focus":null,"focus_clock":1}"#;
    let ws: Workspace<u32> = serde_json::from_str(json).expect("older file loads");
    assert_eq!(ws.presentation(1), Some(PanePresentation::Minimised), "alias maps the old value");
}
```

If the serialized shape of `PaneId`/`root` differs, produce the JSON by serialising a workspace and string-replacing `"Full"` with `"CompactManual"` instead of hand-writing it.

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::task_workspace -- --test-threads=4`
Expected: compile errors on `Minimised`.

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PanePresentation {
    Full,
    /// Too little room: the pane renders as its title row alone (28 px) until
    /// allocation gives it space again. Never chosen by the user.
    #[serde(alias = "CompactAutomatic", alias = "CompactManual")]
    Minimised,
}
```

Delete `set_manual_compact` and every caller (`set_workspace_task_compact` in the shell and the header toggle). In `allocation.rs` replace the two-variant minimum-size match with `Full => 320×160`, `Minimised => 320×28`, and the "restore CompactAutomatic to Full" pass now restores `Minimised`. In `workspace_layout.rs` `sanitized()`, set every `Minimised` pane to `Full` on load (allocation re-derives it). Update the `surfaces.rs:2078` test expectation.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::task_workspace ui::workspace_layout -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_workspace src/ui/workspace_layout.rs src/ui/native_shell.rs
git commit -m "feat(workspace): minimised strip replaces manual compact panes"
```

---

### Task 3: Zoom

**Files:**
- Modify: `src/ui/task_workspace/layout.rs` (`Workspace` fields and methods)
- Modify: `src/ui/task_workspace/allocation.rs`, `src/ui/task_workspace/view.rs`
- Test: `layout.rs`, `view.rs`

**Interfaces:**
- `Workspace` gains `#[serde(skip)] zoomed: Option<PaneId>`; methods `pub fn zoomed(&self) -> Option<PaneId>`, `pub fn zoom(&mut self, pane: PaneId) -> Result<(), WorkspaceError>` (also focuses it), `pub fn unzoom(&mut self)`, `pub fn toggle_zoom_focused(&mut self)`. `remove_pane` clears `zoomed` if it removed that pane. `validate()` requires `zoomed` to name an existing pane.
- Allocation: when `zoomed` is `Some`, the only rectangle produced is that pane at the full canvas; nothing else changes and no pane is minimised.
- `TaskPaneViewModel` gains `pub view: PaneView`, `pub minimised: bool`, `pub zoomed: bool`; `TaskWorkspaceViewModel::build` returns a root containing only the zoomed pane when zoom is on. `paint_terminal`, `body: TaskPaneBody`, `latest_snippet` are removed (no compact body exists any more).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn zoom_is_transient_and_leaves_the_tree_unchanged() {
    let mut ws = Workspace::single(1u32);
    ws.insert_after_focused(2u32, Axis::Horizontal).expect("pane");
    let before = serde_json::to_string(&ws).expect("json");
    let pane = ws.pane_for_task(2).expect("pane").id;
    ws.zoom(pane).expect("zoom");
    assert_eq!(ws.zoomed(), Some(pane));
    assert_eq!(ws.focused_task(), Some(2), "zoom focuses the pane");
    assert_eq!(serde_json::to_string(&ws).expect("json"), before, "zoom is not serialised");
    ws.unzoom();
    assert_eq!(ws.zoomed(), None);
    ws.toggle_zoom_focused();
    assert_eq!(ws.zoomed(), Some(pane));
    ws.remove_pane(pane).expect("remove");
    assert_eq!(ws.zoomed(), None, "removing the zoomed pane clears zoom");
}

#[test]
fn a_zoomed_workspace_allocates_one_full_canvas_rectangle() {
    let mut ws = Workspace::single(1u32);
    ws.insert_after_focused(2u32, Axis::Horizontal).expect("pane");
    let pane = ws.pane_for_task(2).expect("pane").id;
    ws.zoom(pane).expect("zoom");
    let rects = allocate(&mut ws, Size { width: 500.0, height: 400.0 });
    assert_eq!(rects.len(), 1);
    assert_eq!((rects[0].rect.width, rects[0].rect.height), (500.0, 400.0));
    assert!(ws.task_ids().into_iter().all(|t| ws.presentation(t) == Some(PanePresentation::Full)), "zoom never minimises");
}
```

And in `view.rs` tests replace `compact_view_model_contains_status_and_snippet_but_no_heavy_surface` with:

```rust
#[test]
fn view_model_carries_view_minimised_and_zoomed_flags() {
    let mut ws = Workspace::single(TaskId::new());
    let task = ws.task_ids()[0];
    ws.set_view(task, PaneView::Files).expect("view");
    let pane = ws.pane_for_task(task).expect("pane").id;
    ws.zoom(pane).expect("zoom");
    let vm = TaskWorkspaceViewModel::build(&ws, &[projection(task, "Snake Frontend")]);
    let panes = vm.panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].view, PaneView::Files);
    assert!(panes[0].zoomed);
    assert!(!panes[0].minimised);
}
```

(Adapt `projection(...)` to the existing fixture helper's real signature after removing `snippet` and `terminal`.)

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::task_workspace -- --test-threads=4`
Expected: compile errors.

- [ ] **Step 3: Implement**

`layout.rs`:

```rust
pub fn zoomed(&self) -> Option<PaneId> { self.zoomed }

pub fn zoom(&mut self, pane: PaneId) -> Result<(), WorkspaceError> {
    self.focus_pane(pane)?;
    self.zoomed = Some(pane);
    Ok(())
}

pub fn unzoom(&mut self) { self.zoomed = None; }

pub fn toggle_zoom_focused(&mut self) {
    match (self.zoomed, self.focused) {
        (Some(_), _) => self.zoomed = None,
        (None, Some(focused)) => self.zoomed = Some(focused),
        (None, None) => {}
    }
}
```

In `remove_pane`, after a successful removal: `if self.zoomed == Some(pane_id) { self.zoomed = None; }`. In `validate`, `if let Some(z) = self.zoomed { self.pane(z).ok_or(WorkspaceError::InvalidTree)?; }`.

`allocation.rs`: at the top of the allocation entry, `if let Some(z) = workspace.zoomed() { return vec![PaneRect { pane_id: z, task_id: …, rect: canvas }]; }` (fill from `workspace.pane(z)`), skipping the minimisation pass entirely.

`view.rs`: replace `TaskPaneBody`, `latest_snippet`, `show_terminal`, `paint_terminal` with `view: PaneView`, `minimised: bool` (from `presentation == Minimised`), `zoomed: bool`; when zoomed, `build` emits a root of just that pane. `build_composer` stays (focused pane owns the composer).

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::task_workspace -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/task_workspace
git commit -m "feat(workspace): transient zoom of one pane"
```

---

### Task 4: Panel chrome model

**Files:**
- Create: `src/ui/panel/mod.rs`, `src/ui/panel/model.rs`
- Modify: `src/ui/mod.rs` (`pub mod panel;`)
- Test: `src/ui/panel/model.rs`

**Interfaces:**
- Consumes plan 1's `BoardRow` (title, state, why, state_age_ms, progress, provider, project_colour, project_label, branch) and `PaneView`.
- Produces:

```rust
pub enum NeedsYou { Question { choices: usize }, Permission { names_a_file: bool }, Blocked { cause: String } }
pub enum PrimaryAction { Done, Reopen }
pub struct PanelStatus { pub icon: &'static str, pub text: String, pub age: String, pub progress: Option<BoardProgress>, pub tone: StatusTone }
pub enum StatusTone { Neutral, Attention, Blocked }
pub struct PanelChrome {
    pub key: HostTaskKey,
    pub title: String,
    pub crumb: String,             // "Snake Game · Claude · main", shown only when zoomed
    pub provider: PrimaryProviderIcon,
    pub project_colour: u8,
    pub status: PanelStatus,
    pub needs_you: Option<NeedsYou>,
    pub primary: PrimaryAction,
    pub view: PaneView,
    pub focused: bool,
    pub zoomed: bool,
    pub minimised: bool,
}
pub struct StatusLayout { pub show_segments: bool, pub show_text: bool }
pub fn status_layout(panel_width_px: f32) -> StatusLayout   // <320 no segments; <260 no text
pub fn panel_chrome(row: &BoardRow, view: PaneView, focused: bool, zoomed: bool, minimised: bool, needs_you: Option<NeedsYou>, done: bool, crumb: String) -> PanelChrome
```

Status rules: Question → icon "?", text "Asked a question", tone Attention; Permission → "?", "Permission", Attention; Blocked → "!", the cause (bounded to 60 chars), tone Blocked; Working → "▶", the row's doing-now text; Idle → "·", "Idle"; Done → "✓", "Done". `age` is `format_age(row.state_age_ms)`; `progress` is the row's.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: BoardState) -> BoardRow { /* same helper shape as plan 1's model tests, why = state.why_label() */ }

    #[test]
    fn question_panels_are_amber_with_the_asked_a_question_status() {
        let chrome = panel_chrome(&row(BoardState::Question), PaneView::Conversation, true, false, false, Some(NeedsYou::Question { choices: 3 }), false, String::new());
        assert_eq!(chrome.status.tone, StatusTone::Attention);
        assert_eq!(chrome.status.icon, "?");
        assert_eq!(chrome.status.text, "Asked a question");
        assert_eq!(chrome.primary, PrimaryAction::Done);
    }

    #[test]
    fn blocked_panels_are_red_and_name_the_cause_bounded() {
        let long = "x".repeat(200);
        let chrome = panel_chrome(&row(BoardState::Blocked), PaneView::Conversation, false, false, false, Some(NeedsYou::Blocked { cause: long }), false, String::new());
        assert_eq!(chrome.status.tone, StatusTone::Blocked);
        assert_eq!(chrome.status.text.chars().count(), 60);
    }

    #[test]
    fn working_panels_show_doing_now_and_done_tasks_offer_reopen() {
        let mut r = row(BoardState::Working);
        r.why = "cargo test".into();
        let chrome = panel_chrome(&r, PaneView::Terminal, false, false, false, None, false, String::new());
        assert_eq!(chrome.status.text, "cargo test");
        assert_eq!(chrome.status.tone, StatusTone::Neutral);
        let done = panel_chrome(&row(BoardState::Done), PaneView::Conversation, false, false, false, None, true, String::new());
        assert_eq!(done.primary, PrimaryAction::Reopen);
    }

    #[test]
    fn status_drops_segments_under_320_and_text_under_260() {
        assert_eq!(status_layout(470.0), StatusLayout { show_segments: true, show_text: true });
        assert_eq!(status_layout(319.0), StatusLayout { show_segments: false, show_text: true });
        assert_eq!(status_layout(259.0), StatusLayout { show_segments: false, show_text: false });
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::panel::model -- --test-threads=4`
Expected: compile error.

- [ ] **Step 3: Implement** the types above and:

```rust
pub const STATUS_CAUSE_MAX_CHARS: usize = 60;

pub fn status_layout(panel_width_px: f32) -> StatusLayout {
    StatusLayout { show_segments: panel_width_px >= 320.0, show_text: panel_width_px >= 260.0 }
}

pub fn panel_chrome(row: &BoardRow, view: PaneView, focused: bool, zoomed: bool, minimised: bool, needs_you: Option<NeedsYou>, done: bool, crumb: String) -> PanelChrome {
    let (icon, text, tone) = match (&needs_you, row.state) {
        (Some(NeedsYou::Question { .. }), _) => ("?", "Asked a question".to_string(), StatusTone::Attention),
        (Some(NeedsYou::Permission { .. }), _) => ("?", "Permission".to_string(), StatusTone::Attention),
        (Some(NeedsYou::Blocked { cause }), _) => ("!", bound(cause, STATUS_CAUSE_MAX_CHARS), StatusTone::Blocked),
        (None, BoardState::Working) => ("▶", row.why.clone(), StatusTone::Neutral),
        (None, BoardState::Done) => ("✓", "Done".to_string(), StatusTone::Neutral),
        (None, _) => ("·", row.why.clone(), StatusTone::Neutral),
    };
    PanelChrome {
        key: row.key.clone(), title: row.title.clone(), crumb,
        provider: row.provider, project_colour: row.project_colour,
        status: PanelStatus { icon, text, age: format_age(row.state_age_ms), progress: row.progress, tone },
        needs_you, primary: if done { PrimaryAction::Reopen } else { PrimaryAction::Done },
        view, focused, zoomed, minimised,
    }
}
```

`bound` truncates to N chars with a trailing `…` exactly as plan 1's `activity.rs` does; import it from there rather than duplicating (make plan 1's `bound` `pub(crate)` in `activity.rs`).

- [ ] **Step 4: Run the tests**, expected PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/mod.rs src/ui/panel
git commit -m "feat(panel): pure panel chrome model"
```

---

### Task 5: Panel chrome painter and the ⋯ menu rows

> **Painter task (Task 5).** Load the `frontend-design` skill and Read `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/02-panel-chrome-2.png` and `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.png` BEFORE writing code; copy every size and colour from the mockup HTML (same basename, `.html`) beside the PNG. The reviewer reads the same PNG. See the Visual fidelity rule in Global Constraints.

**Files:**
- Create: `src/ui/panel/render.rs`, `src/ui/panel/menu.rs`
- Test: `menu.rs` (pure), `render.rs` (headless smoke)

**Interfaces:**
- `menu.rs`:

```rust
pub enum PanelMenuItem { AddAction, Commit, Zoom, PinSize, Move, Swap, MoreViews, Rename, Archive, Delete }
pub struct PanelMenuRow { pub item: PanelMenuItem, pub label: &'static str, pub key: &'static str, pub danger: bool, pub separator_before: bool }
pub fn panel_menu_rows(zoomed: bool) -> Vec<PanelMenuRow>   // Zoom label is "Unzoom" when zoomed
pub fn panel_key_action(key: &str, has_pending_question: bool, has_pending_permission: bool) -> Option<PanelKeyAction>
pub enum PanelKeyAction { Menu(PanelMenuItem), Done, Answer(u8), Allow, Deny, ViewDiff, Unzoom }
```

`panel_key_action`: `"a"`→AddAction, `"c"`→Commit, `"z"`→Zoom, `"p"`→PinSize, `"s"`→Swap, `"d"` → ViewDiff when a permission is pending else None, `"1".."9"` → Answer(n) when a question is pending, `"enter"` → Allow when a permission is pending, `"escape"` → Deny when a permission is pending else Unzoom. Ctrl+D → Done is handled by the shell's keyboard model, not here.

- `render.rs`:

```rust
pub struct PanelHandlers {
    pub on_focus: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_select_view: Rc<dyn Fn(&HostTaskKey, PaneView, &mut Window, &mut App)>,
    pub on_primary: Rc<dyn Fn(&HostTaskKey, PrimaryAction, &mut Window, &mut App)>,
    pub on_zoom: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_menu: Rc<dyn Fn(&HostTaskKey, Point<Pixels>, &mut Window, &mut App)>,
    pub on_retry: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_key: Rc<dyn Fn(&HostTaskKey, &KeyDownEvent, &mut Window, &mut App)>,
}
pub const TITLE_ROW_HEIGHT: f32 = 30.0;
pub const TAB_ROW_HEIGHT: f32 = 26.0;
pub const MINIMISED_HEIGHT: f32 = 28.0;
pub fn panel_chrome_element(chrome: &PanelChrome, colours: &ProjectColourBook, tokens: ThemeTokens, width_px: f32, handlers: &PanelHandlers) -> AnyElement
pub fn panel_frame(chrome: &PanelChrome, colours: &ProjectColourBook, tokens: ThemeTokens, chrome_element: AnyElement, body: AnyElement) -> AnyElement
pub fn panel_element_id(key: &HostTaskKey) -> ElementId   // == stable_host_task_element_key(key, "pane")
```

`panel_chrome_element` draws the title row (stripe-offset padding 13 px; provider mark 11 px; title 13 px semibold truncating last; inline status per `status_layout(width)` with the segments reused from plan 1's `render.rs` by moving `segments(...)` into a shared `crate::ui::board::render::segments_element(progress, tokens, show_count)`; ⤢ or ⤡; the primary button; ⋯) and the tab row (5 tabs, active one with `surfaces.selection` background and primary text, others secondary; the tab row has a 1 px bottom border `borders.subtle`). Minimised chrome is the title row alone at 28 px with the primary button and ⋯ hidden. `panel_frame` wraps chrome and body: border 1 px `borders.subtle`, 2 px `borders.focus` when focused; when `needs_you` is Question/Permission the border is `status.attention` at 0.7 alpha plus a 1 px outer glow at 0.25; when Blocked, `status.destructive` the same way; radius `density.radii.md`; the 3 px stripe absolutely positioned on the left edge. Blocked status text carries a trailing "Retry" link element calling `on_retry`.

- [ ] **Step 1: Write the failing menu tests**

```rust
#[test]
fn menu_rows_follow_the_spec_order_with_two_separators() {
    let rows = panel_menu_rows(false);
    let items: Vec<_> = rows.iter().map(|r| r.item).collect();
    assert_eq!(items, vec![AddAction, Commit, Zoom, PinSize, Move, Swap, MoreViews, Rename, Archive, Delete]);
    assert!(rows[2].separator_before && rows[6].separator_before);
    assert!(rows[9].danger);
    assert_eq!(panel_menu_rows(true)[2].label, "Unzoom");
}

#[test]
fn letter_keys_map_only_when_the_matching_state_is_pending() {
    assert_eq!(panel_key_action("3", true, false), Some(PanelKeyAction::Answer(3)));
    assert_eq!(panel_key_action("3", false, false), None);
    assert_eq!(panel_key_action("enter", false, true), Some(PanelKeyAction::Allow));
    assert_eq!(panel_key_action("escape", false, true), Some(PanelKeyAction::Deny));
    assert_eq!(panel_key_action("escape", false, false), Some(PanelKeyAction::Unzoom));
    assert_eq!(panel_key_action("d", false, true), Some(PanelKeyAction::ViewDiff));
    assert_eq!(panel_key_action("d", false, false), None);
    assert_eq!(panel_key_action("z", false, false), Some(PanelKeyAction::Menu(PanelMenuItem::Zoom)));
}
```

- [ ] **Step 2: Run to see them fail**, then **Step 3: implement `menu.rs`** exactly as specified, and `render.rs` following the gpui idioms in the shell's current `render_task_workspace_pane` (`.id(...)`, `.on_mouse_down`, `.on_click`, `.on_drag` is kept by the shell, not here).

- [ ] **Step 4: Headless smoke test** in `render.rs`: build one `PanelChrome` per `NeedsYou` variant plus a Working one, render `panel_chrome_element` at 470 px and at 250 px and `panel_frame` around an empty body, inside `gpui::Application::headless()` as plan 1's board smoke test does. Assert nothing panics.

- [ ] **Step 5: Run** `cargo test --lib ui::panel -- --test-threads=4`, expected PASS.

- [ ] **Step 6: Commit**

```bash
git add src/ui/panel src/ui/board/render.rs
git commit -m "feat(panel): two-row chrome painter and menu vocabulary"
```

---

### Task 6: Question card and permission dock

> **Painter task (Task 6).** Load the `frontend-design` skill and Read `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/06-needs-you-question-1-permission.png` BEFORE writing code; copy every size and colour from the mockup HTML (same basename, `.html`) beside the PNG. The reviewer reads the same PNG. See the Visual fidelity rule in Global Constraints.

**Files:**
- Modify: `src/ui/conversation/render.rs` (`question_element`, `conversation_row_height` for Question)
- Create: `src/ui/panel/permission.rs`
- Test: `conversation/render.rs` (height test), `permission.rs`

**Interfaces:**
- `question_element(prompt, choices, settled_choice, recommended: Option<usize>, tokens) -> AnyElement`: amber label "QUESTION" (10.5 px uppercase, `status.attention`), prompt (12 px primary), one row per choice with its 1-based number in muted text, the recommended row's border `status.attention` at full strength and the others at 0.4 alpha, a settled choice rendered filled and the rest dimmed, footer "Type to answer in your own words" left and "1-N pick · ⏎ send" right. `recommended` is the choice whose text contains "(Recommended)" or "recommended", case-insensitive, else `None`. Card background `surfaces.raised` mixed toward amber at 0.06, border `status.attention` at 0.45, radius 7 px, padding 8 by 10 px.
- `conversation_row_height` for Question becomes `28 + prompt_lines*line_height + choices*26 + 22`.
- `permission.rs`:

```rust
pub struct PermissionHandlers { pub on_allow, pub on_always, pub on_deny, pub on_view_diff: Rc<dyn Fn(&mut Window, &mut App)> }
pub fn permission_names_a_file(summary: &str) -> bool        // true when a token contains '/' or '\' or ends with a known source extension
pub fn permission_dock_element(summary: &str, tokens: ThemeTokens, handlers: &PermissionHandlers) -> AnyElement
```

The dock: label "ALLOW?" in amber, the summary in `Cascadia Mono` 11.5 px on `surfaces.sunken`, then buttons: `Allow ⏎` (amber outline, primary), `Always for this task`, `Deny Esc` (muted), and `D view diff` right-aligned only when `permission_names_a_file`.

- [ ] **Step 1: Write the failing tests**

```rust
// permission.rs
#[test]
fn a_summary_naming_a_file_offers_the_diff() {
    assert!(permission_names_a_file("Write src/terminal/pty.rs (+41 -6)"));
    assert!(permission_names_a_file("Edit C:\\Code\\x\\main.rs"));
    assert!(!permission_names_a_file("cargo test --lib ui::"));
}
// conversation/render.rs
#[test]
fn question_row_height_grows_by_one_line_per_choice() {
    let tokens = crate::ui::tokens::dark(Default::default(), Default::default());
    let two = ConversationRow::Question { id: TimelineItemId::Event(EventId::new()), prompt: "p".into(), choices: vec!["a".into(), "b".into()], settled_choice: None };
    let three = ConversationRow::Question { id: TimelineItemId::Event(EventId::new()), prompt: "p".into(), choices: vec!["a".into(), "b".into(), "c".into()], settled_choice: None };
    assert_eq!(conversation_row_height(&three, tokens) - conversation_row_height(&two, tokens), 26);
}
```

- [ ] **Step 2: Run to see them fail.** **Step 3: Implement.** **Step 4: Run** `cargo test --lib ui::panel::permission ui::conversation -- --test-threads=4`, expected PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/conversation/render.rs src/ui/panel/permission.rs src/ui/panel/mod.rs
git commit -m "feat(panel): amber question card and docked permission prompt"
```

---

### Task 7: Keyboard model

**Files:**
- Modify: `src/ui/actions.rs:339-353` (`KeyboardAction`), `:415-460` (default bindings)
- Modify: `src/ui/native_shell.rs:28017` (`apply_keyboard_shell_effects`)
- Test: `actions.rs`

**Interfaces:**
- `KeyboardAction::SelectDock(DockTool)` is replaced by `KeyboardAction::SelectView(PaneView)` bound to Ctrl+1..5 for `PaneView::TABS`; `KeyboardAction::SettleTask` on Ctrl+D; `KeyboardAction::ToggleZoom` on Ctrl+Shift+Z; `KeyboardAction::MovePane(Edge)` on Ctrl+Shift+arrows; `KeyboardAction::FocusPane(Edge)` on Ctrl+arrows. Alt+digit dock bindings are removed.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn view_tabs_are_ctrl_digits_and_the_dock_bindings_are_gone() {
    let model = KeyboardModel::default();
    assert_eq!(model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(1))), Some(KeyboardAction::SelectView(PaneView::Conversation)));
    assert_eq!(model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(5))), Some(KeyboardAction::SelectView(PaneView::Browser)));
    assert_eq!(model.resolve(KeyboardShortcut::alt(ShortcutKey::Digit(1))), None);
    assert_eq!(model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Character('d'))), Some(KeyboardAction::SettleTask));
    assert_eq!(model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Character('z'))), Some(KeyboardAction::ToggleZoom));
}
```

- [ ] **Step 2: Run to see it fail.** **Step 3: Implement** the enum variants and bindings; if `ShortcutKey` has no arrow variant, add `ShortcutKey::Arrow(ArrowKey)` with `Left, Right, Up, Down`. In `apply_keyboard_shell_effects` route `SelectView(v)` to `set_pane_view(selected, v)`, `SettleTask` to `settle_task_key(selected)`, `ToggleZoom` to `toggle_zoom` (Task 9), `MovePane(edge)` to `move_workspace_pane(focused, DropTarget::Edge{..})`, `FocusPane(edge)` to a new `focus_pane_toward(edge)` that picks the nearest pane rectangle in that direction from the current allocation (nearest by centre distance among panes whose centre lies past the focused pane's edge). Check the shortcut-conflict test still passes (Ctrl+D or Ctrl+Shift+Z may already be bound; if so, report the conflict and choose Ctrl+Shift+D / Ctrl+Shift+Z, updating the spec's key table in the same commit).

- [ ] **Step 4: Run** `cargo test --lib ui::actions -- --test-threads=4`, expected PASS. **Step 5: Commit** `feat(keys): view tabs on ctrl-digits, done, zoom, pane moves`.

---

### Task 8: Done closes the panel; per-key lifecycle actions

**Files:**
- Modify: `src/ui/native_shell.rs:31092-31160` (`archive_selected_task`, `settle_selected_task`, `reopen_task_key`, `begin_task_delete_key`)
- Test: `native_shell.rs` tests

**Interfaces:**
- `fn settle_task_key(&mut self, key: HostTaskKey)` (the body of `settle_selected_task` parameterised by key; `settle_selected_task` calls it), `fn archive_task_key(&mut self, key: HostTaskKey)` likewise. After a successful settle or archive dispatch, the shell removes the task's pane from the workspace (`remove_pane`, collapsing the split), clears zoom if it was that pane, and moves focus to the previously focused pane. `reopen_task_key` and the message-send restore path insert a pane beside the focused one (existing `insert_after_focused`) if the task has none.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn done_closes_the_pane_and_reopen_brings_one_back() {
    // headless harness as in the existing workspace tests; build a client model with two open tasks,
    // both in the workspace; settle the second via settle_task_key; assert the workspace has one pane
    // and its focus is the first task; then reopen_task_key(second) and assert two panes again.
}
```

Write it against the `open_task_without_agent_client_model`-style fixtures the neighbouring tests use; the test must assert `workspace.pane_count()` before and after, not just the absence of a panic.

- [ ] **Step 2: Run to see it fail.** **Step 3: Implement.** **Step 4: Run** `cargo test --lib ui::native_shell::tests::done_closes -- --test-threads=1`. **Step 5: Commit** `feat(shell): done and archive close the pane`.

---

### Task 9: Wire the panel into the shell

> **Painter task (Task 9).** Load the `frontend-design` skill and Read `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.png`, `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/02-panel-chrome-2.png` and `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/06-needs-you-question-1-permission.png` BEFORE writing code; copy every size and colour from the mockup HTML (same basename, `.html`) beside the PNG. The reviewer reads the same PNG. See the Visual fidelity rule in Global Constraints.

**Files:**
- Modify: `src/ui/native_shell.rs`: `render_task_workspace_pane` (24711-24990), `workspace_dock_surface` (28334), the dock render call site (search `workspace_dock_surface(`), `conversation_delete_action` and the whole action strip (34020-34200), `provider_setup_approval_card` (25068), the composer render where `pending_approval_identity` is read (~27884), the chip-menu overlay pattern (27288-27420) for the new pane menu, `render_terminal_chip_menu_overlay`'s caller for overlay placement
- Test: `native_shell.rs` tests

**Interfaces:**
- `fn panel_chrome_for(&mut self, pane: &TaskPaneViewModel<HostTaskKey>, now_ms: i64) -> PanelChrome` builds from `self.board_rows(now_ms)` (find the row by key; if absent, a minimal Idle row), `needs_you` from: pending question (`composer.pending_question_identity()` for the composer owner, else the task snapshot's `provider_sessions[*].open_question.is_some()`), pending approval likewise (`open_approval`), blocked from `task_row_status_for_owner` being Failed/Disconnected/UncertainOutcome with the cause from the provider-terminal unavailable detail the shell already holds (the `TaskCockpitResult::Unavailable { detail }` path at 737-763) or the visible-status label when no detail exists.
- `fn workspace_dock_surface_for(&self, owner: HostTaskKey, tool: CockpitDockTool, tokens, shell_entity) -> AnyElement`: the existing `workspace_dock_surface` with the owner passed in instead of read from `selected_task_key`; `workspace_dock_surface` becomes a thin wrapper for any remaining caller.
- `PaneMenu { owner: HostTaskKey, position: Point<Pixels>, more_views_open: bool, confirming_delete: bool }` stored as `self.pane_menu: Option<PaneMenu>`, rendered as an overlay like the terminal chip menu, dismissed on Esc/outside click; rows from `panel_menu_rows`.

- [ ] **Step 1: Write the failing tests**

Replace `workspace_panes_render_their_exact_task_and_done_has_one_scroll_owner` assertions that reference the 42 px header or the "Compact" control with: the pane element id is `panel_element_id(&key)`; the accessibility tree has a "Conversation" tab node and a "Done" button node for an open task; selecting `PaneView::Files` via `set_pane_view` and re-rendering yields a Files body (assert on the accessibility tree containing the Files panel's region name, which the dock already registers).

Add:

```rust
#[test]
fn a_pending_question_paints_the_amber_card_and_number_keys_answer_it() {
    // fixture: a task with an open question (see the existing question fixtures used by
    // activate_composer_answer_option tests); render; assert the tree has a region named after the
    // prompt and a "QUESTION" label; send key "2" to the pane's on_key handler; assert the dispatched
    // composer intent carries AnswerPayload::Option { index: 1, .. } (use the TestRuntime's recorded
    // commands the neighbouring composer tests inspect).
}

#[test]
fn a_pending_permission_replaces_the_composer_and_enter_allows() {
    // fixture with an open approval; assert no composer input node and an "Allow" button node;
    // send "enter" to the pane's on_key handler; assert the recorded intent is ApprovalDecision::Approve.
}

#[test]
fn always_for_this_task_auto_allows_the_next_matching_request_and_nothing_else() {
    // choose "Always for this task" on an approval whose summary is "Bash: cargo test";
    // admit a second approval "Bash: cargo build" -> auto-approved without a click;
    // admit "Write src/x.rs" -> still pending.
}
```

- [ ] **Step 2: Run to see them fail.**

- [ ] **Step 3: Replace the pane renderer**

`render_task_workspace_pane` becomes:

```rust
let now_ms = /* the shell's wall clock helper */;
let chrome = self.panel_chrome_for(pane, now_ms);
let handlers = PanelHandlers { /* closures over cx.entity().downgrade() calling: select_fleet_task_key (focus), set_pane_view, settle_task_key / reopen_task_key (primary), toggle_zoom, open_pane_menu(owner, position), retry_provider_for(owner), handle_panel_key(owner, event) */ };
let chrome_element = panel_chrome_element(&chrome, &self.project_colours, tokens, f32::from(pane_size.width), &handlers);
let body = if chrome.minimised { div().into_any_element() } else {
    let body_height = (f32::from(pane_size.height) - TITLE_ROW_HEIGHT - TAB_ROW_HEIGHT).max(1.0);
    match chrome.view {
        PaneView::Conversation => self.task_conversation_surface_for(task_key.clone(), pane.build_composer, tokens, size(pane_size.width, px(body_height)), cx),
        PaneView::Terminal => self.task_terminal_surface_for(&task_key, tokens, cx),
        PaneView::Files => self.workspace_dock_surface_for(task_key.clone(), CockpitDockTool::Files, tokens, Some(cx.entity().downgrade())),
        PaneView::Changes => /* Changes */, PaneView::Browser => /* Browser */, PaneView::Review => /* Review */, PaneView::Artifacts => /* Artifacts */, PaneView::Services => /* Services */,
    }
};
let framed = panel_frame(&chrome, &self.project_colours, tokens, chrome_element, body);
// keep: .on_drag(dragged_pane, ...) on the chrome, .can_drop/.drag_over/.on_drop on the frame, and the four edge drop zones, exactly as today.
```

The composer inside `task_conversation_surface_for` is replaced by `permission_dock_element(summary, tokens, handlers)` when the owner has a pending approval: find where the composer element is appended inside that function and branch on `self.composer.as_ref().and_then(|c| c.pending_approval_identity()).is_some()` for the owner. The summary comes from the pending approval projection (`slot.cockpit.pending_approval_projection(...)`, the sibling of `pending_question_projection` used at ~27884). Allow → `activate_composer_approval(ApprovalDecision::Approve)`; Deny → `Reject { reason: None }`; Always → insert `(owner, first_word(summary))` into `self.auto_allow: HashSet<(HostTaskKey, String)>`, then Approve; on every admitted approval, if `(owner, first_word)` is in the set, dispatch Approve immediately. `D` → `set_pane_view(owner, PaneView::Changes)`.

`handle_panel_key(owner, event)`: `panel_key_action(key, has_question, has_permission)` → `Answer(n)` → `activate_composer_answer_option(n-1, choices[n-1])` when `n <= choices.len()`; `Allow`/`Deny`/`ViewDiff` as above; `Unzoom` → `unzoom` when zoomed, else nothing; `Done` unused here; `Menu(item)` → the same handler the menu row would run. Letter keys reach this handler only through the chrome's `on_key_down`; the conversation surface's own input keeps its keys.

Delete the action strip (`conversation_delete_action` and its siblings) and stop rendering the right dock: the dock's toggle actions (`NativeToggleDock`, `DockSelect*`) become no-ops that log once, and `dock_collapsed` is forced true on load. Keep `dock.rs` for its panel renderers; note in the report that the module's chrome (tab strip, collapse button) is now dead code to delete in a follow-up.

- [ ] **Step 4: Blocked Retry**: `retry_provider_for(owner)` calls the existing provider restore/retry path the "Retry" affordance in the unavailable surface uses today (find it near line 737-763 and `provider_terminal_unavailable`); if none exists, it re-selects the task, which is what triggers a restore attempt today, and says so in a comment.

- [ ] **Step 5: Run** `cargo test --lib ui::native_shell -- --test-threads=4`, then the gates, then the full `cargo test --lib -- --test-threads=4`.

- [ ] **Step 6: Commit**

```bash
git add src/ui/native_shell.rs
git commit -m "feat(shell): two-row panel chrome, tabs, needs-you states, zoom, pane menu"
```

---

### Task 10: Visual acceptance against the reference images

> **Painter task (Task 10).** Load the `frontend-design` skill and Read `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.png`, `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/02-panel-chrome-2.png` and `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/06-needs-you-question-1-permission.png` BEFORE writing code; copy every size and colour from the mockup HTML (same basename, `.html`) beside the PNG. The reviewer reads the same PNG. See the Visual fidelity rule in Global Constraints.

Same procedure as plan 1 Task 9, against `01-composition-A.png`, `02-panel-chrome-2.png` and `06-needs-you-question-1-permission.png`. Not allowed: a third chrome row, an action strip, a right dock, a compact summary pane, a question docked at the bottom, a permission card in the stream, any amber or red on a panel that does not need you. The user launches and captures; you compare and list every difference with a verdict.

---

## Self-review

**Spec coverage (sections 3, 6, 10):** composition and zoom, Task 3 and 9; chrome rows and truncation, Tasks 4, 5; views as tabs with More views in the menu, Tasks 1, 5, 9; primary action and menu order, Tasks 5, 9; question card, Task 6 and 9; permission dock with Always-for-this-task and D, Tasks 6, 9; blocked with Retry, Tasks 4, 5, 9; minimised strip and its threshold, Task 2; `PaneView` migration, Task 1; Done and lifecycle, Task 8; keyboard, Tasks 5 (panel letters) and 7 (global chords); the composer placeholder appearing once per panel is inherent to one surface per pane, Task 9.

**Placeholder scan:** the three shell tests in Task 9 are described in prose because they depend on fixture helpers whose exact names live in the 60k-line shell test module; each names the fixture family to copy, the action to send, and the exact assertion, which is what an implementer needs. No "TBD".

**Type consistency:** `PaneView` variants and `TABS`/`MORE` in Tasks 1, 5, 7, 9; `PanelChrome` fields from Task 4 used in 5 and 9; `PanelHandlers`, `panel_chrome_element`, `panel_frame` from Task 5 in 9; `NeedsYou` variants in 4, 6, 9; `PanelKeyAction` from 5 in 9; `settle_task_key`/`archive_task_key` from 8 in 9.
