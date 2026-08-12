# Phase 8 Task 8.1 Partial Report

Status: partial foundation only. This worktree contains the dependency-safe,
portable contract boundary; it does not claim the real WebView2/GPUI proof.

## Implemented

- Added `src/browser/surface.rs` and exported its contract types from
  `src/browser/mod.rs`.
- Added `tests/browser_surface.rs` with nine behavior tests and no source-text
  assertions.
- Reused the existing typed `TaskId`, `BrowserContextId`, and `ResourceId`
  identities. The new descriptor also carries host PID, process creation time,
  executable path, runtime generation, nonce, host-owned bounds/focus epochs,
  physical bounds, and DPI.
- Represented the child HWND as a validated opaque `hwnd:<u64>` wire token;
  no raw pointer, `usize`, or platform handle is serialized. The parking HWND
  remains host-registry state and is not part of the client descriptor.
- Added host-owned attach, park, detach, reattach, task-switch, client-crash,
  bounds, focus, input, and context-close transitions. Descriptor identity,
  nonce, process, generation, epochs, geometry, lifecycle, and permissions are
  validated before mutation.
- Bounds/focus receipt order advances host epochs. Client numeric sequences are
  accepted only as diagnostic fields and never select a global epoch.
- Added explicit host ownership/permissions and teardown evidence requiring a
  parked surface, closed controller/environment/context, and zero remaining
  helpers before a terminal state is recorded. Client drop/crash only detaches
  and retains the context.
- Added a bounded deterministic fixture contract for visible token, trusted
  click, text input, resize/DPI token, and retained state. No fixture server or
  visible browser was launched because this slice has no host bridge to isolate.

## TDD and verification evidence

RED was captured first with:

```text
cargo test --test browser_surface -- --nocapture
```

The test failed because the new surface exports and implementation were absent.
After the smallest implementation and formatting pass:

```text
cargo test --test browser_surface -- --nocapture  # 9 passed
cargo check --lib                                 # passed
cargo fmt --all -- --check                        # passed
git diff --check                                   # passed
```

All Rust commands used the isolated worktree target directory
`target-phase-8-1-browser-surface`. The final scoped process check found no
Cargo, rustc, or browser-surface test harness for this worktree.

## Exact remaining Task 8.1 proof gates

The following are intentionally unimplemented and must remain open before
Task 8.1 or Phase 8 can be called complete:

1. A real Windows host must create WebView2 beneath a host-owned parking window
   and prove controller/child HWND ownership and process identity (PID,
   creation time, executable) at runtime.
2. The host must duplicate/validate the child HWND descriptor across the real
   host/client boundary; the owning UI/COM thread must perform SetParent,
   style, bounds, visibility, focus, and teardown operations.
3. GPUI must attach the child surface to a dedicated native container and prove
   visible attach, park, detach, reattach, task switching, pointer consumption,
   and client-crash recovery without creating a second browser context.
4. The real deterministic page must prove visible-token discovery, trusted
   click, text input, physical resize, 100/125/150/200% DPI tokens, retained
   page state, minimize/restore, stale coordinates, and focus transitions.
5. Host crash/recovery, WebView2 renderer/helper failure, and full host/context
   teardown must be exercised with real PIDs, creation times, window hierarchy,
   screenshots, and process trees.
6. Every close path must prove zero WebView2/browser helper members and zero
   owned listeners/ports. No zero-helper result is claimed by this report.
7. The proof script, isolated fixture server/page, ADR 0001, and real Windows
   evidence artifacts from the Phase 8 plan are still outstanding.

No legacy browser host, GPUI shell, app/process/provider/config/prompt/Connect
code, persistence, installed app, or production browser data was touched.
