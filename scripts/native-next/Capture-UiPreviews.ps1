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
    private const uint MOVEFILE_REPLACE_EXISTING = 1;
    private const uint MOVEFILE_WRITE_THROUGH = 8;
    private const uint FILE_GENERIC_WRITE = 0x40000000;

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool WriteFile(
        SafeFileHandle file,
        byte[] buffer,
        uint bytesToWrite,
        out uint bytesWritten,
        IntPtr overlapped);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool MoveFileExW(string existingName, string newName, uint flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool DeleteFileW(string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool FlushFileBuffers(SafeFileHandle file);

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

    public static void WriteAtomicPreviewReceipt(string directory, string fileName, string contents)
    {
        if (fileName.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0 ||
            fileName.Contains(Path.DirectorySeparatorChar) ||
            fileName.Contains(Path.AltDirectorySeparatorChar))
        {
            throw new ArgumentException("receipt file name must be a single path component", nameof(fileName));
        }
        var temporary = Path.Combine(directory, $".{fileName}.{Guid.NewGuid():N}.tmp");
        var destination = Path.Combine(directory, fileName);
        try
        {
            var handle = CreateFileW(
                temporary,
                FILE_GENERIC_WRITE,
                FILE_SHARE_NONE,
                IntPtr.Zero,
                CREATE_NEW,
                FileAttributeNormal | OpenReparsePoint,
                IntPtr.Zero);
            if (handle.IsInvalid)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "preview receipt temporary file could not be created");
            }
            using (handle)
            {
                var bytes = Encoding.UTF8.GetBytes(contents);
                uint written;
                if (!WriteFile(handle, bytes, (uint)bytes.Length, out written, IntPtr.Zero) ||
                    written != bytes.Length || !FlushFileBuffers(handle))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(),
                        "preview receipt temporary file could not be fully flushed");
                }
            }
            if (!MoveFileExW(temporary, destination,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "preview receipt could not be atomically published");
            }
        }
        finally
        {
            DeleteFileW(temporary);
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

function Get-PreviewArtifactSha256 {
    param([IO.Stream]$Stream)

    $hashAlgorithm = [Security.Cryptography.SHA256]::Create()
    try {
        $Stream.Position = 0
        $digest = $hashAlgorithm.ComputeHash($Stream)
        ([BitConverter]::ToString($digest)).Replace('-', '').ToLowerInvariant()
    } finally {
        $hashAlgorithm.Dispose()
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

function Get-PreviewFileSha256 {
    param([string]$Path)

    $opened = Open-PreviewArtifactNoFollow -Path $Path
    try {
        Get-PreviewArtifactSha256 -Stream $opened.Stream
    } finally {
        $opened.Stream.Dispose()
    }
}

function Get-PreviewSourceTreeDigest {
    $treeLines = @(& git -C $canonicalWorktree rev-parse --verify 'HEAD^{tree}' 2>$null)
    if ($LASTEXITCODE -ne 0 -or $treeLines.Count -ne 1) {
        throw 'preview artifact identity could not resolve the canonical source tree.'
    }
    $treeLines[0].ToString().Trim()
}

function Get-PreviewSourceRevision {
    $revisionLines = @(& git -C $canonicalWorktree rev-parse --verify HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or $revisionLines.Count -ne 1) {
        throw 'preview artifact identity could not resolve the canonical worktree revision.'
    }
    $revisionLines[0].ToString().Trim()
}

function Get-PreviewSourceContentDigest {
    $deadline = [DateTime]::UtcNow.AddSeconds($SOURCE_DIGEST_DEADLINE_SECONDS)
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
            foreach ($entry in @(Get-ChildItem -LiteralPath $directoryPath -Force -ErrorAction Stop)) {
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
                $digest = Get-PreviewArtifactSha256 -Stream $opened.Stream
                $identityAfter = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
                if ($identityBefore -ne $identityAfter -or
                    [int64]$opened.Length -ne [int64][DevManagerPreviewArtifactNative]::Length($opened.Stream.SafeFileHandle)) {
                    throw "preview artifact identity source input changed while hashed: $path"
                }
                $totalBytes += [int64]$opened.Length
                if ($totalBytes -gt $MAX_SOURCE_DIGEST_BYTES) {
                    throw 'preview artifact identity source digest exceeded its bounded byte count.'
                }
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
        $hash = [Security.Cryptography.SHA256]::Create()
        try {
            ([BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes($canonical)))).Replace('-', '').ToLowerInvariant()
        } finally {
            $hash.Dispose()
        }
    } finally {
        foreach ($directoryAuthority in @($retainedSourceDirectoryAuthorities)) {
            if ($null -ne $directoryAuthority -and $null -ne $directoryAuthority.Handle) {
                $directoryAuthority.Handle.Dispose()
            }
        }
    }
}

function Get-PreviewToolPath {
    param([string]$Name)

    # rustup which from the canonical user install is the only accepted source for the build tool path.
    $canonicalRustupPath = [IO.Path]::GetFullPath($rustupPath)
    if (-not ([IO.Path]::GetExtension($canonicalRustupPath)).Equals('.exe', [StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $canonicalRustupPath -PathType Leaf)) {
        throw "preview artifact identity requires the canonical rustup.exe tool to resolve $Name."
    }
    $toolLines = @(& $canonicalRustupPath which $Name 2>$null)
    if ($LASTEXITCODE -ne 0 -or $toolLines.Count -ne 1) {
        throw "preview artifact identity could not resolve the rustup-managed $Name.exe tool."
    }
    $path = [IO.Path]::GetFullPath($toolLines[0].ToString().Trim())
    if (-not ([IO.Path]::GetExtension($path)).Equals('.exe', [StringComparison]::OrdinalIgnoreCase)) {
        throw "preview artifact identity rejects a non-executable rustup-managed $Name tool."
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "preview artifact identity could not open the rustup-managed $Name.exe tool."
    }
    $path
}

function Get-PreviewHostTarget {
    $rustcPath = Get-PreviewToolPath -Name 'rustc'
    $versionLines = @(& $rustcPath -vV 2>$null)
    if ($LASTEXITCODE -ne 0) {
        throw 'preview artifact identity could not resolve the Rust host target.'
    }
    $hostLine = $versionLines | Where-Object { $_ -match '^host:\s*(.+)$' } | Select-Object -First 1
    if ($null -eq $hostLine -or $hostLine -notmatch '^host:\s*(.+)$') {
        throw 'preview artifact identity could not parse the Rust host target.'
    }
    $Matches[1].Trim()
}

function Get-PreviewBuildIdentity {
    # The clean-tree contract is the canonical git status --porcelain check.
    $statusLines = @(& git -C $canonicalWorktree status --porcelain --untracked-files=all 2>$null)
    if ($LASTEXITCODE -ne 0) {
        throw 'preview artifact identity could not inspect the canonical source tree.'
    }
    $status = ($statusLines -join "`n").Trim()
    if (-not [string]::IsNullOrWhiteSpace($status)) {
        throw "preview artifact identity requires a clean source tree: $status"
    }
    $overrideNames = @(
        'CARGO_BUILD_TARGET',
        'CARGO_BUILD_RUSTFLAGS',
        'RUSTFLAGS',
        'CARGO_ENCODED_RUSTFLAGS',
        'CARGO_HOME',
        'RUSTUP_TOOLCHAIN'
    )
    $overrideNames += @(Get-ChildItem Env: | Where-Object {
            $_.Name -like 'CARGO_PROFILE_*' -or
            $_.Name -match '^CARGO_TARGET_.+_LINKER$'
        } | Select-Object -ExpandProperty Name)
    foreach ($name in $overrideNames | Sort-Object -Unique) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not [string]::IsNullOrWhiteSpace($value)) {
            throw "preview artifact identity rejects caller build override $name."
        }
    }

    $features = @()
    $rustcPath = Get-PreviewToolPath -Name 'rustc'
    $cargoPath = Get-PreviewToolPath -Name 'cargo'
    $rustcSha256 = Get-PreviewFileSha256 -Path $rustcPath
    $cargoSha256 = Get-PreviewFileSha256 -Path $cargoPath
    $rustcVersion = ((& $rustcPath -vV 2>$null) -join "`n").Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($rustcVersion)) {
        throw 'preview artifact identity could not resolve the Rust toolchain.'
    }
    $cargoVersion = ((& $cargoPath -V 2>$null) -join "`n").Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($cargoVersion)) {
        throw 'preview artifact identity could not resolve Cargo.'
    }
    $contract = [ordered]@{
        sourceRevision = Get-PreviewSourceRevision
        sourceTree = Get-PreviewSourceTreeDigest
        sourceContentDigest = Get-PreviewSourceContentDigest
        manifestSha256 = Get-PreviewFileSha256 -Path $manifestPath
        lockSha256 = Get-PreviewFileSha256 -Path $lockPath
        cargoConfigSha256 = if (Test-Path -LiteralPath $cargoConfigPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $cargoConfigPath
        } else {
            ''
        }
        rustToolchainSha256 = if (Test-Path -LiteralPath $rustToolchainTomlPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $rustToolchainTomlPath
        } elseif (Test-Path -LiteralPath $rustToolchainPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $rustToolchainPath
        } else {
            ''
        }
        globalCargoConfigSha256 = if (Test-Path -LiteralPath $globalCargoConfigTomlPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $globalCargoConfigTomlPath
        } elseif (Test-Path -LiteralPath $globalCargoConfigPath -PathType Leaf) {
            Get-PreviewFileSha256 -Path $globalCargoConfigPath
        } else {
            ''
        }
        rustcPath = $rustcPath
        rustcSha256 = $rustcSha256
        cargoPath = $cargoPath
        cargoSha256 = $cargoSha256
        rustcVersion = $rustcVersion
        cargoVersion = $cargoVersion
        target = Get-PreviewHostTarget
        profile = $buildProfile
        features = @($features)
        locked = $true
        offline = $true
        manifestPath = $manifestPath
        canonicalWorktree = $canonicalWorktree
    }
    $canonical = $contract | ConvertTo-Json -Depth 12 -Compress
    $hash = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = ([BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes($canonical)))).Replace('-', '').ToLowerInvariant()
    } finally {
        $hash.Dispose()
    }
    [pscustomobject]@{
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
        RustcVersion = $contract.rustcVersion
        CargoVersion = $contract.cargoVersion
        Target = $contract.target
        Profile = $contract.profile
        Features = @($features)
        ManifestPath = $manifestPath
        CanonicalWorktree = $canonicalWorktree
    }
}

function Get-PreviewEmbeddedBuildIdentity {
    param([IO.Stream]$Stream)

    $marker = 'DEV_MANAGER_PREVIEW_BUILD_IDENTITY='
    $buffer = New-Object byte[] 65536
    $carry = ''
    $found = $null
    $Stream.Position = 0
    while (($read = $Stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
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
    $requiredContract = @('package', 'binary', 'profile', 'target', 'locked', 'offline', 'manifestPath', 'features', 'targetDir', 'rustToolchainSha256', 'globalCargoConfigSha256', 'rustcPath', 'rustcSha256', 'cargoPath', 'cargoSha256', 'rustcVersion', 'cargoVersion')
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
    param([object]$ParentAuthority)

    $opened = Open-PreviewArtifactNoFollow -Path $artifactReceiptPath
    try {
        $reader = [IO.StreamReader]::new($opened.Stream, [Text.Encoding]::UTF8, $true, 1024, $true)
        try {
            $receiptJson = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
        $receipt = $receiptJson | ConvertFrom-Json
        Assert-PreviewReceiptSchema -Receipt $receipt
        $hash = Get-PreviewArtifactSha256 -Stream $opened.Stream
        if ([DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle) -ne $opened.FileIdentity) {
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
        [object]$BuildIdentity
    )

    $binary = $Authority.Binary
    $hash = Get-PreviewArtifactSha256 -Stream $binary.Stream
    $embedded = Get-PreviewEmbeddedBuildIdentity -Stream $binary.Stream
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
    [DevManagerPreviewArtifactNative]::WriteAtomicPreviewReceipt(
        $Authority.ArtifactParent.Path,
        [IO.Path]::GetFileName($artifactReceiptPath),
        $json)
    $receipt
}

function Assert-PreviewLaunchAuthorityStable {
    param(
        [object]$Authority,
        [object]$ReceiptState,
        [object]$BuildIdentity
    )

    foreach ($directory in @($Authority.Directories)) {
        if ([DevManagerPreviewArtifactNative]::Identity($directory.Handle) -ne $directory.FileIdentity) {
            throw "preview artifact authority directory changed: $($directory.Path)"
        }
    }
    if ([DevManagerPreviewArtifactNative]::Identity($Authority.Binary.Stream.SafeFileHandle) -ne $Authority.Binary.FileIdentity) {
        throw 'preview artifact authority executable identity changed.'
    }
    $binaryHash = Get-PreviewArtifactSha256 -Stream $Authority.Binary.Stream
    if ($binaryHash -ne [string]$ReceiptState.Receipt.binarySha256 -or
        [int64]$Authority.Binary.Length -ne [int64]$ReceiptState.Receipt.binaryLength) {
        throw 'preview artifact authority executable content changed.'
    }
    if ((Get-PreviewEmbeddedBuildIdentity -Stream $Authority.Binary.Stream) -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview artifact authority executable build identity changed.'
    }
    if ([DevManagerPreviewArtifactNative]::Identity($ReceiptState.Stream.SafeFileHandle) -ne $ReceiptState.FileIdentity) {
        throw 'preview receipt handle remains held but its identity changed.'
    }
    $currentIdentity = Get-PreviewBuildIdentity
    if ($currentIdentity.BuildIdentityDigest -ne $BuildIdentity.BuildIdentityDigest) {
        throw 'preview artifact identity source tree changed during the isolated build.'
    }
}

function Invoke-TrustedPreview {
    param(
        [object]$ReceiptState,
        [object]$BuildIdentity,
        [string]$Path,
        [string[]]$Arguments
    )

    $authority = Open-PreviewLaunchAuthority -Path $Path -ExistingAuthority $script:PreviewArtifactAuthority
    try {
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity
        & $authority.Path @Arguments
        $script:PreviewLastExitCode = $LASTEXITCODE
    } finally {
        Close-PreviewLaunchAuthority -Authority $authority
    }
}

function Start-TrustedPreview {
    param(
        [object]$ReceiptState,
        [object]$BuildIdentity,
        [string]$Path,
        [string[]]$Arguments
    )

    $authority = Open-PreviewLaunchAuthority -Path $Path -ExistingAuthority $script:PreviewArtifactAuthority
    try {
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity
        $process = Start-Process -FilePath $authority.Path -ArgumentList $Arguments -PassThru -WindowStyle Normal
        Assert-PreviewLaunchAuthorityStable -Authority $authority -ReceiptState $ReceiptState -BuildIdentity $BuildIdentity
        [pscustomobject]@{ Process = $process; Authority = $authority }
        $authority = $null
    } finally {
        if ($null -ne $authority) {
            Close-PreviewLaunchAuthority -Authority $authority
        }
    }
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
try {
    $buildWorktreeAuthority = Open-PreviewDirectoryNoFollow -Path $canonicalWorktree
    $buildTargetRootAuthority = Open-PreviewDirectoryNoFollow -Path $targetRoot
    $buildTargetRunAuthority = Open-PreviewDirectoryNoFollow -Path $TargetRunDir
    $buildIdentity = Get-PreviewBuildIdentity
    $env:CARGO_TARGET_DIR = $TargetRunDir
    $env:CARGO_BUILD_JOBS = '1'
    $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $buildIdentity.BuildIdentityDigest

    $artifactPaths = @(
        & $buildIdentity.CargoPath build --locked --offline --manifest-path $manifestPath --target $buildIdentity.Target --profile $buildProfile --no-default-features --bin devmanager-next --target-dir $env:CARGO_TARGET_DIR --message-format=json-render-diagnostics |
            ForEach-Object {
                $line = $_.ToString()
                try { $message = $line | ConvertFrom-Json } catch { return }
                if ($message.reason -eq 'compiler-artifact' -and
                    $message.target.name -eq 'devmanager-next' -and
                    -not [string]::IsNullOrWhiteSpace($message.executable)) {
                    $message.executable.ToString()
                }
            })
    if ($LASTEXITCODE -ne 0) {
        throw "isolated devmanager-next build failed with exit code $LASTEXITCODE"
    }
    $uniqueArtifactPaths = @($artifactPaths | Sort-Object -Unique)
    if ($uniqueArtifactPaths.Count -ne 1) {
        throw 'isolated devmanager-next build did not produce exactly one parsed executable artifact.'
    }
    $binary = [IO.Path]::GetFullPath($uniqueArtifactPaths[0])
    if (-not ([IO.Path]::GetFileName($binary)).Equals($artifactBinaryName, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'isolated devmanager-next build produced an executable with the wrong name.'
    }
    $afterBuildIdentity = Get-PreviewBuildIdentity
    if ($afterBuildIdentity.BuildIdentityDigest -ne $buildIdentity.BuildIdentityDigest) {
        throw 'preview artifact identity source tree changed during the isolated build.'
    }
    $artifactAuthority = Open-PreviewLaunchAuthority -Path $binary -ExistingDirectories @($buildWorktreeAuthority, $buildTargetRootAuthority, $buildTargetRunAuthority)
    $script:PreviewArtifactAuthority = $artifactAuthority
    $artifactReceipt = New-PreviewArtifactReceipt -Authority $artifactAuthority -BuildIdentity $buildIdentity
    $artifactReceiptState = Read-PreviewArtifactReceipt -ParentAuthority $artifactAuthority.ArtifactParent
    Assert-PreviewReceiptMatches -Receipt $artifactReceiptState.Receipt -Authority $artifactAuthority -BuildIdentity $buildIdentity
    Assert-PreviewLaunchAuthorityStable -Authority $artifactAuthority -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity
    Write-Verbose ("using one trusted isolated binary per invocation {0} ({1})" -f $binary, $artifactAuthority.Binary.FileIdentity)

    if ($ValidateOnly) {
        Write-Output ("Validated preview artifact identity for {0}." -f $binary)
        return
    }

    $fixtureFiles = @(Get-ChildItem -LiteralPath $fixtureRoot -Filter '*.json' -File |
        Where-Object {
            try {
                $fixture = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
                $fixture.schema -eq 'devmanager.ui.preview/v1'
            } catch {
                $false
            }
        })
    if (-not $AllFixtures) {
        $fixtureFiles = @($fixtureFiles | Where-Object { $_.Name -eq 'component-gallery.json' })
    }
    if ($fixtureFiles.Count -eq 0) {
        throw 'No isolated UI preview fixtures were found beneath tests/fixtures/ui.'
    }

    $themes = if ($AllThemes) { @('dark', 'light') } else { @('dark') }
    $densities = @('compact', 'comfortable')
    $scales = if ($AllScales) { @(100, 125, 150, 200) } else { @(100) }
    $manifest = [System.Collections.Generic.List[object]]::new()
    $captureFailures = 0

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

    function Get-PngDimensions {
        param([string]$Path)
        $bytes = [IO.File]::ReadAllBytes($Path)
        if ($bytes.Length -lt 24) {
            throw 'PNG is shorter than its signature and IHDR.'
        }
        $signature = [byte[]](137, 80, 78, 71, 13, 10, 26, 10)
        for ($index = 0; $index -lt $signature.Length; $index++) {
            if ($bytes[$index] -ne $signature[$index]) {
                throw 'PNG signature is invalid.'
            }
        }
        $chunkType = [Text.Encoding]::ASCII.GetString($bytes, 12, 4)
        if ($chunkType -ne 'IHDR') {
            throw 'PNG first chunk is not IHDR.'
        }
        $width = ([uint32]$bytes[16] -shl 24) -bor
            ([uint32]$bytes[17] -shl 16) -bor
            ([uint32]$bytes[18] -shl 8) -bor [uint32]$bytes[19]
        $height = ([uint32]$bytes[20] -shl 24) -bor
            ([uint32]$bytes[21] -shl 16) -bor
            ([uint32]$bytes[22] -shl 8) -bor [uint32]$bytes[23]
        [pscustomobject]@{ Width = [uint32]$width; Height = [uint32]$height }
    }

    function Invoke-WindowStateProbe {
        param(
            [string]$State,
            [string[]]$Arguments,
            [string]$OutputPath
        )
        $probe = $null
        $launch = $null
        $window = $null
        $exitCode = $null
        $failure = $null
        $joined = $false
        $outcome = 'probe-failed'
        $holdEvidence = 'probe-lifecycle-failed'
        try {
            $launch = Start-TrustedPreview -Path $binary -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Arguments $Arguments
            $probe = $launch.Process
            $probeDeadline = [DateTime]::UtcNow.AddSeconds(2)
            while ([DateTime]::UtcNow -lt $probeDeadline -and -not $probe.HasExited) {
                Start-Sleep -Milliseconds 25
                $probe.Refresh()
                if ($probe.MainWindowHandle -ne 0) {
                    $window = [IntPtr]$probe.MainWindowHandle
                    break
                }
            }
            if ($null -eq $window) {
                throw "isolated $State window did not become discoverable"
            }
            if ($State -eq 'minimized') {
                [DevManagerPreviewWindow]::ShowWindow($window, 6) | Out-Null
            } elseif ($State -eq 'closed') {
                [DevManagerPreviewWindow]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            }
            $joined = $probe.WaitForExit(4000)
            if (-not $joined) {
                try { $probe.Kill($true) } catch { }
                $joined = $probe.WaitForExit(1000)
                if (-not $joined) {
                    $holdEvidence = 'process-join-timeout-after-kill'
                    throw "isolated $State probe could not be joined within its bounded wait"
                }
                throw "isolated $State probe exceeded its bounded wait and required a bounded kill"
            }
            $exitCode = $probe.ExitCode
            if ($exitCode -eq 0) {
                throw "isolated $State probe unexpectedly published a frame"
            }
            $outputPresent = $false
            try {
                $null = Get-Item -LiteralPath $OutputPath -ErrorAction Stop
                $outputPresent = $true
            } catch {
                if ($_.CategoryInfo.Category -ne 'ObjectNotFound') {
                    throw
                }
            }
            if ($outputPresent) {
                throw "isolated $State probe left an output after an unavailable window state"
            }
            $outcome = 'rejected'
            $holdEvidence = 'output-absent-after-state-transition'
        } catch {
            $failure = $_.Exception.Message
        } finally {
            if ($null -ne $probe) {
                try { $probe.Refresh() } catch { }
                if (-not $probe.HasExited) {
                    try { $probe.Kill($true) } catch { }
                    try { $joined = $probe.WaitForExit(1000) } catch { }
                }
                $probe.Dispose()
            }
            if ($null -ne $launch) {
                Close-PreviewLaunchAuthority -Authority $launch.Authority
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

    foreach ($fixtureFile in $fixtureFiles) {
        $fixture = Get-Content -LiteralPath $fixtureFile.FullName -Raw | ConvertFrom-Json
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
                Invoke-TrustedPreview -Path $binary -ReceiptState $artifactReceiptState -BuildIdentity $buildIdentity -Arguments $arguments
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
                $image = Get-Item -LiteralPath $output -ErrorAction Stop
                if ($image.Length -le 0) {
                    throw "isolated preview produced an empty PNG for page $suffix"
                }
                $dimensions = Get-PngDimensions -Path $output
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
                    Bytes = $image.Length
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
                $probeResult = Invoke-WindowStateProbe -State $state -Arguments $probeArguments -OutputPath $probeOutput
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

    $manifestPath = Join-Path $OutputRoot 'manifest.json'
    $manifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
    if ($captureFailures -gt 0) {
        throw "isolated preview capture completed with $captureFailures failure(s); see manifest HOLD evidence"
    }
    Write-Output ("Captured {0} isolated preview page(s)." -f $manifest.Count)
    Write-Output 'Manifest and PNGs are under the process/run-unique native-next evidence root.'
} finally {
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
    if ($null -eq $oldTargetDir) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue } else { $env:CARGO_TARGET_DIR = $oldTargetDir }
    if ($null -eq $oldBuildJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue } else { $env:CARGO_BUILD_JOBS = $oldBuildJobs }
    if ($null -eq $oldBuildIdentity) { Remove-Item Env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY -ErrorAction SilentlyContinue } else { $env:DEV_MANAGER_PREVIEW_BUILD_IDENTITY = $oldBuildIdentity }
}
