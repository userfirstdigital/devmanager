# Phase 3 process-supervisor gate entrypoint.
# The list preflight is behavioral: a recipe that selects zero tests is never green.
# The final union is dependency-gated: unavailable host/client inputs produce an explicit HOLD.

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
    $helper = Join-Path $WorktreeRoot 'target-native-next\debug\devmanager-process-test-helper.exe'
    if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
        throw 'typed-unavailable: fixed Rust process supervisor helper is absent.'
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $helper
    return [System.IO.Path]::GetFullPath($candidate.FullName)
}

function Invoke-ProcessSupervisorHarness {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [Parameter(Mandatory = $true)]
        [int]$TimeoutMilliseconds
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
    foreach ($argument in $Arguments) {
        [void]$listInfo.ArgumentList.Add($argument)
    }

    $listResult = Invoke-DevManagerPhaseGateBoundedCommand -StartInfo $listInfo -TimeoutMilliseconds $TimeoutMilliseconds -StdoutBytes 262144 -StderrBytes 65536
    return $listResult
}

function Invoke-ProcessSupervisorTestList {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $listResult = Invoke-ProcessSupervisorHarness -WorktreeRoot $WorktreeRoot -Arguments @('--list') -TimeoutMilliseconds 120000
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

function Invoke-ProcessSupervisorTestSuite {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $result = Invoke-ProcessSupervisorHarness `
        -WorktreeRoot $WorktreeRoot `
        -Arguments @('--test-threads=1', '--nocapture') `
        -TimeoutMilliseconds 600000
    if ($result.ExitCode -ne 0 -or $result.StderrBytes -ne 0) {
        throw ("process-soak focused suite failed ({0}): {1}" -f $result.ExitCode, $result.Stderr.Trim())
    }
    $match = [regex]::Match($result.Stdout, 'test result:\s+ok\.\s+(\d+) passed;\s+0 failed')
    if (-not $match.Success) {
        throw 'process-soak focused suite did not publish an all-green test result.'
    }
    $passed = [int]$match.Groups[1].Value
    if ($passed -lt 32) { throw "process-soak focused suite executed only $passed tests; expected at least 32." }
    return $passed
}

function Invoke-Phase3FinalUnion {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $pwshCommands = @(
        Get-Command -Name 'pwsh' -All -CommandType Application -ErrorAction SilentlyContinue |
            Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_.Source) }
    )
    if ($pwshCommands.Count -ne 1) { throw "final union requires exactly one pwsh.exe (found $($pwshCommands.Count))." }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = [IO.Path]::GetFullPath([string]$pwshCommands[0].Source)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = $WorktreeRoot
    $info.Environment.Clear()
    $systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) { throw 'final union cannot establish SystemRoot.' }
    $info.Environment['SystemRoot'] = $systemRoot
    $temp = Join-Path $WorktreeRoot '.tmp-phase3-soak'
    $info.Environment['TEMP'] = $temp
    $info.Environment['TMP'] = $temp
    $info.Environment['PATH'] = @((Join-Path $systemRoot 'System32'), (Split-Path -Parent $info.FileName)) -join ';'
    foreach ($argument in @('-NoProfile', '-NonInteractive', '-File', $soakScript, '-Iterations', '100', '-Seed', [string]$Seed)) {
        [void]$info.ArgumentList.Add([string]$argument)
    }
    $result = Invoke-DevManagerPhaseGateBoundedCommand -StartInfo $info -TimeoutMilliseconds 600000 -StdoutBytes 65536 -StderrBytes 16384
    $lines = @($result.Stdout -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($result.ExitCode -eq 78 -and $lines.Count -eq 1) {
        $document = $lines[0] | ConvertFrom-Json
        if ([string]$document.status -eq 'hold') {
            Write-Host ("{0} HOLD: {1}" -f $phase, [string]$document.error)
            return 78
        }
    }
    if ($result.ExitCode -ne 0 -or $result.StderrBytes -ne 0 -or $lines.Count -ne 1) {
        throw "final host/client union failed closed (exit=$($result.ExitCode) stderrBytes=$($result.StderrBytes) lines=$($lines.Count))."
    }
    $document = $lines[0] | ConvertFrom-Json
    if ([string]$document.status -ne 'passed') { throw 'final host/client union did not report passed.' }
    return 0
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
    $focusedTestCount = Invoke-ProcessSupervisorTestSuite -WorktreeRoot $worktreeRoot
    Write-Output ("{0} focused-tests={1}" -f $phase, $focusedTestCount)
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
    if ([int]$exitCode -eq 0) {
        $exitCode = Invoke-Phase3FinalUnion -WorktreeRoot $worktreeRoot
    }
}
finally {
    [Environment]::SetEnvironmentVariable($iterationBridge, $priorIterationBridge, 'Process')
    [Environment]::SetEnvironmentVariable($seedBridge, $priorSeedBridge, 'Process')
}
exit ([int]$exitCode)
