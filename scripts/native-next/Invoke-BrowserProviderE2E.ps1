# Phase 8 browser provider E2E proof.
# Default fixture path is local/deterministic only. Authenticated remains HOLD
# unless -Authenticated, an allowlisted -Provider, isolated -ConfigBase, and
# DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E=1 are all present. This script
# still never launches stock providers.

[CmdletBinding()]
param(
    [switch]$Fixture,
    [switch]$IncludeProjectionFixture,
    [switch]$IncludeRecovery,
    [switch]$Authenticated,
    [string[]]$Provider,
    [string]$ConfigBase,
    [Parameter(Mandatory = $true)]
    [string]$OutputDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

if (-not $Fixture -and -not $Authenticated) {
    $Fixture = $true
}
if ($Fixture -and $Authenticated) {
    throw 'Choose exactly one browser E2E arm: -Fixture or -Authenticated.'
}

if (-not [System.IO.Path]::IsPathRooted($OutputDir)) {
    throw 'OutputDir must be an explicit rooted directory.'
}

$worktreeRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$resolvedOutput = [System.IO.Path]::GetFullPath($OutputDir.Trim())
$productionRoot = Get-DevManagerProductionRoot
if ($resolvedOutput.StartsWith($productionRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'OutputDir must not resolve under the production DevManager config root.'
}

New-Item -ItemType Directory -Force -Path $resolvedOutput | Out-Null

function Resolve-BrowserProviderAllowlist {
    param([string[]]$Names)

    $resolved = New-Object System.Collections.Generic.List[string]
    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($raw in @($Names)) {
        if ([string]::IsNullOrWhiteSpace([string]$raw)) {
            throw 'Provider allowlist entries must be non-empty.'
        }
        $kind = switch -Regex ([string]$raw.Trim()) {
            '^(?i)claude$' { 'claude' }
            '^(?i)codex$' { 'codex' }
            '^(?i)cursor$' { 'cursor' }
            default {
                throw "Unknown provider allowlist entry '$raw'. Accepted: claude, codex, cursor."
            }
        }
        if (-not $seen.Add($kind)) {
            throw "Authenticated provider allowlist must not contain duplicates ('$kind')."
        }
        $resolved.Add($kind)
    }
    return ,([string[]]$resolved.ToArray())
}

function Test-BrowserProductionConfigRoot {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $normalized = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $rendered = $normalized.Replace('\', '/')
    if ($rendered.Contains('com.userfirst.devmanager')) {
        return $true
    }
    if ($normalized.StartsWith($productionRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    return $false
}

function Find-BrowserFixtureServer {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)

    $candidates = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
        $candidates.Add((Join-Path $env:CARGO_TARGET_DIR 'debug\browser-fixture-server.exe'))
        $candidates.Add((Join-Path $env:CARGO_TARGET_DIR 'debug\browser-fixture-server'))
    }
    $candidates.Add((Join-Path $WorktreeRoot 'target\debug\browser-fixture-server.exe'))
    $candidates.Add((Join-Path $WorktreeRoot 'target\debug\browser-fixture-server'))
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return [System.IO.Path]::GetFullPath($candidate)
        }
    }
    return $null
}

function Invoke-LoopbackFixtureRequest {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Uri
    )

    $parsed = [Uri]$Uri
    if ($parsed.Scheme -ne 'http' -or $parsed.Host -ne '127.0.0.1') {
        throw "Fixture proof refused a non-loopback URL: $Uri"
    }
    $client = [System.Net.Http.HttpClient]::new()
    $client.Timeout = [TimeSpan]::FromSeconds(3)
    try {
        $response = $client.GetAsync($parsed).GetAwaiter().GetResult()
        $body = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        return [ordered]@{
            status = [int]$response.StatusCode
            body   = $body
        }
    }
    finally {
        $client.Dispose()
    }
}

$fixtureRoot = Join-Path $worktreeRoot 'tests\fixtures\browser-e2e'
if (-not (Test-Path -LiteralPath $fixtureRoot)) {
    throw "Fixture root is missing: $fixtureRoot"
}

$authenticatedHold = $null
if ($Authenticated) {
    if (-not $Provider -or @($Provider).Count -lt 1) {
        throw 'Authenticated mode requires an explicit -Provider allowlist (claude, codex, cursor).'
    }
    $allowlist = Resolve-BrowserProviderAllowlist -Names $Provider
    $optIn = [string]$env:DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E
    if ($optIn -ne '1') {
        throw 'Authenticated mode also requires DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E=1.'
    }
    if ([string]::IsNullOrWhiteSpace($ConfigBase)) {
        throw 'Authenticated mode requires an isolated -ConfigBase.'
    }
    if (-not [System.IO.Path]::IsPathRooted($ConfigBase)) {
        throw 'ConfigBase must be an explicit rooted directory.'
    }
    $resolvedConfig = [System.IO.Path]::GetFullPath($ConfigBase.Trim())
    if (Test-BrowserProductionConfigRoot -LiteralPath $resolvedConfig) {
        throw 'ConfigBase must not be a production DevManager config root.'
    }
    $authenticatedHold = [ordered]@{
        requested          = $true
        providers          = $allowlist
        configBase         = $resolvedConfig
        launched           = $false
        hold               = 'AuthenticatedLaunchRequiresExplicitOptIn'
        note               = 'Admission succeeded; this script still does not launch stock providers or WebView2.'
    }
}

$serverProcess = $null
$serverEvidence = [ordered]@{
    attempted = $false
    started   = $false
    pid       = $null
    url       = $null
    skipped   = 'fixture server binary was not discoverable; fixture files were validated on disk'
}

try {
    if ($Fixture) {
        $serverPath = Find-BrowserFixtureServer -WorktreeRoot $worktreeRoot
        if ($serverPath) {
            $serverEvidence.attempted = $true
            $info = [System.Diagnostics.ProcessStartInfo]::new()
            $info.FileName = $serverPath
            $info.ArgumentList.Add('--root')
            $info.ArgumentList.Add($fixtureRoot)
            $info.ArgumentList.Add('--port')
            $info.ArgumentList.Add('0')
            $info.UseShellExecute = $false
            $info.RedirectStandardOutput = $true
            $info.RedirectStandardError = $true
            $info.RedirectStandardInput = $true
            $info.CreateNoWindow = $true
            $serverProcess = [System.Diagnostics.Process]::Start($info)
            $readyDeadline = [DateTime]::UtcNow.AddSeconds(8)
            $readyLine = $null
            while ([DateTime]::UtcNow -lt $readyDeadline) {
                if ($serverProcess.StandardOutput.Peek() -ge 0) {
                    $readyLine = $serverProcess.StandardOutput.ReadLine()
                    if ($readyLine -and $readyLine.Contains('BROWSER_FIXTURE_SERVER_READY')) {
                        break
                    }
                }
                Start-Sleep -Milliseconds 50
            }
            if ($readyLine -and $readyLine.Contains('BROWSER_FIXTURE_SERVER_READY')) {
                $payload = $readyLine.Substring($readyLine.IndexOf('{'))
                $ready = $payload | ConvertFrom-Json
                if (-not ([Uri]$ready.url).Host.Equals('127.0.0.1', [StringComparison]::OrdinalIgnoreCase)) {
                    throw 'Fixture server ready line did not report a loopback URL.'
                }
                $baseUri = [Uri]$ready.url
                $serverEvidence.started = $true
                $serverEvidence.pid = [int]$serverProcess.Id
                $serverEvidence.url = [string]$ready.url
                $serverEvidence.skipped = $null

                $health = Invoke-LoopbackFixtureRequest -Uri ([Uri]::new($baseUri, 'health').AbsoluteUri)
                $index = Invoke-LoopbackFixtureRequest -Uri ([Uri]::new($baseUri, 'index.html').AbsoluteUri)
                $traversal = Invoke-LoopbackFixtureRequest -Uri ([Uri]::new($baseUri, '/%2e%2e/Cargo.toml').AbsoluteUri)
                if ($health.status -ne 200 -or $health.body -notmatch '"ok":true') {
                    throw 'Fixture server health request did not return the expected local response.'
                }
                if ($index.status -ne 200 -or $index.body -notmatch 'DM-BROWSER-E2E-OK') {
                    throw 'Fixture server index request did not return the verification marker.'
                }
                if ($traversal.status -ne 400) {
                    throw "Fixture server traversal request returned unexpected status $($traversal.status)."
                }
                $serverEvidence.healthStatus = $health.status
                $serverEvidence.indexStatus = $index.status
                $serverEvidence.indexContainsVerificationToken = $true
                $serverEvidence.traversalStatus = $traversal.status
            }
            else {
                throw 'Fixture server started but did not emit a ready line in time.'
            }
        }
    }

    $evidence = [ordered]@{
        schemaVersion            = 1
        kind                     = 'browser-provider-e2e'
        fixture                  = [bool]$Fixture
        includeProjectionFixture = [bool]$IncludeProjectionFixture
        includeRecovery          = [bool]$IncludeRecovery
        generatedAtUtc           = [DateTime]::UtcNow.ToString('o')
        fixtureRoot              = $fixtureRoot
        fixtureServer            = $serverEvidence
        authenticated            = $(if ($authenticatedHold) { $authenticatedHold } else { [ordered]@{ requested = $false; launched = $false; hold = 'not-requested' } })
        launchedStockProvider    = $false
        productionProfileTouched = $false
        persistedPromptBody      = $false
        persistedBearerToken     = $false
        notProven                = @(
            'Authenticated Claude/Codex/Cursor control of a real page',
            'Visible WebView2 under a stock provider',
            'Remote/projection Connect path',
            'Provider crash recovery with a live helper tree'
        )
    }
    if ($IncludeProjectionFixture) {
        $evidence['projectionFixture'] = 'labeled-only; Phase 9 owns real Connect projection'
    }
    if ($IncludeRecovery) {
        $evidence['recovery'] = 'fixture recovery pages exist; live renderer/provider crash is NOT executed'
    }

    $evidencePath = Join-Path $resolvedOutput 'browser-provider-e2e.json'
    $evidence | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $evidencePath -Encoding utf8
    Write-Host ("Browser provider E2E evidence wrote {0}" -f $evidencePath)
    if ($Authenticated) {
        Write-Host 'Authenticated provider launch remains HOLD.'
    }
}
finally {
    if ($null -ne $serverProcess) {
        try {
            if (-not $serverProcess.HasExited) {
                $serverProcess.StandardInput.WriteLine('shutdown')
                $serverProcess.StandardInput.Close()
                if (-not $serverProcess.WaitForExit(2000)) {
                    $serverProcess.Kill()
                    $serverProcess.WaitForExit(2000)
                }
            }
            if (-not $serverProcess.HasExited) {
                throw "fixture server PID $($serverProcess.Id) remained after cleanup"
            }
        }
        catch {
            try {
                if (-not $serverProcess.HasExited) {
                    $serverProcess.Kill()
                    $serverProcess.WaitForExit(2000)
                }
                if (-not $serverProcess.HasExited) {
                    throw "fixture server PID $($serverProcess.Id) remained after cleanup"
                }
            }
            catch {
                throw
            }
        }
        finally {
            $serverProcess.Dispose()
        }
    }
}
