# Phase 6 Task 6.1: Config/workspace union correction report

Date: 2026-08-10
Candidate: `059e56e` plus the focused harness correction in this worktree
Scope: Luna-max receiving-review correction in the isolated worktree

## Correction delivered

- Removed the duplicate `src/lib.rs` inclusion of the integration harnesses.
  `tests/config_service.rs` and `tests/workspace_service.rs` now compile and
  execute as real integration targets.
- Replaced private fixture openers, raw project-pair constructors, private
  authority issuers, private temp/fault seams, and the private raw host start
  route with `ConfigStore::open_host`, the host-issued workspace mapping, and
  `HostRequestExecutor::start_supervised_with_config_store`.
- Added the public typed `WorkspaceProjectRoots::from_host_config_store`
  adapter. It accepts only a loaded host store plus revision/action/runtime
  fences; configured IDs and roots are read from the sealed issuer rather than
  supplied as caller-owned pairs.
- Repaired executable proof for strict external export bytes, strict unknown
  field policy, SSH credential-reference preservation, canonical authority
  leaves, concurrent store admission, opaque configured-ID mapping across
  reopen and root change, stale revision rejection, and wrong-root rebind
  behavior.

## TDD and verification evidence

The initial integration compile was intentionally reproduced before editing:
`config_service.rs` reported 24 import/private-API errors and
`workspace_service.rs` reported 7 private-API errors. The corrected compile
gate was then run with the isolated target:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase61-config-union-correction7
CARGO_BUILD_JOBS=1
cargo test --test config_service --test workspace_service --no-run       passed
```

Focused serial behavior results:

```text
cargo test --test config_service -- --test-threads=1       84 passed, 1 ignored
cargo test --test workspace_service -- --test-threads=1    30 passed
cargo check --bin devmanager-host --quiet                 passed
cargo test --lib -- --test-threads=1                      1,234 passed, 1 ignored
```

The `host_admission` diagnostic on the candidate ran 36 tests: 15 passed and
21 failed at the existing direct-bus task-creation helpers with
`HostAuthorityRequired`. The `tests/host_admission.rs` file has no diff from
the accepted `46a1200` sibling, and none of the owned harness corrections
touch it; these failures are therefore recorded as the known baseline, not
patched as Config/Workspace union regressions. A fresh accepted-sibling run
was attempted in its own target but its cold dependency build reached the
10-minute command ceiling before executing tests; its exact cargo/rustc tree
was stopped and verified absent afterward.

No production profile, installed DevManager process, production config, or
`session.json` was read or modified. All fixture/config/database paths were
under unique temporary directories.

## Remaining dependencies

- The direct-bus `HostAuthorityRequired` baseline remains owned by the host
  admission follow-up; no stale harness workaround or raw authority seam was
  added here.
