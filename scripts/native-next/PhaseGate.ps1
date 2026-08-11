# Phase 0 phase-gate helpers (recipe admission + observe/fail-closed residue).
# The phase command remains observe/fail-closed; bounded helper children use a
# private kill-on-close Job so timeout cleanup cannot orphan PowerShell trees.
# Malicious same-user junction races on evidence dirs are outside
# the Phase 0 accidental-isolation threat model; component/reparse checks still run
# before creation and publication.
# Requires Isolation.ps1 to be dot-sourced first.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-DevManagerPhaseGateRecipeTable {
    return [ordered]@{
        'cargo-version'                 = [string[]]@('--version')
        'cargo-fmt-check'               = [string[]]@('fmt', '--all', '--', '--check')
        'development-isolation-tests'   = [string[]]@(
            'test',
            '--test', 'development_isolation',
            '--', '--test-threads=1'
        )
        'library-tests-serial'          = [string[]]@('test', '--lib', '--', '--test-threads=1')
        'phase-02-host-lock'            = [string[]]@(
            'test',
            '--test', 'host_lock',
            '--', '--nocapture'
        )
        'phase-02-cli-client'           = [string[]]@(
            'test',
            '--test', 'cli_client',
            '--', '--nocapture'
        )
        'phase-02-host-lifecycle'       = [string[]]@(
            'test',
            '--test', 'host_lifecycle',
            '--', '--nocapture'
        )
        'phase-02-host-recovery'        = [string[]]@(
            'test',
            '--test', 'host_recovery',
            '--', '--nocapture'
        )
        'phase-02-diagnostics'          = [string[]]@(
            'test',
            '--test', 'diagnostic_logging',
            '--', '--nocapture'
        )
        'phase-03-process-identity'     = [string[]]@(
            'test',
            '--test', 'process_supervisor',
            'identity::',
            '--', '--nocapture'
        )
        'phase-03-process-job'          = [string[]]@(
            'test',
            '--test', 'process_supervisor',
            'job::',
            '--', '--nocapture'
        )
        'phase-03-process-registry'     = [string[]]@(
            'test',
            '--test', 'process_supervisor',
            'registry::',
            '--', '--nocapture'
        )
        'phase-03-process-launcher'     = [string[]]@(
            'test',
            '--test', 'process_supervisor',
            'launcher::',
            '--', '--nocapture'
        )
        'phase-03-process-supervisor'   = [string[]]@(
            'test',
            '--test', 'process_supervisor',
            '--', '--nocapture'
        )
    }
}

function Assert-DevManagerPhaseName {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Phase
    )

    if ([string]::IsNullOrWhiteSpace($Phase)) {
        throw "Phase name is empty."
    }
    $trimmed = $Phase.Trim()
    if ($trimmed -ne $Phase) {
        throw "Phase name must not have leading or trailing whitespace ('$Phase')."
    }
    if ($trimmed.Length -gt 64) {
        throw "Phase name exceeds 64 characters."
    }
    if ($trimmed -eq '.' -or $trimmed -eq '..') {
        throw "Phase name rejects path traversal segments ('$trimmed')."
    }
    if ($trimmed -match '[\\/:\*\?"<>\|\s]') {
        throw "Phase name must be a single path-safe segment without separators or whitespace ('$trimmed')."
    }
    if ($trimmed -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {
        throw "Phase name must start with alphanumeric and use only [A-Za-z0-9._-] ('$trimmed')."
    }
    return $trimmed
}

function Resolve-DevManagerPhaseGateRecipe {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$Recipe,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    if ([string]::IsNullOrWhiteSpace($Recipe)) {
        throw "Recipe is empty."
    }
    $name = $Recipe.Trim()
    $table = Get-DevManagerPhaseGateRecipeTable
    if (-not $table.Contains($name)) {
        throw "Unknown phase-gate recipe '$name'. Accepted: $((@($table.Keys) -join ', '))."
    }

    if (-not (Test-DevManagerAbsolutePath -LiteralPath $WorktreeRoot)) {
        throw "WorktreeRoot must be fully qualified ('$WorktreeRoot')."
    }
    $worktree = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $worktree

    $cargoTargetDir = [System.IO.Path]::GetFullPath((Join-Path $worktree 'target-native-next'))
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $cargoTargetDir
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $cargoTargetDir -AncestorPath $worktree)) {
        throw "CARGO_TARGET_DIR escapes worktree."
    }

    $cargoCmd = @(Get-Command -Name 'cargo' -All -CommandType Application -ErrorAction SilentlyContinue)
    if ($cargoCmd.Count -eq 0) {
        throw "Unable to resolve PATH cargo.exe for recipe admission."
    }
    $cargoExes = @(
        $cargoCmd |
            Where-Object { [System.IO.Path]::GetFileName([string]$_.Source) -ieq 'cargo.exe' } |
            ForEach-Object { [System.IO.Path]::GetFullPath([string]$_.Source) } |
            ForEach-Object { Normalize-DevManagerPath -LiteralPath $_ } |
            Select-Object -Unique
    )
    if ($cargoExes.Count -eq 0) {
        throw "Unable to resolve PATH cargo.exe for recipe admission."
    }
    if ($cargoExes.Count -ne 1) {
        throw "Ambiguous PATH cargo.exe resolution ($($cargoExes.Count) matches): $($cargoExes -join '; ')"
    }
    $resolved = $cargoExes[0]
    $leaf = [System.IO.Path]::GetFileName($resolved)
    if ($leaf -ine 'cargo.exe') {
        throw "Phase 0 recipes require PATH cargo.exe (got '$resolved')."
    }

    foreach ($install in @(Get-DevManagerSupportedInstallPaths)) {
        if ([string]::IsNullOrWhiteSpace([string]$install)) { continue }
        if ((Normalize-DevManagerPath -LiteralPath $resolved) -eq (Normalize-DevManagerPath -LiteralPath ([string]$install))) {
            throw "Rejecting installed DevManager path masquerading as cargo ('$resolved')."
        }
    }

    $systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) {
        throw 'SystemRoot is unavailable for the explicit phase-gate environment.'
    }
    $tempDirectory = [System.IO.Path]::GetFullPath((Join-Path $worktree '.tmp-phase3-soak'))
    $cargoDirectory = [System.IO.Path]::GetDirectoryName($resolved)
    $environment = [ordered]@{
        SystemRoot       = [string]$systemRoot
        TEMP             = $tempDirectory
        TMP              = $tempDirectory
        PATH             = @(
            [System.IO.Path]::Combine([string]$systemRoot, 'System32'),
            [string]$cargoDirectory
        ) -join ';'
        CARGO_TARGET_DIR = $cargoTargetDir
    }
    $arguments = [string[]]@($table[$name])
    $environmentRemovals = [string[]]@(
        'DEVMANAGER_CONFIG_DIR',
        'DEVMANAGER_APP_IDENTITY'
    )
    if ($name -eq 'library-tests-serial') {
        # The complete lib suite owns its test identity and profile environment.
        $environmentRemovals = [string[]]@(
            'DEVMANAGER_PROFILE',
            'DEVMANAGER_INSTANCE_LABEL',
            'DEVMANAGER_RUNTIME_KIND',
            'DEVMANAGER_CONFIG_DIR',
            'DEVMANAGER_APP_IDENTITY'
        )
    }
    else {
        $environment['DEVMANAGER_INSTANCE_LABEL'] = 'Next'
        $environment['DEVMANAGER_RUNTIME_KIND'] = 'native-next'
        $environment['DEVMANAGER_PROFILE'] = 'native-next-dev'
    }

    return [pscustomobject]@{
        recipe               = $name
        executable           = $resolved
        arguments            = $arguments
        workingDirectory     = [System.IO.Path]::GetFullPath($worktree)
        cargoTargetDir       = $cargoTargetDir
        environment          = $environment
        environmentRemovals  = $environmentRemovals
    }
}

function Assert-DevManagerPhaseGateExecutionPlan {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    if ($null -eq $Plan) {
        throw 'Phase-gate execution plan is null.'
    }
    if ([string]::IsNullOrWhiteSpace([string]$Plan.recipe)) {
        throw 'Phase-gate execution plan is missing recipe.'
    }
    if ($null -eq $Plan.environment) {
        throw 'Phase-gate execution plan is missing environment overrides.'
    }
    if ($null -eq $Plan.environmentRemovals) {
        throw 'Phase-gate execution plan is missing environmentRemovals.'
    }

    foreach ($requiredEnv in @('SystemRoot', 'TEMP', 'TMP', 'PATH', 'CARGO_TARGET_DIR')) {
        if (-not $Plan.environment.Contains($requiredEnv)) {
            throw "Execution plan missing required environment key '$requiredEnv'."
        }
    }

    $removals = [string[]]@($Plan.environmentRemovals)
    if ([string]$Plan.recipe -eq 'library-tests-serial') {
        foreach ($removed in @(
            'DEVMANAGER_PROFILE',
            'DEVMANAGER_INSTANCE_LABEL',
            'DEVMANAGER_RUNTIME_KIND',
            'DEVMANAGER_CONFIG_DIR',
            'DEVMANAGER_APP_IDENTITY'
        )) {
            if ($Plan.environment.Contains($removed)) {
                throw "library-tests-serial must not include a $removed override."
            }
        }
        if (@($Plan.environment.Keys).Count -ne 5) {
            throw "library-tests-serial must declare only explicit system and Cargo environment."
        }
        if (($removals -join ',') -cne 'DEVMANAGER_PROFILE,DEVMANAGER_INSTANCE_LABEL,DEVMANAGER_RUNTIME_KIND,DEVMANAGER_CONFIG_DIR,DEVMANAGER_APP_IDENTITY') {
            throw "library-tests-serial environmentRemovals must clear all DevManager runtime identity (got: $($removals -join ', '))."
        }
    }
    else {
        foreach ($requiredEnv in @('DEVMANAGER_INSTANCE_LABEL', 'DEVMANAGER_RUNTIME_KIND', 'DEVMANAGER_PROFILE')) {
            if (-not $Plan.environment.Contains($requiredEnv)) {
                throw "Execution plan missing required environment key '$requiredEnv'."
            }
        }
        if (@($Plan.environment.Keys).Count -ne 8) {
            throw "Non-library Phase 0 recipes must declare exactly eight explicit environment values."
        }
        if ([string]$Plan.environment['DEVMANAGER_INSTANCE_LABEL'] -ne 'Next') {
            throw "Execution plan must force DEVMANAGER_INSTANCE_LABEL=Next."
        }
        if ([string]$Plan.environment['DEVMANAGER_RUNTIME_KIND'] -ne 'native-next') {
            throw "Execution plan must force DEVMANAGER_RUNTIME_KIND=native-next."
        }
        if ([string]$Plan.environment['DEVMANAGER_PROFILE'] -ne 'native-next-dev') {
            throw "Execution plan must force DEVMANAGER_PROFILE=native-next-dev."
        }
        if (($removals -join ',') -cne 'DEVMANAGER_CONFIG_DIR,DEVMANAGER_APP_IDENTITY') {
            throw "Non-library Phase 0 recipes must remove only inherited config/app identity (got: $($removals -join ', '))."
        }
    }
}

function Set-DevManagerPhaseGateProcessEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.ProcessStartInfo]$StartInfo,
        [Parameter(Mandatory = $true)]
        [object]$Plan
    )

    Assert-DevManagerPhaseGateExecutionPlan -Plan $Plan

    # Start from an empty block. Inherited caller state is never a source of
    # authority for a Cargo, pwsh, or helper child.
    $StartInfo.Environment.Clear()

    foreach ($key in @($Plan.environment.Keys)) {
        $StartInfo.Environment[[string]$key] = [string]$Plan.environment[$key]
    }
}

function Ensure-DevManagerPhaseGateJobType {
    $type = ([System.Management.Automation.PSTypeName]'DevManagerPhaseGateJob').Type
    if ($null -ne $type) { return }
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

public sealed class DevManagerPhaseGateJob : IDisposable
{
    private IntPtr handle;
    private const uint BasicProcessIdListInformationClass = 3;
    private const uint ExtendedLimitInformationClass = 9;
    private const uint KillOnJobClose = 0x00002000;
    private const uint CreateSuspended = 0x00000004;
    private const uint CreateUnicodeEnvironment = 0x00000400;
    private const uint CreateNoWindow = 0x08000000;
    private const uint StartfUseStdHandles = 0x00000100;
    private const uint HandleFlagInherit = 0x00000001;
    private const uint GenericRead = 0x80000000;
    private const uint FileShareRead = 0x00000001;
    private const uint FileShareWrite = 0x00000002;
    private const uint OpenExisting = 3;
    private static readonly IntPtr InvalidHandleValue = new IntPtr(-1);

    [StructLayout(LayoutKind.Sequential)]
    private struct BasicLimitInformation
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
    private struct IoCounters
    {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ExtendedLimitInformation
    {
        public BasicLimitInformation BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SecurityAttributes
    {
        public uint Length;
        public IntPtr SecurityDescriptor;
        public int InheritHandle;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo
    {
        public uint Cb;
        public string Reserved;
        public string Desktop;
        public string Title;
        public uint X;
        public uint Y;
        public uint XSize;
        public uint YSize;
        public uint XCountChars;
        public uint YCountChars;
        public uint FillAttribute;
        public uint Flags;
        public ushort ShowWindow;
        public ushort Reserved2;
        public IntPtr Reserved2Ptr;
        public IntPtr StdInput;
        public IntPtr StdOutput;
        public IntPtr StdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation
    {
        public IntPtr Process;
        public IntPtr Thread;
        public uint ProcessId;
        public uint ThreadId;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(IntPtr job, uint infoClass, ref ExtendedLimitInformation info, uint length);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(IntPtr job, uint infoClass, IntPtr info, uint length, IntPtr returnLength);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CreatePipe(out IntPtr readPipe, out IntPtr writePipe, ref SecurityAttributes attributes, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CreateProcess(
        string applicationName,
        IntPtr commandLine,
        IntPtr processAttributes,
        IntPtr threadAttributes,
        bool inheritHandles,
        uint creationFlags,
        IntPtr environment,
        string currentDirectory,
        ref StartupInfo startupInfo,
        out ProcessInformation processInformation);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateFile(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        ref SecurityAttributes securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    public sealed class SuspendedProcess : IDisposable
    {
        private IntPtr threadHandle;
        public Process Process { get; }
        public StreamReader Stdout { get; }
        public StreamReader Stderr { get; }

        internal SuspendedProcess(Process process, IntPtr thread, StreamReader stdout, StreamReader stderr)
        {
            Process = process;
            threadHandle = thread;
            Stdout = stdout;
            Stderr = stderr;
        }

        public void Resume()
        {
            if (threadHandle == IntPtr.Zero) throw new InvalidOperationException("suspended process thread is already closed");
            var result = ResumeThread(threadHandle);
            if (result == UInt32.MaxValue)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "ResumeThread failed");
            CloseHandle(threadHandle);
            threadHandle = IntPtr.Zero;
        }

        public void Dispose()
        {
            if (threadHandle != IntPtr.Zero)
            {
                CloseHandle(threadHandle);
                threadHandle = IntPtr.Zero;
            }
            GC.SuppressFinalize(this);
        }
    }

    public DevManagerPhaseGateJob()
    {
        handle = CreateJobObject(IntPtr.Zero, null);
        if (handle == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObject failed");
        var limits = new ExtendedLimitInformation();
        limits.BasicLimitInformation.LimitFlags = KillOnJobClose;
        if (!SetInformationJobObject(handle, ExtendedLimitInformationClass, ref limits, (uint)Marshal.SizeOf<ExtendedLimitInformation>()))
        {
            var error = new Win32Exception(Marshal.GetLastWin32Error(), "SetInformationJobObject failed");
            CloseHandle(handle);
            handle = IntPtr.Zero;
            throw error;
        }
    }

    public void Assign(Process process)
    {
        if (process == null || process.HasExited) throw new InvalidOperationException("cannot assign an exited process to the owned Job");
        if (!AssignProcessToJobObject(handle, process.Handle))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "AssignProcessToJobObject failed");
    }

    public void Terminate()
    {
        if (handle != IntPtr.Zero && !TerminateJobObject(handle, 124))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "TerminateJobObject failed");
    }

    public static void TerminateUnassigned(Process process)
    {
        if (process == null || process.HasExited) return;
        if (!TerminateProcess(process.Handle, 125))
            throw new Win32Exception(Marshal.GetLastWin32Error(), "TerminateProcess(handle) failed before Job assignment");
    }

    public uint ActiveProcessCount(long absoluteDeadlineTicks)
    {
        var capacity = 16;
        for (var attempt = 0; attempt < 8; attempt++)
        {
            if (Stopwatch.GetTimestamp() >= absoluteDeadlineTicks)
                throw new TimeoutException("Job active member inspection exceeded its absolute deadline");
            var bytes = checked(8 + (IntPtr.Size * capacity));
            var buffer = Marshal.AllocHGlobal(bytes);
            try
            {
                if (QueryInformationJobObject(handle, BasicProcessIdListInformationClass, buffer, (uint)bytes, IntPtr.Zero))
                {
                    if (Stopwatch.GetTimestamp() >= absoluteDeadlineTicks)
                        throw new TimeoutException("Job active member inspection exceeded its absolute deadline");
                    return (uint)Marshal.ReadInt32(buffer, 4);
                }
                var error = Marshal.GetLastWin32Error();
                if (error == 234)
                {
                    if (Stopwatch.GetTimestamp() >= absoluteDeadlineTicks)
                        throw new TimeoutException("Job active member inspection exceeded its absolute deadline");
                    capacity = Math.Min(capacity * 2, 4096);
                    continue;
                }
                throw new Win32Exception(error, "QueryInformationJobObject(active members) failed");
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        throw new InvalidOperationException("QueryInformationJobObject(active members) exceeded its retry bound");
    }

    private static string QuoteArgument(string value)
    {
        if (String.IsNullOrEmpty(value)) return "\"\"";
        if (!value.Any(character => Char.IsWhiteSpace(character) || character == '\"')) return value;
        var result = new StringBuilder("\"");
        var slashes = 0;
        foreach (var character in value)
        {
            if (character == '\\') slashes++;
            else if (character == '\"')
            {
                result.Append('\\', slashes * 2 + 1);
                result.Append('\"');
                slashes = 0;
            }
            else
            {
                result.Append('\\', slashes);
                result.Append(character);
                slashes = 0;
            }
        }
        result.Append('\\', slashes * 2);
        result.Append('\"');
        return result.ToString();
    }

    private static string CommandLine(ProcessStartInfo info)
    {
        var values = new List<string> { info.FileName };
        if (info.ArgumentList.Count > 0) values.AddRange(info.ArgumentList);
        else if (!String.IsNullOrWhiteSpace(info.Arguments)) values.Add(info.Arguments);
        var command = QuoteArgument(values[0]);
        for (var index = 1; index < values.Count; index++)
        {
            command += " " + (index == values.Count - 1 && info.ArgumentList.Count == 0
                ? values[index]
                : QuoteArgument(values[index]));
        }
        return command;
    }

    public static SuspendedProcess StartSuspended(ProcessStartInfo info)
    {
        if (info == null) throw new ArgumentNullException(nameof(info));
        if (info.UseShellExecute || !info.RedirectStandardOutput || !info.RedirectStandardError)
            throw new InvalidOperationException("bounded phase-gate launch requires redirected, shell-free output");
        var attributes = new SecurityAttributes { Length = (uint)Marshal.SizeOf<SecurityAttributes>(), InheritHandle = 1 };
        IntPtr stdoutRead = IntPtr.Zero, stdoutWrite = IntPtr.Zero;
        IntPtr stderrRead = IntPtr.Zero, stderrWrite = IntPtr.Zero;
        IntPtr stdin = IntPtr.Zero;
        IntPtr process = IntPtr.Zero, thread = IntPtr.Zero;
        IntPtr commandLine = IntPtr.Zero, environment = IntPtr.Zero;
        var success = false;
        try
        {
            if (!CreatePipe(out stdoutRead, out stdoutWrite, ref attributes, 0))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreatePipe(stdout) failed");
            if (!SetHandleInformation(stdoutRead, HandleFlagInherit, 0))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SetHandleInformation(stdout) failed");
            if (!CreatePipe(out stderrRead, out stderrWrite, ref attributes, 0))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreatePipe(stderr) failed");
            if (!SetHandleInformation(stderrRead, HandleFlagInherit, 0))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "SetHandleInformation(stderr) failed");
            stdin = CreateFile("NUL", GenericRead, FileShareRead | FileShareWrite, ref attributes, OpenExisting, 0, IntPtr.Zero);
            if (stdin == IntPtr.Zero || stdin == InvalidHandleValue)
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateFile(NUL) failed");
            var command = CommandLine(info);
            commandLine = Marshal.StringToHGlobalUni(command);
            var environmentEntries = new List<string>();
            foreach (var entry in info.Environment) environmentEntries.Add(entry.Key + "=" + entry.Value);
            environmentEntries.Sort(StringComparer.OrdinalIgnoreCase);
            environment = Marshal.StringToHGlobalUni(String.Join("\0", environmentEntries) + "\0\0");
            var startup = new StartupInfo
            {
                Cb = (uint)Marshal.SizeOf<StartupInfo>(),
                Flags = StartfUseStdHandles,
                StdInput = stdin,
                StdOutput = stdoutWrite,
                StdError = stderrWrite,
            };
            var workingDirectory = String.IsNullOrWhiteSpace(info.WorkingDirectory) ? null : info.WorkingDirectory;
            if (!CreateProcess(info.FileName, commandLine, IntPtr.Zero, IntPtr.Zero, true,
                    CreateSuspended | CreateUnicodeEnvironment | CreateNoWindow, environment,
                    workingDirectory, ref startup, out var processInformation))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcess(suspended) failed");
            process = processInformation.Process;
            thread = processInformation.Thread;
            CloseHandle(stdoutWrite); stdoutWrite = IntPtr.Zero;
            CloseHandle(stderrWrite); stderrWrite = IntPtr.Zero;
            CloseHandle(stdin); stdin = IntPtr.Zero;
            var child = Process.GetProcessById((int)processInformation.ProcessId);
            var stdoutHandle = new SafeFileHandle(stdoutRead, true); stdoutRead = IntPtr.Zero;
            var stderrHandle = new SafeFileHandle(stderrRead, true); stderrRead = IntPtr.Zero;
            var stdout = new StreamReader(new FileStream(stdoutHandle, FileAccess.Read, 8192, false), Encoding.UTF8, false, 8192, false);
            var stderr = new StreamReader(new FileStream(stderrHandle, FileAccess.Read, 8192, false), Encoding.UTF8, false, 8192, false);
            var suspended = new SuspendedProcess(child, thread, stdout, stderr);
            CloseHandle(process); process = IntPtr.Zero;
            success = true;
            thread = IntPtr.Zero;
            return suspended;
        }
        finally
        {
            if (!success && process != IntPtr.Zero)
            {
                TerminateProcess(process, 127);
                CloseHandle(process);
                process = IntPtr.Zero;
            }
            if (thread != IntPtr.Zero) CloseHandle(thread);
            if (stdoutRead != IntPtr.Zero) CloseHandle(stdoutRead);
            if (stdoutWrite != IntPtr.Zero) CloseHandle(stdoutWrite);
            if (stderrRead != IntPtr.Zero) CloseHandle(stderrRead);
            if (stderrWrite != IntPtr.Zero) CloseHandle(stderrWrite);
            if (stdin != IntPtr.Zero && stdin != InvalidHandleValue) CloseHandle(stdin);
            if (commandLine != IntPtr.Zero) Marshal.FreeHGlobal(commandLine);
            if (environment != IntPtr.Zero) Marshal.FreeHGlobal(environment);
        }
    }

    public void Dispose()
    {
        if (handle != IntPtr.Zero) { CloseHandle(handle); handle = IntPtr.Zero; }
        GC.SuppressFinalize(this);
    }
}
'@
}

function Assert-DevManagerPhase3FinalUnionDocument {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Document
    )

    if ($null -eq $Document) { throw 'final union document is missing.' }
    $numericTypes = [Type[]]@(
        [byte], [sbyte], [int16], [uint16], [int32], [uint32],
        [int64], [uint64], [single], [double], [decimal]
    )
    foreach ($property in @('schemaVersion', 'iterations', 'completedCycles')) {
        $entry = $Document.PSObject.Properties[$property]
        if ($null -eq $entry -or $null -eq $entry.Value) {
            throw "final union document is missing $property."
        }
        if ($entry.Value.GetType() -notin $numericTypes -or
            [double]::IsNaN([double]$entry.Value) -or
            [double]::IsInfinity([double]$entry.Value) -or
            [math]::Truncate([double]$entry.Value) -ne [double]$entry.Value) {
            throw "final union $property must be an exact numeric value."
        }
    }
    if ([int64]$Document.schemaVersion -ne 1) {
        throw "final union schemaVersion must equal 1 exactly."
    }
    if ([int64]$Document.iterations -ne 100) {
        throw "final union iterations must equal 100 exactly."
    }
    if ([int64]$Document.completedCycles -ne 100) {
        throw "final union completedCycles must equal 100 exactly."
    }
    foreach ($property in @('status', 'jobZero', 'releaseEligible', 'realLifecycle')) {
        if ($null -eq $Document.PSObject.Properties[$property]) {
            throw "final union document is missing $property."
        }
    }
    if ([string]$Document.status -ne 'passed' -or
        $Document.jobZero -ne $true -or
        $Document.releaseEligible -ne $true -or
        $Document.realLifecycle -ne $true) {
        throw 'final union document did not prove passed, Job-zero, real-lifecycle release eligibility.'
    }
    return $Document
}

function Invoke-DevManagerPhaseGateBoundedCommand {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.ProcessStartInfo]$StartInfo,
        [int]$TimeoutMilliseconds = 120000,
        [int]$StdoutBytes = 256KB,
        [int]$StderrBytes = 64KB
    )

    Ensure-DevManagerPhaseGateJobType
    $process = $null
    $launch = $null
    $stdout = [pscustomobject]@{ reader = $null; task = $null; buffer = (New-Object char[] 8192); text = [Text.StringBuilder]::new(); totalBytes = 0L; truncated = $false; done = $false }
    $stderr = [pscustomobject]@{ reader = $null; task = $null; buffer = (New-Object char[] 8192); text = [Text.StringBuilder]::new(); totalBytes = 0L; truncated = $false; done = $false }
    $job = [DevManagerPhaseGateJob]::new()
    $started = $false
    $jobAssigned = $false
    $deadline = [Diagnostics.Stopwatch]::GetTimestamp() + [int64]($TimeoutMilliseconds * [Diagnostics.Stopwatch]::Frequency / 1000)
    $cleanupDeadline = $deadline
    $cleanupErrors = [System.Collections.Generic.List[string]]::new()
    $closeReaders = {
        foreach ($state in @($stdout, $stderr)) {
            if ($null -eq $state.reader) { continue }
            try { $state.reader.Dispose() }
            catch { [void]$cleanupErrors.Add("$($state -eq $stdout ? 'stdout' : 'stderr') reader close failed: $($_.Exception.Message)") }
            $state.reader = $null
        }
    }
    $drainReaders = {
        while ($started -and -not (($stdout.done -or $null -eq $stdout.task) -and
                ($stderr.done -or $null -eq $stderr.task) -and $process.HasExited)) {
            $remaining = [int](($cleanupDeadline - [Diagnostics.Stopwatch]::GetTimestamp()) * 1000 / [Diagnostics.Stopwatch]::Frequency)
            if ($remaining -le 0) { return $false }
            $tasks = @(@($stdout.task, $stderr.task) | Where-Object { $null -ne $_ -and -not $_.IsCompleted })
            if ($tasks.Count -gt 0) { [void][Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]$tasks, [Math]::Min(1000, $remaining)) }
            if ($started -and -not $process.HasExited) { [void]$process.WaitForExit([Math]::Min(1000, $remaining)) }
            foreach ($state in @($stdout, $stderr)) {
                if ($state.done -or $null -eq $state.task -or -not $state.task.IsCompleted) { continue }
                try { $count = $state.task.GetAwaiter().GetResult() }
                catch { $state.done = $true; continue }
                if ($count -eq 0) { $state.done = $true; continue }
                $state.totalBytes += [Text.Encoding]::UTF8.GetByteCount($state.buffer, 0, $count)
                $cap = if ($state -eq $stdout) { $StdoutBytes } else { $StderrBytes }
                if ($state.text.Length -lt $cap) { [void]$state.text.Append($state.buffer, 0, [Math]::Min($count, $cap - $state.text.Length)) }
                if ($state.totalBytes -gt $cap) { $state.truncated = $true }
                if ($null -eq $state.reader) { $state.done = $true; continue }
                $state.task = $state.reader.ReadAsync($state.buffer, 0, $state.buffer.Length)
            }
        }
        return (-not $started -or
            (($stdout.done -or $null -eq $stdout.task) -and
                ($stderr.done -or $null -eq $stderr.task) -and $process.HasExited))
    }
    $terminateOwned = {
        try {
            if (-not $started) { return $null }
            if ($jobAssigned) {
                # The root may already have exited while a grandchild remains;
                # terminate the owned Job based on membership, not root PID.
                $job.Terminate()
            }
            elseif (-not $process.HasExited) {
                [DevManagerPhaseGateJob]::TerminateUnassigned($process)
            }
            return $null
        }
        catch { return [string]$_.Exception.Message }
    }
    try {
        $launch = [DevManagerPhaseGateJob]::StartSuspended($StartInfo)
        $process = $launch.Process
        $started = $true
        $job.Assign($process)
        $jobAssigned = $true
        $stdout.reader = $launch.Stdout
        $stderr.reader = $launch.Stderr
        $stdout.task = $stdout.reader.ReadAsync($stdout.buffer, 0, $stdout.buffer.Length)
        $stderr.task = $stderr.reader.ReadAsync($stderr.buffer, 0, $stderr.buffer.Length)
        $launch.Resume()
        while (-not ($stdout.done -and $stderr.done -and $process.HasExited)) {
            $remaining = [int](($deadline - [Diagnostics.Stopwatch]::GetTimestamp()) * 1000 / [Diagnostics.Stopwatch]::Frequency)
            if ($remaining -le 0) {
                throw 'typed-unavailable: bounded phase-gate command exceeded its absolute deadline; owned Job termination is required.'
            }
            $tasks = @(@($stdout.task, $stderr.task) | Where-Object { $null -ne $_ -and -not $_.IsCompleted })
            if ($tasks.Count -gt 0) { [void][Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]$tasks, [Math]::Max(1, $remaining)) }
            foreach ($state in @($stdout, $stderr)) {
                if ($state.done -or -not $state.task.IsCompleted) { continue }
                try { $count = $state.task.GetAwaiter().GetResult() }
                catch { $state.done = $true; continue }
                if ($count -eq 0) { $state.done = $true; continue }
                $state.totalBytes += [Text.Encoding]::UTF8.GetByteCount($state.buffer, 0, $count)
                $cap = if ($state -eq $stdout) { $StdoutBytes } else { $StderrBytes }
                if ($state.text.Length -lt $cap) { [void]$state.text.Append($state.buffer, 0, [Math]::Min($count, $cap - $state.text.Length)) }
                if ($state.totalBytes -gt $cap) { $state.truncated = $true }
                $state.task = $state.reader.ReadAsync($state.buffer, 0, $state.buffer.Length)
            }
        }
        if ($stdout.truncated -or $stderr.truncated) { throw 'bounded phase-gate command exceeded its output cap.' }
        return [pscustomobject]@{
            ExitCode = [int]$process.ExitCode
            Stdout = $stdout.text.ToString()
            Stderr = $stderr.text.ToString()
            StdoutBytes = $stdout.totalBytes
            StderrBytes = $stderr.totalBytes
        }
    }
    finally {
        try {
            $terminationError = & $terminateOwned
            if (-not [string]::IsNullOrWhiteSpace([string]$terminationError)) {
                [void]$cleanupErrors.Add("owned Job termination failed: $terminationError")
            }
            & $closeReaders
            try {
                $joined = [bool](& $drainReaders)
            }
            catch {
                $joined = $false
                [void]$cleanupErrors.Add("capped reader join failed: $($_.Exception.Message)")
            }
            if (-not $joined) {
                # A first termination can race process assignment/exit.  Make
                # one more bounded kill-and-drain attempt before inspecting or
                # closing the Job; a reader JoinHandle is never abandoned.
                $terminationError = & $terminateOwned
                if (-not [string]::IsNullOrWhiteSpace([string]$terminationError)) {
                    [void]$cleanupErrors.Add("owned Job retry termination failed: $terminationError")
                }
                try {
                    $joined = [bool](& $drainReaders)
                }
                catch {
                    $joined = $false
                    [void]$cleanupErrors.Add("capped reader retry join failed: $($_.Exception.Message)")
                }
            }
            if (-not $joined) {
                [void]$cleanupErrors.Add('owned Job child/readers did not join before the cleanup deadline.')
            }
            try {
                $activeProcesses = [uint32]$job.ActiveProcessCount($cleanupDeadline)
                if ($activeProcesses -ne 0) {
                    $terminationError = & $terminateOwned
                    if (-not [string]::IsNullOrWhiteSpace([string]$terminationError)) {
                        [void]$cleanupErrors.Add("owned Job final termination failed: $terminationError")
                    }
                    $activeProcesses = [uint32]$job.ActiveProcessCount($cleanupDeadline)
                    if ($activeProcesses -ne 0) {
                        [void]$cleanupErrors.Add("owned Job retained $activeProcesses active process(es) after termination.")
                    }
                }
            }
            catch {
                [void]$cleanupErrors.Add("owned Job member inspection failed: $($_.Exception.Message)")
            }
        }
        catch { [void]$cleanupErrors.Add("owned Job cleanup failed: $($_.Exception.Message)") }
        finally {
            if ($null -ne $launch) { $launch.Dispose() }
            if ($null -ne $job) { $job.Dispose() }
            if ($null -ne $process) { $process.Dispose() }
        }
        if ($cleanupErrors.Count -gt 0) { throw ($cleanupErrors -join '; ') }
    }
}

function New-DevManagerPhaseGateRunDirectory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Phase,
        [Parameter(Mandatory = $true)]
        [string]$EvidenceRoot,
        [Parameter(Mandatory = $true)]
        [string]$ProtectedProductionRoot
    )

    $phaseName = Assert-DevManagerPhaseName -Phase $Phase
    $runsRoot = [System.IO.Path]::GetFullPath((Join-Path $EvidenceRoot "$phaseName\runs"))
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runsRoot `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot

    $runId = [guid]::NewGuid().ToString('N')
    $runDirectory = [System.IO.Path]::GetFullPath((Join-Path $runsRoot $runId))
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runDirectory `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot

    New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $runDirectory `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $EvidenceRoot
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory

    return [pscustomobject]@{
        phase        = $phaseName
        runId        = $runId
        runDirectory = $runDirectory
    }
}

function Test-DevManagerWorktreeTargetExecutable {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$ExecutablePath,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    if ([string]::IsNullOrWhiteSpace($ExecutablePath)) { return $false }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $ExecutablePath)) { return $false }
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $ExecutablePath -AncestorPath $WorktreeRoot)) {
        return $false
    }
    $leaf = [System.IO.Path]::GetFileName($ExecutablePath)
    if ($leaf -imatch '^(cargo|rustc|rustdoc|clippy-driver)(\.exe)?$') { return $true }
    foreach ($part in (Get-DevManagerNormalizedPathComponents -LiteralPath $ExecutablePath)) {
        if ($part -like 'target*') { return $true }
    }
    return $false
}

function Get-DevManagerProcessInventoryEntry {
    param(
        [Parameter(Mandatory = $true)]
        [object]$CimProcess,
        [switch]$RequireCompleteIdentity
    )

    $rawPath = $null
    if ($null -ne $CimProcess.PSObject.Properties['ExecutablePath']) {
        $rawPath = $CimProcess.ExecutablePath
    }
    $creation = $null
    if ($null -ne $CimProcess.PSObject.Properties['CreationDate']) {
        $creation = $CimProcess.CreationDate
    }
    if ([string]::IsNullOrWhiteSpace([string]$rawPath) -or [string]::IsNullOrWhiteSpace([string]$creation)) {
        if ($RequireCompleteIdentity) {
            throw "Missing executable path or CreationDate for attributable process Id=$($CimProcess.ProcessId)."
        }
        return $null
    }

    $parentId = [uint32]0
    if ($null -ne $CimProcess.PSObject.Properties['ParentProcessId'] -and $null -ne $CimProcess.ParentProcessId) {
        if (Test-DevManagerIntegralNumber -Value $CimProcess.ParentProcessId) {
            $parentId = [uint32]$CimProcess.ParentProcessId
        }
    }

    try {
        $normalized = Normalize-DevManagerPath -LiteralPath ([string]$rawPath)
    }
    catch {
        if ($RequireCompleteIdentity) {
            throw "Unnormalizable executable path for attributable process Id=$($CimProcess.ProcessId)."
        }
        return $null
    }

    $creationText = [string]$creation
    if ($creation -is [DateTimeOffset]) {
        $creationText = ([DateTimeOffset]$creation).UtcDateTime.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    }
    elseif ($creation -is [DateTime]) {
        $creationText = ([DateTime]$creation).ToUniversalTime().ToString('o', [Globalization.CultureInfo]::InvariantCulture)
    }

    return [pscustomobject]@{
        processId       = [uint32]$CimProcess.ProcessId
        executablePath  = [string]$normalized
        creationDate    = $creationText
        parentProcessId = $parentId
    }
}

function ConvertTo-DevManagerProcessCreationUtc {
    param(
        [Parameter(Mandatory = $true)]
        [object]$CreationDate
    )

    if ($CreationDate -is [DateTimeOffset]) {
        return ([DateTimeOffset]$CreationDate).UtcDateTime
    }
    if ($CreationDate -is [DateTime]) {
        return ([DateTime]$CreationDate).ToUniversalTime()
    }

    $text = [string]$CreationDate
    if ([string]::IsNullOrWhiteSpace($text)) {
        throw 'Process CreationDate is empty.'
    }

    $dmtf = [regex]::Match(
        $text,
        '^(?<timestamp>\d{14}\.\d{6})(?<sign>[+-])(?<offset>\d{3})$'
    )
    if ($dmtf.Success) {
        try {
            $wallClock = [DateTime]::ParseExact(
                $dmtf.Groups['timestamp'].Value,
                'yyyyMMddHHmmss.ffffff',
                [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::None
            )
            $offsetMinutes = [int]$dmtf.Groups['offset'].Value
            if ($dmtf.Groups['sign'].Value -eq '-') {
                $offsetMinutes = -$offsetMinutes
            }
            return ([DateTimeOffset]::new(
                    $wallClock,
                    [TimeSpan]::FromMinutes($offsetMinutes)
                )).UtcDateTime
        }
        catch {
            throw "Invalid DMTF process CreationDate '$text'."
        }
    }

    $parsed = [DateTimeOffset]::MinValue
    $styles = [Globalization.DateTimeStyles]::AllowWhiteSpaces -bor [Globalization.DateTimeStyles]::RoundtripKind
    if ([DateTimeOffset]::TryParse(
            $text,
            [Globalization.CultureInfo]::InvariantCulture,
            $styles,
            [ref]$parsed
        )) {
        return $parsed.UtcDateTime
    }

    throw "Unparseable process CreationDate '$text'."
}

function Get-DevManagerDisposableDevelopmentProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    $worktree = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $matched = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $CimProcesses) {
        $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $entry) { continue }
        if (-not (Test-DevManagerWorktreeTargetExecutable -ExecutablePath ([string]$entry.executablePath) -WorktreeRoot $worktree)) {
            continue
        }
        $matched.Add($entry)
    }
    return @($matched | Sort-Object processId, executablePath, creationDate, parentProcessId)
}

function Get-DevManagerProcessInventory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    $processes = Get-DevManagerDisposableDevelopmentProcesses -WorktreeRoot $WorktreeRoot -CimProcesses $CimProcesses
    return [pscustomobject]@{
        schemaVersion = [int]1
        capturedAtUtc = [DateTime]::UtcNow.ToString('o')
        worktreeRoot  = Normalize-DevManagerPath -LiteralPath $WorktreeRoot
        processes     = [object[]]@($processes)
    }
}

function Get-DevManagerProcessInventoryIdentityKey {
    param([Parameter(Mandatory = $true)][object]$Process)
    $path = Normalize-DevManagerPath -LiteralPath ([string]$Process.executablePath)
    return "pid=$([uint32]$Process.processId);exe=$path;start=$([string]$Process.creationDate)"
}

function Get-DevManagerObservedProcessIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [uint32]$ProcessId,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses,
        [switch]$RequireCompleteIdentity
    )

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $match = @($CimProcesses | Where-Object {
            $null -ne $_.ProcessId -and [uint32]$_.ProcessId -eq $ProcessId
        } | Select-Object -First 1)
    if ($match.Count -eq 0) {
        if ($RequireCompleteIdentity) {
            throw "Unable to locate CIM identity for process Id=$ProcessId."
        }
        return $null
    }
    return Get-DevManagerProcessInventoryEntry -CimProcess $match[0] -RequireCompleteIdentity:$RequireCompleteIdentity
}

function Get-DevManagerRefreshedCimProcess {
    param(
        [Parameter(Mandatory = $true)]
        [uint32]$ProcessId
    )

    try {
        $refreshRows = @(Get-CimInstance -ClassName Win32_Process -Filter ("ProcessId = {0}" -f $ProcessId) -ErrorAction Stop)
    }
    catch {
        throw "CIM lookup failed while refreshing attributable process Id=${ProcessId}: $($_.Exception.Message)"
    }
    if ($refreshRows.Count -eq 0) { return $null }
    if ($refreshRows.Count -ne 1) {
        throw "Ambiguous CIM refresh for attributable process Id=${ProcessId}."
    }

    $refreshed = $refreshRows[0]
    if ($null -eq $refreshed.ProcessId -or -not (Test-DevManagerIntegralNumber -Value $refreshed.ProcessId) -or [uint32]$refreshed.ProcessId -ne $ProcessId) {
        throw "Mismatched ProcessId for refreshed attributable process Id=${ProcessId}."
    }
    return $refreshed
}

function Update-DevManagerObservedProcessTree {
    param(
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [System.Collections.Generic.HashSet[uint32]]$TrackedPids,
        [Parameter(Mandatory = $true)]
        [DateTime]$AttributionFloorUtc,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[uint32, DateTime]]$LineageEndExclusiveByPid,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    $AttributionFloorUtc = $AttributionFloorUtc.ToUniversalTime()

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    # PIDs are only traversal links. ObservedByKey remains the generation-aware
    # authority (PID + executable + creation time) for live residue decisions.
    $lineagePids = New-Object 'System.Collections.Generic.HashSet[uint32]'
    foreach ($trackedPid in $TrackedPids) {
        $null = $lineagePids.Add([uint32]$trackedPid)
    }

    # Windows can reuse a PID while older processes still retain that value as
    # ParentProcessId. Keep the newest observed generation start per PID so a
    # candidate must be temporally possible for both this run and its parent.
    $latestCreationByPid = @{}
    foreach ($observed in $ObservedByKey.Values) {
        $observedPid = [string][uint32]$observed.processId
        $observedCreationUtc = ConvertTo-DevManagerProcessCreationUtc -CreationDate $observed.creationDate
        if (-not $latestCreationByPid.ContainsKey($observedPid) -or $observedCreationUtc -gt $latestCreationByPid[$observedPid]) {
            $latestCreationByPid[$observedPid] = $observedCreationUtc
        }
    }

    # A tracked PID can later be reused by a process outside the admitted
    # lineage. Its start is an exclusive upper bound for children of the old
    # generation, and the bound must survive later polls after it exits.
    foreach ($proc in $CimProcesses) {
        if ($null -eq $proc.ProcessId -or $null -eq $proc.ParentProcessId) { continue }
        if (-not (Test-DevManagerIntegralNumber -Value $proc.ProcessId) -or -not (Test-DevManagerIntegralNumber -Value $proc.ParentProcessId)) { continue }
        $reusedPid = [uint32]$proc.ProcessId
        if (-not $lineagePids.Contains($reusedPid)) { continue }
        $reusedEntry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $reusedEntry) { continue }
        $reusedKey = Get-DevManagerProcessInventoryIdentityKey -Process $reusedEntry
        if ($ObservedByKey.ContainsKey($reusedKey)) { continue }
        if ($lineagePids.Contains([uint32]$reusedEntry.parentProcessId)) { continue }
        $reusedCreationUtc = ConvertTo-DevManagerProcessCreationUtc -CreationDate $reusedEntry.creationDate
        if (-not $LineageEndExclusiveByPid.ContainsKey($reusedPid) -or $reusedCreationUtc -lt $LineageEndExclusiveByPid[$reusedPid]) {
            $LineageEndExclusiveByPid[$reusedPid] = $reusedCreationUtc
        }
    }

    $changed = $true
    while ($changed) {
        $changed = $false
        foreach ($proc in $CimProcesses) {
            if ($null -eq $proc.ProcessId -or $null -eq $proc.ParentProcessId) { continue }
            if (-not (Test-DevManagerIntegralNumber -Value $proc.ParentProcessId)) { continue }
            $pidValue = [uint32]$proc.ProcessId
            $parentId = [uint32]$proc.ParentProcessId
            if (-not $lineagePids.Contains($parentId)) { continue }

            $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
            if ($null -eq $entry) {
                # Snapshot may tear down with null ExecutablePath/CreationDate after exit.
                # Confirm via a fresh PID-specific lookup before failing closed.
                $refreshed = Get-DevManagerRefreshedCimProcess -ProcessId $pidValue
                if ($null -eq $refreshed) {
                    # The process exited, but descendants from this same CIM snapshot
                    # or a later quiet-window poll can outlive it. Keep a gate-lifetime
                    # lineage tombstone; ObservedByKey remains the live authority.
                    $null = $TrackedPids.Add($pidValue)
                    if ($lineagePids.Add($pidValue)) { $changed = $true }
                    continue
                }
                if ($null -eq $refreshed.PSObject.Properties['ParentProcessId'] -or -not (Test-DevManagerIntegralNumber -Value $refreshed.ParentProcessId)) {
                    throw "Missing or invalid ParentProcessId for refreshed attributable process Id=${pidValue}."
                }
                if (-not $lineagePids.Contains([uint32]$refreshed.ParentProcessId)) {
                    # PID reuse under a different parent is not live attributable,
                    # but the old snapshot generation may have surviving children.
                    $reusedEntry = Get-DevManagerProcessInventoryEntry -CimProcess $refreshed -RequireCompleteIdentity
                    $reusedCreationUtc = ConvertTo-DevManagerProcessCreationUtc -CreationDate $reusedEntry.creationDate
                    if (-not $LineageEndExclusiveByPid.ContainsKey($pidValue) -or $reusedCreationUtc -lt $LineageEndExclusiveByPid[$pidValue]) {
                        $LineageEndExclusiveByPid[$pidValue] = $reusedCreationUtc
                    }
                    $null = $TrackedPids.Add($pidValue)
                    if ($lineagePids.Add($pidValue)) { $changed = $true }
                    continue
                }
                $entry = Get-DevManagerProcessInventoryEntry -CimProcess $refreshed -RequireCompleteIdentity
            }

            $parentId = [uint32]$entry.parentProcessId
            $candidateCreationUtc = ConvertTo-DevManagerProcessCreationUtc -CreationDate $entry.creationDate
            $minimumCreationUtc = $AttributionFloorUtc
            $parentGenerationKey = [string]$parentId
            if ($latestCreationByPid.ContainsKey($parentGenerationKey) -and $latestCreationByPid[$parentGenerationKey] -gt $minimumCreationUtc) {
                $minimumCreationUtc = $latestCreationByPid[$parentGenerationKey]
            }
            if ($candidateCreationUtc -lt $minimumCreationUtc) {
                continue
            }
            if ($LineageEndExclusiveByPid.ContainsKey($parentId) -and $candidateCreationUtc -ge $LineageEndExclusiveByPid[$parentId]) {
                continue
            }

            $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
            if ($ObservedByKey.ContainsKey($key)) { continue }
            $ObservedByKey[$key] = $entry
            $candidatePidKey = [string]$pidValue
            if ($LineageEndExclusiveByPid.ContainsKey($pidValue) -and $candidateCreationUtc -gt $LineageEndExclusiveByPid[$pidValue]) {
                # A later generation of this PID has re-entered the admitted
                # lineage. Its own start becomes the lower bound for children,
                # so the intervening unrelated generation's upper bound is done.
                $null = $LineageEndExclusiveByPid.Remove($pidValue)
            }
            if (-not $latestCreationByPid.ContainsKey($candidatePidKey) -or $candidateCreationUtc -gt $latestCreationByPid[$candidatePidKey]) {
                $latestCreationByPid[$candidatePidKey] = $candidateCreationUtc
            }
            $null = $TrackedPids.Add($pidValue)
            $null = $lineagePids.Add($pidValue)
            $changed = $true
        }
    }
}

function Get-DevManagerPhaseGateResidueProcesses {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $beforeKeys = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($proc in @($BeforeProcesses)) {
        $null = $beforeKeys.Add((Get-DevManagerProcessInventoryIdentityKey -Process $proc))
    }

    $liveByKey = @{}
    $refreshedEntries = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $CimProcesses) {
        if ($null -eq $proc.ProcessId) { continue }
        $entry = Get-DevManagerProcessInventoryEntry -CimProcess $proc
        if ($null -eq $entry) {
            $observedPid = @($ObservedByKey.Values | Where-Object {
                    [uint32]$_.processId -eq [uint32]$proc.ProcessId
                })
            if ($observedPid.Count -eq 0) { continue }
            $refreshed = Get-DevManagerRefreshedCimProcess -ProcessId ([uint32]$proc.ProcessId)
            if ($null -eq $refreshed) { continue }
            $entry = Get-DevManagerProcessInventoryEntry -CimProcess $refreshed -RequireCompleteIdentity
            $refreshedEntries.Add($entry)
        }
        $liveByKey[(Get-DevManagerProcessInventoryIdentityKey -Process $entry)] = $entry
    }

    $residue = New-Object System.Collections.Generic.List[object]
    foreach ($key in @($ObservedByKey.Keys)) {
        if ($liveByKey.ContainsKey($key)) {
            $residue.Add($liveByKey[$key])
        }
    }
    foreach ($entry in @(Get-DevManagerDisposableDevelopmentProcesses -WorktreeRoot $WorktreeRoot -CimProcesses $CimProcesses)) {
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if ($beforeKeys.Contains($key)) { continue }
        if ($ObservedByKey.ContainsKey($key)) { continue }
        $residue.Add($entry)
    }
    foreach ($entry in $refreshedEntries) {
        if (-not (Test-DevManagerWorktreeTargetExecutable -ExecutablePath ([string]$entry.executablePath) -WorktreeRoot $WorktreeRoot)) {
            continue
        }
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if ($beforeKeys.Contains($key)) { continue }
        if ($ObservedByKey.ContainsKey($key)) { continue }
        $residue.Add($entry)
    }

    $unique = New-Object 'System.Collections.Generic.Dictionary[string, object]'
    foreach ($entry in $residue) {
        $key = Get-DevManagerProcessInventoryIdentityKey -Process $entry
        if (-not $unique.ContainsKey($key)) { $unique[$key] = $entry }
    }
    return @($unique.Values | Sort-Object processId, executablePath, creationDate, parentProcessId)
}

function Wait-DevManagerPhaseGateQuietWindow {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[string, object]]$ObservedByKey,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [System.Collections.Generic.HashSet[uint32]]$TrackedPids,
        [Parameter(Mandatory = $true)]
        [DateTime]$AttributionFloorUtc,
        [Parameter(Mandatory = $true)]
        [System.Collections.Generic.Dictionary[uint32, DateTime]]$LineageEndExclusiveByPid,
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [int]$TimeoutMilliseconds = 20000,
        [int]$PollMilliseconds = 250,
        [int]$QuietMilliseconds = 1000,
        [AllowEmptyCollection()]
        [object[]]$CimProcesses
    )

    if ($QuietMilliseconds -lt 1000) {
        throw "QuietMilliseconds must be at least 1000."
    }
    $stopwatch = [Diagnostics.Stopwatch]::StartNew()
    $deadlineMilliseconds = [Math]::Max(1, $TimeoutMilliseconds)
    $lastDirtyElapsedMilliseconds = 0L
    $lastResidue = @()

    while ($true) {
        # Every quiet-window poll refreshes descendant attribution before residue classification.
        Update-DevManagerObservedProcessTree `
            -ObservedByKey $ObservedByKey `
            -TrackedPids $TrackedPids `
            -AttributionFloorUtc $AttributionFloorUtc `
            -LineageEndExclusiveByPid $LineageEndExclusiveByPid `
            -CimProcesses $CimProcesses

        $lastResidue = @(Get-DevManagerPhaseGateResidueProcesses `
                -WorktreeRoot $WorktreeRoot `
                -ObservedByKey $ObservedByKey `
                -BeforeProcesses $BeforeProcesses `
                -CimProcesses $CimProcesses)

        if ($lastResidue.Count -eq 0) {
            if (($stopwatch.ElapsedMilliseconds - $lastDirtyElapsedMilliseconds) -ge $QuietMilliseconds) {
                return ,([object[]]@())
            }
        }
        else {
            $lastDirtyElapsedMilliseconds = $stopwatch.ElapsedMilliseconds
        }

        if ($stopwatch.ElapsedMilliseconds -ge $deadlineMilliseconds) {
            return ,([object[]]$lastResidue)
        }
        if ($null -ne $CimProcesses -and $lastResidue.Count -gt 0) {
            return ,([object[]]$lastResidue)
        }
        $remainingMilliseconds = $deadlineMilliseconds - $stopwatch.ElapsedMilliseconds
        Start-Sleep -Milliseconds ([Math]::Min($PollMilliseconds, [Math]::Max(1, $remainingMilliseconds)))
    }
}

function Classify-DevManagerPhaseCleanupResult {
    param(
        [AllowEmptyCollection()]
        [object[]]$BeforeProcesses,
        [AllowEmptyCollection()]
        [object[]]$AfterProcesses
    )

    $null = $BeforeProcesses
    if (@($AfterProcesses).Count -eq 0) { return 'clean' }
    return 'residue'
}

function New-DevManagerPhaseGateUnavailableAfterInventory {
    param(
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot,
        [Parameter(Mandatory = $true)]
        [string]$RunId,
        [AllowNull()]
        [object]$RootIdentity,
        [Parameter(Mandatory = $true)]
        [string]$ObservationFailure
    )

    $bounded = [string]$ObservationFailure
    if ($bounded.Length -gt 512) {
        $bounded = $bounded.Substring(0, 512)
    }

    return [pscustomobject]@{
        schemaVersion       = [int]1
        status              = 'unavailable'
        capturedAtUtc       = [DateTime]::UtcNow.ToString('o')
        worktreeRoot        = (Normalize-DevManagerPath -LiteralPath $WorktreeRoot)
        runId               = [string]$RunId
        processes           = [object[]]@()
        rootIdentity        = $RootIdentity
        observationFailure  = $bounded
    }
}

function Get-DevManagerPhaseGateFinalExitCode {
    param(
        [AllowNull()]
        [object]$ChildExitCode,
        [switch]$ProductionAssertFailed,
        [switch]$VerificationWriteFailed,
        [switch]$EvidenceWriteFailed,
        [string]$OriginalGuardFailure
    )

    if ($ProductionAssertFailed -or $VerificationWriteFailed -or $EvidenceWriteFailed) {
        return 1
    }
    if (-not [string]::IsNullOrWhiteSpace($OriginalGuardFailure)) {
        if ($null -ne $ChildExitCode -and [int]$ChildExitCode -ne 0) {
            return [int]$ChildExitCode
        }
        return 1
    }
    if ($null -eq $ChildExitCode) {
        return 1
    }
    return [int]$ChildExitCode
}
