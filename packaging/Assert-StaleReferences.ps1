#Requires -Version 5.1
<##
.SYNOPSIS
  Fail-closed stale-reference scan for active Phase 11 package sources.

.DESCRIPTION
  Scans only the roots and forbidden patterns declared by package-contract.json.
  Historical residuals are allowed only by the narrow, reviewed allowlist in
  packaging/stale-reference-historical-allowlist.txt. Intentional safety and
  compatibility references are recognized only by exact path+token contracts in
  staleReferenceScan.intentionalSafetyReferences; an unexpected forbidden token
  in those files still fails. This package check is intentionally separate from
  scripts/native-next/Invoke-CutoverAudit.ps1, which owns the authenticated,
  bounded cutover parity audit.
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

function Get-RelativePath([string]$Root, [string]$FullPath) {
    $rootNorm = (Resolve-Path -LiteralPath $Root).Path.Replace('\', '/').TrimEnd('/')
    $fullNorm = (Resolve-Path -LiteralPath $FullPath).Path.Replace('\', '/')
    if ($fullNorm.Length -le $rootNorm.Length -or
        -not $fullNorm.StartsWith($rootNorm + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
        Write-Failure "Scanned path escaped repository root: $FullPath"
    }
    return $fullNorm.Substring($rootNorm.Length + 1)
}

function Test-HistoricalAllowlisted([string]$RelativePath, [string[]]$Allowlist) {
    $rel = $RelativePath.Replace('\', '/')
    foreach ($entry in $Allowlist) {
        if (-not $entry) { continue }
        if ($entry.EndsWith('/')) {
            if ($rel.StartsWith($entry) -or $rel.Contains('/' + $entry.TrimEnd('/') + '/')) {
                return $true
            }
        } elseif ($rel -eq $entry) {
            return $true
        }
    }
    return $false
}

function Test-IntentionalSafetyReference {
    param(
        [Parameter(Mandatory = $true)][string]$RelativePath,
        [Parameter(Mandatory = $true)][string]$Pattern,
        [Parameter(Mandatory = $true)]$Intentional
    )

    $rel = $RelativePath.Replace('\', '/')
    foreach ($entry in @($Intentional)) {
        if (-not $entry) { continue }
        $entryPath = ([string]$entry.path).Replace('\', '/')
        if ($rel -ne $entryPath) { continue }
        $tokens = @($entry.tokens | ForEach-Object { [string]$_ })
        if ($tokens -contains $Pattern) {
            return $true
        }
    }
    return $false
}

function Assert-IntentionalSafetyContractShape {
    param(
        [Parameter(Mandatory = $true)]$Intentional,
        [Parameter(Mandatory = $true)][string[]]$ForbiddenPatterns,
        [Parameter(Mandatory = $true)][string]$Root
    )

    if (@($Intentional).Count -lt 1) {
        Write-Failure 'staleReferenceScan.intentionalSafetyReferences must declare at least one exact path+token contract'
    }

    $seenPaths = New-Object 'System.Collections.Generic.HashSet[string]'
    $byPath = @{}
    foreach ($entry in @($Intentional)) {
        $entryPath = ([string]$entry.path).Replace('\', '/').Trim()
        if (-not $entryPath -or $entryPath.Contains('..') -or $entryPath.StartsWith('/') -or $entryPath.EndsWith('/')) {
            Write-Failure "intentionalSafetyReferences path must be an exact relative file path, found '$entryPath'"
        }
        if (-not $seenPaths.Add($entryPath)) {
            Write-Failure "intentionalSafetyReferences path must be unique: $entryPath"
        }
        $full = Join-Path $Root ($entryPath.Replace('/', [IO.Path]::DirectorySeparatorChar))
        if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
            Write-Failure "intentionalSafetyReferences path missing on disk: $entryPath"
        }
        $tokens = @($entry.tokens | ForEach-Object { [string]$_ })
        if ($tokens.Count -lt 1) {
            Write-Failure "intentionalSafetyReferences for $entryPath must declare at least one token"
        }
        foreach ($token in $tokens) {
            if (-not $token) {
                Write-Failure "intentionalSafetyReferences for $entryPath contains an empty token"
            }
            if ($ForbiddenPatterns -notcontains $token) {
                Write-Failure "intentionalSafetyReferences token '$token' for $entryPath is not a forbiddenPatterns entry"
            }
        }
        $byPath[$entryPath] = $tokens
    }

    # Narrow matching self-check driven by contract values (no hard-coded stale
    # token literals in this script body): exact path+token passes; same path
    # with an undeclared forbidden token must not be treated as intentional.
    $handoffPath = 'src/updater/handoff.rs'
    if (-not $byPath.ContainsKey($handoffPath)) {
        Write-Failure "intentional safety contract missing required path $handoffPath"
    }
    foreach ($token in @($byPath[$handoffPath])) {
        if (-not (Test-IntentionalSafetyReference -RelativePath $handoffPath -Pattern $token -Intentional $Intentional)) {
            Write-Failure "intentional safety contract failed exact path+token recognition for $handoffPath"
        }
    }
    $undeclared = @(
        $ForbiddenPatterns |
            Where-Object { @($byPath[$handoffPath]) -notcontains $_ } |
            Select-Object -First 1
    )
    if ($undeclared.Count -ne 1) {
        Write-Failure "intentional safety contract for $handoffPath must leave at least one forbidden token undeclared"
    }
    if (Test-IntentionalSafetyReference -RelativePath $handoffPath -Pattern $undeclared[0] -Intentional $Intentional) {
        Write-Failure "intentional safety contract must not blanket-allow undeclared tokens in $handoffPath"
    }
    if (Test-IntentionalSafetyReference -RelativePath 'src/updater/other.rs' -Pattern $byPath[$handoffPath][0] -Intentional $Intentional) {
        Write-Failure 'intentional safety contract must not match by token alone without an exact path'
    }

    $cutoverPath = 'scripts/native-next/Invoke-CutoverAudit.ps1'
    if (-not $byPath.ContainsKey($cutoverPath)) {
        Write-Failure "intentional safety contract missing required path $cutoverPath"
    }
    if (@($byPath[$cutoverPath]).Count -lt 2) {
        Write-Failure "intentional safety contract for $cutoverPath must declare multiple protected/forbidden tokens"
    }
    foreach ($token in @($byPath[$cutoverPath])) {
        if (-not (Test-IntentionalSafetyReference -RelativePath $cutoverPath -Pattern $token -Intentional $Intentional)) {
            Write-Failure "intentional safety contract failed exact path+token recognition for $cutoverPath"
        }
    }
}

$contractPath = Join-Path $RepoRoot 'packaging\package-contract.json'
$allowlistPath = Join-Path $RepoRoot 'packaging\stale-reference-historical-allowlist.txt'
if (-not (Test-Path -LiteralPath $contractPath -PathType Leaf)) {
    Write-Failure "Missing package contract: $contractPath"
}
if (-not (Test-Path -LiteralPath $allowlistPath -PathType Leaf)) {
    Write-Failure "Missing stale-reference allowlist: $allowlistPath"
}

$contract = Get-Content -LiteralPath $contractPath -Raw | ConvertFrom-Json
$scan = $contract.staleReferenceScan
$roots = @($scan.scanRoots | ForEach-Object { [string]$_ })
$patterns = @($scan.forbiddenPatterns | ForEach-Object { [string]$_ })
$intentional = @($scan.intentionalSafetyReferences)
$allowlist = @(
    Get-Content -LiteralPath $allowlistPath |
        Where-Object { $_ -and -not $_.StartsWith('#') } |
        ForEach-Object { $_.Trim().Replace('\', '/') }
)
if ($roots.Count -lt 1 -or $patterns.Count -lt 1 -or $allowlist.Count -lt 1) {
    Write-Failure 'staleReferenceScan requires scan roots, forbidden patterns, and allowlist entries'
}

Assert-IntentionalSafetyContractShape -Intentional $intentional -ForbiddenPatterns $patterns -Root $RepoRoot

$failures = New-Object 'System.Collections.Generic.List[string]'
$scanned = 0
$intentionalHits = 0
foreach ($root in $roots) {
    $path = Join-Path $RepoRoot $root
    if (-not (Test-Path -LiteralPath $path)) {
        $failures.Add("missing scan root: $root")
        continue
    }
    $files = @()
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $files = @(Get-Item -LiteralPath $path)
    } else {
        $files = @(Get-ChildItem -LiteralPath $path -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Extension -in @('.md', '.yml', '.yaml', '.ps1', '.json', '.rs', '.toml', '.txt', '.sh')
            })
    }
    foreach ($file in $files) {
        $relative = Get-RelativePath -Root $RepoRoot -FullPath $file.FullName
        if (Test-HistoricalAllowlisted -RelativePath $relative -Allowlist $allowlist) {
            continue
        }
        $text = Get-Content -LiteralPath $file.FullName -Raw -ErrorAction SilentlyContinue
        if (-not $text) { continue }
        $scanned += 1
        foreach ($pattern in $patterns) {
            if ($text.Contains($pattern)) {
                if (Test-IntentionalSafetyReference -RelativePath $relative -Pattern $pattern -Intentional $intentional) {
                    $intentionalHits += 1
                    continue
                }
                $failures.Add("stale reference '$pattern' in $relative")
            }
        }
    }
}

if ($failures.Count -gt 0) {
    Write-Failure ("Stale reference scan failed:`n - " + ($failures -join "`n - "))
}
Write-Host ("Stale reference scan passed ({0} files scanned; {1} historical allowlist entries; {2} intentional path+token hits)." -f $scanned, $allowlist.Count, $intentionalHits)
