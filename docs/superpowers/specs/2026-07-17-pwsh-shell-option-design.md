# PowerShell 7 (pwsh) Shell Option — Design

Date: 2026-07-17
Status: Draft, pending user approval

## Problem

Windows settings offer only Bash (Git Bash), PowerShell (Windows PowerShell
5.1), and CMD as the default terminal shell (`DefaultTerminal`,
`src/models/config.rs:91`). PowerShell 7 (`pwsh`) is not selectable, even
though the rest of the codebase already treats `pwsh` as a first-class
PowerShell-kind shell (`claude_shell_kind`, command quoting).

Requirement: add PowerShell 7 as a selectable shell. If it is not installed,
the settings UI must warn and refuse the selection.

## Design

### Model

- Add `DefaultTerminal::Pwsh`, serialized as `"pwsh"` (existing
  `rename_all = "lowercase"`). Label: "PowerShell 7 (pwsh)".
- Cycle/selection order: Bash → PowerShell → PowerShell 7 → CMD.

### Availability detection

- New helper `pwsh_program() -> Option<PathBuf>` that locates `pwsh.exe` via
  PATH (honoring PATHEXT, same approach as the existing executable
  resolution), falling back to the conventional install location
  `%ProgramFiles%\PowerShell\7\pwsh.exe`.
- Detection runs when the settings editor opens (cached for the editor's
  lifetime, re-checked on each open — no background polling).

### Settings UI behavior

- If `pwsh` is not found, the PowerShell 7 option renders disabled with a
  warning ("PowerShell 7 is not installed"), and selecting it is rejected:
  the select action no-ops with the warning, and the cycle action skips the
  unavailable option. The stored setting is never silently rewritten.

### Launch behavior

- `build_interactive_shell_command` maps `Pwsh` to the resolved `pwsh.exe`
  path at launch time.
- If the setting is `pwsh` but resolution fails at launch (uninstalled after
  being selected, or config edited by hand), fall back to `powershell.exe`
  and surface a session warning — never fail the terminal launch.

### Unchanged

- `claude_shell_kind` and shell quoting already classify `pwsh`/`pwsh.exe`
  as PowerShell-kind; no changes needed.
- Non-Windows platforms: `Pwsh` behaves like the other non-Bash variants
  (macOS ignores `default_terminal`; Linux resolves via `resolve_shell_path`).
- Bash shell-integration args are Bash-only and unaffected.

## Testing

- Unit: serde round-trip for `"pwsh"`; legacy configs (no `pwsh`) still
  deserialize; cycle order skips Pwsh when unavailable; launch mapping picks
  the resolved path; launch fallback to `powershell.exe` when resolution
  fails.
- Manual QA: with pwsh installed, select PowerShell 7 and open a terminal
  (verify `$PSVersionTable.PSVersion` is 7.x); without pwsh (or with a
  simulated missing resolver), verify the option is disabled with the
  warning and cannot be selected.
