# Phase 0 guarded phase-gate runner — exact Cargo recipes only.
# Observes admitted process-tree residue and fails closed; does not kill.
# Authoritative Job Object cleanup arrives in Phase 3.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Phase,

    [Parameter(Mandatory = $true)]
    [string]$Recipe,

    [switch]$LongRustRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

$phaseName = Assert-DevManagerPhaseName -Phase $Phase
$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
$protectedRoot = Get-DevManagerProductionRoot

# Reject unknown recipes before any baseline/evidence work.
$plan = Resolve-DevManagerPhaseGateRecipe -Recipe $Recipe -WorktreeRoot $worktreeRoot
Assert-DevManagerPhaseGateExecutionPlan -Plan $plan

$run = New-DevManagerPhaseGateRunDirectory `
    -Phase $phaseName `
    -EvidenceRoot $evidenceRoot `
    -ProtectedProductionRoot $protectedRoot

$runId = [string]$run.runId
$runDirectory = [string]$run.runDirectory
$baselinePath = Join-Path $runDirectory 'baseline.json'
$processesBeforePath = Join-Path $runDirectory 'processes-before.json'
$processesAfterPath = Join-Path $runDirectory 'processes-after.json'
$verificationPath = Join-Path $runDirectory 'verification.json'

foreach ($path in @($baselinePath, $processesBeforePath, $processesAfterPath, $verificationPath)) {
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $path `
        -ProtectedProductionRoot $protectedRoot `
        -AllowedEvidenceRoot $evidenceRoot
}

$captureScript = Join-Path $PSScriptRoot 'Capture-ProductionBaseline.ps1'
$assertScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'

$commandAdmitted = $false
$processStartSucceeded = $false
$exitCode = $null
$durationMs = $null
$cleanupResult = $null
$productionAssert = 'not-run'
$originalGuardFailure = $null
$productionAssertFailure = $null
$verificationWriteFailure = $null
$evidenceWriteFailed = $false
$afterPublished = $false
$beforeInventory = $null
$afterInventory = $null
$rootIdentity = $null
$observationFailure = $null
$stopwatch = [System.Diagnostics.Stopwatch]::new()

$observedByKey = New-Object 'System.Collections.Generic.Dictionary[string, object]'
$trackedPids = New-Object 'System.Collections.Generic.HashSet[uint32]'
$lineageEndExclusiveByPid = New-Object 'System.Collections.Generic.Dictionary[uint32, DateTime]'

function Write-PhaseGateVerification {
    param([Parameter(Mandatory = $true)][string]$Path)

    $verification = [pscustomobject]@{
        schemaVersion         = [int]1
        capturedAtUtc         = [DateTime]::UtcNow.ToString('o')
        phase                 = $phaseName
        recipe                = [string]$plan.recipe
        runId                 = $runId
        runDirectory          = $runDirectory
        command               = [string]$plan.executable
        arguments             = [string[]]$plan.arguments
        workingDirectory      = [string]$plan.workingDirectory
        environment           = [pscustomobject]$plan.environment
        environmentRemovals   = [string[]]@($plan.environmentRemovals)
        exitCode              = $(if ($null -eq $exitCode) { $null } else { [int]$exitCode })
        durationMs            = $durationMs
        processStartSucceeded = [bool]$processStartSucceeded
        longRustRun           = [bool]$LongRustRun
        cleanupResult         = $cleanupResult
        productionAssert      = $productionAssert
        baselinePath          = $baselinePath
        processesBeforePath   = $processesBeforePath
        processesAfterPath    = $processesAfterPath
        rootIdentity          = $rootIdentity
        originalGuardFailure  = $originalGuardFailure
        productionAssertFailure = $productionAssertFailure
    }

    Write-DevManagerJsonEvidence `
        -Value $verification `
        -OutputPath $Path `
        -ProtectedProductionRoot $protectedRoot `
        -AllowedEvidenceRoot $evidenceRoot
}

function Publish-PhaseGateProcessesAfter {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Document
    )

    Write-DevManagerJsonEvidence `
        -Value $Document `
        -OutputPath $processesAfterPath `
        -ProtectedProductionRoot $protectedRoot `
        -AllowedEvidenceRoot $evidenceRoot
    $script:afterInventory = $Document
    $script:afterPublished = $true
}

try {
    try {
        & $captureScript -OutputPath $baselinePath
    }
    catch {
        $evidenceWriteFailed = $true
        throw
    }

    try {
        $beforeInventory = Get-DevManagerProcessInventory -WorktreeRoot $worktreeRoot
        $beforeInventory.processes = [object[]]@($beforeInventory.processes)
        Write-DevManagerJsonEvidence `
            -Value $beforeInventory `
            -OutputPath $processesBeforePath `
            -ProtectedProductionRoot $protectedRoot `
            -AllowedEvidenceRoot $evidenceRoot
    }
    catch {
        $evidenceWriteFailed = $true
        throw
    }

    if ($LongRustRun) {
        $longRustMessage = @"
LongRustRun: about to run recipe '$($plan.recipe)' under phase '$phaseName' runId=$runId.
This may start Cargo, rustc, and test harness processes.
Phase 0 observes residue and fails closed; it does not kill.
Command: $($plan.executable) $($plan.arguments -join ' ')
"@
        Write-Warning $longRustMessage
        Write-Host $longRustMessage
    }

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = [string]$plan.executable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $false
    $startInfo.WorkingDirectory = [string]$plan.workingDirectory
    Set-DevManagerPhaseGateProcessEnvironment -StartInfo $startInfo -Plan $plan
    foreach ($arg in @($plan.arguments)) {
        [void]$startInfo.ArgumentList.Add([string]$arg)
    }

    $commandAdmitted = $true
    $stopwatch.Start()
    $process = [System.Diagnostics.Process]::Start($startInfo)
    if ($null -eq $process) {
        throw "Failed to start process for '$($plan.executable)'."
    }
    $processStartSucceeded = $true

    try {
        $rootIdentity = $null
        for ($i = 0; $i -lt 40; $i++) {
            try {
                $rootIdentity = Get-DevManagerObservedProcessIdentity `
                    -ProcessId ([uint32]$process.Id) `
                    -RequireCompleteIdentity
                break
            }
            catch {
                if ($process.HasExited) {
                    $fallbackPath = [string]$plan.executable
                    try {
                        if ($null -ne $process.MainModule -and -not [string]::IsNullOrWhiteSpace([string]$process.MainModule.FileName)) {
                            $fallbackPath = [string]$process.MainModule.FileName
                        }
                    }
                    catch { }
                    $rootIdentity = [pscustomobject]@{
                        processId       = [uint32]$process.Id
                        executablePath  = (Normalize-DevManagerPath -LiteralPath $fallbackPath)
                        creationDate    = $process.StartTime.ToUniversalTime().ToString('o')
                        parentProcessId = [uint32]0
                    }
                    break
                }
                Start-Sleep -Milliseconds 50
            }
        }
        if ($null -eq $rootIdentity) {
            throw "Unable to capture admitted root process identity for PID=$($process.Id)."
        }
        $rootKey = Get-DevManagerProcessInventoryIdentityKey -Process $rootIdentity
        $attributionFloorUtc = ConvertTo-DevManagerProcessCreationUtc -CreationDate $rootIdentity.creationDate
        $observedByKey[$rootKey] = $rootIdentity
        $null = $trackedPids.Add([uint32]$rootIdentity.processId)

        while (-not $process.HasExited) {
            Update-DevManagerObservedProcessTree `
                -ObservedByKey $observedByKey `
                -TrackedPids $trackedPids `
                -AttributionFloorUtc $attributionFloorUtc `
                -LineageEndExclusiveByPid $lineageEndExclusiveByPid
            $null = $process.WaitForExit(250)
        }
        Update-DevManagerObservedProcessTree `
            -ObservedByKey $observedByKey `
            -TrackedPids $trackedPids `
            -AttributionFloorUtc $attributionFloorUtc `
            -LineageEndExclusiveByPid $lineageEndExclusiveByPid
        $exitCode = [int]$process.ExitCode
    }
    finally {
        $process.Dispose()
    }

    $stopwatch.Stop()
    $durationMs = [int64]$stopwatch.ElapsedMilliseconds

    $settled = Wait-DevManagerPhaseGateQuietWindow `
        -WorktreeRoot $worktreeRoot `
        -ObservedByKey $observedByKey `
        -TrackedPids $trackedPids `
        -AttributionFloorUtc $attributionFloorUtc `
        -LineageEndExclusiveByPid $lineageEndExclusiveByPid `
        -BeforeProcesses $beforeInventory.processes `
        -TimeoutMilliseconds 20000 `
        -PollMilliseconds 250 `
        -QuietMilliseconds 1000

    $afterInventory = [pscustomobject]@{
        schemaVersion = [int]1
        capturedAtUtc = [DateTime]::UtcNow.ToString('o')
        worktreeRoot  = Normalize-DevManagerPath -LiteralPath $worktreeRoot
        runId         = $runId
        processes     = [object[]]@($settled)
    }
    try {
        Publish-PhaseGateProcessesAfter -Document $afterInventory
    }
    catch {
        # A fallback envelope can make the artifact set inspectable, but the
        # original publication failure remains a safety-gate failure.
        $evidenceWriteFailed = $true
        $observationFailure = [string]$_.Exception.Message
        throw
    }

    $cleanupResult = Classify-DevManagerPhaseCleanupResult `
        -BeforeProcesses $beforeInventory.processes `
        -AfterProcesses $afterInventory.processes

    if ($cleanupResult -eq 'residue') {
        throw "Disposable development process residue remains after phase '$phaseName' (observe/fail-closed; Phase 0 does not kill)."
    }
}
catch {
    if ($null -eq $originalGuardFailure) {
        $originalGuardFailure = [string]$_.Exception.Message
    }
    if (-not $commandAdmitted) {
        throw
    }
    if ([string]::IsNullOrWhiteSpace($observationFailure)) {
        $observationFailure = [string]$originalGuardFailure
    }
}
finally {
    if ($commandAdmitted) {
        if ($stopwatch.IsRunning) { $stopwatch.Stop() }
        if ($null -eq $durationMs -and $stopwatch.ElapsedMilliseconds -gt 0) {
            $durationMs = [int64]$stopwatch.ElapsedMilliseconds
        }

        if (-not $afterPublished) {
            try {
                $failureText = $observationFailure
                if ([string]::IsNullOrWhiteSpace($failureText)) {
                    $failureText = $originalGuardFailure
                }
                if ([string]::IsNullOrWhiteSpace($failureText)) {
                    $failureText = 'After-inventory observation unavailable after command admission.'
                }
                $unavailable = New-DevManagerPhaseGateUnavailableAfterInventory `
                    -WorktreeRoot $worktreeRoot `
                    -RunId $runId `
                    -RootIdentity $rootIdentity `
                    -ObservationFailure $failureText
                Publish-PhaseGateProcessesAfter -Document $unavailable
                $cleanupResult = 'residue'
            }
            catch {
                $evidenceWriteFailed = $true
                if ($null -eq $cleanupResult) {
                    $cleanupResult = 'residue'
                }
                if ($null -eq $originalGuardFailure) {
                    $originalGuardFailure = [string]$_.Exception.Message
                }
            }
        }

        try {
            & $assertScript -BaselinePath $baselinePath
            if ($productionAssert -ne 'failed') {
                $productionAssert = 'unchanged'
            }
        }
        catch {
            $productionAssert = 'failed'
            $productionAssertFailure = [string]$_.Exception.Message
        }

        if ($null -eq $cleanupResult) {
            if ($null -ne $afterInventory -and $null -ne $afterInventory.PSObject.Properties['status'] -and [string]$afterInventory.status -eq 'unavailable') {
                $cleanupResult = 'residue'
            }
            elseif ($null -ne $afterInventory) {
                $cleanupResult = Classify-DevManagerPhaseCleanupResult `
                    -BeforeProcesses $(if ($null -ne $beforeInventory) { $beforeInventory.processes } else { @() }) `
                    -AfterProcesses $afterInventory.processes
            }
            else {
                $cleanupResult = 'residue'
            }
        }

        try {
            Write-PhaseGateVerification -Path $verificationPath
            Write-Host ("Wrote verification.json -> {0}" -f $verificationPath)
            Write-Host ("runId={0}; recipe={1}; cleanupResult={2}; exitCode={3}; durationMs={4}" -f `
                    $runId, $plan.recipe, $cleanupResult, $exitCode, $durationMs)
        }
        catch {
            $verificationWriteFailure = [string]$_.Exception.Message
            Write-Host ("Failed to write verification.json: {0}" -f $verificationWriteFailure)
        }
    }
}

$final = Get-DevManagerPhaseGateFinalExitCode `
    -ChildExitCode $exitCode `
    -ProductionAssertFailed:($productionAssert -eq 'failed') `
    -VerificationWriteFailed:(-not [string]::IsNullOrWhiteSpace($verificationWriteFailure)) `
    -EvidenceWriteFailed:$evidenceWriteFailed `
    -OriginalGuardFailure $originalGuardFailure

if ($final -ne 0) {
    if ($productionAssert -eq 'failed') {
        Write-Host ("Phase gate production/evidence guard failed: {0}" -f $productionAssertFailure)
    }
    elseif (-not [string]::IsNullOrWhiteSpace($verificationWriteFailure)) {
        Write-Host ("Phase gate verification publication failed: {0}" -f $verificationWriteFailure)
    }
    elseif ($evidenceWriteFailed) {
        Write-Host ("Phase gate evidence publication failed: {0}" -f $originalGuardFailure)
    }
    elseif (-not [string]::IsNullOrWhiteSpace($originalGuardFailure)) {
        Write-Host ("Phase gate failed: {0}" -f $originalGuardFailure)
    }
}

exit $final
