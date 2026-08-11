# Phase 3.10 process soak entrypoint.
#
# PowerShell owns only immutable manifest validation and evidence publication.
# The Rust process-test helper owns every cycle process, Job Object, deadline,
# pipe reader, identity check, and bounded JSON result. No caller-supplied
# script is imported and no process is killed by PID from this script.

[CmdletBinding()]
param(
    [string]$ManifestPath = (Join-Path $PSScriptRoot 'phase3-process-soak.manifest.json')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

$phase = 'phase-03-process-soak'
$schemaVersion = 1
$maxManifestBytes = 1MB
$maxIterations = 100
$maxScenarios = 32
$maxArguments = 32
$maxArgumentBytes = 512
$maxOutputBytes = 4MB
$maxResultBytes = 256KB

function ConvertTo-SafeError {
    param([AllowNull()][object]$Value)

    if ($null -eq $Value) { return $null }
    $text = [string]$Value
    if ($text.Length -gt 512) { $text = $text.Substring(0, 512) }
    $text = [regex]::Replace($text, '(?i)(password|secret|token|api[_-]?key|authorization)\s*[:=]\s*[^\s;]+', '$1=<redacted>')
    $text = [regex]::Replace($text, '(?i)([A-Za-z]:\\|\\\\)[^\s;]+', '<path>')
    $text = [regex]::Replace($text, '[\x00-\x1f\x7f]', '_')
    return $text
}

function New-SoakResult {
    param(
        [Parameter(Mandatory = $true)][string]$Status,
        [Parameter(Mandatory = $true)][bool]$Launched,
        [AllowNull()][string]$Error,
        [AllowNull()][object]$SupervisorResult
    )

    return [pscustomobject][ordered]@{
        schemaVersion = $schemaVersion
        phase = $phase
        status = $Status
        launched = $Launched
        error = (ConvertTo-SafeError $Error)
        supervisor = $SupervisorResult
    }
}

function Assert-ExactProperties {
    param(
        [Parameter(Mandatory = $true)][object]$Object,
        [Parameter(Mandatory = $true)][string]$Label,
        [Parameter(Mandatory = $true)][string[]]$Allowed
    )

    if ($null -eq $Object -or $Object -is [string] -or $Object -is [array]) {
        throw "$Label must be an object."
    }
    $actual = @($Object.PSObject.Properties | ForEach-Object { [string]$_.Name })
    $extra = @($actual | Where-Object { $Allowed -notcontains $_ })
    $missing = @($Allowed | Where-Object { $actual -notcontains $_ })
    if ($extra.Count -ne 0 -or $missing.Count -ne 0) {
        throw "$Label field set is not exact (missing=$($missing -join ',') extra=$($extra -join ','))."
    }
}

function Assert-BoundedString {
    param(
        [AllowNull()][object]$Value,
        [Parameter(Mandatory = $true)][string]$Label,
        [int]$Maximum = 512,
        [string]$Pattern
    )

    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        throw "$Label must be a non-empty string."
    }
    $text = [string]$Value
    if ($text.Length -gt $Maximum -or $text -match '[\x00-\x1f\x7f]') {
        throw "$Label is unbounded or contains control characters."
    }
    if (-not [string]::IsNullOrWhiteSpace($Pattern) -and $text -notmatch $Pattern) {
        throw "$Label contains unsafe characters."
    }
    return $text
}

function Assert-BoundedInteger {
    param(
        [AllowNull()][object]$Value,
        [Parameter(Mandatory = $true)][string]$Label,
        [int64]$Minimum = 0,
        [int64]$Maximum = [int64]::MaxValue
    )

    if (-not (Test-DevManagerIntegralNumber -Value $Value)) {
        throw "$Label must be an integer."
    }
    $number = [int64]$Value
    if ($number -lt $Minimum -or $number -gt $Maximum) {
        throw "$Label must be within [$Minimum,$Maximum]."
    }
    return $number
}

function Resolve-ManifestPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $candidate = if (Test-DevManagerAbsolutePath -LiteralPath $Path) {
        [System.IO.Path]::GetFullPath($Path)
    }
    else {
        [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot $Path))
    }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $candidate -AncestorPath $worktreeRoot)) {
        throw "manifest must remain beneath the isolated worktree root: $candidate"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $candidate
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw "manifest does not exist: $candidate"
    }
    return $candidate
}

function Read-ImmutableManifest {
    param([Parameter(Mandatory = $true)][string]$Path)

    $item = Get-Item -LiteralPath $Path -Force
    if ([int64]$item.Length -gt $maxManifestBytes) {
        throw "manifest exceeds $maxManifestBytes bytes."
    }
    $raw = Get-Content -LiteralPath $Path -Raw -Encoding utf8
    if ([string]::IsNullOrWhiteSpace($raw)) { throw 'manifest is empty.' }
    $manifest = $raw | ConvertFrom-Json
    Assert-ExactProperties -Object $manifest -Label 'manifest' -Allowed @(
        'schemaVersion','revision','supervisorExecutable','supervisorSha256',
        'helperExecutable','helperSha256','cycleExecutable','cycleSha256',
        'workingDirectory','seed','iterations','budgets','scenarioCatalog'
    )
    Assert-BoundedInteger $manifest.schemaVersion 'manifest.schemaVersion' $schemaVersion $schemaVersion | Out-Null
    Assert-BoundedString $manifest.revision 'manifest.revision' 128 '^[A-Za-z0-9._:-]+$' | Out-Null
    foreach ($field in @('supervisorSha256','helperSha256','cycleSha256')) {
        Assert-BoundedString $manifest.$field "manifest.$field" 64 '^[A-Fa-f0-9]{64}$' | Out-Null
    }
    Assert-BoundedInteger $manifest.seed 'manifest.seed' 0 $([int64]::MaxValue) | Out-Null
    $iterations = Assert-BoundedInteger $manifest.iterations 'manifest.iterations' 1 $maxIterations
    if (-not [bool]([int64]$iterations -le $maxIterations)) { throw 'iterations exceeded bound.' }

    Assert-ExactProperties -Object $manifest.budgets -Label 'manifest.budgets' -Allowed @(
        'suiteDeadlineMs','cycleDeadlineMs','cleanupDeadlineMs','stdoutBytes','stderrBytes','resultBytes'
    )
    $suite = Assert-BoundedInteger $manifest.budgets.suiteDeadlineMs 'budgets.suiteDeadlineMs' 1 600000
    $cycle = Assert-BoundedInteger $manifest.budgets.cycleDeadlineMs 'budgets.cycleDeadlineMs' 1 60000
    $cleanup = Assert-BoundedInteger $manifest.budgets.cleanupDeadlineMs 'budgets.cleanupDeadlineMs' 1 60000
    if ($cycle -gt $suite) { throw 'cycle deadline exceeds suite deadline.' }
    foreach ($field in @('stdoutBytes','stderrBytes')) {
        Assert-BoundedInteger $manifest.budgets.$field "budgets.$field" 1 $maxOutputBytes | Out-Null
    }
    Assert-BoundedInteger $manifest.budgets.resultBytes 'budgets.resultBytes' 1024 $maxResultBytes | Out-Null

    $catalog = @($manifest.scenarioCatalog)
    if ($catalog.Count -lt 1 -or $catalog.Count -gt $maxScenarios) { throw 'scenarioCatalog count is outside its bound.' }
    foreach ($scenario in $catalog) {
        Assert-ExactProperties -Object $scenario -Label 'scenario' -Allowed @('name','arguments','expectedExitCode')
        Assert-BoundedString $scenario.name 'scenario.name' 96 '^[A-Za-z0-9._:-]+$' | Out-Null
        $arguments = @($scenario.arguments)
        if ($arguments.Count -lt 1 -or $arguments.Count + 4 -gt $maxArguments) { throw 'scenario arguments count is outside its bound.' }
        if ([string]$arguments[0] -cne 'cycle') { throw 'scenario must invoke the fixed cycle protocol.' }
        foreach ($argument in $arguments) {
            Assert-BoundedString $argument 'scenario.argument' $maxArgumentBytes | Out-Null
        }
        $expectedExit = Assert-BoundedInteger $scenario.expectedExitCode 'scenario.expectedExitCode' 0 1
        [void]$expectedExit
    }

    foreach ($field in @('supervisorExecutable','helperExecutable','cycleExecutable','workingDirectory')) {
        $rawPath = Assert-BoundedString $manifest.$field "manifest.$field" 4096
        $path = if (Test-DevManagerAbsolutePath -LiteralPath $rawPath) {
            [System.IO.Path]::GetFullPath($rawPath)
        }
        else {
            [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot $rawPath))
        }
        if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $path -AncestorPath $worktreeRoot)) {
            throw "$field must remain beneath the isolated worktree root: $path"
        }
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $path
        if ($field -eq 'workingDirectory') {
            if (-not (Test-Path -LiteralPath $path -PathType Container)) { throw "$field is not a directory." }
        }
        elseif (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "$field is not a file."
        }
        $manifest.$field = $path
    }
    return $manifest
}

function Assert-ManifestHashes {
    param([Parameter(Mandatory = $true)][object]$Manifest)

    foreach ($field in @(
        @{ Path = 'supervisorExecutable'; Hash = 'supervisorSha256' },
        @{ Path = 'helperExecutable'; Hash = 'helperSha256' },
        @{ Path = 'cycleExecutable'; Hash = 'cycleSha256' }
    )) {
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath ([string]$Manifest.($field.Path))).Hash.ToLowerInvariant()
        if ($actual -cne ([string]$Manifest.($field.Hash)).ToLowerInvariant()) {
            throw "$($field.Path) SHA-256 does not match the immutable manifest."
        }
    }
}

function Invoke-RustSupervisor {
    param(
        [Parameter(Mandatory = $true)][object]$Manifest,
        [Parameter(Mandatory = $true)][string]$ManifestPath
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = [string]$Manifest.supervisorExecutable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.WorkingDirectory = [string]$Manifest.workingDirectory
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($remove in @('DEVMANAGER_PROFILE','DEVMANAGER_CONFIG_DIR','DEVMANAGER_APP_IDENTITY','DEVMANAGER_RUNTIME_KIND')) {
        [void]$startInfo.Environment.Remove($remove)
    }
    [void]$startInfo.ArgumentList.Add('supervise')
    [void]$startInfo.ArgumentList.Add('--manifest')
    [void]$startInfo.ArgumentList.Add($ManifestPath)
    $process = [System.Diagnostics.Process]::Start($startInfo)
    if ($null -eq $process) { throw 'unable to start Rust supervisor.' }
    try {
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $lines = @($stdout -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($lines.Count -ne 1) {
            throw "Rust supervisor emitted $($lines.Count) JSON result lines."
        }
        if ($lines[0].Length -gt [int]$Manifest.budgets.resultBytes) {
            throw 'Rust supervisor result exceeds the manifest result byte cap.'
        }
        $result = $lines[0] | ConvertFrom-Json
        if ([string]$result.status -eq 'rejected') {
            Assert-ExactProperties -Object $result -Label 'rejected supervisor result' -Allowed @(
                'schemaVersion','status','launched','error'
            )
            if ([int]$result.schemaVersion -ne $schemaVersion -or [bool]$result.launched) {
                throw 'rejected supervisor result was not fail-closed.'
            }
            return [pscustomobject][ordered]@{ result = $result; exitCode = [int]$process.ExitCode; stderr = (ConvertTo-SafeError $stderr) }
        }
        Assert-ExactProperties -Object $result -Label 'supervisor result' -Allowed @(
            'schemaVersion','status','revision','seed','iterations','completedCycles','cycles'
        )
        if ([int]$result.schemaVersion -ne $schemaVersion) { throw 'supervisor result schema mismatch.' }
        if ([string]$result.revision -cne [string]$Manifest.revision) { throw 'supervisor revision mismatch.' }
        if ([string]$result.status -notin @('passed','failed')) { throw 'supervisor result status is invalid.' }
        if ([uint64]$result.seed -ne [uint64]$Manifest.seed) { throw 'supervisor seed mismatch.' }
        if ([int]$result.iterations -ne [int]$Manifest.iterations) { throw 'supervisor iteration mismatch.' }
        Assert-BoundedInteger $result.completedCycles 'supervisor.completedCycles' 1 $Manifest.iterations | Out-Null
        $cycles = @($result.cycles)
        if ($cycles.Count -lt 1 -or $cycles.Count -gt [int]$Manifest.iterations) { throw 'supervisor cycle count is invalid.' }
        if ($cycles.Count -ne [int]$result.completedCycles) { throw 'supervisor completed cycle count mismatch.' }
        foreach ($cycle in $cycles) {
            Assert-ExactProperties -Object $cycle -Label 'supervisor cycle' -Allowed @(
                'iteration','scenario','status','outcome','exitCode','durationMs','stdoutBytes','stderrBytes',
                'activeProcessZero','rootIdentity','memberIdentities','result','error'
            )
            Assert-BoundedInteger $cycle.iteration 'cycle.iteration' 1 $Manifest.iterations | Out-Null
            Assert-BoundedInteger $cycle.durationMs 'cycle.durationMs' 0 ([int64]$Manifest.budgets.cycleDeadlineMs + [int64]$Manifest.budgets.cleanupDeadlineMs) | Out-Null
            Assert-BoundedInteger $cycle.stdoutBytes 'cycle.stdoutBytes' 0 $maxOutputBytes | Out-Null
            Assert-BoundedInteger $cycle.stderrBytes 'cycle.stderrBytes' 0 $maxOutputBytes | Out-Null
            if (-not [bool]$cycle.activeProcessZero -and [string]$result.status -eq 'passed') { throw 'passed cycle did not prove ACTIVE_PROCESS_ZERO.' }
            if ($null -eq $cycle.rootIdentity) {
                if ([string]$cycle.status -eq 'passed' -or [string]$cycle.outcome -notin @('suite-timeout','launch-failed','invalid-arguments')) {
                    throw 'cycle without a live root identity was not an explicit prelaunch failure.'
                }
                continue
            }
            Assert-ExactProperties -Object $cycle.rootIdentity -Label 'cycle.rootIdentity' -Allowed @('processId','creationTime100ns','executablePath')
            Assert-BoundedInteger $cycle.rootIdentity.processId 'rootIdentity.processId' 1 ([uint32]::MaxValue) | Out-Null
            Assert-BoundedInteger $cycle.rootIdentity.creationTime100ns 'rootIdentity.creationTime100ns' 1 ([int64]::MaxValue) | Out-Null
            Assert-BoundedString $cycle.rootIdentity.executablePath 'rootIdentity.executablePath' 2048 | Out-Null
        }
        return [pscustomobject][ordered]@{ result = $result; exitCode = [int]$process.ExitCode; stderr = (ConvertTo-SafeError $stderr) }
    }
    finally {
        $process.Dispose()
    }
}

function Publish-AtomicJson {
    param(
        [Parameter(Mandatory = $true)][object]$Value,
        [Parameter(Mandatory = $true)][string]$OutputPath,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot
    )

    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $OutputPath -AncestorPath $EvidenceRoot)) {
        throw "evidence output escapes retained evidence root: $OutputPath"
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $OutputPath
    $parent = Split-Path -Parent $OutputPath
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $temporary = Join-Path $parent ('.{0}.{1}.tmp' -f ([System.IO.Path]::GetFileName($OutputPath)), [guid]::NewGuid().ToString('N'))
    $json = $Value | ConvertTo-Json -Depth 32 -Compress
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
        [System.IO.File]::WriteAllBytes($temporary, $bytes)
        [System.IO.File]::Move($temporary, $OutputPath)
    }
    finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}

if (-not ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)) {
    Write-Output ((New-SoakResult -Status 'unavailable' -Launched:$false -Error 'Phase 3.10 requires Windows Job Objects.' -SupervisorResult:$null) | ConvertTo-Json -Depth 8 -Compress)
    exit 78
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
$runId = [guid]::NewGuid().ToString('N')
$runDirectory = [System.IO.Path]::GetFullPath((Join-Path $evidenceRoot "$phase\runs\$runId"))
$manifestResolved = $null
$manifestBeforeHash = $null
$finalStatus = 'failed'
$failure = $null
$supervisorDocument = $null

try {
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $evidenceRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory
    if (Test-Path -LiteralPath $runDirectory) {
        throw "run directory already exists; append-only evidence run collision: $runDirectory"
    }
    New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
    $manifestResolved = Resolve-ManifestPath -Path $ManifestPath
    $manifestBeforeHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $manifestResolved).Hash.ToLowerInvariant()
    $manifest = Read-ImmutableManifest -Path $manifestResolved
    Assert-ManifestHashes -Manifest $manifest
    $manifestJson = Get-Content -LiteralPath $manifestResolved -Raw -Encoding utf8
    $manifestArtifact = [pscustomobject][ordered]@{
            schemaVersion = $schemaVersion
            revision = $manifest.revision
            sourceName = [System.IO.Path]::GetFileName($manifestResolved)
            sha256 = $manifestBeforeHash
            bytes = [System.Text.Encoding]::UTF8.GetByteCount($manifestJson)
            capturedAtUtc = [DateTime]::UtcNow.ToString('o')
            seed = [uint64]$manifest.seed
            iterations = [int]$manifest.iterations
            budgets = $manifest.budgets
            scenarioCatalog = @($manifest.scenarioCatalog | ForEach-Object {
                    [pscustomobject][ordered]@{
                        name = [string]$_.name
                        arguments = [string[]]@($_.arguments)
                        expectedExitCode = [int]$_.expectedExitCode
                    }
                })
            binaries = [pscustomobject][ordered]@{
                supervisorSha256 = [string]$manifest.supervisorSha256
                helperSha256 = [string]$manifest.helperSha256
                cycleSha256 = [string]$manifest.cycleSha256
            }
        }
    Publish-AtomicJson -Value $manifestArtifact -OutputPath (Join-Path $runDirectory 'manifest.json') -EvidenceRoot $evidenceRoot

    $supervisorDocument = Invoke-RustSupervisor -Manifest $manifest -ManifestPath $manifestResolved
    $result = $supervisorDocument.result
    $afterHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $manifestResolved).Hash.ToLowerInvariant()
    if ($afterHash -cne $manifestBeforeHash) { throw 'immutable manifest changed during the run.' }
    $durations = @($result.cycles | ForEach-Object { [int64]$_.durationMs } | Sort-Object)
    if ($durations.Count -eq 0) { throw 'supervisor returned no cycle timings.' }
    $p50 = $durations[[Math]::Max(0, [int][Math]::Ceiling($durations.Count * 0.50) - 1)]
    $p95 = $durations[[Math]::Max(0, [int][Math]::Ceiling($durations.Count * 0.95) - 1)]
    $performance = [pscustomobject][ordered]@{
        schemaVersion = $schemaVersion
        sampleCount = $durations.Count
        samplesMs = [int64[]]$durations
        durationMs = [pscustomobject][ordered]@{
            p50 = [int64]$p50
            p95 = [int64]$p95
            maximum = [int64]$durations[-1]
        }
        cpu = [pscustomobject][ordered]@{
            convention = 'whole-machine'
            formula = 'clamp(100 * process-tree-time / (wall-time * logical-processor-count), 0, 100)'
            coreEquivalentFormula = '100 * process-tree-time / wall-time'
            logicalProcessorCount = [Environment]::ProcessorCount
        }
    }
    $conformance = [pscustomobject][ordered]@{
        schemaVersion = $schemaVersion
        revision = $manifest.revision
        manifestSha256 = $manifestBeforeHash
        ansiCorpus = 'tests/fixtures/ansi/phase3-v1.json'
        outputProtocol = 'exactly-one-json-result'
        activeProcessZeroRequired = $true
        identity = 'PID + creationTime100ns + canonical executable path'
        readerCaps = [pscustomobject][ordered]@{
            stdoutBytes = [int64]$manifest.budgets.stdoutBytes
            stderrBytes = [int64]$manifest.budgets.stderrBytes
            resultBytes = [int64]$manifest.budgets.resultBytes
        }
        deadlinesMs = [pscustomobject][ordered]@{
            suite = [int64]$manifest.budgets.suiteDeadlineMs
            cycle = [int64]$manifest.budgets.cycleDeadlineMs
            cleanup = [int64]$manifest.budgets.cleanupDeadlineMs
        }
        scenarios = @($result.cycles | ForEach-Object { [pscustomobject][ordered]@{ iteration = $_.iteration; name = $_.scenario; status = $_.status; outcome = $_.outcome } })
    }
    Publish-AtomicJson -Value $result -OutputPath (Join-Path $runDirectory 'summary.json') -EvidenceRoot $evidenceRoot
    Publish-AtomicJson -Value $performance -OutputPath (Join-Path $runDirectory 'performance.json') -EvidenceRoot $evidenceRoot
    Publish-AtomicJson -Value $conformance -OutputPath (Join-Path $runDirectory 'conformance.json') -EvidenceRoot $evidenceRoot
    $finalStatus = if ([string]$result.status -eq 'passed' -and $supervisorDocument.exitCode -eq 0) { 'passed' } else { 'failed' }
    if ($finalStatus -ne 'passed') { $failure = 'Rust supervisor reported a failed cycle; no pass is inferred.' }
}
catch {
    $failure = ConvertTo-SafeError $_.Exception.Message
    $finalStatus = if ($failure -match '(?i)manifest does not exist|requires Windows|unable to start Rust supervisor') { 'unavailable' } else { 'failed' }
}

$summary = New-SoakResult -Status $finalStatus -Launched:($null -ne $supervisorDocument) -Error $failure -SupervisorResult:$(if ($null -eq $supervisorDocument) { $null } else { $supervisorDocument.result })
$summary | Add-Member -NotePropertyName runId -NotePropertyValue $runId
$summary | Add-Member -NotePropertyName runDirectory -NotePropertyValue $runDirectory
try {
    Publish-AtomicJson -Value $summary -OutputPath (Join-Path $runDirectory 'run.json') -EvidenceRoot $evidenceRoot
}
catch {
    $finalStatus = 'failed'
    $summary.status = 'failed'
    $summary.error = ConvertTo-SafeError $_.Exception.Message
}
Write-Output ($summary | ConvertTo-Json -Depth 32 -Compress)
if ($finalStatus -eq 'passed') { exit 0 }
if ($finalStatus -eq 'unavailable') { exit 78 }
exit 1
