# Task 5.2 token integration correction report

Date: 2026-08-09

Base: `bae05ab` (`fix(ui): render isolated native preview`)

Cherry-pick: `549fd24` was cherry-picked and resolved as `496eee8`. The
resolution retains the Task 5.1 preview module, adds the single library-owned
`ui::tokens` module, and keeps the checked-in preview fixture compatible with
Task 5.1's strict `deny_unknown_fields` loader.

## TDD evidence

The original commit's focused test run was RED because the test compiled a
private `src/ui` module and `preview.rs` could not resolve the library's
`assets`, `client`, and `terminal` modules: 3 unresolved-import errors.

After moving token imports to `devmanager::ui::tokens` and adding the reviewed
contract tests, the required pre-correction RED was captured before the
production correction: 5 compile errors for the intentionally missing
interaction-state and status-surface contrast APIs.

Focused GREEN:

```text
cargo test --test ui_tokens -- --nocapture
12 passed; 0 failed
```

Relevant projection GREEN:

```text
cargo test --test ui_projection -- --nocapture
12 passed; 0 failed; 1 ignored
```

The ignored test is the concrete native PNG acceptance test, with the reason
that GPUI 0.2.2 has no official isolated pixel readback or PNG encoder.

## Corrections

- Exposed semantic colors now include action primary/destructive states and
  truthful foreground/background status surfaces.
- Normal text, including disabled text, is checked at 4.5:1. The 3:1
  threshold remains limited to large text and UI indicators.
- Interaction contrast pairs explicitly declare default, hover, focus,
  selected, and disabled for both action families; all six status lights have
  explicit surface and indicator pair declarations.
- Legacy `theme::*` constants retain their prior values, including the
  indigo primary and the less-common editor/selection colors, until cutover.
- The native UI source scan rejects actual numeric/hex color literals while
  allowing token-to-`rgb(...)` conversion calls.
- The token test matrix binds long labels, Unicode, disabled content, themes,
  densities, scales, interaction states, and all six status lights across
  2 x 2 x 4 cases.

The checked-in `theme-gallery.json` remains the strict Task 5.1 preview
fixture. Its Task 5.2 matrix is kept as deterministic scoped test data because
the current preview parser rejects extension fields; weakening or changing the
preview renderer was outside this task's allowed files.

## Verification

All Rust commands used the isolated target directory:
`C:\Temp\devmanager-task52-red-20260809`.

- `cargo check --locked --bin devmanager-next`: exit 0.
- `cargo fmt --all -- --check`: exit 0.
- `git diff --check`: exit 0.
- Exact native UI scan: 2 non-token Rust source files, 0 direct numeric/hex
  color hits.
- Preview CLI: exit 2 with the documented GPUI headless readback error;
  `theme-gallery-task52-20260809.png` was not created.

## Remaining blocker

Task 5.2's visual matrix is not complete. The pinned GPUI 0.2.2 stack still
cannot emit a real isolated native PNG, so no screenshot is claimed or
fabricated. The PNG acceptance test must become green after an approved
capture/readback capability or explicitly authorized dependency change.

No production AppData/session file, installed DevManager, legacy app/chrome,
preview renderer, Cargo manifest/lockfile, merge, push, install, or unrelated
source path was changed.
