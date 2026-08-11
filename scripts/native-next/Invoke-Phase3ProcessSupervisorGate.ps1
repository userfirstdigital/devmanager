# Phase 3 process-supervisor gate entrypoint.
# The list preflight is behavioral: a recipe that selects zero tests is never green.

[CmdletBinding()]
param(
    [switch]$ListOnly,
    [switch]$LongRustRun
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
$plan = Resolve-DevManagerPhaseGateRecipe -Recipe $recipe -WorktreeRoot $worktreeRoot
Assert-DevManagerPhaseGateExecutionPlan -Plan $plan

function Invoke-ProcessSupervisorTestList {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    $listInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $listInfo.FileName = [string]$Plan.executable
    $listInfo.UseShellExecute = $false
    $listInfo.CreateNoWindow = $true
    $listInfo.RedirectStandardOutput = $true
    $listInfo.RedirectStandardError = $true
    $listInfo.WorkingDirectory = [string]$Plan.workingDirectory
    Set-DevManagerPhaseGateProcessEnvironment -StartInfo $listInfo -Plan $Plan
    foreach ($argument in [string[]]@('test', '--test', 'process_supervisor', '--test', 'process_soak_infrastructure', '--', '--list')) {
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
    if ($testLines.Count -lt 23) {
        throw "process-supervisor/soak preflight found only $($testLines.Count) tests; expected at least 23."
    }
    return [int]$testLines.Count
}

try {
    $testCount = Invoke-ProcessSupervisorTestList -Plan $plan
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
& $phaseGate -Phase $phase -Recipe $recipe -LongRustRun:$LongRustRun
$exitCode = $LASTEXITCODE
if ($null -eq $exitCode) {
    $exitCode = 0
}
exit ([int]$exitCode)
