# Headroom, Dify, and OpenHands — reuse review — 2026-09-08

## Recommendation and scope

Bring the useful behavior into DevManager's existing host and standard-harness workflow. Headroom supplies the most relevant compression component candidates; OpenHands supplies the strongest software-agent execution reference. Dify's separately packaged Graphon engine is a better code-reuse candidate than its full application.

Do not add these complete applications as mandatory services. Their components have different responsibilities and authority models. A second event store, credential manager, workflow engine, or model loop would need a demonstrated advantage over the DevManager foundation already present.

Robin confirmed that “diffy” means Dify, the AI workflow platform at langgenius/dify. Diffy’s unrelated visual-testing plugin was briefly inspected while clarification was pending; it is excluded from this recommendation and the spec amendment.

Inspected pinned source, manifests, licenses, documentation, and selected relevant tests. Downloaded individual public files into temporary research directories; installed no packages, models, containers, or plugins. Executed selected Headroom/OpenHands source definitions with synthetic fixtures, described below. No application, model-backed compression, live provider, or full upstream test suite was run. External skill files were examined as product artifacts; their installation/execution instructions were not applied.

| Source | Inspected revision/version | License at that revision |
| --- | --- | --- |
| [Headroom](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/pyproject.toml) | e67b3c8a29443a60d6b0018fb22f525c5cd7e709; Python package 0.37.0; Rust core 0.1.0; September 6 | [Apache-2.0](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/LICENSE), with [NOTICE](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/NOTICE) |
| [OpenHands SDK](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/pyproject.toml) | 56b016e7f2d028a31ab59af4424d3172ac0b5aec; SDK 1.45.0; September 8 | [MIT](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/LICENSE) |
| [OpenHands application](https://github.com/OpenHands/OpenHands/blob/6f240ccbdced01e76bc2aea1bec6cc761460b618/README.md) | 6f240ccbdced01e76bc2aea1bec6cc761460b618; September 8; boundaries/license inspected | [MIT](https://github.com/OpenHands/OpenHands/blob/6f240ccbdced01e76bc2aea1bec6cc761460b618/LICENSE) |
| [Dify](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/README.md) | fc062bfc6d1611af2c4d80fccfe511b899506868; September 8 | [Modified Apache-2.0 with additional conditions](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/LICENSE) |
| [Graphon](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/pyproject.toml) | 11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5; tag v0.7.0, matching Dify's pinned dependency | [Apache-2.0](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/LICENSE) |

## Headroom: retain originals, reduce the working view

### Useful design and code

Headroom offers a compression library, proxy, and explicit MCP tools. Its Compress-Cache-Retrieve approach replaces bulky content with a smaller representation and a reference to the original. The original can be retrieved when needed. It also treats tool calls and their results as related units and tries to avoid changing the older prompt prefix merely to compress the newest content. These are useful ideas for large test inventories, logs, search results, and long-running goals. [CCR design](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/wiki/ccr.md), [MCP interface](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/wiki/mcp.md), [tool-pair handling](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/crates/headroom-core/src/transforms/safety.rs)

There is actual Rust source to evaluate, rather than a need to translate the entire Python project. The core separates transforms that reformat content from transforms that offload content and return a retrieval key. The pipeline can skip a failed transform instead of destroying the input. That separation is worth adapting: a compact view must carry the facts needed to recover omitted detail and explain failures. [Transform contracts](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/crates/headroom-core/src/transforms/pipeline/traits.rs), [pipeline](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/crates/headroom-core/src/transforms/pipeline/orchestrator.rs)

The inspected core is not a tiny dependency. Its manifest declares 34 direct dependencies and enables its ML feature by default, including ONNX-related components. Disabling default features removes optional ML dependencies, but tokenization and other dependencies remain. An in-process library or selected routine is a plausible candidate only if it reduces implementation/maintenance work and fits the actual build targets. Importing the full proxy is unnecessary for the current plan. [Rust manifest](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/crates/headroom-core/Cargo.toml)

The highest-value integration point is a host-owned diagnostic/context view with explicit retrieval, using existing evidence storage and supported tools. It is not transparent interception of native Claude/Codex conversations. The existing [DevManager provider boundary](../../src/providers/adapter.rs), [capability qualification](../../src/providers/capabilities.rs), and [A7 evidence requirements](../../spec/ai-orchestration/SPEC.md) still apply.

### What must be stronger for DevManager

Retrievability has a lifetime. The Rust CCR defaults are 1,000 entries and a 30-minute idle TTL, capped at eight times that TTL. Its interface permits missing or expired content. The Python default backend is volatile memory, although other backends exist. That is a cache contract, not the durable evidence contract needed by a project that resumes tomorrow. [CCR contract](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/crates/headroom-core/src/ccr/mod.rs), [default Python backend](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/cache/backends/memory.py)

DevManager should bind each retrieval reference to the owning goal/project, immutable evidence version, permitted reader, and source provenance. A content hash is an identifier, not authorization. Retain the canonical source for as long as active work depends on it. If a cache entry expires, recover the same source version; reading today's changed file must not silently substitute for yesterday's evidence. If that original cannot be recovered, report the gap and re-establish affected evidence. Do not make unlimited retention a requirement: release unreferenced material under the actual retention policy.

“Available if the model asks” does not guarantee the model will know it needs to ask. A partial search result must be labeled partial; a complete failure inventory or exact code inspection cannot be replaced by heuristic sampling. Some transforms described as reformats include more than whitespace removal. Source text, diffs, instructions, identifiers, and exact test evidence need their own preservation contract. Use current source and existing parsers where they already solve the problem; do not import a compression model to minify JSON.

The Python store records an eviction without retrieval as a compression-success feedback event. That may be a useful heuristic for adapting compression, but it does not show whether the resulting answer was correct: the agent may have missed the need for omitted information. DevManager's learning and routing decisions must use independently verified task outcomes and retain unknown outcomes. [Eviction feedback](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/cache/compression_store.py#L794-L831)

Likewise, fewer visible tokens do not automatically mean lower total cost or latency. Retrieval can add turns, summaries can require model calls, and prompt-prefix changes can reduce cache reuse. Compare the same tasks and acceptance with and without the optimization, including retrieval, compaction, elapsed time, and reported usage where available. Do not publish a universal savings percentage from a favorable structured-data example.

### Sharpening the axe

Headroom Learn correlates failed actions with later successful corrections, normalizes evidence across agent formats, and can use existing learned patterns as input to analysis. That is more useful than merely counting error messages. Its source also detects repeated successful-but-unhelpful retrieval loops. Those observations can inform DevManager's existing cause-based synthesis. [Learn design](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/wiki/learn.md), [models](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/learn/models.py), [analyzer](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/learn/analyzer.py)

The writer maintains generated marker sections. Its merge gives a newly generated section precedence over an older section with the same heading and carries unmatched old sections forward. This prevents simple duplicate blocks, but is not by itself holistic reconciliation of guidance elsewhere in the project. The analyzer has access to prior generated blocks; that still does not prove all competing canonical guidance was reconciled. [Writer](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/learn/writer.py#L162-L193)

Keep DevManager's stronger existing rule: find the actual authoritative homes, compare prior rationale and counterexamples, synthesize a coherent change across affected sections, validate it, and obtain independent review when substantive. A repeatedly denied command is evidence to investigate, not automatic proof of a universal preference or permission change. No new parallel learned-rules file is required.

## OpenHands: reuse execution contracts, preserve harness ownership

### The reusable parts

The SDK is the relevant codebase for agents, conversations, events, tools, workspaces, and server access. The application consumes those capabilities. The inspected SDK includes both a direct model/tool agent and an ACP agent that delegates execution to a compatible external agent server. It is therefore too broad to describe OpenHands as only another custom model loop. [SDK boundaries](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/README.md), [ACP adapter](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/agent/acp_agent.py#L1-L15)

That ACP path is a useful reference for supporting provider-owned execution through structured interfaces. It is not permission to bypass DevManager's current hook-based identity or forbidden adapter modes. An OpenHands-backed profile would be a new integration requiring its own permitted configuration, identity, input/result, cancellation, environment, and live-role qualification. It is not required to deliver the first release.

The most directly applicable concepts are:

- Keep the original event history and derive a bounded context view from it. A condensation event identifies which events the view replaces; it does not delete the underlying events.
- Respect action/result and batch boundaries during context reduction. Asynchronous results can arrive in a different order from their calls.
- Distinguish a completed provider turn, a pending confirmation, interrupted execution, and a complete user goal.
- Propagate cancellation to queued and cooperative in-flight tools, while retaining the actual ownership needed to establish settlement.
- Detect repeated action/result patterns and context-error loops using evidence, with bounded responses rather than repeated generic “continue” messages.

These are implemented or illustrated in the [condenser architecture](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/context/condenser/README.md), [condensation events](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/event/condenser.py), [tool-call matching](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/context/view/properties/tool_call_matching.py), [cancellation token](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/conversation/cancellation.py), and [stuck detector](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/conversation/stuck_detector.py). The isolated probes below checked selected deterministic behaviors, not complete integration.

### Boundaries that do not transfer unchanged

The condenser can summarize older events and has a harder reset path for context-window failure. This is native SDK behavior. DevManager must let a native harness own its own context handling; for host-controlled handoffs, reload current mandatory guidance and unsettled work from authoritative sources instead of trusting a summary of a summary. Reducing the context shown to a model cannot authorize reducing the historical stream consumed by durable recovery. [Summarizing condenser](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/context/condenser/llm_summarizing_condenser.py), [DevManager journal rules](../../AGENTS.md)

The ACP adapter emits a FinishAction to delimit a completed remote turn. This is a concrete example of why an adapter's “finished” is not a SHIPPABLE verdict. An unfinished goal still needs continuation or its specific pending condition. A budget-exhausted, deferred-question, or handled-error result must retain its own meaning.

The delegate implementation bounds child count, persists child conversations under a parent directory when persistence is configured, and aggregates their metrics. It also uses the parent's working directory for child work, waits for delegated threads, and has its own confirmation behavior. Close failures are logged by its helper. These choices are not equivalent to DevManager's disjoint writable ownership, fair cross-goal admission, permission intersection, or verified process-tree cleanup. Copying that executor would not supply those contracts. [Delegation source](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-tools/openhands/tools/delegate/impl.py)

MIT permits selective reuse with notices. The SDK is a real candidate when an application wants OpenHands to own agent execution. For the current Rust host and stock-harness product, take the event/view and lifecycle ideas and their counterexamples first. Do not add the Python SDK as a second authoritative coordinator merely to avoid implementing a small amount of glue. It declares Python 3.12+ and an agent/MCP/model integration dependency stack; this is an integration decision, not a utility import. [SDK manifest](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/pyproject.toml)

## Dify and Graphon: learn from durable branching and resumption

Dify is a visual AI workflow/application platform. Its current API pins graphon==0.7.0. The engine has moved into a separate project; reviewing only older api/core/workflow code would miss that boundary. The inspected Graphon tag matches that exact dependency. [Dify dependency](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/api/pyproject.toml), [Graphon manifest](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/pyproject.toml)

Useful Graphon mechanisms include explicit node error strategies, queued work, serialized runtime state with versioned snapshot schemas, and pause handling that drains relevant in-flight events and snapshots execution frames. Its queue interface also calls out that an approximate queue size cannot safely decide whether execution is complete. [Error handling](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/src/graphon/graph_engine/error_handler.py), [snapshot types](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/src/graphon/runtime/graph_runtime_state.py), [dispatcher](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/src/graphon/graph_engine/orchestration/dispatcher.py), [queue contract](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/src/graphon/runtime/ready_queue.py)

Dify includes a focused test where two human-input branches are answered across separate resumptions and their join must wait for the second answer. That maps well to a software goal waiting on one product decision while independent work proceeds. Preserve completed branch results and pending dependency identities across restart; neither a queue becoming empty nor one branch succeeding satisfies a join requiring both. This source test was inspected, not executed. [Parallel resume test](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/api/tests/unit_tests/core/workflow/graph_engine/test_parallel_human_input_join_resume.py#L285-L404)

Its Agent lifecycle documentation separately distinguishes an agent run from the outer workflow: an agent run can finish with a deferred human tool call while the workflow still needs input. The resumption code correlates results using the original tool-call identity. DevManager should adopt the distinction, while retaining its own rule that missing correlation or elapsed time cannot become approval or an invisible fresh session. [Agent lifecycle](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/dify-agent/docs/dify-agent/concepts/run-lifecycle/index.md), [human-result mapping](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/api/core/workflow/nodes/agent_v2/ask_human_resume.py)

Graphon's generic error strategies include returning a default or following a failure branch. Those can be legitimate business-workflow behavior. For DevManager, handling a failure must not turn a required failed audit/check into verified acceptance. Likewise, the inspected retry handler waits synchronously; adopting its code would require reconciliation with DevManager's responsive host and cancellation contract. These are fit differences, not a claim that every Dify workflow is incorrect.

The full Dify license has additional multi-tenant and frontend branding/copyright conditions; do not label it plain Apache-2.0 or assume every source directory has the same reuse terms. Graphon has a separate standard Apache-2.0 license. This is a material reason to examine the extracted component if code reuse is needed, without assuming how Dify's tenant definition would apply to a future DevManager deployment. [Dify license](https://github.com/langgenius/dify/blob/fc062bfc6d1611af2c4d80fccfe511b899506868/LICENSE), [Graphon license](https://github.com/langgenius/graphon/blob/11e2dee8cbd6dc2e6bf1c2059d9bbf4d0437ebe5/LICENSE)

Graphon is a credible reusable engine for a Python workflow backend, but its package includes broad document/model dependencies and its own execution state. DevManager already has the host/event/outbox authority; adding a second engine is not the smallest implementation of these requirements. Use its snapshot, branch/join, and outcome semantics as references. Do not require a visual DAG editor, new DSL, or user-authored workflow for a natural-language goal.

## Code reuse decisions

| Candidate | Decision for this release | What must be proven before any selective import |
| --- | --- | --- |
| Headroom Rust transform/CCR/tool-pair routines | Most relevant source candidates for a bounded context/evidence utility; prefer existing host storage/parsers and a narrow implementation over the default full core | Actual dependency/platform cost, preservation contract, scope-bound retrieval, cache-miss recovery, and measured task benefit |
| Headroom proxy and automatic learned-section writer | Take the ideas; no mandatory import or transparent conversation interception | Existing native ownership and canonical policy integration would need to remain intact |
| OpenHands SDK/ACP and agent-server | Keep as a separately qualified future harness/integration candidate; use event/view/lifecycle code and tests as references now | Current hook identity, supported mode, authority, independent verification, exact process settlement, and resource/account limits |
| Dify application | Lessons from workflow state and operator visibility; no source import | Its additional license conditions and application-wide integration surface are separate considerations |
| Graphon 0.7.0 | Better Dify-family component candidate; adapt branch/resume/error semantics within the existing host | Avoid competing state/queue authority; measure whether adopting a Python engine actually saves work |

Retain applicable copyright/license/NOTICE material for copied or adapted substantial code, and inspect transitive assets/models separately. No code or dependency was imported by this research. Optional component evaluations do not gate step 4; its build docs must settle any selected dependency before a code phase relies on it.

## Isolated probe results

Selected unmodified definitions were compiled from the pinned Python source using AST selection, with synthetic event/store classes. Imports, model calls, SDK startup, network, and application execution were excluded. These are function/contract probes, not end-to-end library tests.

| Source behavior | Synthetic input | Observed result |
| --- | --- | --- |
| Headroom marker resolution | A valid marker with a fixture store returning original content, then the same marker with a missing entry | Returned the original text on a hit; retained the marker with an explicit unresolved entry-not-found message on a miss |
| OpenHands tool-pair boundaries | action-a, action-b, result-b, result-a | Only positions 0 and 4 were valid manipulation boundaries; the overlapping pairs stayed together |
| OpenHands duplicate observation | The same sequence plus a second result for call-a | Rejected with KeyError in the boundary method; duplicates must be reconciled before serialization |
| OpenHands condensation projection | Summarize the four tool events while retaining latest-user | Produced summary-1, latest-user; the original five-event sequence remained unchanged |

The relevant exact functions are [resolve_markers_in_text](https://github.com/headroomlabs-ai/headroom/blob/e67b3c8a29443a60d6b0018fb22f525c5cd7e709/headroom/ccr/marker_resolution.py), [ToolCallMatchingProperty.manipulation_indices](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/context/view/properties/tool_call_matching.py), and [Condensation.apply](https://github.com/OpenHands/software-agent-sdk/blob/56b016e7f2d028a31ab59af4424d3172ac0b5aec/openhands-sdk/openhands/sdk/event/condenser.py). This does not establish compression accuracy, cost savings, full SDK correctness, or live recovery reliability.

## Incorporated into the specification

[SPEC A8](../../spec/ai-orchestration/SPEC.md) adds G08, M09, D23, and E12. It tightens existing continuation and holistic-learning requirements within the current host and canonical memory model. The four additions cover preserved dependency joins, scoped and durable retrieval references, bounded context recovery, and outcome-based optimization evaluation. All 160 earlier scenario IDs remain, for 164 total.

The readme handoff, autonomy review, and Context Pack point to this research. The pack remains ready for step 4. The new checks are requirements, not implemented or live-verified capabilities.

Document validation passed across all ten specification/research Markdown files: local links resolve, tables and code fences are consistent, all 160 prior scenario IDs remain, and the four additions produce 164 unique scenarios. No build directory was generated. Git whitespace checking also passed; the separate document checks cover these currently untracked files.
