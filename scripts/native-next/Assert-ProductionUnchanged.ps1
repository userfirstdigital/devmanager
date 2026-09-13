# Fail-closed comparison of current production state against a baseline.
# Read-only; capture owns evidence output.
# Does not read/hash session.json and has no launch/stop/kill authority.
# Always targets the real unprofiled production root (no ProductionRoot override).
# BaselinePath must resolve under this worktree's .devmanager-next\evidence tree.
# Relative BaselinePath values resolve against the worktree root (from PSScriptRoot), not process cwd.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BaselinePath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

$resolvedBaselinePath = Resolve-DevManagerEvidenceArgument -Path $BaselinePath -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $resolvedBaselinePath `
    -ProtectedProductionRoot (Get-DevManagerProductionRoot) `
    -AllowedEvidenceRoot $evidenceRoot

Assert-DevManagerProductionState -BaselinePath $resolvedBaselinePath

Write-Host ("Production unchanged relative to baseline {0}" -f $resolvedBaselinePath)
