# Phase 0 phase-gate helpers (recipe admission + observe/fail-closed residue).
# No kill authority. Malicious same-user junction races on evidence dirs are outside
# the Phase 0 accidental-isolation threat model; component/reparse checks still run
# before creation and publication.
# Requires Isolation.ps1 to be dot-sourced first.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-DevManagerPhaseGateRecipeTable {
    return [ordered]@{
        'cargo-version'                 = [string[]]@('--version')
        'cargo-fmt-check'               = [string[]]@('fmt', '--all', '--', '--check')
        'development-isolation-tests'   = [string[]]@(
            'test',
            '--test', 'development_isolation',
            '--', '--test-threads=1'
        )
        'library-tests-serial'          = [string[]]@('test', '--lib', '--', '--test-threads=1')
        'phase-02-host-lock'            = [string[]]@(
            'test',
            '--test', 'host_lock',
            '--', '--nocapture'
        )
        'phase-02-cli-client'           = [string[]]@(
            'test',
            '--test', 'cli_client',
            '--', '--nocapture'
        )
        'phase-02-host-lifecycle'       = [string[]]@(
            'test',
            '--test', 'host_lifecycle',
            '--', '--nocapture'
        )
        'phase-02-host-recovery'        = [string[]]@(
            'test',
            '--test', 'host_recovery',
            '--', '--nocapture'
        )
        'phase-02-diagnostics'          = [string[]]@(
            'test',
            '--test', 'diagnostic_logging',
            '--', '--nocapture'
        )
    }
}

function Assert-DevManagerPhaseName {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Phase
    )

    if ([string]::IsNullOrWhiteSpace($Phase)) {
        throw "Phase name is empty."
    }
    $trimmed = $Phase.Trim()
    if ($trimmed -ne $Phase) {
        throw "Phase name must not have leading or trailing whitespace ('$Phase')."
    }
    if ($trimmed.Length -gt 64) {
        throw "Phase name exceeds 64 characters."
    }
    if ($trimmed -eq '.' -or $trimmed -eq '..') {
        throw "Phase name rejects path traversal segments ('$trimmed')."
    }
    if ($trimmed -match '[\\/:\*\?"<>\|\s]') {
        throw "Phase name must be a single path-safe segment without separators or whitespace ('$trimmed')."
    }
    if ($trimmed -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {
        throw "Phase name must start with alphanumeric and use only [A-Za-z0-9._-] ('$trimmed')."
    }
    return $trimmed
}

function Resolve-DevManagerPhaseGateRecipe {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Recipe,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    if ([string]::IsNullOrWhiteSpace($Recipe)) {
        throw "Recipe is empty."
    }
    $name = $Recipe.Trim()
    $table = Get-DevManagerPhaseGateRecipeTable
    if (-not $table.Contains($name)) {
        throw "Unknown phase-gate recipe '$name'. Accepted: $((@($table.Keys) -join ', '))."
    }

    if (-not (Test-DevManagerAbsolutePath -LiteralPath $WorktreeRoot)) {
        throw "WorktreeRoot must be fully qualified ('$WorktreeRoot')."
    }
    $worktree = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktree

    $cargoTargetDir = [System.IO.Path]::GetFullPath((Join-Path $worktree 'target-native-next'))
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargoTargetDir
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $cargoTargetDir -AncestorPath $worktree)) {
        throw "CARGO_TARGET_DIR escapes worktree."
    }

    $cargoCmd = @(Get-Command -Name 'cargo' -All -CommandType Application -ErrorAction SilentlyContinue)
    if ($cargoCmd.Count -eq 0) {
        throw "Unable to resolve PATH cargo.exe for recipe admission."
    }
    $cargoExes = @(
        $cargoCmd |
            Where-Object { [System.IO.Path]::GetFileName([string]$_.Source) -ieq 'cargo.exe' } |
            ForEach-Object { [System.IO.Path]::GetFullPath([string]$_.Source) } |
            ForEach-Object { Normalize-DevManagerPath -LiteralPath $_ } |
            Select-Object -Unique
    )
    if ($cargoExes.Count -eq 0) {
        throw "Unable to resolve PATH cargo.exe for recipe admission."
    }
    if ($cargoExes.Count -ne 1) {
        throw "Ambiguous PATH cargo.exe resolution ($($cargoExes.Count) matches): $($cargoExes -join '; ')"
    }
    $resolved = $cargoExes[0]
    $leaf = [System.IO.Path]::GetFileName($resolved)
    if ($leaf -ine 'cargo.exe') {
        throw "Phase 0 recipes require PATH cargo.exe (got '$resolved')."
    }

    foreach ($install in @(Get-DevManagerSupportedInstallPaths)) {
        if ([string]::IsNullOrWhiteSpace([string]$install)) { continue }
        if ((Normalize-DevManagerPath -LiteralPath $resolved) -eq (Normalize-DevManagerPath -LiteralPath ([string]$install))) {
            throw "Rejecting installed DevManager path masquerading as cargo ('$resolved')."
        }
    }

    $arguments = [string[]]@($table[$name])
    $environment = [ordered]@{
        CARGO_TARGET_DIR = $cargoTargetDir
    }
    $environmentRemovals = [string[]]@()
    if ($name -eq 'library-tests-serial') {
        # The complete lib suite owns its test identity and profile environment.
        $environmentRemovals = [string[]]@(
            'DEVMANAGER_PROFILE',
            'DEVMANAGER_INSTANCE_LABEL',
            'DEVMANAGER_RUNTIME_KIND'
        )
    }
    else {
        $environment['DEVMANAGER_INSTANCE_LABEL'] = 'Next'
        $environment['DEVMANAGER_RUNTIME_KIND'] = 'native-next'
        $environment['DEVMANAGER_PROFILE'] = 'native-next-dev'
    }

    return [pscustomobject]@{
        recipe               = $name
        executable           = $resolved
        arguments            = $arguments
        workingDirectory     = [System.IO.Path]::GetFullPath($worktree)
        cargoTargetDir       = $cargoTargetDir
        environment          = $environment
        environmentRemovals  = $environmentRemovals
    }
}

function Assert-DevManagerPhaseGateExecutionPlan {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    if ($null -eq $Plan) {
        throw 'Phase-gate execution plan is null.'
    }
    if ([string]::IsNullOrWhiteSpace([string]$Plan.recipe)) {
        throw 'Phase-gate execution plan is missing recipe.'
    }
    if ($null -eq $Plan.environment) {
        throw 'Phase-gate execution plan is missing environment overrides.'
    }
    if ($null -eq $Plan.environmentRemovals) {
        throw 'Phase-gate execution plan is missing environmentRemovals.'
    }

    if (-not $Plan.environment.Contains('CARGO_TARGET_DIR')) {
        throw "Execution plan missing required environment key 'CARGO_TARGET_DIR'."
    }

    $removals = [string[]]@($Plan.environmentRemovals)
    if ([string]$Plan.recipe -eq 'library-tests-serial') {
        foreach ($removed in @('DEVMANAGER_PROFILE', 'DEVMANAGER_INSTANCE_LABEL', 'DEVMANAGER_RUNTIME_KIND')) {
            if ($Plan.environment.Contains($removed)) {
                throw "library-tests-serial must not include a $removed override."
            }
        }
        if (@($Plan.environment.Keys).Count -ne 1) {
            throw "library-tests-serial must override only CARGO_TARGET_DIR."
        }
        if (($removals -join ',') -cne 'DEVMANAGER_PROFILE,DEVMANAGER_INSTANCE_LABEL,DEVMANAGER_RUNTIME_KIND') {
            throw "library-tests-serial environmentRemovals must clear all DevManager runtime identity (got: $($removals -join ', '))."
        }
    }
    else {
        foreach ($requiredEnv in @('DEVMANAGER_INSTANCE_LABEL', 'DEVMANAGER_RUNTIME_KIND', 'DEVMANAGER_PROFILE')) {
            if (-not $Plan.environment.Contains($requiredEnv)) {
                throw "Execution plan missing required environment key '$requiredEnv'."
            }
        }
        if (@($Plan.environment.Keys).Count -ne 4) {
            throw "Non-library Phase 0 recipes must declare exactly four environment overrides."
        }
        if ([string]$Plan.environment['DEVMANAGER_INSTANCE_LABEL'] -ne 'Next') {
            throw "Execution plan must force DEVMANAGER_INSTANCE_LABEL=Next."
        }
        if ([string]$Plan.environment['DEVMANAGER_RUNTIME_KIND'] -ne 'native-next') {
            throw "Execution plan must force DEVMANAGER_RUNTIME_KIND=native-next."
        }
        if ([string]$Plan.environment['DEVMANAGER_PROFILE'] -ne 'native-next-dev') {
            throw "Execution plan must force DEVMANAGER_PROFILE=native-next-dev."
        }
        if ($removals -contains 'DEVMANAGER_PROFILE') {
            throw "Non-library Phase 0 recipes must not remove DEVMANAGER_PROFILE."
        }
        if ($removals.Count -ne 0) {
            throw "Non-library Phase 0 recipes must not declare environmentRemovals (got: $($removals -join ', '))."
        }
    }
}

function Set-DevManagerPhaseGateProcessEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.ProcessStartInfo]$StartInfo,
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    Assert-DevManagerPhaseGateExecutionPlan -Plan $Plan

    foreach ($key in @($Plan.environmentRemovals)) {
        $name = [string]$key
        if ([string]::IsNullOrWhiteSpace($name)) { continue }
        if ($StartInfo.Environment.ContainsKey($name)) {
            [void]$StartInfo.Environment.Remove($name)
        }
    }

    foreach ($key in @($Plan.environment.Keys)) {
        $StartInfo.Environment[[string]$key] = [string]$Plan.environment[$key]
    }
}

function New-DevManagerPhaseGateRunDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Phase,
        [Parameter(Mandatory = $true)]
        [string]$EvidenceRoot,
        [Parameter(Mandatory = $true)]
        [string]$ProtectedProductionRoot
    )

    $phaseName = Assert-DevManagerPhaseName -Phase $Phase
    $runsRoot = [System.IO.Path]::GetFullPath((Join-Path $EvidenceRoot "$phaseName\runs"))
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runsRoot `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot

    $runId = [guid]::NewGuid().ToString('N')
    $runDirectory = [System.IO.Path]::GetFullPath((Join-Path $runsRoot $runId))
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runDirectory `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot

    New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runDirectory `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory

    return [pscustomobject]@{
        phase        = $phaseName
        runId        = $runId
        runDirectory = $runDirectory
    }
}

function Test-DevManagerWorktreeTargetExecutable {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$ExecutablePath,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    if ([string]::IsNullOrWhiteSpace($ExecutablePath)) { return $false }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $ExecutablePath)) { return $false }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $ExecutablePath -AncestorPath $WorktreeRoot)) {
        return $false
    }
    $leaf = [System.IO.Path]::GetFileName($ExecutablePath)
    if ($leaf -imatch '^(cargo|rustc|rustdoc|clippy-driver)(\.exe)?$') { return $true }
    foreach ($part in (Get-DevManagerNormalizedPathComponents -LiteralPath $ExecutablePath)) {
        if ($part -like 'target*') { return $true }
    }
    return $false
}

function Get-DevManagerProcessInventoryEntry {
    param(
        [Parameter(Mandatory = $true)]
        [object]$CimProcess,
        [switch]$RequireCompleteIdentity
    )

    $rawPath = $null
    if ($null -ne $CimProcess.PSObject.Properties['ExecutablePath']) {
        $rawPath = $CimProcess.ExecutablePath
    }
    $creation = $null
    if ($null -ne $CimProcess.PSObject.Properties['CreationDate']) {
        $creation = $CimProcess.CreationDate
    }
    if ([string]::IsNullOrWhiteSpace([string]$rawPath) -or [string]::IsNullOrWhiteSpace([string]$creation)) {
        if ($RequireCompleteIdentity) {
            throw "Missing executable path or CreationDate for attributable process Id=$($CimProcess.ProcessId)."
        }
        return $null
    }

    $parentId = [uint32]0
    if ($null -ne $CimProcess.PSObject.Properties['ParentProcessId'] -and $null -ne $CimProcess.ParentProcessId) {
        if (Test-DevManagerIntegralNumber -Value $CimProcess.ParentProcessId) {
            $parentId = [uint32]$CimProcess.ParentProcessId
        }
    }

    try {
        $normalized = Normalize-DevManagerPath -LiteralPath ([string]$rawPath)
    }
    catch {
        if ($RequireCompleteIdentity) {
            throw "Unnormalizable executable path for attributable process Id=$($CimProcess.ProcessId)."
        }
        return $null
    }

    return [pscustomobject]@{
        processId       = [uint32]$CimProcess.ProcessId
        executablePath  = [string]$normalized
        creationDate    = [string]$creation
        parentProcessId = $parentId
    }
}

function Get-DevManagerDisposableDevelopmentProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    $worktree = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $matched = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $CimProcesses) {
        $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $entry) { continue }
        if (-not (Test-DevManagerWorktreeTargetExecutable -ExecutablePath ([string]$entry.executablePath) -WorktreeRoot $worktree)) {
            continue
        }
        $matched.Add($entry)
    }
    return @($matched | Sort-Object processId, executablePath, creationDate, parentProcessId)
}

function Get-DevManagerProcessInventory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    $processes = Get-DevManagerDisposableDevelopmentProcesses -WorktreeRoot $WorktreeRoot -CimProcesses $CimProcesses
    return [pscustomobject]@{
        schemaVersion = [int]1
        capturedAtUtc = [DateTime]::UtcNow.ToString('o')
        worktreeRoot  = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
        processes     = [object[]]@($processes)
    }
}

function Get-DevManagerProcessInventoryIdentityKey {
    param([Parameter(Mandatory = $true)][object]$Process)
    $path = Normalize-DevManagerPath -LiteralPath ([string]$Process.executablePath)
    return "pid=$([uint32]$Process.processId);exe=$path;start=$([string]$Process.creationDate)"
}

function Get-DevManagerObservedProcessIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [uint32]$ProcessId,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses,
        [switch]$RequireCompleteIdentity
    )

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $match = @($CimProcesses | Where-Object {
            $null -ne $_.ProcessId -and [uint32]$_.ProcessId -eq $ProcessId
        } | Select-Object -First 1)
    if ($match.Count -eq 0) {
        if ($RequireCompleteIdentity) {
            throw "Unable to locate CIM identity for process Id=$ProcessId."
        }
        return $null
    }
    return Get-DevManagerProcessInventoryEntry -CimProcess $match[0] -RequireCompleteIdentity:$RequireCompleteIdentity
}

function Get-DevManagerRefreshedCimProcess {
    param(
        [Parameter(Mandatory = $true)]
        [uint32]$ProcessId
    )

    try {
        $refreshRows = @(Get-CimInstance -ClassName Win32_Process -Filter ("ProcessId = {0}" -f $ProcessId) -ErrorAction Stop)
    }
    catch {
        throw "CIM lookup failed while refreshing attributable process Id=${ProcessId}: $($_.Exception.Message)"
    }
    if ($refreshRows.Count -eq 0) { return $null }
    if ($refreshRows.Count -ne 1) {
        throw "Ambiguous CIM refresh for attributable process Id=${ProcessId}."
    }

    $refreshed = $refreshRows[0]
    if ($null -eq $refreshed.ProcessId -or -not (Test-DevManagerIntegralNumber -Value $refreshed.ProcessId) -or [uint32]$refreshed.ProcessId -ne $ProcessId) {
        throw "Mismatched ProcessId for refreshed attributable process Id=${ProcessId}."
    }
    return $refreshed
}

function Update-DevManagerObservedProcessTree {
    param(
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [System.Collections.Generic.HashSet[uint32]]$TrackedPids,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    # PIDs are only traversal links. ObservedByKey remains the generation-aware
    # authority (PID + executable + creation time) for live residue decisions.
    $lineagePids = New-Object 'System.Collections.Generic.HashSet[uint32]'
    foreach ($trackedPid in $TrackedPids) {
        $null = $lineagePids.Add([uint32]$trackedPid)
    }

    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($proc in $CimProcesses) {
            if ($null -eq $proc.ProcessId -or $null -eq $proc.ParentProcessId) { continue }
            if (-not (Test-DevManagerIntegralNumber -Value $proc.ParentProcessId)) { continue }
            $pidValue = [uint32]$proc.ProcessId
            $parentId = [uint32]$proc.ParentProcessId
            if (-not $lineagePids.Contains($parentId)) { continue }

            $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
            if ($null -eq $entry) {
                # Snapshot may tear down with null ExecutablePath/CreationDate after exit.
                # Confirm via a fresh PID-specific lookup before failing closed.
                $refreshed = Get-DevManagerRefreshedCimProcess -ProcessId $pidValue
                if ($null -eq $refreshed) {
                    # The process exited, but descendants from this same CIM snapshot
                    # or a later quiet-window poll can outlive it. Keep a gate-lifetime
                    # lineage tombstone; ObservedByKey remains the live authority.
                    $null = $TrackedPids.Add($pidValue)
                    if ($lineagePids.Add($pidValue)) { $changed = $true }
                    continue
                }
                if ($null -eq $refreshed.PSObject.Properties['ParentProcessId'] -or -not (Test-DevManagerIntegralNumber -Value $refreshed.ParentProcessId)) {
                    throw "Missing or invalid ParentProcessId for refreshed attributable process Id=${pidValue}."
                }
                if (-not $lineagePids.Contains([uint32]$refreshed.ParentProcessId)) {
                    # PID reuse under a different parent is not live attributable,
                    # but the old snapshot generation may have surviving children.
                    $null = $TrackedPids.Add($pidValue)
                    if ($lineagePids.Add($pidValue)) { $changed = $true }
                    continue
                }
                $entry = Get-DevManagerProcessInventoryEntry -CimProcess $refreshed -RequireCompleteIdentity
            }

            $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
            if ($ObservedByKey.ContainsKey($key)) { continue }
            $ObservedByKey[$key] = $entry
            $null = $TrackedPids.Add($pidValue)
            $null = $lineagePids.Add($pidValue)
            $changed = $true
        }
    }
}

function Get-DevManagerPhaseGateResidueProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $beforeKeys = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($proc in @($BeforeProcesses)) {
        $null = $beforeKeys.Add((Get-DevManagerProcessInventoryIdentityKey -Process $proc))
    }

    $liveByKey = @{}
    $refreshedEntries = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $CimProcesses) {
        if ($null -eq $proc.ProcessId) { continue }
        $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $entry) {
            $observedPid = @($ObservedByKey.Values | Where-Object {
                    [uint32]$_.processId -eq [uint32]$proc.ProcessId
                })
            if ($observedPid.Count -eq 0) { continue }
            $refreshed = Get-DevManagerRefreshedCimProcess -ProcessId ([uint32]$proc.ProcessId)
            if ($null -eq $refreshed) { continue }
            $entry = Get-DevManagerProcessInventoryEntry -CimProcess $refreshed -RequireCompleteIdentity
            $refreshedEntries.Add($entry)
        }
        $liveByKey[(Get-DevManagerProcessInventoryIdentityKey -Process $entry)] = $entry
    }

    $residue = New-Object System.Collections.Generic.List[object]
    foreach ($key in @($ObservedByKey.Keys)) {
        if ($liveByKey.ContainsKey($key)) {
            $residue.Add($liveByKey[$key])
        }
    }
    foreach ($entry in @(Get-DevManagerDisposableDevelopmentProcesses -WorktreeRoot $WorktreeRoot -CimProcesses $CimProcesses)) {
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if ($beforeKeys.Contains($key)) { continue }
        if ($ObservedByKey.ContainsKey($key)) { continue }
        $residue.Add($entry)
    }
    foreach ($entry in $refreshedEntries) {
        if (-not (Test-DevManagerWorktreeTargetExecutable -ExecutablePath ([string]$entry.executablePath) -WorktreeRoot $WorktreeRoot)) {
            continue
        }
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if ($beforeKeys.Contains($key)) { continue }
        if ($ObservedByKey.ContainsKey($key)) { continue }
        $residue.Add($entry)
    }

    $unique = New-Object 'System.Collections.Generic.Dictionary[string, object]'
    foreach ($entry in $residue) {
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if (-not $unique.ContainsKey($key)) { $unique[$key] = $entry }
    }
    return @($unique.Values | Sort-Object processId, executablePath, creationDate, parentProcessId)
}

function Wait-DevManagerPhaseGateQuietWindow {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [System.Collections.Generic.HashSet[uint32]]$TrackedPids,
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [int]$TimeoutMilliseconds = 20000,
        [int]$PollMilliseconds = 250,
        [int]$QuietMilliseconds = 1000,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    if ($QuietMilliseconds -lt 1000) {
        throw "QuietMilliseconds must be at least 1000."
    }
    $requiredCleanPolls = [Math]::Max(2, [int][Math]::Ceiling($QuietMilliseconds / [double]$PollMilliseconds))
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMilliseconds)
    $cleanStreak = 0
    $lastResidue = @()

    while ($true) {
        # Every quiet-window poll refreshes descendant attribution before residue classification.
        Update-DevManagerObservedProcessTree `
            -ObservedByKey $ObservedByKey `
            -TrackedPids $TrackedPids `
            -CimProcesses $CimProcesses

        $lastResidue = @(Get-DevManagerPhaseGateResidueProcesses `
                -WorktreeRoot $WorktreeRoot `
                -ObservedByKey $ObservedByKey `
                -BeforeProcesses $BeforeProcesses `
                -CimProcesses $CimProcesses)

        if ($lastResidue.Count -eq 0) {
            $cleanStreak++
            if ($cleanStreak -ge $requiredCleanPolls) {
                return ,([object[]]@())
            }
        }
        else {
            $cleanStreak = 0
        }

        if ([DateTime]::UtcNow -ge $deadline) {
            return ,([object[]]$lastResidue)
        }
        if ($null -ne $CimProcesses) {
            if ($cleanStreak -ge $requiredCleanPolls) { return ,([object[]]@()) }
            return ,([object[]]$lastResidue)
        }
        Start-Sleep -Milliseconds $PollMilliseconds
    }
}

function Classify-DevManagerPhaseCleanupResult {
    param(
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [AllowEmptyCollection()]
        [object[]]$AfterProcesses
    )

    $null = $BeforeProcesses
    if (@($AfterProcesses).Count -eq 0) { return 'clean' }
    return 'residue'
}

function New-DevManagerPhaseGateUnavailableAfterInventory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [string]$RunId,
        [AllowNull()]
        [object]$RootIdentity,
        [Parameter(Mandatory = $true)]
        [string]$ObservationFailure
    )

    $bounded = [string]$ObservationFailure
    if ($bounded.Length -gt 512) {
        $bounded = $bounded.Substring(0, 512)
    }

    return [pscustomobject]@{
        schemaVersion       = [int]1
        status              = 'unavailable'
        capturedAtUtc       = [DateTime]::UtcNow.ToString('o')
        worktreeRoot        = (Normalize-DevManagerPath -LiteralPath $WorktreeRoot)
        runId               = [string]$RunId
        processes           = [object[]]@()
        rootIdentity        = $RootIdentity
        observationFailure  = $bounded
    }
}

function Get-DevManagerPhaseGateFinalExitCode {
    param(
        [AllowNull()]
        [object]$ChildExitCode,
        [switch]$ProductionAssertFailed,
        [switch]$VerificationWriteFailed,
        [switch]$EvidenceWriteFailed,
        [string]$OriginalGuardFailure
    )

    if ($ProductionAssertFailed -or $VerificationWriteFailed -or $EvidenceWriteFailed) {
        return 1
    }
    if (-not [string]::IsNullOrWhiteSpace($OriginalGuardFailure)) {
        if ($null -ne $ChildExitCode -and [int]$ChildExitCode -ne 0) {
            return [int]$ChildExitCode
        }
        return 1
    }
    if ($null -eq $ChildExitCode) {
        return 1
    }
    return [int]$ChildExitCode
}
