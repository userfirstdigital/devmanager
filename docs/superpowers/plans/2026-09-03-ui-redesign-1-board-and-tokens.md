# UI Redesign 1: Board and Tokens Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the project-grouped task rail with the state-grouped board from the spec (two-line boxed rows, project stripe, progress segments, provider mark) on the redesigned dark token set.

**Architecture:** A new pure module `src/ui/board/` derives a `BoardModel` (groups of `BoardRow`) from facts the shell already holds: the fleet rows, each task's visible status, its semantic journal page, and a client-local state clock. A single gpui painter in `src/ui/board/render.rs` draws it. `native_shell.rs` swaps its project-grouped `uniform_list` for the board painter and keeps every existing row handler (select, rename, delete). Colours come only from `ThemeTokens`, whose dark values change to the spec palette.

**Tech Stack:** Rust, gpui (`div`, `px`, `uniform_list`, `svg`), gpui-component 0.5.1, serde for the layout store. No new crates.

**Spec:** `docs/superpowers/specs/2026-09-03-ui-redesign-design.md` sections 4, 5, 9 (scrollbars only by reference) and 12. Reference images: `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/03-board-rows-boxed-A.png`, `04-project-stripe-1.png`, `05-provider-mark-1.png`.

## Global Constraints

- Groups in this fixed order with these labels: `NEEDS YOU`, `WORKING`, `IDLE`, `DONE` (collapsed by default). Archived tasks are not on the board.
- Row: full-width box, 1 px line above and below, no side margins, 3 px gap between rows; 3 px project stripe on the row's left edge, full row height.
- State dot 7 px: attention amber for question/permission, destructive red for blocked, mid-grey for Working, dark grey for Idle and Done.
- Title 12.5 px single line with ellipsis, white on Needs-you rows; age 10.5 px muted right-aligned on the title line, formatted `12s`, `4m`, `2h`, `3d`.
- Second line 10.5 px muted: why / doing-now / "Last reply 18m" on the left; progress segments (9 by 4 px, 2 px gaps) plus `3/5` count, then the 11 px provider mark, on the right. Segments only when at least one plan step exists; never a smooth bar, never a percent. Doing-now text bounded to 40 characters.
- Below 200 px row width drop the count first, then the segments.
- Colour is reserved for state that needs you: amber `#f2b441`, red `#e5484d`. Grey ramp exactly as spec 5.1: column `#101013`, row `#151518`, selected `#1a1a20`, stream `#111114`, terminal `#0b0b0d`, border subtle `#26262b`, border strong `#34343c`, text primary `#e6e6ea`, secondary `#9a9aa3`, muted `#6b6b74`, success `#7fb07f`.
- Project palette, in assignment order: teal `#5aa3a0`, slate `#7a86c4`, sand `#a78a5c`, mauve `#8c6fa8`, moss `#7a9a6a`, dusk `#9a7a8a`, steel `#6f8fa8`, clay `#a8806f`.
- Provider marks are monochrome SVGs under `assets/icons/provider-{claude,codex,cursor,other}.svg`, each with a stated origin comment; no brand tints.
- Board column default 236 px, collapsible to a 36 px rail; `+ New` in the header; ⋯ menu with Archived…, Collapse to rail, Density.
- The shell's existing row gestures stay: left click selects (`select_fleet_task_key`), shift toggles, right click renames a live task, archived rows keep their Delete affordance, Enter/Space select, other keys forward to the terminal exactly as today.
- Work in your own git worktree off `VisualDevManager`; `$env:CARGO_TARGET_DIR` is your own `C:\Temp\devmanager-*`; copy `web/src/connect/wasm/` from the main checkout if `build.rs` reports a stale fingerprint. Never write to `<repo>/.devmanager-next/`. Never launch the app; the user launches it.
- LF line endings. Gates: `cargo check --locked --lib --bins --tests -j 2` EXIT 0; `cargo fmt --all -- --check` EXIT 0; no new warnings; targeted tests while iterating, one full `cargo test --lib -- --test-threads=4` before handing back.
- Commit trailer on every commit: `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` then `Claude-Session: https://claude.ai/code/session_01BJgrNVntfxuTu79ugvwNHm`.
- The reference PNGs are the acceptance criterion for appearance (spec 12). Task 9 compares a real capture against them.

---

## File structure

| File | Responsibility |
|---|---|
| `src/ui/tokens.rs` (modify) | Dark palette values; new `SurfaceTokens::stream`; terminal background |
| `assets/icons/provider-*.svg` (create) | Four 24-unit monochrome marks |
| `src/icons.rs` (modify) | Paths for the four marks |
| `src/ui/task_cockpit/inbox.rs` (modify) | `PrimaryProviderIcon::glyph_path()` |
| `src/ui/board/mod.rs` (create) | Module root and re-exports |
| `src/ui/board/model.rs` (create) | `BoardState`, `BoardGroup`, `BoardRow`, `BoardModel`, grouping and sorting |
| `src/ui/board/age.rs` (create) | `format_age`, `StateClock` |
| `src/ui/board/activity.rs` (create) | `board_activity` from journal facts: progress and doing-now |
| `src/ui/board/project_colour.rs` (create) | Palette and `ProjectColourBook` |
| `src/ui/board/layout.rs` (create) | `BoardRowLayout` width decisions, `BOARD_ROW_HEIGHT` |
| `src/ui/board/render.rs` (create) | gpui painter for header, groups, rows, rail |
| `src/ui/workspace_layout.rs` (modify) | `project_colours` field, `board_rail` field |
| `src/ui/native_shell.rs` (modify) | Build `BoardModel`, paint it, keep handlers, accessibility tree |
| `src/ui/mod.rs` (modify) | `pub mod board;` |

---

### Task 1: Dark tokens to the spec palette

**Files:**
- Modify: `src/ui/tokens.rs` (constants near lines 1405-1433; `SurfaceTokens` at 249; `dark_theme` builder around 1780-1830; `semantic_color_tokens` at 703)
- Test: `src/ui/tokens.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `tokens.surfaces.canvas` (column), `.raised` (row box), `.selection` (selected row), `.sunken` (stream), new `.stream` alias field is NOT added; instead the spec's `surface.stream` maps to `sunken`. `tokens.terminal.background` is the terminal surface. `tokens.borders.subtle`, `.strong`, `tokens.text.primary/secondary/muted`, `tokens.status.attention/destructive/success/inactive`.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `src/ui/tokens.rs`:

```rust
#[test]
fn dark_palette_matches_the_redesign_spec() {
    let t = dark(Density::Comfortable, Scale::Default);
    assert_eq!(t.surfaces.canvas, Color::from_u32(0x101013), "column");
    assert_eq!(t.surfaces.raised, Color::from_u32(0x151518), "row box");
    assert_eq!(t.surfaces.selection, Color::from_u32(0x1a1a20), "selected row");
    assert_eq!(t.surfaces.sunken, Color::from_u32(0x111114), "stream");
    assert_eq!(t.terminal.background, Color::from_u32(0x0b0b0d), "terminal");
    assert_eq!(t.borders.subtle, Color::from_u32(0x26262b));
    assert_eq!(t.borders.strong, Color::from_u32(0x34343c));
    assert_eq!(t.text.primary, Color::from_u32(0xe6e6ea));
    assert_eq!(t.text.secondary, Color::from_u32(0x9a9aa3));
    assert_eq!(t.text.muted, Color::from_u32(0x6b6b74));
    assert_eq!(t.status.attention, Color::from_u32(0xf2b441));
    assert_eq!(t.status.destructive, Color::from_u32(0xe5484d));
    assert_eq!(t.status.success, Color::from_u32(0x7fb07f));
}
```

If `Density::Comfortable` / `Scale::Default` are not the variant names, use the ones the existing `dark(...)` tests use; do not invent variants.

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --lib ui::tokens::tests::dark_palette_matches_the_redesign_spec -- --exact`
Expected: FAIL on the first assertion (canvas is `0x1f1a24` today).

- [ ] **Step 3: Change the dark constants**

In `src/ui/tokens.rs` replace the dark constant values (keep the names):

```rust
const DARK_SURFACE_CANVAS: Color = Color::from_u32(0x101013);
const DARK_SURFACE_RAISED: Color = Color::from_u32(0x151518);
const DARK_SURFACE_OVERLAY: Color = Color::from_u32(0x1a1a1f);
const DARK_SURFACE_SUNKEN: Color = Color::from_u32(0x111114);
const DARK_SURFACE_HOVER: Color = Color::from_u32(0x17171c);
const DARK_SURFACE_SELECTION: Color = Color::from_u32(0x1a1a20);
const DARK_TEXT_PRIMARY: Color = Color::from_u32(0xe6e6ea);
const DARK_TEXT_SECONDARY: Color = Color::from_u32(0x9a9aa3);
const DARK_TEXT_MUTED: Color = Color::from_u32(0x6b6b74);
const DARK_BORDER_SUBTLE: Color = Color::from_u32(0x26262b);
const DARK_BORDER_DEFAULT: Color = Color::from_u32(0x2c2c33);
const DARK_BORDER_STRONG: Color = Color::from_u32(0x34343c);
const DARK_STATUS_ATTENTION: Color = Color::from_u32(0xf2b441);
const DARK_STATUS_SUCCESS: Color = Color::from_u32(0x7fb07f);
const DARK_STATUS_DESTRUCTIVE: Color = Color::from_u32(0xe5484d);
const DARK_STATUS_INACTIVE: Color = Color::from_u32(0x6b6b74);
```

Set the dark `TerminalPalette` background constant to `0x0b0b0d` (find it with `grep -n 'TERMINAL' src/ui/tokens.rs`; the terminal palette has a background field). Leave `text.on_selection` readable on `0x1a1a20`: set it to `0xffffff`. Leave light and high-contrast themes untouched.

- [ ] **Step 4: Run the tokens tests**

Run: `cargo test --lib ui::tokens -- --test-threads=4`
Expected: PASS. If an existing contrast gate fails for `text.muted` on `surfaces.raised`, the gate outranks the spec number: raise muted to the nearest value that passes (try `0x76767f`, then `0x808089`), re-run, and record the value you landed on in your report. Do not weaken the gate.

- [ ] **Step 5: Commit**

```bash
git add src/ui/tokens.rs
git commit -m "feat(ui): dark palette to the redesign spec"
```

---

### Task 2: Provider marks

**Files:**
- Create: `assets/icons/provider-claude.svg`, `assets/icons/provider-codex.svg`, `assets/icons/provider-cursor.svg`, `assets/icons/provider-other.svg`
- Modify: `src/icons.rs` (add constants after `PANEL_RIGHT`)
- Modify: `src/ui/task_cockpit/inbox.rs:690-706` (`PrimaryProviderIcon`)
- Test: `src/icons.rs` (new tests module) and `src/ui/task_cockpit/inbox.rs` tests

**Interfaces:**
- Produces: `crate::icons::PROVIDER_CLAUDE`, `PROVIDER_CODEX`, `PROVIDER_CURSOR`, `PROVIDER_OTHER: &str`; `PrimaryProviderIcon::glyph_path(self) -> &'static str`.

- [ ] **Step 1: Write the failing tests**

In `src/icons.rs` add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_marks_exist_and_are_small_monochrome_svgs() {
        for path in [PROVIDER_CLAUDE, PROVIDER_CODEX, PROVIDER_CURSOR, PROVIDER_OTHER] {
            let full = crate::assets::asset_path(path);
            let bytes = std::fs::read(&full).unwrap_or_else(|e| panic!("{full:?}: {e}"));
            assert!(bytes.len() < 2048, "{path} must stay under 2 KB");
            let text = String::from_utf8(bytes).expect("utf-8 svg");
            assert!(text.contains("<svg"), "{path} is not an svg");
            assert!(text.contains("currentColor"), "{path} must be monochrome via currentColor");
            assert!(text.contains("<!--"), "{path} must state its origin in a comment");
        }
    }
}
```

In the `inbox.rs` tests module add:

```rust
#[test]
fn provider_icon_glyph_paths_are_distinct_asset_paths() {
    let paths = [
        PrimaryProviderIcon::Claude.glyph_path(),
        PrimaryProviderIcon::Codex.glyph_path(),
        PrimaryProviderIcon::Cursor.glyph_path(),
        PrimaryProviderIcon::Other.glyph_path(),
    ];
    let unique: std::collections::BTreeSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), 4);
    assert!(paths.iter().all(|p| p.starts_with("icons/provider-")));
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib icons::tests provider_icon_glyph_paths -- --test-threads=4`
Expected: compile error, constants and method missing.

- [ ] **Step 3: Add the SVGs**

Write each file as a 24-unit viewBox using `fill="currentColor"` (or `stroke="currentColor"` with `fill="none"`), with a leading comment naming its origin. Origin rule: if the vendor publishes a mark under a licence that permits redistribution, trace that; otherwise use the neutral stand-in shape named here and say so in the comment. Stand-ins:

```svg
<!-- provider-claude.svg: neutral stand-in (eight-point spark); replace with the vendor mark only if its licence permits redistribution. -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2l2.2 6.3L21 9l-5.4 4.1L17.5 20 12 16.4 6.5 20l1.9-6.9L3 9l6.8-.7z"/></svg>
```

```svg
<!-- provider-codex.svg: neutral stand-in (hexagon with core). -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"><path d="M12 3l7.8 4.5v9L12 21l-7.8-4.5v-9z"/><circle cx="12" cy="12" r="2.6" fill="currentColor" stroke="none"/></svg>
```

```svg
<!-- provider-cursor.svg: neutral stand-in (pointer). -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><path d="M5 3l14 9-6.2 1.6L9.4 20z"/></svg>
```

```svg
<!-- provider-other.svg: generic agent (circle). -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"><circle cx="12" cy="12" r="8"/></svg>
```

- [ ] **Step 4: Add the constants and the method**

`src/icons.rs`:

```rust
pub const PROVIDER_CLAUDE: &str = "icons/provider-claude.svg";
pub const PROVIDER_CODEX: &str = "icons/provider-codex.svg";
pub const PROVIDER_CURSOR: &str = "icons/provider-cursor.svg";
pub const PROVIDER_OTHER: &str = "icons/provider-other.svg";
```

`src/ui/task_cockpit/inbox.rs`, inside `impl PrimaryProviderIcon`:

```rust
pub fn glyph_path(self) -> &'static str {
    match self {
        Self::Claude => crate::icons::PROVIDER_CLAUDE,
        Self::Codex => crate::icons::PROVIDER_CODEX,
        Self::Cursor => crate::icons::PROVIDER_CURSOR,
        Self::Other => crate::icons::PROVIDER_OTHER,
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib icons::tests provider_icon_glyph_paths -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add assets/icons/provider-*.svg src/icons.rs src/ui/task_cockpit/inbox.rs
git commit -m "feat(ui): monochrome provider marks"
```

---

### Task 3: Board model, grouping and sorting

**Files:**
- Create: `src/ui/board/mod.rs`, `src/ui/board/model.rs`
- Modify: `src/ui/mod.rs` (add `pub mod board;`)
- Test: `src/ui/board/model.rs`

**Interfaces:**
- Consumes: `crate::domain::task::VisibleTaskStatus`, `crate::ui::native_host_state::HostTaskKey`, `crate::ui::task_cockpit::inbox::PrimaryProviderIcon`.
- Produces:

```rust
pub enum BoardState { Question, Permission, Blocked, Working, Idle, Done }
pub enum BoardGroup { NeedsYou, Working, Idle, Done }
pub struct BoardProgress { pub completed: usize, pub total: usize }
pub struct BoardRow {
    pub key: HostTaskKey,
    pub title: String,
    pub state: BoardState,
    pub why: String,          // second-line left text
    pub state_age_ms: i64,    // time in current state
    pub progress: Option<BoardProgress>,
    pub provider: PrimaryProviderIcon,
    pub project_colour: u8,   // palette index
    pub project_label: String, // tooltip only
    pub branch: String,        // tooltip only
    pub last_activity_ms: i64,
    pub selected: bool,
    pub open: bool,
}
pub struct BoardGroupModel { pub group: BoardGroup, pub rows: Vec<BoardRow>, pub collapsed: bool }
pub struct BoardModel { pub groups: Vec<BoardGroupModel> }
pub fn board_state_of(status: VisibleTaskStatus, done: bool) -> BoardState
pub fn group_of(state: BoardState) -> BoardGroup
pub fn build_board_model(rows: Vec<BoardRow>, done_expanded: bool) -> BoardModel
impl BoardGroup { pub fn label(self) -> &'static str }
impl BoardState { pub fn why_label(self) -> &'static str }
```

- [ ] **Step 1: Write the failing tests**

`src/ui/board/model.rs` tests module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::TaskId;
    use crate::ui::native_host_state::{HostId, HostTaskKey};

    fn row(state: BoardState, state_age_ms: i64, last_activity_ms: i64) -> BoardRow {
        BoardRow {
            key: HostTaskKey::new(HostId::LocalProfile("p".into()), TaskId::new()),
            title: "t".into(),
            state,
            why: state.why_label().to_string(),
            state_age_ms,
            progress: None,
            provider: PrimaryProviderIcon::Claude,
            project_colour: 0,
            project_label: "p".into(),
            branch: "main".into(),
            last_activity_ms,
            selected: false,
            open: false,
        }
    }

    #[test]
    fn visible_status_maps_onto_board_states() {
        use VisibleTaskStatus as V;
        assert_eq!(board_state_of(V::NeedsAnswer, false), BoardState::Question);
        assert_eq!(board_state_of(V::NeedsApproval, false), BoardState::Permission);
        assert_eq!(board_state_of(V::Failed, false), BoardState::Blocked);
        assert_eq!(board_state_of(V::Disconnected, false), BoardState::Blocked);
        assert_eq!(board_state_of(V::UncertainOutcome, false), BoardState::Blocked);
        assert_eq!(board_state_of(V::Working, false), BoardState::Working);
        assert_eq!(board_state_of(V::Settling, false), BoardState::Working);
        assert_eq!(board_state_of(V::ReadyForReview, false), BoardState::Idle);
        assert_eq!(board_state_of(V::Idle, false), BoardState::Idle);
        assert_eq!(board_state_of(V::Working, true), BoardState::Done, "done wins");
    }

    #[test]
    fn groups_come_in_fixed_order_and_done_is_collapsed_by_default() {
        let model = build_board_model(
            vec![row(BoardState::Idle, 1, 1), row(BoardState::Done, 1, 1), row(BoardState::Question, 1, 1), row(BoardState::Working, 1, 1)],
            false,
        );
        let groups: Vec<_> = model.groups.iter().map(|g| g.group).collect();
        assert_eq!(groups, vec![BoardGroup::NeedsYou, BoardGroup::Working, BoardGroup::Idle, BoardGroup::Done]);
        assert!(model.groups[3].collapsed);
        assert_eq!(model.groups[3].rows.len(), 1, "collapsed groups keep their rows for the count");
    }

    #[test]
    fn needs_you_sorts_oldest_ask_first_and_others_most_recent_first() {
        let model = build_board_model(
            vec![
                row(BoardState::Question, 5_000, 10),
                row(BoardState::Permission, 60_000, 20),
                row(BoardState::Working, 1, 100),
                row(BoardState::Working, 1, 300),
            ],
            true,
        );
        let needs: Vec<_> = model.groups[0].rows.iter().map(|r| r.state_age_ms).collect();
        assert_eq!(needs, vec![60_000, 5_000]);
        let working: Vec<_> = model.groups[1].rows.iter().map(|r| r.last_activity_ms).collect();
        assert_eq!(working, vec![300, 100]);
    }

    #[test]
    fn empty_groups_are_omitted_except_done() {
        let model = build_board_model(vec![row(BoardState::Working, 1, 1)], false);
        let groups: Vec<_> = model.groups.iter().map(|g| g.group).collect();
        assert_eq!(groups, vec![BoardGroup::Working, BoardGroup::Done]);
    }

    #[test]
    fn labels_are_the_spec_strings() {
        assert_eq!(BoardGroup::NeedsYou.label(), "Needs you");
        assert_eq!(BoardState::Question.why_label(), "Asked a question");
        assert_eq!(BoardState::Permission.why_label(), "Permission");
        assert_eq!(BoardState::Blocked.why_label(), "Blocked");
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::board::model -- --test-threads=4`
Expected: compile error, module missing.

- [ ] **Step 3: Implement**

`src/ui/mod.rs`: add `pub mod board;` beside the other modules.

`src/ui/board/mod.rs`:

```rust
//! The state-grouped task board (spec 2026-09-03 section 4). Pure model in
//! `model`, `age`, `activity`, `project_colour`, `layout`; the only gpui code
//! is `render`.

pub mod activity;
pub mod age;
pub mod layout;
pub mod model;
pub mod project_colour;
pub mod render;

pub use model::{
    board_state_of, build_board_model, group_of, BoardGroup, BoardGroupModel, BoardModel,
    BoardProgress, BoardRow, BoardState,
};
```

(Add each submodule file as its task lands; until then declare only the ones that exist.)

`src/ui/board/model.rs`:

```rust
use crate::domain::task::VisibleTaskStatus;
use crate::ui::native_host_state::HostTaskKey;
use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoardState {
    Question,
    Permission,
    Blocked,
    Working,
    Idle,
    Done,
}

impl BoardState {
    pub fn why_label(self) -> &'static str {
        match self {
            Self::Question => "Asked a question",
            Self::Permission => "Permission",
            Self::Blocked => "Blocked",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Done => "Done",
        }
    }

    pub fn needs_you(self) -> bool {
        matches!(self, Self::Question | Self::Permission | Self::Blocked)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoardGroup {
    NeedsYou,
    Working,
    Idle,
    Done,
}

impl BoardGroup {
    pub const ORDER: [Self; 4] = [Self::NeedsYou, Self::Working, Self::Idle, Self::Done];

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Done => "Done",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardProgress {
    pub completed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardRow {
    pub key: HostTaskKey,
    pub title: String,
    pub state: BoardState,
    pub why: String,
    pub state_age_ms: i64,
    pub progress: Option<BoardProgress>,
    pub provider: PrimaryProviderIcon,
    pub project_colour: u8,
    /// Shown only in the hover tooltip; the stripe carries the project on the row.
    pub project_label: String,
    pub branch: String,
    pub last_activity_ms: i64,
    pub selected: bool,
    pub open: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardGroupModel {
    pub group: BoardGroup,
    pub rows: Vec<BoardRow>,
    pub collapsed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardModel {
    pub groups: Vec<BoardGroupModel>,
}

pub fn board_state_of(status: VisibleTaskStatus, done: bool) -> BoardState {
    if done {
        return BoardState::Done;
    }
    match status {
        VisibleTaskStatus::NeedsAnswer => BoardState::Question,
        VisibleTaskStatus::NeedsApproval => BoardState::Permission,
        VisibleTaskStatus::Failed
        | VisibleTaskStatus::Disconnected
        | VisibleTaskStatus::UncertainOutcome => BoardState::Blocked,
        VisibleTaskStatus::Working | VisibleTaskStatus::Settling => BoardState::Working,
        VisibleTaskStatus::ReadyForReview | VisibleTaskStatus::Idle => BoardState::Idle,
    }
}

pub fn group_of(state: BoardState) -> BoardGroup {
    match state {
        BoardState::Question | BoardState::Permission | BoardState::Blocked => BoardGroup::NeedsYou,
        BoardState::Working => BoardGroup::Working,
        BoardState::Idle => BoardGroup::Idle,
        BoardState::Done => BoardGroup::Done,
    }
}

/// Groups in fixed order. Empty live groups are omitted; Done is always
/// present so its count and disclosure have a home. Needs-you sorts oldest
/// ask first (it has waited longest); the rest sort most recent activity first.
pub fn build_board_model(rows: Vec<BoardRow>, done_expanded: bool) -> BoardModel {
    let mut buckets: [Vec<BoardRow>; 4] = Default::default();
    for row in rows {
        let index = BoardGroup::ORDER
            .iter()
            .position(|g| *g == group_of(row.state))
            .expect("every group is in ORDER");
        buckets[index].push(row);
    }
    let mut groups = Vec::with_capacity(4);
    for (index, group) in BoardGroup::ORDER.iter().copied().enumerate() {
        let mut rows = std::mem::take(&mut buckets[index]);
        match group {
            BoardGroup::NeedsYou => rows.sort_by(|a, b| b.state_age_ms.cmp(&a.state_age_ms)),
            _ => rows.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms)),
        }
        if rows.is_empty() && group != BoardGroup::Done {
            continue;
        }
        groups.push(BoardGroupModel {
            group,
            rows,
            collapsed: group == BoardGroup::Done && !done_expanded,
        });
    }
    BoardModel { groups }
}
```

If `HostId::LocalProfile` takes a different payload type than a `String`, adjust the test helper to whatever `local_host_id()` returns in the shell tests (search `HostId::LocalProfile(` in `native_shell.rs` tests).

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::board::model -- --test-threads=4`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/ui/mod.rs src/ui/board/mod.rs src/ui/board/model.rs
git commit -m "feat(board): pure board model with state groups and sorting"
```

---

### Task 4: Age formatting and the state clock

**Files:**
- Create: `src/ui/board/age.rs`
- Modify: `src/ui/board/mod.rs` (declare `pub mod age;`, re-export `format_age`, `StateClock`)
- Test: `src/ui/board/age.rs`

**Interfaces:**
- Produces: `pub fn format_age(elapsed_ms: i64) -> String`; `pub struct StateClock<K>` with `pub fn observe(&mut self, key: K, state: BoardState, now_ms: i64) -> i64` returning milliseconds in the current state, and `pub fn forget(&mut self, key: &K)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board::model::BoardState;

    #[test]
    fn age_uses_the_spec_units() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(12_000), "12s");
        assert_eq!(format_age(59_999), "59s");
        assert_eq!(format_age(60_000), "1m");
        assert_eq!(format_age(4 * 60_000 + 30_000), "4m");
        assert_eq!(format_age(2 * 3_600_000), "2h");
        assert_eq!(format_age(3 * 86_400_000 + 3_600_000), "3d");
        assert_eq!(format_age(-5_000), "0s", "clock skew never shows negative");
    }

    #[test]
    fn state_clock_counts_from_the_last_state_change() {
        let mut clock = StateClock::new();
        assert_eq!(clock.observe("a", BoardState::Working, 1_000), 0);
        assert_eq!(clock.observe("a", BoardState::Working, 5_000), 4_000);
        assert_eq!(clock.observe("a", BoardState::Question, 9_000), 0, "state changed");
        assert_eq!(clock.observe("a", BoardState::Question, 9_500), 500);
        clock.forget(&"a");
        assert_eq!(clock.observe("a", BoardState::Question, 20_000), 0);
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::board::age -- --test-threads=4`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
//! Time-in-state for board rows. The kernel records when events occurred,
//! not when a task entered its current visible state, so the client keeps a
//! transient clock keyed by task. It is never persisted.

use std::collections::HashMap;
use std::hash::Hash;

use super::model::BoardState;

pub fn format_age(elapsed_ms: i64) -> String {
    let seconds = elapsed_ms.max(0) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[derive(Debug, Default)]
pub struct StateClock<K: Hash + Eq> {
    entered: HashMap<K, (BoardState, i64)>,
}

impl<K: Hash + Eq> StateClock<K> {
    pub fn new() -> Self {
        Self {
            entered: HashMap::new(),
        }
    }

    /// Records the state seen now and returns how long it has been held.
    pub fn observe(&mut self, key: K, state: BoardState, now_ms: i64) -> i64 {
        match self.entered.get(&key) {
            Some((seen, since)) if *seen == state => (now_ms - since).max(0),
            _ => {
                self.entered.insert(key, (state, now_ms));
                0
            }
        }
    }

    pub fn forget(&mut self, key: &K) {
        self.entered.remove(key);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::board::age -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/board/age.rs src/ui/board/mod.rs
git commit -m "feat(board): age formatting and transient state clock"
```

---

### Task 5: Progress and doing-now from the journal page

**Files:**
- Create: `src/ui/board/activity.rs`
- Modify: `src/ui/board/mod.rs`
- Test: `src/ui/board/activity.rs`

**Interfaces:**
- Consumes: `crate::domain::snapshot::{SemanticJournalFact, SemanticJournalPayload}` (`src/domain/snapshot.rs:433`, payload enum at ~224), `crate::domain::PlanStepStatus::from_wire`.
- Produces: `pub struct BoardActivity { pub progress: Option<BoardProgress>, pub doing_now: Option<String> }`, `pub fn board_activity(facts: &[SemanticJournalFact]) -> BoardActivity`, `pub const DOING_NOW_MAX_CHARS: usize = 40`.

- [ ] **Step 1: Write the failing tests**

Look at how the existing `snapshot.rs` tests construct a `SemanticJournalFact` (search `SemanticJournalFact {` under `#[cfg(test)]` in `src/domain/snapshot.rs`) and write a local helper `fact(sequence, payload)` that fills the other fields with those same defaults. Then:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::snapshot::SemanticJournalPayload as P;

    #[test]
    fn no_plan_steps_means_no_progress() {
        let facts = vec![fact(1, P::UserMessage { text: "hi".into() })];
        assert_eq!(board_activity(&facts).progress, None);
    }

    #[test]
    fn progress_counts_latest_status_per_step_and_never_unknown_as_done() {
        let facts = vec![
            fact(1, P::PlanStep { step_id: "1".into(), title: "a".into(), status: "pending".into() }),
            fact(2, P::PlanStep { step_id: "2".into(), title: "b".into(), status: "pending".into() }),
            fact(3, P::PlanStep { step_id: "1".into(), title: "a".into(), status: "in_progress".into() }),
            fact(4, P::PlanStep { step_id: "1".into(), title: "a".into(), status: "completed".into() }),
            fact(5, P::PlanStep { step_id: "3".into(), title: "c".into(), status: "weird-state".into() }),
        ];
        assert_eq!(board_activity(&facts).progress, Some(BoardProgress { completed: 1, total: 3 }));
    }

    #[test]
    fn a_growing_list_only_grows_the_total() {
        let mut facts = vec![
            fact(1, P::PlanStep { step_id: "1".into(), title: "a".into(), status: "completed".into() }),
            fact(2, P::PlanStep { step_id: "2".into(), title: "b".into(), status: "completed".into() }),
        ];
        assert_eq!(board_activity(&facts).progress, Some(BoardProgress { completed: 2, total: 2 }));
        facts.push(fact(3, P::PlanStep { step_id: "3".into(), title: "c".into(), status: "pending".into() }));
        assert_eq!(board_activity(&facts).progress, Some(BoardProgress { completed: 2, total: 3 }));
    }

    #[test]
    fn doing_now_is_the_last_unresolved_tool_call_bounded_to_forty_chars() {
        let long = "x".repeat(100);
        let facts = vec![
            fact(1, P::ToolCall { tool_name: "Read".into(), call_id: "c1".into() }),
            fact(2, P::ToolResult { call_id: "c1".into(), status: "ok".into() }),
            fact(3, P::ToolCall { tool_name: long.clone(), call_id: "c2".into() }),
        ];
        let doing = board_activity(&facts).doing_now.expect("open tool call");
        assert_eq!(doing.chars().count(), DOING_NOW_MAX_CHARS);
        assert!(doing.ends_with('…'));
    }

    #[test]
    fn doing_now_is_none_when_every_call_has_a_result() {
        let facts = vec![
            fact(1, P::ToolCall { tool_name: "Bash".into(), call_id: "c1".into() }),
            fact(2, P::ToolResult { call_id: "c1".into(), status: "ok".into() }),
        ];
        assert_eq!(board_activity(&facts).doing_now, None);
    }

    #[test]
    fn reasoning_after_the_last_tool_reads_as_thinking() {
        let facts = vec![
            fact(1, P::ToolCall { tool_name: "Bash".into(), call_id: "c1".into() }),
            fact(2, P::ToolResult { call_id: "c1".into(), status: "ok".into() }),
            fact(3, P::ReasoningSummary { text: "hmm".into() }),
        ];
        assert_eq!(board_activity(&facts).doing_now.as_deref(), Some("Thinking"));
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::board::activity -- --test-threads=4`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
//! Board facts derived from a task's semantic journal page: plan progress
//! (one segment per step) and the "doing now" text. Pure; no gpui.

use std::collections::BTreeMap;

use crate::domain::snapshot::{SemanticJournalFact, SemanticJournalPayload};
use crate::domain::PlanStepStatus;

use super::model::BoardProgress;

pub const DOING_NOW_MAX_CHARS: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardActivity {
    pub progress: Option<BoardProgress>,
    pub doing_now: Option<String>,
}

pub fn board_activity(facts: &[SemanticJournalFact]) -> BoardActivity {
    let mut steps: BTreeMap<&str, Option<PlanStepStatus>> = BTreeMap::new();
    let mut open_calls: BTreeMap<&str, &str> = BTreeMap::new();
    let mut last_tool_sequence: Option<u64> = None;
    let mut last_reasoning_sequence: Option<u64> = None;
    for fact in facts {
        match &fact.payload {
            SemanticJournalPayload::PlanStep { step_id, status, .. } => {
                steps.insert(step_id.as_str(), PlanStepStatus::from_wire(status));
            }
            SemanticJournalPayload::ToolCall { tool_name, call_id } => {
                open_calls.insert(call_id.as_str(), tool_name.as_str());
                last_tool_sequence = Some(fact.sequence);
            }
            SemanticJournalPayload::ToolResult { call_id, .. } => {
                open_calls.remove(call_id.as_str());
            }
            SemanticJournalPayload::ReasoningSummary { .. } => {
                last_reasoning_sequence = Some(fact.sequence);
            }
            _ => {}
        }
    }
    let progress = (!steps.is_empty()).then(|| BoardProgress {
        // An unrecognised provider status is deliberately not completion.
        completed: steps
            .values()
            .filter(|s| **s == Some(PlanStepStatus::Completed))
            .count(),
        total: steps.len(),
    });
    let doing_now = if let Some((_, tool)) = open_calls.iter().next_back() {
        Some(bound(tool))
    } else if last_reasoning_sequence > last_tool_sequence && last_reasoning_sequence.is_some() {
        Some("Thinking".to_string())
    } else {
        None
    };
    BoardActivity { progress, doing_now }
}

fn bound(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(DOING_NOW_MAX_CHARS - 1).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}
```

`open_calls.iter().next_back()` returns the lexically last call id, not the latest; if call ids are not monotonic in the fixtures, keep a `Vec<(u64, &str, &str)>` of open calls ordered by `fact.sequence` and take the last instead. The test with two calls settles which you need. If `SemanticJournalFact`'s payload field is not named `payload`, use the real name from line 433 onward.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::board::activity -- --test-threads=4`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add src/ui/board/activity.rs src/ui/board/mod.rs
git commit -m "feat(board): plan progress and doing-now from the journal page"
```

---

### Task 6: Project colours, persisted

**Files:**
- Create: `src/ui/board/project_colour.rs`
- Modify: `src/ui/workspace_layout.rs:91-130` (`KeyedWorkspaceLayout`: add `project_colours`, `board_rail`)
- Modify: `src/ui/board/mod.rs`
- Test: `src/ui/board/project_colour.rs`, `src/ui/workspace_layout.rs` tests

**Interfaces:**
- Produces: `pub const PROJECT_PALETTE: [Color; 8]`; `pub struct ProjectColourBook { assignments: BTreeMap<String, u8> }` with `pub fn colour_index(&mut self, project_id: ProjectId) -> u8` (assigns at first sight), `pub fn colour(&self, index: u8) -> Color`, `pub fn set(&mut self, project_id: ProjectId, index: u8)`, `pub fn from_persisted(map: &BTreeMap<String, u8>) -> Self`, `pub fn to_persisted(&self) -> BTreeMap<String, u8>`.
- Layout: `KeyedWorkspaceLayout.project_colours: BTreeMap<String, u8>` (`#[serde(default)]`), `KeyedWorkspaceLayout.board_rail: bool` (`#[serde(default)]`). Keys are `ProjectId` rendered with its existing `Display`/`to_string`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::ProjectId;

    #[test]
    fn palette_is_the_spec_in_assignment_order() {
        assert_eq!(PROJECT_PALETTE[0], Color::from_u32(0x5aa3a0));
        assert_eq!(PROJECT_PALETTE[1], Color::from_u32(0x7a86c4));
        assert_eq!(PROJECT_PALETTE[7], Color::from_u32(0xa8806f));
    }

    #[test]
    fn first_sight_assigns_the_next_palette_slot_and_wraps() {
        let mut book = ProjectColourBook::default();
        let ids: Vec<_> = (0..9).map(|_| ProjectId::new()).collect();
        let indices: Vec<_> = ids.iter().map(|id| book.colour_index(*id)).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 0]);
        assert_eq!(book.colour_index(ids[3]), 3, "stable on re-ask");
    }

    #[test]
    fn set_overrides_and_persistence_round_trips() {
        let mut book = ProjectColourBook::default();
        let id = ProjectId::new();
        book.colour_index(id);
        book.set(id, 6);
        let restored = ProjectColourBook::from_persisted(&book.to_persisted());
        assert_eq!(restored.colour_index_if_known(id), Some(6));
    }

    #[test]
    fn out_of_range_persisted_indices_fail_closed_to_slot_zero() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(ProjectId::new().to_string(), 200u8);
        let book = ProjectColourBook::from_persisted(&map);
        assert!(book.to_persisted().values().all(|v| *v < 8));
    }
}
```

In `src/ui/workspace_layout.rs` tests, add:

```rust
#[test]
fn layout_without_project_colours_or_rail_still_loads() {
    let json = serde_json::to_value(KeyedWorkspaceLayout::<TaskId>::default()).expect("json");
    let mut stripped = json.as_object().cloned().expect("object");
    stripped.remove("project_colours");
    stripped.remove("board_rail");
    let layout: KeyedWorkspaceLayout<TaskId> =
        serde_json::from_value(serde_json::Value::Object(stripped)).expect("older file loads");
    assert!(layout.project_colours.is_empty());
    assert!(!layout.board_rail);
}
```

(If `KeyedWorkspaceLayout` has no `Default`, build it the way the neighbouring tests do.)

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::board::project_colour ui::workspace_layout::tests::layout_without_project_colours -- --test-threads=4`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/ui/board/project_colour.rs`:

```rust
//! One muted hue per project, assigned at first sight in creation order and
//! persisted in the workspace layout store. Hues are dim and cool so amber and
//! red stay the only saturated colours on screen (spec 5.3).

use std::collections::BTreeMap;

use crate::domain::id::ProjectId;
use crate::ui::tokens::Color;

pub const PROJECT_PALETTE: [Color; 8] = [
    Color::from_u32(0x5aa3a0), // teal
    Color::from_u32(0x7a86c4), // slate
    Color::from_u32(0xa78a5c), // sand
    Color::from_u32(0x8c6fa8), // mauve
    Color::from_u32(0x7a9a6a), // moss
    Color::from_u32(0x9a7a8a), // dusk
    Color::from_u32(0x6f8fa8), // steel
    Color::from_u32(0xa8806f), // clay
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectColourBook {
    assignments: BTreeMap<String, u8>,
    next: u8,
}

impl ProjectColourBook {
    pub fn from_persisted(map: &BTreeMap<String, u8>) -> Self {
        let assignments: BTreeMap<String, u8> = map
            .iter()
            .map(|(k, v)| (k.clone(), if (*v as usize) < PROJECT_PALETTE.len() { *v } else { 0 }))
            .collect();
        let next = (assignments.len() % PROJECT_PALETTE.len()) as u8;
        Self { assignments, next }
    }

    pub fn to_persisted(&self) -> BTreeMap<String, u8> {
        self.assignments.clone()
    }

    pub fn colour_index_if_known(&self, project_id: ProjectId) -> Option<u8> {
        self.assignments.get(&project_id.to_string()).copied()
    }

    pub fn colour_index(&mut self, project_id: ProjectId) -> u8 {
        let key = project_id.to_string();
        if let Some(index) = self.assignments.get(&key) {
            return *index;
        }
        let index = self.next;
        self.next = (self.next + 1) % PROJECT_PALETTE.len() as u8;
        self.assignments.insert(key, index);
        index
    }

    pub fn set(&mut self, project_id: ProjectId, index: u8) {
        let index = if (index as usize) < PROJECT_PALETTE.len() { index } else { 0 };
        self.assignments.insert(project_id.to_string(), index);
    }

    pub fn colour(&self, index: u8) -> Color {
        PROJECT_PALETTE[(index as usize) % PROJECT_PALETTE.len()]
    }
}
```

If `Color::from_u32` is not `const fn`, make the palette a `pub fn project_palette() -> [Color; 8]` and update the test. If `ProjectId` has no `Display`, use whatever the layout store already uses to serialise task ids (search `to_string()` near `selected_task`).

`src/ui/workspace_layout.rs`, in `KeyedWorkspaceLayout`:

```rust
    /// Project palette slot per project id, assigned at first sight (spec 5.3).
    #[serde(default)]
    pub project_colours: BTreeMap<String, u8>,
    /// Board column collapsed to the 36 px rail.
    #[serde(default)]
    pub board_rail: bool,
```

Add both to every constructor of the struct (`Default`, `sanitized()`, the v3→v4 migration) with empty/false values; `sanitized()` must clamp indices to `< 8`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ui::board::project_colour ui::workspace_layout -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/board/project_colour.rs src/ui/board/mod.rs src/ui/workspace_layout.rs
git commit -m "feat(board): persisted project colour book and rail flag"
```

---

### Task 7: Row layout decisions and the painter

**Files:**
- Create: `src/ui/board/layout.rs`, `src/ui/board/render.rs`
- Modify: `src/ui/board/mod.rs`
- Test: `src/ui/board/layout.rs` (pure); `src/ui/board/render.rs` (one headless gpui smoke test)

**Interfaces:**
- `layout.rs` produces: `pub const BOARD_ROW_HEIGHT: f32 = 46.0` (two lines: 6 + 17 + 1 + 15 + 7 px), `pub const BOARD_ROW_GAP: f32 = 3.0`, `pub const BOARD_COLUMN_WIDTH: f32 = 236.0`, `pub const BOARD_RAIL_WIDTH: f32 = 36.0`, `pub struct BoardRowLayout { pub show_segments: bool, pub show_count: bool }`, `pub fn row_layout(width_px: f32, progress: Option<BoardProgress>) -> BoardRowLayout`.
- `render.rs` produces:

```rust
pub struct BoardRowHandlers {
    pub on_left_select: Rc<dyn Fn(&HostTaskKey, bool /*shift*/, &mut Window, &mut App)>,
    pub on_right_click: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_key_down: Rc<dyn Fn(&HostTaskKey, &KeyDownEvent, &mut Window, &mut App)>,
}
pub struct BoardHeaderHandlers {
    pub on_new: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_menu: Rc<dyn Fn(&mut Window, &mut App)>,
    pub on_toggle_done: Rc<dyn Fn(&mut Window, &mut App)>,
}
pub fn render_board(
    model: &BoardModel,
    colours: &ProjectColourBook,
    tokens: ThemeTokens,
    width_px: f32,
    rail: bool,
    compact: bool,
    row_handlers: BoardRowHandlers,
    header_handlers: BoardHeaderHandlers,
) -> AnyElement
pub fn board_row_element(row: &BoardRow, colours: &ProjectColourBook, tokens: ThemeTokens, width_px: f32, handlers: &BoardRowHandlers) -> AnyElement
pub fn board_row_element_id(key: &HostTaskKey) -> ElementId
```

`board_row_element_id` must return exactly what `stable_host_task_element_id(&task_key)` in `native_shell.rs` returns today, so the accessibility tree and tests keep their ids. Move that function into `render.rs` and re-export it from the shell, or call it from the shell; either way one definition.

- [ ] **Step 1: Write the failing layout tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board::model::BoardProgress;

    #[test]
    fn wide_rows_show_segments_and_count_when_a_plan_exists() {
        let l = row_layout(236.0, Some(BoardProgress { completed: 1, total: 4 }));
        assert!(l.show_segments && l.show_count);
    }

    #[test]
    fn narrow_rows_drop_the_count_first_then_the_segments() {
        let p = Some(BoardProgress { completed: 1, total: 4 });
        let at_199 = row_layout(199.0, p);
        assert!(at_199.show_segments && !at_199.show_count);
        let at_150 = row_layout(150.0, p);
        assert!(!at_150.show_segments && !at_150.show_count);
    }

    #[test]
    fn no_plan_means_nothing_regardless_of_width() {
        let l = row_layout(400.0, None);
        assert!(!l.show_segments && !l.show_count);
    }

    #[test]
    fn row_heights_follow_the_spec() {
        assert_eq!(BOARD_ROW_HEIGHT, 46.0);
        assert_eq!(BOARD_ROW_HEIGHT_COMPACT, 42.0);
        assert_eq!(BOARD_DONE_ROW_HEIGHT, 28.0, "done rows are one line");
    }
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test --lib ui::board::layout -- --test-threads=4`
Expected: compile error.

- [ ] **Step 3: Implement `layout.rs`**

```rust
//! Width-dependent row decisions (spec 4.3): the count goes first, then the
//! segments. Pure so the rule is testable without a window.

use super::model::BoardProgress;

pub const BOARD_ROW_HEIGHT: f32 = 46.0;
/// Density "Compact": 2 px less padding per side.
pub const BOARD_ROW_HEIGHT_COMPACT: f32 = 42.0;
/// Done rows are one line: title and age only (spec 4.1).
pub const BOARD_DONE_ROW_HEIGHT: f32 = 28.0;
pub const BOARD_ROW_GAP: f32 = 3.0;
pub const BOARD_COLUMN_WIDTH: f32 = 236.0;
pub const BOARD_RAIL_WIDTH: f32 = 36.0;
pub const SEGMENT_WIDTH: f32 = 9.0;
pub const SEGMENT_HEIGHT: f32 = 4.0;
pub const SEGMENT_GAP: f32 = 2.0;
pub const COUNT_MIN_WIDTH: f32 = 200.0;
pub const SEGMENTS_MIN_WIDTH: f32 = 160.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardRowLayout {
    pub show_segments: bool,
    pub show_count: bool,
}

pub fn row_layout(width_px: f32, progress: Option<BoardProgress>) -> BoardRowLayout {
    let Some(_) = progress else {
        return BoardRowLayout { show_segments: false, show_count: false };
    };
    BoardRowLayout {
        show_segments: width_px >= SEGMENTS_MIN_WIDTH,
        show_count: width_px >= COUNT_MIN_WIDTH,
    }
}
```

- [ ] **Step 4: Implement `render.rs`**

Mirror the existing row code in `native_shell.rs:42286-42520` for gpui idioms (`div()`, `.id(...)`, `.tab_stop(true)`, `.h(px(..))`, `.bg(tokens.surfaces.raised.to_gpui())`, `.border_t/.border_b`, `.text_size(px(..))`, `.truncate()`, `crate::icons::app_icon(path, size, colour_u32)`, `.on_mouse_down(MouseButton::Left, ...)`, `.on_key_down(...)`, `.capture_any_mouse_down(...)`). Structure:

```rust
//! The board painter. Every colour comes from `ThemeTokens` or the project
//! palette; the amber and red states are the only saturated colours here.

use std::rc::Rc;

use gpui::{div, px, AnyElement, App, ElementId, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Styled, Window, FontWeight};

use crate::ui::board::age::format_age;
use crate::ui::board::layout::{row_layout, BOARD_ROW_GAP, BOARD_ROW_HEIGHT, SEGMENT_GAP, SEGMENT_HEIGHT, SEGMENT_WIDTH};
use crate::ui::board::model::{BoardGroup, BoardModel, BoardRow, BoardState};
use crate::ui::board::project_colour::ProjectColourBook;
use crate::ui::native_host_state::HostTaskKey;
use crate::ui::tokens::ThemeTokens;

pub struct BoardRowHandlers { /* as in Interfaces */ }
pub struct BoardHeaderHandlers { /* as in Interfaces */ }

pub fn board_row_element_id(key: &HostTaskKey) -> ElementId { /* moved from native_shell::stable_host_task_element_id */ }

fn state_dot(row: &BoardRow, tokens: ThemeTokens) -> AnyElement {
    let (colour, halo) = match row.state {
        BoardState::Question | BoardState::Permission => (tokens.status.attention, true),
        BoardState::Blocked => (tokens.status.destructive, true),
        BoardState::Working => (tokens.text.muted, false),
        BoardState::Idle | BoardState::Done => (tokens.borders.strong, false),
    };
    let dot = div().w(px(7.0)).h(px(7.0)).rounded_full().bg(colour.to_gpui());
    if halo {
        div().w(px(13.0)).h(px(13.0)).rounded_full().bg(colour.with_alpha(0.18).to_gpui())
            .flex().items_center().justify_center().child(dot).into_any_element()
    } else {
        div().w(px(13.0)).h(px(13.0)).flex().items_center().justify_center().child(dot).into_any_element()
    }
}

fn segments(row: &BoardRow, tokens: ThemeTokens, show_count: bool) -> Option<AnyElement> {
    let progress = row.progress?;
    let mut strip = div().flex().items_center().gap(px(SEGMENT_GAP));
    for index in 0..progress.total {
        let colour = if index < progress.completed {
            tokens.text.secondary
        } else if index == progress.completed {
            tokens.text.primary
        } else {
            tokens.borders.default
        };
        strip = strip.child(div().w(px(SEGMENT_WIDTH)).h(px(SEGMENT_HEIGHT)).rounded(px(1.5)).bg(colour.to_gpui()));
    }
    let mut wrap = div().flex().items_center().gap(px(4.0)).child(strip);
    if show_count {
        wrap = wrap.child(div().text_size(px(10.0)).text_color(tokens.text.muted.to_gpui()).child(format!("{}/{}", progress.completed, progress.total)));
    }
    Some(wrap.into_any_element())
}

pub fn board_row_element(row: &BoardRow, colours: &ProjectColourBook, tokens: ThemeTokens, width_px: f32, compact: bool, handlers: &BoardRowHandlers) -> AnyElement {
    let layout = row_layout(width_px, row.progress);
    let one_line = row.state == BoardState::Done;
    let height = if one_line { BOARD_DONE_ROW_HEIGHT } else if compact { BOARD_ROW_HEIGHT_COMPACT } else { BOARD_ROW_HEIGHT };
    // Tooltip: project, branch, provider on one line and the full title beneath.
    // Use the shell's existing tooltip helper if one exists (grep `fn tooltip` and `.tooltip(` in src/ui); otherwise gpui's `.tooltip(move |_, cx| ...)` with a text child.
    let tooltip_text = format!("{} · {} · {}\n{}", row.project_label, row.branch, row.provider.label(), row.title);
    let stripe = colours.colour(row.project_colour);
    let (bg, border) = if row.selected {
        (tokens.surfaces.selection, tokens.borders.strong)
    } else {
        (tokens.surfaces.raised, tokens.borders.subtle)
    };
    let border = match row.state {
        BoardState::Question | BoardState::Permission => tokens.status.attention.with_alpha(0.45),
        BoardState::Blocked => tokens.status.destructive.with_alpha(0.45),
        _ => border,
    };
    let title_colour = if row.state.needs_you() { tokens.text.primary } else { tokens.text.secondary };
    let second_line_left = match row.state {
        BoardState::Working => row.why.clone(),           // doing-now text, filled by the shell
        BoardState::Idle => format!("Last reply {}", format_age(row.state_age_ms)),
        _ => row.why.clone(),
    };
    let key = row.key.clone();
    let (k1, k2, k3) = (key.clone(), key.clone(), key.clone());
    let (h1, h2, h3) = (handlers.on_left_select.clone(), handlers.on_right_click.clone(), handlers.on_key_down.clone());
    div()
        .id(board_row_element_id(&row.key))
        .tab_stop(true)
        .w_full()
        .h(px(height))
        .mb(px(BOARD_ROW_GAP))
        .tooltip_text(tooltip_text) // see the comment above: the real call is the repo's tooltip idiom
        .relative()
        .bg(bg.to_gpui())
        .border_t(px(1.0)).border_b(px(1.0)).border_color(border.to_gpui())
        .cursor_pointer()
        .hover(|s| s.bg(tokens.surfaces.hover.to_gpui()))
        .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, window, app| { (h1)(&k1, event.modifiers.shift, window, app); })
        .on_mouse_down(MouseButton::Right, move |_event: &MouseDownEvent, window, app| { (h2)(&k2, window, app); })
        .on_key_down(move |event: &KeyDownEvent, window, app| { (h3)(&k3, event, window, app); })
        // stripe on the very left edge, full row height
        .child(div().absolute().left_0().top_0().bottom_0().w(px(3.0)).bg(stripe.to_gpui()))
        .child(
            div().flex().flex_col().pl(px(10.0)).pr(px(10.0)).pt(px(6.0)).gap(px(1.0))
                .child(
                    div().flex().items_center().gap(px(7.0))
                        .child(state_dot(row, tokens))
                        .child(div().flex_1().min_w(px(0.0)).truncate().text_size(px(12.5)).text_color(title_colour.to_gpui()).child(row.title.clone()))
                        .child(div().text_size(px(10.5)).text_color(tokens.text.muted.to_gpui()).child(format_age(row.state_age_ms))),
                )
                .children((!one_line).then(|| {
                    div().flex().items_center().gap(px(6.0)).pl(px(20.0))
                        .child(div().flex_1().min_w(px(0.0)).truncate().text_size(px(10.5)).text_color(tokens.text.muted.to_gpui()).child(second_line_left))
                        .children(layout.show_segments.then(|| segments(row, tokens, layout.show_count)).flatten())
                        .child(crate::icons::app_icon(row.provider.glyph_path(), 11.0, tokens.text.muted.to_u32()))
                })),
        )
        .into_any_element()
}
```

Then `render_board`: a column `div().w(px(width_px)).h_full().bg(tokens.surfaces.canvas.to_gpui()).flex().flex_col()` with the header row ("Board" 13 px semibold, `+ New` bordered button calling `on_new`, `⋯` calling `on_menu`), then for each group a label row (10.5 px, uppercase via `.to_uppercase()`, `tokens.text.secondary`, the count in `tokens.text.muted`, and for Done a `▸`/`▾` and `on_toggle_done`), then the group's rows unless collapsed. In rail mode (`rail == true`) draw only a 36 px column with one 13 px dot per group in the group's colour and the count beneath. Use `uniform_list` only if the shell already needs it for virtualisation; the spec caps the board at the tasks a person can supervise, and the fleet projection is already bounded by `MAX_TASK_LIST_ITEMS`, so a plain column with the shell's existing scroll container is acceptable here. State which you chose in the report.

`Color::with_alpha` may not exist; if not, add `pub fn with_alpha(self, alpha: f32) -> Color` to `src/ui/tokens.rs` following how `to_gpui()` builds an `Rgba`.

- [ ] **Step 5: Headless smoke test**

In `render.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board::model::{build_board_model, BoardProgress, BoardRow, BoardState};
    use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;
    use crate::domain::id::TaskId;
    use crate::ui::native_host_state::HostId;

    #[test]
    fn board_renders_every_state_without_panicking() {
        gpui::Application::headless().run(|cx| {
            crate::ui::init(cx);
            let tokens = crate::ui::tokens::dark(Default::default(), Default::default());
            let rows = [BoardState::Question, BoardState::Permission, BoardState::Blocked, BoardState::Working, BoardState::Idle, BoardState::Done]
                .into_iter().enumerate().map(|(i, state)| BoardRow {
                    key: HostTaskKey::new(HostId::LocalProfile("p".into()), TaskId::new()),
                    title: format!("row {i}"), state, why: state.why_label().into(), state_age_ms: 1_000 * i as i64,
                    progress: (i % 2 == 0).then(|| BoardProgress { completed: 1, total: 3 }),
                    provider: PrimaryProviderIcon::Claude, project_colour: i as u8, project_label: "p".into(), branch: "main".into(), last_activity_ms: 0, selected: i == 3, open: false,
                }).collect();
            let model = build_board_model(rows, true);
            let noop_row = BoardRowHandlers {
                on_left_select: Rc::new(|_, _, _, _| {}), on_right_click: Rc::new(|_, _, _| {}), on_key_down: Rc::new(|_, _, _, _| {}),
            };
            let noop_header = BoardHeaderHandlers { on_new: Rc::new(|_, _| {}), on_menu: Rc::new(|_, _| {}), on_toggle_done: Rc::new(|_, _| {}) };
            let _ = render_board(&model, &ProjectColourBook::default(), tokens, 236.0, false, false, noop_row, noop_header);
            let _ = render_board(&model, &ProjectColourBook::default(), tokens, 36.0, true, true, /* fresh handlers */ BoardRowHandlers { on_left_select: Rc::new(|_, _, _, _| {}), on_right_click: Rc::new(|_, _, _| {}), on_key_down: Rc::new(|_, _, _, _| {}) }, BoardHeaderHandlers { on_new: Rc::new(|_, _| {}), on_menu: Rc::new(|_, _| {}), on_toggle_done: Rc::new(|_, _| {}) });
            cx.quit();
        });
    }
}
```

Use the same headless harness the shell tests use (`rerun_headless_shell_test_in_child` + `HEADLESS_SHELL_TEST_LOCK`) if a bare `Application::headless()` cannot run twice in one process; the shell tests show the pattern.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib ui::board -- --test-threads=4`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/ui/board/layout.rs src/ui/board/render.rs src/ui/board/mod.rs src/ui/tokens.rs
git commit -m "feat(board): row layout rules and gpui painter"
```

---

### Task 8: Wire the board into the shell

**Files:**
- Modify: `src/ui/native_shell.rs`: `project_inbox_items` (30425), `row_models` and the `uniform_list` (41904-42520), `inbox_row_height` (34629), `T3_SIDEBAR_ROW_HEIGHT` (360, and the test at 47013), `for_fleet_task_list_with_composer` (accessibility, ~9806), `task_row_status_for_owner` (33755), `status_tone` (33828)
- Test: `src/ui/native_shell.rs` tests

**Interfaces:**
- Consumes everything from Tasks 3-7.
- Produces on `NativeShell`: `pub fn board_model(&mut self, now_ms: i64) -> BoardModel`, `fn board_rows(&mut self, now_ms: i64) -> Vec<BoardRow>`, fields `board_state_clock: StateClock<HostTaskKey>`, `board_done_expanded: bool`, `project_colours: ProjectColourBook` (loaded from and saved to `layout.project_colours`).

- [ ] **Step 1: Write the failing shell test**

Replace `task_inbox_groups_tasks_under_their_project_and_preserves_project_on_create` (find it with `grep -n 'fn task_inbox_groups_tasks_under_their_project'`) with:

```rust
#[test]
fn board_groups_tasks_by_state_and_keeps_project_as_a_stripe() {
    if rerun_headless_shell_test_in_child(
        "ui::native_shell::tests::board_groups_tasks_by_state_and_keeps_project_as_a_stripe",
    ) {
        return;
    }
    let _test_guard = HEADLESS_SHELL_TEST_LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock");
    gpui::Application::headless().run(move |cx| {
        crate::ui::init(cx);
        let (runtime, _shared) = TestRuntime::new(true, NativeHostActionResult::Queued);
        let (model, task_id) = open_task_without_agent_client_model();
        let project_id = model.task(task_id).expect("task").task.project_id;
        with_test_shell_in_app(cx, runtime, |shell| {
            shell.install_project_for_test("DevManager", project_id);
            shell.apply_client_model(Arc::new(model)).expect("apply model");
            let board = shell.board_model(1_000);
            let groups: Vec<_> = board.groups.iter().map(|g| g.group).collect();
            assert_eq!(groups, vec![BoardGroup::Idle, BoardGroup::Done], "an unstarted task is Idle; Done is always present");
            let row = &board.groups[0].rows[0];
            assert_eq!(row.key, shell.local_task_key(task_id));
            assert_eq!(row.project_colour, 0, "first project takes palette slot 0");
            assert_eq!(row.provider, PrimaryProviderIcon::Other, "no agent yet");
            assert!(shell.accessibility_tree().find_by_element_id(&board_row_element_id(&row.key).to_string()).is_some(), "row keeps its stable element id");
        });
        cx.quit();
    });
}
```

Adapt the accessibility lookup to the tree API the neighbouring tests use (search `accessibility_tree()` in tests for an existing lookup helper).

- [ ] **Step 2: Run to see it fail**

Run: `cargo test --lib ui::native_shell::tests::board_groups_tasks_by_state -- --test-threads=1`
Expected: compile error (`board_model` missing).

- [ ] **Step 3: Build the rows**

Add to `NativeShell`:

```rust
fn board_rows(&mut self, now_ms: i64) -> Vec<BoardRow> {
    let fleet = self.fleet_inbox_projection();
    let selected = self.selected_task_key.clone();
    let open: HashSet<HostTaskKey> = self.layout.task_workspace.as_ref()
        .map(|w| w.task_ids().into_iter().collect()).unwrap_or_default();
    let mut rows = Vec::new();
    for fleet_row in fleet.rail_rows() {
        if fleet_row.archived { continue; }
        let status = self.task_row_status_for_owner(&fleet_row.key);
        let state = crate::ui::board::board_state_of(status.unwrap_or(VisibleTaskStatus::Idle), fleet_row.done);
        let state_age_ms = self.board_state_clock.observe(fleet_row.key.clone(), state, now_ms);
        let activity = self.task_surfaces.conversation_page(fleet_row.key.clone())
            .map(|page| crate::ui::board::activity::board_activity(&page.facts))
            .unwrap_or_default();
        let why = match state {
            BoardState::Working => activity.doing_now.clone().unwrap_or_else(|| "Working".to_string()),
            other => other.why_label().to_string(),
        };
        let provider = match self.task_provider_kind_for_owner(&fleet_row.key) {
            Some(ProviderKind::ClaudeCode) => PrimaryProviderIcon::Claude,
            Some(ProviderKind::Codex) => PrimaryProviderIcon::Codex,
            Some(ProviderKind::Cursor) => PrimaryProviderIcon::Cursor,
            None => PrimaryProviderIcon::Other,
        };
        let project_colour = fleet_row.project_id.map(|id| self.project_colours.colour_index(id)).unwrap_or(0);
        rows.push(BoardRow {
            key: fleet_row.key.clone(),
            title: if fleet_row.key.host == self.local_host_id() { fleet_row.title.clone() } else { format!("{} · {}", fleet_row.host_label, fleet_row.title) },
            state, why, state_age_ms,
            progress: activity.progress,
            provider, project_colour,
            project_label: fleet_row.project_label.clone(),
            branch: self.host_slot(&fleet_row.key.host)
                .and_then(|slot| slot.inbox.row(fleet_row.key.task_id))
                .map(|r| r.display.worktree.clone())
                .filter(|b| !b.trim().is_empty())
                .unwrap_or_else(|| "main".to_string()),
            last_activity_ms: fleet_row.occurred_at_ms,
            selected: selected.as_ref() == Some(&fleet_row.key),
            open: open.contains(&fleet_row.key),
        });
    }
    rows
}

pub fn board_model(&mut self, now_ms: i64) -> BoardModel {
    let rows = self.board_rows(now_ms);
    crate::ui::board::build_board_model(rows, self.board_done_expanded)
}
```

`rail_rows()` is the existing iterator over `active` rows; if it also yields done rows, keep them (they belong in the Done group). Persist `project_colours` whenever `colour_index` assigns a new slot: after building rows, if `self.project_colours.to_persisted() != self.layout.project_colours`, copy it into the layout and call the existing layout-save path. The "Permission: Bash" refinement (naming the tool) is out of scope for this sub-project; the why is the state label.

- [ ] **Step 4: Paint it**

In the sidebar render path, replace `project_inbox_items()` plus the `uniform_list` with:

```rust
let now_ms = crate::domain::clock::now_ms(); // use the shell's existing wall-clock helper
let board = self.board_model(now_ms);
let width = if self.layout.board_rail { BOARD_RAIL_WIDTH } else { self.layout.inbox_width.max(INBOX_MIN) };
let shell_entity = cx.entity().downgrade();
let row_handlers = BoardRowHandlers {
    on_left_select: Rc::new({ let e = shell_entity.clone(); move |key, shift, window, app| {
        let _ = e.update(app, |shell, cx| {
            cx.stop_propagation();
            shell.focus_handle.focus(window);
            let mode = if shift { FleetSelectMode::Toggle } else { FleetSelectMode::Replace };
            let _ = shell.select_fleet_task_key(key.clone(), mode);
            shell.refresh_accessibility_tree();
            cx.notify();
        });
    }}),
    on_right_click: Rc::new({ let e = shell_entity.clone(); move |key, _window, app| {
        let _ = e.update(app, |shell, cx| {
            cx.stop_propagation();
            if key.host == shell.local_host_id() { /* keep today's selected_project_id behaviour */ }
            shell.begin_task_rename_key(key.clone());
            cx.notify();
        });
    }}),
    on_key_down: Rc::new({ let e = shell_entity.clone(); move |key, event, window, app| {
        // Enter/Space select; every other key forwards to the terminal exactly as
        // the old key_handler at native_shell.rs:42388 did. Move that body here verbatim.
    }}),
};
let header_handlers = BoardHeaderHandlers {
    on_new: Rc::new({ let e = shell_entity.clone(); move |_w, app| { let _ = e.update(app, |shell, cx| { shell.open_board_new_task_menu(); cx.notify(); }); }}),
    on_menu: Rc::new({ let e = shell_entity.clone(); move |_w, app| { let _ = e.update(app, |shell, cx| { shell.open_board_menu(); cx.notify(); }); }}),
    on_toggle_done: Rc::new({ let e = shell_entity.clone(); move |_w, app| { let _ = e.update(app, |shell, cx| { shell.board_done_expanded = !shell.board_done_expanded; cx.notify(); }); }}),
};
let board_element = crate::ui::board::render::render_board(&board, &self.project_colours, tokens, width, self.layout.board_rail, self.board_density_compact(), row_handlers, header_handlers);
```

`open_board_new_task_menu` shows a menu listing every local project (from `local_slot().config_sidebar.projects`) with the providers `inbox_agent_actions` reports available; choosing one calls the existing `start_task_with_agent_for_project(project_id, provider)`. `open_board_menu` shows: "Archived…" (toggles `show_archived_tasks`, which renders the archived list in place of the board exactly as today's archived view does), "Collapse to rail" / "Expand board" (flips `layout.board_rail` and saves), "Density" (Comfortable/Compact stored in the existing density preference; `fn board_density_compact(&self) -> bool` reads it). Build both menus with whatever menu primitive the shell already uses for the project ⋯ menu (search `ProjectActionMenuMode`).

Delete `ProjectInboxItem::Project`, `project_inbox_items`, the `row_models` map, `T3_SIDEBAR_ROW_HEIGHT` and its test; keep `ProjectInboxItem::Task`/`ArchivedHeader` only if the archived view still needs them.

- [ ] **Step 5: Accessibility tree**

In `for_fleet_task_list_with_composer`, replace the project-header nodes with one static node per group (`format!("{} {}", group.label(), rows.len())`, role `StaticText`) and a button node for the Done header ("Done, N tasks, collapsed/expanded"); task rows keep `push_task_row` unchanged so their ids and names do not move. Drop the per-project "+Claude"/"+Codex" nodes and add one "New task" button node for the board header.

- [ ] **Step 6: Run the shell tests**

Run: `cargo test --lib ui::native_shell -- --test-threads=4`
Expected: the new test passes; any test that asserted project headers, `T3_SIDEBAR_ROW_HEIGHT == 78.0`, or the "+Claude" project buttons is updated to the board's shape (group labels, `BOARD_ROW_HEIGHT`, the header "New task" button). Do not delete a test file; edit the assertions.

- [ ] **Step 7: Gates**

Run: `cargo check --locked --lib --bins --tests -j 2` then `cargo fmt --all -- --check`, then the full `cargo test --lib -- --test-threads=4`, reading the `EXIT=` line from a file rather than a piped tail.

- [ ] **Step 8: Commit**

```bash
git add src/ui/native_shell.rs
git commit -m "feat(board): state-grouped board replaces the project rail"
```

---

### Task 9: Visual acceptance against the reference images

**Files:**
- Create: `.superpowers/sdd/<this plan's workspace>/board-capture.png` (git-ignored) and the report
- Read: `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/03-board-rows-boxed-A.png`, `04-project-stripe-1.png`, `05-provider-mark-1.png`

- [ ] **Step 1: Build for the user**

Run the dev build script the user launches with, without launching: `pwsh ./dev-watch.ps1 -Once -NoLaunch` if the flag exists, otherwise `cargo build --locked` into your own target dir and say where the binaries are. Never launch the app yourself.

- [ ] **Step 2: Ask the user to launch and capture**

Hand back with: the commit hash, the build location, and the request that the user launch it with several tasks in different states and trigger the existing preview capture (the path that produced `task-cockpit-ux-review-latest.png`). The capture is their action.

- [ ] **Step 3: Compare**

When the capture arrives, place it beside each reference and write, in the report, every visible difference with a verdict "allowed by spec" or "defect", and for each defect the task above that owns the fix. Allowed differences: real titles instead of mock text, font hinting, widths following the live window. Not allowed: a third line, side margins on rows, a stripe that is not on the column edge, a percent or smooth bar, a coloured provider mark, any colour on Working or Idle rows.

- [ ] **Step 4: Fix defects, re-capture, commit**

Each defect is a scoped fix commit on the owning task's files with its test updated; re-run Step 2 once at the end.

---

## Self-review

**Spec coverage (section 4 and 5):** groups and order, Task 3; row anatomy, Task 7; stripe, Tasks 6 and 7; state dot colours, Task 7; title/age, Tasks 4 and 7; second line why/doing-now/last reply, Tasks 5, 7, 8; segments and count with width rule, Tasks 5 and 7; provider mark, Tasks 2 and 7; board header with `+ New` and ⋯ menu, Task 8; rail, Tasks 6, 7, 8; density, Tasks 7 and 8 (`BOARD_ROW_HEIGHT_COMPACT`, `board_density_compact()`); hover tooltip with project, branch, provider and full title, Task 7 (tooltip) with `project_label`/`branch` on `BoardRow` from Tasks 3 and 8; colour tokens, Task 1; palette, Task 6; Done collapsed by default with one-line rows when expanded, Tasks 3 and 7 (`BOARD_DONE_ROW_HEIGHT`). Archived behind the menu, Task 8.

**Placeholder scan:** none of "TBD/TODO/similar to"; the `on_key_down` body in Task 8 says "move verbatim from 42388", which is a concrete instruction to a named location.

**Type consistency:** `BoardRow` fields used in Tasks 7 and 8 match Task 3 (including `project_label: String`, `branch: String`); `BoardProgress { completed, total }` throughout; `ProjectColourBook::colour_index(ProjectId) -> u8` and `colour(u8) -> Color` throughout; `board_row_element_id(&HostTaskKey) -> ElementId` in Tasks 7 and 8.
