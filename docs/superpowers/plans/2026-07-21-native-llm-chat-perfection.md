# Native LLM Chat Perfection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Claude and Codex sessions reliably open and behave as a native mobile conversation, with terminal semantics hidden unless the user explicitly requests Raw Terminal.

**Architecture:** Keep Claude/Codex running in their PTYs, but treat provider hooks and rollout events as the primary semantic transcript. When structured events are unavailable, project the authoritative terminal screen as one replacing fallback snapshot instead of appending terminal redraw bytes. Keep presentation decisions in the pure React timeline/session models and use the session summary's semantic AI activity for controls.

**Tech Stack:** Rust 2021, existing terminal model and semantic journal, React/TypeScript, Vitest/Testing Library, existing mobile CSS and hot-load profile.

## Global Constraints

- The native DevManager host remains the only process and transcript authority.
- The ordinary Claude/Codex view must never require reading terminal escape/redraw artifacts.
- Raw Terminal remains available explicitly and automatically only when true terminal-grid interaction is required.
- Composer delivery, reconnect reconciliation, drafts, slash commands, attachments, and writer-lease safety must remain intact.
- Every behavior change follows a witnessed failing test before implementation.

---

### Task 1: Restore current Codex CLI launch and semantic ingestion

**Files:**
- Modify: `src/ai/codex_hooks.rs`
- Test: `src/ai/codex_hooks.rs`

**Interfaces:**
- Consumes: `build_codex_hooks_command(...)` and Codex `-c hooks.<Event>=...` inline TOML overrides.
- Produces: a shell-safe hook command whose TOML value survives Windows native argument parsing and starts current Codex CLI.

- [ ] Add a regression test proving Windows executable paths in generated hook command values use forward slashes while remaining quoted, and that the exact argument parses as a hook sequence.
- [ ] Run the focused test and confirm it fails because the generated override contains escaped Windows backslashes.
- [ ] Normalize only the relay executable path to forward slashes before TOML encoding; preserve endpoint, nonce, startup command, and existing safety validation.
- [ ] Run `cargo test --lib codex_hooks -- --test-threads=1` and confirm the focused launch-builder and relay tests pass.
- [ ] Run the current installed Codex CLI config probe with the generated shape and confirm `features list` exits successfully.

### Task 2: Replace degraded AI byte dumps with one terminal-screen projection

**Files:**
- Modify: `src/terminal/session.rs`
- Modify: `src/services/process_manager.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/remote/mod.rs`
- Modify: `src/remote/presentation.rs`
- Test: the same Rust modules

**Interfaces:**
- Consumes: `TerminalScreenSnapshot` produced after the native terminal model applies each PTY chunk.
- Produces: `SemanticEventKind::Output` carrying one bounded, deduplicated current-screen text snapshot for AI fallback; server/shell output remains append-oriented.

- [ ] Add failing projector tests for cursor-up redraw, clear-screen replacement, carriage-return progress, repeated frames, blank frames, Unicode, TUI chrome suppression, and bounded output.
- [ ] Add a failing remote-event test proving AI output includes its post-parse screen snapshot while server output does not pay the snapshot cost.
- [ ] Extend the output-notifier boundary with an optional screen snapshot captured only for AI sessions after the terminal model has applied the bytes.
- [ ] Add a screen projector that trims blank cells/lines, removes repeated frames and recognized provider chrome, bounds output, and emits no event for an unchanged/empty projection.
- [ ] Record fallback snapshots with one session-scoped deduplication key so each update replaces the prior fallback block rather than appending another terminal frame.
- [ ] Preserve the existing line-oriented byte projector for server, SSH, and shell logs.
- [ ] Run the focused presentation, terminal-session, process-manager, and remote-service tests.

### Task 3: Make the conversation model prose-first and group tool activity once

**Files:**
- Modify: `web/src/sessions/timeline/timelineModel.ts`
- Modify: `web/src/sessions/timeline/timelineModel.test.ts`
- Modify: `web/src/sessions/timeline/eventRenderers.tsx`
- Modify: `web/src/sessions/timeline/eventRenderers.test.tsx`

**Interfaces:**
- Consumes: normalized semantic events, including hidden raw output interleaved between structured tool events.
- Produces: user messages, assistant Markdown, one activity group per conversational interval, actionable questions/errors, and at most one replacing fallback snapshot.

- [ ] Add a failing fixture matching the live Claude trace: tool start/result events separated by hidden output must become one activity group rather than six one-action cards.
- [ ] Add failing summary tests that aggregate repeated actions (`Read 2`, `Searched 3`, `Ran 2`) and keep running/failed groups expanded.
- [ ] Ignore output events before flushing activity when fallback output is disabled; preserve output as a boundary only in degraded mode.
- [ ] Replace single-action labels with compact aggregate summaries and concise detail rows.
- [ ] Render degraded output as one readable full-width live-terminal snapshot with a single `Limited details` label, never a nested tiny scroll box.
- [ ] Add accessible choice buttons for question events and route a selected answer through the existing acknowledged composer submission.
- [ ] Run the focused timeline and renderer tests.

### Task 4: Make session controls and titles reflect the actual AI turn

**Files:**
- Modify: `web/src/sessions/sessionModel.ts`
- Modify: `web/src/sessions/sessionModel.test.ts`
- Modify: `web/src/sessions/SessionScreen.tsx`
- Modify: `web/src/sessions/SessionScreen.test.tsx`
- Modify: `web/src/sessions/views/AiSessionView.tsx`
- Modify: `web/src/sessions/Composer.tsx`
- Modify: `web/src/sessions/Composer.test.tsx`

**Interfaces:**
- Consumes: `WebSessionSummary.aiActivity`, task title, runtime title, lifecycle status, and existing composer mutation methods.
- Produces: a stable semantic title, compact header state, contextual interrupt/send/reopen control, and native textarea behavior.

- [ ] Add failing tests proving runtime spinner/check glyphs never appear in the visible title and a semantic task title remains stable while `aiActivity` changes.
- [ ] Add a failing session test proving Interrupt is absent while an open AI session is idle and visible only while `aiActivity === "Thinking"`.
- [ ] Prefer the semantic task title for AI sessions and sanitize provider status glyphs from fallback runtime titles.
- [ ] Remove the permanent AI action strip; place Interrupt in the composer action position only during an active turn, and show Reopen only for ended sessions.
- [ ] Move degraded-adapter state to one compact header/detail affordance instead of a full-width notice above the transcript.
- [ ] Remove `capture="environment"` so iPhone offers camera, Photos, and Files; align `enterKeyHint` with the configured Return behavior; hide desktop keyboard hints on phone layouts.
- [ ] Run focused session/composer tests.

### Task 5: Mobile layout and reconnect continuity

**Files:**
- Modify: `web/src/index.css`
- Modify only if tests expose a defect: `web/src/app/AppShell.tsx`, `web/src/app/useOfflineIndicator.tsx`, `web/src/store/index.ts`
- Test: relevant React tests

**Interfaces:**
- Consumes: existing semantic timeline, composer, connection presentation, safe-area tokens, and responsive shell.
- Produces: an iPhone-first chat that devotes the viewport to prose and composer, without horizontal page scrolling or reconnect controls.

- [ ] Add/extend component tests for the compact idle/thinking layouts and absence of Resume/Reconnect/Take Control controls.
- [ ] Collapse successful activity into one low-height disclosure row, give prose and fallback text the full content width, and keep only code/tables locally horizontally scrollable.
- [ ] Keep composer sticky above the bottom safe area, use 44-point actions and 16px textarea text, and preserve timeline follow/anchor behavior.
- [ ] Verify the seven-second offline presentation remains compact and automatic; change it only if the test or browser audit disproves the existing contract.
- [ ] Run all web tests, TypeScript checking, and the production web build.

### Task 6: Real-provider hot-load acceptance

**Files:**
- Regenerate: `web/bundle/**`
- Update: this plan's checkboxes and evidence notes

**Interfaces:**
- Consumes: the finished Rust/web implementation in the isolated `dev-watch` profile.
- Produces: browser-observed acceptance evidence for current Claude and Codex CLIs at iPhone and desktop sizes.

- [ ] Start `dev-watch.ps1 -Once` on the isolated profile and use the separate web port.
- [ ] Start fresh Claude and Codex sessions; verify both processes reach a native empty conversation rather than a terminal/config error.
- [ ] For each provider, send an ordinary prompt and a tool-using prompt; verify one user bubble, one streaming assistant message, one grouped activity disclosure, Markdown rendering, and no raw redraw dump.
- [ ] Verify slash-command selection and delivery, answer-choice submission where available, interrupt only while thinking, explicit Raw Terminal round-trip, and idle composer reuse.
- [ ] At 390x844, verify title/project/state, scroll/follow behavior, textarea dictation-compatible attributes, safe-area composer, no horizontal page scroll, and compact activity.
- [ ] Reload and foreground the page; confirm the same host session reconciles automatically with no manual resume/reconnect control.
- [ ] Run Rust formatting, focused Rust suites, complete web gates, production bundle embedding checks, and the broadest stable Rust suite practical in the environment.
- [ ] Stop only hot-load test processes, restore the user's live browser tab, and perform a final diff/security review.
