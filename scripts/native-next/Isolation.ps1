# Shared read-only production isolation helpers for native-next development.
# Capture/assert wrappers may write evidence JSON only; they never mutate production
# storage, never read/hash session.json, and never launch/stop/kill processes.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-DevManagerProductionRoot {
    param(
        [string]$AppDataRoot = $env:APPDATA
    )

    if ([string]::IsNullOrWhiteSpace($AppDataRoot)) {
        throw "APPDATA is missing; cannot resolve unprofiled production root."
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $AppDataRoot)) {
        throw "APPDATA must be a fully qualified path ('$AppDataRoot')."
    }

    $canonicalAppData = [System.IO.Path]::GetFullPath($AppDataRoot.Trim())
    return [System.IO.Path]::GetFullPath((Join-Path $canonicalAppData 'com.userfirst.devmanager'))
}

function Assert-DevManagerKnownFolderRoot {
    param(
        [AllowEmptyString()]
        [string]$Root,
        [Parameter(Mandatory = $true)]
        [string]$Name,
        [switch]$Required
    )

    if ([string]::IsNullOrWhiteSpace($Root)) {
        if ($Required) {
            throw "$Name is missing or empty; a fully qualified root is required."
        }
        return $null
    }

    if (-not (Test-DevManagerAbsolutePath -LiteralPath $Root)) {
        throw "$Name must be a fully qualified path ('$Root')."
    }

    return [System.IO.Path]::GetFullPath($Root.Trim())
}

function Get-DevManagerSupportedInstallPaths {
    param(
        [string]$LocalAppDataRoot = $env:LOCALAPPDATA,
        [string]$ProgramFilesRoot = ${env:ProgramFiles},
        [string]$ProgramFilesX86Root = ${env:ProgramFiles(x86)}
    )

    # LOCALAPPDATA and ProgramFiles are required when resolving defaults.
    # ProgramFiles(x86) is optional only when genuinely absent; if present it must validate.
    $localAppData = Assert-DevManagerKnownFolderRoot -Root $LocalAppDataRoot -Name 'LOCALAPPDATA' -Required
    $programFiles = Assert-DevManagerKnownFolderRoot -Root $ProgramFilesRoot -Name 'ProgramFiles' -Required
    $programFilesX86 = Assert-DevManagerKnownFolderRoot -Root $ProgramFilesX86Root -Name 'ProgramFiles(x86)'

    $paths = New-Object System.Collections.Generic.List[string]
    $paths.Add((Join-Path $localAppData 'DevManager\devmanager.exe'))
    $paths.Add((Join-Path $programFiles 'DevManager\devmanager.exe'))
    if (-not [string]::IsNullOrWhiteSpace($programFilesX86)) {
        $paths.Add((Join-Path $programFilesX86 'DevManager\devmanager.exe'))
    }

    return ,([string[]]@(
            $paths |
                ForEach-Object { Normalize-DevManagerPath -LiteralPath $_ } |
                Select-Object -Unique
        ))
}

function Normalize-DevManagerPath {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$LiteralPath
    )

    if ([string]::IsNullOrWhiteSpace($LiteralPath)) {
        throw "Missing executable identity: path is empty."
    }

    $expanded = [Environment]::ExpandEnvironmentVariables($LiteralPath.Trim())
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $expanded)) {
        throw "Path is not fully qualified (drive-relative and relative paths are rejected): '$LiteralPath'."
    }

    try {
        $full = [System.IO.Path]::GetFullPath($expanded)
    }
    catch {
        throw "Ambiguous executable identity: cannot normalize path '$LiteralPath'."
    }

    if (-not (Test-DevManagerAbsolutePath -LiteralPath $full)) {
        throw "Normalized path is not fully qualified: '$full'."
    }

    return $full.TrimEnd('\', '/').ToLowerInvariant()
}

function Get-ProtectedFileState {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath
    )

    $leaf = [System.IO.Path]::GetFileName($LiteralPath)
    if ($leaf -ieq 'session.json') {
        throw "Refusing to hash session.json; record sessionPath only."
    }

    if (-not (Test-Path -LiteralPath $LiteralPath)) {
        return [pscustomobject]@{
            exists = $false
            length = $null
            sha256 = $null
        }
    }

    $item = Get-Item -LiteralPath $LiteralPath -Force
    if ($item -is [System.IO.DirectoryInfo]) {
        throw "Protected path '$LiteralPath' is a directory, not a file."
    }

    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $LiteralPath).Hash.ToLowerInvariant()
    return [pscustomobject]@{
        exists = $true
        length = [int64]$item.Length
        sha256 = [string]$hash
    }
}

function Get-DevManagerInstalledProcesses {
    param(
        [string[]]$SupportedExecutablePaths,
        [object[]]$CimProcesses
    )

    if ($null -eq $SupportedExecutablePaths -or @($SupportedExecutablePaths).Count -eq 0) {
        $SupportedExecutablePaths = Get-DevManagerSupportedInstallPaths
    }

    $supported = @(
        $SupportedExecutablePaths |
            ForEach-Object { Normalize-DevManagerPath -LiteralPath ([string]$_) } |
            Select-Object -Unique
    )
    if ($supported.Count -eq 0) {
        throw "Missing executable identity: no supported install paths provided."
    }

    if ($null -eq $CimProcesses) {
        $CimProcesses = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    }
    else {
        $CimProcesses = @($CimProcesses)
    }

    $matched = New-Object System.Collections.Generic.List[object]
    foreach ($proc in $CimProcesses) {
        $name = [string]$proc.Name
        $rawPath = $null
        if ($null -ne $proc.PSObject.Properties['ExecutablePath']) {
            $rawPath = $proc.ExecutablePath
        }

        $hasPath = -not [string]::IsNullOrWhiteSpace([string]$rawPath)
        if (($name -ieq 'devmanager.exe') -and -not $hasPath) {
            throw "Missing executable identity for Win32_Process Name=devmanager.exe ProcessId=$($proc.ProcessId)."
        }
        if (-not $hasPath) {
            continue
        }

        $normalized = Normalize-DevManagerPath -LiteralPath ([string]$rawPath)
        if ($supported -notcontains $normalized) {
            continue
        }

        if ([string]::IsNullOrWhiteSpace([string]$proc.CreationDate)) {
            throw "Missing CreationDate for installed DevManager process $($proc.ProcessId)."
        }

        $matched.Add([pscustomobject]@{
                processId      = [uint32]$proc.ProcessId
                executablePath = [string](Normalize-DevManagerPath -LiteralPath ([string]$rawPath))
                # Preserve the CIM CreationDate string for exact start-time comparison.
                creationDate   = [string]$proc.CreationDate
            })
    }

    return @(
        $matched |
            Sort-Object -Property processId, executablePath, creationDate
    )
}

function Get-DevManagerProductionState {
    param(
        [string]$ProductionRoot,
        [string[]]$SupportedExecutablePaths,
        [object[]]$CimProcesses
    )

    if ([string]::IsNullOrWhiteSpace($ProductionRoot)) {
        $ProductionRoot = Get-DevManagerProductionRoot
    }
    else {
        if (-not (Test-DevManagerAbsolutePath -LiteralPath $ProductionRoot)) {
            throw "ProductionRoot must be a fully qualified path ('$ProductionRoot')."
        }
        $ProductionRoot = [System.IO.Path]::GetFullPath($ProductionRoot).TrimEnd('\', '/')
    }

    $installedProcesses = Get-DevManagerInstalledProcesses `
        -SupportedExecutablePaths $SupportedExecutablePaths `
        -CimProcesses $CimProcesses

    $processList = New-Object System.Collections.Generic.List[object]
    foreach ($proc in @($installedProcesses)) {
        $processList.Add([pscustomobject]@{
                processId      = [uint32]$proc.processId
                executablePath = [string]$proc.executablePath
                creationDate   = [string]$proc.creationDate
            })
    }

    return [pscustomobject]@{
        schemaVersion      = [int]1
        capturedAtUtc      = [DateTime]::UtcNow.ToString('o')
        productionRoot     = $ProductionRoot
        config             = Get-ProtectedFileState -LiteralPath (Join-Path $ProductionRoot 'config.json')
        remote             = Get-ProtectedFileState -LiteralPath (Join-Path $ProductionRoot 'remote.json')
        sessionPath        = Join-Path $ProductionRoot 'session.json'
        installedProcesses = [object[]]$processList.ToArray()
    }
}

function Get-DevManagerNormalizedPathComponents {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath
    )

    $normalized = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $root = [System.IO.Path]::GetPathRoot($normalized)
    if ([string]::IsNullOrWhiteSpace($root)) {
        throw "Ambiguous executable identity: path root missing for '$LiteralPath'."
    }

    $components = New-Object System.Collections.Generic.List[string]
    $components.Add($root.TrimEnd('\', '/').ToLowerInvariant())
    $relative = $normalized.Substring([Math]::Min($root.Length, $normalized.Length))
    foreach ($part in @($relative.Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))) {
        $components.Add($part.ToLowerInvariant())
    }
    # Unary comma prevents pipeline enumeration of the string[].
    return ,([string[]]$components.ToArray())
}

function Test-DevManagerPathEqualsOrBeneath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true)]
        [string]$AncestorPath
    )

    # Do not wrap with @() — that would nest the unary-comma array into a single element.
    $candidate = Get-DevManagerNormalizedPathComponents -LiteralPath $LiteralPath
    $ancestor = Get-DevManagerNormalizedPathComponents -LiteralPath $AncestorPath
    if ($candidate.Count -lt $ancestor.Count) {
        return $false
    }
    for ($i = 0; $i -lt $ancestor.Count; $i++) {
        if ($candidate[$i] -ne $ancestor[$i]) {
            return $false
        }
    }
    return $true
}

function Assert-DevManagerPathHasNoReparsePoints {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath
    )

    if ([string]::IsNullOrWhiteSpace($LiteralPath)) {
        throw "Evidence path is empty."
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $LiteralPath)) {
        throw "Evidence path must be absolute ('$LiteralPath')."
    }

    $full = [System.IO.Path]::GetFullPath($LiteralPath.Trim())
    $root = [System.IO.Path]::GetPathRoot($full)
    $relative = if ($full.Length -gt $root.Length) { $full.Substring($root.Length) } else { '' }
    $parts = @($relative.Split([char[]]@('\', '/'), [System.StringSplitOptions]::RemoveEmptyEntries))

    $current = $root.TrimEnd('\')
    foreach ($part in $parts) {
        $current = Join-Path $current $part
        if (-not (Test-Path -LiteralPath $current)) {
            break
        }
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Evidence path contains a reparse point component (junction/symlink): '$current'."
        }
    }
}

function Assert-DevManagerEvidencePathSafeForIO {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true)]
        [string]$ProtectedProductionRoot,
        [string]$AllowedEvidenceRoot
    )

    if ([string]::IsNullOrWhiteSpace($LiteralPath)) {
        throw "Evidence path is empty."
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $LiteralPath)) {
        throw "Evidence path must be absolute ('$LiteralPath')."
    }
    if ([string]::IsNullOrWhiteSpace($ProtectedProductionRoot)) {
        throw "Protected production root is required for evidence path validation."
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $ProtectedProductionRoot)) {
        throw "Protected production root must be absolute ('$ProtectedProductionRoot')."
    }

    $null = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $null = Normalize-DevManagerPath -LiteralPath $ProtectedProductionRoot

    if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $LiteralPath -AncestorPath $ProtectedProductionRoot) {
        throw "Evidence path '$LiteralPath' is equal to or beneath the protected production root '$ProtectedProductionRoot'."
    }

    if (-not [string]::IsNullOrWhiteSpace($AllowedEvidenceRoot)) {
        if (-not (Test-DevManagerAbsolutePath -LiteralPath $AllowedEvidenceRoot)) {
            throw "Allowed evidence root must be absolute ('$AllowedEvidenceRoot')."
        }
        $null = Normalize-DevManagerPath -LiteralPath $AllowedEvidenceRoot
        Assert-DevManagerPathHasNoReparsePoints -LiteralPath $AllowedEvidenceRoot
        if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $LiteralPath -AncestorPath $AllowedEvidenceRoot)) {
            throw "Evidence path '$LiteralPath' is outside the allowed worktree evidence root '$AllowedEvidenceRoot'."
        }
    }

    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $LiteralPath
}

function Get-DevManagerNativeNextWorktreeRoot {
    param(
        [string]$ScriptRoot = $PSScriptRoot
    )

    if ([string]::IsNullOrWhiteSpace($ScriptRoot)) {
        throw "ScriptRoot is required to resolve the native-next worktree root."
    }

    $scriptsDir = Split-Path -Parent $ScriptRoot
    $worktreeRoot = Split-Path -Parent $scriptsDir
    if ([string]::IsNullOrWhiteSpace($worktreeRoot)) {
        throw "Unable to resolve worktree root from ScriptRoot '$ScriptRoot'."
    }
    return [System.IO.Path]::GetFullPath($worktreeRoot)
}

function Get-DevManagerNativeNextEvidenceRoot {
    param(
        [string]$ScriptRoot = $PSScriptRoot
    )

    $worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $ScriptRoot
    return [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot '.devmanager-next\evidence'))
}

function Resolve-DevManagerEvidenceArgument {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [string]$ScriptRoot = $PSScriptRoot
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "Evidence path argument is empty."
    }

    $trimmed = $Path.Trim()
    if (Test-DevManagerAbsolutePath -LiteralPath $trimmed) {
        return [System.IO.Path]::GetFullPath($trimmed)
    }

    $worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $ScriptRoot
    return [System.IO.Path]::GetFullPath((Join-Path $worktreeRoot $trimmed))
}

function Write-DevManagerBaseline {
    param(
        [Parameter(Mandatory = $true)]
        [object]$State,
        [Parameter(Mandatory = $true)]
        [string]$OutputPath,
        [string]$AllowedEvidenceRoot
    )

    Assert-DevManagerEvidenceShape -Evidence $State -Label 'baseline state'
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $OutputPath `
        -ProtectedProductionRoot ([string]$State.productionRoot) `
        -AllowedEvidenceRoot $AllowedEvidenceRoot

    # Preserve zero-/one-element inventories as JSON arrays (never scalar objects).
    $State.installedProcesses = [object[]](Get-DevManagerInstalledProcessArray -Value $State.installedProcesses -Label 'baseline state')

    Publish-DevManagerAtomicJsonFile `
        -Value $State `
        -OutputPath $OutputPath `
        -ProtectedProductionRoot ([string]$State.productionRoot) `
        -AllowedEvidenceRoot $AllowedEvidenceRoot
}

function Test-DevManagerCurrentEvidenceBaselineOutput {
    param(
        [Parameter(Mandatory = $true)]
        [string]$LiteralPath,
        [Parameter(Mandatory = $true)]
        [string]$EvidenceRoot
    )

    if (-not (Test-DevManagerAbsolutePath -LiteralPath $LiteralPath)) {
        return $false
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $EvidenceRoot)) {
        return $false
    }

    $normalizedPath = Normalize-DevManagerPath -LiteralPath $LiteralPath
    $normalizedEvidence = Normalize-DevManagerPath -LiteralPath $EvidenceRoot
    $currentDir = Normalize-DevManagerPath -LiteralPath ([System.IO.Path]::GetFullPath((Join-Path $normalizedEvidence 'current')))
    $parent = Normalize-DevManagerPath -LiteralPath ([System.IO.Path]::GetFullPath((Split-Path -Parent $normalizedPath)))
    if ($parent -ne $currentDir) {
        return $false
    }

    $leaf = [System.IO.Path]::GetFileName($normalizedPath)
    if ([string]::IsNullOrWhiteSpace($leaf)) {
        return $false
    }
    if ($leaf -notmatch '(?i)\.json$') {
        return $false
    }
    if ($leaf -match '[\\/]') {
        return $false
    }
    return $true
}

function Read-DevManagerBaseline {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BaselinePath,
        [string]$ProtectedProductionRoot
    )

    if ([string]::IsNullOrWhiteSpace($ProtectedProductionRoot)) {
        $ProtectedProductionRoot = Get-DevManagerProductionRoot
    }

    # Validate before Test-Path/Get-Content so production session.json is never read.
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $BaselinePath `
        -ProtectedProductionRoot $ProtectedProductionRoot

    if (-not (Test-Path -LiteralPath $BaselinePath)) {
        throw "Baseline evidence not found: $BaselinePath"
    }

    try {
        $raw = Get-Content -LiteralPath $BaselinePath -Raw -Encoding utf8
        if ([string]::IsNullOrWhiteSpace($raw)) {
            throw "Baseline evidence is empty."
        }
        $baseline = $raw | ConvertFrom-Json
    }
    catch {
        throw "Malformed baseline evidence at '$BaselinePath': $($_.Exception.Message)"
    }

    Assert-DevManagerEvidenceShape -Evidence $baseline -Label 'baseline'
    return $baseline
}

function Test-DevManagerAbsolutePath {
    param(
        [Parameter(Mandatory = $true)]
        [AllowEmptyString()]
        [string]$LiteralPath
    )

    if ([string]::IsNullOrWhiteSpace($LiteralPath)) {
        return $false
    }

    # Reject drive-relative forms such as C:relative; require a fully qualified path.
    return [System.IO.Path]::IsPathFullyQualified($LiteralPath.Trim())
}

function Test-DevManagerIntegralNumber {
    param(
        [Parameter(Mandatory = $true)]
        [AllowNull()]
        [object]$Value
    )

    if ($null -eq $Value) {
        return $false
    }
    if ($Value -is [bool] -or $Value -is [string] -or $Value -is [char]) {
        return $false
    }
    if ($Value -is [float] -or $Value -is [double] -or $Value -is [decimal] -or $Value -is [single]) {
        return $false
    }

    return (
        $Value -is [byte] -or
        $Value -is [sbyte] -or
        $Value -is [int16] -or
        $Value -is [uint16] -or
        $Value -is [int32] -or
        $Value -is [uint32] -or
        $Value -is [int64] -or
        $Value -is [uint64]
    )
}

function Get-DevManagerInstalledProcessArray {
    param(
        [Parameter(Mandatory = $true)]
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    if ($null -eq $Value) {
        throw "Malformed ${Label}: installedProcesses must be a non-null array."
    }
    if ($Value -is [string] -or $Value -is [bool] -or $Value -is [hashtable]) {
        throw "Malformed ${Label}: installedProcesses must be an array (got $($Value.GetType().FullName))."
    }
    if ($Value -is [System.Management.Automation.PSCustomObject]) {
        throw "Malformed ${Label}: installedProcesses must be an array, not a scalar object."
    }
    if ($Value -is [System.Array]) {
        return ,([object[]]$Value)
    }
    if ($Value -is [System.Collections.IList]) {
        return ,([object[]]@($Value))
    }

    throw "Malformed ${Label}: installedProcesses must be an array (got $($Value.GetType().FullName))."
}

function Assert-DevManagerEvidenceShape {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Evidence,
        [string]$Label = 'evidence'
    )

    if ($null -eq $Evidence) {
        throw "Malformed ${Label}: object is null."
    }

    $required = @(
        'schemaVersion',
        'capturedAtUtc',
        'productionRoot',
        'config',
        'remote',
        'sessionPath',
        'installedProcesses'
    )
    foreach ($field in $required) {
        if (-not ($Evidence.PSObject.Properties.Name -contains $field)) {
            throw "Malformed ${Label}: missing required field '$field'."
        }
    }

    if (-not (Test-DevManagerIntegralNumber -Value $Evidence.schemaVersion)) {
        throw "Malformed ${Label}: schemaVersion must be an integral number (got '$($Evidence.schemaVersion)' / $($Evidence.schemaVersion.GetType().Name))."
    }
    # Compare without boolean/string coercion: already proven integral above.
    if ([int64]$Evidence.schemaVersion -ne 1) {
        throw "Malformed ${Label}: unsupported schemaVersion '$($Evidence.schemaVersion)'."
    }

    # Live state keeps a string; ConvertFrom-Json may promote ISO-8601 to DateTime/DateTimeOffset.
    if ($Evidence.capturedAtUtc -is [string]) {
        $capturedAtUtc = [string]$Evidence.capturedAtUtc
    }
    elseif ($Evidence.capturedAtUtc -is [datetimeoffset]) {
        $capturedAtUtc = ([datetimeoffset]$Evidence.capturedAtUtc).ToString('o')
    }
    elseif ($Evidence.capturedAtUtc -is [datetime]) {
        $capturedAtUtc = ([datetime]$Evidence.capturedAtUtc).ToUniversalTime().ToString('o')
    }
    else {
        throw "Malformed ${Label}: capturedAtUtc must be a string (got $($Evidence.capturedAtUtc.GetType().Name))."
    }
    if ([string]::IsNullOrWhiteSpace($capturedAtUtc)) {
        throw "Malformed ${Label}: capturedAtUtc is empty."
    }
    try {
        $null = [DateTimeOffset]::Parse(
            $capturedAtUtc,
            [System.Globalization.CultureInfo]::InvariantCulture,
            [System.Globalization.DateTimeStyles]::RoundtripKind
        )
    }
    catch {
        throw "Malformed ${Label}: capturedAtUtc is unparseable ('$capturedAtUtc')."
    }

    if ($Evidence.productionRoot -isnot [string]) {
        throw "Malformed ${Label}: productionRoot must be a string."
    }
    $productionRoot = [string]$Evidence.productionRoot
    if ([string]::IsNullOrWhiteSpace($productionRoot)) {
        throw "Malformed ${Label}: productionRoot is empty."
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $productionRoot)) {
        throw "Malformed ${Label}: productionRoot must be fully qualified ('$productionRoot')."
    }
    try {
        $null = Normalize-DevManagerPath -LiteralPath $productionRoot
    }
    catch {
        throw "Malformed ${Label}: productionRoot is unnormalizable ('$productionRoot')."
    }

    if ($Evidence.sessionPath -isnot [string]) {
        throw "Malformed ${Label}: sessionPath must be a string."
    }
    $sessionPath = [string]$Evidence.sessionPath
    if ([string]::IsNullOrWhiteSpace($sessionPath)) {
        throw "Malformed ${Label}: sessionPath is empty."
    }
    $expectedSession = Normalize-DevManagerPath -LiteralPath (Join-Path $productionRoot 'session.json')
    try {
        $normalizedSession = Normalize-DevManagerPath -LiteralPath $sessionPath
    }
    catch {
        throw "Malformed ${Label}: sessionPath is unnormalizable ('$sessionPath')."
    }
    if ($normalizedSession -ne $expectedSession) {
        throw "Malformed ${Label}: sessionPath must be exactly <productionRoot>\session.json (got '$sessionPath')."
    }

    Assert-ProtectedFileEvidenceShape -FileState $Evidence.config -Label "$Label.config"
    Assert-ProtectedFileEvidenceShape -FileState $Evidence.remote -Label "$Label.remote"

    $processes = Get-DevManagerInstalledProcessArray -Value $Evidence.installedProcesses -Label $Label
    foreach ($proc in $processes) {
        if ($null -eq $proc -or $proc -isnot [System.Management.Automation.PSCustomObject]) {
            # Live state and ConvertFrom-Json both surface objects; also accept Hashtable-like only if PSObject.
            if ($null -eq $proc -or $null -eq $proc.PSObject) {
                throw "Malformed ${Label}: installed process entry must be an object."
            }
        }
        foreach ($field in @('processId', 'executablePath', 'creationDate')) {
            if (-not ($proc.PSObject.Properties.Name -contains $field)) {
                throw "Malformed ${Label}: installed process missing '$field'."
            }
        }

        $processIdRaw = $proc.processId
        if (-not (Test-DevManagerIntegralNumber -Value $processIdRaw)) {
            throw "Malformed ${Label}: installed process processId must be an integral number (got '$processIdRaw')."
        }
        # Range check only after exact integral-type validation.
        if ([int64]$processIdRaw -le 0 -or [uint64]$processIdRaw -gt [uint32]::MaxValue) {
            throw "Malformed ${Label}: installed process processId must be a non-zero uint32 ('$processIdRaw')."
        }

        if ($proc.executablePath -isnot [string]) {
            throw "Malformed ${Label}: installed process executablePath must be a string."
        }
        $executablePath = [string]$proc.executablePath
        if ([string]::IsNullOrWhiteSpace($executablePath)) {
            throw "Malformed ${Label}: installed process executablePath is empty."
        }
        if (-not (Test-DevManagerAbsolutePath -LiteralPath $executablePath)) {
            throw "Malformed ${Label}: installed process executablePath must be fully qualified ('$executablePath')."
        }
        try {
            $null = Normalize-DevManagerPath -LiteralPath $executablePath
        }
        catch {
            throw "Malformed ${Label}: installed process executablePath is unnormalizable ('$executablePath')."
        }

        if ($proc.creationDate -isnot [string]) {
            throw "Malformed ${Label}: installed process creationDate must be a string."
        }
        if ([string]::IsNullOrWhiteSpace([string]$proc.creationDate)) {
            throw "Malformed ${Label}: installed process creationDate is empty."
        }
    }
}

function Assert-ProtectedFileEvidenceShape {
    param(
        [Parameter(Mandatory = $true)]
        [object]$FileState,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    foreach ($field in @('exists', 'length', 'sha256')) {
        if (-not ($FileState.PSObject.Properties.Name -contains $field)) {
            throw "Malformed ${Label}: missing required field '$field'."
        }
    }

    if ($FileState.exists -isnot [bool]) {
        throw "Malformed ${Label}: exists must be a boolean (got '$($FileState.exists)')."
    }

    $exists = $FileState.exists
    if ($exists) {
        if ($null -eq $FileState.length) {
            throw "Malformed ${Label}: length is required when exists=true."
        }
        if (-not (Test-DevManagerIntegralNumber -Value $FileState.length)) {
            throw "Malformed ${Label}: length must be an integral number (got '$($FileState.length)')."
        }
        $length = [int64]$FileState.length
        if ($length -lt 0) {
            throw "Malformed ${Label}: length must not be negative ('$length')."
        }

        if ($FileState.sha256 -isnot [string]) {
            throw "Malformed ${Label}: sha256 must be a string."
        }
        $sha = [string]$FileState.sha256
        if ($sha -notmatch '^[0-9a-fA-F]{64}$') {
            throw "Malformed ${Label}: sha256 must be exactly 64 hex characters."
        }
    }
    else {
        if ($null -ne $FileState.length) {
            throw "Malformed ${Label}: length must be null when exists=false."
        }
        if ($null -ne $FileState.sha256) {
            throw "Malformed ${Label}: sha256 must be null when exists=false."
        }
    }
}

function Get-ProtectedFileCompareKey {
    param([object]$FileState)

    $exists = [bool]$FileState.exists
    if (-not $exists) {
        return 'exists=false'
    }

    $sha = ([string]$FileState.sha256).ToLowerInvariant()
    return "exists=true;length=$([int64]$FileState.length);sha256=$sha"
}

function Get-InstalledProcessCompareKey {
    param([object]$Process)

    $path = Normalize-DevManagerPath -LiteralPath ([string]$Process.executablePath)
    return "pid=$([uint32]$Process.processId);exe=$path;start=$([string]$Process.creationDate)"
}

function Assert-DevManagerProductionState {
    param(
        [string]$BaselinePath,
        [object]$Baseline,
        [object]$Current,
        [string]$ProductionRoot,
        [string[]]$SupportedExecutablePaths,
        [object[]]$CimProcesses
    )

    if ($null -eq $Baseline) {
        if ([string]::IsNullOrWhiteSpace($BaselinePath)) {
            throw "BaselinePath or Baseline is required."
        }
        $Baseline = Read-DevManagerBaseline -BaselinePath $BaselinePath
    }
    else {
        Assert-DevManagerEvidenceShape -Evidence $Baseline -Label 'baseline'
    }

    if ($null -eq $Current) {
        $Current = Get-DevManagerProductionState `
            -ProductionRoot $ProductionRoot `
            -SupportedExecutablePaths $SupportedExecutablePaths `
            -CimProcesses $CimProcesses
    }
    else {
        Assert-DevManagerEvidenceShape -Evidence $Current -Label 'current'
    }

    $baselineRoot = Normalize-DevManagerPath -LiteralPath ([string]$Baseline.productionRoot)
    $currentRoot = Normalize-DevManagerPath -LiteralPath ([string]$Current.productionRoot)
    if ($baselineRoot -ne $currentRoot) {
        throw "Production root mismatch: baseline='$($Baseline.productionRoot)' current='$($Current.productionRoot)'."
    }

    $baselineConfig = Get-ProtectedFileCompareKey -FileState $Baseline.config
    $currentConfig = Get-ProtectedFileCompareKey -FileState $Current.config
    if ($baselineConfig -ne $currentConfig) {
        throw "Protected file config.json mismatch (exists/length/hash): baseline=[$baselineConfig] current=[$currentConfig]."
    }

    $baselineRemote = Get-ProtectedFileCompareKey -FileState $Baseline.remote
    $currentRemote = Get-ProtectedFileCompareKey -FileState $Current.remote
    if ($baselineRemote -ne $currentRemote) {
        throw "Protected file remote.json mismatch (exists/length/hash): baseline=[$baselineRemote] current=[$currentRemote]."
    }

    $baselineProcessObjects = Get-DevManagerInstalledProcessArray -Value $Baseline.installedProcesses -Label 'baseline'
    $currentProcessObjects = Get-DevManagerInstalledProcessArray -Value $Current.installedProcesses -Label 'current'
    $baselineProcesses = @($baselineProcessObjects | ForEach-Object { Get-InstalledProcessCompareKey -Process $_ } | Sort-Object)
    $currentProcesses = @($currentProcessObjects | ForEach-Object { Get-InstalledProcessCompareKey -Process $_ } | Sort-Object)
    if ($baselineProcesses.Count -ne $currentProcesses.Count) {
        throw "Installed process count mismatch: baseline=$($baselineProcesses.Count) current=$($currentProcesses.Count)."
    }
    for ($i = 0; $i -lt $baselineProcesses.Count; $i++) {
        if ($baselineProcesses[$i] -ne $currentProcesses[$i]) {
            throw "Installed process identity mismatch (pid/executable/start-time): baseline='$($baselineProcesses[$i])' current='$($currentProcesses[$i])'."
        }
    }

    return $true
}

function Publish-DevManagerAtomicJsonFile {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$OutputPath,
        [Parameter(Mandatory = $true)]
        [string]$ProtectedProductionRoot,
        [string]$AllowedEvidenceRoot
    )

    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $OutputPath `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $AllowedEvidenceRoot

    $directory = Split-Path -Parent $OutputPath
    if ([string]::IsNullOrWhiteSpace($directory)) {
        throw "Output path has no directory component: '$OutputPath'."
    }

    if (-not (Test-Path -LiteralPath $directory)) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }

    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $directory `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $AllowedEvidenceRoot
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $OutputPath `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $AllowedEvidenceRoot

    $replaceCurrentEvidence = $false
    if (-not [string]::IsNullOrWhiteSpace($AllowedEvidenceRoot)) {
        $replaceCurrentEvidence = Test-DevManagerCurrentEvidenceBaselineOutput `
            -LiteralPath $OutputPath `
            -EvidenceRoot $AllowedEvidenceRoot
    }

    if ((Test-Path -LiteralPath $OutputPath) -and -not $replaceCurrentEvidence) {
        throw "Refusing to overwrite existing evidence file '$OutputPath'."
    }

    $tempName = '.pending-{0}.json' -f ([guid]::NewGuid().ToString('N'))
    $tempPath = Join-Path $directory $tempName
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $tempPath `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $AllowedEvidenceRoot

    $json = $Value | ConvertTo-Json -Depth 8
    try {
        Set-Content -LiteralPath $tempPath -Value $json -Encoding utf8
        Assert-DevManagerEvidencePathSafeForIO `
            -LiteralPath $OutputPath `
            -ProtectedProductionRoot $ProtectedProductionRoot `
            -AllowedEvidenceRoot $AllowedEvidenceRoot
        if ((Test-Path -LiteralPath $OutputPath) -and -not $replaceCurrentEvidence) {
            throw "Refusing to overwrite existing evidence file '$OutputPath'."
        }
        if ((Test-Path -LiteralPath $OutputPath) -and $replaceCurrentEvidence) {
            $backupName = '.backup-{0}.json' -f ([guid]::NewGuid().ToString('N'))
            $backupPath = Join-Path $directory $backupName
            [System.IO.File]::Replace($tempPath, $OutputPath, $backupPath)
            Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue
        }
        else {
            [System.IO.File]::Move($tempPath, $OutputPath)
        }
    }
    catch {
        if (Test-Path -LiteralPath $tempPath) {
            Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue
        }
        throw
    }
}

function Write-DevManagerJsonEvidence {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$OutputPath,
        [Parameter(Mandatory = $true)]
        [string]$ProtectedProductionRoot,
        [Parameter(Mandatory = $true)]
        [string]$AllowedEvidenceRoot
    )

    Publish-DevManagerAtomicJsonFile `
        -Value $Value `
        -OutputPath $OutputPath `
        -ProtectedProductionRoot $ProtectedProductionRoot `
        -AllowedEvidenceRoot $AllowedEvidenceRoot
}
