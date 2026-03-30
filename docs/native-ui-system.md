# Native UI System

This app stays native Rust because the terminal experience is non-negotiable. That means the UI has to get easier through a stronger internal design system, not by moving back to a web stack.

## Reference projects

- `gpui-component`
  - https://github.com/longbridge/gpui-component/releases
- `scopeclient/components`
  - https://github.com/scopeclient/components

These are reference points for architecture and interaction patterns. Do not vendor code from external projects without a license review.

## Rules

1. Build semantic components, not raw `div()` trees.
   - Buttons, action rows, switches, field blocks, notices, cards, and section headers should live in reusable modules.

2. Keep visual tokens centralized.
   - Backgrounds, borders, accents, status colors, and editor-specific surfaces belong in `src/theme/mod.rs`.

3. Every editor gets the same shell.
   - Toolbar
   - Context card
   - Section cards
   - Shared actions
   - Shared field states

4. Separate field types by intent.
   - Editable text fields
   - Multi-line text fields
   - Action rows
   - Toggle fields
   - Read-only detected info
   - Selectable scan rows

5. Prefer metadata-first forms.
   - The top of each editor should answer: what is this, where does it live, and what matters here.

6. Write copy for humans, not config files.
   - Labels should describe the real concept.
   - Hints should explain why a field exists.
   - Status labels should use words like `Detected`, `Saved`, `Hidden`, `Visible`, `Manual`, `Auto`.

7. Avoid one-off styling in feature code.
   - New forms should compose shared editor components first.
   - New custom styling should only happen when the design system is missing a primitive.

8. Prefer safe iteration surfaces.
   - If a screen is complex, add a preview or sample-data rendering path before adding more ad-hoc UI.

## Current implementation direction

- Shared editor primitives live in `src/workspace/editor_ui.rs`.
- Editor metadata stays on `EditorPanel` in `src/workspace/mod.rs`.
- Editor forms should only describe content and behavior, not low-level styling.

## Next improvements

- Move the add-project wizard onto the same editor primitives.
- Bring settings onto the same card and field system.
- Add a dedicated UI preview/debug surface with seeded editor states.
- Replace remaining ad-hoc sidebar and dialog rows with semantic components.

