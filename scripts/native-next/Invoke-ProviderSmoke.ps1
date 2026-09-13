# Phase 4 provider smoke/conformance runner.
# Default fixture mode validates committed provider matrices and command
# policy without spawning providers. Explicit live/probe mode may run only
# bounded read-only stock probes. This script never sends a user prompt,
# never starts an interactive provider session, and never reads or writes
# production DevManager config.

[CmdletBinding()]
param(
    [ValidateSet('fixture', 'live', 'probe')]
    [string]$Mode = 'fixture',
    [switch]$Live,
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
$script:MaxResultBytes = 16384
$script:MaxProbeOutputBytes = 65536
$script:MaxProbeTimeoutMs = 30000
$script:HoldExitCode = 2
$script:RejectExitCode = 1
$script:PassExitCode = 0
$script:ProhibitedLiveTokens = [string[]]@(
    'exec',
    '--print',
    '-p',
    'create-chat',
    '--continue',
    '--last'
)
$script:ClearedEnvironmentKeys = [string[]]@(
    'DEVMANAGER_PROFILE',
    'DEVMANAGER_INSTANCE_LABEL',
    'DEVMANAGER_RUNTIME_KIND',
    'DEVMANAGER_CONFIG_DIR',
    'DEVMANAGER_APP_IDENTITY',
    'ANTHROPIC_API_KEY',
    'ANTHROPIC_AUTH_TOKEN',
    'CLAUDE_CODE_OAUTH_TOKEN',
    'CLAUDE_API_KEY',
    'OPENAI_API_KEY',
    'CODEX_API_KEY',
    'CURSOR_API_KEY'
)
$script:SavedEnvironment = $null
$script:ResultWritten = $false

function Get-ProviderSmokeExplicitEnvironment {
    return [ordered]@{
        DEVMANAGER_PROFILE        = 'native-next-dev'
        DEVMANAGER_INSTANCE_LABEL = 'Next'
        DEVMANAGER_RUNTIME_KIND   = 'native-next'
    }
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
            throw 'allowlist-empty-entry'
        }
        $kind = switch -Regex ([string]$raw.Trim()) {
            '^(?i)(claude|claude_code)$' { 'claude_code' }
            '^(?i)codex$' { 'codex' }
            '^(?i)cursor$' { 'cursor' }
            default { throw "allowlist-unknown:$raw" }
        }
        if (-not $seen.Add($kind)) {
            throw "allowlist-duplicate:$kind"
        }
        $resolved.Add($kind)
    }
    return , ([string[]]$resolved.ToArray())
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

function ConvertTo-ProviderSmokeRedactedText {
    param([AllowNull()][string]$Text)

    if ([string]::IsNullOrEmpty($Text)) {
        return $Text
    }
    $redacted = $Text
    $redacted = [regex]::Replace($redacted, '(?i)sk-[a-z0-9_-]{8,}', '[redacted-credential]')
    $redacted = [regex]::Replace($redacted, '(?i)(api[_-]?key|token|password|secret|authorization|cookie|auth_token)\s*[:=]\s*\S+', '$1=[redacted-credential]')
    $redacted = [regex]::Replace($redacted, '(?i)sess_[a-z0-9_-]+', '[redacted-session]')
    $redacted = [regex]::Replace($redacted, '(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}', '[redacted-session]')
    $redacted = [regex]::Replace($redacted, '(?i)[A-Z]:/Users/[^/\s"]+', '[redacted-user-path]')
    $redacted = [regex]::Replace($redacted, '(?i)[A-Z]:\\Users\\[^\\\s"]+', '[redacted-user-path]')
    $redacted = [regex]::Replace($redacted, '(?i)/Users/[^/\s"]+', '[redacted-user-path]')
    $redacted = [regex]::Replace($redacted, '(?i)/home/[^/\s"]+', '[redacted-user-path]')
    $redacted = [regex]::Replace($redacted, '(?is)Please rewrite.{0,200}', '[redacted-prompt]')
    $redacted = [regex]::Replace($redacted, '(?is)Here is the rewritten.{0,200}', '[redacted-response]')
    return $redacted
}

function Test-ProviderSmokeProhibitedToken {
    param([string]$Token)

    if ([string]::IsNullOrWhiteSpace($Token) -or $Token -eq '<id>') {
        return $false
    }
    foreach ($forbidden in $script:ProhibitedLiveTokens) {
        if ([string]::Equals($Token, $forbidden, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Get-ProviderSmokeMatrix {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)

    $matrixPath = [System.IO.Path]::GetFullPath((Join-Path $WorktreeRoot 'tests\fixtures\providers\smoke\matrix.json'))
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $matrixPath -AncestorPath $WorktreeRoot)) {
        throw 'matrix-escapes-worktree'
    }
    if (-not (Test-Path -LiteralPath $matrixPath -PathType Leaf)) {
        throw 'matrix-missing'
    }
    $raw = Get-Content -LiteralPath $matrixPath -Raw -Encoding UTF8
    return ($raw | ConvertFrom-Json)
}

function Read-ProviderSmokeFixtureText {
    param(
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$RelativePath
    )

    if ([string]::IsNullOrWhiteSpace($RelativePath) -or $RelativePath.Contains('..')) {
        throw 'fixture-relative-invalid'
    }
    $full = [System.IO.Path]::GetFullPath((Join-Path $WorktreeRoot (Join-Path 'tests\fixtures\providers' $RelativePath)))
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $full -AncestorPath $WorktreeRoot)) {
        throw 'fixture-escapes-worktree'
    }
    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) {
        throw "fixture-missing:$RelativePath"
    }
    return [string](Get-Content -LiteralPath $full -Raw -Encoding UTF8)
}

function Get-ProviderSmokeClaudeAuthClass {
    param([string]$Raw)

    try {
        $value = $Raw | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        return 'unknown'
    }
    if ($null -eq $value.PSObject.Properties['loggedIn']) {
        return 'unknown'
    }
    if ($value.loggedIn -eq $false) {
        return 'auth_required'
    }
    if ($value.loggedIn -eq $true -and [string]$value.authMethod -eq 'claude.ai') {
        return 'authenticated_subscription'
    }
    return 'unknown'
}

function Get-ProviderSmokeCodexAuthClass {
    param([string]$Raw)

    $lines = @(
        $Raw -split "(`r`n|`n|`r)" |
            ForEach-Object { $_.Trim() } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    foreach ($line in $lines) {
        if ($line -in @('not authenticated', 'not logged in', 'auth required', 'login required')) {
            return 'auth_required'
        }
    }
    $method = $lines -contains 'Logged in using ChatGPT'
    $plan = $lines -contains 'ChatGPT Plus subscription'
    if ($method -and $plan) {
        return 'authenticated_subscription'
    }
    return 'unknown'
}

function Test-ProviderSmokeCodexExactResumeHelp {
    param([string]$Raw)

    foreach ($line in @($Raw -split "(`r`n|`n|`r)")) {
        $tokens = @($line.Split(@(' '), [System.StringSplitOptions]::RemoveEmptyEntries))
        if ($tokens.Count -eq 6 -and $tokens[0] -eq 'Usage:' -and $tokens[1] -eq 'codex' -and $tokens[2] -eq 'resume' -and $tokens[3] -eq '[OPTIONS]' -and $tokens[4] -eq '[SESSION_ID]' -and $tokens[5] -eq '[PROMPT]') {
            return $true
        }
    }
    return $false
}

function New-ProviderSmokeCheck {
    param(
        [string]$Id,
        [string]$Provider = '',
        [ValidateSet('pass', 'hold', 'fail', 'rejected')]
        [string]$Status,
        [string]$Detail = ''
    )

    return [pscustomobject]@{
        id       = [string]$Id
        provider = [string]$Provider
        status   = [string]$Status
        detail   = [string](ConvertTo-ProviderSmokeRedactedText -Text $Detail)
    }
}

function ConvertTo-ProviderSmokeResultJson {
    param([Parameter(Mandatory = $true)]$Result)

    $json = $Result | ConvertTo-Json -Depth 8 -Compress
    $json = ConvertTo-ProviderSmokeRedactedText -Text $json
    if ($json.Length -gt $script:MaxResultBytes) {
        throw 'result-too-large'
    }
    return $json
}

function ConvertTo-ProviderSmokeObjectArray {
    param($Value)

    if ($null -eq $Value) {
        return , ([object[]]@())
    }
    if ($Value -is [System.Collections.IEnumerable] -and -not ($Value -is [string])) {
        return , ([object[]]@($Value))
    }
    return , ([object[]]@($Value))
}

function Write-ProviderSmokeFinished {
    param(
        [Parameter(Mandatory = $true)][string]$Disposition,
        [Parameter(Mandatory = $true)][string]$ModeName,
        $Providers = @(),
        $Checks = @(),
        [int]$ResidueCount = 0,
        [string]$Rejection = '',
        [switch]$LaunchedProviders
    )

    if ($script:ResultWritten) {
        return
    }
    $script:ResultWritten = $true
    $rejectionValue = $null
    if (-not [string]::IsNullOrWhiteSpace($Rejection)) {
        $rejectionValue = [string]$Rejection
    }
    $result = [pscustomobject]@{
        schemaVersion     = [int]1
        mode              = [string]$ModeName
        providers         = ConvertTo-ProviderSmokeObjectArray -Value $Providers
        checks            = ConvertTo-ProviderSmokeObjectArray -Value $Checks
        launchedProviders = [bool]$LaunchedProviders
        residueCount      = [int]$ResidueCount
        disposition       = [string]$Disposition
        rejection         = $rejectionValue
    }
    Write-Output (ConvertTo-ProviderSmokeResultJson -Result $result)
}

function Enter-ProviderSmokeIsolatedEnvironment {
    param([string]$IsolatedProfileRoot)

    $script:SavedEnvironment = @{}
    foreach ($key in $script:ClearedEnvironmentKeys) {
        $script:SavedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, $null, 'Process')
    }
    $explicit = Get-ProviderSmokeExplicitEnvironment
    foreach ($entry in $explicit.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable([string]$entry.Key, [string]$entry.Value, 'Process')
    }
    if (-not [string]::IsNullOrWhiteSpace($IsolatedProfileRoot)) {
        [Environment]::SetEnvironmentVariable('DEVMANAGER_CONFIG_DIR', $IsolatedProfileRoot, 'Process')
    }
}

function Exit-ProviderSmokeIsolatedEnvironment {
    if ($null -eq $script:SavedEnvironment) {
        return
    }
    foreach ($key in $script:SavedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable([string]$key, $script:SavedEnvironment[$key], 'Process')
    }
    $script:SavedEnvironment = $null
}

function Resolve-ProviderSmokeMode {
    $liveRequested = [bool]($Live -or $Authenticated -or $Mode -in @('live', 'probe'))
    $explicitFixture = $Mode -eq 'fixture' -and -not $Live -and -not $Authenticated
    if ($liveRequested -and $Mode -eq 'fixture' -and ($Live -or $Authenticated)) {
        return 'live'
    }
    if ($liveRequested -and $explicitFixture) {
        return 'fixture'
    }
    if ($liveRequested) {
        return 'live'
    }
    return 'fixture'
}

function Get-OwnedProbeResidue {
    param([uint32]$ParentProcessId)

    $residue = New-Object System.Collections.Generic.List[uint32]
    try {
        $children = @(Get-CimInstance -ClassName Win32_Process -Filter "ParentProcessId=$ParentProcessId" -ErrorAction Stop)
    }
    catch {
        return , ([uint32[]]@())
    }
    foreach ($child in $children) {
        $residue.Add([uint32]$child.ProcessId)
    }
    return , ([uint32[]]$residue.ToArray())
}

function Stop-OwnedProbeTree {
    param($Process)

    if ($null -eq $Process) {
        return
    }
    try {
        if (-not $Process.HasExited) {
            $Process.Kill($true)
        }
    }
    catch {
    }
}

function Invoke-OwnedReadOnlyProbe {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$ArgumentList,
        [Parameter(Mandatory = $true)][int]$TimeoutMs,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory
    )

    foreach ($argument in $ArgumentList) {
        if (Test-ProviderSmokeProhibitedToken -Token $argument) {
            throw "live-prohibited-token:$argument"
        }
    }

    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $FilePath
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.CreateNoWindow = $true
    $info.WorkingDirectory = $WorkingDirectory
    if ($null -eq $info.ArgumentList) {
        throw 'probe-argumentlist-unavailable'
    }
    foreach ($argument in $ArgumentList) {
        [void]$info.ArgumentList.Add([string]$argument)
    }

    $process = $null
    $stdout = ''
    $stderr = ''
    $exitCode = $null
    $overflowed = $false
    $timedOut = $false
    $residue = [uint32[]]@()
    try {
        $process = [System.Diagnostics.Process]::Start($info)
        if ($null -eq $process) {
            throw 'probe-start-failed'
        }
        $process.StandardInput.Close()
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutMs)) {
            $timedOut = $true
            Stop-OwnedProbeTree -Process $process
            [void]$process.WaitForExit(2000)
        }
        if (-not $stdoutTask.Wait([Math]::Min(2000, $TimeoutMs))) {
            $overflowed = $true
        }
        if (-not $stderrTask.Wait([Math]::Min(2000, $TimeoutMs))) {
            $overflowed = $true
        }
        if ($stdoutTask.IsCompleted) {
            $stdout = [string]$stdoutTask.Result
        }
        if ($stderrTask.IsCompleted) {
            $stderr = [string]$stderrTask.Result
        }
        if ($stdout.Length -gt $script:MaxProbeOutputBytes -or $stderr.Length -gt $script:MaxProbeOutputBytes -or ($stdout.Length + $stderr.Length) -gt $script:MaxProbeOutputBytes) {
            $overflowed = $true
            $stdout = $stdout.Substring(0, [Math]::Min($stdout.Length, 64))
            $stderr = $stderr.Substring(0, [Math]::Min($stderr.Length, 64))
        }
        if ($process.HasExited) {
            $exitCode = [int]$process.ExitCode
        }
        $residue = Get-OwnedProbeResidue -ParentProcessId ([uint32]$process.Id)
        if (@($residue).Count -gt 0) {
            Stop-OwnedProbeTree -Process $process
            $residue = Get-OwnedProbeResidue -ParentProcessId ([uint32]$process.Id)
        }
    }
    finally {
        if ($null -ne $process) {
            $process.Dispose()
        }
    }

    return [pscustomobject]@{
        completed  = (-not $timedOut -and -not $overflowed -and $null -ne $exitCode)
        timedOut   = [bool]$timedOut
        overflowed = [bool]$overflowed
        exitCode   = $exitCode
        residue    = [int](@($residue).Count)
        stdout     = [string]$stdout
        stderr     = [string]$stderr
    }
}

function Resolve-StockProviderEntrypoint {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [string[]]$ForbiddenLeaves
    )

    $command = Get-Command -Name $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $command -or [string]::IsNullOrWhiteSpace([string]$command.Source)) {
        return $null
    }
    $path = [string]$command.Source
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $path)) {
        throw 'entrypoint-not-absolute'
    }
    $full = [System.IO.Path]::GetFullPath($path)
    $identity = Test-ProviderSmokeProductionIdentityRoot -LiteralPath $full
    if ($null -ne $identity) {
        throw $identity
    }
    $leaf = [System.IO.Path]::GetFileName($full)
    foreach ($forbidden in @($ForbiddenLeaves)) {
        if ([string]::Equals($leaf, $forbidden, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "entrypoint-forbidden:$leaf"
        }
    }
    $stem = [System.IO.Path]::GetFileNameWithoutExtension($leaf)
    if (-not [string]::Equals($stem, $Name, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "entrypoint-stem-mismatch:$leaf"
    }
    return $full
}

function Invoke-ProviderSmokeFixture {
    param(
        [Parameter(Mandatory = $true)]$Matrix,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot
    )

    $checks = New-Object System.Collections.Generic.List[object]
    $providers = New-Object System.Collections.Generic.List[object]
    $failed = $false

    if ([int]$Matrix.schemaVersion -ne 1 -or [string]$Matrix.kind -ne 'phase4.provider_smoke.matrix') {
        $checks.Add((New-ProviderSmokeCheck -Id 'matrix.schema' -Status fail -Detail 'unsupported matrix schema'))
        return [pscustomobject]@{ checks = $checks; providers = $providers; failed = $true; holds = $false }
    }
    $checks.Add((New-ProviderSmokeCheck -Id 'matrix.schema' -Status pass -Detail 'committed smoke matrix'))

    if ([bool]$Matrix.launchesProvider -ne $false -or [int]$Matrix.residueCount -ne 0) {
        $checks.Add((New-ProviderSmokeCheck -Id 'matrix.no_launch' -Status fail -Detail 'matrix must be no-launch'))
        $failed = $true
    }
    else {
        $checks.Add((New-ProviderSmokeCheck -Id 'matrix.no_launch' -Status pass -Detail 'fixture matrix launchesProvider=false residueCount=0'))
    }

    $probeKindArgv = @{
        version      = @('--version')
        help         = @('--help')
        auth_status  = @('auth', 'status')
        login_status = @('login', 'status')
        resume_help  = @('resume', '--help')
    }

    foreach ($provider in @($Matrix.providers)) {
        $id = [string]$provider.id
        $providerFailed = $false
        $probeSummaries = New-Object System.Collections.Generic.List[object]
        foreach ($probe in @($provider.probes)) {
            $kind = [string]$probe.kind
            $argv = @($probe.argv | ForEach-Object { [string]$_ })
            $expected = @($probeKindArgv[$kind])
            $argvMatch = ($argv.Count -eq $expected.Count)
            if ($argvMatch) {
                for ($index = 0; $index -lt $argv.Count; $index++) {
                    if ($argv[$index] -ne $expected[$index]) {
                        $argvMatch = $false
                    }
                }
            }
            $prohibited = $false
            foreach ($token in $argv) {
                if (Test-ProviderSmokeProhibitedToken -Token $token) {
                    $prohibited = $true
                }
            }
            if (-not $argvMatch -or $prohibited) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status fail -Detail 'probe argv mismatch or prohibited token'))
                $providerFailed = $true
                $failed = $true
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status pass -Detail ($argv -join ' ')))
            }
            $probeSummaries.Add([pscustomobject]@{
                    kind = $kind
                    argv = [string[]]$argv
                })
        }

        $freshForbidden = $true
        if ($id -eq 'claude_code') {
            $help = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.exactResume.helpFixture)
            $hasResume = $help.Split([char[]]@(' ', "`t", "`r", "`n"), [System.StringSplitOptions]::RemoveEmptyEntries) -contains '--resume'
            $failure = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath 'claude/resume_not_found.txt'
            $usesFallback = $failure.Contains('--continue') -or $failure.Contains('--last')
            $subscription = Get-ProviderSmokeClaudeAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.subscriptionFixture))
            $apiKey = Get-ProviderSmokeClaudeAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.apiKeyFixture))
            $ambiguous = Get-ProviderSmokeClaudeAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.ambiguousFixture))
            $required = Get-ProviderSmokeClaudeAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.requiredFixture))
            if (-not $hasResume -or $usesFallback) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status fail -Detail 'exact resume contract drifted'))
                $providerFailed = $true
                $failed = $true
                $freshForbidden = $false
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status pass -Detail '--resume <id>; failure stays visible'))
            }
            if ($subscription -ne 'authenticated_subscription' -or $apiKey -eq 'authenticated_subscription' -or $ambiguous -eq 'authenticated_subscription' -or $required -eq 'authenticated_subscription') {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status fail -Detail 'subscription policy drifted'))
                $providerFailed = $true
                $failed = $true
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status pass -Detail 'api-key/ambiguous stay unknown'))
            }
            $authSummary = [pscustomobject]@{
                subscriptionOnly           = $true
                apiKeyPromotedToSubscription = $false
                ambiguousPromoted            = $false
            }
        }
        elseif ($id -eq 'codex') {
            $help = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.exactResume.helpFixture)
            $unproven = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.exactResume.unprovenFixture)
            $exact = Test-ProviderSmokeCodexExactResumeHelp -Raw $help
            $lastOnlyProvesExact = Test-ProviderSmokeCodexExactResumeHelp -Raw $unproven
            $subscription = Get-ProviderSmokeCodexAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.subscriptionFixture))
            $apiKey = Get-ProviderSmokeCodexAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.apiKeyFixture))
            $ambiguous = Get-ProviderSmokeCodexAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.ambiguousFixture))
            $required = Get-ProviderSmokeCodexAuthClass -Raw (Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.requiredFixture))
            if (-not $exact -or $lastOnlyProvesExact) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status fail -Detail 'exact resume must require resume <id>'))
                $providerFailed = $true
                $failed = $true
                $freshForbidden = $false
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status pass -Detail 'resume <id>; --last is not exact resume'))
            }
            if ($subscription -ne 'authenticated_subscription' -or $apiKey -eq 'authenticated_subscription' -or $ambiguous -eq 'authenticated_subscription' -or $required -eq 'authenticated_subscription') {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status fail -Detail 'subscription policy drifted'))
                $providerFailed = $true
                $failed = $true
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status pass -Detail 'api-key/ambiguous stay unknown'))
            }
            $authSummary = [pscustomobject]@{
                subscriptionOnly             = $true
                apiKeyPromotedToSubscription = $false
                ambiguousPromoted            = $false
            }
        }
        else {
            $contractRaw = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath ([string]$provider.auth.contractFixture)
            $contract = $contractRaw | ConvertFrom-Json
            $resumeUnsupported = [string]$contract.capabilities.exact_resume -eq 'Unsupported' -and [bool]$contract.assertions.no_fresh_conversation
            $authUnknown = [string]$contract.capabilities.auth_state -eq 'Unknown' -and [bool]$contract.claims_auth -eq $false
            $quotaUnsupported = [string]$contract.capabilities.observe_quota -eq 'Unsupported'
            if (-not $resumeUnsupported) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status fail -Detail 'cursor exact resume must stay unsupported'))
                $providerFailed = $true
                $failed = $true
                $freshForbidden = $false
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.exact_resume" -Provider $id -Status pass -Detail 'UnsupportedCapability(ExactResume); no fresh fallback'))
            }
            if (-not $authUnknown) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status fail -Detail 'cursor must not claim subscription auth'))
                $providerFailed = $true
                $failed = $true
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.auth" -Provider $id -Status pass -Detail 'auth_state unknown; claims_auth false'))
            }
            if (-not $quotaUnsupported) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.quota" -Provider $id -Status fail -Detail 'cursor quota must stay unsupported'))
                $providerFailed = $true
                $failed = $true
            }
            $authSummary = [pscustomobject]@{
                supported                    = $false
                state                        = 'unknown'
                apiKeyPromotedToSubscription = $false
            }
        }

        $quotaRecord = [string]$provider.quota.record
        if ($quotaRecord -ne 'unsupported' -or [bool]$provider.quota.officialProbe) {
            $checks.Add((New-ProviderSmokeCheck -Id "$id.quota" -Provider $id -Status fail -Detail 'quota must stay unsupported without official probe'))
            $providerFailed = $true
            $failed = $true
        }
        elseif ($id -ne 'cursor' -or -not $providerFailed) {
            $existingQuota = @($checks | Where-Object { $_.id -eq "$id.quota" })
            if ($existingQuota.Count -eq 0) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.quota" -Provider $id -Status pass -Detail 'unsupported; never scraped'))
            }
        }

        $providers.Add([pscustomobject]@{
                id          = $id
                launched    = $false
                probes      = [object[]]$probeSummaries.ToArray()
                exactResume = [pscustomobject]@{
                    failureVisible = $true
                    freshFallback  = (-not $freshForbidden)
                }
                auth        = $authSummary
                quota       = 'unsupported'
                failed      = [bool]$providerFailed
            })
    }

    $redactionPath = Join-Path $WorktreeRoot 'tests\fixtures\providers\smoke\redaction.json'
    $redaction = Read-ProviderSmokeFixtureText -WorktreeRoot $WorktreeRoot -RelativePath 'smoke/redaction.json'
    if ($redaction.Contains('sk-ant-api03-fixture-secret') -and (Test-Path -LiteralPath $redactionPath)) {
        $checks.Add((New-ProviderSmokeCheck -Id 'redaction.samples' -Status pass -Detail 'redaction samples stay in fixture files only'))
    }
    else {
        $checks.Add((New-ProviderSmokeCheck -Id 'redaction.samples' -Status fail -Detail 'redaction fixture missing'))
        $failed = $true
    }

    return [pscustomobject]@{
        checks    = [object[]]$checks.ToArray()
        providers = [object[]]$providers.ToArray()
        failed    = [bool]$failed
        holds     = $false
    }
}

function Invoke-ProviderSmokeLive {
    param(
        [Parameter(Mandatory = $true)]$Matrix,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string[]]$Allowlist,
        [Parameter(Mandatory = $true)][string]$IsolatedProfileRoot,
        [Parameter(Mandatory = $true)][int]$DeadlineMs
    )

    $checks = New-Object System.Collections.Generic.List[object]
    $providers = New-Object System.Collections.Generic.List[object]
    $failed = $false
    $holds = $false
    $residueCount = 0
    $deadline = [DateTime]::UtcNow.AddMilliseconds($DeadlineMs)
    $workDir = $IsolatedProfileRoot
    if (-not (Test-Path -LiteralPath $workDir -PathType Container)) {
        New-Item -ItemType Directory -Path $workDir -Force | Out-Null
    }

    $byId = @{}
    foreach ($provider in @($Matrix.providers)) {
        $byId[[string]$provider.id] = $provider
    }

    foreach ($id in $Allowlist) {
        if (-not $byId.ContainsKey($id)) {
            $checks.Add((New-ProviderSmokeCheck -Id "$id.allowlist" -Provider $id -Status rejected -Detail 'provider is not in the committed matrix'))
            $failed = $true
            continue
        }
        $provider = $byId[$id]
        $entry = [string]@($provider.entrypoints)[0]
        $forbidden = @($provider.forbiddenEntrypoints | ForEach-Object { [string]$_ })
        $resolved = $null
        try {
            $resolved = Resolve-StockProviderEntrypoint -Name $entry -ForbiddenLeaves $forbidden
        }
        catch {
            $checks.Add((New-ProviderSmokeCheck -Id "$id.entrypoint" -Provider $id -Status rejected -Detail $_.Exception.Message))
            $failed = $true
            continue
        }
        if ([string]::IsNullOrWhiteSpace($resolved)) {
            $checks.Add((New-ProviderSmokeCheck -Id "$id.entrypoint" -Provider $id -Status hold -Detail "optional CLI '$entry' is not on PATH"))
            $holds = $true
            $providers.Add([pscustomobject]@{
                    id       = $id
                    launched = $false
                    quota    = 'unsupported'
                    hold     = 'missing-cli'
                })
            continue
        }

        $probeSummaries = New-Object System.Collections.Generic.List[object]
        foreach ($probe in @($provider.probes)) {
            $kind = [string]$probe.kind
            if (@($provider.liveProbes) -notcontains $kind) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status hold -Detail 'unsupported live capability'))
                $holds = $true
                continue
            }
            $argv = @($probe.argv | ForEach-Object { [string]$_ })
            $remaining = [int][Math]::Max(1, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
            $timeout = [Math]::Min($script:MaxProbeTimeoutMs, $remaining)
            if ($timeout -le 0) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status fail -Detail 'deadline exhausted'))
                $failed = $true
                break
            }
            $probeResult = Invoke-OwnedReadOnlyProbe -FilePath $resolved -ArgumentList $argv -TimeoutMs $timeout -WorkingDirectory $workDir
            $residueCount += [int]$probeResult.residue
            if ([int]$probeResult.residue -ne 0) {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind.residue" -Provider $id -Status fail -Detail 'probe left residue'))
                $failed = $true
            }
            if ($probeResult.timedOut -or $probeResult.overflowed -or -not $probeResult.completed -or $probeResult.exitCode -ne 0) {
                $status = 'fail'
                $detail = 'probe failed'
                if ($probeResult.timedOut) { $detail = 'probe timed out' }
                elseif ($probeResult.overflowed) { $detail = 'probe output exceeded bound' }
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status $status -Detail $detail))
                $failed = $true
            }
            else {
                $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status pass -Detail 'bounded read-only probe completed'))
            }
            $probeSummaries.Add([pscustomobject]@{
                    kind      = $kind
                    argv      = [string[]]$argv
                    completed = [bool]$probeResult.completed
                })
        }

        $unsupportedLive = @()
        if ($null -ne $provider.PSObject.Properties['unsupportedLiveProbes'] -and $null -ne $provider.unsupportedLiveProbes) {
            $unsupportedLive = @($provider.unsupportedLiveProbes)
        }
        foreach ($kind in $unsupportedLive) {
            $checks.Add((New-ProviderSmokeCheck -Id "$id.probe.$kind" -Provider $id -Status hold -Detail 'unsupported live capability; not invoked'))
            $holds = $true
        }

        $checks.Add((New-ProviderSmokeCheck -Id "$id.quota" -Provider $id -Status pass -Detail 'unsupported; never scraped'))
        $providers.Add([pscustomobject]@{
                id       = $id
                launched = $false
                probes   = [object[]]$probeSummaries.ToArray()
                quota    = 'unsupported'
            })
    }

    return [pscustomobject]@{
        checks       = [object[]]$checks.ToArray()
        providers    = [object[]]$providers.ToArray()
        failed       = [bool]$failed
        holds        = [bool]$holds
        residueCount = [int]$residueCount
    }
}

$worktreeRoot = $null
$isolatedProfileRoot = $null
try {
    $worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
    $protectedRoot = Get-DevManagerProductionRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktreeRoot
    $resolvedMode = Resolve-ProviderSmokeMode
    $null = $HostRegistered

    if ($DeadlineMs -le 0 -or $DeadlineMs -gt $script:MaxDeadlineMs) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'deadline' -Status rejected -Detail 'deadline-out-of-bounds')
        ) -Rejection 'deadline-out-of-bounds'
        exit $script:RejectExitCode
    }

    $defaultProfile = [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot '.devmanager-cutover-provider-smoke\profile'))
    if ([string]::IsNullOrWhiteSpace($IsolatedProfile)) {
        $isolatedProfileRoot = $defaultProfile
    }
    else {
        $trimmed = $IsolatedProfile.Trim()
        if (-not (Test-DevManagerAbsolutePath -LiteralPath $trimmed)) {
            Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
                (New-ProviderSmokeCheck -Id 'profile.relative' -Status rejected -Detail 'relative-or-ambiguous-profile')
            ) -Rejection 'relative-or-ambiguous-profile'
            exit $script:RejectExitCode
        }
        $isolatedProfileRoot = [System.IO.Path]::GetFullPath($trimmed)
    }

    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $isolatedProfileRoot -AncestorPath $protectedRoot) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'profile.production' -Status rejected -Detail 'production-profile')
        ) -Rejection 'production-profile'
        exit $script:RejectExitCode
    }
    $identityKind = Test-ProviderSmokeProductionIdentityRoot -LiteralPath $isolatedProfileRoot
    if ($identityKind -eq 'production-profile') {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'profile.production' -Status rejected -Detail 'production-profile')
        ) -Rejection 'production-profile'
        exit $script:RejectExitCode
    }
    if ($identityKind -eq 'production-browser-profile') {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'profile.browser' -Status rejected -Detail 'production-browser-profile')
        ) -Rejection 'production-browser-profile'
        exit $script:RejectExitCode
    }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $isolatedProfileRoot -AncestorPath $worktreeRoot)) {
        if ($resolvedMode -ne 'live' -or -not $IAcknowledgeIsolatedNonproductionProfile) {
            Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $resolvedMode -Providers @() -Checks @(
                (New-ProviderSmokeCheck -Id 'profile.ack' -Status rejected -Detail 'isolated profile outside worktree requires live opt-in')
            ) -Rejection 'authenticated-without-opt-in'
            exit $script:RejectExitCode
        }
    }

    Enter-ProviderSmokeIsolatedEnvironment -IsolatedProfileRoot $isolatedProfileRoot
    $matrix = Get-ProviderSmokeMatrix -WorktreeRoot $worktreeRoot

    if ($resolvedMode -eq 'fixture') {
        $outcome = Invoke-ProviderSmokeFixture -Matrix $matrix -WorktreeRoot $worktreeRoot
        if ([bool]$outcome.failed) {
            Write-ProviderSmokeFinished -Disposition 'failed' -ModeName 'fixture' -Providers @($outcome.providers) -Checks @($outcome.checks) -ResidueCount 0
            exit $script:RejectExitCode
        }
        Write-ProviderSmokeFinished -Disposition 'pass' -ModeName 'fixture' -Providers @($outcome.providers) -Checks @($outcome.checks) -ResidueCount 0
        exit $script:PassExitCode
    }

    if (-not $IAcknowledgeIsolatedNonproductionProfile) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName 'live' -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'live.opt_in' -Status rejected -Detail 'authenticated-without-opt-in')
        ) -Rejection 'authenticated-without-opt-in'
        exit $script:RejectExitCode
    }
    $allowlist = @()
    try {
        $allowlist = Resolve-ProviderSmokeAllowlist -Names $Provider
    }
    catch {
        $reason = 'authenticated-without-allowlist'
        if ("$($_.Exception.Message)".StartsWith('allowlist-unknown')) {
            $reason = 'allowlist-unknown'
        }
        elseif ("$($_.Exception.Message)".StartsWith('allowlist-duplicate')) {
            $reason = 'authenticated-duplicate-allowlist'
        }
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName 'live' -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'live.allowlist' -Status rejected -Detail $reason)
        ) -Rejection $reason
        exit $script:RejectExitCode
    }
    if (@($allowlist).Count -eq 0) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName 'live' -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'live.allowlist' -Status rejected -Detail 'authenticated-without-allowlist')
        ) -Rejection 'authenticated-without-allowlist'
        exit $script:RejectExitCode
    }
    if (Test-ProviderSmokeCiOrNoninteractive) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName 'live' -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'live.ci' -Status rejected -Detail 'authenticated-in-ci-or-noninteractive')
        ) -Rejection 'authenticated-in-ci-or-noninteractive'
        exit $script:RejectExitCode
    }

    $outcome = Invoke-ProviderSmokeLive -Matrix $matrix -WorktreeRoot $worktreeRoot -Allowlist $allowlist -IsolatedProfileRoot $isolatedProfileRoot -DeadlineMs $DeadlineMs
    if ([bool]$outcome.failed) {
        Write-ProviderSmokeFinished -Disposition 'failed' -ModeName 'live' -Providers @($outcome.providers) -Checks @($outcome.checks) -ResidueCount ([int]$outcome.residueCount)
        exit $script:RejectExitCode
    }
    if ([bool]$outcome.holds) {
        Write-ProviderSmokeFinished -Disposition 'hold' -ModeName 'live' -Providers @($outcome.providers) -Checks @($outcome.checks) -ResidueCount ([int]$outcome.residueCount)
        exit $script:HoldExitCode
    }
    Write-ProviderSmokeFinished -Disposition 'pass' -ModeName 'live' -Providers @($outcome.providers) -Checks @($outcome.checks) -ResidueCount ([int]$outcome.residueCount)
    exit $script:PassExitCode
}
catch {
    $detail = ConvertTo-ProviderSmokeRedactedText -Text $_.Exception.Message
    if (-not $script:ResultWritten) {
        Write-ProviderSmokeFinished -Disposition 'rejected' -ModeName $(if ($Live -or $Authenticated) { 'live' } else { $Mode }) -Providers @() -Checks @(
            (New-ProviderSmokeCheck -Id 'runner' -Status rejected -Detail $detail)
        ) -Rejection 'runner-error'
    }
    exit $script:RejectExitCode
}
finally {
    Exit-ProviderSmokeIsolatedEnvironment
}
