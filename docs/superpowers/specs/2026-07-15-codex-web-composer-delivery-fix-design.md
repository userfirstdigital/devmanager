# Codex Web Composer Delivery Fix Design

## Problem

The native web composer acknowledges a Codex submission and immediately publishes the user's native transcript bubble, but the prompt can remain in Codex's multiline terminal editor instead of reaching the model. The existing recovery runs only for the first composer submission, so one command can appear to work while every later prompt silently accumulates in the editor. Codex `/status` has a second visibility problem: it renders its result only inside the provider terminal, while the native interface currently keeps that command inline.

Manual reproduction confirmed the boundary failure. Writing a prompt followed by a raw carriage return leaves it in the Codex editor. Writing Escape, waiting briefly, then writing carriage return submits it. Hot-load acceptance then exposed two additional provider differences: Claude clears an ordinary composed prompt if Escape is sent after the text, and the providers handle slash autocomplete differently. Codex Tab accepts a command while Claude Tab can expand argument placeholders; Escape leaves ambiguous commands such as `/status` pending. In both TUIs, one harmless trailing space closes autocomplete and is ignored when Enter executes the command.

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

- Codex: write one preflight Escape and wait 180 ms.
- Claude: when the user returns from a known provider interaction, the web session screen sends one Escape at that boundary; ordinary composer delivery adds no speculative preflight key.
- Then write ordinary prompts as one PTY write or type the leading slash-command token at 100 ms per character.
- Claude and Codex slash commands: wait 250 ms for autocomplete, write one trailing space, wait 500 ms for the queued PTY write to reach the provider, then write carriage return. Exact Claude commands without arguments write a second carriage return because the first accepts the queued autocomplete entry; commands with arguments submit once.
- Ordinary Codex prompts: wait 50 ms, write Escape, wait 120 ms, then write carriage return.
- Ordinary Claude prompts: wait 50 ms, then write carriage return without a second Escape.
- Other sessions: write prompt, wait 50 ms, write carriage return.
- Draft-only writes without a trailing carriage return still write text without submitting.

The Codex preflight Escape exits a lingering provider-owned screen before the new prompt is written. Claude is different: an extra Escape at a fresh composer opens its rewind UI, so the web screen closes a known provider interaction exactly when the user returns to native mode. Raw input and composer mutations share the ordered writer lane, ensuring that boundary Escape reaches the PTY before any immediately submitted prompt. The short slash-command token is typed character by character so provider paste debouncing cannot delay it; any arguments still use a normal bulk write. Both providers close autocomplete when a trailing separator arrives and ignore that whitespace when executing. A conservative 500 ms settle accounts for the in-process ConPTY queue before Enter. Claude's queued exact commands consume the first Enter to accept autocomplete, so a second Enter executes them; explicit arguments already dismiss autocomplete and receive one Enter. Codex retains its post-text Escape for multiline editing; Claude omits it so the prompt is not cleared.

### Remove obsolete lifecycle recovery

The delayed one-shot recovery and its `initial_composer_escape_claimed` lifecycle state will be removed. Keeping both mechanisms would add a second delayed Escape/Enter after a legitimate first request and could interfere with a provider response or menu.

### Terminal-only command handoff

Codex `/status` will be classified as `providerMenu`. The native composer already waits for the matching command acknowledgement before switching to the provider view, so the user sees the actual status pane without racing the PTY write. Commands that already produce native semantic output remain inline; this change is limited to the terminal-only command proven during reproduction.

## Error Handling

Attachment rollback and writer-lease validation remain unchanged. Any failed prompt, Escape, or carriage-return write returns the existing composer error, so the web client does not treat a partially delivered batch as successful. No timeout-based retry is introduced because retries can duplicate commands after a slow provider response.

## Verification

Automated tests will prove:

- Ordinary Codex submissions produce Escape, prompt, Escape, carriage return on every call.
- Ordinary Claude submissions produce prompt and carriage return without clearing the prompt or opening rewind.
- Returning from a known Claude provider interaction sends one Escape before native mode resumes.
- Claude and Codex unique, prefix-colliding, and argument-bearing slash commands close autocomplete with a harmless trailing space before carriage return.
- Draft-only Codex writes do not submit.
- Codex `/status` is a provider interaction and triggers handoff only after acknowledgement.
- Existing attachment rollback and authority checks still pass.

Manual hot-reload acceptance will use a phone-sized browser and real Codex and Claude sessions. It will submit ordinary prompts, open `/model`, return to native while that provider screen remains open, and open `/status`. Full Rust and web test/build gates will run before merging to `master`.
