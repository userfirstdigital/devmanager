#Requires -Version 5.1
<#
.SYNOPSIS
  Fail-closed release-candidate packaging gates (no publish).

.DESCRIPTION
  Confirms package contract source validity, stale-reference scan, and optional
  payload assertion. Never publishes, tags, or mutates production installs.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$TargetReleaseDir = '',
    [string]$StageDir = '',
    [string]$PayloadDir = '',
    [switch]$ExtractInstallers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $RepoRoot) {
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
    $RepoRoot = (Resolve-Path (Join-Path $scriptDir '../..')).Path
}

Write-Host 'Release packaging is independent from publication.'
& (Join-Path $RepoRoot 'packaging\Assert-StaleReferences.ps1') -RepoRoot $RepoRoot
& (Join-Path $RepoRoot 'packaging\Assert-ThirdPartyProvenance.ps1') -RepoRoot $RepoRoot
& (Join-Path $RepoRoot 'packaging\Assert-PackageContract.ps1') `
    -RepoRoot $RepoRoot `
    -TargetReleaseDir $TargetReleaseDir `
    -StageDir $StageDir `
    -PayloadDir $PayloadDir `
    -ExtractInstallers:$ExtractInstallers `
    -SkipBinaryPresence:(-not ($TargetReleaseDir -or $StageDir -or $PayloadDir))

Write-Host 'Release candidate packaging gates passed (publication still requires manual approval of an existing draft tag).'
