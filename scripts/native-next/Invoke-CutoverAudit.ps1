# Phase 11.1 read-only cutover contract audit.
#
# This script is intentionally PowerShell 7+ only. Windows PowerShell is a
# fail-closed unsupported runtime because its .NET process and async handle
# surface is not sufficient for the bounded Job Object wrapper below.
#
# The audit accepts the script's own candidate worktree or an explicitly
# authenticated generated fixture. It never reads production AppData,
# session.json, or a caller-selected arbitrary repository.

[CmdletBinding()]
param(
    [ValidateSet('Parity')]
    [string]$Mode = 'Parity',

    [string]$Root,

    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($PSVersionTable.PSVersion.Major -lt 7) {
    [Console]::Error.WriteLine('AUDIT_ERROR[unsupported_runtime] PowerShell 7 or later is required.')
    exit 2
}

# This is the only audit deadline. It is created before any process start,
# handle open, read, or report write and is passed unchanged to every bounded
# operation. Cleanup is never given a fresh budget.
$maxAuditDurationMs = 15000
$auditDeadlineUtc = [DateTime]::UtcNow.AddMilliseconds($maxAuditDurationMs)
$fixtureAuthToken = 'phase-11.1a-generated-fixture-v1'

$contractErrors = New-Object 'System.Collections.Generic.List[string]'
$globalBlockers = New-Object 'System.Collections.Generic.List[string]'
$rowReports = New-Object 'System.Collections.Generic.List[object]'
$nodeReports = New-Object 'System.Collections.Generic.List[object]'
$entrypointFindings = New-Object 'System.Collections.Generic.List[string]'
$protectedTrackedFiles = New-Object 'System.Collections.Generic.List[string]'
$trackedFiles = @()
$sortedEntrypointFindings = @()
$sortedContractErrors = @()
$sortedGlobalBlockers = @()
$contract = $null
$reportPath = $null
$humanPath = $null
$evidenceRoot = $null
$rootPath = $null
$maxLedgerBytes = [int64]524288
$maxTrackedFiles = 4096
$maxTrackedBytes = [int64]4194304
$maxRows = 256
$maxNodes = 256
$maxStringsPerRow = 64
$maxNeedles = 1024
$maxNeedleChars = 512
$maxMatchesPerOwner = 20
$maxScanBytesPerFile = [int64]1048576
$maxScannerFiles = 4096
$maxScannerOutputBytes = [int64]262144
$maxScannerDurationMs = $maxAuditDurationMs
$maxErrorCount = 64
$maxReportJsonBytes = [int64]262144
$maxReportHumanBytes = [int64]131072
$safetyBoundReached = $false
$safetyDiagnosticEmitted = $false
$safetyDiagnostic = 'audit[safety_bound]'
$maxMatches = $maxMatchesPerOwner
$rootIdentity = $null
$commonDirectoryIdentity = $null
$authorizedRootKind = $null
$candidateRootPath = $null
$gitIdentity = $null
$authorizationFailure = $null
$fatalDiagnosticCategory = 'audit_internal_error'

function Add-SafetyBound {
    if ($script:safetyBoundReached -eq $true) {
        return
    }
    $script:safetyBoundReached = $true
    if ($script:safetyDiagnosticEmitted -eq $false) {
        $script:safetyDiagnosticEmitted = $true
        $globalBlockers.Add('audit[safety_bound]')
    }
}

function Get-CutoverDiagnosticCategory {
    param([AllowEmptyString()][string]$Message)

    $text = ([string]$Message).ToLowerInvariant()
    if ($text.Contains('unsupported_runtime') -or $text.Contains('powershell 7')) { return 'unsupported_runtime' }
    if ($text.Contains('unauthorized') -or $text.Contains('authenticated fixture')) { return 'root_unauthorized' }
    if ($text.Contains('common') -or $text.Contains('git identity') -or $text.Contains('worktree')) { return 'git_identity_invalid' }
    if ($text.Contains('session.json')) { return 'protected_filename' }
    if ($text.Contains('output path') -or $text.Contains('report path')) { return 'output_path_rejected' }
    if ($text.Contains('hard link') -or $text.Contains('hardlink')) { return 'path_hardlink_rejected' }
    if ($text.Contains('reparse') -or $text.Contains('junction') -or $text.Contains('symlink')) { return 'path_reparse_rejected' }
    if ($text.Contains('content') -or $text.Contains('changed') -or $text.Contains('identity')) { return 'file_identity_changed' }
    if ($text.Contains('legacy ')) { return 'contract_invalid' }
    if ($text.Contains('stdout') -and $text.Contains('overflow')) { return 'process_stdout_overflow' }
    if ($text.Contains('stderr') -and $text.Contains('overflow')) { return 'process_stderr_overflow' }
    if ($text.Contains('stdout-overflow')) { return 'process_stdout_overflow' }
    if ($text.Contains('stderr-overflow')) { return 'process_stderr_overflow' }
    if ($text.Contains('process-error') -or $text.Contains('process-resolve') -or $text.Contains('process-create')) { return 'process_error' }
    if ($text.Contains('nonzero') -or $text.Contains('exit code')) { return 'process_nonzero' }
    if ($text.Contains('timeout') -or $text.Contains('deadline')) { return 'process_deadline_exceeded' }
    if ($text.Contains('scanner') -or $text.Contains('rg')) { return 'scanner_failed' }
    if ($text.Contains('git')) { return 'git_enumeration_failed' }
    if ($text.Contains('evidence')) { return 'evidence_invalid' }
    if ($text.Contains('prerequisite') -or $text.Contains('dependency')) { return 'prerequisite_invalid' }
    if ($text.Contains('ledger')) { return 'ledger_invalid' }
    if ($text.Contains('contract') -or $text.Contains('row') -or $text.Contains('node')) { return 'contract_invalid' }
    return 'audit_internal_error'
}

function Format-CutoverDiagnostic {
    param(
        [Parameter(Mandatory = $true)][string]$Category,
        [AllowEmptyString()][string]$RelativePath
    )

    $message = "audit[$Category]"
    if (-not [string]::IsNullOrEmpty($RelativePath)) {
        try {
            $safePath = Assert-CutoverRelativePath -Value $RelativePath -Label 'diagnostic path'
            if (-not [string]::IsNullOrEmpty($safePath)) {
                $message += ";path=$safePath"
            }
        }
        catch { }
    }
    if ($Category -eq 'protected_filename') {
        $message += ';path=session.json'
    }
    return $message
}

function ConvertTo-CutoverDiagnostic {
    param(
        [AllowEmptyString()][string]$Message,
        [AllowEmptyString()][string]$RelativePath
    )

    return Format-CutoverDiagnostic -Category (Get-CutoverDiagnosticCategory -Message $Message) -RelativePath $RelativePath
}

function Get-ContractProperty {
    param(
        [object]$Object,
        [Parameter(Mandatory = $true)][string]$Name
    )

    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Get-ContractArray {
    param([object]$Value)

    if ($null -eq $Value) {
        return @()
    }
    return @($Value)
}

function Add-ContractError {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [AllowEmptyString()][string]$RelativePath,
        [string]$Category = 'contract_invalid'
    )

    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        if ($contractErrors.Count -ge $maxErrorCount) {
            Add-SafetyBound
            return
        }
        $contractErrors.Add((Format-CutoverDiagnostic -Category $Category -RelativePath $RelativePath))
    }
}

function Add-GlobalBlocker {
    param(
        [Parameter(Mandatory = $true)][string]$Message,
        [AllowEmptyString()][string]$RelativePath
    )

    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        if ($globalBlockers.Count -ge $maxErrorCount) {
            Add-SafetyBound
            return
        }
        $globalBlockers.Add((ConvertTo-CutoverDiagnostic -Message $Message -RelativePath $RelativePath))
    }
}

function Add-RowBlocker {
    param(
        [Parameter(Mandatory = $true)][ref]$Blockers,
        [Parameter(Mandatory = $true)][string]$Message,
        [AllowEmptyString()][string]$RelativePath
    )

    if ([string]::IsNullOrWhiteSpace($Message)) {
        return
    }
    if ($Blockers.Value.Count -ge $maxErrorCount) {
        Add-SafetyBound
        return
    }
    $Blockers.Value.Add((ConvertTo-CutoverDiagnostic -Message $Message -RelativePath $RelativePath))
}

function Assert-CutoverRootStable {
    if ($null -eq $rootPath -or $null -eq $rootIdentity -or $null -eq $gitIdentity -or $null -eq $commonDirectoryIdentity) {
        throw 'repository root identity was not established.'
    }
    $current = Get-CutoverPathIdentity -LiteralPath $rootPath -AllowDirectory
    if (-not (Compare-CutoverIdentity -Before $rootIdentity -After $current)) {
        throw 'repository root changed during the audit.'
    }
    $currentCommon = Get-CutoverPathIdentity -LiteralPath $gitIdentity.commonDirectory -AllowDirectory
    if (-not (Compare-CutoverIdentity -Before $commonDirectoryIdentity -After $currentCommon)) {
        throw 'Git common directory changed during the audit.'
    }
}

function Assert-CutoverRelativePath {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($Value -isnot [string] -or [string]::IsNullOrEmpty([string]$Value)) {
        Add-ContractError "${Label} is missing or empty."
        return $null
    }

    $raw = [string]$Value
    if ($raw -ne $raw.Trim() -or $raw.Contains('\') -or $raw.EndsWith('/')) {
        Add-ContractError "${Label} must use its exact repository-relative spelling."
        return $null
    }
    if ($raw.IndexOfAny([char[]](0..31 + 127)) -ge 0 -or $raw.Contains(':')) {
        Add-ContractError "${Label} contains a control character, drive-relative form, or alternate stream."
        return $null
    }
    if ($raw.StartsWith('/') -or $raw.StartsWith('//') -or $raw -match '^[A-Za-z]:') {
        Add-ContractError "${Label} must be repository-relative: '$raw'."
        return $null
    }

    $parts = @($raw.Split('/'))
    if ($parts.Count -eq 0) {
        Add-ContractError "${Label} must be a normalized repository-relative path."
        return $null
    }
    for ($partIndex = 0; $partIndex -lt $parts.Count; $partIndex++) {
        $part = [string]$parts[$partIndex]
        if ([string]::IsNullOrEmpty($part)) {
            Add-ContractError "${Label} contains an empty path component."
            return $null
        }
        if ($part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) {
            Add-ContractError "${Label} contains a dot, parent, or trailing-dot-space path component."
            return $null
        }
    }
    if ([string]::IsNullOrEmpty($raw) -or $raw -eq '.') {
        Add-ContractError "${Label} must be a normalized repository-relative path."
        return $null
    }
    return $raw
}

function Normalize-CutoverAbsolutePath {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ([string]::IsNullOrEmpty($LiteralPath) -or $LiteralPath -ne $LiteralPath.Trim()) {
        throw "${Label} must be an exact fully qualified path."
    }
    if ($LiteralPath.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
        throw "${Label} contains a control character."
    }
    if ($LiteralPath.StartsWith('\\?\') -or $LiteralPath.StartsWith('\\.\') -or $LiteralPath.StartsWith('\??\') -or $LiteralPath.StartsWith('\\')) {
        throw "${Label} uses an unsupported Win32 alias or UNC path."
    }
    if ($LiteralPath -notmatch '^[A-Za-z]:[\\/]') {
        throw "${Label} must be drive-absolute; relative and drive-relative paths are rejected."
    }

    $spelling = $LiteralPath.Replace('/', '\')
    $parts = @($spelling.Split('\', [System.StringSplitOptions]::RemoveEmptyEntries))
    foreach ($part in @($parts | Select-Object -Skip 1)) {
        if ($part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ') -or $part.Contains(':')) {
            throw "${Label} contains an alias, dot-segment, trailing-dot-space, or alternate-stream component."
        }
    }
    try {
        $full = [System.IO.Path]::GetFullPath($spelling)
    }
    catch {
        throw "${Label} cannot be normalized safely."
    }
    return $full
}

function Test-CutoverPathEqualsOrBeneath {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Ancestor
    )

    $candidate = Normalize-CutoverAbsolutePath -LiteralPath $Path -Label 'candidate path'
    $parent = Normalize-CutoverAbsolutePath -LiteralPath $Ancestor -Label 'ancestor path'
    if ($candidate.Equals($parent, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    return $candidate.StartsWith($parent.TrimEnd('\') + '\', [System.StringComparison]::OrdinalIgnoreCase)
}

function Assert-CutoverPathChain {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [switch]$AllowMissingLeaf
    )

    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'filesystem path'
    $root = [System.IO.Path]::GetPathRoot($full)
    $relative = if ($full.Length -gt $root.Length) { $full.Substring($root.Length) } else { '' }
    $parts = @($relative.Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))
    $current = $root.TrimEnd('\', '/')
    for ($index = 0; $index -lt $parts.Count; $index++) {
        Assert-CutoverDeadline
        $current = Join-Path $current $parts[$index]
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($null -eq $item) {
            if ($AllowMissingLeaf -and $index -eq ($parts.Count - 1)) {
                break
            }
            throw "filesystem path component is missing: '$current'."
        }
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "filesystem path contains a symlink, junction, or reparse component: '$current'."
        }
        if ($index -lt ($parts.Count - 1) -and $item -isnot [System.IO.DirectoryInfo]) {
            throw "filesystem path component is not a directory: '$current'."
        }
    }
    return $full
}

function Initialize-CutoverNativeMethods {
    if ($null -eq ([System.Management.Automation.PSTypeName]'CutoverNativeMethods').Type) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class CutoverNativeMethods
{
    [StructLayout(LayoutKind.Sequential)]
    public struct ByHandleFileInformation
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

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandle(
        IntPtr hFile,
        out ByHandleFileInformation lpFileInformation);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr CreateFileW(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode, ExactSpelling = true)]
    private static extern uint GetFinalPathNameByHandleW(
        IntPtr file,
        StringBuilder path,
        uint pathLength,
        uint flags);

    public static string GetFinalPath(IntPtr file)
    {
        var capacity = 512;
        while (capacity <= 32768)
        {
            var path = new StringBuilder(capacity);
            var length = GetFinalPathNameByHandleW(file, path, (uint)path.Capacity, 0);
            if (length == 0) throw new InvalidOperationException("final-path");
            if (length < path.Capacity) return path.ToString();
            capacity *= 2;
        }
        throw new InvalidOperationException("final-path-length");
    }
}
'@
    }
}

function Get-CutoverHandleIdentity {
    param([Parameter(Mandatory = $true)][System.IO.FileStream]$Stream)

    Initialize-CutoverNativeMethods
    $information = New-Object 'CutoverNativeMethods+ByHandleFileInformation'
    if (-not [CutoverNativeMethods]::GetFileInformationByHandle($Stream.SafeFileHandle.DangerousGetHandle(), [ref]$information)) {
        throw 'Unable to obtain a stable Windows filesystem identity.'
    }
    $index = '{0:X8}{1:X8}' -f $information.FileIndexHigh, $information.FileIndexLow
    $length = [int64]-1
    if (($information.FileAttributes -band 0x10) -eq 0) {
        try { $length = [int64]$Stream.Length } catch { }
    }
    $nativeFinalPath = [CutoverNativeMethods]::GetFinalPath($Stream.SafeFileHandle.DangerousGetHandle())
    $finalPath = if ($nativeFinalPath.StartsWith('\\?\', [System.StringComparison]::Ordinal)) { $nativeFinalPath.Substring(4) } else { $nativeFinalPath }
    return [pscustomobject]@{
        volume = [uint32]$information.VolumeSerialNumber
        index = $index
        links = [uint32]$information.NumberOfLinks
        length = $length
        attributes = [uint32]$information.FileAttributes
        finalPath = $finalPath
    }
}

function Open-CutoverConfinedFile {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [string]$AuthorizedRoot,
        [switch]$AllowDirectory,
        [switch]$ReadOnlyShare
    )

    if ([System.IO.Path]::GetFileName($LiteralPath).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Protected session.json variants must be excluded before opening.'
    }
    Assert-CutoverDeadline
    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'filesystem path'
    if ([string]::IsNullOrWhiteSpace($AuthorizedRoot)) { $AuthorizedRoot = $rootPath }
    if ([string]::IsNullOrWhiteSpace($AuthorizedRoot)) {
        throw 'authorized root was not established.'
    }
    $authorized = Normalize-CutoverAbsolutePath -LiteralPath $AuthorizedRoot -Label 'authorized root'
    Initialize-CutoverNativeMethods
    $shareMode = 0x00000001 -bor 0x00000002 -bor 0x00000004
    $flags = 0x00200000 -bor 0x02000000 # OPEN_REPARSE_POINT | BACKUP_SEMANTICS
    try {
        $rawHandle = [CutoverNativeMethods]::CreateFileW(
            $full,
            [uint32]2147483648,
            [uint32]$shareMode,
            [IntPtr]::Zero,
            3,
            [uint32]$flags,
            [IntPtr]::Zero)
    }
    catch {
        throw 'confined handle open failed.'
    }
    if ($rawHandle -eq [IntPtr]::Zero -or $rawHandle -eq [IntPtr](-1)) {
        throw 'confined handle open failed.'
    }
    $safeHandle = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true)
    $stream = $null
    try {
        $stream = [System.IO.FileStream]::new($safeHandle, [System.IO.FileAccess]::Read, 8192, $false)
        $identity = Get-CutoverHandleIdentity -Stream $stream
        if (-not (Test-CutoverPathEqualsOrBeneath -Path $identity.finalPath -Ancestor $authorized)) {
            throw 'opened handle escaped the authorized root.'
        }
        if (($identity.attributes -band 0x400) -ne 0) {
            throw 'opened handle is a reparse point.'
        }
        $isDirectory = ($identity.length -lt 0)
        if (-not $AllowDirectory -and $isDirectory) {
            throw 'expected a file, got a directory.'
        }
        if (-not $isDirectory -and $identity.links -gt 1) {
            throw 'Refusing a hard-linked tracked/evidence file.'
        }
        return [pscustomobject]@{ path = $full; stream = $stream; identity = $identity }
    }
    catch {
        if ($null -ne $stream) { $stream.Dispose() } else { $safeHandle.Dispose() }
        throw
    }
}

function Compare-CutoverIdentity {
    param(
        [Parameter(Mandatory = $true)][object]$Before,
        [Parameter(Mandatory = $true)][object]$After
    )

    return $Before.volume -eq $After.volume -and
        $Before.index -eq $After.index -and
        $Before.links -eq $After.links -and
        $Before.length -eq $After.length -and
        [string]::Equals([string]$Before.finalPath, [string]$After.finalPath, [System.StringComparison]::OrdinalIgnoreCase)
}

function Open-CutoverConfinedWriteFile {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$AuthorizedRoot
    )

    Assert-CutoverDeadline
    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'filesystem path'
    $authorized = Normalize-CutoverAbsolutePath -LiteralPath $AuthorizedRoot -Label 'authorized root'
    Initialize-CutoverNativeMethods
    $flags = ([uint32]35651584) -bor ([uint32]2147483648) -bor ([uint32]1073741824) # OPEN_REPARSE_POINT | BACKUP_SEMANTICS | WRITE_THROUGH | OVERLAPPED
    try {
        $rawHandle = [CutoverNativeMethods]::CreateFileW(
            $full,
            [uint32]1073741824,
            [uint32]0,
            [IntPtr]::Zero,
            [uint32]1,
            [uint32]$flags,
            [IntPtr]::Zero)
    }
    catch {
        throw 'confined write handle open failed.'
    }
    if ($rawHandle -eq [IntPtr]::Zero -or $rawHandle -eq [IntPtr](-1)) {
        throw 'confined write handle open failed.'
    }
    $safeHandle = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true)
    $stream = $null
    try {
        $stream = [System.IO.FileStream]::new($safeHandle, [System.IO.FileAccess]::Write, 8192, $true)
        $identity = Get-CutoverHandleIdentity -Stream $stream
        if (-not (Test-CutoverPathEqualsOrBeneath -Path $identity.finalPath -Ancestor $authorized)) {
            throw 'opened write handle escaped the authorized root.'
        }
        if (($identity.attributes -band 0x400) -ne 0) {
            throw 'opened write handle is a reparse point.'
        }
        if ($identity.length -lt 0) {
            throw 'opened write handle is a directory.'
        }
        if ($identity.links -gt 1) {
            throw 'opened write handle is hard-linked.'
        }
        return [pscustomobject]@{ path = $full; stream = $stream; identity = $identity }
    }
    catch {
        if ($null -ne $stream) { $stream.Dispose() } else { $safeHandle.Dispose() }
        throw
    }
}

function Get-CutoverPathIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [switch]$AllowDirectory
    )

    $opened = Open-CutoverConfinedFile -LiteralPath $LiteralPath -AuthorizedRoot $LiteralPath -AllowDirectory:$AllowDirectory
    try { return $opened.identity }
    finally { $opened.stream.Dispose() }
}

function Get-CutoverDeadlineRemainingMilliseconds {
    $remaining = [int64]([Math]::Floor(($auditDeadlineUtc - [DateTime]::UtcNow).TotalMilliseconds))
    if ($remaining -le 0) { return 0 }
    return [int][Math]::Min($remaining, [int32]::MaxValue)
}

function Assert-CutoverDeadline {
    if ((Get-CutoverDeadlineRemainingMilliseconds) -le 0) {
        throw 'audit deadline exceeded.'
    }
}

function Read-CutoverStreamBytes {
    param(
        [Parameter(Mandatory = $true)][System.IO.Stream]$Stream,
        [Parameter(Mandatory = $true)][int64]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )

    Assert-CutoverDeadline
    $bytes = New-Object 'System.Collections.Generic.List[byte]'
    $buffer = New-Object byte[] 8192
    while ($true) {
        Assert-CutoverDeadline
        $remaining = [Math]::Min($buffer.Length, [int]($MaxBytes + 1 - $bytes.Count))
        if ($remaining -le 0) {
            Add-SafetyBound
            throw "${Label} exceeds the bounded input byte limit."
        }
        $task = $Stream.ReadAsync($buffer, 0, $remaining)
        $waitMs = Get-CutoverDeadlineRemainingMilliseconds
        if ($waitMs -le 0 -or -not $task.Wait($waitMs)) {
            throw 'audit deadline exceeded while reading a file.'
        }
        $read = $task.Result
        if ($read -le 0) { break }
        for ($offset = 0; $offset -lt $read; $offset++) { $bytes.Add($buffer[$offset]) }
        if ($bytes.Count -gt $MaxBytes) {
            Add-SafetyBound
            throw "${Label} exceeds the bounded input byte limit."
        }
    }
    return ,([byte[]]$bytes.ToArray())
}

function Read-CutoverConfinedUtf8 {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][int64]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $opened = Open-CutoverConfinedFile -LiteralPath $LiteralPath -AuthorizedRoot $rootPath
    try {
        $bytes = Read-CutoverStreamBytes -Stream $opened.stream -MaxBytes $MaxBytes -Label $Label
        $after = Get-CutoverHandleIdentity -Stream $opened.stream
        if (-not (Compare-CutoverIdentity -Before $opened.identity -After $after)) {
            throw "${Label} changed during its confined read."
        }
        $opened.stream.Position = 0
        $recheck = Read-CutoverStreamBytes -Stream $opened.stream -MaxBytes $MaxBytes -Label $Label
        if (-not [string]::Equals((Get-CutoverSha256Hex -Bytes $bytes), (Get-CutoverSha256Hex -Bytes $recheck), [System.StringComparison]::Ordinal)) {
            throw "${Label} content changed during its confined read."
        }
        try {
            return ([System.Text.UTF8Encoding]::new($false, $true)).GetString($bytes)
        }
        catch {
            throw "${Label} is not valid UTF-8."
        }
    }
    finally {
        $opened.stream.Dispose()
    }
}

function Read-CutoverScanBytes {
    param(
        [Parameter(Mandatory = $true)][object]$Opened,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    $length = [int64]$Opened.identity.length
    if ($length -lt 0) {
        throw 'Unable to determine the bounded scan file length.'
    }
    if ($length -gt $MaxBytes) {
        Add-SafetyBound
        throw 'tracked scanner input exceeds the bounded input byte limit.'
    }
    if ($length -gt [int32]::MaxValue) {
        Add-SafetyBound
        throw 'tracked scanner input cannot be represented in memory.'
    }

    $Opened.stream.Position = 0
    return ,(Read-CutoverStreamBytes -Stream $Opened.stream -MaxBytes $MaxBytes -Label 'tracked scanner input')
}

function Get-CutoverSha256Hex {
    param([Parameter(Mandatory = $true)][byte[]]$Bytes)

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return (($sha.ComputeHash($Bytes) | ForEach-Object { $_.ToString('x2') }) -join '')
    }
    finally {
        $sha.Dispose()
    }
}

function Ensure-CutoverAuditDirectory {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    Assert-CutoverDeadline
    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'directory path'
    $root = [System.IO.Path]::GetPathRoot($full)
    $relative = if ($full.Length -gt $root.Length) { $full.Substring($root.Length) } else { '' }
    $parts = @($relative.Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))
    $current = $root.TrimEnd('\', '/')
    foreach ($part in $parts) {
        Assert-CutoverDeadline
        $current = Join-Path $current $part
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($null -eq $item) {
            Assert-CutoverDeadline
            [System.IO.Directory]::CreateDirectory($current) | Out-Null
            $item = Get-Item -LiteralPath $current -Force
        }
        if ($item -isnot [System.IO.DirectoryInfo] -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "directory path contains a non-directory or reparse component: '$current'."
        }
    }
    Assert-CutoverPathChain -LiteralPath $full | Out-Null
    return $full
}

function Assert-CutoverConfinedPath {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$AncestorPath,
        [switch]$AllowMissingLeaf
    )

    Assert-CutoverDeadline
    $full = Assert-CutoverPathChain -LiteralPath $LiteralPath -AllowMissingLeaf:$AllowMissingLeaf
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor $AncestorPath)) {
        throw "Path is outside its confined root: '$full'."
    }
    return $full
}

function Test-CutoverConfinedFilePresent {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $opened = $null
    try {
        $opened = Open-CutoverConfinedFile -LiteralPath $LiteralPath -AuthorizedRoot $rootPath
        if ($opened.identity.length -lt 0) { return $false }
        return $true
    }
    catch {
        $category = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
        if ($category -eq 'audit_internal_error' -or $category -eq 'evidence_invalid') {
            return $false
        }
        throw
    }
    finally {
        if ($null -ne $opened) { $opened.stream.Dispose() }
    }
}

function Normalize-CutoverIdentifier {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($Value -is [string] -and [string]$Value -match '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') {
        return [string]$Value
    }
    Add-ContractError "${Label} is not a safe identifier."
    return 'untrusted-id'
}

function Get-CutoverReportContractId {
    param([object]$Value)

    if ($Value -is [string] -and [string]$Value -match '^[a-z0-9][a-z0-9.-]{0,127}$') {
        return [string]$Value
    }
    return 'untrusted-contract-id'
}

function Get-CutoverSafeReportIdentifier {
    param([object]$Value)

    if ($Value -is [string] -and [string]$Value -match '^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$') {
        return [string]$Value
    }
    return 'untrusted-id'
}

function Get-CutoverCandidateRoot {
    $scriptDirectory = Normalize-CutoverAbsolutePath -LiteralPath $PSScriptRoot -Label 'script directory'
    $candidate = Normalize-CutoverAbsolutePath `
        -LiteralPath ([System.IO.Path]::GetFullPath((Join-Path $scriptDirectory '..\..'))) `
        -Label 'candidate worktree'
    Assert-CutoverPathChain -LiteralPath $candidate | Out-Null
    return $candidate
}

function Get-CutoverGitIdentity {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)

    Assert-CutoverDeadline
    try {
        $result = Invoke-CutoverProcess `
            -FileName 'git' `
            -Arguments @('-C', $RepositoryRoot, 'rev-parse', '--show-toplevel', '--git-common-dir', '--is-inside-work-tree') `
            -InputBytes ([byte[]]@()) `
            -MaxStdoutBytes 16384 `
            -MaxStderrBytes 16384 `
            -DeadlineUtc $auditDeadlineUtc
    }
    catch {
        throw
    }
    if (-not $result.Success) { throw 'git identity process failed.' }
    if ($result.ExitCode -ne 0) { throw 'git identity returned nonzero.' }
    try {
        $text = ([System.Text.UTF8Encoding]::new($false, $true)).GetString($result.StandardOutput)
    }
    catch { throw 'git identity was not valid UTF-8.' }
    $lines = @($text -split "`r?`n" | Where-Object { $_ -ne '' })
    if ($lines.Count -ne 3 -or $lines[2] -ne 'true') { throw 'git worktree identity was malformed.' }
    $top = Normalize-CutoverAbsolutePath -LiteralPath ([string]$lines[0]) -Label 'git worktree root'
    $common = [string]$lines[1]
    if (-not [System.IO.Path]::IsPathFullyQualified($common)) {
        $common = Join-Path $RepositoryRoot $common
    }
    $common = Normalize-CutoverAbsolutePath -LiteralPath $common -Label 'git common directory'
    Assert-CutoverPathChain -LiteralPath $common | Out-Null
    return [pscustomobject]@{
        topLevel = $top
        commonDirectory = $common
        insideWorktree = $true
    }
}

function Assert-CutoverAuthorizedRoot {
    param([string]$RequestedRoot)

    $script:candidateRootPath = Get-CutoverCandidateRoot
    $requested = if ([string]::IsNullOrWhiteSpace($RequestedRoot)) {
        $script:candidateRootPath
    }
    else {
        Normalize-CutoverAbsolutePath -LiteralPath $RequestedRoot -Label 'Root'
    }
    $isCandidate = $requested.Equals($script:candidateRootPath, [System.StringComparison]::OrdinalIgnoreCase)
    if ($isCandidate) {
        Assert-CutoverPathChain -LiteralPath $requested | Out-Null
        $script:authorizedRootKind = 'candidate-worktree'
    }
    else {
        $providedToken = [string]$env:DEVMANAGER_CUTOVER_FIXTURE_AUTH
        if (-not [string]::Equals($providedToken, $fixtureAuthToken, [System.StringComparison]::Ordinal)) {
            throw 'root is not an authorized candidate or authenticated fixture.'
        }
        # The authentication boundary precedes every read of a caller-selected
        # fixture. Only an explicitly authenticated generated fixture may reach
        # path-chain metadata or the marker handle below.
        Assert-CutoverPathChain -LiteralPath $requested | Out-Null
        $marker = Join-Path $requested '.devmanager-next\audit-fixture.auth'
        $markerHandle = $null
        try {
            $markerHandle = Open-CutoverConfinedFile -LiteralPath $marker -AuthorizedRoot $requested
            $markerBytes = Read-CutoverStreamBytes -Stream $markerHandle.stream -MaxBytes 256 -Label 'fixture authorization'
            $markerText = ([System.Text.UTF8Encoding]::new($false, $true)).GetString($markerBytes)
            if (-not [string]::Equals($markerText, "$fixtureAuthToken`n", [System.StringComparison]::Ordinal)) {
                throw 'fixture authorization marker was invalid.'
            }
        }
        finally {
            if ($null -ne $markerHandle) { $markerHandle.stream.Dispose() }
        }
        $script:authorizedRootKind = 'authenticated-fixture'
    }

    # Establish the caller-selected root and its opened-handle identity before
    # invoking Git.  If Git itself is unavailable or times out, the audit can
    # still publish a bounded HOLD report inside this already-authorized root;
    # it must not silently turn a process failure into a no-report failure.
    $script:rootPath = $requested
    $script:rootIdentity = Get-CutoverPathIdentity -LiteralPath $script:rootPath -AllowDirectory
    try {
        $script:gitIdentity = Get-CutoverGitIdentity -RepositoryRoot $script:rootPath
    }
    catch {
        $script:gitIdentity = $null
        $script:authorizationFailure = 'authorized root Git identity was not established.'
        return $script:rootPath
    }
    if (-not $script:gitIdentity.topLevel.Equals($script:rootPath, [System.StringComparison]::OrdinalIgnoreCase)) {
        $script:authorizationFailure = 'authorized root Git identity did not match the root.'
        return $script:rootPath
    }
    $script:commonDirectoryIdentity = Get-CutoverPathIdentity -LiteralPath $script:gitIdentity.commonDirectory -AllowDirectory
    return $script:rootPath
}

function Get-CutoverProcessEnvironment {
    param([Parameter(Mandatory = $true)][string]$ResolvedExecutable)

    # Child processes receive a deliberately small environment.  In
    # particular, no user Git config, helper, hook, credential, proxy, alias,
    # working-tree override, or arbitrary secret is inherited from DevManager.
    # The executable directory and the Windows runtime directories are enough
    # for the real tools and for the generated test shims (which launch pwsh
    # and cmd by name).
    $pathDirectories = New-Object 'System.Collections.Generic.List[string]'
    foreach ($directory in @(
            (Split-Path -Parent $ResolvedExecutable),
            (Join-Path $env:SystemRoot 'System32'),
            $env:SystemRoot,
            (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0')
        )) {
        Assert-CutoverDeadline
        if ([string]::IsNullOrWhiteSpace($directory)) { continue }
        try {
            $full = Normalize-CutoverAbsolutePath -LiteralPath $directory -Label 'process runtime directory'
            if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor ([System.IO.Path]::GetPathRoot($full)))) { continue }
            if (-not $pathDirectories.Contains($full)) { $pathDirectories.Add($full) }
        }
        catch { }
    }
    try {
        $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $pwsh -and -not [string]::IsNullOrWhiteSpace([string]$pwsh.Source)) {
            $pwshDirectory = Split-Path -Parent ([string]$pwsh.Source)
            if (-not $pathDirectories.Contains($pwshDirectory)) { $pathDirectories.Add($pwshDirectory) }
        }
    }
    catch { }

    # Authenticated generated fixtures may place their shim directory at the
    # front of PATH.  Preserve that fixture-only launch path so the test
    # helper is exercised; normal candidate-worktree scans stay on the
    # runtime-only path assembled above.
    if ($authorizedRootKind -eq 'authenticated-fixture') {
        foreach ($directory in [Environment]::GetEnvironmentVariable('PATH').Split(';')) {
            Assert-CutoverDeadline
            if ([string]::IsNullOrWhiteSpace($directory)) { continue }
            try {
                $full = Normalize-CutoverAbsolutePath -LiteralPath $directory -Label 'fixture process path'
                if (-not $pathDirectories.Contains($full)) { $pathDirectories.Insert(0, $full) }
            }
            catch { }
        }
    }

    $environment = New-Object 'System.Collections.Generic.List[string]'
    foreach ($entry in @(
            "SystemRoot=$env:SystemRoot",
            "WINDIR=$env:WINDIR",
            "TEMP=$env:TEMP",
            "TMP=$env:TMP",
            "COMSPEC=$env:ComSpec",
            'PATHEXT=.COM;.EXE;.BAT;.CMD',
            ('PATH=' + ($pathDirectories -join ';')),
            'LANG=C',
            'LC_ALL=C',
            'GIT_CONFIG_NOSYSTEM=1',
            'GIT_CONFIG_SYSTEM=NUL',
            'GIT_CONFIG_GLOBAL=NUL',
            'GIT_TERMINAL_PROMPT=0',
            'GIT_OPTIONAL_LOCKS=0',
            'GIT_CONFIG_COUNT=4',
            'GIT_CONFIG_KEY_0=core.hooksPath',
            'GIT_CONFIG_VALUE_0=NUL',
            'GIT_CONFIG_KEY_1=core.fsmonitor',
            'GIT_CONFIG_VALUE_1=false',
            'GIT_CONFIG_KEY_2=credential.helper',
            'GIT_CONFIG_VALUE_2=',
            'GIT_CONFIG_KEY_3=protocol.file.allow',
            'GIT_CONFIG_VALUE_3=never'
        )) {
        Assert-CutoverDeadline
        if ($entry -notmatch '^[^=]+=') { continue }
        $environment.Add($entry)
    }

    # Generated fixture shims are intentionally opt-in and are the only
    # non-runtime variables permitted across the boundary.  They are never
    # present for the normal candidate-worktree invocation.
    foreach ($name in @(
            'GIT_FAKE_MODE', 'GIT_REAL', 'GIT_CHILD_SENTINEL', 'GIT_PROBE_LOG',
            'RG_FAKE_MODE', 'RG_FAKE_TARGET', 'RG_FAKE_RESIDUE', 'RG_FAKE_RESIDUE_PID',
            'RG_FAKE_OUTSIDE', 'RG_SHIM_LOG', 'RG_REAL'
        )) {
        Assert-CutoverDeadline
        $value = [Environment]::GetEnvironmentVariable($name)
        if ($null -ne $value) { $environment.Add("$name=$value") }
    }
    return @($environment.ToArray())
}

function Get-BoundedContractStringArray {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $result = New-Object 'System.Collections.Generic.List[string]'
    foreach ($item in Get-ContractArray $Value) {
        Assert-CutoverDeadline
        if ($result.Count -ge $maxStringsPerRow) { Add-SafetyBound; break }
        if ($item -isnot [string] -or [string]::IsNullOrEmpty([string]$item)) { continue }
        $text = [string]$item
        if ($text.Length -gt $maxNeedleChars -or $text.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
            Add-SafetyBound
            continue
        }
        $result.Add($text)
    }
    return @($result.ToArray())
}

function Sort-CutoverOrdinalStrings {
    param([AllowEmptyCollection()][object[]]$Values)

    $unique = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $result = New-Object 'System.Collections.Generic.List[string]'
    foreach ($value in $Values) {
        Assert-CutoverDeadline
        if ($null -ne $value) { $null = $unique.Add([string]$value) }
    }
    foreach ($value in $unique) { Assert-CutoverDeadline; $null = $result.Add($value) }
    $result.Sort([System.StringComparer]::Ordinal)
    return @($result.ToArray())
}

function Sort-CutoverOrdinalDiagnostics {
    param([AllowEmptyCollection()][object[]]$Values)

    $result = New-Object 'System.Collections.Generic.List[string]'
    foreach ($value in $Values) {
        Assert-CutoverDeadline
        if ($null -ne $value) { $null = $result.Add([string]$value) }
    }
    $result.Sort([System.StringComparer]::Ordinal)
    return @($result.ToArray())
}

function Sort-CutoverOrdinalObjects {
    param(
        [AllowEmptyCollection()][object[]]$Values,
        [Parameter(Mandatory = $true)][string[]]$Fields
    )

    $result = New-Object 'System.Collections.Generic.List[object]'
    foreach ($value in $Values) { Assert-CutoverDeadline; $result.Add($value) }
    $result.Sort([System.Comparison[object]]{
            param($left, $right)
            foreach ($field in $Fields) {
                $comparison = [string]::Compare([string]$left.$field, [string]$right.$field, [System.StringComparison]::Ordinal)
                if ($comparison -ne 0) { return $comparison }
            }
            return 0
        })
    return @($result.ToArray())
}

function Normalize-ContractRelativePath {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    return Assert-CutoverRelativePath -Value $Value -Label $Label
}

function Normalize-TrackedPath {
    param(
        [Parameter(Mandatory = $true)][string]$RawPath,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $path = $RawPath.Replace('\', '/')
    $root = Normalize-CutoverAbsolutePath -LiteralPath $RepositoryRoot -Label 'repository root'
    if ($path.StartsWith($root.Replace('\', '/') + '/', [System.StringComparison]::OrdinalIgnoreCase)) {
        $path = $path.Substring($root.Length + 1)
    }
    return $path
}

function Test-TrackedPathPresent {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string[]]$Tracked
    )

    foreach ($tracked in $Tracked) {
        if ([string]::Equals([string]$tracked, $Path, [System.StringComparison]::Ordinal)) {
            return $true
        }
    }
    return $false
}

function Read-CutoverContract {
    param(
        [Parameter(Mandatory = $true)][string]$LedgerPath
    )

    $jsonSource = Read-CutoverConfinedUtf8 `
        -LiteralPath $LedgerPath `
        -MaxBytes $maxLedgerBytes `
        -Label 'cutover ledger'
    $lines = @([regex]::Split($jsonSource, "\r\n|\n|\r"))
    $openings = @()
    for ($index = 0; $index -lt $lines.Count; $index++) {
        Assert-CutoverDeadline
        if ([string]$lines[$index] -eq '```json cutover-contract') {
            $openings += $index
        }
    }
    if ($openings.Count -ne 1) {
        throw "Ledger must contain exactly one ```json cutover-contract block."
    }

    $opening = [int]$openings[0]
    $closing = $null
    for ($index = $opening + 1; $index -lt $lines.Count; $index++) {
        Assert-CutoverDeadline
        if ([string]$lines[$index] -eq '```') {
            $closing = $index
            break
        }
    }
    if ($null -eq $closing -or $closing -le $opening + 1) {
        throw 'Ledger contract JSON block is missing its closing fence or is empty.'
    }

    $jsonText = ($lines[($opening + 1)..($closing - 1)] -join [Environment]::NewLine)
    try {
        return ($jsonText | ConvertFrom-Json -Depth 100)
    }
    catch {
        throw 'Ledger contract JSON is invalid.'
    }
}

function Invoke-GitTrackedFiles {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)

    $arguments = @('-C', $RepositoryRoot, 'ls-files', '--full-name', '-z', '--')
    $result = Invoke-CutoverProcess `
        -FileName 'git' `
        -Arguments $arguments `
        -InputBytes ([byte[]]@()) `
        -MaxStdoutBytes $maxTrackedBytes `
        -MaxStderrBytes $maxTrackedBytes `
        -DeadlineUtc $auditDeadlineUtc
    if (-not $result.Success) {
        throw "git enumeration failed ($($result.FailureCategory))."
    }
    if ($result.ExitCode -ne 0) {
        throw 'git enumeration returned nonzero.'
    }
    $bytes = $result.StandardOutput
    $text = try {
        ([System.Text.UTF8Encoding]::new($false, $true)).GetString($bytes)
    }
    catch {
        Add-SafetyBound
        throw 'git ls-files returned invalid UTF-8.'
    }

    $exact = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $physical = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)
    $paths = New-Object 'System.Collections.Generic.List[string]'
    foreach ($rawPath in @($text.Split([char]0))) {
        if ([string]::IsNullOrEmpty($rawPath)) { continue }
        if ($paths.Count -ge $maxTrackedFiles) {
            Add-SafetyBound
            break
        }
        $path = Assert-CutoverTrackedGitPath -RawPath $rawPath
        if ($null -eq $path) {
            if (Test-CutoverTrackedPathHasUnsupportedControl -RawPath $rawPath) {
                Add-GlobalBlocker 'tracked path policy rejected an unsupported control-name; reference collection failed closed for that path.'
            }
            else {
                Add-SafetyBound
            }
            continue
        }
        if (-not $exact.Add($path)) { continue }
        if (-not $physical.Add($path)) {
            Add-SafetyBound
            continue
        }
        $paths.Add($path)
    }
    return Sort-CutoverOrdinalStrings -Values @($paths.ToArray())
}

function Invoke-CutoverProcess {
    param(
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][byte[]]$InputBytes,
        [Parameter(Mandatory = $true)][int64]$MaxStdoutBytes,
        [Parameter(Mandatory = $true)][int64]$MaxStderrBytes,
        [Parameter(Mandatory = $true)][DateTime]$DeadlineUtc
    )

    Initialize-CutoverProcessMethodsV2
    $resolvedExecutable = $FileName
    $environment = Get-CutoverProcessEnvironment -ResolvedExecutable $resolvedExecutable
    $processResult = [CutoverProcessMethodsV2]::Run(
        $FileName,
        $Arguments,
        $InputBytes,
        [int]$MaxStdoutBytes,
        [int]$MaxStderrBytes,
        $DeadlineUtc.Ticks,
        $environment)
    return $processResult
}

function Assert-CutoverTrackedGitPath {
    param([Parameter(Mandatory = $true)][string]$RawPath)

    if ([string]::IsNullOrEmpty($RawPath) -or $RawPath.Contains('\') -or $RawPath.StartsWith('/') -or $RawPath.Contains(':')) {
        return $null
    }
    foreach ($part in @($RawPath.Split('/'))) {
        if ([string]::IsNullOrEmpty($part) -or $part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) {
            return $null
        }
        if (Test-CutoverTrackedPathHasUnsupportedControl -RawPath $part) {
            return $null
        }
    }
    return $RawPath
}

function Test-CutoverTrackedPathHasUnsupportedControl {
    param([Parameter(Mandatory = $true)][string]$RawPath)

    foreach ($character in $RawPath.ToCharArray()) {
        $code = [int][char]$character
        # Git's NUL-delimited output and ProcessStartInfo argument list preserve
        # tabs/newlines; every other control name fails closed before scanning.
        if (($code -lt 32 -and $code -ne 9 -and $code -ne 10) -or $code -eq 127) {
            return $true
        }
    }
    return $false
}

function Assert-NodeGraph {
    param(
        [Parameter(Mandatory = $true)][object[]]$Nodes,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$NodeById
    )

    $visitState = New-Object 'System.Collections.Generic.Dictionary[string,int]' ([System.StringComparer]::Ordinal)
    function Visit-Node {
        param([Parameter(Mandatory = $true)][string]$NodeId)

        Assert-CutoverDeadline

        if (-not $NodeById.ContainsKey($NodeId)) {
            Add-ContractError "unknown prerequisite node '$NodeId'."
            return
        }
        if ($visitState.ContainsKey($NodeId) -and $visitState[$NodeId] -eq 1) {
            Add-ContractError "circular prerequisite dependency at '$NodeId'."
            return
        }
        if ($visitState.ContainsKey($NodeId) -and $visitState[$NodeId] -eq 2) {
            return
        }

        $visitState[$NodeId] = 1
        $node = $NodeById[$NodeId]
        foreach ($dependency in Get-ContractArray (Get-ContractProperty -Object $node -Name 'dependsOn')) {
            if ($dependency -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$dependency)) {
                Add-ContractError "prerequisite node '$NodeId' has an empty dependency."
                continue
            }
            Visit-Node -NodeId ([string]$dependency)
        }
        $visitState[$NodeId] = 2
    }

    foreach ($node in $Nodes) {
        Assert-CutoverDeadline
        $nodeId = [string](Get-ContractProperty -Object $node -Name 'id')
        if (-not [string]::IsNullOrWhiteSpace($nodeId)) {
            Visit-Node -NodeId $nodeId
        }
    }
}

function Add-Needle {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[object]]$Needles,
        [Parameter(Mandatory = $true)][System.Collections.IDictionary]$NeedleKeys,
        [Parameter(Mandatory = $true)][string]$OwnerId,
        [Parameter(Mandatory = $true)][string]$Kind,
        [object]$Value,
        [AllowEmptyString()][string]$ContextPath
    )

    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        return
    }
    $needle = [string]$Value
    if ($needle.Length -gt $maxNeedleChars -or $needle.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
        Add-SafetyBound
        return
    }
    if ($Needles.Count -ge $maxNeedles) {
        Add-SafetyBound
        return
    }
    $key = "$OwnerId|$Kind|$ContextPath|$needle"
    if ($NeedleKeys.ContainsKey($key)) {
        return
    }
    $NeedleKeys[$key] = $true
    $Needles.Add([pscustomobject]@{
            ownerId = $OwnerId
            kind    = $Kind
            needle  = $needle
            contextPath = $ContextPath
    })
}

function Invoke-ReferenceScan {
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string[]]$Tracked,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[object]]$Needles,
        [Parameter(Mandatory = $true)][int]$MaxMatches
    )

    $matches = New-Object 'System.Collections.Generic.List[object]'
    if ($Needles.Count -eq 0) { return @($matches) }

    $counts = New-Object 'System.Collections.Generic.Dictionary[string,int]' ([System.StringComparer]::Ordinal)
    $fileCount = 0
    foreach ($relativePath in $Tracked) {
        Assert-CutoverDeadline
        if ($fileCount -ge $maxScannerFiles) { Add-SafetyBound; break }
        $leaf = [System.IO.Path]::GetFileName($relativePath)
        if ($leaf.Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase) -or
            [string]::Equals($relativePath, '.devmanager-next/audit-fixture.auth', [System.StringComparison]::Ordinal) -or
            $relativePath -eq 'docs/replacement-deletion-ledger.md') {
            continue
        }
        $fileCount++
        $absolutePath = Join-Path $RepositoryRoot ($relativePath.Replace('/', '\'))
        $opened = $null
        try {
            $absolutePath = Assert-CutoverConfinedPath -LiteralPath $absolutePath -AncestorPath $RepositoryRoot
            $opened = Open-CutoverConfinedFile -LiteralPath $absolutePath -ReadOnlyShare
            if ($opened.identity.length -gt $maxScanBytesPerFile) {
                Add-SafetyBound
                continue
            }

            $scanBytes = Read-CutoverScanBytes -Opened $opened -MaxBytes $maxScanBytesPerFile
            $scanDigest = Get-CutoverSha256Hex -Bytes $scanBytes

            $scanText = $null
            try {
                $scanText = [System.Text.UTF8Encoding]::new($false, $true).GetString($scanBytes)
            }
            catch {
                # Invalid UTF-8 remains eligible for rg --text byte scanning;
                # only valid UTF-8 can use the no-process prefilter safely.
            }
            $scanNeedles = if ($null -eq $scanText) {
                @($Needles.ToArray())
            }
            else {
                @($Needles | Where-Object {
                        $scanText.IndexOf([string]$_.needle, [System.StringComparison]::Ordinal) -ge 0
                    })
            }
            $scan = [pscustomobject]@{ lines = @(); exitCode = 0; boundHit = $false }
            if ($scanNeedles.Count -gt 0) {
                $arguments = @(
                    '--json', '--fixed-strings', '--line-number', '--no-heading', '--color', 'never',
                    '--no-messages', '--text', '--hidden', '--no-ignore', '--max-count', [string]$MaxMatches,
                    '--max-columns', '4096', '--max-columns-preview'
                )
                foreach ($needle in $scanNeedles) {
                    $arguments += '-e'
                    $arguments += [string]$needle.needle
                }
                $arguments += '--'
                $arguments += '-'
                $scan = Invoke-CutoverProcessLines `
                    -FileName 'rg' `
                    -Arguments $arguments `
                    -InputBytes $scanBytes `
                    -MaxBytes $maxScannerOutputBytes
            }
            $after = Get-CutoverHandleIdentity -Stream $opened.stream
            if (-not (Compare-CutoverIdentity -Before $opened.identity -After $after)) {
                throw 'tracked scanner opened-handle identity changed during the scan.'
            }
            $recheckBytes = Read-CutoverScanBytes -Opened $opened -MaxBytes $maxScanBytesPerFile
            $recheckDigest = Get-CutoverSha256Hex -Bytes $recheckBytes
            if (-not [string]::Equals($scanDigest, $recheckDigest, [System.StringComparison]::Ordinal)) {
                throw 'tracked scanner opened-handle content changed during the scan.'
            }

            $reopened = $null
            try {
                $reopened = Open-CutoverConfinedFile -LiteralPath $absolutePath -ReadOnlyShare
                if (-not (Compare-CutoverIdentity -Before $opened.identity -After $reopened.identity)) {
                    throw 'tracked scanner pathname identity changed during the scan.'
                }
            }
            finally {
                if ($null -ne $reopened) { $reopened.stream.Dispose() }
            }
            if ($scan.exitCode -gt 1) {
                Add-GlobalBlocker 'rg reference scan failed for a validated tracked file.'
                continue
            }
            if ($scan.boundHit) { Add-SafetyBound; break }
            foreach ($rawLine in $scan.lines) {
                if ([string]::IsNullOrWhiteSpace([string]$rawLine)) { continue }
                try { $event = ([string]$rawLine | ConvertFrom-Json -Depth 30) }
                catch { Add-GlobalBlocker 'rg returned a non-JSON event in JSON mode.'; continue }
                if ([string](Get-ContractProperty -Object $event -Name 'type') -ne 'match') { continue }
                $data = Get-ContractProperty -Object $event -Name 'data'
                $submatches = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
                foreach ($submatch in Get-ContractArray (Get-ContractProperty -Object $data -Name 'submatches')) {
                    $matchData = Get-ContractProperty -Object $submatch -Name 'match'
                    $matchText = [string](Get-ContractProperty -Object $matchData -Name 'text')
                    if (-not [string]::IsNullOrEmpty($matchText)) { $null = $submatches.Add($matchText) }
                }
                $lineNumber = 0
                $lineValue = Get-ContractProperty -Object $data -Name 'line_number'
                if ($null -ne $lineValue) { $lineNumber = [int]$lineValue }
                foreach ($needle in $scanNeedles) {
                    if (-not [string]::IsNullOrEmpty([string]$needle.contextPath) -and
                        -not [string]::Equals($relativePath, [string]$needle.contextPath, [System.StringComparison]::Ordinal)) {
                        continue
                    }
                    if (-not $submatches.Contains([string]$needle.needle)) { continue }
                    $key = "$($needle.ownerId)|$($needle.kind)"
                    if (-not $counts.ContainsKey($key)) { $counts[$key] = 0 }
                    if ($counts[$key] -ge [Math]::Min($MaxMatches, $maxMatchesPerOwner)) {
                        Add-SafetyBound
                        continue
                    }
                    $counts[$key]++
                    $matches.Add([pscustomobject]@{
                            ownerId = [string]$needle.ownerId
                            kind = [string]$needle.kind
                            path = $relativePath
                            line = $lineNumber
                        })
                }
            }
        }
        catch {
            $category = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
            Add-GlobalBlocker (Format-CutoverDiagnostic -Category $category)
        }
        finally {
            if ($null -ne $opened) { $opened.stream.Dispose() }
        }
        if ($safetyBoundReached) { break }
    }

    $matches.Sort([System.Comparison[object]]{
            param($left, $right)
            foreach ($field in @('ownerId', 'kind', 'path')) {
                $comparison = [string]::Compare([string]$left.$field, [string]$right.$field, [System.StringComparison]::Ordinal)
                if ($comparison -ne 0) { return $comparison }
            }
            return ([int]$left.line).CompareTo([int]$right.line)
        })
    return @($matches.ToArray())
}

function Initialize-CutoverProcessMethods {
    # Retained only as a compatibility no-op; all audit processes use the V2 Job Object wrapper.
    return
}
function Initialize-CutoverProcessMethodsV2 {
    if ($null -eq ([System.Management.Automation.PSTypeName]'CutoverProcessMethodsV2').Type) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

public sealed class CutoverProcessResultV2
{
    public bool Success { get; set; }
    public string FailureCategory { get; set; }
    public int ExitCode { get; set; }
    public byte[] StandardOutput { get; set; }
    public byte[] StandardError { get; set; }
    public bool ActiveProcessZero { get; set; }
}

public static class CutoverProcessMethodsV2
{
    // Keep a slice of the single audit deadline available for job termination,
    // ACTIVE_PROCESS_ZERO proof, report construction, and bounded publication.
    // This is not a second deadline; every wait still derives from the one
    // absolute deadline supplied by the caller.
    private const int PublicationReserveMilliseconds = 4000;
    private const uint JobObjectExtendedLimitInformation = 9;
    private const uint JobObjectBasicAccountingInformation = 1;
    private const uint JobObjectLimitKillOnJobClose = 0x2000;

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
    private struct BasicAccountingInformation
    {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateJobObject(IntPtr attributes, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetInformationJobObject(
        IntPtr job,
        uint informationClass,
        IntPtr information,
        uint informationLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateJobObject(IntPtr job, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool QueryInformationJobObject(
        IntPtr job,
        uint informationClass,
        IntPtr information,
        uint informationLength,
        out uint returnLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CloseHandle(IntPtr handle);

    private sealed class OutputLimitException : Exception { }

    private sealed class OutputResult
    {
        public byte[] Bytes { get; set; }
    }

    private sealed class NativeProcess
    {
        public IntPtr ProcessHandle;
        public IntPtr ThreadHandle;
        public IntPtr StandardInputWrite;
        public IntPtr StandardOutputRead;
        public IntPtr StandardErrorRead;
        public uint ProcessId;
        public long CreationTime;
    }

    private sealed class TrackedProcess
    {
        public uint ProcessId;
        public long CreationTime;
        public IntPtr Handle;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint LowDateTime;
        public uint HighDateTime;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct ProcessEntry
    {
        public uint Size;
        public uint Usage;
        public uint ProcessId;
        public IntPtr DefaultHeapId;
        public uint ModuleId;
        public uint Threads;
        public uint ParentProcessId;
        public int BasePriority;
        public uint Flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)]
        public string ExecutableFile;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct SecurityAttributes
    {
        public int Length;
        public IntPtr SecurityDescriptor;
        public bool InheritHandle;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo
    {
        public int Cb;
        public string Reserved;
        public string Desktop;
        public string Title;
        public int X;
        public int Y;
        public int XSize;
        public int YSize;
        public int XCountChars;
        public int YCountChars;
        public int FillAttribute;
        public int Flags;
        public short ShowWindow;
        public short Reserved2;
        public IntPtr Reserved2Pointer;
        public IntPtr StandardInput;
        public IntPtr StandardOutput;
        public IntPtr StandardError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation
    {
        public IntPtr ProcessHandle;
        public IntPtr ThreadHandle;
        public uint ProcessId;
        public uint ThreadId;
    }

    private const int StartfUseStdHandles = 0x00000100;
    private const uint CreateSuspended = 0x00000004;
    private const uint CreateNoWindow = 0x08000000;
    private const uint CreateUnicodeEnvironment = 0x00000400;
    private const uint HandleFlagInherit = 0x00000001;
    private const uint WaitObject0 = 0x00000000;
    private const uint WaitTimeout = 0x00000102;
    private const uint SnapshotProcesses = 0x00000002;
    private const uint ProcessTerminate = 0x00000001;
    private const uint ProcessQueryLimitedInformation = 0x00001000;
    private const uint Synchronize = 0x00100000;
    private const uint InvalidHandleValue = 0xFFFFFFFF;

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool CreatePipe(
        out IntPtr readPipe,
        out IntPtr writePipe,
        ref SecurityAttributes attributes,
        uint size);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetHandleInformation(
        IntPtr handle,
        uint mask,
        uint flags);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CreateProcessW(
        string applicationName,
        StringBuilder commandLine,
        IntPtr processAttributes,
        IntPtr threadAttributes,
        bool inheritHandles,
        uint creationFlags,
        IntPtr environment,
        string currentDirectory,
        ref StartupInfo startupInfo,
        out ProcessInformation processInformation);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern uint SearchPathW(
        string path,
        string fileName,
        string extension,
        int bufferLength,
        StringBuilder buffer,
        IntPtr filePart);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint ResumeThread(IntPtr thread);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint access, bool inheritHandle, uint processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint processId);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool Process32FirstW(IntPtr snapshot, ref ProcessEntry entry);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool Process32NextW(IntPtr snapshot, ref ProcessEntry entry);

    private static DateTime Deadline(long ticks)
    {
        return new DateTime(ticks, DateTimeKind.Utc);
    }

    private static bool Remaining(DateTime deadline, out int milliseconds)
    {
        var remaining = deadline - DateTime.UtcNow;
        if (remaining <= TimeSpan.Zero)
        {
            milliseconds = 0;
            return false;
        }
        milliseconds = (int)Math.Min(int.MaxValue, Math.Max(1, remaining.TotalMilliseconds));
        return true;
    }

    private static string QuoteCommandLineValue(string value)
    {
        if (value == null) return "\"\"";
        var quoted = new StringBuilder();
        quoted.Append('"');
        var slashes = 0;
        foreach (var character in value)
        {
            if (character == '\\')
            {
                slashes++;
                continue;
            }
            if (character == '"')
            {
                quoted.Append('\\', slashes * 2 + 1);
                quoted.Append('"');
                slashes = 0;
                continue;
            }
            quoted.Append('\\', slashes);
            slashes = 0;
            quoted.Append(character);
        }
        quoted.Append('\\', slashes * 2);
        quoted.Append('"');
        return quoted.ToString();
    }

    private static StringBuilder BuildCommandLine(string fileName, string[] arguments)
    {
        var commandLine = new StringBuilder(QuoteCommandLineValue(fileName));
        foreach (var argument in arguments)
        {
            commandLine.Append(' ');
            commandLine.Append(QuoteCommandLineValue(argument));
        }
        return commandLine;
    }

    private static IntPtr BuildEnvironmentBlock(string[] environment)
    {
        var values = new List<string>();
        if (environment != null)
        {
            foreach (var entry in environment)
            {
                if (string.IsNullOrEmpty(entry) || entry.IndexOf('\0') >= 0) continue;
                values.Add(entry);
            }
        }
        values.Sort(StringComparer.OrdinalIgnoreCase);
        var block = string.Join("\0", values) + "\0\0";
        return Marshal.StringToHGlobalUni(block);
    }

    private static string ResolveExecutable(string fileName)
    {
        var buffer = new StringBuilder(32768);
        var path = Environment.GetEnvironmentVariable("PATH");
        var length = SearchPathW(path, fileName, ".exe", buffer.Capacity, buffer, IntPtr.Zero);
        if (length == 0 || length >= buffer.Capacity) throw new InvalidOperationException("process-resolve");
        return buffer.ToString();
    }

    private static void CloseIfOpen(ref IntPtr handle)
    {
        if (handle != IntPtr.Zero)
        {
            CloseHandle(handle);
            handle = IntPtr.Zero;
        }
    }

    private static long GetCreationTime(IntPtr process)
    {
        FileTime creation;
        FileTime exit;
        FileTime kernel;
        FileTime user;
        if (!GetProcessTimes(process, out creation, out exit, out kernel, out user)) return 0;
        return ((long)creation.HighDateTime << 32) | creation.LowDateTime;
    }

    private static List<TrackedProcess> FindDescendants(NativeProcess root)
    {
        var tracked = new List<TrackedProcess>();
        if (root == null || root.ProcessId == 0 || root.CreationTime == 0) return tracked;
        tracked.Add(new TrackedProcess
        {
            ProcessId = root.ProcessId,
            CreationTime = root.CreationTime,
            Handle = root.ProcessHandle
        });
        for (var pass = 0; pass < 4; pass++)
        {
            var added = false;
            var snapshot = CreateToolhelp32Snapshot(SnapshotProcesses, 0);
            if (snapshot == IntPtr.Zero || snapshot == new IntPtr(-1)) break;
            try
            {
                var entry = new ProcessEntry { Size = (uint)Marshal.SizeOf(typeof(ProcessEntry)) };
                if (!Process32FirstW(snapshot, ref entry)) continue;
                do
                {
                    if (entry.ProcessId == 0 || entry.ProcessId == root.ProcessId) continue;
                    var parentKnown = false;
                    foreach (var parent in tracked)
                    {
                        if (parent.ProcessId == entry.ParentProcessId)
                        {
                            parentKnown = true;
                            break;
                        }
                    }
                    if (!parentKnown) continue;
                    var alreadyKnown = false;
                    foreach (var existing in tracked)
                    {
                        if (existing.ProcessId == entry.ProcessId)
                        {
                            alreadyKnown = true;
                            break;
                        }
                    }
                    if (alreadyKnown) continue;
                    var handle = OpenProcess(
                        ProcessTerminate | ProcessQueryLimitedInformation | Synchronize,
                        false,
                        entry.ProcessId);
                    if (handle == IntPtr.Zero) continue;
                    var creationTime = GetCreationTime(handle);
                    if (creationTime == 0)
                    {
                        CloseHandle(handle);
                        continue;
                    }
                    tracked.Add(new TrackedProcess
                    {
                        ProcessId = entry.ProcessId,
                        CreationTime = creationTime,
                        Handle = handle
                    });
                    added = true;
                }
                while (Process32NextW(snapshot, ref entry));
            }
            finally { CloseHandle(snapshot); }
            if (!added) break;
        }
        return tracked;
    }

    private static bool TerminateTrackedDescendants(NativeProcess root, DateTime deadline)
    {
        var tracked = FindDescendants(root);
        var settled = true;
        foreach (var process in tracked)
        {
            if (process.Handle == root.ProcessHandle) continue;
            try
            {
                if (WaitForSingleObject(process.Handle, 0) != WaitObject0)
                {
                    if (!TerminateProcess(process.Handle, 1))
                    {
                        settled = false;
                        continue;
                    }
                    int milliseconds;
                    if (!Remaining(deadline, out milliseconds) ||
                        WaitForSingleObject(process.Handle, (uint)Math.Max(1, milliseconds)) != WaitObject0)
                    {
                        settled = false;
                    }
                }
            }
            catch { settled = false; }
            finally { CloseHandle(process.Handle); }
        }
        return settled;
    }

    private static NativeProcess CreateSuspendedProcess(string fileName, string[] arguments, string[] environment)
    {
        var security = new SecurityAttributes
        {
            Length = Marshal.SizeOf(typeof(SecurityAttributes)),
            InheritHandle = true,
            SecurityDescriptor = IntPtr.Zero
        };
        IntPtr childInput = IntPtr.Zero;
        IntPtr parentInput = IntPtr.Zero;
        IntPtr parentOutput = IntPtr.Zero;
        IntPtr childOutput = IntPtr.Zero;
        IntPtr parentError = IntPtr.Zero;
        IntPtr childError = IntPtr.Zero;
        ProcessInformation information = new ProcessInformation();
        IntPtr environmentBlock = IntPtr.Zero;
        try
        {
            if (!CreatePipe(out childInput, out parentInput, ref security, 0) ||
                !CreatePipe(out parentOutput, out childOutput, ref security, 0) ||
                !CreatePipe(out parentError, out childError, ref security, 0))
            {
                throw new InvalidOperationException("pipe-create");
            }
            if (!SetHandleInformation(parentInput, HandleFlagInherit, 0) ||
                !SetHandleInformation(parentOutput, HandleFlagInherit, 0) ||
                !SetHandleInformation(parentError, HandleFlagInherit, 0))
            {
                throw new InvalidOperationException("pipe-inheritance");
            }

            var startup = new StartupInfo
            {
                Cb = Marshal.SizeOf(typeof(StartupInfo)),
                Flags = StartfUseStdHandles,
                StandardInput = childInput,
                StandardOutput = childOutput,
                StandardError = childError
            };
            var executable = ResolveExecutable(fileName);
            var commandLine = BuildCommandLine(executable, arguments);
            environmentBlock = BuildEnvironmentBlock(environment);
            var created = CreateProcessW(
                executable,
                commandLine,
                IntPtr.Zero,
                IntPtr.Zero,
                true,
                CreateSuspended | CreateNoWindow | CreateUnicodeEnvironment,
                environmentBlock,
                null,
                ref startup,
                out information);
            if (!created)
            {
                throw new InvalidOperationException("process-create");
            }

            CloseIfOpen(ref childInput);
            CloseIfOpen(ref childOutput);
            CloseIfOpen(ref childError);
            var creationTime = GetCreationTime(information.ProcessHandle);
            if (creationTime == 0) throw new InvalidOperationException("process-identity");
            return new NativeProcess
            {
                ProcessHandle = information.ProcessHandle,
                ThreadHandle = information.ThreadHandle,
                StandardInputWrite = parentInput,
                StandardOutputRead = parentOutput,
                StandardErrorRead = parentError,
                ProcessId = information.ProcessId,
                CreationTime = creationTime
            };
        }
        catch
        {
            CloseIfOpen(ref childInput);
            CloseIfOpen(ref parentInput);
            CloseIfOpen(ref childOutput);
            CloseIfOpen(ref parentOutput);
            CloseIfOpen(ref childError);
            CloseIfOpen(ref parentError);
            CloseIfOpen(ref information.ThreadHandle);
            CloseIfOpen(ref information.ProcessHandle);
            throw;
        }
        finally
        {
            if (environmentBlock != IntPtr.Zero) Marshal.FreeHGlobal(environmentBlock);
        }
    }

    private static async Task WaitForProcessAsync(IntPtr process, CancellationToken cancellation)
    {
        while (true)
        {
            var state = WaitForSingleObject(process, 0);
            if (state == WaitObject0) return;
            if (state != WaitTimeout) throw new InvalidOperationException("process-wait");
            if (cancellation.IsCancellationRequested) return;
            await Task.Delay(25).ConfigureAwait(false);
        }
    }

    private static async Task<OutputResult> ReadBoundedAsync(Stream stream, int maxBytes, CancellationToken cancellation)
    {
        using (cancellation.Register(() => { try { stream.Dispose(); } catch { } }))
        using (var output = new MemoryStream(Math.Min(maxBytes + 1, 8192)))
        {
            var buffer = new byte[8192];
            while (true)
            {
                var remaining = maxBytes - output.Length + 1;
                if (remaining <= 0) throw new OutputLimitException();
                var read = await stream.ReadAsync(buffer, 0, (int)Math.Min(buffer.Length, remaining), cancellation).ConfigureAwait(false);
                if (read == 0) break;
                if (output.Length + read > maxBytes) throw new OutputLimitException();
                output.Write(buffer, 0, read);
            }
            return new OutputResult { Bytes = output.ToArray() };
        }
    }

    private static async Task WriteInputAsync(Stream stream, byte[] input, CancellationToken cancellation)
    {
        using (cancellation.Register(() => { try { stream.Dispose(); } catch { } }))
        {
            try
            {
                if (input.Length > 0)
                {
                    await stream.WriteAsync(input, 0, input.Length, cancellation).ConfigureAwait(false);
                    await stream.FlushAsync(cancellation).ConfigureAwait(false);
                }
            }
            finally
            {
                stream.Close();
            }
        }
    }

    private static async Task<bool> WaitUntilAsync(Task task, DateTime deadline)
    {
        while (!task.IsCompleted)
        {
            int milliseconds;
            if (!Remaining(deadline, out milliseconds)) return false;
            var winner = await Task.WhenAny(task, Task.Delay(milliseconds)).ConfigureAwait(false);
            if (winner != task) return false;
        }
        return true;
    }

    private static async Task<bool> WaitUntilWorkAsync(Task task, DateTime deadline)
    {
        while (!task.IsCompleted)
        {
            int milliseconds;
            if (!Remaining(deadline, out milliseconds) || milliseconds <= PublicationReserveMilliseconds) return false;
            var winner = await Task.WhenAny(task, Task.Delay(milliseconds - PublicationReserveMilliseconds)).ConfigureAwait(false);
            if (winner != task) return false;
        }
        return true;
    }

    private static IntPtr CreateOwnedJob()
    {
        var job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) return IntPtr.Zero;
        var limits = new ExtendedLimitInformation();
        limits.BasicLimitInformation.LimitFlags = JobObjectLimitKillOnJobClose;
        var size = Marshal.SizeOf(typeof(ExtendedLimitInformation));
        var memory = Marshal.AllocHGlobal(size);
        try
        {
            Marshal.StructureToPtr(limits, memory, false);
            if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, memory, (uint)size))
            {
                CloseHandle(job);
                return IntPtr.Zero;
            }
            return job;
        }
        finally { Marshal.FreeHGlobal(memory); }
    }

    private static bool ActiveProcessZero(IntPtr job)
    {
        uint active;
        return TryGetActiveProcessCount(job, out active) && active == 0;
    }

    private static bool TryGetActiveProcessCount(IntPtr job, out uint active)
    {
        var size = Marshal.SizeOf(typeof(BasicAccountingInformation));
        var memory = Marshal.AllocHGlobal(size);
        try
        {
            uint returnLength;
            var queried = QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                memory,
                (uint)size,
                out returnLength);
            var accounting = (BasicAccountingInformation)Marshal.PtrToStructure(memory, typeof(BasicAccountingInformation));
            active = accounting.ActiveProcesses;
            return queried;
        }
        finally { Marshal.FreeHGlobal(memory); }
    }

    private static bool WaitForActiveProcessZero(IntPtr job, DateTime deadline)
    {
        while (true)
        {
            if (ActiveProcessZero(job)) return true;
            int milliseconds;
            if (!Remaining(deadline, out milliseconds)) return false;
            Thread.Sleep(Math.Min(25, milliseconds));
        }
    }

    private static async Task<CutoverProcessResultV2> AbortAsync(
        NativeProcess nativeProcess,
        IntPtr job,
        CancellationTokenSource cancellation,
        Task stdoutTask,
        Task stderrTask,
        Task stdinTask,
        Task exitTask,
        DateTime deadline,
        string failure)
    {
        try { cancellation?.Cancel(); } catch { }
        var processHandle = nativeProcess == null ? IntPtr.Zero : nativeProcess.ProcessHandle;
        var zero = TerminateTrackedDescendants(nativeProcess, deadline);
        if (job != IntPtr.Zero)
        {
            var jobZero = !TerminateJobObject(job, 1) ? ActiveProcessZero(job) : WaitForActiveProcessZero(job, deadline);
            var descendantsZero = TerminateTrackedDescendants(nativeProcess, deadline);
            zero = zero && jobZero && descendantsZero;
        }
        else if (processHandle != IntPtr.Zero && WaitForSingleObject(processHandle, 0) != WaitObject0)
        {
            // Process ownership was never established. Terminate only this
            // retained process handle; never enumerate or kill a PID tree.
            try
            {
                if (!TerminateProcess(processHandle, 1)) zero = zero && WaitForSingleObject(processHandle, 0) == WaitObject0;
                else
                {
                    int milliseconds;
                    zero = zero && (WaitForSingleObject(processHandle, 0) == WaitObject0 ||
                        (Remaining(deadline, out milliseconds) &&
                            WaitForSingleObject(processHandle, (uint)Math.Max(1, milliseconds)) == WaitObject0));
                }
            }
            catch { zero = false; }
        }

        var cleanupTasks = new List<Task>();
        if (stdoutTask != null) cleanupTasks.Add(stdoutTask);
        if (stderrTask != null) cleanupTasks.Add(stderrTask);
        if (stdinTask != null) cleanupTasks.Add(stdinTask);
        if (exitTask != null) cleanupTasks.Add(exitTask);
        if (cleanupTasks.Count > 0)
        {
            var cleanup = Task.WhenAll(cleanupTasks.ToArray());
            var cleanupCompleted = await WaitUntilAsync(cleanup, deadline).ConfigureAwait(false);
        }
        return new CutoverProcessResultV2
        {
            Success = false,
            FailureCategory = zero ? failure : "termination-unconfirmed",
            ExitCode = -1,
            StandardOutput = Array.Empty<byte>(),
            StandardError = Array.Empty<byte>(),
            ActiveProcessZero = zero
        };
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);

    public static CutoverProcessResultV2 Run(
        string fileName,
        string[] arguments,
        byte[] input,
        int maxStdoutBytes,
        int maxStderrBytes,
        long deadlineTicks,
        string[] environment)
    {
        var deadline = Deadline(deadlineTicks);
        if (!Remaining(deadline, out _))
        {
            return new CutoverProcessResultV2 { Success = false, FailureCategory = "deadline", ExitCode = -1, StandardOutput = Array.Empty<byte>(), StandardError = Array.Empty<byte>(), ActiveProcessZero = true };
        }

        NativeProcess nativeProcess = null;
        IntPtr job = IntPtr.Zero;
        CancellationTokenSource cancellation = null;
        Task<OutputResult> stdoutTask = null;
        Task<OutputResult> stderrTask = null;
        Task stdinTask = null;
        Task exitTask = null;
        OutputResult stdout = null;
        OutputResult stderr = null;
        FileStream standardInput = null;
        FileStream standardOutput = null;
        FileStream standardError = null;
        var stdoutChecked = false;
        var stderrChecked = false;
        var stdinChecked = false;
        var exitChecked = false;
        try
        {
            job = CreateOwnedJob();
            if (job == IntPtr.Zero)
            {
                return new CutoverProcessResultV2 { Success = false, FailureCategory = "ownership", ExitCode = -1, StandardOutput = Array.Empty<byte>(), StandardError = Array.Empty<byte>(), ActiveProcessZero = true };
            }
            // The primary thread stays suspended until the process handle is
            // assigned to the Job. This closes the start/assign race in which
            // a fast helper could create an unowned descendant.
            nativeProcess = CreateSuspendedProcess(fileName, arguments, environment);
            if (!AssignProcessToJobObject(job, nativeProcess.ProcessHandle))
            {
                return AbortAsync(nativeProcess, job, cancellation, null, null, null, null, deadline, "ownership").GetAwaiter().GetResult();
            }
            if (ResumeThread(nativeProcess.ThreadHandle) == UInt32.MaxValue)
            {
                return AbortAsync(nativeProcess, job, cancellation, null, null, null, null, deadline, "start").GetAwaiter().GetResult();
            }
            CloseIfOpen(ref nativeProcess.ThreadHandle);
            standardInput = new FileStream(
                new Microsoft.Win32.SafeHandles.SafeFileHandle(nativeProcess.StandardInputWrite, true),
                FileAccess.Write,
                8192,
                false);
            nativeProcess.StandardInputWrite = IntPtr.Zero;
            standardOutput = new FileStream(
                new Microsoft.Win32.SafeHandles.SafeFileHandle(nativeProcess.StandardOutputRead, true),
                FileAccess.Read,
                8192,
                false);
            nativeProcess.StandardOutputRead = IntPtr.Zero;
            standardError = new FileStream(
                new Microsoft.Win32.SafeHandles.SafeFileHandle(nativeProcess.StandardErrorRead, true),
                FileAccess.Read,
                8192,
                false);
            nativeProcess.StandardErrorRead = IntPtr.Zero;
            cancellation = new CancellationTokenSource();
            // CreatePipe returns synchronous handles. Start every pipe operation
            // on the thread pool so a no-output read cannot prevent stdin from
            // closing or the other output reader from starting.
            stdoutTask = Task.Run(() => ReadBoundedAsync(standardOutput, maxStdoutBytes, cancellation.Token));
            stderrTask = Task.Run(() => ReadBoundedAsync(standardError, maxStderrBytes, cancellation.Token));
            stdinTask = Task.Run(() => WriteInputAsync(standardInput, input, cancellation.Token));
            exitTask = WaitForProcessAsync(nativeProcess.ProcessHandle, cancellation.Token);

            while (!(stdoutChecked && stderrChecked && stdinChecked && exitChecked))
            {
                if (stdoutTask.IsCompleted && !stdoutChecked)
                {
                    try { stdout = stdoutTask.GetAwaiter().GetResult(); }
                    catch (OutputLimitException) { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdout-overflow").GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdout-error").GetAwaiter().GetResult(); }
                    stdoutChecked = true;
                }
                if (stderrTask.IsCompleted && !stderrChecked)
                {
                    try { stderr = stderrTask.GetAwaiter().GetResult(); }
                    catch (OutputLimitException) { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stderr-overflow").GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stderr-error").GetAwaiter().GetResult(); }
                    stderrChecked = true;
                }
                if (stdinTask.IsCompleted && !stdinChecked)
                {
                    try { stdinTask.GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdin-error").GetAwaiter().GetResult(); }
                    stdinChecked = true;
                }
                if (exitTask.IsCompleted && !exitChecked)
                {
                    try { exitTask.GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "exit-error").GetAwaiter().GetResult(); }
                    exitChecked = true;
                }
                if (stdoutChecked && stderrChecked && stdinChecked && exitChecked) break;
                var pending = new List<Task>();
                if (!stdoutChecked) pending.Add(stdoutTask);
                if (!stderrChecked) pending.Add(stderrTask);
                if (!stdinChecked) pending.Add(stdinTask);
                if (!exitChecked) pending.Add(exitTask);
                if (!WaitUntilWorkAsync(Task.WhenAny(pending.ToArray()), deadline).GetAwaiter().GetResult())
                {
                    return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "timeout").GetAwaiter().GetResult();
                }
            }
            uint exitCode;
            if (!GetExitCodeProcess(nativeProcess.ProcessHandle, out exitCode))
            {
                return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "exit-error").GetAwaiter().GetResult();
            }
            // Process exit and Job accounting are observed independently on
            // Windows. Give the Job a bounded settlement window before
            // treating a still-visible count as an owned descendant.
            if (!ActiveProcessZero(job) && !WaitForActiveProcessZero(job, deadline))
            {
                return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "descendant").GetAwaiter().GetResult();
            }
            return new CutoverProcessResultV2
            {
                Success = true,
                FailureCategory = null,
                ExitCode = (int)exitCode,
                StandardOutput = stdout.Bytes,
                StandardError = stderr.Bytes,
                ActiveProcessZero = true
            };
        }
        catch
        {
            return AbortAsync(nativeProcess, job, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "process-error").GetAwaiter().GetResult();
        }
        finally
        {
            try { standardInput?.Dispose(); } catch { }
            try { standardOutput?.Dispose(); } catch { }
            try { standardError?.Dispose(); } catch { }
            try { cancellation?.Dispose(); } catch { }
            if (nativeProcess != null)
            {
                CloseIfOpen(ref nativeProcess.StandardInputWrite);
                CloseIfOpen(ref nativeProcess.StandardOutputRead);
                CloseIfOpen(ref nativeProcess.StandardErrorRead);
                CloseIfOpen(ref nativeProcess.ThreadHandle);
                CloseIfOpen(ref nativeProcess.ProcessHandle);
            }
            if (job != IntPtr.Zero) CloseHandle(job);
        }
    }
}
'@
    }
}

function Invoke-CutoverProcessLines {
    param(
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][byte[]]$InputBytes,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    Initialize-CutoverProcessMethodsV2
    $resolvedExecutable = $FileName
    $environment = Get-CutoverProcessEnvironment -ResolvedExecutable $resolvedExecutable
    $result = [CutoverProcessMethodsV2]::Run(
        $FileName,
        $Arguments,
        $InputBytes,
        [int]$MaxBytes,
        [int]$MaxBytes,
        $auditDeadlineUtc.Ticks,
        $environment)
    if (-not $result.Success) {
        Add-SafetyBound
        throw "bounded scanner invocation failed ($($result.FailureCategory))."
    }
    try {
        $stdout = [System.Text.UTF8Encoding]::new($false, $true).GetString($result.StandardOutput)
    }
    catch {
        Add-SafetyBound
        throw 'bounded scanner output was not valid UTF-8.'
    }
    $lines = @($stdout -split "`r?`n" | Where-Object { $_ -ne '' })
    return [pscustomobject]@{ lines = $lines; exitCode = $result.ExitCode; boundHit = $false }
}

function Get-RelativeReportPath {
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$Path
    )

    return ([System.IO.Path]::GetRelativePath($RepositoryRoot, $Path).Replace('\', '/'))
}

function Assert-AuditOutputPath {
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [string]$RequestedPath
    )

    $candidate = $RequestedPath
    if ([string]::IsNullOrEmpty($candidate)) {
        $candidate = Join-Path $EvidenceRoot 'current/cutover-audit.json'
    }
    elseif ($candidate -notmatch '^[A-Za-z]:[\\/]') {
        $candidate = Join-Path $RepositoryRoot (Assert-CutoverRelativePath -Value $candidate -Label 'OutputPath')
    }
    $full = Normalize-CutoverAbsolutePath -LiteralPath $candidate -Label 'OutputPath'
    if ([System.IO.Path]::GetFileName($full).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'OutputPath basename must not be the protected exact session.json name.'
    }
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor $EvidenceRoot)) {
        throw 'OutputPath must remain beneath .devmanager-next/evidence.'
    }
    if ([System.IO.Path]::GetExtension($full) -ine '.json') { throw 'OutputPath must end in .json.' }
    $parent = Split-Path -Parent $full
    Ensure-CutoverAuditDirectory -LiteralPath $parent | Out-Null
    Assert-CutoverConfinedPath -LiteralPath $full -AncestorPath $EvidenceRoot -AllowMissingLeaf | Out-Null
    $existing = $null
    try {
        $existing = Open-CutoverConfinedFile -LiteralPath $full
    }
    catch {
        if ($_.Exception.Message -ne 'confined handle open failed.') { throw }
    }
    finally {
        if ($null -ne $existing) { $existing.stream.Dispose() }
    }
    return $full
}

function Assert-AuditHumanPath {
    param(
        [Parameter(Mandatory = $true)][string]$JsonPath,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot
    )

    $full = Normalize-CutoverAbsolutePath -LiteralPath ([System.IO.Path]::ChangeExtension($JsonPath, '.txt')) -Label 'human report path'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor $EvidenceRoot)) {
        throw 'Human report path must remain beneath .devmanager-next/evidence.'
    }
    Ensure-CutoverAuditDirectory -LiteralPath (Split-Path -Parent $full) | Out-Null
    Assert-CutoverConfinedPath -LiteralPath $full -AncestorPath $EvidenceRoot -AllowMissingLeaf | Out-Null
    return $full
}

function Test-CutoverCurrentReportPath {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot
    )

    $parent = Normalize-CutoverAbsolutePath -LiteralPath (Split-Path -Parent $LiteralPath) -Label 'report parent'
    $current = Normalize-CutoverAbsolutePath -LiteralPath (Join-Path $EvidenceRoot 'current') -Label 'current report directory'
    $leaf = [System.IO.Path]::GetFileName($LiteralPath)
    return $parent.Equals($current, [System.StringComparison]::OrdinalIgnoreCase) -and
        $leaf -match '^[^\\/:\x00-\x1F\x7F]+\.(json|txt)$'
}

function Write-CutoverAtomicUtf8 {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$Text,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    if ([System.IO.Path]::GetFileName($LiteralPath).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to publish the protected exact session.json name.'
    }
    $full = Assert-CutoverConfinedPath -LiteralPath $LiteralPath -AncestorPath $EvidenceRoot -AllowMissingLeaf
    $parent = Split-Path -Parent $full
    $parentHandle = Open-CutoverConfinedFile -LiteralPath $parent -AllowDirectory
    $tempPath = Join-Path $parent ('.pending-{0}.tmp' -f ([guid]::NewGuid().ToString('N')))
    $backupPath = $null
    $encoding = [System.Text.UTF8Encoding]::new($false)
    $bytes = $encoding.GetBytes($Text)
    try {
        if ($bytes.LongLength -gt $MaxBytes) {
            Add-SafetyBound
            throw 'report text exceeded its bounded output byte limit.'
        }
        Assert-CutoverConfinedPath -LiteralPath $tempPath -AncestorPath $EvidenceRoot -AllowMissingLeaf | Out-Null
        $tempHandle = Open-CutoverConfinedWriteFile -LiteralPath $tempPath -AuthorizedRoot $EvidenceRoot
        $temp = $tempHandle.stream
        try {
            Assert-CutoverDeadline
            $writeTask = $temp.WriteAsync($bytes, 0, $bytes.Length)
            $waitMs = Get-CutoverDeadlineRemainingMilliseconds
            if ($waitMs -le 0 -or -not $writeTask.Wait($waitMs)) {
                throw 'audit deadline exceeded while writing a report.'
            }
            Assert-CutoverDeadline
            $flushTask = $temp.FlushAsync()
            $waitMs = Get-CutoverDeadlineRemainingMilliseconds
            if ($waitMs -le 0 -or -not $flushTask.Wait($waitMs)) {
                throw 'audit deadline exceeded while flushing a report.'
            }
        }
        finally {
            $temp.Dispose()
        }

        $parentAfter = Get-CutoverHandleIdentity -Stream $parentHandle.stream
        if (-not (Compare-CutoverIdentity -Before $parentHandle.identity -After $parentAfter)) {
            throw 'report parent changed before atomic replacement.'
        }
        $tempCheck = Open-CutoverConfinedFile -LiteralPath $tempPath
        $tempIdentity = $tempCheck.identity
        $tempCheck.stream.Dispose()
        Assert-CutoverDeadline
        $destination = $null
        $exists = $false
        try {
            $destination = Open-CutoverConfinedFile -LiteralPath $full
            $exists = $true
        }
        catch {
            $exists = $false
        }
        if ($exists) {
            $destinationIdentity = $destination.identity
            $destination.stream.Dispose()
            if (-not (Test-CutoverCurrentReportPath -LiteralPath $full -EvidenceRoot $EvidenceRoot)) {
                throw "Refusing to overwrite a non-current report path: '$full'."
            }
            $destinationCheck = Open-CutoverConfinedFile -LiteralPath $full
            $destinationCheck.stream.Dispose()
            if (-not (Compare-CutoverIdentity -Before $destinationIdentity -After $destinationCheck.identity)) {
                throw "report destination changed before atomic replacement: '$full'."
            }
            $backupPath = Join-Path $parent ('.backup-{0}.tmp' -f ([guid]::NewGuid().ToString('N')))
            Assert-CutoverConfinedPath -LiteralPath $backupPath -AncestorPath $EvidenceRoot -AllowMissingLeaf | Out-Null
            Assert-CutoverDeadline
            [System.IO.File]::Replace($tempPath, $full, $backupPath, $true)
            if ([System.IO.File]::Exists($backupPath)) { [System.IO.File]::Delete($backupPath) }
        }
        else {
            Assert-CutoverDeadline
            [System.IO.File]::Move($tempPath, $full)
        }
        Assert-CutoverPathChain -LiteralPath $full | Out-Null
        $final = Open-CutoverConfinedFile -LiteralPath $full
        $final.stream.Dispose()
    }
    catch {
        try {
            Assert-CutoverPathChain -LiteralPath $tempPath -AllowMissingLeaf | Out-Null
            if ([System.IO.File]::Exists($tempPath)) { [System.IO.File]::Delete($tempPath) }
        } catch { }
        if ($null -ne $backupPath) {
            try {
                Assert-CutoverPathChain -LiteralPath $backupPath -AllowMissingLeaf | Out-Null
                if ([System.IO.File]::Exists($backupPath)) { [System.IO.File]::Delete($backupPath) }
            } catch { }
        }
        throw
    }
    finally {
        $parentHandle.stream.Dispose()
    }
}

function New-BoundedAuditReport {
    param([Parameter(Mandatory = $true)][object]$Report)

    return [pscustomobject]([ordered]@{
            schemaVersion = 1
            contractId = Get-CutoverReportContractId -Value (Get-ContractProperty -Object $Report -Name 'contractId')
            mode = [string](Get-ContractProperty -Object $Report -Name 'mode')
            contractStatus = 'HOLD'
            ledgerPath = 'docs/replacement-deletion-ledger.md'
            trackedFileCount = [int](Get-ContractProperty -Object $Report -Name 'trackedFileCount')
            protectedFilesSkipped = @()
            contractErrors = @()
            blockers = @($safetyDiagnostic)
            entrypointFindings = @()
            prerequisiteNodes = @()
            rows = @()
            safety = [ordered]@{
                boundReached = $true
                diagnostic = $safetyDiagnostic
                limits = [ordered]@{
                    ledgerBytes = $maxLedgerBytes
                    trackedFiles = $maxTrackedFiles
                    trackedBytes = $maxTrackedBytes
                    rows = $maxRows
                    nodes = $maxNodes
                    stringsPerRow = $maxStringsPerRow
                    needles = $maxNeedles
                    scannerFiles = $maxScannerFiles
                    scanBytesPerFile = $maxScanBytesPerFile
                    scannerOutputBytes = $maxScannerOutputBytes
                    scannerDeadlineMilliseconds = $maxScannerDurationMs
                    errors = $maxErrorCount
                    jsonBytes = $maxReportJsonBytes
                    humanBytes = $maxReportHumanBytes
                }
            }
            scanner = [ordered]@{
                trackedUniverse = 'git-ls-files'
                referenceScanner = 'fixed-string-line-scanner'
                allowedLedgerSelfReferences = @('docs/replacement-deletion-ledger.md')
                protectedFileBasenames = @('session.json')
                maxMatchesPerRow = $maxMatches
                maxOutputBytes = $maxScannerOutputBytes
                deadlineMilliseconds = $maxScannerDurationMs
            }
        })
}

function Write-AuditReports {
    param(
        [Parameter(Mandatory = $true)][object]$Report,
        [Parameter(Mandatory = $true)][string]$JsonPath,
        [Parameter(Mandatory = $true)][string]$TextPath,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][ref]$ContractStatus
    )

    $json = $Report | ConvertTo-Json -Depth 50
    $jsonBytes = [System.Text.UTF8Encoding]::new($false).GetByteCount($json)
    if ($jsonBytes -gt $maxReportJsonBytes) {
        $Report = New-BoundedAuditReport -Report $Report
        $ContractStatus.Value = 'HOLD'
        $json = $Report | ConvertTo-Json -Depth 50
    }

    $lines = New-Object 'System.Collections.Generic.List[string]'
    $lineBytes = [int64]0
    $lineBytesRef = [ref]$lineBytes
    $addLine = {
        param([string]$Line)
        $candidate = [string]$Line + [Environment]::NewLine
        $candidateBytes = [System.Text.UTF8Encoding]::new($false).GetByteCount($candidate)
        if ($lineBytesRef.Value + $candidateBytes -gt $maxReportHumanBytes) {
            return $false
        }
        $lineBytesRef.Value += $candidateBytes
        $lines.Add([string]$Line)
        return $true
    }
    $null = & $addLine 'Phase 11.1 cutover audit'
    $null = & $addLine "status: $($Report.contractStatus)"
    $null = & $addLine "mode: $($Report.mode)"
    $null = & $addLine "tracked files: $($Report.trackedFileCount)"
    $null = & $addLine "protected exact session.json files skipped: $(@($Report.protectedFilesSkipped).Count)"
    $null = & $addLine ''
    $null = & $addLine 'contract errors:'
    foreach ($error in @($Report.contractErrors)) { if (-not (& $addLine "- $error")) { break } }
    $null = & $addLine 'blockers:'
    foreach ($blocker in @($Report.blockers)) { if (-not (& $addLine "- $blocker")) { break } }
    $null = & $addLine 'rows:'
    foreach ($row in @($Report.rows)) {
        if (-not (& $addLine "- $($row.id): $($row.status); legacy=$($row.legacy.path); present=$([bool]$row.legacy.pathPresent)")) { break }
        foreach ($blocker in @($row.blockers)) { if (-not (& $addLine "  blocker: $blocker")) { break } }
    }
    $null = & $addLine 'forbidden entrypoint findings:'
    foreach ($finding in @($Report.entrypointFindings)) { if (-not (& $addLine "- $finding")) { break } }
    $human = ($lines -join [Environment]::NewLine) + [Environment]::NewLine
    if ([System.Text.UTF8Encoding]::new($false).GetByteCount($human) -gt $maxReportHumanBytes) {
        $Report.contractStatus = 'HOLD'
        $ContractStatus.Value = 'HOLD'
        $json = $Report | ConvertTo-Json -Depth 50
        if ([System.Text.UTF8Encoding]::new($false).GetByteCount($json) -gt $maxReportJsonBytes) {
            $Report = New-BoundedAuditReport -Report $Report
            $json = $Report | ConvertTo-Json -Depth 50
        }
        $human = "Phase 11.1 cutover audit`nstatus: HOLD`n- $safetyDiagnostic`n"
    }
    Write-CutoverAtomicUtf8 -LiteralPath $JsonPath -Text $json -EvidenceRoot $EvidenceRoot -MaxBytes $maxReportJsonBytes
    Write-CutoverAtomicUtf8 -LiteralPath $TextPath -Text $human -EvidenceRoot $EvidenceRoot -MaxBytes $maxReportHumanBytes
}

try {
    $rootPath = Assert-CutoverAuthorizedRoot -RequestedRoot $Root
    $evidenceRoot = Normalize-CutoverAbsolutePath -LiteralPath (Join-Path $rootPath '.devmanager-next\evidence') -Label 'evidence root'
    $defaultReportPath = Assert-AuditOutputPath -RepositoryRoot $rootPath -EvidenceRoot $evidenceRoot -RequestedPath $null
    if ([string]::IsNullOrEmpty($OutputPath)) {
        $reportPath = $defaultReportPath
    }
    else {
        try {
            $reportPath = Assert-AuditOutputPath -RepositoryRoot $rootPath -EvidenceRoot $evidenceRoot -RequestedPath $OutputPath
        }
        catch {
            Add-GlobalBlocker 'requested output path was rejected; using the confined current-report fallback.'
            $reportPath = $defaultReportPath
        }
    }
    $humanPath = Assert-AuditHumanPath -JsonPath $reportPath -EvidenceRoot $evidenceRoot

    if (-not [string]::IsNullOrEmpty($authorizationFailure)) {
        throw 'authorized root Git identity was not established.'
    }
    Assert-CutoverRootStable
    $trackedFiles = @(Invoke-GitTrackedFiles -RepositoryRoot $rootPath)
    foreach ($tracked in $trackedFiles) {
        Assert-CutoverDeadline
        if ([System.IO.Path]::GetFileName($tracked).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase) -or
            [string]::Equals($tracked, '.devmanager-next/audit-fixture.auth', [System.StringComparison]::Ordinal)) {
            $protectedTrackedFiles.Add($tracked)
        }
    }

    $ledgerPath = Assert-CutoverConfinedPath `
        -LiteralPath (Join-Path $rootPath 'docs\replacement-deletion-ledger.md') `
        -AncestorPath $rootPath
    $contract = Read-CutoverContract -LedgerPath $ledgerPath

    $expectedStatuses = @('HOLD', 'READY', 'DELETED')
    $contractSchemaVersion = Get-ContractProperty -Object $contract -Name 'schemaVersion'
    if ($contractSchemaVersion -ne 1) {
        Add-ContractError "schemaVersion must be 1."
    }
    if ([string](Get-ContractProperty -Object $contract -Name 'ledgerPath') -ne 'docs/replacement-deletion-ledger.md') {
        Add-ContractError 'ledgerPath must be docs/replacement-deletion-ledger.md.'
    }
    $statusModel = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'statusModel') | ForEach-Object { [string]$_ })
    if (($statusModel -join ',') -ne ($expectedStatuses -join ',')) {
        Add-ContractError 'statusModel must be exactly HOLD, READY, DELETED.'
    }

    $policy = Get-ContractProperty -Object $contract -Name 'referencePolicy'
    if ([string](Get-ContractProperty -Object $policy -Name 'trackedUniverse') -ne 'git-ls-files') {
        Add-ContractError 'referencePolicy.trackedUniverse must be git-ls-files.'
    }
    if ([string](Get-ContractProperty -Object $policy -Name 'referenceScanner') -ne 'rg --fixed-strings --line-number') {
        Add-ContractError 'referencePolicy.referenceScanner must name rg fixed-string line scanning.'
    }
    $allowedSelf = @(Get-ContractArray (Get-ContractProperty -Object $policy -Name 'allowedLedgerSelfReferences') | ForEach-Object { [string]$_ })
    if ($allowedSelf.Count -ne 1 -or $allowedSelf[0] -ne 'docs/replacement-deletion-ledger.md') {
        Add-ContractError 'Only docs/replacement-deletion-ledger.md may be an allowed ledger self-reference.'
    }
    $protectedBasenames = @(Get-ContractArray (Get-ContractProperty -Object $policy -Name 'protectedFileBasenames') | ForEach-Object { [string]$_ })
    if (-not ($protectedBasenames -contains 'session.json')) {
        Add-ContractError 'referencePolicy.protectedFileBasenames must contain the exact session.json name.'
    }
    $maxMatches = 20
    $maxMatchesValue = Get-ContractProperty -Object $policy -Name 'maxMatchesPerRow'
    if ($null -ne $maxMatchesValue -and [int]$maxMatchesValue -gt 0) {
        $maxMatches = [Math]::Min([int]$maxMatchesValue, 100)
    }

    $nodeById = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    $nodes = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'prerequisiteNodes'))
    foreach ($node in $nodes) {
        Assert-CutoverDeadline
        if ($nodeReports.Count -ge $maxNodes) { Add-SafetyBound; break }
        $nodeId = [string](Get-ContractProperty -Object $node -Name 'id')
        if ([string]::IsNullOrWhiteSpace($nodeId)) {
            Add-ContractError 'prerequisite node id is missing.'
            continue
        }
        if ($nodeById.ContainsKey($nodeId)) {
            Add-ContractError "duplicate prerequisite node '$nodeId'."
        }
        else {
            $nodeById[$nodeId] = $node
        }
        $nodeKind = [string](Get-ContractProperty -Object $node -Name 'kind')
        if ($nodeKind -notin @('phase', 'gate')) {
            Add-ContractError "prerequisite node '$nodeId' has invalid kind '$nodeKind'."
        }
        $nodeStatus = [string](Get-ContractProperty -Object $node -Name 'status')
        if ($nodeStatus -notin $expectedStatuses) {
            Add-ContractError "prerequisite node '$nodeId' has invalid status '$nodeStatus'."
        }
        $nodeDependencies = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $node -Name 'dependsOn') -Label "node '$nodeId' dependency")
        $nodeEvidence = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $node -Name 'evidence') -Label "node '$nodeId' evidence")
        if ($nodeEvidence.Count -eq 0) {
            Add-ContractError "prerequisite node '$nodeId' has no evidence artifact declaration."
        }
        $nodeOutputId = Get-CutoverSafeReportIdentifier -Value $nodeId
        $nodeOutputDependencies = @($nodeDependencies | ForEach-Object {
                Get-CutoverSafeReportIdentifier -Value $_
            })
        $nodeEvidenceReports = New-Object 'System.Collections.Generic.List[object]'
        foreach ($artifact in $nodeEvidence) {
            $artifactPath = Normalize-ContractRelativePath -Value $artifact -Label "prerequisite node '$nodeId' evidence artifact"
            $artifactPresent = $false
            if ($null -ne $artifactPath) {
                try {
                    $artifactPresent = Test-CutoverConfinedFilePresent -LiteralPath (Join-Path $rootPath ($artifactPath.Replace('/', '\')))
                }
                catch {
                    Add-GlobalBlocker "prerequisite evidence artifact was rejected by filesystem safety: $artifactPath"
                    $artifactPresent = $false
                }
                if (-not $artifactPresent) {
                    Add-GlobalBlocker "prerequisite node '$nodeId' missing evidence artifact: $artifactPath"
                }
            }
            $nodeEvidenceReports.Add([pscustomobject]@{
                path = $artifactPath
                present = $artifactPresent
            })
        }
        $nodeReports.Add([pscustomobject]@{
            id = $nodeOutputId
            kind = $nodeKind
            status = $nodeStatus
            dependsOn = $nodeOutputDependencies
            evidence = @($nodeEvidenceReports.ToArray())
        })
    }
    Assert-NodeGraph -Nodes $nodes -NodeById $nodeById
    foreach ($node in $nodes) {
        Assert-CutoverDeadline
        $nodeId = [string](Get-ContractProperty -Object $node -Name 'id')
        $nodeStatus = [string](Get-ContractProperty -Object $node -Name 'status')
        if ($nodeStatus -ne 'READY' -or -not $nodeById.ContainsKey($nodeId)) {
            continue
        }
        foreach ($dependency in Get-ContractArray (Get-ContractProperty -Object $node -Name 'dependsOn')) {
            Assert-CutoverDeadline
            if ($nodeById.ContainsKey([string]$dependency)) {
                $dependencyStatus = [string](Get-ContractProperty -Object $nodeById[[string]$dependency] -Name 'status')
                if ($dependencyStatus -ne 'READY') {
                    Add-GlobalBlocker "prerequisite node '$nodeId' is READY but dependency is not READY: $dependency (status=$dependencyStatus)"
                }
            }
        }
    }

    $rowById = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    $rowModels = New-Object 'System.Collections.Generic.List[object]'
    $legacyPathOwners = New-Object 'System.Collections.Generic.Dictionary[string,string]' ([System.StringComparer]::Ordinal)
    $legacyPathPhysicalOwners = New-Object 'System.Collections.Generic.Dictionary[string,string]' ([System.StringComparer]::OrdinalIgnoreCase)
    $rows = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'rows'))
    if ($rows.Count -eq 0) {
        Add-ContractError 'rows must contain at least one ledger row.'
    }
    foreach ($row in $rows) {
        Assert-CutoverDeadline
        if ($rowModels.Count -ge $maxRows) { Add-SafetyBound; break }
        $rowId = [string](Get-ContractProperty -Object $row -Name 'id')
        if ([string]::IsNullOrWhiteSpace($rowId)) {
            Add-ContractError 'ledger row id is missing.'
            continue
        }
        if ($rowById.ContainsKey($rowId)) {
            Add-ContractError "duplicate ledger row '$rowId'."
        }
        else {
            $rowById[$rowId] = $row
        }

        $legacy = Get-ContractProperty -Object $row -Name 'legacy'
        $legacyPath = Normalize-ContractRelativePath `
            -Value (Get-ContractProperty -Object $legacy -Name 'path') `
            -Label "row '$rowId' legacy.path"
        $replacement = Get-ContractProperty -Object $row -Name 'replacementOwner'
        $replacementPath = Normalize-ContractRelativePath `
            -Value (Get-ContractProperty -Object $replacement -Name 'path') `
            -Label "row '$rowId' replacementOwner.path"
        if ($null -ne $legacyPath) {
            $legacyKey = $legacyPath
            if ($legacyPathOwners.ContainsKey($legacyKey)) {
                Add-ContractError "duplicate legacy path '$legacyPath' in rows '$($legacyPathOwners[$legacyKey])' and '$rowId'."
            }
            else {
                $legacyPathOwners[$legacyKey] = $rowId
            }
            if ($legacyPathPhysicalOwners.ContainsKey($legacyPath) -and $legacyPathPhysicalOwners[$legacyPath] -cne $legacyPath) {
                Add-ContractError "case-colliding legacy path '$legacyPath' is not an exact tracked spelling."
            }
            elseif (-not $legacyPathPhysicalOwners.ContainsKey($legacyPath)) {
                $legacyPathPhysicalOwners[$legacyPath] = $legacyPath
            }
        }

        $symbols = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $legacy -Name 'symbols') -Label "row '$rowId' symbol")
        if ($symbols.Count -eq 0) {
            Add-ContractError "row '$rowId' legacy.symbols must contain at least one symbol."
        }
        $tokens = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $legacy -Name 'tokens') -Label "row '$rowId' token")
        $prerequisites = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $row -Name 'prerequisites') -Label "row '$rowId' prerequisite")
        if ($prerequisites.Count -eq 0) {
            Add-ContractError "row '$rowId' has no prerequisite phase/gate."
        }
        $evidence = Get-ContractProperty -Object $row -Name 'evidence'
        $commands = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $evidence -Name 'commands') -Label "row '$rowId' evidence command")
        $artifacts = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $evidence -Name 'artifacts') -Label "row '$rowId' evidence artifact")
        if ($commands.Count -eq 0) {
            Add-ContractError "row '$rowId' evidence.commands is empty."
        }
        if ($artifacts.Count -eq 0) {
            Add-ContractError "row '$rowId' evidence.artifacts is empty."
        }
        $status = [string](Get-ContractProperty -Object $row -Name 'status')
        if ($status -notin $expectedStatuses) {
            Add-ContractError "row '$rowId' has invalid status '$status'."
        }
        if ((Get-ContractProperty -Object $row -Name 'approvalRequired') -ne $true) {
            Add-ContractError "row '$rowId' must require explicit approval."
        }
        if ([string]::IsNullOrWhiteSpace([string](Get-ContractProperty -Object $row -Name 'approvalRequirement'))) {
            Add-ContractError "row '$rowId' approvalRequirement is empty."
        }

        $rowModels.Add([pscustomobject]@{
                source         = $row
                id             = $rowId
                reportId       = (Get-CutoverSafeReportIdentifier -Value $rowId)
                legacyPath     = $legacyPath
                symbols        = $symbols
                tokens         = $tokens
                replacementPath = $replacementPath
                prerequisites  = $prerequisites
                commands       = $commands
                artifacts      = $artifacts
                status         = $status
            })
    }

    foreach ($model in $rowModels) {
        Assert-CutoverDeadline
        foreach ($prerequisite in $model.prerequisites) {
            Assert-CutoverDeadline
            if (-not $nodeById.ContainsKey($prerequisite)) {
                Add-ContractError "row '$($model.id)' has unknown prerequisite '$prerequisite'."
            }
        }
    }

    $needles = New-Object 'System.Collections.Generic.List[object]'
    $needleKeys = New-Object 'System.Collections.Generic.Dictionary[string,bool]' ([System.StringComparer]::Ordinal)
    foreach ($model in $rowModels) {
        Assert-CutoverDeadline
        Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'path' -Value $model.legacyPath
        foreach ($symbol in $model.symbols) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'symbol' -Value $symbol
        }
        foreach ($token in $model.tokens) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'token' -Value $token
        }
    }

    $forbidden = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'forbiddenEntrypoints') | Select-Object -First $maxRows)
    if (@(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'forbiddenEntrypoints')).Count -gt $maxRows) {
        Add-SafetyBound
    }
    if ($forbidden.Count -eq 0) {
        Add-ContractError 'forbiddenEntrypoints must contain at least one legacy entrypoint contract.'
    }
    foreach ($entrypoint in $forbidden) {
        Assert-CutoverDeadline
        $entrypointId = [string](Get-ContractProperty -Object $entrypoint -Name 'id')
        if ($entrypointId.Length -gt $maxNeedleChars -or $entrypointId.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
            Add-SafetyBound
            continue
        }
        $entrypointPath = Normalize-ContractRelativePath `
            -Value (Get-ContractProperty -Object $entrypoint -Name 'path') `
            -Label "forbidden entrypoint '$entrypointId' path"
        if ([string]::IsNullOrWhiteSpace($entrypointId) -or $null -eq $entrypointPath) {
            continue
        }
        $entrypointReportId = Get-CutoverSafeReportIdentifier -Value $entrypointId
        $entrypointOwner = "entrypoint:$entrypointReportId"
        Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $entrypointOwner -Kind 'path' -Value $entrypointPath
        foreach ($token in @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $entrypoint -Name 'tokens') -Label "forbidden entrypoint '$entrypointId' token")) {
            Add-Needle `
                -Needles $needles `
                -NeedleKeys $needleKeys `
                -OwnerId $entrypointOwner `
                -Kind 'token' `
                -Value $token `
                -ContextPath $entrypointPath
            foreach ($model in $rowModels) {
                Assert-CutoverDeadline
                if ($model.legacyPath -eq $entrypointPath) {
                    Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'token' -Value $token
                }
            }
        }
        if (Test-TrackedPathPresent -Path $entrypointPath -Tracked $trackedFiles) {
            $entrypointFindings.Add("${entrypointReportId}:$entrypointPath")
        }
    }

    $scanMatches = @(Invoke-ReferenceScan `
        -RepositoryRoot $rootPath `
        -Tracked $trackedFiles `
        -Needles $needles `
        -MaxMatches $maxMatches)
    Assert-CutoverRootStable

    foreach ($match in $scanMatches | Where-Object { $_.ownerId -like 'entrypoint:*' }) {
        Assert-CutoverDeadline
        $entrypointFindings.Add("$($match.ownerId.Substring(11)):$($match.path)")
    }

    foreach ($model in $rowModels) {
        Assert-CutoverDeadline
        $rowBlockers = New-Object 'System.Collections.Generic.List[string]'
        $pathPresent = $false
        if ($null -ne $model.legacyPath) {
            $pathPresent = Test-TrackedPathPresent -Path $model.legacyPath -Tracked $trackedFiles
            if (-not $pathPresent -and $model.status -ne 'DELETED') {
                Add-ContractError "row '$($model.id)' legacy path is not an exact tracked path: $($model.legacyPath)."
            }
        }
        $replacementPresent = $false
        if ($null -ne $model.replacementPath) {
            $replacementPresent = Test-TrackedPathPresent -Path $model.replacementPath -Tracked $trackedFiles
            if (-not $replacementPresent) {
                Add-ContractError "row '$($model.id)' replacement owner path is not an exact tracked path: $($model.replacementPath)."
            }
        }

        $artifactReports = New-Object 'System.Collections.Generic.List[object]'
        foreach ($artifact in $model.artifacts) {
            $artifactPath = Normalize-ContractRelativePath -Value $artifact -Label "row '$($model.id)' evidence artifact"
            $artifactPresent = $false
            if ($null -ne $artifactPath) {
                try {
                    $artifactPresent = Test-CutoverConfinedFilePresent -LiteralPath (Join-Path $rootPath ($artifactPath.Replace('/', '\')))
                }
                catch {
                    Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "evidence artifact rejected by filesystem safety: $artifactPath"
                }
                if (-not $artifactPresent) {
                    Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "missing evidence artifact: $artifactPath"
                }
            }
            $artifactReports.Add([pscustomobject]@{ path = $artifactPath; present = $artifactPresent })
        }

        foreach ($prerequisite in $model.prerequisites) {
            $prerequisiteStatus = $null
            if ($nodeById.ContainsKey($prerequisite)) {
                $prerequisiteStatus = [string](Get-ContractProperty -Object $nodeById[$prerequisite] -Name 'status')
            }
            if ($prerequisiteStatus -ne 'READY') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "prerequisite is not READY: $prerequisite (status=$prerequisiteStatus)"
            }
        }

        $rowMatches = @($scanMatches | Where-Object { $_.ownerId -eq $model.id })
        $references = [ordered]@{}
        foreach ($kind in @('path', 'symbol', 'token')) {
            $referenceSet = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
            foreach ($match in @($rowMatches | Where-Object { $_.kind -eq $kind })) {
                $null = $referenceSet.Add([string]$match.path)
            }
            $references[$kind] = @(Sort-CutoverOrdinalStrings -Values @($referenceSet | ForEach-Object { [string]$_ }))
        }
        if ($model.status -eq 'DELETED') {
            if ($pathPresent) {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "legacy path still present: $($model.legacyPath)"
            }
            foreach ($kind in @('path', 'symbol', 'token')) {
                if (@($references[$kind]).Count -gt 0) {
                    Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "legacy $kind references remain: $(@($references[$kind]) -join ', ')"
                }
            }
        }
        if ($model.status -eq 'READY' -and $rowBlockers.Count -gt 0) {
            Add-GlobalBlocker "row '$($model.id)' is READY but has blockers."
        }

        $rowDocument = [pscustomobject]([ordered]@{
                    id = $model.reportId
                    status = $model.status
                    legacy = [ordered]@{
                        path = $model.legacyPath
                        symbolCount = @($model.symbols).Count
                        tokenCount = @($model.tokens).Count
                        pathPresent = $pathPresent
                    }
                    replacementOwner = [ordered]@{
                        path = $model.replacementPath
                        present = $replacementPresent
                    }
                    prerequisites = @($model.prerequisites | ForEach-Object {
                            Get-CutoverSafeReportIdentifier -Value $_
                        })
                    evidence = [ordered]@{
                        commandCount = @($model.commands).Count
                    artifacts = @($artifactReports.ToArray())
                    }
                    references = $references
                    blockers = @(Sort-CutoverOrdinalStrings -Values @($rowBlockers.ToArray()))
                })
        $rowReports.Add($rowDocument)
        foreach ($blocker in @($rowBlockers)) {
            Add-GlobalBlocker "row '$($model.id)': $blocker"
        }
    }

    $sortedEntrypointFindings = @(Sort-CutoverOrdinalStrings -Values @($entrypointFindings.ToArray()))
    foreach ($finding in $sortedEntrypointFindings) {
        Add-GlobalBlocker "forbidden legacy entrypoint finding: $finding"
    }
    $sortedContractErrors = @(Sort-CutoverOrdinalDiagnostics -Values @($contractErrors.ToArray()))
    $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))

    $allRowsTerminal = $rowReports.Count -gt 0 -and @($rowReports | Where-Object { $_.status -eq 'HOLD' }).Count -eq 0
    $contractStatus = if ($contractErrors.Count -gt 0 -or $globalBlockers.Count -gt 0 -or -not $allRowsTerminal) { 'HOLD' } else { 'READY' }
}
catch {
    $fatalDiagnosticCategory = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
    Add-ContractError -Message "fatal audit error: $fatalDiagnosticCategory" -Category $fatalDiagnosticCategory
    $sortedEntrypointFindings = @(Sort-CutoverOrdinalStrings -Values @($entrypointFindings.ToArray()))
    $sortedContractErrors = @(Sort-CutoverOrdinalDiagnostics -Values @($contractErrors.ToArray()))
    $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))
    if ($null -eq $reportPath) {
        [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    }
    $contractStatus = 'HOLD'
}

if ($null -eq $rootPath -or $null -eq $evidenceRoot -or $null -eq $reportPath -or $null -eq $humanPath) {
    [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    exit 2
}

$report = [pscustomobject]([ordered]@{
        schemaVersion = 1
        contractId = Get-CutoverReportContractId -Value (Get-ContractProperty -Object $contract -Name 'contractId')
        mode = $Mode
        contractStatus = $contractStatus
        ledgerPath = 'docs/replacement-deletion-ledger.md'
        trackedFileCount = @($trackedFiles).Count
        protectedFilesSkipped = @(Sort-CutoverOrdinalStrings -Values @($protectedTrackedFiles.ToArray()))
        contractErrors = @($sortedContractErrors)
        blockers = @($sortedGlobalBlockers)
        entrypointFindings = @($sortedEntrypointFindings)
        prerequisiteNodes = @(Sort-CutoverOrdinalObjects -Values @($nodeReports.ToArray()) -Fields @('id'))
        rows = @(Sort-CutoverOrdinalObjects -Values @($rowReports.ToArray()) -Fields @('id'))
        safety = [ordered]@{
            boundReached = [bool]$safetyBoundReached
            diagnostic = if ($safetyBoundReached) { $safetyDiagnostic } else { $null }
            limits = [ordered]@{
                ledgerBytes = $maxLedgerBytes
                trackedFiles = $maxTrackedFiles
                trackedBytes = $maxTrackedBytes
                rows = $maxRows
                nodes = $maxNodes
                stringsPerRow = $maxStringsPerRow
                needles = $maxNeedles
                scannerFiles = $maxScannerFiles
                scanBytesPerFile = $maxScanBytesPerFile
                scannerOutputBytes = $maxScannerOutputBytes
                scannerDeadlineMilliseconds = $maxScannerDurationMs
                errors = $maxErrorCount
                jsonBytes = $maxReportJsonBytes
                humanBytes = $maxReportHumanBytes
            }
        }
            scanner = [ordered]@{
                trackedUniverse = 'git-ls-files'
                referenceScanner = 'fixed-string-line-scanner'
            allowedLedgerSelfReferences = @('docs/replacement-deletion-ledger.md')
            protectedFileBasenames = @('session.json')
            maxMatchesPerRow = $maxMatches
            maxOutputBytes = $maxScannerOutputBytes
            deadlineMilliseconds = $maxScannerDurationMs
        }
    })

try {
    if ($null -ne $gitIdentity) { Assert-CutoverRootStable }
    Write-AuditReports -Report $report -JsonPath $reportPath -TextPath $humanPath -EvidenceRoot $evidenceRoot -ContractStatus ([ref]$contractStatus)
    Write-Host ("Wrote cutover audit JSON -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $reportPath))
    Write-Host ("Wrote cutover audit report -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $humanPath))
}
catch {
    $fatalDiagnosticCategory = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
    [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    exit 2
}

if ($contractStatus -eq 'READY') {
    exit 0
}
exit 2
