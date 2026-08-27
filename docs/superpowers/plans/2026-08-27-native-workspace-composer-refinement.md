# Native Workspace and Composer Refinement

**Goal:** Implement the user's 2026-08-27 interaction, provider discovery, usage,
branding and composer requests without another design/approval round.

**Architecture:** Keep the recursive TaskWorkspace, native GPUI event loop, typed
host IPC and durable conversation identity. Read provider metadata immediately
from a profile-scoped cache, refreshing off-thread. Draft provider choice must be
independent of the task title and become bound only when the first turn launches.
Use existing attachment authority and real provider input; never simulate usage.

**References:** local t3code-main provider drivers/model picker; DevManager tag
v0.4.1 branding/title/input; existing GPUI components and Zed interaction code.

## Constraints

- Work in C:\Code\userfirst\devmanager, VisualDevManager, preserving hot reload.
- One source writer at a time; Cursor is the bounded implementation worker.
- Preserve the existing root AGENTS.md modification. No production app/profile
  writes, provider installs/auth changes, fake conversation IDs, or fresh fallback.
- Grok/OpenCode remain unavailable stubs. Multi-folder project authority remains.
- User overrides approval questions and per-edit test runs: write regressions,
  consolidate execution after coherent implementation. Root owns final Cargo
  in C:\Temp\devmanager-native-multi-task-VisualDevManager; no duplicate builds.
- No worker commits, merges, recursive delegation or broad formatting.

## Phase 1: Native interactions and presentation

Owned files: src/ui/native_shell.rs, src/ui/task_workspace/{layout,allocation,
surfaces,view}.rs, src/ui/components/text_field.rs, src/ui/task_cockpit/composer*
as needed, src/icons.rs, assets/icons, existing branding/window initialization.

- [ ] Trace real selection flow and add a regression: with A/B open and B focused,
  plain-click C yields A/C with B's pane ID/geometry transferred; no sibling loss.
  Shift-click remains toggle and clicking an already-open task focuses it.
- [ ] Fix divider drag ownership/hit area so motion continues over either pane.
  Use stable gesture origin; resize requested child and redistribute auto peers.
  Preserve unrelated explicit pins; when all pinned use least-recent focus fallback.
  Assert repeated forward/back drags and nested geometry conservation.
- [ ] Done is compact at sidebar bottom, excluded from active rows. Viewing Done
  does not restore it; only explicit Restore or an accepted new-message workflow
  reopens it. Test mouse, keyboard and message-send paths.
- [ ] Refine new-task action beside All projects with an existing icon/accent and
  accessible tooltip. Restore real DevManager branding in shell and Windows chrome.
  Dynamic title is `project | task title | DevManager`, with sensible empty values.
- [ ] Make model/thinking/access controls compact (about 24px high, small labels,
  constrained dropdown width/height). Keep keyboard selection/accessibility.
- [ ] Replace row-sized wheel jumps/rebuilt accessibility per wheel with bounded
  smooth pixel scrolling; avoid double event handling and preserve scrollbar gutter.
- [ ] Implement pointer drag/shift/keyboard text selection using actual shaped text
  and UTF-16/scalar conversion; preserve IME, copy/cut/paste and parked drafts.
- [ ] Support direct clipboard images through existing bounded attachment pipeline,
  removable previews, and actual image-bearing send payloads; reject unsupported
  provider attachment types visibly and do not silently convert them to text paths.

## Phase 2: Provider metadata, draft routing and usage

Owned files: provider settings/health/catalog, quota collectors, host provider launch
and cockpit IPC, draft/task command projection, and compact composer consumption.

- [ ] Trace T3 live model/reasoning discovery for Claude/Codex/Cursor. Cache last-good
  validated results by provider instance/account/config; stale results stay usable
  during bounded refresh. Fable is included when reported by Claude; do not hardcode
  availability from this request. Preserve explicit custom models and visibility.
- [ ] Track unstarted draft using durable provider-session/message facts rather than
  placeholder title. Allow any enabled supported instance/model before first send;
  bind exact selected provider and options at launch, even after rename/restart.
- [ ] Query real configured-provider usage off-thread and cache validated snapshots.
  Display reported five-hour/week/month windows/reset times and Cursor included/API
  buckets separately. Unknown/stale/error are honest states, never synthetic 100%.
- [ ] Test account/config isolation, cache reload/staleness, malformed metadata,
  model-specific thinking options, draft switching and actual launch/payload routing.

## Phase 3: Consolidated verification and handoff

- [x] Join worker descendants, inspect complete diff and resolve integration defects.
- [x] Run changed-domain regressions and relevant integrations, then sequential
  library suite and `cargo check --locked --lib --bins --tests`; one compiler owner.
- [ ] Run formatting and whitespace gates, native click/drag/type/paste/send checks.
  Verify focused-pane replacement, both-direction resize, Done persistence,
  first-send provider selection, image payload and genuine AI response.
- [ ] Recheck installed PID/start and production config/remote hashes; no harnesses
  left. Review and commit only owned paths locally; no push.

## Acceptance ledger

Source integration is complete for the requested workspace/Done/branding/input
refinements, cached model/thinking discovery and provider usage. The checklist
above remains an acceptance checklist, not a claim of live verification.

Current evidence:

- Final isolated `cargo check --locked --lib --bins --tests` passed (warnings
  only), after all source reconciliations.
- Final `cargo test --locked --lib -- --test-threads=1`: **3,154 passed, zero
  failed, one ignored** (803.73 seconds). The ignored test invokes the real
  npx/Codex binary. This includes current Cursor wrapper/injection fixtures,
  launch-only settings guards, canceled-start readiness, transactional selection,
  nested layout floors and pin preservation.
- `cargo test --locked --test provider_input`: **28 passed**, including durable
  receipt/reopen and no-ID first-turn cases. Focused metadata (30) and workspace
  (50) groups also passed during reconciliation.
- Changed Rust files pass `rustfmt --check --edition 2021`; whitespace checks
  pass. All delegated Cursor wrappers and descendants have exited, and no root
  Cargo, compiler, linker or test-harness process remains after final verification.
- The running dev host populated its cache with five Claude model rows (including
  provider-reported Fable), seven Codex rows, and 35 Cursor rows. Fresh Codex usage
  contains the provider-reported weekly and model-scoped five-hour/week windows
  with numeric remaining percentages. Cursor reports separate included/API
  percentages, not a synthetic blended allowance.
- Claude's selected credential file currently contains no access/refresh token;
  usage therefore reports authentication required. No sign-in/reset was attempted.
- Installed PID 73848/start time and production config/remote hashes are unchanged.
- Computer Use initially captured a black desktop and the latest capture shows
  only the screensaver. Click/drag/type/image-send and genuine new AI-response
  acceptance remain unverified; no user drafts were sent or cleared.

Explicit residuals, not completed features:

- Cursor model/usage discovery is implemented, but the existing Cursor adapter
  has no chat input/semantic transport or exact resume. First send now fails
  visibly and preserves the draft instead of waiting indefinitely.
- Model/thinking/access selections are wired to provider launch. Changing these
  on an already-running provider is not a live reconfiguration operation. Started
  sessions therefore show "Options managed by provider" rather than editable
  controls or misleading access/model labels. Direct/keyboard selection is also
  fenced; new drafts retain the working compact dropdowns.
- Typed image identity, validation and separate bracketed path-paste writes are
  implemented. End-to-end inline vision, especially Claude, is not yet proven.

The no-conversation-ID Codex first-send path uses a host-attested exact live
runtime/terminal binding with the adapter's explicit Unsupported identity
capability, and a fresh owned query. It never fabricates a conversation ID.
