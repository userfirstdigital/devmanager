# Conversation Derivation as a Single Rust Source of Truth

## Context

DevManager's native conversation surface has resisted several rounds of visual work. The
2026-08-18 native conversation-first redesign specified the presentation correctly but assumed a
derivation layer that does not exist. This design supplies that layer and states where its truth
lives.

The trigger for this work was an evaluation of T3 Code (`C:\Code\userfirst\t3code-main`, MIT,
v0.0.33) as a possible replacement or host. That evaluation is recorded here because its outcome
constrains this design.

### Rejected alternatives

**A T3 Code plugin or extension.** T3 Code exposes no plugin loader, no extension host, and no
dynamic driver registration. `apps/server/src/provider/builtInDrivers.ts` declares
`BUILT_IN_DRIVERS` as a static compile-time array, and its own docblock states that adding a
driver means editing that array. The project README states that large feature contributions will
not be accepted. Building an extension seam would therefore mean authoring it, carrying it as a
permanent fork patch, and rebasing it against a v0.0.x upstream. Rejected.

**Porting T3 Code into DevManager.** T3 Code is roughly 710,000 lines of TypeScript on Effect-TS,
Effect RPC, React, Electron, and Clerk. There is no mechanical path from that to GPUI and Rust;
adopting it would be a reimplementation, not a port. Rejected.

**Adopting T3 Code as the host and rebuilding DevManager on it.** T3 Code has no equivalent of
DevManager's browser automation subsystem (`src/browser`, 39 files) or its organization and
Connect layer (`src/org` and `src/connect`, 49 files) — Ed25519 enrollment, membership,
envelope crypto, org prompts, invitations, deletion ledger. Its only browser surface is a
Playwright-backed preview MCP toolkit, and its "Connect" is Clerk authentication plus a Cloudflare
relay to the user's own devices, with no multi-user or organization concept. Both subsystems are
shipping product. Rejected.

**Retained value from T3 Code.** Three derivation behaviours that neither of DevManager's existing
implementations has, and which are specified in framework-free TypeScript that translates
directly: turn folding, stable row identity across streaming deltas, and elapsed-time attribution.
Plus a composer inline-token model described under "Composer derivation" below. These are adopted
as designs, not as code.

## Problem

The conversation type is closed at the host boundary, destroyed in the middle, and guessed back at
the leaf.

1. `src/remote/presentation.rs:110` defines `SemanticEventKind` as a closed Rust enum:
   `UserMessage`, `AssistantMessage`, `Reasoning`, `Tool`, `Diff`, `Command`, `Output`,
   `Question`, `Status`, `Error`, `TerminalMode`.
2. `src/kernel/semantic_journal.rs:67` stores that discriminant as `pub kind: String`. The
   vocabulary is still closed — `semantic_journal.rs:957` validates every fact against
   `ALLOWED_JOURNAL_KINDS` — but the type no longer carries it.
3. `src/ui/renderers/journal_view.rs:358` propagates the string as
   `source_type: fact.kind.clone()`.
4. `src/ui/renderers/generic.rs:77` funnels any unmapped payload into
   `TimelineItemContent::Generic(GenericSemanticCard)`, keyed by that string truncated to 64
   characters.
5. `src/ui/task_cockpit/timeline.rs:33-46` recovers the role by substring
   (`role.starts_with("error")`, `role.contains("reason")`), and `timeline.rs:69-78` suppresses
   chrome with a substring denylist over `source_type` and `title`
   (`contains("session_state")`, `contains("usage")`, `contains("unknown")`,
   `contains("diagnostic")`).

Step 5 is fail-open. An event kind that no one has written a denylist entry for renders as a
visible conversation card by default. The transcript therefore regresses toward debug output every
time a provider's event vocabulary changes, and the remediation work is unbounded because the
denylist is chasing an open set.

The correct derivation already exists, in TypeScript, serving only the PWA.
`web/src/tasks/timeline/timelineModel.ts:16` declares `ConversationItem` as a closed five-variant
union — `message`, `activity`, `question`, `error`, `fallbackOutput` — and
`timelineModel.ts:125` derives it with a pure function over `SemanticEvent[]`. It classifies on
the discriminant rather than on substrings, groups activity by a computed identity, and drops
lifecycle noise with an explicit kind check (`if (event.kind === "status" || event.kind ===
"terminalMode") continue;`). It is covered by 283 lines of tests.

This is a single-source-of-truth violation. Two conversation derivations exist, in two languages,
and only the one on the wrong side of the boundary is correct.

## Scope of the TypeScript boundary

The project rule is that all truth lives in Rust and TypeScript exists only as a web front end.
An audit of every `include_str!` and `include_bytes!` site in `src/` — 57 in total, 42 and 15
respectively — found that exactly three read anything under `web/`:

- `src/remote/web/bridge.rs:10315` reads `web/bundle/source-fingerprint.txt`, a generated build
  artifact. Legitimate; unchanged by this design.
- **`src/ui/task_cockpit/composer.rs:174` and `composer.rs:237` read
  `web/src/tasks/commands/builtinCatalog.ts`.** These are the only two sites in the entire crate
  that read a TypeScript file, and the only two that treat TypeScript as truth.

Of the remaining 54 sites, 33 read test fixtures under `tests/fixtures` and the rest are
Rust-internal relative includes.

Everything else under `web/src` is presentation and transport: store, WebSocket API, components,
PWA registration and notifications, Connect transport. That part of the boundary is correct and is
not changed by this design.

The two exceptions are both closed by this design.

## Design

### 1. Restore the discriminant

Introduce a closed Rust enum for the UI-facing conversation vocabulary, derived from
`SemanticEventKind`. Durable journal storage keeps `kind: String` — that is a persisted schema and
is not churned by this work — but every read path that feeds the conversation converts through a
**total** mapping into the closed enum.

The mapping has an explicit `Unknown { source_type, diagnostic_ref }` variant. `Unknown` is
representable, addressable by the inspector and diagnostics surfaces, and **not renderable as
conversation**. It has no branch in the conversation renderer, so a new provider event kind is
invisible in the transcript by construction rather than by denylist.

`is_hidden_conversation_chrome` in `src/ui/task_cockpit/timeline.rs` is deleted, not extended.
`TimelineItemContent::Generic` is removed from the conversation path and retained only for the
inspector.

### 2. `src/ui/conversation/rows.rs` — pure derivation

A new module with no `gpui` imports, unit-testable without a window. It owns:

- **`ConversationRow`** — a closed enum. Its initial variants are the five already proven in
  `timelineModel.ts` (`Message`, `Activity`, `Question`, `Error`, `FallbackOutput`) plus the three
  adopted from T3 Code: `TurnFold`, `ActivityToggle`, and `Working`. There is no generic,
  diagnostic, or escape-hatch variant.
- **`ConversationVerbosity`** — a closed enum `Minimal`, `Calm`, `Full`, controlling how much
  settled activity is derived at all. This is the TypeScript `InterfaceDensity`
  (`web/src/tasks/timeline/eventRenderers.tsx:14`) under a different name, and the rename is
  deliberate: `src/ui/tokens.rs:15` already defines `Density { Compact, Comfortable }`, which is a
  visual metric scale over spacing, radii, typography, icons, controls, and motion. The two
  concepts are unrelated — one selects which rows exist, the other selects how large they are —
  and sharing the word `density` across both would put two vocabularies behind one identifier in
  one crate. Note that the native client currently has no content-verbosity concept at all; this
  is a capability the PWA has and GPUI does not.
- **`derive_conversation_rows(events, verbosity) -> Vec<ConversationRow>`** — a port of
  `buildConversationItems`, preserving its activity-identity grouping, its explicit lifecycle-kind
  skip, and its fallback-output bounding (`MAX_FALLBACK_OUTPUT_CHARS`).
- **`stable_conversation_rows(prev, next) -> Vec<ConversationRow>`** — row identity preserved
  across streaming deltas, modelled on T3 Code's `computeStableMessagesTimelineRows` (a keyed map
  plus a result vector). This is what prevents the transcript re-mounting rows while assistant
  tokens stream.
- **Turn folding** — modelled on T3 Code's `TurnFold`: a turn contributes an anchor row, a set of
  hidden entry ids, and a label; the fold is expandable.
- **Elapsed-time attribution** — modelled on `computeMessageDurationStart`: a user message opens a
  duration boundary, a settled assistant message closes it.

Tests are translated from the two existing specifications: `timelineModel.test.ts` (283 lines,
DevManager's own) and the relevant cases of T3 Code's `MessagesTimeline.logic.test.ts` (1,185
lines) for the three adopted behaviours.

### 3. PWA cutover, staged

The end state is that Rust derives and both clients consume. Because the wire currently carries
raw `SemanticEvent[]`, the cutover is staged so nothing is destroyed before its replacement is
proven.

- **Phase 1.** `rows.rs` drives the GPUI client. `timelineModel.ts` continues to drive the PWA
  unchanged. A conformance test runs both derivations over a shared fixture corpus and asserts
  they agree. The corpus is checked in and is the authority for both.
- **Phase 2.** The host emits derived rows on the wire; the PWA renders them; `timelineModel.ts`
  and `buildConversationItems` are deleted along with the conformance test.

Phase 2 is a wire change and carries its own protocol-compatibility work. It is in scope for this
design but may land as a separate implementation train.

### 4. Composer derivation

DevManager has no inline-token model. `src/ui/task_cockpit/composer.rs` and
`src/client/composer.rs` contain no mention, chip, file-reference, or inline-token concept. This
adopts T3 Code's design, which is specified in two framework-free modules
(`composer-logic.ts`, 287 lines; `composer-editor-mentions.ts`, 223 lines).

New module `src/ui/composer/segments.rs`:

- **`PromptSegment`** — a closed enum: `Text`, `Mention { path, source }`, `Skill { name }`.
  T3 Code's fourth variant, `terminal-context`, is deferred; DevManager's terminal context is
  attached through a different mechanism and folding it in is out of scope.
- **A dual cursor model.** *Expanded* text is what the provider receives, carrying the full
  `@path/to/file` source. *Collapsed* text is what the user edits, in which each non-text segment
  occupies exactly one position. `expand_collapsed_cursor`, `collapse_expanded_cursor`,
  `clamp_collapsed_cursor`, and `collapsed_cursor_adjacent_to_inline_token` map between them.
  This is what makes a mention behave as an atomic chip — arrow keys step over it whole, backspace
  deletes it whole — while the provider still receives the full path.

New module `src/ui/composer/trigger.rs`:

- **`ComposerTrigger`** — a closed enum: `Path` (`@`), `SlashCommand` (`/`), `Skill` (`$`), each
  carrying a query and an explicit replace range.
- **`detect_trigger(text, cursor) -> Option<ComposerTrigger>`** and
  **`replace_text_range(text, start, end, replacement)`**.

### 5. Slash-command catalog: Rust owns it

`provider_command_catalog` at `src/ui/task_cockpit/composer.rs:171-227` reads
`web/src/tasks/commands/builtinCatalog.ts` through `include_str!` and scrapes it with a
hand-written parser: it splits on the literal source markers `"const CLAUDE_SEEDS:"`,
`"const CODEX_SEEDS:"`, and `"export const CLAUDE_BUILTIN_COMMANDS"`, keeps only lines beginning
`["/`, and parses quoted fields by index.

This is anchored on source text and fails silently. Any rename, reorder, reformat, or line-wrap in
`builtinCatalog.ts` causes `split_once` to miss and the function to return `Vec::new()`; the slash
menu goes empty and no gate goes red. `ProviderKind::Cursor` already returns `Vec::new()`
unconditionally. A second scrape site exists at `composer.rs:237`.

Replacement: a Rust const table in `src/ui/composer/catalog.rs` is the sole source of truth.
`build.rs` — which already exists in this repository — emits the TypeScript module the web front
end imports, as a generated artifact. Both `include_str!` scrape sites and both hand-written
parsers (`parse_catalog_string`, `catalog_aliases`) are deleted.

Because the generated module is now a build output, a rename in the Rust table is a compile error
rather than an empty menu. A test asserts a non-zero command count per supported provider so that
"the catalog is empty" and "the catalog was not read" cannot report as the same fact.

### 6. Reduce the two oversized files

`src/ui/task_cockpit/timeline.rs` (971 lines) currently performs projection, virtualization,
painting, activity counting, and classification together. `src/ui/task_cockpit/composer.rs`
(3,715 lines) carries domain payloads, host projection, catalog scraping, GPUI rendering, and
error types together.

After the modules above land, both are reduced to virtualization and painting over already-derived
models. Domain payloads and host projection move to sibling modules under their existing names.

This mirrors a discipline T3 Code applies systematically — 33 `.logic.ts` modules with no
framework imports, separated from their renderers. It is worth noting that T3 Code does not apply
it uniformly: `ChatComposer.tsx` is 3,245 lines and `composerDraftStore.ts` is 3,767. The
transferable practice is the pure-logic extraction, not their file sizes.

## Invariants preserved

This design changes presentation derivation only. The following are unchanged and every new module
must respect them:

- The host remains the execution authority. No derivation reaches around the host to mutate task,
  process, Git, browser, or provider state.
- Provider conversation identity (`providerSessionId`) is never synthesized or inferred, and is
  captured only from correlated current-generation `SessionStart` hooks.
- `ComposerFence`, `action_epoch`, and `runtime_generation` continue to fence composer submissions.
- Question and approval identity fencing (`pending_question_identity`,
  `pending_approval_identity`) is unchanged.
- `PromptVersionRef`, `ExactPromptPayload`, and `DraftProvenance` continue to carry exact prompt
  provenance.
- `ComposerHostProjection` and `projection_for_task` remain the composer's host boundary.
- No raw PTY transcript becomes semantic conversation truth.
- Durable journal storage and `ALLOWED_JOURNAL_KINDS` are unchanged.

## Target UX

The visual target is T3 Code's conversation surface, reproduced as closely as GPUI allows. This
section records it as measured values rather than description, so the native implementation is
checkable against it. Values are extracted from
`apps/web/src/components/chat/MessagesTimeline.tsx`, `ChatComposer.tsx`,
`MessagesTimeline.logic.ts`, and `apps/web/src/index.css`, and confirmed against a running
instance of the desktop app.

### Governing principles

These are what make the surface read as a conversation rather than a log, and they are more
important than any single measurement below.

1. **No borders inside the conversation.** Separation comes from whitespace and surface lightness.
   The only border in the transcript is the hairline under a turn fold. DevManager's current
   bordered-row treatment is the single largest visual difference.
2. **Radical user/assistant asymmetry.** The user's message is a small right-aligned pill with a
   surface. The assistant's message has **no bubble, no border, no avatar, and no role label** —
   it is document-flow markdown at the full column measure. Nothing marks it as a message.
3. **Chrome appears on hover.** Timestamps, copy, and revert controls are `opacity-0` and fade in
   over 200 ms on row hover or focus-within. At rest the transcript shows content only.
4. **Numbers are tabular.** Every timestamp, duration, and count uses tabular numerals so nothing
   shifts while a timer ticks.
5. **The composer is a surface, not a field.** Model, reasoning effort, and access mode live
   inside it as inline dropdown pills.
6. **Affordances are advertised, not discovered.** The composer placeholder names all three
   triggers verbatim.

### Geometry

| Property | Value | Source |
| --- | --- | --- |
| Conversation column | **768 px max, 16 px side gutters** | T3 Code `max-w-3xl` / accepted reference / `CONVERSATION_CONTENT_MAX_WIDTH` |
| Minimap gutter | 48 px persistent | `TIMELINE_MINIMAP_PERSISTENT_GUTTER` |
| Minimap item spacing | 8 px, minimum 2 items, height `min(natural, 100vh - 18rem)` | `MessagesTimeline.logic.ts` |
| Follow re-arm band | **40 px** above the true content bottom | `TIMELINE_FOLLOW_REARM_THRESHOLD_PX` |
| Visible work entries | **1**, remainder behind a toggle row | `MAX_VISIBLE_WORK_LOG_ENTRIES` |

**Column width matches T3 Code.** T3 Code uses 768 px (`max-w-3xl`, rendered as
`min(48rem, 100% - 2rem)`). DevManager uses the same **768 px** measure through
`CONVERSATION_CONTENT_MAX_WIDTH` in `src/ui/task_cockpit/timeline.rs`, as confirmed by the accepted
full-shell visual reference.

Everything downstream of the measure follows it. Any surface T3 Code sizes to its conversation
column — floating banners, the load-earlier header — uses **768 px** here, so the canvas stays
optically aligned. Per-row proportions that are expressed as fractions rather than pixels, notably
the user bubble's 80 % cap, are unchanged and simply resolve wider.

The 40 px follow band is not cosmetic. Their own comment records why: a half-viewport
"near end" test re-armed live-follow while the user was reading history and yanked them back down
on the next stream chunk. DevManager's `at_bottom` / `follow_latest` logic in `timeline.rs` must
adopt a comparable pixel band rather than an exact-bottom epsilon.

### Row treatments

**User message.** Outer `flex flex-col items-end gap-1` (4 px). Bubble: `max-w-[80%]`,
**16 px radius**, **12 px padding**, background `--message-surface` (which resolves to `--accent`,
not `--secondary`), foreground `--message-foreground` (plain `--foreground`). Long messages
collapse behind a "show more" affordance. Attached images render as a two-column grid, `max-w-420 px`,
8 px gap, each `8 px radius` with a `--border/80` hairline and `max-h-220 px` cover-fit.
Meta row sits **below** the bubble, right-aligned, `text-xs tabular-nums`, hidden until hover, and
carries timestamp, revert-agent-work, and copy.

**Assistant message.** `px-1 py-0.5` and nothing else — 4 px horizontal, 2 px vertical. No
surface, no border, no marker. Markdown renders at full column width. A changed-files summary
attaches directly beneath. Meta row is `mt-1.5`, `text-xs tabular-nums`, hidden until hover,
carrying copy and timestamp; the copy control is suppressed entirely while streaming.

**Working indicator.** `py-0.5 pl-1.5`; inner row `gap-2`, **11 px** type, `--secondary-label`.
Three **4 px** dots, `--muted-foreground/30`, pulsing on a shared animation staggered
**0 / 200 / 400 ms**. Label reads `Working for 12s`, followed by an optional
`· <step label>` in `--muted-foreground/55`, truncated. The elapsed timer **mutates its own text
node on a 1 s interval** rather than re-rendering — a detail worth preserving in GPUI, since a
per-second repaint of the transcript while tokens stream is exactly the jank this avoids.

**Work / tool entry.** `rounded-md px-0.5 py-0.5` with a colour transition. Expandable entries get
`cursor-pointer`, a `--accent/20` hover wash, and an **inset** focus ring. Icon is 20 px with a
6 px gap. Heading is `font-medium` on `--foreground`; a runtime warning switches it to
`--warning`, a runtime error or non-tool failure to `--destructive`. Display text is
`heading - preview`, and the preview is dropped when it normalises equal to the heading, so
nothing reads `Read file - Read file`.

**Turn fold.** A `--border/60` hairline underneath, `pb-2 pt-1`. The control is a text button:
`gap-1 rounded-md px-1 text-xs`, `--muted-foreground` rising to `--foreground` on hover, with a
14 px chevron that rotates between collapsed and expanded.

### Composer

A **22 px radius** outer wrapper with 1 px of padding, containing a **20 px radius** inner
surface — the padding ring is how the hairline is drawn, so the border is a surface, not a stroke.
A pending/banner region sits above the input with a **19 px** top radius, a `--border/65` bottom
divider, and a `--muted/20` wash.

Placeholder text, verbatim:

> `Ask anything, @tag files/folders, $use skills, or / for commands`

with `Ask anything...` as the reduced form and `Enable a provider in Settings` when no provider is
available. This single string is what teaches all three triggers from section 4, and DevManager
should carry the same sentence rather than inventing one.

The bottom control row holds inline dropdown pills — model, reasoning effort and context window,
access mode — left-aligned, with circular actions on the right. The primary send control is a
**32 px circle** filled with `--message-action` (which resolves to `--primary`), dropping to
**30 % opacity** when disabled, and swapping to a stop control while a turn runs. Attachment
thumbnails are **64 x 64** with an 8 px radius and a `--border/80` hairline. A footer strip beneath
the composer carries the checkout location on the left and the branch on the right.

### Colour system

Semantic tokens only; no literal colours in components. The relevant ones and their light-theme
resolutions are `--background: zinc-25`, `--foreground: zinc-800`, `--card/--popover: white`,
`--muted / --secondary: zinc-50`, `--muted-foreground: zinc-500`, `--accent: zinc-100`,
`--border: zinc-200`, `--input: zinc-300`, `--primary: oklch(0.488 0.217 264)`,
`--error: red-500`, `--success: emerald-500`, `--warning: amber-500`, `--info: blue-500`, with
`--error-surface` and `--warning-surface` as 8 % mixes toward transparent.

DevManager's `src/ui/tokens.rs` already carries a token system with the required semantic slots.
This design does not introduce a second one — the conversation renderers consume the existing
`ThemeTokens`, extended where a slot is genuinely missing (notably a dedicated message-surface and
message-action pair, which today has no equivalent).

### Label typography signature

One recurring treatment identifies every secondary label in the product, and reproducing it is a
large part of reproducing the look. Section headings, question prompts, and approval headers all
use **small, uppercase, wide-tracked, semibold** type on `--secondary-label`:

| Context | Size | Tracking | Weight |
| --- | --- | --- | --- |
| Popover section header | 10 px | 0.08em | semibold |
| Pending-question label | 11 px | widest | semibold |
| Approval panel header | 14 px | 0.2em | (uppercase) |

Everything numeric alongside them is `tabular-nums`.

### Shared popover recipe

The slash/mention menu and the prompt-stash menu use one recipe, and any new popover should reuse
it rather than inventing a second:

- **`dropdown-glass`** — background `color-mix(in srgb, var(--popover) 80%, transparent)` over
  `backdrop-filter: blur(12px) saturate(1.14)`, becoming `blur(16px) saturate(1.08)` in dark. A
  `@supports not` fallback replaces the blur with an opaque surface where backdrop-filter is
  unavailable.
- **20 px radius**, `overflow-hidden`, full width of the composer.
- Shadow `0 16px 40px -18px rgb(0 0 0 / 55%)`, deepening to `0 18px 44px -18px rgb(0 0 0 / 80%)`
  in dark. Note this is a **large, very soft, downward-offset** shadow with a strong negative
  spread — not a tight drop shadow.
- Maximum height 288 px, 12 px vertical padding only when non-empty.
- Rows: 16 px icon on `--icon-muted`, 8 px gap, truncating description at 12 px on
  `--secondary-label`, trailing shortcut hint pinned right.

### Interactive surfaces

**Pending question panel.** Each choice is a full-width button, `gap-3`, 6 px radius, 10 px/6 px
padding, `hover:bg-muted/40`, and a **1 px** focus ring at `--primary/25`. The prompt label uses
the uppercase signature above and brightens to `--foreground` on row hover. Keyboard hints render
as 20 px-tall chips: 6 px radius, `--muted/60`, 10 px medium tabular type. Body copy is 14 px at
`--foreground/90`.

**Pending approval panel.** Padding 16 px/14 px, rising to 20 px/16 px at the small breakpoint.
The command under review renders in a monospace block, 12 px, relaxed leading, capped at 160 px
with its own scroll and `overflow-wrap: anywhere` so a long path cannot widen the composer. A
detail box sits beneath at 8 px radius with a `--border/65` hairline on `--background/70`. Actions
are a wrapping 8 px-gap row.

**Proposed plan card.** **24 px radius**, `--border/80` hairline, `--card/70` surface, 16 px
padding (20 px at the small breakpoint). When the plan overflows, a **96 px** bottom gradient
fades it out — `--card/95` to `--card/80` to transparent — with pointer events disabled, and the
expand control sits centred below.

**Changed-files section.** 16 px radius, `--border/70` on `--secondary`; in dark the border goes
transparent and the surface becomes `--input/32`. File rows are 12 px radius with `--accent/60`
hover. Paths render monospace at 11 px on `--muted-foreground/80`, brightening on hover; per-file
stats are monospace 10 px tabular, pinned right. The tree is a **container query** — the inline
path preview is hidden below a 384 px container width rather than at a viewport breakpoint.

**Work group toggle.** Full-width button, 6 px radius, 2 px padding, 12 px type on 20 px leading,
`--accent/20` hover, inset focus ring. A 14 px chevron at 70 % opacity sits in a 20 px slot and
rotates 180 degrees over 200 ms on expand. The copy is count- and kind-aware:
`+3 previous tool calls`, `+1 previous log entry`, and `Show fewer tool calls` when open.

**Load-earlier header.** Full width, 6 px vertical padding, 12 px type at
`--muted-foreground/60` rising to `--foreground` on hover, disabled state drops the pointer
cursor.

**Banners.** Thread errors and provider status float centred above the transcript at the same
768 px measure, sized to their content (`w-fit`), 12 px top padding, with a 24 px dismiss control
inset 8 px from the top-right. Body copy clamps to three lines; the full text is available in a
tooltip capped at 384 px with preserved whitespace.

**Context-window meter.** A 20 px circular gauge drawn as a rotated SVG ring, animating
`stroke-dashoffset` and `stroke` over 500 ms with an ease-out curve and honouring reduced-motion.
Its popover uses a 6 px linear track on `--muted/60` with the same 500 ms transition, 11 px
tabular labels, and `--floating-content-inset` (12 px) padding.

**Empty state.** A centred headline, 24 px rising to 30 px at the small breakpoint, **normal**
weight with tight tracking on `--foreground`. Not bold, not a hero graphic — one quiet line.

**Timeline minimap.** A hover-scrub rail in the left gutter. Pointer position maps continuously to
an item index, a tooltip previews that item's text, and clicking scrolls to it. The hit strip
starts 12 px from the left at up to 40 px wide, expanding to 22 rem while active, and the tooltip
anchors start/centre/end depending on whether the active item is first, last, or in between.

### Composer glass shell

The composer is not a rectangle with a border. It is a single continuous glass layer:

- A `::before` pseudo-surface fills the shell at 22 px radius, painting
  `color-mix(in srgb, var(--chat-composer-glass-surface) 80%, transparent)` under the same blur
  and saturation as the popover recipe. `--chat-composer-glass-surface` is itself a mix —
  `color-mix(in srgb, var(--background) 96%, white)` — so the composer reads a shade above the
  canvas without being a different colour.
- When context is attached, the shell and the strip beneath it become **one shape**. A
  `clip-path: shape()` joins the composer's 22 px top corners to a 16 px-radius strip inset
  1.375 rem per side, with 9.85 px bezier control points, extended by a 2.25 rem
  `--chat-composer-context-extension`. The `Local checkout / master` bar visible under the
  composer is not a separate bar — it is the same glass surface, necked in.
- A 5 %-white outline and a 3 %-white inner highlight sit over it in dark.

**This is the one part of the target that GPUI cannot reproduce literally**, and it is called out
under divergences below rather than left to be discovered during implementation.

### Adjacent surfaces

DevManager already owns equivalents of these, so they are recorded as calibration targets rather
than as things to rebuild. Where DevManager's existing surface disagrees, the difference should be
a decision rather than an accident.

**Task sidebar.** Task rows are **78 px** tall (4.875 rem), inset by `--sidebar-row-content-inset`
(10 px) horizontally and `--sidebar-content-inset` (8 px) vertically, with a 6 px radius and
`overflow-hidden`. Three states are distinct surfaces, not opacity changes:
`--sidebar-row-hover`, `--sidebar-row-active`, `--sidebar-row-selected`. Navigation rows such as
Search and Settings are **36 px** with a 10 px gap; group headers are **32 px**, 14 px medium.
Separators are a 1 px `--sidebar-border/60` rule inset 10 px with 6 px of margin. The empty state
is centred, 12 px, `--muted-foreground/60`.

Rows carry `content-visibility: auto` with a 96 px intrinsic-size hint — the browser equivalent of
row virtualization. DevManager already virtualizes via `DEFAULT_OVERSCAN` and `MAX_PAINTED_ITEMS`
in `timeline.rs`, so this needs no port; it is noted so the sidebar's own list is not assumed to
be cheap.

**Thread status indicators.** A single-line strip capped at `min(34rem, 100vw - 2rem)`, segments
divided by a **left border** (`--border/70`, minimum 16 px tall) rather than by a dot or a pipe
character. Leading segments truncate with 8 px of left padding; trailing segments stay at natural
width with 8 px right padding and medium weight. Icons are 12 px at `--muted-foreground/40` and
`/60`.

**Chat header.** The thread title is an inline button that reveals a 14 px edit affordance on
hover or keyboard focus. Renaming happens in place: the input has **no border**, only
`ring-1 ring-ring/50` tightening to `ring-ring` on focus, on a transparent background. The
breadcrumb is `--muted-foreground` rising to `--foreground` on hover. The action row is a
container query, so controls collapse on the header's own width rather than the window's.

**Model picker.** The search field is **underline-only** — `border-b border-border/70 pb-2.5`
transitioning to `border-ring` on focus-within — not a boxed input. Rows are 6 px radius with 8 px
padding; the model name is 12 px medium on snug leading, the description 12 px normal at
`--muted-foreground/70`. Capability badges are 16 px tall, 2 px radius, 10 px type. A
"new" marker uses the update palette: `border-update/35` on `bg-update/15`, 10 px bold uppercase
with wide tracking on `--update-foreground`.

**Right panel and tabs.** The panel is `42vw`, clamped between **360 px and 560 px**, separated by
a single left border. Its tab strip matches `--workspace-topbar-height` (52 px). Tabs are **24 px**
tall, capped at 144 px, 6 px radius, 12 px type; active is `--accent`, inactive is
`--muted-foreground` with an `--accent/60` hover. An unread marker is a **6 px** dot pinned to the
tab's bottom-right corner in the tab's own colour.

**Terminal drawer.** The tab strip is **22 px** tall with a `--border/70` bottom rule. The session
list is a fixed **144 px** column, `--border/70` on `--muted/10`. The terminal viewport itself has
a **4 px** radius on `--background` — noticeably tighter than every other radius in the product,
which is deliberate: a terminal reads as a device, not a card. Floating controls sit in a
6 px-radius capsule with a `--border/80` hairline and an extra-small shadow, with control groups
divided by a 1 px `--border/60` rule.

**Application empty state.** A 512 px column with 32 px horizontal and 48 px vertical padding.
Title at 20 px on `--foreground`, body at 14 px on `--muted-foreground/78`, 8 px apart. Quiet, not
promotional — the same restraint as the composer's draft headline.

### Deliberate divergences

Three things are **not** copied literally:
- **Terminal-context inline placeholder.** T3 Code represents inline terminal context with a
  `U+FFFC` OBJECT REPLACEMENT CHARACTER embedded in the prompt string. DevManager attaches terminal
  context through a different mechanism, and section 4 already defers that segment variant.
- **The composer's joined clip-path shell.** GPUI has no arbitrary path clipping, so the single
  continuous glass shape cannot be reproduced as authored. The approximation is two stacked
  surfaces sharing one background fill: the composer at 22 px radius with square bottom corners
  when context is attached, and the context strip directly beneath it at square top corners and
  16 px bottom corners, inset 22 px per side. This reads as one shape at rest and differs only
  where the necking curve would be. **This is an approximation and should be reviewed against a
  side-by-side window before it is accepted.**
- **Backdrop blur.** The glass recipe depends on `backdrop-filter`. Where GPUI cannot blur what is
  behind a surface, use the same `@supports not` fallback T3 Code already ships — an opaque
  surface at the same resolved colour — rather than approximating blur with opacity, which reads
  as washed-out rather than glassy.

One caution for anyone reading the upstream source: `ChatComposer.tsx` contains six raw NUL bytes,
so `grep` reports it as a binary file and refuses to search it. Read it through a tool that
tolerates the bytes rather than concluding the file is missing or corrupt.

## Relationship to the 2026-08-18 spec

`docs/superpowers/specs/2026-08-18-native-conversation-first-redesign.md` remains the source for
image attachment rules, dock collapse and layout migration, slash-command overlay behaviour, and
accessibility identities. Its visual sections are superseded by "Target UX" above wherever the two
disagree. The remaining material disagreement is row treatment:

- **Row treatment.** The earlier spec describes assistant turns as "document-like blocks with a
  small provider/assistant marker". The target has **no marker at all**.

One of its acceptance criteria is subsumed: criterion 1 requires that `session_state` and
`session_stale` no longer appear as message rows. Under this design that is structural rather than
stylistic — those kinds map to `Unknown`, which has no conversation renderer — so the criterion is
satisfied by construction and needs no denylist entry.

## Acceptance criteria

1. `src/ui/conversation/rows.rs` derives a closed `ConversationRow` set with no `gpui` import and
   is fully unit-tested without a window.
2. An event kind absent from the closed conversation vocabulary maps to `Unknown` and produces no
   conversation row. A test asserts this with a synthetic unrecognised kind.
3. `is_hidden_conversation_chrome` and the conversation path's use of
   `TimelineItemContent::Generic` no longer exist.
4. Row identity is stable across streaming assistant deltas; a test asserts unchanged row keys
   while a message's text grows.
5. Turn folding, activity toggling, and elapsed-time attribution behave per the translated test
   cases.
6. The Phase 1 conformance test proves `rows.rs` and `timelineModel.ts` agree over the shared
   fixture corpus, and asserts a non-zero executed-case count so a skipped run cannot read as a
   pass.
7. `src/ui/composer/segments.rs` round-trips cursors between collapsed and expanded forms, and a
   mention deletes and traverses as one unit.
8. `detect_trigger` recognises `@`, `/`, and `$` with correct replace ranges.
9. No `include_str!` in `src/` reads any file under `web/src`. The TypeScript catalog module is a
   generated build artifact, and a test asserts a non-zero command count per supported provider.
10. The conversation canvas draws **no border on any transcript row**. The only hairline is the
    one beneath a turn fold. A test asserts no row renderer emits a border.
11. A user message renders as a right-aligned 16 px-radius surface at most 80 % of the column
    wide; an assistant message renders with no surface, no border, and no role marker.
12. Timestamp, copy, and revert controls are not painted at rest and appear on row hover or
    keyboard focus.
13. The conversation column measures 768 px maximum with 16 px gutters, and the composer,
    floating banners, and load-earlier header all share
    that measure.
14. The composer placeholder is the verbatim string in "Target UX", and `@`, `$`, and `/` each
    open their menu from it.
15. Live-follow re-arms within a 40 px band above the content bottom, not at an exact-bottom
    epsilon; scrolling up to read history does not snap back on the next streamed chunk.
16. The elapsed-time label updates without repainting the transcript.
17. Secondary labels use the uppercase/wide-tracked signature at the sizes tabled in "Target UX",
    and every numeric run uses tabular figures.
18. The slash menu and any later popover share one recipe — 20 px radius, the glass surface, the
    soft `-18px`-spread shadow, and the 10 px uppercase section header — rather than each
    defining its own.
19. Question, approval, plan, and changed-files surfaces match their tabled radii, insets, and
    hover/focus treatments, and the approval command block scrolls internally rather than
    widening the composer.
20. The work-group toggle copy is count- and kind-aware (`+1 previous log entry` versus
    `+3 previous tool calls`), and its chevron rotates rather than swapping glyph.
21. The composer and its context strip read as one surface; the approximation named under
    "Deliberate divergences" is reviewed side by side and explicitly accepted or revised.
22. Each surface under "Adjacent surfaces" is compared against DevManager's existing equivalent,
    and every difference is recorded as an accepted decision or a follow-up item. Silent
    divergence is not an outcome; neither is rebuilding a surface this design says to leave alone.
23. Every invariant listed under "Invariants preserved" is covered by an existing or new test.
24. Focused tests, the isolated `cargo check --locked --lib --bins --tests` gate, the full library
    suite, and a live watch-mode visual pass against a side-by-side T3 Code window all succeed.

## Implementation order

This design is deliberately larger than one implementation plan. It is stated whole because the
parts share one root cause, but it lands as four trains, each independently verifiable and each
leaving the product working:

1. **Transcript derivation.** Sections 1 and 2 — restore the discriminant, add
   `src/ui/conversation/rows.rs`, delete `is_hidden_conversation_chrome` and the conversation
   path's use of `TimelineItemContent::Generic`. Plus the Phase 1 conformance test from section 3.
   This makes the correct rows exist.
2. **Target UX.** The "Target UX" section — row treatments, geometry, hover-revealed chrome,
   composer surface, follow band. This is the train the user actually sees, and it is the reason
   the work is being done; trains 3 and 4 can slip without affecting it, and it must not be
   deferred as polish. It depends only on train 1.
3. **Catalog ownership.** Section 5 — Rust const table, `build.rs` generation, both scrape sites
   and both hand-written parsers deleted. Independent of everything else and the smallest; it can
   land at any point, and it closes a live silent-failure path.
4. **Composer derivation.** Section 4 — `segments.rs` and `trigger.rs`, inline-token cursor model,
   `@` mentions. Needed before the composer placeholder's promise of `@tag files/folders` and
   `$use skills` is honest, so train 2 ships the placeholder only for triggers that work.
5. **PWA cutover and file reduction.** Section 3 Phase 2 and section 6 — host emits derived rows,
   `timelineModel.ts` deleted, `timeline.rs` and `composer.rs` reduced to painting.

Trains 1 and 2 together are the deliverable that answers the original complaint. The remaining
three are correctness and consolidation and can be scheduled independently.

## Open questions

None.
