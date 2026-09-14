**DevManager's first orchestration workflow: execute a spec directory**

Status: proposed product contract, refined from Robin's supplied “Spec Workflow Prompts — Claude Code edition.” This document describes the executor to build. It does not execute those prompts, install commands, change workspace policies, or select a feature for implementation.

The product contract now lives in [spec/ai-orchestration/SPEC.md](../../spec/ai-orchestration/SPEC.md) for step 4. September 8 amendments A3–A4 add software briefs/problems, Command-first workloads, scoped memory/process improvement, and concurrent task leads beyond this report's finished-spec workflow. Use the current SPEC and Context Pack for build-doc generation; the narrower proposal below remains historical background and rationale.

The first product should accept a request such as: “Do `specs/customer-exports/`: generate all build docs, implement, audit, run E2E, and close the gaps.” The input is the specification Robin hands over as finished and the project's existing rules. The outcome is a feature verified against that agreement, with the prescribed commits, evidence, and any manual deployment steps recorded.

Robin normally owns discovery, gap/stakeholder review, and feedback (Prompts 1–3), and finishes that work before handing it over. The lead owns generation through verification (4, 4b, 5, 7, 6, and the repeat loop). Starting at an existing build directory should resume the appropriate stage after reconciliation. One invocation authorizes the normal transitions covered by the chosen workflow policy; it does not authorize the lead to approve its own changes to the human agreement.

Robin's clarification establishes the entry rule: the handoff itself means the spec is ready. A literal `LOCKED` status is optional. Do not stop for a missing or stale status line, request a separate lock confirmation, or require a status edit before starting. Record the handoff and the specification version in the existing run record; the lead need not rewrite the SPEC's status. References to an “approved spec” in this proposal mean the scope accepted through that handoff or a later explicit amendment, not an additional sign-off step. Check for substantive contradictions or missing product decisions and resolve ordinary technical gaps within delegated authority. A status label alone is not such a contradiction.

This narrows the earlier [orchestration research](2026-09-07-goal-orchestration-strategy.md). A complete execution of this familiar workflow is a better first product demonstration than an open-ended master or an isolated worker/reviewer demo.

**Prove the workflow with standard agents before expanding the runtime.**

The first experiment should be a portable lead workflow built from the existing prompts, with their contradictions resolved once in the authoritative policy. Give a standard Claude or Codex lead the stage instructions, approved artifacts, and the tools needed to launch permitted workers and fresh verifiers. Evaluate the actual loop before adding a workflow language, another model/tool loop, or a model-ranking service. The baseline is a proposal; this research has not demonstrated that any provider or competing product completes it reliably.

Record where the baseline needs Robin: selecting the next prompt, transferring findings, restoring lost work, resolving ordinary engineering questions, or supplying an actual protected decision. Repeat with an interrupted session. If an existing product handles the full process at acceptable effort and cost, adopt it. If the missing pieces are dispatch recovery, process ownership, independent verification, or a coherent workspace view, add those to DevManager's existing host. This makes the investment respond to observed failures.

The product boundary is small: the standard harness runs each agent; the lead reasons about and coordinates the work; ordinary host code records dispatch and evidence and enforces ownership. Use the existing workflow as a versioned project profile rather than introducing a generic workflow editor. Provider choice starts with the project's established role preferences and demonstrated tool capabilities. Later compare complete profiles—model, harness, tools, instructions, and environment—using success, repair effort, time, and available usage data. A model name alone does not identify the best agent for a job.

**Keep the existing documents in their existing roles.**

| Artifact | Authority and ownership |
| --- | --- |
| `specs/CONTEXT-PACK.md` | Verified project facts, commands, conventions, and invariants. Check currency at entry; do not treat its date alone as proof. |
| `<feature>/SPEC.md` | The agreed behavior and scope. Bind the run to the version supplied at handoff and subsequent approved amendments; no `LOCKED` marker is required. |
| `build/00-overview.md` and phase docs | Derived implementation instructions and exact contracts. The generator resolves technical details within delegated authority. |
| `build/TRACKING.md` | Durable build/test progress, pending blockers, and where execution stopped. Add a compact run section here rather than a competing progress file. |
| `build/TEST-PLAN.md` | Cross-phase journeys, environment/setup requirements, pinned routes, and E2E results. |
| `build/AUDIT.md` | Stable findings, disputes, rework, and independent verification of fixes. Fixers propose closure; auditors verify it. |
| Root shared tracker and `DONE.md`, where present | Existing problems, recurrence checks, and out-of-scope findings. Bind the actual filenames and preserve their conventions and case. |
| `specs/PARKED.md` | Deferred work promoted idempotently during generation. Parked work stays outside the build. |

DevManager's database owns execution facts: process/assignment identity, dispatch receipts, cancellation, and active ownership. It references document revisions rather than maintaining another editable copy of the SPEC, checklist, or audit. UI progress is derived from the authoritative artifacts and execution facts. A historical “worker finished” receipt is not a current claim that a feature is shippable.

A small run header can record the selected workflow/policy version, actual repository roots and source revisions, document versions, current stage, and evidence references. Use existing durable evidence locations for logs and captures; final reports must not depend on temporary scratch paths. Before any restart, read the artifacts and inspect unsettled work. A chat ending or a wrapper timing out does not prove its child process stopped.

**The lead runs the whole loop, with separate contexts for independent verification.**

| Stage | What the lead arranges | Condition for advancing |
| --- | --- | --- |
| Entry | Resolve the workspace and spec directory; record the handoff and spec version; read the actual policy/import files; establish repo state and required capabilities | User has handed over the spec for execution; policy coherent under current instructions; required environments available; existing work reconciled |
| 4 — Generate | Write overview, all phase docs, and TEST-PLAN; promote parked items once; settle technical decisions | Every agreed scenario covered; contracts consistent; no unresolved blocker needed by the next stage |
| 4b — Implement | Build complete features with lean phase updates; delegate only useful independent work; follow the project's checks and landing rules | Code built, required tests/build complete, and prescribed commits recorded per repository |
| 5 — Audit | Launch a fresh auditor with SPEC, Context Pack, overview, phase docs, and the actual code | Findings and acceptance evidence recorded; required checks completed or explicitly marked unverified |
| 7 — E2E | Launch an independent tester with the real browser/app, personas, service logs, and TEST-PLAN | Journeys, non-visual checks, performance checks, and log findings recorded |
| 6 — Fix | Fix the scoped audit list, record evidence and disputes, and leave closure to an independent verifier | Each attempted fix has evidence; no self-certified checkboxes |
| Repeat and close | Arrange fresh audit and affected E2E/regression checks after fixes | Required findings independently closed, current-revision evidence sufficient, and the authorized delivery boundary reached |

The lead should not pause for `NEXT` between generated phase documents or for a person to paste the next numbered prompt. These are workflow transitions it has been delegated. Give it an explicit stage-advance capability; do not have it impersonate Robin or synthesize a user approval message. The actual handoff is sufficient to start; subsequent changes to the agreed product or expansions of authority still require the appropriate user decision.

Use one feature as the default unit of implementation and landing. Across multiple named features, complete one before starting the next, matching the stated common path. Parallelize bounded work inside that feature when dependencies and resource ownership permit it. Broader cross-feature concurrency can remain a later, explicit policy choice.

Preserve the supplied role preferences as the initial routing policy: Claude for lead judgment and difficult work, Cursor for suitable mechanical assignments, Codex for the prescribed landing review. Read the actual `delegation.md` before executing its ladder. A generic “best model” router should not override these rules. The independent full-feature auditor is a separate role from the pre-commit reviewer; choose its provider under the loaded policy.

“Fresh auditor” means a new conversation constructed from the specification, policy, code, and relevant findings, without the builder's chat history or reasoning. It can read earlier findings to maintain stable IDs, but it must re-establish their facts. Choosing a different model while forwarding the builder's entire conversation does not create the intended independence.

Independent sessions also need bounded write authority. The auditor writes the audit; the E2E tester updates the test plan and submits findings; implementation/fix sessions change allowed application paths. Serialize writes to shared reports and the root tracker, even if evidence gathering runs concurrently. Read-only application access still permits test execution in an isolated environment; it does not permit the auditor to quietly fix production code.

**The lead answers procedural and technical questions; Robin retains product authority.**

| Situation | Proposed handling |
| --- | --- |
| Another phase document is ready to be generated | Continue automatically after checking its contracts. |
| A worker needs a convention, command, or settled decision | Read the authoritative artifact and answer with its source. |
| A small implementation detail is unspecified | Decide under the design values and record it in the appropriate build artifact. |
| Audit finding demonstrably misstates code or the docs | Record the dispute with evidence and obtain independent adjudication; no human needed for an established factual correction. |
| Valid in-scope defect or failed check | Assign the fix and independent verification. |
| Missing browser, credentials, policy import, or usable environment | Record the exact capability problem; arrange authorized setup where possible, otherwise block the dependent stage. |
| An agreed behavior, invariant, exact approved contract, or acceptance check must change | Prepare one focused decision for Robin, with consequences and affected artifacts. Continue independent work. |
| A known unrelated issue or parked idea would be convenient to fix | Record it under existing conventions; do not expand scope. |
| The same finding survives two fix rounds | Apply the current human-stop rule. A future policy could delegate a fresh diagnosis to the lead, but that change must be explicit. |

Required input stays pending until answered. “The lead handles 4+” removes routine handoffs; it does not erase the workflow's protected decisions. Once an answer is given, the lead applies it through every affected overview, phase, test plan, and rework entry, with no half-completed amendment ripple. It then resumes the affected work automatically within the existing authority.

A factual Context Pack correction is different from changing an invariant. Correcting a verified moved command can be recorded without changing the agreed product. A discovered policy or design contradiction remains a blocker under the applicable policy. If current prompts mandate a human stop even for a pure relocation, the executable workflow must explicitly reconcile that rule before granting the lead broader repair authority.

**Several prompt conflicts need explicit normalization before unattended execution.**

The handoff rule above is Robin's explicit clarification and supersedes the older Prompt 4 requirement to check for `LOCKED` or ask for an exception. The remaining rules below are proposed defaults, not edits to the supplied templates or a claim that their external house policies have been read. The canonical policy files were not supplied with this example. Read them before a real run and apply them under the user's current instructions.

| Conflict in the supplied text | Proposed consistent rule |
| --- | --- |
| 4b forbids build-pass tests, then requires narrow suites during that pass | Phase-boundary typecheck/lint and focused tests where required by the actual house policy; complete required test coverage and one full suite in the final test pass. Keep verification economical. |
| One landing per feature, but later every phase boundary is a landing; Prompt 4 also prescribes phase commits | Default to one feature landing, with explicitly required high-risk exceptions. Intermediate phase status is not a claim of having landed. Reconcile this with the actual `landing.md`; do not leave both instructions active. |
| Prompt 5 permits only one write, but also directs writes to the root tracker | Auditor reports out-of-scope findings in its result; the lead merges them into the existing tracker. Keep one owner of each shared artifact. |
| Full suite must be green, but a standing red baseline is accepted elsewhere | Complete the suite, compare failing identities against a valid baseline, require feature acceptance and build checks to pass, and report every remaining pre-existing failure. Do not call that an entirely green suite. |
| Prompt 4 requires human `NEXT` and repeated technical close-out questions | The run authorizes procedural continuation and technical decisions within scope. Only unresolved protected decisions reach Robin. |
| “Report-only” E2E must diagnose a root cause before a finding is actionable | Preserve the observed failure even when its cause is not yet established. Mark diagnosis incomplete and delegate investigation; do not omit a failed journey. |
| Audit reads generated build docs but does not explicitly require the SPEC supplied at handoff | Add SPEC to the auditor's input. Verify both SPEC → build-doc coverage and SPEC → implementation coverage in the existing audit, without adding another review stage. |

The audit's SPEC coverage is particularly important: generation and implementation can consistently omit the same requirement. An auditor comparing only those two would validate an incomplete feature. Put a concise scenario-to-phase/check map in the existing overview, anchored to the approved spec version; keep the human SPEC as the authority.

Canaries are useful diagnostics, not proof of successful imports. The supplied prompt itself contains `WORKSPACE-POLICY-OK` and `DELEGATION-POLICY-OK`. Seeing those strings in context cannot prove the referenced files were loaded. Before execution, read the actual import chain and record its sources; do not recreate missing policy from its name or from this example. Likewise, resolve the existing tracker once instead of creating both `todo.md` and `TODO.md` on a case-sensitive filesystem.

The stated migrations rule remains: author migrations and record the run-sheet, but do not apply them. If E2E depends on an unapplied migration and policy makes no exception for a disposable test environment, that journey is unverified until the user provides the required setup or authorization. A clean build does not turn this into a tested deployment.

**Multi-repository execution is part of the initial contract.**

The project root may be a Git repository for specs, or it may only contain separate repositories. Bind each actual repo root explicitly; a Git command run in a child folder can otherwise discover an ancestor repo. Record the root/spec revision, backend revision, frontend revision, and any other participating repo revisions as one coherent workspace version. Include relevant uncommitted content when pinning verification inputs.

An isolated assignment that touches multiple repos needs the expected sibling directory layout, so commands and relative references still work. Isolate the participating writable checkouts as a unit while preserving access to approved spec/policy artifacts. Do not assume one worktree of one repository covers a complete vertical slice.

Landing across independent repositories is a group of commits, not an atomic transaction. If one commit fails after another succeeds, record partial landing and reconcile it before declaring completion. Follow the loaded target-branch and review rules; this research does not authorize commits to DevManager's `master` or to any example project's repository.

Capture baseline failure identities before edits when required. A test process without its completion summary or with an unfinished child tree has not established a baseline or a final result. Reuse completed evidence only when its inputs, scope, environment, and independence meet the next stage's requirements. Do not rerun the global suite for every phase, but do not replace an explicitly independent audit run with the builder's unverified claim.

Collect check evidence from the actual execution: command, working directory, input revisions, exit/completion status, and durable output location. Preserve relevant browser captures and service-log intervals similarly. The agent can interpret this evidence, but its statement that a command passed is not the receipt. Validate required evidence before advancing the run; subjective product judgments still belong to the designated verifier or human.

After fixes, rerun affected journeys and relevant regression checks. Audit and E2E evidence must both apply to the final workspace version. Preserve unrelated valid evidence when its dependencies are demonstrably unchanged; do not silently promote old evidence to a new revision. This provides selective re-verification without a full restart after every change.

**Completion is a supported verdict, not a model's final sentence.**

The run can report SHIPPABLE only when the SPEC supplied at handoff and subsequent approved amendments are covered, every required phase/check is verified, required build checks pass, blocking and should-fix findings are independently closed, remaining minor items and baseline failures are explained under policy, and commits/evidence correspond to the final workspace version. Required checks that could not run remain NOT VERIFIED. A manual migration or publication step is recorded as a remaining deployment action; it is never described as already executed.

Most of the UI can reuse the Task cockpit: spec directory, current stage and worker, phase progress, open findings, one “Needs you” decision area, pause/stop, and the final evidence. The common input should stay as small as “do this spec directory.” The project supplies standing workflow and provider preferences; the user should not choose an agent for every stage or manually acknowledge every transition.

The first acceptance run should include generation from a spec handed over as finished with no `LOCKED` marker and no build directory, several phases spanning two repos, a real implementation/test pass, fresh audit, actual E2E, an injected in-scope defect, a fix, and independent closure. Also check that a stale `DISCOVERY` status alone does not trigger another readiness question. Interrupt the lead once and prove that recovery preserves the agreed spec, stable finding IDs, workspace state, and process ownership. Add a protected spec-change case to prove that only dependent work blocks and a single precise decision reaches Robin.

Success means Robin supplies the approved directory once and never has to type `NEXT`, choose the next numbered prompt, transfer audit findings, restart a forgotten fix round, or remind the lead to verify its work. The supplied workflow already describes the intellectual process. DevManager's first job is to make its execution dependable.
