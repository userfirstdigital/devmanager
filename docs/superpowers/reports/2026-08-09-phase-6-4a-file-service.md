# Phase 6 Task 6.4 file-service report

## Scope

This slice provides the host-owned workspace-file service and temporary
fixture behavioral tests. The service can only be constructed inside the
crate from an `ApprovedWorkspaceRoot` token; integration tests use the
test-only constructor with `tempfile` roots. The minimal `workspace::files`
module export exposes the typed service surface without exposing an arbitrary
filesystem backend constructor.

Artifact persistence/import/export, UI panels, host command registration,
trash integration, and SQLite metadata remain later work outside this owned
slice.

## Contract and limits

- The approved root and every ancestor are checked before canonicalization;
  every operation revalidates the root, parents, final targets, and canonical
  containment. Reparse points, symlinks, junctions, traversal, device paths,
  alternate data streams, Windows reserved names, invalid/control characters,
  and trailing-dot/space aliases fail closed.
- Reads are forward-only, capped at 64 KiB per chunk and 4 MiB total. Opened
  handle metadata and the final handle path are compared with the resolved
  target to reject path substitution during a read. Text line pages are
  capped at 100,000 lines and 64 lines per page; binary content is never
  decoded as text.
- Metadata listings are body-free, sorted deterministically, capped at 256
  entries, and exposed through deterministic 64-entry-or-smaller pages with
  explicit offsets and next offsets.
- Recursive search is UTF-8-only, skips binary files, and bounds query size,
  scanned entries/files/bytes, matching lines, and result count. Exceeding a
  bound returns an explicit error rather than silently truncating.
- Every public filesystem operation (listing, reads, search, revisions, writes,
  and deletes) consumes one RAII admission permit from the eight-operation cap.
  Writes and deletes require separate typed plans with expected revisions;
  writes use a flushed same-directory temporary and an atomic replace/no-clobber
  operation only during plan execution.

## Safety evidence

The tests use temporary directories and mutable files only. They cover sorted
metadata and overflow, strict relative paths, text/binary reads, chunking and
hashes, bounded pages/line pages/search, operation admission, atomic writes,
revision conflicts, deletion previews, secret-like classification, root
symlink/junction rejection, bound-root replacement, and reparse swaps during
planned writes. Windows link fixtures use an ordinary symlink when permitted
and an unprivileged directory junction otherwise.

Focused verification passed:

`cargo test --test file_service -- --test-threads=1 --nocapture`

Result: 64 passed, 0 failed. The build also reports existing repository
warnings in unrelated kernel/host modules; no focused test failure remains.

## Phase 6.4 correction

Mutation planning/execution and tombstone discovery/recovery now consume one
absolute `OperationDeadline` with a shared atomic work budget. Target-lock map
and per-target lock acquisition use a deadline-aware condition variable, while
Drop paths use only nonblocking lock attempts. The deadline is checked before
and after authority rechecks, descriptor enumeration, revision hash reads,
temporary writes, fsync/flush, rename/reopen, cleanup, and bounded recovery.
Expired entry and deterministic mid-operation tests return `DeadlineExceeded`
before entry I/O and leave only private, recoverable state.

Linux explicit reservations and both temporary-file cleanup guards now share a
single `CleanupLedger` capped at `MAX_TOMBSTONES` (64). Uncertainty is retained
as a durable record containing the guarded parent/name and both identities,
never as an anonymous counter. Startup discovery recognizes strict private
temporary and tombstone names, verifies descriptor identity, and performs
reservation-plus-ledger insertion atomically; capacity, contention, expiry,
and Drop failures leave discoverable residue for bounded recovery. Cleanup
ownership transfers exactly once from `TemporaryFile` to `TempCleanup`, and
rollback/recovery rename and restore paths perform immediate pre/post deadline
checks without deleting substitutions. Repeated reservation-flood tests
confirm the occupancy cap remains stable.

Focused Windows tests passed in three serial repeats (64 tests per repeat), and
the complete serial library suite passed with 1,190 tests passing, 0 failed,
and 1 ignored. `cargo check --lib --bins`, `cargo fmt --all -- --check`, and
`git diff --check` also passed.
Cross-target checks for Linux and macOS were attempted but stopped in the
dependency build because this Windows environment has no `cc`/GNU toolchain;
no target binary was launched.

## Correction owner follow-up

The cleanup contract now uses strict identity-bound names for every recoverable
temporary, tombstone, and Linux authority entry. Each name carries an opaque
operation nonce plus the expected parent and target identities; legacy or
missing pre-identity names remain visible foreign residue and are never
adopted. Temporary creation records the exact handle identity before the bound
name is exposed, and restart discovery requires the encoded parent/target and
the reopened identity to agree.

Linux replacement exchange creates an old-inode tombstone anchor before the
exchange, and post-effect errors return that generated name with committed
effect flags. Restart discovery never substitutes the expected-target field
for a temporary name's encoded created identity; the old-inode tombstone is
the exchange recovery anchor. Recovery
transfers the existing ledger slot in place, updates current authority path
and identity before post-move deadline checks, and never reserves a second slot
at the 64-entry cap. Authority startup scanning is caller-deadline bounded,
counts both directory and child work, and uses the same strict binding parser;
OnceLock initialization also accepts the operation deadline and cleans up an
empty candidate when setup expires.

Restore transfers now distinguish a committed restore from a post-effect
uncertain restore and publish the existing ledger path before any fsync or
deadline result. Linux hard-link setup falls back from capability-gated
`AT_EMPTY_PATH` only through an immediately revalidated source path, and a
successful quarantine never loses its generated tombstone on an observation or
sync error. Restart authority entries are accepted only from mode-0700
directories.

The correction adversaries cover unbound and relabelled same-name replacement,
full-capacity recovery, post-effect deadline flags/record publication, Linux
generated tombstone retention, wrong-path guards, and bounded authority
restart/init. Focused Windows verification and the complete library/bin/fmt/diff
gates are reported from the final correction-owner run; Linux/macOS execution
remains blocked by the missing cross-target GNU toolchain described above.

## Non-goals and dependencies

Artifact storage/IDs, content-addressed artifact files, import/export, UI,
host command wiring, trash/recoverability, checkpoint/Git integration, and
Connect projection remain dependent on their later Phase 6 tasks. A future
workspace binder must mint `ApprovedWorkspaceRoot` from its validated task
workspace before constructing this service.
