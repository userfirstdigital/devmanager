# Command as the primary orchestration workload

Inspected 2026-09-08. This is a read-only inspection of the available Command working trees and guidance, to inform DevManager's [orchestration SPEC](../../spec/ai-orchestration/SPEC.md), amendment A4. No Command files, memory, policies, databases, or running processes were changed. No application commands, tests, provider workers, or production probes were executed. File/manifest evidence below is not runtime qualification or a clean-tree verification claim.

## The actual project

Command is the primary maintenance and feature-development workload; DevManager and other projects must remain supported. It is larger than an API/web example. These repository roots and HEADs were verified with read-only Git queries from each directory:

| Directory | HEAD observed |
| --- | --- |
| Workspace root | `1ea64253da4f5ef1f0d154dc40c7d21e0b7cc640` |
| `api/` | `d5b1d80677744d59741e687111b446cdc3c99214` |
| `web/` | `ccdc73471346edfba591ed392040c6c8c4b08205` |
| `portal/` | `c423ce554ffda04ec2d67dcce866287b88885d6b` |
| `agent/` | `07a789f9e0e8fc6a64c31b76302fe4523a49eb51` |
| `service/` | `ffe47d94436647f9a83cdda242d6b08d3ffdbce1` |
| `updater/` | `0d9fb4dd8659516cca01673942745bd4a44bb0bc` |
| `extension/` | `b291111346bccdf9595b246027e6350239589a9e` |

The root guidance (`../../../../command/CLAUDE.md`; local snapshot) identifies the root repository as the home for workspace specs, policy entry points, memory, and TODO/DONE history. It imports workspace policy from the API repository. The code repositories remain separate. The inspected `extension/` has its own Git root; the September 4 Context Pack (`../../../../command/specs/CONTEXT-PACK.md`; local snapshot) says it is not a repository. That is a concrete stale fact to revalidate during adoption, not a reason to discard the whole pack or silently copy its older topology.

Manifest inspection establishes these different execution surfaces:

- API (`../../../../command/api/package.json`; local snapshot): TypeScript, Express, Sequelize/PostgreSQL conventions, Vitest, and a build chain including generated authorization evidence and specialized gates. Its `test` script enters watch mode; `test:unit` and `test:ci` are distinct commands.
- Staff web (`../../../../command/web/package.json`; local snapshot), portal (`../../../../command/portal/package.json`; local snapshot), and extension (`../../../../command/extension/package.json`; local snapshot): separate React packages with different builds and checks. Staff web also has a source-backed page registry and a distinct TypeScript checking path.
- Desktop agent (`../../../../command/agent/package.json`; local snapshot): React with Tauri; service (`../../../../command/service/go.mod`; local snapshot) and updater (`../../../../command/updater/go.mod`; local snapshot) are Go modules with separately declared toolchains.

The Context Pack maps repository names such as `COMMANDIT-API` to the actual `api/` directory. It records a previous stray `COMMANDIT-API/` tree created by treating that alias literally. The orchestrator should resolve declared names against verified roots and preserve cross-repo layout; it must not assume a root Git status covers application code.

## Existing memory and process

Command already separates long-lived knowledge from task status. Its memory index (`../../../../command/.memory/MEMORY.md`; local snapshot) places durable causes, constraints, and preferences in `.memory/`, current work in `TODO.md`, and closed work in `DONE.md`. The inspected workspace setting points Claude's `autoMemoryDirectory` at this `.memory/` directory. Its index says general preferences moved to user-level rules; this inspection did not validate those external rules' contents.

The workspace playbook (`../../../../command/api/.claude/playbooks/workspace.md`; local snapshot) and axe procedure (`../../../../command/.memory/feedback_sharpen_the_axe.md`; local snapshot) already ask for holistic assimilation: search existing knowledge, merge related causes, rewrite the canonical rule, remove superseded guidance, and keep the index small. The axe memory still points at an older root-file location for its trigger, while the current import chain locates the ritual in the workspace playbook. The lead needs to verify references before maintaining them, rather than creating a second rule because a remembered location moved.

The workspace playbook names performance hypotheses as VALIDATED, DISPROVEN, or UNVALIDATED and requires consulting TODO/DONE before speed work. It records repeated investigation of disproven theories and confusion between cold and warm measurements. That supports retrieving negative results with their conditions, and reassessing them only when relevant inputs or evidence change.

The speed playbook (`../../../../command/api/.claude/commands/z-speed-up-page.md`; local snapshot) defines a specific cold-load target: under 300 ms, production assets, empty HTTP/module caches with valid authentication, fixed network/CPU conditions, and five navigations with median and spread. It names Chrome tooling and guards that establish the actual conditions. DevManager's generic warm-response defaults must not replace this project-specific acceptance. Instrument/setup capability must be qualified; this inspection did not establish that the current host can perform it unattended.

The error-repair playbook (`../../../../command/api/.claude/commands/z-fixerrors.md`; local snapshot) already groups defects, checks evidence freshness, forbids making gates green by moving baselines, and routes review through canonical shared procedures. The API guide (`../../../../command/api/AGENTS.md`; local snapshot) further establishes boundary testing, failure-identity comparison, generated evidence, and scoped database/authorization requirements. Memories about these domains are leads to verified sources; they do not authorize a production action or substitute for current gate results.

Some current procedures differ from the proposed orchestrator's defaults: Command permits a shared-checkout workflow, its delegation playbook describes wider concurrency and provider-specific control paths, and some workflows require specific production read-only evidence. DevManager's host limits, provider identity rules, ownership requirements, and current user authority still govern what its integration may execute. A project preference does not waive a host restriction. Read-only investigation or queued writers can continue when overlapping writes cannot be safely isolated under the effective policy.

Command also has an AI-employees memory feature note (`../../../../command/.memory/project_ai_employees_memory.md`; local snapshot). That concerns Command's application functionality and contains dated implementation/status claims. It is not DevManager's developer-lead memory store, and this inspection did not verify those claims. Reuse useful ideas about curation without making development orchestration depend on Command's production data or AI-employee subsystem.

## Consequences for the specification

1. **Memory has three scopes.** Run facts remain with the goal and execution journal. Project knowledge references its actual canonical files. User-wide memory holds applicable preferences and generalized practices, with provenance and scope; it does not copy Command's data, deployment state, or RLS-specific rules into DevManager.
2. **Learning is a controlled consolidation.** Observations and contradictory evidence accumulate by cause. General workflow changes require corroboration and evaluation, not one failed attempt or one worker's recommendation. Factual corrections and explicit user corrections can be incorporated immediately with their evidence. Rewrite the relevant guidance coherently and retain the reason for an earlier decision.
3. **One lead context per goal.** Multiple task leads share scoped knowledge and host scheduling, not one conversation containing every project's private details. Coordinate overlapping source, integration, test resources, memory edits, and account capacity across all goals.
4. **Command supplies real acceptance.** Include maintenance and feature work across its actual repositories, an applicable performance/gate workflow, existing memory adoption, and concurrent-task conflicts. DevManager supplies a different stack and host-policy boundary to prove portability and prevent project-rule leakage. Controlled fixtures supply deterministic failures; they do not replace live project/provider evidence.

These are reflected in SPEC A4. The research identifies constraints and examples; it does not apply learning edits to Command, approve an exception to its rules, or establish that the proposed orchestration is implemented.
