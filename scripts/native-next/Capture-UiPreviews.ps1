[CmdletBinding()]
param(
    [switch]$AllFixtures,
    [switch]$AllThemes,
    [switch]$AllScales,
    [switch]$AutomateWindowStates,
    [switch]$ValidateOnly,
    [string]$TargetDir = 'C:\Temp\devmanager-phase5-ui-capture-correction3',
    [string]$BinaryPath,
    [string]$OutputRoot
)

$ErrorActionPreference = 'Stop'
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = [IO.Path]::GetFullPath((Join-Path $scriptRoot '..\..')).TrimEnd('\')
$fixtureRoot = Join-Path $repoRoot 'tests\fixtures\ui'
$approvedEvidenceRoot = Join-Path $repoRoot '.devmanager-next\evidence\phase-05\screenshots'
$approvedEvidencePrefix = ([IO.Path]::GetFullPath($approvedEvidenceRoot)).TrimEnd('\') + '\'
$artifactReceiptPath = Join-Path $repoRoot '.devmanager-next\preview-artifact.json'
$artifactReceiptParentPath = Split-Path -Parent $artifactReceiptPath
$canonicalWorktree = ([IO.Path]::GetFullPath($repoRoot)).TrimEnd('\')
$manifestPath = Join-Path $canonicalWorktree 'Cargo.toml'
$lockPath = Join-Path $canonicalWorktree 'Cargo.lock'
$cargoConfigPath = Join-Path $canonicalWorktree '.cargo\config.toml'
$rustToolchainTomlPath = Join-Path $canonicalWorktree 'rust-toolchain.toml'
$rustToolchainPath = Join-Path $canonicalWorktree 'rust-toolchain'
$cargoHomePath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)) '.cargo'
$rustupPath = Join-Path $cargoHomePath 'bin\rustup.exe'
$rustupHomePath = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)) '.rustup'
$globalCargoConfigTomlPath = Join-Path $cargoHomePath 'config.toml'
$globalCargoConfigPath = Join-Path $cargoHomePath 'config'
$buildProfile = 'dev'
$artifactSchema = 'devmanager.native-next.preview-artifact/v1'
$artifactName = 'devmanager-next'
$artifactBinaryName = 'devmanager-next.exe'
$MAX_SOURCE_DIGEST_FILES = 4096
$MAX_SOURCE_DIGEST_DIRECTORIES = 4096
$MAX_SOURCE_DIGEST_BYTES = 536870912
$SOURCE_DIGEST_DEADLINE_SECONDS = 30
$MAX_PREVIEW_ARTIFACT_BYTES = 536870912
$MAX_PREVIEW_RECEIPT_BYTES = 1048576
$MAX_PREVIEW_MANIFEST_BYTES = 8388608
$MAX_PREVIEW_PNG_BYTES = 134217728
$MAX_PREVIEW_FIXTURE_BYTES = 4194304
$PREVIEW_HASH_CHUNK_BYTES = 65536
$PREVIEW_IO_DEADLINE_SECONDS = 30
$runToken = '{0}-{1}' -f $PID, ([Guid]::NewGuid().ToString('N'))

if ($env:OS -eq 'Windows_NT' -and $null -eq ('DevManagerPreviewArtifactNative' -as [type])) {
    Add-Type @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;

public static class DevManagerPreviewArtifactNative
{
    private const uint GenericRead = 0x80000000;
    private const uint ShareRead = 0x00000001;
    private const uint OpenExisting = 3;
    private const uint OpenReparsePoint = 0x00200000;
    private const uint FileAttributeNormal = 0x00000080;
    private const uint FileAttributeReparsePoint = 0x00000400;

    [StructLayout(LayoutKind.Sequential)]
    private struct ByHandleFileInformation
    {
        public uint FileAttributes;
        public uint CreationTimeLow;
        public uint CreationTimeHigh;
        public uint LastAccessTimeLow;
        public uint LastAccessTimeHigh;
        public uint LastWriteTimeLow;
        public uint LastWriteTimeHigh;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFileW(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetFileInformationByHandle(
        SafeFileHandle file,
        out ByHandleFileInformation information);

    public static SafeFileHandle OpenReadNoFollow(string path)
    {
        var handle = CreateFileW(
            path,
            GenericRead,
            ShareRead,
            IntPtr.Zero,
            OpenExisting,
            FileAttributeNormal | OpenReparsePoint,
            IntPtr.Zero);
        if (handle.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "preview artifact identity could not open the file without following a reparse point");
        }
        return handle;
    }

    private static ByHandleFileInformation Read(SafeFileHandle handle)
    {
        if (!GetFileInformationByHandle(handle, out var information))
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "preview artifact identity could not read the retained file identity");
        }
        return information;
    }

    public static bool IsReparsePoint(SafeFileHandle handle)
    {
        return (Read(handle).FileAttributes & FileAttributeReparsePoint) != 0;
    }

    public static long Length(SafeFileHandle handle)
    {
        var information = Read(handle);
        return ((long)information.FileSizeHigh << 32) | information.FileSizeLow;
    }

    public static string Identity(SafeFileHandle handle)
    {
        var information = Read(handle);
        return $"{information.VolumeSerialNumber:x8}:{information.FileIndexHigh:x8}{information.FileIndexLow:x8}";
    }

    private const uint FILE_SHARE_NONE = 0;
    private const uint FILE_SHARE_READ_WRITE = 0x00000003;
    private const uint FILE_READ_ATTRIBUTES = 0x00000080;
    private const uint FILE_TRAVERSE = 0x00000020;
    private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    private const uint CREATE_NEW = 1;
    private const uint FILE_GENERIC_WRITE = 0x40000000;
    private const uint DELETE_ACCESS = 0x00010000;
    private const uint FILE_READ_DATA = 0x00000001;
    private const uint FILE_WRITE_DATA = 0x00000002;
    private const uint SYNCHRONIZE = 0x00100000;
    private const uint FILE_SHARE_WRITE = 0x00000002;
    private const uint FILE_SHARE_DELETE = 0x00000004;
    private const uint FILE_CREATE = 2;
    private const uint FILE_OPEN = 1;
    private const uint FILE_NON_DIRECTORY_FILE = 0x00000040;
    private const uint FILE_SYNCHRONOUS_IO_NONALERT = 0x00000020;
    private const uint OBJ_CASE_INSENSITIVE = 0x00000040;
    private const int JobObjectExtendedLimitInformationClass = 9;
    private const int JobObjectBasicAccountingInformationClass = 1;
    private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
    private const int FileRenameInfo = 3;
    private const int FileDispositionInfo = 4;

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool WriteFile(
        SafeFileHandle file,
        byte[] buffer,
        uint bytesToWrite,
        out uint bytesWritten,
        IntPtr overlapped);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FlushFileBuffers(SafeFileHandle file);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetFileInformationByHandle(
        SafeFileHandle file,
        int fileInformationClass,
        IntPtr fileInformation,
        uint bufferSize);

    [StructLayout(LayoutKind.Sequential)]
    private struct FileRenameInfoHeader
    {
        public byte ReplaceIfExists;
        public IntPtr RootDirectory;
        public uint FileNameLength;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileDispositionInfoData
    {
        public byte DeleteFile;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NativeUnicodeString
    {
        public ushort Length;
        public ushort MaximumLength;
        public IntPtr Buffer;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NativeObjectAttributes
    {
        public uint Length;
        public IntPtr RootDirectory;
        public IntPtr ObjectName;
        public uint Attributes;
        public IntPtr SecurityDescriptor;
        public IntPtr SecurityQualityOfService;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NativeIoStatusBlock
    {
        public IntPtr Status;
        public UIntPtr Information;
    }

    [DllImport("ntdll.dll")]
    private static extern int NtCreateFile(
        out IntPtr fileHandle,
        uint desiredAccess,
        ref NativeObjectAttributes objectAttributes,
        out NativeIoStatusBlock ioStatusBlock,
        IntPtr allocationSize,
        uint fileAttributes,
        uint shareAccess,
        uint createDisposition,
        uint createOptions,
        IntPtr eaBuffer,
        uint eaLength);

    [DllImport("ntdll.dll")]
    private static extern uint RtlNtStatusToDosError(int status);

    private static SafeFileHandle OpenRelative(
        SafeFileHandle parentDirectory,
        string fileName,
        uint desiredAccess,
        uint createDisposition)
    {
        if (parentDirectory == null || parentDirectory.IsInvalid || string.IsNullOrWhiteSpace(fileName) ||
            fileName.IndexOf('\\') >= 0 || fileName.IndexOf('/') >= 0 || fileName.IndexOf('\0') >= 0)
        {
            throw new ArgumentException("preview relative child name is invalid");
        }
        var nameBytes = Encoding.Unicode.GetBytes(fileName);
        if (nameBytes.Length > ushort.MaxValue)
        {
            throw new ArgumentException("preview relative child name is too long");
        }
        var nameBuffer = Marshal.AllocHGlobal(nameBytes.Length);
        var unicode = new NativeUnicodeString
        {
            Length = (ushort)nameBytes.Length,
            MaximumLength = (ushort)nameBytes.Length,
            Buffer = nameBuffer
        };
        var unicodePointer = Marshal.AllocHGlobal(Marshal.SizeOf<NativeUnicodeString>());
        try
        {
            Marshal.Copy(nameBytes, 0, nameBuffer, nameBytes.Length);
            Marshal.StructureToPtr(unicode, unicodePointer, false);
            var attributes = new NativeObjectAttributes
            {
                Length = (uint)Marshal.SizeOf<NativeObjectAttributes>(),
                RootDirectory = parentDirectory.DangerousGetHandle(),
                ObjectName = unicodePointer,
                Attributes = OBJ_CASE_INSENSITIVE,
                SecurityDescriptor = IntPtr.Zero,
                SecurityQualityOfService = IntPtr.Zero
            };
            NativeIoStatusBlock ioStatus;
            IntPtr rawHandle;
            var status = NtCreateFile(
                out rawHandle,
                desiredAccess,
                ref attributes,
                out ioStatus,
                IntPtr.Zero,
                FileAttributeNormal,
                ShareRead | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                createDisposition,
                FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | OpenReparsePoint,
                IntPtr.Zero,
                0);
            if (status < 0)
            {
                throw new Win32Exception((int)RtlNtStatusToDosError(status),
                    "preview relative child open failed");
            }
            return new SafeFileHandle(rawHandle, true);
        }
        finally
        {
            Marshal.FreeHGlobal(unicodePointer);
            Marshal.FreeHGlobal(nameBuffer);
        }
    }

    public static SafeFileHandle CreateFileRelative(SafeFileHandle parentDirectory, string fileName)
    {
        return OpenRelative(
            parentDirectory,
            fileName,
            FILE_READ_DATA | FILE_WRITE_DATA | DELETE_ACCESS | SYNCHRONIZE,
            FILE_CREATE);
    }

    public static SafeFileHandle OpenReadRelativeNoFollow(SafeFileHandle parentDirectory, string fileName)
    {
        var handle = OpenRelative(
            parentDirectory,
            fileName,
            GenericRead | FILE_READ_DATA | SYNCHRONIZE,
            FILE_OPEN);
        if (IsReparsePoint(handle))
        {
            handle.Dispose();
            throw new IOException("preview relative child is a reparse point");
        }
        return handle;
    }

    public static Task<string> ReadBoundedUtf8Async(Stream stream, long maxBytes)
    {
        if (stream == null || maxBytes < 0 || maxBytes > int.MaxValue)
        {
            throw new ArgumentException("preview bounded stream arguments are invalid");
        }
        return ReadBoundedUtf8CoreAsync(stream, maxBytes);
    }

    private static async Task<string> ReadBoundedUtf8CoreAsync(Stream stream, long maxBytes)
    {
        var buffer = new byte[16 * 1024];
        using (var output = new MemoryStream())
        {
            long total = 0;
            while (true)
            {
                var read = await stream.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false);
                if (read == 0)
                {
                    return Encoding.UTF8.GetString(output.ToArray());
                }
                total += read;
                if (total > maxBytes)
                {
                    throw new InvalidDataException("preview command output exceeded its bounded byte count");
                }
                output.Write(buffer, 0, read);
            }
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JobObjectBasicLimitInformation
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
    private struct JobObjectExtendedLimitInformation
    {
        public JobObjectBasicLimitInformation BasicLimitInformation;
        public IoCounters IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct JobObjectBasicAccountingInformationData
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateJobObjectW(IntPtr attributes, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        SafeFileHandle job,
        int informationClass,
        IntPtr information,
        uint informationLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(SafeFileHandle job, IntPtr process);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(SafeFileHandle job, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        SafeFileHandle job,
        int informationClass,
        IntPtr information,
        uint informationLength,
        out uint returnLength);

    private static SafeFileHandle CreatePreviewJob()
    {
        var job = CreateJobObjectW(IntPtr.Zero, null);
        if (job == null || job.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "preview process job creation failed");
        }
        var limits = new JobObjectExtendedLimitInformation
        {
            BasicLimitInformation = new JobObjectBasicLimitInformation
            {
                LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            }
        };
        var size = Marshal.SizeOf<JobObjectExtendedLimitInformation>();
        var buffer = Marshal.AllocHGlobal(size);
        try
        {
            Marshal.StructureToPtr(limits, buffer, false);
            if (!SetInformationJobObject(job, JobObjectExtendedLimitInformationClass, buffer, (uint)size))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "preview process job configuration failed");
            }
        }
        catch
        {
            job.Dispose();
            throw;
        }
        finally
        {
            Marshal.FreeHGlobal(buffer);
        }
        return job;
    }

    public sealed class PreviewJobProcess : IDisposable
    {
        public Process Process { get; private set; }
        public SafeFileHandle Job { get; private set; }

        internal PreviewJobProcess(Process process, SafeFileHandle job)
        {
            Process = process;
            Job = job;
        }

        public void Terminate()
        {
            if (Job != null && !Job.IsInvalid)
            {
                if (!TerminateJobObject(Job, 1))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "preview process job termination failed");
                }
            }
        }

        public uint ActiveProcessCount()
        {
            if (Job == null || Job.IsInvalid)
            {
                return 0;
            }
            var size = Marshal.SizeOf<JobObjectBasicAccountingInformationData>();
            var buffer = Marshal.AllocHGlobal(size);
            try
            {
                uint returned;
                if (!QueryInformationJobObject(Job, JobObjectBasicAccountingInformationClass, buffer, (uint)size, out returned))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "preview process job query failed");
                }
                return Marshal.PtrToStructure<JobObjectBasicAccountingInformationData>(buffer).ActiveProcesses;
            }
            finally
            {
                Marshal.FreeHGlobal(buffer);
            }
        }

        public void Dispose()
        {
            if (Job != null)
            {
                Job.Dispose();
                Job = null;
            }
            if (Process != null)
            {
                Process.Dispose();
                Process = null;
            }
        }
    }

    public static PreviewJobProcess StartProcessInJob(ProcessStartInfo startInfo)
    {
        var job = CreatePreviewJob();
        Process process = null;
        try
        {
            process = Process.Start(startInfo);
            if (process == null || !AssignProcessToJobObject(job, process.Handle))
            {
                try { TerminateJobObject(job, 1); } catch { }
                if (process != null)
                {
                    try { process.Kill(true); } catch { }
                    try { process.WaitForExit(0); } catch { }
                    process.Dispose();
                }
                job.Dispose();
                throw new InvalidOperationException("preview process job assignment failed");
            }
            return new PreviewJobProcess(process, job);
        }
        catch
        {
            if (process != null)
            {
                try { process.Dispose(); } catch { }
            }
            job.Dispose();
            throw;
        }
    }

    public static SafeFileHandle OpenReadExclusiveNoFollow(string path)
    {
        var handle = CreateFileW(
            path,
            GenericRead,
            FILE_SHARE_NONE,
            IntPtr.Zero,
            OpenExisting,
            FileAttributeNormal | OpenReparsePoint,
            IntPtr.Zero);
        if (handle.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "preview artifact authority could not open the file exclusively without following a reparse point");
        }
        return handle;
    }

    public static SafeFileHandle OpenDirectoryNoFollow(string path)
    {
        var handle = CreateFileW(
            path,
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ_WRITE,
            IntPtr.Zero,
            OpenExisting,
            FILE_FLAG_BACKUP_SEMANTICS | OpenReparsePoint,
            IntPtr.Zero);
        if (handle.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "preview artifact authority could not open the directory without following a reparse point");
        }
        var information = Read(handle);
        if ((information.FileAttributes & 0x00000010) == 0 ||
            (information.FileAttributes & FileAttributeReparsePoint) != 0)
        {
            handle.Dispose();
            throw new IOException("preview artifact authority requires a regular directory");
        }
        return handle;
    }

    public static SafeFileHandle OpenDirectoryForPublication(string path)
    {
        var handle = CreateFileW(
            path,
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE | FILE_GENERIC_WRITE | DELETE_ACCESS,
            FILE_SHARE_READ_WRITE,
            IntPtr.Zero,
            OpenExisting,
            FILE_FLAG_BACKUP_SEMANTICS | OpenReparsePoint,
            IntPtr.Zero);
        if (handle.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "preview publication directory could not be opened without following a reparse point");
        }
        var information = Read(handle);
        if ((information.FileAttributes & 0x00000010) == 0 ||
            (information.FileAttributes & FileAttributeReparsePoint) != 0)
        {
            handle.Dispose();
            throw new IOException("preview publication authority requires a regular directory");
        }
        return handle;
    }

    public static void WriteAtomicPreviewReceiptRelative(
        SafeFileHandle parentDirectory,
        string fileName,
        string contents)
    {
        // PublishRelative uses FILE_RENAME_INFO.RootDirectory so the final name
        // is resolved only through the retained parent handle.
        if (parentDirectory == null || parentDirectory.IsInvalid)
        {
            throw new ArgumentException("receipt publication requires a retained parent directory handle", nameof(parentDirectory));
        }
        if (fileName.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0 ||
            fileName.Contains(Path.DirectorySeparatorChar) ||
            fileName.Contains(Path.AltDirectorySeparatorChar))
        {
            throw new ArgumentException("receipt file name must be a single path component", nameof(fileName));
        }
        var temporaryName = $".{fileName}.{Guid.NewGuid():N}.tmp";
        var handle = CreateFileRelative(parentDirectory, temporaryName);
        if (handle.IsInvalid)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "relative preview receipt temporary file could not be created");
        }
        var renamed = false;
        using (handle)
        {
            try
            {
                var bytes = Encoding.UTF8.GetBytes(contents);
                uint written;
                if (!WriteFile(handle, bytes, (uint)bytes.Length, out written, IntPtr.Zero) ||
                    written != bytes.Length || !FlushFileBuffers(handle))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(),
                        "relative preview receipt temporary file could not be fully flushed");
                }

                var fileNameBytes = Encoding.Unicode.GetBytes(fileName);
                var headerSize = Marshal.SizeOf<FileRenameInfoHeader>();
                var bufferSize = checked(headerSize + fileNameBytes.Length);
                var renameInfo = Marshal.AllocHGlobal(bufferSize);
                try
                {
                    var header = new FileRenameInfoHeader
                    {
                        ReplaceIfExists = 1,
                        RootDirectory = parentDirectory.DangerousGetHandle(),
                        FileNameLength = (uint)fileNameBytes.Length
                    };
                    Marshal.StructureToPtr(header, renameInfo, false);
                    Marshal.Copy(fileNameBytes, 0, IntPtr.Add(renameInfo, headerSize), fileNameBytes.Length);
                    if (!SetFileInformationByHandle(handle, FileRenameInfo, renameInfo, (uint)bufferSize))
                    {
                        throw new Win32Exception(Marshal.GetLastWin32Error(),
                            "relative preview receipt could not be atomically published");
                    }
                    renamed = true;
                }
                finally
                {
                    Marshal.FreeHGlobal(renameInfo);
                }
            }
            finally
            {
                if (!renamed)
                {
                    var disposition = new FileDispositionInfoData { DeleteFile = 1 };
                    var dispositionInfo = Marshal.AllocHGlobal(Marshal.SizeOf<FileDispositionInfoData>());
                    try
                    {
                        Marshal.StructureToPtr(disposition, dispositionInfo, false);
                        if (!SetFileInformationByHandle(
                            handle,
                            FileDispositionInfo,
                            dispositionInfo,
                            (uint)Marshal.SizeOf<FileDispositionInfoData>()))
                        {
                            throw new Win32Exception(Marshal.GetLastWin32Error(),
                                "relative preview receipt temporary file could not be removed by handle");
                        }
                    }
                    finally
                    {
                        Marshal.FreeHGlobal(dispositionInfo);
                    }
                }
            }
            }
        }
    }
'@
}

if (-not ([IO.Path]::GetFullPath((Get-Location).Path)).TrimEnd('\').Equals($canonicalWorktree, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'preview artifact identity requires the caller CWD to be the canonical worktree.'
}
if (-not [string]::IsNullOrWhiteSpace($BinaryPath)) {
    throw 'preview artifact identity caller-supplied warm binary paths are disabled; build one trusted artifact per invocation.'
}
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf) -or
    -not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
    throw 'preview artifact identity requires the canonical Cargo manifest and lockfile.'
}

function Assert-PreviewDeadline {
    param([datetime]$Deadline)

    if ($Deadline -eq [datetime]::MinValue -or [datetime]::UtcNow -gt $Deadline) {
        throw 'preview artifact authority operation exceeded its bounded deadline.'
    }
}

function New-PreviewIoDeadline {
    [datetime]::UtcNow.AddSeconds($PREVIEW_IO_DEADLINE_SECONDS)
}

function Get-PreviewRemainingMilliseconds {
    param([datetime]$Deadline)

    Assert-PreviewDeadline -Deadline $Deadline
    $remaining = [int][Math]::Ceiling(($Deadline - [datetime]::UtcNow).TotalMilliseconds)
    if ($remaining -le 0) {
        throw 'preview artifact authority operation exceeded its bounded deadline.'
    }
    [Math]::Min($remaining, [int]::MaxValue)
}

function Wait-PreviewBackoff {
    param(
        [datetime]$Deadline,
        [int]$Milliseconds
    )

    $remaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline
    $delay = [Math]::Min([Math]::Max($Milliseconds, 1), $remaining)
    if ($delay -gt 0) {
        [void][Threading.Tasks.Task]::Delay($delay).Wait($delay)
    }
}

 $PreviewEnvironmentAllowlist = @(
    'CARGO_HOME',
    'RUSTUP_HOME',
    'CARGO_NET_OFFLINE',
    'CARGO_TERM_COLOR',
    'CARGO_TARGET_DIR',
    'CARGO_BUILD_JOBS',
    'DEV_MANAGER_PREVIEW_BUILD_IDENTITY',
    'SystemRoot',
    'WINDIR',
    'ComSpec',
    'USERPROFILE',
    'TEMP',
    'TMP',
    'PATH'
)

function Get-PreviewToolEnvironment {
    param([object]$BuildIdentity)

    $environment = @{
        CARGO_HOME = [IO.Path]::GetFullPath($cargoHomePath)
        RUSTUP_HOME = [IO.Path]::GetFullPath($rustupHomePath)
        CARGO_NET_OFFLINE = 'true'
        CARGO_TERM_COLOR = 'never'
    }
    foreach ($name in @('SystemRoot', 'WINDIR', 'ComSpec', 'USERPROFILE')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            $environment[$name] = $value
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($TargetRunDir)) {
        $environment['TEMP'] = [IO.Path]::GetFullPath($TargetRunDir)
        $environment['TMP'] = [IO.Path]::GetFullPath($TargetRunDir)
    }
    $ambientPath = [Environment]::GetEnvironmentVariable('PATH')
    if (-not [string]::IsNullOrWhiteSpace($ambientPath)) {
        $environment['PATH'] = $ambientPath
    }
    if ($null -ne $BuildIdentity) {
        $toolDirectories = @(
            [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($BuildIdentity.CargoPath)),
            [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($BuildIdentity.RustcPath))
        ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique
        $environment['PATH'] = $toolDirectories -join [IO.Path]::PathSeparator
    }
    $environment
}

function ConvertTo-PreviewSafeDiagnostic {
    param([object]$ErrorRecord)

    if ($null -eq $ErrorRecord) {
        return 'preview.unknown-failure'
    }
    $message = [string]$ErrorRecord.Exception.Message
    if ($message -match 'preview\.[a-z0-9.-]+') {
        return $Matches[0]
    }
    'preview.operation-failed'
}

function New-PreviewProcessStartInfo {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [hashtable]$Environment,
        [switch]$RedirectOutput
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.RedirectStandardOutput = [bool]$RedirectOutput
    $startInfo.RedirectStandardError = [bool]$RedirectOutput
    foreach ($argument in @($Arguments)) {
        [void]$startInfo.ArgumentList.Add([string]$argument)
    }
    $startInfo.Environment.Clear()
    foreach ($name in @($PreviewEnvironmentAllowlist)) {
        if ($Environment.ContainsKey($name)) {
            $startInfo.Environment[$name] = [string]$Environment[$name]
        }
    }
    $startInfo
}

function Invoke-PreviewExternalCommand {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [datetime]$Deadline,
        [hashtable]$Environment = @{},
        [string]$WorkingDirectory = $canonicalWorktree,
        [long]$MaxOutputBytes = $MAX_PREVIEW_RECEIPT_BYTES
    )

    [void](Get-PreviewRemainingMilliseconds -Deadline $Deadline)
    $startInfo = New-PreviewProcessStartInfo -FilePath $FilePath -Arguments $Arguments -WorkingDirectory $WorkingDirectory -Environment $Environment -RedirectOutput
    $owned = $null
    $launch = $null
    try {
        $owned = [DevManagerPreviewArtifactNative]::StartProcessInJob($startInfo)
        $launch = [pscustomobject]@{
            Process = $owned.Process
            Job = $owned
            JoinState = 'started'
            ExitCode = $null
        }
        $stdoutTask = [DevManagerPreviewArtifactNative]::ReadBoundedUtf8Async($owned.Process.StandardOutput.BaseStream, $MaxOutputBytes)
        $stderrTask = [DevManagerPreviewArtifactNative]::ReadBoundedUtf8Async($owned.Process.StandardError.BaseStream, $MaxOutputBytes)
        $processWaitTask = $owned.Process.WaitForExitAsync()
        while (-not ($stdoutTask.IsCompleted -and $stderrTask.IsCompleted -and $processWaitTask.IsCompleted)) {
            if ($stdoutTask.IsFaulted -or $stderrTask.IsFaulted) {
                throw 'preview.command.output-limit'
            }
            $pending = [System.Collections.Generic.List[Threading.Tasks.Task]]::new()
            if (-not $stdoutTask.IsCompleted) { [void]$pending.Add($stdoutTask) }
            if (-not $stderrTask.IsCompleted) { [void]$pending.Add($stderrTask) }
            if (-not $processWaitTask.IsCompleted) { [void]$pending.Add($processWaitTask) }
            if ($pending.Count -eq 0) { break }
            $remaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline
            if (-not [Threading.Tasks.Task]::WhenAny($pending.ToArray()).Wait($remaining)) {
                throw 'preview.command.deadline'
            }
        }
        if ($stdoutTask.IsFaulted -or $stderrTask.IsFaulted) {
            throw 'preview.command.output-limit'
        }
        if (-not $processWaitTask.IsCompleted) {
            throw 'preview.command.deadline'
        }
        try {
            $stdout = $stdoutTask.GetAwaiter().GetResult()
            [void]$stderrTask.GetAwaiter().GetResult()
        } catch {
            throw 'preview.command.output-read-failed'
        }
        $exitCode = $owned.Process.ExitCode
        if ($exitCode -ne 0) {
            throw 'preview.command.exit-nonzero'
        }
        $launch.ExitCode = $exitCode
        [pscustomobject]@{
            ExitCode = $exitCode
            Output = $stdout
            Error = $null
            AbsoluteDeadline = $Deadline
        }
    } finally {
        if ($null -ne $launch) {
            try {
                [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'external command')
            } catch {
                throw 'preview.cleanup.failed'
            }
        }
    }
}

function ConvertTo-PreviewSha256 {
    param(
        [byte[]]$Bytes,
        [datetime]$Deadline
    )

    Assert-PreviewDeadline -Deadline $Deadline
    $hashAlgorithm = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = $hashAlgorithm.ComputeHash($Bytes)
        Assert-PreviewDeadline -Deadline $Deadline
        ([BitConverter]::ToString($digest)).Replace('-', '').ToLowerInvariant()
    } finally {
        $hashAlgorithm.Dispose()
    }
}

function Get-PreviewArtifactSha256 {
    param(
        [IO.Stream]$Stream,
        [datetime]$Deadline,
        [long]$MaxBytes = $MAX_PREVIEW_ARTIFACT_BYTES
    )

    Assert-PreviewDeadline -Deadline $Deadline
    if (-not $Stream.CanRead) {
        throw 'preview artifact authority requires a readable stream.'
    }
    if ($Stream.CanSeek -and $Stream.Length -gt $MaxBytes) {
        throw 'preview.io.input-too-large'
    }

    $hashAlgorithm = [Security.Cryptography.SHA256]::Create()
    $buffer = New-Object byte[] $PREVIEW_HASH_CHUNK_BYTES
    $total = [int64]0
    try {
        if ($Stream.CanSeek) { $Stream.Position = 0 }
        while (($read = $Stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Assert-PreviewDeadline -Deadline $Deadline
            $total += $read
            if ($total -gt $MaxBytes) {
                throw 'preview.io.input-too-large'
            }
            [void]$hashAlgorithm.TransformBlock($buffer, 0, $read, $buffer, 0)
        }
        Assert-PreviewDeadline -Deadline $Deadline
        [void]$hashAlgorithm.TransformFinalBlock([byte[]]::new(0), 0, 0)
        ([BitConverter]::ToString($hashAlgorithm.Hash)).Replace('-', '').ToLowerInvariant()
    } finally {
        $hashAlgorithm.Dispose()
    }
}

function Read-PreviewUtf8Text {
    param(
        [IO.Stream]$Stream,
        [datetime]$Deadline,
        [long]$MaxBytes = $MAX_PREVIEW_RECEIPT_BYTES
    )

    Assert-PreviewDeadline -Deadline $Deadline
    if ($Stream.CanSeek -and $Stream.Length -gt $MaxBytes) {
        throw 'preview.io.text-too-large'
    }
    $buffer = New-Object byte[] $PREVIEW_HASH_CHUNK_BYTES
    $bytes = [IO.MemoryStream]::new()
    $total = [int64]0
    try {
        if ($Stream.CanSeek) { $Stream.Position = 0 }
        while (($read = $Stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            Assert-PreviewDeadline -Deadline $Deadline
            $total += $read
            if ($total -gt $MaxBytes) {
                throw 'preview.io.text-too-large'
            }
            $bytes.Write($buffer, 0, $read)
        }
        Assert-PreviewDeadline -Deadline $Deadline
        [Text.UTF8Encoding]::new($false, $true).GetString($bytes.ToArray())
    } finally {
        $bytes.Dispose()
    }
}

function Read-PreviewFixture {
    param(
        [string]$Path,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $opened = Open-PreviewArtifactNoFollow -Path $Path
    try {
        $json = Read-PreviewUtf8Text -Stream $opened.Stream -Deadline $Deadline -MaxBytes $MAX_PREVIEW_FIXTURE_BYTES
        $json | ConvertFrom-Json
    } finally {
        $opened.Stream.Dispose()
    }
}

function Get-PreviewFixtureFilesBounded {
    param(
        [string]$Root,
        [int]$MaxFiles,
        [datetime]$Deadline
    )

    $files = [System.Collections.Generic.List[object]]::new()
    try {
        foreach ($candidate in Get-ChildItem -LiteralPath $Root -Filter '*.json' -File) {
            Assert-PreviewDeadline -Deadline $Deadline
            if ($files.Count -ge $MaxFiles) {
                throw 'preview.fixture.enumeration-limit'
            }
            [void]$files.Add($candidate)
        }
    } catch {
        if ([string]$_.Exception.Message -eq 'preview.fixture.enumeration-limit') {
            throw
        }
        throw 'preview.fixture.enumeration-failed'
    }
    @($files | Sort-Object -Property Name)
}

function Open-PreviewArtifactNoFollow {
    param([string]$Path)

    $canonicalPath = [IO.Path]::GetFullPath($Path)
    $handle = $null
    $stream = $null
    try {
        $handle = [DevManagerPreviewArtifactNative]::OpenReadExclusiveNoFollow($canonicalPath)
        if ([DevManagerPreviewArtifactNative]::IsReparsePoint($handle)) {
            throw 'preview artifact authority refuses a reparse-point executable.'
        }
        $stream = [IO.FileStream]::new($handle, [IO.FileAccess]::Read)
        $handle = $null
        [pscustomobject]@{
            Path = $canonicalPath
            Stream = $stream
            FileIdentity = [DevManagerPreviewArtifactNative]::Identity($stream.SafeFileHandle)
            Length = [DevManagerPreviewArtifactNative]::Length($stream.SafeFileHandle)
        }
    } catch {
        if ($null -ne $stream) {
            $stream.Dispose()
        } elseif ($null -ne $handle) {
            $handle.Dispose()
        }
        throw
    }
}

function Open-PreviewArtifactRelative {
    param(
        [object]$ParentAuthority,
        [string]$Name
    )

    if ($null -eq $ParentAuthority -or [string]::IsNullOrWhiteSpace($Name) -or
        [IO.Path]::GetFileName($Name) -ne $Name) {
        throw 'preview relative artifact name is invalid.'
    }
    Assert-PreviewDirectoryAuthorityStable -Authority $ParentAuthority
    $handle = $null
    $stream = $null
    try {
        $handle = [DevManagerPreviewArtifactNative]::OpenReadRelativeNoFollow($ParentAuthority.Handle, $Name)
        $stream = [IO.FileStream]::new($handle, [IO.FileAccess]::Read)
        $handle = $null
        [pscustomobject]@{
            Path = Join-Path $ParentAuthority.Path $Name
            Stream = $stream
            FileIdentity = [DevManagerPreviewArtifactNative]::Identity($stream.SafeFileHandle)
            Length = [DevManagerPreviewArtifactNative]::Length($stream.SafeFileHandle)
        }
    } catch {
        if ($null -ne $stream) {
            $stream.Dispose()
        } elseif ($null -ne $handle) {
            $handle.Dispose()
        }
        throw
    }
}

function Open-PreviewDirectoryNoFollow {
    param(
        [string]$Path,
        [switch]$ForPublication
    )

    $fullPath = [IO.Path]::GetFullPath($Path)
    $rootPath = [IO.Path]::GetPathRoot($fullPath)
    $canonicalPath = if ($fullPath.Length -gt $rootPath.Length) {
        $fullPath.TrimEnd('\')
    } else {
        $rootPath
    }
    $handle = if ($ForPublication) {
        [DevManagerPreviewArtifactNative]::OpenDirectoryForPublication($canonicalPath)
    } else {
        [DevManagerPreviewArtifactNative]::OpenDirectoryNoFollow($canonicalPath)
    }
    try {
        [pscustomobject]@{
            Path = $canonicalPath
            Handle = $handle
            FileIdentity = [DevManagerPreviewArtifactNative]::Identity($handle)
        }
    } catch {
        $handle.Dispose()
        throw
    }
}

function Get-PreviewDirectoryChain {
    param([string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path)
    $rootPath = [IO.Path]::GetPathRoot($fullPath)
    $cursor = if ($fullPath.Length -gt $rootPath.Length) {
        $fullPath.TrimEnd('\')
    } else {
        $rootPath
    }
    $paths = [System.Collections.Generic.List[string]]::new()
    while ($true) {
        [void]$paths.Add($cursor)
        if ($cursor.Equals($rootPath, [StringComparison]::OrdinalIgnoreCase)) {
            break
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if ($null -eq $parent) {
            break
        }
        $parentPath = $parent.FullName
        $cursor = if ($parentPath.Length -gt $rootPath.Length) {
            $parentPath.TrimEnd('\')
        } else {
            $rootPath
        }
    }
    @($paths)
}

function Open-PreviewDirectoryAuthorityChain {
    param([string]$Path)

    $paths = @(Get-PreviewDirectoryChain -Path $Path)
    if ($paths.Count -eq 0) {
        throw 'preview directory authority could not resolve its ancestor chain.'
    }
    $authorities = [System.Collections.Generic.List[object]]::new()
    try {
        # Open from the volume root down to the requested leaf and retain every
        # non-reparse ancestor through the final publication.
        for ($index = $paths.Count - 1; $index -ge 0; $index--) {
            [void]$authorities.Add((Open-PreviewDirectoryNoFollow -Path $paths[$index] -ForPublication:($index -eq 0)))
        }
        $leaf = $authorities[$authorities.Count - 1]
        $leaf | Add-Member -NotePropertyName AncestorChain -NotePropertyValue @($authorities) -Force
        $leaf | Add-Member -NotePropertyName OutputRootAncestorChain -NotePropertyValue @($authorities) -Force
        $leaf
    } catch {
        foreach ($authority in @($authorities)) {
            if ($null -ne $authority -and $null -ne $authority.Handle) {
                try { $authority.Handle.Dispose() } catch { }
            }
        }
        throw
    }
}

function Close-PreviewDirectoryAuthorityChain {
    param([object]$Authority)

    $chain = if ($null -ne $Authority -and $null -ne $Authority.OutputRootAncestorChain) {
        @($Authority.OutputRootAncestorChain)
    } elseif ($null -ne $Authority -and $null -ne $Authority.Directories) {
        @($Authority.Directories)
    } else {
        @($Authority)
    }
    foreach ($ancestor in @($chain | Sort-Object { $_.Path.Length } -Descending)) {
        if ($null -ne $ancestor -and $null -ne $ancestor.Handle) {
            try { $ancestor.Handle.Dispose() } catch { }
        }
    }
}

function Close-PreviewLaunchAuthority {
    param([object]$Authority)

    if ($null -eq $Authority) {
        return
    }
    if (-not [bool]$Authority.OwnsHandles) {
        return
    }
    if ($null -ne $Authority.Binary -and $null -ne $Authority.Binary.Stream) {
        $Authority.Binary.Stream.Dispose()
    }
    foreach ($directory in @($Authority.Directories)) {
        if ($null -ne $directory -and $null -ne $directory.Handle) {
            $directory.Handle.Dispose()
        }
    }
}

function Assert-PreviewDirectoryAuthorityStable {
    param([object]$Authority)

    if ($null -eq $Authority -or $null -eq $Authority.Handle) {
        throw 'preview directory authority is missing its retained handle.'
    }
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Handle) -ne $Authority.FileIdentity) {
        throw 'preview.authority.directory-changed'
    }
    if ($null -ne $Authority.OutputRootAncestorChain) {
        foreach ($ancestor in @($Authority.OutputRootAncestorChain)) {
            if ($null -eq $ancestor -or $null -eq $ancestor.Handle -or
                [DevManagerPreviewArtifactNative]::Identity($ancestor.Handle) -ne $ancestor.FileIdentity) {
                throw 'preview.authority.ancestor-changed'
            }
        }
    }
}

function Open-PreviewOutputAuthority {
    param(
        [string]$Path,
        [object]$ParentAuthority,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Assert-PreviewDeadline -Deadline $Deadline
    Assert-PreviewDirectoryAuthorityStable -Authority $ParentAuthority
    $canonicalPath = [IO.Path]::GetFullPath($Path)
    $parentPath = [IO.Path]::GetFullPath($ParentAuthority.Path).TrimEnd('\') + '\'
    if (-not $canonicalPath.StartsWith($parentPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview output authority is outside the retained output directory.'
    }
    $opened = Open-PreviewArtifactRelative -ParentAuthority $ParentAuthority -Name ([IO.Path]::GetFileName($canonicalPath))
    try {
        if ([int64]$opened.Length -le 0 -or [int64]$opened.Length -gt $MAX_PREVIEW_PNG_BYTES) {
            throw 'preview PNG output exceeds its bounded byte limit.'
        }
        [pscustomobject]@{
            Path = $opened.Path
            Stream = $opened.Stream
            FileIdentity = $opened.FileIdentity
            Length = [int64]$opened.Length
            Parent = $ParentAuthority
        }
        $opened = $null
    } finally {
        if ($null -ne $opened) {
            $opened.Stream.Dispose()
        }
    }
}

function Close-PreviewOutputAuthority {
    param([object]$Authority)

    if ($null -ne $Authority -and $null -ne $Authority.Stream) {
        $Authority.Stream.Dispose()
    }
}

function Try-Open-PreviewOutputAuthority {
    param(
        [string]$Path,
        [object]$ParentAuthority,
        [datetime]$Deadline
    )

    try {
        Open-PreviewOutputAuthority -Path $Path -ParentAuthority $ParentAuthority -Deadline $Deadline
    } catch {
        $win32 = $_.Exception -as [System.ComponentModel.Win32Exception]
        if ($null -ne $win32 -and $win32.NativeErrorCode -in @(2, 3)) {
            return $null
        }
        throw
    }
}

function Assert-PreviewOutputAuthorityStable {
    param(
        [object]$Authority,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Assert-PreviewDeadline -Deadline $Deadline
    Assert-PreviewDirectoryAuthorityStable -Authority $Authority.Parent
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Stream.SafeFileHandle) -ne $Authority.FileIdentity) {
        throw 'preview.output.identity-changed'
    }
    if ([int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw 'preview.output.length-changed'
    }
    $hash = Get-PreviewArtifactSha256 -Stream $Authority.Stream -Deadline $Deadline -MaxBytes $MAX_PREVIEW_PNG_BYTES
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Stream.SafeFileHandle) -ne $Authority.FileIdentity -or
        [int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw 'preview.output.changed-while-hashed'
    }
    $Authority | Add-Member -NotePropertyName Sha256 -NotePropertyValue $hash -Force
    $hash
}

function Write-PreviewAtomicJson {
    param(
        [string]$Path,
        [object]$Value,
        [object]$ParentAuthority,
        [datetime]$Deadline,
        [long]$MaxBytes = $MAX_PREVIEW_RECEIPT_BYTES
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Assert-PreviewDeadline -Deadline $Deadline
    Assert-PreviewDirectoryAuthorityStable -Authority $ParentAuthority
    $parentPath = [IO.Path]::GetFullPath($ParentAuthority.Path).TrimEnd('\')
    $canonicalPath = [IO.Path]::GetFullPath($Path)
    if (-not $canonicalPath.StartsWith($parentPath + '\', [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetDirectoryName($canonicalPath) -ne $parentPath) {
        throw 'preview JSON publication path is outside the retained output directory.'
    }
    $json = $Value | ConvertTo-Json -Depth 8 -Compress
    $jsonBytes = [Text.Encoding]::UTF8.GetBytes($json)
    if ($jsonBytes.Length -gt $MaxBytes) {
        throw 'preview JSON publication exceeded its bounded byte count.'
    }
    [DevManagerPreviewArtifactNative]::WriteAtomicPreviewReceiptRelative(
        $ParentAuthority.Handle,
        [IO.Path]::GetFileName($canonicalPath),
        $json)
    Assert-PreviewDirectoryAuthorityStable -Authority $ParentAuthority
    $authority = Open-PreviewOutputAuthority -Path $canonicalPath -ParentAuthority $ParentAuthority -Deadline $Deadline
    try {
        [void](Assert-PreviewOutputAuthorityStable -Authority $authority -Deadline $Deadline)
        $authority
        $authority = $null
    } finally {
        if ($null -ne $authority) {
            Close-PreviewOutputAuthority -Authority $authority
        }
    }
}

function Get-PreviewFileSha256 {
    param(
        [string]$Path,
        [datetime]$Deadline,
        [long]$MaxBytes = $MAX_PREVIEW_ARTIFACT_BYTES
    )

    $opened = Open-PreviewArtifactNoFollow -Path $Path
    try {
        Get-PreviewArtifactSha256 -Stream $opened.Stream -Deadline $Deadline -MaxBytes $MaxBytes
    } finally {
        $opened.Stream.Dispose()
    }
}

function Get-PreviewSourceTreeDigest {
    param([datetime]$Deadline)

    $result = Invoke-PreviewExternalCommand -FilePath 'git' -Arguments @(
        '-C', $canonicalWorktree, 'rev-parse', '--verify', 'HEAD^{tree}'
    ) -Deadline $Deadline -Environment @{}
    $treeLines = @($result.Output -split "`r?`n" | Where-Object { $_ -ne '' })
    if ($treeLines.Count -ne 1) {
        throw 'preview artifact identity could not resolve the canonical source tree.'
    }
    $treeLines[0].ToString().Trim()
}

function Get-PreviewSourceRevision {
    param([datetime]$Deadline)

    $result = Invoke-PreviewExternalCommand -FilePath 'git' -Arguments @(
        '-C', $canonicalWorktree, 'rev-parse', '--verify', 'HEAD'
    ) -Deadline $Deadline -Environment @{}
    $revisionLines = @($result.Output -split "`r?`n" | Where-Object { $_ -ne '' })
    if ($revisionLines.Count -ne 1) {
        throw 'preview artifact identity could not resolve the canonical worktree revision.'
    }
    $revisionLines[0].ToString().Trim()
}

function Get-PreviewSourceContentDigest {
    param([datetime]$Deadline)

    $deadline = [DateTime]::UtcNow.AddSeconds($SOURCE_DIGEST_DEADLINE_SECONDS)
    if ($Deadline -ne [datetime]::MinValue) {
        $deadline = $Deadline
    }
    $excludedDirectories = @('.devmanager-next', '.git', 'node_modules', 'target')
    $pendingDirectories = [System.Collections.Generic.Stack[string]]::new()
    $sourceFiles = [System.Collections.Generic.List[string]]::new()
    $retainedSourceDirectoryAuthorities = [System.Collections.Generic.List[object]]::new()
    [void]$pendingDirectories.Push($canonicalWorktree)
    $sourceDirectoryCount = 0
    try {
        while ($pendingDirectories.Count -gt 0) {
            if ([DateTime]::UtcNow -gt $deadline) {
                throw 'preview artifact identity source digest exceeded its bounded deadline.'
            }
            $directoryPath = $pendingDirectories.Pop()
            $sourceDirectoryCount++
            if ($sourceDirectoryCount -gt $MAX_SOURCE_DIGEST_DIRECTORIES) {
                throw 'preview artifact identity source digest exceeded its bounded directory count.'
            }
            $directoryAuthority = Open-PreviewDirectoryNoFollow -Path $directoryPath
            [void]$retainedSourceDirectoryAuthorities.Add($directoryAuthority)
            foreach ($entry in (Get-ChildItem -LiteralPath $directoryPath -Force -ErrorAction Stop |
                    Select-Object -First ($MAX_SOURCE_DIGEST_FILES + $MAX_SOURCE_DIGEST_DIRECTORIES + 1))) {
                if ([DateTime]::UtcNow -gt $deadline) {
                    throw 'preview artifact identity source digest exceeded its bounded deadline.'
                }
                if ($entry.Name -in $excludedDirectories) {
                    continue
                }
                if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw 'preview.identity.reparse-input'
                }
                if ($entry.PSIsContainer) {
                    if (($sourceDirectoryCount + $pendingDirectories.Count + 1) -gt $MAX_SOURCE_DIGEST_DIRECTORIES) {
                        throw 'preview artifact identity source digest exceeded its bounded directory count.'
                    }
                    [void]$pendingDirectories.Push($entry.FullName)
                } else {
                    [void]$sourceFiles.Add($entry.FullName)
                    if ($sourceFiles.Count -gt $MAX_SOURCE_DIGEST_FILES) {
                        throw 'preview artifact identity source digest exceeded its bounded file count.'
                    }
                }
            }
        }

        $entries = [System.Collections.Generic.List[object]]::new()
        $totalBytes = [int64]0
        foreach ($path in @($sourceFiles | Sort-Object -Unique)) {
            if ([DateTime]::UtcNow -gt $deadline) {
                throw 'preview artifact identity source digest exceeded its bounded deadline.'
            }
            $opened = Open-PreviewArtifactNoFollow -Path $path
            try {
                $identityBefore = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
                if ([int64]$opened.Length -gt $MAX_SOURCE_DIGEST_BYTES -or
                    $totalBytes + [int64]$opened.Length -gt $MAX_SOURCE_DIGEST_BYTES) {
                    throw 'preview artifact identity source digest exceeded its bounded byte count.'
                }
                $digest = Get-PreviewArtifactSha256 -Stream $opened.Stream -Deadline $deadline -MaxBytes $MAX_SOURCE_DIGEST_BYTES
                $identityAfter = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
                if ($identityBefore -ne $identityAfter -or
                    [int64]$opened.Length -ne [int64][DevManagerPreviewArtifactNative]::Length($opened.Stream.SafeFileHandle)) {
                    throw 'preview.identity.source-changed'
                }
                $totalBytes += [int64]$opened.Length
                $relative = [IO.Path]::GetRelativePath($canonicalWorktree, $path).Replace('\', '/')
                [void]$entries.Add([ordered]@{
                        path = $relative
                        length = [int64]$opened.Length
                        sha256 = $digest
                    })
            } finally {
                $opened.Stream.Dispose()
            }
        }
        $canonical = $entries | ConvertTo-Json -Depth 8 -Compress
        $canonicalBytes = [Text.Encoding]::UTF8.GetBytes($canonical)
        if ($canonicalBytes.Length -gt $MAX_PREVIEW_RECEIPT_BYTES) {
            throw 'preview artifact identity source digest metadata exceeded its bounded byte count.'
        }
        ConvertTo-PreviewSha256 -Bytes $canonicalBytes -Deadline $deadline
    } finally {
        foreach ($directoryAuthority in @($retainedSourceDirectoryAuthorities)) {
            if ($null -ne $directoryAuthority -and $null -ne $directoryAuthority.Handle) {
                $directoryAuthority.Handle.Dispose()
            }
        }
    }
}

function Get-PreviewToolPath {
    param(
        [string]$Name,
        [datetime]$Deadline,
        [object]$RustupAuthority
    )

    # rustup which from the canonical user install is the only accepted source for the build tool path.
    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $canonicalRustupPath = [IO.Path]::GetFullPath($rustupPath)
    if (-not ([IO.Path]::GetExtension($canonicalRustupPath)).Equals('.exe', [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $canonicalRustupPath -PathType Leaf)) {
            throw 'preview.identity.rustup-resolution-failed'
    }
    $ownsRustupAuthority = $null -eq $RustupAuthority
    $rustup = if ($ownsRustupAuthority) {
        Open-PreviewArtifactNoFollow -Path $canonicalRustupPath
    } else {
        $RustupAuthority
    }
    try {
        if (-not $rustup.Path.Equals($canonicalRustupPath, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'preview artifact identity rejected a rustup executable outside the canonical user install.'
        }
        Assert-PreviewToolAuthorityStable -Authority $rustup -Deadline $Deadline
        Assert-PreviewDeadline -Deadline $Deadline
        $result = Invoke-PreviewExternalCommand -FilePath $rustup.Path -Arguments @('which', $Name) -Deadline $Deadline -Environment (Get-PreviewToolEnvironment)
        $toolLines = @($result.Output -split "`r?`n" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    } finally {
        if ($ownsRustupAuthority) {
            $rustup.Stream.Dispose()
        }
    }
    if ($toolLines.Count -ne 1) {
        throw 'preview.identity.tool-resolution-failed'
    }
    $path = [IO.Path]::GetFullPath($toolLines[0].ToString().Trim())
    if (-not ([IO.Path]::GetExtension($path)).Equals('.exe', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview.identity.non-executable-tool'
    }
    $rustupHomePrefix = [IO.Path]::GetFullPath($rustupHomePath).TrimEnd('\') + '\'
    if (-not $path.StartsWith($rustupHomePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview.identity.tool-outside-rustup-home'
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw 'preview.identity.tool-open-failed'
    }
    $path
}

function Open-PreviewToolAuthority {
    param(
        [string]$Name,
        [datetime]$Deadline,
        [object]$RustupAuthority
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $path = Get-PreviewToolPath -Name $Name -Deadline $Deadline -RustupAuthority $RustupAuthority
    $toolParentChain = Open-PreviewDirectoryAuthorityChain -Path (Split-Path -Parent $path)
    $opened = $null
    try {
        $opened = Open-PreviewArtifactNoFollow -Path $path
        Assert-PreviewDeadline -Deadline $Deadline
        [pscustomobject]@{
            Name = $Name
            Path = $opened.Path
            Stream = $opened.Stream
            FileIdentity = $opened.FileIdentity
            Length = [int64]$opened.Length
            Directories = @($toolParentChain.OutputRootAncestorChain)
        }
        $opened = $null
        $toolParentChain = $null
    } finally {
        if ($null -ne $opened) {
            $opened.Stream.Dispose()
        }
        if ($null -ne $toolParentChain) {
            Close-PreviewDirectoryAuthorityChain -Authority $toolParentChain
        }
    }
}

function Assert-PreviewToolAuthorityStable {
    param(
        [object]$Authority,
        [datetime]$Deadline
    )

    Assert-PreviewDeadline -Deadline $Deadline
    if ($null -eq $Authority -or $null -eq $Authority.Stream) {
        throw 'preview tool authority is missing its retained stream.'
    }
    foreach ($directory in @($Authority.Directories)) {
        if ($null -eq $directory -or $null -eq $directory.Handle -or
            [DevManagerPreviewArtifactNative]::Identity($directory.Handle) -ne $directory.FileIdentity) {
            throw 'preview.identity.tool-directory-changed'
        }
    }
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Stream.SafeFileHandle) -ne $Authority.FileIdentity -or
        [int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw 'preview.identity.tool-changed'
    }
}

function Close-PreviewToolAuthority {
    param([object]$Authority)

    if ($null -ne $Authority -and $null -ne $Authority.Stream) {
        $Authority.Stream.Dispose()
    }
    if ($null -ne $Authority -and $null -ne $Authority.Directories) {
        Close-PreviewDirectoryAuthorityChain -Authority $Authority
    }
}

function Get-PreviewHostTarget {
    param(
        [string]$RustcPath,
        [datetime]$Deadline,
        [object]$RustupAuthority
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $rustcPath = if ([string]::IsNullOrWhiteSpace($RustcPath)) {
        Get-PreviewToolPath -Name 'rustc' -Deadline $Deadline -RustupAuthority $RustupAuthority
    } else {
        $RustcPath
    }
    $versionResult = Invoke-PreviewExternalCommand -FilePath $rustcPath -Arguments @('-vV') -Deadline $Deadline -Environment (Get-PreviewToolEnvironment)
    $versionLines = @($versionResult.Output -split "`r?`n")
    $hostLine = $versionLines | Where-Object { $_ -match '^host:\s*(.+)$' } | Select-Object -First 1
    if ($null -eq $hostLine -or $hostLine -notmatch '^host:\s*(.+)$') {
        throw 'preview artifact identity could not parse the Rust host target.'
    }
    $Matches[1].Trim()
}

function Get-PreviewBuildIdentity {
    param(
        [datetime]$Deadline,
        [hashtable]$ToolAuthorities,
        [switch]$RetainToolAuthorities
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $ownsToolAuthorities = $null -eq $ToolAuthorities
    $identityResult = $null
    $newRustupAuthority = $null
    $newRustupHomeAuthority = $null
    $newRustcAuthority = $null
    $newCargoAuthority = $null
    try {
    if ($ownsToolAuthorities) {
        $newRustupAuthority = Open-PreviewArtifactNoFollow -Path $rustupPath
        $newRustupHomeAuthority = Open-PreviewDirectoryNoFollow -Path $rustupHomePath
        $newRustcAuthority = Open-PreviewToolAuthority -Name 'rustc' -Deadline $Deadline -RustupAuthority $newRustupAuthority
        $newCargoAuthority = Open-PreviewToolAuthority -Name 'cargo' -Deadline $Deadline -RustupAuthority $newRustupAuthority
        $ToolAuthorities = @{
            rustup = $newRustupAuthority
            rustupHome = $newRustupHomeAuthority
            rustc = $newRustcAuthority
            cargo = $newCargoAuthority
        }
    } elseif (-not $ToolAuthorities.ContainsKey('rustup') -or
        -not $ToolAuthorities.ContainsKey('rustupHome')) {
        throw 'preview artifact identity requires retained rustup and RUSTUP_HOME authorities.'
    }
    # The clean-tree contract is the canonical git status --porcelain check.
    Assert-PreviewDeadline -Deadline $Deadline
    $statusResult = Invoke-PreviewExternalCommand -FilePath 'git' -Arguments @(
        '-C', $canonicalWorktree, 'status', '--porcelain', '--untracked-files=all'
    ) -Deadline $Deadline -Environment @{}
    $status = $statusResult.Output.Trim()
    if (-not [string]::IsNullOrWhiteSpace($status)) {
            throw 'preview.identity.source-tree-dirty'
    }
    $overrideNames = @(
        'CARGO_BUILD_TARGET',
        'CARGO_BUILD_RUSTFLAGS',
        'RUSTFLAGS',
        'CARGO_ENCODED_RUSTFLAGS',
        'CARGO_HOME',
        'RUSTUP_HOME',
        'RUSTUP_TOOLCHAIN',
        'RUSTC',
        'RUSTC_WRAPPER',
        'RUSTC_WORKSPACE_WRAPPER',
        'RUSTC_BOOTSTRAP',
        'RUSTDOC',
        'RUSTDOCFLAGS',
        'CARGO_BUILD_RUSTC',
        'CARGO_BUILD_RUSTC_WRAPPER',
        'CARGO_INCREMENTAL'
    )
    $overrideNames += @(Get-ChildItem Env: | Where-Object {
            $_.Name -like 'CARGO_PROFILE_*' -or
            $_.Name -match '^CARGO_TARGET_.+_(RUSTFLAGS|RUSTC|LINKER)$'
        } | Select-Object -ExpandProperty Name)
    foreach ($name in $overrideNames | Sort-Object -Unique) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
                throw 'preview.identity.caller-build-override'
        }
    }

    $features = @()
    $rustupAuthority = $ToolAuthorities['rustup']
    $rustupHomeAuthority = $ToolAuthorities['rustupHome']
    $rustcAuthority = $ToolAuthorities['rustc']
    $cargoAuthority = $ToolAuthorities['cargo']
    Assert-PreviewToolAuthorityStable -Authority $rustupAuthority -Deadline $Deadline
    Assert-PreviewDirectoryAuthorityStable -Authority $rustupHomeAuthority
    $rustcPath = $rustcAuthority.Path
    $cargoPath = $cargoAuthority.Path
    $rustcSha256 = Get-PreviewArtifactSha256 -Stream $rustcAuthority.Stream -Deadline $Deadline
    $cargoSha256 = Get-PreviewArtifactSha256 -Stream $cargoAuthority.Stream -Deadline $Deadline
    $rustcVersion = (Invoke-PreviewExternalCommand -FilePath $rustcPath -Arguments @('-vV') -Deadline $Deadline -Environment (Get-PreviewToolEnvironment)).Output.Trim()
    if ([string]::IsNullOrWhiteSpace($rustcVersion)) {
        throw 'preview artifact identity could not resolve the Rust toolchain.'
    }
    $cargoVersion = (Invoke-PreviewExternalCommand -FilePath $cargoPath -Arguments @('-V') -Deadline $Deadline -Environment (Get-PreviewToolEnvironment)).Output.Trim()
    if ([string]::IsNullOrWhiteSpace($cargoVersion)) {
        throw 'preview artifact identity could not resolve Cargo.'
    }
    Assert-PreviewDeadline -Deadline $Deadline
    $contract = [ordered]@{
        sourceRevision = Get-PreviewSourceRevision -Deadline $Deadline
        sourceTree = Get-PreviewSourceTreeDigest -Deadline $Deadline
        sourceContentDigest = Get-PreviewSourceContentDigest -Deadline $Deadline
        manifestSha256 = Get-PreviewFileSha256 -Path $manifestPath -Deadline $Deadline
        lockSha256 = Get-PreviewFileSha256 -Path $lockPath -Deadline $Deadline
        cargoConfigSha256 = if (Test-Path -LiteralPath $cargoConfigPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $cargoConfigPath -Deadline $Deadline
        } else {
            ''
        }
        rustToolchainSha256 = if (Test-Path -LiteralPath $rustToolchainTomlPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $rustToolchainTomlPath -Deadline $Deadline
        } elseif (Test-Path -LiteralPath $rustToolchainPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $rustToolchainPath -Deadline $Deadline
        } else {
            ''
        }
        globalCargoConfigSha256 = if (Test-Path -LiteralPath $globalCargoConfigTomlPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $globalCargoConfigTomlPath -Deadline $Deadline
        } elseif (Test-Path -LiteralPath $globalCargoConfigPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $globalCargoConfigPath -Deadline $Deadline
        } else {
            ''
        }
        rustcPath = $rustcPath
        rustcSha256 = $rustcSha256
        cargoPath = $cargoPath
        cargoSha256 = $cargoSha256
        rustupPath = $rustupAuthority.Path
        rustupSha256 = Get-PreviewArtifactSha256 -Stream $rustupAuthority.Stream -Deadline $Deadline
        rustupHomePath = $rustupHomeAuthority.Path
        rustupHomeFileIdentity = $rustupHomeAuthority.FileIdentity
        rustcVersion = $rustcVersion
        cargoVersion = $cargoVersion
        target = Get-PreviewHostTarget -RustcPath $rustcPath -Deadline $Deadline -RustupAuthority $rustupAuthority
        profile = $buildProfile
        features = @($features)
        locked = $true
        offline = $true
        manifestPath = $manifestPath
        canonicalWorktree = $canonicalWorktree
    }
    $canonical = $contract | ConvertTo-Json -Depth 12 -Compress
    $canonicalBytes = [Text.Encoding]::UTF8.GetBytes($canonical)
    if ($canonicalBytes.Length -gt $MAX_PREVIEW_RECEIPT_BYTES) {
        throw 'preview artifact identity build contract exceeded its bounded byte count.'
    }
    $digest = ConvertTo-PreviewSha256 -Bytes $canonicalBytes -Deadline $Deadline
    $identityResult = [pscustomobject]@{
        BuildIdentityDigest = $digest
        SourceRevision = $contract.sourceRevision
        SourceTree = $contract.sourceTree
        SourceContentDigest = $contract.sourceContentDigest
        ManifestSha256 = $contract.manifestSha256
        LockSha256 = $contract.lockSha256
        CargoConfigSha256 = $contract.cargoConfigSha256
        GlobalCargoConfigSha256 = $contract.globalCargoConfigSha256
        RustToolchainSha256 = $contract.rustToolchainSha256
        RustcPath = $contract.rustcPath
        RustcSha256 = $contract.rustcSha256
        CargoPath = $contract.cargoPath
        CargoSha256 = $contract.cargoSha256
        RustupPath = $contract.rustupPath
        RustupSha256 = $contract.rustupSha256
        RustupHomePath = $contract.rustupHomePath
        RustupHomeFileIdentity = $contract.rustupHomeFileIdentity
        RustcVersion = $contract.rustcVersion
        CargoVersion = $contract.cargoVersion
        Target = $contract.target
        Profile = $contract.profile
        Features = @($features)
        ManifestPath = $manifestPath
        CanonicalWorktree = $canonicalWorktree
        ToolAuthorities = $ToolAuthorities
    }
    $identityResult
    } finally {
        if ($ownsToolAuthorities -and ($null -eq $identityResult -or -not $RetainToolAuthorities) -and $null -ne $ToolAuthorities) {
            Close-PreviewToolAuthority -Authority $ToolAuthorities['rustup']
            Close-PreviewToolAuthority -Authority $ToolAuthorities['rustc']
            Close-PreviewToolAuthority -Authority $ToolAuthorities['cargo']
            if ($null -ne $ToolAuthorities['rustupHome'] -and $null -ne $ToolAuthorities['rustupHome'].Handle) {
                $ToolAuthorities['rustupHome'].Handle.Dispose()
            }
        } elseif ($ownsToolAuthorities -and $null -eq $identityResult) {
            Close-PreviewToolAuthority -Authority $newRustupAuthority
            Close-PreviewToolAuthority -Authority $newRustcAuthority
            Close-PreviewToolAuthority -Authority $newCargoAuthority
            if ($null -ne $newRustupHomeAuthority -and $null -ne $newRustupHomeAuthority.Handle) {
                $newRustupHomeAuthority.Handle.Dispose()
            }
        }
    }
}

function Get-PreviewEmbeddedBuildIdentity {
    param(
        [IO.Stream]$Stream,
        [datetime]$Deadline,
        [long]$MaxBytes = $MAX_PREVIEW_ARTIFACT_BYTES
    )

    Assert-PreviewDeadline -Deadline $Deadline
    if ($Stream.CanSeek -and $Stream.Length -gt $MaxBytes) {
        throw 'preview.identity.executable-too-large'
    }
    $marker = 'DEV_MANAGER_PREVIEW_BUILD_IDENTITY='
    $buffer = New-Object byte[] 65536
    $carry = ''
    $found = $null
    $Stream.Position = 0
    $total = [int64]0
    while (($read = $Stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
        Assert-PreviewDeadline -Deadline $Deadline
        $total += $read
        if ($total -gt $MaxBytes) {
            throw 'preview.identity.executable-too-large'
        }
        $text = [Text.Encoding]::ASCII.GetString($buffer, 0, $read)
        $search = $carry + $text
        $matches = [Text.RegularExpressions.Regex]::Matches($search, [Text.RegularExpressions.Regex]::Escape($marker) + '([0-9a-fA-F]{64})')
        foreach ($match in $matches) {
            $identity = $match.Groups[1].Value.ToLowerInvariant()
            if ($null -ne $found -and $found -ne $identity) {
                throw 'preview artifact authority found conflicting embedded build identities.'
            }
            $found = $identity
        }
        $keep = [Math]::Min($search.Length, $marker.Length + 64)
        $carry = if ($keep -eq 0) { '' } else { $search.Substring($search.Length - $keep) }
    }
    if ($null -eq $found) {
        throw 'preview artifact authority found no embedded build identity.'
    }
    $found
}

function Open-PreviewLaunchAuthority {
    param(
        [string]$Path,
        [object[]]$ExistingDirectories,
        [object]$ExistingAuthority
    )

    $canonicalPath = [IO.Path]::GetFullPath($Path)
    if ($null -ne $ExistingAuthority) {
        if ($canonicalPath -ne $ExistingAuthority.Path) {
            throw 'preview artifact authority rejected a launch path different from the retained canonical artifact.'
        }
        return [pscustomobject]@{
            Path = $canonicalPath
            Binary = $ExistingAuthority.Binary
            Directories = $ExistingAuthority.Directories
            Worktree = $ExistingAuthority.Worktree
            TargetRoot = $ExistingAuthority.TargetRoot
            TargetRun = $ExistingAuthority.TargetRun
            BinaryParent = $ExistingAuthority.BinaryParent
            ArtifactParent = $ExistingAuthority.ArtifactParent
            OwnsHandles = $false
        }
    }
    $authority = $null
    $directoryMap = @{}
    $createdDirectories = [System.Collections.Generic.List[object]]::new()
    $binary = $null
    try {
        if ($null -ne $ExistingDirectories -and @($ExistingDirectories).Count -eq 3) {
            foreach ($existing in @($ExistingDirectories)) {
                $directoryMap[$existing.Path] = $existing
            }
        }

        $requiredDirectories = @(
            $canonicalWorktree,
            $targetRoot,
            $TargetRunDir,
            (Split-Path -Parent $canonicalPath),
            (Split-Path -Parent $artifactReceiptPath)
        )
        foreach ($requiredPath in $requiredDirectories) {
            foreach ($directoryPath in @(Get-PreviewDirectoryChain -Path $requiredPath)) {
                $normalizedPath = [IO.Path]::GetFullPath($directoryPath)
                $rootPath = [IO.Path]::GetPathRoot($normalizedPath)
                if ($normalizedPath.Length -gt $rootPath.Length) {
                    $normalizedPath = $normalizedPath.TrimEnd('\')
                } else {
                    $normalizedPath = $rootPath
                }
                if (-not $directoryMap.ContainsKey($normalizedPath)) {
                    $isReceiptParent = $normalizedPath.Equals(
                        [IO.Path]::GetFullPath((Split-Path -Parent $artifactReceiptPath)).TrimEnd('\'),
                        [StringComparison]::OrdinalIgnoreCase)
                    $openedDirectory = Open-PreviewDirectoryNoFollow -Path $normalizedPath -ForPublication:$isReceiptParent
                    $directoryMap[$normalizedPath] = $openedDirectory
                    [void]$createdDirectories.Add($openedDirectory)
                }
            }
        }

        $binary = Open-PreviewArtifactNoFollow -Path $canonicalPath
        $directories = @($directoryMap.Values)
        $authority = [pscustomobject]@{
            Path = $canonicalPath
            Binary = $binary
            Directories = $directories
            Worktree = $directoryMap[$canonicalWorktree]
            TargetRoot = $directoryMap[[IO.Path]::GetFullPath($targetRoot)]
            TargetRun = $directoryMap[[IO.Path]::GetFullPath($TargetRunDir)]
            BinaryParent = $directoryMap[[IO.Path]::GetFullPath((Split-Path -Parent $canonicalPath))]
            ArtifactParent = $directoryMap[[IO.Path]::GetFullPath((Split-Path -Parent $artifactReceiptPath))]
            OwnsHandles = $true
        }
        $authority
    } catch {
        foreach ($directory in @($createdDirectories)) {
            if ($null -ne $directory -and $null -ne $directory.Handle) {
                try { $directory.Handle.Dispose() } catch { }
            }
        }
        if ($null -ne $authority -and $null -ne $authority.Binary) {
            $authority.Binary.Stream.Dispose()
        } elseif ($null -ne $binary) {
            $binary.Stream.Dispose()
        }
        throw
    }
}

function Assert-PreviewReceiptSchema {
    param([object]$Receipt)

    $top = @('schema', 'artifact', 'canonicalWorktree', 'sourceRevision', 'sourceTree', 'sourceContentDigest', 'buildIdentityDigest', 'buildContract', 'binaryPath', 'binaryName', 'binaryFileIdentity', 'binaryLength', 'binarySha256', 'embeddedBuildIdentity')
    $actual = @($Receipt.PSObject.Properties.Name)
    if ($actual.Count -ne $top.Count -or @($top | Where-Object { $_ -notin $actual }).Count -ne 0) {
        throw 'preview receipt has a non-strict schema.'
    }
    $contract = $Receipt.buildContract
    $requiredContract = @('package', 'binary', 'profile', 'target', 'locked', 'offline', 'manifestPath', 'features', 'targetDir', 'rustToolchainSha256', 'globalCargoConfigSha256', 'rustcPath', 'rustcSha256', 'cargoPath', 'cargoSha256', 'rustupPath', 'rustupSha256', 'rustupHomePath', 'rustupHomeFileIdentity', 'rustcVersion', 'cargoVersion')
    $actualContract = @($contract.PSObject.Properties.Name)
    if ($actualContract.Count -ne $requiredContract.Count -or @($requiredContract | Where-Object { $_ -notin $actualContract }).Count -ne 0) {
        throw 'preview receipt has a non-strict build contract.'
    }
}

function Assert-PreviewReceiptMatches {
    param(
        [object]$Receipt,
        [object]$Authority,
        [object]$BuildIdentity
    )

    Assert-PreviewReceiptSchema -Receipt $Receipt
    if ($Receipt.schema -ne $artifactSchema -or
        $Receipt.artifact -ne $artifactName -or
        $Receipt.canonicalWorktree -ne $canonicalWorktree -or
        $Receipt.sourceRevision -ne $BuildIdentity.SourceRevision -or
        $Receipt.sourceTree -ne $BuildIdentity.SourceTree -or
        $Receipt.sourceContentDigest -ne $BuildIdentity.SourceContentDigest -or
        $Receipt.buildIdentityDigest -ne $BuildIdentity.BuildIdentityDigest -or
        $Receipt.binaryPath -ne $Authority.Path -or
        $Receipt.binaryName -ne $artifactBinaryName -or
        $Receipt.binaryFileIdentity -ne $Authority.Binary.FileIdentity -or
        [int64]$Receipt.binaryLength -ne [int64]$Authority.Binary.Length -or
        $Receipt.embeddedBuildIdentity -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview receipt does not match the current retained build authority.'
    }
    $contract = $Receipt.buildContract
    if ($contract.package -ne 'devmanager' -or
        $contract.binary -ne $artifactName -or
        $contract.profile -ne $BuildIdentity.Profile -or
        $contract.target -ne $BuildIdentity.Target -or
        -not [bool]$contract.locked -or
        -not [bool]$contract.offline -or
        $contract.manifestPath -ne $BuildIdentity.ManifestPath -or
        @($contract.features).Count -ne 0 -or
        $contract.targetDir -ne [IO.Path]::GetFullPath($TargetRunDir) -or
        $contract.rustToolchainSha256 -ne $BuildIdentity.RustToolchainSha256 -or
        $contract.globalCargoConfigSha256 -ne $BuildIdentity.GlobalCargoConfigSha256 -or
        $contract.rustcPath -ne $BuildIdentity.RustcPath -or
        $contract.rustcSha256 -ne $BuildIdentity.RustcSha256 -or
        $contract.cargoPath -ne $BuildIdentity.CargoPath -or
        $contract.cargoSha256 -ne $BuildIdentity.CargoSha256 -or
        $contract.rustupPath -ne $BuildIdentity.RustupPath -or
        $contract.rustupSha256 -ne $BuildIdentity.RustupSha256 -or
        $contract.rustupHomePath -ne $BuildIdentity.RustupHomePath -or
        $contract.rustupHomeFileIdentity -ne $BuildIdentity.RustupHomeFileIdentity -or
        $contract.rustcVersion -ne $BuildIdentity.RustcVersion -or
        $contract.cargoVersion -ne $BuildIdentity.CargoVersion) {
        throw 'preview receipt build contract does not match the current retained authority.'
    }
}

function Assert-PreviewArtifactIdentity {
    param(
        [object]$Receipt,
        [object]$Authority,
        [object]$BuildIdentity
    )

    Assert-PreviewReceiptMatches -Receipt $Receipt -Authority $Authority -BuildIdentity $BuildIdentity
}

function Read-PreviewArtifactReceipt {
    param(
        [object]$ParentAuthority,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Assert-PreviewDirectoryAuthorityStable -Authority $ParentAuthority
    $opened = Open-PreviewArtifactRelative -ParentAuthority $ParentAuthority -Name ([IO.Path]::GetFileName($artifactReceiptPath))
    try {
        $receiptJson = Read-PreviewUtf8Text -Stream $opened.Stream -Deadline $Deadline -MaxBytes $MAX_PREVIEW_RECEIPT_BYTES
        $receipt = $receiptJson | ConvertFrom-Json
        Assert-PreviewReceiptSchema -Receipt $receipt
        $hash = Get-PreviewArtifactSha256 -Stream $opened.Stream -Deadline $Deadline -MaxBytes $MAX_PREVIEW_RECEIPT_BYTES
        if ([DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle) -ne $opened.FileIdentity -or
            [int64][DevManagerPreviewArtifactNative]::Length($opened.Stream.SafeFileHandle) -ne [int64]$opened.Length) {
            throw 'preview receipt file identity changed while its retained handle was read.'
        }
        [pscustomobject]@{
            Receipt = $receipt
            Stream = $opened.Stream
            FileIdentity = $opened.FileIdentity
            Length = [int64]$opened.Length
            Sha256 = $hash
            Parent = $ParentAuthority
        }
        $opened = $null
    } finally {
        if ($null -ne $opened) {
            $opened.Stream.Dispose()
        }
    }
}

function New-PreviewArtifactReceipt {
    param(
        [object]$Authority,
        [object]$BuildIdentity,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $binary = $Authority.Binary
    $hash = Get-PreviewArtifactSha256 -Stream $binary.Stream -Deadline $Deadline
    $embedded = Get-PreviewEmbeddedBuildIdentity -Stream $binary.Stream -Deadline $Deadline
    if ($embedded -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview artifact authority embedded build identity does not match the current source/build digest.'
    }
    $receipt = [pscustomobject][ordered]@{
        schema = $artifactSchema
        artifact = $artifactName
        canonicalWorktree = $canonicalWorktree
        sourceRevision = $BuildIdentity.SourceRevision
        sourceTree = $BuildIdentity.SourceTree
        sourceContentDigest = $BuildIdentity.SourceContentDigest
        buildIdentityDigest = $BuildIdentity.BuildIdentityDigest
        buildContract = [pscustomobject][ordered]@{
            package = 'devmanager'
            binary = $artifactName
            profile = $BuildIdentity.Profile
            target = $BuildIdentity.Target
            locked = $true
            offline = $true
            manifestPath = $BuildIdentity.ManifestPath
            features = @($BuildIdentity.Features)
            targetDir = [IO.Path]::GetFullPath($TargetRunDir)
            rustToolchainSha256 = $BuildIdentity.RustToolchainSha256
            globalCargoConfigSha256 = $BuildIdentity.GlobalCargoConfigSha256
            rustcPath = $BuildIdentity.RustcPath
            rustcSha256 = $BuildIdentity.RustcSha256
            cargoPath = $BuildIdentity.CargoPath
            cargoSha256 = $BuildIdentity.CargoSha256
            rustupPath = $BuildIdentity.RustupPath
            rustupSha256 = $BuildIdentity.RustupSha256
            rustupHomePath = $BuildIdentity.RustupHomePath
            rustupHomeFileIdentity = $BuildIdentity.RustupHomeFileIdentity
            rustcVersion = $BuildIdentity.RustcVersion
            cargoVersion = $BuildIdentity.CargoVersion
        }
        binaryPath = $Authority.Path
        binaryName = $artifactBinaryName
        binaryFileIdentity = $binary.FileIdentity
        binaryLength = [int64]$binary.Length
        binarySha256 = $hash
        embeddedBuildIdentity = $embedded
    }
    Assert-PreviewReceiptSchema -Receipt $receipt
    $json = $receipt | ConvertTo-Json -Depth 20 -Compress
    $jsonBytes = [Text.Encoding]::UTF8.GetBytes($json)
    if ($jsonBytes.Length -gt $MAX_PREVIEW_RECEIPT_BYTES) {
        throw 'preview artifact receipt exceeded its bounded byte count.'
    }
    [DevManagerPreviewArtifactNative]::WriteAtomicPreviewReceiptRelative(
        $Authority.ArtifactParent.Handle,
        [IO.Path]::GetFileName($artifactReceiptPath),
        $json)
    $receipt
}

function Assert-PreviewLaunchAuthorityStable {
    param(
        [object]$Authority,
        [object]$ReceiptState,
        [object]$BuildIdentity,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $deadline = $Deadline
    foreach ($directory in @($Authority.Directories)) {
        if ([DevManagerPreviewArtifactNative]::Identity($directory.Handle) -ne $directory.FileIdentity) {
            throw 'preview.authority.directory-changed'
        }
    }
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Binary.Stream.SafeFileHandle) -ne $Authority.Binary.FileIdentity) {
        throw 'preview artifact authority executable identity changed.'
    }
    Assert-PreviewToolAuthorityStable -Authority $BuildIdentity.ToolAuthorities['rustup'] -Deadline $deadline
    Assert-PreviewDirectoryAuthorityStable -Authority $BuildIdentity.ToolAuthorities['rustupHome']
    Assert-PreviewToolAuthorityStable -Authority $BuildIdentity.ToolAuthorities['rustc'] -Deadline $deadline
    Assert-PreviewToolAuthorityStable -Authority $BuildIdentity.ToolAuthorities['cargo'] -Deadline $deadline
    $binaryHash = Get-PreviewArtifactSha256 -Stream $Authority.Binary.Stream -Deadline $deadline
    if ($binaryHash -ne [string]$ReceiptState.Receipt.binarySha256 -or
        [int64]$Authority.Binary.Length -ne [int64]$ReceiptState.Receipt.binaryLength) {
        throw 'preview artifact authority executable content changed.'
    }
    if ((Get-PreviewEmbeddedBuildIdentity -Stream $Authority.Binary.Stream -Deadline $deadline) -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview artifact authority executable build identity changed.'
    }
    if ([DevManagerPreviewArtifactNative]::Identity($ReceiptState.Stream.SafeFileHandle) -ne $ReceiptState.FileIdentity) {
        throw 'preview receipt handle remains held but its identity changed.'
    }
    $currentIdentity = Get-PreviewBuildIdentity -Deadline $deadline -ToolAuthorities $BuildIdentity.ToolAuthorities -RetainToolAuthorities
    if ($currentIdentity.BuildIdentityDigest -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview artifact identity source tree changed during the isolated build.'
    }
}

function Join-PreviewProcessBounded {
    param(
        [object]$Launch,
        [datetime]$Deadline,
        [string]$Label
    )

    $process = $Launch.Process
    $job = $Launch.Job
    $joined = $false
    if ($null -eq $process -or $null -eq $job) {
        throw 'preview.process.missing-owned-process'
    }
    $remaining = 0
    try { $remaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline } catch { $remaining = 0 }
    if ($process.HasExited) {
        $joined = $true
    } elseif ($remaining -gt 0) {
        $joined = $process.WaitForExit($remaining)
    }
    $active = $job.ActiveProcessCount()
    if (-not $joined -or $active -gt 0) {
        try {
            $job.Terminate()
        } catch {
            $Launch.JoinState = 'join-failed-job-termination'
            throw 'preview.process.job-termination-failed'
        }
        $remaining = 0
        try { $remaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline } catch { $remaining = 0 }
        if (-not $process.HasExited -and $remaining -gt 0) {
            $joined = $process.WaitForExit($remaining)
        } else {
            $joined = $process.HasExited
        }
        $active = $job.ActiveProcessCount()
        if (-not $joined -or $active -gt 0) {
            $Launch.JoinState = 'join-unconfirmed-after-job-termination'
            throw 'preview.process.join-unconfirmed'
        }
        $Launch.JoinState = 'killed-and-joined'
        $Launch.ExitCode = -1
    } else {
        $Launch.JoinState = 'joined'
        $Launch.ExitCode = $process.ExitCode
    }
    $job.Dispose()
    $Launch.ExitCode
}

function Assert-NoLivePreviewProcesses {
    param([datetime]$Deadline)

    foreach ($launch in @($activePreviewProcesses)) {
        if ($null -eq $launch -or $launch.JoinState -in @('joined', 'killed-and-joined')) {
            continue
        }
        if ($null -ne $launch -and $null -ne $launch.Process -and -not $launch.Process.HasExited) {
            [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'before manifest publication')
        }
        if ($null -ne $launch -and $null -ne $launch.Job -and $launch.Job.ActiveProcessCount() -gt 0) {
            throw 'preview process remained live before manifest publication.'
        }
    }
}

function Close-PreviewTrackedProcesses {
    param([datetime]$Deadline)

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    $cleanupFailures = [System.Collections.Generic.List[string]]::new()
    foreach ($launch in @($activePreviewProcesses)) {
        if ($null -eq $launch -or $launch.JoinState -in @('joined', 'killed-and-joined')) { continue }
        try {
            [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'tracked process cleanup')
        } catch {
            [void]$cleanupFailures.Add('preview.cleanup.failed')
            try {
                if ($null -ne $launch.Job) {
                    $launch.Job.Dispose()
                }
            } catch {
                [void]$cleanupFailures.Add('preview.cleanup.failed')
            }
        }
        if ($null -ne $launch.Authority) {
            try {
                Close-PreviewLaunchAuthority -Authority $launch.Authority
            } catch {
                [void]$cleanupFailures.Add('preview.cleanup.failed')
            }
        }
    }
    $activePreviewProcesses.Clear()
    if ($cleanupFailures.Count -gt 0) {
        throw 'preview.cleanup.failed'
    }
}

function Invoke-PreviewCleanupStep {
    param(
        [scriptblock]$Action,
        [System.Collections.Generic.List[string]]$Failures
    )

    try {
        & $Action
    } catch {
        [void]$Failures.Add('preview.cleanup.failed')
    }
}

function Invoke-PreviewFinalCleanup {
    param(
        [datetime]$Deadline,
        [object]$ManifestAuthority,
        [object]$OutputRootAuthority,
        [object[]]$RetainedOutputAuthorities,
        [object]$ArtifactReceiptState,
        [object]$ArtifactAuthority,
        [object]$BuildIdentity,
        [object[]]$BuildDirectories,
        [object]$OldTargetDir,
        [object]$OldBuildJobs,
        [object]$OldBuildIdentity
    )

    $failures = [System.Collections.Generic.List[string]]::new()
    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        Close-PreviewTrackedProcesses -Deadline $Deadline
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        Close-PreviewOutputAuthority -Authority $ManifestAuthority
    }
    foreach ($outputAuthority in @($RetainedOutputAuthorities)) {
        Invoke-PreviewCleanupStep -Failures $failures -Action {
            Close-PreviewOutputAuthority -Authority $outputAuthority
        }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        if ($null -ne $OutputRootAuthority) {
            Assert-PreviewDirectoryAuthorityStable -Authority $OutputRootAuthority
        }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        Close-PreviewDirectoryAuthorityChain -Authority $OutputRootAuthority
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        if ($null -ne $ArtifactReceiptState -and $null -ne $ArtifactReceiptState.Stream) {
            $ArtifactReceiptState.Stream.Dispose()
        }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        Close-PreviewLaunchAuthority -Authority $ArtifactAuthority
    }
    $script:PreviewArtifactAuthority = $null
    foreach ($directory in @($BuildDirectories)) {
        Invoke-PreviewCleanupStep -Failures $failures -Action {
            if ($null -ne $directory -and $null -ne $directory.Handle) {
                $directory.Handle.Dispose()
            }
        }
    }
    if ($null -ne $BuildIdentity -and $null -ne $BuildIdentity.ToolAuthorities) {
        foreach ($name in @('rustup', 'rustc', 'cargo')) {
            Invoke-PreviewCleanupStep -Failures $failures -Action {
                Close-PreviewToolAuthority -Authority $BuildIdentity.ToolAuthorities[$name]
            }
        }
        Invoke-PreviewCleanupStep -Failures $failures -Action {
            if ($null -ne $BuildIdentity.ToolAuthorities['rustupHome'] -and
                $null -ne $BuildIdentity.ToolAuthorities['rustupHome'].Handle) {
                $BuildIdentity.ToolAuthorities['rustupHome'].Handle.Dispose()
            }
        }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        if ($null -eq $OldTargetDir) { $env:CARGO_TARGET_DIR = $null } else { $env:CARGO_TARGET_DIR = $OldTargetDir }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        if ($null -eq $OldBuildJobs) { $env:CARGO_BUILD_JOBS = $null } else { $env:CARGO_BUILD_JOBS = $OldBuildJobs }
    }
    Invoke-PreviewCleanupStep -Failures $failures -Action {
        if ($null -eq $OldBuildIdentity) { $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $null } else { $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $OldBuildIdentity }
    }
    if ($failures.Count -gt 0) {
        throw 'preview.cleanup.failed'
    }
}

function Wait-PreviewWindowReady {
    param(
        [object]$Process,
        [datetime]$Deadline,
        [int]$MaxAttempts = 80
    )

    for ($ReadinessAttempt = 1; $ReadinessAttempt -le $MaxAttempts; $ReadinessAttempt++) {
        Assert-PreviewDeadline -Deadline $Deadline
        if ($Process.HasExited) {
            throw 'preview readiness handshake observed an exited process.'
        }
        try {
            $Process.Refresh()
        } catch {
            throw 'preview.readiness-refresh-failed'
        }
        if ($Process.MainWindowHandle -ne 0) {
            return [pscustomobject]@{
                Window = [IntPtr]$Process.MainWindowHandle
                Attempts = $ReadinessAttempt
                ReadinessHandshake = 'ready'
            }
        }
        Write-Verbose ("preview readiness-retry attempt {0}" -f $ReadinessAttempt)
        Wait-PreviewBackoff -Deadline $Deadline -Milliseconds 25
    }
    throw 'preview window readiness handshake did not complete before its deadline.'
}

function Invoke-TrustedPreview {
    param(
        [object]$ReceiptState,
        [object]$BuildIdentity,
        [string]$Path,
        [string[]]$Arguments,
        [datetime]$Deadline
    )

    $launch = Start-TrustedPreview -Path $Path -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity -Arguments $Arguments -Deadline $Deadline
    [void]$activePreviewProcesses.Add($launch)
    $script:PreviewLastExitCode = Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'regular capture'
    Close-PreviewLaunchAuthority -Authority $launch.Authority
    $launch.Authority = $null
}

function Start-TrustedPreview {
    param(
        [object]$ReceiptState,
        [object]$BuildIdentity,
        [string]$Path,
        [string[]]$Arguments,
        [datetime]$Deadline
    )

    $authority = Open-PreviewLaunchAuthority -Path $Path -ExistingAuthority $script:PreviewArtifactAuthority
    $launch = $null
    $rollbackError = $null
    try {
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity -Deadline $Deadline
        Assert-PreviewDeadline -Deadline $Deadline
        $startInfo = New-PreviewProcessStartInfo -FilePath $authority.Path -Arguments $Arguments -WorkingDirectory $canonicalWorktree -Environment (Get-PreviewToolEnvironment -BuildIdentity $BuildIdentity)
        $owned = [DevManagerPreviewArtifactNative]::StartProcessInJob($startInfo)
        $launch = [pscustomobject]@{
            Process = $owned.Process
            Job = $owned
            Authority = $authority
            JoinState = 'started'
            ReadinessHandshake = 'pending'
        }
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity -Deadline $Deadline
        $launch
        $authority = $null
    } finally {
        if ($null -ne $launch -and $null -ne $authority) {
            try {
                [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'launch rollback')
            } catch {
                $rollbackError = 'preview.cleanup.failed'
            }
        }
        if ($null -ne $authority) {
            Close-PreviewLaunchAuthority -Authority $authority
        }
        if ($null -ne $rollbackError) {
            throw $rollbackError
        }
    }
}

function Get-PngDimensions {
    param(
        [object]$Authority,
        [datetime]$Deadline
    )

    if ($Deadline -eq [datetime]::MinValue) {
        $Deadline = New-PreviewIoDeadline
    }
    Assert-PreviewDeadline -Deadline $Deadline
    if ([int64]$Authority.Length -lt 24 -or [int64]$Authority.Length -gt $MAX_PREVIEW_PNG_BYTES) {
        throw 'PNG is outside its bounded size contract.'
    }
    $header = New-Object byte[] 24
    $offset = 0
    $Authority.Stream.Position = 0
    while ($offset -lt $header.Length) {
        Assert-PreviewDeadline -Deadline $Deadline
        $read = $Authority.Stream.Read($header, $offset, $header.Length - $offset)
        if ($read -le 0) {
            throw 'PNG is shorter than its signature and IHDR.'
        }
        $offset += $read
    }
    $signature = [byte[]](137, 80, 78, 71, 13, 10, 26, 10)
    for ($index = 0; $index -lt $signature.Length; $index++) {
        if ($header[$index] -ne $signature[$index]) {
            throw 'PNG signature is invalid.'
        }
    }
    $chunkType = [Text.Encoding]::ASCII.GetString($header, 12, 4)
    if ($chunkType -ne 'IHDR') {
        throw 'PNG first chunk is not IHDR.'
    }
    $width = ([uint32]$header[16] -shl 24) -bor
        ([uint32]$header[17] -shl 16) -bor
        ([uint32]$header[18] -shl 8) -bor [uint32]$header[19]
    $height = ([uint32]$header[20] -shl 24) -bor
        ([uint32]$header[21] -shl 16) -bor
        ([uint32]$header[22] -shl 8) -bor [uint32]$header[23]
    [pscustomobject]@{ Width = [uint32]$width; Height = [uint32]$height }
}

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $approvedEvidenceRoot "ui-capture-$runToken"
} else {
    $OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
    if (-not ($OutputRoot.Equals($approvedEvidenceRoot, [StringComparison]::OrdinalIgnoreCase) -or
            $OutputRoot.StartsWith($approvedEvidencePrefix, [StringComparison]::OrdinalIgnoreCase))) {
        throw 'OutputRoot must remain beneath the isolated native-next evidence root.'
    }
    $OutputRoot = Join-Path $OutputRoot "run-$runToken"
}

$targetRoot = [IO.Path]::GetFullPath($TargetDir)
$TargetRunDir = Join-Path $targetRoot "run-$runToken"
if (-not $ValidateOnly) {
    New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
}
if (-not (Test-Path -LiteralPath $artifactReceiptParentPath -PathType Container)) {
    New-Item -ItemType Directory -Force -Path $artifactReceiptParentPath | Out-Null
}
if (-not (Test-Path -LiteralPath $targetRoot -PathType Container)) {
    New-Item -ItemType Directory -Force -Path $targetRoot | Out-Null
}
if (-not (Test-Path -LiteralPath $TargetRunDir -PathType Container)) {
    New-Item -ItemType Directory -Force -Path $TargetRunDir | Out-Null
}

$oldTargetDir = $env:CARGO_TARGET_DIR
$oldBuildJobs = $env:CARGO_BUILD_JOBS
$oldBuildIdentity = $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY
$buildWorktreeAuthority = $null
$buildTargetRootAuthority = $null
$buildTargetRunAuthority = $null
$artifactAuthority = $null
$artifactReceiptState = $null
$outputRootAuthority = $null
$manifestAuthority = $null
$retainedOutputAuthorities = [System.Collections.Generic.List[object]]::new()
$activePreviewProcesses = [System.Collections.Generic.List[object]]::new()
$previewDeadline = [datetime]::MinValue
try {
    $buildWorktreeAuthority = Open-PreviewDirectoryNoFollow -Path $canonicalWorktree
    $buildTargetRootAuthority = Open-PreviewDirectoryNoFollow -Path $targetRoot
    $buildTargetRunAuthority = Open-PreviewDirectoryNoFollow -Path $TargetRunDir
    $buildDeadline = [DateTime]::UtcNow.AddMinutes(10)
    $buildIdentity = Get-PreviewBuildIdentity -Deadline $buildDeadline -RetainToolAuthorities
    $env:CARGO_TARGET_DIR = $TargetRunDir
    $env:CARGO_BUILD_JOBS = '1'
    $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $buildIdentity.BuildIdentityDigest

    $buildEnvironment = Get-PreviewToolEnvironment -BuildIdentity $buildIdentity
    $buildEnvironment['CARGO_TARGET_DIR'] = $TargetRunDir
    $buildEnvironment['CARGO_BUILD_JOBS'] = '1'
    $buildEnvironment['DEV_MANAGER_PREVIEW_BUILD_IDENTITY'] = $buildIdentity.BuildIdentityDigest
    $buildResult = Invoke-PreviewExternalCommand -FilePath $buildIdentity.CargoPath -Arguments @(
        'build', '--locked', '--offline', '--manifest-path', $manifestPath,
        '--target', $buildIdentity.Target, '--profile', $buildProfile,
        '--no-default-features', '--bin', 'devmanager-next', '--target-dir', $TargetRunDir,
        '--message-format=json-render-diagnostics'
    ) -Deadline $buildDeadline -Environment $buildEnvironment -MaxOutputBytes $MAX_PREVIEW_ARTIFACT_BYTES
    $artifactPaths = @(
        $buildResult.Output -split "`r?`n" |
            ForEach-Object {
                $line = $_.ToString()
                if ([string]::IsNullOrWhiteSpace($line)) { return }
                try { $message = $line | ConvertFrom-Json } catch { return }
                if ($message.reason -eq 'compiler-artifact' -and
                    $message.target.name -eq 'devmanager-next' -and
                    -not [string]::IsNullOrWhiteSpace($message.executable)) {
                    $message.executable.ToString()
                }
            })
    $uniqueArtifactPaths = @($artifactPaths | Sort-Object -Unique)
    if ($uniqueArtifactPaths.Count -ne 1) {
        throw 'isolated devmanager-next build did not produce exactly one parsed executable artifact.'
    }
    $binary = [IO.Path]::GetFullPath($uniqueArtifactPaths[0])
    $targetRunPrefix = [IO.Path]::GetFullPath($TargetRunDir).TrimEnd('\') + '\'
    if (-not $binary.StartsWith($targetRunPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'isolated devmanager-next build produced an executable outside the retained target run.'
    }
    if (-not ([IO.Path]::GetFileName($binary)).Equals($artifactBinaryName, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'isolated devmanager-next build produced an executable with the wrong name.'
    }
    $afterBuildIdentity = Get-PreviewBuildIdentity -Deadline $buildDeadline -ToolAuthorities $buildIdentity.ToolAuthorities -RetainToolAuthorities
    if ($afterBuildIdentity.BuildIdentityDigest -ne $buildIdentity.BuildIdentityDigest) {
        throw 'preview artifact identity source tree changed during the isolated build.'
    }
    $artifactAuthority = Open-PreviewLaunchAuthority -Path $binary -ExistingDirectories @($buildWorktreeAuthority, $buildTargetRootAuthority, $buildTargetRunAuthority)
    $script:PreviewArtifactAuthority = $artifactAuthority
    $artifactReceipt = New-PreviewArtifactReceipt -Authority $artifactAuthority -BuildIdentity $buildIdentity -Deadline $buildDeadline
    $artifactReceiptState = Read-PreviewArtifactReceipt -ParentAuthority $artifactAuthority.ArtifactParent -Deadline $buildDeadline
    Assert-PreviewReceiptMatches -Receipt $artifactReceiptState.Receipt -Authority $artifactAuthority -BuildIdentity $buildIdentity
    Assert-PreviewLaunchAuthorityStable -Authority $artifactAuthority -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Deadline $buildDeadline
    Write-Verbose ("using one trusted isolated binary per invocation {0} ({1})" -f $binary, $artifactAuthority.Binary.FileIdentity)

    if ($ValidateOnly) {
        Write-Output ("Validated preview artifact identity for {0}." -f $binary)
        return
    }

    $previewDeadline = New-PreviewIoDeadline
    $allFixtureFiles = Get-PreviewFixtureFilesBounded -Root $fixtureRoot -MaxFiles $MAX_SOURCE_DIGEST_FILES -Deadline $previewDeadline
    $fixtureRecords = [System.Collections.Generic.List[object]]::new()
    foreach ($fixtureFile in $allFixtureFiles) {
        try {
            $fixture = Read-PreviewFixture -Path $fixtureFile.FullName -Deadline $previewDeadline
        } catch {
            throw 'preview.fixture.enumeration-failed'
        }
        if ($fixture.schema -ne 'devmanager.ui.preview/v1') {
            Write-Warning 'preview.fixture.unsupported-schema'
            [void]$fixtureRecords.Add([pscustomobject]@{
                FixtureRecord = $fixtureFile.Name
                File = $fixtureFile
                Fixture = $fixture
                Unsupported = $true
            })
            continue
        }
        [void]$fixtureRecords.Add([pscustomobject]@{
            FixtureRecord = $fixtureFile.Name
            File = $fixtureFile
            Fixture = $fixture
            Unsupported = $false
        })
    }
    $fixtureFiles = @($fixtureRecords)
    if (-not $AllFixtures) {
        $fixtureFiles = @($fixtureFiles | Where-Object { $_.FixtureRecord -eq 'component-gallery.json' })
    }
    if ($fixtureFiles.Count -eq 0) {
        throw 'No isolated UI preview fixtures were found beneath tests/fixtures/ui.'
    }

    $themes = if ($AllThemes) { @('dark', 'light') } else { @('dark') }
    $densities = @('compact', 'comfortable')
    $scales = if ($AllScales) { @(100, 125, 150, 200) } else { @(100) }
    $manifest = [System.Collections.Generic.List[object]]::new()
    $captureFailures = 0
    $outputRootAuthority = Open-PreviewDirectoryAuthorityChain -Path $OutputRoot
    Assert-PreviewDirectoryAuthorityStable -Authority $outputRootAuthority

    if ($AutomateWindowStates) {
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class DevManagerPreviewWindow {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")] private static extern IntPtr GetWindowLongPtr(IntPtr hwnd, int index);
    private const long WS_MINIMIZEBOX = 0x00020000L;
    public static bool CanMinimize(IntPtr hwnd) {
        return (GetWindowLongPtr(hwnd, -16).ToInt64() & WS_MINIMIZEBOX) != 0;
    }
}
'@
    }

    function Invoke-WindowStateProbe {
        param(
            [string]$State,
            [string[]]$Arguments,
            [string]$OutputPath,
            [datetime]$Deadline
        )
        $probe = $null
        $launch = $null
        $window = $null
        $exitCode = $null
        $failure = $null
        $joined = $false
        $outcome = 'probe-failed'
        $holdEvidence = 'probe-lifecycle-failed'
        $readiness = $null
        $stateTransitionApplied = $false
        try {
            $launch = Start-TrustedPreview -Path $binary -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Arguments $Arguments -Deadline $Deadline
            [void]$activePreviewProcesses.Add($launch)
            $probe = $launch.Process
            $readiness = Wait-PreviewWindowReady -Process $probe -Deadline $Deadline
            $window = $readiness.Window
            $launch.ReadinessHandshake = $readiness.ReadinessHandshake
            if ($State -eq 'minimized') {
                if ([DevManagerPreviewWindow]::CanMinimize($window)) {
                    [DevManagerPreviewWindow]::ShowWindow($window, 6) | Out-Null
                    $stateTransitionApplied = $true
                } else {
                    $outcome = 'deferred'
                    $holdEvidence = 'window-not-minimizable'
                }
            } elseif ($State -eq 'closed') {
                [DevManagerPreviewWindow]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
                $stateTransitionApplied = $true
            }
            # Join-PreviewProcessBounded owns the probe job and applies the
            # same absolute deadline to natural exit and termination.
            $exitCode = Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label "isolated $State probe"
            $joined = $launch.JoinState -in @('joined', 'killed-and-joined')
            Close-PreviewLaunchAuthority -Authority $launch.Authority
            $launch.Authority = $null
            if ($outcome -ne 'deferred') {
                if ($exitCode -eq 0) {
                    throw 'preview.window-state.output-published-unexpectedly'
                }
                $outputPresent = $false
                $probeOutputAuthority = Try-Open-PreviewOutputAuthority -Path $OutputPath -ParentAuthority $outputRootAuthority -Deadline $Deadline
                if ($null -ne $probeOutputAuthority) {
                    $outputPresent = $true
                    [void]$retainedOutputAuthorities.Add($probeOutputAuthority)
                }
                if ($outputPresent) {
                    throw 'preview.window-state.output-present-after-rejection'
                }
                $outcome = 'rejected'
                $holdEvidence = 'output-absent-after-state-transition'
            }
        } catch {
            $failure = ConvertTo-PreviewSafeDiagnostic -ErrorRecord $_
        } finally {
            if ($null -ne $launch -and $null -ne $launch.Process -and $launch.JoinState -notin @('joined', 'killed-and-joined')) {
                try {
                    [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label "isolated $State probe cleanup")
                } catch {
                    $holdEvidence = 'preview.cleanup.failed'
                    $failure = 'preview.cleanup.failed'
                    $outcome = 'probe-failed'
                }
            }
        }
        [pscustomobject]@{
            Fixture = 'component-gallery'
            Page = "automated-$State-$outcome"
            ScaleMode = 'window-state-automation'
            Outcome = $outcome
            HoldEvidence = $holdEvidence
            Error = $failure
            ExitCode = $exitCode
            JoinState = if ($joined) { 'joined' } else { 'join-unconfirmed' }
            ReadinessHandshake = if ($null -ne $readiness) { $readiness.ReadinessHandshake } else { 'readiness-retry-exhausted' }
            Output = [IO.Path]::GetFileName($OutputPath)
            OutputEvidence = if ($outcome -eq 'rejected' -and $stateTransitionApplied) {
                'output-absent-after-state-transition'
            } elseif ($outcome -eq 'deferred') {
                'window-state-proof-unavailable'
            } else {
                'probe-output-state-unconfirmed'
            }
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        }
    }

    foreach ($fixtureRecord in $fixtureFiles) {
        $fixtureFile = $fixtureRecord.File
        $fixture = $fixtureRecord.Fixture
        if ($fixtureRecord.Unsupported) {
            [void]$manifest.Add([pscustomobject]@{
                Fixture = [IO.Path]::GetFileNameWithoutExtension($fixtureFile.Name)
                Page = 'unsupported-schema'
                ScaleMode = 'fixture-schema-unavailable'
                Outcome = 'deferred'
                HoldEvidence = 'preview.fixture.unsupported-schema'
                Output = $null
                OutputEvidence = 'no-capture-attempted'
                Bytes = 0
                Width = 0
                Height = 0
                ExpectedWidth = 640
                ExpectedHeight = 360
            })
            continue
        }
        $isGallery = $fixture.root.kind -eq 'component_gallery'
        $pages = if ($isGallery) {
            foreach ($theme in $themes) {
                foreach ($density in $densities) {
                    foreach ($scale in $scales) {
                        foreach ($section in @('states', 'status', 'samples')) {
                            if ($section -eq 'samples') {
                                foreach ($samplePage in @(0, 1)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = 0; StatusPage = 0; SamplePage = $samplePage; Section = $section }
                                }
                            } elseif ($section -eq 'status') {
                                foreach ($statusPage in @(0, 1)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = 0; StatusPage = $statusPage; SamplePage = 0; Section = $section }
                                }
                            } else {
                                foreach ($statePage in @(0, 1, 2)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = $statePage; StatusPage = 0; SamplePage = 0; Section = $section }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            @([pscustomobject]@{ Theme = $null; Density = $null; Scale = $null; StatePage = $null; StatusPage = $null; SamplePage = $null; Section = $null })
        }

        foreach ($page in $pages) {
            $baseName = [IO.Path]::GetFileNameWithoutExtension($fixtureFile.Name)
            $suffix = if ($null -eq $page.Theme) {
                'default'
            } elseif ($page.Section -eq 'samples') {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-samples-$($page.SamplePage)"
            } elseif ($page.Section -eq 'status') {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-status-$($page.StatusPage)"
            } else {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-states-$($page.StatePage)"
            }
            $attempt = 0
            $output = $null
            do {
                $attempt++
                $output = Join-Path $OutputRoot "$baseName-$suffix-attempt-$attempt.png"
                $arguments = @('--ui-preview', $fixtureFile.FullName, '--output', $output)
                if ($null -ne $page.Theme) {
                    $arguments += @('--theme', $page.Theme, '--density', $page.Density, '--scale', [string]$page.Scale, '--section', $page.Section)
                    if ($page.Section -eq 'states') {
                        $arguments += @('--state-page', [string]$page.StatePage)
                    } elseif ($page.Section -eq 'status') {
                        $arguments += @('--status-page', [string]$page.StatusPage)
                    } elseif ($page.Section -eq 'samples') {
                        $arguments += @('--sample-page', [string]$page.SamplePage)
                    }
                }
                Invoke-TrustedPreview -Path $binary -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Arguments $arguments -Deadline $previewDeadline
                $exitCode = $script:PreviewLastExitCode
                if ($exitCode -eq 0) { break }
                if ($attempt -lt 3) {
                    Wait-PreviewBackoff -Deadline $previewDeadline -Milliseconds 150
                }
            } while ($attempt -lt 3)
            if ($exitCode -ne 0) {
                $captureFailures++
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'capture-failed'
                    HoldEvidence = 'rust-retained-authority-failure-evidence'
                    Error = "isolated preview failed with exit $exitCode after $attempt unique attempts"
                    Output = [IO.Path]::GetFileName($output)
                    OutputEvidence = 'output-left-for-forensics-no-script-delete'
                    Bytes = 0
                    Width = 0
                    Height = 0
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
                continue
            }
            try {
                $validationDeadline = $previewDeadline
                $outputAuthority = Open-PreviewOutputAuthority -Path $output -ParentAuthority $outputRootAuthority -Deadline $validationDeadline
                [void]$retainedOutputAuthorities.Add($outputAuthority)
                $outputHash = Assert-PreviewOutputAuthorityStable -Authority $outputAuthority -Deadline $validationDeadline
                $dimensions = Get-PngDimensions -Authority $outputAuthority -Deadline $validationDeadline
                if ($dimensions.Width -ne 640 -or $dimensions.Height -ne 360) {
                    throw 'preview.png.dimension-mismatch'
                }
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'captured'
                    HoldEvidence = 'frame-published'
                    Output = [IO.Path]::GetFileName($output)
                    OutputEvidence = 'published-output-name-unique-per-attempt'
                    Bytes = $outputAuthority.Length
                    OutputSha256 = $outputHash
                    Width = $dimensions.Width
                    Height = $dimensions.Height
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
            } catch {
                $captureFailures++
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'capture-failed'
                    HoldEvidence = 'png-validation-failed-output-left-for-forensics'
                    Error = ConvertTo-PreviewSafeDiagnostic -ErrorRecord $_
                    Output = [IO.Path]::GetFileName($output)
                    OutputEvidence = 'png-validation-failed-output-left-for-forensics'
                    Bytes = 0
                    Width = 0
                    Height = 0
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
            }
        }

        if ($AutomateWindowStates -and $isGallery) {
            foreach ($state in @('minimized', 'closed')) {
                $probeOutput = Join-Path $OutputRoot "component-gallery-$state.png"
                $probeArguments = @('--ui-preview', $fixtureFile.FullName, '--output', $probeOutput, '--theme', 'dark', '--density', 'compact', '--scale', '100', '--hold-ms', '800')
                $probeResult = Invoke-WindowStateProbe -State $state -Arguments $probeArguments -OutputPath $probeOutput -Deadline $previewDeadline
                [void]$manifest.Add($probeResult)
                if ($probeResult.Outcome -eq 'probe-failed') {
                    $captureFailures++
                }
            }
        }
    }

    if ($AutomateWindowStates) {
        # These values are captured as deterministic fixture token scales above.
        # Mutating the host monitor's physical DPI would affect the desktop and
        # installed applications, so exact OS-DPI evidence belongs in a
        # disposable VM/desktop harness rather than this isolated process.
        $manifest.Add([pscustomobject]@{
            Fixture = 'window-state-matrix'
            Page = 'deferred-os-dpi-100-125-150-200'
            ScaleMode = 'os-monitor-dpi-deferred'
            Outcome = 'deferred'
            HoldEvidence = 'disposable-vm-required-for-physical-monitor-dpi'
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        })
        # A separate VM/desktop harness is still required to put an unrelated
        # top-level window over the preview at the exact first-frame boundary.
        # Keep that one matrix cell visibly deferred instead of claiming a
        # deterministic occlusion capture from a desktop that may be busy.
        $manifest.Add([pscustomobject]@{
            Fixture = 'window-state-matrix'
            Page = 'deferred-occluded-external-desktop-race'
            ScaleMode = 'external-desktop-occlusion-deferred'
            Outcome = 'deferred'
            HoldEvidence = 'disposable-vm-required-for-external-occlusion-race'
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        })
    }

    # Every regular capture and window probe must be joined before manifest publication.
    Assert-NoLivePreviewProcesses -Deadline $previewDeadline
    $manifestPath = Join-Path $OutputRoot 'manifest.json'
    $manifestAuthority = Write-PreviewAtomicJson -Path $manifestPath -Value $manifest -ParentAuthority $outputRootAuthority -Deadline $previewDeadline -MaxBytes $MAX_PREVIEW_MANIFEST_BYTES
    if ($captureFailures -gt 0) {
        throw 'preview.capture.failures-present'
    }
    Write-Output ("Captured {0} isolated preview page(s)." -f $manifest.Count)
    Write-Output 'Manifest and PNGs are under the process/run-unique native-next evidence root.'
} finally {
    Invoke-PreviewFinalCleanup `
        -Deadline $previewDeadline `
        -ManifestAuthority $manifestAuthority `
        -OutputRootAuthority $outputRootAuthority `
        -RetainedOutputAuthorities $retainedOutputAuthorities `
        -ArtifactReceiptState $artifactReceiptState `
        -ArtifactAuthority $artifactAuthority `
        -BuildIdentity $buildIdentity `
        -BuildDirectories @($buildTargetRunAuthority, $buildTargetRootAuthority, $buildWorktreeAuthority) `
        -OldTargetDir $oldTargetDir `
        -OldBuildJobs $oldBuildJobs `
        -OldBuildIdentity $oldBuildIdentity
}
