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
    foreach ($argument in [string[]]@('test', '--test', 'process_supervisor', '--', '--list')) {
        [void]$listInfo.ArgumentList.Add($argument)
    }

    $listProcess = [System.Diagnostics.Process]::Start($listInfo)
    if ($null -eq $listProcess) {
        throw 'Unable to start the process-supervisor test-list preflight.'
    }
    try {
        $stdoutTask = $listProcess.StandardOutput.ReadToEndAsync()
        $stderrTask = $listProcess.StandardError.ReadToEndAsync()
        $listProcess.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        if ($listProcess.ExitCode -ne 0) {
            throw ("process-supervisor test-list preflight failed ({0}): {1}" -f $listProcess.ExitCode, $stderr.Trim())
        }
        $testLines = @(
            $stdout -split "`r?`n" |
                Where-Object { $_ -match ':\s*test$' }
        )
        if ($testLines.Count -eq 0) {
            throw 'process-supervisor preflight found zero tests; refusing a green gate.'
        }
        return [int]$testLines.Count
    }
    finally {
        $listProcess.Dispose()
    }
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
& $phaseGate -Phase $phase -Recipe $recipe -LongRustRun:$LongRustRun
$exitCode = $LASTEXITCODE
if ($null -eq $exitCode) {
    $exitCode = 0
}
exit ([int]$exitCode)
