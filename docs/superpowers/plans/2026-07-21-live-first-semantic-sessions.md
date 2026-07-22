# Live-first semantic Sessions implementation plan

**Goal:** Make the mobile Sessions screen a truthful control surface: current live sessions always appear first, every row explains the task, project, provider/type, state, and activity, and stale failures cannot bury active work.

**Architecture:** Keep session identity, provider title, task title, and AI activity host-authoritative. `SessionRuntimeState` tracks the terminal/provider title and AI activity used by the native desktop sidebar, while `SemanticSessionMetadata` retains a bounded stable title from the first substantive user message when the provider emits only a generic terminal title such as `✳ Claude Code`. Expose both through `WebSessionSummary`. The web presentation layer prefers a meaningful non-generic runtime title, then the semantic task title, then a meaningful explicit tab label and safe fallbacks. Grouping always places live runtime states in the first section regardless of attention state; stopped failures remain historical.

**Tech stack:** Rust/Serde semantic journal and web DTO, React/TypeScript, Vitest/Testing Library, existing mobile CSS, `dev-watch.ps1` hot reload.

## Task 1: Project the existing native title and AI activity

**Files:**
- Modify: `src/remote/web/dto.rs`
- Modify: `web/src/api/types.ts`
- Test: `src/remote/web/dto.rs`

1. Add a failing DTO assertion that the snapshot includes the runtime session `title` and `aiActivity` in camelCase.
2. Run the focused Rust test and confirm it fails for the missing fields.
3. Add the nullable/optional additive fields to the Rust DTO and TypeScript contract without changing the protocol version.
4. Re-run the DTO test and relevant TypeScript checks.

## Task 1b: Add a semantic fallback for generic provider titles

**Files:**
- Modify: `src/remote/presentation.rs`
- Modify: `src/remote/web/dto.rs`
- Modify: `web/src/api/types.ts`
- Modify: `web/src/sessions/sessionModel.ts`
- Test: `src/remote/presentation.rs`
- Test: `src/remote/web/dto.rs`
- Test: `web/src/sessions/sessionModel.test.ts`

1. Preserve the hot-reload RED evidence: a real live session projects `✳ Claude Code` rather than a task title.
2. Add failing journal tests proving the first non-command `UserMessage` becomes a whitespace-normalized, Unicode-safe bounded task title; leading slash commands and later messages do not rename it.
3. Add failing DTO and web-model tests proving `taskTitle` is projected and wins when runtime/tab titles are generic provider chrome, including decorated `✳ Claude Code` and `OpenAI Codex` forms.
4. Store the fallback in semantic metadata, expose it as an additive nullable DTO field without a protocol bump, and extend generic-label detection narrowly.
5. Re-run focused Rust/web tests, full gates, rebuild the embedded bundle, and repeat the real hot-reload browser acceptance.

## Task 2: Make session presentation live-first and descriptive

**Files:**
- Modify: `web/src/sessions/sessionModel.ts`
- Modify: `web/src/sessions/sessionModel.test.ts`

1. Replace the existing grouping expectation with failing tests that all `Starting`, `Running`, and `Stopping` sessions remain in the live group even when they need input or have failed attention; stopped/crashed failures remain recent history.
2. Add failing title tests for authoritative runtime titles replacing generated labels such as `Claude 6`, while preserving configured command/SSH names.
3. Add failing state tests for Thinking, Ready, Needs input, degraded native adapters, and live failed attention so rows communicate real activity or terminal fallback instead of generic `Open`.
4. Implement the minimal presentation rules and deterministic sorting (actionable live work first, then newest activity).
5. Re-run the model tests.

## Task 3: Recompose the Sessions screen for phone scanning

**Files:**
- Modify: `web/src/sessions/SessionsScreen.tsx`
- Modify: `web/src/App.tsx`
- Modify: `web/src/styles.css`
- Create: `web/src/sessions/SessionsScreen.test.tsx`

1. Add failing DOM tests proving `Live now` renders before historical sections and each row exposes title, project, provider/type, state, and activity.
2. Render the live section first, make attention a state badge rather than a competing top-level archive, and compact row spacing without hiding metadata.
3. Add accessible row labels and avoid numeric badges for non-unread historical failures.
4. Make the bottom navigation badge count only genuinely actionable live/needs-input work, not stale failures.
5. Re-run the focused web tests and typecheck.

## Task 4: Independent review and correction

1. Review the complete diff for truthfulness, compatibility, and unrelated changes.
2. Resume the same Cursor worker session with concrete correction requests if the implementation or tests miss the acceptance contract.
3. Run the focused Rust and web checks after corrections.

## Task 5: Full verification and hot-reload acceptance

1. Run the full web test suite, typecheck/build, Rust formatting, and relevant/full Rust tests.
2. Start the isolated worktree using `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1 -Once` without stopping the installed live DevManager.
3. At a 390 x 844 browser viewport, verify real live sessions appear first and every visible row has a meaningful task title or honest fallback, project, provider/type, truthful state, and activity.
4. Exercise navigation into a live session and back, reconnect once, and confirm ordering and identity remain stable without a resume action.
5. Restore the user's browser tab and stop only the isolated hot-reload process.
