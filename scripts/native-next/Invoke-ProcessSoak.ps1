# Phase 3.10 guarded process/terminal soak runner.
#
# This runner owns the deterministic scenario loop and the safety evidence
# boundary. The real host/client cycle is deliberately injected later through
# Invoke-DevManagerProcessSoakCycle; absence of that API is unavailable, never
# a passing no-op. Invoke with pwsh -NoProfile -NonInteractive.

[CmdletBinding()]
param(
    [ValidateRange(1, 1000)]
    [int]$Iterations = 100,

    [int]$Seed = 3403,

    [string]$CycleApiScript
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

function Write-SoakStatus {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value
    )

    Write-Output ($Value | ConvertTo-Json -Depth 12 -Compress)
}

function Exit-SoakUnavailable {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Reason
    )

    Write-Output ("UNAVAILABLE: {0}" -f $Reason)
    Write-SoakStatus ([pscustomobject][ordered]@{
            schemaVersion = [int]1
            status        = 'unavailable'
            phase         = 'phase-03-process-soak'
            iterations    = [int]$Iterations
            seed          = [int]$Seed
            reason        = $Reason
        })
    exit 78
}

function Resolve-SoakApiPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    $candidate = $Path.Trim()
    if ([string]::IsNullOrWhiteSpace($candidate)) {
        throw 'CycleApiScript is empty.'
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $candidate)) {
        $candidate = Join-Path $WorktreeRoot $candidate
    }
    $resolved = [System.IO.Path]::GetFullPath($candidate)
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $resolved -AncestorPath $WorktreeRoot)) {
        throw "CycleApiScript escapes the worktree ('$resolved')."
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $resolved
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "CycleApiScript does not exist ('$resolved')."
    }
    return $resolved
}

function Get-SoakProcessKey {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Process
    )

    return Get-DevManagerProcessInventoryIdentityKey -Process $Process
}

function Get-SoakNewProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [object[]]$Before,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [object[]]$After
    )

    $beforeKeys = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($process in @($Before)) {
        $null = $beforeKeys.Add((Get-SoakProcessKey -Process $process))
    }
    $unique = New-Object 'System.Collections.Generic.Dictionary[string, object]'
    foreach ($process in @($After)) {
        $key = Get-SoakProcessKey -Process $process
        if (-not $beforeKeys.Contains($key)) {
            $unique[$key] = $process
        }
    }
    return @($unique.Values | Sort-Object processId, executablePath, creationDate)
}

function Wait-SoakProcessQuiet {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [object[]]$Before,
        [int]$TimeoutMilliseconds = 15000,
        [int]$PollMilliseconds = 250
    )

    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    do {
        $inventory = Get-DevManagerProcessInventory -WorktreeRoot $WorktreeRoot
        $newProcesses = Get-SoakNewProcesses -Before $Before -After @($inventory.processes)
        if (@($newProcesses).Count -eq 0) {
            return [pscustomobject]@{
                inventory    = $inventory
                orphaned     = [object[]]@()
                timedOut     = $false
            }
        }
        Start-Sleep -Milliseconds $PollMilliseconds
    } while ([DateTime]::UtcNow -lt $deadline)

    $inventory = Get-DevManagerProcessInventory -WorktreeRoot $WorktreeRoot
    return [pscustomobject]@{
        inventory = $inventory
        orphaned  = [object[]]@(Get-SoakNewProcesses -Before $Before -After @($inventory.processes))
        timedOut  = $true
    }
}

function Convert-SoakProcessEvidence {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [object[]]$Processes
    )

    return @(
        foreach ($process in @($Processes)) {
            [pscustomobject][ordered]@{
                processId      = [uint32]$process.processId
                parentProcessId = [uint32]$process.parentProcessId
                identity       = (Get-SoakProcessKey -Process $process)
                executable     = [System.IO.Path]::GetFileName([string]$process.executablePath)
            }
        }
    )
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot

if (-not ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)) {
    Exit-SoakUnavailable -Reason 'Phase 3.10 process soak requires the Windows host/client surface.'
}

if (-not [string]::IsNullOrWhiteSpace($CycleApiScript)) {
    try {
        $apiPath = Resolve-SoakApiPath -Path $CycleApiScript -WorktreeRoot $worktreeRoot
        . $apiPath
    }
    catch {
        Exit-SoakUnavailable -Reason ("cycle API script could not be loaded: {0}" -f $_.Exception.Message)
    }
}

$cycleApi = Get-Command -Name 'Invoke-DevManagerProcessSoakCycle' -CommandType Function -ErrorAction SilentlyContinue
if ($null -eq $cycleApi) {
    Exit-SoakUnavailable -Reason 'Invoke-DevManagerProcessSoakCycle is not present; no real host/client cycle was run.'
}

$protectedRoot = Get-DevManagerProductionRoot
$runId = [guid]::NewGuid().ToString('N')
$runDirectory = [System.IO.Path]::GetFullPath((Join-Path $evidenceRoot "phase-03-process-soak\runs\$runId"))
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $runDirectory `
    -ProtectedProductionRoot $protectedRoot `
    -AllowedEvidenceRoot $evidenceRoot
New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory

$baselinePath = Join-Path $runDirectory 'baseline.json'
$summaryPath = Join-Path $runDirectory 'summary.json'
$captureScript = Join-Path $PSScriptRoot 'Capture-ProductionBaseline.ps1'
$assertScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'
$beforeInventory = $null
$afterInventory = $null
$orphaned = [object[]]@()
$cycleResults = New-Object System.Collections.Generic.List[object]
$failure = $null
$productionAssert = 'not-run'
$status = 'failed'

try {
    & $captureScript -OutputPath $baselinePath
    $beforeInventory = Get-DevManagerProcessInventory -WorktreeRoot $worktreeRoot

    $random = [Random]::new($Seed)
    for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
        $cycleSeed = $random.Next()
        $scenario = [pscustomobject][ordered]@{
            iteration         = [int]$iteration
            seed              = [int]$cycleSeed
            disconnectDelayMs = [int]$random.Next(0, 250)
            closeDelayMs      = [int]$random.Next(0, 250)
            resizeRows        = [int]$random.Next(20, 60)
            resizeColumns     = [int]$random.Next(80, 180)
        }
        $cycleResult = @(
            & $cycleApi.Name `
                -Iteration $iteration `
                -Seed $cycleSeed `
                -Scenario $scenario `
                -WorktreeRoot $worktreeRoot
        )
        if ($cycleResult.Count -ne 1) {
            throw "soak cycle $iteration returned $($cycleResult.Count) result objects; exactly one completed result is required."
        }
        $resultStatus = $cycleResult[0].PSObject.Properties['status']
        if ($null -eq $resultStatus -or [string]$resultStatus.Value -cne 'completed') {
            throw "soak cycle $iteration did not settle as completed."
        }
        $cycleResults.Add([pscustomobject][ordered]@{
                iteration = [int]$iteration
                seed      = [int]$cycleSeed
                scenario  = $scenario
                status    = 'completed'
            })
    }

    $quiet = Wait-SoakProcessQuiet `
        -WorktreeRoot $worktreeRoot `
        -Before @($beforeInventory.processes)
    $afterInventory = $quiet.inventory
    $orphaned = [object[]]$quiet.orphaned
    if ($quiet.timedOut -or @($orphaned).Count -ne 0) {
        throw ("orphan process residue remains after soak: {0}" -f (@(Convert-SoakProcessEvidence -Processes $orphaned) | ConvertTo-Json -Depth 8 -Compress))
    }

    $status = 'passed'
}
catch {
    $failure = [string]$_.Exception.Message
}
finally {
    if (Test-Path -LiteralPath $baselinePath -PathType Leaf) {
        try {
            & $assertScript -BaselinePath $baselinePath
            $productionAssert = 'unchanged'
        }
        catch {
            $productionAssert = 'failed'
            if ([string]::IsNullOrWhiteSpace($failure)) {
                $failure = "production baseline changed or could not be verified: $($_.Exception.Message)"
            }
        }
    }

    if ($productionAssert -ne 'unchanged' -and [string]::IsNullOrWhiteSpace($failure)) {
        $failure = 'production baseline was not verified.'
    }
    if ($status -eq 'passed' -and -not [string]::IsNullOrWhiteSpace($failure)) {
        $status = 'failed'
    }

    $summary = [pscustomobject][ordered]@{
        schemaVersion    = [int]1
        capturedAtUtc    = [DateTime]::UtcNow.ToString('o')
        status           = $status
        phase            = 'phase-03-process-soak'
        runId            = $runId
        runDirectory     = $runDirectory
        iterations       = [int]$Iterations
        seed             = [int]$Seed
        completedCycles  = @($cycleResults).Count
        productionAssert = $productionAssert
        beforeProcessCount = if ($null -eq $beforeInventory) { $null } else { @($beforeInventory.processes).Count }
        afterProcessCount  = if ($null -eq $afterInventory) { $null } else { @($afterInventory.processes).Count }
        orphanedProcesses = @(Convert-SoakProcessEvidence -Processes $orphaned)
        failure           = $failure
    }
    Write-DevManagerJsonEvidence `
        -Value $summary `
        -OutputPath $summaryPath `
        -ProtectedProductionRoot $protectedRoot `
        -AllowedEvidenceRoot $evidenceRoot
    Write-SoakStatus $summary
}

if ($status -ne 'passed') {
    Write-Error ("Phase 3.10 process soak failed closed: {0}" -f $failure)
    exit 1
}

exit 0
