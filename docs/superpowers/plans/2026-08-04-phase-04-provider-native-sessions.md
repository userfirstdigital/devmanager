# Phase 4: Provider-Native Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run stock subscription-authenticated Claude Code, Codex, and Cursor CLI sessions as Task-owned native processes while projecting exact conversation identity, semantic activity, raw terminal state, usage freshness, and optional specialists without reimplementing any provider harness.

**Architecture:** A small adapter per provider describes executable discovery, current supported CLI arguments, hook/event correlation, resume, cooperative interruption, and optional usage discovery. The generic host launches the stock CLI through the Phase 3 terminal/process services. One runtime generation produces both semantic journal events and raw terminal deltas. DevManager coordinates providers with explicit commands and artifact handoffs; provider-native planning/tool loops/subagents stay inside their stock harnesses.

**Tech Stack:** Rust, async-trait for the object-safe adapter boundary, stock `claude`, `codex`, and Cursor CLI executables, current provider hook/event surfaces, terminal/process/kernel contracts from Phases 1–3.

## Global Constraints

- Use subscription CLI authentication already established by the user. Do not request/store API keys and do not call model APIs directly.
- Verify current installed CLI help/version and official provider documentation at implementation time before locking flags; adapters fail visibly when a capability is absent.
- One session means one provider process generation. Semantic and raw terminal views subscribe to that same generation.
- `providerSessionId` is accepted only from a correlated, current-generation provider event/hook. Never infer it from cwd, newest transcript, timestamps, filenames, or terminal text.
- Exact resume is the default for an open remembered session. Resume failure remains visible and must not fall back to a new conversation.
- User actions are `Stop turn`, `Close`, `Resume`, and `New conversation`; there is no generic LLM `Restart` action.
- Provider upgrades should degrade to terminal-only operation when semantic hooks change, while keeping the provider process usable and visibly marking unsupported features.
- Primary/specialist orchestration never invents a second planner. A stock Primary may use its native child-agent feature; cross-provider specialists receive explicit bounded work and return artifacts.
- Quota/usage is one cached observation per provider type, performed off hot paths and hidden when older than one hour.

---

## File map

- Create: `src/providers/mod.rs`
- Create: `src/providers/adapter.rs`
- Create: `src/providers/registry.rs`
- Create: `src/providers/capabilities.rs`
- Create: `src/providers/session.rs`
- Create: `src/providers/journal.rs`
- Create: `src/providers/input.rs`
- Create: `src/providers/orchestrator.rs`
- Create: `src/providers/quota.rs`
- Create: `src/providers/claude.rs`
- Create: `src/providers/codex.rs`
- Create: `src/providers/cursor.rs`
- Refactor: `src/ai/claude_hooks.rs`
- Refactor: `src/ai/codex_hooks.rs`
- Refactor then delete at cutover: `src/ai/codex_cli.rs`
- Delete as canonical source at cutover: `src/ai/codex_rollout.rs`
- Modify: `src/main.rs` hook-relay entry handling
- Modify: `src/domain/{agent,command,event,snapshot}.rs`
- Modify: `src/process/{launcher,teardown}.rs`
- Modify: `src/terminal/{protocol,service}.rs`
- Modify: `src/protocol/{capabilities,envelope}.rs`
- Modify: `src/client/action.rs`
- Modify: `Cargo.toml`, `Cargo.lock`
- Create: `tests/provider_registry.rs`
- Create: `tests/provider_identity.rs`
- Create: `tests/provider_sessions.rs`
- Create: `tests/provider_input.rs`
- Create: `tests/provider_orchestration.rs`
- Create: `tests/provider_quota.rs`
- Create: `tests/fixtures/providers/{claude,codex,cursor}/`

### Task 4.1: Define the adapter boundary and provider capability cache

**Files:** `src/providers/{mod,adapter,capabilities,registry}.rs`, `src/domain/agent.rs`, `tests/provider_registry.rs`

**Interface:**

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn probe(&self, executable: &Path) -> Result<ProviderCapabilities, ProviderError>;
    fn build_launch(&self, request: LaunchProviderRequest) -> Result<ProviderLaunchSpec, ProviderError>;
    fn parse_signal(&self, signal: ProviderSignal) -> Vec<JournalEvent>;
    fn cooperative_stop(&self, session: &ProviderRuntime) -> StopStrategy;
    async fn observe_quota(&self, executable: &Path) -> Result<Option<QuotaObservation>, ProviderError>;
}
```

- [ ] **Step 1: Write failing tests** for unique provider kinds, executable identity, versioned capability cache keys, authenticated subscription status, auth-required state without credential disclosure, unsupported resume, missing CLI, malformed version output, and cache invalidation when executable path/version changes.
- [ ] **Step 2: Run** `cargo test --test provider_registry -- --nocapture` and save the red output.
- [ ] **Step 3: Add `async-trait` and define** `ProviderKind::{ClaudeCode, Codex, Cursor}`, `ProviderCapabilities`, `ProviderExecutable`, `ProviderVersion`, `ProviderAuthState::{AuthenticatedSubscription, AuthRequired, Unknown}`, `ProviderSessionId` as a validated provider-issued opaque string, and `CapabilityEvidence { source, observed_at, detail }`.
- [ ] **Step 4: Implement discovery** from configured override then PATH, canonicalize the executable, obtain file/process identity, and probe version/help off the UI/terminal hot path with timeouts.
- [ ] **Step 5: Cache by provider kind + executable identity + version** in host memory and persist only non-secret observation metadata. Unknown capability is distinct from unsupported.
- [ ] **Step 6: Run** registry tests; commit as `feat(providers): define native cli adapter boundary`.

### Task 4.2: Add provider runtime generations and one-process/two-view invariants

**Files:** `src/providers/session.rs`, `src/domain/{agent,resource}.rs`, `src/process/launcher.rs`, `src/terminal/service.rs`, `tests/provider_sessions.rs`

- [ ] **Step 1: Write failing tests** `starting_session_launches_one_process`, `open_agent_with_exact_id_defaults_to_resume`, `agent_without_exact_id_requires_explicit_new_conversation`, `semantic_and_terminal_views_share_generation`, `closing_view_does_not_stop_session`, `closing_task_stops_provider_tree`, and `stale_hook_cannot_bind_replacement_generation`.
- [ ] **Step 2: Run** `cargo test --test provider_sessions runtime_ -- --nocapture` and record the red result.
- [ ] **Step 3: Define `ProviderRuntime`** with `AgentSessionId`, `TaskId`, role, provider, resource/terminal IDs, runtime generation, executable identity, capability snapshot, provider session ID state, and lifecycle.
- [ ] **Step 4: Launch through `TerminalService`/`ProcessSupervisor`** as a Task-owned resource. Register semantic journal and terminal subscriptions against the same `AgentSessionId` and generation.
- [ ] **Step 5: Make UI subscription count irrelevant to lifetime.** Only `CloseAgentSession`, task close, host full quit, or unrecoverable process exit changes runtime ownership. When an open AgentSession has no live generation, its default Start action is exact `Resume` when a correlated providerSessionId exists; `New conversation` is always separate and explicit.
- [ ] **Step 6: Run** runtime tests and inspect the real process registry count; commit as `feat(providers): host provider runtime generations`.

### Task 4.3: Integrate Claude Code stock CLI and exact hook correlation

**Files:** `src/providers/claude.rs`, `src/ai/claude_hooks.rs`, `src/main.rs`, `tests/{provider_identity,provider_sessions}.rs`, `tests/fixtures/providers/claude/`

- [ ] **Step 1: Capture sanitized fixtures** for supported Claude session-start, user-message, tool, permission, result, error, and stop signals from the installed stock CLI; record CLI version/help alongside fixtures.
- [ ] **Step 2: Write failing tests** for fresh ID capture, exact resume argument construction, wrong task nonce, wrong generation, late prior-session hook, missing ID, hook relay restart, and explicit resume failure.
- [ ] **Step 3: Run** `cargo test --test provider_identity claude_ -- --nocapture` and save the red result.
- [ ] **Step 4: Generate a cryptographic launch nonce** per runtime and pass only supported environment/hook configuration. The hook relay envelope must carry provider, nonce, expected Task/Agent IDs, generation, and provider payload over an authenticated local host endpoint.
- [ ] **Step 5: Accept `providerSessionId` only** from the first valid current-generation Claude session-start signal and make rebinding to a different ID an explicit protocol error.
- [ ] **Step 6: Build new/resume launches from probed current Claude flags.** If exact resume reports not found/incompatible/auth failure, emit a visible typed failure and leave `New conversation` as a separate user action.
- [ ] **Step 7: Run** Claude fixture/identity/session tests plus one isolated authenticated smoke session that is immediately closed; commit as `feat(providers): integrate stock claude code sessions`.

### Task 4.4: Integrate Codex stock CLI without a parallel app-server harness

**Files:** `src/providers/codex.rs`, `src/ai/{codex_hooks,codex_cli,codex_rollout}.rs`, `src/main.rs`, `tests/{provider_identity,provider_sessions}.rs`, `tests/fixtures/providers/codex/`

- [ ] **Step 1: Capture sanitized current Codex CLI fixtures/help** for new/resume, lifecycle, turn, tool/approval, result, and failure signals available to the installed subscription-authenticated CLI.
- [ ] **Step 2: Write failing tests** for exact ID binding, exact resume, unsupported semantic event fallback, no rollout-directory inference, and exactly one Codex process for semantic+terminal views.
- [ ] **Step 3: Run** `cargo test --test provider_identity codex_ -- --nocapture` and retain the red result.
- [ ] **Step 4: Build Codex launches solely from current supported CLI entry points.** Do not start Codex app-server, Responses API clients, or a second observation process.
- [ ] **Step 5: Bind IDs through the correlated current-generation hook/event surface.** Remove `codex_rollout` from the new runtime's identity/transcript path; retain old code only in the deletion ledger until Phase 10.
- [ ] **Step 6: Project available structured events.** When the installed Codex version lacks a signal, expose terminal-only/partial semantic capability rather than parsing unstable screen text into invented facts.
- [ ] **Step 7: Run** fixtures and one isolated authenticated smoke session; assert one Job root and zero members after close; commit as `feat(providers): integrate stock codex cli sessions`.

### Task 4.5: Integrate Cursor CLI behind verified capabilities

**Files:** `src/providers/cursor.rs`, `tests/{provider_registry,provider_identity,provider_sessions}.rs`, `tests/fixtures/providers/cursor/`

- [ ] **Step 1: Probe the installed Cursor CLI and official documentation** for subscription authentication, new session, exact resume, machine-readable events/hooks, interruption, and usage. Store sanitized help/version fixtures; do not assume Claude/Codex flags.
- [ ] **Step 2: Write failing tests** for every observed supported capability plus explicit unsupported results for absent features. Include exact ID correlation if and only if a provider-native signal supplies it.
- [ ] **Step 3: Run** `cargo test --test provider_registry cursor_ -- --nocapture` and save the red output.
- [ ] **Step 4: Implement launch/resume/stop with the verified stock interface.** If Cursor supports only interactive terminal mode, mark semantic conversation and exact resume unavailable while keeping a first-class terminal session.
- [ ] **Step 5: Never scrape local history to infer a conversation ID.** An unavailable exact ID disables automatic exact resume and is visible in capabilities.
- [ ] **Step 6: Run** fixtures and an isolated authenticated smoke only when a subscription login exists; otherwise verify the deterministic missing-auth path; commit as `feat(providers): add capability-driven cursor cli adapter`.

### Task 4.6: Build one provider-neutral semantic journal

**Files:** `src/providers/journal.rs`, `src/domain/{event,snapshot}.rs`, `src/protocol/envelope.rs`, `tests/provider_sessions.rs`

**Semantic kinds:** user message, assistant text, reasoning summary when provider supplies it, tool call, tool result, approval request/result, question/options, plan step, usage observation, error, turn state, session state, and artifact reference.

- [ ] **Step 1: Write failing tests** that normalize Claude/Codex/Cursor fixtures into stable journal events, preserve provider-specific extension metadata, deduplicate retried hook delivery, and never persist raw terminal bytes as semantic text.
- [ ] **Step 2: Run** `cargo test --test provider_sessions journal_ -- --nocapture` and record the red result.
- [ ] **Step 3: Define `JournalEvent`** with stable event ID, provider event ID when supplied, task/agent/generation, monotonic per-session sequence, semantic kind, timestamps, visibility, and redaction class.
- [ ] **Step 4: Deduplicate on provider-native event ID** or authenticated relay delivery ID. Never deduplicate by content equality or timestamps.
- [ ] **Step 5: Persist semantic facts selectively** according to privacy policy; raw terminal remains runtime projection unless the user explicitly exports it. Unknown provider payloads may be retained locally as bounded extension data but are not forwarded by default.
- [ ] **Step 6: Run** journal tests and commit as `feat(providers): normalize native events into semantic journal`.

### Task 4.7: Model provider input, questions, approvals, and turn control

**Files:** `src/providers/input.rs`, `src/domain/{command,event}.rs`, `src/protocol/envelope.rs`, `tests/provider_input.rs`

- [ ] **Step 1: Write failing tests** for `SendNow`, `SteerCurrentTurn`, `QueueFollowUp`, `AnswerQuestion`, `ResolveApproval`, `StopTurn`, duplicate request IDs, two-device first-answer-wins, stale question ID, and unsupported provider action.
- [ ] **Step 2: Run** `cargo test --test provider_input -- --nocapture` and save the red result.
- [ ] **Step 3: Define commands with explicit target** Task/Agent/generation/turn/question/approval IDs. A semantic action may map to provider-native control or exact terminal bytes, but the mapping lives only in that provider adapter.
- [ ] **Step 4: Serialize provider input through one per-session sequencer** and persist the accepted intent before side-effect delivery. Retry uses the same command ID; adapters return delivered, duplicate, rejected, or uncertain.
- [ ] **Step 5: Implement first-answer-wins atomically** at the kernel revision boundary. Later answers get an `AlreadyResolved` receipt with the winning timestamp/device, not another input write.
- [ ] **Step 6: Register provider actions in the shared `ActionCatalog`** and expose availability from capability/current turn state. Do not show `Restart`; `New conversation` creates a new AgentSession identity.
- [ ] **Step 7: Run** input tests and commit as `feat(providers): add exact semantic input controls`.

### Task 4.8: Implement Primary plus optional specialists without replacing native harnesses

**Files:** `src/providers/orchestrator.rs`, `src/domain/{agent,artifact,command,event}.rs`, `tests/provider_orchestration.rs`

- [ ] **Step 1: Write failing tests** for one Primary per task, promotion, native-child lineage, cross-provider specialist request, read-only specialist default, isolated writable specialist workspace, artifact return, cancellation, and no recursive uncontrolled fan-out.
- [ ] **Step 2: Run** `cargo test --test provider_orchestration -- --nocapture` and retain the red result.
- [ ] **Step 3: Model roles** `Primary`, `NativeChild { parent }`, and `Specialist { requested_by, purpose }`. Provider-native child agents are observed/reported from provider signals; DevManager does not schedule them internally.
- [ ] **Step 4: Define `SpecialistRequest`** with provider, bounded objective, selected context/artifact IDs, permission mode, workspace choice, timeout, and expected artifact kind. Default is read-only and one active specialist unless the user/Primary explicitly asks for more.
- [ ] **Step 5: Deliver context through files/artifacts and supported CLI prompt/input**, not direct model APIs. A writable specialist receives its own worktree or explicit shared-write approval.
- [ ] **Step 6: Return a durable `SpecialistResult` artifact** with summary, changed files/commit when any, tests, and status. The Primary/user decides integration; no automatic overwrite of another agent's worktree.
- [ ] **Step 7: Run** orchestration tests and commit as `feat(providers): coordinate primary and optional specialists`.

### Task 4.9: Observe one fresh quota summary per provider type

**Files:** `src/providers/quota.rs`, `src/providers/{claude,codex,cursor}.rs`, `src/domain/snapshot.rs`, `tests/provider_quota.rs`

- [ ] **Step 1: Write failing clock-controlled tests** for one observation shared by ten sessions, supported/unavailable/auth-required states, refresh jitter, concurrent refresh collapse, last success replacement, and hidden display after exactly one hour.
- [ ] **Step 2: Run** `cargo test --test provider_quota -- --nocapture` and record the red result.
- [ ] **Step 3: Define** `QuotaObservation { provider, observed_at, reset_at, windows, source_version }` and `QuotaState::{Fresh, Refreshing, Unavailable, AuthRequired, Failed}` without claiming fields the provider does not expose.
- [ ] **Step 4: Schedule one background observer per provider executable/version.** Use only a documented subscription CLI/local status surface; never start one probe per terminal and never use an API key.
- [ ] **Step 5: Coalesce concurrent requests**, apply bounded timeout/backoff/jitter, and retain error diagnostics locally. Client snapshots omit the display value when `now - observed_at >= 1 hour`.
- [ ] **Step 6: Run** quota tests with a fake clock/adapter; commit as `feat(providers): cache fresh provider quota summaries`.

### Task 4.10: Fail open to a usable terminal across provider upgrades

**Files:** `src/providers/{registry,session,journal}.rs`, `tests/provider_sessions.rs`, `docs/replacement-deletion-ledger.md`

- [ ] **Step 1: Add fixture tests** for a newer unknown provider version, missing hooks, unknown event types, malformed individual events, CLI launch success with probe failure, and CLI executable replacement during an active session.
- [ ] **Step 2: Run** `cargo test --test provider_sessions compatibility_ -- --nocapture` and retain the red output.
- [ ] **Step 3: Separate launch-critical capability from enhancement capability.** A working interactive CLI may start as `TerminalOnly`; exact resume is offered only when both ID and resume command are proven.
- [ ] **Step 4: Quarantine malformed semantic signals** with diagnostics and keep PTY reading/input alive. Never terminate an otherwise usable provider solely because the semantic projection parser rejected an event.
- [ ] **Step 5: Pin capabilities to the active runtime generation.** A binary upgrade affects the next generation after a new probe, not the already-running process contract.
- [ ] **Step 6: Update the deletion ledger** for old Claude/Codex hook routing, rollout parsing, and any UI-owned provider launch path.
- [ ] **Step 7: Run** compatibility tests and commit as `feat(providers): degrade safely across cli upgrades`.

### Task 4.11: Prove native provider sessions end to end

**Files:** `scripts/native-next/Invoke-ProviderSmoke.ps1`, `tests/provider_sessions.rs`

- [ ] **Step 1: Create fixture-backed smoke modes** that do not require network/auth and a separately explicit `-Authenticated` mode that uses existing subscription login without printing credentials.
- [ ] **Step 2: For each available provider**, launch one Task-owned runtime, attach semantic and raw terminal clients, send a harmless prompt, observe provider-native session ID when supported, detach/reconnect, exact-resume the same ID in a new generation, then close.
- [ ] **Step 3: Assert** one provider root per active session, one PTY reader, identical agent/generation in both views, and zero Job members/listeners/helpers after close.
- [ ] **Step 4: Exercise exact-resume failure** with a nonexistent ID and prove no fresh conversation starts.
- [ ] **Step 5: Exercise provider upgrade fallback** with fixture versions and prove terminal-only use remains available.
- [ ] **Step 6: Run** all provider tests plus the smoke; commit as `test(providers): prove subscription native session lifecycle`.

## Phase 4 verification gate

- [ ] Capture production baseline and announce that isolated provider/test executables will appear during this long gate.
- [ ] Run `cargo test --test provider_registry --test provider_identity --test provider_sessions --test provider_input --test provider_orchestration --test provider_quota -- --nocapture`.
- [ ] Run `pwsh scripts/native-next/Invoke-ProviderSmoke.ps1`; use `-Authenticated` only with explicit in-session confirmation and the isolated profile/worktree.
- [ ] Run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
- [ ] Search the new runtime with `rg -n "ANTHROPIC_API_KEY|OPENAI_API_KEY|api\.openai|api\.anthropic|codex_rollout" src/providers tests`; require no API-key/API-client path and no rollout inference.
- [ ] Inspect process registry evidence: semantic plus terminal views never exceed one provider root for one AgentSession generation.
- [ ] Confirm every authenticated smoke Job reaches active-process-zero and no hooks/helper/Cargo/rustc/test process remains.
- [ ] Compare production hashes and installed PID/start time; review the complete Phase 4 diff/deletion ledger.

## Phase 4 exit criteria

- Available stock Claude, Codex, and Cursor CLIs run through existing subscription authentication and are never replaced by an in-house model harness.
- Exact native conversation IDs are generation-correlated; exact resume succeeds or fails visibly with no inferred/fresh fallback.
- One provider runtime powers semantic and terminal views and survives all presentation-client detach/reconnect cycles.
- Questions, approvals, steering, follow-ups, and stop-turn are idempotent and capability-aware.
- One Primary may use native children and explicit cross-provider specialists through artifacts/worktrees without uncontrolled write sharing.
- Quota observations are singleton, off-hot-path, honestly scoped, and hidden after one hour.
- Provider surface drift degrades semantic enhancements without making a functioning stock terminal unusable.
