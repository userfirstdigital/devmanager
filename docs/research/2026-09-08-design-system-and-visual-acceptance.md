# Design system and visual acceptance — 2026-09-08

## Recommendation

Make visual and interaction quality part of the lead’s delivery contract. Reuse the target project’s design language, establish concrete reference states before implementation, build with shared components, capture the actual product, and require independent comparison and interaction checks before completion. The lead owns this loop within its existing authority and finite recovery policy. The user supplies product direction; routine screen composition and comparison should not require repeated human coordination.

Robin subsequently clarified that the immediate request is about the team building the new DevManager features. A10 supplies the concrete [native UX chapter](../../spec/ai-orchestration/UX-SPEC.md) and reference screens, and makes complete native delivery the implementation/release obligation. The broader design method can support the future lead when a project requires it; it is not a new design-management product to build in place of DevManager’s own UI.

## What exists in DevManager

Scoped source inspection was made while HEAD was `3513c1b783c3394366c7efc4db47c868d255e53d`. This is not a complete refresh of the Context Pack or a live UI audit.

| Existing foundation | Inspected evidence and implication |
| --- | --- |
| Shared native architecture | [src/ui/AGENTS.md](../../src/ui/AGENTS.md) already requires GPUI, `gpui-component` initialized through `ui::init`, and product controls through `ui::components`. Domain renderers remain bespoke but consume shared tokens and controls. Reuse this boundary. |
| Semantic controls | [Component vocabulary](../../src/ui/components/mod.rs) includes buttons, badges, text fields, icon buttons, interaction metadata, empty/error states, status presentation, and tabs. This is a starting point to extend, not proof that every screen already uses every desired primitive. |
| Tokens and themes | [Tokens](../../src/ui/tokens.rs) define semantic roles, typography/density/scale and contrast contracts; [theme system](../../src/ui/theme_system.rs) owns theme definitions and selection. [src/theme/mod.rs](../../src/theme/mod.rs) is a compatibility re-export. The central guide’s former instruction to define colors there was stale. |
| Existing appearance targets | [September 3 redesign](../superpowers/specs/2026-09-03-ui-redesign-design.md), section 12, already requires built captures compared with reference PNGs and written adjudication of visible differences. This was a feature-specific rule; the orchestration spec did not generalize it into an obligatory completion check. |
| Capture and examples | [Canonical preview](../../src/ui/preview.rs), [native capture](../../src/ui/preview_capture.rs), [capture script](../../scripts/native-next/Capture-UiPreviews.ps1), and [UI fixtures](../../tests/fixtures/ui) exist. Reuse and extend these instead of building a disconnected design-demo application. |
| Honest quality gates | [VisualGate](../../src/ui/quality.rs), specifically `RequiresCanonicalShell` and `approved_for_pixel_inspection`, deliberately cannot approve synthetic quality data as pixel evidence. [Quality tests](../../tests/ui_quality_gates.rs) preserve that boundary. Passing them does not mean the product looks correct; they must not be weakened to manufacture a visual pass. |
| Interaction and appearance contracts | [Accessibility tests](../../tests/ui_accessibility.rs), [token tests](../../tests/ui_tokens.rs), and [capture tests](../../tests/ui_preview_capture.rs) cover useful contracts. Actual rendered keyboard behavior, whole-shell appearance, live terminal input, provider readiness, and recovery retain their separate acceptance requirements. |

The existing [Native UI System](../native-ui-system.md) has been consolidated as the maintained home for design vocabulary, reference preparation, shared examples, visual comparison, interaction checks, and completion evidence. Its obsolete palette location and future-tense preview advice were corrected. The UI guidance links that home rather than duplicating the procedure.

## What the inspected images establish

Viewed the existing [composition reference](../superpowers/specs/2026-09-03-ui-redesign-mockups/01-composition-A.png) and a [historical native capture](../superpowers/specs/2026-09-03-ui-redesign-mockups/captures/fix-wave-4-grid-after.png). The reference is a presentation sheet containing a chosen composition, explanatory material, and dimmed alternatives. The native capture uses a tall window and different task/content states, including unavailable terminal panes.

These are not matched conditions for a pixel verdict. Their existence demonstrates why a comparison must identify the intended reference region, real shell geometry, data state, and exact build. A long image of a mostly empty UI cannot stand in for a populated reference state; a presentation sheet’s labels and rejected options are not product chrome. Neither historical file proves the current branch’s appearance or behavior.

The redesign document’s header still says it awaits user review, despite selected variants within the reference material. This review does not infer a new approval from that label or choose a new theme. Its historical per-subproject user-launch rule remains scoped to that agreement; new work resolves current authorization and applicable project rules rather than copying an old ritual indiscriminately.

## Tools worth reusing

Command already has a different visual system. Its staff-web tokens (`../../../../command/web/src/styles/_variables.scss`; local snapshot) separate primitive, semantic, and derived CSS roles; global styles (`../../../../command/web/src/styles/global.scss`; local snapshot) compose shared styles and fonts. Its package manifest (`../../../../command/web/package.json`; local snapshot) and E2E configuration (`../../../../command/web/playwright.config.e2e.ts`; local snapshot) include Playwright. These sources were inspected read-only. No Command file, dependency, browser, or application was changed or run.

[Playwright’s screenshot comparison documentation](https://playwright.dev/docs/test-snapshots) provides a useful existing web mechanism: versioned screenshot baselines, explicit tolerances, and rendering-environment distinctions. It warns that operating system, browser, fonts, and other conditions affect the result. Its ability to create or update a baseline is a mechanism, not approval. For DevManager’s lead, baseline creation still needs the intended target and independent review, and a mask or tolerance must not hide the behavior under test.

[Storybook’s visual-testing documentation](https://storybook.js.org/docs/writing-tests/visual-testing) illustrates the useful connection between reusable component states, rendered comparisons, and a required review check. This can inform web component catalogues where the existing project supports them. Its documented hosted integration is not required for this plan. A runnable Storybook setup was not verified in Command; a lint configuration mentioning Storybook does not establish one. Native DevManager should extend its GPUI preview/fixture path.

No new design SaaS, screenshot service, GUI framework, component suite, image-generation service, or model loop is required. Select an existing renderer/comparison path appropriate to the actual platform. Code-level component/token checks catch repeatable violations, visual inspection catches composition and finish, and real interaction tests establish usable behavior. None substitutes for the others.

## Required changes to the delivery process

1. Discover the project’s canonical design sources alongside its code and policy. Find the actual token/component APIs and reusable screen patterns; do not make each worker infer styling independently from unrelated screenshots.
2. Prepare the feature’s journey and visual targets before assigning UI implementation. Reuse an existing reference for a small change. For a new surface, the lead prepares a high-fidelity target in the existing language and gets independent review. A wireframe alone establishes neither typography nor final finish. Keep source, scope, target version, authority, and comparison conditions explicit.
3. Carry that contract into build instructions and worker context. Include required screenshots and actual image-inspection capability when qualifying a worker. Settle visual design during discovery/generation; coding phases implement settled decisions and ship their UI in the same vertical slice.
4. Implement through shared controls and tokens. An imported product idea must be translated into the project’s own navigation, terminology, and visual roles. Extend a missing shared pattern with representative states and review affected consumers.
5. Capture the rebuilt canonical product in the intended state. Review composition, hierarchy, spacing, typography, color roles, controls, and interaction. Use overlays/diffs under matched conditions, with explicit allowances for legitimate rendering differences. Preserve full captures and their provenance alongside any cropped details.
6. Send material differences through the ordinary finding/fix/reverification loop. A builder cannot bless its own baseline or quietly change a target, mask, or fixture to turn a failure green. Shared changes invalidate dependent evidence; concurrent leads reconcile against the actual current design/component versions.
7. Finish only when the required visual and interaction outcomes are verified. Use a representative state/geometry matrix rather than an unbounded Cartesian product. Keep small edits proportionate, but a missing required renderer, an unseen screenshot, or an unusable matched screen remains unverified or failed. Existing recovery bounds prevent endless cosmetic loops; they do not waive the target.

## Amendment and verification scope

[SPEC A9](../../spec/ai-orchestration/SPEC.md) adds U05–U12 and strengthens build generation, worker inputs, visual evidence, completion, and full acceptance. The eight cases cover reference readiness, consistent components, visible mismatches, capture validity, interaction/state coverage, baseline integrity, shared-change reconciliation, and project-scoped design memory. All 164 previous scenario IDs are retained for 172 total. The handoff, Context Pack, and autonomy review point to the same requirements.

This task updates research, specifications, and maintained UI guidance. It does not generate step-4 build documents, implement the runtime gates, render new product screenshots, or claim current visual acceptance. No Rust verification or live application was run.

Document validation passed across thirteen specification, research, and guidance files: local links, table widths, code fences, whitespace, all 172 unique scenario IDs, preservation of the earlier 164 cases, and all 46 autonomy-review records. Git whitespace checking passed as well. The checks include the untracked spec/research files, which a normal Git diff does not cover.
