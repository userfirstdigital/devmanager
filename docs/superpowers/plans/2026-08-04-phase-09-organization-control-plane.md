# Phase 9: Organization Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional organization layer to DevManager Connect for managed Task assignment/Kanban, realtime read-only management, honest activity/usage/Git summaries, collaboration policy, DB Flow/ENV local execution contracts, and DevAgent EvidenceBundle intake—without moving local execution or raw personal work into the cloud.

**Architecture:** Portal accounts/tenants and the existing Board/BoardCard ecosystem remain the organization/task-management source of truth. An opt-in `ManagedTaskLink` associates one local DevManager Task with one BoardCard; metadata synchronizes through Connect, while raw content remains local/E2E-authorized. Managers receive a Watcher projection and summarized coordination evidence, never hidden screen surveillance or a productivity score. DB Flow and ENV retain hosted planning/approval metadata but dispatch privileged operations to an explicitly enrolled local host, which owns credentials and returns signed/redacted receipts. DevAgent exports a versioned EvidenceBundle that becomes a reviewed Task draft.

**Tech Stack:** DevManager Rust host/Connect protocol, Portal Express/Sequelize/PostgreSQL, Portal React/TypeScript, existing Board/BoardCard models and UI, existing DB Flow/Hosting ENV modules, DevAgent Tauri/Rust/React only at its explicit export boundary.

## Global Constraints

- Personal Tasks remain local-only by default. Signing into Connect does not enroll existing or future Tasks automatically.
- Reuse Portal's existing authenticated user, tenant, Board, BoardCard, columns, assignments, phases, dependencies, comments, handoffs, documents, and activity systems. Do not build a parallel Kanban/task stack.
- One `ManagedTaskLink` connects one `(ConnectHostId, local TaskId)` to one BoardCard. Link/unlink is explicit, revisioned, and locally visible.
- The Portal never becomes process/provider/file/browser authority. Every local side effect is a typed request re-authorized and receipted by the local host.
- Default organization sync is metadata only: Task title/status/attention, assignment/link, provider kind/state, timestamps, provider-reported usage, aggregate message/turn counts, active-session intervals, Git summaries, host health, and approved artifacts.
- Raw prompts/responses, terminal, browser, recordings, file bodies, full diffs, credentials, environment values, and database connection strings remain excluded unless an authorized E2E viewer/content class is explicitly granted.
- “Active DevManager session time” is coordination evidence with a visible 15-minute idle rule. It is not payroll time or proof of productivity.
- Do not produce worker rankings, productivity scores, sentiment scores, keystroke counts, or hidden screenshots.
- Provider-reported usage and local estimates are labeled separately; do not convert either into cost or quota certainty without a documented provider source.
- A manager Watcher is read-only unless separately granted Collaborator permissions on that Task. Dangerous approvals remain Owner-only by default.
- DB/ENV secrets stay in local OS credential storage. Portal stores identifiers, policies, planned changes, approvals, and redacted receipts only.
- Inventory licenses/provenance before porting code from Traycer or DevAgent. DevManager is Apache-2.0; do not copy incompatible or proprietary code into it merely because both repos are locally accessible.

---

## Repository and file map

**DevManager (`C:\Code\userfirst\devmanager`):**

- Create: `src/connect/managed.rs`
- Create: `src/connect/policy.rs`
- Create: `src/connect/telemetry.rs`
- Create: `src/connect/local_actions.rs`
- Create: `src/connect/evidence.rs`
- Modify: `src/connect/{relay,projection,permissions}.rs`
- Modify: `src/domain/{task,artifact,command,event,snapshot}.rs`
- Modify: `src/kernel/outbox.rs`
- Modify: `src/client/action.rs`
- Create: `src/ui/task_cockpit/managed_task.rs`
- Create: `src/ui/command_center/organization.rs`
- Create: `tests/managed_task_sync.rs`
- Create: `tests/management_telemetry.rs`
- Create: `tests/local_action_policy.rs`
- Create: `tests/evidence_bundle.rs`

**Portal API (`C:\Code\happier\portal\api`):**

- Create: `src/database/models/devmanager/DevManagerHostMembership.ts`
- Create: `src/database/models/devmanager/DevManagerManagedTask.ts`
- Create: `src/database/models/devmanager/DevManagerTaskObservation.ts`
- Create: `src/database/models/devmanager/DevManagerOrganizationPolicy.ts`
- Create: `src/database/models/devmanager/DevManagerActionRequest.ts`
- Create: `src/database/migrations/20260804000001-create-devmanager-management.cjs`
- Create: `src/routes/devmanagerManagementRoutes.ts`
- Create: `src/controllers/devmanagerManagementController.ts`
- Create: `src/services/devmanagerManagement/{authorization,taskSync,telemetry,localActions,evidence}.ts`
- Modify: `src/database/index.ts`
- Modify: `src/routes/index.ts`
- Add focused tests beside models/services/routes.

**Portal web (`C:\Code\happier\portal\web`):**

- Create: `src/api/devmanagerManagement.ts`
- Create: `src/types/devmanagerManagement.ts`
- Create: `src/pages/devmanager/OrganizationTasksPage.tsx`
- Create: `src/pages/devmanager/ManagedTaskPage.tsx`
- Create: `src/pages/devmanager/HostFleetPage.tsx`
- Create: `src/components/devmanager/{ManagedTaskPanel,TaskLiveView,TaskActivitySummary,HostHealthCard,TaskEnrollmentDialog,LocalActionApproval}.tsx`
- Modify: existing BoardCard detail/task components to host the `ManagedTaskPanel` rather than duplicating card UI.
- Modify: `src/App.tsx`
- Modify: `src/pageRegistry.ts`
- Add focused tests beside components/pages/API types.

**DevAgent (`C:\Code\happier\portal\agent`):**

- Create: `src/types/evidenceBundle.ts`
- Create: `src/services/evidenceBundle.ts`
- Create: `src-tauri/src/commands/evidence_bundle.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify the recording review/export surface discovered at implementation time.
- Create corresponding TypeScript/Rust tests and fixtures.

Before editing each repo, read its local `AGENTS.md`, capture clean status/baseline, and use an isolated branch/worktree. Commits remain repository-local and no migration is applied to production without explicit authority.

All cross-repo commands below run from the DevManager worktree unless a different workdir is stated. Use `npm --prefix C:\Code\happier\portal\api`, `npm --prefix C:\Code\happier\portal\web`, and `npm --prefix C:\Code\happier\portal\agent` so the owning lockfile/runtime is unambiguous; use `cargo test --manifest-path C:\Code\happier\portal\agent\src-tauri\Cargo.toml` for DevAgent Rust.

### Task 9.1: Model organization membership and host enrollment using Portal identities

**Files:** Portal API `DevManagerHostMembership.ts`, `DevManagerOrganizationPolicy.ts`, migration, authorization service/routes/tests; Portal web host enrollment UI; DevManager `src/connect/policy.rs`, tests

- [ ] **Step 1: Map existing Portal user/tenant/permission models** in an ADR note and write failing authorization tests for owner, admin, manager, member, disabled user, cross-tenant access, host unenroll, and revoked device/account.
- [ ] **Step 2: Run** `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/authorization.test.ts` and `npm --prefix C:\Code\happier\portal\api run type-check`; record the red missing-module result before models/migration.
- [ ] **Step 3: Add `DevManagerHostMembership`** referencing the existing tenant/user and Phase 8 ConnectHost, with role, status, enrolled/last-seen/revoked timestamps, display label, and policy revision. Do not introduce a second login or organization table.
- [ ] **Step 4: Add organization policy** only for DevManager-specific enrollment, metadata fields, retention, idle interval, raw-sharing permission ceiling, and local-action approval requirements. Default policy is deny/minimal metadata.
- [ ] **Step 5: Require local host confirmation** of enrollment and show the exact tenant, account, metadata classes, retention, and manager visibility before accepting. The host stores the signed policy revision and may unenroll offline.
- [ ] **Step 6: Add Portal administration UI** for enrolled hosts/members/policy with audit entries and explicit revoke; run tests and commit Portal/DevManager changes independently.

### Task 9.2: Link local Tasks to existing BoardCards instead of replacing Boards

**Files:** Portal API `DevManagerManagedTask.ts`, `taskSync.ts`, routes/controller/tests; DevManager `src/connect/managed.rs`, `src/ui/task_cockpit/managed_task.rs`, `tests/managed_task_sync.rs`; Portal web `ManagedTaskPanel` and BoardCard integration/tests

**Contract:**

```text
ManagedTaskLink = {
  hostId, localTaskId, boardCardId, enrollmentState,
  localRevision, portalRevision, metadataPolicyVersion,
  linkedBy, linkedAt, unlinkedAt
}
```

- [ ] **Step 1: Write failing tests** for create/link from BoardCard, enroll existing local Task, duplicate link conflict, cross-tenant denial, assignment/column/status sync, concurrent edit conflict, offline local update, unlink, close, and delete retention.
- [ ] **Step 2: Run** `cargo test --test managed_task_sync link_ -- --nocapture` and `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/taskSync.test.ts`; save red results.
- [ ] **Step 3: Add a unique mapping** on `(connectHostId, localTaskId)` and `boardCardId` according to the one-to-one v1 rule. Store the local TaskId as an opaque UUID; no raw local path or provider ID enters BoardCard tables.
- [ ] **Step 4: Define field authority explicitly:** BoardCard owns assignment, board/column, deadlines, phases, dependencies, comments/handoffs; local Task owns runtime lifecycle/attention/provider/resource truth; title changes use revisioned bidirectional policy with conflict UI.
- [ ] **Step 5: Sync through outbox/inbox receipts** with source revision/idempotency key and tombstones. Offline changes reconcile deterministically and conflicts remain visible; last-write-wins is forbidden for fields with two writers.
- [ ] **Step 6: Embed managed runtime state inside existing BoardCard detail/Kanban** and show Board context inside GPUI Task header. Do not create a second card/comment/dependency UI.
- [ ] **Step 7: Run** `cargo test --test managed_task_sync -- --nocapture`, `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/taskSync.test.ts`, and `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager/ManagedTaskPanel.test.tsx`; commit as `feat(management): link tasks to existing board cards` in each repo.

### Task 9.3: Add deliberate Personal versus Managed enrollment

**Files:** `src/connect/{managed,projection,policy}.rs`, `src/ui/task_cockpit/managed_task.rs`, Portal enrollment dialog/API/tests, `tests/managed_task_sync.rs`

- [ ] **Step 1: Write failing tests** showing sign-in alone exports zero personal Tasks, new Tasks default Personal, enroll preview lists exact fields/viewers/retention, cancel exports nothing, enroll begins at acknowledged revision, and unenroll stops future sync.
- [ ] **Step 2: Run** `cargo test --test managed_task_sync enrollment_ -- --nocapture`, `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/taskSync.test.ts`, and `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager/TaskEnrollmentDialog.test.tsx`; record red results.
- [ ] **Step 3: Add `TaskScope::{Personal, Managed { link_id, policy_revision }}`** as a durable local fact. A remote BoardCard request creates a local pending Task only after owner acceptance or an explicitly enabled organization auto-accept policy.
- [ ] **Step 4: Build enrollment preview/consent** in both GPUI and Portal, with field-by-field metadata classes and raw content explicitly off by default.
- [ ] **Step 5: On unenroll**, emit a final metadata tombstone according to retention policy, stop outbox projection, retain local Task/content, and revoke organization Watcher channels.
- [ ] **Step 6: Run** the same Rust/API/web commands from Step 2 without the Rust test-name filter and commit as `feat(management): make task enrollment explicit`.

### Task 9.4: Publish honest, bounded management observations

**Files:** `src/connect/telemetry.rs`, `src/kernel/outbox.rs`, `tests/management_telemetry.rs`, Portal API `DevManagerTaskObservation.ts`/`telemetry.ts`/tests

**Observation fields:** Task state/attention, Primary/specialist provider kinds/states, provider-reported usage by named window/unit, semantic message/turn counts, active-session intervals, Git branch/commit/change summary, host health, observed/source timestamps, completeness/confidence.

- [ ] **Step 1: Write failing clock-controlled tests** for the 15-minute idle rule, overlapping desktop/phone activity deduplication, provider-reported versus estimated usage, message count from semantic events only, Git summary without paths/full diff under restrictive policy, stale observation, and offline backlog.
- [ ] **Step 2: Run** `cargo test --test management_telemetry -- --nocapture` and `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/telemetry.test.ts`; save red results.
- [ ] **Step 3: Define activity intervals** from explicit accepted user commands and foreground Task interaction; close an interval after 15 minutes without qualifying activity and expose the rule in every UI. Do not count provider background CPU as user active time.
- [ ] **Step 4: Aggregate locally into bounded interval/summary events** before outbox delivery. Never upload keystrokes, periodic screenshots, raw terminal, prompts/responses, file bodies, or full diffs for analytics.
- [ ] **Step 5: Label every usage value** with provider/source/window/unit/observed_at and `provider_reported` or `local_estimate`. Unknown/unavailable stays unknown.
- [ ] **Step 6: Store append-only observation windows** with retention/policy revision and dedupe keys; run tests and commit as `feat(management): publish honest task observations`.

### Task 9.5: Build manager Task/fleet views without surveillance scoring

**Files:** Portal web OrganizationTasks/ManagedTask/HostFleet pages and components/tests; Portal API query endpoints/tests; DevManager Command Center organization view

- [ ] **Step 1: Write failing API/UI tests** for role-filtered board/fleet list, attention filters, assignment, offline/stale host, Task live Watcher, usage/source labels, active-time rule disclosure, Git summary, empty/error/partial data, and forbidden raw view.
- [ ] **Step 2: Run** `npm --prefix C:\Code\happier\portal\api test -- src/controllers/devmanagerManagementController.test.ts src/services/devmanagerManagement/telemetry.test.ts` and `npm --prefix C:\Code\happier\portal\web test -- src/pages/devmanager/OrganizationTasksPage.test.tsx src/pages/devmanager/ManagedTaskPage.test.tsx src/pages/devmanager/HostFleetPage.test.tsx`; save red results.
- [ ] **Step 3: Build manager overview** around coordination: assigned/in-progress/waiting/blocked/review, host online/stale, last activity, provider/session state, approved usage summary, message/turn counts, active-session time, changed-file count, commits/PR links.
- [ ] **Step 4: Reuse Board Kanban/detail/assignment/dependency/comment/handoff controls** and add live DevManager panels rather than another task editor.
- [ ] **Step 5: Add a Task Live View** that is Watcher-only by default and displays only fields/raw classes granted by local policy; state updates stream through Connect without refresh.
- [ ] **Step 6: Add explicit copy explaining** active-session time and data freshness; ban ranking/score/payroll labels with source/UI tests.
- [ ] **Step 7: Visual/browser-validate role and mobile/desktop states**; commit as `feat(management): add realtime coordination views`.

### Task 9.6: Enforce manager Watcher access locally and end to end

**Files:** `src/connect/{permissions,managed}.rs`, Portal authorization service, Connect route grants, `tests/{connect_permissions,managed_task_sync}.rs`, Portal tests

- [ ] **Step 1: Write failing tests** for manager Watcher on enrolled Task, no access to Personal/other tenant/unlinked Tasks, policy-limited raw classes, explicit Collaborator elevation, owner-only dangerous approval, membership revoke, Task unenroll, and offline policy expiry.
- [ ] **Step 2: Run** `cargo test --test connect_permissions organization_ -- --nocapture` and `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/authorization.test.ts`; record red results.
- [ ] **Step 3: Bind organization grants** to account/device public identity, membership/role, tenant, Task link, policy revision, expiry, and allowed content/action classes. Hosted authorization issues routing eligibility; local host independently validates the signed grant and current local policy.
- [ ] **Step 4: Raw live view remains E2E** between authorized manager device and host. Relay/Portal application code must not decrypt even when the account has permission.
- [ ] **Step 5: Revoke active channels immediately** on membership/policy/link changes and reject queued stale-grant commands.
- [ ] **Step 6: Run** `cargo test --test connect_permissions --test managed_task_sync -- --nocapture` and the Portal authorization test from Step 2; commit as `feat(management): enforce local organization grants`.

### Task 9.7: Add review, comments, handoffs, and assignment reconciliation

**Files:** Portal existing BoardCard comments/handoffs/dependencies APIs/UI plus `taskSync.ts`; DevManager `src/connect/managed.rs`, `src/ui/task_cockpit/managed_task.rs`; tests

- [ ] **Step 1: Write failing tests** for Board assignment to a worker, pending local acceptance, phase/dependency blocking, Portal comment appearing as metadata event, local handoff/summary returning to BoardCard, review requested/approved/changes requested, duplicate/offline delivery, and content-class filtering.
- [ ] **Step 2: Run** `cargo test --test managed_task_sync board_workflow_ -- --nocapture`, `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/taskSync.test.ts`, and `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager/ManagedTaskPanel.test.tsx`; retain red results.
- [ ] **Step 3: Map existing BoardCard activities** into versioned managed-task metadata commands/events. Do not mirror entire BoardCard rows into local SQLite; store link/revision and the minimal task projection.
- [ ] **Step 4: Represent a handoff as an ArtifactId/summary/status/checkpoint/Git reference** with raw artifact access separately granted. Assignment never auto-launches a provider without local user/organization policy authorizing start.
- [ ] **Step 5: Reflect local attention/review state back to BoardCard** without changing its unrelated fields or duplicating notifications.
- [ ] **Step 6: Run** the complete `managed_task_sync`, Portal task-sync, and ManagedTaskPanel tests from Step 2 and commit as `feat(management): connect board workflows to local tasks`.

### Task 9.8: Extract a local-action contract for DB Flow and ENV

**Files:** `src/connect/local_actions.rs`, `src/connect/policy.rs`, `tests/local_action_policy.rs`; Portal API `DevManagerActionRequest.ts`/`localActions.ts`/tests; existing `dbflowRoutes.ts` and hosting ENV service/routes; Portal web `LocalActionApproval` and existing DB Flow/ENV pages/tests

**Contract:** `LocalActionRequest` includes request ID, organization/host/project binding, action kind/version, declarative payload, risk, required approvals, expected target fingerprint/revision, expiry, and signature. `LocalActionReceipt` includes accepted/rejected, local actor, start/end, target fingerprint, redacted result, artifact references, and error class.

- [ ] **Step 1: Inventory existing DB Flow and Hosting ENV capabilities/secrets/side effects** and write a parity table. Select the smallest complete vertical slices: database schema introspection + approved change apply; environment diff + approved apply.
- [ ] **Step 2: Write failing policy tests** for wrong tenant/host/project, expired/replayed request, missing approval, production risk, target fingerprint mismatch, dry run, cancellation, secret redaction, host offline, and signed receipt verification.
- [ ] **Step 3: Run** `cargo test --test local_action_policy -- --nocapture`, `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/localActions.test.ts`, and `npm --prefix C:\Code\happier\portal\web test -- src/components/devmanager/LocalActionApproval.test.tsx`; save red results.
- [ ] **Step 4: Implement signed, versioned request/receipt transport and shared ActionCatalog approval entries** over managed Connect metadata/E2E payload as classification requires. Local host maps a Portal project binding to locally configured credentials/targets and requires owner confirmation unless a narrow policy explicitly pre-authorizes the exact class/environment.
- [ ] **Step 5: Move credential resolution/execution for the selected vertical slices local:** connection strings/provider tokens stay in OS vault; host performs validate/dry-run/apply via bounded Task-owned operations and returns redacted structured results/artifacts.
- [ ] **Step 6: Adapt existing Portal DB Flow/ENV UIs** to show dispatch target, local approval, progress, receipt, target fingerprint, and offline/failure state while retaining their planning/promotion/history UX.
- [ ] **Step 7: Prove parity/safety** against isolated fake/dev database and hosting provider fixtures; no production target or credential is used. Commit each repo independently.

### Task 9.9: Define and ingest DevAgent EvidenceBundles

**Files:** DevAgent evidence files listed above; DevManager `src/connect/evidence.rs`, `tests/evidence_bundle.rs`; Portal API `evidence.ts`/tests; Portal web managed Task draft UI/tests; `tests/fixtures/evidence/v1/*`

**Bundle v1:** manifest/version, capture time range/timezone, source device/user identity, transcript segments, recording/screenshot references with hashes, observed window/application metadata, proposed title/summary/acceptance criteria/steps, privacy labels, redactions, and signature. Large media remains separate encrypted objects/artifacts.

- [ ] **Step 1: Write failing cross-repo fixture tests** for minimal/full bundle, schema version, content hashes, tamper, missing media, redacted segment, timezone, duplicate import, untrusted signer, and review-before-Task creation.
- [ ] **Step 2: Add a DevAgent `test` script backed by Vitest if absent, then run** `npm --prefix C:\Code\happier\portal\agent test -- src/services/evidenceBundle.test.ts`, `cargo test --manifest-path C:\Code\happier\portal\agent\src-tauri\Cargo.toml evidence_bundle -- --nocapture`, `cargo test --test evidence_bundle -- --nocapture`, and `npm --prefix C:\Code\happier\portal\api test -- src/services/devmanagerManagement/evidence.test.ts`; save red results.
- [ ] **Step 3: Implement DevAgent export** from existing durable recordings/frame/transcription data without changing capture lifecycle. Present a privacy/redaction review and sign the manifest with the device identity.
- [ ] **Step 4: Implement local/Connect ingestion** into a `TaskDraft`, not a running Task. User reviews title, summary, acceptance criteria, selected evidence, Personal/Managed scope, project/workspace, and provider before creation.
- [ ] **Step 5: Store selected media as local/E2E artifacts** and upload only policy-approved encrypted objects. Portal stores metadata/object ciphertext and cannot decrypt raw evidence by default.
- [ ] **Step 6: Validate import from file, direct local handoff, and Connect handoff** with duplicate/idempotency behavior; commit each repo independently.

### Task 9.10: Prove the open-source/product boundary and organization workflow

**Files:** `docs/licensing-and-product-boundary.md`, `scripts/native-next/Invoke-ManagementE2E.ps1`, all Phase 9 tests, each repo's attribution/license files, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Inventory every reused/copied dependency/file** from Portal, DevAgent, Traycer, and third parties with origin, license, changes, destination, and compatibility. Prefer contract-level reuse and clean reimplementation when code provenance is not clearly compatible with Apache-2.0.
- [ ] **Step 2: Document boundary:** DevManager local host/GPUI/direct client/protocol is open source; hosted accounts/routing/management/billing/retention is proprietary Connect; shared wire schemas are open and versioned; no proprietary key is required for local/direct operation.
- [ ] **Step 3: Execute an organization E2E:** manager creates/assigns BoardCard; worker accepts managed Task; local stock provider works; manager watches permitted metadata/live content; worker switches phone/desktop; Git/checkpoint/usage/activity update; review/handoff/close syncs.
- [ ] **Step 4: Execute privacy/authorization negatives:** Personal Task invisibility, cross-tenant denial, Watcher mutation denial, raw-content absent by default, revoke/unenroll, offline policy expiry, relay plaintext scan.
- [ ] **Step 5: Execute DB Flow/ENV fake-target vertical slices** and DevAgent EvidenceBundle-to-reviewed-Task flow.
- [ ] **Step 6: Confirm local execution continues** while Portal/relay is unavailable and reconciles idempotently after recovery.
- [ ] **Step 7: Update the deletion ledger** and commit docs/E2E proofs in their owning repos.

## Phase 9 verification gate

- [ ] Read/follow all repo-local guidance and capture clean status/baselines in every isolated worktree.
- [ ] Capture production DevManager hashes/PID/start time and announce the long multi-repo organization gate.
- [ ] Run `cargo test --test managed_task_sync --test management_telemetry --test local_action_policy --test evidence_bundle --test connect_permissions -- --nocapture`.
- [ ] In Portal API run focused management/Board/local-action/evidence tests, `npm run type-check`, and `npm run build`.
- [ ] In Portal web run focused management/Board/DB Flow/ENV tests, `npm run type-check`, `npm run lint`, and `npm run build`.
- [ ] In DevAgent run available TypeScript/Rust tests, type-check/build, and a fixture export/import; if it lacks a test script, add one during Task 9.9 rather than accepting build-only proof.
- [ ] Run `pwsh scripts/native-next/Invoke-ManagementE2E.ps1 -Organization -PrivacyNegatives -LocalActions -EvidenceBundle -OfflineRecovery`.
- [ ] Inspect database/log/relay captures for seeded raw content/secrets and verify policy/role/revocation audit events.
- [ ] Confirm no production database/hosting target, Portal migration, or deployment was touched without explicit authority.
- [ ] Confirm all local test/helper/provider/browser/host processes and routes are gone; compare production invariants.
- [ ] Review complete diffs, migrations, authorization, licenses, and deletion ledger in all repos.

## Phase 9 exit criteria

- Existing Portal membership and Board/Card workflows manage opt-in local Tasks without a parallel task system.
- Personal Tasks remain invisible; Managed metadata is explicit; raw live content is E2E-granted and locally enforced.
- Managers can watch permitted realtime state and honest usage/activity/Git summaries without mutation rights, surveillance scoring, or payroll claims.
- Assignment, phases, dependencies, comments, review, and handoffs reconcile offline and idempotently.
- DB Flow/ENV selected vertical slices execute locally against explicit fake/dev targets with local credentials/approval and signed redacted receipts.
- DevAgent produces a versioned, tamper-evident, privacy-reviewed EvidenceBundle that becomes a reviewed Task draft.
- DevManager remains fully useful open source/local/direct while Connect-specific hosted management stays cleanly separated.
