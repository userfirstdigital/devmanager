# Task 5.3a reusable native component vocabulary report

Date: 2026-08-09

Status: PARTIAL 5.3a. The first reusable native component vocabulary slice is
implemented and behaviorally verified. The real gallery capture/inspection
gate remains intentionally open for the concurrent capture lane.

## Scope delivered

- Added one shared interaction-state and accessibility boundary under
  `src/ui/components/interaction.rs`.
- Added DevManager-owned Button and IconButton models backed only by typed
  requests from the existing client `ActionCatalog`; activation returns typed
  events and presentational components store or execute no callbacks or host
  effects. They retain semantic role/name/state metadata, Enter/Space
  activation, token focus rings, disabled reasons, loading rejection, and
  focus-epoch/pointer-press fencing.
- Added noninteractive Badge and StatusLight semantic presentations using the
  existing status tokens and explicit text/description signals.
- Added bounded TextField state with Unicode scalar and UTF-8 byte limits,
  focus/keyboard behavior, read-only/disabled metadata, error metadata, and a
  paste path gated by the focused current epoch with scalar/byte preflight
  before candidate allocation.
- Added bounded EmptyState and ErrorBoundary models with only explicitly
  supplied typed recovery actions. ErrorBoundary accepts a typed redacted safe
  projection; no raw debug/provider payload field or rendering path exists.
- Added the typed `component-gallery.json` fixture and structural preview
  validation for both themes, both densities, 100/125/150/200 scales, every
  implemented interaction state, long text, Unicode, missing/error/loading/
  empty/overflow samples.

## TDD evidence

The required RED pass used the exact target directory
`C:\Temp\devmanager-phase53a-20260809`.

```text
cargo test --locked --test ui_accessibility -- --nocapture
error: could not compile `devmanager` ... due to 9 previous errors
could not find `components` in `ui`
```

```text
cargo test --locked --test ui_projection components_ -- --nocapture
error: could not compile `devmanager` ... due to 3 previous errors
could not find `components` in `ui`
no method named `component_gallery`
```

Focused GREEN after implementation and the one complete-diff correction batch:

```text
cargo test --locked --test ui_accessibility -- --nocapture
9 passed; 0 failed

cargo test --locked --test ui_projection components_ -- --nocapture
3 passed; 0 failed; 13 filtered out
```

The tests cover transition rejection/fail-closed state, keyboard and pointer
activation, stale focus-epoch click-through rejection, accessibility metadata,
disabled/loading reasons, semantic token focus rings/status signals, Unicode
and byte bounds, read-only/disabled text behavior, safe paste, typed recovery,
and structural fixture consumption/rejection.

## Verification

- `cargo check --locked --lib`: exit 0.
- `cargo fmt --all -- --check`: exit 0.
- `git diff --check`: exit 0.
- Owned native UI source color scan: no direct color literal/API matches
  outside `tokens.rs` (which was not changed).
- Exact process query: no Cargo, rustc, focused test harness, or DevManager
  process remained whose command line referenced this worktree or target.
- No full test suite was run.
- No installed DevManager, production AppData/session state, network,
  merge, push, install, publish, or screenshot claim was made.

## Commit

Commit subject: `feat(ui): add reusable native component vocabulary (partial 5.3a)`

## Files owned by this slice

- `src/ui/components/{mod,interaction,button,icon_button,badge,status_light,text_field,empty_state,error_boundary}.rs`
- the existing typed request extension in `src/client/action.rs`
- `src/ui/mod.rs`
- `src/ui/preview.rs`
- `tests/ui_accessibility.rs`
- focused component cases in `tests/ui_projection.rs`
- `tests/fixtures/ui/component-gallery.json`
- this report

## Review correction

The focused review fix added RED coverage for the four findings above and then
GREEN coverage for 13 accessibility tests and 3 component projection tests.
The RED pass failed at the expected missing `ActionRequest`, focus-epoch paste,
and safe-projection APIs; the final focused passes were 13/13 and 3/3. No
5.3b widget, real capture, runtime/provider/process/config/AppData/session
surface, installed app, network, merge, or push was touched.

## Explicit 5.3b blockers

The following remain outside this slice and must not be implied as complete:

- menu/tooltip behavior (only the IconButton tooltip contract exists here)
- dialog focus trap
- toast lifetime
- splitter bounds
- virtual-list row reuse
- real component-gallery capture and visual inspection

The concurrent Task 5.1 real-capture gate and Task 5.2 final-review gate also
remain independent acceptance gates. The current pinned GPUI preview path is
structural/headless-only and does not provide screenshot acceptance evidence.
