# Atomic: code and lessons for DevManager

Reviewed 2026-09-10. Incorporated as A11 in [SPEC.md](../../spec/ai-orchestration/SPEC.md) and its [native UX chapter](../../spec/ai-orchestration/UX-SPEC.md).

## Recommendation

Atomic is a particularly relevant reference for this goal. Its actual source contains a goal controller, work receipts, independent review stages, nested durable work, parent/worker messaging, and design workflows. Adopt selected execution contracts and test cases inside DevManager’s existing host. Keep the standard qualified coding-agent harnesses and native UI. Importing Atomic’s complete runtime would add a second owner of sessions, scheduling, storage, tools, and completion.

The largest useful correction is subtle: distinguish **implementation evidence, review-process obligations, and final delivery actions** so reviews do not deadlock waiting for an action that correctly happens after review. Enforce every obligation at its proper stage. Do not solve that ordering problem by letting reviewer votes override missing evidence.

Other strong lessons are explicit invocation identity during nested replay, retained identity for transport retries, and one shared path for propagating steering. Several reinforce requirements already present; they do not justify another workflow editor, broker, memory store, or settings page.

## Evidence and provenance

Inspected the supplied atomic-main directory (`../../../atomic-main`; local snapshot), its workflow/goal implementation, relevant subagent/intercom code, design tooling, and selected tests. It has no `.git` metadata or installed `node_modules`. Its package versions use `0.0.0`; those values do not identify a release. No commit or archive date is inferred. The [source manifest](evidence/atomic-review/sources.json) pins the selected files by full SHA-256 and an aggregate fingerprint; inclusion identifies review inputs, not a line-by-line audit of every file.

The [official repository](https://github.com/bastani-inc/atomic), [workflow documentation](https://docs.bastani.ai/workflows), and [current root license](https://raw.githubusercontent.com/bastani-inc/atomic/main/LICENSE) were checked separately. Upstream main is mutable and is not asserted to be identical to this downloaded directory. Product claims about merge rates or time saved were not independently evaluated.

Seventeen [isolated source probes](evidence/atomic-review/probe-results.json) passed using the [reproducible probe](evidence/atomic-review/probe.mjs), Node’s TypeScript erasure, and an allowlisted VM module graph. They exercise pure helpers and process-local retry identity with synthetic data. The reviewer predicate and reducer quorum branch are explicitly extracted; the full schema parser, reviewer models, workflow runner, broker, database, and side effects are not exercised. No dependency was installed, provider launched, external effect executed, or Atomic/DevManager full suite run.

## What to take, and what to change

### 1. Preserve the original goal through every repair

goal.ts (`../../../atomic-main/packages/workflows/builtin/goal.ts`; local snapshot) separates a turn’s objective from original acceptance criteria. goal-types.ts (`../../../atomic-main/packages/workflows/builtin/goal-types.ts`; local snapshot) defines receipts, requirement traceability, reviews, blockers, lifecycle events, and final status. This is close to the user’s requested “hand over the goal and rules” experience and supports keeping a bounded controller outside disposable worker conversations.

DevManager already specifies this under its goal agreement, proportional planning, and durable artifact authority. Reuse the contract and adversarial examples, not a duplicate `goal-ledger.json` next to independently editable SPEC/TRACKING/AUDIT state. The current [orchestration helpers](../../src/providers/orchestrator.rs) explicitly sit on canonical command/event facts; extend those facts rather than introducing an Atomic state authority.

### 2. Review votes are useful judgments, but cannot replace the completion check

review-convergence.ts (`../../../atomic-main/packages/workflows/builtin/review-convergence.ts`; local snapshot) has useful scope-aware finding helpers: required work blocks even at minor priority; optional minor improvements need not inflate scope; unclassified findings remain unresolved. Findings are consolidated before planning repair. Port these behavioral cases into DevManager’s existing audit/finding model.

There is an important boundary in the actual goal path. goal-review.ts (`../../../atomic-main/packages/workflows/builtin/goal-review.ts`; local snapshot), `reviewApproved`, accepts `stop_review_loop=true` when there is no reviewer execution error. goal-reducer.ts (`../../../atomic-main/packages/workflows/builtin/goal-reducer.ts`; local snapshot), `reduceGoalDecision`, completes on a quorum of those decisions without rechecking findings or traceability. The boolean-convergence tests (`../../../atomic-main/test/unit/goal-review-objective-drift.test.ts`; local snapshot) document this choice deliberately; it avoids self-referential requirements such as reviewers trying to prove the review quorum or a later PR action.

The isolated predicate accepts a synthetic contradictory report, and the reducer’s quorum branch reports complete with two complete votes while required work remains missing. This demonstrates the helper boundary, not a measured failure rate or a full live exploit of Atomic.

DevManager should keep a typed, host-evaluated completion check over current required outcomes, independent finding dispositions, actual check receipts, and settled effects. The model still makes evidence-based judgments about behavior and findings; schema validity and vote counts alone do not establish their truth. A disagreement is resolved through the existing evidence-backed dispute/fix path. A required missing behavior cannot disappear by lowering its severity or averaging reviewer confidence. This strengthens A08 and C08 without mandating three reviewers for ordinary work.

### 3. Separate review readiness from the final requested delivery

Atomic’s goal definition (`../../../atomic-main/packages/workflows/builtin/goal.ts`; local snapshot) and workflow guide (`../../../atomic-main/packages/workflows/README.md`; local snapshot) place PR/MR creation in a final stage selected by `create_pr`. Its convergence helper also recognizes final actions using text patterns. Take the sequencing lesson; do not require users to remove phrases from their goal or provide a special boolean.

At intake/planning, DevManager records which requirements concern the implementation, which concern the independent review process, and which are final authorized effects. A review of the implementation need not claim that its own quorum or a later PR already exists. The host verifies those later obligations at their own boundaries. A failed final action leaves delivery incomplete and is reconciled before retry; “implementation approved” remains distinct from “goal delivered.” The user’s existing authority decides whether that effect is permitted. A “ready to deploy” request may legitimately end before production deployment; a requested and authorized PR does not. C09 makes this ordering explicit and rejects keyword-only exceptions to acceptance.

### 4. Replay the same owned invocation, not merely the same-looking call

scoped-backend.ts (`../../../atomic-main/packages/workflows/src/durable/scoped-backend.ts`; local snapshot) preserves nested checkpoints under a stable root/child namespace. boundary-topology.ts (`../../../atomic-main/packages/workflows/src/durable/boundary-topology.ts`; local snapshot) validates the child, parent edges, source order, and lifecycle before replay. tool-resume-frontier.ts (`../../../atomic-main/packages/workflows/src/durable/tool-resume-frontier.ts`; local snapshot) refuses missing/ambiguous frontiers, duplicate identities, cycles, and absent completed checkpoints. The nested resume (`../../../atomic-main/test/unit/durable-nested-resume-hydration.test.ts`; local snapshot) and repeated child (`../../../atomic-main/test/unit/durable-repeated-child-replay.test.ts`; local snapshot) tests are useful acceptance sources.

child-invocation.ts (`../../../atomic-main/packages/workflows/src/durable/child-invocation.ts`; local snapshot) hashes definition names and inputs through durable-hash.ts (`../../../atomic-main/packages/workflows/src/durable/durable-hash.ts`; local snapshot). The probe confirms canonical object ordering and sensitivity to changed input. It also confirms that this helper alone does not bind implementation code: changing a same-named definition’s body leaves its fingerprint unchanged. Other runtime checks are outside that helper’s claim.

DevManager must bind replay to the admitted plan/procedure version, root and parent invocation, occurrence, validated inputs, ownership, and required evidence lineage. Two intentional identical calls are different occurrences; a transport retry of one is the same intent. Changed code/methods or missing checkpoints require reconciliation rather than reuse based on a matching name/hash. Replayed observations keep their original environment/time and must be refreshed when their acceptance requires present state. D24 extends existing version and dependency requirements with these cases.

### 5. A checkpoint cannot make an arbitrary external effect exactly once

Atomic’s tool primitive (`../../../atomic-main/packages/workflows/src/durable/tool-primitive.ts`; local snapshot) executes a callback, then records its completed checkpoint, then publishes completion. Its cached tool outcome (`../../../atomic-main/packages/workflows/src/durable/tool-outcome.ts`; local snapshot) distinguishes success/failure, attempts and replay; that distinction is worth preserving. A caught failure value must not become successful acceptance simply because the containing stage continued.

There remains an unavoidable boundary for arbitrary effects: an external action can finish before its local result is durably recorded. No crash was injected here; this follows from the inspected execution order. Do not import the caching claim as an exactly-once guarantee for PR creation, provider input, repository changes, or other external actions.

DevManager already has [outbox replay policies](../../src/kernel/outbox.rs): retry-safe, reconcile-before-retry, and no-automatic-retry. Use that existing model. Identify the destination’s actual receipt/idempotency contract or reconcile the effect; an unknown result remains unknown and is never blindly repeated. D25 adds this fault boundary to acceptance, including preservation of independent work while the affected branch is reconciled.

### 6. Treat worker communication as owned operations

retry-identity.ts (`../../../atomic-main/packages/intercom/retry-identity.ts`; local snapshot) is a useful small adaptation candidate. A fresh call creates a fresh intent; a typed recoverable disconnect can retain an opaque retry token for the exact session/action/target/content. The original deadline and allowance persist, concurrent claims are refused, and settled tokens cannot be replayed. send-signature.ts (`../../../atomic-main/packages/intercom/broker/send-signature.ts`; local snapshot) excludes transport-attempt metadata from the logical send identity.

The probes confirm distinct identical fresh asks, preservation of the original identity during an admitted retry, and rejection of changed recipients/content, concurrent claims, settled operations and expiry. These are process-local tests, not proof of broker delivery or restart durability.

inbound-message-admission.ts (`../../../atomic-main/packages/intercom/inbound-message-admission.ts`; local snapshot) separates reservation from delivery commit. terminal-ordering-barrier.ts (`../../../atomic-main/packages/intercom/terminal-ordering-barrier.ts`; local snapshot) drains earlier worker messages before terminal publication. subagent-reply-capability.ts (`../../../atomic-main/packages/intercom/subagent-reply-capability.ts`; local snapshot) distinguishes the end of execution from an idle agent turn. Reuse these distinctions through DevManager’s existing durable receipts and ordering; do not import an in-memory cache or global callback registry as durable authority.

steering-context.ts (`../../../atomic-main/packages/workflows/builtin/steering-context.ts`; local snapshot) applies a common steering contract to newly added task/chain/parallel paths. Its prototype-based wrapper also preserves a live working-directory getter rather than copying stale context. The propagation test (`../../../atomic-main/test/unit/builtin-workflow-steering-propagation.test.ts`; local snapshot) is a useful regression pattern. Prompt text alone cannot guarantee delivery: DevManager’s current host must record which assignments received/applied the correction and reconcile stale work. Q05 and U18 cover retries, late replies, nested workers, and visible steering state without requiring the user to message each worker.

### 7. Learn from convergence without turning scores into truth

goal-convergence.ts (`../../../atomic-main/packages/workflows/builtin/goal-convergence.ts`; local snapshot) explicitly makes trend evidence observational. progress-scoring.ts (`../../../atomic-main/packages/workflows/builtin/progress-scoring.ts`; local snapshot) distinguishes observed results from effort or success narration. goal-reverify.ts (`../../../atomic-main/packages/workflows/builtin/goal-reverify.ts`; local snapshot) preserves original findings and re-verification records but uses repeated model scores for demotion. Keep evidence history and focused fresh diagnosis; do not copy confidence thresholds as authority to remove required work.

DevManager’s existing cause-based repair budgets and holistic learning already cover the useful behavior. Reconcile stable findings, changed code/evidence, rejected approaches, and counterexamples before a different repair or process revision. A plateau is a diagnostic reason to reconsider an approach, not permission to lower acceptance or a new automatic cutoff for productive work. Do not add per-turn scoring calls or a universal lesson from one unsuccessful run. The existing memory/procedure homes remain authoritative.

Atomic’s activity watchdog (`../../../atomic-main/packages/coding-agent/src/modes/interactive-engine/activity-watchdog.ts`; local snapshot) also separates internal heartbeat diagnostics from concrete engine failures. Take that distinction, not its engine-specific millisecond thresholds. DevManager’s current process evidence, elapsed budgets, and productive compiler ownership remain the basis for recovery.

### 8. Carry the useful behavior into the native product

Atomic’s design guide (`../../../atomic-main/DESIGN.md`; local snapshot) connects status appearance to real execution state. Its design workflow (`../../../atomic-main/packages/workflows/builtin/open-claude-design-runner.ts`; local snapshot) connects discovery, existing design sources, references and handoff. The Impeccable source lock (`../../../atomic-main/packages/workflows/skills/impeccable/scripts/live/source-lock.mjs`; local snapshot) uses an acquisition token so an old owner does not remove a replacement lock; its accept helper (`../../../atomic-main/packages/workflows/skills/impeccable/scripts/live-accept.mjs`; local snapshot) reports failed source publication rather than a successful visual choice. These are useful ownership/publication lessons, not a native implementation toolkit.

Keep DevManager’s A10 shell and references. Plan shows versioned work and replay/reconciliation reasons. Agents shows scoped assignments and steering delivery. Checks distinguishes passed evidence, disputed findings, verification readiness and unmet delivery actions. Recovery does not erase the conversation. Use existing diagnostics for internal detail; routine heartbeats do not become repeated alarms. Native screenshots and real user interactions remain required. Atomic’s TUI palette, web variant wrappers, DOM publication tools, and arbitrary TypeScript graph authoring are not part of this feature.

## Code reuse and license disposition

The local root and `packages/workflows/LICENSE` say “MIT License” but add an obligation to display Atomic if the product/service exceeds 100 million monthly active users or $20 million in monthly revenue. The [official root file](https://raw.githubusercontent.com/bastani-inc/atomic/main/LICENSE) also contains that addition. Treat these as the actual supplied terms, not an unqualified ordinary-MIT dependency based on package metadata.

The local `packages/intercom/LICENSE` and `packages/ai/LICENSE` contain conventional MIT text with their own notices. That identifies useful candidates; it does not establish that every file’s provenance or root/package interaction is settled. Check the exact copied files, upstream notices and applicable terms before an actual copy/port. An independent implementation within existing DevManager primitives can meet A11 without making code import a prerequisite.

| Candidate | Decision for DevManager |
| --- | --- |
| Intercom retry identity, logical send signatures, and relevant tests | Adapt the bounded state-machine behavior into the existing Rust receipt/outbox path. Consider a source port only after checking exact-file terms. Do not ship a second Node broker. |
| Goal finding alignment, requirement traceability shapes, and synthetic counterexamples | Reuse acceptance ideas and small typed contracts. Implement completion against DevManager’s canonical evidence; do not copy the reviewer-boolean/quorum completion policy. |
| Nested checkpoint topology, resume-frontier and repeated-child tests | Translate relevant cases into the existing Rust journal/replay tests. Reuse the contracts, not the DBOS/backend implementation. |
| Steering propagation and source-publication ownership | Extend existing assignment/context and artifact publication boundaries; add coverage for all entry paths. No separate mutable wrapper state or copied UI publication engine. |
| Atomic/pi coding runtime, DBOS/embedded Postgres stack, arbitrary executable workflow discovery, global intercom broker | Do not import for this spec. They overlap the host/harness/storage architecture and add substantial ownership and maintenance work. |
| Model ranking lists, quorum sizes, automatic confidence demotion, hardcoded timing/loop defaults | Do not adopt as DevManager policy. Keep qualified profiles, real budgets, explicit evidence and existing authority. |
| Atomic TUI, Impeccable browser variants, complete design workflow | Learn the interaction/publication discipline. Build the specified DevManager GPUI experience with its existing components and reference targets. |

## Specification changes and remaining proof

A11 adds seven scenarios, retaining all previous IDs: A08, Q05, D24–D25, U18, C08–C09. The total is 184. Five review entries cover the new boundaries. The native UI chapter and step-4 handoff are updated together. No separate build plan or application code was produced.

Step 4 must place these cases in the existing verification, orchestration/replay, communication and native feature slices. Use real interrupted-effect, nested-worker, steering/reconnect and final-delivery runs, not only these helper probes. Test incomplete, contradictory and stale evidence as well as the successful path. Preserve the earlier four live workloads and complete native journey; an Atomic trial is optional and does not substitute for DevManager acceptance.

The source inspection and probes establish design/code evidence only. They do not demonstrate full autonomous delivery, performance improvement, native appearance, restart durability of a port, or license compatibility of an unselected import.

Documentation verification passed across 16 Markdown files and 271 local links, with 184 unique acceptance scenarios, all 177 previous IDs retained, and 54 review entries. Tables, fences, whitespace and probe syntax passed. All 57 pinned Atomic source files remained unchanged; probe results match their source manifest. The eleven A10 image hashes/dimensions and gallery source hash still match their original manifest. No build directory was generated.
