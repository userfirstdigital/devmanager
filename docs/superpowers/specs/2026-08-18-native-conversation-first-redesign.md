# Native Conversation-First Redesign

## Outcome

DevManager's native task surface should feel like a modern AI development conversation rather than a host-debug projection. The conversation is the primary canvas; task navigation, provider activity, context tools, and the terminal remain available without visually competing with it.

## Product principles

- Preserve the existing Task, Agent, provider-session, action-epoch, and runtime-generation authority model.
- Change presentation and native composer input without deriving conversation state from the raw PTY.
- Keep the task inbox visible, collapse the right context dock by default, and retain the existing Dock toggle so Files, Changes, Browser, Review, and other tools remain one action away.
- Keep ordinary messages visually quiet and make questions, approvals, failures, active tools, plans, and subagents visibly distinct.
- Do not show internal session/turn lifecycle facts as transcript rows. They may contribute to a compact activity summary or connection state.
- Keep the composer mounted at the bottom of the conversation, centered to the same readable measure as the transcript.

## Conversation hierarchy

The transcript uses a centered content column with a maximum width of 860 logical pixels and comfortable vertical spacing.

- User turns are right-aligned, compact rounded bubbles with a subtle selected-surface background.
- Assistant turns are left-aligned document-like blocks with a small provider/assistant marker and no full-width row border.
- Reasoning summaries use quieter typography and a low-emphasis inset.
- Running tools and active plan/subagent state use compact status cards. Completed tool events collapse to one subdued line unless their summary communicates a failure.
- Questions and approvals use prominent bordered cards and keep their existing fenced answer/decision controls.
- Errors use the destructive palette and remain visible.
- Raw `session_state`, `turn_state`, usage, and unknown diagnostic extension rows do not appear as conversation messages.

The activity summary appears only when it has useful state to report: running shell/tool count, active subagents, or open goal steps. The raw semantic event count is not user-facing.

## Composer

The composer is a raised, rounded, shadowed surface floating above the bottom edge of the conversation. It shares the transcript's 860-pixel readable width.

- The input supports multiple wrapped lines; Enter submits and Shift+Enter inserts a newline.
- The bottom action row contains image attachment, provider-terminal toggle, contextual question/approval actions, and a compact primary send action.
- Slash-command search remains above the composer and uses the same width and rounded overlay treatment.
- Connection and validation errors stay immediately beneath the input without replacing the full app.
- Draft text remains task-and-agent scoped as it is today.

## Images and attachments

The native composer accepts PNG and JPEG images by Ctrl+V and by a non-blocking file picker. It supports at most eight images and at most 5 MiB per image, matching the established web-composer boundary.

Each pending image:

- is decoded before admission;
- is copied to the selected Task workspace's hidden `.devmanager/pasted-images` directory using the existing sanitized/TTL staging rules;
- is represented in the composer by a thumbnail chip with a removable action;
- contributes its `@relative/path` reference to the exact provider text only when the user submits;
- remains scoped to the current Task and primary Agent while unsent;
- is preserved on an immediate submission failure and removed from the composer only after the existing provider-input command settles successfully.

The durable provider input continues to contain bounded text, not image bytes. This keeps existing command/event persistence and provider identity rules intact. Removing an unsent image removes its staged copy when possible; old staged files remain bounded by the existing TTL cleanup.

## Layout and migration

The context dock is collapsed for new layouts. Existing version-1 native layouts migrate once to the new schema by preserving widths, window frame, selected task, sidebar state, and terminal state while setting the context dock to collapsed. The existing Dock control and keyboard shortcut reopen it and subsequent user choices persist normally.

The conversation panel uses low-chrome canvas styling rather than the same heavy card/header treatment as inspector panels. A Tools/Dock affordance remains visible while the dock is collapsed.

## Accessibility and interaction

- Preserve the existing AccessKit composer input and send identities.
- Add accessible names for attach, remove-image, tools/dock, and contextual actions.
- Image paste and file reads must never block the GPUI mouse/key handler.
- Focus returns to the composer after picking images or closing provider-terminal mode.
- Keyboard slash-command, answer, approval, and provider-terminal behavior remains intact.

## Acceptance criteria

1. The live task conversation no longer looks like bordered debug rows and no longer displays `session_state` or `session_stale` as message rows.
2. User, assistant, reasoning, tool, goal/subagent, question, approval, and error content have distinct visual hierarchy.
3. The transcript and composer share a centered readable width and the composer remains at the bottom.
4. Ctrl+V and file selection admit valid PNG/JPEG images, show removable previews, and send provider-readable staged references with the message.
5. Invalid type, oversize, decode failure, too many images, missing workspace binding, or staging failure is shown inline and does not lose the draft.
6. Slash commands, Shift+Enter, Enter-to-send, provider-terminal mode, AI questions, and approvals continue to work.
7. The right context dock starts collapsed after the one-time layout migration and reopens through the existing Dock control.
8. No full-screen connection gate is introduced, no provider session identity is synthesized, and no raw PTY transcript becomes semantic conversation truth.
9. Focused tests, the isolated Rust compile/full library gates, and a live watch-mode visual/interaction pass succeed.

