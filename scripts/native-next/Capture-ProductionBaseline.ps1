# Read-only production baseline capture.
# Writes only the requested evidence file; does not mutate production storage,
# does not read/hash session.json, and has no launch/stop/kill authority.
# Always targets the real unprofiled production root (no ProductionRoot override).
# OutputPath must resolve under this worktree's .devmanager-next\evidence tree.
# Relative OutputPath values resolve against the worktree root (from PSScriptRoot), not process cwd.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

$resolvedOutputPath = Resolve-DevManagerEvidenceArgument -Path $OutputPath -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $resolvedOutputPath `
    -ProtectedProductionRoot (Get-DevManagerProductionRoot) `
    -AllowedEvidenceRoot $evidenceRoot

$state = Get-DevManagerProductionState
Write-DevManagerBaseline -State $state -OutputPath $resolvedOutputPath

Write-Host ("Captured production baseline -> {0}" -f $resolvedOutputPath)
Write-Host ("productionRoot={0}" -f $state.productionRoot)
Write-Host ("config.exists={0}; length={1}; sha256={2}" -f $state.config.exists, $state.config.length, $state.config.sha256)
Write-Host ("remote.exists={0}; length={1}; sha256={2}" -f $state.remote.exists, $state.remote.length, $state.remote.sha256)
Write-Host ("sessionPath={0} (path only; not hashed)" -f $state.sessionPath)
Write-Host ("installedProcesses={0}" -f @($state.installedProcesses).Count)
