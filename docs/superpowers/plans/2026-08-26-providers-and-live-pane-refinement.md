# Providers and Live Pane Refinement Implementation Plan

> **For agentic workers:** Use the existing cursor-worker execution boundary; one source writer at a time. No recursive delegation.

**Goal:** Port T3 provider management to native Rust for Codex, Claude and Cursor, expose honest Grok/OpenCode stubs, and make every full chat pane live, bottom-anchored and correctly sized.

**Architecture:** Reuse the existing typed host/provider registry and profile-scoped persistence. Give each visible task a retained semantic timeline using GPUI's measured list/scroll machinery; focus controls input ownership, not transcript rendering or freshness. Keep manual compact presentation and pinned geometry independent from focus.

**Tech Stack:** Rust, GPUI, existing provider adapters and typed cockpit IPC; read-only T3 and Zed source references.

**Spec:** User-approved in-chat design and clarification: Grok and OpenCode are stubs only.

## Global Constraints

- Work in `C:\Code\userfirst\devmanager`, branch `VisualDevManager`, to preserve the user's hot-reload workflow.
- Preserve the pre-existing unstaged `AGENTS.md`; never stage or edit it.
- Only one implementation writer; no recursive delegation, worker commits, pushes, or broad refactors.
- Preserve providerSessionId exact-resume, all task/runtime/generation fences, local history, and multi-folder project authority.
- Stub providers must be visibly unavailable and cannot be enabled or launched. Never fabricate health, authentication, versions, models or replies.
- User requests lean verification at the end. Write behavioral regression tests alongside each slice, but consolidate execution into final focused/live/full gates.
- Cargo final verification target: `C:\Temp\devmanager-native-multi-task-VisualDevManager`; no overlapping Cargo commands and no production profile for tests.

## Task 1: Live full-pane rendering and scroll ownership

**Files:** `src/ui/native_shell.rs`, `src/ui/task_workspace/surfaces.rs`, `src/ui/task_workspace/view.rs`, `src/ui/task_cockpit/shell.rs`, `src/ui/task_cockpit/timeline.rs`.

**Reference:** Zed `crates/agent_ui/src/conversation_view.rs` uses `ListState` with bottom alignment; inspect the locally installed GPUI version's API rather than assuming Zed's version matches.

- [ ] Add regressions for all visible tasks receiving non-starving queries, retained independent task timelines, and focus affecting only composer ownership.
- [ ] Replace the nonfocused twelve-line snippet path with the same semantic row renderer as the focused chat; retain explicit compact cards as the deliberate condensed mode.
- [ ] Give each task independent real scroll/list state. Initial opening follows the bottom; new messages follow only if already following. Scrolling up must stay detached during streaming and focus changes.
- [ ] Replace fixed-line-count height assumptions with GPUI measured variable-height list or real scroll handles, including wrapped text, actual viewport, footer and pane header geometry.
- [ ] Keep query admission bounded and fair across every open task; never permanently starve older panes behind the two most recently focused panes. Compact cards still refresh their summaries.

## Task 2: Gap-free recursive geometry and action affordances

**Files:** `src/ui/task_workspace/allocation.rs`, `src/ui/task_workspace/layout.rs`, `src/ui/native_shell.rs`; reuse `src/ui/components` icons/buttons.

- [ ] Add a vertical two-compact-child regression: a 700px parent with a 4px divider allocates 348px to each auto child, not two minimum-height children with a gap.
- [ ] Add geometry conservation tests: child extents plus dividers equal their parent on both axes, mixed nested/pinned/compact cases included.
- [ ] Preserve pinned requested dimensions when other auto children can absorb changes. If none can, use the least recently focused pane as the resizing fallback so there is no unowned viewport area.
- [ ] Do not silently turn a full chat into a snippet merely on focus changes; retain user-requested compact mode and existing least-recent-focus sizing policy.
- [ ] Show Delete beside Archive for every selected task, routed through the existing confirmation flow.
- [ ] Add consistent existing-system icons to Done, Restore, Archive, Delete, Commit, Add action and pane controls; retain accessible labels/tooltips.
- [ ] Remove the redundant visible Open button; keep Files reachable through the dock and its keyboard path.

## Task 3: Real provider management, with two honest stubs

**Files:** add focused `src/ui/provider_settings.rs` and a profile-scoped provider-settings owner; extend the existing `src/host/agent_connection.rs`, typed cockpit query/result contract, provider registry/launch configuration, and `src/ui/native_shell.rs` only as required.

**References:** T3 `ProviderSettingsPanel.tsx`, `ProviderInstanceCard.tsx`, `ProviderSettingsForm.tsx`, `ProviderModelsSection.tsx`, `AddProviderInstanceDialog*`, `providerStatus.ts`, driver/settings contracts and host registry.

- [ ] Inventory every T3 provider-screen control and map it to a real DevManager implementation or the two explicitly approved unavailable stubs.
- [ ] Port the compact provider cards, actual health/version/auth/account detail, expand/collapse, enabled policy, manual refresh, last-checked time and persisted health interval (300 seconds default, 0 manual-only).
- [ ] Port supported-provider instance settings, reset/remove/add flows, provider-specific configuration and environment fields, custom models and model visibility/order/favorites; feed the same policy into discovery and composer choices.
- [ ] Keep updates and authentication user-initiated with honest progress/errors; no automatic CLI upgrade, sign-in, or credential rewriting during acceptance.
- [ ] Persist policy atomically under the selected DevManager profile, migrate defaults without touching unrelated project config, and enforce disabled providers at launch rather than only hiding them.
- [ ] Add Grok and OpenCode branded cards explicitly labelled not yet supported, with disabled activation and no runtime adapter.
- [ ] Add serialization, validation, scheduler, projection and launch-policy tests for the real controls.

## Task 4: Final verification and local commit

- [ ] Review the complete diff and reconcile every user request; no fabricated or cosmetic-only controls.
- [ ] In the live dev app, exercise Settings, supported provider controls, stub refusal, background simultaneous chats, focus/composer ownership, scroll-to-latest/scroll-up retention, nested 50/50 layout, resize/pin/compact, Delete confirmation and dock Files access.
- [ ] Run the final sequential suite and compile gate once the coherent changes are in place:

```powershell
$env:CARGO_TARGET_DIR='C:\Temp\devmanager-native-multi-task-VisualDevManager'
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --locked --lib -- --test-threads=1
cargo test --locked --test host_admission --test provider_registry --test provider_journal --test provider_quota --test provider_cursor -- --test-threads=1
cargo check --locked --lib --bins --tests
cargo fmt --check
git diff --check
```

- [ ] Join exact test/compiler processes, recheck installed PID/start/config/remote hashes, and report any independently changing live state without overwriting it.
- [ ] Commit only reviewed source, tests and this plan. Do not stage `AGENTS.md`; do not push.

## Integration checkpoint (2026-08-27)

Source is implemented for independent full-pane timelines and scroll ownership,
fair background queries, conserved recursive geometry, pane body focus, action
icons, Delete confirmation ownership, archive reopening, and removal of Open.
These are not a substitute for the remaining live acceptance sweep.

Provider settings now have host-owned profile persistence, compact native editors,
manual/background health refresh, instance configuration, environment custody,
model policy, and add/reset/remove flows. Grok and OpenCode are explicitly disabled
catalog placeholders: no discovery, health probe, activation, or launch adapter.
The startup launch-proof JSON panic with a real child environment was corrected.

Do not describe this checkpoint as complete T3 parity. Remaining integration gaps:

- Custom instance selection and changing provider/model on an already-created
  draft are not fully wired into task creation; draft detection still depends on
  the placeholder title. Cursor conversation execution is not implemented by this
  settings change. The wizard Configuration step is narrower than the full editor.
- Older persisted sessions can still fail exact restoration after CLI identity or
  launch-context changes; inactive resources and missing root evidence remain
  fail-closed. No provider IDs, process roots, or replacement conversations were
  fabricated, and no persisted production rows were rewritten.
- Explicit new-task creation does not yet automatically add a pane to an existing
  multi-pane workspace.
- Live Settings exposed a native stack overflow in expanded provider editors.
  Render builders are now separated, only the selected page is constructed, and
  the Windows UI binary reserves an 8 MiB stack (4 KiB initially committed).
  Settings/Providers/expanded Codex fields now work in the real dev window.
- Live acceptance verified both unavailable stub cards, actual Codex version and
  health, editor typing, draft retention across Close/reopen, explicit Cancel,
  and locally retained history at the latest messages after a watcher restart.
  Simultaneous pane/resize/scroll-up acceptance remains incomplete.
- A no-tools reply smoke test was accepted, but remained pending with no provider
  process after that exact historical session failed launch-identity restoration.
  An accepted send and the optimistic Working label are not a successful reply.
  No persisted identity was rewritten and no fresh-conversation fallback was used.

### Verification ledger

- Full sequential library run: 3,064 passed, 12 failed, 1 ignored. It was not
  rerun in full after correction. The affected regression rerun passed 198/198;
  the unrelated Git hard-link check passed its isolated rerun and remains an
  intermittent full-suite result, not evidence of an all-green full run.
- The new archive/reopen/restart regression initially used an obsolete raw-create
  integration fixture. It now uses the existing crate-private host-authorized
  fixture in `src/host/shutdown.rs` and passes three archive/reopen cycles.
  The unchanged `host_admission` integration suite still has 21 older raw-create
  fixtures rejected by `HostAuthorityRequired`; migrating that entire suite is
  not part of this checkpoint. The production guard was not relaxed.
- Cursor refusal fixtures now inspect a real test executable without executing
  it, rather than asserting capabilities through a nonexistent `C:/bin` path.
- Final provider integration results: Cursor 6/6, journal 3/3, quota 8/8,
  registry 56/56 (73 total). The registry wire test now follows the versioned
  scoped-identity contract and proves the duplicate-field fixture really mutates.
- Final `cargo check --locked --lib --bins --tests` passed in 42.51 seconds.
  `cargo fmt --check` and `git diff --check` passed. Existing warnings remain.
- All owned Cargo/compiler/linker/test processes exited. The installed production
  PID 73848 and start time (2026-08-23T11:55:05.6273041-07:00) are unchanged;
  production config.json and remote.json hashes match the pre-verification values.
  The user's original unsent draft was restored in the dev window, unchanged.

This is a source checkpoint, not full application acceptance or a release-ready
verdict. No push, production profile mutation, provider upgrade, or sign-in was
performed. The sole durable learning update is `src/ui/AGENTS.md`: destructive
confirmations must own exact command receipts and tests must exercise the actual
epoch-fenced completion path. The pre-existing root `AGENTS.md` edit is excluded.
