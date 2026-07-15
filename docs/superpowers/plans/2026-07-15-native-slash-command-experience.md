# Native Slash Command Experience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a provider-aware native slash-command sheet for Claude and Codex web sessions, including safe host discovery of project and personal commands and transparent provider-menu fallback.

**Architecture:** Versioned built-ins remain a pure TypeScript catalog. An authenticated Rust HTTP endpoint discovers bounded custom command metadata for the requested live AI session without exposing paths or bodies. A pure catalog model merges and filters entries; React renders one accessible native sheet inside the existing composer, while the existing acknowledged composer remains the only execution path.

**Tech Stack:** Rust 2021, Axum 0.7, Serde, React 18, TypeScript 5.6, Zustand 5, Vitest 4, Testing Library, Vite 6.

## Global Constraints

- Claude and Codex remain execution authorities; DevManager never emulates provider state mutations.
- The browser receives command names and safe descriptions only, never filesystem paths, file bodies, credentials, or provider settings.
- Arbitrary slash command text remains valid when catalog discovery is incomplete or unavailable.
- Existing writer lease, composer acknowledgement, reconnect, draft, and runtime-reset behavior is preserved.
- Project entries override personal/plugin entries, which override shipped built-in metadata by normalized command name.
- Provider-menu fallback is explicit command metadata, never terminal-output scraping.
- Every production behavior begins with a failing test and receives targeted plus final verification.

---

### Task 1: Safe Host Command Discovery

**Files:**
- Create: `src/remote/web/command_catalog.rs`
- Modify: `src/remote/web/mod.rs`

**Interfaces:**
- Produces: `SlashCommandProvider::{Claude, Codex}`.
- Produces: `DiscoveredSlashCommand { name: String, description: String, source: SlashCommandSource }`.
- Produces: `discover_slash_commands(provider, project_root, session_cwd, home_dir, limits) -> Vec<DiscoveredSlashCommand>`.
- Produces: authenticated `GET /api/slash-commands?sessionKey=<stable key>` returning `{ provider, commands }`.

- [ ] **Step 1: Write scanner RED tests**

Create temporary Claude and Codex trees and assert provider-specific roots, frontmatter parsing, nested command names, project-over-personal precedence, deterministic ordering, bounded file size/count/depth, invalid-file skipping, and no serialized path/body leakage.

```rust
let commands = discover_slash_commands(
    SlashCommandProvider::Claude,
    Some(project.path()),
    project.path(),
    Some(home.path()),
    DiscoveryLimits::for_tests(),
);
assert_eq!(commands[0].name, "/deploy");
assert_eq!(commands[0].source, SlashCommandSource::Project);
assert!(!serde_json::to_string(&commands).unwrap().contains(project.path().to_str().unwrap()));
```

- [ ] **Step 2: Verify scanner RED**

Run: `cargo test remote::web::command_catalog::tests -- --test-threads=1`
Expected: FAIL because the module does not exist.

- [ ] **Step 3: Implement bounded discovery**

Use `std::fs::read_dir` with explicit maximum depth, entry count, and Markdown byte length. Normalize only safe command segments `[A-Za-z0-9_.:-]`; parse simple frontmatter without adding a YAML dependency; deduplicate by normalized name and source precedence.

- [ ] **Step 4: Verify scanner GREEN**

Run: `cargo test remote::web::command_catalog::tests -- --test-threads=1`
Expected: all scanner tests pass.

- [ ] **Step 5: Write endpoint RED tests**

Add router tests proving valid paired cookies receive the matching live Claude/Codex catalog, missing/invalid cookies receive 401, unknown/non-AI sessions receive 404, query length is bounded, and response JSON contains no host paths.

- [ ] **Step 6: Implement authenticated endpoint**

Add `SlashCommandQuery { session_key: String }`, authenticate with `authenticate_request`, resolve exactly one runtime by `StableSessionKey::resolve`, derive project root from `AppState`, call the scanner outside long-lived state locks, and serialize a no-store JSON response.

- [ ] **Step 7: Verify Task 1**

Run: `cargo test remote::web::command_catalog::tests remote::web::tests::slash_command -- --test-threads=1`
Expected: scanner and route tests pass.

- [ ] **Step 8: Commit**

Run: `git add src/remote/web/command_catalog.rs src/remote/web/mod.rs && git commit -m "feat(web): discover provider slash commands"`.

---

### Task 2: Provider Catalog and Pure Merge Model

**Files:**
- Create: `web/src/sessions/commands/types.ts`
- Create: `web/src/sessions/commands/builtinCatalog.ts`
- Create: `web/src/sessions/commands/commandCatalog.ts`
- Create: `web/src/sessions/commands/commandCatalog.test.ts`

**Interfaces:**
- Produces: `SlashCommand { name, description, provider, source, category, argumentHint, suggestions, interaction }`.
- Produces: `commandsForProvider(provider): readonly SlashCommand[]`.
- Produces: `mergeCommandCatalog(provider, builtins, discovered): SlashCommand[]`.
- Produces: `filterCommandCatalog(commands, draft, limit = 80): SlashCommandMatch[]`.
- Produces: `replaceLeadingSlashToken(draft, commandName): string`.

- [ ] **Step 1: Write catalog RED tests**

Assert Claude/Codex separation, representative and total built-in coverage, alias handling, source precedence, stable sorting, `/mod` filtering, description filtering, leading-token replacement, preservation of arguments after the first token, and a hard result limit.

```ts
expect(commandsForProvider("claude").some((item) => item.name === "/compact")).toBe(true);
expect(commandsForProvider("codex").some((item) => item.name === "/permissions")).toBe(true);
expect(replaceLeadingSlashToken("/mod keep this", "/model")).toBe("/model keep this");
```

- [ ] **Step 2: Verify catalog RED**

Run: `npm test -- src/sessions/commands/commandCatalog.test.ts`
Expected: FAIL because the catalog modules do not exist.

- [ ] **Step 3: Implement types, complete built-ins, merge, and filtering**

Encode the reviewed 2026-07-15 Claude and Codex built-ins with concise original descriptions, stable argument hints, safe suggestions, and `providerMenu` only where the provider owns a second interaction. Normalize aliases into searchable metadata without duplicating visible rows.

- [ ] **Step 4: Verify Task 2**

Run: `npm test -- src/sessions/commands/commandCatalog.test.ts`
Expected: all pure catalog tests pass.

- [ ] **Step 5: Commit**

Run: `git add web/src/sessions/commands && git commit -m "feat(web): add Claude and Codex command catalogs"`.

---

### Task 3: Native Command Sheet and Discovery Hook

**Files:**
- Create: `web/src/sessions/commands/useSlashCommandCatalog.ts`
- Create: `web/src/sessions/commands/useSlashCommandCatalog.test.tsx`
- Create: `web/src/sessions/commands/SlashCommandSheet.tsx`
- Create: `web/src/sessions/commands/SlashCommandSheet.test.tsx`
- Modify: `web/src/sessions/Composer.tsx`
- Modify: `web/src/sessions/Composer.test.tsx`
- Modify: `web/src/index.css`

**Interfaces:**
- Produces: `useSlashCommandCatalog({ scopeKey, provider, enabled }): SlashCommandCatalogState`.
- Produces: `SlashCommandSheet({ commands, query, activeIndex, onActiveIndexChange, onSelect, onClose })`.
- Extends: `ComposerProps` with `provider?: WebAiKind` and `onProviderCommandSubmitted?: (command: SlashCommand) => void`.

- [ ] **Step 1: Write discovery-hook RED tests**

Mock `fetch` and assert correct encoded session key, provider validation, built-in fallback on HTTP/network errors, stale response rejection after scope changes, deduplicated in-flight requests, and refresh after the freshness window.

- [ ] **Step 2: Verify discovery RED**

Run: `npm test -- src/sessions/commands/useSlashCommandCatalog.test.tsx`
Expected: FAIL because the hook does not exist.

- [ ] **Step 3: Implement discovery hook**

Fetch only while an AI command sheet is eligible, merge safe response data with built-ins, abort on scope change/unmount, and never block the built-in catalog on discovery.

- [ ] **Step 4: Write sheet and composer RED tests**

Assert `/` opens a labelled listbox, `/mod` filters, Arrow keys update selection, Enter accepts without submitting, Escape closes, tap selection preserves arguments, suggestions insert supported arguments, sheet state reconstructs from a restored draft, ordinary text shows no sheet, non-AI composers show no sheet, and reconnect disables Send without hiding the list/draft.

- [ ] **Step 5: Verify UI RED**

Run: `npm test -- src/sessions/commands/SlashCommandSheet.test.tsx src/sessions/Composer.test.tsx`
Expected: FAIL because command-sheet behavior is absent.

- [ ] **Step 6: Implement native sheet and composer integration**

Derive visibility from the leading draft token, keep active row state scoped to `scopeKey`, prevent listbox keyboard handling from triggering Send, retain the real textarea and dictation attributes, render compact source/category metadata, and keep touch rows at least 44px tall.

- [ ] **Step 7: Verify Task 3**

Run: `npm test -- src/sessions/commands src/sessions/Composer.test.tsx`
Expected: all command UI tests pass without React warnings.

- [ ] **Step 8: Commit**

Run: `git add web/src/sessions/commands web/src/sessions/Composer.tsx web/src/sessions/Composer.test.tsx web/src/index.css && git commit -m "feat(web): add native slash command sheet"`.

---

### Task 4: Session Integration and Provider-Menu Fallback

**Files:**
- Modify: `web/src/sessions/SessionScreen.tsx`
- Create: `web/src/sessions/SessionScreen.test.tsx`
- Modify: `web/src/sessions/views/RawTerminalView.tsx`
- Modify: `web/src/sessions/views/RawTerminalView.test.tsx`
- Modify: `web/src/index.css`

**Interfaces:**
- Passes: `provider={summary.kind}` only for Claude/Codex composers.
- Adds: a scoped `providerInteractionCommand` state set only after acknowledged submission of a `providerMenu` command.
- Extends: `RawTerminalView` with optional `interactionLabel` used for a compact provider-interaction header.

- [ ] **Step 1: Write session RED tests**

Render Claude and Codex summaries and assert provider-specific menus. Submit an inline command and remain native. Submit a `providerMenu` command, resolve the composer acknowledgement, and assert Raw Terminal opens only afterward with the provider/command label. Change runtime/session and assert stale pending callbacks cannot switch the new session.

- [ ] **Step 2: Verify session RED**

Run: `npm test -- src/sessions/SessionScreen.test.tsx src/sessions/views/RawTerminalView.test.tsx`
Expected: FAIL because provider integration and interaction labels are absent.

- [ ] **Step 3: Implement provider wiring and safe fallback**

Pass the provider into `Composer`. Parse only the selected catalog entry associated with the submitted leading token. After `submitComposer` resolves, set terminal pin and interaction label for `providerMenu`; keep inline commands in the native view. Existing raw/native header toggle returns to conversation and clears the interaction label.

- [ ] **Step 4: Verify Task 4**

Run: `npm test -- src/sessions/SessionScreen.test.tsx src/sessions/views/RawTerminalView.test.tsx src/sessions/Composer.test.tsx`
Expected: all integration tests pass.

- [ ] **Step 5: Commit**

Run: `git add web/src/sessions web/src/index.css && git commit -m "feat(web): route interactive provider commands"`.

---

### Task 5: Full Verification and Hot-Load Acceptance

**Files:**
- Modify if defects are found: files from Tasks 1-4 and their matching tests.
- Update: `docs/superpowers/specs/2026-07-15-native-slash-command-experience-design.md` only if verified behavior requires a documented correction.

- [ ] **Step 1: Run complete web verification**

Run: `npm test -- --run && npm run typecheck && npm run build` from `web/`.
Expected: all tests, TypeScript checks, and Vite production build pass.

- [ ] **Step 2: Run Rust formatting and focused tests**

Run: `cargo fmt --check` and `cargo test remote::web::command_catalog::tests remote::web::tests::slash_command -- --test-threads=1`.
Expected: formatting and focused tests pass.

- [ ] **Step 3: Run full stable Rust verification**

Run: `cargo test -- --test-threads=1`.
Expected: all library, integration, and doc tests pass. Do not use the flaky parallel baseline as the release gate.

- [ ] **Step 4: Start hot-load DevManager**

Run: `powershell -ExecutionPolicy Bypass -File .\dev-watch.ps1` from the worktree. Use the isolated `DEVMANAGER_PROFILE=dev-watch`, configure a non-live browser port if required, and pair the in-app browser.

- [ ] **Step 5: Browser acceptance**

At an iPhone viewport and desktop viewport, verify Claude and Codex have different catalogs; `/`, filtering, touch and keyboard selection work; native suggestions preserve dictation/text; custom project commands appear; disconnect/reconnect retains draft and sheet; arbitrary unknown slash text submits unchanged; inline commands stay native; and provider-menu commands open the real terminal picker after acknowledgement.

- [ ] **Step 6: Review requirements and diff**

Read the approved spec line by line, inspect `git diff --check`, `git status --short`, and the full diff. Fix every uncovered requirement or regression with a new failing test first.

- [ ] **Step 7: Commit verification corrections**

Run: `git add src/remote/web/command_catalog.rs src/remote/web/mod.rs web/src/sessions/commands web/src/sessions/Composer.tsx web/src/sessions/Composer.test.tsx web/src/sessions/SessionScreen.tsx web/src/sessions/SessionScreen.test.tsx web/src/sessions/views/RawTerminalView.tsx web/src/sessions/views/RawTerminalView.test.tsx web/src/index.css docs/superpowers/specs/2026-07-15-native-slash-command-experience-design.md && git commit -m "test(web): verify native slash command experience"` only when Task 5 produced corrections; otherwise leave the prior task commits unchanged.
