# Task 6.10 dependency-safe workspace conformance smoke.
# Fixture/fake-host cargo test only. Uses temporary repositories only.
# integration_claim=none
# Resolves rustup/cargo and Git from retained identities, not PATH/shims.
# Unique per-run isolated target. Owned Windows Job tree only.
# Does not read or hash production config.json / remote.json / session.json.
# Refuse inherited protocol.file.allow / GIT_ALLOW_PROTOCOL overrides.

[CmdletBinding()]
param(
    [switch]$Authenticated,
    [string]$Profile,
    [int]$TimeoutSeconds = 600,
    [int]$MaxOutputBytes = 262144,
    [switch]$ProbeGuards
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

if ($TimeoutSeconds -lt 30 -or $TimeoutSeconds -gt 3600) {
    throw "TimeoutSeconds must be between 30 and 3600."
}
if ($MaxOutputBytes -lt 4096 -or $MaxOutputBytes -gt 10485760) {
    throw "MaxOutputBytes must be between 4 KiB and 10 MiB."
}

if ($Authenticated) {
    throw "Refusing authenticated external actions. Invoke-WorkspaceSmoke.ps1 is fixture/fake-host only."
}

function Test-WorkspaceSmokeForbiddenProfile {
    param([AllowEmptyString()][string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    $normalized = $Value.Trim().ToLowerInvariant()
    return @(
        'production',
        'installed',
        'default',
        'unprofiled',
        'com.userfirst.devmanager'
    ) -contains $normalized
}

function Assert-WorkspaceSmokeClearEnvironment {
    $forbidden = @(
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
    foreach ($name in $forbidden) {
        $value = [Environment]::GetEnvironmentVariable($name, 'Process')
        if (-not [string]::IsNullOrWhiteSpace([string]$value)) {
            throw "Refusing inherited build/Git override '$name'."
        }
    }
}

function Resolve-WorkspaceSmokeRetainedCargo {
    $userProfile = [Environment]::GetFolderPath('UserProfile')
    if ([string]::IsNullOrWhiteSpace($userProfile)) {
        throw "UserProfile is required to resolve retained rustup."
    }
    $rustup = [System.IO.Path]::GetFullPath((Join-Path $userProfile '.cargo\bin\rustup.exe'))
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $rustup
    if (-not (Test-Path -LiteralPath $rustup)) {
        throw "Retained rustup identity is missing."
    }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $rustup
    # Resolve cargo via rustup which cargo from retained rustup.exe.
    $psi.ArgumentList.Add('which') | Out-Null
    $psi.ArgumentList.Add('cargo') | Out-Null
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    foreach ($name in @(
            'RUSTC_WRAPPER',
            'RUSTC_WORKSPACE_WRAPPER',
            'RUSTFLAGS',
            'CARGO_ENCODED_RUSTFLAGS',
            'GIT_CONFIG_GLOBAL',
            'GIT_CONFIG_SYSTEM',
            'GIT_CONFIG_COUNT',
            'GIT_CONFIG_PARAMETERS',
            'GIT_ALLOW_PROTOCOL'
        )) {
        [void]$psi.Environment.Remove($name)
    }
    $proc = [System.Diagnostics.Process]::Start($psi)
    if (-not $proc.WaitForExit(15000)) {
        try { $proc.Kill() } catch { }
        throw "rustup which cargo exceeded DEADLINE_READY_MS."
    }
    $stdoutBuf = New-Object char[] 4097
    $stderrBuf = New-Object char[] 4097
    $stdoutRead = $proc.StandardOutput.Read($stdoutBuf, 0, $stdoutBuf.Length)
    $null = $proc.StandardError.Read($stderrBuf, 0, $stderrBuf.Length)
    if ($stdoutRead -gt 4096 -or $proc.StandardOutput.Peek() -ge 0) {
        throw "rustup which cargo exceeded RUSTUP_OUTPUT_CAP."
    }
    if ($proc.ExitCode -ne 0 -or $stdoutRead -le 0) {
        throw "rustup which cargo failed."
    }
    $stdout = -join $stdoutBuf[0..($stdoutRead - 1)]
    $cargo = [System.IO.Path]::GetFullPath($stdout.Trim())
    $leaf = [System.IO.Path]::GetFileName($cargo)
    if ($leaf -ine 'cargo.exe') {
        throw "Retained cargo identity must be cargo.exe."
    }
    $toolchainRoot = [System.IO.Path]::GetFullPath((Join-Path $userProfile '.rustup\toolchains'))
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $cargo -AncestorPath $toolchainRoot)) {
        throw "Refusing cargo identity outside retained rustup toolchains."
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargo
    return $cargo
}

# Sanitized Git config for any retained-git invocation:
# credential.helper=, core.fsmonitor=, core.hooksPath=, protocol.file.allow
$script:WorkspaceSmokeGitSanitize = @(
    '-c', 'credential.helper=',
    '-c', 'core.fsmonitor=',
    '-c', 'core.hooksPath=',
    '-c', 'protocol.file.allow=never'
)

function Resolve-WorkspaceSmokeRetainedGit {
    $candidates = @(
        (Join-Path ${env:ProgramFiles} 'Git\cmd\git.exe'),
        (Join-Path ${env:ProgramFiles} 'Git\bin\git.exe')
    )
    foreach ($candidate in $candidates) {
        if ([string]::IsNullOrWhiteSpace([string]$candidate)) { continue }
        if (-not (Test-Path -LiteralPath $candidate)) { continue }
        $full = [System.IO.Path]::GetFullPath($candidate)
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $full
        if ([System.IO.Path]::GetFileName($full) -ine 'git.exe') { continue }
        return $full
    }
    throw "Retained Git identity is missing."
}

function New-WorkspaceSmokeRunRoot {
    param(
        [Parameter(Mandatory = $true)][string]$ProtectedRoot,
        [Parameter(Mandatory = $true)][string]$WorktreeRoot
    )

    $runId = [guid]::NewGuid().ToString('N')
    $runRoot = [System.IO.Path]::GetFullPath((Join-Path 'C:\Temp' ("devmanager-ws-{0}" -f $runId)))
    $normalized = Normalize-DevManagerPath -LiteralPath $runRoot
    if ($normalized -notmatch '^[a-z]:\\temp\\devmanager-ws-[0-9a-f]{32}$') {
        throw "Generated run root is not a unique C:\Temp\devmanager-ws-* identity."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $runRoot -AncestorPath $ProtectedRoot) {
        throw "Run root must not land under production."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $runRoot -AncestorPath $WorktreeRoot) {
        throw "Run root must not alias the source worktree."
    }
    New-Item -ItemType Directory -Force -Path $runRoot | Out-Null
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runRoot
    $identityPath = Join-Path $runRoot 'run.identity'
    Set-Content -LiteralPath $identityPath -Value $runId -Encoding ascii
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $identityPath
    return [pscustomobject]@{
        runId   = $runId
        runRoot = $runRoot
        target  = [System.IO.Path]::GetFullPath((Join-Path $runRoot 'target'))
        profile = ('ws{0}' -f $runId.Substring(0, 12))
        config  = [System.IO.Path]::GetFullPath((Join-Path $runRoot 'profile'))
    }
}

function Assert-WorkspaceSmokeRunIdentity {
    param(
        [Parameter(Mandatory = $true)]$Run,
        [Parameter(Mandatory = $true)][string]$ProtectedRoot
    )

    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $Run.runRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $Run.target
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $Run.config
    $identityPath = Join-Path $Run.runRoot 'run.identity'
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $identityPath
    $onDisk = (Get-Content -LiteralPath $identityPath -Encoding ascii -TotalCount 1).Trim()
    if ($onDisk -cne [string]$Run.runId) {
        throw "Refusing use/cleanup: run.identity does not match this run."
    }
    $tempRoot = [System.IO.Path]::GetFullPath('C:\Temp')
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $Run.runRoot -AncestorPath $tempRoot)) {
        throw "Run root is outside C:\Temp."
    }
    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $Run.runRoot -AncestorPath $ProtectedRoot) {
        throw "Run root overlaps production."
    }
}

function Remove-WorkspaceSmokeRunRoot {
    param(
        [Parameter(Mandatory = $true)]$Run,
        [Parameter(Mandatory = $true)][string]$ProtectedRoot
    )

    Assert-WorkspaceSmokeRunIdentity -Run $Run -ProtectedRoot $ProtectedRoot
    $cleanupStarted = [datetime]::UtcNow
    Remove-Item -LiteralPath $Run.runRoot -Recurse -Force
    $cleanupMs = ([datetime]::UtcNow - $cleanupStarted).TotalMilliseconds
    if ($cleanupMs -gt 5000) {
        throw "CLEANUP_DEADLINE_MS exceeded."
    }
}

if (Test-WorkspaceSmokeForbiddenProfile -Value $Profile) {
    throw "Refusing production/installed profile '$Profile'."
}
$inheritedProfile = [Environment]::GetEnvironmentVariable('DEVMANAGER_PROFILE', 'Process')
if (Test-WorkspaceSmokeForbiddenProfile -Value $inheritedProfile) {
    throw "Refusing inherited production/installed DEVMANAGER_PROFILE."
}
Assert-WorkspaceSmokeClearEnvironment

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$protectedRoot = Get-DevManagerProductionRoot
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktreeRoot

$cargoExe = Resolve-WorkspaceSmokeRetainedCargo
$gitExe = Resolve-WorkspaceSmokeRetainedGit
$run = New-WorkspaceSmokeRunRoot -ProtectedRoot $protectedRoot -WorktreeRoot $worktreeRoot
New-Item -ItemType Directory -Force -Path $run.target | Out-Null
New-Item -ItemType Directory -Force -Path $run.config | Out-Null
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $run.target

Write-Host 'integration_claim=none'
Write-Host 'mode=fixture-fake-host'
Write-Host 'HOLD=S2,S3,S4,S5,S6,S7,S8,S9'
Write-Host 'CLAIM_PROMOTION=forbidden'
Write-Host 'DEADLINE_READY_MS=15000'
Write-Host 'DEADLINE_CTL_MS=10000'
Write-Host 'DEADLINE_STOP_MS=5000'
Write-Host 'CLEANUP_DEADLINE_MS=5000'
Write-Host 'RUSTUP_OUTPUT_CAP=4096'
Write-Host 'STOP_JOIN_MS=5000'
Write-Host 'EVIDENCE_REQUIRED=host.lock,kernel.sqlite3,ManagedProcessJob.active_process_ids'
Write-Host ("cargo={0}" -f $cargoExe)
Write-Host ("git={0}" -f $gitExe)
Write-Host ("runRoot={0}" -f $run.runRoot)
Write-Host ("profile={0}" -f $run.profile)
Write-Host 'TARGET_ISOLATION=ok'
Assert-WorkspaceSmokeRunIdentity -Run $run -ProtectedRoot $protectedRoot

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public sealed class WorkspaceSmokeOwnedJob : IDisposable
{
    const int JobObjectExtendedLimitInformation = 9;
    const int JobObjectBasicProcessIdList = 3;
    const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;
    const uint CREATE_SUSPENDED = 0x00000004;
    const uint CREATE_UNICODE_ENVIRONMENT = 0x00000400;
    const uint CREATE_NO_WINDOW = 0x08000000;
    const uint STARTF_USESTDHANDLES = 0x00000100;
    const uint HANDLE_FLAG_INHERIT = 0x00000001;

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION
    {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS
    {
        public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
        public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
    {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        public int bInheritHandle;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO
    {
        public int cb;
        public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION
    {
        public IntPtr hProcess, hThread;
        public uint dwProcessId, dwThreadId;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr hJob, int infoClass, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION lpJobObjectInfo, int cbJobObjectInfoLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool TerminateJobObject(IntPtr hJob, uint uExitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool QueryInformationJobObject(IntPtr hJob, int infoClass, byte[] lpJobObjectInfo, int cbJobObjectInfoLength, out int lpReturnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr hObject);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcess(string lpApplicationName, string lpCommandLine, IntPtr lpProcessAttributes, IntPtr lpThreadAttributes, bool bInheritHandles, uint dwCreationFlags, IntPtr lpEnvironment, string lpCurrentDirectory, ref STARTUPINFO lpStartupInfo, out PROCESS_INFORMATION lpProcessInformation);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint ResumeThread(IntPtr hThread);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CreatePipe(out IntPtr hReadPipe, out IntPtr hWritePipe, ref SECURITY_ATTRIBUTES lpPipeAttributes, int nSize);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetHandleInformation(IntPtr hObject, uint dwMask, uint dwFlags);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);

    IntPtr _job;

    public int ExitCode;
    public bool TimedOut;
    public bool OutputExceeded;
    public bool InvalidUtf8;
    public bool ActiveProcessZero;
    public string Reason = "ok";
    public int CapturedBytes;

    public static string SelfTestOutputBounds(int maxBytes)
    {
        if (maxBytes < 16) throw new InvalidOperationException("self-test cap too small.");
        var huge = new OutputCap(maxBytes);
        var hugeLine = Encoding.UTF8.GetBytes(new string('A', maxBytes + 64));
        huge.Append(hugeLine, hugeLine.Length);
        if (!huge.Exceeded || huge.Used > maxBytes) throw new InvalidOperationException("huge line was not bounded.");

        var bad = new OutputCap(maxBytes);
        var invalid = new byte[] { 0x80, 0xFF };
        bad.Append(invalid, invalid.Length);
        if (!bad.InvalidUtf8 || bad.Used != 0) throw new InvalidOperationException("invalid UTF-8 was not rejected.");

        var flood = new OutputCap(maxBytes);
        var chunk = Encoding.UTF8.GetBytes(new string('B', 256));
        while (!flood.Exceeded && !flood.InvalidUtf8) flood.Append(chunk, chunk.Length);
        if (!flood.Exceeded || flood.Used > maxBytes) throw new InvalidOperationException("flood was not bounded.");

        var cap = new OutputCap(maxBytes);
        var exact = Encoding.UTF8.GetBytes(new string('C', maxBytes));
        cap.Append(exact, exact.Length);
        cap.Append(new byte[] { (byte)'x' }, 1);
        if (!cap.Exceeded || cap.Used > maxBytes) throw new InvalidOperationException("cap+1 was not bounded.");

        return "HUGE_LINE_BOUNDED,INVALID_UTF8_BOUNDED,FLOOD_BOUNDED,CAP_PLUS_ONE_BOUNDED";
    }

    public void Run(string exe, string arguments, string cwd, Dictionary<string, string> environment, int timeoutMs, int maxBytes)
    {
        _job = CreateJobObject(IntPtr.Zero, null);
        if (_job == IntPtr.Zero) throw new InvalidOperationException("CreateJobObject failed.");
        var limit = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
        limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if (!SetInformationJobObject(_job, JobObjectExtendedLimitInformation, ref limit, Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION))))
            throw new InvalidOperationException("SetInformationJobObject failed.");

        var sa = new SECURITY_ATTRIBUTES { nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), bInheritHandle = 1 };
        IntPtr outRead, outWrite, errRead, errWrite;
        if (!CreatePipe(out outRead, out outWrite, ref sa, 0) || !CreatePipe(out errRead, out errWrite, ref sa, 0))
            throw new InvalidOperationException("CreatePipe failed.");
        SetHandleInformation(outRead, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(errRead, HANDLE_FLAG_INHERIT, 0);

        var si = new STARTUPINFO();
        si.cb = Marshal.SizeOf(typeof(STARTUPINFO));
        si.dwFlags = (int)STARTF_USESTDHANDLES;
        si.hStdOutput = outWrite;
        si.hStdError = errWrite;
        PROCESS_INFORMATION pi;
        var envBlock = BuildEnvironment(environment);
        var commandLine = "\"" + exe + "\" " + arguments;
        var created = CreateProcess(exe, commandLine, IntPtr.Zero, IntPtr.Zero, true, CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW, envBlock, cwd, ref si, out pi);
        Marshal.FreeHGlobal(envBlock);
        CloseHandle(outWrite);
        CloseHandle(errWrite);
        if (!created)
        {
            CloseHandle(outRead);
            CloseHandle(errRead);
            throw new InvalidOperationException("CreateProcess failed.");
        }
        try
        {
            if (!AssignProcessToJobObject(_job, pi.hProcess))
                throw new InvalidOperationException("AssignProcessToJobObject failed.");
            ResumeThread(pi.hThread);

            var cap = new OutputCap(maxBytes);
            var stdoutThread = new Thread(() => Drain(outRead, cap));
            var stderrThread = new Thread(() => Drain(errRead, cap));
            stdoutThread.IsBackground = false;
            stderrThread.IsBackground = false;
            stdoutThread.Start();
            stderrThread.Start();

            var wait = WaitForSingleObject(pi.hProcess, (uint)timeoutMs);
            if (wait != 0)
            {
                TimedOut = true;
                Reason = "timeout";
                TerminateJobObject(_job, 1);
            }
            if (cap.Exceeded)
            {
                OutputExceeded = true;
                Reason = "output_overflow";
                TerminateJobObject(_job, 1);
            }
            if (cap.InvalidUtf8)
            {
                InvalidUtf8 = true;
                Reason = "invalid_utf8";
                TerminateJobObject(_job, 1);
            }

            int stopJoinMs = timeoutMs < 5000 ? timeoutMs : 5000;
            stdoutThread.Join(stopJoinMs);
            stderrThread.Join(stopJoinMs);
            WaitForSingleObject(pi.hProcess, 0);
            CapturedBytes = cap.Used;
            int code;
            GetExitCode(pi.hProcess, out code);
            ExitCode = code;
            ActiveProcessZero = QueryActiveProcessCount() == 0;
            if (!ActiveProcessZero)
            {
                TerminateJobObject(_job, 1);
                ActiveProcessZero = QueryActiveProcessCount() == 0;
            }
            if (!TimedOut && !OutputExceeded && !InvalidUtf8 && ExitCode != 0)
                Reason = "cargo_failed";
        }
        finally
        {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(outRead);
            CloseHandle(errRead);
        }
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr hProcess, out int lpExitCode);

    static void GetExitCode(IntPtr process, out int code)
    {
        if (!GetExitCodeProcess(process, out code)) code = -1;
    }

    int QueryActiveProcessCount()
    {
        var buffer = new byte[16 + (IntPtr.Size * 32)];
        int written;
        if (!QueryInformationJobObject(_job, JobObjectBasicProcessIdList, buffer, buffer.Length, out written))
            return -1;
        return BitConverter.ToInt32(buffer, 4);
    }

    static void Drain(IntPtr read, OutputCap cap)
    {
        using (var fs = new FileStream(new Microsoft.Win32.SafeHandles.SafeFileHandle(read, false), FileAccess.Read))
        {
            var buf = new byte[4096];
            while (true)
            {
                int n = fs.Read(buf, 0, buf.Length);
                if (n <= 0) break;
                cap.Append(buf, n);
                if (cap.Exceeded || cap.InvalidUtf8) break;
            }
        }
    }

    static IntPtr BuildEnvironment(Dictionary<string, string> environment)
    {
        var sb = new StringBuilder();
        foreach (var kv in environment)
        {
            sb.Append(kv.Key).Append('=').Append(kv.Value).Append('\0');
        }
        sb.Append('\0');
        var bytes = Encoding.Unicode.GetBytes(sb.ToString());
        var ptr = Marshal.AllocHGlobal(bytes.Length);
        Marshal.Copy(bytes, 0, ptr, bytes.Length);
        return ptr;
    }

    sealed class OutputCap
    {
        readonly int _max;
        readonly Decoder _decoder = new UTF8Encoding(false, true).GetDecoder();
        readonly StringBuilder _text = new StringBuilder();
        public int Used;
        public bool Exceeded;
        public bool InvalidUtf8;

        public OutputCap(int max) { _max = max; }

        public void Append(byte[] data, int count)
        {
            if (Exceeded || InvalidUtf8) return;
            if (Used + count > _max)
            {
                Exceeded = true;
                return;
            }
            try
            {
                int charCount = _decoder.GetCharCount(data, 0, count, false);
                var chars = new char[charCount];
                _decoder.GetChars(data, 0, count, chars, 0, false);
                _text.Append(chars);
            }
            catch (DecoderFallbackException)
            {
                InvalidUtf8 = true;
                return;
            }
            Used += count;
        }
    }

    public void Dispose()
    {
        if (_job != IntPtr.Zero)
        {
            CloseHandle(_job);
            _job = IntPtr.Zero;
        }
    }
}
'@

$envMap = New-Object 'System.Collections.Generic.Dictionary[string,string]'
$envMap['CARGO_TARGET_DIR'] = [string]$run.target
$envMap['CARGO_TERM_COLOR'] = 'never'
$envMap['CARGO_INCREMENTAL'] = '0'
$envMap['GIT_TERMINAL_PROMPT'] = '0'
$envMap['GIT_CONFIG_NOSYSTEM'] = '1'
$envMap['DEVMANAGER_PROFILE'] = [string]$run.profile
$envMap['DEVMANAGER_CONFIG_DIR'] = [string]$run.config
$envMap['DEVMANAGER_INSTANCE_LABEL'] = 'workspace-conformance'
$envMap['DEVMANAGER_RUNTIME_KIND'] = 'test'
$toolchainBin = [System.IO.Path]::GetDirectoryName($cargoExe)
$gitBin = [System.IO.Path]::GetDirectoryName($gitExe)
$envMap['PATH'] = "$toolchainBin;$gitBin"
$envMap['USERPROFILE'] = [Environment]::GetFolderPath('UserProfile')
$envMap['SystemRoot'] = [Environment]::GetFolderPath('Windows')
$envMap['WINDIR'] = $envMap['SystemRoot']
$envMap['TEMP'] = [string]$run.runRoot
$envMap['TMP'] = [string]$run.runRoot

$capProof = [WorkspaceSmokeOwnedJob]::SelfTestOutputBounds(4096)
Write-Host ("OUTPUT_CAP={0}" -f $capProof)

if ($ProbeGuards) {
    $systemRoot = [Environment]::GetFolderPath('Windows')
    $cmdExe = [System.IO.Path]::GetFullPath((Join-Path $systemRoot 'System32\cmd.exe'))
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cmdExe
    $stallEnv = New-Object 'System.Collections.Generic.Dictionary[string,string]'
    $stallEnv['SystemRoot'] = $systemRoot
    $stallEnv['WINDIR'] = $systemRoot
    $stallEnv['TEMP'] = [string]$run.runRoot
    $stallEnv['TMP'] = [string]$run.runRoot
    $stallEnv['PATH'] = [System.IO.Path]::GetDirectoryName($cmdExe)
    $stall = $null
    try {
        $stall = New-Object WorkspaceSmokeOwnedJob
        $stall.Run($cmdExe, '/c ping -n 60 127.0.0.1', $run.runRoot, $stallEnv, 250, 4096)
        if (-not [bool]$stall.TimedOut) {
            throw "stalled-pipe probe must hit the absolute deadline."
        }
        if (-not [bool]$stall.ActiveProcessZero) {
            throw "stalled-pipe probe did not reach ACTIVE_PROCESS_ZERO."
        }
        Write-Host 'STALLED_PIPE_BOUNDED'
        Write-Host ("ACTIVE_PROCESS_ZERO={0}" -f ([bool]$stall.ActiveProcessZero).ToString().ToLowerInvariant())
    }
    finally {
        if ($null -ne $stall) { $stall.Dispose() }
    }
    Remove-WorkspaceSmokeRunRoot -Run $run -ProtectedRoot $protectedRoot
    Write-Host 'CLEANED=exact-identity'
    Write-Host 'PROBE_GUARDS_OK'
    return
}

Assert-WorkspaceSmokeRunIdentity -Run $run -ProtectedRoot $protectedRoot
$cargoOverride = Join-Path $run.config 'cargo-smoke.toml'
Set-Content -LiteralPath $cargoOverride -Encoding ascii -Value @"
[build]
incremental = false
rustc-wrapper = ""
rustc-workspace-wrapper = ""
"@
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargoOverride
$argumentLine = ('test --offline --locked --config "{0}" --test workspace_conformance --target-dir "{1}" -- --test-threads=1 --nocapture' -f $cargoOverride, $run.target)
$job = $null
try {
    $job = New-Object WorkspaceSmokeOwnedJob
    $job.Run($cargoExe, $argumentLine, $worktreeRoot, $envMap, ($TimeoutSeconds * 1000), [int]$MaxOutputBytes)
    Write-Host ("reason={0}" -f $job.Reason)
    Write-Host ("captured_bytes={0}" -f $job.CapturedBytes)
    Write-Host ("ACTIVE_PROCESS_ZERO={0}" -f ([bool]$job.ActiveProcessZero).ToString().ToLowerInvariant())
    if (-not [bool]$job.ActiveProcessZero) {
        throw "owned Job did not reach ACTIVE_PROCESS_ZERO."
    }
    if ([bool]$job.TimedOut -or [bool]$job.OutputExceeded -or [bool]$job.InvalidUtf8 -or $job.ExitCode -ne 0) {
        throw ("workspace conformance failed reason={0}" -f $job.Reason)
    }
    Write-Host 'WORKSPACE_SMOKE_OK'
    Write-Host 'residue=0'
}
finally {
    if ($null -ne $job) { $job.Dispose() }
    Remove-WorkspaceSmokeRunRoot -Run $run -ProtectedRoot $protectedRoot
    Write-Host 'CLEANED=exact-identity'
}
