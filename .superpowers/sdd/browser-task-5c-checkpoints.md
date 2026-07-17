# Task 5C Sequential Checkpoints

Source requirements: `browser-task-5c-brief.md`. Start only after Task 5B is fully approved. Every checkpoint uses strict RED-to-GREEN TDD, one implementer, immutable-range spec/quality review, and a separate commit.

Status (2026-07-17): checkpoints 1 and 2 are complete through their strict RED-to-GREEN implementations and independent-review hardening follow-ups recorded in `browser-task-5c-report.md`. Checkpoints 3 through 12 are pending and were not started.

1. **Strict recipe wire/store — complete** — strengthened `recipes.rs` with one duplicate-member-rejecting, deny-unknown-fields v1 wire; checked public nested deserialization; typed action/value/wait/assertion structures; reference/type validation; Secret/File default prohibition; redaction gates; deterministic pretty JSON plus newline; contained list/load/save; Windows reparse/operation-boundary checks; bounded owned stale-temp scavenging; and atomic sibling-temp replacement with failure coverage. See `browser-task-5c-report.md`.
2. **Pure recording/review domain — complete** — added a bounded platform-neutral recorder keyed only by `BrowserWorkspaceKey`; inactive by default; exact instance and reserve/commit/cancel source ordering; workspace/restart/late-completion fencing; safe coalescing; content-free generated Secret/File inputs; immutable review metadata, step/input/wait/assertion mutations, strict v1 save handoff, and discard. Independent-review hardening adds repeated percent-decoded query/fragment credential inspection, fail-closed malformed encoding and unresolved generic input references, generated-input provenance/step-delete garbage collection, and atomic fixed input/assertion bounds. No store/UI/IPC/MCP/host/replay wiring is present. See `browser-task-5c-report.md`.
3. **Semantic page recording IPC** — bounded strict IPC active only during recording; trusted semantic click/type/select/navigation/tab/upload/download; no password/file/clipboard values.
4. **Unified host capture** — feed user chrome and successful queued agent actions into the same recorder; runtime password inspection creates unset secret input before value retention.
5. **Pane Record/review UI** — explicit start/stop/review/discard/save, metadata, delete/reorder, typed-input conversion, waits/assertions, validation and preview.
6. **Exact `browser_recording` MCP** — `status|start|stop|review|discard|save`, exact risk/nonblank intent, no route/secret fields, compact resources.
7. **Replay compiler/status/cancellation lease** — validated public inputs and `Pending|Running|NeedsUserSecret|PausedLocatorRepair|Completed|Failed|Cancelled`; one replay-lifetime cancellation lease spanning gaps between steps.
8. **Replay via existing queue/approval/journal** — start URL/viewport then one controller step at a time; normal approval/runtime risk/fencing/journal; typed assertion failure stops later steps.
9. **Memory-only secrets** — non-Debug/non-Serialize zeroizing bundle; MCP cannot submit secret inputs; names-only masked UI event; no secret in BrowserAction/status/resource/journal.
10. **Typed locator failure/repair state** — typed `LocatorNotFound`, pinned fresh screenshot/snapshot, stable route-bound repair instance.
11. **Repair preview/atomic apply** — user/agent replacement through one instance; current revision validation; highlight only; explicit confirmation; exact-step atomic recipe update; optional same-step resume.
12. **Exact `browser_workflow` MCP/lifecycle** — `list|get|replay|status|cancel|repairPreview|repairApply`; Stop/direct input/close/switch/reset/revocation/process loss/shutdown share cancellation; unsupported/macOS compile gate.

High-risk review gates: no secret ever enters serializable/debuggable action state; cancellation works between controller calls; Windows atomic replacement leaves old or new file; async recorder ordering; workspace-switch cancellation; repair cannot apply against changed recipe/page/workspace.
