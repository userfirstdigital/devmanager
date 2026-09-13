# Workspace Git manager

Target: the repository scope and Changes/History/AI commit workflow in 0.4.1,
using the current native shell tokens and shared button/input primitives.
The toolbar Git button opens without selecting or creating a task. A persistent
repository rail makes local changes and ahead/behind counts visible across the
configured project roots and folders, including repositories inside a base folder.
Task Changes remains scoped to that task's workspace.

Discovery uses configured roots, excludes generated/hidden trees and symlinks,
and searches four levels with explicit folder/repository limits. The host resolves
opaque, filesystem-identity-bound selectors on every operation. Browser Connect
cannot invoke this local desktop authority. Opening or refreshing the window is
read-only; stage, commit, fetch, pull, push and branch operations require gestures.
Sync counts reflect the last fetch; the UI states that limitation and provides
Fetch all to update them. Refresh repeats discovery and local status. Publishing
retains the named upstream and permits only that branch’s two upstream keys.

Visual acceptance: inspect the complete native window with two repositories and
no selected task; exercise repository switching, status refresh, staging, commit,
history and branch actions against disposable repositories. Keep existing task
Git tests passing. Windows interactive acceptance remains separate from Linux.

Linux stress verification exposed zero-link metadata observations while Git
atomically replaced HEAD/index/config files. The existing bounded metadata retry
now also covers the parent-directory snapshot and other operation-permitted
in-flight replacements. Static inputs, hard links, read-only operations and the
final post-effect proof retain strict validation.
