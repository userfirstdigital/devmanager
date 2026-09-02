# Port Ideas Audit: herdr, traycer, t3code

**Date:** 2026-09-01
**Status:** Reference. Items are candidates, not commitments; each needs its own spec before implementation.
**Sources:** `C:\Code\userfirst\herdr-master` (herdr v0.8.2, Rust), `C:\Code\userfirst\traycer-main` (Traycer client + protocol, TypeScript), `C:\Code\userfirst\t3code-main` (T3 Code, TypeScript/Effect).

## 1. What each product is

- **herdr**: a Rust terminal multiplexer and coding-agent runtime. A detached background server owns PTYs; TUI clients attach. Strong on agent status detection, agent-native CLI, Windows install hygiene, performance discipline.
- **traycer**: the open-source client and protocol half of an agent-orchestration desktop app. A durable host (not in the repo) owns PTYs, provider CLIs and worktrees; Electron, web and CLI clients attach over versioned RPC. Strong on durable-vs-ephemeral modelling, worktree evidence, host lifecycle tooling, contract discipline.
- **t3code**: an agent harness control surface. A local server runtime owns agent sessions, workspaces, git and terminals; web, desktop and mobile clients render. Event-sourced SQLite like DevManager. Strong on per-thread terminals UX, activity labels, resume cursors.

## 2. Already applied (2026-09-01)

From the herdr persistence audit, landed on `VisualDevManager`:

- `39caf754` reader never ends a terminal on PTY EOF/error; only observed child exit does.
- `762fe36c` debug hosts detach on window close; host spawned with `CREATE_NO_WINDOW` and conditional Job breakaway.
- `c99a0c5f` single-terminal teardown misses are reported, never escalated to a host abort.

Sub-projects specified the same day, drawing on all three sources: task shell terminals, shell restore, terminal screen history (see sibling specs).

## 3. Ideas worth porting

Ranked by value over cost. Size S/M/L. "Where" names the file that embodies the idea in its source repo.

### Agent status and inbox

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 1 | Debounced idle confirmation and stable-blocker refresh: hold Working→Idle up to 700 ms unless idle chrome is visible; re-publish a stable blocker every 800 ms | herdr `src/pane/agent_detection.rs` | Stops tasks bouncing through Ready between tool calls; ~200 lines of pure functions over injected `Instant` | S |
| 2 | Delayed, cancellable, focus-suppressed notifications: re-arm on each change, fire only survivors, suppress while focused | herdr `src/app/actions.rs:3137-3336` | "Needs Me" must not ping for a blocker that resolves itself | S |
| 3 | Three-tier hook authority (full / identity-only / none) with per-source `--seq` and `--ttl-ms` | herdr `src/detect/mod.rs:295,307`, `src/metadata_tokens.rs:15` | herdr demoted Claude and Codex hooks to identity-only because they miss permission prompts; DevManager still trusts them for lifecycle | S |
| 4 | Manifest-driven screen detection as a fallback, with `skip_state_update` for viewer screens and an `explain` verb | herdr `src/detect/manifests/*.toml`, `src/detect/manifest.rs` | Fallback when a hook is missing or the agent is unsupported; start with three manifests, no OTA catalog | M |
| 5 | Agent mailbox with backlog replay on reattach and stalled-receiver detection; awareness broadcasts never queued | traycer `protocol/src/host/agent/inbox.ts:1-31` | Multiple CLIs per task need a supervised bus; replay-on-reattach plus stall timeout is the hard part | M |
| 6 | Todo/plan tool normalization across providers into one checklist model | traycer `protocol/src/host/agent/gui/task-todo-tools.ts` | One plan UI for Claude `TodoWrite` and Codex equivalents | M |
| 7 | "Done" derived from Idle plus not-yet-seen, where seen is set by focus, never by CLI reads | herdr `src/app/state.rs:966` | Ready vs Recent becomes derived rather than stored | S |

### Worktrees and environment

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 8 | Worktree evidence tiers (in-use / review / orphaned / merged / at-base / unreferenced), each green tier needing positive proof, with an ordered facts list shared by UI, CLI and agents | traycer `clients/shared/worktree/classify-worktree.ts:7-45` | Turns "safe to delete?" into an evidence ledger | M |
| 9 | Declarative per-task environment setup with OS overrides | traycer `.traycer/environment.json` | Worktree bootstrap contract; trivial to adopt | S |
| 10 | Worktree setup state machine and owned-submodule branches recorded separately from probes | traycer `protocol/src/host/worktree-schemas.ts:48-70` | Merge roll-up can require every owned branch landed | S |

### Host, install, update

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 11 | Windows install shape: versioned release dirs plus junction swap; staging → backup → move → verify (`--version`) → rollback; retry on locked files; install lock | herdr `website/install.ps1:427-837` | A running host cannot be overwritten; a junction can be repointed | M |
| 12 | Tri-state liveness probe on process start identity: alive / dead / could-not-determine | traycer `clients/shared/host-lock/process-identity.ts:15-30` | HostLock has the identity; the tri-state is the gap | S |
| 13 | Claim-gated host restart naming blockers (`runningTerminals`, `workingAgents`), `null` meaning "unstated" | traycer `protocol/src/host/restart/schemas.ts:32-51` | Needed before shell restore ships; extends `inspect_host_quit` | S |
| 14 | Structured doctor issues with `fixAction` and a copyable `terminalCommand` | traycer `protocol/src/host/maintenance/schemas.ts:11-25` | "Why is my host unhealthy" panel | M |
| 15 | Install generation as a content-derived stamp attested under the install lock | traycer `clients/traycer-cli/src/host/attested-install-runtime.ts` | Host self-update correctness | M |
| 16 | App-local ConPTY runtime pinned by hash and PE machine | herdr `packaging/windows/conpty.json`, `scripts/package_windows_conpty.py` | Removes OS-version ConPTY variance | M |

### CLI and automation

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 17 | `ctl` verbs: `wait --until`, `prompt --wait` (refuses if already blocked; fails on no observed change within 5 s), `read --source detection\|visible`, `wait-output --match`, `explain` | herdr `src/cli/spec.rs:344-694`, `src/api/wait.rs:626` | Makes DevManager scriptable by agents | M |
| 18 | Ship `skills/devmanager/SKILL.md` for agents driving `ctl`, pinned to the stable release | herdr `skills/herdr/SKILL.md` | Agents discover syntax from `--help`, read ids from JSON | S |
| 19 | Integration version stamps with `integration status` reporting current / outdated / needs repair; CST-preserving edits of `settings.json`; hooks exit 0 on every failure | herdr `src/integration/mod.rs`, `src/integration/claude_settings.rs` | Silent hook rot becomes visible and fixable | S |

### Terminal plumbing

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 20 | Server-computed terminal label from one process-table snapshot per tick fanned out to all terminals | t3code `apps/server/src/terminal/Manager.ts:1162`; herdr `src/platform/windows.rs:90` | Per-terminal activity at O(1) process walks; in sub-project 1 | S |
| 21 | Terminal PIDs feed port discovery so a dev server started in a shell surfaces as a preview | t3code `Manager.ts:1140`, `preview/PortScanner.ts:624` | Dev server becomes a task artifact | M |
| 22 | Selection → chat context chip | t3code `apps/web/src/components/chat/TerminalContextInlineChip.tsx` | Select output, attach to the composer | S |
| 23 | Two-lane client writer: reliable control lane plus capacity-1 droppable render lane | herdr `src/server/client_transport.rs:29` | Protects the desktop client from a slow Connect client | S |
| 24 | Git Bash exec-boundary runtime marker and cyclic parent-chain guard | herdr `src/platform/windows.rs:485,1143` | Process attribution under Git Bash; PID-reuse cycles must not hang the host | M |
| 25 | Win32 input-mode key encoding fallback (Shift+Enter, Escape) for ConPTY | herdr `src/platform/windows.rs:217-250` | If Shift+Enter fails to reach Claude Code on Windows | S |

### Engineering discipline

| # | Idea | Source and where | Why it fits | Size |
| --- | --- | --- | --- | --- |
| 26 | Hot-path architecture test: source scanner banning aggregate state reads and process inspection in render paths, one reason string per rule | herdr `scripts/test_ui_hot_path_architecture.py` | Permanent guardrail for GPUI paint paths | S |
| 27 | Protocol surface baseline diff in CI (name-set stability), without runtime handshake negotiation | traycer `protocol/src/host/RELEASE-INVARIANT.md`, `protocol/scripts/compat/check-protocol-compat.ts` | Catches a new method added where a version bump was needed | M |
| 28 | CHANGELOG curated at release time from commit bodies; never edited in feature work | herdr `AGENTS.md:200` | Removes the worst merge-conflict source | S |
| 29 | Risk classification before editing core surfaces; characterization tests first | herdr `AGENTS.md:132` | Process only | S |
| 30 | One canonical notification formatter shared by in-app feed and OS toasts, degrading unknown payloads to safe copy | traycer `protocol/src/host/notifications/presentation.ts:19-30` | Windows toasts and inbox cannot drift | S |

## 4. Not to port

- **herdr plugin marketplace, GitHub plugin install, `panes[]`, `link_handlers[]`**: 1,800+ lines plus a Cloudflare worker; unsigned code running inside the process that owns PTYs and worktrees. Take `actions[]` + `events[]` + a command log if extension demand appears.
- **herdr `--remote` SSH bootstrapper**: 3,400 lines solving what Connect already solves. Port only the ideas: matching-binary guarantee, "what a restart would disturb" confirmation, pushed keybinding snapshot.
- **herdr's 17-agent integration matrix**: DevManager's differentiation is Claude Code, Codex and Cursor done deeply.
- **traycer relay + Noise + attach-grant remote stack**: multi-quarter subsystem for a hosted relay; a localhost pipe with OS permissions is equivalent today.
- **traycer Yjs/CRDT artifact bodies and collaboration ACLs**: team product surface.
- **traycer per-method version negotiation, released floors, bidirectional bridging**: client and host ship together; adopt the baseline-diff test only.
- **t3code terminal layout in client localStorage**: needs suppression hacks against stale server metadata; the host DB owns the roster (sub-project 1 does this).
- **t3code PTYs as in-memory children that die on every server restart**: DevManager's durable host is the differentiator.
- **t3code 5,000-line plain-text history as the only scrollback model**: loses colors and alt-screen; sub-project 3 uses a styled serializer.

## 5. Suggested order after the three terminal sub-projects

1. Items 1, 2, 3 (agent status hygiene): small, independent, immediate user-visible improvement to Needs Me / Ready.
2. Item 13 (restart blockers) alongside sub-project 2.
3. Item 11 (Windows install shape) before the next packaged release.
4. Items 17, 18, 19 (ctl verbs, skill, integration status).
5. Item 8 (worktree evidence tiers) when worktree cleanup UX is next touched.
6. Items 26, 28 (engineering gates) at any quiet moment.
