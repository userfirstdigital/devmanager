# Browser E2E matrix

This matrix separates portable fixture proof, Windows/WebView2 surface
proof, remote/projection fixture labeling, and explicit authenticated
provider conformance. Portable tests and scripts do **not** prove a real
WebView2. Authenticated provider launches stay HOLD unless every opt-in
gate is present, and even then the current scripts do not start stock
CLIs.

## Arms

| Arm | What it proves | What is NOT proven | Exact command |
| --- | --- | --- | --- |
| Portable fixture proof | Public fixture validation, `BROWSER_FIXTURE_CASES` action/recovery coverage, local-only pages/assets, fixture-server source/loopback contract, authenticated HOLD | Real page automation, stock provider control, WebView2 | `cargo test --locked --test browser_provider_e2e -- --test-threads=1` |
| Portable surface contract | Opaque nonzero window/process identity, host register/attach/park/detach/reattach, task/client authority, stale epoch rejection, DPI/bounds matrix, shell gesture consumption, retained fixture tokens, teardown zero-helper proof | Visible HWND attach, OS DPI, GPUI rehost, helper-process disappearance | `cargo test --locked --test browser_surface -- --test-threads=1` |
| Windows/WebView2 surface proof | Capability-gated source evidence plus the opt-in Windows harness entry point | A passing visible WebView2 run until explicitly enabled on a Windows UI thread | `pwsh scripts/native-next/Invoke-BrowserSurfaceProof.ps1 -Stage All -AllDpi -ClientCrash -HostRecovery -OutputDir <rooted-evidence-dir>` |
| Remote/projection fixture | Label-only inclusion via `-IncludeProjectionFixture` | Direct or hosted Connect projection (Phase 9) | `pwsh scripts/native-next/Invoke-BrowserProviderE2E.ps1 -Fixture -IncludeProjectionFixture -OutputDir <rooted-evidence-dir>` |
| Recovery fixture | Recovery pages and `BROWSER_FIXTURE_CASES` recovery IDs | Live renderer crash, provider crash, host full quit | `pwsh scripts/native-next/Invoke-BrowserProviderE2E.ps1 -Fixture -IncludeRecovery -OutputDir <rooted-evidence-dir>` |
| Explicit authenticated provider conformance | Admission gates only: allowlist `claude,codex,cursor`, `DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E=1`, isolated `-ConfigBase` | Any launched Claude/Codex/Cursor session | `pwsh scripts/native-next/Invoke-BrowserProviderE2E.ps1 -Authenticated -Provider claude,codex -ConfigBase <isolated-root> -OutputDir <rooted-evidence-dir>` |

Default fixture commands never launch installed Claude, Codex, Cursor, or
DevManager. They are safe to run while the installed app is active.

The surface script's `Green`/`All` stages run the portable
`browser_surface` tests using a process-unique `C:\Temp\devmanager-*` Cargo
target and with any inherited `DEVMANAGER_PROFILE` removed. The scenario
switches select the DPI, client-crash/reattach, and host-shutdown/zero-residue
tests; they do not claim to exercise a real HWND or WebView2 controller.

The separate visible Windows capability arm is:

```powershell
$env:DEVMANAGER_BROWSER_WEBVIEW2_E2E = '1'
cargo test --locked --test browser_webview2_e2e -- --test-threads=1 --nocapture
```

It requires an installed Evergreen WebView2 runtime and an appropriate UI
thread/windowing environment. It remains opt-in and is not launched by the
ordinary fixture scripts.

## Safety and cleanup

- `-OutputDir` and authenticated `-ConfigBase` must be explicit rooted paths and must not resolve under `%APPDATA%\com.userfirst.devmanager`.
- Focused Rust proof runs use an exact process-unique `C:\Temp\devmanager-*`
  target and remove it after Cargo exits; no shared repository target is used.
- Production `config.json` / `remote.json` are not opened. Production provider and browser profiles are not used.
- The fixture server binds `127.0.0.1` only, serves `tests/fixtures/browser-e2e` or an isolated temp root, and is killed in `finally`.
- Evidence JSON stores token *names* and hold reasons, never prompt bodies, page bodies, or bearer tokens.
- Residue criteria for the portable arms: no fixture-server PID left, no stock provider process started, no installed DevManager launch, no production profile write.

## Residue and pass criteria

| Arm | Pass | Fail / HOLD |
| --- | --- | --- |
| Portable fixture / surface | Focused cargo tests pass; evidence JSON has `launchedStockProvider=false` and `visibleWebView2Proven=false` | Missing fixtures, external URLs, secrets, or a provider launch |
| Windows/WebView2 | Script writes evidence that labels the gap | Claiming a visible attach/park/reattach success without a later host/GPUI proof |
| Authenticated | Script exits after recording HOLD | Missing allowlist, missing env opt-in, production ConfigBase, or any provider spawn |

## What is NOT proven

- Host-owned WebView2 child HWND attach, park, hide, reattach, or focus across a live GPUI client.
- Real 100/125/150/200% OS DPI with a visible surface.
- Authenticated Claude, Codex, or Cursor control of the fixture page.
- Remote/projection clients observing the same live browser.
- Zero WebView2 helper processes after a real context close.
