# Phase 8 portable browser surface proof.
# Default/Red/Green/All never launch WebView2, GPUI, installed DevManager,
# or stock providers. OutputDir is required so evidence stays explicit.

[CmdletBinding()]
param(
    [ValidateSet('Red', 'Green', 'All')]
    [string]$Stage = 'All',
    [switch]$AllDpi,
    [switch]$ClientCrash,
    [switch]$HostRecovery,
    [Parameter(Mandatory = $true)]
    [string]$OutputDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

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

$scriptPath = $MyInvocation.MyCommand.Path
$surfaceTest = Join-Path $worktreeRoot 'tests\browser_surface.rs'
$serverSource = Join-Path $worktreeRoot 'src\bin\browser-fixture-server.rs'
$scriptText = Get-Content -LiteralPath $scriptPath -Raw
$testText = Get-Content -LiteralPath $surfaceTest -Raw
$serverText = Get-Content -LiteralPath $serverSource -Raw

$scenarioFilters = New-Object System.Collections.Generic.List[string]
if ($AllDpi) {
    $scenarioFilters.Add('dpi_and_bounds_matrix_updates_physical_geometry')
}
if ($ClientCrash) {
    $scenarioFilters.Add('client_crash_detaches_and_allows_reattach_to_a_new_client')
}
if ($HostRecovery) {
    $scenarioFilters.Add('host_shutdown_requires_parked_surface_and_zero_helpers')
}
if ($scenarioFilters.Count -eq 0) {
    # An unqualified Green/All run covers the entire portable surface suite.
    $scenarioFilters.Add('')
}

$forbidden = @(
    'Start-Process',
    'claude.exe',
    'codex.exe',
    'cursor.exe',
    'devmanager.exe'
)
foreach ($token in $forbidden) {
    if ($scriptText.Contains($token)) {
        throw "Surface proof script must not contain '$token'."
    }
}

function Get-BrowserVisibleHostProofClass {
    param(
        [bool]$FixtureOnly,
        [bool]$VisibleClaimed,
        [bool]$OptInMarker,
        [bool]$ObservedHostOwnedWebView2,
        [bool]$ObservedWindowLifecycle,
        [bool]$ObservedHelperLifecycle
    )

    if ($FixtureOnly -or -not $VisibleClaimed) {
        return 'FixtureProtocolOnly'
    }
    if ($OptInMarker -and $ObservedHostOwnedWebView2 -and $ObservedWindowLifecycle -and $ObservedHelperLifecycle) {
        return 'VisibleGreen'
    }
    return 'VisibleHold'
}

$visibleOptIn = [string]$env:DEVMANAGER_BROWSER_WEBVIEW2_E2E -eq '1'
$redChecks = [ordered]@{
    worktreeRoot              = $worktreeRoot
    surfaceTestPresent        = Test-Path -LiteralPath $surfaceTest
    fixtureServerSourcePresent = Test-Path -LiteralPath $serverSource
    scriptAvoidsProcessLaunch = -not $scriptText.Contains('Start-Process')
    serverBindsLoopback       = $serverText.Contains('127.0.0.1')
    serverExposesHealth       = $serverText.Contains('/health')
    portableHostOnly          = $testText.Contains('do not launch WebView2')
    visibleWebView2Claimed    = $false
}

$greenChecks = [ordered]@{
    rustTestCommand = 'cargo test --locked --test browser_surface [optional-filter] -- --test-threads=1 --nocapture'
    executedCargo   = $false
    targetDirectory = $null
    scenarioFilters = @($scenarioFilters)
    note            = 'Green/All execute only the portable browser_surface test suite with a process-unique C:\Temp target. They do not invoke WebView2 or the installed app.'
}

function Invoke-PortableSurfaceScenario {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Filter
    )

    $targetDirectory = Join-Path 'C:\Temp' ('devmanager-browser-surface-{0}' -f $PID)
    New-Item -ItemType Directory -Force -Path $targetDirectory | Out-Null
    $greenChecks.targetDirectory = $targetDirectory

    $hadTarget = Test-Path Env:CARGO_TARGET_DIR
    $previousTarget = $env:CARGO_TARGET_DIR
    $hadProfile = Test-Path Env:DEVMANAGER_PROFILE
    $previousProfile = $env:DEVMANAGER_PROFILE
    try {
        $env:CARGO_TARGET_DIR = $targetDirectory
        # Test profiles are process-unique and must not inherit the installed
        # profile selector from the invoking terminal.
        Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
        Push-Location $worktreeRoot
        try {
            $cargoArgs = @('test', '--locked', '--test', 'browser_surface')
            if (-not [string]::IsNullOrEmpty($Filter)) {
                $cargoArgs += $Filter
            }
            $cargoArgs += @('--', '--test-threads=1', '--nocapture')
            & cargo @cargoArgs
            if ($LASTEXITCODE -ne 0) {
                throw "portable browser surface scenario '$Filter' failed with exit code $LASTEXITCODE"
            }
        }
        finally {
            Pop-Location
        }
        $greenChecks.executedCargo = $true
    }
    finally {
        if ($hadTarget) {
            $env:CARGO_TARGET_DIR = $previousTarget
        }
        else {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        }
        if ($hadProfile) {
            $env:DEVMANAGER_PROFILE = $previousProfile
        }
        else {
            Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $targetDirectory) {
            Remove-Item -LiteralPath $targetDirectory -Recurse -Force
        }
    }
}

$evidence = [ordered]@{
    schemaVersion           = 1
    kind                    = 'browser-surface-proof'
    stage                   = $Stage
    generatedAtUtc          = [DateTime]::UtcNow.ToString('o')
    scenarioFlags           = [ordered]@{
        allDpi      = [bool]$AllDpi
        clientCrash = [bool]$ClientCrash
        hostRecovery = [bool]$HostRecovery
    }
    visibleWebView2Proven   = $false
    visibleHostProofClass   = 'FixtureProtocolOnly'
    productionProfileTouched = $false
    residue                 = [ordered]@{
        launchedInstalledApp = $false
        launchedProvider     = $false
        launchedWebView2     = $false
        leftoverFixturePid   = $null
    }
    red                     = $redChecks
    green                   = $greenChecks
    notProven               = @(
        'Visible host-owned WebView2 child HWND attach/park/reattach',
        'GPUI client crash/rehost with a live controller',
        '100/125/150/200 percent OS DPI with a real surface',
        'Zero WebView2 helper processes after a real context close'
    )
}

if ($Stage -in @('Red', 'All')) {
    if (-not $redChecks.surfaceTestPresent -or -not $redChecks.fixtureServerSourcePresent) {
        throw 'Required surface proof sources are missing.'
    }
}

if ($Stage -in @('Green', 'All')) {
    foreach ($filter in $scenarioFilters) {
        Invoke-PortableSurfaceScenario -Filter $filter
    }
}

$evidence.visibleHostProofClass = Get-BrowserVisibleHostProofClass `
    -FixtureOnly $true `
    -VisibleClaimed ([bool]$evidence.visibleWebView2Proven -or [bool]$redChecks.visibleWebView2Claimed) `
    -OptInMarker $visibleOptIn `
    -ObservedHostOwnedWebView2 $false `
    -ObservedWindowLifecycle $false `
    -ObservedHelperLifecycle $false
if ($evidence.visibleHostProofClass -eq 'VisibleGreen' -or [bool]$evidence.visibleWebView2Proven -or [bool]$redChecks.visibleWebView2Claimed) {
    throw 'Portable surface proof cannot claim visible WebView2 success. Fixture-only runs stay FixtureProtocolOnly.'
}

$evidencePath = Join-Path $resolvedOutput 'browser-surface-proof.json'
$evidence | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $evidencePath -Encoding utf8
Write-Host ("Browser surface proof stage {0} wrote {1}" -f $Stage, $evidencePath)
Write-Host 'Visible WebView2 proof is NOT claimed by this portable run.'
