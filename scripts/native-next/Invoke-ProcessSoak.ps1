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

    [string]$ManifestPath = (Join-Path $PSScriptRoot 'phase3-process-soak.manifest.json')
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
    if ($text.Length -gt 512) { $text = $text.Substring(0, 512) }
    [regex]::Replace($text, '[\x00-\x1f\x7f]', '_')
}

function Resolve-SoakPath {
    param([Parameter(Mandatory = $true)][string]$Path)
    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    $worktree = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
    [System.IO.Path]::GetFullPath((Join-Path $worktree $Path))
}

function Invoke-BoundedRustSupervisor {
    param(
        [Parameter(Mandatory = $true)][string]$SupervisorPath,
        [Parameter(Mandatory = $true)][string]$Manifest,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$TempDirectory,
        [Parameter(Mandatory = $true)][bool]$HasIterations,
        [Parameter(Mandatory = $true)][bool]$HasSeed
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
    [void]$startInfo.ArgumentList.Add('supervise')
    [void]$startInfo.ArgumentList.Add('--manifest')
    [void]$startInfo.ArgumentList.Add($Manifest)
    if ($HasIterations) {
        [void]$startInfo.ArgumentList.Add('--iterations')
        [void]$startInfo.ArgumentList.Add([string]$Iterations)
    }
    if ($HasSeed) {
        [void]$startInfo.ArgumentList.Add('--seed')
        [void]$startInfo.ArgumentList.Add([string]$Seed)
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
    try {
        $started = $process.Start()
        if (-not $started) { throw 'unable to start Rust supervisor.' }
        $stdoutState.reader = $process.StandardOutput
        $stderrState.reader = $process.StandardError
        $stdoutState.task = $stdoutState.reader.ReadAsync($stdoutState.buffer, 0, $stdoutState.buffer.Length)
        $stderrState.task = $stderrState.reader.ReadAsync($stderrState.buffer, 0, $stderrState.buffer.Length)
        $deadline = [Diagnostics.Stopwatch]::GetTimestamp() +
            [int64]($supervisorDeadlineMilliseconds * [Diagnostics.Stopwatch]::Frequency / 1000)
        $cleanupDeadline = $deadline
        while (-not ($stdoutState.done -and $stderrState.done -and $process.HasExited)) {
            $now = [Diagnostics.Stopwatch]::GetTimestamp()
            $remaining = [int](($deadline - $now) * 1000 / [Diagnostics.Stopwatch]::Frequency)
            if ($remaining -le 0 -and -not $timedOut) {
                $timedOut = $true
                $process.Kill($true)
                $cleanupDeadline = [Diagnostics.Stopwatch]::GetTimestamp() +
                    [int64](5000 * [Diagnostics.Stopwatch]::Frequency / 1000)
                $remaining = 5000
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
            if ($timedOut -and [Diagnostics.Stopwatch]::GetTimestamp() -gt $cleanupDeadline) {
                throw 'Rust supervisor output readers exceeded the bounded cleanup deadline.'
            }
        }
        [void]$process.WaitForExit(1000)
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
        if ($process.ExitCode -eq 0 -and [string]$result.status -ne 'passed') { throw 'zero exit with a non-passing supervisor result.' }
        [pscustomobject][ordered]@{
            result = $result
            exitCode = [int]$process.ExitCode
            stderr = (ConvertTo-BoundedError $stderrText)
        }
    }
    finally {
        try {
            if ($started -and -not $process.HasExited) {
                try {
                    $process.Kill($true)
                    [void]$process.WaitForExit(5000)
                }
                catch {
                    throw "Rust supervisor cleanup failed: $($_.Exception.Message)"
                }
            }
        }
        finally {
            $process.Dispose()
        }
    }
}

if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
    Write-Output ((New-SoakSummary -Status 'unavailable' -Launched:$false -Error 'Phase 3.10 requires Windows Job Objects.' -Supervisor:$null -RunId:$null -RunDirectory:$null) | ConvertTo-Json -Depth 16 -Compress)
    exit 78
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$manifestResolved = Resolve-SoakPath -Path $ManifestPath
$supervisorPath = Join-Path $worktreeRoot 'target\debug\devmanager-process-test-helper.exe'
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

try {
    if (-not (Test-Path -LiteralPath $manifestResolved -PathType Leaf)) {
        throw "manifest does not exist: $manifestResolved"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $manifestResolved
    if (-not (Test-Path -LiteralPath $supervisorPath -PathType Leaf)) {
        throw "fixed Rust supervisor is unavailable: $supervisorPath"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $supervisorPath
    $hasIterations = $PSBoundParameters.ContainsKey('Iterations')
    $hasSeed = $PSBoundParameters.ContainsKey('Seed')
    $supervisorDocument = Invoke-BoundedRustSupervisor `
        -SupervisorPath $supervisorPath `
        -Manifest $manifestResolved `
        -WorkingDirectory $worktreeRoot `
        -TempDirectory ([System.IO.Path]::GetFullPath($defaultTempDirectory)) `
        -HasIterations:$hasIterations `
        -HasSeed:$hasSeed
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
