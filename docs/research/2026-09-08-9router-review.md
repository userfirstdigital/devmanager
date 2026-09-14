# 9router reuse review — 2026-09-08

## Recommendation

Take the lessons and turn the important edge cases into DevManager acceptance checks. Do not fork or embed the complete 9router application for this release. No inspected module offers enough benefit to justify importing it unchanged into the current Rust host.

9router routes model requests. DevManager must route accountable software assignments, preserve their authority and work, and prove their results. Request routing can help that workflow, but does not supply its project lead, durable milestones, independent audit, memory synthesis, or repair loop. Keep the standard coding harnesses responsible for their model/tool loops and supported authentication. Extend DevManager's existing host where it needs shared provider availability, assignment selection, and recovery.

This follows Robin's direction: bring useful behavior into DevManager. It does not require adopting 9router, changing CLI endpoints, or opening another orchestrator.

## Inspection and evidence

Inspected the original [decolua/9router repository](https://github.com/decolua/9router), pinned to [eb712ca821f0ba6bc41043fbd14494c5af5daba5](https://github.com/decolua/9router/commit/eb712ca821f0ba6bc41043fbd14494c5af5daba5), committed September 5, 2026. The pinned package is version 0.5.69. Read its architecture, license, manifests, routing/account selection, capability handling, retries, refresh deduplication, compression modules, and selected related tests. Downloaded individual source files into a temporary research directory; did not install or start the application.

Executed three isolated pure-function probes against those exact downloaded modules with Node v24.20.0. The module loader allowed only local relative imports beneath the pinned source's open-sse directory; the VM context had no application, network, credential, or process interfaces. These establish the specific function behaviors below. They are not a live-provider trial, a run of 9router's full test suite, or proof of every application path.

Rechecked DevManager's [orchestration helpers](../../src/providers/orchestrator.rs), [capability evidence](../../src/providers/capabilities.rs), [adapter boundary](../../src/providers/adapter.rs), and [Codex integration](../../src/providers/codex.rs). No DevManager code, provider settings, account state, or installed application was changed.

## What it actually supplies

The [architecture](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/docs/ARCHITECTURE.md) and [manifest](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/package.json) describe a Node/Next.js gateway and dashboard, OpenAI-compatible request endpoints, provider-specific execution/translation, account management, and its own persistence. This is a separate application stack, rather than a small orchestration library.

| Layer | Useful responsibility | Owner in the DevManager plan |
| --- | --- | --- |
| Goal coordination | Diagnose the problem, maintain milestones, delegate, integrate, audit, repair, and establish completion | DevManager host and each task's lead |
| Coding harness | Conversation, tool loop, native delegation, supported login, and provider execution | Qualified standard Claude Code/Codex/Cursor profiles |
| Model-request gateway | Choose an upstream request route, translate formats, rotate connections, retry requests, and observe usage | 9router's principal layer; no new mandatory gateway in DevManager |

A model combo is not a team of software specialists. 9router's fusion path collects model answers and asks a judge to synthesize them; it strips active tool definitions from panel requests. It does not establish separate source ownership or independent implementation evidence. Its timeout wrapper can stop waiting while the losing request continues. These are different semantics from DevManager settling an owned worker before reassignment. [Fusion implementation](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/services/combo.js#L473-L635)

## What to adopt

| 9router pattern | Useful behavior in DevManager | Reuse decision |
| --- | --- | --- |
| Ordered candidates and capability-aware selection | Filter to qualified, permitted profiles first; rank those by project preference, applicable results, and available resources | Adapt the behavior in existing host admission |
| Preferred connection and sticky routing | Preserve a working assignment's profile/conversation; make a necessary replacement explicit and explain why | Adopt the continuity principle, not per-request round robin |
| Per-model and account cooldowns | Share supported availability observations across goals using their actual scope and freshness | Adapt with stricter scope composition and recovery checks |
| One in-flight refresh for concurrent callers | Coalesce equivalent host-owned readiness probes and retry wakeups, while each waiting goal retains its own authority | Adopt the coordination pattern; leave credential refresh with its existing owner |
| Usage and reset visibility | Show observed usage, the cause of a wait, known reset time versus local retry choice, and fallback reason | Extend the current cockpit, with unavailable measurements labeled |
| Tool-output reduction | Offer compact diagnostic views with links to the original evidence and complete failure identities | Adopt only where evidence integrity is preserved |
| Answer fusion | Multiple independent perspectives can inform diagnosis | No fusion service; agreement cannot certify code or justify a process-rule change |
| Protocol translation and account gateway | Potentially useful for a future explicitly required direct-API mode | Outside this release; do not copy as the lead architecture |

The preferred-connection and sticky behavior is visible in [account selection](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/src/sse/services/auth.js#L139-L184). Request-level affinity suggests a useful principle, but DevManager's unit is the assignment and exact provider conversation. A stable alias hiding different upstream models cannot stand in for actual qualification or exact-resume identity.

## Findings that change the specification

### 1. Required capabilities must exclude incompatible fallbacks

The capability sorter puts suitable models first but retains models missing required modalities. Its own test explicitly expects the non-vision candidate to remain. Separately, the non-passthrough chat path can remove unsupported media before translation. That can be a gateway's chosen compatibility behavior; it is unacceptable as an implicit way to satisfy an assignment that requires inspecting a screenshot or using a particular tool. [Sorter](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/services/combo.js#L60-L81), [test](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/tests/unit/combo-autoswitch.test.js#L53-L77), [media handling](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/handlers/chatCore.js#L155-L169)

For DevManager, a fallback must still satisfy the original assignment's required tools, input interpretation, authority, verification independence, and identity contract. If none qualifies, retain the exact missing capability and continue independent work. Do not remove the requirement to make another candidate eligible.

### 2. Availability is scoped evidence, not a single healthy/unhealthy flag

The inspected cooldown helper selects a model-specific expiry before the account-wide expiry. The probe supplied an expired model-specific value and an active account-wide value. It returned false for “lock active.” The selector calls this helper when filtering connections. This is a demonstrated utility-level counterexample; no claim is made that every deployed provider can reach that state. [Lock helper](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/services/accountFallback.js), [selector](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/src/sse/services/auth.js#L81-L96)

The same account service can cap some provider-reported future reset times to a shorter local cooldown, and its success-clearing path includes the account-wide lock. Those choices reinforce the need to keep observation scope and expiry semantics explicit. [Cooldown and clearing](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/src/sse/services/auth.js#L239-L319)

DevManager must combine all applicable current restrictions: account, model/profile, provider, project, and resource. A success in one scope cannot erase a wider or unrelated restriction. Keep a provider-reported reset distinct from a bounded locally selected recheck time. Unknown quota stays unknown. Temporary health belongs in host execution observations, not universal memory or a permanent model ranking.

### 3. Nested retries need one accountable assignment history

9router has retries within an executor, connection fallback in the chat handler, and model fallback within a combo. Its base executor has explicit finite retry counts, with tests exercising network and status fallback. Those local bounds do not by themselves bound a higher-level workflow that retries the whole operation again. [Executor](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/executors/base.js), [executor tests](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/tests/unit/base-executor-retry.test.js), [connection loop](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/src/sse/handlers/chat.js), [combo loop](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/services/combo.js#L281-L387)

DevManager must own one retry history for the interrupted work across every host-controlled layer. Native harness retries remain owned by the harness; unavailable detail is explicitly unknown. The host must not start a competing request while the harness is still retrying. A network response or partial output cannot settle the worker, prove delivery, or establish a successful check. Cancellation must suppress queued retries and preserve actual in-flight ownership.

### 4. Compression must not rewrite the evidence of failure

The tool-message compressor mutates input content and keeps a transformation when it is nonempty and shorter. Its build filter recognizes selected lines, omits others, and summarizes compilation progress. This is lossy filtering; shorter text alone does not establish preserved meaning. [Compressor](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/rtk/index.js), [build filter](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/rtk/filters/buildOutput.js)

The executed probe passed an OpenAI-style tool message containing thirty synthetic compilation-progress lines, an out-of-memory error, and a final killed-process line. Its 948 characters became 20 characters: “Compiled 30 packages.” Both failure lines disappeared. No actual compiler or test suite ran.

That is directly relevant to the 300-red-tests workflow: an interrupted run must remain incomplete even when its compact view looks reassuring. Keep the original collected output under existing redaction rules, completion/exit receipts, complete failing-case identities, and missing/truncated-output status. Summaries are navigational aids. Do not transparently rewrite a native harness's conversation or treat summarized warnings/counts as the authoritative test inventory.

The inspected savings fields use JavaScript string length, despite their byte labels. They do not establish token savings or a lower total bill. DevManager should distinguish observed usage, estimated output reduction, and actual verified task cost.

### 5. Coalesce shared recovery without copying credential ownership

The refresh helper makes concurrent calls for one provider/token await a single operation. A synthetic two-caller probe executed the refresh function once and returned the same result to both callers. That is a useful coordination pattern. [Refresh deduplication](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/open-sse/services/tokenRefresh/dedup.js)

Apply it to equivalent readiness checks and wakeups already owned by DevManager's host. Two blocked goals should not stampede a limited provider when one retry time arrives. Scope shared results to the actual profile/account/configuration generation, preserve fair admission, and prevent an old probe from clearing a newer restriction. Do not import token-keyed caches or a second login/refresh manager to obtain this benefit.

## Reproducible probe record

These are the actual inputs and results of the isolated probes, not 9router's published test results.

| Probe | Input construction | Actual result |
| --- | --- | --- |
| Account-wide lock dominance | Call isModelLockActive for model probe with modelLock_probe set to now minus 60 seconds and modelLock___all to now plus 1 hour | false; an active account-wide restriction is masked |
| Failure-preserving output | Call compressMessages(body, true) on one tool message; join 30 lines of three spaces plus Compiling crate_0 through crate_29 plus v0.1.0, then the two lines below | Compiled 30 packages; neither failure line remains |
| Shared refresh | Call dedupRefresh twice with one synthetic provider/token key and an async callback held pending until both calls exist | 2 requests, 1 callback execution, equal returned results |

Exact appended failure lines:

```text
FATAL ERROR: Reached heap limit Allocation failed - JavaScript heap out of memory
Killed
```

The compression statistics were bytesBefore 948, bytesAfter 20, filter build-output, saved 928. Here the input was ASCII, so the string-length/byte distinction does not alter this particular observation. It still does not measure tokens.

## License and selective code reuse

The pinned [9router LICENSE](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/LICENSE) is MIT, copyright 2024–2026 decolua and contributors. It permits reuse and modification with the required copyright/permission notice retained. Licensing is not the primary reason to decline the full application; architectural fit and the behavioral differences above are.

If a later bounded module really saves implementation work, pin its source, trace its provenance and dependencies, retain applicable notices, and prove DevManager's contract against it. The small inspected selection/cooldown/deduplication helpers are better expressed using existing Rust host primitives than maintained as a separate JavaScript runtime.

The [README credits](https://github.com/decolua/9router/blob/eb712ca821f0ba6bc41043fbd14494c5af5daba5/README.md) identify CLIProxyAPI as an earlier implementation and RTK as the source of the compression port. If output filtering later warrants a component evaluation, examine original [RTK](https://github.com/rtk-ai/rtk) before porting its JavaScript derivative back to Rust. The separately inspected RTK [manifest at f7495466](https://github.com/rtk-ai/rtk/blob/f7495466edf7d2ed93d6ea4d2caab18dc8806b5a/Cargo.toml) declares version 0.42.4 and Apache-2.0; its [license](https://github.com/rtk-ai/rtk/blob/f7495466edf7d2ed93d6ea4d2caab18dc8806b5a/LICENSE) is separate from 9router's. This is a provenance lead, not approval to import RTK or a finding about the license of the historical code 9router ported. RTK was not executed or audited here.

## Changes incorporated into the plan

[SPEC amendment A7](../../spec/ai-orchestration/SPEC.md) adds R11–R12, V10–V11, D21–D22, and E11: hard capability eligibility, stable assignment identity, scoped shared restrictions, coalesced recovery, composed retry ownership, truthful partial completion, and evidence-preserving compact output. It extends current routing, recovery, evidence, and cockpit requirements rather than introducing another gateway or scheduler.

These are implementations of the existing determinism and verification values. One synthetic bug does not justify a permanent provider blacklist, a new universal rule after every failure, or claims that 9router is broadly unreliable. The existing holistic learning and independent-verification requirements remain the authority.

The spec pack remains ready for step 4. The seven new scenarios bring it to 160; all previous scenario IDs remain. No build docs, application changes, dependency imports, account changes, provider trials, or new runtime-acceptance claims were produced.
