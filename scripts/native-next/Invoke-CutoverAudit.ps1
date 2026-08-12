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

    [string]$OutputPath,

    [string]$RemoteChangeEvidencePath
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

$contractErrors = New-Object 'System.Collections.Generic.List[string]'
$globalBlockers = New-Object 'System.Collections.Generic.List[string]'
$rowReports = New-Object 'System.Collections.Generic.List[object]'
$nodeReports = New-Object 'System.Collections.Generic.List[object]'
$entrypointFindings = New-Object 'System.Collections.Generic.List[string]'
$compatibilityFindings = New-Object 'System.Collections.Generic.List[string]'
$packagingFindings = New-Object 'System.Collections.Generic.List[string]'
$protectedTrackedFiles = New-Object 'System.Collections.Generic.List[string]'
$trackedFiles = @()
$sortedEntrypointFindings = @()
$sortedCompatibilityFindings = @()
$sortedPackagingFindings = @()
$sortedContractErrors = @()
$sortedGlobalBlockers = @()
$productEntrypointReport = New-Object 'System.Collections.Generic.List[object]'
$packagingHandoffReport = $null
$remainingIntegratedPrerequisites = @()
$historicalReferenceAllowlist = New-Object 'System.Collections.Generic.List[string]'
$isolationReport = [ordered]@{
    remappedAppData = $false
    setDevmanagerProfile = $false
    inheritedDevmanagerProfileCleared = $false
    productionRootRead = $false
    evidenceRootBeneathWorktree = $false
}
$installedAppReport = [ordered]@{
    observedInstalledProcesses = $false
    openSessionJson = $false
    hashProductionFiles = $false
    installPublishDeleteUserData = $false
}
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
$maxScannerOutputLines = 4096
$maxScannerOutputLineChars = 32768
$maxScannerDurationMs = $maxAuditDurationMs
$maxErrorCount = 64
$maxReportJsonBytes = [int64]262144
$maxReportHumanBytes = [int64]131072
$maxEnvironmentEntries = 64
$maxEnvironmentEntryChars = 4096
$maxEnvironmentBytes = [int64]32768
$maxEnvironmentPathEntries = 64
$maxEnvironmentPathChars = 1024
$maxLedgerLines = 8192
$maxGitIdentityLines = 8
$maxReportStrings = 16384
$publicationReserveMs = 4000
$processSettlementReserveMs = 3000
$processWorkDeadlineReserveMs = $publicationReserveMs + $processSettlementReserveMs
$workDeadlineReserveMs = $publicationReserveMs
$safetyBoundReached = $false
$safetyDiagnosticEmitted = $false
$safetyDiagnostic = 'audit[safety_bound]'
$maxMatches = $maxMatchesPerOwner
$rootIdentity = $null
$rootDirectoryHandle = $null
$reportDirectoryHandle = $null
$commonDirectoryIdentity = $null
$authorizedRootKind = $null
$candidateRootPath = $null
$gitIdentity = $null
$authorizationFailure = $null
$fatalDiagnosticCategory = 'audit_internal_error'
$boundedPublicationRequired = $false
$remoteChangeAttribution = [pscustomobject]([ordered]@{
        evaluated = $false
        classification = 'not-evaluated'
        writer = 'not-evaluated'
        changedCategories = @()
    })

# Child process environment is deliberately assembled from a fixed allowlist.
# These limits are enforced before the C# environment block is materialized;
# the C# layer repeats the aggregate check at the final allocation boundary.
$allowedFixtureEnvironmentNames = @(
    'GIT_FAKE_MODE', 'GIT_REAL', 'GIT_CHILD_SENTINEL', 'GIT_PROBE_LOG',
    'GIT_FAKE_ROOT', 'GIT_FAKE_MOVED_ROOT', 'GIT_FAKE_SWAP_LOG',
    'RG_FAKE_MODE', 'RG_FAKE_TARGET', 'RG_FAKE_RESIDUE', 'RG_FAKE_RESIDUE_PID',
    'RG_FAKE_OUTSIDE', 'RG_SHIM_LOG', 'RG_REAL'
)

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

$script:recognizedDiagnosticCategories = @(
    'report_parent_changed',
    'report_temp_invalid',
    'report_replacement_invalid',
    'handle_escape',
    'filesystem_identity_unavailable',
    'root_identity_changed',
    'remote_change_protected',
    'remote_change_unattributed',
    'unsupported_runtime',
    'unverified',
    'production_profile',
    'root_unauthorized',
    'git_identity_invalid',
    'protected_filename',
    'output_path_rejected',
    'path_hardlink_rejected',
    'path_reparse_rejected',
    'file_identity_changed',
    'contract_invalid',
    'process_stdout_overflow',
    'process_stderr_overflow',
    'process_error',
    'process_nonzero',
    'process_deadline_exceeded',
    'scanner_failed',
    'git_enumeration_failed',
    'evidence_invalid',
    'prerequisite_invalid',
    'ledger_invalid',
    'safety_bound',
    'audit_internal_error'
)

function Get-CutoverDiagnosticCategory {
    param([AllowEmptyString()][string]$Message)

    $text = ([string]$Message).ToLowerInvariant()
    if ($text -match '^audit\[([a-z0-9_]+)\](?:;path=.*)?$') {
        $category = [string]$Matches[1]
        if ($script:recognizedDiagnosticCategories -contains $category) {
            return $category
        }
        return 'audit_internal_error'
    }
    if ($text.Contains('report parent')) { return 'report_parent_changed' }
    if ($text.Contains('relative report')) { return 'report_temp_invalid' }
    if ($text.Contains('temporary report')) { return 'report_temp_invalid' }
    if ($text.Contains('report replacement')) { return 'report_replacement_invalid' }
    if ($text.Contains('opened handle escaped')) { return 'handle_escape' }
    if ($text.Contains('stable windows filesystem identity')) { return 'filesystem_identity_unavailable' }
    if ($text.Contains('repository root changed')) { return 'root_identity_changed' }
    if ($text.Contains('repository root retained handle changed')) { return 'root_identity_changed' }
    if ($text.Contains('remote change protected')) { return 'remote_change_protected' }
    if ($text.Contains('remote change unattributed')) { return 'remote_change_unattributed' }
    if ($text.Contains('unsupported_runtime') -or $text.Contains('powershell 7')) { return 'unsupported_runtime' }
    if ($text.Contains('unverified') -or $text.Contains('not bound')) { return 'unverified' }
    if ($text.Contains('production profile') -or $text.Contains('devmanager_profile')) { return 'production_profile' }
    if ($text.Contains('unauthorized') -or $text.Contains('authenticated fixture')) { return 'root_unauthorized' }
    if ($text.Contains('git identity') -or $text.Contains('git worktree identity') -or $text.Contains('git common directory') -or $text.Contains('git repository identity')) { return 'git_identity_invalid' }
    if ($text.Contains('session.json')) { return 'protected_filename' }
    if ($text.Contains('output path') -or $text.Contains('report path')) { return 'output_path_rejected' }
    if ($text.Contains('hard link') -or $text.Contains('hardlink')) { return 'path_hardlink_rejected' }
    if ($text.Contains('reparse') -or $text.Contains('junction') -or $text.Contains('symlink')) { return 'path_reparse_rejected' }
    if ($text.Contains('relative evidence directory')) { return 'report_parent_changed' }
    if ($text.Contains('publication handle') -or $text.Contains('parenthandle')) { return 'report_parent_changed' }
    if ($text.Contains('opened-handle identity changed') -or $text.Contains('opened-handle content changed') -or $text.Contains('pathname identity changed') -or $text.Contains('content changed during') -or $text.Contains('handle identity changed')) { return 'file_identity_changed' }
    if ($text.Contains('legacy path') -or $text.Contains('deletion set') -or $text.Contains('duplicate ledger')) { return 'contract_invalid' }
    if ($text.Contains('stdout') -and $text.Contains('overflow')) { return 'process_stdout_overflow' }
    if ($text.Contains('stderr') -and $text.Contains('overflow')) { return 'process_stderr_overflow' }
    if ($text.Contains('stdout-overflow')) { return 'process_stdout_overflow' }
    if ($text.Contains('stderr-overflow')) { return 'process_stderr_overflow' }
    if ($text.Contains('process-error') -or $text.Contains('process-resolve') -or $text.Contains('process-create')) { return 'process_error' }
    if ($text.Contains('nonzero') -or $text.Contains('exit code')) { return 'process_nonzero' }
    if ($text.Contains('timeout') -or $text.Contains('deadline')) { return 'process_deadline_exceeded' }
    if ($text.Contains('rg.exe') -or $text.Contains('ripgrep') -or $text.Contains('reference scan') -or $text.Contains('scanner')) { return 'scanner_failed' }
    if ($text.Contains('git enumeration') -or $text.Contains('git ls-files') -or $text.Contains('git path')) { return 'git_enumeration_failed' }
    if ($text.Contains('evidence artifact') -or $text.Contains('missing evidence')) { return 'evidence_invalid' }
    if ($text.Contains('prerequisite') -or $text.Contains('dependency is not ready')) { return 'prerequisite_invalid' }
    if ($text.Contains('ledger')) { return 'ledger_invalid' }
    return 'audit_internal_error'
}

function Copy-CutoverRedactedBlockers {
    param(
        [AllowEmptyCollection()][System.Collections.IEnumerable]$Blockers
    )

    foreach ($blocker in @($Blockers)) {
        Assert-CutoverWorkDeadline
        $token = [string]$blocker
        if ([string]::IsNullOrWhiteSpace($token)) {
            continue
        }
        if ($globalBlockers.Count -ge $maxErrorCount) {
            Add-SafetyBound
            return
        }
        if ($token -match '^audit\[([a-z0-9_]+)\](?:;path=.*)?$' -and
            $script:recognizedDiagnosticCategories -contains [string]$Matches[1]) {
            if (-not $globalBlockers.Contains($token)) {
                $globalBlockers.Add($token)
            }
            continue
        }
        Add-GlobalBlocker -Message $token
    }
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

function Assert-CutoverAuthorizedRootIdentityStable {
    if ($null -eq $rootPath -or $null -eq $rootIdentity -or $null -eq $rootDirectoryHandle) {
        throw 'authorized repository root identity was not established.'
    }
    $retained = Get-CutoverHandleIdentity -Stream $rootDirectoryHandle.stream
    if (-not (Compare-CutoverDirectoryIdentity -Before $rootIdentity -After $retained)) {
        throw 'repository root retained handle changed during the audit.'
    }
    $current = Get-CutoverPathIdentity -LiteralPath $rootPath -AllowDirectory
    if (-not (Compare-CutoverDirectoryIdentity -Before $rootIdentity -After $current)) {
        throw 'repository root changed during the audit.'
    }
}

function Assert-CutoverRootStable {
    Assert-CutoverAuthorizedRootIdentityStable
    if ($null -eq $gitIdentity -or $null -eq $commonDirectoryIdentity) {
        throw 'Git repository identity was not established.'
    }
    $currentCommon = Get-CutoverPathIdentity -LiteralPath $gitIdentity.commonDirectory -AllowDirectory
    if (-not (Compare-CutoverDirectoryIdentity -Before $commonDirectoryIdentity -After $currentCommon)) {
        throw 'Git common directory changed during the audit.'
    }
}

function Assert-CutoverPublicationAuthority {
    param(
        [Parameter(Mandatory = $true)][object]$ParentHandle,
        [Parameter(Mandatory = $true)][string]$ExpectedParentPath
    )

    # Surround the retained-parent check with root pathname revalidation. If
    # the authorized root is renamed or replaced, publication cannot bind to a
    # newly opened replacement tree. Once validated, every mutation is made
    # relative to this retained parent handle.
    Assert-CutoverAuthorizedRootIdentityStable
    $parentAfter = Get-CutoverHandleIdentity -Stream $ParentHandle.stream
    if (-not (Compare-CutoverDirectoryIdentity -Before $ParentHandle.identity -After $parentAfter) -or
        -not [string]::Equals(
            [string]$parentAfter.finalPath,
            [string]$ExpectedParentPath,
            [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'report parent no longer belongs to the authorized repository root.'
    }
    Assert-CutoverAuthorizedRootIdentityStable
}

function Assert-CutoverRelativePath {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label,
        [switch]$AllowDirectory
    )

    if ($Value -isnot [string] -or [string]::IsNullOrEmpty([string]$Value)) {
        Add-ContractError "${Label} is missing or empty."
        return $null
    }

    $raw = [string]$Value
    $directoryOwner = $false
    if ($AllowDirectory -and $raw.EndsWith('/') -and -not $raw.EndsWith('//')) {
        $directoryOwner = $true
        $raw = $raw.Substring(0, $raw.Length - 1)
    }
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
    if ($directoryOwner) {
        return "$raw/"
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
    // NtCreateFile CreateDisposition values are not Win32 CREATE_* values.
    // FILE_OPEN_IF (3) atomically opens an existing relative directory or
    // creates it beneath the retained parent handle when it is absent.
    private const uint FileOpenIf = 3;

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

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool SetFileInformationByHandle(
        IntPtr file,
        int fileInformationClass,
        IntPtr fileInformation,
        uint bufferSize);

    [StructLayout(LayoutKind.Sequential)]
    private struct IoStatusBlock
    {
        public IntPtr Status;
        public IntPtr Information;
    }

    [DllImport("ntdll.dll")]
    private static extern int NtSetInformationFile(
        IntPtr file,
        out IoStatusBlock status,
        IntPtr fileInformation,
        uint length,
        int informationClass);

    [StructLayout(LayoutKind.Sequential)]
    private struct UnicodeString
    {
        public ushort Length;
        public ushort MaximumLength;
        public IntPtr Buffer;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ObjectAttributes
    {
        public int Length;
        public IntPtr RootDirectory;
        public IntPtr ObjectName;
        public uint Attributes;
        public IntPtr SecurityDescriptor;
        public IntPtr SecurityQualityOfService;
    }

    [DllImport("ntdll.dll")]
    private static extern int NtCreateFile(
        out IntPtr fileHandle,
        uint desiredAccess,
        ref ObjectAttributes objectAttributes,
        out IoStatusBlock ioStatusBlock,
        IntPtr allocationSize,
        uint fileAttributes,
        uint shareAccess,
        uint createDisposition,
        uint createOptions,
        IntPtr eaBuffer,
        uint eaLength);

    public static int CreateRelativeWriteFile(
        IntPtr parentDirectory,
        string leafName,
        out IntPtr fileHandle)
    {
        fileHandle = IntPtr.Zero;
        if (parentDirectory == IntPtr.Zero || String.IsNullOrEmpty(leafName) ||
            leafName.IndexOf('\\') >= 0 || leafName.IndexOf('/') >= 0 ||
            leafName.IndexOf('\0') >= 0 || leafName == "." || leafName == "..")
        {
            return unchecked((int)0xC000000D);
        }
        var nameBuffer = Marshal.StringToHGlobalUni(leafName);
        var name = new UnicodeString
        {
            Length = checked((ushort)(leafName.Length * 2)),
            MaximumLength = checked((ushort)((leafName.Length + 1) * 2)),
            Buffer = nameBuffer
        };
        var namePointer = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UnicodeString)));
        try
        {
            Marshal.StructureToPtr(name, namePointer, false);
            var attributes = new ObjectAttributes
            {
                Length = Marshal.SizeOf(typeof(ObjectAttributes)),
                RootDirectory = parentDirectory,
                ObjectName = namePointer,
                Attributes = 0x40,
                SecurityDescriptor = IntPtr.Zero,
                SecurityQualityOfService = IntPtr.Zero
            };
            IoStatusBlock status;
            return NtCreateFile(
                out fileHandle,
                0x00130196,
                ref attributes,
                out status,
                IntPtr.Zero,
                0x00000080,
                0x00000007,
                2,
                0x00200062,
                IntPtr.Zero,
                0);
        }
        finally
        {
            Marshal.FreeHGlobal(namePointer);
            Marshal.FreeHGlobal(nameBuffer);
        }
    }

    public static int CreateOrOpenRelativeDirectory(
        IntPtr parentDirectory,
        string leafName,
        out IntPtr directoryHandle)
    {
        directoryHandle = IntPtr.Zero;
        if (parentDirectory == IntPtr.Zero || String.IsNullOrEmpty(leafName) ||
            leafName.IndexOf('\\') >= 0 || leafName.IndexOf('/') >= 0 ||
            leafName.IndexOf('\0') >= 0 || leafName == "." || leafName == "..")
        {
            return unchecked((int)0xC000000D);
        }
        var nameBuffer = Marshal.StringToHGlobalUni(leafName);
        var name = new UnicodeString
        {
            Length = checked((ushort)(leafName.Length * 2)),
            MaximumLength = checked((ushort)((leafName.Length + 1) * 2)),
            Buffer = nameBuffer
        };
        var namePointer = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(UnicodeString)));
        try
        {
            Marshal.StructureToPtr(name, namePointer, false);
            var attributes = new ObjectAttributes
            {
                Length = Marshal.SizeOf(typeof(ObjectAttributes)),
                RootDirectory = parentDirectory,
                ObjectName = namePointer,
                Attributes = 0x40,
                SecurityDescriptor = IntPtr.Zero,
                SecurityQualityOfService = IntPtr.Zero
            };
            IoStatusBlock status;
            return NtCreateFile(
                out directoryHandle,
                0x001201BF,
                ref attributes,
                out status,
                IntPtr.Zero,
                0x00000010,
                0x00000003,
                FileOpenIf,
                0x00200021,
                IntPtr.Zero,
                0);
        }
        finally
        {
            Marshal.FreeHGlobal(namePointer);
            Marshal.FreeHGlobal(nameBuffer);
        }
    }

    // FILE_RENAME_INFO is supplied to NtSetInformationFile with a verified
    // RootDirectory handle; no destination pathname is followed.
    public static int RenameRelativeToHandle(
        IntPtr file,
        IntPtr parentDirectory,
        string leafName,
        bool replaceExisting)
    {
        if (file == IntPtr.Zero || parentDirectory == IntPtr.Zero ||
            String.IsNullOrEmpty(leafName) || leafName.IndexOf('\\') >= 0 ||
            leafName.IndexOf('/') >= 0 || leafName.IndexOf('\0') >= 0)
        {
            return 87;
        }
        var nameBytes = Encoding.Unicode.GetBytes(leafName);
        var headerSize = IntPtr.Size == 8 ? 24 : 16;
        var size = checked(headerSize + nameBytes.Length);
        var memory = Marshal.AllocHGlobal(size);
        try
        {
            for (var index = 0; index < size; index++) Marshal.WriteByte(memory, index, 0);
            Marshal.WriteInt32(memory, replaceExisting ? 1 : 0);
            Marshal.WriteIntPtr(memory, 8, parentDirectory);
            Marshal.WriteInt32(memory, IntPtr.Size == 8 ? 16 : 8, nameBytes.Length);
            var nameOffset = IntPtr.Size == 8 ? 20 : 12;
            Marshal.Copy(nameBytes, 0, IntPtr.Add(memory, nameOffset), nameBytes.Length);
            IoStatusBlock status;
            return NtSetInformationFile(file, out status, memory, (uint)size, 10);
        }
        finally { Marshal.FreeHGlobal(memory); }
    }

    public static bool DeleteByHandle(IntPtr file)
    {
        if (file == IntPtr.Zero) return false;
        var memory = Marshal.AllocHGlobal(1);
        try
        {
            Marshal.WriteByte(memory, 0, 1);
            return SetFileInformationByHandle(file, 4, memory, 1);
        }
        finally { Marshal.FreeHGlobal(memory); }
    }

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

function Rename-CutoverFileRelative {
    param(
        [Parameter(Mandatory = $true)][System.IO.FileStream]$FileStream,
        [Parameter(Mandatory = $true)][System.IO.FileStream]$ParentStream,
        [Parameter(Mandatory = $true)][string]$LeafName,
        [switch]$ReplaceExisting
    )

    Assert-CutoverDeadline
    $renameError = [CutoverNativeMethods]::RenameRelativeToHandle(
            $FileStream.SafeFileHandle.DangerousGetHandle(),
            $ParentStream.SafeFileHandle.DangerousGetHandle(),
            $LeafName,
            [bool]$ReplaceExisting)
    if ($renameError -ne 0) {
        throw 'relative report replacement failed.'
    }
}

function Remove-CutoverFileByHandle {
    param([Parameter(Mandatory = $true)][System.IO.FileStream]$FileStream)

    try {
        [CutoverNativeMethods]::DeleteByHandle($FileStream.SafeFileHandle.DangerousGetHandle()) | Out-Null
    }
    catch { }
}

function Open-CutoverConfinedFile {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [string]$AuthorizedRoot,
        [switch]$AllowDirectory,
        [switch]$AllowDirectoryWrite,
        [switch]$ReadOnlyShare,
        [switch]$DenyDeleteShare
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
    $shareMode = if ($ReadOnlyShare) {
        0x00000001
    }
    elseif ($DenyDeleteShare) {
        0x00000001 -bor 0x00000002
    }
    else {
        0x00000001 -bor 0x00000002 -bor 0x00000004
    }
    $flags = 0x00200000 -bor 0x02000000 # OPEN_REPARSE_POINT | BACKUP_SEMANTICS
    try {
        $desiredAccess = [uint32]2147483648
        if ($AllowDirectory) {
            # GENERIC_EXECUTE maps to FILE_TRAVERSE on directories. Relative
            # NtCreateFile opens of .devmanager-next/evidence require that
            # right on the retained root handle; Modify-only worktree ACLs
            # do not invent it when the handle was opened read/write only.
            $desiredAccess = [uint32]($desiredAccess -bor 536870912)
        }
        if ($AllowDirectoryWrite) {
            $desiredAccess = [uint32]($desiredAccess -bor 1073741824)
            if (-not $DenyDeleteShare) {
                $desiredAccess = [uint32]($desiredAccess -bor 65536)
            }
        }
        $rawHandle = [CutoverNativeMethods]::CreateFileW(
            $full,
            $desiredAccess,
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

function Compare-CutoverDirectoryIdentity {
    param(
        [Parameter(Mandatory = $true)][object]$Before,
        [Parameter(Mandatory = $true)][object]$After
    )

    # Directory link metadata may change when this audit safely creates its
    # evidence hierarchy. The stable directory identity is volume + file ID +
    # directory type + final path; child-count metadata is not identity.
    return $Before.volume -eq $After.volume -and
        $Before.index -eq $After.index -and
        (($Before.attributes -band 0x10) -ne 0) -and
        (($After.attributes -band 0x10) -ne 0) -and
        [string]::Equals([string]$Before.finalPath, [string]$After.finalPath, [System.StringComparison]::OrdinalIgnoreCase)
}

function Compare-CutoverStableFileIdentity {
    param(
        [Parameter(Mandatory = $true)][object]$Before,
        [Parameter(Mandatory = $true)][object]$After
    )

    # File length is intentionally excluded: a newly-created report grows
    # during the bounded write, while volume/index/link/path are immutable
    # identity fields for the retained handle.
    return $Before.volume -eq $After.volume -and
        $Before.index -eq $After.index -and
        $Before.links -eq $After.links -and
        [string]::Equals([string]$Before.finalPath, [string]$After.finalPath, [System.StringComparison]::OrdinalIgnoreCase)
}

function Open-CutoverRelativeWriteFile {
    param(
        [Parameter(Mandatory = $true)][System.IO.FileStream]$ParentStream,
        [Parameter(Mandatory = $true)][string]$ParentPath,
        [Parameter(Mandatory = $true)][string]$LeafName,
        [Parameter(Mandatory = $true)][string]$AuthorizedRoot
    )

    Assert-CutoverDeadline
    if ([string]::IsNullOrWhiteSpace($LeafName) -or
        $LeafName -match '[\\/:\x00-\x1F\x7F]' -or
        $LeafName -in @('.', '..')) {
        throw 'relative report leaf name was rejected.'
    }
    $expectedPath = Normalize-CutoverAbsolutePath `
        -LiteralPath (Join-Path $ParentPath $LeafName) `
        -Label 'relative report path'
    $authorized = Normalize-CutoverAbsolutePath -LiteralPath $AuthorizedRoot -Label 'authorized root'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $expectedPath -Ancestor $authorized)) {
        throw 'relative report path escaped its authorized root.'
    }
    Initialize-CutoverNativeMethods
    $rawHandle = [IntPtr]::Zero
    $status = [CutoverNativeMethods]::CreateRelativeWriteFile(
        $ParentStream.SafeFileHandle.DangerousGetHandle(),
        $LeafName,
        [ref]$rawHandle)
    if ($status -lt 0 -or $rawHandle -eq [IntPtr]::Zero -or $rawHandle -eq [IntPtr](-1)) {
        if ($rawHandle -ne [IntPtr]::Zero -and $rawHandle -ne [IntPtr](-1)) {
            [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true).Dispose()
        }
        throw 'relative confined write handle open failed.'
    }
    $safeHandle = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true)
    $stream = $null
    try {
        $stream = [System.IO.FileStream]::new($safeHandle, [System.IO.FileAccess]::Write, 8192, $false)
        $identity = Get-CutoverHandleIdentity -Stream $stream
        if (-not [string]::Equals(
                [string]$identity.finalPath,
                [string]$expectedPath,
                [System.StringComparison]::OrdinalIgnoreCase)) {
            throw 'relative report handle identity did not match its retained parent.'
        }
        if (($identity.attributes -band 0x400) -ne 0 -or $identity.length -lt 0 -or $identity.links -gt 1) {
            throw 'relative report handle identity was unsafe.'
        }
        return [pscustomobject]@{ path = $expectedPath; stream = $stream; identity = $identity }
    }
    catch {
        try {
            $handle = if ($null -ne $stream) {
                $stream.SafeFileHandle.DangerousGetHandle()
            }
            else { $safeHandle.DangerousGetHandle() }
            [CutoverNativeMethods]::DeleteByHandle($handle) | Out-Null
        }
        catch { }
        if ($null -ne $stream) { $stream.Dispose() } else { $safeHandle.Dispose() }
        throw
    }
}

function Open-CutoverRelativeDirectory {
    param(
        [Parameter(Mandatory = $true)][System.IO.FileStream]$ParentStream,
        [Parameter(Mandatory = $true)][string]$ParentPath,
        [Parameter(Mandatory = $true)][string]$LeafName,
        [Parameter(Mandatory = $true)][string]$AuthorizedRoot
    )

    Assert-CutoverDeadline
    if ([string]::IsNullOrWhiteSpace($LeafName) -or
        $LeafName -match '[\\/:\x00-\x1F\x7F]' -or
        $LeafName -in @('.', '..') -or
        $LeafName.EndsWith('.') -or
        $LeafName.EndsWith(' ')) {
        throw 'relative evidence directory leaf name was rejected.'
    }
    $expectedPath = Normalize-CutoverAbsolutePath `
        -LiteralPath (Join-Path $ParentPath $LeafName) `
        -Label 'relative evidence directory'
    $authorized = Normalize-CutoverAbsolutePath -LiteralPath $AuthorizedRoot -Label 'authorized root'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $expectedPath -Ancestor $authorized)) {
        throw 'relative evidence directory escaped its authorized root.'
    }

    Initialize-CutoverNativeMethods
    $rawHandle = [IntPtr]::Zero
    $status = [CutoverNativeMethods]::CreateOrOpenRelativeDirectory(
        $ParentStream.SafeFileHandle.DangerousGetHandle(),
        $LeafName,
        [ref]$rawHandle)
    if ($status -lt 0 -or $rawHandle -eq [IntPtr]::Zero -or $rawHandle -eq [IntPtr](-1)) {
        if ($rawHandle -ne [IntPtr]::Zero -and $rawHandle -ne [IntPtr](-1)) {
            [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true).Dispose()
        }
        throw ("relative evidence directory open failed (status={0})." -f $status)
    }

    $safeHandle = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($rawHandle, $true)
    $stream = $null
    try {
        $stream = [System.IO.FileStream]::new(
            $safeHandle,
            [System.IO.FileAccess]::ReadWrite,
            8192,
            $false)
        $identity = Get-CutoverHandleIdentity -Stream $stream
        if (-not [string]::Equals(
                [string]$identity.finalPath,
                [string]$expectedPath,
                [System.StringComparison]::OrdinalIgnoreCase)) {
            throw 'relative evidence directory identity did not match its retained parent.'
        }
        if (($identity.attributes -band 0x10) -eq 0 -or
            ($identity.attributes -band 0x400) -ne 0) {
            throw 'relative evidence directory identity was unsafe.'
        }
        return [pscustomobject]@{
            path = $expectedPath
            stream = $stream
            identity = $identity
        }
    }
    catch {
        if ($null -ne $stream) { $stream.Dispose() } else { $safeHandle.Dispose() }
        throw
    }
}

function Open-CutoverRelativeDirectoryChain {
    param(
        [Parameter(Mandatory = $true)][object]$RootHandle,
        [Parameter(Mandatory = $true)][string]$RootPath,
        [Parameter(Mandatory = $true)][string]$LiteralPath
    )

    Assert-CutoverDeadline
    $normalizedRoot = Normalize-CutoverAbsolutePath -LiteralPath $RootPath -Label 'authorized root'
    $target = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'evidence directory'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $target -Ancestor $normalizedRoot) -or
        $target.Equals($normalizedRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'evidence directory must be strictly beneath the authorized root.'
    }
    $relative = [System.IO.Path]::GetRelativePath($normalizedRoot, $target)
    $parts = @($relative.Split(
            [char[]]@('\', '/'),
            [System.StringSplitOptions]::RemoveEmptyEntries))
    if ($parts.Count -eq 0) {
        throw 'evidence directory chain was empty.'
    }

    $handles = New-Object 'System.Collections.Generic.List[object]'
    $parentStream = $RootHandle.stream
    $parentPath = $normalizedRoot
    try {
        foreach ($part in $parts) {
            Assert-CutoverDeadline
            $opened = Open-CutoverRelativeDirectory `
                -ParentStream $parentStream `
                -ParentPath $parentPath `
                -LeafName ([string]$part) `
                -AuthorizedRoot $normalizedRoot
            $handles.Add($opened)
            $parentStream = $opened.stream
            $parentPath = $opened.path
        }
        $final = $handles[$handles.Count - 1]
        return [pscustomobject]@{
            path = $final.path
            stream = $final.stream
            identity = $final.identity
            chainHandles = @($handles.ToArray())
        }
    }
    catch {
        for ($index = $handles.Count - 1; $index -ge 0; $index--) {
            try { $handles[$index].stream.Dispose() } catch { }
        }
        throw
    }
}

function Close-CutoverPublicationHandles {
    if ($null -ne $script:reportDirectoryHandle) {
        $chain = @($script:reportDirectoryHandle.chainHandles)
        for ($index = $chain.Count - 1; $index -ge 0; $index--) {
            try { $chain[$index].stream.Dispose() } catch { }
        }
        $script:reportDirectoryHandle = $null
    }
    if ($null -ne $script:rootDirectoryHandle) {
        try { $script:rootDirectoryHandle.stream.Dispose() } catch { }
        $script:rootDirectoryHandle = $null
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

function Assert-CutoverWorkDeadline {
    if ((Get-CutoverDeadlineRemainingMilliseconds) -le $workDeadlineReserveMs) {
        Add-SafetyBound
        throw 'audit work deadline reached its bounded publication reserve.'
    }
}

function Assert-CutoverProcessStartDeadline {
    if ((Get-CutoverDeadlineRemainingMilliseconds) -le $processWorkDeadlineReserveMs) {
        Add-SafetyBound
        throw 'audit process start reached its bounded settlement and publication reserve.'
    }
}

function Read-CutoverStreamBytes {
    param(
        [Parameter(Mandatory = $true)][System.IO.Stream]$Stream,
        [Parameter(Mandatory = $true)][int64]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($MaxBytes -le 0 -or $MaxBytes -gt [int32]::MaxValue) {
        throw "${Label} byte limit cannot be represented safely."
    }
    Assert-CutoverWorkDeadline
    $bytes = [System.IO.MemoryStream]::new()
    $buffer = New-Object byte[] 8192
    try {
        while ($true) {
            Assert-CutoverWorkDeadline
            $remainingAllowed = $MaxBytes - $bytes.Length
            $requestBytes = [int][Math]::Min([int64]$buffer.Length, $remainingAllowed + 1)
            if ($requestBytes -le 0) {
                Add-SafetyBound
                throw "${Label} exceeds the bounded input byte limit."
            }
            $task = $Stream.ReadAsync($buffer, 0, $requestBytes)
            $waitMs = (Get-CutoverDeadlineRemainingMilliseconds) - $workDeadlineReserveMs
            if ($waitMs -le 0 -or -not $task.Wait($waitMs)) {
                Add-SafetyBound
                throw 'audit work deadline reached while reading a file.'
            }
            $read = $task.Result
            if ($read -le 0) { break }
            if ($read -gt $remainingAllowed) {
                Add-SafetyBound
                throw "${Label} exceeds the bounded input byte limit."
            }
            # MemoryStream performs one bounded block copy. The prior check is
            # deliberately before accumulation, so MaxBytes + 1 is observed
            # but never retained in the audit's in-memory input.
            $bytes.Write($buffer, 0, $read)
        }
        return ,([byte[]]$bytes.ToArray())
    }
    finally { $bytes.Dispose() }
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

function ConvertTo-CutoverCanonicalJson {
    param(
        [AllowNull()][object]$Value,
        [string]$Path = '$',
        [Parameter(Mandatory = $true)][hashtable]$State,
        [switch]$IgnoreBrowserActivityFields
    )

    Assert-CutoverWorkDeadline
    $State.nodes++
    if ($State.nodes -gt 8192 -or $Path.Length -gt 1024) {
        throw 'remote change evidence exceeded its bounded semantic shape.'
    }
    if ($null -eq $Value) { return 'null' }

    if ($Value -is [System.Array] -or
        ($Value -is [System.Collections.IList] -and $Value -isnot [string])) {
        $parts = New-Object 'System.Collections.Generic.List[string]'
        $values = @($Value)
        if ($values.Count -gt 1024) {
            throw 'remote change evidence array exceeded its bounded count.'
        }
        for ($index = 0; $index -lt $values.Count; $index++) {
            $parts.Add((ConvertTo-CutoverCanonicalJson `
                        -Value $values[$index] `
                        -Path "$Path[$index]" `
                        -State $State `
                        -IgnoreBrowserActivityFields:$IgnoreBrowserActivityFields))
        }
        return '[' + ($parts -join ',') + ']'
    }

    if ($Value -is [pscustomobject] -or $Value -is [System.Collections.IDictionary]) {
        $properties = if ($Value -is [System.Collections.IDictionary]) {
            @($Value.Keys | ForEach-Object {
                    [pscustomobject]@{ Name = [string]$_; Value = $Value[$_] }
                })
        }
        else { @($Value.PSObject.Properties) }
        if ($properties.Count -gt 256) {
            throw 'remote change evidence object exceeded its bounded property count.'
        }
        $names = @($properties | ForEach-Object { [string]$_.Name })
        [Array]::Sort($names, [System.StringComparer]::Ordinal)
        $parts = New-Object 'System.Collections.Generic.List[string]'
        foreach ($name in $names) {
            $skip = $false
            if ($IgnoreBrowserActivityFields) {
                if ($Path -eq '$.host.web' -and $name -eq 'activityLog') {
                    $skip = $true
                }
                elseif ($Path -match '^\$\.host\.web\.pairedClients\[[0-9]+\]$' -and
                    $name -in @('lastSeenEpochMs', 'lastSeenIp')) {
                    $skip = $true
                }
            }
            if ($skip) { continue }
            $property = @($properties | Where-Object { [string]$_.Name -ceq $name })
            if ($property.Count -ne 1) {
                throw 'remote change evidence contained duplicate properties.'
            }
            $encodedName = [string]($name | ConvertTo-Json -Compress)
            $encodedValue = ConvertTo-CutoverCanonicalJson `
                -Value $property[0].Value `
                -Path "$Path.$name" `
                -State $State `
                -IgnoreBrowserActivityFields:$IgnoreBrowserActivityFields
            $parts.Add("${encodedName}:${encodedValue}")
        }
        return '{' + ($parts -join ',') + '}'
    }

    if ($Value -is [string] -or $Value -is [bool] -or
        $Value -is [byte] -or $Value -is [sbyte] -or
        $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or
        $Value -is [int64] -or $Value -is [uint64] -or
        $Value -is [single] -or $Value -is [double] -or $Value -is [decimal]) {
        return [string]($Value | ConvertTo-Json -Compress)
    }
    throw 'remote change evidence contained an unsupported JSON value.'
}

function Test-CutoverCanonicalJsonEqual {
    param(
        [AllowNull()][object]$Left,
        [AllowNull()][object]$Right,
        [switch]$IgnoreBrowserActivityFields
    )

    $leftState = @{ nodes = 0 }
    $rightState = @{ nodes = 0 }
    $leftJson = ConvertTo-CutoverCanonicalJson `
        -Value $Left `
        -State $leftState `
        -IgnoreBrowserActivityFields:$IgnoreBrowserActivityFields
    $rightJson = ConvertTo-CutoverCanonicalJson `
        -Value $Right `
        -State $rightState `
        -IgnoreBrowserActivityFields:$IgnoreBrowserActivityFields
    return [string]::Equals($leftJson, $rightJson, [System.StringComparison]::Ordinal)
}

function Test-CutoverBrowserActivityEvent {
    param([AllowNull()][object]$Event)

    if ($null -eq $Event -or $Event -isnot [pscustomobject]) { return $false }
    $eventProperties = @($Event.PSObject.Properties | ForEach-Object { [string]$_.Name })
    $allowedEventProperties = @(
        'clientId', 'source', 'eventKind', 'label', 'ipAddress', 'eventAtEpochMs',
        'browserFamily', 'browserVersion', 'osFamily', 'deviceClass'
    )
    return @($eventProperties | Where-Object { $_ -notin $allowedEventProperties }).Count -eq 0 -and
        [string](Get-ContractProperty -Object $Event -Name 'source') -eq 'browser' -and
        [string](Get-ContractProperty -Object $Event -Name 'eventKind') -in @('paired', 'connected', 'reconnected')
}

function Get-CutoverRemoteChangeAttribution {
    param([Parameter(Mandatory = $true)][string]$EvidencePath)

    # This is an offline, read-only semantic classifier for generated audit
    # evidence. It is intentionally unavailable to the normal candidate audit
    # and never opens production remote.json. The report emits only fixed
    # category labels: IDs, IPs, process IDs, start times, and credentials are
    # neither copied nor logged.
    if ($authorizedRootKind -ne 'authenticated-fixture') {
        throw 'remote change attribution is restricted to an authenticated fixture.'
    }
    $path = Normalize-CutoverAbsolutePath -LiteralPath $EvidencePath -Label 'remote change evidence path'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $path -Ancestor $rootPath)) {
        throw 'remote change evidence escaped the authenticated fixture.'
    }
    $path = Assert-CutoverConfinedPath -LiteralPath $path -AncestorPath $rootPath
    $source = Read-CutoverConfinedUtf8 `
        -LiteralPath $path `
        -MaxBytes 131072 `
        -Label 'remote change evidence'
    Assert-CutoverWorkDeadline
    try { $evidence = $source | ConvertFrom-Json -Depth 40 }
    catch { throw 'remote change evidence was not valid bounded JSON.' }
    Assert-CutoverWorkDeadline
    if ((Get-ContractProperty -Object $evidence -Name 'schemaVersion') -ne 1) {
        throw 'remote change evidence schema was unsupported.'
    }
    $before = Get-ContractProperty -Object $evidence -Name 'before'
    $after = Get-ContractProperty -Object $evidence -Name 'after'
    if ($null -eq $before -or $null -eq $after) {
        throw 'remote change evidence snapshots were missing.'
    }

    $writerEvidence = Get-ContractProperty -Object $evidence -Name 'writer'
    $writerVerified =
        (Get-ContractProperty -Object $writerEvidence -Name 'installedDevManagerImageAttested') -eq $true -and
        (Get-ContractProperty -Object $writerEvidence -Name 'processIdMatched') -eq $true -and
        (Get-ContractProperty -Object $writerEvidence -Name 'creationTimeMatched') -eq $true
    $writer = if ($writerVerified) {
        'verified-installed-app-generation'
    }
    else { 'unverified' }

    if (Test-CutoverCanonicalJsonEqual -Left $before -Right $after) {
        return [pscustomobject]([ordered]@{
                evaluated = $true
                classification = 'unchanged'
                writer = $writer
                changedCategories = @()
            })
    }

    $categories = New-Object 'System.Collections.Generic.List[string]'
    $protectedChanged = -not (Test-CutoverCanonicalJsonEqual `
            -Left $before `
            -Right $after `
            -IgnoreBrowserActivityFields)

    $beforeHost = Get-ContractProperty -Object $before -Name 'host'
    $afterHost = Get-ContractProperty -Object $after -Name 'host'
    $beforeWeb = Get-ContractProperty -Object $beforeHost -Name 'web'
    $afterWeb = Get-ContractProperty -Object $afterHost -Name 'web'
    $beforeActivity = @(Get-ContractArray (Get-ContractProperty -Object $beforeWeb -Name 'activityLog'))
    $afterActivity = @(Get-ContractArray (Get-ContractProperty -Object $afterWeb -Name 'activityLog'))
    $activityChanged = $false
    $activityRelationValid = $false
    $maxActivityAppend = 8
    if ($beforeActivity.Count -le 100 -and $afterActivity.Count -le 100) {
        # The installed app appends browser events to a 100-entry bounded log.
        # At capacity, one append legitimately drops the oldest entry. Accept
        # only a small suffix-preserving trim followed by a small validated
        # append; arbitrary rewrites remain protected/unclassified.
        $maxDropped = [Math]::Min($maxActivityAppend, $beforeActivity.Count)
        for ($dropped = 0; $dropped -le $maxDropped; $dropped++) {
            $preserved = $beforeActivity.Count - $dropped
            $appended = $afterActivity.Count - $preserved
            if ($appended -lt 0 -or $appended -gt $maxActivityAppend -or
                ($dropped -gt 0 -and $appended -eq 0)) { continue }
            $requiredCapacityDrop = [Math]::Max(
                0,
                $beforeActivity.Count + $appended - 100)
            if ($dropped -ne $requiredCapacityDrop) { continue }
            $matches = $true
            for ($index = 0; $index -lt $preserved; $index++) {
                if (-not (Test-CutoverCanonicalJsonEqual `
                            -Left $beforeActivity[$index + $dropped] `
                            -Right $afterActivity[$index])) {
                    $matches = $false
                    break
                }
            }
            if (-not $matches) { continue }
            for ($index = $preserved; $index -lt $afterActivity.Count; $index++) {
                if (-not (Test-CutoverBrowserActivityEvent -Event $afterActivity[$index])) {
                    $matches = $false
                    break
                }
            }
            if ($matches) {
                $activityRelationValid = $true
                $activityChanged = $dropped -gt 0 -or $appended -gt 0
                break
            }
        }
    }
    if (-not $activityRelationValid) { $protectedChanged = $true }
    if ($activityChanged) { $categories.Add('browser-activity-log') }

    $beforeClients = @(Get-ContractArray (Get-ContractProperty -Object $beforeWeb -Name 'pairedClients'))
    $afterClients = @(Get-ContractArray (Get-ContractProperty -Object $afterWeb -Name 'pairedClients'))
    $lastSeenChanged = $false
    if ($beforeClients.Count -ne $afterClients.Count -or $beforeClients.Count -gt 256) {
        $protectedChanged = $true
    }
    else {
        for ($index = 0; $index -lt $beforeClients.Count; $index++) {
            $beforeEpoch = Get-ContractProperty -Object $beforeClients[$index] -Name 'lastSeenEpochMs'
            $afterEpoch = Get-ContractProperty -Object $afterClients[$index] -Name 'lastSeenEpochMs'
            $beforeIp = Get-ContractProperty -Object $beforeClients[$index] -Name 'lastSeenIp'
            $afterIp = Get-ContractProperty -Object $afterClients[$index] -Name 'lastSeenIp'
            if (-not (Test-CutoverCanonicalJsonEqual -Left $beforeEpoch -Right $afterEpoch) -or
                -not (Test-CutoverCanonicalJsonEqual -Left $beforeIp -Right $afterIp)) {
                $lastSeenChanged = $true
                if ($null -eq $afterEpoch -or
                    ($null -ne $beforeEpoch -and [uint64]$afterEpoch -lt [uint64]$beforeEpoch)) {
                    $protectedChanged = $true
                }
            }
        }
    }
    if ($lastSeenChanged) { $categories.Add('browser-last-seen') }

    if ($protectedChanged -or (-not $activityChanged -and -not $lastSeenChanged)) {
        $categories.Add('protected-or-unclassified')
        $classification = 'protected-or-unclassified-change'
    }
    elseif ($writerVerified) {
        $classification = 'authorized-installed-app-browser-activity'
    }
    else {
        $classification = 'browser-activity-unattributed'
    }
    return [pscustomobject]([ordered]@{
            evaluated = $true
            classification = $classification
            writer = $writer
            changedCategories = @($categories.ToArray())
        })
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

function Get-CutoverFixtureAuthority {
    # This is a test-only, per-fixture capability supplied by the fixture
    # builder.  It must be unpredictable and never be a source-checked
    # constant.  The marker and environment value are compared below, while
    # the root remains completely unavailable to a normal candidate audit.
    # It is a one-time test-only authority, never a product authentication key.
    $token = [Environment]::GetEnvironmentVariable('DEVMANAGER_CUTOVER_FIXTURE_AUTH')
    if ([string]::IsNullOrWhiteSpace($token) -or
        $token.Length -ne 64 -or
        $token -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'fixture authority must be a per-fixture 256-bit test capability.'
    }
    return $token
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
    $lines = Read-CutoverUtf8Lines `
        -Bytes $result.StandardOutput `
        -MaxBytes 16384 `
        -MaxLines $maxGitIdentityLines `
        -MaxLineChars $maxEnvironmentEntryChars
    $lines = @($lines | Where-Object { $_ -ne '' })
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
        try { $providedToken = Get-CutoverFixtureAuthority }
        catch { throw 'root is not an authorized candidate or authenticated fixture.' }
        # The authentication boundary precedes every read of a caller-selected
        # fixture. Only an explicitly authenticated generated fixture may reach
        # path-chain metadata or the marker handle below. This capability is
        # generated by the test process and is not reusable across fixtures.
        Assert-CutoverPathChain -LiteralPath $requested | Out-Null
        $marker = Join-Path $requested '.devmanager-next\audit-fixture.auth'
        $markerHandle = $null
        try {
            $markerHandle = Open-CutoverConfinedFile -LiteralPath $marker -AuthorizedRoot $requested
            $markerBytes = Read-CutoverStreamBytes -Stream $markerHandle.stream -MaxBytes 256 -Label 'fixture authorization'
            $markerText = ([System.Text.UTF8Encoding]::new($false, $true)).GetString($markerBytes)
            if (-not [string]::Equals($markerText, "$providedToken`n", [System.StringComparison]::Ordinal)) {
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
    $script:rootDirectoryHandle = Open-CutoverConfinedFile `
        -LiteralPath $script:rootPath `
        -AuthorizedRoot $script:rootPath `
        -AllowDirectory `
        -AllowDirectoryWrite `
        -DenyDeleteShare
    $script:rootIdentity = $script:rootDirectoryHandle.identity
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

function Get-CutoverCanonicalInstalledExecutableCandidates {
    param([Parameter(Mandatory = $true)][string]$LeafName)

    if ($LeafName -notin @('git.exe', 'rg.exe')) {
        throw 'canonical tool candidate name was rejected.'
    }
    $programFiles = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)
    $programFilesX86 = [Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFilesX86)
    $localAppData = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
    $localPrograms = if ([string]::IsNullOrWhiteSpace($localAppData)) {
        $null
    }
    else {
        Join-Path $localAppData 'Programs'
    }
    # Keep the accepted install surface exact. These trusted tool roots are
    # expanded into canonical executable paths; a broad "under Program Files"
    # check would still allow an unrelated executable inside that directory.
    $trustedRoots = @($programFiles, $programFilesX86, $localPrograms) |
        Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    $trustedExecutables = New-Object 'System.Collections.Generic.List[string]'
    foreach ($trustedRoot in $trustedRoots) {
        $paths = if ($LeafName -eq 'git.exe') {
            @(
                (Join-Path $trustedRoot 'Git\bin\git.exe'),
                (Join-Path $trustedRoot 'Git\cmd\git.exe'),
                (Join-Path $trustedRoot 'Git\mingw64\bin\git.exe'),
                (Join-Path $trustedRoot 'Git\mingw64\libexec\git-core\git.exe')
            )
        }
        else {
            @(
                (Join-Path $trustedRoot 'ripgrep\rg.exe'),
                (Join-Path $trustedRoot 'ripgrep\bin\rg.exe')
            )
        }
        foreach ($path in $paths) { $null = $trustedExecutables.Add($path) }
    }
    return @($trustedExecutables.ToArray())
}

function Test-CutoverTrustedExecutablePath {
    param(
        [Parameter(Mandatory = $true)][string]$ExecutablePath,
        [Parameter(Mandatory = $true)][string]$LeafName
    )

    $full = Normalize-CutoverAbsolutePath -LiteralPath $ExecutablePath -Label 'resolved process executable'
    if (-not $full.EndsWith("\$LeafName", [System.StringComparison]::OrdinalIgnoreCase)) {
        return $false
    }

    foreach ($trustedExecutable in @(Get-CutoverCanonicalInstalledExecutableCandidates -LeafName $LeafName)) {
        if ([string]::IsNullOrWhiteSpace([string]$trustedExecutable)) { continue }
        try {
            $normalizedTrusted = Normalize-CutoverAbsolutePath `
                -LiteralPath $trustedExecutable -Label 'trusted tool executable'
            if ([string]::Equals($full, $normalizedTrusted, [System.StringComparison]::OrdinalIgnoreCase)) {
                return $true
            }
        }
        catch { }
    }
    return $false
}

function Get-CutoverTrustedExecutable {
    param([Parameter(Mandatory = $true)][string]$FileName)

    Assert-CutoverDeadline
    if ([string]::IsNullOrWhiteSpace($FileName)) {
        throw 'process executable name was empty.'
    }
    if ($FileName -notin @('git', 'rg', 'git.exe', 'rg.exe')) {
        throw "process executable is not in the canonical audit tool allowlist: '$FileName'."
    }

    $leaf = if ($FileName.EndsWith('.exe', [System.StringComparison]::OrdinalIgnoreCase)) {
        $FileName
    }
    else { "$FileName.exe" }
    $candidatePaths = New-Object 'System.Collections.Generic.List[string]'
    if ($authorizedRootKind -eq 'authenticated-fixture') {
        # Fixture-only shims are deliberately available for negative tests, but
        # only after the per-fixture authority has been verified. They can never
        # be selected for the normal candidate worktree.
        $rawPath = [Environment]::GetEnvironmentVariable('PATH')
        if ([string]::IsNullOrEmpty($rawPath) -or
            $rawPath.Length -gt ($maxEnvironmentPathEntries * $maxEnvironmentPathChars)) {
            throw 'fixture process PATH exceeded its bounded resolution size.'
        }
        $pathEntries = @($rawPath.Split(';'))
        if ($pathEntries.Count -gt $maxEnvironmentPathEntries) {
            throw 'fixture process PATH exceeded its bounded entry count.'
        }
        foreach ($directory in $pathEntries) {
            Assert-CutoverDeadline
            if ([string]::IsNullOrWhiteSpace($directory)) { continue }
            if ($directory.Length -gt $maxEnvironmentPathChars) {
                throw 'fixture tool PATH entry exceeded its bounded length.'
            }
            $fullDirectory = Normalize-CutoverAbsolutePath -LiteralPath $directory -Label 'fixture tool PATH entry'
            $candidate = Normalize-CutoverAbsolutePath `
                -LiteralPath (Join-Path $fullDirectory $leaf) `
                -Label 'fixture tool executable'
            if ([System.IO.File]::Exists($candidate)) { $null = $candidatePaths.Add($candidate) }
        }
    }
    else {
        # Candidate audits never enumerate ambient PATH. Only explicit trusted
        # tool root candidates are considered, and the executed image is
        # attested again from its process handle after CreateProcessW.
        foreach ($candidate in @(Get-CutoverCanonicalInstalledExecutableCandidates -LeafName $leaf)) {
            if (-not [string]::IsNullOrWhiteSpace($candidate)) { $null = $candidatePaths.Add($candidate) }
        }
    }

    foreach ($candidate in $candidatePaths) {
        Assert-CutoverDeadline
        try {
            $full = Normalize-CutoverAbsolutePath -LiteralPath $candidate -Label 'resolved process executable'
            if (-not [System.IO.File]::Exists($full)) { continue }
            Assert-CutoverPathChain -LiteralPath (Split-Path -Parent $full) | Out-Null
            Assert-CutoverPathChain -LiteralPath $full | Out-Null
            $item = Get-Item -LiteralPath $full -Force -ErrorAction Stop
            if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
            if ($authorizedRootKind -ne 'authenticated-fixture' -and
                -not (Test-CutoverTrustedExecutablePath -ExecutablePath $full -LeafName $leaf)) { continue }
            return $full
        }
        catch { }
    }
    throw "unable to resolve trusted audit executable '$leaf'."
}

function Resolve-CutoverExecutable {
    param([Parameter(Mandatory = $true)][string]$FileName)

    return Get-CutoverTrustedExecutable -FileName $FileName
}

function Add-CutoverEnvironmentEntry {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[string]]$Environment,
        [Parameter(Mandatory = $true)][hashtable]$AggregateState,
        [Parameter(Mandatory = $true)][string]$Name,
        [AllowEmptyString()][string]$Value,
        [switch]$AllowEmptyValue
    )

    Assert-CutoverDeadline
    if ($Name -notmatch '^[A-Za-z_][A-Za-z0-9_]*$' -or
        $Name -match '(?i)(secret|token|password|passwd|credential|private.?key|auth)') {
        throw "environment allowlist rejected variable name '$Name'."
    }
    if ($null -eq $Value) { $Value = '' }
    if (-not $AllowEmptyValue -and [string]::IsNullOrEmpty($Value)) {
        throw "environment allowlist rejected empty value for '$Name'."
    }
    if ($Value.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
        throw "environment allowlist rejected control characters for '$Name'."
    }
    $entry = "$Name=$Value"
    if ($entry.Length -gt $maxEnvironmentEntryChars) {
        throw "environment entry '$Name' exceeded its bounded length."
    }
    if ($Environment.Count -ge $maxEnvironmentEntries) {
        throw 'environment block exceeded its bounded entry count.'
    }
    $entryBytes = [System.Text.Encoding]::Unicode.GetByteCount($entry) + 2
    if ($AggregateState.AggregateBytes -gt ($maxEnvironmentBytes - $entryBytes)) {
        throw 'environment block exceeded its bounded aggregate size.'
    }
    $AggregateState.AggregateBytes += $entryBytes
    $null = $Environment.Add($entry)
}

function Get-CutoverProcessEnvironment {
    param([Parameter(Mandatory = $true)][string]$ResolvedExecutable)

    # Child processes receive a deliberately small, canonical environment.
    # Parent PATH is consulted only for an authenticated test fixture; a
    # candidate audit receives a canonical absolute executable image.
    $windowsRoot = [Environment]::GetFolderPath([Environment+SpecialFolder]::Windows)
    $systemDirectory = [Environment]::SystemDirectory
    if ([string]::IsNullOrWhiteSpace($windowsRoot) -or
        [string]::IsNullOrWhiteSpace($systemDirectory)) {
        throw 'canonical Windows directories were unavailable.'
    }
    $pathDirectories = New-Object 'System.Collections.Generic.List[string]'
    $addPathDirectory = {
        param([string]$Directory)
        Assert-CutoverDeadline
        if ([string]::IsNullOrWhiteSpace($Directory)) { return }
        if ($Directory.Length -gt $maxEnvironmentPathChars) {
            throw 'child PATH entry exceeded its bounded length.'
        }
        $full = Normalize-CutoverAbsolutePath -LiteralPath $Directory -Label 'process runtime directory'
        if (-not [System.IO.Directory]::Exists($full)) { return }
        try { Assert-CutoverPathChain -LiteralPath $full | Out-Null }
        catch { return }
        if (-not $pathDirectories.Contains($full)) {
            if ($pathDirectories.Count -ge $maxEnvironmentPathEntries) {
                throw 'child PATH exceeded its bounded entry count.'
            }
            $null = $pathDirectories.Add($full)
        }
    }
    & $addPathDirectory (Split-Path -Parent $ResolvedExecutable)
    & $addPathDirectory $systemDirectory
    & $addPathDirectory $windowsRoot
    & $addPathDirectory (Join-Path $systemDirectory 'WindowsPowerShell\v1.0')

    # Authenticated generated fixtures may place their shim directory at the
    # front of PATH. Every entry remains bounded and canonicalized; no other
    # ambient PATH is inherited by a normal candidate-worktree scan.
    if ($authorizedRootKind -eq 'authenticated-fixture') {
        $rawPath = [Environment]::GetEnvironmentVariable('PATH')
        if ($null -eq $rawPath -or $rawPath.Length -gt ($maxEnvironmentPathEntries * $maxEnvironmentPathChars)) {
            throw 'fixture PATH exceeded its bounded aggregate size.'
        }
        $fixtureEntries = $rawPath.Split(';')
        if ($fixtureEntries.Count -gt $maxEnvironmentPathEntries) {
            throw 'fixture PATH exceeded its bounded entry count.'
        }
        foreach ($directory in $fixtureEntries) {
            Assert-CutoverDeadline
            if ([string]::IsNullOrWhiteSpace($directory)) { continue }
            if ($directory.Length -gt $maxEnvironmentPathChars) {
                throw 'fixture PATH entry exceeded its bounded length.'
            }
            $full = Normalize-CutoverAbsolutePath -LiteralPath $directory -Label 'fixture process path'
            & $addPathDirectory $full
            if ($pathDirectories.Contains($full)) {
                $null = $pathDirectories.Remove($full)
                $pathDirectories.Insert(0, $full)
            }
        }
    }

    $pathValue = $pathDirectories -join ';'
    $environment = New-Object 'System.Collections.Generic.List[string]'
    $aggregateState = @{ AggregateBytes = [int64]0 }
    $add = {
        param([string]$Name, [AllowEmptyString()][string]$Value, [switch]$AllowEmptyValue)
        Add-CutoverEnvironmentEntry `
            -Environment $environment `
            -AggregateState $aggregateState `
            -Name $Name `
            -Value $Value `
            -AllowEmptyValue:$AllowEmptyValue
    }
    & $add 'SystemRoot' $windowsRoot
    & $add 'WINDIR' $windowsRoot
    & $add 'COMSPEC' (Join-Path $systemDirectory 'cmd.exe')
    & $add 'PATHEXT' '.COM;.EXE;.BAT;.CMD'
    & $add 'PATH' $pathValue
    & $add 'LANG' 'C'
    & $add 'LC_ALL' 'C'
    & $add 'GIT_CONFIG_NOSYSTEM' '1'
    & $add 'GIT_CONFIG_SYSTEM' 'NUL'
    & $add 'GIT_CONFIG_GLOBAL' 'NUL'
    & $add 'GIT_TERMINAL_PROMPT' '0'
    & $add 'GIT_OPTIONAL_LOCKS' '0'
    & $add 'GIT_CONFIG_COUNT' '4'
    & $add 'GIT_CONFIG_KEY_0' 'core.hooksPath'
    & $add 'GIT_CONFIG_VALUE_0' 'NUL'
    & $add 'GIT_CONFIG_KEY_1' 'core.fsmonitor'
    & $add 'GIT_CONFIG_VALUE_1' 'false'
    & $add 'GIT_CONFIG_KEY_2' 'credential.helper'
    & $add 'GIT_CONFIG_VALUE_2' '' -AllowEmptyValue
    & $add 'GIT_CONFIG_KEY_3' 'protocol.file.allow'
    & $add 'GIT_CONFIG_VALUE_3' 'never'

    # Authenticated fixture-only environment controls use the same canonical
    # allowlist. They are never copied into a candidate audit child.
    if ($authorizedRootKind -eq 'authenticated-fixture') {
        foreach ($name in $allowedFixtureEnvironmentNames) {
            Assert-CutoverDeadline
            $value = [Environment]::GetEnvironmentVariable($name)
            if ($null -ne $value) {
                & $add $name ([string]$value) -AllowEmptyValue
            }
        }
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

function Test-CutoverForbiddenEvidenceClaim {
    param([AllowEmptyString()][string]$Text)

    $lower = ([string]$Text).ToLowerInvariant()
    if ([string]::IsNullOrEmpty($lower)) {
        return $false
    }
    return $lower.Contains('assumed') -or
        $lower.Contains('partial') -or
        $lower.Contains('compile-only') -or
        $lower.Contains('compile only') -or
        $lower.Contains('compile_only')
}

function Assert-CutoverVerifiableClaimText {
    param(
        [AllowEmptyString()][string]$Text,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if (Test-CutoverForbiddenEvidenceClaim -Text $Text) {
        Add-ContractError "${Label} must not claim assumed, partial, or compile-only evidence."
        return $false
    }
    return $true
}

function Get-CutoverJsonObjectKeys {
    param(
        [Parameter(Mandatory = $true)][string]$Json,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $keys = New-Object 'System.Collections.Generic.List[string]'
    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $index = 0
    $length = $Json.Length
    while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
    if ($index -ge $length -or $Json[$index] -ne '{') {
        throw "${Label} is not a JSON object."
    }
    $index++
    while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
    if ($index -lt $length -and $Json[$index] -eq '}') {
        return @()
    }
    while ($index -lt $length) {
        Assert-CutoverWorkDeadline
        while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
        if ($index -ge $length -or $Json[$index] -ne '"') {
            throw "${Label} has a malformed object key."
        }
        $index++
        $keyChars = New-Object 'System.Text.StringBuilder'
        while ($index -lt $length) {
            $character = $Json[$index]
            if ($character -eq '"') { break }
            if ($character -eq '\') {
                $index++
                if ($index -ge $length) { throw "${Label} has a truncated escape." }
                $null = $keyChars.Append($Json[$index])
            }
            else {
                $null = $keyChars.Append($character)
            }
            $index++
        }
        if ($index -ge $length -or $Json[$index] -ne '"') {
            throw "${Label} has an unterminated object key."
        }
        $index++
        $key = $keyChars.ToString()
        if (-not $seen.Add($key)) {
            throw "${Label} has a duplicate key."
        }
        $keys.Add($key)
        while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
        if ($index -ge $length -or $Json[$index] -ne ':') {
            throw "${Label} is missing a key separator."
        }
        $index++
        while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
        if ($index -ge $length) { throw "${Label} is truncated." }
        $depth = 0
        $inString = $false
        $escape = $false
        while ($index -lt $length) {
            $character = $Json[$index]
            if ($inString) {
                if ($escape) { $escape = $false }
                elseif ($character -eq '\') { $escape = $true }
                elseif ($character -eq '"') { $inString = $false }
            }
            else {
                if ($character -eq '"') { $inString = $true }
                elseif ($character -eq '{' -or $character -eq '[') { $depth++ }
                elseif ($character -eq '}' -or $character -eq ']') {
                    if ($depth -eq 0) { break }
                    $depth--
                }
                elseif ($character -eq ',' -and $depth -eq 0) { break }
            }
            $index++
        }
        while ($index -lt $length -and [char]::IsWhiteSpace($Json[$index])) { $index++ }
        if ($index -ge $length) { throw "${Label} is truncated." }
        if ($Json[$index] -eq ',') {
            $index++
            continue
        }
        if ($Json[$index] -eq '}') {
            return @($keys.ToArray())
        }
        throw "${Label} has malformed object punctuation."
    }
    throw "${Label} is truncated."
}

function Get-CutoverEvidenceArtifactVerdict {
    param(
        [AllowEmptyString()][string]$ArtifactPath,
        [Parameter(Mandatory = $true)][string]$RowId,
        [AllowEmptyCollection()][string[]]$ExpectedGateIds,
        [AllowEmptyCollection()][string[]]$ExpectedTestIds,
        [AllowEmptyCollection()][string[]]$ExpectedCommands
    )

    if ([string]::IsNullOrWhiteSpace($ArtifactPath)) {
        return 'missing'
    }
    $leaf = [System.IO.Path]::GetFileName($ArtifactPath)
    if ($leaf.Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        return 'protected'
    }
    $literal = Join-Path $rootPath ($ArtifactPath.Replace('/', '\'))
    $present = $false
    try {
        $present = Test-CutoverConfinedFilePresent -LiteralPath $literal
    }
    catch {
        return 'rejected'
    }
    if (-not $present) {
        return 'missing'
    }

    try {
        $raw = Read-CutoverConfinedUtf8 `
            -LiteralPath $literal `
            -MaxBytes 8192 `
            -Label "row '$RowId' evidence artifact"
        $null = Get-CutoverJsonObjectKeys -Json $raw -Label "row '$RowId' evidence artifact"
        $parsed = $raw | ConvertFrom-Json -Depth 8
    }
    catch {
        $message = [string]$_.Exception.Message
        if ($message.Contains('duplicate key')) { return 'malformed' }
        if ($message.Contains('exceeds the bounded') -or $message.Contains('exceeded')) { return 'oversized' }
        return 'malformed'
    }

    $required = @(
        'schemaVersion', 'kind', 'verdict', 'gateId', 'testId', 'recipe',
        'source', 'completedAtUtc', 'freshnessSeconds'
    )
    foreach ($field in $required) {
        if ($null -eq (Get-ContractProperty -Object $parsed -Name $field)) {
            return 'unknown'
        }
    }
    if ((Get-ContractProperty -Object $parsed -Name 'schemaVersion') -ne 1) {
        return 'unknown'
    }
    $kind = [string](Get-ContractProperty -Object $parsed -Name 'kind')
    $verdict = [string](Get-ContractProperty -Object $parsed -Name 'verdict')
    $gateId = [string](Get-ContractProperty -Object $parsed -Name 'gateId')
    $testId = [string](Get-ContractProperty -Object $parsed -Name 'testId')
    $recipe = [string](Get-ContractProperty -Object $parsed -Name 'recipe')
    foreach ($claim in @($kind, $verdict, $gateId, $testId, $recipe)) {
        if (Test-CutoverForbiddenEvidenceClaim -Text $claim) {
            return 'compile-only'
        }
        if ($claim.Equals('stale', [System.StringComparison]::OrdinalIgnoreCase)) {
            return 'stale'
        }
    }
    if ($kind -notin @('phase-gate', 'focused-e2e', 'soak')) {
        return 'unknown'
    }
    if ($verdict -in @('failed', 'cancelled', 'pending')) {
        return 'failed'
    }
    if (-not [string]::Equals($verdict, 'pass', [System.StringComparison]::Ordinal)) {
        return 'unknown'
    }
    if (@($ExpectedGateIds | Where-Object { [string]::Equals([string]$_, $gateId, [System.StringComparison]::Ordinal) }).Count -eq 0) {
        return 'mismatched'
    }
    if (@($ExpectedTestIds).Count -gt 0 -and
        @($ExpectedTestIds | Where-Object { [string]::Equals([string]$_, $testId, [System.StringComparison]::Ordinal) }).Count -eq 0) {
        return 'mismatched'
    }

    $source = Get-ContractProperty -Object $parsed -Name 'source'
    $commit = [string](Get-ContractProperty -Object $source -Name 'commit')
    $digest = [string](Get-ContractProperty -Object $source -Name 'contentSha256')
    $sourcePath = Get-ContractProperty -Object $source -Name 'path'
    $attested = $false
    $sourceVolume = ''
    $sourceIndex = ''
    if (-not [string]::IsNullOrWhiteSpace($digest)) {
        if ($digest -notmatch '^[0-9a-fA-F]{64}$') {
            return 'malformed'
        }
        $attestedPath = Normalize-ContractRelativePath -Value $sourcePath -Label "row '$RowId' evidence source.path"
        if ($null -eq $attestedPath) {
            return 'mismatched'
        }
        $openedSource = $null
        try {
            $attestedLiteral = Assert-CutoverConfinedPath `
                -LiteralPath (Join-Path $rootPath ($attestedPath.Replace('/', '\'))) `
                -AncestorPath $rootPath
            $openedSource = Open-CutoverConfinedFile -LiteralPath $attestedLiteral
            $attestedBytes = Read-CutoverScanBytes -Opened $openedSource -MaxBytes $maxScanBytesPerFile
            $actual = Get-CutoverSha256Hex -Bytes $attestedBytes
            if (-not [string]::Equals($actual, $digest, [System.StringComparison]::OrdinalIgnoreCase)) {
                return 'mismatched'
            }
            $sourceIdentity = Get-CutoverHandleIdentity -Stream $openedSource.stream
            $sourceVolume = [string]$sourceIdentity.volume
            $sourceIndex = [string]$sourceIdentity.index
            if ([string]::IsNullOrWhiteSpace($sourceVolume) -or [string]::IsNullOrWhiteSpace($sourceIndex)) {
                return 'mismatched'
            }
            $attested = $true
        }
        catch {
            return 'mismatched'
        }
        finally {
            if ($null -ne $openedSource) { $openedSource.stream.Dispose() }
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($commit)) {
        if ($commit -notmatch '^[0-9a-fA-F]{40}$') {
            return 'malformed'
        }
    }
    if (-not $attested) {
        return 'unknown'
    }

    $completedRaw = [string](Get-ContractProperty -Object $parsed -Name 'completedAtUtc')
    try {
        $completed = [DateTimeOffset]::Parse(
            $completedRaw,
            [System.Globalization.CultureInfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::RoundtripKind
        )
    }
    catch {
        return 'malformed'
    }
    $freshness = Get-ContractProperty -Object $parsed -Name 'freshnessSeconds'
    if ($freshness -isnot [int] -and $freshness -isnot [long] -and $freshness -isnot [decimal]) {
        try { $freshness = [int]$freshness } catch { return 'malformed' }
    }
    $maxAge = [int]$freshness
    if ($maxAge -le 0 -or $maxAge -gt 315360000) {
        return 'malformed'
    }
    $ageSeconds = ([DateTimeOffset]::UtcNow - $completed.ToUniversalTime()).TotalSeconds
    if ($ageSeconds -gt $maxAge -or $ageSeconds -lt -300) {
        return 'stale'
    }

    $execution = Get-ContractProperty -Object $parsed -Name 'execution'
    $capturedCommand = [string](Get-ContractProperty -Object $execution -Name 'command')
    $resultDigest = [string](Get-ContractProperty -Object $execution -Name 'resultSha256')
    $exitCode = Get-ContractProperty -Object $execution -Name 'exitCode'
    $capturedAt = [string](Get-ContractProperty -Object $execution -Name 'completedAtUtc')
    if ([string]::IsNullOrWhiteSpace($capturedCommand) -or
        $capturedCommand.Length -gt $maxNeedleChars -or
        $capturedCommand.IndexOfAny([char[]](0..31 + 127)) -ge 0) {
        return 'unknown'
    }
    if (Test-CutoverForbiddenEvidenceClaim -Text $capturedCommand) {
        return 'compile-only'
    }
    if ([string]::Equals($capturedCommand, $recipe, [System.StringComparison]::Ordinal)) {
        return 'unknown'
    }
    if ($resultDigest -notmatch '^[0-9a-fA-F]{64}$') {
        return 'unknown'
    }
    if ($exitCode -isnot [int] -and $exitCode -isnot [long]) {
        try { $exitCode = [int]$exitCode } catch { return 'malformed' }
    }
    if ([int]$exitCode -ne 0) {
        return 'failed'
    }
    if (-not [string]::Equals($capturedAt, $completedRaw, [System.StringComparison]::Ordinal)) {
        return 'unknown'
    }
    $executionSource = [string](Get-ContractProperty -Object $execution -Name 'sourceSha256')
    $runClaim = [string](Get-ContractProperty -Object $execution -Name 'runSha256')
    if ($executionSource -notmatch '^[0-9a-fA-F]{64}$' -or $runClaim -notmatch '^[0-9a-fA-F]{64}$') {
        return 'unknown'
    }
    $sourceDigest = $digest.ToLowerInvariant()
    $resultNorm = $resultDigest.ToLowerInvariant()
    $runNorm = $runClaim.ToLowerInvariant()
    $zeroDigest = '0000000000000000000000000000000000000000000000000000000000000000'
    if ($resultNorm -eq $zeroDigest -or $sourceDigest -eq $zeroDigest -or $runNorm -eq $zeroDigest) {
        return 'malformed'
    }
    if (-not [string]::Equals($executionSource, $sourceDigest, [System.StringComparison]::OrdinalIgnoreCase)) {
        return 'mismatched'
    }
    $claimedVolume = [string](Get-ContractProperty -Object $execution -Name 'sourceVolume')
    $claimedIndex = [string](Get-ContractProperty -Object $execution -Name 'sourceIndex')
    if ([string]::IsNullOrWhiteSpace($claimedVolume) -or [string]::IsNullOrWhiteSpace($claimedIndex) -or
        -not [string]::Equals($claimedVolume, $sourceVolume, [System.StringComparison]::Ordinal) -or
        -not [string]::Equals($claimedIndex, $sourceIndex, [System.StringComparison]::Ordinal)) {
        return 'mismatched'
    }
    $runMaterial = [System.Text.UTF8Encoding]::new($false, $true).GetBytes(
        ($capturedCommand + "`n" + $resultNorm + "`n" + [string]([int]$exitCode) + "`n" + $capturedAt + "`n" + $sourceDigest + "`n" + $sourceVolume + "`n" + $sourceIndex)
    )
    $actualRun = Get-CutoverSha256Hex -Bytes $runMaterial
    if (-not [string]::Equals($actualRun, $runNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
        return 'mismatched'
    }
    if (@($ExpectedCommands).Count -gt 0) {
        $commandBound = $false
        foreach ($declared in @($ExpectedCommands)) {
            if ([string]::Equals([string]$declared, $capturedCommand, [System.StringComparison]::Ordinal)) {
                $commandBound = $true
                break
            }
        }
        if (-not $commandBound) {
            return 'mismatched'
        }
    }
    return 'present'
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
        [Parameter(Mandatory = $true)][string]$Label,
        [switch]$AllowDirectory
    )

    return Assert-CutoverRelativePath -Value $Value -Label $Label -AllowDirectory:$AllowDirectory
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

    $directoryOwner = $Path.EndsWith('/')
    $normalized = if ($directoryOwner) { $Path.Substring(0, $Path.Length - 1) } else { $Path }
    if ([string]::IsNullOrEmpty($normalized)) {
        return $false
    }
    $prefix = "$normalized/"
    foreach ($tracked in $Tracked) {
        $candidate = [string]$tracked
        if (-not $directoryOwner -and [string]::Equals($candidate, $normalized, [System.StringComparison]::Ordinal)) {
            return $true
        }
        if ($directoryOwner -and $candidate.StartsWith($prefix, [System.StringComparison]::Ordinal)) {
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
    $jsonText = Read-CutoverContractLines -Source $jsonSource
    try {
        Assert-CutoverWorkDeadline
        $parsed = $jsonText | ConvertFrom-Json -Depth 100
        Assert-CutoverWorkDeadline
        return $parsed
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
    $rawPaths = Read-CutoverNulDelimitedPaths -Bytes $bytes -MaxPaths $maxTrackedFiles

    $exact = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    $physical = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)
    $paths = New-Object 'System.Collections.Generic.List[string]'
    foreach ($rawPath in $rawPaths) {
        Assert-CutoverWorkDeadline
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

    Assert-CutoverProcessStartDeadline
    Initialize-CutoverProcessMethodsV2
    $resolvedExecutable = Resolve-CutoverExecutable -FileName $FileName
    $environment = Get-CutoverProcessEnvironment -ResolvedExecutable $resolvedExecutable
    $processResult = [CutoverProcessMethodsV2]::Run(
        $resolvedExecutable,
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


function Test-PathUnderScanRoots {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Path,
        [AllowEmptyCollection()][string[]]$ScanRoots
    )

    if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
    if (@($ScanRoots).Count -eq 0) { return $true }
    $normalized = $Path.Replace('\', '/')
    foreach ($root in @($ScanRoots)) {
        $candidate = ([string]$root).Replace('\', '/').TrimEnd('/')
        if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
        if ([string]::Equals($normalized, $candidate, [System.StringComparison]::Ordinal)) { return $true }
        if ($normalized.StartsWith($candidate + '/', [System.StringComparison]::Ordinal)) { return $true }
    }
    return $false
}

function Test-CutoverHistoricalReferenceAllowed {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Path,
        [AllowEmptyCollection()][string[]]$Allowlist
    )

    if ([string]::IsNullOrWhiteSpace($Path)) { return $false }
    $normalized = $Path.Replace('\', '/')
    foreach ($allowed in @($Allowlist)) {
        $candidate = ([string]$allowed).Replace('\', '/').TrimEnd('/')
        if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
        if ([string]::Equals($normalized, $candidate, [System.StringComparison]::Ordinal)) { return $true }
        if ($normalized.StartsWith($candidate + '/', [System.StringComparison]::Ordinal)) { return $true }
    }
    return $false
}

function Enable-CutoverAuditIsolation {
    param(
        [Parameter(Mandatory = $true)][string]$RepositoryRoot,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot
    )

    $inherited = [Environment]::GetEnvironmentVariable('DEVMANAGER_PROFILE')
    if (-not [string]::IsNullOrWhiteSpace([string]$inherited)) {
        [Environment]::SetEnvironmentVariable('DEVMANAGER_PROFILE', $null, 'Process')
        $script:isolationReport.inheritedDevmanagerProfileCleared = $true
    }
    else {
        $script:isolationReport.inheritedDevmanagerProfileCleared = $true
    }
    $script:isolationReport.setDevmanagerProfile = -not [string]::IsNullOrWhiteSpace(
        [string][Environment]::GetEnvironmentVariable('DEVMANAGER_PROFILE')
    )

    $isolatedAppData = Normalize-CutoverAbsolutePath `
        -LiteralPath (Join-Path $EvidenceRoot 'appdata') `
        -Label 'isolated APPDATA'
    New-Item -ItemType Directory -Force -Path $isolatedAppData | Out-Null
    [Environment]::SetEnvironmentVariable('APPDATA', $isolatedAppData, 'Process')
    $currentAppData = [Environment]::GetEnvironmentVariable('APPDATA')
    $script:isolationReport.remappedAppData = Test-CutoverPathEqualsOrBeneath `
        -Path $currentAppData `
        -Ancestor $RepositoryRoot
    $script:isolationReport.evidenceRootBeneathWorktree = Test-CutoverPathEqualsOrBeneath `
        -Path $EvidenceRoot `
        -Ancestor $RepositoryRoot
    $script:isolationReport.productionRootRead = $false
    $script:installedAppReport.observedInstalledProcesses = $false
    $script:installedAppReport.openSessionJson = $false
    $script:installedAppReport.hashProductionFiles = $false
    $script:installedAppReport.installPublishDeleteUserData = $false
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

function Invoke-CutoverInternalReferenceScan {
    param(
        [AllowEmptyString()][string]$ScanText,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][byte[]]$ScanBytes,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][object[]]$Needles,
        [Parameter(Mandatory = $true)][string]$RelativePath,
        [Parameter(Mandatory = $true)][int]$MaxMatches,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.IDictionary]$Counts,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[object]]$Matches
    )

    $lines = New-Object 'System.Collections.Generic.List[string]'
    if (-not [string]::IsNullOrEmpty($ScanText)) {
        $buffer = [System.Text.UTF8Encoding]::new($false, $true).GetBytes($ScanText)
        foreach ($line in @(Read-CutoverUtf8Lines -Bytes $buffer -MaxBytes $maxScannerOutputBytes -MaxLines $maxScannerOutputLines -MaxLineChars $maxScannerOutputLineChars)) {
            Assert-CutoverWorkDeadline
            $lines.Add([string]$line)
        }
    }
    $lineNumber = 0
    foreach ($line in $lines) {
        Assert-CutoverWorkDeadline
        $lineNumber++
        foreach ($needle in $Needles) {
            Assert-CutoverWorkDeadline
            $needleText = [string]$needle.needle
            if ([string]::IsNullOrEmpty($needleText)) { continue }
            if (-not [string]::IsNullOrEmpty([string]$needle.contextPath) -and
                -not [string]::Equals($RelativePath, [string]$needle.contextPath, [System.StringComparison]::Ordinal)) {
                continue
            }
            if ($line.IndexOf($needleText, [System.StringComparison]::Ordinal) -lt 0) { continue }
            $key = "$($needle.ownerId)|$($needle.kind)"
            if (-not $Counts.ContainsKey($key)) { $Counts[$key] = 0 }
            if ($Counts[$key] -ge [Math]::Min($MaxMatches, $maxMatchesPerOwner)) {
                Add-SafetyBound
                continue
            }
            $Counts[$key]++
            $Matches.Add([pscustomobject]@{
                    ownerId = [string]$needle.ownerId
                    kind = [string]$needle.kind
                    path = $RelativePath
                    line = $lineNumber
                })
        }
    }
    if ($lines.Count -gt 0) {
        return
    }
    foreach ($needle in $Needles) {
        Assert-CutoverWorkDeadline
        $needleBytes = [System.Text.UTF8Encoding]::new($false, $true).GetBytes([string]$needle.needle)
        if ($needleBytes.Length -eq 0 -or $needleBytes.Length -gt $ScanBytes.Length) { continue }
        if (-not [string]::IsNullOrEmpty([string]$needle.contextPath) -and
            -not [string]::Equals($RelativePath, [string]$needle.contextPath, [System.StringComparison]::Ordinal)) {
            continue
        }
        $found = $false
        $last = $ScanBytes.Length - $needleBytes.Length
        for ($offset = 0; $offset -le $last; $offset++) {
            Assert-CutoverWorkDeadline
            $matched = $true
            for ($needleIndex = 0; $needleIndex -lt $needleBytes.Length; $needleIndex++) {
                if ($ScanBytes[$offset + $needleIndex] -ne $needleBytes[$needleIndex]) {
                    $matched = $false
                    break
                }
            }
            if ($matched) { $found = $true; break }
        }
        if (-not $found) { continue }
        $key = "$($needle.ownerId)|$($needle.kind)"
        if (-not $Counts.ContainsKey($key)) { $Counts[$key] = 0 }
        if ($Counts[$key] -ge [Math]::Min($MaxMatches, $maxMatchesPerOwner)) {
            Add-SafetyBound
            continue
        }
        $Counts[$key]++
        $Matches.Add([pscustomobject]@{
                ownerId = [string]$needle.ownerId
                kind = [string]$needle.kind
                path = $RelativePath
                line = 0
            })
    }
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
        Assert-CutoverWorkDeadline
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
            $opened = Open-CutoverConfinedFile -LiteralPath $absolutePath
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
            $scanNeedleList = New-Object 'System.Collections.Generic.List[object]'
            foreach ($needle in $Needles) {
                Assert-CutoverWorkDeadline
                if ($null -eq $scanText -or
                    $scanText.IndexOf([string]$needle.needle, [System.StringComparison]::Ordinal) -ge 0) {
                    $scanNeedleList.Add($needle)
                }
            }
            $scanNeedles = @($scanNeedleList.ToArray())
            $scan = [pscustomobject]@{ lines = @(); exitCode = 0; boundHit = $false }
            $useExternalRg = $authorizedRootKind -eq 'authenticated-fixture'
            if ($useExternalRg) {
                try {
                    $null = Resolve-CutoverExecutable -FileName 'rg'
                }
                catch {
                    $useExternalRg = $false
                }
            }
            if ($scanNeedles.Count -gt 0 -and -not $useExternalRg) {
                Invoke-CutoverInternalReferenceScan `
                    -ScanText $scanText `
                    -ScanBytes $scanBytes `
                    -Needles $scanNeedles `
                    -RelativePath $relativePath `
                    -MaxMatches $MaxMatches `
                    -Counts $counts `
                    -Matches $matches
            }
            elseif ($scanNeedles.Count -gt 0) {
                $arguments = @(
                    '--json', '--fixed-strings', '--line-number', '--no-heading', '--color', 'never',
                    '--no-messages', '--text', '--hidden', '--no-ignore', '--max-count', [string]$MaxMatches,
                    '--max-columns', '4096', '--max-columns-preview'
                )
                foreach ($needle in $scanNeedles) {
                    Assert-CutoverWorkDeadline
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
                $reopened = Open-CutoverConfinedFile -LiteralPath $absolutePath
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
                Assert-CutoverWorkDeadline
                if ([string]::IsNullOrWhiteSpace([string]$rawLine)) { continue }
                try { $event = ([string]$rawLine | ConvertFrom-Json -Depth 30) }
                catch { Add-GlobalBlocker 'rg returned a non-JSON event in JSON mode.'; continue }
                Assert-CutoverWorkDeadline
                if ([string](Get-ContractProperty -Object $event -Name 'type') -ne 'match') { continue }
                $data = Get-ContractProperty -Object $event -Name 'data'
                $submatches = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
                foreach ($submatch in Get-ContractArray (Get-ContractProperty -Object $data -Name 'submatches')) {
                    Assert-CutoverWorkDeadline
                    $matchData = Get-ContractProperty -Object $submatch -Name 'match'
                    $matchText = [string](Get-ContractProperty -Object $matchData -Name 'text')
                    if (-not [string]::IsNullOrEmpty($matchText)) { $null = $submatches.Add($matchText) }
                }
                $lineNumber = 0
                $lineValue = Get-ContractProperty -Object $data -Name 'line_number'
                if ($null -ne $lineValue) { $lineNumber = [int]$lineValue }
                foreach ($needle in $scanNeedles) {
                    Assert-CutoverWorkDeadline
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
    // Partition the tail of the single caller-owned deadline: scanner work
    // stops first, owned-process settlement gets its bounded slice, and the
    // final slice remains available for a fail-closed report. These are derived
    // cutoffs, never fresh or extended deadlines.
    private const int ReportPublicationReserveMilliseconds = 4000;
    private const int ProcessSettlementReserveMilliseconds = 3000;
    private const int WorkReserveMilliseconds =
        ReportPublicationReserveMilliseconds + ProcessSettlementReserveMilliseconds;
    private const uint JobObjectExtendedLimitInformation = 9;
    private const uint JobObjectBasicAccountingInformation = 1;
    private const uint JobObjectLimitKillOnJobClose = 0x2000;
    private const uint JobObjectLimitActiveProcess = 0x00000008;

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

    // CreateProcess can succeed before identity/creation-time validation
    // fails. Preserve the root cleanup proof across that boundary instead of
    // allowing the outer catch to treat a lost handle as ACTIVE_PROCESS_ZERO.
    private sealed class ProcessLaunchException : Exception
    {
        public string FailureCategory { get; private set; }
        public bool RootCleanupProven { get; private set; }

        public ProcessLaunchException(string failureCategory, bool rootCleanupProven)
            : base(failureCategory)
        {
            FailureCategory = failureCategory;
            RootCleanupProven = rootCleanupProven;
        }
    }

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
        public string ExecutablePath;
        public FileIdentity ExecutableIdentity;
    }

    private sealed class TrackedProcess
    {
        public uint ProcessId;
        public long CreationTime;
        public IntPtr Handle;
        public string ExecutablePath;
        public FileIdentity ExecutableIdentity;
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
    private const uint ProcessQueryInformation = 0x00000400;
    private const uint ProcessQueryLimitedInformation = 0x00001000;
    private const uint Synchronize = 0x00100000;
    private const uint InvalidHandleValue = 0xFFFFFFFF;
    private const uint GenericRead = 0x80000000;
    private const uint FileShareRead = 0x00000001;
    private const uint FileShareWrite = 0x00000002;
    private const uint OpenExisting = 3;
    private const uint FileFlagOpenReparsePoint = 0x00200000;
    private const int MaxEnvironmentEntries = 64;
    private const int MaxEnvironmentEntryChars = 4096;
    private const int MaxEnvironmentBytes = 32768;
    private const int MaxTrackedProcesses = 256;

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
    private static extern IntPtr CreateFileW(
        string fileName,
        uint desiredAccess,
        uint shareMode,
        IntPtr securityAttributes,
        uint creationDisposition,
        uint flagsAndAttributes,
        IntPtr templateFile);

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

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetFileInformationByHandle(
        IntPtr file,
        out ByHandleFileInformation information);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool QueryFullProcessImageNameW(
        IntPtr process,
        uint flags,
        StringBuilder imageName,
        ref uint size);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern uint GetFinalPathNameByHandleW(
        IntPtr file,
        StringBuilder path,
        uint pathLength,
        uint flags);

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
        long aggregateBytes = 4;
        if (environment != null)
        {
            foreach (var entry in environment)
            {
                if (string.IsNullOrEmpty(entry) || entry.IndexOf('\0') >= 0 || entry.IndexOf('=') <= 0)
                    throw new InvalidOperationException("environment-invalid");
                if (values.Count >= MaxEnvironmentEntries || entry.Length > MaxEnvironmentEntryChars)
                    throw new InvalidOperationException("environment-limit");
                var name = entry.Substring(0, entry.IndexOf('='));
                if (name.IndexOf("SECRET", StringComparison.OrdinalIgnoreCase) >= 0 ||
                    name.IndexOf("TOKEN", StringComparison.OrdinalIgnoreCase) >= 0 ||
                    name.IndexOf("PASSWORD", StringComparison.OrdinalIgnoreCase) >= 0 ||
                    name.IndexOf("CREDENTIAL", StringComparison.OrdinalIgnoreCase) >= 0 ||
                    name.IndexOf("PRIVATE_KEY", StringComparison.OrdinalIgnoreCase) >= 0 ||
                    name.IndexOf("AUTH", StringComparison.OrdinalIgnoreCase) >= 0)
                    throw new InvalidOperationException("environment-secret");
                aggregateBytes = checked(aggregateBytes + ((long)entry.Length + 1L) * 2L);
                if (aggregateBytes > MaxEnvironmentBytes)
                    throw new InvalidOperationException("environment-limit");
                values.Add(entry);
            }
        }
        values.Sort(StringComparer.OrdinalIgnoreCase);
        var block = string.Join("\0", values) + "\0\0";
        return Marshal.StringToHGlobalUni(block);
    }

    private sealed class FileIdentity
    {
        public uint Volume;
        public ulong Index;
        public uint Links;
        public uint Attributes;
        public string FinalPath;
    }

    private static string GetFinalPathByHandle(IntPtr file)
    {
        var capacity = 512;
        while (capacity <= 32768)
        {
            var path = new StringBuilder(capacity);
            var length = GetFinalPathNameByHandleW(file, path, (uint)path.Capacity, 0);
            if (length == 0) throw new InvalidOperationException("process-identity");
            if (length < path.Capacity)
            {
                var value = path.ToString();
                return value.StartsWith("\\\\?\\", StringComparison.Ordinal) ? value.Substring(4) : value;
            }
            capacity *= 2;
        }
        throw new InvalidOperationException("process-identity");
    }

    private static FileIdentity OpenExecutableIdentity(string path, out IntPtr handle)
    {
        handle = CreateFileW(
            path,
            GenericRead,
            FileShareRead | FileShareWrite,
            IntPtr.Zero,
            OpenExisting,
            FileFlagOpenReparsePoint,
            IntPtr.Zero);
        if (handle == IntPtr.Zero || handle == new IntPtr(-1))
            throw new InvalidOperationException("process-resolve");
        try
        {
            ByHandleFileInformation information;
            if (!GetFileInformationByHandle(handle, out information))
                throw new InvalidOperationException("process-identity");
            if ((information.FileAttributes & 0x10) != 0 ||
                (information.FileAttributes & 0x400) != 0)
                throw new InvalidOperationException("process-identity");
            return new FileIdentity
            {
                Volume = information.VolumeSerialNumber,
                Index = ((ulong)information.FileIndexHigh << 32) | information.FileIndexLow,
                Links = information.NumberOfLinks,
                Attributes = information.FileAttributes,
                FinalPath = GetFinalPathByHandle(handle)
            };
        }
        catch
        {
            CloseIfOpen(ref handle);
            throw;
        }
    }

    private static bool SameFileIdentity(FileIdentity left, FileIdentity right)
    {
        return left != null && right != null && left.Volume == right.Volume &&
            left.Index == right.Index && left.Links == right.Links &&
            string.Equals(left.FinalPath, right.FinalPath, StringComparison.OrdinalIgnoreCase);
    }

    private static FileIdentity ValidateExecutableIdentity(
        string resolvedExecutable,
        IntPtr process,
        FileIdentity expected)
    {
        if (string.IsNullOrWhiteSpace(resolvedExecutable) ||
            !Path.IsPathRooted(resolvedExecutable) || expected == null)
            throw new InvalidOperationException("process-resolve");
        var capacity = 512;
        var image = new StringBuilder(capacity);
        uint imageLength = (uint)image.Capacity;
        if (!QueryFullProcessImageNameW(process, 0, image, ref imageLength))
            throw new InvalidOperationException("process-identity");
        var childPath = image.ToString();
        IntPtr childHandle;
        var actual = OpenExecutableIdentity(childPath, out childHandle);
        try
        {
            if (!SameFileIdentity(expected, actual) ||
                !string.Equals(expected.FinalPath, childPath, StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException("process-identity");
            return actual;
        }
        finally { CloseIfOpen(ref childHandle); }
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

    private static string GetProcessImagePath(IntPtr process)
    {
        if (process == IntPtr.Zero) throw new InvalidOperationException("process-identity");
        var capacity = 512;
        while (capacity <= 32768)
        {
            var image = new StringBuilder(capacity);
            uint imageLength = (uint)image.Capacity;
            if (QueryFullProcessImageNameW(process, 0, image, ref imageLength))
            {
                var path = image.ToString();
                if (!string.IsNullOrWhiteSpace(path)) return path;
            }
            capacity *= 2;
        }
        throw new InvalidOperationException("process-identity");
    }

    private static FileIdentity GetProcessExecutableIdentity(
        IntPtr process,
        out string executablePath)
    {
        executablePath = GetProcessImagePath(process);
        IntPtr identityHandle;
        var identity = OpenExecutableIdentity(executablePath, out identityHandle);
        CloseIfOpen(ref identityHandle);
        return identity;
    }

    private static bool SameTrackedProcessIdentity(
        TrackedProcess existing,
        uint processId,
        long creationTime,
        string executablePath,
        FileIdentity executableIdentity)
    {
        return existing != null && existing.ProcessId == processId &&
            existing.CreationTime == creationTime &&
            string.Equals(
                existing.ExecutablePath,
                executablePath,
                StringComparison.OrdinalIgnoreCase) &&
            SameFileIdentity(existing.ExecutableIdentity, executableIdentity);
    }

    private static List<TrackedProcess> FindDescendants(NativeProcess root, DateTime deadline)
    {
        var tracked = new List<TrackedProcess>();
        if (root == null || root.ProcessId == 0 || root.CreationTime == 0) return tracked;
        var parents = new List<TrackedProcess>
        {
            new TrackedProcess
            {
                ProcessId = root.ProcessId,
                CreationTime = root.CreationTime,
                Handle = root.ProcessHandle,
                ExecutablePath = root.ExecutablePath,
                ExecutableIdentity = root.ExecutableIdentity
            }
        };
        var snapshotCount = 0;
        while (snapshotCount++ < MaxTrackedProcesses)
        {
            int remainingMilliseconds;
            if (!Remaining(deadline, out remainingMilliseconds)) break;
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
                    TrackedProcess parentMatch = null;
                    foreach (var parent in parents)
                    {
                        if (parent.ProcessId == entry.ParentProcessId)
                        {
                            parentMatch = parent;
                            break;
                        }
                    }
                    if (parentMatch == null) continue;
                    var parentHandle = OpenProcess(
                        ProcessQueryInformation | ProcessQueryLimitedInformation | Synchronize,
                        false,
                        entry.ParentProcessId);
                    if (parentHandle == IntPtr.Zero) continue;
                    var parentCreation = GetCreationTime(parentHandle);
                    CloseHandle(parentHandle);
                    // Exact generation relation: candidate parent CreationTime == parent.CreationTime.
                    if (parentCreation == 0 || parentCreation != parentMatch.CreationTime) continue;
                    if (parents.Count >= MaxTrackedProcesses) break;
                    var handle = OpenProcess(
                        ProcessTerminate | ProcessQueryInformation | ProcessQueryLimitedInformation | Synchronize,
                        false,
                        entry.ProcessId);
                    if (handle == IntPtr.Zero) continue;
                    var creationTime = GetCreationTime(handle);
                    if (creationTime == 0)
                    {
                        CloseHandle(handle);
                        continue;
                    }
                    string executablePath;
                    FileIdentity executableIdentity;
                    try
                    {
                        executableIdentity = GetProcessExecutableIdentity(handle, out executablePath);
                    }
                    catch
                    {
                        // An inaccessible or already-unlinked image cannot be
                        // safely attributed to this Job. Do not claim it as a
                        // managed descendant without a full executable proof.
                        CloseHandle(handle);
                        continue;
                    }
                    var pidIdentityConflict = false;
                    var alreadyKnown = false;
                    foreach (var existing in parents)
                    {
                        if (existing.ProcessId == entry.ProcessId)
                        {
                            // Dedupe is the complete process generation:
                            // PID + creation time + canonical executable path
                            // + opened executable file identity. Any mismatch
                            // for an already observed PID is ambiguous and is
                            // never adopted as an owned descendant.
                            alreadyKnown = SameTrackedProcessIdentity(
                                existing,
                                entry.ProcessId,
                                creationTime,
                                executablePath,
                                executableIdentity);
                            pidIdentityConflict = !alreadyKnown;
                            break;
                        }
                    }
                    if (alreadyKnown || pidIdentityConflict)
                    {
                        CloseHandle(handle);
                        continue;
                    }
                    var child = new TrackedProcess
                    {
                        ProcessId = entry.ProcessId,
                        CreationTime = creationTime,
                        Handle = handle,
                        ExecutablePath = executablePath,
                        ExecutableIdentity = executableIdentity
                    };
                    parents.Add(child);
                    tracked.Add(child);
                    added = true;
                }
                while (Remaining(deadline, out remainingMilliseconds) && Process32NextW(snapshot, ref entry));
            }
            finally { CloseHandle(snapshot); }
            if (!added) break;
        }
        return tracked;
    }

    private static bool TerminateTrackedProcesses(List<TrackedProcess> tracked, DateTime deadline)
    {
        var settled = true;
        foreach (var process in tracked)
        {
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

    private static bool TerminateTrackedDescendants(NativeProcess root, DateTime deadline)
    {
        return TerminateTrackedProcesses(FindDescendants(root, deadline), deadline);
    }

    private static bool TerminateAndWaitRoot(IntPtr processHandle, DateTime deadline)
    {
        if (processHandle == IntPtr.Zero) return true;
        try
        {
            if (WaitForSingleObject(processHandle, 0) != WaitObject0)
            {
                if (!TerminateProcess(processHandle, 1)) return false;
            }
            int milliseconds;
            if (WaitForSingleObject(processHandle, 0) == WaitObject0) return true;
            if (!Remaining(deadline, out milliseconds)) return false;
            return WaitForSingleObject(processHandle, (uint)Math.Max(1, milliseconds)) == WaitObject0;
        }
        catch { return false; }
    }

    private static NativeProcess CreateSuspendedProcess(
        string resolvedExecutable,
        string[] arguments,
        string[] environment,
        DateTime deadline)
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
        IntPtr executableIdentityHandle = IntPtr.Zero;
        FileIdentity expectedIdentity = null;
        NativeProcess nativeProcess = null;
        try
        {
            expectedIdentity = OpenExecutableIdentity(resolvedExecutable, out executableIdentityHandle);
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
            var commandLine = BuildCommandLine(resolvedExecutable, arguments);
            environmentBlock = BuildEnvironmentBlock(environment);
            var created = CreateProcessW(
                resolvedExecutable,
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
            nativeProcess = new NativeProcess
            {
                ProcessHandle = information.ProcessHandle,
                ThreadHandle = information.ThreadHandle,
                StandardInputWrite = parentInput,
                StandardOutputRead = parentOutput,
                StandardErrorRead = parentError,
                ProcessId = information.ProcessId,
                CreationTime = 0,
                ExecutablePath = resolvedExecutable,
                ExecutableIdentity = expectedIdentity
            };
            information.ProcessHandle = IntPtr.Zero;
            information.ThreadHandle = IntPtr.Zero;
            ValidateExecutableIdentity(resolvedExecutable, nativeProcess.ProcessHandle, expectedIdentity);
            nativeProcess.CreationTime = GetCreationTime(nativeProcess.ProcessHandle);
            if (nativeProcess.CreationTime == 0) throw new InvalidOperationException("process-identity");
            return nativeProcess;
        }
        catch
        {
            var rootCleanupProven = true;
            if (nativeProcess != null)
            {
                rootCleanupProven = TerminateAndWaitRoot(nativeProcess.ProcessHandle, deadline);
                CloseIfOpen(ref nativeProcess.StandardInputWrite);
                CloseIfOpen(ref nativeProcess.StandardOutputRead);
                CloseIfOpen(ref nativeProcess.StandardErrorRead);
                CloseIfOpen(ref nativeProcess.ThreadHandle);
                CloseIfOpen(ref nativeProcess.ProcessHandle);
            }
            else if (information.ProcessHandle != IntPtr.Zero)
            {
                rootCleanupProven = TerminateAndWaitRoot(information.ProcessHandle, deadline);
            }
            CloseIfOpen(ref childInput);
            CloseIfOpen(ref parentInput);
            CloseIfOpen(ref childOutput);
            CloseIfOpen(ref parentOutput);
            CloseIfOpen(ref childError);
            CloseIfOpen(ref parentError);
            CloseIfOpen(ref information.ThreadHandle);
            CloseIfOpen(ref information.ProcessHandle);
            throw new ProcessLaunchException("process-identity", rootCleanupProven);
        }
        finally
        {
            CloseIfOpen(ref executableIdentityHandle);
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
            if (!Remaining(deadline, out milliseconds) || milliseconds <= WorkReserveMilliseconds) return false;
            var winner = await Task.WhenAny(task, Task.Delay(milliseconds - WorkReserveMilliseconds)).ConfigureAwait(false);
            if (winner != task) return false;
        }
        return true;
    }

    private static IntPtr CreateOwnedJob()
    {
        var job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) return IntPtr.Zero;
        var limits = new ExtendedLimitInformation();
        limits.BasicLimitInformation.LimitFlags =
            JobObjectLimitKillOnJobClose | JobObjectLimitActiveProcess;
        limits.BasicLimitInformation.ActiveProcessLimit = (uint)MaxTrackedProcesses;
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

    private static DateTime CleanupDeadline(DateTime auditDeadline)
    {
        // Settlement remains inside the original absolute deadline and cannot
        // consume the final report-publication slice. If settlement cannot be
        // proven by this cutoff, ActiveProcessZero remains false.
        var now = DateTime.UtcNow;
        var cutoff = auditDeadline.AddMilliseconds(-ReportPublicationReserveMilliseconds);
        return cutoff > now ? cutoff : now;
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
        bool jobAssigned,
        CancellationTokenSource cancellation,
        Task stdoutTask,
        Task stderrTask,
        Task stdinTask,
        Task exitTask,
        DateTime deadline,
        string failure)
    {
        try { cancellation?.Cancel(); } catch { }
        var cleanupDeadline = CleanupDeadline(deadline);
        var processHandle = nativeProcess == null ? IntPtr.Zero : nativeProcess.ProcessHandle;
        // Snapshot owned descendants while the root generation is still
        // observable. A root-first kill can make the parent PID unavailable
        // before cleanup can prove the child relationship.
        var trackedBeforeRoot = new List<TrackedProcess>();
        var trackedBeforeRootSettled = true;
        if (nativeProcess != null)
        {
            try
            {
                trackedBeforeRoot = FindDescendants(nativeProcess, cleanupDeadline);
                trackedBeforeRootSettled = TerminateTrackedProcesses(trackedBeforeRoot, cleanupDeadline);
            }
            catch { trackedBeforeRootSettled = false; }
        }
        // Always settle the retained root handle directly. Job accounting is
        // only evidence for descendants after assignment; it cannot prove an
        // unassigned suspended root is gone.
        var rootZero = processHandle == IntPtr.Zero || TerminateAndWaitRoot(processHandle, cleanupDeadline);
        var descendantsZero = true;
        if (jobAssigned && job != IntPtr.Zero)
        {
            var jobTerminated = TerminateJobObject(job, 1);
            descendantsZero = trackedBeforeRootSettled && jobTerminated && WaitForActiveProcessZero(job, cleanupDeadline);
            if (!descendantsZero)
            {
                // Do not take repeated unbounded snapshots or infer ownership
                // from a recycled PID. The pre-root snapshot above is the
                // single bounded generation-checked fallback.
                descendantsZero = trackedBeforeRootSettled &&
                    WaitForActiveProcessZero(job, cleanupDeadline);
            }
        }
        else if (nativeProcess != null)
        {
            // Assignment never succeeded, so a descendant snapshot is not
            // authoritative. Keep the failure visible instead of claiming a
            // Job ACTIVE_PROCESS_ZERO result.
            descendantsZero = false;
        }
        var zero = rootZero && descendantsZero;

        var cleanupTasks = new List<Task>();
        if (stdoutTask != null) cleanupTasks.Add(stdoutTask);
        if (stderrTask != null) cleanupTasks.Add(stderrTask);
        if (stdinTask != null) cleanupTasks.Add(stdinTask);
        if (exitTask != null) cleanupTasks.Add(exitTask);
        if (cleanupTasks.Count > 0)
        {
            var cleanup = Task.WhenAll(cleanupTasks.ToArray());
            var cleanupCompleted = await WaitUntilAsync(cleanup, cleanupDeadline).ConfigureAwait(false);
        }
        return new CutoverProcessResultV2
        {
            Success = false,
            // Preserve the originating bounded failure category; settlement
            // proof is carried separately by ActiveProcessZero and must never
            // be upgraded merely because a Job was not assigned.
            FailureCategory = failure,
            ExitCode = -1,
            StandardOutput = Array.Empty<byte>(),
            StandardError = Array.Empty<byte>(),
            ActiveProcessZero = zero
        };
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);

    public static CutoverProcessResultV2 Run(
        string resolvedExecutable,
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
        var jobAssigned = false;
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
            nativeProcess = CreateSuspendedProcess(resolvedExecutable, arguments, environment, deadline);
            if (!AssignProcessToJobObject(job, nativeProcess.ProcessHandle))
            {
                return AbortAsync(nativeProcess, job, jobAssigned, cancellation, null, null, null, null, deadline, "ownership").GetAwaiter().GetResult();
            }
            jobAssigned = true;
            if (ResumeThread(nativeProcess.ThreadHandle) == UInt32.MaxValue)
            {
                return AbortAsync(nativeProcess, job, jobAssigned, cancellation, null, null, null, null, deadline, "start").GetAwaiter().GetResult();
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
                    catch (OutputLimitException) { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdout-overflow").GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdout-error").GetAwaiter().GetResult(); }
                    stdoutChecked = true;
                }
                if (stderrTask.IsCompleted && !stderrChecked)
                {
                    try { stderr = stderrTask.GetAwaiter().GetResult(); }
                    catch (OutputLimitException) { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stderr-overflow").GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stderr-error").GetAwaiter().GetResult(); }
                    stderrChecked = true;
                }
                if (stdinTask.IsCompleted && !stdinChecked)
                {
                    try { stdinTask.GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "stdin-error").GetAwaiter().GetResult(); }
                    stdinChecked = true;
                }
                if (exitTask.IsCompleted && !exitChecked)
                {
                    try { exitTask.GetAwaiter().GetResult(); }
                    catch { return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "exit-error").GetAwaiter().GetResult(); }
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
                    return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "timeout").GetAwaiter().GetResult();
                }
            }
            uint exitCode;
            if (!GetExitCodeProcess(nativeProcess.ProcessHandle, out exitCode))
            {
                return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "exit-error").GetAwaiter().GetResult();
            }
            // Process exit and Job accounting are observed independently on
            // Windows. Give the Job a bounded settlement window before
            // treating a still-visible count as an owned descendant.
            if (!ActiveProcessZero(job) && !WaitForActiveProcessZero(job, CleanupDeadline(deadline)))
            {
                return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "descendant").GetAwaiter().GetResult();
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
        catch (ProcessLaunchException launch)
        {
            return new CutoverProcessResultV2
            {
                Success = false,
                FailureCategory = launch.FailureCategory,
                ExitCode = -1,
                StandardOutput = Array.Empty<byte>(),
                StandardError = Array.Empty<byte>(),
                ActiveProcessZero = launch.RootCleanupProven
            };
        }
        catch
        {
            return AbortAsync(nativeProcess, job, jobAssigned, cancellation, stdoutTask, stderrTask, stdinTask, exitTask, deadline, "process-error").GetAwaiter().GetResult();
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

function Read-CutoverNulDelimitedPaths {
    param(
        [Parameter(Mandatory = $true)][byte[]]$Bytes,
        [Parameter(Mandatory = $true)][int]$MaxPaths
    )

    if ($Bytes.LongLength -gt $maxTrackedBytes) {
        throw 'Git output exceeded its bounded aggregate size.'
    }
    $decoder = [System.Text.UTF8Encoding]::new($false, $true)
    $paths = New-Object 'System.Collections.Generic.List[string]'
    $start = 0
    for ($index = 0; $index -lt $Bytes.Length; $index++) {
        Assert-CutoverWorkDeadline
        if ($Bytes[$index] -ne 0) { continue }
        if ($paths.Count -ge $MaxPaths) {
            throw 'Git path count exceeded its bounded limit.'
        }
        $length = $index - $start
        if ($length -le 0 -or $length -gt ($maxEnvironmentEntryChars * 4)) {
            throw 'Git path entry exceeded its bounded length.'
        }
        try {
            $path = $decoder.GetString($Bytes, $start, $length)
        }
        catch { throw 'git ls-files returned invalid UTF-8.' }
        if ($path.Length -gt $maxEnvironmentEntryChars) {
            throw 'Git path entry exceeded its bounded character length.'
        }
        $paths.Add($path)
        $start = $index + 1
    }
    if ($start -ne $Bytes.Length) {
        throw 'git ls-files output was not NUL terminated.'
    }
    return @($paths.ToArray())
}

function Read-CutoverUtf8Lines {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][byte[]]$Bytes,
        [Parameter(Mandatory = $true)][int64]$MaxBytes,
        [Parameter(Mandatory = $true)][int]$MaxLines,
        [Parameter(Mandatory = $true)][int]$MaxLineChars
    )

    if ($MaxBytes -le 0 -or $MaxLines -le 0 -or $MaxLineChars -le 0) {
        throw 'process text bounds were invalid.'
    }
    if ($Bytes.LongLength -gt $MaxBytes) {
        throw 'process text exceeded its bounded aggregate size.'
    }
    Assert-CutoverWorkDeadline
    try {
        $text = ([System.Text.UTF8Encoding]::new($false, $true)).GetString($Bytes)
    }
    catch { throw 'process output was not valid UTF-8.' }
    Assert-CutoverWorkDeadline
    $reader = [System.IO.StringReader]::new($text)
    $lines = New-Object 'System.Collections.Generic.List[string]'
    try {
        while ($null -ne ($line = $reader.ReadLine())) {
            Assert-CutoverWorkDeadline
            if ($lines.Count -ge $MaxLines) {
                throw 'process line count exceeded its bounded limit.'
            }
            if ($line.Length -gt $MaxLineChars) {
                throw 'process line exceeded its bounded length.'
            }
            $lines.Add($line)
        }
    }
    finally { $reader.Dispose() }
    return @($lines.ToArray())
}

function Read-CutoverContractLines {
    param([Parameter(Mandatory = $true)][string]$Source)

    $reader = [System.IO.StringReader]::new($Source)
    $builder = [System.Text.StringBuilder]::new()
    $openingCount = 0
    $inside = $false
    $closed = $false
    $lineCount = 0
    $bodyLineCount = 0
    $bodyBytes = [int64]0
    try {
        while ($null -ne ($line = $reader.ReadLine())) {
            Assert-CutoverWorkDeadline
            $lineCount++
            if ($lineCount -gt $maxLedgerLines) {
                throw 'ledger line count exceeded its bounded limit.'
            }
            if (-not $inside -and -not $closed -and $line -eq '```json cutover-contract') {
                $openingCount++
                $inside = $true
                continue
            }
            if (-not $inside -and $line -eq '```json cutover-contract') {
                $openingCount++
                continue
            }
            if ($inside -and $line -eq '```') {
                $inside = $false
                $closed = $true
                continue
            }
            if (-not $inside) { continue }
            $bodyLineCount++
            $nextBytes = [System.Text.Encoding]::UTF8.GetByteCount($line) + 1
            if ($bodyBytes -gt ($maxLedgerBytes - $nextBytes)) {
                throw 'ledger contract body exceeded its bounded aggregate size.'
            }
            $bodyBytes += $nextBytes
            $null = $builder.AppendLine($line)
        }
    }
    finally { $reader.Dispose() }
    if ($openingCount -ne 1) {
        throw "Ledger must contain exactly one ```json cutover-contract block."
    }
    if ($inside -or -not $closed -or $bodyLineCount -eq 0) {
        throw 'Ledger contract JSON block is missing its closing fence or is empty.'
    }
    return $builder.ToString().TrimEnd([char[]]@("`r", "`n"))
}

function Invoke-CutoverProcessLines {
    param(
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][byte[]]$InputBytes,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    Assert-CutoverProcessStartDeadline
    Initialize-CutoverProcessMethodsV2
    $resolvedExecutable = Resolve-CutoverExecutable -FileName $FileName
    $environment = Get-CutoverProcessEnvironment -ResolvedExecutable $resolvedExecutable
    $result = [CutoverProcessMethodsV2]::Run(
        $resolvedExecutable,
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
        $boundedLines = @(Read-CutoverUtf8Lines `
                -Bytes $result.StandardOutput `
                -MaxBytes $MaxBytes `
                -MaxLines $maxScannerOutputLines `
                -MaxLineChars $maxScannerOutputLineChars)
    }
    catch {
        Add-SafetyBound
        throw 'bounded scanner output shape was invalid.'
    }
    $lines = @($boundedLines | Where-Object { $_ -ne '' })
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
        [Parameter(Mandatory = $true)][object]$ParentHandle,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    if ([System.IO.Path]::GetFileName($LiteralPath).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to publish the protected exact session.json name.'
    }
    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'report path'
    $authorizedEvidenceRoot = Normalize-CutoverAbsolutePath `
        -LiteralPath $EvidenceRoot `
        -Label 'evidence root'
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor $authorizedEvidenceRoot)) {
        throw 'report path escaped the authorized evidence root.'
    }
    $parent = Normalize-CutoverAbsolutePath `
        -LiteralPath (Split-Path -Parent $full) `
        -Label 'report parent'
    if (-not [string]::Equals(
            [string]$ParentHandle.path,
            [string]$parent,
            [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'report parent did not match the retained publication handle.'
    }
    $leaf = [System.IO.Path]::GetFileName($full)
    if ([string]::IsNullOrWhiteSpace($leaf) -or
        $leaf -notmatch '^[^\\/:\x00-\x1F\x7F]+\.(json|txt)$') {
        throw 'report leaf name was rejected.'
    }
    Assert-CutoverPublicationAuthority -ParentHandle $ParentHandle -ExpectedParentPath $parent
    $tempLeaf = '.pending-{0}.tmp' -f ([guid]::NewGuid().ToString('N'))
    $tempPath = Join-Path $parent $tempLeaf
    $tempHandle = $null
    $published = $false
    $encoding = [System.Text.UTF8Encoding]::new($false)
    $bytes = $encoding.GetBytes($Text)
    try {
        if ($bytes.LongLength -gt $MaxBytes) {
            Add-SafetyBound
            throw 'report text exceeded its bounded output byte limit.'
        }
        $tempHandle = Open-CutoverRelativeWriteFile `
            -ParentStream $ParentHandle.stream `
            -ParentPath $parent `
            -LeafName $tempLeaf `
            -AuthorizedRoot $EvidenceRoot
        $tempIdentity = $tempHandle.identity
        if (-not [string]::Equals(
                [string]$tempIdentity.finalPath,
                [string]$tempPath,
                [System.StringComparison]::OrdinalIgnoreCase)) {
            throw 'temporary report handle identity did not match its created path.'
        }
        Assert-CutoverPublicationAuthority -ParentHandle $ParentHandle -ExpectedParentPath $parent
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
        finally { }

        Assert-CutoverPublicationAuthority -ParentHandle $ParentHandle -ExpectedParentPath $parent
        $tempAfter = Get-CutoverHandleIdentity -Stream $tempHandle.stream
        if (-not (Compare-CutoverStableFileIdentity -Before $tempIdentity -After $tempAfter)) {
            throw 'temporary report handle identity changed before replacement.'
        }
        Assert-CutoverDeadline
        if (-not (Test-CutoverCurrentReportPath -LiteralPath $full -EvidenceRoot $EvidenceRoot)) {
            throw "Refusing to overwrite a non-current report path: '$full'."
        }
        # Mutation is relative to the already verified, no-follow parent
        # handle. A concurrent pathname/junction swap cannot redirect this
        # rename outside the directory represented by that handle.
        Rename-CutoverFileRelative `
            -FileStream $tempHandle.stream `
            -ParentStream $ParentHandle.stream `
            -LeafName $leaf `
            -ReplaceExisting
        # Success of the handle-relative rename is the publication commit
        # point. Root and every parent handle still deny delete sharing here;
        # all fallible identity/authority checks completed before this call.
        $published = $true
    }
    catch {
        if ($null -ne $tempHandle -and -not $published) {
            Remove-CutoverFileByHandle -FileStream $tempHandle.stream
        }
        throw
    }
    finally {
        if ($null -ne $tempHandle) { $tempHandle.stream.Dispose() }
    }
}

function New-BoundedAuditReport {
    param(
        [Parameter(Mandatory = $true)][object]$Report,
        [ValidateRange(0, 32768)][int]$HumanOmittedLineCount = 0
    )

    $boundedBlockers = New-Object 'System.Collections.Generic.List[string]'
    $boundedBlockers.Add($safetyDiagnostic)
    foreach ($retainedBlocker in @(
            'audit[remote_change_protected]',
            'audit[remote_change_unattributed]',
            'audit[process_deadline_exceeded]'
        )) {
        if (@(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'blockers')) -contains $retainedBlocker) {
            $boundedBlockers.Add($retainedBlocker)
        }
    }

    return [pscustomobject]([ordered]@{
            schemaVersion = 1
            contractId = Get-CutoverReportContractId -Value (Get-ContractProperty -Object $Report -Name 'contractId')
            mode = [string](Get-ContractProperty -Object $Report -Name 'mode')
            contractStatus = 'HOLD'
            ledgerPath = 'docs/replacement-deletion-ledger.md'
            trackedFileCount = [int](Get-ContractProperty -Object $Report -Name 'trackedFileCount')
            protectedFilesSkipped = @()
            contractErrors = @()
            blockers = @($boundedBlockers.ToArray())
            entrypointFindings = @()
            compatibilityFindings = @()
            packagingFindings = @()
            remainingIntegratedPrerequisites = @()
            productEntrypoints = @()
            packagingHandoff = $null
            isolation = [pscustomobject]$isolationReport
            installedApp = [pscustomobject]$installedAppReport
            prerequisiteNodes = @()
            rows = @()
            remoteChangeAttribution = $remoteChangeAttribution
            safety = [ordered]@{
                boundReached = $true
                diagnostic = $safetyDiagnostic
                humanReportTruncated = $HumanOmittedLineCount -gt 0
                humanReportOmittedLineCount = $HumanOmittedLineCount
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
                    scannerOutputLines = $maxScannerOutputLines
                    scannerOutputLineCharacters = $maxScannerOutputLineChars
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
                maxOutputLines = $maxScannerOutputLines
                maxOutputLineCharacters = $maxScannerOutputLineChars
                deadlineMilliseconds = $maxScannerDurationMs
            }
        })
}

function Assert-CutoverReportBounds {
    param([Parameter(Mandatory = $true)][object]$Report)

    $stringState = [pscustomobject]@{ bytes = [int64]0; count = 0 }
    $addString = {
        param([AllowEmptyString()][string]$Value)
        Assert-CutoverDeadline
        if ($null -eq $Value) { return }
        $stringState.count++
        if ($stringState.count -gt $maxReportStrings -or $Value.Length -gt $maxEnvironmentEntryChars) {
            throw 'report string shape exceeded its bounded limit.'
        }
        $bytes = [System.Text.Encoding]::UTF8.GetByteCount($Value)
        if ($stringState.bytes -gt ($maxReportJsonBytes - $bytes)) {
            throw 'report aggregate string shape exceeded its bounded limit.'
        }
        $stringState.bytes += $bytes
    }
    & $addString ([string]$Report.contractId)
    & $addString ([string]$Report.mode)
    & $addString ([string]$Report.remoteChangeAttribution.classification)
    & $addString ([string]$Report.remoteChangeAttribution.writer)
    foreach ($category in @($Report.remoteChangeAttribution.changedCategories)) {
        & $addString ([string]$category)
    }
    foreach ($value in @(
            @($Report.contractErrors) +
            @($Report.blockers) +
            @($Report.entrypointFindings) +
            @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'compatibilityFindings')) +
            @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'packagingFindings')) +
            @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'remainingIntegratedPrerequisites')) +
            @($Report.protectedFilesSkipped)
        )) {
        & $addString ([string]$value)
    }
    foreach ($entrypoint in @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'productEntrypoints'))) {
        & $addString ([string](Get-ContractProperty -Object $entrypoint -Name 'id'))
        & $addString ([string](Get-ContractProperty -Object $entrypoint -Name 'role'))
        & $addString ([string](Get-ContractProperty -Object $entrypoint -Name 'path'))
    }
    $isolation = Get-ContractProperty -Object $Report -Name 'isolation'
    if ($null -ne $isolation) {
        foreach ($name in @('remappedAppData', 'setDevmanagerProfile', 'inheritedDevmanagerProfileCleared', 'productionRootRead', 'evidenceRootBeneathWorktree')) {
            & $addString ([string](Get-ContractProperty -Object $isolation -Name $name))
        }
    }
    $installedApp = Get-ContractProperty -Object $Report -Name 'installedApp'
    if ($null -ne $installedApp) {
        foreach ($name in @('observedInstalledProcesses', 'openSessionJson', 'hashProductionFiles', 'installPublishDeleteUserData')) {
            & $addString ([string](Get-ContractProperty -Object $installedApp -Name $name))
        }
    }
    $packaging = Get-ContractProperty -Object $Report -Name 'packagingHandoff'
    if ($null -ne $packaging) {
        foreach ($binary in @(Get-ContractArray (Get-ContractProperty -Object $packaging -Name 'requiredBinaries'))) {
            & $addString ([string]$binary)
        }
        foreach ($token in @(Get-ContractArray (Get-ContractProperty -Object $packaging -Name 'missingManifestTokens'))) {
            & $addString ([string]$token)
        }
        foreach ($file in @(Get-ContractArray (Get-ContractProperty -Object $packaging -Name 'requiredFiles'))) {
            & $addString ([string](Get-ContractProperty -Object $file -Name 'path'))
        }
        & $addString ([string](Get-ContractProperty -Object $packaging -Name 'packagerManifest'))
    }
    $nodes = @($Report.prerequisiteNodes)
    $rows = @($Report.rows)
    if ($nodes.Count -gt $maxNodes -or $rows.Count -gt $maxRows) {
        throw 'report collection shape exceeded its bounded count.'
    }
    foreach ($node in $nodes) {
        & $addString ([string]$node.id)
        & $addString ([string]$node.kind)
        & $addString ([string]$node.status)
        foreach ($dependency in @($node.dependsOn)) { & $addString ([string]$dependency) }
        foreach ($artifact in @($node.evidence)) { & $addString ([string]$artifact.path) }
    }
    foreach ($row in $rows) {
        & $addString ([string]$row.id)
        & $addString ([string]$row.status)
        & $addString ([string](Get-ContractProperty -Object $row -Name 'cutoverAction'))
        & $addString ([string]$row.legacy.path)
        & $addString ([string]$row.replacementOwner.path)
        & $addString ([string]$row.replacementOwner.symbol)
        foreach ($testClaim in @($row.tests)) {
            if ($testClaim -is [string]) {
                continue
            }
            & $addString ([string](Get-ContractProperty -Object $testClaim -Name 'kind'))
            & $addString ([string](Get-ContractProperty -Object $testClaim -Name 'path'))
            & $addString ([string](Get-ContractProperty -Object $testClaim -Name 'filter'))
        }
        & $addString ([string]$row.e2eProof.artifact)
        & $addString ([string]$row.e2eProof.kind)
        & $addString ([string]$row.productionImpact.profile)
        foreach ($preserve in @($row.productionImpact.preserves)) { & $addString ([string]$preserve) }
        foreach ($neverTouches in @($row.productionImpact.neverTouches)) { & $addString ([string]$neverTouches) }
        foreach ($deletionPath in @($row.deletionSet.paths)) { & $addString ([string]$deletionPath) }
        foreach ($deletionPath in @($row.deletionSet.present)) { & $addString ([string]$deletionPath) }
        foreach ($prerequisite in @($row.prerequisites)) { & $addString ([string]$prerequisite) }
        foreach ($kind in @('path', 'symbol', 'token')) {
            foreach ($reference in @($row.references[$kind])) { & $addString ([string]$reference) }
        }
        foreach ($artifact in @($row.evidence.artifacts)) { & $addString ([string]$artifact.path) }
        foreach ($blocker in @($row.blockers)) { & $addString ([string]$blocker) }
    }
}

function Write-AuditReports {
    param(
        [Parameter(Mandatory = $true)][object]$Report,
        [Parameter(Mandatory = $true)][string]$JsonPath,
        [Parameter(Mandatory = $true)][string]$TextPath,
        [Parameter(Mandatory = $true)][string]$EvidenceRoot,
        [Parameter(Mandatory = $true)][object]$ParentHandle,
        [Parameter(Mandatory = $true)][ref]$ContractStatus
    )

    Assert-CutoverReportBounds -Report $Report
    $json = $Report | ConvertTo-Json -Depth 50
    $jsonBytes = [System.Text.UTF8Encoding]::new($false).GetByteCount($json)
    if ($jsonBytes -gt $maxReportJsonBytes) {
        $Report = New-BoundedAuditReport -Report $Report
        $ContractStatus.Value = 'HOLD'
        Assert-CutoverReportBounds -Report $Report
        $json = $Report | ConvertTo-Json -Depth 50
    }

    $lines = New-Object 'System.Collections.Generic.List[string]'
    $lineBytes = [int64]0
    $lineBytesRef = [ref]$lineBytes
    $humanTruncated = $false
    $humanTruncatedRef = [ref]$humanTruncated
    $humanOmittedLineCount = 0
    $humanOmittedLineCountRef = [ref]$humanOmittedLineCount
    $addLine = {
        param([string]$Line)
        if ($humanTruncatedRef.Value) {
            $humanOmittedLineCountRef.Value++
            return $false
        }
        $candidate = [string]$Line + [Environment]::NewLine
        $candidateBytes = [System.Text.UTF8Encoding]::new($false).GetByteCount($candidate)
        if ($lineBytesRef.Value + $candidateBytes -gt $maxReportHumanBytes) {
            $humanTruncatedRef.Value = $true
            $humanOmittedLineCountRef.Value++
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
    $null = & $addLine "remote change attribution: $($Report.remoteChangeAttribution.classification); writer=$($Report.remoteChangeAttribution.writer)"
    $null = & $addLine ''
    $null = & $addLine 'contract errors:'
    foreach ($error in @($Report.contractErrors)) { $null = & $addLine "- $error" }
    $null = & $addLine 'blockers:'
    foreach ($blocker in @($Report.blockers)) { $null = & $addLine "- $blocker" }
    $null = & $addLine 'rows:'
    foreach ($row in @($Report.rows)) {
        $null = & $addLine "- $($row.id): $($row.status); legacy=$($row.legacy.path); present=$([bool]$row.legacy.pathPresent)"
        foreach ($blocker in @($row.blockers)) { $null = & $addLine "  blocker: $blocker" }
    }
    $null = & $addLine 'forbidden entrypoint findings:'
    foreach ($finding in @($Report.entrypointFindings)) { $null = & $addLine "- $finding" }
    $null = & $addLine 'compatibility findings:'
    foreach ($finding in @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'compatibilityFindings'))) {
        $null = & $addLine "- $finding"
    }
    $null = & $addLine 'packaging/update handoff:'
    $packagingReport = Get-ContractProperty -Object $Report -Name 'packagingHandoff'
    if ($null -eq $packagingReport) {
        $null = & $addLine '- none'
    }
    else {
        $null = & $addLine ("- binaries: {0}" -f ((@(Get-ContractArray (Get-ContractProperty -Object $packagingReport -Name 'requiredBinaries')) -join ', ')))
        $null = & $addLine ("- atomic two-binary identity: {0}" -f (Get-ContractProperty -Object $packagingReport -Name 'atomicTwoBinaryIdentity'))
        $null = & $addLine ("- install/publish forbidden: {0}" -f (Get-ContractProperty -Object $packagingReport -Name 'forbidInstallOrPublish'))
        foreach ($file in @(Get-ContractArray (Get-ContractProperty -Object $packagingReport -Name 'requiredFiles'))) {
            $null = & $addLine ("- required file: {0}; present={1}" -f (Get-ContractProperty -Object $file -Name 'path'), (Get-ContractProperty -Object $file -Name 'present'))
        }
    }
    $null = & $addLine ("installed app touched: {0}" -f (Get-ContractProperty -Object (Get-ContractProperty -Object $Report -Name 'installedApp') -Name 'observedInstalledProcesses'))
    $null = & $addLine ("session.json opened: {0}" -f (Get-ContractProperty -Object (Get-ContractProperty -Object $Report -Name 'installedApp') -Name 'openSessionJson'))
    $null = & $addLine ("DEVMANAGER_PROFILE set: {0}" -f (Get-ContractProperty -Object (Get-ContractProperty -Object $Report -Name 'isolation') -Name 'setDevmanagerProfile'))
    $human = ($lines -join [Environment]::NewLine) + [Environment]::NewLine
    if ($humanTruncated -or [System.Text.UTF8Encoding]::new($false).GetByteCount($human) -gt $maxReportHumanBytes) {
        Add-SafetyBound
        $Report = New-BoundedAuditReport `
            -Report $Report `
            -HumanOmittedLineCount $humanOmittedLineCount
        $ContractStatus.Value = 'HOLD'
        Assert-CutoverReportBounds -Report $Report
        $json = $Report | ConvertTo-Json -Depth 50
        if ([System.Text.UTF8Encoding]::new($false).GetByteCount($json) -gt $maxReportJsonBytes) {
            $Report = New-BoundedAuditReport `
                -Report $Report `
                -HumanOmittedLineCount $humanOmittedLineCount
            Assert-CutoverReportBounds -Report $Report
            $json = $Report | ConvertTo-Json -Depth 50
        }
        $boundedHumanBlockers = @(Get-ContractArray (Get-ContractProperty -Object $Report -Name 'blockers'))
        $human = "Phase 11.1 cutover audit`nstatus: HOLD`nblockers: $($boundedHumanBlockers -join ',')`n- report content omitted due to safety bound; omitted lines: $humanOmittedLineCount`n"
        if ([System.Text.UTF8Encoding]::new($false).GetByteCount($human) -gt $maxReportHumanBytes) {
            $human = "HOLD`n$($boundedHumanBlockers -join ',')`nomitted=$humanOmittedLineCount`n"
        }
        if ([System.Text.UTF8Encoding]::new($false).GetByteCount($human) -gt $maxReportHumanBytes) {
            throw 'bounded human HOLD exceeded its output byte limit.'
        }
    }

    # JSON is the authoritative publication gate. Publish a small typed HOLD
    # before attempting either user-facing output or the final JSON. If any
    # later write fails, a prior READY JSON can never remain current.
    $guardReport = New-BoundedAuditReport -Report $Report
    Assert-CutoverReportBounds -Report $guardReport
    $guardJson = $guardReport | ConvertTo-Json -Depth 50
    if ([System.Text.UTF8Encoding]::new($false).GetByteCount($guardJson) -gt $maxReportJsonBytes) {
        throw 'bounded publication guard exceeded its output byte limit.'
    }
    Write-CutoverAtomicUtf8 `
        -LiteralPath $JsonPath `
        -Text $guardJson `
        -EvidenceRoot $EvidenceRoot `
        -ParentHandle $ParentHandle `
        -MaxBytes $maxReportJsonBytes

    if ($authorizedRootKind -eq 'authenticated-fixture') {
        $moveTarget = [Environment]::GetEnvironmentVariable('DEVMANAGER_CUTOVER_TEST_MOVE_ROOT_AFTER_GUARD')
        if (-not [string]::IsNullOrWhiteSpace($moveTarget)) {
            $normalizedMoveTarget = Normalize-CutoverAbsolutePath `
                -LiteralPath $moveTarget `
                -Label 'fixture publication move target'
            if ((Test-CutoverPathEqualsOrBeneath -Path $normalizedMoveTarget -Ancestor $rootPath) -or
                (Test-CutoverPathEqualsOrBeneath -Path $rootPath -Ancestor $normalizedMoveTarget) -or
                [System.IO.Directory]::Exists($normalizedMoveTarget) -or
                [System.IO.File]::Exists($normalizedMoveTarget)) {
                throw 'fixture publication move target was unsafe.'
            }
            Assert-CutoverPathChain -LiteralPath (Split-Path -Parent $normalizedMoveTarget) | Out-Null
            $moveWasBlocked = $false
            try {
                [System.IO.Directory]::Move($rootPath, $normalizedMoveTarget)
            }
            catch {
                $moveWasBlocked = $true
            }
            if (-not $moveWasBlocked -or
                -not [System.IO.Directory]::Exists($rootPath) -or
                [System.IO.Directory]::Exists($normalizedMoveTarget)) {
                throw 'retained publication root could be moved during report publication.'
            }
            [Console]::Out.WriteLine('FIXTURE_PUBLICATION_ROOT_MOVE_BLOCKED')
        }

        if ([Environment]::GetEnvironmentVariable('DEVMANAGER_CUTOVER_TEST_FAIL_HUMAN_AFTER_GUARD') -eq '1') {
            [Console]::Out.WriteLine('FIXTURE_HUMAN_PUBLICATION_FAILURE_INJECTED')
            throw 'fixture injected human report publication failure.'
        }
    }

    Write-CutoverAtomicUtf8 `
        -LiteralPath $TextPath `
        -Text $human `
        -EvidenceRoot $EvidenceRoot `
        -ParentHandle $ParentHandle `
        -MaxBytes $maxReportHumanBytes
    if ($authorizedRootKind -eq 'authenticated-fixture' -and
        [Environment]::GetEnvironmentVariable('DEVMANAGER_CUTOVER_TEST_FAIL_FINAL_JSON_AFTER_HUMAN') -eq '1') {
        [Console]::Out.WriteLine('FIXTURE_FINAL_JSON_PUBLICATION_FAILURE_INJECTED')
        throw 'fixture injected final JSON publication failure.'
    }
    Write-CutoverAtomicUtf8 `
        -LiteralPath $JsonPath `
        -Text $json `
        -EvidenceRoot $EvidenceRoot `
        -ParentHandle $ParentHandle `
        -MaxBytes $maxReportJsonBytes
}

try {
    $rootPath = Assert-CutoverAuthorizedRoot -RequestedRoot $Root
    $requestedProfile = [Environment]::GetEnvironmentVariable('DEVMANAGER_PROFILE')
    if (-not [string]::IsNullOrWhiteSpace([string]$requestedProfile) -and
        [string]::Equals([string]$requestedProfile, 'production', [System.StringComparison]::OrdinalIgnoreCase)) {
        Add-GlobalBlocker 'production profile is forbidden'
    }
    if ($authorizedRootKind -eq 'authenticated-fixture') {
        # Executable contract tests can lower only the human-output ceiling to
        # exercise the real bounded fallback. Candidate audits never read or
        # honor this test-only control.
        $testHumanBytes = [Environment]::GetEnvironmentVariable('DEVMANAGER_CUTOVER_TEST_HUMAN_BYTES')
        if (-not [string]::IsNullOrEmpty($testHumanBytes)) {
            if ($testHumanBytes -notmatch '^[0-9]{1,6}$') {
                throw 'fixture human report bound was invalid.'
            }
            $parsedTestHumanBytes = [int]$testHumanBytes
            if ($parsedTestHumanBytes -lt 192 -or $parsedTestHumanBytes -gt 131072) {
                throw 'fixture human report bound was outside its safe test range.'
            }
            $maxReportHumanBytes = [int64]$parsedTestHumanBytes
        }
    }
    $evidenceRoot = Normalize-CutoverAbsolutePath -LiteralPath (Join-Path $rootPath '.devmanager-next\evidence') -Label 'evidence root'
    Enable-CutoverAuditIsolation -RepositoryRoot $rootPath -EvidenceRoot $evidenceRoot
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
    $reportParentPath = Normalize-CutoverAbsolutePath `
        -LiteralPath (Split-Path -Parent $reportPath) `
        -Label 'report parent'
    $script:reportDirectoryHandle = Open-CutoverRelativeDirectoryChain `
        -RootHandle $rootDirectoryHandle `
        -RootPath $rootPath `
        -LiteralPath $reportParentPath
    Assert-CutoverPublicationAuthority `
        -ParentHandle $script:reportDirectoryHandle `
        -ExpectedParentPath $reportParentPath

    if (-not [string]::IsNullOrWhiteSpace($RemoteChangeEvidencePath)) {
        $remoteChangeAttribution = Get-CutoverRemoteChangeAttribution `
            -EvidencePath $RemoteChangeEvidencePath
        if ($remoteChangeAttribution.classification -eq 'protected-or-unclassified-change') {
            Add-GlobalBlocker 'remote change protected'
        }
        elseif ($remoteChangeAttribution.classification -eq 'browser-activity-unattributed') {
            Add-GlobalBlocker 'remote change unattributed'
        }
    }

    if (-not [string]::IsNullOrEmpty($authorizationFailure)) {
        throw 'authorized root Git identity was not established.'
    }
    Assert-CutoverRootStable
    $trackedFiles = @(Invoke-GitTrackedFiles -RepositoryRoot $rootPath)
    foreach ($tracked in $trackedFiles) {
        Assert-CutoverWorkDeadline
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
    $historicalReferenceAllowlist = New-Object 'System.Collections.Generic.List[string]'
    foreach ($allowedHistorical in @(Get-BoundedContractStringArray `
                -Value (Get-ContractProperty -Object $policy -Name 'intentionalHistoricalReferencePaths') `
                -Label 'referencePolicy.intentionalHistoricalReferencePaths')) {
        $normalizedHistorical = Normalize-ContractRelativePath `
            -Value $allowedHistorical `
            -Label 'referencePolicy.intentionalHistoricalReferencePaths' `
            -AllowDirectory
        if ($null -ne $normalizedHistorical) {
            $historicalReferenceAllowlist.Add($normalizedHistorical)
        }
    }
    if (-not ($historicalReferenceAllowlist -contains 'docs/replacement-deletion-ledger.md')) {
        $historicalReferenceAllowlist.Add('docs/replacement-deletion-ledger.md')
    }
    $maxMatches = 20
    $maxMatchesValue = Get-ContractProperty -Object $policy -Name 'maxMatchesPerRow'
    if ($null -ne $maxMatchesValue -and [int]$maxMatchesValue -gt 0) {
        $maxMatches = [Math]::Min([int]$maxMatchesValue, 100)
    }

    $nodeById = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    $nodes = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'prerequisiteNodes'))
    foreach ($node in $nodes) {
        Assert-CutoverWorkDeadline
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
                $verdict = Get-CutoverEvidenceArtifactVerdict `
                    -ArtifactPath $artifactPath `
                    -RowId $nodeId `
                    -ExpectedGateIds @($nodeId) `
                    -ExpectedTestIds @($nodeId)
                if ($verdict -eq 'present') {
                    $artifactPresent = $true
                }
                elseif ($verdict -eq 'missing') {
                    Add-GlobalBlocker "prerequisite node '$nodeId' missing evidence artifact: $artifactPath"
                }
                else {
                    Add-GlobalBlocker "prerequisite node '$nodeId' evidence artifact is $verdict"
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
        Assert-CutoverWorkDeadline
        $nodeId = [string](Get-ContractProperty -Object $node -Name 'id')
        $nodeStatus = [string](Get-ContractProperty -Object $node -Name 'status')
        if ($nodeStatus -ne 'READY' -or -not $nodeById.ContainsKey($nodeId)) {
            continue
        }
        foreach ($dependency in Get-ContractArray (Get-ContractProperty -Object $node -Name 'dependsOn')) {
            Assert-CutoverWorkDeadline
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
        Assert-CutoverWorkDeadline
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
            -Label "row '$rowId' legacy.path" `
            -AllowDirectory
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
        $cutoverAction = [string](Get-ContractProperty -Object $row -Name 'cutoverAction')
        if ([string]::IsNullOrWhiteSpace($cutoverAction)) {
            $cutoverAction = 'delete'
        }
        if ($cutoverAction -notin @('delete', 'handoff')) {
            Add-ContractError "row '$rowId' cutoverAction must be delete or handoff."
        }
        if ($cutoverAction -eq 'handoff' -and $status -eq 'DELETED') {
            Add-ContractError "row '$rowId' is a handoff row and cannot be DELETED."
        }
        if ((Get-ContractProperty -Object $row -Name 'approvalRequired') -ne $true) {
            Add-ContractError "row '$rowId' must require explicit approval."
        }
        if ([string]::IsNullOrWhiteSpace([string](Get-ContractProperty -Object $row -Name 'approvalRequirement'))) {
            Add-ContractError "row '$rowId' approvalRequirement is empty."
        }
        $null = Assert-CutoverVerifiableClaimText `
            -Text ([string](Get-ContractProperty -Object $row -Name 'approvalRequirement')) `
            -Label "row '$rowId' approvalRequirement"

        $replacementSymbol = [string](Get-ContractProperty -Object $replacement -Name 'symbol')
        if ([string]::IsNullOrWhiteSpace($replacementSymbol)) {
            Add-ContractError "row '$rowId' replacementOwner.symbol is empty."
        }
        $null = Assert-CutoverVerifiableClaimText -Text $replacementSymbol -Label "row '$rowId' replacementOwner.symbol"

        $declaredTests = New-Object 'System.Collections.Generic.List[object]'
        $testsMissing = $false
        $testsValue = Get-ContractProperty -Object $row -Name 'tests'
        if ($null -eq $testsValue) {
            $testsMissing = $true
        }
        else {
            foreach ($item in Get-ContractArray $testsValue) {
                Assert-CutoverWorkDeadline
                if ($item -is [string]) {
                    $null = Assert-CutoverVerifiableClaimText -Text ([string]$item) -Label "row '$rowId' tests"
                    $declaredTests.Add([pscustomobject]@{ unbound = $true; kind = ''; path = $null; filter = '' })
                    continue
                }
                $testKind = [string](Get-ContractProperty -Object $item -Name 'kind')
                $testPath = Normalize-ContractRelativePath `
                    -Value (Get-ContractProperty -Object $item -Name 'path') `
                    -Label "row '$rowId' tests.path"
                $testFilter = [string](Get-ContractProperty -Object $item -Name 'filter')
                $testEvidence = Normalize-ContractRelativePath `
                    -Value (Get-ContractProperty -Object $item -Name 'evidence') `
                    -Label "row '$rowId' tests.evidence"
                $null = Assert-CutoverVerifiableClaimText -Text $testKind -Label "row '$rowId' tests.kind"
                $null = Assert-CutoverVerifiableClaimText -Text $testFilter -Label "row '$rowId' tests.filter"
                if ($testKind -notin @('cargo-test', 'pwsh', 'node-test')) {
                    $declaredTests.Add([pscustomobject]@{ unbound = $true; kind = $testKind; path = $testPath; filter = $testFilter })
                    continue
                }
                $declaredTests.Add([pscustomobject]@{
                        unbound = $false
                        kind = $testKind
                        path = $testPath
                        filter = $testFilter
                        evidence = $testEvidence
                    })
            }
        }

        $e2eProof = Get-ContractProperty -Object $row -Name 'e2eProof'
        $e2eMissing = $false
        $e2eArtifact = $null
        $e2eKind = ''
        if ($null -eq $e2eProof) {
            $e2eMissing = $true
        }
        else {
            $e2eArtifact = Normalize-ContractRelativePath `
                -Value (Get-ContractProperty -Object $e2eProof -Name 'artifact') `
                -Label "row '$rowId' e2eProof.artifact"
            $e2eKind = [string](Get-ContractProperty -Object $e2eProof -Name 'kind')
            if (-not [string]::IsNullOrWhiteSpace($e2eKind) -and $e2eKind -notin @('phase-gate', 'focused-e2e', 'soak')) {
                Add-ContractError "row '$rowId' e2eProof.kind is not a machine-verifiable proof kind."
            }
            $null = Assert-CutoverVerifiableClaimText -Text $e2eKind -Label "row '$rowId' e2eProof.kind"
        }

        $productionImpact = Get-ContractProperty -Object $row -Name 'productionImpact'
        $impactMissing = $false
        $impactProfile = ''
        $impactPreserves = @()
        $impactNeverTouches = @()
        if ($null -eq $productionImpact) {
            $impactMissing = $true
        }
        else {
            $impactProfile = [string](Get-ContractProperty -Object $productionImpact -Name 'profile')
            if (-not [string]::IsNullOrWhiteSpace($impactProfile) -and $impactProfile -notin @('isolated-fixture', 'isolated', 'none')) {
                Add-ContractError "row '$rowId' productionImpact.profile must be isolated-fixture, isolated, or none."
            }
            if ([string]::Equals($impactProfile, 'production', [System.StringComparison]::OrdinalIgnoreCase)) {
                Add-GlobalBlocker 'production profile is forbidden'
            }
            $null = Assert-CutoverVerifiableClaimText -Text $impactProfile -Label "row '$rowId' productionImpact.profile"
            $impactPreserves = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $productionImpact -Name 'preserves') -Label "row '$rowId' productionImpact.preserves")
            $impactNeverTouches = @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $productionImpact -Name 'neverTouches') -Label "row '$rowId' productionImpact.neverTouches")
            if ($impactNeverTouches.Count -gt 0 -and -not ($impactNeverTouches -contains 'session.json')) {
                Add-ContractError "row '$rowId' productionImpact.neverTouches must include session.json."
            }
            foreach ($impactClaim in @($impactPreserves + $impactNeverTouches)) {
                $null = Assert-CutoverVerifiableClaimText -Text $impactClaim -Label "row '$rowId' productionImpact"
            }
        }

        $deletionSet = @()
        $deletionValue = Get-ContractProperty -Object $row -Name 'deletionSet'
        if ($null -ne $deletionValue) {
            foreach ($deletionPath in @(Get-BoundedContractStringArray -Value $deletionValue -Label "row '$rowId' deletionSet")) {
                $normalizedDeletion = Normalize-ContractRelativePath -Value $deletionPath -Label "row '$rowId' deletionSet" -AllowDirectory
                if ($null -ne $normalizedDeletion) {
                    $deletionSet += $normalizedDeletion
                }
            }
        }
        if ($cutoverAction -eq 'delete') {
            if ($deletionSet.Count -eq 0) {
                Add-ContractError "row '$rowId' deletionSet is empty."
            }
            elseif ($null -ne $legacyPath -and -not ($deletionSet -contains $legacyPath)) {
                Add-ContractError "row '$rowId' deletionSet must include the legacy owner path."
            }
        }
        elseif ($deletionSet.Count -gt 0) {
            Add-ContractError "row '$rowId' is a handoff row and must not declare a deletionSet."
        }

        $rowModels.Add([pscustomobject]@{
                source         = $row
                id             = $rowId
                reportId       = (Get-CutoverSafeReportIdentifier -Value $rowId)
                legacyPath     = $legacyPath
                symbols        = $symbols
                tokens         = $tokens
                replacementPath = $replacementPath
                replacementSymbol = $replacementSymbol
                prerequisites  = $prerequisites
                commands       = $commands
                artifacts      = $artifacts
                declaredTests  = @($declaredTests.ToArray())
                testsMissing   = $testsMissing
                e2eArtifact    = $e2eArtifact
                e2eKind        = $e2eKind
                e2eMissing     = $e2eMissing
                impactMissing  = $impactMissing
                impactProfile  = $impactProfile
                impactPreserves = $impactPreserves
                impactNeverTouches = $impactNeverTouches
                deletionSet    = $deletionSet
                status         = $status
                cutoverAction  = $cutoverAction
            })
    }

    foreach ($model in $rowModels) {
        Assert-CutoverWorkDeadline
        foreach ($prerequisite in $model.prerequisites) {
            Assert-CutoverWorkDeadline
            if (-not $nodeById.ContainsKey($prerequisite)) {
                Add-ContractError "row '$($model.id)' has unknown prerequisite '$prerequisite'."
            }
        }
    }

    $needles = New-Object 'System.Collections.Generic.List[object]'
    $needleKeys = New-Object 'System.Collections.Generic.Dictionary[string,bool]' ([System.StringComparer]::Ordinal)
    foreach ($model in $rowModels) {
        Assert-CutoverWorkDeadline
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
        Assert-CutoverWorkDeadline
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
                Assert-CutoverWorkDeadline
                if ($model.legacyPath -eq $entrypointPath) {
                    Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'token' -Value $token
                }
            }
        }
        if (Test-TrackedPathPresent -Path $entrypointPath -Tracked $trackedFiles) {
            $entrypointFindings.Add("${entrypointReportId}:$entrypointPath")
        }
    }

    $compatibilityScanRoots = @()
    $productEntrypoints = Get-ContractProperty -Object $contract -Name 'productEntrypoints'
    if ($null -ne $productEntrypoints) {
        foreach ($entrypointName in @('desktopClient', 'durableHost')) {
            Assert-CutoverWorkDeadline
            $entrypoint = Get-ContractProperty -Object $productEntrypoints -Name $entrypointName
            if ($null -eq $entrypoint) {
                Add-ContractError "productEntrypoints.$entrypointName is required when productEntrypoints is present."
                continue
            }
            $entrypointId = [string](Get-ContractProperty -Object $entrypoint -Name 'id')
            $entrypointPath = Normalize-ContractRelativePath `
                -Value (Get-ContractProperty -Object $entrypoint -Name 'path') `
                -Label "productEntrypoints.$entrypointName.path"
            $entrypointRole = [string](Get-ContractProperty -Object $entrypoint -Name 'role')
            $entrypointPresent = $false
            if ($null -ne $entrypointPath) {
                $entrypointPresent = Test-TrackedPathPresent -Path $entrypointPath -Tracked $trackedFiles
                if (-not $entrypointPresent) {
                    Add-GlobalBlocker "required product entrypoint is missing: $entrypointPath"
                }
            }
            $productOwner = "product:$(Get-CutoverSafeReportIdentifier -Value $entrypointId)"
            foreach ($dispatch in @(Get-BoundedContractStringArray `
                        -Value (Get-ContractProperty -Object $entrypoint -Name 'forbiddenDispatch') `
                        -Label "productEntrypoints.$entrypointName.forbiddenDispatch")) {
                Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $productOwner -Kind 'token' -Value $dispatch
            }
            if ($entrypointRole -eq 'durable-host') {
                $lifecycle = @(Get-BoundedContractStringArray `
                        -Value (Get-ContractProperty -Object $entrypoint -Name 'lifecycle') `
                        -Label "productEntrypoints.$entrypointName.lifecycle")
                foreach ($requiredLifecycle in @('attach', 'detach', 'full-quit')) {
                    if (-not ($lifecycle -contains $requiredLifecycle)) {
                        Add-ContractError "productEntrypoints.durableHost.lifecycle must include $requiredLifecycle."
                    }
                }
            }
            $productEntrypointReport.Add([pscustomobject]@{
                    id = Get-CutoverSafeReportIdentifier -Value $entrypointId
                    role = $entrypointRole
                    path = $entrypointPath
                    present = $entrypointPresent
                })
        }
        $desktopRoleCount = @($productEntrypointReport | Where-Object { $_.role -eq 'gpui-client' }).Count
        $hostRoleCount = @($productEntrypointReport | Where-Object { $_.role -eq 'durable-host' }).Count
        if ($desktopRoleCount -ne 1 -or $hostRoleCount -ne 1) {
            Add-ContractError 'productEntrypoints must declare exactly one gpui-client and one durable-host.'
        }
    }

    $compatibilityPolicy = Get-ContractProperty -Object $contract -Name 'compatibilityPolicy'
    if ($null -ne $compatibilityPolicy) {
        if ((Get-ContractProperty -Object $compatibilityPolicy -Name 'permanentDualUi') -ne $false) {
            Add-ContractError 'compatibilityPolicy.permanentDualUi must be false.'
        }
        if ((Get-ContractProperty -Object $compatibilityPolicy -Name 'backwardCompatibilityMode') -ne $false) {
            Add-ContractError 'compatibilityPolicy.backwardCompatibilityMode must be false.'
        }
        $compatibilityScanRoots = @(Get-BoundedContractStringArray `
                -Value (Get-ContractProperty -Object $compatibilityPolicy -Name 'scanPaths') `
                -Label 'compatibilityPolicy.scanPaths')
        foreach ($switchToken in @(Get-BoundedContractStringArray `
                    -Value (Get-ContractProperty -Object $compatibilityPolicy -Name 'forbiddenRuntimeSwitches') `
                    -Label 'compatibilityPolicy.forbiddenRuntimeSwitches')) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId 'compatibility:switch' -Kind 'token' -Value $switchToken
        }
    }

    $packagingHandoff = Get-ContractProperty -Object $contract -Name 'packagingHandoff'
    if ($null -ne $packagingHandoff) {
        $packagerManifest = Normalize-ContractRelativePath `
            -Value (Get-ContractProperty -Object $packagingHandoff -Name 'packagerManifest') `
            -Label 'packagingHandoff.packagerManifest'
        $requiredBinaries = @(Get-BoundedContractStringArray `
                -Value (Get-ContractProperty -Object $packagingHandoff -Name 'requiredBinaries') `
                -Label 'packagingHandoff.requiredBinaries')
        $requiredManifestTokens = @(Get-BoundedContractStringArray `
                -Value (Get-ContractProperty -Object $packagingHandoff -Name 'requiredManifestTokens') `
                -Label 'packagingHandoff.requiredManifestTokens')
        $requiredFiles = @(Get-BoundedContractStringArray `
                -Value (Get-ContractProperty -Object $packagingHandoff -Name 'requiredFiles') `
                -Label 'packagingHandoff.requiredFiles')
        if ((Get-ContractProperty -Object $packagingHandoff -Name 'forbidInstallOrPublish') -ne $true) {
            Add-ContractError 'packagingHandoff.forbidInstallOrPublish must be true.'
        }
        if ((Get-ContractProperty -Object $packagingHandoff -Name 'atomicTwoBinaryIdentity') -ne $true) {
            Add-ContractError 'packagingHandoff.atomicTwoBinaryIdentity must be true.'
        }
        if (-not ($requiredBinaries -contains 'devmanager.exe') -or -not ($requiredBinaries -contains 'devmanager-host.exe')) {
            Add-ContractError 'packagingHandoff.requiredBinaries must include devmanager.exe and devmanager-host.exe.'
        }
        if ($requiredBinaries -contains 'devmanager-next.exe') {
            Add-ContractError 'packagingHandoff.requiredBinaries must not include devmanager-next.exe.'
        }
        $manifestPresent = $false
        $missingTokens = New-Object 'System.Collections.Generic.List[string]'
        if ($null -ne $packagerManifest) {
            $manifestPresent = Test-TrackedPathPresent -Path $packagerManifest -Tracked $trackedFiles
            if (-not $manifestPresent) {
                Add-GlobalBlocker "packager manifest is missing: $packagerManifest"
            }
            else {
                try {
                    $manifestLiteral = Assert-CutoverConfinedPath `
                        -LiteralPath (Join-Path $rootPath ($packagerManifest.Replace('/', '\'))) `
                        -AncestorPath $rootPath
                    $manifestText = Read-CutoverConfinedUtf8 `
                        -LiteralPath $manifestLiteral `
                        -MaxBytes $maxScanBytesPerFile `
                        -Label 'packager manifest'
                    foreach ($token in $requiredManifestTokens) {
                        Assert-CutoverWorkDeadline
                        if ($manifestText.IndexOf([string]$token, [System.StringComparison]::Ordinal) -lt 0) {
                            $missingTokens.Add([string]$token)
                            $packagingFindings.Add("missing packager token '$token' in $packagerManifest")
                        }
                    }
                }
                catch {
                    Add-GlobalBlocker "packager manifest could not be read safely: $packagerManifest"
                }
            }
        }
        $requiredFileReports = New-Object 'System.Collections.Generic.List[object]'
        foreach ($requiredFile in $requiredFiles) {
            Assert-CutoverWorkDeadline
            $requiredPath = Normalize-ContractRelativePath -Value $requiredFile -Label 'packagingHandoff.requiredFiles'
            $requiredPresent = $false
            if ($null -ne $requiredPath) {
                $requiredPresent = Test-TrackedPathPresent -Path $requiredPath -Tracked $trackedFiles
                if (-not $requiredPresent) {
                    $packagingFindings.Add("missing packaging/update handoff file: $requiredPath")
                }
            }
            $requiredFileReports.Add([pscustomobject]@{ path = $requiredPath; present = $requiredPresent })
        }
        $packagingHandoffReport = [pscustomobject]([ordered]@{
                requiredBinaries = @($requiredBinaries)
                atomicTwoBinaryIdentity = $true
                packagerManifest = $packagerManifest
                manifestPresent = $manifestPresent
                missingManifestTokens = @($missingTokens.ToArray())
                requiredFiles = @($requiredFileReports.ToArray())
                forbidInstallOrPublish = $true
            })
        foreach ($finding in @($packagingFindings)) {
            Add-GlobalBlocker $finding
        }
    }

    $profileIsolation = Get-ContractProperty -Object $contract -Name 'profileIsolation'
    if ($null -ne $profileIsolation) {
        if ((Get-ContractProperty -Object $profileIsolation -Name 'forbidSettingDevmanagerProfile') -ne $true) {
            Add-ContractError 'profileIsolation.forbidSettingDevmanagerProfile must be true.'
        }
        if ((Get-ContractProperty -Object $profileIsolation -Name 'remapAppData') -ne $true) {
            Add-ContractError 'profileIsolation.remapAppData must be true.'
        }
        if ((Get-ContractProperty -Object $profileIsolation -Name 'productionProfileOnlyInSignedRelease') -ne $true) {
            Add-ContractError 'profileIsolation.productionProfileOnlyInSignedRelease must be true.'
        }
        if ([string](Get-ContractProperty -Object $profileIsolation -Name 'evidenceRoot') -ne '.devmanager-next/evidence') {
            Add-ContractError 'profileIsolation.evidenceRoot must be .devmanager-next/evidence.'
        }
        if ($isolationReport.setDevmanagerProfile) {
            Add-GlobalBlocker 'audit process still has DEVMANAGER_PROFILE set.'
        }
        if (-not $isolationReport.remappedAppData) {
            Add-GlobalBlocker 'audit APPDATA is not isolated beneath the repository root.'
        }
    }

    $installedAppPolicy = Get-ContractProperty -Object $contract -Name 'installedAppPolicy'
    if ($null -ne $installedAppPolicy) {
        foreach ($flag in @('touchInstalledApp', 'hashProductionFiles', 'openSessionJson', 'installPublishDeleteUserData')) {
            if ((Get-ContractProperty -Object $installedAppPolicy -Name $flag) -ne $false) {
                Add-ContractError "installedAppPolicy.$flag must be false."
            }
        }
    }

    $publicationPolicy = Get-ContractProperty -Object $contract -Name 'publicationPolicy'
    if ($null -ne $publicationPolicy) {
        if ((Get-ContractProperty -Object $publicationPolicy -Name 'requireExplicitManualApproval') -ne $true) {
            Add-ContractError 'publicationPolicy.requireExplicitManualApproval must be true.'
        }
        if ((Get-ContractProperty -Object $publicationPolicy -Name 'forbidAutomatedPublish') -ne $true) {
            Add-ContractError 'publicationPolicy.forbidAutomatedPublish must be true.'
        }
    }

    $scanMatches = @(Invoke-ReferenceScan `
        -RepositoryRoot $rootPath `
        -Tracked $trackedFiles `
        -Needles $needles `
        -MaxMatches $maxMatches)
    Assert-CutoverRootStable

    foreach ($match in $scanMatches | Where-Object { $_.ownerId -like 'entrypoint:*' }) {
        Assert-CutoverWorkDeadline
        $entrypointFindings.Add("$($match.ownerId.Substring(11)):$($match.path)")
    }

    foreach ($match in $scanMatches | Where-Object { $_.ownerId -like 'product:*' }) {
        Assert-CutoverWorkDeadline
        if (Test-CutoverHistoricalReferenceAllowed -Path $match.path -Allowlist @($historicalReferenceAllowlist.ToArray())) {
            continue
        }
        $entrypointFindings.Add("$($match.ownerId.Substring(8)):$($match.path)")
        Add-GlobalBlocker "desktop client still dispatches a forbidden legacy runtime: $($match.path)"
    }
    foreach ($match in @($scanMatches | Where-Object { $_.ownerId -eq 'compatibility:switch' })) {
        Assert-CutoverWorkDeadline
        if (-not (Test-PathUnderScanRoots -Path $match.path -ScanRoots $compatibilityScanRoots)) {
            continue
        }
        if (Test-CutoverHistoricalReferenceAllowed -Path $match.path -Allowlist @($historicalReferenceAllowlist.ToArray())) {
            continue
        }
        $compatibilityFindings.Add("$($match.path)")
    }

    foreach ($model in $rowModels) {
        Assert-CutoverWorkDeadline
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
                if ($model.cutoverAction -eq 'handoff') {
                    Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "handoff replacement owner is not yet present: $($model.replacementPath)"
                }
                else {
                    Add-ContractError "row '$($model.id)' replacement owner path is not an exact tracked path: $($model.replacementPath)."
                }
            }
        }
        if ($null -ne $model.legacyPath -and -not $pathPresent -and $model.status -eq 'DELETED' -and $model.cutoverAction -ne 'delete') {
            Add-ContractError "row '$($model.id)' DELETED status is only valid for delete actions."
        }

        $artifactReports = New-Object 'System.Collections.Generic.List[object]'
        $evidencePaths = New-Object 'System.Collections.Generic.List[string]'
        foreach ($artifact in $model.artifacts) {
            $artifactPath = Normalize-ContractRelativePath -Value $artifact -Label "row '$($model.id)' evidence artifact"
            if ($null -ne $artifactPath) {
                $evidencePaths.Add($artifactPath)
            }
        }
        if ($null -ne $model.e2eArtifact -and -not ($evidencePaths -contains $model.e2eArtifact)) {
            $evidencePaths.Add([string]$model.e2eArtifact)
        }
        $expectedTestIds = New-Object 'System.Collections.Generic.List[string]'
        $expectedTestIds.Add([string]$model.id)
        foreach ($declared in @($model.declaredTests)) {
            if (-not [string]::IsNullOrWhiteSpace([string]$declared.filter)) {
                $expectedTestIds.Add([string]$declared.filter)
            }
        }
        foreach ($artifactPath in $evidencePaths) {
            $artifactPresent = $false
                $verdict = Get-CutoverEvidenceArtifactVerdict `
                    -ArtifactPath $artifactPath `
                    -RowId $model.id `
                    -ExpectedGateIds @($model.prerequisites) `
                    -ExpectedTestIds @($expectedTestIds.ToArray()) `
                    -ExpectedCommands @($model.commands)
            if ($verdict -eq 'present') {
                $artifactPresent = $true
            }
            elseif ($verdict -eq 'missing') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "missing evidence artifact: $artifactPath"
            }
            elseif ($verdict -eq 'protected') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "evidence artifact uses protected session.json"
            }
            elseif ($verdict -eq 'rejected') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "evidence artifact rejected by filesystem safety: $artifactPath"
            }
            elseif ($verdict -eq 'compile-only') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "compile-only evidence artifact: $artifactPath"
            }
            elseif ($verdict -eq 'stale') {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "stale evidence artifact: $artifactPath"
            }
            else {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "evidence artifact is $verdict"
            }
            $artifactReports.Add([pscustomobject]@{ path = $artifactPath; present = $artifactPresent })
        }

        $testReports = New-Object 'System.Collections.Generic.List[object]'
        if ($model.testsMissing -eq $true) {
            Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified missing tests'
        }
        elseif (@($model.declaredTests).Count -eq 0) {
            Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified empty tests'
        }
        foreach ($declared in @($model.declaredTests)) {
            Assert-CutoverWorkDeadline
            if ($declared.unbound -eq $true -or [string]::IsNullOrWhiteSpace([string]$declared.path)) {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified unbound test declaration'
                continue
            }
            $testPathPresent = Test-TrackedPathPresent -Path ([string]$declared.path) -Tracked $trackedFiles
            $filterPresent = $false
            if ($testPathPresent -and -not [string]::IsNullOrWhiteSpace([string]$declared.filter)) {
                try {
                    $testLiteral = Assert-CutoverConfinedPath `
                        -LiteralPath (Join-Path $rootPath ([string]$declared.path).Replace('/', '\')) `
                        -AncestorPath $rootPath
                    $testSource = Read-CutoverConfinedUtf8 -LiteralPath $testLiteral -MaxBytes $maxScanBytesPerFile -Label "row '$($model.id)' test source"
                    $filterPresent = $testSource.IndexOf("fn $([string]$declared.filter)", [System.StringComparison]::Ordinal) -ge 0
                }
                catch {
                    $filterPresent = $false
                }
            }
            elseif ([string]::IsNullOrWhiteSpace([string]$declared.filter)) {
                $filterPresent = $true
            }
            $evidenceBound = $false
            foreach ($artifactPath in $evidencePaths) {
                if ([string]::Equals([string]$declared.evidence, [string]$artifactPath, [System.StringComparison]::Ordinal)) {
                    $evidenceBound = $true
                    break
                }
            }
            if (-not $testPathPresent -or -not $filterPresent -or -not $evidenceBound) {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified test path, filter, or evidence binding'
                continue
            }
            $testReports.Add([ordered]@{
                    kind = Get-CutoverSafeReportIdentifier -Value $declared.kind
                    path = [string]$declared.path
                    filter = Get-CutoverSafeReportIdentifier -Value $declared.filter
                })
        }
        if ($model.e2eMissing -eq $true) {
            Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified missing e2e proof'
        }
        if ($model.impactMissing -eq $true) {
            Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message 'unverified missing production impact'
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
        $deletionPresent = New-Object 'System.Collections.Generic.List[string]'
        foreach ($deletionPath in @($model.deletionSet)) {
            if (Test-TrackedPathPresent -Path $deletionPath -Tracked $trackedFiles) {
                $deletionPresent.Add([string]$deletionPath)
            }
        }
        if ($model.status -eq 'DELETED') {
            if ($pathPresent) {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "legacy path still present: $($model.legacyPath)"
            }
            foreach ($remaining in $deletionPresent) {
                Add-RowBlocker -Blockers ([ref]$rowBlockers) -Message "row deletion set still present: $remaining"
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
                    cutoverAction = $model.cutoverAction
                    legacy = [ordered]@{
                        path = $model.legacyPath
                        symbolCount = @($model.symbols).Count
                        tokenCount = @($model.tokens).Count
                        pathPresent = $pathPresent
                    }
                    replacementOwner = [ordered]@{
                        path = $model.replacementPath
                        symbol = $model.replacementSymbol
                        present = $replacementPresent
                    }
                    tests = @($testReports.ToArray())
                    e2eProof = [ordered]@{
                        artifact = $model.e2eArtifact
                        kind = $model.e2eKind
                    }
                    productionImpact = [ordered]@{
                        profile = $model.impactProfile
                        preserves = @($model.impactPreserves)
                        neverTouches = @($model.impactNeverTouches)
                    }
                    deletionSet = [ordered]@{
                        paths = @($model.deletionSet)
                        present = @($deletionPresent.ToArray())
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
        Copy-CutoverRedactedBlockers -Blockers $rowBlockers
    }

    $sortedEntrypointFindings = @(Sort-CutoverOrdinalStrings -Values @($entrypointFindings.ToArray()))
    foreach ($finding in $sortedEntrypointFindings) {
        Add-GlobalBlocker "forbidden legacy entrypoint finding: $finding"
    }
    $sortedCompatibilityFindings = @(Sort-CutoverOrdinalStrings -Values @($compatibilityFindings.ToArray()))
    if ($sortedCompatibilityFindings.Count -gt 60) {
        $sortedCompatibilityFindings = @($sortedCompatibilityFindings | Select-Object -First 60)
    }
    foreach ($finding in $sortedCompatibilityFindings) {
        Add-GlobalBlocker "forbidden compatibility/runtime switch reference: $finding"
    }
    $sortedPackagingFindings = @(Sort-CutoverOrdinalStrings -Values @($packagingFindings.ToArray()))
    $sortedContractErrors = @(Sort-CutoverOrdinalDiagnostics -Values @($contractErrors.ToArray()))
    $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))
    $remainingIntegratedPrerequisites = @(
        Sort-CutoverOrdinalStrings -Values @(
            $nodeReports |
                Where-Object { $_.status -ne 'READY' } |
                ForEach-Object { [string]$_.id }
        )
    )

    $allRowsTerminal = $rowReports.Count -gt 0 -and @($rowReports | Where-Object { $_.status -eq 'HOLD' }).Count -eq 0
    $handoffReady = $true
    foreach ($row in @($rowReports | Where-Object { $_.cutoverAction -eq 'handoff' })) {
        if ($row.status -ne 'READY' -or @($row.blockers).Count -gt 0) {
            $handoffReady = $false
        }
    }
    $packagingReady = $true
    if ($null -ne $packagingHandoffReport) {
        $missingHandoffFiles = @($packagingHandoffReport.requiredFiles | Where-Object { -not $_.present })
        if ($missingHandoffFiles.Count -gt 0 -or @($packagingHandoffReport.missingManifestTokens).Count -gt 0) {
            $packagingReady = $false
        }
    }
    $contractStatus = if (
        $contractErrors.Count -gt 0 -or
        $globalBlockers.Count -gt 0 -or
        -not $allRowsTerminal -or
        -not $handoffReady -or
        -not $packagingReady
    ) { 'HOLD' } else { 'READY' }
}
catch {
    $fatalDiagnosticCategory = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
    Add-ContractError -Message "fatal audit error: $fatalDiagnosticCategory" -Category $fatalDiagnosticCategory
    if ($fatalDiagnosticCategory -eq 'process_deadline_exceeded' -or
        (Get-CutoverDeadlineRemainingMilliseconds) -le $publicationReserveMs) {
        # Do not spend the publication slice sorting or serializing a partial,
        # attacker-influenced report. A small typed HOLD below retains fixed
        # safety-critical remote and deadline blockers and can be published before the one
        # absolute audit deadline expires.
        Add-SafetyBound
        $boundedPublicationRequired = $true
        $sortedEntrypointFindings = @()
        $sortedCompatibilityFindings = @()
        $sortedPackagingFindings = @()
        $sortedContractErrors = @()
        $sortedGlobalBlockers = @()
        $remainingIntegratedPrerequisites = @()
    }
    else {
        $sortedEntrypointFindings = @(Sort-CutoverOrdinalStrings -Values @($entrypointFindings.ToArray()))
        $sortedCompatibilityFindings = @(Sort-CutoverOrdinalStrings -Values @($compatibilityFindings.ToArray()))
        $sortedPackagingFindings = @(Sort-CutoverOrdinalStrings -Values @($packagingFindings.ToArray()))
        $sortedContractErrors = @(Sort-CutoverOrdinalDiagnostics -Values @($contractErrors.ToArray()))
        $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))
    }
    if ($null -eq $reportPath) {
        [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    }
    $contractStatus = 'HOLD'
}

if ($null -eq $rootPath -or $null -eq $evidenceRoot -or $null -eq $reportPath -or $null -eq $humanPath) {
    [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    Close-CutoverPublicationHandles
    exit 2
}

if ($boundedPublicationRequired) {
    $boundedPublicationBlockers = New-Object 'System.Collections.Generic.List[string]'
    $boundedPublicationBlockers.Add($safetyDiagnostic)
    if ($fatalDiagnosticCategory -eq 'process_deadline_exceeded') {
        $boundedPublicationBlockers.Add('audit[process_deadline_exceeded]')
    }
    if ($remoteChangeAttribution.classification -eq 'protected-or-unclassified-change') {
        $boundedPublicationBlockers.Add('audit[remote_change_protected]')
    }
    elseif ($remoteChangeAttribution.classification -eq 'browser-activity-unattributed') {
        $boundedPublicationBlockers.Add('audit[remote_change_unattributed]')
    }
    $report = New-BoundedAuditReport -Report ([pscustomobject]([ordered]@{
                contractId = Get-ContractProperty -Object $contract -Name 'contractId'
                mode = $Mode
                trackedFileCount = @($trackedFiles).Count
                blockers = @($boundedPublicationBlockers.ToArray())
            }))
    $contractStatus = 'HOLD'
}
else {
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
        compatibilityFindings = @($sortedCompatibilityFindings)
        packagingFindings = @($sortedPackagingFindings)
        remainingIntegratedPrerequisites = @($remainingIntegratedPrerequisites)
        productEntrypoints = @(Sort-CutoverOrdinalObjects -Values @($productEntrypointReport.ToArray()) -Fields @('id'))
        packagingHandoff = $packagingHandoffReport
        isolation = [pscustomobject]$isolationReport
        installedApp = [pscustomobject]$installedAppReport
        prerequisiteNodes = @(Sort-CutoverOrdinalObjects -Values @($nodeReports.ToArray()) -Fields @('id'))
        rows = @(Sort-CutoverOrdinalObjects -Values @($rowReports.ToArray()) -Fields @('id'))
        remoteChangeAttribution = $remoteChangeAttribution
        safety = [ordered]@{
            boundReached = [bool]$safetyBoundReached
            diagnostic = if ($safetyBoundReached) { $safetyDiagnostic } else { $null }
            humanReportTruncated = $false
            humanReportOmittedLineCount = 0
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
                scannerOutputLines = $maxScannerOutputLines
                scannerOutputLineCharacters = $maxScannerOutputLineChars
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
            maxOutputLines = $maxScannerOutputLines
            maxOutputLineCharacters = $maxScannerOutputLineChars
            deadlineMilliseconds = $maxScannerDurationMs
        }
        })
}

try {
    # Always revalidate the authorized root before report publication, even when Git identity failed.
    Assert-CutoverAuthorizedRootIdentityStable
    if ($null -ne $gitIdentity) { Assert-CutoverRootStable }
    if ($null -eq $script:reportDirectoryHandle) {
        if ((Get-CutoverDeadlineRemainingMilliseconds) -le 0) {
            throw 'audit deadline exceeded before a publication handle was retained.'
        }
        if ($null -eq $rootDirectoryHandle -or [string]::IsNullOrWhiteSpace([string]$reportPath) -or [string]::IsNullOrWhiteSpace([string]$rootPath)) {
            throw 'publication handle was not retained.'
        }
        $reportParentPath = Normalize-CutoverAbsolutePath `
            -LiteralPath (Split-Path -Parent $reportPath) `
            -Label 'report parent'
        $script:reportDirectoryHandle = Open-CutoverRelativeDirectoryChain `
            -RootHandle $rootDirectoryHandle `
            -RootPath $rootPath `
            -LiteralPath $reportParentPath
        Assert-CutoverPublicationAuthority `
            -ParentHandle $script:reportDirectoryHandle `
            -ExpectedParentPath $reportParentPath
    }
    Write-AuditReports `
        -Report $report `
        -JsonPath $reportPath `
        -TextPath $humanPath `
        -EvidenceRoot $evidenceRoot `
        -ParentHandle $script:reportDirectoryHandle `
        -ContractStatus ([ref]$contractStatus)
    Write-Host ("Wrote cutover audit JSON -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $reportPath))
    Write-Host ("Wrote cutover audit report -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $humanPath))
}
catch {
    $fatalDiagnosticCategory = Get-CutoverDiagnosticCategory -Message $_.Exception.Message
    [Console]::Error.WriteLine("AUDIT_ERROR[$fatalDiagnosticCategory]")
    exit 2
}
finally {
    Close-CutoverPublicationHandles
}

if ($contractStatus -eq 'READY') {
    exit 0
}
exit 2
