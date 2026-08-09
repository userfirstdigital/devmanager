# Phase 5 Task 5.1 native visual-capture report

Status: PARTIAL: the focused hardening batch is GREEN on the current visible Windows desktop. The required DPI/occlusion/minimize VM matrix is represented by an ignored manual contract and was not run on unsupported or unconfigured environments.

## Final Task 5.1 finisher (2026-08-09)

The remaining typed-error caveat is closed. `PreviewCaptureError::CleanupFailed`
now retains both `primary: Box<PreviewCaptureError>` and
`secondary: Box<PreviewCaptureError>`; cleanup settlement passes the secondary
error through without reducing it to `String`. The structural and behavioral
assertions cover both typed values and their categorized display output.

Fresh external evidence at 09:16 local recorded the exact
`visible_capture_uses_isolated_process_and_decodes_exact_sentinel` test passing
1/1 on `C:\Temp\devmanager-phase51-wgc-final`, using a real isolated GPUI
process and the exact RGBA sentinel `[0x91, 0x2b, 0xd4, 0xff]`. The same test
also passed in the final focused suite below.

### Final gates

All commands used the same isolated target and `native-next-dev` profile:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-wgc-final'
$env:DEVMANAGER_PROFILE='native-next-dev'
cargo test --test ui_preview_capture -- --nocapture
cargo test --test ui_projection -- --nocapture
cargo check --locked --bin devmanager-next
cargo fmt --all -- --check
git diff --check
```

Results:

- `ui_preview_capture`: 12 passed, 0 failed, 1 ignored; the only ignored test
  is `manual_vm_visual_capture_matrix_contract`.
- `ui_projection`: 11 passed, 0 failed, 0 ignored.
- `cargo check --locked --bin devmanager-next`: passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
- Exact preview/Cargo/rustc residue query: `OWNED_RESIDUE_COUNT=0`,
  `OTHER_MATCHING_PROCESS_COUNT=0`.

The 100%/125%/150%/200% DPI, occlusion, minimized, and closed-preview VM
matrix remains pending and is not represented as green by these gates.

## Final narrow WGC correction (2026-08-09)

This correction stayed within `src/ui/preview.rs`, `src/ui/preview_capture.rs`,
and `tests/ui_preview_capture.rs`. It did not launch GPUI/WGC capture, the
preview CLI, or the installed app, and did not touch AppData or session state.

### RED evidence

With the new focused tests added first, compile-only verification using the
required isolated target failed on the missing bounded settle/reaper seam,
typed unavailable-kind mapping, and typed high-level cleanup fields. The
asymmetric RGB sentinel was also mutation-checked: temporarily replacing the
BGRA-to-RGBA conversion with identity failed with the decoded pixel reversed
from `[0x91, 0x2b, 0xd4, 0xe7]` to `[0xd4, 0x2b, 0x91, 0xe7]`.

### GREEN evidence

Using `C:\Temp\devmanager-phase51-wgc-final-20260809`:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-wgc-final-20260809'
cargo test --test ui_preview_capture -- --nocapture --skip visible_capture_preserves_focus_decodes_actual_png_and_leaves_no_capture_thread
cargo check --locked --bin devmanager-next
cargo fmt --check -- src/ui/preview.rs src/ui/preview_capture.rs tests/ui_preview_capture.rs
git diff --check
```

Results: 11 non-visual focused tests passed, 1 manual VM test remained
ignored, the locked binary check passed, focused rustfmt passed, and the diff
check passed. The skipped test is the existing visible capture test; no
capture/app was launched in this correction.

The high-level preview error now retains an explicit unavailable kind for all
six previously collapsed capture causes and carries the typed primary through
cleanup failures. Capture `stop`/`wait` runs behind the shared absolute
deadline; a late worker remains owned by an explicit cleanup reaper until it
finishes. The behavioral test blocks that worker past the deadline, observes
the composed primary/cleanup error, releases it, and verifies eventual
settlement. The public output path continues to require a validated
`PreviewRequest`.

## Salvage hardening batch (2026-08-09)

Scope stayed within the preserved Task 5.1 WGC slice: error-category
preservation, one absolute capture deadline, visible stop/join failures,
BGRA channel-order proof, and policy-bound low-level helpers. No external
VM, RDP, DPI, occlusion, minimize, merge, push, install, network, agent, or
production/AppData/session surface was used.

### RED evidence

After the red tests were added and before the hardening implementation, the
focused command below exited 1 during test compilation with the expected
missing `CaptureDeadline`, cleanup composition, actionable `PreviewError`
mapping, and policy-bound writer contracts:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-hardening-20260809'
cargo test --test ui_preview_capture -- --nocapture
```

The RGB assertion was separately mutation-checked by temporarily replacing
BGRA-to-RGBA conversion with identity. The focused test exited 1 and showed
all four expected RGB tuples reversed; the correct conversion was restored.

### GREEN evidence

Using the exact isolated target directory
`C:\Temp\devmanager-phase51-hardening-20260809`:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-hardening-20260809'
cargo test --test ui_preview_capture -- --nocapture
cargo test --test ui_projection -- --nocapture
cargo check --locked --bin devmanager-next
```

Results: `ui_preview_capture` 10 passed, 0 failed, 1 ignored; `ui_projection`
11 passed, 0 failed; and the locked `devmanager-next` check passed. Focused
rustfmt completed for the four owned Rust/test files. The existing seven
unrelated library warnings remain unchanged. The exact hardening target had no
remaining Cargo, rustc, harness, or `devmanager-next` process; unrelated Cargo/
rustc verification processes in other worktrees were left untouched.

### Commit and remaining matrix

Commit: this report is included in the coherent partial Task 5.1 hardening
commit; the final short SHA is recorded in the session handoff.

The sole remaining visual acceptance blocker is the external matrix: run the
ordinary visible capture at 100%, 125%, 150%, and 200% DPI, then repeat with a
compositor window occluding the preview and with the preview minimized and
closed. No VM was available/configured for that matrix in this batch.

## Delivered path

- Pinned `windows-capture = "=1.5.0"`; the locked manifest metadata reports MIT,
  edition 2024, and compatibility with the workspace's exact `windows =
  0.61.3`. The focused notice entry records the reviewed source, manifest,
  license, and direct HWND APIs.
- The Windows-only path creates a hidden GPUI popup, obtains its exact HWND from
  `raw_window_handle::HasWindowHandle`, adds `WS_EX_NOACTIVATE`, shows it with
  `SW_SHOWNOACTIVATE`/`SWP_NOACTIVATE`, verifies the foreground HWND before and
  after capture, validates ownership/visibility/minimized state/dimensions, and
  captures one BGRA window item with cursor, border, and secondary windows
  excluded.
- Capture request admission starts one absolute five-second deadline covering
  GPUI setup, WGC startup, first valid frame, capture stop/join, and PNG
  settlement. Capture control is stopped and joined, the GPUI application quits
  on success and failure, and atomic sibling temp files are removed or renamed
  into the approved PNG path.
- Headless GPUI remains structural initialization only. Metadata reports
  `HeadlessProjectionOnly` off Windows and
  `VisibleWindowsNativeCapture` on Windows; unavailable/closed/hidden/minimized/
  unsupported failures remain explicit unavailable errors, while PNG/output,
  foreground, application, WGC, and cleanup failures remain actionable.
- The fixture makes cursor/border exclusion explicit and the preview root paints
  a deterministic GPUI surface so the first valid frame cannot silently be an
  unrendered black swap-chain frame.

## RED evidence

Initial strict-TDD command, before the capture module/API existed:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-capture-red-20260809'
cargo test --test ui_preview_capture -- --nocapture
```

Result: exit code 1 during test compilation with the expected missing
`devmanager::ui::preview_capture` module, capture contract, and visible-window
unavailable API.

After the first implementation, the newly added visual-content assertion also
went red against the real desktop: the CLI/test PNG was a valid 624x352 image
but its interior was an unrendered black frame. The targeted test failed with:

```text
the first valid frame must contain the fixture's visible text
```

The correction was limited to the preview surface/window kind: a deterministic
GPUI background/text surface and a borderless popup window. No arbitrary sleep
was added.

## GREEN evidence

Using the unique external target directory
`C:\Temp\devmanager-phase51-capture-green-20260809`:

```powershell
cargo test --test ui_preview_capture -- --nocapture
cargo test --test ui_projection -- --nocapture
cargo check --locked --bin devmanager-next
cargo fmt --check -- src/ui/mod.rs src/ui/preview.rs src/ui/preview_capture.rs tests/ui_projection.rs tests/ui_preview_capture.rs
```

Final results:

- `ui_preview_capture`: 6 passed, 0 failed, 1 ignored (the manual VM contract).
- `ui_projection`: 11 passed, 0 failed.
- `devmanager-next` locked check: passed.
- Formatter and `git diff --check`: passed.
- Capture tests covered BGRA/PNG decode, dimensions and alpha, atomic temp
  cleanup, deadline behavior, invalid/foreign HWND rejection, focus stability,
  fixture cursor/border semantics, output isolation, and capture-thread residue.

Dependency/license checks:

```text
windows         0.61.3  MIT OR Apache-2.0  edition 2021
windows-capture 1.5.0   MIT               edition 2024
windows-capture v1.5.0 -> devmanager v0.4.2
```

## Real CLI visual evidence

Command:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-capture-green-20260809'
cargo run --locked --bin devmanager-next -- --ui-preview tests/fixtures/ui/theme-gallery.json --output .devmanager-next/evidence/phase-05/screenshots/theme-gallery-phase51-final-20260809.png
```

Result: exit code 0. The inspected PNG is:

- `[theme-gallery-phase51-final-20260809.png](../../../.devmanager-next/evidence/phase-05/screenshots/theme-gallery-phase51-final-20260809.png)`
- PNG signature `89-50-4E-47-0D-0A-1A-0A`, 624x352 physical pixels, 13,696 bytes.
- SHA-256: `4F19BE50533917719CC2AB081C0C733DEDAB185C11A0E3605C71F6CCC5391132`.
- Decoded inspection found 217,592 fully opaque pixels, nonzero alpha, and
  1,687 light interior pixels containing the fixture label. No sibling capture
  temp file remained.
- Visual inspection showed the actual `DevManager native preview: Theme Gallery`
  label on the GPUI surface. Cursor and capture-border exclusion are enforced
  by the fixture and Windows capture settings; the platform's rounded corner
  alpha remains part of the actual window item.

## Manual/VM-only contract and blocker

`manual_vm_visual_capture_matrix_contract` is intentionally ignored with the
explicit contract to run at 100%, 125%, 150%, and 200% DPI, then repeat with a
compositor window occluding the preview and with the preview minimized/closed.
Occlusion must still produce a PNG; minimized/closed must produce
`VisibleWindowsCaptureUnavailable`, no output, and no capture-thread residue.
The current desktop run proves only the ordinary visible baseline. No VM was
available/configured for the four-scale and state-transition matrix, so those
claims remain pending rather than being faked in CI.

## Isolation and handoff

The CLI and focused tests used only the checked-in fixture, temporary policy
roots, the approved evidence directory, and the external Cargo target. No
production profile, AppData/config/session state, installed DevManager, merge,
push, install, or reviewer was used. Final process-idle verification must show
no owned Cargo/rustc/test-harness/`devmanager-next` child; any pre-existing
installed DevManager process is reported separately and left untouched.
