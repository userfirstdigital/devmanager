#Requires -Version 5.1
<#
.SYNOPSIS
  Fail-closed package contract checks for DevManager release staging.

.DESCRIPTION
  Validates packaging/package-contract.json against Cargo.toml metadata and an
  optional staged package / release binary directory. Does not build, sign, or
  publish. Preserves updater metadata generation by refusing to invent
  latest.json contents.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$StageDir = '',
    [string]$TargetReleaseDir = '',
    [switch]$SkipBinaryPresence
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-Failure([string]$Message) {
    Write-Error -Message $Message -ErrorAction Stop
}

function Get-TomlPackageVersion([string]$CargoTomlPath) {
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $CargoTomlPath) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '[package]') {
            $inPackage = $true
            continue
        }
        if ($inPackage -and $trimmed.StartsWith('[') -and $trimmed -ne '[package]') {
            break
        }
        if ($inPackage -and $trimmed -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    Write-Failure "Failed to read package.version from $CargoTomlPath"
}

function Test-PackagerBinaryList([string]$CargoTomlText, [object[]]$ExpectedBinaries) {
    foreach ($binary in $ExpectedBinaries) {
        $name = [string]$binary.name
        $mainLiteral = if ([bool]$binary.main) { 'true' } else { 'false' }
        if ($CargoTomlText -notmatch [regex]::Escape("path = `"$name`"")) {
            Write-Failure "Cargo.toml packager binaries missing path = `"$name`""
        }
        # Require an explicit binaries table that pairs path + main nearby.
        $pattern = "(?s)\[\[package\.metadata\.packager\.binaries\]\]\s*path\s*=\s*`"$([regex]::Escape($name))`"\s*main\s*=\s*$mainLiteral"
        if ($CargoTomlText -notmatch $pattern) {
            Write-Failure "Cargo.toml missing explicit packager binary entry for $name (main=$mainLiteral)"
        }
    }
    foreach ($forbidden in @('devmanager-next', 'devmanager-process-test-helper')) {
        $pattern = "(?s)\[\[package\.metadata\.packager\.binaries\]\]\s*path\s*=\s*`"$([regex]::Escape($forbidden))`""
        if ($CargoTomlText -match $pattern) {
            Write-Failure "Cargo.toml packager binaries must not include forbidden binary $forbidden"
        }
    }
}

$contractPath = Join-Path $RepoRoot 'packaging\package-contract.json'
if (-not (Test-Path -LiteralPath $contractPath -PathType Leaf)) {
    Write-Failure "Missing package contract: $contractPath"
}

$contract = Get-Content -LiteralPath $contractPath -Raw | ConvertFrom-Json
$cargoTomlPath = Join-Path $RepoRoot 'Cargo.toml'
$cargoTomlText = Get-Content -LiteralPath $cargoTomlPath -Raw
$version = Get-TomlPackageVersion -CargoTomlPath $cargoTomlPath

$before = [string]$contract.beforePackagingCommand
if ($cargoTomlText -notmatch [regex]::Escape("before-packaging-command = `"$before`"")) {
    Write-Failure "Cargo.toml before-packaging-command must equal '$before'"
}

Test-PackagerBinaryList -CargoTomlText $cargoTomlText -ExpectedBinaries $contract.binaries

foreach ($icon in $contract.icons) {
    $iconPath = Join-Path $RepoRoot ([string]$icon)
    if (-not (Test-Path -LiteralPath $iconPath -PathType Leaf)) {
        Write-Failure "Missing packaging icon: $iconPath"
    }
}

foreach ($resource in $contract.resources) {
    $resourcePath = Join-Path $RepoRoot ([string]$resource)
    if (-not (Test-Path -LiteralPath $resourcePath)) {
        Write-Failure "Missing packaging resource root: $resourcePath"
    }
}

$exclusionsPath = Join-Path $RepoRoot 'packaging\exclusions.txt'
if (-not (Test-Path -LiteralPath $exclusionsPath -PathType Leaf)) {
    Write-Failure "Missing exclusions list: $exclusionsPath"
}

if (-not $SkipBinaryPresence) {
    $exeSuffix = if ($env:OS -match 'Windows' -or $env:WINDIR) { '.exe' } else { '' }

    if ($TargetReleaseDir) {
        if (-not (Test-Path -LiteralPath $TargetReleaseDir)) {
            Write-Failure "Package search root does not exist: $TargetReleaseDir"
        }
        foreach ($binary in $contract.binaries) {
            $fileName = "$($binary.name)$exeSuffix"
            $matches = @(Get-ChildItem -LiteralPath $TargetReleaseDir -File -Filter $fileName -ErrorAction SilentlyContinue)
            if ($matches.Count -lt 1) {
                Write-Failure "Required binary $fileName not found under $TargetReleaseDir"
            }
        }
        foreach ($forbidden in $contract.forbiddenBinaries) {
            $fileName = "$forbidden$exeSuffix"
            $matches = @(Get-ChildItem -LiteralPath $TargetReleaseDir -File -Filter $fileName -ErrorAction SilentlyContinue)
            if ($matches.Count -gt 0) {
                Write-Failure "Forbidden binary $fileName found under $TargetReleaseDir"
            }
        }
    }

    if ($StageDir) {
        if (-not (Test-Path -LiteralPath $StageDir)) {
            Write-Failure "Package search root does not exist: $StageDir"
        }
        foreach ($forbidden in $contract.forbiddenBinaries) {
            $patterns = @($forbidden, "$forbidden.exe", "$forbidden.app")
            foreach ($pattern in $patterns) {
                $matches = @(Get-ChildItem -LiteralPath $StageDir -Recurse -Force -ErrorAction SilentlyContinue |
                    Where-Object { $_.Name -like $pattern -or $_.Name -like "$pattern.*" })
                if ($matches.Count -gt 0) {
                    Write-Failure "Forbidden packaged path matching $pattern found under $StageDir"
                }
            }
        }
        foreach ($exclude in (Get-Content -LiteralPath $exclusionsPath | Where-Object { $_ -and -not $_.StartsWith('#') })) {
            $token = $exclude.Trim()
            if ($token -in @('session.json', 'config.json', 'remote.json', '.env')) {
                $hits = @(Get-ChildItem -LiteralPath $StageDir -Recurse -Force -File -Filter $token -ErrorAction SilentlyContinue)
                if ($hits.Count -gt 0) {
                    Write-Failure "Excluded payload '$token' found under $StageDir"
                }
            }
        }
    }

    if (-not $TargetReleaseDir -and -not $StageDir) {
        Write-Host "Package contract source checks passed (no stage/target directories supplied)."
        exit 0
    }
}

Write-Host ("Package contract passed for DevManager {0} (protocol {1}.{2})." -f $version, $contract.protocol.major, $contract.protocol.minor)
