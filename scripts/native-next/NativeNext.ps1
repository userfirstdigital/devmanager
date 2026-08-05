# Phase 0 native-next validation scaffold.
# Plans isolated paths/env and drives Capture/Assert only.
# Real build/start/stop/kill/ctl/runtime IO is deferred to Phase 2+ (Rust host/supervisor).

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:NativeNextProfile = 'native-next-dev'
$script:NativeNextInstanceLabel = 'Next'
$script:NativeNextRuntimeKind = 'native-next'

function Get-NativeNextChildEnvironment {
    return [ordered]@{
        DEVMANAGER_PROFILE        = $script:NativeNextProfile
        DEVMANAGER_INSTANCE_LABEL = $script:NativeNextInstanceLabel
        DEVMANAGER_RUNTIME_KIND   = $script:NativeNextRuntimeKind
    }
}

function Get-NativeNextValidationPlan {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ScriptRoot
    )

    $worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $ScriptRoot
    $buildTargetDir = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot 'target-native-next'))
    $liveDir = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot 'target-live-native-next'))
    $runtimeDir = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot '.devmanager-next'))
    $runtimeJson = [System.IO.Path]::GetFullPath((Join-Path $runtimeDir 'runtime.json'))
    $evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $ScriptRoot

    $plan = [pscustomobject]@{
        worktreeRoot   = $worktreeRoot
        buildTargetDir = $buildTargetDir
        liveDir        = $liveDir
        runtimeDir     = $runtimeDir
        runtimeJson    = $runtimeJson
        evidenceRoot   = $evidenceRoot
        hostLiveExe    = [System.IO.Path]::GetFullPath((Join-Path $liveDir 'devmanager-host.exe'))
        desktopLiveExe = [System.IO.Path]::GetFullPath((Join-Path $liveDir 'devmanager-next.exe'))
        profile        = $script:NativeNextProfile
        instanceLabel  = $script:NativeNextInstanceLabel
        runtimeKind    = $script:NativeNextRuntimeKind
        scriptRoot     = [System.IO.Path]::GetFullPath($ScriptRoot)
    }

    Assert-NativeNextValidationPlan -Plan $plan
    return $plan
}

function Assert-NativeNextValidationPlan {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    $protected = Get-DevManagerProductionRoot
    foreach ($path in @(
            $Plan.worktreeRoot,
            $Plan.buildTargetDir,
            $Plan.liveDir,
            $Plan.runtimeDir,
            $Plan.runtimeJson,
            $Plan.evidenceRoot,
            $Plan.hostLiveExe,
            $Plan.desktopLiveExe
        )) {
        if (-not (Test-DevManagerAbsolutePath -LiteralPath ([string]$path))) {
            throw "Native-next validation path must be fully qualified ('$path')."
        }
        $null = Normalize-DevManagerPath -LiteralPath ([string]$path)
        if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath ([string]$path) -AncestorPath ([string]$Plan.worktreeRoot))) {
            throw "Native-next path '$path' escapes worktree root '$($Plan.worktreeRoot)'."
        }
        if (Test-DevManagerPathEqualsOrBeneath -LiteralPath ([string]$path) -AncestorPath $protected) {
            throw "Native-next path '$path' collides with protected production root '$protected'."
        }
    }

    foreach ($path in @($Plan.buildTargetDir, $Plan.liveDir, $Plan.runtimeDir, $Plan.runtimeJson, $Plan.evidenceRoot)) {
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath ([string]$path)
    }

    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath ([string]$Plan.hostLiveExe) -AncestorPath ([string]$Plan.liveDir))) {
        throw 'Host live executable escapes live directory.'
    }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath ([string]$Plan.desktopLiveExe) -AncestorPath ([string]$Plan.liveDir))) {
        throw 'Desktop live executable escapes live directory.'
    }
}

function Write-NativeNextValidationPlan {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan,
        [Parameter(Mandatory = $true)]
        [string]$Action
    )

    $envMap = Get-NativeNextChildEnvironment
    Write-Host ("Native-next {0} validation plan worktree={1}" -f $Action, $Plan.worktreeRoot)
    Write-Host ("buildTargetDir={0}" -f $Plan.buildTargetDir)
    Write-Host ("liveDir={0}" -f $Plan.liveDir)
    Write-Host ("runtimeJson={0} (planned path only; Phase 0 does not read/write it)" -f $Plan.runtimeJson)
    Write-Host ("evidenceRoot={0}" -f $Plan.evidenceRoot)
    Write-Host ("env DEVMANAGER_PROFILE={0}; DEVMANAGER_INSTANCE_LABEL={1}; DEVMANAGER_RUNTIME_KIND={2}" -f `
            $envMap.DEVMANAGER_PROFILE, $envMap.DEVMANAGER_INSTANCE_LABEL, $envMap.DEVMANAGER_RUNTIME_KIND)
    Write-Host ("planned live exes: {0} ; {1}" -f $Plan.hostLiveExe, $Plan.desktopLiveExe)
}

function Invoke-NativeNextCaptureBaseline {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan,
        [Parameter(Mandatory = $true)]
        [string]$LeafName
    )

    $baselinePath = Join-Path ([string]$Plan.evidenceRoot) ("current\$LeafName")
    $capture = Join-Path ([string]$Plan.scriptRoot) 'Capture-ProductionBaseline.ps1'
    & $capture -OutputPath $baselinePath
    return [System.IO.Path]::GetFullPath($baselinePath)
}

function Invoke-NativeNextAssertUnchanged {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan,
        [Parameter(Mandatory = $true)]
        [string]$BaselinePath
    )

    $assert = Join-Path ([string]$Plan.scriptRoot) 'Assert-ProductionUnchanged.ps1'
    & $assert -BaselinePath $BaselinePath
}

function Invoke-NativeNextStartValidation {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ScriptRoot
    )

    $plan = Get-NativeNextValidationPlan -ScriptRoot $ScriptRoot
    Write-NativeNextValidationPlan -Plan $plan -Action 'Start'
    Write-Host 'ValidateOnly: path/env plan only; no cargo/copy/start/stop/kill/ctl; no runtime.json IO.'
    $baselinePath = Invoke-NativeNextCaptureBaseline -Plan $plan -LeafName 'start-baseline.json'
    Invoke-NativeNextAssertUnchanged -Plan $plan -BaselinePath $baselinePath
    Write-Host 'ValidateOnly complete; production unchanged; no process started.'
}

function Invoke-NativeNextStopValidation {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ScriptRoot
    )

    $plan = Get-NativeNextValidationPlan -ScriptRoot $ScriptRoot
    Write-NativeNextValidationPlan -Plan $plan -Action 'Stop'
    Write-Host 'ValidateOnly: path/env plan only; no stop/kill/ctl; no runtime.json IO.'
    $baselinePath = Invoke-NativeNextCaptureBaseline -Plan $plan -LeafName 'stop-baseline.json'
    Invoke-NativeNextAssertUnchanged -Plan $plan -BaselinePath $baselinePath
    Write-Host 'ValidateOnly complete; production unchanged; no process stopped.'
}
