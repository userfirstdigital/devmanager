# Phase 6 Task 6.2: Workspace binding correction report

Date: 2026-08-09
Candidate: `711c155`
Scope: Luna-max receiving-review correction in the isolated worktree

## Correction delivered

### Host authority and exact admission

- `WorkspaceLeaseScope`, lease admissions, and internal generation keys are no
  longer public authority values. Admissions are private, non-`Copy`,
  non-`Clone`, one-shot values. They bind task, resource, client, connection,
  request, command nonce, coordinator, and monotonic generation; spent and
  revoked scopes are tombstoned.
- Workspace authorization is private-field, redacted proof issued only after
  host resolution. The kernel checks the exact task/project/client/connection/
  request/command/workspace tuple before the transaction and again immediately
  before commit while the file pins remain owned. Substitution produces no
  durable effect.
- The test-only command adapter now constructs the service with the intent's
  exact task ID; it no longer creates a fresh service task identity and then
  rejects its own authorization.
- Raw V1 `CreateTask` remains rejected at the authenticated host boundary.
  `CreateTaskV2` is resolved by the host before a durable `CreateTaskIntent`
  reaches the kernel. External creation choices use the configured host project
  root; caller-supplied external paths cannot become durable roots.

### Durable binding identity and fail-closed replay

- New `HostBound` and `ExternalWithFingerprint` references carry a host-issued
  fact containing project root, workspace root, repository root, common Git
  directory, linked-worktree admin directory, `.git` marker, `commondir`,
  `gitdir` back-reference, `HEAD`, branch, stable file identity, metadata byte
  length/digest, and a recomputed binding fingerprint.
- Main and external repository-backed bindings capture `HEAD`; linked
  worktrees capture both sides of the registration and reject duplicate
  metadata lines, forged markers, wrong admin roots, missing back-references,
  in-place rewrites, and same-path replacement.
- Handle-backed pins stay owned through authorization and the protected write;
  the transaction performs a final identity/content revalidation before
  commit. Windows opens use backup/reparse-point flags and explicitly reject
  final symlink/reparse ambiguity. Canonical final-path and stable-identity
  checks cover case, UNC, and extended-length aliases.
- Legacy Main/Worktree/External and old fingerprint variants remain wire
  readable where valid, but runtime reconstruction returns typed
  `RebindRequired`; no path-only replay or automatic migration is performed.

### Config IDs and diagnostics

- Host bootstrap preserves arbitrary validated bounded config IDs such as
  `project-native`/`project-*`. The existing config ID remains the canonical
  host map key; a deterministic domain `ProjectId` adapter is used only where
  the existing command domain requires its UUID-shaped type. No second root
  configuration is created.
- Authority, binding, repository identity, durable facts, leases, and workspace
  errors have bounded redacted Debug/Display surfaces. They do not disclose
  paths, IDs, file identities, or arbitrary underlying error strings.

## TDD and verification evidence

All Cargo commands used the isolated target:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase62-correction2
CARGO_BUILD_JOBS=1
```

RED was established by adding adversarial tests for forged lease dimensions,
spent/stale/revoked generations, durable replay replacement, linked metadata
tampering, V1 rebind requirements, host command substitution, config IDs, and
redacted diagnostics. The initial workspace integration baseline was 28
passing tests before the new contracts were made green.

Final focused results:

```text
cargo test --test workspace_service -- --test-threads=1       30 passed
cargo test --lib workspace::service::fingerprint_tests -- --test-threads=1
                                                               3 passed
cargo test --lib workspace::service::lease_security_tests -- --test-threads=1
                                                               6 passed
cargo test --lib host::connection::workspace_security_tests -- --test-threads=1
                                                               5 passed
cargo test --lib workspace::model::project_root_tests -- --test-threads=1
                                                               4 passed
cargo test --lib client::action::tests -- --test-threads=1  8 passed
cargo test --test cli_client -- --test-threads=1              9 passed
cargo check --bin devmanager-host                            passed
```

The complete serial library suite was also run. After correcting the exact
task identity in its fixture adapter, it finished with 1,210 passed, 2 failed,
and 1 ignored out of 1,213 tests. The only remaining failures are:

```text
host::connection::output_tests::detach_removes_exact_output_and_live_binding_before_ack_shutdown
host::connection::output_tests::duplex_non_quit_rejected_quit_and_command_id_collision_remain_caller_owned
```

Both fixtures configure `std::env::current_dir()` as a `Main` project. In this
isolated linked worktree that path is a linked worktree rather than the main
repository root, so the existing Main-root invariant rejects the create. The
focused host-security and CLI/bootstrap suites pass; no production code was
relaxed to make these environment-sensitive fixtures accept an invalid Main
root.

Final formatting and hygiene gates:

```text
cargo fmt --all -- --check       passed
git diff --check                 passed
cargo check --lib                passed
```

The post-suite exact-target process check found no Cargo, rustc, or Rust test
harness process under the isolated target. No production profiles, installed
DevManager state, or `session.json` were read or modified.

## Honest remaining dependencies

- Phase 5 UI still needs to present workspace choice and map it to the V2
  `WorkspaceRequest`; unresolved choice remains typed `Ask`/rejected.
- Phase 6.3 still owns Git worktree creation, collision-safe allocation, and
  base-commit capture. New worktrees remain pending unless already registered.
- File, Git, process, browser, and other consumers still need to acquire and
  retain exact scoped leases. Task 6.2 exposes only the fail-closed host seam;
  no consumer implementation was added.
