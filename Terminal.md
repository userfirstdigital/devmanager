# Terminal Decisions

This file is the source of truth for DevManager terminal parity decisions.

Use Zed as a reference for terminal behavior, but do not treat Zed as something we must copy feature-for-feature. Prefer the parts that improve a mouse-first, English-language workflow and skip power-user terminal modes that add complexity without real value for this app.

## Port And Maintain

These are the Zed-inspired terminal features we want in DevManager and want to keep.

### Clipboard And Paste

- Image clipboard escape hatch: if the clipboard contains an image, forward raw `Ctrl+V` into the PTY so terminal tools can pull the image from the OS clipboard themselves.
- Hardened bracketed paste: strip embedded escape bytes before wrapping the paste payload.
- Terminal clipboard protocol bridge: support terminal clipboard load/store with a bounded system clipboard bridge instead of ignoring those events.

### Terminal Settings

- `option_as_meta`
- `copy_on_select`
- `keep_selection_on_copy`

These settings should remain exposed in the app settings UI and respected by terminal behavior.

### Key And Input Behavior

- Keep the `SendText` vs `SendKeystroke` split for terminal input routing.
- Keep `Ctrl+Enter -> \n`.
- Keep `Shift+Enter -> \n`.
- Keep selection-aware copy behavior so `Ctrl+C` copies when text is selected and still behaves like a normal terminal interrupt when nothing is selected.
- Keep option/meta behavior aligned with Zed's current platform behavior.

### Selection And Mouse UX

- Support `copy_on_select`.
- Support `keep_selection_on_copy`.
- Keep terminal behavior optimized for mouse selection and simple clipboard flows first.

## Permanently Skipped

These are terminal features we do not want to keep reopening unless requirements change significantly.

### Vi Mode

- Skip terminal-local vi mode permanently.
- Skip vim-style scrollback navigation, vim-style selection mode, and vi-mode indicators.

Reason: the intended workflow here is mouse-first, and modal keyboard navigation does not match how this app is being used.

### Extra Power-User Keymap Chasing

- Do not keep expanding terminal parity just to mirror more of Zed's keyboard-heavy terminal shortcuts.
- Do not add more modal or shortcut-dense terminal behavior unless there is a concrete workflow problem to solve.

Reason: the goal is practical terminal UX, not exhaustive Zed keymap parity.

### Additional IME-Specific Investment

- Skip IME/composition work for now.
- Do not add or re-add IME/composition plumbing unless multilingual input becomes a real requirement later.

Reason: this app is currently being used only for English input, so extra IME-focused work is not a product priority.

## Future Default

When future terminal discussions come up, assume this by default:

- Keep improving paste, clipboard, mouse selection, and simple terminal ergonomics.
- Keep the Zed-inspired clipboard and binding behavior that is already implemented.
- Do not propose vi mode again unless explicitly requested.
- Do not propose IME/composition work unless input requirements change.
