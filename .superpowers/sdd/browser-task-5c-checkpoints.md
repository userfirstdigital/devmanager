# Task 5C Sequential Checkpoints

Source requirements: `browser-task-5c-brief.md`. Start only after Task 5B is fully approved. Every checkpoint uses strict RED-to-GREEN TDD, one implementer, immutable-range spec/quality review, and a separate commit.

Status (2026-07-17): checkpoint 1 is complete through the strict-TDD independent-review hardening follow-up recorded in `browser-task-5c-report.md`. Checkpoints 2 through 12 are pending and were not started.

1. **Strict recipe wire/store — complete** — strengthened `recipes.rs` with one duplicate-member-rejecting, deny-unknown-fields v1 wire; checked public nested deserialization; typed action/value/wait/assertion structures; reference/type validation; Secret/File default prohibition; redaction gates; deterministic pretty JSON plus newline; contained list/load/save; Windows reparse/operation-boundary checks; bounded owned stale-temp scavenging; and atomic sibling-temp replacement with failure coverage. See `browser-task-5c-report.md`.
2. **Pure recording/review domain** — new platform-neutral recorder keyed by workspace; off by default; reserve/commit ordering for async user+agent actions; coalescing; generated secret/file inputs; review mutations/discard.
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
