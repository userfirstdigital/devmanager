# Grok Bot research and implications for DevManager

Date: 2026-09-08. Scope: current official product documentation, a pinned public bridge implementation, and DevManager's existing browser contracts. Research and specification changes only; no Grok Bot account, provider, connector, tunnel, or application was started.

Direction clarified by Robin in SPEC A6: use these lessons to improve DevManager's own product. The recommendation below incorporates that correction; A5's earlier adoption-first framing is superseded.

## Recommendation

Build the persistent lead experience into DevManager. Use Grok Bot's documented product patterns to improve how a user hands over work, how agents coordinate, how useful context survives, and how the lead reports results. Apply those lessons to the actual Command/DevManager requirements: permitted local execution, multiple repositories, existing policy and memory, precise ownership, recovery, and independently verified outcomes.

The [SPEC](../../spec/ai-orchestration/SPEC.md) incorporates the concrete improvements in A5 and the product-direction correction in A6. Reuse standard coding harnesses and existing host/tool capabilities inside DevManager, while building the coordination and user experience the agreement requires. No Grok Bot trial is needed to proceed with step 4. Our own live acceptance remains necessary to prove the result.

## What was identified and verified

“Grok Bot” here means the product documented on x.ai and docs.x.ai, rather than the general Grok chatbot, a similarly named GitHub account, or an unofficial reconstruction. The August 11 launch describes persistent cloud workers, memory, and background work. Product documentation is evidence of supported/intended behavior; its end-to-end completion language is not an independent reliability benchmark. [Official introduction](https://x.ai/news/introducing-grok-bot), [product overview](https://docs.x.ai/grok-bot/overview).

| Primary source | Relevant documented behavior | Implication for this plan |
| --- | --- | --- |
| [Engineering example in the PM guide](https://x.ai/bot/guides/grok-bot-for-pms) | A manager decomposes work, delegates to engineers using Cloud Agents, and checks outputs against the goal. The guide stresses environments with code, dependencies, and test access. | Give DevManager's lead responsibility for the whole outcome and appropriate worker environments. The guide's team size and claimed internal adoption are not our measured optimum or quality evidence. |
| [Create and manage Bots](https://docs.x.ai/grok-bot/bots) | Persistent roles retain preferences and summaries, but changing facts belong in their authoritative systems. Separate conversations can exchange context. | Supports our scoped memory and one accountable task lead. Retain current-source verification and canonical knowledge homes; a remembered conclusion cannot certify a new result. |
| [Skills and routines](https://docs.x.ai/grok-bot/skills-routines-and-automations) | Skills describe a reusable method; routines determine when to run it. Demonstrated workflows produce drafts that need decision rules, validation, and safe testing. | Make procedural learning explicit within our existing memory/skill workflow. Saving a useful method does not authorize recurring execution or make one example a universal rule. |
| [Message and collaborate](https://docs.x.ai/grok-bot/chat-and-collaboration) | Asynchronous messages can wake another Bot; user steering can redirect work. The guide recommends a clear owner to avoid duplication and noisy handoffs. | Tighten event origin, actionable wakeup handling, and lead-owned communication. Do not make workers freely spawn another planning loop through chat. |
| [Computer and apps](https://docs.x.ai/grok-bot/computer-and-apps) | Bots share one user-scoped computer, files, logins, and browser state, with separate screens. Connectors are preferred when suitable; human takeover handles authentication. Durable files survive normal recovery, but temporary setup is replaceable. | Qualify each actual execution environment and browser/session resource. Context separation is neither account isolation nor protection from competing UI input. Reuse setup only after checking its current state. |
| [Private networks](https://docs.x.ai/grok-bot/private-networks) | A Bot computer's network setup and delegated Cloud Agent networking are separate. | A lead reaching a service proves nothing about its worker's reachability. Bind environment and access evidence to the assignment that needs them. |
| [Security](https://docs.x.ai/grok-bot/security) | Auto Review uses a separate model and does not cover every side effect, including memory writes. The documentation distinguishes it from other access controls. | Keep DevManager's typed host authority and A4's versioned memory publication. A favorable model review cannot create permission, bypass a refusal, or validate its own new rule. |
| [Settings and notifications](https://docs.x.ai/grok-bot/settings-and-notifications) | Work, unread results, and input-needed states are distinct. Model selection is managed by the product; local settings are not automatically equivalent on every installation. | Separate progress from requests needing a human. Do not treat unofficial model-router forks as evidence of supported Grok Bot provider selection. |
| [Files and results](https://docs.x.ai/grok-bot/files-and-results) | Results should link to inspectable artifacts and distinguish observations, assumptions, completed actions, and unresolved work. | Reinforces existing evidence requirements. More chat activity and a final reassuring message do not establish completion. |

These pages were read on September 8; most carry August 11 or early-September update dates. No product internals, failover guarantees, or recovery protocol are inferred from unofficial decompilations, sandbox dumps, or marketing claims.

## What a public coding bridge proves—and does not

[Locum](https://github.com/HarjjotSinghh/locum) is the original author's public MCP bridge from Grok Bot to locally installed Claude Code/Codex CLIs. Its README describes asynchronous delegation with job handles and subsequent polling. That is a useful concrete example of reusing an existing coding harness behind a coordinating agent. It is third-party software, not an official Grok Bot integration maintained by the model providers.

Inspected revision: [c602f9049216ad149e9e649175a4cf2729892d07](https://github.com/HarjjotSinghh/locum/tree/c602f9049216ad149e9e649175a4cf2729892d07), committed August 22. The repository identifies an Apache-2.0 [license](https://github.com/HarjjotSinghh/locum/blob/c602f9049216ad149e9e649175a4cf2729892d07/LICENSE). Downloaded source files were read without execution.

The [source](https://github.com/HarjjotSinghh/locum/blob/c602f9049216ad149e9e649175a4cf2729892d07/server.py) provides a semaphore, assignment handles, result inspection, and cancellation. Its job registry is process memory (lines 175–176, 525–534); cancellation/timeout kills the direct subprocess without joining a demonstrated descendant tree in that path (453–523); its Codex route invokes `exec` (631–672). Those observations do not establish restart recovery, DevManager-compatible identity, complete child cleanup, or acceptance of the software being built. No Locum tests were run.

The author's [security model](https://github.com/HarjjotSinghh/locum/blob/c602f9049216ad149e9e649175a4cf2729892d07/SECURITY.md) explicitly distinguishes a permitted starting directory from confinement of shell access. This supports the distinction already in our SPEC between workspace ownership and enforced isolation.

**Disposition:** learn from the small assignment/result interface; do not install or adopt this bridge as DevManager's executor. Its current Codex control path conflicts with our adapter policy, and its lifecycle does not satisfy our durable ownership contract. The research did not evaluate or authorize subscription/account arrangements.

## Existing DevManager capabilities to reuse

The local [browser contract](../browser-automation.md) already describes typed recording/replay, logical tab aliases, ordered assertions, workspace/registration ownership, native-only Secret entry, and cancellation fencing. Relevant implementations exist in [replay.rs](../../src/browser/replay.rs), [workflow_mcp.rs](../../src/browser/workflow_mcp.rs), [gateway.rs](../../src/browser/gateway.rs), and [pane.rs](../../src/browser/pane.rs). These are starting points for integration, not evidence that the new multi-goal workflow has passed.

In particular:

- Reuse the native secret handoff. Do not add a chat/MCP operation that exposes secret values to the lead.
- Reuse browser recipes where the actual project permits that browser and the recipe meets the current journey. Do not build another recorder or use remembered runtime tab IDs.
- Keep Command's required Chrome/performance tools and measurement conditions. An existing embedded-browser recipe is not a substitute for a check whose contract requires a different browser.
- Preserve browser ownership through takeover, cancellation, provider replacement, and competing goals. A second Task or separate checkout does not inherently isolate cookies, focus, or pointer input.

The source review also reinforces that browser availability is optional for ordinary terminal work today. A software goal that requires browser acceptance needs that capability qualified explicitly; a terminal that still works cannot turn missing browser evidence into a pass.

## Changes incorporated into A5

These are our requirements derived from the comparison, rather than claims that Grok Bot implements DevManager's contract.

| Area | Change to the existing specification | Observable coverage |
| --- | --- | --- |
| Actual tools and environments | Choose the least costly permitted tool that supplies the required evidence; verify source/service/browser/account reachability in the actual worker environment. An authorization refusal cannot be bypassed with another tool. | R09–R10 |
| Human-only access and shared UI | Reuse qualified access and native secret entry; bind authentication to the intended account/environment. Coordinate browser profiles and mutable UI surfaces across goals, and fence queued input on human takeover. | H10, W07–W08 |
| Reusable procedures | Consolidate a validated method into the existing project skill/playbook when useful, with applicability, inputs, current preconditions, checks, and failure behavior. Reuse the method with fresh evidence; do not copy a previous run's authority, acceptance ticks, or schedule. | L09–L10 |
| Actionable events and human attention | Retain event origin and exact run lineage, coalesce redundant wake notifications while preserving durable facts, prioritize current user controls, and prevent worker progress/chatter from repeatedly waking a model or the user. | D19–D20, U04 |
| Product reference — clarified by A6 | Translate useful Grok Bot patterns into DevManager's own behavior and checks. Keep any external trial optional; reuse qualified harness/tool capabilities within DevManager. | Existing behavioral coverage below; standard-agent baseline retained |

The full source agreement, independent audit/repair loop, shared scheduling, cause-based test repair, memory provenance, anti-oscillation, exact provider identity, budgets, and process ownership already cover much of the relevant ground. Those sections are retained rather than duplicated as “Grok-style” features.

## Product patterns to deliver inside DevManager

The following mapping includes both earlier requirements and A5's additions. It describes DevManager's required behavior; it does not claim either product has passed these checks.

| Pattern | DevManager behavior | Existing acceptance |
| --- | --- | --- |
| A persistent lead owns the outcome | Hand over a project/spec/problem in the Task conversation; the lead carries investigation, build, audit, testing, and repair through completion and recovers after interruption. | N01–N12, D09–D18, C01–C07 |
| Several workers collaborate without human routing | Separate task leads share host scheduling; each lead selects permitted specialists, receives results, resolves routine questions, and coordinates dependencies. | R01–R10, Q01–Q04, V01–V09 |
| Useful context accumulates | Project/user memory survives provider replacement and is retrieved by scope, with current-source checks and coherent learning instead of appended anecdotes. | M01–M08, L01–L08 |
| A successful method becomes reusable | Maintain a canonical project skill/playbook or compatible browser recipe; adapt to current inputs and verify the new result. | L09–L10 |
| The lead can work in the actual tools | Prepare and qualify worker access and required verification surfaces, coordinate shared browser state, and handle user-only authentication once through a secure path. | H09–H10, R09–R10, W07–W08 |
| The user gets a focused account of progress | Present useful results and material questions through the lead, preserve detailed evidence, and keep steering responsive during worker activity. | D19–D20, U01–U04 |

## Compare to improve the implementation

The required standard-agent baseline tests whether DevManager reduces coordination effort around the same underlying capabilities. An optional external-product comparison can start with one representative Command spec handoff and one maintenance problem, using equivalent permitted source, policy, memory, tools, and acceptance. Expand to the existing full workload corpus before making a broad reliability claim. Measure user coordination, valid final evidence, failure recovery, elapsed work, and reported usage.

| Reference | What it can teach the implementation |
| --- | --- |
| Standard Claude Code/Codex with the existing workflow | Which capabilities can DevManager reuse directly, and where does its host need to coordinate continuity, ownership, and verification? |
| Grok Bot through documented product capabilities, optionally exercised live | Which interaction, handoff, memory, or tool-use behaviors would improve DevManager's accepted workflow? |
| DevManager's bounded host integration | Do the resulting capabilities meet our actual goals with less human coordination and trustworthy evidence? |

Grok Bot's documented cloud and local execution paths need to be evaluated as those actual paths. A cloud-only example cannot qualify local Command behavior or DevManager's native Windows/Linux checks. Lack of a usable account, tool, or permitted route is an unevaluated constraint, not a measured failure or success. This research did not establish a supported embeddable Grok Bot control API meeting our host contract; general xAI model API availability does not establish one.

Use qualified existing harnesses, tools, and components where they satisfy DevManager's implementation needs. Build the remaining product behavior in the current host and native interface. External comparisons can improve that design; they do not make adopting another orchestration product the deliverable or gate step 4. A material change to this product direction would require a new user instruction.

## Scope kept out of this release

Cloud-machine provisioning and work while the local host is off remain parked. So do scheduled/event-triggered maintenance campaigns, a bot marketplace or manually curated employee roster, a new screen recorder, and unrestricted agent group chats. Useful procedure capture is in scope; automatically running it tomorrow is separate authority and scheduling behavior.

This is not a recommendation to add another model-based approval layer for every routine action. Retain current permissions and do authorized work. The point of the stronger access/attention contract is fewer avoidable human interruptions and truthful handling of the few steps that genuinely need the user.

## Verification and limits

Official pages and the pinned public source were inspected, existing specification coverage was compared, and A5 was integrated into the product agreement and step-4 handoff. A6 incorporates Robin's correction to the recommendation and comparison purpose. No implementation, live Grok Bot trial, application test, authentication change, or external write was performed. DevManager's runtime reliability, comparative value, and actual provider/browser qualification remain to be demonstrated.
