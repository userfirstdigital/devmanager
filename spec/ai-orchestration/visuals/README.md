# DevManager orchestration reference screens

Open [reference.html](reference.html) directly from disk. It is self-contained and needs no server, packages, account, or network. The view selector switches among the defined product states. Details, goal selection, scoped drafts, explicit answer submission, pause/resume, and knowledge inspection have local simulated interactions. Existing native viewers are represented by explanatory links; the prototype executes no host/provider/workspace actions.

These are **design targets for building DevManager**, governed by [UX-SPEC.md](../UX-SPEC.md). They are not captures of implemented orchestration and do not pass native or live-provider acceptance. All names, status, counts, revisions, timing samples, and evidence text are illustrative. Do not copy them into production as facts.

The PNG targets and [manifest.json](manifest.json) bind each state to its HTML source hash, renderer, geometry, product region, and image hash. The top gallery bar is reference navigation, not product chrome. Use the recorded product region for comparison. Default captures are 1440×900; the narrow decision target is 960×720, both at device scale 1. The typography contract follows the existing native direction; native renderer/font differences require documented allowances, not a blanket pixel-equality claim.

To regenerate a deliberately revised target set, use an existing Playwright installation and isolated Chromium:

```text
node spec/ai-orchestration/visuals/render.mjs --playwright /absolute/path/to/playwright/index.js --browser /absolute/path/to/chromium
```

The renderer installs nothing. It uses a temporary browser profile, blocks external HTTP requests, renders all targets, checks selected prototype interactions, closes its owned browser process tree, removes that temporary profile, and records the result. It overwrites this design target set; preserve and review the intended design change through the feature agreement before rerendering. This is not an automatic update of a production regression baseline.

Step 4 must include native implementation, host/action wiring, populated and failure states, corresponding user journeys, and current native screenshot comparisons in the relevant feature slices. Completing this gallery or a visual test utility is not delivery of the feature-to-surface map.

Reference verification on 2026-09-08: all eleven PNG targets were rendered and visually inspected. The local prototype passed 29 checks covering scene overflow, start validation, scoped drafts, explicit decision submission, pause/resume, knowledge inspection, and narrow-overlay focus/return. The manifest records no JavaScript errors or external HTTP requests. All nine owned browser processes exited and the temporary profile was removed. This verification establishes the reference artifact only; native implementation and acceptance remain step-4 obligations.

At A10 on 2026-09-08, pack integrity checks passed across 15 Markdown files and 222 local links, with all 177 acceptance scenario IDs and 49 review IDs accounted for. All eleven image hashes/dimensions and the HTML source hash match the manifest. Markdown tables/fences/whitespace, renderer syntax, and the tracked diff whitespace check passed. No step-4 build directory was generated.

A12 (2026-09-10) adds source/decision/requirement/evidence navigation, measured-outcome states, context freshness/tool availability and delivery-linked knowledge updates within the existing compositions. See the normative [A12 UX states](../UX-SPEC.md#sources-outcomes-and-shared-knowledge--a12). The eleven PNGs and their manifest are unchanged and do not depict or verify all added states. Prepare any needed expanded target before its code slice and prove it through actual native interactions and matched captures; this amendment does not introduce a graph view or another sidebar.

A13 (2026-09-10) adds actual-versus-selected provider access, account revocation and precise decision scope, incomplete check coverage, and safe source/provenance inspection in the existing compositions. See the normative [A13 UX states](../UX-SPEC.md#access-coverage-and-safe-evidence--a13). Prepare needed expanded targets before their slices and prove the real receipt-driven journeys at normal and narrow geometry. The eleven A10 PNGs/manifest remain unchanged and do not depict or verify these additional states; no new native screenshot was taken.


## Step 4 state extension — 2026-09-12

[step4-reference.html](step4-reference.html) and [step4-manifest.json](step4-manifest.json) add ten rendered targets for the later steering, lineage, authority, coverage, context, learning, uncertainty and final-delivery states. The original gallery/manifest/eleven images remain unchanged. These targets settle intended composition before native implementation; actual approval/acceptance still requires the build pack's independent current native comparison and interaction checks. Loading/empty/query-failure variants use the precise retained-shell behavior in the build overview.

Regenerate only this version with `node spec/ai-orchestration/visuals/render-step4.mjs --playwright <absolute-existing-playwright-index.js> --browser <absolute-chromium>`. The renderer installs nothing, blocks external HTTP, captures the exact requested state, exercises scoped prototype interactions and verifies its owned browser descendants exited. The manifest records geometry, hashes, checks and cleanup; it does not certify the native application.
