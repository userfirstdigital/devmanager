# Native UI System

This is DevManager’s maintained design-language and visual-delivery guide. Use it with the current feature agreement and [native UI architecture rules](../src/ui/AGENTS.md). New capabilities must feel like part of the same product and make their main task easy to complete. A UI-bearing feature is finished only when its built appearance matches its recorded visual target and its required interactions work.

The application and terminal stay on GPUI. The existing component library, semantic tokens, and canonical native preview are the foundation. This guide records the delivery contract; it does not claim that every screen has migrated or that automated visual acceptance already exists for every state.

## One maintained vocabulary

| Concern | Canonical home and use |
| --- | --- |
| Product controls | [ui/components](../src/ui/components/mod.rs) wraps the pinned `gpui-component` primitives. Extend the shared vocabulary before assembling another screen-specific control. |
| Visual meaning and layout metrics | [ui/tokens.rs](../src/ui/tokens.rs) defines semantic theme, typography, density, scale, and interaction contracts. Choose roles rather than copying palette values or inventing a spacing scale. |
| Theme definitions and selection | [ui/theme_system.rs](../src/ui/theme_system.rs) owns the theme library and its mapping to product roles. Preserve theme preferences and keep new controls compatible with the supported appearances. |
| Older theme imports | [theme/mod.rs](../src/theme/mod.rs) re-exports the token module; it is not a second palette home. |
| Existing editor forms | [workspace/editor_ui.rs](../src/workspace/editor_ui.rs) remains the existing editor composition helper. Reconcile touched forms with the shared control vocabulary instead of creating a parallel form kit. |
| Rendered examples | [Native preview](../src/ui/preview.rs), [capture](../src/ui/preview_capture.rs), and [UI fixtures](../tests/fixtures/ui) provide the existing place to exercise representative product states. |
| Appearance references | The [September 3 redesign](superpowers/specs/2026-09-03-ui-redesign-design.md) links its [reference images and source HTML](superpowers/specs/2026-09-03-ui-redesign-mockups). Resolve the applicable selection and later decisions for the current feature. Historical images and a CHOSEN label alone do not establish current approval. |
| New orchestration features | [UX-SPEC](../spec/ai-orchestration/UX-SPEC.md) and its [reference gallery](../spec/ai-orchestration/visuals/reference.html) define their native composition, actions/states, and complete capability-to-surface acceptance. These proposed targets guide implementation; they are not production captures. |

Initialize `gpui-component` through `ui::init` as required by the architecture guidance. Keep bespoke rendering for the terminal, transcript, recursive workspace, and task rail; use shared controls and tokens for their surrounding chrome. Integrate missing primitives incrementally with representative consumers. A new feature is not a reason to replace the GUI framework or restyle unrelated screens.

## Product language

- Lead with the user’s task. The goal and current result are prominent; workers, transport details, and internal identifiers are available in diagnostics. Use familiar product terms consistently across entry, progress, decisions, and completion.
- Give each task surface one visually dominant next action. Group secondary actions and separate destructive actions. Keep navigation and current selection predictable as panes resize, tasks change state, and overlays open.
- Establish hierarchy through the existing typography, spacing, alignment, and surface roles. Keep related information together, allow useful content to occupy the space, and preserve readable line lengths. Density is a supported preference, not arbitrary miniaturization of each new screen.
- Give status color a stable meaning and pair it with text or an icon. Working, idle, waiting for the user, failure, and verified completion must remain distinguishable. A decorative accent cannot imply success or a request for approval.
- Compose editors consistently: toolbar, concise metadata/context, grouped fields, and shared actions. Distinguish editable input, selections, toggles, actions, and detected/read-only information. Labels describe the real concept; help text explains consequences or why input is needed.
- Specify the whole interaction: initial/default state, next action, feedback, recovery, focus, and return path. Preserve drafts where required. Empty, loading, disabled, error, and partial-result states belong to the feature that needs them.

External projects contribute behavior or reusable implementation. Express that behavior using this vocabulary. Their sidebar layout, typography, palette, component library, and terminology do not become DevManager defaults merely because their code was reused. A new shared pattern needs a concrete feature reason, representative states, and reconciliation with its existing consumers.

Review the source license before copying or vendoring external component code and retain its required copyright and notice material.

## Establish the target before building

For each new screen, major visual change, or new interaction state, record the user journey and a concrete visual target before the implementation assignment starts. Keep these references in the existing feature/design artifacts; link them from the build instructions and test plan. A target identifies:

- The intended screen/state, persona, representative content, and the next action the user should understand.
- Its source and version, applicable design/tokens/components, selected reference region when an image contains alternatives, and who established the direction under what authority.
- The layout, typography hierarchy, spacing, color roles, key controls, and behavior that must be preserved, plus explicitly allowed differences.
- Representative window/pane geometry, display scale, theme/density, fonts/renderer, and state/data assumptions needed for comparison.

Reuse an applicable existing target for a localized change and record its intended difference. A copy fix does not need a new design exercise. For a new screen without a target, the lead prepares one from the project’s shared patterns and obtains independent design review within the current agreement. Prefer a rendered example using production components; a high-fidelity mockup can establish the composition where that is not yet possible. An early wireframe establishes flow and layout only; it cannot certify final visual finish.

Routine composition and technical choices belong to the lead. A requested brand change, unresolved product behavior, or material departure from the user’s selected reference remains a user decision. Record lead-derived targets honestly; do not call them user-approved. Finish target preparation during discovery or build-document generation so a coding phase receives settled visual decisions.

Maintain a small, runnable catalogue of shared components and their relevant states through the existing fixtures/preview path. Include production composition examples and links to actual component APIs so agents can find and reuse them. Fixture data may supply content; the renderer and controls must be the product’s. New patterns extend this catalogue with their owning component rather than creating a disconnected demo page.

## Build, capture, compare, repair

1. Implement the vertical slice using the shared components and the recorded target version. Keep its user/operator UI in the same slice as its behavior.
2. Capture the rebuilt product in the prescribed state. Record source/build, target, fixture/data, environment, window/pane geometry, scale, theme, and capture identity. Capture the complete canonical shell at representative reference geometry, plus focused details when useful.
3. Give a fresh reviewer the target, actual capture, required journey, and actual evidence. The reviewer names material differences in composition, hierarchy, spacing, typography, palette, controls, and states, and distinguishes allowed variation from a defect. Merely having the same widgets is insufficient.
4. Use image overlays/differences where the renderer and conditions make them meaningful. Set tolerances and tightly scoped dynamic regions before judging the result. Font rasterization, live timestamps, and permitted responsive behavior need explicit treatment; controls, status, text overflow, and the region under test cannot be masked away. A small global difference score can still hide a missing primary action.
5. Run the real interaction checks, record actionable visual/UX findings in the normal audit/fix list, repair them, recapture, and obtain fresh verification. Use the same cause-based finite recovery policy as other findings. An unresolved material mismatch prevents completion; an exhausted recovery allowance produces a precise blocker rather than an easier target.

A design target and a regression baseline have different jobs. The target expresses the intended result. A baseline is a capture of an implementation already checked against that target and its interaction requirements. The first capture of new code is a candidate baseline, never automatic approval. Keep prior versions, expected changes, and independent review when a baseline changes. Adjusting a mask, tolerance, fixture, or target to hide a failing result is a change to the check and follows the existing evidence/acceptance authority rules.

When a shared component, theme, font, or layout metric changes, exercise its affected representative consumers and invalidate dependent visual evidence. Pin the design/component versions assigned to concurrent work and reconcile intervening changes before landing. A local screenshot pass cannot certify the rest of the product after a shared change.

## Verify usability as well as appearance

Choose a bounded matrix from the actual changed surface and supported configurations. Cover the normal journey, empty/loading/error/partial states, long or localized content where supported, narrow panes and required scale/theme/density variants. Test keyboard traversal, visible focus, activation, dismissal and focus return, scrolling, text entry, contrast, accessible names, and the project’s required assistive-technology behavior. Keep critical controls reachable without clipping or overlap. Source-level token/accessibility tests are useful checks; they do not prove rendered behavior.

Verify status and available actions against actual host state, including reconnect, pending decisions, and completion. Use real browser/native interaction and the feature’s functional/performance checks. A static image cannot establish that sending input, answering a question, pausing work, or recovering a failure actually works.

## Existing verification tools and their limits

A debug build can open the canonical shell with bounded fixture data:

```text
devmanager --ui-preview-live tests/fixtures/ui/task-cockpit-two-panels.json
```

This uses native controls and keyboard bindings with an isolated development profile and no production host. Close the window to stop it. The existing Windows capture interface is:

```text
devmanager --ui-preview <fixture.json> --output <preview.png>
```

Use [Capture-UiPreviews.ps1](../scripts/native-next/Capture-UiPreviews.ps1) and the current [capture implementation](../src/ui/preview_capture.rs) where applicable; resolve supported arguments and the authorized platform lane before use. Follow the root [AGENTS.md](../AGENTS.md) for build isolation, capture geometry, process ownership, and live input acceptance. These commands were inspected for this guide, not executed as part of its update.

[ui_tokens](../tests/ui_tokens.rs), [ui_accessibility](../tests/ui_accessibility.rs), and [ui_preview_capture](../tests/ui_preview_capture.rs) exercise token, interaction-contract, and capture protections. [ui_quality_gates](../tests/ui_quality_gates.rs) deliberately keeps synthetic quality surfaces from claiming pixel acceptance; `VisualGate::RequiresCanonicalShell` is not a missing success flag to enable. Use canonical rendering and current evidence instead. Native fixture captures establish only the states they actually render; provider delivery, recovery, persistence, and live terminal input require their separate real acceptance.

## Completion evidence

Link the current target, actual captures, comparison findings and their independent disposition, interaction results, and affected shared-component checks from the feature’s test/audit artifacts. Record unavailable required capture or input tooling as NOT VERIFIED. Use existing coverage and precise scope for small changes; reserve full state matrices for the surfaces they affect. A UI-bearing slice cannot become complete while its required visual or interaction findings remain open, and visual work cannot be postponed to an optional final polish phase.
