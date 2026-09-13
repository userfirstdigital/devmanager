# Native remaining-gates implementation plan

> **For agentic workers:** Use superpowers:subagent-driven-development with the Cursor implementation worker. Continue the approved remote architecture; do not request another design approval.

**Goal:** Close concrete native acceptance failures while preserving immutable host ownership.

**Architecture:** Extend the existing indexed inbox and per-host cockpit; keep one HostFleet, one semantic projection path and the existing recursive workspace. Cache is presentation-only and never grants action authority.

**Tech Stack:** Rust, GPUI, existing HostClient/HostFleet, SQLite journal.

**Spec:** `docs/superpowers/specs/2026-08-27-seamless-remote-work-design.md`

## Gate status — 2026-08-28

All four source tasks below are implemented and independently reviewed. The
source-only portions of the checklists are complete; steps containing visible
app acceptance remain open. This is not a claim of complete remote-work parity.

- Focused final correction gate: 18 named regressions passed, followed by three
  successful repeats of unstage-before-first-commit. Coverage includes the exact
  Stage/current-index disappearance race, strict negative cases, isolated host
  fixtures, forged panel results, remote lifecycle fencing and receipt ownership.
- Full serial library suite: **3,639 passed, 0 failed, 1 ignored** in 880.46s.
  The ignored real Codex `--help` capability probe passed separately using an
  isolated npm cache. No provider messages were sent by that probe.
- After the full suite, one test-only success arm was changed to panic so the
  reparse rejection regression cannot silently pass a successful result. Its
  focused rerun passed. Production source is unchanged from the passing suite.
- Final `cargo check --locked --lib --bins --tests --examples` and
  `cargo build --locked --bins --examples` passed. Existing warnings remain.
- Integration: `ui_native_shell` passed 21/21; the real foreground-host bootstrap
  case passed; three linked-worktree authority cases passed. `ui_projection`
  initially passed 48/52, then all four failed fixture cases plus the fifth
  changed GPUI isolation case passed after test-only corrections. The other 47
  cases are unchanged; the expensive full projection target was not repeated.
  Its three GPUI lifetimes now run in fresh child harnesses, and four metadata
  fixtures retain their own temporary output authority. The host fixture now
  uses the supported CreateTaskV2/opaque-project path, not rejected legacy V1.
  The other 18 `host_lifecycle` integration cases were not run in this gate.
- Both final executable transport smokes passed: two real loopback hosts with
  colliding task IDs, owner routing, outage isolation and trusted reconnect;
  and ephemeral-CA HTTPS/WSS/Noise cross-origin pairing/resume with reused-ticket
  and wrong-Origin rejection. No OS trust store changes or provider calls.
  Paired-client revoke/resume is not asserted by the cross-origin fixture because
  RemoteSetupRequest does not expose that operation.
- All owned Cursor wrappers, compiler/test harnesses and smoke host processes
  exited. Installed app PID/start and production config/remote hashes are
  unchanged; the user's root AGENTS.md is preserved and excluded from the commit.
- Windows did not permit symlink creation in the new conditional reparse test.
  The unsupported-operation rejection path ran; that case is not proof of the
  actual replacement-symlink path on this machine.
- Connect API relay hardening is locally committed as `7d7dc39`; build and all
  761 tests passed. It is not deployed or integrated into native WAN account flow.
- Live UI acceptance is paused: the user stopped Computer Use with Escape.
  No replacement UI automation or app/provider input is authorized in this turn.
- Physical second-PC/phone LAN, browser-trusted deployment and public WAN remain
  unverified. WAN also requires source work for account credential resolution,
  Portal/Noise identity association, opaque DMCT admission and host rendezvous;
  these are not merely missing credentials.
- Existing explicit remote holds remain for raw terminal input, image staging,
  Browser, Services, Commit and Add Project. Terminal display is not input support.

## Global constraints

- A task always runs on its owning computer; viewing it, changing focus, or changing network does not migrate or restart its provider.
- Projects remain multi-folder workspaces on the owning computer.
- A disconnected owner never falls back to the local PC.
- No deployment, firewall change or public listener is part of source-level implementation acceptance.
- Preserve the user's running checkout and installed application. Source changes belong in `C:\t\devmanager-remote-completion-20260827`, based on `bdf8355`.
- Write regressions before fixes; run grouped checks at the end as the user requested. One compiler owner, isolated target, no duplicate Cargo trees.

### Task 1: Restore the real indexed Done projection

**Files:**
- Modify: `src/client/model.rs` (TaskProjectionIndex)
- Modify: `src/ui/task_cockpit/inbox.rs` (Inbox projection)
- Modify: `src/ui/native_fleet.rs` (fleet adapter)
- Modify only if needed: `src/ui/native_shell.rs`, `src/ui/native_host_state.rs` (Done presentation and tests)

**Interfaces:** Consume the existing `ClientModel`, `TaskProjectionIndex`, `Inbox`, and `fleet_rows_from_inbox`. Produce a separate bounded settled index/page and `Inbox::settled_rows()`; do not redefine active or archived counts. Carry existing `HostTaskKey` and task occurrence time into the compact Done rows.

- [ ] Add regression using a real model and Inbox: Open -> Settled moves the row out of active into Done; a fresh snapshot also contains Done; Settled -> Open restores it; Archive and Delete remain distinct. Repeat with the same raw task ID under two owners and assert the fleet rows are independent.
- [ ] Implement an incrementally maintained `settled_order: BTreeSet<TaskOrderKey>` alongside active/archived order in initialization, update, and removal. Add `settled_count` and `top_settled_task_ids(limit)`; retrieve a bounded first page without a full map scan during paint.
- [ ] Project that page into a distinct Inbox settled collection in full and incremental refresh and read-only preview. Preserve filtering, count bounds, errors and archived semantics. Feed fleet Done from this collection, not from `active_rows()`.
- [ ] Keep Done rows compact, visible at the inbox bottom, and selectable without reopening. Use the owner-qualified project filter and real occurrence time rather than a synthetic age. Ensure the section has a real measured/minimum height when active rows consume all available space.
- [ ] Run the grouped index/inbox/fleet/native regressions and compiler gate after the source wave. Inspect the actual native UI to verify Done appears; open it without restore, then send a new message and verify explicit reopen/send uses the same provider conversation.

### Task 2: Owner-safe remote Files, Changes, Review and terminal display

**Files:**
- Modify: `src/ui/native_shell.rs` (render, active-surface refresh, panel callbacks, terminal admission and accessibility)
- Modify only if necessary: `src/ui/native_host_state.rs`, `src/ui/task_cockpit/{dock,shell,cockpit_projection}.rs`, `src/ui/task_workspace/surfaces.rs`
- Modify: `src/ui/task_cockpit/files_panel.rs` for typed directory navigation already supported by FilesList
- Reuse unchanged: `src/ui/task_cockpit/{changes_panel,review_panel,panel}.rs`, `src/terminal/view.rs`, host typed cockpit query handlers

**Interfaces:** Consume `HostTaskKey`, `HostUiState`, `dispatch_action_recorded_for_owner`, `TaskCockpitQuery`, `TaskCockpitResult`, `TaskSurfaceRegistry::admit_terminal`, and each host slot's `TaskCockpitShell`. Render `ChangesPanelProjection::from_host`, `FilesPanelProjection::from_host`, `ReviewPanelProjection::from_model` using the exact owner's model/projection. Render remote terminal replicas with `terminal_pane_from_replica`/`render_terminal_surface_with_tokens` and no input actions.

- [ ] Read the bounded implementation map at `C:\Temp\devmanager-remote-tools-map.md`. Add regressions through actual native owner dispatch/outcome/render entrypoints for two hosts with the same task UUID, then implement the following changes.
- [ ] Add owner-qualified active-surface refresh: Changes requests WorkspaceStatus and GitRepositories; Files requests FilesList with path None and limit 64; Terminal requests Terminal; Review uses the owning model and canonical task query. No local query fallback; no automatic queries to hidden surfaces.
- [ ] Resolve selected remote Files/Changes/Review from its slot. Every panel click captures HostTaskKey and validates task revision against that slot before owner dispatch. Scope repository selection by HostTaskKey, including callbacks; never use a stale local selected task or local filesystem catalog.
- [ ] Complete host-served folder navigation in the reused Files panel: directory rows issue FilesList for their exact relative path, file rows issue FilesRead, secret rows stay disabled. Preserve current directory on refresh and provide parent/root navigation using validated host-relative paths (never filesystem access on the viewer). The current local panel disables all directories with an unavailable message despite a working host FilesList API; do not carry that dead end into the remote panel.
- [ ] Admit remote Terminal results only after exact query task, owner, generation and foreground request fences pass. Update the exact task surface and owner dock, then render its replica display-only. No key/focus input listener, TerminalDockAdapter, local PTY input or global terminal state. Local terminal behavior remains unchanged.
- [ ] Enable only these supported remote surfaces in visual and accessibility controls. Keep remote raw input, Browser, Services, image staging, Commit and Add Project explicitly unavailable until their own owner-safe implementation. Do not remove a hold without replacing every reachable action path it protects.
- [ ] Cover focus-switch-before-query-result, stale generation and duplicate raw task IDs. Test remote Files list/read action and Changes targeted repository refresh route only to that host; Review reads only its model; Terminal becomes visible with no input dispatch; local terminal still works. Reuse the existing fixtures named in the map, not source-string assertions.
- [ ] At source freeze, run the grouped native/cockpit/surface tests and isolated all-target compiler check. Then use the two-host native fixture for real visible remote panel acceptance. Keep physical LAN/phone and WAN status separate.

### Task 3: Owner-routed remote task lifecycle

**Files:** `src/ui/native_shell.rs`; small companion changes in `src/ui/native_host_state.rs` and `src/ui/native_fleet.rs` only if required.

**Interfaces:** Reuse `HostTaskKey`, `HostProjectKey`, `dispatch_action_recorded_for_owner`, `TaskCreateV2`, and the existing exact-command Delete confirmation state machine. No new task bus, local task remapping or provider process start on the viewing computer.

- [ ] Capture `HostTaskKey` in Delete and Rename drafts, and `HostProjectKey` in New Task drafts. A focus/project change after opening a dialog cannot retarget its command. Derive titles/catalogs/lifecycle/capabilities from the captured owner's slot, not global local selection.
- [ ] New Task asks only for a project. Populate owner-qualified configured projects, retain multi-folder workspace identity, dispatch deferred-provider `TaskCreateV2` to the captured owner and select the newly created owner key only after the existing canonical result path confirms it. No remote Add Project, no implicit local fallback, and no provider start before the first message.
- [ ] Route Rename through the same owner and clear automatic-title state only for that full task key. Enable remote row/context/accessibility rename only after its dialog and outcome path are owner-safe.
- [ ] Route Delete's Archive -> canonical Archived projection -> Delete through the captured owner and exact command IDs. Preserve duplicate-confirm protection, admission-failure cleanup, cancellation tombstones, terminal Close semantics and receipt-before-projection/projection-before-receipt behavior. Do not evict unresolved retired command ownership. Remote outcomes and model updates must advance the same flow; local state must remain untouched.
- [ ] Owner-qualify all deletion retry/error/discard/retirement paths, including late receipts after dialog replacement, disconnect, host removal and same raw task ID on another owner. Delete stays an explicit confirmation; merely opening it never submits.
- [ ] Remove only lifecycle-related remote holds in visual and accessibility controls once those paths are implemented. Browser, Services, Commit, image staging, raw terminal input and Add Project remain outside this task.
- [ ] Add regressions through actual production dispatch and epoch-fenced outcome handlers: remote draft create and rename; two-host same IDs; focus switch; delete both receipt/projection orders; duplicate confirmation; cancel before archive receipt; replacement confirmation; stale generation; failed/uncertain admission. Keep existing local lifecycle tests.
- [ ] Root runs grouped lifecycle/native tests and live fixture acceptance after the source freeze. No worker build/test or commit; no real task deletion through Computer Use without the required action-time confirmation.

### Task 4: Current-worktree Git status with external sibling worktrees

**Files:** `src/git/command.rs`, `src/git/test_git_service.rs`, `src/host/cockpit.rs`; necessary explicit read-admission plumbing only.

**Evidence:** The actual local Changes panel never populates; host stderr records `linked worktree descriptor target is outside the approved Git graph`. `push_worktree_graph` resolves every sibling backlink even for bounded current-repository status. Registered sibling worktrees may legitimately live under `C:\t`.

- [ ] Read the bounded diagnosis `C:\Temp\devmanager-git-worktree-gate-map.md`. Add production-path regressions before changing admission.
- [ ] Separate descriptor file validation/snapshot from unrelated backlink target resolution for proven current-worktree read command shapes. Keep the current linked-worktree's backlink and common directory fully validated against explicitly approved roots. A main checkout has no current linked metadata directory.
- [ ] Do not authorize unrelated targets, broaden configured roots, trust `C:\t` wholesale, or weaken alternate, common-directory, reparse, descriptor size/type/UTF-8, graph mutation, deadline or aggregate limits. Sibling descriptors remain bounded snapshotted input, not filesystem authority.
- [ ] In particular, keep `commondir` equality validation for every sibling; healthy values resolve to the admitted common store. Only unrelated `gitdir` backlink target resolution is unnecessary for current-only reads. Upstream [Git status entrypoint](https://github.com/git/git/blob/v2.49.0/builtin/commit.c) and [status collection](https://github.com/git/git/blob/v2.49.0/wt-status.c) inform this narrow command boundary; installed Git is 2.49.0.windows.1.
- [ ] Preserve strict validation for descriptor-consuming operations. If an operation can consume sibling backlinks (worktree commands or descriptor-aware branch/switch forms), it must not reuse a current-only admission without a strict preflight; existing unsupported operations remain unsupported. Use an explicit policy/command boundary, not a global bypass.
- [ ] Test current status through `serve_task_cockpit` with a genuine external sibling; status must succeed and show changes without touching the sibling. Test current linked metadata still needs its approved roots. Retain hostile descriptor/reparse/alternate/common-dir and post-admission mutation rejection tests, and add a strict-policy negative test if needed.
- [ ] Group Git/host/native verification with the final source gate. Repeat the actual dev Changes panel after verified integration; confirm installed config/remote hashes and app identity remain unchanged.

### Acceptance evidence already collected on bdf8355

- Fresh task `01a04704-d928-7a91-a5c4-cae4d8bc694d`, project devmanager: first and second prompts retained in full and exact replies displayed in chat and terminal; Working returned to Idle; switching away/back preserved both turns.
- Read-only SQLite query confirms provider `codex`, conversation ID `01a04705-8c16-7b32-b88b-0519f85da2ee`, runtime generation 1, lifecycle open.
- Header Done persisted task lifecycle settled, but no Done row appeared. The active index deliberately excludes Settled; the fleet adapter incorrectly tries to obtain Done from active rows. This is a live acceptance failure, not a hypothetical review finding.
- The existing production app PID 73848 and dev app/host PIDs 130100/87528 remain running. A mistakenly launched diagnostic client PID 77820 was stopped and joined; no provider or installed app was stopped.

Remote read-only tools, remote lifecycle parity, physical LAN/phone, and public WAN remain separate gates in the approved remote plan. This bounded task does not claim to close them.
