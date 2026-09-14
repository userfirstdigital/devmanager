Before anything else, read spec/CONTEXT-PACK.md — it defines the commands, conventions, and constraints this doc assumes.

# Build: DevManager autonomous software goals

Status: COMPLETE BUILD PACK — 2026-09-12. Implementation: NOT STARTED BY THIS PACK.

## Why

Give one lead a finished spec, a project outcome or a software problem and have it take care of investigation, build documents, implementation, fresh audit, testing, repairs and verified delivery. Command maintenance and features are the primary real workload, with DevManager and other projects using their own rules and design language. Each goal owns a separate lead context while the existing host controls shared resources, effect authority and durable recovery. The native Task experience shows current progress and only the decisions the lead cannot settle from existing authority. Delivery includes actual user/operator behavior and current evidence, not a provider's assertion that it finished.

Robin authorized locking and complete generation on 2026-09-12. [SPEC.md](../SPEC.md) is LOCKED; its body and all 197 scenarios are unchanged. [UX-SPEC.md](../UX-SPEC.md) remains normative. All 17 phase documents and [TEST-PLAN.md](TEST-PLAN.md) are supplied; no NEXT, design proposal or research handoff is needed to implement a phase. No BLOCKERS remain in this document pack. Live provider, platform, project and appearance qualification are acceptance work, explicitly not claimed to have passed during generation.

## Repository and implementation boundary

The implementation is in **this DevManager Git repository**, a Rust/GPUI application with an existing host, typed IPC, provider adapters and a web/Connect client in the same checkout. There is no new `/api` or `/web` service contract and no external orchestration server. Target projects can have independent nested Git roots; their topology is data admitted by WorkspaceService, not the structure of this application. A phase commits only its scoped files in every repository it actually changes, using the actual branch and applicable landing policy. Do not assume `master`, adopt foreign edits or initialize target repositories merely because a prompt template assumed them.

The inspected source base is `VisualDevManager` at `b788b14307890e0a986d8f11aae4bfb7d340eb64`, with existing uncommitted work. [generation-checks.json](generation-checks.json) records source and document hashes, not a clean-checkout or runtime claim. Reconcile changed source at implementation start. Existing host/provider invariants in the Context Pack and AGENTS.md remain binding. The historical automatic-prompt-chain exclusion is superseded only to the extent explicitly required by the locked goal workflow; the provider still owns its model loop.

## Settled implementation choices

| Choice | Result and rationale |
| --- | --- |
| Orchestration boundary | Extend existing command decisions, event journal, outbox, process manager and native action/query path. One host authority prevents two independent owners from executing the same effect. |
| Agent harness | Use qualified installed Claude/Codex sessions and permitted workers. Use the current per-process rmcp/loopback registration pattern for a separate goal-scoped MCP tool surface; do not introduce another SDK/model loop. |
| Identity | Existing UUIDv7 IDs and provider SessionStart rules stay authoritative. Goal/attempt/invocation IDs supplement, rather than replace, provider identity and runtime fences. |
| Persistence | Store new typed goal facts in the existing `events` stream and rebuild their indexed projections in the same SQLite database/transaction. Canonical requirements, plans, guidance and human tracker content stay in files; host execution receipts are operational facts, not another copy of those documents. |
| Wire compatibility | Introduce protocol **major 2, minor 0**, preserving existing numeric capability bits and adding GoalOrchestration at bit 20. New durable event variants are not safe for v1 exhaustive decoders; explicit version mismatch is preferable to dropping events or pretending old clients applied them. Ship matching native/host/CLI/Connect/WASM artifacts together. Existing manual Task behavior remains. |
| Human interface | One New goal action and the current Task conversation; Plan/Agents/Checks/Knowledge use the existing contextual detail slot. Use shared GPUI components/tokens and both reference manifests; no permanent extra sidebar. |
| Plan representation | An internal exhaustive typed dependency model indexes canonical documents. It is not a workflow language, plugin registry or user-authored graph editor. Dispatch requires settled concrete inputs. |
| Memory | Adopt existing canonical project homes. Only absent homes are initialized at the paths below. Host indexes and harness views are rebuildable; neither becomes authority. |
| Third-party code | No external runtime, package or copied source is required for these phases. Reuse DevManager's current rmcp, SQLite, browser, workspace and provider primitives. Research-informed behaviors and independently authored regression fixtures cover the useful lessons; this avoids importing unqualified scanners or restrictive runtimes. |
| Concurrency | Two executing goals by default, two top-level runtimes each including lead, four globally. One host admission owner intersects resource/account/project limits. |
| Recovery | Native-owned retries stay native. All host retry layers share at most two retries of the same unchanged assignment; completed repairs use the separate four-per-cause policy. |
| Completion | Three ordered obligation classes: implementation/outcome → independent review → final delivery. File publication, effect settlement and learning disposition have explicit receipts; a passed reviewer cannot close a pending delivery. |

## Shared types and encoding

These are **new implementation contracts**, not APIs already present. Use Rust structs/exhaustive enums, serde validation and existing typed domain IDs. Every named reference below is checked at the host boundary; no free-form dictionary controls execution. New record schemas are version 1, unknown fields/discriminants fail closed, and required fields are never guessed from missing data. JSON uses snake_case; new tagged unions use `{ "kind": "variant", "value": { ... } }`. Existing outer `ClientRequest::{Command,Query,...}`, `CommandEnvelope` and receipts retain their shapes. UUIDs use the existing JSON-string/MessagePack-binary encoding; enforce UUIDv7/RFC variant and nonzero new IDs. Time is UTC epoch milliseconds; sequence/revision/epoch numbers are unsigned 64-bit values. Digests are lowercase 64-character SHA-256 hex of retained exact bytes. Native/CLI display conversions must not round sequence values.

| Type | Required fields and constraints |
| --- | --- |
| `GoalKey` | `goal_id: GoalId`, `run_id: GoalRunId`, `task_id: TaskId`. New typed UUIDv7 IDs; GoalId survives follow-up runs, RunId never reused. |
| `GoalFence` | `key: GoalKey`, `goal_revision: u64`, `action_epoch: u64`. Envelope `expected_task_revision` is additionally required for mutations of an existing goal. Source/plan/permission versions are checked by the requested action. |
| `InvocationKey` | `root_id: InvocationId`, `id: InvocationId`, `parent_id: Option<InvocationId>`, `occurrence_id: InvocationId`, `method_id: String`, `method_version: SourceRef`, `plan_version: Option<SourceRef>`, `predecessor_event_id: EventId`. Identity is this entire lineage, never method name/input equality. Root's parent is absent; all child parents must exist in the same run. Bootstrap preparation/lead invocation has no plan version until a plan exists; every build/check/fix/delivery child requires the current Some(plan). Publishing a plan creates new child invocation lineage and never retroactively edits the bootstrap identity. |
| `RuntimeBinding` | `agent_session_id: AgentSessionId`, `resource_id: ResourceId`, `runtime_generation: u64`, `attempt_id: AttemptId`, `invocation: InvocationKey`, `permission_revision: u64`, `provider_session_id: Option<ProviderSessionId>`. The provider ID binds only from the existing correlated hook; unsupported first-input exception uses the current exact fences and no synthetic ID. |
| `SourceRef` | `source_id: SourceId`, `version: u64`, `sha256: String`, `locator: SourceLocator`, `origin: SourceOrigin`, `scope: SourceScope`, `retained_artifact_id: Option<ArtifactId>`. A retained version is immutable. Current-reader authorization is checked each time; hash possession is not access. |
| `SourceLocator` | `project_file { root_id: ResourceId, relative_path: String }`, `user_file { relative_path: String }`, `artifact { artifact_id: ArtifactId }`, or `web { url: String }`. File locations are normalized handle-relative paths beneath authorized roots; reject traversal/foreign-root aliases. Web URLs identify sources, not executable actions. |
| `SourceOrigin` | `user_instruction`, `project_policy`, `agreement`, `code`, `tool_output`, `external_document`, `transcript`, `agent_proposal`, `derived_view`. Derived records additionally carry every parent SourceRef; origin and effective authority are derived conservatively from those parents, never promoted by summarization. |
| `SourceScope` | `goal { goal_id }`, `project { project_id }`, or `user`. Scope grants no effect permissions. User-scope content must be safe to reuse without exposing project-private facts. |
| `InputVersion` | `agreement: SourceRef`, `plan: Option<SourceRef>`, `guidance: Vec<SourceRef>`, `repos: Vec<RepoInput>`, `generated: Vec<SourceRef>`, `tools: Vec<ToolVersion>`, `environment: Vec<NamedDigest>`, `data: Vec<SourceRef>`, `visual: Vec<SourceRef>`. Only bootstrap preparation and the initial lead invocation may omit `plan`; every later assignment, check, fix, and delivery requires the current plan version. Store all actually load-bearing inputs; empty categories are explicit. |
| `RepoInput` | `root_id: ResourceId`, `head: Option<String>`, `tree: Option<String>`, `owned_diff_sha256: String`, `foreign_diff_sha256: String`, `untracked_manifest: SourceRef`. Non-Git roots have absent head/tree, with content manifests. Git hashes retain the repository's actual object format, not an assumed length. |
| `NamedDigest` / `ToolVersion` | `{ name: String, digest: String }` / `{ tool_id: String, executable_sha256: String, version_output: SourceRef, qualification_id: SourceId }`. Credential values never enter these records; secret references/digests use the existing secure store and are not exposed to leads. |
| `RequirementRef` | `id: String`, `source: SourceRef`, `anchor: String`. Stable identity remains through document movement; changed meaning creates a versioned amendment. |
| `EvidenceRef` | `check_id: CheckId`, `receipt_id: ArtifactId`, `sha256: String`, `input_version: InputVersion`. Host validates the executor receipt and current source lineage before use. |
| `GoalError` | `code: GoalErrorCode`, `message: String`, `key: Option<GoalKey>`, `current_goal_revision: Option<u64>`, `source_refs: Vec<SourceRef>`, `retry_at_ms: Option<i64>`. Message is safe bounded text; a future local recheck is labelled separately from a provider-reported reset. |

`Money` is `{ currency: String, minor_units: u64 }`, with a validated ISO currency code and its actual minor-unit scale; unknown price is absent, not zero. `BudgetPolicy` is `{ authority_sources: Vec<SourceRef>, max_cost: Option<Money>, max_elapsed_ms: Option<u64>, unknown_paid_cost: permitted_within_existing_authority/needs_decision }`. Bounds are taken from current user/project policy; no arbitrary timeout or new paid authority is invented. `UsageObservation` is `{ id: SourceId, attempt_id: AttemptId, source: SourceRef, observed_at_ms: i64, coverage: complete/partial/unknown, basis: delta/cumulative, interval_start_ms: i64, interval_end_ms: i64, input_tokens: Option<u64>, output_tokens: Option<u64>, cost: Option<Money> }`. Deduplicate source observation IDs; cumulative observations replace the prior covered interval rather than being added again. Totals are computed over nonoverlapping covered intervals and carry missing coverage. Bounds/observations participate in admission and survive provider replacement.

`CheckExecutor = host_resource { resource_id: ResourceId, runtime_generation: u64, assigned_verifier: Option<RuntimeBinding> } | qualified_observer { runtime: RuntimeBinding, qualification: SourceRef }`. The first is issued only by the actual host executor; the second requires a live qualified observation/evidence path and does not upgrade an arbitrary MCP report. `ItemActivation = all_dependencies | decision { decision_id: DecisionId, version: u64, choice_id: String } | outcome { item_id: WorkItemId, disposition: verified/failed }`. Activation depends on current durable facts; an unchosen branch is explicitly inactive/cancelled with the selecting fact, not a fabricated pass. A join requires every active required branch's promised output; handled failure remains failed acceptance unless the agreement's alternative outcome is actually verified.

`GoalErrorCode` is exhaustive: `not_found`, `invalid_input`, `revision_conflict`, `already_owned`, `idempotency_conflict`, `missing_source`, `stale_source`, `missing_policy`, `permission_denied`, `stale_authority`, `unsupported_capability`, `unqualified_profile`, `resource_busy`, `budget_exhausted`, `invalid_plan`, `evidence_incomplete`, `evidence_stale`, `effect_uncertain`, `storage_unavailable`, `corrupt_lineage`, `closing`. Existing outer rejection codes remain unchanged: map version/source conflicts to `revision_conflict`, authority/owner conflicts to `ownership_conflict`, missing capability to `unsupported_capability`, and other invalid requests to `invalid_transition`; retain the typed safe detail in the operation/query result. No error is a synthetic successful outcome.

## IPC, actions and queries

Add `Command::Goal(GoalIntent)` and `Query::Goal(GoalQuery)` under the current envelope. A `GoalIntent` contains `{ fence: Option<GoalFence>, request: GoalRequest }`; only Create has no existing fence and no expected Task revision. Create includes the existing authorized `CreateTaskRequestIntent` fields with `defer_primary_provider_start=true` and the `GoalStart` below. The host uses WorkspaceAuthorization to atomically create Task + run + source ownership + first operation receipt. An unready source leaves that same accepted run Preparing/waiting; it never launches an unfenced lead.

All command IDs are client-generated once per logical operation. Retry sends the same ID and byte-equivalent normalized arguments; changed arguments with that ID return `idempotency_conflict`. A new intentional message with identical text has a new logical ID. Canonical active-directory ownership can return the existing GoalKey for attachment rather than creating a second owner. Follow-up creates a new RunId on the same GoalId/Task after prior run settlement; it cannot reuse the old operation epoch.

| Public native/CLI action | `GoalRequest` payload, after the shared fence | Effect and result |
| --- | --- | --- |
| `goal.start` | `create { task: CreateTaskRequestIntent, start: GoalStart }` | `GoalStart { goal_id, run_id, input: GoalInput, additional_rules: String, boundary: DeliveryBoundary }`; durable Create receipt, then preparation. |
| `goal.follow_up` | `follow_up { run_id: GoalRunId, input: GoalInput, additional_rules: String, boundary: DeliveryBoundary }` | New run, previous delivered outcome retained. |
| `goal.message` | `message { message_id: MessageId, text: String, attachment_refs: Vec<SourceRef>, in_reply_to: Option<MessageId> }` | Durable current user message, then lead response/steering under current authority. |
| `goal.answer` | `answer { decision_id: DecisionId, decision_version: u64, prepared_action_id: Option<OperationId>, choice_id: Option<String>, text: Option<String>, account_revision: Option<u64> }` | Explicit submit only; require selected choice or nonempty text, current action/account binding where present. |
| `goal.pause` | `pause {}` | Prevent new dispatch; Pausing until admitted effects settle, then Paused. |
| `goal.resume` | `resume {}` | Reconcile current durable inputs/effects and enqueue only eligible work. |
| `goal.stop` | `stop { reason: String }` | Advance epoch, cancel queued work, settle exact owned tree/resources; Stopping until receipt. |
| `goal.priority` | `priority { priority: Priority }` where `Priority=low/normal/high` | Reorder eligible admission only; does not preempt productive work. Existing project/user permission controls who may change it. |
| `goal.correct_knowledge` | `correct_knowledge { source: SourceRef, instruction: String, disposition: correct/retire }` | Scoped lead revision request, canonical CAS publication and future retrieval reconciliation. |
| `goal.restore_source` | `restore_source { current: SourceRef, restore_from: SourceRef }` | Proposes/publishes a new version under existing scope/amendment authority, invalidates affected work; never rewinds effects. |

`GoalInput = text { text: String } | directory { root_id: ResourceId, relative_path: String }`. The selected project/workspace in the existing Task intent is the anchor; a path outside it requires existing authorized WorkspaceService resolution before Create. The lead infers problem/feature/test/performance intent from the user's outcome; there is no required workflow-mode selection. `DeliveryBoundary = docs_only | diagnosis_only | ready_to_deploy | delivered`. “Delivered” means the actual authorized user-defined delivery action, not automatic permission for production or accounts. Resolve an ambiguous consequential boundary as a real decision; don't silently choose broader authority.

`GoalQuery = list { cursor: Option<Vec<u8>>, limit: u16 } | detail { key: GoalKey, section: summary/plan/agents/checks/knowledge, cursor: Option<Vec<u8>>, limit: u16 } | source { key: GoalKey, source: SourceRef, offset: u64, limit_bytes: u32 }`. Require `1..=200` items or `1..=131072` source bytes. Cursors are opaque snapshot-bound binary values using the existing pagination codec, expiring through the existing snapshot release/TTL rules. Every encoded page also stays within the existing 512 KiB snapshot bound and the smaller negotiated physical-carrier limit; reduce item count rather than exceed that limit. Query pages carry `{ key: Option<GoalKey>, goal_revision: u64, high_water: u64, section, items: Vec<GoalViewItem>, next_cursor: Option<Vec<u8>> }` or a typed GoalError. `GoalViewItem` is the exhaustive corresponding record below, not arbitrary JSON. For list pages, goal_revision is 0 and non-authoritative; each run item carries its own GoalFence. All pages are pinned to their snapshot high-water. A source read returns exact SourceRef, offset/next offset, redacted inert bytes, complete/omitted/unavailable status and provenance. Host text decoding marks invalid bytes/control characters visibly.

Expose these actions through the existing action catalogue/`ctl invoke`; add `ctl goals --profile <name> --json` and `ctl goal-show --profile <name> --task <TaskId> --section <section> --json` as read-only wrappers for the above queries. Their output is the typed query page/error, with no implied approval. Capability bit 20 gates every new action/query and goal projection. Native Task status queries use the same host facts; selecting Details queries only the active section with its existing bounded deadline/cancellation path. Include protocol-v2 handling in the Rust Connect bridge and regenerate the real tracked bridge/bundle through existing commands; a native-only wire variant cannot be emitted to an unupdated bridge.

## Goal-scoped MCP bridge and trust

Use a separate `devmanager-goal` Claude MCP server / `devmanager_goal` Codex configuration, served by the existing host process on a loopback ephemeral port at **`/goal/mcp`**. Register an opaque bearer token in **`DEVMANAGER_GOAL_TOKEN`** for only the owned process lifetime. The private overlay/config contains the URL and environment-variable reference, never a committed secret. The host registration derives `RuntimeBinding`, GoalKey, role and permitted tool/effect set. No user tool argument can select another actor, process, run or permission revision.

| MCP tool | Input | Result / authority |
| --- | --- | --- |
| `goal_read` | `{ section, cursor?, limit? }`, same section/page limits as IPC | Current scoped query projection; defaults section=summary, limit=100. |
| `goal_source_read` | `{ source: SourceRef, offset: u64, limit_bytes: u32 }` | Current-reader-checked original/derived source, with origin and omissions. |
| `goal_propose` | `{ message_id: MessageId, base_goal_revision: u64, proposal: GoalProposal }` | Host receipt or GoalError after normalization/authorization/semantic validation; never a trusted check result. |
| `goal_question` | `{ message_id, in_reply_to?, question: QuestionProposal }` | Durable correlated worker→lead question; host/lead decides whether human input is actually needed. |
| `goal_report` | `{ message_id, assignment_id: AssignmentId, result: proposed_complete/blocked/progress, evidence_refs: Vec<SourceRef>, text: String }` | Untrusted claim attached to the exact current assignment. Completion depends on actual outcome/receipts and independent verification. |
| `goal_tools` | `{ required: Vec<String> }` | Qualified available/missing tools and permitted discovery path; cannot install tools or expand permissions implicitly. |

`GoalProposal` is one of `plan { base: Option<SourceRef>, items: Vec<PlanItem>, publications: Vec<PublicationFile> }`, `assign { assignment: AssignmentSpec }`, `check { request: CheckRequest }`, `fix { cause_id: CauseId, finding_ids: Vec<FindingId>, hypothesis: String, assignment: AssignmentSpec }`, `answer { decision_id, decision_version, answer: String, supporting_sources: Vec<SourceRef> }`, `knowledge { change: KnowledgeProposal }`, `deliver { input_version: InputVersion }`. All references are scoped to the registered run and current fence; source changes affecting WHAT use the existing user-decision/amendment authority, not this proposal's content. `QuestionProposal` contains `{ subject: String, question: String, consequence: String, options: Vec<DecisionOption>, recommendation: Option<String>, supporting_sources: Vec<SourceRef>, affected_item_ids: Vec<WorkItemId>, prepared_action_id: Option<OperationId> }`; an option is `{ id, label, consequence }`, recommendation names one option, and an empty options vector permits a genuinely open question.

Apply the 64 KiB input limit before decoding, rate/backpressure through the existing bounded admission lane, constant-time token comparison and no token in logs. Tools never accept passwords/OTP values. An obsolete runtime token produces no current mutation. Store the complete logical-message lineage and receipt even when wake hints are coalesced. Initial input is admitted only after actual qualification and SessionStart binding, except the existing explicitly unsupported-identity first-turn exception with every exact current fence. Revocation/cleanup settles the registration, relay nonce and provider lease on pre- and post-publication mismatch. A MCP “done” report does not impersonate SessionStart, native Stop, physical input delivery, child-process exit or a check receipt.

## Canonical files, journal and publication

Existing handed-off files keep their paths and conventions. A substantial existing spec uses its own `build/00-overview.md`, phase docs, `TRACKING.md`, `TEST-PLAN.md` and `AUDIT.md`. A new brief/problem without an agreement gets one `GOAL.md` under the project's existing spec root (prefer discovered `spec/` or `specs/`; when neither exists use `spec/goals/<GoalId>/GOAL.md`). Small repair plan/check sections remain in that file; no second SPEC is created. Host operational progress is authoritative in receipts/events; `TRACKING.md` is a human working view written at real boundaries and carries the exact event high-water/source versions, never an independent resume cursor. AUDIT.md remains the canonical human finding/worklist source. Host Finding records identify its exact finding_source version and index that content plus actual disposition receipts; they are not a second editable finding body. External tracker/audit edits are ingested as new source versions and independently reconciled, never trusted as verified closure from a checked box. Requirements, decisions and prose planning remain canonical file content; execution indexes refer to their exact versions rather than creating another editable requirement body.

Adopt existing project memory/procedures first. If absent, initialize **`docs/knowledge/README.md`** as the canonical project knowledge index with scoped topic files beneath that directory; link it from the existing project guidance once, without repeating rules. User-wide knowledge is private canonical Markdown under **`persistence::app_config_dir()/knowledge/`**, independent of any target project and accessed through existing configuration-root safety. Runtime tests must use the process-unique test root. Keep project facts out of user-wide content. No vector service or additional database is required; lexical/path/declared-scope lookup over verified source metadata is sufficient for v1.

Append one typed `Event::GoalFact(GoalFact)` family to the existing durable journal. `GoalFact` is `{ schema_version: 1, key: GoalKey, goal_revision: u64, action_epoch: u64, invocation: Option<InvocationKey>, cause_event_ids: Vec<EventId>, body: GoalFactBody }`. The body is one of `started`, `source_bound`, `source_publication_prepared`, `source_file_published`, `source_publication_settled`, `plan_accepted`, `item_state_changed`, `assignment_requested`, `assignment_bound`, `assignment_settled`, `message_recorded`, `decision_recorded`, `decision_superseded`, `check_recorded`, `finding_recorded`, `finding_disposition_recorded`, `repair_attempt_recorded`, `lease_requested`, `lease_settled`, `availability_observed`, `budget_bound`, `usage_observed`, `knowledge_disposition_recorded`, `control_recorded`, `delivery_recorded`. Each carries the corresponding complete typed record below plus its predecessor references; it is not a payload with caller-controlled event names. Existing provider-delivery, resource and operation facts remain in the same stream and participate in replay. Goal revision increments once per accepted goal transaction; Task revision follows the existing command bus contract. Multi-fact transactions share their accepted operation and ordered event lineage.

Add this exact rebuildable projection schema in migration 18 (reallocate only if another source train has legitimately taken 18, updating its immutable manifest/checksum and this contract consistently):

```sql
CREATE TABLE goal_projection (
  run_id TEXT NOT NULL,
  entity_kind TEXT NOT NULL CHECK (entity_kind IN
    ('run','source','item','assignment','message','decision','check','finding',
     'repair','lease','availability','budget','usage','knowledge','publication','delivery')),
  entity_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  source_event_id TEXT NOT NULL,
  source_sequence INTEGER NOT NULL CHECK (source_sequence > 0),
  source_task_revision INTEGER NOT NULL CHECK (source_task_revision >= 0),
  projection_schema INTEGER NOT NULL CHECK (projection_schema = 1),
  payload BLOB NOT NULL,
  PRIMARY KEY (run_id, entity_kind, entity_id)
);
CREATE INDEX goal_projection_task ON goal_projection(task_id, entity_kind, run_id);
CREATE INDEX goal_projection_lineage ON goal_projection(source_sequence, source_event_id);
```

The projection payload is canonical MessagePack of the exhaustive `GoalViewItem` union: run, source, item, assignment, message, decision, check, finding, repair, lease, availability, budget, usage, knowledge, publication, delivery, each wrapping its named record. Source items contain the exact SourceRef plus current readability; budget and usage wrap BudgetPolicy/UsageObservation. Error and raw command output are not projection variants. The entity_kind/payload/run/task must match and exact source event identity/sequence/revision is validated in the same store transaction. SQL projection rows are never the only evidence of an event and never grant authority to resume. No writable phase/status/counter mirrors are kept elsewhere. Cross-goal admission reads validated lease projections while committing the decisive goal event in one IMMEDIATE transaction; it does not race two per-Task counters. Existing operations/outbox/resource recipes own effects. Corrupt projections rebuild only from a validated full event stream; corrupt journals refuse execution rather than synthesize cleanup failures.

`PublicationFile` is `{ target: SourceLocator, expected_old: Option<SourceRef>, staged: SourceRef }`; absent expected_old means “must not exist.” `PublicationRecord` includes `{ publication_id: OperationId, files: Vec<PublicationFile>, current_index: u32, prepared_event_id: EventId, file_receipts: Vec<SourceRef>, state: prepared/publishing/settled/conflict }`. Stage bytes in the existing owned artifact store and durably record the plan before the first file replace. Recheck target old hash/identity, write an owned sibling temporary, fsync as supported, atomically replace that file, record each receipt, then settle the ordered set. File system plus SQLite is not falsely called atomic: crash recovery compares expected/staged/current hashes and receipts; already matching output can be reconciled, foreign content is a conflict. Only a settled consistent set can become a new plan/agreement input. A restore is another such publication with a new version. Signed user approval is never inferred from file content.

## Execution, evidence and resource records

| Record | Exact fields and rules |
| --- | --- |
| `GoalRun` | `key`, `input: GoalInput`, `boundary: DeliveryBoundary`, `agreement: Option<SourceRef>`, `plan: Option<SourceRef>`, `permission_revision: u64`, `priority: Priority`, `created_at_ms`, `control: running/pause_requested/paused/stop_requested/stopped`, `terminal: Option<GoalDelivery>`. Display stage/wait/counts are computed from open items/decisions/leases/receipts. No “done” boolean. |
| `PlanItem` | `id: WorkItemId`, `invocation: InvocationKey`, `title: String`, `kind: investigate/generate/build/check/fix/review/deliver/reconcile_knowledge`, `requirements: Vec<RequirementRef>`, `depends_on: Vec<WorkItemId>`, `activation: ItemActivation`, `inputs: Vec<SourceRef>`, `output_contract: SourceRef`, `repos: Vec<ResourceId>`, `resources: Vec<ResourceRequest>`, `state: planned/ready/running/built/verified/waiting/failed/cancelled`, `state_evidence: Vec<EvidenceRef>`. Investigation's contract names its exact uncertainty/evidence/decision. Verified requires actual applicable receipts. |
| `AssignmentSpec` | `id: AssignmentId`, `item_id: WorkItemId`, `role: lead/researcher/builder/mechanical_worker/auditor/verifier/diagnoser`, `required_capabilities: Vec<String>`, `profile_id: String`, `brief: SourceRef`, `inputs: InputVersion`, `allowed_paths: Vec<SourceLocator>`, `resources: Vec<ResourceRequest>`, `output_contract: SourceRef`, `effect_scope: Vec<EffectPermission>`. Profile ID refers to existing configured profiles; no hard-coded model names. |
| `AssignmentRecord` | `spec: AssignmentSpec`, `attempt_id: AttemptId`, `runtime: Option<RuntimeBinding>`, `state: requested/queued/starting/running/settling/settled/failed/uncertain`, `last_activity: Option<EvidenceRef>`, `result_claims: Vec<SourceRef>`, `host_retry_ids: Vec<OperationId>`, `settlement: Option<EvidenceRef>`. Claim and settlement are distinct. |
| `ResourceRequest` | `resource_id: ResourceId`, `kind: repo_path/workspace/browser_context/test_environment/account/runtime/integration`, `mode: read/write/exclusive`, `path_prefix: Option<String>`. Normalize root and segment-aware path overlap. Sort requests by canonical key and grant all or none. Existing process/browser ResourceIds remain exact owners. |
| `EffectPermission` | `action: String`, `target: SourceRef`, `account_ref: Option<String>`, `account_revision: Option<u64>`, `cost_limit: Option<Money>`, `authority_sources: Vec<SourceRef>`. Host resolves named action to an existing typed operation; never eval arbitrary strings. No account secret or implicit billing-mode change. |
| `DecisionRecord` | `id: DecisionId`, `version: u64`, `question: QuestionProposal`, `asked_by: RuntimeBinding`, `affected_items: Vec<WorkItemId>`, `prepared_action_id: Option<OperationId>`, `authority_revision: u64`, `state: pending/answered/superseded/deferred`, `answer: Option<SourceRef>`, `answer_event_id: Option<EventId>`. Only current accepted answer affects dispatch. |
| `MessageRecord` | `id: MessageId`, `origin: user/lead/worker/host/tool`, `sender: Option<RuntimeBinding>`, `recipient: Option<RuntimeBinding>`, `in_reply_to: Option<MessageId>`, `predecessor_event_id: Option<EventId>`, `content: SourceRef`, `state: accepted/delivered/applied/superseded/failed`, `delivery_event_id: Option<EventId>`, `application_evidence: Option<SourceRef>`. Applied requires an independently attributable worker acknowledgment/result referencing the instruction and current work; delivery alone is not application. |
| `CheckRequest` | `id: CheckId`, `item_id: WorkItemId`, `requirements: Vec<RequirementRef>`, `kind: command/browser/native/measurement/source_review`, `recipe: SourceRef`, `inputs: InputVersion`, `persona: Option<String>`, `expected_inventory: Option<SourceRef>`, `required: bool`. Recipe is validated against current scope, exact executable/argv/target and actual policy before a host-owned executor runs it. |
| `CheckReceipt` | `request: CheckRequest`, `executor: CheckExecutor`, `operation_id: OperationId`, `started_at_ms`, `ended_at_ms: Option<i64>`, `exit_code: Option<i32>`, `completion: complete/incomplete/cancelled`, `output: Vec<SourceRef>`, `inventory: Option<SourceRef>`, `coverage: CoverageRecord`, `observations: Vec<SourceRef>`, `input_after: InputVersion`, `verdict: pass/fail/partial/not_verified`, `receipt_event_id: EventId`. Output artifacts are sealed by the owning executor; the CheckExecutor union keeps a host process distinct from a provider runtime; no provider ID is fabricated for host checks. |
| `CoverageRecord` | `method: ToolVersion`, `applicable: SourceRef`, `inspected: SourceRef`, `skipped: SourceRef`, `errors: SourceRef`, `independently_reviewed: bool`. The four artifacts contain exact identities/reasons, including explicit empty arrays. Summary counts are computed. Zero coverage, parser errors or uninspected required scope cannot produce a complete pass. |
| `Finding` | `id: String`, `finding_source: SourceRef`, `requirement: Option<RequirementRef>`, `scope: required/introduced_regression/outside_scope`, `severity: blocking/should_fix/minor`, `confidence: confirmed/suspected`, `category: missing/broken/diverged/extra/shortcut/values/invariant`, `cause_id: CauseId`, `description: String`, `locations: Vec<SourceRef>`, `evidence: Vec<EvidenceRef>`, `fix: String`, `disposition: open/fix_claimed/disputed/verified_closed`, `disposition_evidence: Vec<EvidenceRef>`. Preserve existing P-/X-/T-/R- IDs and allocate monotonically, never rename or reuse them. |
| `RepairAttempt` | `id: AttemptId`, `cause_id: CauseId`, `parent_cause_ids: Vec<CauseId>`, `original_incident_ids: Vec<String>`, `finding_ids: Vec<String>`, `hypothesis: SourceRef`, `assignment_id: AssignmentId`, `diagnosis: Option<SourceRef>`, `verification: Option<EvidenceRef>`, `result: pending/incomplete/succeeded/failed`. Only completed independently observed failed repairs count toward 2+2; preserve transitive cause/attempt union through regrouping. |
| `Availability` | `scope: provider/model/account/tool`, `scope_ref: String`, `observed_at_ms`, `expires_at_ms: Option<i64>`, `provider_reset_at_ms: Option<i64>`, `local_recheck_at_ms: Option<i64>`, `restriction: available/unavailable/limited/unknown`, `evidence: SourceRef`. Intersect all active relevant restrictions; keep unknown spend/usage explicit. |
| `KnowledgeProposal` | `scope: SourceScope`, `current: SourceRef`, `replacement: SourceRef`, `kind: factual/preference/process`, `incidents: Vec<SourceRef>`, `counterexamples: Vec<SourceRef>`, `rationale: SourceRef`, `independent_review: Option<EvidenceRef>`, `projects_observed: Vec<ProjectId>`. Canonical publication is CAS and source ownership is checked. |
| `KnowledgeDisposition` | `proposal: Option<KnowledgeProposal>`, `state: no_change/proposed/deferred/published/validated/retired`, `publication: Option<SourceRef>`, `validation: Vec<EvidenceRef>`, `delivered_input_sha256: String`. One reconciliation per delivered version; publication does not imply later validation. |
| `GoalDelivery` | `input_version: InputVersion`, `implementation: Vec<EvidenceRef>`, `review: Vec<EvidenceRef>`, `final_effects: Vec<EvidenceRef>`, `repository_results: Vec<SourceRef>`, `outcomes: Vec<SourceRef>`, `remaining_actions: Vec<SourceRef>`, `cleanup: Vec<EvidenceRef>`, `knowledge: KnowledgeDisposition`, `boundary: DeliveryBoundary`, `final_account: SourceRef`. Derive delivered status only after the full required predicate below. |

`FindingId` in typed proposals is a validated stable string using the project's existing allocation convention; it is not another UUID type. All new opaque ID types named above use existing UUIDv7 helpers. A `LeaseRecord` combines the ResourceRequest, owning GoalKey/AssignmentId, admitted operation and current exact resource generation; release/renewal are existing host resource facts. Never derive liveness from a heartbeat string alone. Source, publication, lease, check and receipt admission constructors are host-private; public serde values are proposals until validated.

## Dispatch and completion algorithms

At one host admission boundary: replay/validate the current goal and resource lineage → apply pending user control/authority changes → settle known effects → verify source/plan/permission/tool freshness → choose the next eligible plan item → atomically acquire all required resources and append its assignment/outbox operation → launch through the existing process manager. Default role preference comes from the actual project policy; filter hard capability/authority first, then working-profile affinity and declared preference. A read-only status question does not pause this progression. Coalesced wake hints point at the highest durable sequence; their absence cannot erase outstanding actionable work on restart. One continuation owner polls ordered durable facts and sends a bounded canonical handoff only at a qualified provider-input boundary.

An item with a missing prerequisite waits on that prerequisite. A failed verification becomes a finding; repair attempts use the persistent 2+diagnosis+2 cause ledger. A failed/incomplete transport uses the shared two-host-retry ledger only when its effect is known safe to retry. Current native activity is joined, not duplicated. A changed requirement triggers an authorized amendment/publication and invalidates affected dependencies; a technical graph repair preserves valid finished branches. A repeated empirical investigation with no new evidence consumes the same cause/recovery policy rather than recursively making another planning task.

Completion is an exhaustive predicate, not a terminal suggestion: every required current outcome and branch has complete valid evidence; every required/introduced-regression finding is independently closed or independently disproved against the current agreement; implementation review is independent; all requested final delivery effects and repo landings have actual receipts; no current in-flight/uncertain conflicting effect exists; the final input fence still matches; required cleanup or explicit preserved-work disposition is recorded; and knowledge/tracker reconciliation has a disposition for this delivered input version. Review-process and final-effect obligations are ordered to avoid requiring a reviewer to prove its future approval. Missing live tooling/measurements remain Not verified. Manual Task Done, worker exit, provider turn end, review votes or severity changes cannot bypass the goal predicate.

## Native composition, states and proof

Use [Native UI System](../../../docs/native-ui-system.md), [UI guidance](../../../src/ui/AGENTS.md), the shared component wrappers, tokens and theme system. Base targets are in [manifest.json](../visuals/manifest.json); additional A11–A13 states are in [step4-manifest.json](../visuals/step4-manifest.json) and [step4-reference.html](../visuals/step4-reference.html). Both are versioned design inputs prepared before coding, with original references retained. The top 38-pixel reference-gallery bar is outside the productRegion and is not built into DevManager.

Every slice uses its phase's populated target as the composition. Loading/preparing keeps the same shell, labels and retained data, with a labelled current-state message; it never flashes another goal's data. Empty detail states say which work/source does not yet exist and give the existing permitted next action. Failed queries show a scoped Retry while preserving draft, selected task and the last explicitly stale view; preview data cannot enable mutations before canonical synchronization. Unavailable sources/checks show their reason with a source link and do not resemble a successful empty list. Long text wraps/scrolls in the relevant detail/feed; primary controls retain access. These are concrete variants of the supplied composition, not invitations to redesign the panel.

REF-STEERING distinguishes accepted/delivered/applied; REF-LINEAGE and REF-SOURCE-MISSING show current source/plan repair; REF-EFFECTIVE-ACCESS and REF-ACCESS-NARROW show actual restrictions and revoked prior authority; REF-CHECK-COVERAGE shows incomplete applicable coverage and inert evidence; REF-CONTEXT-DRIFT shows generated view freshness; REF-KNOWLEDGE-STATES distinguishes proposed/published/validated; REF-UNCERTAIN-EFFECT retains unknown prior action; REF-DELIVERY-PENDING separates review from final delivery. Reuse the existing decision card for any actual unanswered approval, with nonsecret account/action/target/cost and preserved draft. A state that needs no human input stays Waiting, not Needs you.

Phase acceptance has three required evidence categories: real host/action/check behavior, actual native interaction, and current full-shell native appearance compared independently against its target. Capture source/build/target/component/token versions, persona/data/state, window geometry, scale/font/theme/density, renderer/platform and test environment. Compare hierarchy, composition, spacing, palette, controls and state truth; record legitimate platform allowances and every material discrepancy. No blanket pixel threshold, crop, broad mask or updated snapshot can approve the implementation. A shared component/target change invalidates affected consumers' proof and gets rechecked. Windows native terminal and platform-specific capture/teardown rules from AGENTS.md apply; Linux/HTML evidence cannot certify them.

## Phase order and contracts

Implement in this order. Each phase includes its own native surface, real actions and current functional/interaction/appearance proof. Phase 17 proves integration; it is not a late UI delivery phase.

| Phase | User-visible outcome | Repo |
| --- | --- | --- |
| [01 — Start one durable goal in the native Task](01-intake.md) | A finished spec, a problem report and a project brief enter through one native action. | DevManager |
| [02 — Run a qualified lead and continue its work](02-lead-bridge.md) | The lead uses the installed Claude or Codex harness, with the host handling durable coordination. | DevManager |
| [03 — Generate complete plans and repair technical dependencies](03-plans.md) | The lead turns a finished agreement or clarified outcome into directly buildable work. | DevManager |
| [04 — Build and integrate across owned repository workspaces](04-workspaces.md) | Independent implementation work can run without corrupting another writer’s checkout. | DevManager |
| [05 — Resolve questions and apply current scoped steering](05-decisions.md) | The lead answers routine worker questions from the agreement, policy and current evidence. | DevManager |
| [06 — Independently verify outcomes and preserve real evidence](06-evidence.md) | A passing claim is insufficient to close work. | DevManager |
| [07 — Close audit gaps with bounded independent diagnosis](07-repairs.md) | The lead fixes audit and verification gaps without requiring the user to coordinate repeated sessions. | DevManager |
| [08 — Verify real user and operator journeys in owned environments](08-journeys.md) | The goal is checked where its users and operators can observe it. | DevManager |
| [09 — Diagnose and repair measured performance outcomes](09-performance.md) | A slow-page report starts the same goal workflow without needing a prepared SPEC. | DevManager |
| [10 — Repair the full requested failing-test inventory](10-test-campaigns.md) | A large red suite becomes a tracked campaign over exact test identities. | DevManager |
| [11 — Use canonical project and universal memory with scoped context](11-memory.md) | A fresh lead can recover relevant knowledge without depending on an earlier chat. | DevManager |
| [12 — Sharpen rules through coherent evidence-based revisions](12-learning.md) | The lead improves durable guidance without reacting to every isolated failure. | DevManager |
| [13 — Run multiple task leads under fair shared ownership](13-scheduling.md) | Several goals can make progress without competing blindly for the same files, browser or test environment. | DevManager |
| [14 — Recover from profile limits without changing authority](14-availability.md) | A goal can wait or select another permitted capable profile when its current one is unavailable. | DevManager |
| [15 — Resume exact work after interruption and reconcile effects](15-recovery.md) | A closed window or dead session does not erase the goal. | DevManager |
| [16 — Finish reviewed delivery with current outcomes and evidence](16-delivery.md) | The final account states what the goal actually achieved at its authorized boundary. | DevManager |
| [17 — Prove complete orchestration in the native app and real projects](17-acceptance.md) | This phase validates the integrated product that earlier slices already expose. | DevManager |

### Exact contract register

The Exposes paragraph in each phase is byte-identical to its paragraph here. These are fixed implementation interfaces, not design work assigned to a later agent.

**CONTRACT-G01.** CreateGoal/FollowUp admission, GoalKey/GoalFence/SourceRef, atomic Task/run receipt, GoalRun/source projections, migration/rebuild and protocol-v2 GoalOrchestration bit 20. Public start/follow-up/read actions expose current source/authority and preserve manual Tasks.

**CONTRACT-G02.** Per-runtime /goal/mcp registration and six typed tools; RuntimeBinding/AssignmentRecord/MessageRecord; qualified current-generation provider input and one actionable continuation owner. MCP output is a proposal, never provider identity or a check receipt.

**CONTRACT-G03.** PlanItem/InvocationKey/RequirementRef, complete canonical build-doc publication, semantic graph validation, versioned source/decision/work/evidence links and recoverable PublicationRecord. Plan query exposes real dependency/repair/source-unavailable state.

**CONTRACT-G04.** AssignmentSpec/RepoInput/ResourceRequest and writable specialist admission through the goal authority path; isolated multi-repo ownership, current-input reconciliation and per-repo integration receipts. Changes and Agents show exact roots and owners.

**CONTRACT-G05.** DecisionRecord and correlated message/answer/restore actions, prepared-effect account/authority validation and existing secure browser handoff. Native Needs you/composer retains scoped sending/superseded/draft/focus state.

**CONTRACT-G06.** CheckRequest/CheckReceipt/CoverageRecord/EvidenceRef/Finding, host-owned actual receipt admission, independent disposition and stale-input invalidation. Checks and safe source reads expose complete coverage/provenance and current native visual evidence.

**CONTRACT-G07.** RepairAttempt with original cause lineage, 2+independent-diagnosis+2 completed-repair admission, fresh closure/dispute and exact productive-process ownership. Checks exposes preserved hypotheses, attempts and finite recovery disposition.

**CONTRACT-G08.** Command/browser/native observation recipes with pinned routes/personas, per-action log windows and real setup/cleanup receipts for proven owned disposable environments. Journey findings retain hidden warnings and unknown causes.

**CONTRACT-G09.** Performance observation artifacts in CheckReceipt: pinned target/conditions, complete comparable samples, generic warm p95/cold sample rules and actual project overrides. Diagnose-only and measured repair boundaries share the existing Plan/Checks flow.

**CONTRACT-G10.** Complete exact test inventories, root-cause campaign membership, requested/new/regression scope, integrity checks, flake evidence and final independent suite reconciliation. Native Counts are derived from retained identities and current receipts.

**CONTRACT-G11.** Canonical scoped memory/procedure sources, authorized exact-version retrieval, replaceable harness views, mandatory-context freshness and canonical correction/retirement. Knowledge and Agents expose source/version/tool accessibility without a second authority store.

**CONTRACT-G12.** KnowledgeProposal/KnowledgeDisposition, causal incident synthesis, independent process review, canonical CAS publication and once-per-delivered-version reconciliation. Axe states distinguish no-change/proposed/published/validated/retired.

**CONTRACT-G13.** One shared host admission owner, current ResourceRequest/lease intersection, persisted priority/eligible queue lineage and fair default 2-goal/2-runtime-per-goal/4-global limits. The native board and scoped controls show exact waits and independent lead state.

**CONTRACT-G14.** Scoped Availability/UsageObservation/BudgetPolicy records, hard-capability and effective-access filtering, profile affinity, coalesced recovery and one composed host-retry allowance. Agents shows current limits, unknown observations and authorized fallback.

**CONTRACT-G15.** Complete-stream invocation/operation replay, exact-prefix publication/effect recovery, uncertainty reconciliation and pause/stop settlement. Native recovery retains current decisions/drafts/work and never substitutes a fresh provider identity for exact resume.

**CONTRACT-G16.** GoalDelivery and its current-input final fence, ordered implementation/review/final-effect obligations, per-repo/boundary outcome receipts and cleanup/knowledge disposition. Final account and follow-up preserve the actual achieved version.

**CONTRACT-G17.** Integrated acceptance corpus and release evidence covering every scenario/native surface, four workloads, configured Claude/Codex modes, cross-provider/fresh verification, real Command/DevManager, concurrency/recovery/learning and equivalent standard-agent trials. No fixture-only or HTML-only completion claim.

## Acceptance traceability

All **197** stable scenarios have one primary phase/check below. Shared/native/live requirements in that phase also apply; the primary assignment does not restrict cross-phase verification. TEST-PLAN covers integration, state sweeps, real workloads and delivery.

| SPEC scenario | Primary phase | Exact acceptance |
| --- | --- | --- |
| H01 | [01](01-intake.md) | P01-H01 |
| H02 | [03](03-plans.md) | P03-H02 |
| H03 | [01](01-intake.md) | P01-H03 |
| H04 | [01](01-intake.md) | P01-H04 |
| H05 | [01](01-intake.md) | P01-H05 |
| H06 | [01](01-intake.md) | P01-H06 |
| H07 | [05](05-decisions.md) | P05-H07 |
| H08 | [01](01-intake.md) | P01-H08 |
| H09 | [01](01-intake.md) | P01-H09 |
| H10 | [05](05-decisions.md) | P05-H10 |
| N01 | [09](09-performance.md) | P09-N01 |
| N02 | [03](03-plans.md) | P03-N02 |
| N03 | [03](03-plans.md) | P03-N03 |
| N04 | [05](05-decisions.md) | P05-N04 |
| N05 | [03](03-plans.md) | P03-N05 |
| N06 | [09](09-performance.md) | P09-N06 |
| N07 | [03](03-plans.md) | P03-N07 |
| N08 | [03](03-plans.md) | P03-N08 |
| N09 | [01](01-intake.md) | P01-N09 |
| N10 | [13](13-scheduling.md) | P13-N10 |
| N11 | [15](15-recovery.md) | P15-N11 |
| N12 | [01](01-intake.md) | P01-N12 |
| N13 | [03](03-plans.md) | P03-N13 |
| J01 | [04](04-workspaces.md) | P04-J01 |
| J02 | [17](17-acceptance.md) | P17-J02 |
| J03 | [09](09-performance.md) | P09-J03 |
| J04 | [17](17-acceptance.md) | P17-J04 |
| G01 | [03](03-plans.md) | P03-G01 |
| G02 | [03](03-plans.md) | P03-G02 |
| G03 | [03](03-plans.md) | P03-G03 |
| G04 | [03](03-plans.md) | P03-G04 |
| G05 | [03](03-plans.md) | P03-G05 |
| G06 | [03](03-plans.md) | P03-G06 |
| G07 | [03](03-plans.md) | P03-G07 |
| G08 | [03](03-plans.md) | P03-G08 |
| G09 | [03](03-plans.md) | P03-G09 |
| G10 | [03](03-plans.md) | P03-G10 |
| I01 | [04](04-workspaces.md) | P04-I01 |
| I02 | [04](04-workspaces.md) | P04-I02 |
| I03 | [04](04-workspaces.md) | P04-I03 |
| R01 | [02](02-lead-bridge.md) | P02-R01 |
| R02 | [02](02-lead-bridge.md) | P02-R02 |
| R03 | [14](14-availability.md) | P14-R03 |
| R04 | [14](14-availability.md) | P14-R04 |
| R05 | [02](02-lead-bridge.md) | P02-R05 |
| R06 | [02](02-lead-bridge.md) | P02-R06 |
| R07 | [14](14-availability.md) | P14-R07 |
| R08 | [02](02-lead-bridge.md) | P02-R08 |
| R09 | [02](02-lead-bridge.md) | P02-R09 |
| R10 | [02](02-lead-bridge.md) | P02-R10 |
| R11 | [02](02-lead-bridge.md) | P02-R11 |
| R12 | [02](02-lead-bridge.md) | P02-R12 |
| R13 | [02](02-lead-bridge.md) | P02-R13 |
| R14 | [02](02-lead-bridge.md) | P02-R14 |
| Q01 | [05](05-decisions.md) | P05-Q01 |
| Q02 | [05](05-decisions.md) | P05-Q02 |
| Q03 | [05](05-decisions.md) | P05-Q03 |
| Q04 | [05](05-decisions.md) | P05-Q04 |
| Q05 | [02](02-lead-bridge.md) | P02-Q05 |
| Q06 | [05](05-decisions.md) | P05-Q06 |
| M01 | [11](11-memory.md) | P11-M01 |
| M02 | [11](11-memory.md) | P11-M02 |
| M03 | [11](11-memory.md) | P11-M03 |
| M04 | [11](11-memory.md) | P11-M04 |
| M05 | [11](11-memory.md) | P11-M05 |
| M06 | [11](11-memory.md) | P11-M06 |
| M07 | [11](11-memory.md) | P11-M07 |
| M08 | [11](11-memory.md) | P11-M08 |
| M09 | [11](11-memory.md) | P11-M09 |
| M10 | [11](11-memory.md) | P11-M10 |
| L01 | [12](12-learning.md) | P12-L01 |
| L02 | [12](12-learning.md) | P12-L02 |
| L03 | [12](12-learning.md) | P12-L03 |
| L04 | [12](12-learning.md) | P12-L04 |
| L05 | [12](12-learning.md) | P12-L05 |
| L06 | [12](12-learning.md) | P12-L06 |
| L07 | [12](12-learning.md) | P12-L07 |
| L08 | [12](12-learning.md) | P12-L08 |
| L09 | [11](11-memory.md) | P11-L09 |
| L10 | [11](11-memory.md) | P11-L10 |
| L11 | [12](12-learning.md) | P12-L11 |
| A01 | [06](06-evidence.md) | P06-A01 |
| A02 | [06](06-evidence.md) | P06-A02 |
| A03 | [06](06-evidence.md) | P06-A03 |
| A04 | [06](06-evidence.md) | P06-A04 |
| A05 | [07](07-repairs.md) | P07-A05 |
| A06 | [07](07-repairs.md) | P07-A06 |
| A07 | [07](07-repairs.md) | P07-A07 |
| A08 | [06](06-evidence.md) | P06-A08 |
| E01 | [08](08-journeys.md) | P08-E01 |
| E02 | [08](08-journeys.md) | P08-E02 |
| E03 | [08](08-journeys.md) | P08-E03 |
| E04 | [06](06-evidence.md) | P06-E04 |
| E05 | [06](06-evidence.md) | P06-E05 |
| E06 | [06](06-evidence.md) | P06-E06 |
| E07 | [08](08-journeys.md) | P08-E07 |
| E08 | [06](06-evidence.md) | P06-E08 |
| E09 | [06](06-evidence.md) | P06-E09 |
| E10 | [06](06-evidence.md) | P06-E10 |
| E11 | [06](06-evidence.md) | P06-E11 |
| E12 | [11](11-memory.md) | P11-E12 |
| E13 | [06](06-evidence.md) | P06-E13 |
| E14 | [06](06-evidence.md) | P06-E14 |
| P01 | [09](09-performance.md) | P09-P01 |
| P02 | [09](09-performance.md) | P09-P02 |
| P03 | [09](09-performance.md) | P09-P03 |
| P04 | [09](09-performance.md) | P09-P04 |
| P05 | [09](09-performance.md) | P09-P05 |
| P06 | [09](09-performance.md) | P09-P06 |
| P07 | [09](09-performance.md) | P09-P07 |
| P08 | [09](09-performance.md) | P09-P08 |
| T01 | [10](10-test-campaigns.md) | P10-T01 |
| T02 | [10](10-test-campaigns.md) | P10-T02 |
| T03 | [10](10-test-campaigns.md) | P10-T03 |
| T04 | [10](10-test-campaigns.md) | P10-T04 |
| T05 | [10](10-test-campaigns.md) | P10-T05 |
| T06 | [10](10-test-campaigns.md) | P10-T06 |
| T07 | [10](10-test-campaigns.md) | P10-T07 |
| T08 | [10](10-test-campaigns.md) | P10-T08 |
| T09 | [10](10-test-campaigns.md) | P10-T09 |
| T10 | [10](10-test-campaigns.md) | P10-T10 |
| T11 | [10](10-test-campaigns.md) | P10-T11 |
| T12 | [10](10-test-campaigns.md) | P10-T12 |
| W01 | [04](04-workspaces.md) | P04-W01 |
| W02 | [04](04-workspaces.md) | P04-W02 |
| W03 | [04](04-workspaces.md) | P04-W03 |
| W04 | [04](04-workspaces.md) | P04-W04 |
| W05 | [04](04-workspaces.md) | P04-W05 |
| W06 | [04](04-workspaces.md) | P04-W06 |
| W07 | [13](13-scheduling.md) | P13-W07 |
| W08 | [05](05-decisions.md) | P05-W08 |
| V01 | [13](13-scheduling.md) | P13-V01 |
| V02 | [13](13-scheduling.md) | P13-V02 |
| V03 | [13](13-scheduling.md) | P13-V03 |
| V04 | [13](13-scheduling.md) | P13-V04 |
| V05 | [13](13-scheduling.md) | P13-V05 |
| V06 | [13](13-scheduling.md) | P13-V06 |
| V07 | [13](13-scheduling.md) | P13-V07 |
| V08 | [13](13-scheduling.md) | P13-V08 |
| V09 | [17](17-acceptance.md) | P17-V09 |
| V10 | [14](14-availability.md) | P14-V10 |
| V11 | [14](14-availability.md) | P14-V11 |
| D01 | [15](15-recovery.md) | P15-D01 |
| D02 | [15](15-recovery.md) | P15-D02 |
| D03 | [15](15-recovery.md) | P15-D03 |
| D04 | [15](15-recovery.md) | P15-D04 |
| D05 | [15](15-recovery.md) | P15-D05 |
| D06 | [14](14-availability.md) | P14-D06 |
| D07 | [15](15-recovery.md) | P15-D07 |
| D08 | [15](15-recovery.md) | P15-D08 |
| D09 | [02](02-lead-bridge.md) | P02-D09 |
| D10 | [02](02-lead-bridge.md) | P02-D10 |
| D11 | [02](02-lead-bridge.md) | P02-D11 |
| D12 | [07](07-repairs.md) | P07-D12 |
| D13 | [07](07-repairs.md) | P07-D13 |
| D14 | [15](15-recovery.md) | P15-D14 |
| D15 | [15](15-recovery.md) | P15-D15 |
| D16 | [15](15-recovery.md) | P15-D16 |
| D17 | [15](15-recovery.md) | P15-D17 |
| D18 | [14](14-availability.md) | P14-D18 |
| D19 | [02](02-lead-bridge.md) | P02-D19 |
| D20 | [02](02-lead-bridge.md) | P02-D20 |
| D21 | [15](15-recovery.md) | P15-D21 |
| D22 | [15](15-recovery.md) | P15-D22 |
| D23 | [15](15-recovery.md) | P15-D23 |
| D24 | [15](15-recovery.md) | P15-D24 |
| D25 | [15](15-recovery.md) | P15-D25 |
| U01 | [17](17-acceptance.md) | P17-U01 |
| U02 | [05](05-decisions.md) | P05-U02 |
| U03 | [17](17-acceptance.md) | P17-U03 |
| U04 | [05](05-decisions.md) | P05-U04 |
| U05 | [17](17-acceptance.md) | P17-U05 |
| U06 | [17](17-acceptance.md) | P17-U06 |
| U07 | [06](06-evidence.md) | P06-U07 |
| U08 | [06](06-evidence.md) | P06-U08 |
| U09 | [17](17-acceptance.md) | P17-U09 |
| U10 | [06](06-evidence.md) | P06-U10 |
| U11 | [17](17-acceptance.md) | P17-U11 |
| U12 | [17](17-acceptance.md) | P17-U12 |
| U13 | [01](01-intake.md) | P01-U13 |
| U14 | [17](17-acceptance.md) | P17-U14 |
| U15 | [05](05-decisions.md) | P05-U15 |
| U16 | [17](17-acceptance.md) | P17-U16 |
| U17 | [17](17-acceptance.md) | P17-U17 |
| U18 | [02](02-lead-bridge.md) | P02-U18 |
| U19 | [03](03-plans.md) | P03-U19 |
| U20 | [05](05-decisions.md) | P05-U20 |
| C01 | [16](16-delivery.md) | P16-C01 |
| C02 | [16](16-delivery.md) | P16-C02 |
| C03 | [08](08-journeys.md) | P08-C03 |
| C04 | [12](12-learning.md) | P12-C04 |
| C05 | [08](08-journeys.md) | P08-C05 |
| C06 | [16](16-delivery.md) | P16-C06 |
| C07 | [16](16-delivery.md) | P16-C07 |
| C08 | [16](16-delivery.md) | P16-C08 |
| C09 | [16](16-delivery.md) | P16-C09 |
| C10 | [16](16-delivery.md) | P16-C10 |

### Every normative UX feature-to-surface row

Each phase listed owns functional/native/appearance evidence in its own slice. Phase 17 repeats the integrated journey and shared-state regressions.

| UX-SPEC capability row | Delivering phases | Required proof |
| --- | --- | --- |
| Goal/problem/spec intake | [01](01-intake.md), [03](03-plans.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Complete generation/build/audit/E2E/fix workflow | [03](03-plans.md), [04](04-workspaces.md), [06](06-evidence.md), [07](07-repairs.md), [08](08-journeys.md), [16](16-delivery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Whole-project steering and technical plan repair | [03](03-plans.md), [05](05-decisions.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Multiple goals and shared scheduling | [13](13-scheduling.md), [14](14-availability.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Worker choice, delegation, provider fallback | [02](02-lead-bridge.md), [04](04-workspaces.md), [14](14-availability.md), [15](15-recovery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Decisions, access, and browser control | [05](05-decisions.md), [08](08-journeys.md), [13](13-scheduling.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Process ownership, pause/stop/restart | [02](02-lead-bridge.md), [07](07-repairs.md), [15](15-recovery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Multi-repo implementation and integration | [04](04-workspaces.md), [13](13-scheduling.md), [16](16-delivery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Audits, fixes, evidence, and test campaigns | [06](06-evidence.md), [07](07-repairs.md), [10](10-test-campaigns.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Performance outcomes | [09](09-performance.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Memory and reusable procedures | [11](11-memory.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Holistic sharpening | [12](12-learning.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Context reduction and durable resumption | [11](11-memory.md), [15](15-recovery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Safe independent delivery | [06](06-evidence.md), [16](16-delivery.md), [17](17-acceptance.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Source-backed research and work lineage | [03](03-plans.md), [11](11-memory.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Outcome measurement and delivery boundary | [09](09-performance.md), [10](10-test-campaigns.md), [16](16-delivery.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Canonical harness context and delivery knowledge reconciliation | [02](02-lead-bridge.md), [11](11-memory.md), [12](12-learning.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Effective access and current effect authority | [02](02-lead-bridge.md), [05](05-decisions.md), [14](14-availability.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |
| Transformed evidence and check coverage | [06](06-evidence.md), [11](11-memory.md) | Each Pxx-J native/live check plus Pxx-UI and current host receipts |

## Verification, risks and close-out

Run commands only from the active isolated DevManager worktree with the Context Pack’s owned target/process preparation. These are checks to run during implementation, **not results of document generation**. Before the first Rust invocation print and validate `CARGO_TARGET_DIR`; restore the ignored WASM inputs, build the process-test helper and use one quiet Rust verification owner. A wrapper yield/timeout does not end Cargo ownership.

- `cargo fmt --all -- --check` — Rust formatting clean (this repository’s formatting/lint gate).
- `cargo check --locked --lib --bins --tests` — compiler/type gate succeeds in the isolated target.
- `cargo build --locked --bin devmanager-process-test-helper` — exact sibling helper exists before the complete suite.
- `cargo test --locked --lib -- --test-threads=1` — complete serial existing library suite; retain exact failing identities and completion summary.
- `cargo test --locked --tests -- --test-threads=1` — complete integration lane, including this phase’s new target.
- `cargo build --locked --bin devmanager --bin devmanager-host` — actual application/host build succeeds.

Run the phase’s focused checks while iterating and the shared global gates once at its required landing/integration boundary; reuse a still-current result across phases instead of rerunning global suites per scenario. Resolve introduced failures. Report exact pre-existing failures and incomplete environment runs; neither can certify a missing required behavior. If this phase changes web/Connect assets, also run `npm --prefix web test`, `npm --prefix web run typecheck`, and `npm --prefix web run build`. The web package has no lint script; do not invent one. Required Windows/Linux/native CI lanes in the Context Pack remain additional platform acceptance.

The concrete acceptance utility and fixture wire contract are specified in TEST-PLAN. Phase 08 implements seed/start/status/adopt-native/stop/assert-clean and the user/operator fixture; phase 09 supplies the measured latency fixture, phase 10 the actual 300-test inventory/fault corpus, and phase 17 implements report and complete corpus reconciliation. These are owned test resources, not additional product capabilities. The utility accepts argv arrays, validates ownership before mutations, and never changes an installed profile or authenticates a provider on its own.

For setup, log capture, actual personas, exact planned fixture/CLI commands and the complete cross-phase/native corpus, run [TEST-PLAN.md](TEST-PLAN.md). Per-scenario test targets and acceptance fixture commands are **new code delivered by the named phases**; the Context Pack lists them as planned command contracts, not already executed tools. The source inspection does not establish current installed harness readiness, a real Command baseline, Windows visual acceptance or passing Rust tests.

Known source facts and mitigations are incorporated in the phases: existing writable specialist rejection is extended only through the goal-authorized path; arbitrary stock provider ingress stays HOLD; native process/session identity remains correlated; generated WASM inputs and isolated Cargo ownership are required; actual Command speed policy overrides generic defaults; canonical file/DB publication is recoverable rather than falsely atomic; old clients receive an explicit protocol upgrade requirement. No unrelated dirty app file is part of this generation change.

The first implementation session can start phase 01 with this overview, its phase document and Context Pack. The full agreement/UX sources remain available for disputed scope, while the copied scenarios, exact contracts, native targets and checks make each assignment buildable without this conversation. The spec’s deferred backlog is preserved separately and no deferred item is assigned to an implementation phase. There are no unresolved BLOCKERS or unrecorded questions in the generated pack.
