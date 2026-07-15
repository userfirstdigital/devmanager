# Native Slash Command Experience Design

**Date:** 2026-07-15
**Status:** Approved for implementation
**Scope:** Add a provider-aware, native-mobile slash command experience to Claude and Codex web sessions while preserving each CLI as the execution authority.

## Context

The native web composer currently treats every message as undifferentiated text. Claude and Codex both expose large, different slash-command catalogs, and both may add commands from project skills, personal skills, plugins, custom command files, and MCP prompts. A single hard-coded merged menu would show invalid commands and drift quickly; reproducing provider behavior in DevManager would create a second source of truth.

## Goals

- Typing `/` in a Claude or Codex composer opens a compact, searchable command sheet above the mobile keyboard.
- Show only commands for the active provider and label built-in, project, personal, plugin, and MCP sources.
- Ship complete versioned built-in catalogs for the supported Claude and Codex releases.
- Discover local project and personal skills/custom commands without exposing filesystem paths to the browser.
- Keep arbitrary slash text valid even when a new provider command is not yet cataloged.
- Preserve drafts and automatically reconstruct the open command sheet after app suspension, reload, and reconnect.
- Keep command execution host-authoritative by submitting the selected text through the existing acknowledged composer mutation path.
- Use native argument guidance where the provider documents stable arguments; fall back to the provider's own terminal interaction for commands whose menus are not safely reproducible.

## Non-goals

- Scraping terminal pixels or ANSI output to discover the provider menu.
- Pretending that platform-, plan-, account-, model-, plugin-, or MCP-dependent commands are always available.
- Reimplementing destructive provider actions such as logout, deletion, or process exit inside the browser.
- Exposing project roots, home directories, command-file contents, credentials, or provider configuration to the browser.
- Changing the PTY, composer acknowledgement, writer lease, or reconnect ownership protocols.

## Catalog architecture

`web/src/sessions/commands/builtinCatalog.ts` contains provider-specific built-ins with a reviewed catalog date. Each entry has a stable name, concise description, optional argument hint and suggestions, category, source, and interaction mode.

`src/remote/web/command_catalog.rs` discovers only safe metadata for custom entries. It resolves the requested stable session key against host state, uses the session provider and working directory, and scans bounded known roots:

- Claude: project/personal `.claude/skills` and `.claude/commands`, plus bounded installed-plugin skill roots.
- Codex: project/personal `.agents/skills`, legacy `.codex/skills`, `.codex/prompts`, and bounded installed-plugin skill roots.

The scanner reads only bounded Markdown metadata. `SKILL.md` uses frontmatter `name` and `description`; command/prompt files use frontmatter when present and otherwise the first non-empty prose line. Invalid, oversized, hidden, duplicate, or path-escaping entries are ignored. Project entries override personal entries with the same provider command name, matching provider precedence where known.

The authenticated `GET /api/slash-commands?sessionKey=...` endpoint returns the provider plus discovered command metadata. It never returns paths or file bodies. The browser merges that response with its built-ins, with discovered entries taking precedence by name. Fetch failure leaves the complete built-in menu usable and shows no blocking error.

MCP prompt catalogs are runtime-owned and not consistently exposed by either provider through a stable external enumeration API. Known/discovered MCP-style commands can be shown, but arbitrary command text always remains executable so a newly introduced provider or MCP command is never blocked by DevManager.

## Native interaction

The command sheet is derived from the leading command token in the persisted draft. `/` opens all entries; `/mod` filters by command name, description, category, and source. The sheet uses one compact scrolling list rather than cards.

- Touch: tap an entry to replace the leading token while preserving later argument text.
- Keyboard: Arrow Up/Down changes the active row, Enter accepts it, Escape closes it, and ordinary typing continues filtering.
- Accessibility: the sheet is a labelled listbox, rows are options with selected state, status text announces result counts, and touch targets remain at least 44 logical pixels.
- Dictation: the textarea remains a real native textarea with autocorrect and speech-to-text behavior unchanged.
- Scope: changing runtime or stable session clears ephemeral selection state; the persisted draft remains scoped by runtime and stable session as today.

Commands with documented argument suggestions expose native suggestion chips after selection. Suggestions only insert provider-supported text; they do not mutate DevManager state. Commands without arguments simply insert the command and leave the existing Send affordance in control, avoiding accidental destructive execution.

## Provider interaction fallback

Commands marked `providerMenu` are still submitted through the acknowledged composer. After acceptance, `SessionScreen` switches to Raw Terminal so the real Claude/Codex picker is immediately visible and usable. The existing native/raw toggle returns to the conversation; no separate resume or reconnect action is introduced. Commands marked `inline` remain in the native transcript.

This fallback is intentionally command metadata, not terminal heuristics. It protects compatibility when provider pickers, plan availability, model catalogs, permissions, accounts, or plugins change.

## Error and reconnect behavior

- Discovery unavailable: keep built-ins and allow arbitrary text; retry on the next menu open after a short client-side freshness window.
- Session removed or changed provider: discard the stale discovery response using the request scope key.
- Offline: keep the filtered sheet and draft visible but disable submission through the existing composer rules.
- Host runtime change: the existing runtime-scoped draft cleanup clears old commands and UI state.
- Unknown command: submit it unchanged; the provider owns its error message.
- Duplicate command: prefer project, then personal/plugin, then shipped built-in metadata.

## Test strategy

Rust unit tests cover bounded scanning, frontmatter parsing, provider roots, precedence, invalid UTF-8/oversized files, path privacy, authentication, unknown sessions, and non-AI sessions. React/Vitest tests cover provider separation, filtering, keyboard/touch selection, argument preservation, suggestions, custom-command merging, fetch failure, reconnect restoration, session-scope races, and provider-menu fallback. Final verification includes all web tests, TypeScript, production web build, serial Rust tests, and hot-load browser checks at iPhone and desktop viewports against real Claude and Codex sessions where locally available.

## Acceptance criteria

- Claude and Codex show different correct built-in catalogs.
- Project/personal custom commands appear without leaking paths or contents.
- Typing, filtering, selection, dictation, and Send remain native and responsive on iPhone.
- A draft beginning with `/` restores the command sheet automatically after reload/reconnect.
- Any slash command can still be typed and submitted even if it is absent from the catalog.
- Provider-owned interactive pickers open automatically in Raw Terminal after acknowledged submission.
- All automated verification and hot-load acceptance checks pass before integration.
