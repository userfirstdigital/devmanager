# Phase 11.1 read-only cutover contract audit.
#
# This script observes the tracked repository and writes only its bounded audit
# report beneath .devmanager-next\evidence. It never reads production AppData,
# never reads or hashes an exact session.json file, and has no process lifecycle
# authority.

[CmdletBinding()]
param(
    [ValidateSet('Parity')]
    [string]$Mode = 'Parity',

    [string]$Root,

    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')

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
$maxErrorCount = 64
$maxReportJsonBytes = [int64]262144
$maxReportHumanBytes = [int64]131072
$safetyBoundReached = $false
$safetyDiagnosticEmitted = $false
$safetyDiagnostic = 'HOLD: audit safety bound reached; collection stopped.'
$maxMatches = $maxMatchesPerOwner
$rootIdentity = $null

function Add-SafetyBound {
    if ($script:safetyBoundReached -eq $true) {
        return
    }
    $script:safetyBoundReached = $true
    if ($script:safetyDiagnosticEmitted -eq $false) {
        $script:safetyDiagnosticEmitted = $true
        $globalBlockers.Add($safetyDiagnostic)
    }
}

function ConvertTo-SafeDiagnosticText {
    param([AllowEmptyString()][string]$Message)

    $safe = [regex]::Replace([string]$Message, '[\x00-\x1F\x7F]', '?')
    if ($safe.Length -gt 256) {
        return $safe.Substring(0, 253) + '...'
    }
    return $safe
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
    param([Parameter(Mandatory = $true)][string]$Message)

    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        if ($contractErrors.Count -ge $maxErrorCount) {
            Add-SafetyBound
            return
        }
        $contractErrors.Add((ConvertTo-SafeDiagnosticText -Message $Message.Trim()))
    }
}

function Add-GlobalBlocker {
    param([Parameter(Mandatory = $true)][string]$Message)

    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        if ($globalBlockers.Count -ge $maxErrorCount) {
            Add-SafetyBound
            return
        }
        $globalBlockers.Add((ConvertTo-SafeDiagnosticText -Message $Message.Trim()))
    }
}

function Add-RowBlocker {
    param(
        [Parameter(Mandatory = $true)][ref]$Blockers,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if ([string]::IsNullOrWhiteSpace($Message)) {
        return
    }
    if ($Blockers.Value.Count -ge $maxErrorCount) {
        Add-SafetyBound
        return
    }
    $Blockers.Value.Add((ConvertTo-SafeDiagnosticText -Message $Message.Trim()))
}

function Assert-CutoverRootStable {
    if ($null -eq $rootPath -or $null -eq $rootIdentity) {
        throw 'repository root identity was not established.'
    }
    $current = Get-CutoverPathIdentity -LiteralPath $rootPath -AllowDirectory
    if (-not (Compare-CutoverIdentity -Before $rootIdentity -After $current)) {
        throw 'repository root changed during the audit.'
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
    return [pscustomobject]@{
        volume = [uint32]$information.VolumeSerialNumber
        index = $index
        links = [uint32]$information.NumberOfLinks
        length = $length
    }
}

function Open-CutoverConfinedFile {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [switch]$AllowDirectory
    )

    if ([System.IO.Path]::GetFileName($LiteralPath).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw 'Protected session.json variants must be excluded before opening.'
    }
    $full = Assert-CutoverPathChain -LiteralPath $LiteralPath
    $item = Get-Item -LiteralPath $full -Force
    if ($item -is [System.IO.DirectoryInfo] -and -not $AllowDirectory) {
        throw "Expected a file, got a directory: '$full'."
    }
    $options = [System.IO.FileOptions]::SequentialScan
    if ($item -is [System.IO.DirectoryInfo]) {
        $options = [System.IO.FileOptions]0x02000000
    }
    $stream = [System.IO.FileStream]::new(
        $full,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::ReadWrite,
        8192,
        $options)
    try {
        $identity = Get-CutoverHandleIdentity -Stream $stream
        if ($item -isnot [System.IO.DirectoryInfo] -and $identity.links -gt 1) {
            throw "Refusing a hard-linked tracked/evidence file: '$full'."
        }
        return [pscustomobject]@{ path = $full; stream = $stream; identity = $identity }
    }
    catch {
        $stream.Dispose()
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
        $Before.length -eq $After.length
}

function Get-CutoverPathIdentity {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [switch]$AllowDirectory
    )

    $opened = Open-CutoverConfinedFile -LiteralPath $LiteralPath -AllowDirectory:$AllowDirectory
    try { return $opened.identity }
    finally { $opened.stream.Dispose() }
}

function Read-CutoverConfinedUtf8 {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][int64]$MaxBytes,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $opened = Open-CutoverConfinedFile -LiteralPath $LiteralPath
    try {
        $bytes = New-Object 'System.Collections.Generic.List[byte]'
        $buffer = New-Object byte[] 8192
        while ($true) {
            $read = $opened.stream.Read($buffer, 0, [Math]::Min($buffer.Length, [int]($MaxBytes + 1 - $bytes.Count)))
            if ($read -le 0) { break }
            for ($offset = 0; $offset -lt $read; $offset++) { $bytes.Add($buffer[$offset]) }
            if ($bytes.Count -gt $MaxBytes) {
                Add-SafetyBound
                throw "${Label} exceeds the bounded input byte limit."
            }
        }
        $after = Get-CutoverHandleIdentity -Stream $opened.stream
        if (-not (Compare-CutoverIdentity -Before $opened.identity -After $after)) {
            throw "${Label} changed during its confined read."
        }
        try {
            return ([System.Text.UTF8Encoding]::new($false, $true)).GetString($bytes.ToArray())
        }
        catch {
            throw "${Label} is not valid UTF-8."
        }
    }
    finally {
        $opened.stream.Dispose()
    }
}

function Ensure-CutoverAuditDirectory {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $full = Normalize-CutoverAbsolutePath -LiteralPath $LiteralPath -Label 'directory path'
    $root = [System.IO.Path]::GetPathRoot($full)
    $relative = if ($full.Length -gt $root.Length) { $full.Substring($root.Length) } else { '' }
    $parts = @($relative.Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))
    $current = $root.TrimEnd('\', '/')
    foreach ($part in $parts) {
        $current = Join-Path $current $part
        $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
        if ($null -eq $item) {
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

    $full = Assert-CutoverPathChain -LiteralPath $LiteralPath -AllowMissingLeaf:$AllowMissingLeaf
    if (-not (Test-CutoverPathEqualsOrBeneath -Path $full -Ancestor $AncestorPath)) {
        throw "Path is outside its confined root: '$full'."
    }
    return $full
}

function Test-CutoverConfinedFilePresent {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    try {
        $full = Assert-CutoverPathChain -LiteralPath $LiteralPath -AllowMissingLeaf
    }
    catch {
        if ($_.Exception.Message.StartsWith('filesystem path component is missing:', [System.StringComparison]::Ordinal)) {
            return $false
        }
        throw
    }
    $item = Get-Item -LiteralPath $full -Force -ErrorAction SilentlyContinue
    if ($null -eq $item -or $item -is [System.IO.DirectoryInfo]) { return $false }
    $opened = Open-CutoverConfinedFile -LiteralPath $full
    $opened.stream.Dispose()
    return $true
}

function Get-BoundedContractStringArray {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $result = New-Object 'System.Collections.Generic.List[string]'
    foreach ($item in Get-ContractArray $Value) {
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
        if ($null -ne $value) { $null = $unique.Add([string]$value) }
    }
    foreach ($value in $unique) { $null = $result.Add($value) }
    $result.Sort([System.StringComparer]::Ordinal)
    return @($result.ToArray())
}

function Sort-CutoverOrdinalObjects {
    param(
        [AllowEmptyCollection()][object[]]$Values,
        [Parameter(Mandatory = $true)][string[]]$Fields
    )

    $result = New-Object 'System.Collections.Generic.List[object]'
    foreach ($value in $Values) { $result.Add($value) }
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
        throw "Ledger contract JSON is invalid: $($_.Exception.Message)"
    }
}

function Invoke-GitTrackedFiles {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)

    $arguments = @('-C', $RepositoryRoot, 'ls-files', '--full-name', '-z', '--')
    $bytes = Invoke-CutoverProcessBytes -FileName 'git' -Arguments $arguments -MaxBytes $maxTrackedBytes
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

function Invoke-CutoverProcessBytes {
    param(
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FileName
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $false
    foreach ($argument in $Arguments) { $null = $startInfo.ArgumentList.Add($argument) }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) { throw "Unable to start bounded $FileName process." }
    try {
        $stream = $process.StandardOutput.BaseStream
        $bytes = New-Object 'System.Collections.Generic.List[byte]'
        $buffer = New-Object byte[] 8192
        while ($true) {
            $read = $stream.Read($buffer, 0, [Math]::Min($buffer.Length, [int]($MaxBytes + 1 - $bytes.Count)))
            if ($read -le 0) { break }
            for ($offset = 0; $offset -lt $read; $offset++) { $bytes.Add($buffer[$offset]) }
            if ($bytes.Count -gt $MaxBytes) {
                Add-SafetyBound
                try { $process.Kill($true) } catch { $process.Kill() }
                throw "bounded $FileName output exceeded its limit."
            }
        }
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) {
            throw "$FileName failed with exit code $($process.ExitCode)."
        }
        return ,([byte[]]$bytes.ToArray())
    }
    finally {
        $process.Dispose()
    }
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
        if ($fileCount -ge $maxScannerFiles) { Add-SafetyBound; break }
        $leaf = [System.IO.Path]::GetFileName($relativePath)
        if ($leaf.Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase) -or $relativePath -eq 'docs/replacement-deletion-ledger.md') {
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

            $arguments = @(
                '--json', '--fixed-strings', '--line-number', '--no-heading', '--color', 'never',
                '--no-messages', '--text', '--hidden', '--no-ignore', '--max-count', [string]$MaxMatches,
                '--max-columns', '4096', '--max-columns-preview'
            )
            foreach ($needle in $Needles) {
                $arguments += '-e'
                $arguments += [string]$needle.needle
            }
            $arguments += '--'
            $arguments += $absolutePath
            $scan = Invoke-CutoverProcessLines -FileName 'rg' -Arguments $arguments -MaxBytes ([int64]262144)
            if ($scan.exitCode -gt 1) {
                Add-GlobalBlocker 'rg reference scan failed for a validated tracked file.'
                continue
            }
            if ($scan.boundHit) { Add-SafetyBound; break }

            $after = Get-CutoverHandleIdentity -Stream $opened.stream
            if (-not (Compare-CutoverIdentity -Before $opened.identity -After $after)) {
                Add-SafetyBound
                break
            }
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
                foreach ($needle in $Needles) {
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
            Add-GlobalBlocker ("tracked scanner skipped a validated file: " + $_.Exception.Message)
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

function Invoke-CutoverProcessLines {
    param(
        [Parameter(Mandatory = $true)][string]$FileName,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][int64]$MaxBytes
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FileName
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $false
    foreach ($argument in $Arguments) { $null = $startInfo.ArgumentList.Add($argument) }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) { throw "Unable to start bounded $FileName process." }
    $lines = New-Object 'System.Collections.Generic.List[string]'
    $bytesRead = [int64]0
    $boundHit = $false
    try {
        while (($line = $process.StandardOutput.ReadLine()) -ne $null) {
            $lineBytes = [System.Text.Encoding]::UTF8.GetByteCount($line) + 1
            if ($bytesRead + $lineBytes -gt $MaxBytes) {
                $boundHit = $true
                try { $process.Kill($true) } catch { $process.Kill() }
                break
            }
            $bytesRead += $lineBytes
            $lines.Add($line)
        }
        $process.WaitForExit()
        return [pscustomobject]@{ lines = $lines.ToArray(); exitCode = $process.ExitCode; boundHit = $boundHit }
    }
    finally {
        $process.Dispose()
    }
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
    if (Test-Path -LiteralPath $full -PathType Leaf) {
        $existing = Open-CutoverConfinedFile -LiteralPath $full
        $existing.stream.Dispose()
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
        $temp = [System.IO.FileStream]::new($tempPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None, 8192, [System.IO.FileOptions]::WriteThrough)
        try {
            $temp.Write($bytes, 0, $bytes.Length)
            $temp.Flush($true)
        }
        finally { $temp.Dispose() }

        $parentAfter = Get-CutoverHandleIdentity -Stream $parentHandle.stream
        if (-not (Compare-CutoverIdentity -Before $parentHandle.identity -After $parentAfter)) {
            throw 'report parent changed before atomic replacement.'
        }
        $tempCheck = Open-CutoverConfinedFile -LiteralPath $tempPath
        $tempIdentity = $tempCheck.identity
        $tempCheck.stream.Dispose()
        $exists = Test-Path -LiteralPath $full -PathType Leaf
        if ($exists) {
            $destination = Open-CutoverConfinedFile -LiteralPath $full
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
            [System.IO.File]::Replace($tempPath, $full, $backupPath, $true)
            if ([System.IO.File]::Exists($backupPath)) { [System.IO.File]::Delete($backupPath) }
        }
        else {
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
            contractId = ConvertTo-SafeDiagnosticText -Message ([string](Get-ContractProperty -Object $Report -Name 'contractId'))
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
                    errors = $maxErrorCount
                    jsonBytes = $maxReportJsonBytes
                    humanBytes = $maxReportHumanBytes
                }
            }
            scanner = [ordered]@{
                trackedUniverse = 'git-ls-files'
                referenceScanner = 'rg --fixed-strings --line-number'
                allowedLedgerSelfReferences = @('docs/replacement-deletion-ledger.md')
                protectedFileBasenames = @('session.json')
                maxMatchesPerRow = $maxMatches
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
    if ([string]::IsNullOrWhiteSpace($Root)) {
        $rootPath = Normalize-CutoverAbsolutePath `
            -LiteralPath (Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot) `
            -Label 'Root'
    }
    else {
        $rootPath = Normalize-CutoverAbsolutePath -LiteralPath $Root -Label 'Root'
    }
    Assert-CutoverPathChain -LiteralPath $rootPath | Out-Null
    $rootItem = Get-Item -LiteralPath $rootPath -Force
    if ($rootItem -isnot [System.IO.DirectoryInfo]) { throw "Root directory is missing: $rootPath" }
    $rootIdentity = Get-CutoverPathIdentity -LiteralPath $rootPath -AllowDirectory
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

    Assert-CutoverRootStable
    $trackedFiles = @(Invoke-GitTrackedFiles -RepositoryRoot $rootPath)
    foreach ($tracked in $trackedFiles) {
        if ([System.IO.Path]::GetFileName($tracked).Equals('session.json', [System.StringComparison]::OrdinalIgnoreCase)) {
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
            id = $nodeId
            kind = $nodeKind
            status = $nodeStatus
            dependsOn = $nodeDependencies
            evidence = @($nodeEvidenceReports.ToArray())
        })
    }
    Assert-NodeGraph -Nodes $nodes -NodeById $nodeById
    foreach ($node in $nodes) {
        $nodeId = [string](Get-ContractProperty -Object $node -Name 'id')
        $nodeStatus = [string](Get-ContractProperty -Object $node -Name 'status')
        if ($nodeStatus -ne 'READY' -or -not $nodeById.ContainsKey($nodeId)) {
            continue
        }
        foreach ($dependency in Get-ContractArray (Get-ContractProperty -Object $node -Name 'dependsOn')) {
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
        foreach ($prerequisite in $model.prerequisites) {
            if (-not $nodeById.ContainsKey($prerequisite)) {
                Add-ContractError "row '$($model.id)' has unknown prerequisite '$prerequisite'."
            }
        }
    }

    $needles = New-Object 'System.Collections.Generic.List[object]'
    $needleKeys = New-Object 'System.Collections.Generic.Dictionary[string,bool]' ([System.StringComparer]::Ordinal)
    foreach ($model in $rowModels) {
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
        Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId "entrypoint:$entrypointId" -Kind 'path' -Value $entrypointPath
        foreach ($token in @(Get-BoundedContractStringArray -Value (Get-ContractProperty -Object $entrypoint -Name 'tokens') -Label "forbidden entrypoint '$entrypointId' token")) {
            Add-Needle `
                -Needles $needles `
                -NeedleKeys $needleKeys `
                -OwnerId "entrypoint:$entrypointId" `
                -Kind 'token' `
                -Value $token `
                -ContextPath $entrypointPath
            foreach ($model in $rowModels) {
                if ($model.legacyPath -eq $entrypointPath) {
                    Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'token' -Value $token
                }
            }
        }
        if (Test-TrackedPathPresent -Path $entrypointPath -Tracked $trackedFiles) {
            $entrypointFindings.Add("${entrypointId}:$entrypointPath")
        }
    }

    $scanMatches = @(Invoke-ReferenceScan `
        -RepositoryRoot $rootPath `
        -Tracked $trackedFiles `
        -Needles $needles `
        -MaxMatches $maxMatches)
    Assert-CutoverRootStable

    foreach ($match in $scanMatches | Where-Object { $_.ownerId -like 'entrypoint:*' }) {
        $entrypointFindings.Add("$($match.ownerId.Substring(11)):$($match.path)")
    }

    foreach ($model in $rowModels) {
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
                    id = $model.id
                    status = $model.status
                    legacy = [ordered]@{
                        path = $model.legacyPath
                        symbols = $model.symbols
                        tokens = $model.tokens
                        pathPresent = $pathPresent
                    }
                    replacementOwner = [ordered]@{
                        path = $model.replacementPath
                        present = $replacementPresent
                    }
                    prerequisites = $model.prerequisites
                    evidence = [ordered]@{
                        commands = $model.commands
                    artifacts = @($artifactReports.ToArray())
                    }
                    references = $references
                    blockers = Sort-CutoverOrdinalStrings -Values @($rowBlockers.ToArray())
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
    $sortedContractErrors = @(Sort-CutoverOrdinalStrings -Values @($contractErrors.ToArray()))
    $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))

    $allRowsTerminal = $rowReports.Count -gt 0 -and @($rowReports | Where-Object { $_.status -eq 'HOLD' }).Count -eq 0
    $contractStatus = if ($contractErrors.Count -gt 0 -or $globalBlockers.Count -gt 0 -or -not $allRowsTerminal) { 'HOLD' } else { 'READY' }
}
catch {
    $lineNumber = $_.InvocationInfo.ScriptLineNumber
    $sourceLine = ([string]$_.InvocationInfo.Line).Trim()
    Add-ContractError "fatal audit error at line ${lineNumber}: $($_.Exception.Message) [$sourceLine]"
    $sortedEntrypointFindings = @(Sort-CutoverOrdinalStrings -Values @($entrypointFindings.ToArray()))
    $sortedContractErrors = @(Sort-CutoverOrdinalStrings -Values @($contractErrors.ToArray()))
    $sortedGlobalBlockers = @(Sort-CutoverOrdinalStrings -Values @($globalBlockers.ToArray()))
    if ($null -eq $reportPath) {
        Write-Error (ConvertTo-SafeDiagnosticText -Message ("Audit initialization failed at line ${lineNumber}: " + $_.Exception.Message + " [" + $sourceLine + "]"))
    }
    $contractStatus = 'HOLD'
}

if ($null -eq $rootPath -or $null -eq $evidenceRoot -or $null -eq $reportPath -or $null -eq $humanPath) {
    Write-Error 'Unable to establish a confined repository, evidence, and output path; no report was written.'
    exit 2
}

$report = [pscustomobject]([ordered]@{
        schemaVersion = 1
        contractId = [string](Get-ContractProperty -Object $contract -Name 'contractId')
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
                errors = $maxErrorCount
                jsonBytes = $maxReportJsonBytes
                humanBytes = $maxReportHumanBytes
            }
        }
        scanner = [ordered]@{
            trackedUniverse = 'git-ls-files'
            referenceScanner = 'rg --fixed-strings --line-number'
            allowedLedgerSelfReferences = @('docs/replacement-deletion-ledger.md')
            protectedFileBasenames = @('session.json')
            maxMatchesPerRow = $maxMatches
        }
    })

try {
    Assert-CutoverRootStable
    Write-AuditReports -Report $report -JsonPath $reportPath -TextPath $humanPath -EvidenceRoot $evidenceRoot -ContractStatus ([ref]$contractStatus)
    Write-Host ("Wrote cutover audit JSON -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $reportPath))
    Write-Host ("Wrote cutover audit report -> {0}" -f (Get-RelativeReportPath -RepositoryRoot $rootPath -Path $humanPath))
}
catch {
    Write-Error "Unable to publish cutover audit report: $($_.Exception.Message)"
    exit 2
}

if ($contractStatus -eq 'READY') {
    exit 0
}
exit 2
