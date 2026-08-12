# Task 4.11 fail-closed provider smoke skeleton.
# Fixture arm is the default. This revision never launches providers, Cargo,
# hook relays, or Job helpers. Missing session/journal/adapter/runtime
# dependencies emit a typed HOLD and exit 2. The script cannot PASS.
# Authenticated mode is admission-only: explicit opt-in, provider allowlist,
# isolated nonproduction profile, interactive operator; it still HOLDs.

[CmdletBinding()]
param(
    [switch]$Authenticated,
    [string[]]$Provider,
    [string]$IsolatedProfile,
    [switch]$IAcknowledgeIsolatedNonproductionProfile,
    [switch]$HostRegistered,
    [int]$DeadlineMs = 120000
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

$script:MaxDeadlineMs = 120000
$script:HoldExitCode = 2
$script:RejectExitCode = 1

function Get-ProviderSmokeExplicitEnvironment {
    return [ordered]@{
        DEVMANAGER_PROFILE        = 'native-next-dev'
        DEVMANAGER_INSTANCE_LABEL = 'Next'
        DEVMANAGER_RUNTIME_KIND   = 'native-next'
    }
}

function Get-ProviderSmokeClearedEnvironmentKeys {
    return [string[]]@(
        'DEVMANAGER_PROFILE',
        'DEVMANAGER_INSTANCE_LABEL',
        'DEVMANAGER_RUNTIME_KIND',
        'DEVMANAGER_CONFIG_DIR',
        'DEVMANAGER_APP_IDENTITY',
        'ANTHROPIC_API_KEY',
        'OPENAI_API_KEY',
        'CURSOR_API_KEY',
        'CLAUDE_API_KEY'
    )
}

function Test-ProviderSmokeCiOrNoninteractive {
    if (-not [Environment]::UserInteractive) {
        return $true
    }
    foreach ($name in @('CI', 'GITHUB_ACTIONS', 'TF_BUILD', 'BUILD_BUILDID')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace([string]$value)) {
            return $true
        }
    }
    return $false
}

function Resolve-ProviderSmokeAllowlist {
    param([string[]]$Names)

    $resolved = New-Object System.Collections.Generic.List[string]
    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($raw in @($Names)) {
        if ([string]::IsNullOrWhiteSpace([string]$raw)) {
            throw 'Provider allowlist entries must be non-empty.'
        }
        $kind = switch -Regex ([string]$raw.Trim()) {
            '^(?i)(claude|claude_code)$' { 'claude_code' }
            '^(?i)codex$' { 'codex' }
            '^(?i)cursor$' { 'cursor' }
            default {
                throw "Unknown provider allowlist entry '$raw'. Accepted: claude_code, codex, cursor."
            }
        }
        if (-not $seen.Add($kind)) {
            throw "Authenticated provider allowlist must not contain duplicates ('$kind')."
        }
        $resolved.Add($kind)
    }
    return ,([string[]]$resolved.ToArray())
}

function Test-ProviderSmokeProductionIdentityRoot {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $normalized = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $rendered = $normalized.Replace('\', '/')
    if ($rendered.Contains('com.userfirst.devmanager')) {
        return 'production-profile'
    }
    if ($rendered.Contains('/google/chrome/user data') -or $rendered.Contains('/microsoft/edge/user data')) {
        return 'production-browser-profile'
    }
    $parts = Get-DevManagerNormalizedPathComponents -LiteralPath $normalized
    foreach ($part in $parts) {
        if ($part -in @('.claude', '.codex', '.cursor')) {
            return 'production-browser-profile'
        }
    }
    return $null
}

function Get-ProviderSmokeDependencyHolds {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)

    $holds = New-Object System.Collections.Generic.List[object]
    $holds.Add([pscustomobject]@{
            id     = 'provider_smoke_fixture_runtime'
            reason = 'fixture-only smoke runtime is unimplemented; this skeleton cannot PASS'
        })

    $checks = @(
        @{ id = 'provider_runtime_session'; rel = 'src\providers\session.rs'; directory = $false; reason = 'src/providers/session.rs is absent; runtime generations are not launched here' },
        @{ id = 'provider_journal'; rel = 'src\providers\journal.rs'; directory = $false; reason = 'src/providers/journal.rs is absent; semantic persistence is not claimed' },
        @{ id = 'provider_sessions_compatibility_gate'; rel = 'tests\provider_sessions.rs'; directory = $false; reason = 'tests/provider_sessions.rs is absent; compatibility_ filters cannot run' },
        @{ id = 'phase2_conformance_artifact_runner'; rel = 'src\conformance'; directory = $true; reason = 'src/conformance is absent; this lab is not the Phase 2 manifest/trace runner' },
        @{ id = 'provider_claude_adapter'; rel = 'src\providers\claude.rs'; directory = $false; reason = 'src/providers/claude.rs is absent; stock Claude sessions are not launched here' },
        @{ id = 'provider_codex_adapter'; rel = 'src\providers\codex.rs'; directory = $false; reason = 'src/providers/codex.rs is absent; stock Codex sessions are not launched here' },
        @{ id = 'provider_cursor_adapter'; rel = 'src\providers\cursor.rs'; directory = $false; reason = 'src/providers/cursor.rs is absent; stock Cursor sessions are not launched here' }
    )

    foreach ($check in $checks) {
        $candidate = [System.IO.Path]::GetFullPath((Join-Path $WorktreeRoot ([string]$check.rel)))
        if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $candidate -AncestorPath $WorktreeRoot)) {
            throw "Smoke dependency path '$candidate' escapes worktree."
        }
        $present = if ([bool]$check.directory) {
            Test-Path -LiteralPath $candidate -PathType Container
        }
        else {
            Test-Path -LiteralPath $candidate -PathType Leaf
        }
        if (-not $present) {
            $holds.Add([pscustomobject]@{
                    id     = [string]$check.id
                    reason = [string]$check.reason
                })
        }
    }

    return ,([object[]]$holds.ToArray())
}

function Write-ProviderSmokeResult {
    param(
        [Parameter(Mandatory = $true)][string]$Disposition,
        [Parameter(Mandatory = $true)][string]$Arm,
        [object[]]$Holds,
        [string]$Rejection,
        [int]$DeadlineMs,
        [string[]]$Allowlist,
        [string]$IsolatedProfileRoot
    )

    $holdItems = @($Holds)
    $result = [pscustomobject]@{
        schemaVersion      = [int]1
        disposition        = $Disposition
        pass               = $false
        arm                = $Arm
        launchedProviders  = $false
        deadlineMs         = [int]$DeadlineMs
        providerAllowlist  = [string[]]@($Allowlist)
        isolatedProfile    = $IsolatedProfileRoot
        requiredEvidence   = [string[]]@(
            'executable',
            'version',
            'capabilities',
            'task_id',
            'agent_id',
            'generation',
            'action_id',
            'nonce'
        )
        invariants         = [pscustomobject]@{
            exactResumeFailureNeverFresh    = $true
            oneProviderRoot                 = $true
            onePtyReader                    = $true
            zeroJobListenerHelperResidue    = $true
        }
        holds              = [object[]]$holdItems
        rejection          = $Rejection
        explicitEnvironment = [pscustomobject](Get-ProviderSmokeExplicitEnvironment)
        clearedEnvironment  = [string[]](Get-ProviderSmokeClearedEnvironmentKeys)
    }

    $json = $result | ConvertTo-Json -Depth 6 -Compress
    if ($json.Length -gt 16384) {
        throw 'Provider smoke output exceeded the 16KiB bound.'
    }
    Write-Output $json
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$protectedRoot = Get-DevManagerProductionRoot
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktreeRoot

if ($DeadlineMs -le 0 -or $DeadlineMs -gt $script:MaxDeadlineMs) {
    Write-ProviderSmokeResult -Disposition 'rejected' -Arm $(if ($Authenticated) { 'authenticated' } else { 'fixture' }) `
        -Holds @() -Rejection 'deadline-out-of-bounds' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot ''
    exit $script:RejectExitCode
}

if ([string]::IsNullOrWhiteSpace($IsolatedProfile)) {
    $IsolatedProfile = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot '.devmanager-next\provider-smoke-profile'))
}
else {
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $IsolatedProfile)) {
        throw "IsolatedProfile must be a fully qualified path ('$IsolatedProfile')."
    }
    $IsolatedProfile = [System.IO.Path]::GetFullPath($IsolatedProfile.Trim())
}

if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $IsolatedProfile -AncestorPath $protectedRoot) {
    Write-ProviderSmokeResult -Disposition 'rejected' -Arm $(if ($Authenticated) { 'authenticated' } else { 'fixture' }) `
        -Holds @() -Rejection 'production-profile' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot $IsolatedProfile
    exit $script:RejectExitCode
}

$identityKind = Test-ProviderSmokeProductionIdentityRoot -LiteralPath $IsolatedProfile
if ($identityKind -eq 'production-profile') {
    Write-ProviderSmokeResult -Disposition 'rejected' -Arm $(if ($Authenticated) { 'authenticated' } else { 'fixture' }) `
        -Holds @() -Rejection 'production-profile' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot $IsolatedProfile
    exit $script:RejectExitCode
}
if ($identityKind -eq 'production-browser-profile') {
    Write-ProviderSmokeResult -Disposition 'rejected' -Arm $(if ($Authenticated) { 'authenticated' } else { 'fixture' }) `
        -Holds @() -Rejection 'production-browser-profile' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot $IsolatedProfile
    exit $script:RejectExitCode
}

$allowlist = [string[]]@()
$arm = 'fixture'
if ($Authenticated) {
    $arm = 'authenticated'
    if (-not $IAcknowledgeIsolatedNonproductionProfile) {
        Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
            -Rejection 'authenticated-without-opt-in' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot $IsolatedProfile
        exit $script:RejectExitCode
    }
    $allowlist = Resolve-ProviderSmokeAllowlist -Names $Provider
    if (@($allowlist).Count -eq 0) {
        Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
            -Rejection 'authenticated-without-allowlist' -DeadlineMs $DeadlineMs -Allowlist @() -IsolatedProfileRoot $IsolatedProfile
        exit $script:RejectExitCode
    }
    if (Test-ProviderSmokeCiOrNoninteractive) {
        Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
            -Rejection 'authenticated-in-ci-or-noninteractive' -DeadlineMs $DeadlineMs -Allowlist $allowlist -IsolatedProfileRoot $IsolatedProfile
        exit $script:RejectExitCode
    }
    if (-not $HostRegistered) {
        Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
            -Rejection 'authenticated-without-host-registration' -DeadlineMs $DeadlineMs -Allowlist $allowlist -IsolatedProfileRoot $IsolatedProfile
        exit $script:RejectExitCode
    }
    $sessionPresent = Test-Path -LiteralPath (Join-Path $worktreeRoot 'src\providers\session.rs') -PathType Leaf
    if (-not $sessionPresent) {
        Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
            -Rejection 'authenticated-without-host-registration' -DeadlineMs $DeadlineMs -Allowlist $allowlist -IsolatedProfileRoot $IsolatedProfile
        exit $script:RejectExitCode
    }
    # Fail closed: this skeleton never invents Supported capability evidence and does not probe.
    Write-ProviderSmokeResult -Disposition 'rejected' -Arm $arm -Holds @() `
        -Rejection 'authenticated-capability-unsupported' -DeadlineMs $DeadlineMs -Allowlist $allowlist -IsolatedProfileRoot $IsolatedProfile
    exit $script:RejectExitCode
}

$holds = Get-ProviderSmokeDependencyHolds -WorktreeRoot $worktreeRoot
Write-ProviderSmokeResult -Disposition 'hold' -Arm $arm -Holds $holds -Rejection $null `
    -DeadlineMs $DeadlineMs -Allowlist $allowlist -IsolatedProfileRoot $IsolatedProfile
exit $script:HoldExitCode
