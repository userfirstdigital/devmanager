# Phase 3 process-supervisor gate entrypoint.
# The list preflight is behavioral: a recipe that selects zero tests is never green.

[CmdletBinding()]
param(
    [switch]$ListOnly,
    [switch]$LongRustRun,
    [ValidateRange(1, 100)]
    [int]$Iterations = 2,
    [ValidateRange(0, [int]::MaxValue)]
    [int]$Seed = 3403
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

$phase = 'phase-03-process-supervisor'
$recipe = 'phase-03-process-supervisor'
$soakScript = Join-Path $PSScriptRoot 'Invoke-ProcessSoak.ps1'
$captureBaselineScript = Join-Path $PSScriptRoot 'Capture-ProductionBaseline.ps1'
$assertUnchangedScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'
$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$plan = $null

function Resolve-ProcessSoakHarness {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $deps = Join-Path $WorktreeRoot 'target-native-next\debug\deps'
    if (-not (Test-Path -LiteralPath $deps -PathType Container)) {
        throw "typed-unavailable: prebuilt process soak harness directory is absent: $deps"
    }
    $candidates = @(
        Get-ChildItem -LiteralPath $deps -Filter 'process_soak_infrastructure-*.exe' -File |
            Where-Object { $_.Name -notmatch '\.d\.exe$' }
    )
    if ($candidates.Count -eq 0) {
        throw 'typed-unavailable: no prebuilt process soak harness is available.'
    }
    $candidate = $candidates | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $candidate.FullName
    return [System.IO.Path]::GetFullPath($candidate.FullName)
}

function Invoke-ProcessSupervisorTestList {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    $listInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $listInfo.FileName = Resolve-ProcessSoakHarness -WorktreeRoot $WorktreeRoot
    $listInfo.UseShellExecute = $false
    $listInfo.CreateNoWindow = $true
    $listInfo.RedirectStandardOutput = $true
    $listInfo.RedirectStandardError = $true
    $listInfo.WorkingDirectory = $WorktreeRoot
    # ListOnly is a prebuilt-harness query and must remain usable when Cargo
    # is ambiguous or unavailable. It still starts from an empty, explicit
    # environment rather than inheriting caller state.
    $listInfo.Environment.Clear()
    $systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) {
        throw 'SystemRoot is unavailable for process-supervisor list preflight.'
    }
    $debugDirectory = Join-Path $WorktreeRoot 'target-native-next\debug'
    $listInfo.Environment['SystemRoot'] = $systemRoot
    $listInfo.Environment['TEMP'] = Join-Path $WorktreeRoot '.tmp-phase3-soak'
    $listInfo.Environment['TMP'] = Join-Path $WorktreeRoot '.tmp-phase3-soak'
    $listInfo.Environment['PATH'] = @(
        (Join-Path $systemRoot 'System32'),
        $debugDirectory,
        (Join-Path $debugDirectory 'deps')
    ) -join ';'
    foreach ($argument in [string[]]@('--list')) {
        [void]$listInfo.ArgumentList.Add($argument)
    }

    $listResult = Invoke-DevManagerPhaseGateBoundedCommand -StartInfo $listInfo -TimeoutMilliseconds 120000
    if ($listResult.ExitCode -ne 0) {
        throw ("process-supervisor test-list preflight failed ({0}): {1}" -f $listResult.ExitCode, $listResult.Stderr.Trim())
    }
    $testLines = @(
        $listResult.Stdout -split "`r?`n" |
            Where-Object { $_ -match ':\s*test$' }
    )
    if ($testLines.Count -lt 29) {
        throw "process-soak preflight found only $($testLines.Count) tests; expected at least 29."
    }
    return [int]$testLines.Count
}

try {
    if (-not $ListOnly) {
        $plan = Resolve-DevManagerPhaseGateRecipe -Recipe $recipe -WorktreeRoot $worktreeRoot
        Assert-DevManagerPhaseGateExecutionPlan -Plan $plan
    }
    $testCount = Invoke-ProcessSupervisorTestList -WorktreeRoot $worktreeRoot
    if ($ListOnly) {
        Write-Output ("{0} tests={1}" -f $phase, $testCount)
        exit 0
    }
}
catch {
    Write-Error -Message ("{0} unavailable/failed closed: {1}" -f $phase, $_.Exception.Message) -ErrorAction Continue
    exit 1
}

$phaseGate = Join-Path $PSScriptRoot 'Invoke-PhaseGate.ps1'
# Invoke-PhaseGate owns the baseline/identity/quiet-window guard and invokes
# Invoke-ProcessSoak.ps1 with two cycles before publishing its after inventory.
# All preflight output uses bounded WaitForExit(milliseconds), never an
# unbounded parameterless wait.
# These explicit paths are kept here so the end-to-end gate cannot silently
# regress to a process-supervisor-only recipe.
$null = $soakScript
$null = $captureBaselineScript
$null = $assertUnchangedScript
$iterationBridge = 'DEVMANAGER_PHASE3_SOAK_ITERATIONS'
$seedBridge = 'DEVMANAGER_PHASE3_SOAK_SEED'
$priorIterationBridge = [Environment]::GetEnvironmentVariable($iterationBridge, 'Process')
$priorSeedBridge = [Environment]::GetEnvironmentVariable($seedBridge, 'Process')
$exitCode = 1
try {
    [Environment]::SetEnvironmentVariable($iterationBridge, [string]$Iterations, 'Process')
    [Environment]::SetEnvironmentVariable($seedBridge, [string]$Seed, 'Process')
    & $phaseGate -Phase $phase -Recipe $recipe -LongRustRun:$LongRustRun
    $exitCode = $LASTEXITCODE
    if ($null -eq $exitCode) {
        $exitCode = 0
    }
}
finally {
    [Environment]::SetEnvironmentVariable($iterationBridge, $priorIterationBridge, 'Process')
    [Environment]::SetEnvironmentVariable($seedBridge, $priorSeedBridge, 'Process')
}
exit ([int]$exitCode)
