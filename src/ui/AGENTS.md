# Native confirmation ownership

- A multi-step destructive confirmation owns exact host command IDs, not only
  a task ID or the currently visible dialog. Cancel, replacement, failed queue
  admission, and closure must disarm follow-up commands and remove their retry
  records. Never evict an unresolved cancellation marker to admit new work.
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
