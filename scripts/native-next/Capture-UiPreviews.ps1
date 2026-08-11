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
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
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

    public static void WriteAtomicPreviewReceiptRelative(
        SafeFileHandle parentDirectory,
        string directory,
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
        var temporary = Path.Combine(directory, temporaryName);
        var handle = CreateFileW(
                temporary,
                FILE_GENERIC_WRITE | DELETE_ACCESS,
                FILE_SHARE_NONE,
                IntPtr.Zero,
                CREATE_NEW,
                FileAttributeNormal | OpenReparsePoint,
                IntPtr.Zero);
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

function Get-PreviewToolEnvironment {
    [ordered]@{
        CARGO_HOME = [IO.Path]::GetFullPath($cargoHomePath)
        RUSTUP_HOME = [IO.Path]::GetFullPath($rustupHomePath)
        CARGO_NET_OFFLINE = 'true'
        CARGO_TERM_COLOR = 'never'
    }
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

    $remaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.WorkingDirectory = $WorkingDirectory
    foreach ($argument in @($Arguments)) {
        [void]$startInfo.ArgumentList.Add([string]$argument)
    }
    foreach ($name in @($Environment.Keys)) {
        $startInfo.Environment[$name] = [string]$Environment[$name]
    }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "preview external command could not start: $FilePath"
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($remaining)) {
            try { $process.Kill($true) } catch { }
            try { [void]$process.WaitForExit(1000) } catch { }
            throw "preview external command exceeded its absolute deadline: $FilePath"
        }
        $stdoutRemaining = Get-PreviewRemainingMilliseconds -Deadline $Deadline
        if (-not $stdoutTask.Wait($stdoutRemaining) -or
            -not $stderrTask.Wait((Get-PreviewRemainingMilliseconds -Deadline $Deadline))) {
            throw "preview external command output read exceeded its absolute deadline: $FilePath"
        }
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $stdoutBytes = [Text.Encoding]::UTF8.GetByteCount($stdout)
        $stderrBytes = [Text.Encoding]::UTF8.GetByteCount($stderr)
        if ($stdoutBytes -gt $MaxOutputBytes -or $stderrBytes -gt $MaxOutputBytes) {
            throw "preview external command output exceeded its bounded byte count: $FilePath"
        }
        if ($process.ExitCode -ne 0) {
            throw "preview external command failed with exit $($process.ExitCode): $FilePath $stderr"
        }
        [pscustomobject]@{
            ExitCode = $process.ExitCode
            Output = $stdout
            Error = $stderr
            AbsoluteDeadline = $Deadline
        }
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill($true) } catch { }
            try { [void]$process.WaitForExit(1000) } catch { }
        }
        $process.Dispose()
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
        throw "preview artifact authority input exceeds its bounded byte limit of $MaxBytes."
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
                throw "preview artifact authority input exceeds its bounded byte limit of $MaxBytes."
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
        throw "preview UTF-8 input exceeds its bounded byte limit of $MaxBytes."
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
                throw "preview UTF-8 input exceeds its bounded byte limit of $MaxBytes."
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

function Open-PreviewDirectoryNoFollow {
    param([string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path)
    $rootPath = [IO.Path]::GetPathRoot($fullPath)
    $canonicalPath = if ($fullPath.Length -gt $rootPath.Length) {
        $fullPath.TrimEnd('\')
    } else {
        $rootPath
    }
    $handle = [DevManagerPreviewArtifactNative]::OpenDirectoryNoFollow($canonicalPath)
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
            [void]$authorities.Add((Open-PreviewDirectoryNoFollow -Path $paths[$index]))
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
        throw "preview directory authority changed: $($Authority.Path)"
    }
    if ($null -ne $Authority.OutputRootAncestorChain) {
        foreach ($ancestor in @($Authority.OutputRootAncestorChain)) {
            if ($null -eq $ancestor -or $null -eq $ancestor.Handle -or
                [DevManagerPreviewArtifactNative]::Identity($ancestor.Handle) -ne $ancestor.FileIdentity) {
                throw "preview output ancestor authority changed: $($Authority.Path)"
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
    $opened = Open-PreviewArtifactNoFollow -Path $canonicalPath
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
        throw "preview output identity changed while retained: $($Authority.Path)"
    }
    if ([int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw "preview output length changed while retained: $($Authority.Path)"
    }
    $hash = Get-PreviewArtifactSha256 -Stream $Authority.Stream -Deadline $Deadline -MaxBytes $MAX_PREVIEW_PNG_BYTES
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Stream.SafeFileHandle) -ne $Authority.FileIdentity -or
        [int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw "preview output changed while hashed: $($Authority.Path)"
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
        $ParentAuthority.Path,
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
                    throw "preview artifact identity refuses a reparse source input: $($entry.FullName)"
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
                    throw "preview artifact identity source input changed while hashed: $path"
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
        throw "preview artifact identity requires the canonical rustup.exe tool to resolve $Name."
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
        throw "preview artifact identity could not resolve the rustup-managed $Name.exe tool."
    }
    $path = [IO.Path]::GetFullPath($toolLines[0].ToString().Trim())
    if (-not ([IO.Path]::GetExtension($path)).Equals('.exe', [StringComparison]::OrdinalIgnoreCase)) {
        throw "preview artifact identity rejects a non-executable rustup-managed $Name tool."
    }
    $rustupHomePrefix = [IO.Path]::GetFullPath($rustupHomePath).TrimEnd('\') + '\'
    if (-not $path.StartsWith($rustupHomePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "preview artifact identity rejects a rustup-managed $Name tool outside the pinned RUSTUP_HOME."
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "preview artifact identity could not open the rustup-managed $Name.exe tool."
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
            throw "preview tool authority directory changed: $($Authority.Path)"
        }
    }
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Stream.SafeFileHandle) -ne $Authority.FileIdentity -or
        [int64][DevManagerPreviewArtifactNative]::Length($Authority.Stream.SafeFileHandle) -ne [int64]$Authority.Length) {
        throw "preview tool authority changed: $($Authority.Path)"
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
        throw "preview artifact identity requires a clean source tree: $status"
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
            throw "preview artifact identity rejects caller build override $name."
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
        throw "preview artifact identity executable exceeds its bounded byte limit of $MaxBytes."
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
            throw "preview artifact identity executable exceeds its bounded byte limit of $MaxBytes."
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
                    $openedDirectory = Open-PreviewDirectoryNoFollow -Path $normalizedPath
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
    $opened = Open-PreviewArtifactNoFollow -Path $artifactReceiptPath
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
        $Authority.ArtifactParent.Path,
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
            throw "preview artifact authority directory changed: $($directory.Path)"
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
    $joined = $false
    if ($null -eq $process) {
        throw "preview process launch has no process for $Label"
    }
    try {
        if ($process.HasExited) {
            $joined = $true
        } else {
            $joined = $process.WaitForExit((Get-PreviewRemainingMilliseconds -Deadline $Deadline))
        }
        if (-not $joined) {
            try { $process.Kill($true) } catch { }
            $joined = $process.WaitForExit(1000)
            if (-not $joined) {
                $Launch.JoinState = 'join-unconfirmed-after-kill'
                throw "preview process could not be joined after bounded kill: $Label"
            }
            $Launch.JoinState = 'killed-and-joined'
            $Launch.ExitCode = -1
            return -1
        }
        $Launch.JoinState = 'joined'
        $Launch.ExitCode = $process.ExitCode
        $Launch.ExitCode
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill($true) } catch { }
            try { [void]$process.WaitForExit(1000) } catch { }
        }
    }
}

function Assert-NoLivePreviewProcesses {
    param([datetime]$Deadline)

    foreach ($launch in @($activePreviewProcesses)) {
        if ($null -ne $launch -and $null -ne $launch.Process -and -not $launch.Process.HasExited) {
            [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label 'before manifest publication')
        }
        if ($null -ne $launch -and $null -ne $launch.Process -and -not $launch.Process.HasExited) {
            throw 'preview process remained live before manifest publication.'
        }
    }
}

function Close-PreviewTrackedProcesses {
    foreach ($launch in @($activePreviewProcesses)) {
        if ($null -eq $launch) { continue }
        if ($null -ne $launch.Process) {
            try {
                if (-not $launch.Process.HasExited) {
                    try { $launch.Process.Kill($true) } catch { }
                    try { [void]$launch.Process.WaitForExit(1000) } catch { }
                }
            } catch { }
            try { $launch.Process.Dispose() } catch { }
        }
        if ($null -ne $launch.Authority) {
            Close-PreviewLaunchAuthority -Authority $launch.Authority
        }
    }
    $activePreviewProcesses.Clear()
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
        try { $Process.Refresh() } catch { }
        if ($Process.MainWindowHandle -ne 0) {
            return [pscustomobject]@{
                Window = [IntPtr]$Process.MainWindowHandle
                Attempts = $ReadinessAttempt
                ReadinessHandshake = 'ready'
            }
        }
        Write-Verbose ("preview readiness-retry attempt {0}" -f $ReadinessAttempt)
        Start-Sleep -Milliseconds 25
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
    $process = $null
    try {
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity -Deadline $Deadline
        Assert-PreviewDeadline -Deadline $Deadline
        $process = Start-Process -FilePath $authority.Path -ArgumentList $Arguments -PassThru -WindowStyle Normal
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity -Deadline $Deadline
        [pscustomobject]@{ Process = $process; Authority = $authority; JoinState = 'started'; ReadinessHandshake = 'pending' }
        $authority = $null
    } finally {
        if ($null -ne $process -and -not $process.HasExited -and $null -ne $authority) {
            try { $process.Kill($true) } catch { }
            try { [void]$process.WaitForExit(1000) } catch { }
        }
        if ($null -ne $authority) {
            Close-PreviewLaunchAuthority -Authority $authority
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

    $buildEnvironment = Get-PreviewToolEnvironment
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
    $allFixtureFiles = [System.Collections.Generic.List[object]]::new()
    foreach ($candidate in (Get-ChildItem -LiteralPath $fixtureRoot -Filter '*.json' -File)) {
        if ($allFixtureFiles.Count -ge $MAX_SOURCE_DIGEST_FILES) {
            throw 'preview fixture enumeration exceeded its bounded file count.'
        }
        [void]$allFixtureFiles.Add($candidate)
    }
    $allFixtureFiles = @($allFixtureFiles | Sort-Object -Property Name)
    $fixtureRecords = [System.Collections.Generic.List[object]]::new()
    foreach ($fixtureFile in $allFixtureFiles) {
        try {
            $fixture = Read-PreviewFixture -Path $fixtureFile.FullName -Deadline $previewDeadline
        } catch {
            throw "fixture enumeration failed for $($fixtureFile.Name): $($_.Exception.Message)"
        }
        if ($fixture.schema -ne 'devmanager.ui.preview/v1') {
            Write-Warning ("fixture enumeration skipped {0}: unsupported schema {1}" -f $fixtureFile.Name, $fixture.schema)
            continue
        }
        [void]$fixtureRecords.Add([pscustomobject]@{
            FixtureRecord = $fixtureFile.Name
            File = $fixtureFile
            Fixture = $fixture
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
        try {
            $launch = Start-TrustedPreview -Path $binary -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Arguments $Arguments -Deadline $Deadline
            [void]$activePreviewProcesses.Add($launch)
            $probe = $launch.Process
            $readiness = Wait-PreviewWindowReady -Process $probe -Deadline $Deadline
            $window = $readiness.Window
            $launch.ReadinessHandshake = $readiness.ReadinessHandshake
            if ($State -eq 'minimized') {
                [DevManagerPreviewWindow]::ShowWindow($window, 6) | Out-Null
            } elseif ($State -eq 'closed') {
                [DevManagerPreviewWindow]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            }
            # Preserve the bounded probe join contract (WaitForExit(4000), then Kill($true)
            # and WaitForExit(1000)) inside Join-PreviewProcessBounded.
            $exitCode = Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label "isolated $State probe"
            $joined = $launch.JoinState -in @('joined', 'killed-and-joined')
            if ($exitCode -eq 0) {
                throw "isolated $State probe unexpectedly published a frame"
            }
            $outputPresent = $false
            $probeOutputAuthority = Try-Open-PreviewOutputAuthority -Path $OutputPath -ParentAuthority $outputRootAuthority -Deadline $Deadline
            if ($null -ne $probeOutputAuthority) {
                $outputPresent = $true
                [void]$retainedOutputAuthorities.Add($probeOutputAuthority)
            }
            if ($outputPresent) {
                throw "isolated $State probe left an output after an unavailable window state"
            }
            $outcome = 'rejected'
            $holdEvidence = 'output-absent-after-state-transition'
        } catch {
            $failure = $_.Exception.Message
        } finally {
            if ($null -ne $launch -and $null -ne $launch.Process -and -not $launch.Process.HasExited) {
                try {
                    [void](Join-PreviewProcessBounded -Launch $launch -Deadline $Deadline -Label "isolated $State probe cleanup")
                } catch {
                    $holdEvidence = 'process-join-timeout-after-kill'
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
            OutputEvidence = if ($outcome -eq 'rejected') {
                'output-absent-after-state-transition'
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
                    Start-Sleep -Milliseconds 150
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
                    throw "decoded PNG dimensions were $($dimensions.Width)x$($dimensions.Height), expected 640x360"
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
                    Error = $_.Exception.Message
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
        throw "isolated preview capture completed with $captureFailures failure(s); see manifest HOLD evidence"
    }
    Write-Output ("Captured {0} isolated preview page(s)." -f $manifest.Count)
    Write-Output 'Manifest and PNGs are under the process/run-unique native-next evidence root.'
} finally {
    Close-PreviewTrackedProcesses
    Close-PreviewOutputAuthority -Authority $manifestAuthority
    foreach ($outputAuthority in @($retainedOutputAuthorities)) {
        Close-PreviewOutputAuthority -Authority $outputAuthority
    }
    if ($null -ne $outputRootAuthority) {
        Assert-PreviewDirectoryAuthorityStable -Authority $outputRootAuthority
    }
    Close-PreviewDirectoryAuthorityChain -Authority $outputRootAuthority
    if ($null -ne $artifactReceiptState -and $null -ne $artifactReceiptState.Stream) {
        $artifactReceiptState.Stream.Dispose()
    }
    Close-PreviewLaunchAuthority -Authority $artifactAuthority
    $script:PreviewArtifactAuthority = $null
    foreach ($directory in @($buildTargetRunAuthority, $buildTargetRootAuthority, $buildWorktreeAuthority)) {
        if ($null -ne $directory -and $null -ne $directory.Handle) {
            $directory.Handle.Dispose()
        }
    }
    if ($null -ne $buildIdentity -and $null -ne $buildIdentity.ToolAuthorities) {
        Close-PreviewToolAuthority -Authority $buildIdentity.ToolAuthorities['rustup']
        Close-PreviewToolAuthority -Authority $buildIdentity.ToolAuthorities['rustc']
        Close-PreviewToolAuthority -Authority $buildIdentity.ToolAuthorities['cargo']
        if ($null -ne $buildIdentity.ToolAuthorities['rustupHome'] -and $null -ne $buildIdentity.ToolAuthorities['rustupHome'].Handle) {
            $buildIdentity.ToolAuthorities['rustupHome'].Handle.Dispose()
        }
    }
    if ($null -eq $oldTargetDir) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue } else { $env:CARGO_TARGET_DIR = $oldTargetDir }
    if ($null -eq $oldBuildJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue } else { $env:CARGO_BUILD_JOBS = $oldBuildJobs }
    if ($null -eq $oldBuildIdentity) { Remove-Item Env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY -ErrorAction SilentlyContinue } else { $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $oldBuildIdentity }
}
