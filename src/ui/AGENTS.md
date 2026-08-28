# Native multi-step action ownership

- A multi-step destructive confirmation owns exact host command IDs, not only
  a task ID or the currently visible dialog. Cancel, replacement, failed queue
  admission, and closure must disarm follow-up commands and remove their retry
  records. Never evict an unresolved cancellation marker to admit new work.
- The same ownership applies to reopen/start/send: capture the draft and attachment
  identities once, block duplicate activation, and advance only from the exact
  owned receipt plus current runtime fences. A queued-start receipt is not live
  readiness. Providers explicitly lacking conversation identity need a host-attested
  current-generation readiness result, never an invented ID or a PTY-text heuristic.
  Likewise, a saved launch preference is not confirmed live configuration: never
  display a new model or reduced access level as applied to a running provider
  unless the runtime has acknowledged that change.
- An accepted command can change the task revision or remove the task before
  its receipt reaches the UI. Admit that exact owned receipt using the host,
  connection, resource, and runtime-generation fences; do not reject it solely
  because its own projection changed. Reject a mismatched receipt ID.
- Once a physical Delete has been submitted, label dismissal as Close rather
  than promising cancellation. Late results must not revive retries, recreate
  dialogs, or modify a replacement confirmation.
- Regression tests must enter through the production epoch-fenced outcome
  handler. Cover both receipt/projection orders, admission failure, cancellation
  and replacement, late outcomes, and duplicate confirmation. Calling only an
  inner success helper bypasses the ownership checks that these tests must prove.
- Lifecycle visibility tests must follow canonical model events through the
  maintained index, Inbox and host-qualified fleet rows; synthetic render rows
  cannot prove that a task reaches the sidebar. Cover full snapshot, incremental
  transition and preview for Active, Done, Archive and Delete independently.
  Filtered lifecycle sections must retain the same bounded search work and
  continuation contract as the active list, including matches beyond page one.
