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
$rowReports = @()
$nodeReports = @()
$entrypointFindings = New-Object 'System.Collections.Generic.List[string]'
$protectedTrackedFiles = New-Object 'System.Collections.Generic.List[string]'
$trackedFiles = @()
$contract = $null
$reportPath = $null
$humanPath = $null
$evidenceRoot = $null
$rootPath = $null

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
        $contractErrors.Add($Message.Trim())
    }
}

function Add-GlobalBlocker {
    param([Parameter(Mandatory = $true)][string]$Message)

    if (-not [string]::IsNullOrWhiteSpace($Message)) {
        $globalBlockers.Add($Message.Trim())
    }
}

function Normalize-ContractRelativePath {
    param(
        [object]$Value,
        [Parameter(Mandatory = $true)][string]$Label
    )

    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        Add-ContractError "${Label} is missing or empty."
        return $null
    }

    $path = ([string]$Value).Trim().Replace('\', '/')
    $path = $path.TrimEnd('/')
    if ([string]::IsNullOrWhiteSpace($path)) {
        Add-ContractError "${Label} must be a normalized repository-relative path."
        return $null
    }
    if ($path.StartsWith('/') -or $path -match '^[A-Za-z]:/' -or $path -eq '.' -or $path.Contains('//')) {
        Add-ContractError "${Label} must be a normalized repository-relative path: '$path'."
        return $null
    }
    if ($path -match '(^|/)\.\.(/|$)' -or $path -match '(^|/)\.$') {
        Add-ContractError "${Label} may not escape or name the repository root: '$path'."
        return $null
    }
    return $path
}

function Normalize-TrackedPath {
    param(
        [Parameter(Mandatory = $true)][string]$RawPath,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $path = $RawPath.Trim().Replace('\', '/')
    if ([System.IO.Path]::IsPathRooted($path)) {
        $path = [System.IO.Path]::GetRelativePath($RepositoryRoot, $path).Replace('\', '/')
    }
    while ($path.StartsWith('./')) {
        $path = $path.Substring(2)
    }
    return $path
}

function Test-TrackedPathPresent {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string[]]$Tracked
    )

    foreach ($tracked in $Tracked) {
        if ($tracked -eq $Path -or $tracked.StartsWith("$Path/", [System.StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

function Read-CutoverContract {
    param(
        [Parameter(Mandatory = $true)][string]$LedgerPath
    )

    if (-not (Test-Path -LiteralPath $LedgerPath -PathType Leaf)) {
        throw "Ledger file is missing: docs/replacement-deletion-ledger.md"
    }

    $lines = @(Get-Content -LiteralPath $LedgerPath -Encoding utf8)
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

    $output = & git -C $RepositoryRoot ls-files --full-name 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "git ls-files failed: $($output -join ' ')"
    }

    $normalized = New-Object 'System.Collections.Generic.List[string]'
    foreach ($line in @($output)) {
        $path = ([string]$line).Trim().Replace('\', '/')
        if (-not [string]::IsNullOrWhiteSpace($path)) {
            $normalized.Add($path)
        }
    }
    return @($normalized | Sort-Object -Unique)
}

function Assert-NodeGraph {
    param(
        [Parameter(Mandatory = $true)][object[]]$Nodes,
        [Parameter(Mandatory = $true)][hashtable]$NodeById
    )

    $visitState = @{}
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
        [Parameter(Mandatory = $true)][hashtable]$NeedleKeys,
        [Parameter(Mandatory = $true)][string]$OwnerId,
        [Parameter(Mandatory = $true)][string]$Kind,
        [object]$Value
    )

    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        return
    }
    $needle = ([string]$Value).Trim()
    $key = "$OwnerId|$Kind|$needle"
    if ($NeedleKeys.ContainsKey($key)) {
        return
    }
    $NeedleKeys[$key] = $true
    $Needles.Add([pscustomobject]@{
            ownerId = $OwnerId
            kind    = $Kind
            needle  = $needle
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
    if ($Needles.Count -eq 0) {
        return @($matches)
    }

    $arguments = @(
        '--json',
        '--fixed-strings',
        '--line-number',
        '--no-heading',
        '--color',
        'never',
        '--glob',
        '!.git/**',
        '--glob',
        '!session.json',
        '--glob',
        '!**/session.json'
    )
    foreach ($needle in $Needles) {
        $arguments += '-e'
        $arguments += [string]$needle.needle
    }
    $arguments += '.'

    $rawOutput = @()
    $rgExitCode = 0
    Push-Location $RepositoryRoot
    try {
        $rawOutput = @(& rg @arguments 2>$null)
        $rgExitCode = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
    if ($rgExitCode -gt 1) {
        Add-GlobalBlocker "rg reference scan failed with exit code $rgExitCode."
    }

    $trackedSet = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($path in $Tracked) {
        $null = $trackedSet.Add($path)
    }

    foreach ($rawLine in $rawOutput) {
        if ($rawLine -isnot [string] -or [string]::IsNullOrWhiteSpace([string]$rawLine)) {
            continue
        }
        try {
            $event = ([string]$rawLine | ConvertFrom-Json -Depth 30)
        }
        catch {
            Add-GlobalBlocker 'rg returned a non-JSON event in JSON mode.'
            continue
        }
        if ([string](Get-ContractProperty -Object $event -Name 'type') -ne 'match') {
            continue
        }

        $data = Get-ContractProperty -Object $event -Name 'data'
        $pathData = Get-ContractProperty -Object $data -Name 'path'
        $rawPath = [string](Get-ContractProperty -Object $pathData -Name 'text')
        if ([string]::IsNullOrWhiteSpace($rawPath)) {
            continue
        }
        $relativePath = Normalize-TrackedPath -RawPath $rawPath -RepositoryRoot $RepositoryRoot
        if (-not $trackedSet.Contains($relativePath)) {
            continue
        }
        if ($relativePath -eq 'docs/replacement-deletion-ledger.md') {
            continue
        }
        if ([System.IO.Path]::GetFileName($relativePath) -ieq 'session.json') {
            continue
        }

        $submatches = @()
        foreach ($submatch in Get-ContractArray (Get-ContractProperty -Object $data -Name 'submatches')) {
            $matchData = Get-ContractProperty -Object $submatch -Name 'match'
            $matchText = [string](Get-ContractProperty -Object $matchData -Name 'text')
            if (-not [string]::IsNullOrWhiteSpace($matchText)) {
                $submatches += $matchText
            }
        }
        if ($submatches.Count -eq 0) {
            continue
        }

        $lineNumber = 0
        $lineValue = Get-ContractProperty -Object $data -Name 'line_number'
        if ($null -ne $lineValue) {
            $lineNumber = [int]$lineValue
        }
        foreach ($needle in $Needles) {
            if (-not ($submatches -contains [string]$needle.needle)) {
                continue
            }
            $matches.Add([pscustomobject]@{
                    ownerId = [string]$needle.ownerId
                    kind    = [string]$needle.kind
                    path    = $relativePath
                    line    = $lineNumber
                })
        }
    }

    # Choose the lexicographically smallest bounded prefix after sorting. The
    # rg event order is deliberately not treated as deterministic.
    $bounded = @()
    $counts = @{}
    foreach ($match in @($matches | Sort-Object ownerId, kind, path, line -Unique)) {
        $key = "$($match.ownerId)|$($match.kind)"
        if (-not $counts.ContainsKey($key)) {
            $counts[$key] = 0
        }
        if ([int]$counts[$key] -ge $MaxMatches) {
            continue
        }
        $counts[$key] = [int]$counts[$key] + 1
        $bounded += $match
    }
    return @($bounded)
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
    if ([string]::IsNullOrWhiteSpace($candidate)) {
        $candidate = Join-Path $EvidenceRoot 'current/cutover-audit.json'
    }
    elseif (-not [System.IO.Path]::IsPathRooted($candidate)) {
        $candidate = Join-Path $RepositoryRoot $candidate
    }
    $full = [System.IO.Path]::GetFullPath($candidate)
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $full -AncestorPath $EvidenceRoot)) {
        throw "OutputPath must remain beneath .devmanager-next/evidence."
    }
    if ([System.IO.Path]::GetExtension($full) -ine '.json') {
        throw 'OutputPath must end in .json.'
    }
    $parent = Split-Path -Parent $full
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    return $full
}

function Write-AuditReports {
    param(
        [Parameter(Mandatory = $true)][object]$Report,
        [Parameter(Mandatory = $true)][string]$JsonPath,
        [Parameter(Mandatory = $true)][string]$TextPath
    )

    $json = $Report | ConvertTo-Json -Depth 50
    [System.IO.File]::WriteAllText($JsonPath, $json, [System.Text.UTF8Encoding]::new($false))

    $lines = New-Object 'System.Collections.Generic.List[string]'
    $lines.Add('Phase 11.1 cutover audit')
    $lines.Add("status: $($Report.contractStatus)")
    $lines.Add("mode: $($Report.mode)")
    $lines.Add("tracked files: $($Report.trackedFileCount)")
    $lines.Add("protected exact session.json files skipped: $(@($Report.protectedFilesSkipped).Count)")
    $lines.Add('')
    $lines.Add('prerequisite nodes:')
    foreach ($node in @($Report.prerequisiteNodes)) {
        $lines.Add("- $($node.id): $($node.kind); status=$($node.status)")
        foreach ($artifact in @($node.evidence)) {
            $lines.Add("  evidence: $($artifact.path); present=$($artifact.present)")
        }
    }
    $lines.Add('')
    $lines.Add('contract errors:')
    if (@($Report.contractErrors).Count -eq 0) {
        $lines.Add('- none')
    }
    else {
        foreach ($error in @($Report.contractErrors)) { $lines.Add("- $error") }
    }
    $lines.Add('blockers:')
    if (@($Report.blockers).Count -eq 0) {
        $lines.Add('- none')
    }
    else {
        foreach ($blocker in @($Report.blockers)) { $lines.Add("- $blocker") }
    }
    $lines.Add('rows:')
    foreach ($row in @($Report.rows)) {
        $legacy = $row.legacy.path
        $present = [bool]$row.legacy.pathPresent
        $lines.Add("- $($row.id): $($row.status); legacy=$legacy; present=$present")
        foreach ($blocker in @($row.blockers)) {
            $lines.Add("  blocker: $blocker")
        }
    }
    $lines.Add('forbidden entrypoint findings:')
    if (@($Report.entrypointFindings).Count -eq 0) {
        $lines.Add('- none')
    }
    else {
        foreach ($finding in @($Report.entrypointFindings)) { $lines.Add("- $finding") }
    }
    [System.IO.File]::WriteAllText($TextPath, ($lines -join [Environment]::NewLine) + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
}

try {
    if ([string]::IsNullOrWhiteSpace($Root)) {
        $rootPath = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
    }
    else {
        $rootPath = Assert-DevManagerKnownFolderRoot -Root $Root -Name 'Root' -Required
    }
    if (-not (Test-Path -LiteralPath $rootPath -PathType Container)) {
        throw "Root directory is missing: $rootPath"
    }

    $evidenceRoot = [System.IO.Path]::GetFullPath((Join-Path $rootPath '.devmanager-next\evidence'))
    $reportPath = Assert-AuditOutputPath `
        -RepositoryRoot $rootPath `
        -EvidenceRoot $evidenceRoot `
        -RequestedPath $OutputPath
    $humanPath = [System.IO.Path]::ChangeExtension($reportPath, '.txt')

    $trackedFiles = Invoke-GitTrackedFiles -RepositoryRoot $rootPath
    foreach ($tracked in $trackedFiles) {
        if ([System.IO.Path]::GetFileName($tracked) -ieq 'session.json') {
            $protectedTrackedFiles.Add($tracked)
        }
    }

    $ledgerPath = Join-Path $rootPath 'docs/replacement-deletion-ledger.md'
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

    $nodeById = @{}
    $nodes = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'prerequisiteNodes'))
    foreach ($node in $nodes) {
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
        $nodeDependencies = @(Get-ContractArray (Get-ContractProperty -Object $node -Name 'dependsOn') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $nodeEvidence = @(Get-ContractArray (Get-ContractProperty -Object $node -Name 'evidence'))
        if ($nodeEvidence.Count -eq 0) {
            Add-ContractError "prerequisite node '$nodeId' has no evidence artifact declaration."
        }
        $nodeEvidenceReports = @()
        foreach ($artifact in $nodeEvidence) {
            $artifactPath = Normalize-ContractRelativePath -Value $artifact -Label "prerequisite node '$nodeId' evidence artifact"
            $artifactPresent = $false
            if ($null -ne $artifactPath) {
                $artifactPresent = Test-Path -LiteralPath (Join-Path $rootPath $artifactPath) -PathType Leaf
                if (-not $artifactPresent) {
                    Add-GlobalBlocker "prerequisite node '$nodeId' missing evidence artifact: $artifactPath"
                }
            }
            $nodeEvidenceReports += [pscustomobject]@{
                path = $artifactPath
                present = $artifactPresent
            }
        }
        $nodeReports += [pscustomobject]@{
            id = $nodeId
            kind = $nodeKind
            status = $nodeStatus
            dependsOn = $nodeDependencies
            evidence = @($nodeEvidenceReports)
        }
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

    $rowById = @{}
    $rowModels = New-Object 'System.Collections.Generic.List[object]'
    $legacyPathOwners = @{}
    $rows = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'rows'))
    if ($rows.Count -eq 0) {
        Add-ContractError 'rows must contain at least one ledger row.'
    }
    foreach ($row in $rows) {
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
            $legacyKey = $legacyPath.ToLowerInvariant()
            if ($legacyPathOwners.ContainsKey($legacyKey)) {
                Add-ContractError "duplicate legacy path '$legacyPath' in rows '$($legacyPathOwners[$legacyKey])' and '$rowId'."
            }
            else {
                $legacyPathOwners[$legacyKey] = $rowId
            }
        }

        $symbols = @(Get-ContractArray (Get-ContractProperty -Object $legacy -Name 'symbols') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($symbols.Count -eq 0) {
            Add-ContractError "row '$rowId' legacy.symbols must contain at least one symbol."
        }
        $tokens = @(Get-ContractArray (Get-ContractProperty -Object $legacy -Name 'tokens') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $prerequisites = @(Get-ContractArray (Get-ContractProperty -Object $row -Name 'prerequisites') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        if ($prerequisites.Count -eq 0) {
            Add-ContractError "row '$rowId' has no prerequisite phase/gate."
        }
        $evidence = Get-ContractProperty -Object $row -Name 'evidence'
        $commands = @(Get-ContractArray (Get-ContractProperty -Object $evidence -Name 'commands') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        $artifacts = @(Get-ContractArray (Get-ContractProperty -Object $evidence -Name 'artifacts') | ForEach-Object { [string]$_ } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
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
    $needleKeys = @{}
    foreach ($model in $rowModels) {
        Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'path' -Value $model.legacyPath
        foreach ($symbol in $model.symbols) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'symbol' -Value $symbol
        }
        foreach ($token in $model.tokens) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId $model.id -Kind 'token' -Value $token
        }
    }

    $forbidden = @(Get-ContractArray (Get-ContractProperty -Object $contract -Name 'forbiddenEntrypoints'))
    if ($forbidden.Count -eq 0) {
        Add-ContractError 'forbiddenEntrypoints must contain at least one legacy entrypoint contract.'
    }
    foreach ($entrypoint in $forbidden) {
        $entrypointId = [string](Get-ContractProperty -Object $entrypoint -Name 'id')
        $entrypointPath = Normalize-ContractRelativePath `
            -Value (Get-ContractProperty -Object $entrypoint -Name 'path') `
            -Label "forbidden entrypoint '$entrypointId' path"
        if ([string]::IsNullOrWhiteSpace($entrypointId) -or $null -eq $entrypointPath) {
            continue
        }
        Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId "entrypoint:$entrypointId" -Kind 'path' -Value $entrypointPath
        foreach ($token in @(Get-ContractArray (Get-ContractProperty -Object $entrypoint -Name 'tokens'))) {
            Add-Needle -Needles $needles -NeedleKeys $needleKeys -OwnerId "entrypoint:$entrypointId" -Kind 'token' -Value $token
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

    $scanMatches = Invoke-ReferenceScan `
        -RepositoryRoot $rootPath `
        -Tracked $trackedFiles `
        -Needles $needles `
        -MaxMatches $maxMatches

    foreach ($match in $scanMatches | Where-Object { $_.ownerId -like 'entrypoint:*' }) {
        $entrypointFindings.Add("$($match.ownerId.Substring(11)):$($match.path)")
    }

    foreach ($model in $rowModels) {
        $rowBlockers = New-Object 'System.Collections.Generic.List[string]'
        $pathPresent = $false
        if ($null -ne $model.legacyPath) {
            $pathPresent = Test-TrackedPathPresent -Path $model.legacyPath -Tracked $trackedFiles
            if (-not $pathPresent -and $model.status -ne 'DELETED') {
                Add-ContractError "row '$($model.id)' legacy path is missing from tracked repository: $($model.legacyPath)."
            }
        }
        $replacementPresent = $false
        if ($null -ne $model.replacementPath) {
            $replacementPresent = Test-TrackedPathPresent -Path $model.replacementPath -Tracked $trackedFiles
            if (-not $replacementPresent) {
                Add-ContractError "row '$($model.id)' replacement owner path is missing from tracked repository: $($model.replacementPath)."
            }
        }

        $artifactReports = @()
        foreach ($artifact in $model.artifacts) {
            $artifactPath = Normalize-ContractRelativePath -Value $artifact -Label "row '$($model.id)' evidence artifact"
            $artifactPresent = $false
            if ($null -ne $artifactPath) {
                $artifactPresent = Test-Path -LiteralPath (Join-Path $rootPath $artifactPath) -PathType Leaf
                if (-not $artifactPresent) {
                    $rowBlockers.Add("missing evidence artifact: $artifactPath")
                }
            }
            $artifactReports += [pscustomobject]@{ path = $artifactPath; present = $artifactPresent }
        }

        foreach ($prerequisite in $model.prerequisites) {
            $prerequisiteStatus = $null
            if ($nodeById.ContainsKey($prerequisite)) {
                $prerequisiteStatus = [string](Get-ContractProperty -Object $nodeById[$prerequisite] -Name 'status')
            }
            if ($prerequisiteStatus -ne 'READY') {
                $rowBlockers.Add("prerequisite is not READY: $prerequisite (status=$prerequisiteStatus)")
            }
        }

        $rowMatches = @($scanMatches | Where-Object { $_.ownerId -eq $model.id })
        $references = [ordered]@{}
        foreach ($kind in @('path', 'symbol', 'token')) {
            $references[$kind] = @($rowMatches | Where-Object { $_.kind -eq $kind } | Select-Object -ExpandProperty path -Unique | Sort-Object)
        }
        if ($model.status -eq 'DELETED') {
            if ($pathPresent) {
                $rowBlockers.Add("legacy path still present: $($model.legacyPath)")
            }
            foreach ($kind in @('path', 'symbol', 'token')) {
                if (@($references[$kind]).Count -gt 0) {
                    $rowBlockers.Add("legacy $kind references remain: $(@($references[$kind]) -join ', ')")
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
                        artifacts = @($artifactReports)
                    }
                    references = $references
                    blockers = @($rowBlockers | Sort-Object -Unique)
                })
        $rowReports += $rowDocument
        foreach ($blocker in @($rowBlockers)) {
            Add-GlobalBlocker "row '$($model.id)': $blocker"
        }
    }

    $entrypointFindings = @($entrypointFindings | Sort-Object -Unique)
    foreach ($finding in $entrypointFindings) {
        Add-GlobalBlocker "forbidden legacy entrypoint finding: $finding"
    }
    $contractErrors = @($contractErrors | Sort-Object -Unique)
    $globalBlockers = @($globalBlockers | Sort-Object -Unique)

    $allRowsTerminal = $rowReports.Count -gt 0 -and @($rowReports | Where-Object { $_.status -eq 'HOLD' }).Count -eq 0
    $contractStatus = if ($contractErrors.Count -gt 0 -or $globalBlockers.Count -gt 0 -or -not $allRowsTerminal) { 'HOLD' } else { 'READY' }
}
catch {
    $lineNumber = $_.InvocationInfo.ScriptLineNumber
    $sourceLine = ([string]$_.InvocationInfo.Line).Trim()
    Add-ContractError "fatal audit error at line ${lineNumber}: $($_.Exception.Message) [$sourceLine]"
    $contractStatus = 'HOLD'
}

if ($null -eq $rootPath) {
    $rootPath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
}
if ($null -eq $evidenceRoot) {
    $evidenceRoot = [System.IO.Path]::GetFullPath((Join-Path $rootPath '.devmanager-next\evidence'))
}
if ($null -eq $reportPath) {
    $reportPath = [System.IO.Path]::GetFullPath((Join-Path $evidenceRoot 'current/cutover-audit.json'))
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $reportPath) | Out-Null
}
if ($null -eq $humanPath) {
    $humanPath = [System.IO.Path]::ChangeExtension($reportPath, '.txt')
}

$report = [pscustomobject]([ordered]@{
        schemaVersion = 1
        contractId = [string](Get-ContractProperty -Object $contract -Name 'contractId')
        mode = $Mode
        contractStatus = $contractStatus
        ledgerPath = 'docs/replacement-deletion-ledger.md'
        trackedFileCount = @($trackedFiles).Count
        protectedFilesSkipped = @($protectedTrackedFiles | Sort-Object -Unique)
        contractErrors = @($contractErrors | Sort-Object -Unique)
        blockers = @($globalBlockers | Sort-Object -Unique)
        entrypointFindings = @($entrypointFindings | Sort-Object -Unique)
        prerequisiteNodes = @($nodeReports | Sort-Object id)
        rows = @($rowReports | Sort-Object id)
        scanner = [ordered]@{
            trackedUniverse = 'git-ls-files'
            referenceScanner = 'rg --fixed-strings --line-number'
            allowedLedgerSelfReferences = @('docs/replacement-deletion-ledger.md')
            protectedFileBasenames = @('session.json')
            maxMatchesPerRow = $maxMatches
        }
    })

try {
    Write-AuditReports -Report $report -JsonPath $reportPath -TextPath $humanPath
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
