#Requires -Version 5.1
<#
.SYNOPSIS
  Machine-check THIRD_PARTY_NOTICES against Cargo.lock selected crypto and optional similar.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $RepoRoot) {
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
    $RepoRoot = (Resolve-Path (Join-Path $scriptDir '..')).Path
}

function Write-Failure([string]$Message) {
    Write-Error -Message $Message -ErrorAction Stop
}

$contract = Get-Content -LiteralPath (Join-Path $RepoRoot 'packaging\package-contract.json') -Raw | ConvertFrom-Json
$lockText = Get-Content -LiteralPath (Join-Path $RepoRoot 'Cargo.lock') -Raw
$notices = Get-Content -LiteralPath (Join-Path $RepoRoot 'THIRD_PARTY_NOTICES.md') -Raw
$cargoToml = Get-Content -LiteralPath (Join-Path $RepoRoot 'Cargo.toml') -Raw

function Get-LockVersions([string]$Name) {
    $versions = @()
    $regex = [regex]::new("(?m)^\[\[package\]\]\r?\nname = `"$([regex]::Escape($Name))`"\r?\nversion = `"([^`"]+)`"")
    foreach ($match in $regex.Matches($lockText)) {
        $versions += $match.Groups[1].Value
    }
    return $versions
}

function Test-RootDirectDependency([string]$Name) {
    $block = [regex]::Match(
        $lockText,
        '(?ms)^\[\[package\]\]\r?\nname = "devmanager"\r?\nversion = "[^"]+"\r?\ndependencies = \[(.*?)\]'
    )
    if (-not $block.Success) {
        Write-Failure 'Cargo.lock missing root devmanager package dependency list'
    }
    $deps = $block.Groups[1].Value
    return [regex]::IsMatch($deps, '"' + [regex]::Escape($Name) + '(?: [^"]+)?"')
}

$similarSpec = $contract.provenance.similar
$similarVersions = @(Get-LockVersions -Name ([string]$similarSpec.name))
if ($similarVersions.Count -gt 0) {
    if ($similarVersions -notcontains [string]$similarSpec.requiredVersion) {
        Write-Failure ("similar is locked as {0}, expected {1}" -f ($similarVersions -join ','), $similarSpec.requiredVersion)
    }
    if ($notices -notmatch [regex]::Escape("similar $($similarSpec.requiredVersion)")) {
        Write-Failure "THIRD_PARTY_NOTICES.md must document locked similar $($similarSpec.requiredVersion)"
    }
    Write-Host "similar $($similarSpec.requiredVersion) locked and noticed"
} else {
    if (-not [bool]$similarSpec.requiredOnlyIfLocked) {
        Write-Failure 'similar is required but not present in Cargo.lock'
    }
    if ($notices -notmatch 'NOT_LOCKED|not yet activated|not present in `Cargo\.lock`|currently not locked') {
        Write-Failure 'THIRD_PARTY_NOTICES.md must state similar is required only if locked / currently not locked'
    }
    Write-Host 'similar not locked; notices correctly treat it as conditional'
}

foreach ($entry in $contract.provenance.selectedCrypto) {
    $name = [string]$entry.name
    $expected = [string]$entry.version
    $versions = @(Get-LockVersions -Name $name)
    if ($versions.Count -lt 1) {
        Write-Failure "selected crypto crate missing from Cargo.lock: $name"
    }
    if ($versions -notcontains $expected) {
        Write-Failure ("Cargo.lock {0} versions {1} do not include required {2}" -f $name, ($versions -join ','), $expected)
    }
    if ([bool]$entry.direct -and -not (Test-RootDirectDependency -Name $name)) {
        Write-Failure "selected crypto crate $name must be a direct root dependency"
    }
    if ($notices -notmatch [regex]::Escape("$name") -or $notices -notmatch [regex]::Escape($expected)) {
        Write-Failure "THIRD_PARTY_NOTICES.md must include exact $name $expected"
    }
    Write-Host "crypto ok $name $expected"
}

foreach ($entry in $contract.provenance.requiredDirect) {
    $name = [string]$entry.name
    $expected = [string]$entry.version
    $versions = @(Get-LockVersions -Name $name)
    if ($versions -notcontains $expected) {
        Write-Failure ("required direct crate {0} expected {1}, lock has {2}" -f $name, $expected, ($versions -join ','))
    }
    if ($notices -notmatch [regex]::Escape($name)) {
        Write-Failure "THIRD_PARTY_NOTICES.md missing $name"
    }
}

if ($cargoToml -notmatch 'rustls') {
    Write-Failure 'Cargo.toml missing rustls declaration'
}

Write-Host 'Third-party provenance machine-check passed against Cargo.lock'
