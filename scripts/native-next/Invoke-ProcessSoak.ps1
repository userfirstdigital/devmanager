# Phase 3.10 process soak entrypoint.
#
# The manifest is an immutable protocol input.  PowerShell does not open,
# parse, hash, or publish it; the fixed Rust supervisor owns those operations
# under a retained no-reparse evidence root.  PowerShell only starts that
# exact helper with a minimal environment and validates its one JSON result.

[CmdletBinding()]
param(
    [ValidateRange(1, 100)]
    [int]$Iterations = 100,

    [ValidateRange(0, [int]::MaxValue)]
    [int]$Seed = 3403,

    [switch]$SyntheticOnly,

    [AllowNull()][string]$HostExecutable,
    [AllowNull()][string]$HostSha256,
    [AllowNull()][string]$ClientExecutable,
    [AllowNull()][string]$ClientSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

$phase = 'phase-03-process-soak'
$schemaVersion = 1
$supervisorDeadlineMilliseconds = 600000
$stdoutByteCap = 256KB
$stderrByteCap = 64KB

function New-SoakSummary {
    param(
        [Parameter(Mandatory = $true)][string]$Status,
        [Parameter(Mandatory = $true)][bool]$Launched,
        [AllowNull()][string]$Error,
        [AllowNull()][object]$Supervisor,
        [AllowNull()][string]$RunId,
        [AllowNull()][string]$RunDirectory
    )

    [pscustomobject][ordered]@{
        schemaVersion = $schemaVersion
        phase = $phase
        status = $Status
        launched = $Launched
        error = ConvertTo-BoundedError $Error
        supervisor = ConvertTo-RedactedBoundedValue $Supervisor
        isolation = [ordered]@{
            profile = 'native-next-dev'
            runtimeKind = 'native-next'
            instanceLabel = 'Next'
            productionGuard = 'phase-gate-capture-and-assert'
        }
        runId = $RunId
        runDirectory = $RunDirectory
    }
}

function ConvertTo-BoundedError {
    param([AllowNull()][object]$Value)
    if ($null -eq $Value) { return $null }
    $text = [string]$Value
    $text = [regex]::Replace($text, '(?i)(password|token|secret|api[_-]?key|private[_-]?key)\s*[:=]\s*[^\s,;]+', '$1=<redacted>')
    $text = [regex]::Replace($text, '(?i)([A-Z]:[\\/][^\s"'']+)', '<path>')
    if ($text.Length -gt 512) { $text = $text.Substring(0, 512) }
    [regex]::Replace($text, '[\x00-\x1f\x7f]', '_')
}

function ConvertTo-RedactedBoundedValue {
    param(
        [AllowNull()][object]$Value,
        [int]$Depth = 0
    )
    if ($null -eq $Value) { return $null }
    if ($Depth -ge 8) { return '<redacted-depth>' }
    if ($Value -is [string]) { return ConvertTo-BoundedError $Value }
    if ($Value -is [ValueType]) { return $Value }
    if ($Value -is [System.Collections.IDictionary]) {
        $object = [ordered]@{}
        $count = 0
        foreach ($key in $Value.Keys) {
            if ($count++ -ge 128) { break }
            $name = [string]$key
            if ($name -match '(?i)(password|token|secret|api[_-]?key|private[_-]?key)$') {
                $object[$name] = '<redacted>'
            }
            else {
                $object[$name] = ConvertTo-RedactedBoundedValue -Value $Value[$key] -Depth ($Depth + 1)
            }
        }
        return [pscustomobject]$object
    }
    if ($Value -is [System.Collections.IEnumerable] -and -not ($Value -is [string])) {
        $items = [System.Collections.Generic.List[object]]::new()
        foreach ($item in $Value) {
            if ($items.Count -ge 128) { break }
            [void]$items.Add((ConvertTo-RedactedBoundedValue -Value $item -Depth ($Depth + 1)))
        }
        return $items.ToArray()
    }
    $properties = @($Value.PSObject.Properties)
    if ($properties.Count -gt 0) {
        $object = [ordered]@{}
        foreach ($property in ($properties | Select-Object -First 128)) {
            $name = [string]$property.Name
            if ($name -match '(?i)(password|token|secret|api[_-]?key|private[_-]?key)$') {
                $object[$name] = '<redacted>'
            }
            else {
                $object[$name] = ConvertTo-RedactedBoundedValue -Value $property.Value -Depth ($Depth + 1)
            }
        }
        return [pscustomobject]$object
    }
    ConvertTo-BoundedError ([string]$Value)
}

function Get-ExternalSha256 {
    param([Parameter(Mandatory = $true)][string]$Path)
    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    $stream = $null
    try {
        $stream = [IO.File]::OpenRead($Path)
        $buffer = New-Object byte[] 65536
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $hasher.AppendData($buffer, 0, $count)
        }
        return ([BitConverter]::ToString($hasher.GetHashAndReset()) -replace '-', '').ToLowerInvariant()
    }
    finally {
        if ($null -ne $stream) { $stream.Dispose() }
        $hasher.Dispose()
    }
}

function Resolve-CallerPinnedExecutable {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ExpectedSha256,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot
    )
    if ([string]::IsNullOrWhiteSpace($Path) -or [string]::IsNullOrWhiteSpace($ExpectedSha256)) {
        throw "$Label caller pin requires both a canonical executable path and SHA-256."
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "$Label caller-pinned executable is absent." }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $Path
    $canonical = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Path -ErrorAction Stop).Path)
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $canonical -AncestorPath $WorktreeRoot)) {
        throw "$Label caller-pinned executable escapes the worktree."
    }
    $actual = Get-ExternalSha256 -Path $canonical
    if ($actual -cne $ExpectedSha256.Trim().ToLowerInvariant()) {
        throw "$Label caller-pinned SHA-256 mismatch."
    }
    [pscustomobject]@{ path = $canonical; sha256 = $actual }
}

function Resolve-ExternalGitDirectory {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $marker = Join-Path $WorktreeRoot '.git'
    if (Test-Path -LiteralPath $marker -PathType Container) { return $marker }
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) { throw 'external HEAD attestation requires .git metadata.' }
    $line = [IO.File]::ReadAllText($marker).Trim()
    if (-not $line.StartsWith('gitdir:', [StringComparison]::OrdinalIgnoreCase)) { throw 'worktree .git marker is malformed.' }
    $value = $line.Substring(7).Trim()
    if ([IO.Path]::IsPathRooted($value)) { return [IO.Path]::GetFullPath($value) }
    [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $marker) $value))
}

function Get-ExternalGitRevision {
    param([Parameter(Mandatory = $true)][string]$WorktreeRoot)
    $gitDirectory = Resolve-ExternalGitDirectory -WorktreeRoot $WorktreeRoot
    $head = [IO.File]::ReadAllText((Join-Path $gitDirectory 'HEAD')).Trim()
    if ($head.StartsWith('ref: ', [StringComparison]::Ordinal)) {
        $reference = $head.Substring(5).Trim()
        $direct = @(
            (Join-Path $gitDirectory $reference),
            (Join-Path (Join-Path $gitDirectory ([IO.File]::ReadAllText((Join-Path $gitDirectory 'commondir')).Trim())) $reference)
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
        if ($null -ne $direct) { return [IO.File]::ReadAllText($direct).Trim().ToLowerInvariant() }
        $packed = @(
            (Join-Path $gitDirectory 'packed-refs'),
            (Join-Path (Join-Path $gitDirectory ([IO.File]::ReadAllText((Join-Path $gitDirectory 'commondir')).Trim())) 'packed-refs')
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
        if ($null -eq $packed) { throw "git reference '$reference' is not present." }
        foreach ($line in [IO.File]::ReadAllLines($packed)) {
            $parts = $line -split '\s+'
            if ($parts.Count -ge 2 -and $parts[1] -eq $reference) { return $parts[0].ToLowerInvariant() }
        }
        throw "git reference '$reference' is not present."
    }
    $head.ToLowerInvariant()
}

function Get-ExternalSourceTreeState {
    param(
        [Parameter(Mandatory = $true)][string]$WorktreeRoot
    )
    $root = [IO.Path]::GetFullPath($WorktreeRoot)
    $files = [System.Collections.Generic.List[object]]::new()
    $totalBytes = [int64]0
    function Add-ExternalSourceTreeEntries {
        param(
            [Parameter(Mandatory = $true)][string]$RootPath,
            [Parameter(Mandatory = $true)][string]$CurrentPath,
            [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[object]]$Entries,
            [Parameter(Mandatory = $true)][ref]$Bytes
        )
        foreach ($entry in Get-ChildItem -LiteralPath $CurrentPath -Force -ErrorAction Stop) {
            $name = [string]$entry.Name
            if ($name -eq '.git' -or $name -eq '.devmanager-next' -or $name -eq 'target' -or
                $name -eq 'target-native-next' -or $name.StartsWith('.tmp', [StringComparison]::Ordinal)) {
                continue
            }
            if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                $relative = [IO.Path]::GetRelativePath($RootPath, $entry.FullName).Replace('\', '/')
                throw "source tree contains a reparse point: $relative"
            }
            if ($entry.PSIsContainer) {
                Add-ExternalSourceTreeEntries -RootPath $RootPath -CurrentPath $entry.FullName -Entries $Entries -Bytes $Bytes
                continue
            }
            if ($Entries.Count -ge 20000) { throw 'source tree file count exceeds bound.' }
            $nextBytes = $Bytes.Value + [int64]$entry.Length
            if ($nextBytes -gt 128MB) { throw 'source tree bytes exceed bound.' }
            $Bytes.Value = $nextBytes
            $relative = [IO.Path]::GetRelativePath($RootPath, $entry.FullName).Replace('\', '/')
            [void]$Entries.Add([pscustomobject]@{ relative = $relative; full = $entry.FullName; bytes = $entry.Length })
        }
    }
    Add-ExternalSourceTreeEntries -RootPath $root -CurrentPath $root -Entries $files -Bytes ([ref]$totalBytes)
    $files.Sort([System.Collections.Generic.Comparer[object]]::Create([Comparison[object]]{
            param($left, $right)
            [String]::Compare([string]$left.relative, [string]$right.relative, [StringComparison]::Ordinal)
        }))
    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    try {
        foreach ($file in $files) {
            $hasher.AppendData([Text.Encoding]::UTF8.GetBytes([string]$file.relative))
            $hasher.AppendData([byte[]](0))
            $stream = [IO.File]::OpenRead([string]$file.full)
            try {
                $buffer = New-Object byte[] 65536
                while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) { $hasher.AppendData($buffer, 0, $count) }
            }
            finally { $stream.Dispose() }
        }
        return 'sha256:' + (([BitConverter]::ToString($hasher.GetHashAndReset()) -replace '-', '').ToLowerInvariant())
    }
    finally { $hasher.Dispose() }
}

function Invoke-BoundedRustSupervisor {
    param(
        [Parameter(Mandatory = $true)][string]$SupervisorPath,
        [Parameter(Mandatory = $true)][string]$Manifest,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$TempDirectory,
        [Parameter(Mandatory = $true)][bool]$HasIterations,
        [Parameter(Mandatory = $true)][bool]$HasSeed,
        [Parameter(Mandatory = $true)][object]$Attestation
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $SupervisorPath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    # Never inherit the caller's profile, credentials, proxy, or tool state.
    $startInfo.Environment.Clear()
    $systemRoot = [System.Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) { throw 'SystemRoot is unavailable.' }
    $pathDirectories = @(
        (Join-Path $systemRoot 'System32'),
        (Split-Path -Parent $SupervisorPath)
    )
    $startInfo.Environment['SystemRoot'] = $systemRoot
    $startInfo.Environment['TEMP'] = $TempDirectory
    $startInfo.Environment['TMP'] = $TempDirectory
    $startInfo.Environment['PATH'] = ($pathDirectories -join ';')
    $startInfo.Environment['DEVMANAGER_PROFILE'] = 'native-next-dev'
    $startInfo.Environment['DEVMANAGER_INSTANCE_LABEL'] = 'Next'
    $startInfo.Environment['DEVMANAGER_RUNTIME_KIND'] = 'native-next'
    [void]$startInfo.ArgumentList.Add('bounded-supervise')
    [void]$startInfo.ArgumentList.Add('--manifest')
    [void]$startInfo.ArgumentList.Add($Manifest)
    [void]$startInfo.ArgumentList.Add('--timeout-ms')
    [void]$startInfo.ArgumentList.Add([string]($supervisorDeadlineMilliseconds - 5000))
    if ($SyntheticOnly) {
        [void]$startInfo.ArgumentList.Add('--synthetic')
    }
    if ($HasIterations) {
        [void]$startInfo.ArgumentList.Add('--iterations')
        [void]$startInfo.ArgumentList.Add([string]$Iterations)
    }
    if ($HasSeed) {
        [void]$startInfo.ArgumentList.Add('--seed')
        [void]$startInfo.ArgumentList.Add([string]$Seed)
    }
    foreach ($pair in @(
            @('--expected-git-revision', [string]$Attestation.gitRevision),
            @('--expected-source-tree-state', [string]$Attestation.sourceTreeState),
            @('--expected-build-id', [string]$Attestation.buildId),
            @('--expected-helper-sha256', [string]$Attestation.helperSha256))) {
        [void]$startInfo.ArgumentList.Add([string]$pair[0])
        [void]$startInfo.ArgumentList.Add([string]$pair[1])
    }
    if ($null -ne $Attestation.PSObject.Properties['hostExecutable']) {
        foreach ($pair in @(
                @('--expected-host-executable', [string]$Attestation.hostExecutable),
                @('--expected-host-sha256', [string]$Attestation.hostSha256),
                @('--expected-client-executable', [string]$Attestation.clientExecutable),
                @('--expected-client-sha256', [string]$Attestation.clientSha256))) {
            [void]$startInfo.ArgumentList.Add([string]$pair[0])
            [void]$startInfo.ArgumentList.Add([string]$pair[1])
        }
    }
    # The shared PhaseGate helper owns this PowerShell child in a kill-on-close
    # Job, drains both capped readers, and proves the child tree has settled on
    # every timeout/cancel path.  Its bounded WaitForExit(milliseconds) pump
    # is the only process wait here; the Rust wrapper then owns the cycle Job.
    $bounded = Invoke-DevManagerPhaseGateBoundedCommand `
        -StartInfo $startInfo `
        -TimeoutMilliseconds $supervisorDeadlineMilliseconds `
        -StdoutBytes $stdoutByteCap `
        -StderrBytes $stderrByteCap
    $stdoutText = [string]$bounded.Stdout
    $stderrText = [string]$bounded.Stderr
    $lines = @($stdoutText -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($lines.Count -ne 1) { throw "Rust supervisor emitted $($lines.Count) JSON results; exactly one is required." }
    $result = $lines[0] | ConvertFrom-Json
    if ([int]$result.schemaVersion -ne $schemaVersion) { throw 'Rust supervisor result schema mismatch.' }
    if ([string]$result.status -notin @('passed', 'failed', 'rejected')) { throw 'Rust supervisor result status is invalid.' }
    foreach ($required in @('completedCycles', 'iterations', 'realLifecycle', 'releaseEligible')) {
        if ($null -eq $result.PSObject.Properties[$required]) { throw "Rust supervisor result is missing required field '$required'." }
    }
    if ($SyntheticOnly -and [bool]$result.releaseEligible) { throw 'synthetic infrastructure can never be release-eligible.' }
    if ([int]$result.completedCycles -gt [int]$result.iterations -or [int]$result.iterations -gt 100) { throw 'Rust supervisor cycle counts exceed the strict bounded schema.' }
    $wrapperTimedOut = if ($null -ne $result.PSObject.Properties['wrapperTimedOut']) { [string]$result.wrapperTimedOut } else { 'False' }
    $jobZero = if ($null -ne $result.PSObject.Properties['jobZero']) { [string]$result.jobZero } else { 'True' }
    if ($wrapperTimedOut -eq 'True' -and $jobZero -ne 'True') {
        throw 'Rust supervisor wrapper timed out without proving Job zero.'
    }
    if ($bounded.ExitCode -eq 0 -and [string]$result.status -ne 'passed') { throw 'zero exit with a non-passing supervisor result.' }
    [pscustomobject][ordered]@{
        result = $result
        exitCode = [int]$bounded.ExitCode
        stderr = ConvertTo-BoundedError $stderrText
    }
}

function Invoke-BoundedFinalUnion {
    param(
        [Parameter(Mandatory = $true)][string]$Entrypoint,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$TempDirectory,
        [Parameter(Mandatory = $true)][object]$HostPin,
        [Parameter(Mandatory = $true)][object]$ClientPin
    )
    # Revalidate immediately before the union launch so a caller cannot pin a
    # file, mutate it, and rely on an earlier check.  The child entrypoint then
    # receives the exact canonical path/hash pair and Rust validates it again.
    $hostRevalidated = Resolve-CallerPinnedExecutable `
        -Path ([string]$HostPin.path) `
        -ExpectedSha256 ([string]$HostPin.sha256) `
        -Label 'host' `
        -WorktreeRoot $WorktreeRoot
    $clientRevalidated = Resolve-CallerPinnedExecutable `
        -Path ([string]$ClientPin.path) `
        -ExpectedSha256 ([string]$ClientPin.sha256) `
        -Label 'client' `
        -WorktreeRoot $WorktreeRoot
    $pwshCommands = @(
        Get-Command -Name 'pwsh' -All -CommandType Application -ErrorAction SilentlyContinue |
            Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_.Source) }
    )
    if ($pwshCommands.Count -ne 1) { throw "final host/client union requires exactly one pwsh.exe (found $($pwshCommands.Count))." }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = [IO.Path]::GetFullPath([string]$pwshCommands[0].Source)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.WorkingDirectory = $WorktreeRoot
    $info.Environment.Clear()
    $systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) { throw 'final host/client union cannot establish SystemRoot.' }
    if (-not (Test-Path -LiteralPath $Entrypoint -PathType Leaf)) {
        throw 'final host/client union entrypoint is unavailable.'
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $Entrypoint
    $canonicalEntrypoint = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $Entrypoint -ErrorAction Stop).Path)
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $canonicalEntrypoint -AncestorPath $WorktreeRoot)) {
        throw 'final host/client union entrypoint escapes the worktree.'
    }
    $info.Environment['SystemRoot'] = $systemRoot
    $info.Environment['TEMP'] = $TempDirectory
    $info.Environment['TMP'] = $TempDirectory
    $info.Environment['PATH'] = @((Join-Path $systemRoot 'System32'), (Split-Path -Parent $info.FileName)) -join ';'
    $info.Environment['DEVMANAGER_PROFILE'] = 'native-next-dev'
    $info.Environment['DEVMANAGER_INSTANCE_LABEL'] = 'Next'
    $info.Environment['DEVMANAGER_RUNTIME_KIND'] = 'native-next'
    foreach ($argument in @(
            '-NoProfile', '-NonInteractive', '-File', $canonicalEntrypoint,
            '-Iterations', '100', '-Seed', [string]$Seed,
            '-HostExecutable', [string]$hostRevalidated.path, '-HostSha256', [string]$hostRevalidated.sha256,
            '-ClientExecutable', [string]$clientRevalidated.path, '-ClientSha256', [string]$clientRevalidated.sha256)) {
        [void]$info.ArgumentList.Add([string]$argument)
    }
    $bounded = Invoke-DevManagerPhaseGateBoundedCommand `
        -StartInfo $info `
        -TimeoutMilliseconds 600000 `
        -StdoutBytes $stdoutByteCap `
        -StderrBytes $stderrByteCap
    if ($bounded.ExitCode -ne 0 -or $bounded.StderrBytes -ne 0) {
        throw "final host/client union failed closed (exit=$($bounded.ExitCode) stderrBytes=$($bounded.StderrBytes))."
    }
    $lines = @($bounded.Stdout -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    if ($lines.Count -ne 1) { throw "final host/client union emitted $($lines.Count) JSON results; exactly one is required." }
    $result = $lines[0] | ConvertFrom-Json
    if ([string]$result.status -ne 'passed' -or [string]$result.jobZero -ne 'True' -or [string]$result.releaseEligible -ne 'True' -or [string]$result.realLifecycle -ne 'True' -or [int]$result.completedCycles -ne 100) { throw 'final host/client union did not prove 100 real release-eligible cycles + Job zero.' }
    [pscustomobject][ordered]@{ result = $result; exitCode = [int]$bounded.ExitCode; stderr = ConvertTo-BoundedError $bounded.Stderr }
}

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    Write-Output ((New-SoakSummary -Status 'unavailable' -Launched:$false -Error 'Phase 3.10 requires Windows Job Objects.' -Supervisor:$null -RunId:$null -RunDirectory:$null) | ConvertTo-Json -Depth 16 -Compress)
    exit 78
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$manifestResolved = Join-Path $PSScriptRoot 'phase3-process-soak.manifest.json'
$supervisorPath = Join-Path $worktreeRoot 'target-native-next\debug\devmanager-process-test-helper.exe'
# These are only retained isolated roots for the default manifest; Rust owns
# every manifest/evidence file open and all publication under its root.
$defaultTempDirectory = Join-Path $worktreeRoot '.tmp-phase3-soak'
$defaultEvidenceRoot = Join-Path $worktreeRoot '.devmanager-next\evidence'
New-Item -ItemType Directory -Force -Path $defaultTempDirectory, $defaultEvidenceRoot | Out-Null
$supervisorDocument = $null
$failure = $null
$finalStatus = 'failed'
$runId = $null
$runDirectory = $null
$attestation = $null
$unionDependency = $null
$hostPin = $null
$clientPin = $null

try {
    if (-not (Test-Path -LiteralPath $manifestResolved -PathType Leaf)) {
        throw "manifest does not exist: $manifestResolved"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $manifestResolved
    if (-not (Test-Path -LiteralPath $supervisorPath -PathType Leaf)) {
        throw "fixed Rust supervisor is unavailable: $supervisorPath"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $supervisorPath
    $callerPinValues = @(@($HostExecutable, $HostSha256, $ClientExecutable, $ClientSha256) |
        Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    )
    if ($callerPinValues.Count -gt 0 -and $callerPinValues.Count -ne 4) {
        throw 'host/client caller pin requires all four canonical identity/hash arguments.'
    }
    if ($callerPinValues.Count -eq 4) {
        $hostPin = Resolve-CallerPinnedExecutable -Path $HostExecutable -ExpectedSha256 $HostSha256 -Label 'host' -WorktreeRoot $worktreeRoot
        $clientPin = Resolve-CallerPinnedExecutable -Path $ClientExecutable -ExpectedSha256 $ClientSha256 -Label 'client' -WorktreeRoot $worktreeRoot
    }
    if (-not $SyntheticOnly -and $Iterations -eq 100) {
        $candidateHost = Join-Path $worktreeRoot 'target-live-native-next\devmanager-host.exe'
        $candidateClient = Join-Path $worktreeRoot 'target-live-native-next\devmanager-next.exe'
        $candidateUnion = Join-Path $PSScriptRoot 'Invoke-HostClientProcessSoak.ps1'
        if ($null -ne $hostPin -and $null -ne $clientPin -and
            (Test-Path -LiteralPath $candidateUnion -PathType Leaf) -and
            ([IO.Path]::GetFullPath($candidateHost) -ieq [string]$hostPin.path) -and
            ([IO.Path]::GetFullPath($candidateClient) -ieq [string]$clientPin.path)) {
            $unionDependency = [pscustomobject]@{
                host = [string]$hostPin.path
                client = [string]$clientPin.path
                entrypoint = $candidateUnion
            }
        }
        else {
            $finalStatus = 'hold'
            $failure = 'HOLD: final 100-cycle host/client union dependency is unavailable; synthetic infrastructure cannot claim the release soak.'
            $summary = New-SoakSummary -Status $finalStatus -Launched:$false -Error $failure -Supervisor:$null -RunId:$null -RunDirectory:$null
            Write-Output ($summary | ConvertTo-Json -Depth 32 -Compress)
            exit 78
        }
    }
    # Pin the exact helper before and after the external source attestation.
    # A binary that changes while it computes its source view is not a valid
    # caller-pinned supervisor, even if the later hash happens to pass.
    $helperShaBefore = Get-ExternalSha256 -Path $supervisorPath
    $attestation = [pscustomobject]@{
        gitRevision = Get-ExternalGitRevision -WorktreeRoot $worktreeRoot
        sourceTreeState = Get-ExternalSourceTreeState -WorktreeRoot $worktreeRoot
        helperSha256 = Get-ExternalSha256 -Path $supervisorPath
    }
    if ($attestation.helperSha256 -ne $helperShaBefore) {
        throw 'helper changed during external attestation; stale or self-attested input rejected.'
    }
    $attestation | Add-Member -NotePropertyName buildId -NotePropertyValue ('sha256:' + [string]$attestation.helperSha256)
    if ($null -ne $hostPin -and $null -ne $clientPin) {
        $attestation | Add-Member -NotePropertyName hostExecutable -NotePropertyValue ([string]$hostPin.path)
        $attestation | Add-Member -NotePropertyName hostSha256 -NotePropertyValue ([string]$hostPin.sha256)
        $attestation | Add-Member -NotePropertyName clientExecutable -NotePropertyValue ([string]$clientPin.path)
        $attestation | Add-Member -NotePropertyName clientSha256 -NotePropertyValue ([string]$clientPin.sha256)
    }
    $hasIterations = $PSBoundParameters.ContainsKey('Iterations')
    $hasSeed = $PSBoundParameters.ContainsKey('Seed')
    if ($null -ne $unionDependency) {
        $supervisorDocument = Invoke-BoundedFinalUnion `
            -Entrypoint ([string]$unionDependency.entrypoint) `
            -WorktreeRoot $worktreeRoot `
            -TempDirectory ([System.IO.Path]::GetFullPath($defaultTempDirectory)) `
            -HostPin $hostPin `
            -ClientPin $clientPin
    }
    else {
        $supervisorDocument = Invoke-BoundedRustSupervisor `
            -SupervisorPath $supervisorPath `
            -Manifest $manifestResolved `
            -WorkingDirectory $worktreeRoot `
            -TempDirectory ([System.IO.Path]::GetFullPath($defaultTempDirectory)) `
            -HasIterations:$hasIterations `
            -HasSeed:$hasSeed `
            -Attestation $attestation
    }
    $result = $supervisorDocument.result
    $runId = if ($null -ne $result.PSObject.Properties['runId']) {
        [string]$result.runId
    }
    else {
        $null
    }
    $runDirectory = if ($null -ne $result.PSObject.Properties['runDirectory']) {
        [string]$result.runDirectory
    }
    else {
        $null
    }
    $finalStatus = if ([string]$result.status -eq 'passed' -and $supervisorDocument.exitCode -eq 0) { 'passed' } else { 'failed' }
    if ($finalStatus -ne 'passed') { $failure = 'Rust supervisor reported a failed or rejected cycle; no pass is inferred.' }
}
catch {
    $failure = ConvertTo-BoundedError $_.Exception.Message
    $finalStatus = if ($failure -match '(?i)manifest does not exist|requires Windows|unavailable') { 'unavailable' } else { 'failed' }
}

$supervisorResult = if ($null -eq $supervisorDocument) { $null } else { $supervisorDocument.result }
$summary = New-SoakSummary `
    -Status $finalStatus `
    -Launched:($null -ne $supervisorDocument) `
    -Error $failure `
    -Supervisor $supervisorResult `
    -RunId $runId `
    -RunDirectory $runDirectory
Write-Output ($summary | ConvertTo-Json -Depth 32 -Compress)
if ($finalStatus -eq 'passed') { exit 0 }
if ($finalStatus -eq 'unavailable') { exit 78 }
exit 1
