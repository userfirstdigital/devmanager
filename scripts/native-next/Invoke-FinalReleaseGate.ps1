#Requires -Version 7
<#
.SYNOPSIS
  Fail-closed final release verification gate (isolated, fixture-only).

.DESCRIPTION
  Sequences cargo check/tests, web test/typecheck/build, and safe fixture smokes
  under a unique CARGO_TARGET_DIR with no production profile. Captures a
  production baseline (config.json / remote.json hashes and installed PID/start
  time), asserts it after every command path, and writes structured evidence.

  This gate does not replace packaging, signing, or public release workflow.
  It never reads or hashes session.json, never kills the installed app, and
  never invokes soak/install/publish/tag/release scripts.

  -SkipWeb and -SkipSmokes are explicit opt-in escapes. Either one makes the
  overall result HOLD, never PASS. A missing, unsafe, or typed-HOLD smoke is
  also HOLD. Provider smoke fixture HOLD is recorded as a dependency result
  and is never converted to PASS.

.PARAMETER RepoRoot
  Git worktree root. Must canonicalize to this script's worktree.

.PARAMETER RunId
  Optional path-safe run identity ([A-Za-z0-9._-], 64 chars max). Default: 32-hex GUID.

.PARAMETER SkipWeb
  Skip web test/typecheck/build. Documented opt-in; overall result cannot be PASS.

.PARAMETER SkipSmokes
  Skip fixture smokes. Explicit opt-in; overall result cannot be PASS.

.PARAMETER PlanOnly
  Validate isolation, discover commands/smokes, and write plan evidence. Does
  not run cargo/npm/smokes, capture a live baseline, or claim PASS.

.NOTES
  Exit codes: 0 = PASS (or successful PlanOnly), 2 = HOLD, 1 = FAIL.
  Long Rust verification starts expected Cargo/rustc/harness processes under
  the isolated target only. The gate must prove they are gone afterward.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$RunId = '',
    [switch]$SkipWeb,
    [switch]$SkipSmokes,
    [switch]$PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw 'Invoke-FinalReleaseGate.ps1 requires PowerShell 7 or later.'
}

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

$script:RemovedExact = [string[]]@(
    'DEVMANAGER_PROFILE',
    'DEVMANAGER_CONFIG_DIR',
    'DEVMANAGER_APP_IDENTITY',
    'DEVMANAGER_INSTANCE_LABEL',
    'DEVMANAGER_RUNTIME_KIND',
    'ANTHROPIC_API_KEY',
    'OPENAI_API_KEY',
    'CURSOR_API_KEY',
    'CLAUDE_API_KEY',
    'CARGO_TARGET_DIR',
    'CARGO_BUILD_TARGET_DIR',
    'CARGO_TARGET_TMPDIR',
    'RUSTC_WRAPPER',
    'RUSTC_WORKSPACE_WRAPPER',
    'RUSTFLAGS',
    'CARGO_ENCODED_RUSTFLAGS',
    'GIT_CONFIG_GLOBAL',
    'GIT_CONFIG_SYSTEM',
    'GIT_CONFIG_COUNT',
    'GIT_CONFIG_PARAMETERS',
    'GIT_EXEC_PATH',
    'GIT_DIR',
    'GIT_WORK_TREE',
    'GIT_COMMON_DIR',
    'GIT_NAMESPACE',
    'GIT_ALTERNATE_OBJECT_DIRECTORIES',
    'GIT_OBJECT_DIRECTORY',
    'GIT_INDEX_FILE',
    'GIT_ASKPASS',
    'GIT_TRACE',
    'GIT_PROXY_COMMAND',
    'GIT_SSH',
    'GIT_SSH_COMMAND',
    'GIT_EXTERNAL_DIFF',
    'GIT_DIFF_OPTS',
    'GCM_INTERACTIVE',
    'GIT_ALLOW_PROTOCOL'
)
$script:SecretNamePattern = '(?i)(API_KEY|ACCESS_TOKEN|SECRET|PASSWORD|PRIVATE_KEY|AUTH_TOKEN)$'
$script:UnsafeParamNames = [string[]]@(
    'Authenticated', 'Install', 'Publish', 'Tag', 'Release', 'Soak',
    'Kill', 'Stop', 'Start', 'ExtractInstallers'
)
$script:ForbiddenSmokeNames = [string[]]@(
    'Invoke-ProcessSoak.ps1',
    'Invoke-FinalSoak.ps1',
    'Invoke-Phase3ProcessSupervisorGate.ps1',
    'Start-NativeNext.ps1',
    'Stop-NativeNext.ps1'
)

function Get-FinalReleaseProcessEnvironmentSnapshot {
    $map = @{}
    $vars = [Environment]::GetEnvironmentVariables('Process')
    foreach ($key in @($vars.Keys)) {
        $map[[string]$key] = [string]$vars[$key]
    }
    return $map
}

function Restore-FinalReleaseProcessEnvironment {
    param([Parameter(Mandatory = $true)][hashtable]$Snapshot)

    $current = [Environment]::GetEnvironmentVariables('Process')
    foreach ($key in @($current.Keys)) {
        if (-not $Snapshot.ContainsKey([string]$key)) {
            [Environment]::SetEnvironmentVariable([string]$key, $null, 'Process')
        }
    }
    foreach ($key in @($Snapshot.Keys)) {
        [Environment]::SetEnvironmentVariable([string]$key, [string]$Snapshot[$key], 'Process')
    }
}

function Test-FinalReleaseRemovedEnvironmentName {
    param([Parameter(Mandatory = $true)][string]$Name)

    if ($script:RemovedExact -contains $Name) { return $true }
    if ($Name.StartsWith('DEVMANAGER_', [StringComparison]::OrdinalIgnoreCase)) { return $true }
    if ($Name -match $script:SecretNamePattern) { return $true }
    return $false
}

function New-FinalReleaseChildEnvironment {
    param(
        [Parameter(Mandatory = $true)][hashtable]$ParentSnapshot,
        [string]$CargoTargetDir,
        [string]$TempDir
    )

    $child = @{}
    foreach ($key in @($ParentSnapshot.Keys)) {
        if (Test-FinalReleaseRemovedEnvironmentName -Name $key) { continue }
        $child[$key] = $ParentSnapshot[$key]
    }
    if (-not [string]::IsNullOrWhiteSpace($CargoTargetDir)) {
        $child['CARGO_TARGET_DIR'] = $CargoTargetDir
    }
    $child['CARGO_TERM_COLOR'] = 'never'
    $child['CARGO_INCREMENTAL'] = '0'
    $child['GIT_TERMINAL_PROMPT'] = '0'
    $child['GIT_CONFIG_NOSYSTEM'] = '1'
    if (-not [string]::IsNullOrWhiteSpace($TempDir)) {
        $child['TEMP'] = $TempDir
        $child['TMP'] = $TempDir
    }
    return $child
}

function Set-FinalReleaseStartInfoEnvironment {
    param(
        [Parameter(Mandatory = $true)][System.Diagnostics.ProcessStartInfo]$StartInfo,
        [Parameter(Mandatory = $true)][hashtable]$Environment
    )

    $StartInfo.Environment.Clear()
    foreach ($key in @($Environment.Keys)) {
        $StartInfo.Environment[$key] = $Environment[$key]
    }
}

function Resolve-FinalReleaseUniqueApplication {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [string[]]$PreferredLeaves
    )

    $commands = @(
        Get-Command -Name $Name -All -CommandType Application -ErrorAction SilentlyContinue |
            Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_.Source) }
    )
    if ($commands.Count -eq 0) {
        throw "Unable to resolve unique application '$Name'."
    }
    $paths = @(
        $commands |
            ForEach-Object { [System.IO.Path]::GetFullPath([string]$_.Source) } |
            Select-Object -Unique
    )
    if ($null -ne $PreferredLeaves -and @($PreferredLeaves).Count -gt 0) {
        $preferred = @(
            $paths |
                Where-Object { $PreferredLeaves -contains [System.IO.Path]::GetFileName($_) }
        )
        if ($preferred.Count -gt 0) { $paths = $preferred }
    }
    $normalized = @(
        $paths |
            ForEach-Object { Normalize-DevManagerPath -LiteralPath $_ } |
            Select-Object -Unique
    )
    if ($normalized.Count -ne 1) {
        throw "Ambiguous application '$Name' ($($normalized.Count) matches)."
    }
    $resolved = [System.IO.Path]::GetFullPath($paths[0])
    foreach ($install in @(Get-DevManagerSupportedInstallPaths)) {
        if ((Normalize-DevManagerPath -LiteralPath $resolved) -eq (Normalize-DevManagerPath -LiteralPath ([string]$install))) {
            throw "Rejecting installed DevManager path masquerading as '$Name'."
        }
    }
    return $resolved
}

function Resolve-FinalReleaseGit {
    $candidates = @(
        (Join-Path ${env:ProgramFiles} 'Git\cmd\git.exe'),
        (Join-Path ${env:ProgramFiles} 'Git\bin\git.exe')
    )
    foreach ($candidate in $candidates) {
        if ([string]::IsNullOrWhiteSpace([string]$candidate)) { continue }
        if (-not (Test-Path -LiteralPath $candidate)) { continue }
        $full = [System.IO.Path]::GetFullPath($candidate)
        if ([System.IO.Path]::GetFileName($full) -ine 'git.exe') { continue }
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $full
        return $full
    }
    return (Resolve-FinalReleaseUniqueApplication -Name 'git' -PreferredLeaves @('git.exe'))
}

function Get-FinalReleaseScriptParamContract {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($LiteralPath, [ref]$tokens, [ref]$parseErrors)
    if ($null -eq $ast -or @($parseErrors).Count -gt 0) {
        return [pscustomobject]@{
            ok         = $false
            reason     = 'parse-error'
            parameters = [object[]]@()
        }
    }
    $parameters = New-Object System.Collections.Generic.List[object]
    if ($null -ne $ast.ParamBlock) {
        foreach ($parameter in @($ast.ParamBlock.Parameters)) {
            $name = [string]$parameter.Name.VariablePath.UserPath
            $mandatory = $false
            foreach ($attribute in @($parameter.Attributes)) {
                if ([string]$attribute.TypeName.Name -ne 'Parameter') { continue }
                foreach ($named in @($attribute.NamedArguments)) {
                    if ([string]$named.ArgumentName -ne 'Mandatory') { continue }
                    if ([bool]$named.Argument.SafeGetValue()) { $mandatory = $true }
                }
            }
            $parameters.Add([pscustomobject]@{
                    name      = $name
                    mandatory = [bool]$mandatory
                    isSwitch  = ($parameter.StaticType -eq [switch])
                })
        }
    }
    return [pscustomobject]@{
        ok         = $true
        reason     = $null
        parameters = [object[]]$parameters.ToArray()
    }
}

function Test-FinalReleaseSmokeContractSafe {
    param(
        [Parameter(Mandatory = $true)]$Contract,
        [Parameter(Mandatory = $true)][string]$Leaf
    )

    if ($script:ForbiddenSmokeNames -contains $Leaf) {
        return [pscustomobject]@{ safe = $false; reason = 'forbidden-script' }
    }
    if (-not [bool]$Contract.ok) {
        return [pscustomobject]@{ safe = $false; reason = [string]$Contract.reason }
    }
    foreach ($parameter in @($Contract.parameters)) {
        $name = [string]$parameter.name
        if ($script:UnsafeParamNames -contains $name -and [bool]$parameter.mandatory) {
            return [pscustomobject]@{ safe = $false; reason = "mandatory-unsafe-param:$name" }
        }
        if ([bool]$parameter.mandatory -and $name -notin @('IsolatedProfile', 'TimeoutSeconds', 'DeadlineMs', 'MaxOutputBytes', 'RepoRoot', 'RunId')) {
            if (-not [bool]$parameter.isSwitch) {
                return [pscustomobject]@{ safe = $false; reason = "mandatory-undiscoverable-param:$name" }
            }
        }
    }
    return [pscustomobject]@{ safe = $true; reason = $null }
}

function Get-FinalReleaseSmokeCatalog {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)

    $native = Join-Path $WorktreeRoot 'scripts\native-next'
    $groups = @(
        [pscustomobject]@{ id = 'workspace-smoke'; leaves = @('Invoke-WorkspaceSmoke.ps1') },
        [pscustomobject]@{ id = 'provider-smoke'; leaves = @('Invoke-ProviderSmoke.ps1') },
        [pscustomobject]@{ id = 'browser-smoke'; leaves = @('Invoke-BrowserSmoke.ps1', 'Invoke-BrowserFixtureSmoke.ps1') },
        [pscustomobject]@{ id = 'prompt-smoke'; leaves = @('Invoke-PromptLibrarySmoke.ps1', 'Invoke-PromptSmoke.ps1') }
    )
    $items = New-Object System.Collections.Generic.List[object]
    foreach ($group in $groups) {
        $found = New-Object System.Collections.Generic.List[object]
        foreach ($leaf in @($group.leaves)) {
            $candidate = [System.IO.Path]::GetFullPath((Join-Path $native $leaf))
            if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $candidate -AncestorPath $WorktreeRoot)) {
                throw "Smoke path '$candidate' escapes worktree."
            }
            if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
            Assert-DevManagerPathHasNoReparsePoints -LiteralPath $candidate
            $contract = Get-FinalReleaseScriptParamContract -LiteralPath $candidate
            $safety = Test-FinalReleaseSmokeContractSafe -Contract $contract -Leaf $leaf
            $found.Add([pscustomobject]@{
                    path     = $candidate
                    leaf     = $leaf
                    contract = $contract
                    safety   = $safety
                })
        }
        $items.Add([pscustomobject]@{
                id    = [string]$group.id
                found = [object[]]$found.ToArray()
            })
    }
    return , ([object[]]$items.ToArray())
}

function New-FinalReleaseCommandRecord {
    param(
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter(Mandatory = $true)][string]$Kind,
        [string]$Executable,
        [string[]]$Arguments,
        [string]$Status = 'planned',
        [object]$ExitCode = $null,
        [object]$DurationMs = $null,
        [string]$Reason = $null,
        [string]$StdoutLog = $null,
        [string]$StderrLog = $null
    )

    $argumentList = [string[]]@()
    if ($null -ne $Arguments) {
        $argumentList = [string[]]@($Arguments)
    }
    return [pscustomobject]@{
        id         = $Id
        kind       = $Kind
        executable = $(if ([string]::IsNullOrWhiteSpace($Executable)) { $null } else { $Executable })
        arguments  = $argumentList
        status     = $Status
        exitCode   = $ExitCode
        durationMs = $DurationMs
        reason     = $(if ([string]::IsNullOrWhiteSpace($Reason)) { $null } else { $Reason })
        stdoutLog  = $(if ([string]::IsNullOrWhiteSpace($StdoutLog)) { $null } else { $StdoutLog })
        stderrLog  = $(if ([string]::IsNullOrWhiteSpace($StderrLog)) { $null } else { $StderrLog })
        residue    = $null
    }
}

function Get-FinalReleaseOwnedResidue {
    param(
        [Parameter(Mandatory = $true)][string]$CargoTargetDir,
        [Parameter(Mandatory = $true)][string]$RunDirectory
    )

    $cim = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    $owned = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $cim) {
        $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $entry) { continue }
        $exe = [string]$entry.executablePath
        $underTarget = Test-DevManagerPathEqualsOrBeneath -LiteralPath $exe -AncestorPath $CargoTargetDir
        $underRun = Test-DevManagerPathEqualsOrBeneath -LiteralPath $exe -AncestorPath $RunDirectory
        if ($underTarget -or $underRun) {
            $owned.Add($entry)
        }
    }
    return , ([object[]]@($owned | Sort-Object processId, executablePath, creationDate))
}

function Invoke-FinalReleaseBoundedChild {
    param(
        [Parameter(Mandatory = $true)][string]$Executable,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][hashtable]$Environment,
        [Parameter(Mandatory = $true)][int]$TimeoutMilliseconds,
        [int]$StdoutBytes = 16MB,
        [int]$StderrBytes = 4MB
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.WorkingDirectory = $WorkingDirectory
    Set-FinalReleaseStartInfoEnvironment -StartInfo $startInfo -Environment $Environment
    foreach ($argument in @($Arguments)) {
        [void]$startInfo.ArgumentList.Add([string]$argument)
    }
    return (Invoke-DevManagerPhaseGateBoundedCommand `
            -StartInfo $startInfo `
            -TimeoutMilliseconds $TimeoutMilliseconds `
            -StdoutBytes $StdoutBytes `
            -StderrBytes $StderrBytes)
}

function Write-FinalReleaseCommandLogs {
    param(
        [Parameter(Mandatory = $true)][string]$LogDirectory,
        [Parameter(Mandatory = $true)][string]$Id,
        [string]$Stdout,
        [string]$Stderr,
        [Parameter(Mandatory = $true)][string]$ProtectedProductionRoot,
        [Parameter(Mandatory = $true)][string]$AllowedEvidenceRoot
    )

    $stdoutPath = [System.IO.Path]::GetFullPath((Join-Path $LogDirectory "$Id.stdout.log"))
    $stderrPath = [System.IO.Path]::GetFullPath((Join-Path $LogDirectory "$Id.stderr.log"))
    foreach ($path in @($stdoutPath, $stderrPath)) {
        Assert-DevManagerEvidencePathSafeForIO `
            -LiteralPath $path `
            -ProtectedProductionRoot $ProtectedProductionRoot `
            -AllowedEvidenceRoot $AllowedEvidenceRoot
    }
    Set-Content -LiteralPath $stdoutPath -Value $(if ($null -eq $Stdout) { '' } else { $Stdout }) -Encoding utf8
    Set-Content -LiteralPath $stderrPath -Value $(if ($null -eq $Stderr) { '' } else { $Stderr }) -Encoding utf8
    return [pscustomobject]@{ stdoutLog = $stdoutPath; stderrLog = $stderrPath }
}

function Resolve-FinalReleaseOverallStatus {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$CommandRecords,
        [Parameter(Mandatory = $true)][bool]$IsPlanOnly,
        [Parameter(Mandatory = $true)][bool]$WebSkipped,
        [Parameter(Mandatory = $true)][bool]$SmokesSkipped,
        [Parameter(Mandatory = $true)][string]$ProductionAssert,
        [AllowEmptyCollection()][object[]]$FinalResidue,
        [string]$GateError,
        [bool]$EvidenceWriteFailed
    )

    if ($IsPlanOnly) {
        if (-not [string]::IsNullOrWhiteSpace($GateError) -or $EvidenceWriteFailed) { return 'FAIL' }
        return 'PLAN'
    }
    if ($EvidenceWriteFailed -or -not [string]::IsNullOrWhiteSpace($GateError)) { return 'FAIL' }
    if ($ProductionAssert -ne 'unchanged') { return 'FAIL' }
    if (@($FinalResidue).Count -gt 0) { return 'FAIL' }
    $hasHold = $false
    foreach ($command in @($CommandRecords)) {
        $status = [string]$command.status
        if ($status -eq 'FAIL') { return 'FAIL' }
        if ($status -eq 'HOLD') { $hasHold = $true }
        if ($status -notin @('PASS', 'HOLD')) { return 'FAIL' }
    }
    if ($WebSkipped -or $SmokesSkipped -or $hasHold) { return 'HOLD' }
    return 'PASS'
}

function Get-FinalReleaseExitCode {
    param([Parameter(Mandatory = $true)][string]$Status)
    switch ($Status) {
        'PASS' { return 0 }
        'PLAN' { return 0 }
        'HOLD' { return 2 }
        default { return 1 }
    }
}

$originalEnvironment = Get-FinalReleaseProcessEnvironmentSnapshot
$environmentRestored = $false
$protectedRoot = $null
$evidenceRoot = $null
$worktreeRoot = $null
$runDirectory = $null
$cargoTargetDir = $null
$tempDir = $null
$logDirectory = $null
$baselinePath = $null
$verificationPath = $null
$commands = New-Object System.Collections.Generic.List[object]
$finalResidue = [object[]]@()
$productionAssert = 'not-run'
$productionAssertFailure = $null
$gateError = $null
$evidenceWriteFailed = $false
$tools = $null
$runIdValue = $null
$script:FinalReleaseExitCode = 1
$script:FinalReleaseStatus = 'FAIL'

function Assert-FinalReleaseGeneratedPath {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$ProtectedProductionRoot,
        [switch]$EvidencePath,
        [string]$AllowedEvidenceRoot
    )

    if ((Normalize-DevManagerPath -LiteralPath $LiteralPath) -match '\\.scratch(\\|$)') {
        throw "Generated path uses forbidden .scratch root: '$LiteralPath'."
    }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $LiteralPath -AncestorPath $WorktreeRoot)) {
        throw "Generated path escapes worktree: '$LiteralPath'."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $LiteralPath -AncestorPath $ProtectedProductionRoot) {
        throw "Generated path collides with production: '$LiteralPath'."
    }
    if ($EvidencePath) {
        Assert-DevManagerEvidencePathSafeForIO `
            -LiteralPath $LiteralPath `
            -ProtectedProductionRoot $ProtectedProductionRoot `
            -AllowedEvidenceRoot $AllowedEvidenceRoot
    }
    else {
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $LiteralPath
    }
}

try {
    try {
        Ensure-DevManagerPhaseGateJobType
        if ($null -eq ([System.Management.Automation.PSTypeName]'DevManagerPhaseGateJob').Type) {
            throw 'Safe process-tree helper is unavailable; failing closed.'
        }

        $ownRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
        if ([string]::IsNullOrWhiteSpace($RepoRoot)) {
            $worktreeRoot = $ownRoot
        }
        else {
            if (-not (Test-DevManagerAbsolutePath -LiteralPath $RepoRoot)) {
                throw "RepoRoot must be a fully qualified path ('$RepoRoot')."
            }
            $worktreeRoot = [System.IO.Path]::GetFullPath($RepoRoot.Trim())
        }
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktreeRoot
        if ((Normalize-DevManagerPath -LiteralPath $worktreeRoot) -ne (Normalize-DevManagerPath -LiteralPath $ownRoot)) {
            throw "RepoRoot must be this script's Git worktree ('$ownRoot')."
        }
        $gitDir = Join-Path $worktreeRoot '.git'
        if (-not (Test-Path -LiteralPath $gitDir)) {
            throw "RepoRoot is not a Git worktree (missing .git): '$worktreeRoot'."
        }

        $protectedRoot = Get-DevManagerProductionRoot
        $evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
        if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $worktreeRoot -AncestorPath $protectedRoot) {
            throw 'Worktree root collides with the protected production root.'
        }
        if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $evidenceRoot -AncestorPath $worktreeRoot)) {
            throw 'Evidence root must stay beneath the worktree.'
        }

        $currentPwsh = [System.IO.Path]::GetFullPath((Join-Path $PSHome 'pwsh.exe'))
        if (-not (Test-Path -LiteralPath $currentPwsh -PathType Leaf)) {
            throw 'Current PowerShell 7 host pwsh.exe is missing.'
        }
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $currentPwsh
        $tools = [pscustomobject]@{
            git   = Resolve-FinalReleaseGit
            cargo = Resolve-FinalReleaseUniqueApplication -Name 'cargo' -PreferredLeaves @('cargo.exe')
            npm   = $null
            pwsh  = $currentPwsh
        }
        $webManifest = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot 'web\package.json'))
        $webPresent = Test-Path -LiteralPath $webManifest -PathType Leaf
        if ($webPresent) {
            $tools.npm = Resolve-FinalReleaseUniqueApplication -Name 'npm' -PreferredLeaves @('npm.cmd', 'npm.exe')
        }
        elseif (-not $SkipWeb) {
            throw 'web/package.json is missing; web test/typecheck/build are mandatory unless -SkipWeb is set.'
        }

        if ([string]::IsNullOrWhiteSpace($RunId)) {
            $runIdValue = [guid]::NewGuid().ToString('N')
        }
        else {
            $runIdValue = Assert-DevManagerPhaseName -Phase $RunId
        }

        $runDirectory = [System.IO.Path]::GetFullPath((Join-Path $evidenceRoot "final-release\$runIdValue"))
        $cargoTargetDir = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot ".devmanager-next\target-final-release\$runIdValue"))
        $tempDir = [System.IO.Path]::GetFullPath((Join-Path $runDirectory 'tmp'))
        $logDirectory = [System.IO.Path]::GetFullPath((Join-Path $runDirectory 'logs'))
        $baselinePath = Join-Path $runDirectory 'baseline.json'
        $verificationPath = Join-Path $runDirectory $(if ($PlanOnly) { 'plan.json' } else { 'verification.json' })
        foreach ($path in @($runDirectory, $tempDir, $logDirectory, $baselinePath, $verificationPath)) {
            Assert-FinalReleaseGeneratedPath `
                -LiteralPath $path `
                -WorktreeRoot $worktreeRoot `
                -ProtectedProductionRoot $protectedRoot `
                -EvidencePath `
                -AllowedEvidenceRoot $evidenceRoot
        }
        Assert-FinalReleaseGeneratedPath `
            -LiteralPath $cargoTargetDir `
            -WorktreeRoot $worktreeRoot `
            -ProtectedProductionRoot $protectedRoot
        if (Test-Path -LiteralPath $runDirectory) {
            throw "Refusing to reuse existing run directory '$runDirectory'."
        }

        New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
        New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory

        $gitEnv = New-FinalReleaseChildEnvironment -ParentSnapshot $originalEnvironment -TempDir $tempDir
        $gitResult = Invoke-FinalReleaseBoundedChild `
            -Executable $tools.git `
            -Arguments @('-C', $worktreeRoot, 'rev-parse', '--show-toplevel', '--is-inside-work-tree') `
            -WorkingDirectory $worktreeRoot `
            -Environment $gitEnv `
            -TimeoutMilliseconds 15000 `
            -StdoutBytes 16384 `
            -StderrBytes 16384
        if ($gitResult.ExitCode -ne 0) {
            throw 'git rev-parse failed; RepoRoot is not a usable Git worktree.'
        }
        $gitLines = @(
            $gitResult.Stdout -split "`r?`n" |
                ForEach-Object { $_.Trim() } |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
        )
        if ($gitLines.Count -lt 2 -or $gitLines[1] -cne 'true') {
            throw 'git worktree identity was malformed or is-inside-work-tree was not true.'
        }
        $topLevel = [System.IO.Path]::GetFullPath($gitLines[0])
        if ((Normalize-DevManagerPath -LiteralPath $topLevel) -ne (Normalize-DevManagerPath -LiteralPath $worktreeRoot)) {
            throw "Git toplevel '$topLevel' does not match RepoRoot '$worktreeRoot'."
        }

        Write-Host ("CARGO_TARGET_DIR={0}" -f $cargoTargetDir)
        Write-Host ("runDirectory={0}" -f $runDirectory)
        Write-Host ("repoRoot={0}" -f $worktreeRoot)

        $childEnv = New-FinalReleaseChildEnvironment `
            -ParentSnapshot $originalEnvironment `
            -CargoTargetDir $cargoTargetDir `
            -TempDir $tempDir

        $commands.Add((New-FinalReleaseCommandRecord -Id 'cargo-check' -Kind 'mandatory' -Executable $tools.cargo -Arguments @('check', '--locked', '--lib', '--bins', '--tests')))
        $commands.Add((New-FinalReleaseCommandRecord -Id 'cargo-test-lib' -Kind 'mandatory' -Executable $tools.cargo -Arguments @('test', '--locked', '--lib', '--', '--test-threads=1')))
        $commands.Add((New-FinalReleaseCommandRecord -Id 'cargo-test-integration' -Kind 'mandatory' -Executable $tools.cargo -Arguments @('test', '--locked', '--tests', '--', '--test-threads=1')))
        if ($SkipWeb) {
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-test' -Kind 'web' -Status 'HOLD' -Reason 'skip-web-opt-in'))
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-typecheck' -Kind 'web' -Status 'HOLD' -Reason 'skip-web-opt-in'))
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-build' -Kind 'web' -Status 'HOLD' -Reason 'skip-web-opt-in'))
        }
        else {
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-test' -Kind 'web' -Executable $tools.npm -Arguments @('--prefix', 'web', 'test', '--', '--run')))
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-typecheck' -Kind 'web' -Executable $tools.npm -Arguments @('--prefix', 'web', 'run', 'typecheck')))
            $commands.Add((New-FinalReleaseCommandRecord -Id 'web-build' -Kind 'web' -Executable $tools.npm -Arguments @('--prefix', 'web', 'run', 'build')))
        }

        $smokeCatalog = Get-FinalReleaseSmokeCatalog -WorktreeRoot $worktreeRoot
        foreach ($group in @($smokeCatalog)) {
            if ($SkipSmokes) {
                $commands.Add((New-FinalReleaseCommandRecord -Id $group.id -Kind 'smoke' -Status 'HOLD' -Reason 'skip-smokes-opt-in'))
                continue
            }
            if (@($group.found).Count -eq 0) {
                $commands.Add((New-FinalReleaseCommandRecord -Id $group.id -Kind 'smoke' -Status 'HOLD' -Reason 'missing-smoke-script'))
                continue
            }
            foreach ($found in @($group.found)) {
                $smokeId = if (@($group.found).Count -eq 1) { [string]$group.id } else { '{0}:{1}' -f $group.id, [IO.Path]::GetFileNameWithoutExtension($found.leaf) }
                if (-not [bool]$found.safety.safe) {
                    $commands.Add((New-FinalReleaseCommandRecord -Id $smokeId -Kind 'smoke' -Executable $tools.pwsh -Arguments @('-NoProfile', '-NonInteractive', '-File', [string]$found.path) -Status 'HOLD' -Reason ('unsafe-smoke:' + [string]$found.safety.reason)))
                    continue
                }
                $smokeArgs = [System.Collections.Generic.List[string]]::new()
                [void]$smokeArgs.Add('-NoProfile')
                [void]$smokeArgs.Add('-NonInteractive')
                [void]$smokeArgs.Add('-File')
                [void]$smokeArgs.Add([string]$found.path)
                $paramNames = @($found.contract.parameters | ForEach-Object { [string]$_.name })
                if ($paramNames -contains 'IsolatedProfile') {
                    [void]$smokeArgs.Add('-IsolatedProfile')
                    [void]$smokeArgs.Add((Join-Path $runDirectory ("{0}-profile" -f $group.id)))
                }
                $commands.Add((New-FinalReleaseCommandRecord -Id $smokeId -Kind 'smoke' -Executable $tools.pwsh -Arguments @($smokeArgs.ToArray())))
            }
        }

        if ($PlanOnly) {
            Write-Host 'PlanOnly: isolation/command discovery only; no cargo/npm/smoke/baseline execution.'
        }
        else {

        New-Item -ItemType Directory -Force -Path $cargoTargetDir | Out-Null
        New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargoTargetDir
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $tempDir

        $captureScript = Join-Path $PSScriptRoot 'Capture-ProductionBaseline.ps1'
        $assertScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'
        if (-not (Test-Path -LiteralPath $captureScript -PathType Leaf) -or -not (Test-Path -LiteralPath $assertScript -PathType Leaf)) {
            throw 'Capture-ProductionBaseline.ps1 or Assert-ProductionUnchanged.ps1 is missing.'
        }
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $captureScript
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $assertScript
        & $captureScript -OutputPath $baselinePath

        $longRust = @"
Long Rust verification is about to run under isolated CARGO_TARGET_DIR=$cargoTargetDir.
Expected Cargo, rustc, and test harness executables may appear only under that target.
The installed DevManager process and production config/remote files must remain untouched.
session.json may change and is never read or hashed.
"@
        Write-Warning $longRust
        Write-Host $longRust

        $timeouts = @{
            'cargo-check'             = 1200000
            'cargo-test-lib'          = 3600000
            'cargo-test-integration'  = 3600000
            'web-test'                = 900000
            'web-typecheck'           = 900000
            'web-build'               = 900000
            'workspace-smoke'         = 720000
            'provider-smoke'          = 180000
        }

        for ($index = 0; $index -lt $commands.Count; $index++) {
            $command = $commands[$index]
            if ([string]$command.status -ne 'planned') { continue }

            $timeoutMs = 600000
            if ($timeouts.ContainsKey([string]$command.id)) {
                $timeoutMs = [int]$timeouts[[string]$command.id]
            }
            elseif ([string]$command.id -like 'browser-smoke*' -or [string]$command.id -like 'prompt-smoke*') {
                $timeoutMs = 600000
            }

            $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
            try {
                $result = Invoke-FinalReleaseBoundedChild `
                    -Executable ([string]$command.executable) `
                    -Arguments ([string[]]$command.arguments) `
                    -WorkingDirectory $worktreeRoot `
                    -Environment $childEnv `
                    -TimeoutMilliseconds $timeoutMs
                $stopwatch.Stop()
                $logs = Write-FinalReleaseCommandLogs `
                    -LogDirectory $logDirectory `
                    -Id ([string]$command.id) `
                    -Stdout $result.Stdout `
                    -Stderr $result.Stderr `
                    -ProtectedProductionRoot $protectedRoot `
                    -AllowedEvidenceRoot $evidenceRoot
                $command.stdoutLog = [string]$logs.stdoutLog
                $command.stderrLog = [string]$logs.stderrLog
                $command.exitCode = [int]$result.ExitCode
                $command.durationMs = [int64]$stopwatch.ElapsedMilliseconds
                $command.residue = Get-FinalReleaseOwnedResidue -CargoTargetDir $cargoTargetDir -RunDirectory $runDirectory
                if (@($command.residue).Count -gt 0) {
                    $command.status = 'FAIL'
                    $command.reason = 'gate-owned-residue'
                    throw ("Gate-owned process residue remains after '{0}'." -f $command.id)
                }
                if ([string]$command.id -eq 'provider-smoke' -or [string]$command.id -like 'provider-smoke:*') {
                    $payload = $null
                    try { $payload = $result.Stdout | ConvertFrom-Json } catch { $payload = $null }
                    if ($null -eq $payload -or $null -eq $payload.PSObject.Properties['disposition']) {
                        $command.status = 'FAIL'
                        $command.reason = 'provider-smoke-unparseable'
                        throw 'Provider smoke did not emit a typed JSON disposition.'
                    }
                    $disposition = [string]$payload.disposition
                    if ($disposition -eq 'hold') {
                        $command.status = 'HOLD'
                        $command.reason = 'typed-hold'
                    }
                    elseif ($disposition -eq 'rejected' -or [int]$result.ExitCode -ne 0) {
                        $command.status = 'FAIL'
                        $command.reason = 'provider-smoke-rejected'
                        throw 'Provider smoke rejected the fixture arm.'
                    }
                    else {
                        $command.status = 'FAIL'
                        $command.reason = 'provider-smoke-unexpected-pass'
                        throw 'Provider smoke is not allowed to PASS; unexpected success is a contract failure.'
                    }
                }
                elseif ([int]$result.ExitCode -ne 0) {
                    $command.status = 'FAIL'
                    $command.reason = 'nonzero-exit'
                    throw ("Command '{0}' exited {1}." -f $command.id, $result.ExitCode)
                }
                else {
                    $command.status = 'PASS'
                }
            }
            catch {
                if ($stopwatch.IsRunning) { $stopwatch.Stop() }
                if ($null -eq $command.durationMs) { $command.durationMs = [int64]$stopwatch.ElapsedMilliseconds }
                if ([string]$command.status -eq 'planned') {
                    $command.status = 'FAIL'
                    $command.reason = [string]$_.Exception.Message
                }
                if ($null -eq $command.residue) {
                    try {
                        $command.residue = Get-FinalReleaseOwnedResidue -CargoTargetDir $cargoTargetDir -RunDirectory $runDirectory
                    }
                    catch {
                        $command.residue = [object[]]@()
                    }
                }
                throw
            }
        }
        }
    }
    catch {
        $gateError = [string]$_.Exception.Message
    }
}
finally {
    try {
        if (-not $environmentRestored) {
            Restore-FinalReleaseProcessEnvironment -Snapshot $originalEnvironment
            $environmentRestored = $true
        }
    }
    catch {
        if ([string]::IsNullOrWhiteSpace($gateError)) {
            $gateError = "Failed to restore parent environment: $($_.Exception.Message)"
        }
    }

    if (-not $PlanOnly -and -not [string]::IsNullOrWhiteSpace($baselinePath) -and (Test-Path -LiteralPath $baselinePath)) {
        try {
            $assertScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'
            & $assertScript -BaselinePath $baselinePath
            if ($productionAssert -ne 'failed') {
                $productionAssert = 'unchanged'
            }
        }
        catch {
            $productionAssert = 'failed'
            $productionAssertFailure = [string]$_.Exception.Message
            if ([string]::IsNullOrWhiteSpace($gateError)) {
                $gateError = $productionAssertFailure
            }
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($cargoTargetDir) -and -not [string]::IsNullOrWhiteSpace($runDirectory) -and (Test-Path -LiteralPath $runDirectory)) {
        try {
            $finalResidue = Get-FinalReleaseOwnedResidue -CargoTargetDir $cargoTargetDir -RunDirectory $runDirectory
        }
        catch {
            $finalResidue = [object[]]@()
            if ([string]::IsNullOrWhiteSpace($gateError)) {
                $gateError = "Final residue inspection failed: $($_.Exception.Message)"
            }
        }
    }

    $status = Resolve-FinalReleaseOverallStatus `
        -CommandRecords @($commands.ToArray()) `
        -IsPlanOnly ([bool]$PlanOnly) `
        -WebSkipped ([bool]$SkipWeb) `
        -SmokesSkipped ([bool]$SkipSmokes) `
        -ProductionAssert $productionAssert `
        -FinalResidue @($finalResidue) `
        -GateError $gateError `
        -EvidenceWriteFailed $false
    if (-not $PlanOnly -and $status -eq 'PASS' -and @($finalResidue).Count -gt 0) {
        $status = 'FAIL'
    }

    if (-not [string]::IsNullOrWhiteSpace($verificationPath) -and -not [string]::IsNullOrWhiteSpace($protectedRoot)) {
        try {
            $clearedNames = New-Object System.Collections.Generic.List[string]
            foreach ($name in @($originalEnvironment.Keys)) {
                if (Test-FinalReleaseRemovedEnvironmentName -Name $name) {
                    $clearedNames.Add($name)
                }
            }
            $evidence = [pscustomobject]@{
                schemaVersion            = [int]1
                capturedAtUtc            = [DateTime]::UtcNow.ToString('o')
                status                   = $status
                runId                    = $runIdValue
                repoRoot                 = $worktreeRoot
                planOnly                 = [bool]$PlanOnly
                skipWeb                  = [bool]$SkipWeb
                skipSmokes               = [bool]$SkipSmokes
                isolation                = [pscustomobject]@{
                    cargoTargetDir = $cargoTargetDir
                    runDirectory   = $runDirectory
                    tempDir        = $tempDir
                    evidenceRoot   = $evidenceRoot
                    productionRoot = $protectedRoot
                }
                clearedEnvironmentNames  = [string[]]$clearedNames.ToArray()
                baselinePath             = $baselinePath
                productionAssert         = $productionAssert
                productionAssertFailure  = $productionAssertFailure
                sessionJson              = 'path-only-never-read-or-hashed'
                commands                 = [object[]]$commands.ToArray()
                residueFinal             = [object[]]@($finalResidue)
                gateError                = $gateError
                replacesPublicRelease    = $false
            }
            Write-DevManagerJsonEvidence `
                -Value $evidence `
                -OutputPath $verificationPath `
                -ProtectedProductionRoot $protectedRoot `
                -AllowedEvidenceRoot $evidenceRoot
            Write-Host ("Wrote evidence -> {0}" -f $verificationPath)
            Write-Host ("status={0}" -f $status)
        }
        catch {
            $evidenceWriteFailed = $true
            if ([string]::IsNullOrWhiteSpace($gateError)) {
                $gateError = [string]$_.Exception.Message
            }
            Write-Host ("Failed to write evidence: {0}" -f $_.Exception.Message)
            $status = 'FAIL'
        }
    }
    elseif (-not $PlanOnly) {
        $evidenceWriteFailed = $true
        $status = 'FAIL'
    }

    if (-not $environmentRestored) {
        Restore-FinalReleaseProcessEnvironment -Snapshot $originalEnvironment
        $environmentRestored = $true
    }

    $finalStatus = Resolve-FinalReleaseOverallStatus `
        -CommandRecords @($commands.ToArray()) `
        -IsPlanOnly ([bool]$PlanOnly) `
        -WebSkipped ([bool]$SkipWeb) `
        -SmokesSkipped ([bool]$SkipSmokes) `
        -ProductionAssert $productionAssert `
        -FinalResidue @($finalResidue) `
        -GateError $gateError `
        -EvidenceWriteFailed ([bool]$evidenceWriteFailed)
    if ($evidenceWriteFailed) { $finalStatus = 'FAIL' }
    $script:FinalReleaseExitCode = Get-FinalReleaseExitCode -Status $finalStatus
    $script:FinalReleaseStatus = $finalStatus
}

if (-not [string]::IsNullOrWhiteSpace($gateError) -and $script:FinalReleaseStatus -eq 'FAIL') {
    Write-Host ("Final release gate failed: {0}" -f $gateError)
}
elseif ($script:FinalReleaseStatus -eq 'HOLD') {
    Write-Host 'Final release gate HOLD: a smoke is missing, unsafe, skipped, or returned a typed HOLD.'
}

exit $script:FinalReleaseExitCode
