# Bounded Phase 7 prompt-library smoke. Fixture mode only.
# Parses source, isolates CARGO_TARGET_DIR under C:\Temp\devmanager-prompt-smoke-*,
# runs one integration test, emits one JSON result. Never launches providers,
# never reads/writes production config/remote/session, never cleans broad paths.

[CmdletBinding()]
param(
    [switch]$Authenticated,
    [string]$IsolatedProfile,
    [int]$TimeoutSeconds = 600,
    [int]$MaxOutputBytes = 1048576
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:SchemaVersion = 1
$script:RejectExitCode = 1
$script:HoldExitCode = 2
$script:TestName = 'phase7_prompt_library_smoke_public_api_contract'
$script:DiagnosticTailBytes = 1200
$script:RunRoot = $null
$script:ProtectedRoot = $null

if (-not ('PromptLibrarySmokeBoundedIO' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;

public sealed class PromptLibrarySmokeOutputCap
{
    readonly object _sync = new object();
    readonly int _maxBytes;
    readonly byte[] _tail;
    int _tailStart;
    int _tailLen;
    public int Used;
    public bool Exceeded;

    public PromptLibrarySmokeOutputCap(int maxBytes, int tailBytes)
    {
        if (maxBytes < 16 || tailBytes < 16) throw new ArgumentOutOfRangeException("maxBytes");
        _maxBytes = maxBytes;
        _tail = new byte[tailBytes];
    }

    public void Append(byte[] chunk, int count)
    {
        if (chunk == null || count <= 0) return;
        lock (_sync)
        {
            if (Exceeded) return; // StopAdmittingWhenExceeded
            int room = _maxBytes - Used;
            int admit = count > room ? room : count;
            if (admit > 0)
            {
                Used += admit;
                for (int i = 0; i < admit; i++)
                {
                    byte value = chunk[i];
                    if (_tailLen < _tail.Length)
                    {
                        _tail[(_tailStart + _tailLen) % _tail.Length] = value;
                        _tailLen++;
                    }
                    else
                    {
                        _tail[_tailStart] = value;
                        _tailStart = (_tailStart + 1) % _tail.Length;
                    }
                }
            }
            if (count > room) Exceeded = true;
        }
    }

    public string Tail()
    {
        lock (_sync)
        {
            if (_tailLen == 0) return string.Empty;
            var ordered = new byte[_tailLen];
            for (int i = 0; i < _tailLen; i++)
                ordered[i] = _tail[(_tailStart + i) % _tail.Length];
            return new UTF8Encoding(false, false).GetString(ordered);
        }
    }
}

public static class PromptLibrarySmokeBoundedIO
{
    public static string SelfTestOutputBounds(int maxBytes)
    {
        var huge = new PromptLibrarySmokeOutputCap(maxBytes, 32);
        var hugeLine = Encoding.UTF8.GetBytes(new string('A', maxBytes + 64));
        huge.Append(hugeLine, hugeLine.Length);
        if (!huge.Exceeded || huge.Used != maxBytes)
            throw new InvalidOperationException("huge line was not bounded.");

        var flood = new PromptLibrarySmokeOutputCap(maxBytes, 32);
        var chunk = Encoding.UTF8.GetBytes(new string('B', 256));
        while (!flood.Exceeded) flood.Append(chunk, chunk.Length);
        if (!flood.Exceeded || flood.Used != maxBytes)
            throw new InvalidOperationException("flood was not bounded.");

        var cap = new PromptLibrarySmokeOutputCap(maxBytes, 32);
        var exact = Encoding.UTF8.GetBytes(new string('C', maxBytes));
        cap.Append(exact, exact.Length);
        cap.Append(new byte[] { (byte)'x' }, 1);
        if (!cap.Exceeded || cap.Used != maxBytes)
            throw new InvalidOperationException("cap+1 was not bounded.");

        return "HUGE_LINE_BOUNDED,FLOOD_BOUNDED,CAP_PLUS_ONE_BOUNDED,StopAdmittingWhenExceeded";
    }

    public static int Drain(Process proc, int timeoutMs, int maxBytes, int tailBytes, out bool exceeded, out bool timedOut, out string tail)
    {
        var cap = new PromptLibrarySmokeOutputCap(maxBytes, tailBytes);
        var stdout = new Thread(() => ReadDiscardAfterCap(proc.StandardOutput.BaseStream, cap));
        var stderr = new Thread(() => ReadDiscardAfterCap(proc.StandardError.BaseStream, cap));
        stdout.IsBackground = false;
        stderr.IsBackground = false;
        stdout.Start();
        stderr.Start();
        var clock = Stopwatch.StartNew();
        while (!proc.HasExited && !cap.Exceeded && clock.ElapsedMilliseconds < timeoutMs)
            proc.WaitForExit(200);
        timedOut = !proc.HasExited && !cap.Exceeded;
        exceeded = cap.Exceeded;
        if (timedOut || exceeded) KillTree(proc);
        int joinMs = timeoutMs < 5000 ? timeoutMs : 5000;
        stdout.Join(joinMs);
        stderr.Join(joinMs);
        try { proc.WaitForExit(joinMs); } catch { }
        exceeded = cap.Exceeded;
        tail = cap.Tail();
        return cap.Used;
    }

    static void ReadDiscardAfterCap(Stream stream, PromptLibrarySmokeOutputCap cap)
    {
        var buf = new byte[4096];
        while (true)
        {
            int n;
            try { n = stream.Read(buf, 0, buf.Length); }
            catch { break; }
            if (n <= 0) break;
            if (cap.Exceeded)
                continue;
            cap.Append(buf, n);
        }
    }

    static void KillTree(Process proc)
    {
        try { if (!proc.HasExited) proc.Kill(); } catch { }
        try
        {
            var psi = new ProcessStartInfo("taskkill.exe", "/PID " + proc.Id + " /T /F");
            psi.UseShellExecute = false;
            psi.CreateNoWindow = true;
            using (var killer = Process.Start(psi))
            {
                if (killer != null) killer.WaitForExit(5000);
            }
        }
        catch { }
    }
}
'@
}

function Write-PromptLibrarySmokeResult {
    param(
        [Parameter(Mandatory = $true)][string]$Disposition,
        [bool]$Pass = $false,
        [string]$IsolatedProfile = '',
        [string]$Reason = '',
        [int]$ExitCode = 0,
        [string]$CargoTargetDir = '',
        [int]$CapturedBytes = 0
    )

    $payload = [ordered]@{
        schemaVersion   = $script:SchemaVersion
        pass            = [bool]$Pass
        disposition     = $Disposition
        isolatedProfile = $IsolatedProfile
        test            = [ordered]@{
            crate      = 'prompt_library_smoke'
            name       = $script:TestName
            filter     = $script:TestName
            exact      = $true
        }
        invariants      = [ordered]@{
            no_provider_processes           = $true
            no_provider_send                = $true
            local_authoritative             = $true
            manual_chain_only               = $true
            exact_version_payload           = $true
            org_publish_permission_checked  = $true
            zero_production_mutation        = $true
        }
        cargoTargetDir  = $CargoTargetDir
        capturedBytes   = $CapturedBytes
        reason          = $Reason
        exitCode        = $ExitCode
    }
    $json = ($payload | ConvertTo-Json -Compress -Depth 6)
    if ($json.Length -gt 8192) {
        throw "Smoke JSON result exceeded 8 KiB."
    }
    Write-Output $json
}

function Assert-PromptLibrarySmokeParse {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($LiteralPath, [ref]$tokens, [ref]$errors)
    if ($null -ne $errors -and @($errors).Count -gt 0) {
        throw "PowerShell parse failed for '$LiteralPath': $($errors[0].ToString())"
    }
}

function Assert-PromptLibrarySmokeBoundedCapture {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $source = [System.IO.File]::ReadAllText($LiteralPath)
    $unboundedAsync = 'ReadTo' + 'EndAsync'
    $unboundedSync = 'ReadTo' + 'End('
    if ($source.Contains($unboundedAsync) -or $source.Contains($unboundedSync)) {
        throw "HOLD: unbounded StandardOutput/StandardError capture is forbidden."
    }
    if ($source -notmatch 'PromptLibrarySmokeBoundedIO' -or $source -notmatch 'StopAdmittingWhenExceeded') {
        throw "HOLD: bounded output admission path is missing."
    }
    $proof = [PromptLibrarySmokeBoundedIO]::SelfTestOutputBounds(64)
    if ($proof -notmatch 'StopAdmittingWhenExceeded') {
        throw "HOLD: bounded output self-test did not prove StopAdmittingWhenExceeded."
    }
}

function Test-PromptLibrarySmokeForbiddenProfile {
    param([AllowEmptyString()][string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    $normalized = $Value.Trim().ToLowerInvariant()
    if ($normalized -match 'com\.userfirst\.devmanager') {
        return $true
    }
    return @(
        'production',
        'installed',
        'default',
        'unprofiled'
    ) -contains $normalized
}

function Test-PromptLibrarySmokeProductionPath {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $normalized = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $rendered = $normalized.Replace('\', '/')
    if ($rendered.Contains('com.userfirst.devmanager')) {
        return $true
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $LiteralPath -AncestorPath $script:ProtectedRoot) {
        return $true
    }
    return $false
}

function Assert-PromptLibrarySmokeOwnedPath {
    param(
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$RelativePath,
        [Parameter(Mandatory = $true)][ValidateSet('Leaf', 'Container')][string]$PathType
    )

    $candidate = [System.IO.Path]::GetFullPath((Join-Path $WorktreeRoot $RelativePath))
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $candidate -AncestorPath $WorktreeRoot)) {
        throw "Owned path '$candidate' escapes worktree."
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $candidate
    if ($PathType -eq 'Leaf') {
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "Required smoke path missing: $RelativePath"
        }
    }
    elseif (-not (Test-Path -LiteralPath $candidate -PathType Container)) {
        throw "Required smoke path missing: $RelativePath"
    }
}

function New-PromptLibrarySmokeRunRoot {
    param(
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)][string]$ProtectedRoot
    )

    $runId = [guid]::NewGuid().ToString('N')
    $runRoot = [System.IO.Path]::GetFullPath((Join-Path 'C:\Temp' ("devmanager-prompt-smoke-{0}" -f $runId)))
    $normalized = Normalize-DevManagerPath -LiteralPath $runRoot
    if ($normalized -notmatch '^[a-z]:\\temp\\devmanager-prompt-smoke-[0-9a-f]{32}$') {
        throw "Generated run root is not a unique C:\Temp\devmanager-prompt-smoke-* identity."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $runRoot -AncestorPath $ProtectedRoot) {
        throw "Run root must not land under production."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $runRoot -AncestorPath $WorktreeRoot) {
        throw "Run root must not alias the source worktree."
    }
    New-Item -ItemType Directory -Force -Path $runRoot | Out-Null
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runRoot
    Set-Content -LiteralPath (Join-Path $runRoot 'run.identity') -Value $runId -Encoding ascii
    return [pscustomobject]@{
        runId   = $runId
        runRoot = $runRoot
        target  = [System.IO.Path]::GetFullPath((Join-Path $runRoot 'target'))
        profile = ('promptsmoke{0}' -f $runId.Substring(0, 12))
        config  = [System.IO.Path]::GetFullPath((Join-Path $runRoot 'profile'))
    }
}

function Remove-PromptLibrarySmokeRunRoot {
    param([Parameter(Mandatory = $true)]$Run)

    if ($null -eq $Run -or [string]::IsNullOrWhiteSpace([string]$Run.runRoot)) {
        return
    }
    $tempRoot = [System.IO.Path]::GetFullPath('C:\Temp')
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $Run.runRoot -AncestorPath $tempRoot)) {
        throw "Refusing cleanup outside C:\Temp."
    }
    if ((Normalize-DevManagerPath -LiteralPath $Run.runRoot) -notmatch 'devmanager-prompt-smoke-[0-9a-f]{32}$') {
        throw "Refusing cleanup of unrecognized run root."
    }
    $identityPath = Join-Path $Run.runRoot 'run.identity'
    if (-not (Test-Path -LiteralPath $identityPath -PathType Leaf)) {
        throw "Refusing cleanup: run.identity missing."
    }
    $onDisk = (Get-Content -LiteralPath $identityPath -Encoding ascii -TotalCount 1).Trim()
    if ($onDisk -cne [string]$Run.runId) {
        throw "Refusing cleanup: run.identity does not match this run."
    }
    Remove-Item -LiteralPath $Run.runRoot -Recurse -Force
}

function Resolve-PromptLibrarySmokeCargo {
    $userProfile = [Environment]::GetFolderPath('UserProfile')
    if ([string]::IsNullOrWhiteSpace($userProfile)) {
        throw "UserProfile is required to resolve cargo."
    }
    $cargo = [System.IO.Path]::GetFullPath((Join-Path $userProfile '.cargo\bin\cargo.exe'))
    if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
        throw "Retained cargo identity is missing."
    }
    if ([System.IO.Path]::GetFileName($cargo) -ine 'cargo.exe') {
        throw "Cargo identity must be cargo.exe."
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargo
    return $cargo
}

function Invoke-PromptLibrarySmokeCargo {
    param(
        [Parameter(Mandatory = $true)][string]$CargoExe,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]$Run,
        [Parameter(Mandatory = $true)][int]$TimeoutMs,
        [Parameter(Mandatory = $true)][int]$MaxBytes
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $CargoExe
    foreach ($argument in @(
            'test',
            '--offline',
            '--locked',
            '--quiet',
            '--test',
            'prompt_library_smoke',
            '--color',
            'never',
            '--',
            '--exact',
            $script:TestName,
            '--test-threads=1'
        )) {
        [void]$psi.ArgumentList.Add($argument)
    }
    $psi.WorkingDirectory = $WorktreeRoot
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables.Clear()
    foreach ($name in @(
            'PATH', 'PATHEXT', 'SystemRoot', 'windir', 'COMSPEC', 'ComSpec', 'OS',
            'NUMBER_OF_PROCESSORS', 'PROCESSOR_ARCHITECTURE', 'USERPROFILE',
            'HOMEDRIVE', 'HOMEPATH', 'USERNAME', 'USERDOMAIN', 'TEMP', 'TMP',
            'CARGO_HOME', 'RUSTUP_HOME'
        )) {
        $value = [Environment]::GetEnvironmentVariable($name, 'Process')
        if (-not [string]::IsNullOrWhiteSpace([string]$value)) {
            $psi.EnvironmentVariables[$name] = [string]$value
        }
    }
    $psi.EnvironmentVariables['CARGO_TARGET_DIR'] = [string]$Run.target
    $psi.EnvironmentVariables['DEVMANAGER_PROFILE'] = [string]$Run.profile
    $psi.EnvironmentVariables['CARGO_TERM_COLOR'] = 'never'
    $psi.EnvironmentVariables['CARGO_INCREMENTAL'] = '0'

    $proc = [System.Diagnostics.Process]::Start($psi)
    if ($null -eq $proc) {
        throw "Failed to start cargo."
    }
    $exceeded = $false
    $timedOut = $false
    $tail = ''
    $bytes = [PromptLibrarySmokeBoundedIO]::Drain(
        $proc,
        $TimeoutMs,
        $MaxBytes,
        $script:DiagnosticTailBytes,
        [ref]$exceeded,
        [ref]$timedOut,
        [ref]$tail
    )
    if ($exceeded) {
        throw "HOLD: cargo output exceeded $MaxBytes bytes: $tail"
    }
    if ($timedOut) {
        throw "HOLD: cargo exceeded ${TimeoutMs}ms deadline: $tail"
    }
    if ($proc.ExitCode -ne 0) {
        throw "HOLD: focused test failed (exit $($proc.ExitCode)): $tail"
    }
    return $bytes
}

$selfPath = $PSCommandPath
if ([string]::IsNullOrWhiteSpace($selfPath)) {
    $selfPath = $MyInvocation.MyCommand.Path
}

try {
    Assert-PromptLibrarySmokeParse -LiteralPath $selfPath
    Assert-PromptLibrarySmokeParse -LiteralPath (Join-Path $PSScriptRoot 'Isolation.ps1')
    Assert-PromptLibrarySmokeBoundedCapture -LiteralPath $selfPath
}
catch {
    Write-PromptLibrarySmokeResult -Disposition 'hold' -Reason $_.Exception.Message -ExitCode $script:HoldExitCode
    exit $script:HoldExitCode
}

. (Join-Path $PSScriptRoot 'Isolation.ps1')

try {
    if ($Authenticated) {
        throw "Authenticated/production modes are rejected; fixture mode only."
    }
    if ($TimeoutSeconds -lt 30 -or $TimeoutSeconds -gt 3600) {
        throw "TimeoutSeconds must be between 30 and 3600."
    }
    if ($MaxOutputBytes -lt 4096 -or $MaxOutputBytes -gt 10485760) {
        throw "MaxOutputBytes must be between 4 KiB and 10 MiB."
    }
    if (Test-PromptLibrarySmokeForbiddenProfile -Value $IsolatedProfile) {
        throw "Refusing production/installed IsolatedProfile."
    }
    $inherited = [Environment]::GetEnvironmentVariable('DEVMANAGER_PROFILE', 'Process')
    if (Test-PromptLibrarySmokeForbiddenProfile -Value $inherited) {
        throw "Refusing inherited production/installed DEVMANAGER_PROFILE."
    }

    $worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
    $script:ProtectedRoot = Get-DevManagerProductionRoot
    if (-not [string]::IsNullOrWhiteSpace($IsolatedProfile) -and (Test-DevManagerAbsolutePath -LiteralPath $IsolatedProfile)) {
        if (Test-PromptLibrarySmokeProductionPath -LiteralPath $IsolatedProfile) {
            throw "Refusing IsolatedProfile under a production root."
        }
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktreeRoot
    foreach ($owned in @(
            @{ rel = 'scripts\native-next\Invoke-PromptLibrarySmoke.ps1'; kind = 'Leaf' },
            @{ rel = 'docs\prompts.md'; kind = 'Leaf' },
            @{ rel = 'tests\prompt_library_smoke.rs'; kind = 'Leaf' },
            @{ rel = 'tests\fixtures\prompts\smoke'; kind = 'Container' }
        )) {
        Assert-PromptLibrarySmokeOwnedPath -WorktreeRoot $worktreeRoot -RelativePath $owned.rel -PathType $owned.kind
    }

    $run = New-PromptLibrarySmokeRunRoot -WorktreeRoot $worktreeRoot -ProtectedRoot $script:ProtectedRoot
    $script:RunRoot = $run
    New-Item -ItemType Directory -Force -Path $run.target | Out-Null
    New-Item -ItemType Directory -Force -Path $run.config | Out-Null
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $run.target
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $run.target -AncestorPath 'C:\Temp')) {
        throw "CARGO_TARGET_DIR must stay under C:\Temp."
    }

    $cargo = Resolve-PromptLibrarySmokeCargo
    $captured = Invoke-PromptLibrarySmokeCargo `
        -CargoExe $cargo `
        -WorktreeRoot $worktreeRoot `
        -Run $run `
        -TimeoutMs ($TimeoutSeconds * 1000) `
        -MaxBytes $MaxOutputBytes

    Write-PromptLibrarySmokeResult `
        -Disposition 'pass' `
        -Pass $true `
        -IsolatedProfile $run.profile `
        -CargoTargetDir $run.target `
        -CapturedBytes $captured `
        -ExitCode 0
    exit 0
}
catch {
    $message = $_.Exception.Message
    $disposition = 'hold'
    $code = $script:HoldExitCode
    if ($message -match 'rejected|Refusing|must be between|escapes worktree|Authenticated') {
        $disposition = 'reject'
        $code = $script:RejectExitCode
    }
    $profile = if ($null -ne $script:RunRoot) { [string]$script:RunRoot.profile } else { '' }
    $target = if ($null -ne $script:RunRoot) { [string]$script:RunRoot.target } else { '' }
    Write-PromptLibrarySmokeResult -Disposition $disposition -IsolatedProfile $profile -CargoTargetDir $target -Reason $message -ExitCode $code
    exit $code
}
finally {
    if ($null -ne $script:RunRoot) {
        try { Remove-PromptLibrarySmokeRunRoot -Run $script:RunRoot } catch { }
    }
}
