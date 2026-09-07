# Launch candidate, 2026-09-07

Working branch: `codex/launch-ready-20260907`.

The candidate combines `ui-redesign-w4` (grid geometry, restored layouts,
terminal refusal copy and narrow titles) with `ui-redesign-clean` (age-label
refresh, composer hints, terminal pointer regression and obsolete UI removal).
Neither historical branch report is acceptance evidence for this combined build.

## Completion checklist

- [x] Combine the two existing source branches without conflicts.
- [x] Resolve Linux compiler failures exposed by the combined all-target check.
- [x] Implement panel pin/unpin and a target chooser for swapping panels.
- [x] Preserve the zoomed task when swapping its panel.
- [x] Run the new layout and shell interaction regressions (994 UI tests passed).
- [x] Verify both default themes against every strict contrast floor.
- [x] Finish the production color-ownership source gate (29 theme tests passed).
- [x] Implement Claude task-list settings, additive tool enablement and TaskUpdate ingestion.
- [x] Verify provider settings (104 tests passed).
- [ ] Finish provider capability verdicts.
- [ ] Complete provider subagent attribution and tabs, with capability verdicts.
- [ ] Run the complete Rust library suite serially with its process helper built.
- [ ] Run required integration and final compiler checks on frozen source.
- [x] Web tests: 606 passed, one existing skip.
- [x] Web typecheck and production/PWA build after dependency updates.
- [x] Browser dependency audit: zero findings after compatible transitive updates.
- [ ] Capture the complete native shell and compare against the design references.
- [ ] Exercise divider dragging, keyboard questions/permissions, terminal input,
      copy and scrolling, provider resume/recovery, and restart persistence live.
- [ ] Validate packages, signatures and isolated install/update behavior on the
      launch platforms.
- [ ] Integrate the verified candidate into `VisualDevManager` and prepare push.
- [x] Prepare a Windows candidate CI workflow without publication steps.
- [ ] Run Windows CI (GitHub authentication is missing on this machine).

## Evidence and isolation

The Linux worktree is `/home/robin/Projects/devmanager-launch-20260907`; its
Cargo target is beneath that worktree. Compiler logs are sibling files named
`devmanager-launch-20260907-*.log`. Production configuration hashes and process
identity observations are in the worktree's local `launch-evidence` directory.
The daily checkout's original index and merge metadata were backed up there
before refreshing line endings; its staged source tree was preserved exactly.

The first successful `cargo check --locked --lib --bins --tests` completed on
Linux before the final menu/zoom refinements. It does not certify those later
edits. No installer, production app, provider conversation, tag or public release
has been changed by this candidate work.

## Current verification findings

The first complete serial Linux library attempt reported 101 failed tests and
then aborted during `clear_virtual_output_resets_terminal_snapshot`. The abort
reproduces independently: an already-exited child returns Unix `ESRCH`, which
was incorrectly handled as fatal. The candidate now recognizes that result and
still joins its owned actors. The teardown regression and the explicit
100 ms contention test now pass; real PTY tests use the unchanged production
5-second deadline. This suite
is not green. Windows-specific fixtures and other failures still need separate
classification against a Windows run.

The Claude lifecycle regression now exercises pending, active and completed
states through the production cockpit query. It caught a changing plan-step ID
on the third update; projection now uses the same complete replacement lineage
as the conversation fact ID. It passed in the serial library attempt.

Production `config.json` and `remote.json` hashes remained unchanged after the
attempt; no installed DevManager process was present before or afterward.
The test harness and Cargo process exited after the abort.

The original remote `VisualDevManager` head remains `d46fd569`. An HTTPS push
dry run failed for missing credentials; SSH also failed authentication. No
branch has been pushed, and the Windows candidate workflow has not run.

The follow-up compiler gate (`cargo check --locked --lib --bins --tests`) and
`cargo fmt --all -- --check` both pass for the checkpoint source. Focused
verification is green: 994 UI tests, 104 provider-settings tests, 29 theme tests,
and both the real shell-teardown and deterministic contention regressions.
These focused results do not replace the outstanding complete-suite verdict.
