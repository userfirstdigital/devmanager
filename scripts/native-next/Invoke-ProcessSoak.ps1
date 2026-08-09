# Phase 3.10 guarded process/terminal soak runner.
#
# The optional cycle extension is deliberately loaded only after the real
# production baseline has been captured. A cycle is completed only when it
# returns the exact versioned evidence contract below and every emitted exact
# process/resource identity has settled. Missing or incomplete host/client API
# support is unavailable (78), never a passing no-op.

[CmdletBinding()]
param(
    [ValidateRange(1, 1000)]
    [int]$Iterations = 100,

    [int]$Seed = 3403,

    [string]$CycleApiScript
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'PhaseGate.ps1')

$script:SoakCycleSchemaVersion = 1
$script:SoakTimingBudgets = [ordered]@{
    launchMs          = 500
    firstOutputMs     = 500
    inputAckMs        = 250
    closeSettlementMs = 5000
    totalMs           = 30000
}
$script:SoakResourceBudgets = [ordered]@{
    privateBytes = 16MB
    handles      = 32
    listeners    = 0
    namedPipes   = 0
    ptyHandles   = 0
    jobHandles   = 0
}

function Write-SoakStatus {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Value
    )

    Write-Output ($Value | ConvertTo-Json -Depth 16 -Compress)
}

function ConvertTo-SoakSafeText {
    param(
        [AllowNull()]
        [object]$Value,
        [int]$MaximumLength = 512
    )

    if ($null -eq $Value) {
        return $null
    }

    $text = [string]$Value
    if ($text.Length -gt $MaximumLength) {
        $text = $text.Substring(0, $MaximumLength)
    }
    $text = [regex]::Replace(
        $text,
        '(?i)(password|secret|token|api[_-]?key|authorization)\s*[:=]\s*[^\s;]+',
        '$1=<redacted>'
    )
    $text = [regex]::Replace($text, '(?i)([A-Za-z]:\\|\\\\)[^\s;]+', '<path>')
    $text = [regex]::Replace($text, '[\x00-\x1f\x7f]', '_')
    return $text
}

function Add-SoakFailure {
    param(
        [AllowNull()]
        [string]$Current,
        [AllowNull()]
        [object]$Incoming
    )

    $currentText = ConvertTo-SoakSafeText -Value $Current
    $incomingText = ConvertTo-SoakSafeText -Value $Incoming
    if ([string]::IsNullOrWhiteSpace($incomingText)) {
        return $currentText
    }
    if ([string]::IsNullOrWhiteSpace($currentText)) {
        return $incomingText
    }

    $parts = @($currentText -split '\s+\|\s+')
    if ($parts -contains $incomingText) {
        return $currentText
    }
    return "$currentText | $incomingText"
}

function ConvertTo-SoakSafeToken {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value,
        [int]$MaximumLength = 160
    )

    $text = ConvertTo-SoakSafeText -Value $Value -MaximumLength $MaximumLength
    if ([string]::IsNullOrWhiteSpace($text)) {
        return '<empty>'
    }
    $text = $text -replace '[^A-Za-z0-9._:-]', '_'
    if ($text.Length -gt $MaximumLength) {
        $text = $text.Substring(0, $MaximumLength)
    }
    return $text
}

function Assert-SoakExactProperties {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Object,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [string[]]$Allowed
    )

    if ($null -eq $Object -or $Object -is [string] -or $Object -is [System.Array] -or $Object -is [hashtable]) {
        throw "Cycle evidence $Label must be an object."
    }

    $actual = @($Object.PSObject.Properties | ForEach-Object { [string]$_.Name })
    if ($actual.Count -ne $Allowed.Count) {
        throw "Cycle evidence $Label has an unexpected field set (expected $($Allowed -join ','))."
    }
    foreach ($field in $Allowed) {
        if (-not ($actual -ccontains $field)) {
            throw "Cycle evidence $Label is missing required field '$field'."
        }
    }
    $extra = @($actual | Where-Object { -not ($Allowed -ccontains $_) })
    if ($extra.Count -ne 0) {
        throw "Cycle evidence $Label contains rejected extra field(s): $($extra -join ', ')."
    }
}

function Assert-SoakString {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [int]$MaximumLength = 256,
        [string]$Pattern
    )

    if ($Value -isnot [string]) {
        throw "Cycle evidence $Label must be a string."
    }
    $text = [string]$Value
    if ([string]::IsNullOrWhiteSpace($text) -or $text.Length -gt $MaximumLength) {
        throw "Cycle evidence $Label is empty or exceeds $MaximumLength characters."
    }
    if ($text -match '[\x00-\x1f\x7f]') {
        throw "Cycle evidence $Label contains control characters."
    }
    if (-not [string]::IsNullOrWhiteSpace($Pattern) -and $text -notmatch $Pattern) {
        throw "Cycle evidence $Label has an unsafe value."
    }
    return $text
}

function Assert-SoakToken {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    return Assert-SoakString `
        -Value $Value `
        -Label $Label `
        -MaximumLength 160 `
        -Pattern '^[A-Za-z0-9][A-Za-z0-9._:-]*$'
}

function Assert-SoakInteger {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [int64]$Minimum = 0,
        [int64]$Maximum = [int64]::MaxValue
    )

    if (-not (Test-DevManagerIntegralNumber -Value $Value)) {
        throw "Cycle evidence $Label must be an integral number."
    }
    try {
        $number = [int64]$Value
    }
    catch {
        throw "Cycle evidence $Label is outside the supported integer range."
    }
    if ($number -lt $Minimum -or $number -gt $Maximum) {
        throw "Cycle evidence $Label is outside [$Minimum,$Maximum]."
    }
    return $number
}

function Assert-SoakTimestamp {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    $text = Assert-SoakString -Value $Value -Label $Label -MaximumLength 80
    try {
        $parsed = [DateTimeOffset]::Parse(
            $text,
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind
        )
    }
    catch {
        throw "Cycle evidence $Label is not an ISO-8601 timestamp."
    }
    if ($parsed.Offset -eq [TimeSpan]::Zero -and $text -notmatch '(?i)(Z|[+-]\d{2}:\d{2})$') {
        throw "Cycle evidence $Label must declare a UTC offset."
    }
    return $parsed.ToUniversalTime().ToString('o')
}

function Get-SoakArray {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    if ($null -eq $Value -or $Value -is [string] -or $Value -is [hashtable] -or $Value -is [System.Management.Automation.PSCustomObject]) {
        throw "Cycle evidence $Label must be an array, not a scalar."
    }
    if ($Value -is [System.Array] -or $Value -is [System.Collections.IList]) {
        return ,([object[]]$Value)
    }
    throw "Cycle evidence $Label must be an array."
}

function Assert-SoakIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Identity,
        [Parameter(Mandatory = $true)]
        [string]$Label
    )

    Assert-SoakExactProperties `
        -Object $Identity `
        -Label $Label `
        -Allowed @('processId', 'executablePath', 'creationDate')
    $processId = Assert-SoakInteger -Value $Identity.processId -Label "$Label.processId" -Minimum 1 -Maximum ([int64]([uint32]::MaxValue))
    $path = Assert-SoakString -Value $Identity.executablePath -Label "$Label.executablePath" -MaximumLength 1024
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $path)) {
        throw "Cycle evidence $Label.executablePath must be fully qualified."
    }
    $normalized = Normalize-DevManagerPath -LiteralPath $path
    $creationDate = Assert-SoakTimestamp -Value $Identity.creationDate -Label "$Label.creationDate"
    return [pscustomobject][ordered]@{
        processId      = [uint32]$processId
        executablePath = $normalized
        creationDate   = $creationDate
    }
}

function Get-SoakIdentityKey {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Identity
    )

    $normalized = Normalize-DevManagerPath -LiteralPath ([string]$Identity.executablePath)
    $start = ConvertTo-DevManagerProcessCreationUtc -CreationDate ([string]$Identity.creationDate)
    return "pid=$([uint32]$Identity.processId);exe=$normalized;start=$($start.ToString('o'))"
}

function Assert-SoakIdentityArray {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [System.Collections.Generic.HashSet[string]]$Seen
    )

    $items = Get-SoakArray -Value $Value -Label $Label
    $normalized = New-Object System.Collections.Generic.List[object]
    foreach ($item in $items) {
        $identity = Assert-SoakIdentity -Identity $item -Label "$Label.identity"
        $key = Get-SoakIdentityKey -Identity $identity
        if (-not $Seen.Add($key)) {
            throw "Cycle evidence contains duplicate exact process identity '$key'."
        }
        $null = $normalized.Add($identity)
    }
    return ,([object[]]$normalized.ToArray())
}

function Assert-SoakResourceArray {
    param(
        [AllowNull()]
        [object]$Value,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [System.Collections.Generic.HashSet[string]]$Seen
    )

    $items = Get-SoakArray -Value $Value -Label $Label
    $normalized = New-Object System.Collections.Generic.List[string]
    foreach ($item in $items) {
        $resource = Assert-SoakString -Value $item -Label "$Label.resource" -MaximumLength 512
        if ($resource -match '\.\.') {
            throw "Cycle evidence $Label contains an unsafe resource identity."
        }
        if (-not $Seen.Add($resource)) {
            throw "Cycle evidence contains duplicate resource identity '$resource'."
        }
        $null = $normalized.Add($resource)
    }
    return ,([string[]]$normalized.ToArray())
}

function Assert-SoakStage {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Stage,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [System.Collections.Generic.HashSet[string]]$OperationIds
    )

    $names = @($Stage.PSObject.Properties | ForEach-Object { [string]$_.Name })
    if ($names -contains 'operationId') {
        Assert-SoakExactProperties -Object $Stage -Label $Label -Allowed @('operationId')
        $operationId = Assert-SoakToken -Value $Stage.operationId -Label "$Label.operationId"
        if (-not $OperationIds.Add($operationId)) {
            throw "Cycle evidence contains duplicate operation ID '$operationId'."
        }
        return [pscustomobject][ordered]@{ operationId = $operationId }
    }
    if ($names -contains 'evidence') {
        Assert-SoakExactProperties -Object $Stage -Label $Label -Allowed @('evidence')
        Assert-SoakExactProperties `
            -Object $Stage.evidence `
            -Label "$Label.evidence" `
            -Allowed @('marker', 'observedAtUtc')
        $marker = Assert-SoakToken -Value $Stage.evidence.marker -Label "$Label.evidence.marker"
        $observedAtUtc = Assert-SoakTimestamp -Value $Stage.evidence.observedAtUtc -Label "$Label.evidence.observedAtUtc"
        $key = "evidence:${marker}:$observedAtUtc"
        if (-not $OperationIds.Add($key)) {
            throw "Cycle evidence contains duplicate stage evidence '$key'."
        }
        return [pscustomobject][ordered]@{
            evidence = [pscustomobject][ordered]@{
                marker        = $marker
                observedAtUtc = $observedAtUtc
            }
        }
    }
    throw "Cycle evidence $Label must contain exactly one operationId or evidence field."
}

function Assert-SoakResourceDelta {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Delta,
        [Parameter(Mandatory = $true)]
        [string]$Label,
        [Parameter(Mandatory = $true)]
        [int64]$Budget
    )

    Assert-SoakExactProperties `
        -Object $Delta `
        -Label $Label `
        -Allowed @('before', 'after', 'delta', 'budget')
    $before = Assert-SoakInteger -Value $Delta.before -Label "$Label.before" -Minimum 0 -Maximum (1TB)
    $after = Assert-SoakInteger -Value $Delta.after -Label "$Label.after" -Minimum 0 -Maximum (1TB)
    $deltaValue = Assert-SoakInteger -Value $Delta.delta -Label "$Label.delta" -Minimum (-1TB) -Maximum (1TB)
    $declaredBudget = Assert-SoakInteger -Value $Delta.budget -Label "$Label.budget" -Minimum 0 -Maximum (1TB)
    if ($declaredBudget -ne $Budget) {
        throw "Cycle evidence $Label.budget must be $Budget."
    }
    if ($after - $before -ne $deltaValue) {
        throw "Cycle evidence $Label is inconsistent: delta must equal after-before."
    }
    if ([Math]::Abs($deltaValue) -gt $Budget) {
        throw "Cycle evidence $Label exceeds its declared budget."
    }
    return [pscustomobject][ordered]@{
        before = $before
        after  = $after
        delta  = $deltaValue
        budget = $declaredBudget
    }
}

function ConvertTo-SoakSanitizedIdentity {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Identity
    )

    return [pscustomobject][ordered]@{
        processId = [uint32]$Identity.processId
        executable = [System.IO.Path]::GetFileName([string]$Identity.executablePath)
        startTimeUtc = ([DateTimeOffset]::Parse([string]$Identity.creationDate)).ToUniversalTime().ToString('o')
    }
}

function ConvertTo-SoakSanitizedIdentityArray {
    param(
        [object[]]$Identities
    )

    return @(
        foreach ($identity in @($Identities)) {
            ConvertTo-SoakSanitizedIdentity -Identity $identity
        }
    )
}

function Assert-SoakCompletedCycleResult {
    param(
        [Parameter(Mandatory = $true)]
        [object]$Result,
        [Parameter(Mandatory = $true)]
        [int]$ExpectedCycle,
        [Parameter(Mandatory = $true)]
        [int]$ExpectedSeed
    )

    Assert-SoakExactProperties `
        -Object $Result `
        -Label 'cycle result' `
        -Allowed @(
            'schemaVersion', 'status', 'cycle', 'seed', 'host', 'client', 'terminal',
            'operations', 'managedRoot', 'ownedProcessIdentities', 'resources', 'timing'
        )
    if ((Assert-SoakInteger -Value $Result.schemaVersion -Label 'schemaVersion' -Minimum 1 -Maximum 1) -ne $script:SoakCycleSchemaVersion) {
        throw "Cycle evidence schemaVersion is unsupported."
    }
    if ($Result.status -isnot [string] -or [string]$Result.status -cne 'completed') {
        throw "Cycle evidence status is not exactly completed."
    }
    if ((Assert-SoakInteger -Value $Result.cycle -Label 'cycle' -Minimum 1 -Maximum $Iterations) -ne $ExpectedCycle) {
        throw "Cycle evidence cycle does not match the requested cycle."
    }
    if ((Assert-SoakInteger -Value $Result.seed -Label 'seed' -Minimum 0 -Maximum ([int64]([int32]::MaxValue))) -ne $ExpectedSeed) {
        throw "Cycle evidence seed does not match the requested seed."
    }

    $hostAllowed = @('identity', 'generation')
    Assert-SoakExactProperties -Object $Result.host -Label 'host' -Allowed $hostAllowed
    Assert-SoakExactProperties -Object $Result.client -Label 'client' -Allowed $hostAllowed
    $hostIdentity = Assert-SoakIdentity -Identity $Result.host.identity -Label 'host.identity'
    $clientIdentity = Assert-SoakIdentity -Identity $Result.client.identity -Label 'client.identity'
    $hostGeneration = Assert-SoakToken -Value $Result.host.generation -Label 'host.generation'
    $clientGeneration = Assert-SoakToken -Value $Result.client.generation -Label 'client.generation'
    if ($hostGeneration -cne $clientGeneration) {
        throw 'Cycle evidence host/client generations are inconsistent.'
    }

    Assert-SoakExactProperties `
        -Object $Result.terminal `
        -Label 'terminal' `
        -Allowed @('terminalId', 'resourceId', 'generation')
    $terminalId = Assert-SoakToken -Value $Result.terminal.terminalId -Label 'terminal.terminalId'
    $terminalResourceId = Assert-SoakToken -Value $Result.terminal.resourceId -Label 'terminal.resourceId'
    $terminalGeneration = Assert-SoakToken -Value $Result.terminal.generation -Label 'terminal.generation'
    if ($terminalGeneration -cne $hostGeneration) {
        throw 'Cycle evidence terminal generation does not match host/client generation.'
    }

    Assert-SoakExactProperties `
        -Object $Result.operations `
        -Label 'operations' `
        -Allowed @('launch', 'firstOutput', 'inputAck', 'closeSettlement')
    $operationIds = New-Object System.Collections.Generic.HashSet[string]
    $launch = Assert-SoakStage -Stage $Result.operations.launch -Label 'operations.launch' -OperationIds $operationIds
    $firstOutput = Assert-SoakStage -Stage $Result.operations.firstOutput -Label 'operations.firstOutput' -OperationIds $operationIds
    $inputAck = Assert-SoakStage -Stage $Result.operations.inputAck -Label 'operations.inputAck' -OperationIds $operationIds
    $closeSettlement = Assert-SoakStage -Stage $Result.operations.closeSettlement -Label 'operations.closeSettlement' -OperationIds $operationIds

    Assert-SoakExactProperties `
        -Object $Result.managedRoot `
        -Label 'managedRoot' `
        -Allowed @('identity', 'job')
    $managedRootIdentity = Assert-SoakIdentity -Identity $Result.managedRoot.identity -Label 'managedRoot.identity'
    Assert-SoakExactProperties `
        -Object $Result.managedRoot.job `
        -Label 'managedRoot.job' `
        -Allowed @('handleId', 'memberCount', 'memberCountAuthoritative')
    $jobHandleId = Assert-SoakToken -Value $Result.managedRoot.job.handleId -Label 'managedRoot.job.handleId'
    $memberCount = Assert-SoakInteger -Value $Result.managedRoot.job.memberCount -Label 'managedRoot.job.memberCount' -Minimum 0 -Maximum (1TB)
    if ($memberCount -ne 0) {
        throw "Cycle evidence has residue: authoritative Job member count is $memberCount."
    }
    if ($Result.managedRoot.job.memberCountAuthoritative -isnot [bool] -or -not [bool]$Result.managedRoot.job.memberCountAuthoritative) {
        throw 'Cycle evidence Job member count is not authoritative.'
    }

    $identitySeen = New-Object System.Collections.Generic.HashSet[string]
    $hostKey = Get-SoakIdentityKey -Identity $hostIdentity
    $clientKey = Get-SoakIdentityKey -Identity $clientIdentity
    $rootKey = Get-SoakIdentityKey -Identity $managedRootIdentity
    foreach ($key in @($hostKey, $clientKey, $rootKey)) {
        if (-not $identitySeen.Add($key)) {
            throw "Cycle evidence contains duplicate host/client/root identity '$key'."
        }
    }
    Assert-SoakExactProperties `
        -Object $Result.ownedProcessIdentities `
        -Label 'ownedProcessIdentities' `
        -Allowed @('helper', 'provider', 'hostChildren')
    $helperIdentities = Assert-SoakIdentityArray `
        -Value $Result.ownedProcessIdentities.helper `
        -Label 'ownedProcessIdentities.helper' `
        -Seen $identitySeen
    $providerIdentities = Assert-SoakIdentityArray `
        -Value $Result.ownedProcessIdentities.provider `
        -Label 'ownedProcessIdentities.provider' `
        -Seen $identitySeen
    $hostChildIdentities = Assert-SoakIdentityArray `
        -Value $Result.ownedProcessIdentities.hostChildren `
        -Label 'ownedProcessIdentities.hostChildren' `
        -Seen $identitySeen

    Assert-SoakExactProperties `
        -Object $Result.resources `
        -Label 'resources' `
        -Allowed @('listeners', 'namedPipes', 'ptyHandles', 'jobHandles', 'delta')
    $resourceSeen = New-Object System.Collections.Generic.HashSet[string]
    $resourceBlocks = @{}
    foreach ($resourceName in @('listeners', 'namedPipes')) {
        Assert-SoakExactProperties `
            -Object $Result.resources.$resourceName `
            -Label "resources.$resourceName" `
            -Allowed @('observed', 'owned')
        $observed = Assert-SoakResourceArray `
            -Value $Result.resources.$resourceName.observed `
            -Label "resources.$resourceName.observed" `
            -Seen $resourceSeen
        $owned = Assert-SoakResourceArray `
            -Value $Result.resources.$resourceName.owned `
            -Label "resources.$resourceName.owned" `
            -Seen (New-Object System.Collections.Generic.HashSet[string])
        if ($owned.Count -ne 0) {
            throw "Cycle evidence has residue: resources.$resourceName.owned is not empty."
        }
        $resourceBlocks[$resourceName] = [pscustomobject][ordered]@{ observed = $observed; owned = $owned }
    }
    foreach ($resourceName in @('ptyHandles', 'jobHandles')) {
        Assert-SoakExactProperties `
            -Object $Result.resources.$resourceName `
            -Label "resources.$resourceName" `
            -Allowed @('observed', 'leaked')
        $observed = Assert-SoakResourceArray `
            -Value $Result.resources.$resourceName.observed `
            -Label "resources.$resourceName.observed" `
            -Seen $resourceSeen
        $leaked = Assert-SoakResourceArray `
            -Value $Result.resources.$resourceName.leaked `
            -Label "resources.$resourceName.leaked" `
            -Seen (New-Object System.Collections.Generic.HashSet[string])
        if ($observed.Count -eq 0) {
            throw "Cycle evidence resources.$resourceName.observed is empty."
        }
        if ($leaked.Count -ne 0) {
            throw "Cycle evidence has residue: resources.$resourceName.leaked is not empty."
        }
        $resourceBlocks[$resourceName] = [pscustomobject][ordered]@{ observed = $observed; leaked = $leaked }
    }
    if ($resourceBlocks.ptyHandles.observed -notcontains $terminalResourceId) {
        throw 'Cycle evidence terminal.resourceId is not present in PTY resource evidence.'
    }
    if ($resourceBlocks.jobHandles.observed -notcontains $jobHandleId) {
        throw 'Cycle evidence managedRoot.job.handleId is not present in Job resource evidence.'
    }

    Assert-SoakExactProperties `
        -Object $Result.resources.delta `
        -Label 'resources.delta' `
        -Allowed @('privateBytes', 'handles', 'listeners', 'namedPipes', 'ptyHandles', 'jobHandles')
    $resourceDeltas = [ordered]@{}
    foreach ($resourceName in $script:SoakResourceBudgets.Keys) {
        $resourceDeltas[$resourceName] = Assert-SoakResourceDelta `
            -Delta $Result.resources.delta.$resourceName `
            -Label "resources.delta.$resourceName" `
            -Budget ([int64]$script:SoakResourceBudgets[$resourceName])
    }

    Assert-SoakExactProperties `
        -Object $Result.timing `
        -Label 'timing' `
        -Allowed @('launchMs', 'firstOutputMs', 'inputAckMs', 'closeSettlementMs', 'totalMs')
    $timing = [ordered]@{}
    foreach ($timingName in $script:SoakTimingBudgets.Keys) {
        $timing[$timingName] = Assert-SoakInteger `
            -Value $Result.timing.$timingName `
            -Label "timing.$timingName" `
            -Minimum 0 `
            -Maximum ([int64]$script:SoakTimingBudgets[$timingName])
    }
    $maximumStageTime = @($timing.launchMs, $timing.firstOutputMs, $timing.inputAckMs, $timing.closeSettlementMs) | Measure-Object -Maximum | Select-Object -ExpandProperty Maximum
    if ($timing.totalMs -lt $maximumStageTime) {
        throw 'Cycle evidence timing.totalMs is shorter than a recorded lifecycle stage.'
    }

    return [pscustomobject][ordered]@{
        schemaVersion = [int]$script:SoakCycleSchemaVersion
        status = 'completed'
        cycle = [int]$ExpectedCycle
        seed = [int]$ExpectedSeed
        host = [pscustomobject][ordered]@{
            identity = ConvertTo-SoakSanitizedIdentity -Identity $hostIdentity
            generation = $hostGeneration
        }
        client = [pscustomobject][ordered]@{
            identity = ConvertTo-SoakSanitizedIdentity -Identity $clientIdentity
            generation = $clientGeneration
        }
        terminal = [pscustomobject][ordered]@{
            terminalId = $terminalId
            resourceId = ConvertTo-SoakSafeToken -Value $terminalResourceId
            generation = $terminalGeneration
        }
        operations = [pscustomobject][ordered]@{
            launch = $launch
            firstOutput = $firstOutput
            inputAck = $inputAck
            closeSettlement = $closeSettlement
        }
        managedRoot = [pscustomobject][ordered]@{
            identity = ConvertTo-SoakSanitizedIdentity -Identity $managedRootIdentity
            job = [pscustomobject][ordered]@{
                handleId = ConvertTo-SoakSafeToken -Value $jobHandleId
                memberCount = [int64]$memberCount
                memberCountAuthoritative = $true
            }
        }
        ownedProcessIdentities = [pscustomobject][ordered]@{
            helper = ConvertTo-SoakSanitizedIdentityArray -Identities $helperIdentities
            provider = ConvertTo-SoakSanitizedIdentityArray -Identities $providerIdentities
            hostChildren = ConvertTo-SoakSanitizedIdentityArray -Identities $hostChildIdentities
        }
        resources = [pscustomobject][ordered]@{
            listeners = [pscustomobject][ordered]@{
                observed = @($resourceBlocks.listeners.observed | ForEach-Object { ConvertTo-SoakSafeToken -Value $_ })
                owned = @()
            }
            namedPipes = [pscustomobject][ordered]@{
                observed = @($resourceBlocks.namedPipes.observed | ForEach-Object { ConvertTo-SoakSafeToken -Value $_ })
                owned = @()
            }
            ptyHandles = [pscustomobject][ordered]@{
                observed = @($resourceBlocks.ptyHandles.observed | ForEach-Object { ConvertTo-SoakSafeToken -Value $_ })
                leaked = @()
            }
            jobHandles = [pscustomobject][ordered]@{
                observed = @($resourceBlocks.jobHandles.observed | ForEach-Object { ConvertTo-SoakSafeToken -Value $_ })
                leaked = @()
            }
            delta = [pscustomobject][ordered]@{
                privateBytes = $resourceDeltas.privateBytes
                handles = $resourceDeltas.handles
                listeners = $resourceDeltas.listeners
                namedPipes = $resourceDeltas.namedPipes
                ptyHandles = $resourceDeltas.ptyHandles
                jobHandles = $resourceDeltas.jobHandles
            }
        }
        timing = [pscustomobject]$timing
    }
}

function Assert-SoakExactProcessIdentitySettled {
    param(
        [Parameter(Mandatory = $true)]
        [object]$RawResult,
        [Parameter(Mandatory = $true)]
        [int]$Cycle
    )

    $processes = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    foreach ($kind in @('helper', 'provider', 'hostChildren')) {
        $identities = Get-SoakArray -Value $RawResult.ownedProcessIdentities.$kind -Label "ownedProcessIdentities.$kind"
        foreach ($identity in $identities) {
            $expected = Assert-SoakIdentity -Identity $identity -Label "ownedProcessIdentities.$kind.identity"
            $rows = @($processes | Where-Object {
                    $null -ne $_.ProcessId -and [uint32]$_.ProcessId -eq [uint32]$expected.processId
                })
            if ($rows.Count -gt 1) {
                throw "Cycle $Cycle has ambiguous live process identity for PID $($expected.processId)."
            }
            if ($rows.Count -eq 0) {
                continue
            }
            $row = $rows[0]
            if ($null -eq $row.PSObject.Properties['ExecutablePath'] -or [string]::IsNullOrWhiteSpace([string]$row.ExecutablePath) -or
                $null -eq $row.PSObject.Properties['CreationDate'] -or [string]::IsNullOrWhiteSpace([string]$row.CreationDate)) {
                throw "Cycle $Cycle cannot prove exact process identity PID $($expected.processId) has settled."
            }
            try {
                $actualPath = Normalize-DevManagerPath -LiteralPath ([string]$row.ExecutablePath)
                $actualStart = ConvertTo-DevManagerProcessCreationUtc -CreationDate ([string]$row.CreationDate)
                $expectedStart = ConvertTo-DevManagerProcessCreationUtc -CreationDate ([string]$expected.creationDate)
            }
            catch {
                throw "Cycle $Cycle cannot prove exact process identity PID $($expected.processId) has settled."
            }
            if ($actualPath -eq [string]$expected.executablePath -and $actualStart -eq $expectedStart) {
                throw ("Cycle {0} exact {1} process identity remains alive: {2}" -f `
                        $Cycle, $kind, (ConvertTo-SoakSafeText -Value (Get-SoakIdentityKey -Identity $expected)))
            }
        }
    }
}

function New-SoakFailedCycleEvidence {
    param(
        [Parameter(Mandatory = $true)]
        [int]$Cycle,
        [Parameter(Mandatory = $true)]
        [int]$CycleSeed,
        [Parameter(Mandatory = $true)]
        [object]$Error
    )

    return [pscustomobject][ordered]@{
        schemaVersion = [int]$script:SoakCycleSchemaVersion
        status = 'failed'
        cycle = $Cycle
        seed = $CycleSeed
        error = ConvertTo-SoakSafeText -Value $Error
    }
}

function Resolve-SoakApiPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$WorktreeRoot
    )

    $candidate = $Path.Trim()
    if ([string]::IsNullOrWhiteSpace($candidate)) {
        throw 'CycleApiScript is empty.'
    }
    if (-not (Test-DevManagerAbsolutePath -LiteralPath $candidate)) {
        $candidate = Join-Path $WorktreeRoot $candidate
    }
    $resolved = [System.IO.Path]::GetFullPath($candidate)
    if (-not (Test-DevManagerPathEqualsOrBeneath -LiteralPath $resolved -AncestorPath $WorktreeRoot)) {
        throw "CycleApiScript escapes the worktree ('$resolved')."
    }
    Assert-DevManagerPathHasNoReparsePoints -LiteralPath $resolved
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "CycleApiScript does not exist ('$resolved')."
    }
    return $resolved
}

function New-SoakSummary {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Phase,
        [Parameter(Mandatory = $true)]
        [string]$RunId,
        [Parameter(Mandatory = $true)]
        [string]$RunDirectory,
        [Parameter(Mandatory = $true)]
        [string]$BaselinePath,
        [Parameter(Mandatory = $true)]
        [string]$SummaryPath,
        [Parameter(Mandatory = $true)]
        [string]$Status,
        [AllowNull()]
        [string]$Failure,
        [Parameter(Mandatory = $true)]
        [string]$ProductionAssert,
        [Parameter(Mandatory = $true)]
        [string]$BaselineLoadAssert,
        [Parameter(Mandatory = $true)]
        [int]$BaselineAssertions,
        [Parameter(Mandatory = $true)]
        [int]$CompletedCycles,
        [Parameter(Mandatory = $true)]
        [AllowEmptyCollection()]
        [object[]]$Cycles,
        [AllowNull()]
        [object]$BeforeInventory,
        [AllowNull()]
        [object]$AfterInventory
    )

    $beforeProcessCount = $null
    if ($null -ne $BeforeInventory) {
        $beforeProcessCount = [int64]@($BeforeInventory.processes).Count
    }
    $afterProcessCount = $null
    if ($null -ne $AfterInventory) {
        $afterProcessCount = [int64]@($AfterInventory.processes).Count
    }

    return [pscustomobject][ordered]@{
        schemaVersion       = 1
        cycleSchemaVersion   = [int]$script:SoakCycleSchemaVersion
        capturedAtUtc        = [DateTime]::UtcNow.ToString('o')
        status               = $Status
        phase                = $Phase
        runId                = $RunId
        runDirectory         = $RunDirectory
        iterations           = [int]$Iterations
        seed                 = [int]$Seed
        completedCycles      = [int]$CompletedCycles
        productionAssert     = $ProductionAssert
        baselineLoadAssert   = $BaselineLoadAssert
        baselineAssertions   = [int]$BaselineAssertions
        baselinePath         = $BaselinePath
        summaryPath          = $SummaryPath
        beforeProcessCount   = $beforeProcessCount
        afterProcessCount    = $afterProcessCount
        cycles               = [object[]]$Cycles
        failure              = $Failure
    }
}

$phase = 'phase-03-process-soak'

if (-not ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT)) {
    $unavailable = [pscustomobject][ordered]@{
        schemaVersion = 1
        cycleSchemaVersion = [int]$script:SoakCycleSchemaVersion
        status = 'unavailable'
        phase = $phase
        iterations = [int]$Iterations
        seed = [int]$Seed
        completedCycles = 0
        reason = 'Phase 3.10 process soak requires the Windows host/client surface.'
    }
    Write-Output 'UNAVAILABLE: Phase 3.10 process soak requires the Windows host/client surface.'
    Write-SoakStatus -Value $unavailable
    exit 78
}

$worktreeRoot = Get-DevManagerNativeNextWorktreeRoot -ScriptRoot $PSScriptRoot
$evidenceRoot = Get-DevManagerNativeNextEvidenceRoot -ScriptRoot $PSScriptRoot
$protectedRoot = Get-DevManagerProductionRoot
$runId = [guid]::NewGuid().ToString('N')
$runDirectory = [System.IO.Path]::GetFullPath((Join-Path $evidenceRoot "phase-03-process-soak\runs\$runId"))
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $runDirectory `
    -ProtectedProductionRoot $protectedRoot `
    -AllowedEvidenceRoot $evidenceRoot
New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
Assert-DevManagerPathHasNoReparsePoints -LiteralPath $runDirectory

$baselinePath = Join-Path $runDirectory 'baseline.json'
$summaryPath = Join-Path $runDirectory 'summary.json'
$captureScript = Join-Path $PSScriptRoot 'Capture-ProductionBaseline.ps1'
$assertScript = Join-Path $PSScriptRoot 'Assert-ProductionUnchanged.ps1'
$beforeInventory = $null
$afterInventory = $null
$cycleEvidence = New-Object System.Collections.Generic.List[object]
$rawCycleResults = New-Object System.Collections.Generic.List[object]
$failure = $null
$status = 'failed'
$productionAssert = 'not-run'
$baselineLoadAssert = 'not-run'
$baselineAssertions = 0
$completedCycles = 0
$baselineCaptured = $false

try {
    # This is intentionally before Resolve-SoakApiPath dot-sources any optional
    # extension. It captures config.json, remote.json, and installed PID/start
    # identities before extension code can change the protected surface.
    & $captureScript -OutputPath $baselinePath
    $baselineCaptured = $true
    $beforeInventory = Get-DevManagerProcessInventory -WorktreeRoot $worktreeRoot

    $apiPath = $null
    if (-not [string]::IsNullOrWhiteSpace($CycleApiScript)) {
        $apiPath = Resolve-SoakApiPath -Path $CycleApiScript -WorktreeRoot $worktreeRoot
        . $apiPath
        $null = & $assertScript -BaselinePath $baselinePath
        $baselineLoadAssert = 'unchanged'
        $baselineAssertions++
    }

    $cycleApi = Get-Command -Name 'Invoke-DevManagerProcessSoakCycle' -CommandType Function -ErrorAction SilentlyContinue
    if ($null -eq $cycleApi) {
        $status = 'unavailable'
        $failure = 'Invoke-DevManagerProcessSoakCycle is not present; no real host/client cycle was run.'
    }
    else {
        $random = [Random]::new($Seed)
        for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
            $cycleSeed = $random.Next()
            $scenario = [pscustomobject][ordered]@{
                iteration         = [int]$iteration
                seed              = [int]$cycleSeed
                disconnectDelayMs = [int]$random.Next(0, 250)
                closeDelayMs      = [int]$random.Next(0, 250)
                resizeRows        = [int]$random.Next(20, 60)
                resizeColumns     = [int]$random.Next(80, 180)
            }

            try {
                $null = & $assertScript -BaselinePath $baselinePath
                $baselineAssertions++
                $cycleResult = @(
                    & $cycleApi.Name `
                        -Iteration $iteration `
                        -Seed $cycleSeed `
                        -Scenario $scenario `
                        -WorktreeRoot $worktreeRoot
                )
                if ($cycleResult.Count -ne 1) {
                    throw "cycle returned $($cycleResult.Count) result objects; exactly one result is required."
                }
                $sanitized = Assert-SoakCompletedCycleResult `
                    -Result $cycleResult[0] `
                    -ExpectedCycle $iteration `
                    -ExpectedSeed $cycleSeed
                Assert-SoakExactProcessIdentitySettled -RawResult $cycleResult[0] -Cycle $iteration
                $null = & $assertScript -BaselinePath $baselinePath
                $baselineAssertions++
            $null = $cycleEvidence.Add($sanitized)
            $null = $rawCycleResults.Add($cycleResult[0])
                $completedCycles++
            }
            catch {
                $null = $cycleEvidence.Add((New-SoakFailedCycleEvidence -Cycle $iteration -CycleSeed $cycleSeed -Error $_.Exception.Message))
                throw "cycle $iteration failed: $($_.Exception.Message)"
            }
        }

        # A run is green only after all requested cycles have produced typed
        # evidence and the exact identity checks have run for every cycle.
        foreach ($rawCycle in $rawCycleResults) {
            Assert-SoakExactProcessIdentitySettled -RawResult $rawCycle -Cycle ([int]$rawCycle.cycle)
        }
        $status = 'passed'
    }
}
catch {
    $status = 'failed'
    $failure = Add-SoakFailure -Current $failure -Incoming $_.Exception.Message
}
finally {
    if ($baselineCaptured) {
        try {
            $null = & $assertScript -BaselinePath $baselinePath
            $productionAssert = 'unchanged'
            $baselineAssertions++
        }
        catch {
            $productionAssert = 'failed'
            $failure = Add-SoakFailure `
                -Current $failure `
                -Incoming ("production baseline integrity: {0}" -f $_.Exception.Message)
            $status = 'failed'
        }
    }

    if ($status -eq 'passed') {
        try {
            $afterInventory = Get-DevManagerProcessInventory -WorktreeRoot $worktreeRoot
        }
        catch {
            $status = 'failed'
            $failure = Add-SoakFailure `
                -Current $failure `
                -Incoming ("cleanup verification: {0}" -f $_.Exception.Message)
        }
    }

    if ($productionAssert -ne 'unchanged' -and $status -eq 'passed') {
        $status = 'failed'
        $failure = Add-SoakFailure -Current $failure -Incoming 'production baseline was not verified.'
    }

    $summary = New-SoakSummary `
        -Phase $phase `
        -RunId $runId `
        -RunDirectory $runDirectory `
        -BaselinePath $baselinePath `
        -SummaryPath $summaryPath `
        -Status $status `
        -Failure $failure `
        -ProductionAssert $productionAssert `
        -BaselineLoadAssert $baselineLoadAssert `
        -BaselineAssertions $baselineAssertions `
        -CompletedCycles $completedCycles `
        -Cycles ([object[]]$cycleEvidence.ToArray()) `
        -BeforeInventory $beforeInventory `
        -AfterInventory $afterInventory
    try {
        Write-DevManagerJsonEvidence `
            -Value $summary `
            -OutputPath $summaryPath `
            -ProtectedProductionRoot $protectedRoot `
            -AllowedEvidenceRoot $evidenceRoot
    }
    catch {
        $status = 'failed'
        $failure = Add-SoakFailure `
            -Current $failure `
            -Incoming ("summary evidence could not be persisted: {0}" -f $_.Exception.Message)
        $summary.failure = $failure
        $summary.status = $status
    }
    Write-SoakStatus -Value $summary
}

if ($status -eq 'unavailable') {
    Write-Error -Message ("Phase 3.10 process soak UNAVAILABLE: {0}" -f $failure) -ErrorAction Continue
    exit 78
}
if ($status -ne 'passed') {
    Write-Error -Message ("Phase 3.10 process soak failed closed: {0}" -f $failure) -ErrorAction Continue
    exit 1
}

exit 0
