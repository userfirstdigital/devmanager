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
$repoRoot = (Resolve-Path (Join-Path $scriptRoot '..\..')).Path
$fixtureRoot = Join-Path $repoRoot 'tests\fixtures\ui'
$approvedEvidenceRoot = Join-Path $repoRoot '.devmanager-next\evidence\phase-05\screenshots'
$approvedEvidencePrefix = ([IO.Path]::GetFullPath($approvedEvidenceRoot)).TrimEnd('\') + '\'
$artifactReceiptPath = Join-Path $repoRoot '.devmanager-next\preview-artifact.json'
$canonicalWorktree = ([IO.Path]::GetFullPath($repoRoot)).TrimEnd('\')
$artifactSchema = 'devmanager.native-next.preview-artifact/v1'
$artifactName = 'devmanager-next'
$artifactBinaryName = 'devmanager-next.exe'
$runToken = '{0}-{1}' -f $PID, ([Guid]::NewGuid().ToString('N'))

if ($env:OS -eq 'Windows_NT' -and $null -eq ('DevManagerPreviewArtifactNative' -as [type])) {
    Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
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
}
'@
}

function Get-PreviewSourceRevision {
    $revisionLines = @(& git -C $canonicalWorktree rev-parse --verify HEAD 2>$null)
    $gitExitCode = $LASTEXITCODE
    $revision = $revisionLines | Select-Object -First 1
    if ($gitExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($revision)) {
        throw 'preview artifact identity could not resolve the canonical worktree revision.'
    }
    $revision.ToString().Trim()
}

function Get-PreviewHostTarget {
    $versionLines = @(& rustc -vV 2>$null)
    if ($LASTEXITCODE -ne 0) {
        throw 'preview artifact identity could not resolve the Rust host target.'
    }
    $hostLine = $versionLines | Where-Object { $_ -match '^host:\s*(.+)$' } | Select-Object -First 1
    if ($null -eq $hostLine -or $hostLine -notmatch '^host:\s*(.+)$') {
        throw 'preview artifact identity could not parse the Rust host target.'
    }
    $Matches[1].Trim()
}

function Assert-PreviewPathHasNoReparseAncestors {
    param([string]$Path)

    $cursor = [IO.Path]::GetFullPath($Path)
    while ($true) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction Stop
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "preview artifact identity refuses a reparse ancestor: $cursor"
        }
        $parent = $item.Parent
        if ($null -eq $parent -or $parent.FullName.Equals($cursor, [StringComparison]::OrdinalIgnoreCase)) {
            break
        }
        $cursor = $parent.FullName
    }
}

function Open-PreviewArtifactNoFollow {
    param([string]$Path)

    $canonicalPath = [IO.Path]::GetFullPath($Path)
    Assert-PreviewPathHasNoReparseAncestors -Path $canonicalPath
    $handle = $null
    $stream = $null
    try {
        $handle = [DevManagerPreviewArtifactNative]::OpenReadNoFollow($canonicalPath)
        if ([DevManagerPreviewArtifactNative]::IsReparsePoint($handle)) {
            throw 'preview artifact identity refuses a reparse-point executable.'
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

function Get-PreviewArtifactSnapshot {
    param([string]$Path)

    $opened = Open-PreviewArtifactNoFollow -Path $Path
    try {
        $identityBefore = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
        $hash = Get-PreviewArtifactSha256 -Stream $opened.Stream
        $identityAfter = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
        if ($identityBefore -ne $identityAfter) {
            throw 'preview artifact identity changed while its retained handle was read.'
        }
        [pscustomobject]@{
            Path = $opened.Path
            FileIdentity = $identityAfter
            Length = [int64]$opened.Length
            Sha256 = $hash
        }
    } finally {
        $opened.Stream.Dispose()
    }
}

function New-PreviewArtifactReceipt {
    param(
        [string]$Path,
        [string]$SourceRevision,
        [string]$HostTarget
    )

    $snapshot = Get-PreviewArtifactSnapshot -Path $Path
    $receipt = [pscustomobject][ordered]@{
        schema = $artifactSchema
        artifact = $artifactName
        canonicalWorktree = $canonicalWorktree
        sourceRevision = $SourceRevision
        buildContract = [pscustomobject][ordered]@{
            package = 'devmanager'
            binary = $artifactName
            profile = 'debug'
            target = $HostTarget
            locked = $true
            offline = $true
            targetDir = 'isolated-per-capture-run'
        }
        binaryPath = $snapshot.Path
        binaryName = $artifactBinaryName
        binaryFileIdentity = $snapshot.FileIdentity
        binaryLength = $snapshot.Length
        binarySha256 = $snapshot.Sha256
    }
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $artifactReceiptPath) | Out-Null
    $receipt | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $artifactReceiptPath -Encoding utf8NoBOM
    $receipt
}

function Read-PreviewArtifactReceipt {
    $opened = Open-PreviewArtifactNoFollow -Path $artifactReceiptPath
    try {
        $reader = [IO.StreamReader]::new($opened.Stream, [Text.Encoding]::UTF8, $true, 1024, $true)
        try {
            $receiptJson = $reader.ReadToEnd()
        } finally {
            $reader.Dispose()
        }
        if ([DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle) -ne $opened.FileIdentity) {
            throw 'preview artifact identity receipt changed while it was being read.'
        }
        $receipt = $receiptJson | ConvertFrom-Json
    } finally {
        $opened.Stream.Dispose()
    }
    if ($null -eq $receipt -or $receipt.schema -ne $artifactSchema -or $receipt.artifact -ne $artifactName) {
        throw 'preview artifact identity receipt has the wrong schema or artifact.'
    }
    $receipt
}

function Assert-PreviewArtifactIdentity {
    param(
        [string]$RequestedPath,
        [object]$Receipt,
        [string]$SourceRevision,
        [string]$HostTarget
    )

    $requested = [IO.Path]::GetFullPath($RequestedPath)
    $expectedPath = [IO.Path]::GetFullPath([string]$Receipt.binaryPath)
    if (-not $requested.Equals($expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview artifact identity rejected a caller-supplied path that is not the recorded canonical artifact.'
    }
    if (-not ([IO.Path]::GetFileName($expectedPath)).Equals($artifactBinaryName, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview artifact identity rejected an artifact with the wrong executable name.'
    }
    if (-not ([IO.Path]::GetFullPath([string]$Receipt.canonicalWorktree)).TrimEnd('\').Equals($canonicalWorktree, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview artifact identity receipt is bound to another canonical worktree.'
    }
    if (-not ([string]$Receipt.sourceRevision).Equals($SourceRevision, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'preview artifact identity receipt is stale for the current source revision.'
    }
    $contract = $Receipt.buildContract
    if ($null -eq $contract -or
        $contract.package -ne 'devmanager' -or
        $contract.binary -ne $artifactName -or
        $contract.profile -ne 'debug' -or
        $contract.target -ne $HostTarget -or
        -not [bool]$contract.locked -or
        -not [bool]$contract.offline -or
        $contract.targetDir -ne 'isolated-per-capture-run') {
        throw 'preview artifact identity receipt has the wrong build contract.'
    }

    $opened = Open-PreviewArtifactNoFollow -Path $expectedPath
    try {
        $identityBefore = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
        $hash = Get-PreviewArtifactSha256 -Stream $opened.Stream
        $identityAfter = [DevManagerPreviewArtifactNative]::Identity($opened.Stream.SafeFileHandle)
        if ($identityBefore -ne $identityAfter) {
            throw 'preview artifact identity changed during immediate pre-launch revalidation.'
        }
        if ($identityAfter -ne [string]$Receipt.binaryFileIdentity) {
            throw 'preview artifact identity file identity does not match the build receipt.'
        }
        if ([int64]$opened.Length -ne [int64]$Receipt.binaryLength -or $hash -ne [string]$Receipt.binarySha256) {
            throw 'preview artifact identity content hash or length does not match the build receipt.'
        }
        $opened
    } catch {
        $opened.Stream.Dispose()
        throw
    }
}

function Invoke-TrustedPreview {
    param(
        [string]$Path,
        [object]$Receipt,
        [string]$SourceRevision,
        [string]$HostTarget,
        [string[]]$Arguments
    )

    $opened = Assert-PreviewArtifactIdentity -RequestedPath $Path -Receipt $Receipt -SourceRevision $SourceRevision -HostTarget $HostTarget
    try {
        & $opened.Path @Arguments
        $script:PreviewLastExitCode = $LASTEXITCODE
    } finally {
        $opened.Stream.Dispose()
    }
}

function Start-TrustedPreview {
    param(
        [string]$Path,
        [object]$Receipt,
        [string]$SourceRevision,
        [string]$HostTarget,
        [string[]]$Arguments
    )

    $opened = Assert-PreviewArtifactIdentity -RequestedPath $Path -Receipt $Receipt -SourceRevision $SourceRevision -HostTarget $HostTarget
    try {
        $process = Start-Process -FilePath $opened.Path -ArgumentList $Arguments -PassThru -WindowStyle Normal
        [pscustomobject]@{ Process = $process; Stream = $opened.Stream }
        $opened = $null
    } finally {
        if ($null -ne $opened) {
            $opened.Stream.Dispose()
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
if (-not $ValidateOnly -and [string]::IsNullOrWhiteSpace($BinaryPath)) {
    New-Item -ItemType Directory -Force -Path $TargetRunDir | Out-Null
}

$oldTargetDir = $env:CARGO_TARGET_DIR
$oldBuildJobs = $env:CARGO_BUILD_JOBS
try {
    $sourceRevision = Get-PreviewSourceRevision
    $hostTarget = Get-PreviewHostTarget
    if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
        $env:CARGO_TARGET_DIR = $TargetRunDir
        $env:CARGO_BUILD_JOBS = '1'

        & cargo build --locked --offline --bin devmanager-next --target-dir $env:CARGO_TARGET_DIR
        if ($LASTEXITCODE -ne 0) {
            throw "isolated devmanager-next build failed with exit code $LASTEXITCODE"
        }
        $binary = Join-Path $env:CARGO_TARGET_DIR 'debug\devmanager-next.exe'
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw 'isolated devmanager-next binary was not produced.'
        }
        if ((Get-PreviewSourceRevision) -ne $sourceRevision) {
            throw 'preview artifact identity source revision changed during the isolated build.'
        }
        $artifactReceipt = New-PreviewArtifactReceipt -Path $binary -SourceRevision $sourceRevision -HostTarget $hostTarget
    } else {
        # Warm mode is authorized only by the fixed receipt emitted by this script's isolated build.
        $warmBinaryPath = if ([IO.Path]::IsPathRooted($BinaryPath)) {
            $BinaryPath
        } else {
            Join-Path (Get-Location).Path $BinaryPath
        }
        $binary = [IO.Path]::GetFullPath($warmBinaryPath)
        $artifactReceipt = Read-PreviewArtifactReceipt
    }

    $startupValidation = Assert-PreviewArtifactIdentity -RequestedPath $binary -Receipt $artifactReceipt -SourceRevision $sourceRevision -HostTarget $hostTarget
    try {
        Write-Verbose ("using validated warm isolated binary {0} ({1})" -f $binary, $startupValidation.FileIdentity)
    } finally {
        $startupValidation.Stream.Dispose()
    }

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
            $launch = Start-TrustedPreview -Path $binary -Receipt $artifactReceipt -SourceRevision $sourceRevision -HostTarget $hostTarget -Arguments $Arguments
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
                $launch.Stream.Dispose()
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
                Invoke-TrustedPreview -Path $binary -Receipt $artifactReceipt -SourceRevision $sourceRevision -HostTarget $hostTarget -Arguments $arguments
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
    if ($null -eq $oldTargetDir) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue } else { $env:CARGO_TARGET_DIR = $oldTargetDir }
    if ($null -eq $oldBuildJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue } else { $env:CARGO_BUILD_JOBS = $oldBuildJobs }
}
