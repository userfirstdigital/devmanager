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

    [switch]$SyntheticOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

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
        error = $Error
        supervisor = $Supervisor
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
    [void]$startInfo.ArgumentList.Add('bounded-supervise')
    [void]$startInfo.ArgumentList.Add('--manifest')
    [void]$startInfo.ArgumentList.Add($Manifest)
    [void]$startInfo.ArgumentList.Add('--timeout-ms')
    [void]$startInfo.ArgumentList.Add([string]($supervisorDeadlineMilliseconds - 5000))
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

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $stdoutState = [pscustomobject]@{
        reader = $null
        task = $null
        buffer = New-Object char[] 8192
        text = [Text.StringBuilder]::new()
        totalBytes = 0L
        truncated = $false
        done = $false
    }
    $stderrState = [pscustomobject]@{
        reader = $null
        task = $null
        buffer = New-Object char[] 8192
        text = [Text.StringBuilder]::new()
        totalBytes = 0L
        truncated = $false
        done = $false
    }

    $started = $false
    $timedOut = $false
    $deadline = [Diagnostics.Stopwatch]::GetTimestamp() +
        [int64]($supervisorDeadlineMilliseconds * [Diagnostics.Stopwatch]::Frequency / 1000)
    try {
        $started = $process.Start()
        if (-not $started) { throw 'unable to start Rust supervisor.' }
        $stdoutState.reader = $process.StandardOutput
        $stderrState.reader = $process.StandardError
        $stdoutState.task = $stdoutState.reader.ReadAsync($stdoutState.buffer, 0, $stdoutState.buffer.Length)
        $stderrState.task = $stderrState.reader.ReadAsync($stderrState.buffer, 0, $stderrState.buffer.Length)
        while (-not ($stdoutState.done -and $stderrState.done -and $process.HasExited)) {
            $now = [Diagnostics.Stopwatch]::GetTimestamp()
            $remaining = [int](($deadline - $now) * 1000 / [Diagnostics.Stopwatch]::Frequency)
            if ($remaining -le 0 -and -not $timedOut) {
                $timedOut = $true
                throw 'Rust-owned supervisor wrapper exceeded the absolute deadline; no pass is inferred.'
            }
            $tasks = @(@($stdoutState.task, $stderrState.task) | Where-Object { $null -ne $_ -and -not $_.IsCompleted })
            if ($tasks.Count -gt 0) {
                [void][Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]$tasks, [Math]::Max(1, $remaining))
            }
            foreach ($state in @($stdoutState, $stderrState)) {
                if ($state.done -or $null -eq $state.task -or -not $state.task.IsCompleted) { continue }
                $count = $state.task.GetAwaiter().GetResult()
                if ($count -eq 0) {
                    $state.done = $true
                    continue
                }
                $state.totalBytes += [Text.Encoding]::UTF8.GetByteCount($state.buffer, 0, $count)
                $cap = if ($state -eq $stdoutState) { $stdoutByteCap } else { $stderrByteCap }
                if ($state.text.Length -lt $cap) {
                    $remainingChars = $cap - $state.text.Length
                    [void]$state.text.Append($state.buffer, 0, [Math]::Min($count, $remainingChars))
                }
                if ($state.totalBytes -gt $cap) { $state.truncated = $true }
                $state.task = $state.reader.ReadAsync($state.buffer, 0, $state.buffer.Length)
            }
        }
        $remaining = [int](([Diagnostics.Stopwatch]::GetTimestamp() - $deadline) * -1000 / [Diagnostics.Stopwatch]::Frequency)
        if ($remaining -le 0) { throw 'Rust supervisor did not settle before the absolute deadline.' }
        [void]$process.WaitForExit([Math]::Min(1000, $remaining))
        if ($timedOut) { throw 'Rust supervisor exceeded the absolute deadline.' }
        $stdoutText = $stdoutState.text.ToString()
        $stderrText = $stderrState.text.ToString()
        if ($stdoutState.truncated -or $stdoutState.totalBytes -gt $stdoutByteCap) {
            throw 'Rust supervisor stdout exceeded the bounded protocol cap.'
        }
        if ($stderrState.truncated -or $stderrState.totalBytes -gt $stderrByteCap) {
            throw 'Rust supervisor stderr exceeded the bounded protocol cap.'
        }
        $lines = @($stdoutText -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($lines.Count -ne 1) { throw "Rust supervisor emitted $($lines.Count) JSON results; exactly one is required." }
        $result = $lines[0] | ConvertFrom-Json
        if ([int]$result.schemaVersion -ne $schemaVersion) { throw 'Rust supervisor result schema mismatch.' }
        if ([string]$result.status -notin @('passed', 'failed', 'rejected')) { throw 'Rust supervisor result status is invalid.' }
        $wrapperTimedOut = if ($null -ne $result.PSObject.Properties['wrapperTimedOut']) { [string]$result.wrapperTimedOut } else { 'False' }
        $jobZero = if ($null -ne $result.PSObject.Properties['jobZero']) { [string]$result.jobZero } else { 'True' }
        if ($wrapperTimedOut -eq 'True' -and $jobZero -ne 'True') {
            throw 'Rust supervisor wrapper timed out without proving Job zero.'
        }
        if ($process.ExitCode -eq 0 -and [string]$result.status -ne 'passed') { throw 'zero exit with a non-passing supervisor result.' }
        [pscustomobject][ordered]@{
            result = $result
            exitCode = [int]$process.ExitCode
            stderr = (ConvertTo-BoundedError $stderrText)
        }
    }
    finally {
        if ($started -and -not $process.HasExited) {
            throw 'Rust-owned supervisor wrapper returned before its child settled.'
        }
        $process.Dispose()
    }
}

function Invoke-BoundedFinalUnion {
    param(
        [Parameter(Mandatory = $true)][string]$Entrypoint,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$TempDirectory
    )
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
    $info.Environment['SystemRoot'] = $systemRoot
    $info.Environment['TEMP'] = $TempDirectory
    $info.Environment['TMP'] = $TempDirectory
    $info.Environment['PATH'] = @((Join-Path $systemRoot 'System32'), (Split-Path -Parent $info.FileName)) -join ';'
    foreach ($argument in @('-NoProfile', '-NonInteractive', '-File', $Entrypoint, '-Iterations', '100', '-Seed', [string]$Seed)) {
        [void]$info.ArgumentList.Add([string]$argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $stdout = [Text.StringBuilder]::new()
    $stderr = [Text.StringBuilder]::new()
    $started = $false
    try {
        $started = $process.Start()
        if (-not $started) { throw 'unable to start final host/client union entrypoint.' }
        $stdoutTask = $process.StandardOutput.ReadAsync()
        $stderrTask = $process.StandardError.ReadAsync()
        $deadline = [Diagnostics.Stopwatch]::GetTimestamp() + [int64](600000 * [Diagnostics.Stopwatch]::Frequency / 1000)
        while (-not ($stdoutTask.IsCompleted -and $stderrTask.IsCompleted -and $process.HasExited)) {
            if ([Diagnostics.Stopwatch]::GetTimestamp() -ge $deadline) {
                throw 'final host/client union exceeded its absolute deadline; dependency must own Rust Job cleanup.'
            }
            [Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($stdoutTask, $stderrTask), 250) | Out-Null
        }
        $stdout.Append($stdoutTask.GetAwaiter().GetResult()) | Out-Null
        $stderr.Append($stderrTask.GetAwaiter().GetResult()) | Out-Null
        if ($stdout.Length -gt $stdoutByteCap -or $stderr.Length -gt $stderrByteCap) { throw 'final host/client union exceeded the bounded protocol caps.' }
        $lines = @($stdout.ToString() -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($lines.Count -ne 1) { throw "final host/client union emitted $($lines.Count) JSON results; exactly one is required." }
        $result = $lines[0] | ConvertFrom-Json
        if ([string]$result.status -ne 'passed' -or [string]$result.jobZero -ne 'True') { throw 'final host/client union did not prove passed + Job zero.' }
        [pscustomobject][ordered]@{ result = $result; exitCode = [int]$process.ExitCode; stderr = ConvertTo-BoundedError $stderr.ToString() }
    }
    finally {
        if ($started -and -not $process.HasExited) { throw 'final host/client union returned before its owned cleanup settled.' }
        $process.Dispose()
    }
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

try {
    if (-not (Test-Path -LiteralPath $manifestResolved -PathType Leaf)) {
        throw "manifest does not exist: $manifestResolved"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $manifestResolved
    if (-not (Test-Path -LiteralPath $supervisorPath -PathType Leaf)) {
        throw "fixed Rust supervisor is unavailable: $supervisorPath"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $supervisorPath
    if (-not $SyntheticOnly -and $Iterations -eq 100) {
        $candidateHost = Join-Path $worktreeRoot 'target-live-native-next\devmanager-host.exe'
        $candidateClient = Join-Path $worktreeRoot 'target-live-native-next\devmanager-next.exe'
        $candidateUnion = Join-Path $PSScriptRoot 'Invoke-HostClientProcessSoak.ps1'
        if ((Test-Path -LiteralPath $candidateHost -PathType Leaf) -and
            (Test-Path -LiteralPath $candidateClient -PathType Leaf) -and
            (Test-Path -LiteralPath $candidateUnion -PathType Leaf)) {
            $unionDependency = [pscustomobject]@{
                host = $candidateHost
                client = $candidateClient
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
    $hasIterations = $PSBoundParameters.ContainsKey('Iterations')
    $hasSeed = $PSBoundParameters.ContainsKey('Seed')
    if ($null -ne $unionDependency) {
        $supervisorDocument = Invoke-BoundedFinalUnion `
            -Entrypoint ([string]$unionDependency.entrypoint) `
            -WorktreeRoot $worktreeRoot `
            -TempDirectory ([System.IO.Path]::GetFullPath($defaultTempDirectory))
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
