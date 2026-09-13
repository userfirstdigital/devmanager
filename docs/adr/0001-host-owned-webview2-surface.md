# ADR 0001: Host-owned WebView2 surface

## Status

Accepted as the ownership model. Evidence for a visible Windows/WebView2
run is **not** present in the current portable artifacts. Provider
authentication remains opt-in.

## Context

The Task browser must be a host-owned resource: the durable host creates
and tears down the WebView2 environment, parks the child HWND, and issues
opaque descriptors to clients. GPUI and remote viewers attach those
descriptors; they do not own the controller. Phase 8 asked for a first
vertical-slice proof of attach/park/reattach across process and DPI
changes before treating the rest of the browser subsystem as complete.

Current HEAD already exports a portable `BrowserSurfaceHost` plus
`BrowserSurfaceFixture` tokens. Those types record the facts a future
host bridge must prove. They do not create a WebView2 controller, call
Win32 `SetParent`, or attach a GPUI window.

## Decision

1. The host is the sole WebView2/operation owner. Parking HWND stays
   host-registry state. Clients receive an opaque `hwnd:<nonzero>` token,
   never a raw pointer.
2. Authority is the exact Task, agent session, and runtime generation
   captured at registration. Attach, park, detach, reattach, bounds,
   focus, and task-follow require a current descriptor.
3. Task-switch and other shell gestures consume pointer input. Page input
   requires a later armed focus epoch inside the attached surface.
4. Bounds and focus epochs are host-monotonic. Stale descriptors fail
   closed. DPI/bounds updates are physical pixels plus a portable scale.
5. Context close requires a teardown proof: surface parked, controller
   closed, environment closed, context closed, and zero helper processes.
   Residue is a fault, not a warning.
6. Authenticated Claude/Codex/Cursor launches stay HOLD unless an
   operator passes an explicit provider allowlist, an isolated
   `-ConfigBase`, and `DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E=1`.
   Ordinary gates never launch stock providers or production profiles.

## Invariants

- **Ownership.** Host process identity (PID + creation time + executable)
  and child/parking HWND parentage are captured on the host UI/COM thread
  before any client attach.
- **Parking.** Hide/detach returns the child to the host parking window
  without destroying context. Close is a later explicit proof.
- **Attach / reattach.** Attach is from Parked; reattach is from
  Detached. A live client crash detaches without teardown.
- **Focus.** Incoming surfaces attach unfocused. Shell gestures do not
  become page clicks, keys, drags, file choices, or permission answers.
- **DPI.** Portable tests walk 100/125/150/200 percent scales on the
  model. A real OS DPI matrix is still required of the future bridge.
- **Teardown.** `helper_processes_remaining != 0` is rejected. Terminal
  surfaces leave no live task mapping.

## Evidence status (current artifacts)

| Claim | Status |
| --- | --- |
| Portable `BrowserSurfaceHost` register/attach/park/detach/reattach/teardown | Covered by `tests/browser_surface.rs` |
| Fixture tokens retained across click/text/resize | Covered by `BrowserSurfaceFixture` |
| Deterministic local fixture site + loopback server | `tests/fixtures/browser-e2e/*` and `src/bin/browser-fixture-server.rs` |
| Windows/WebView2 capability present in tree | `tests/browser_webview2_e2e.rs` is an explicit `DEVMANAGER_BROWSER_WEBVIEW2_E2E=1` capability gate; no live run is claimed |
| Visible WebView2 attach/park/reattach on GPUI | Not proven |
| Authenticated provider-controlled browsing | HOLD / opt-in; not launched |

Do not treat `cargo test --test browser_surface` or the PowerShell proof
scripts as a passing visible WebView2 run.

## Consequences

- Later host/GPUI work must keep the portable contract and add a labeled
  Windows proof with screenshots, HWND parentage, PIDs, and zero-helper
  residue before Phase 8 can exit.
- Provider E2E remains two arms: ordinary fixture validation and an
  explicit authenticated conformance arm that cannot be satisfied by a
  fake provider.
- Production DevManager and provider profiles stay untouched.
