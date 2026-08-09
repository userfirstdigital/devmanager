# Task 5.1 review-round-1 correction report

Status: BLOCKED on the concrete native GPUI PNG requirement.

The correction closes the two independent review findings that can be fixed
truthfully in the pinned dependency set. It does not claim a screenshot: GPUI
0.2.2 has no official isolated pixel readback or PNG encoder, so the preview
continues to fail closed with `HeadlessRenderingUnsupported`.

## RED evidence before the correction

Command:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-round1-20260809'
cargo test --test ui_projection -- --nocapture
```

Result: 13 tests ran; 10 passed and 3 failed:

- `preview_renders_a_concrete_png_from_the_native_gpui_root`
- `preview_load_revalidates_direct_callers_against_the_isolation_policy`
- `task_cockpit_actions_are_registered_and_dispatch_through_gpui`

The failures were the expected review defects: the render method returned
`HeadlessRenderingUnsupported`, a public request loaded an outside fixture,
and the catalog IDs were not GPUI action registrations.

## Correction and GREEN evidence

- `PreviewRequest` path fields are private, expose read-only accessors, and
  `PreviewApplication::load` revalidates the request against its
  `PreviewPathPolicy` before reading a fixture.
- The six Task Cockpit catalog IDs are real GPUI `Action` types with exact
  names, `KeyBinding` registrations, dynamic construction, and dispatch
  coverage. `PreviewDismiss` remains registered and bound on the root element.
- The unsupported-render error now states the exact pinned-API limitation.

Command:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-round1-20260809'
cargo test --test ui_projection -- --nocapture
```

Result: 13 tests discovered; 12 passed, 0 failed, 1 ignored. The ignored
acceptance test is
`preview_renders_a_concrete_png_from_the_native_gpui_root`, with the explicit
reason `GPUI 0.2.2 has no official isolated pixel readback or PNG encoder`.

## Real CLI evidence

Command:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-phase51-round1-20260809'
cargo run --locked --bin devmanager-next -- --ui-preview tests/fixtures/ui/theme-gallery.json --output .devmanager-next/evidence/phase-05/screenshots/theme-gallery-round1-20260809.png
```

Result: exit code 2 with:

```text
GPUI 0.2.2 exposes no isolated offscreen pixel readback or PNG encoder; Windows rendering ends in a private swap chain
```

The requested output path did not exist after the run (`OUTPUT_EXISTS=False`).
No PNG was fabricated, and there is no screenshot path to inspect.

## Exact API blocker

The pinned registry sources provide the following evidence:

- `gpui-0.2.2/src/app.rs:146` provides `Application::headless`; the public
  root/window path begins at `:943` (`App::open_window`).
- `gpui-0.2.2/src/window.rs:1914` builds a frame and
  `:2008` forwards it to `platform_window.draw`.
- The official test window at
  `gpui-0.2.2/src/platform/test/window.rs:269` implements `draw` as a no-op;
  `TestAppContext::draw` at `gpui-0.2.2/src/app/test_context.rs:814` returns
  layout/prepaint state, not pixels or PNG bytes.
- The Windows renderer stores the swap chain and render target privately at
  `gpui-0.2.2/src/platform/windows/directx_renderer.rs:62-63`; its renderer
  `draw` is `pub(crate)` at `:286` and ends in `present` at `:204-205`.
  There is no public frame readback or PNG encoding API in the GPUI 0.2.2
  source tree.
- The official component initializer is present at
  `gpui-component-0.5.1/src/lib.rs:97`; it does not add capture support.

Using a fixed PNG, the `image` crate, a test-window placeholder, or a renderer
that bypasses the GPUI root would contradict the task requirement. Task 5.2
remains blocked until an approved GPUI capture/readback API is available (or
the pinned dependency is changed by an explicitly authorized follow-up).

## Isolation and remaining risk

All fixture/output tests use temporary policy roots or the checked-in fixture
root. The CLI uses the workspace policy, never production AppData, profile
state, `session.json`, the installed DevManager, or a visible window. The
focused test harness and CLI exited; no owned Cargo, rustc, test-harness, or
`devmanager-next` child remains.

Remaining risk is limited to the upstream capture boundary documented above;
the ignored PNG acceptance test must turn green before Task 5.1 can be
reported complete.

Correction commit subject: `fix(ui): render isolated native preview`
