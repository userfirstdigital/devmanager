# Codex Web Composer Delivery Fix Design

## Problem

The native web composer acknowledges a Codex submission and immediately publishes the user's native transcript bubble, but the prompt can remain in Codex's multiline terminal editor instead of reaching the model. The existing recovery runs only for the first composer submission, so one command can appear to work while every later prompt silently accumulates in the editor. Codex `/status` has a second visibility problem: it renders its result only inside the provider terminal, while the native interface currently keeps that command inline.

Manual reproduction confirmed the boundary failure. Writing a prompt followed by a raw carriage return leaves it in the Codex editor. Writing Escape, waiting briefly, then writing carriage return submits it. The same sequence submits ordinary prompts and opens Codex's real `/model` and `/status` screens. Hot-load acceptance then exposed the equivalent Claude state leak: returning from a provider screen changed only the web view, so the next command could be discarded by the still-open provider screen. Escape then carriage return safely submitted both ordinary Claude prompts and slash commands.

## Goals

- Every Codex native-web submission reaches the provider, including consecutive submissions in one session.
- Claude and Codex submissions recover from provider-owned editor and full-screen states.
- Server, SSH, and shell terminal behavior remains unchanged.
- Exact Codex `/status` submissions automatically open the provider view so the terminal-only result is visible.
- The native composer continues to acknowledge only successful PTY writes and needs no retry or resume button.
- Automated and manual hot-reload tests cover the real provider behavior before merging.

## Design

### Provider-specific submit sequence

`execute_web_composer_batch` already owns prompt construction, attachment staging, writer-lease revalidation, and the final PTY writes. It will also own the provider-specific submit sequence:

- Claude and Codex: write Escape, wait 120 ms, write prompt, wait 50 ms, write Escape, wait 120 ms, write carriage return.
- Other sessions: write prompt, wait 50 ms, write carriage return.
- Draft-only writes without a trailing carriage return still write text without submitting.

The preflight Escape exits a lingering provider-owned screen before the new prompt is written. The second Escape exits Codex's multiline editing state or dismisses Claude autocomplete created by the new text; the separate carriage return then triggers the TUI submit action. Applying the sequence to every AI submission makes returning to native mode safe without adding a visible resume or reset action.

### Remove obsolete lifecycle recovery

The delayed one-shot recovery and its `initial_composer_escape_claimed` lifecycle state will be removed. Keeping both mechanisms would add a second delayed Escape/Enter after a legitimate first request and could interfere with a provider response or menu.

### Terminal-only command handoff

Codex `/status` will be classified as `providerMenu`. The native composer already waits for the matching command acknowledgement before switching to the provider view, so the user sees the actual status pane without racing the PTY write. Commands that already produce native semantic output remain inline; this change is limited to the terminal-only command proven during reproduction.

## Error Handling

Attachment rollback and writer-lease validation remain unchanged. Any failed prompt, Escape, or carriage-return write returns the existing composer error, so the web client does not treat a partially delivered batch as successful. No timeout-based retry is introduced because retries can duplicate commands after a slow provider response.

## Verification

Automated tests will prove:

- Codex submissions produce Escape, prompt, Escape, carriage return on every call.
- Claude submissions produce Escape, prompt, Escape, carriage return.
- Draft-only Codex writes do not submit.
- Codex `/status` is a provider interaction and triggers handoff only after acknowledgement.
- Existing attachment rollback and authority checks still pass.

Manual hot-reload acceptance will use a phone-sized browser and a real Codex session to submit two consecutive marker prompts, open `/model`, return to native, and open `/status`. A Claude prompt and `/model` interaction will confirm its unchanged path. Full Rust and web test/build gates will run before merging to `master`.
