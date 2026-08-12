# Browser provider E2E must emit typed JSON, HOLD when the fixture server is
# unavailable, FAIL malformed/failed checks, and keep authenticated launch HOLD.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')
. (Join-Path $PSScriptRoot '..\Isolation.ps1')

$scriptPath = Get-NativeNextScriptPath -Leaf 'Invoke-BrowserProviderE2E.ps1'
$functionNames = @(Get-TopLevelScriptFunctions -LiteralPath $scriptPath | ForEach-Object { [string]$_.Name })
Assert-Contract `
    -Name 'e2e-exposes-typed-result-helper' `
    -Condition ($functionNames -contains 'Resolve-BrowserProviderE2ETypedResult') `
    -Message 'Resolve-BrowserProviderE2ETypedResult is missing from Invoke-BrowserProviderE2E.ps1.'

if ($functionNames -contains 'Resolve-BrowserProviderE2ETypedResult') {
    . ([scriptblock]::Create((Get-NamedFunctionSource -LiteralPath $scriptPath -Names @('Resolve-BrowserProviderE2ETypedResult'))))

    $missing = Resolve-BrowserProviderE2ETypedResult -FixtureServerAvailable $false
    Assert-Contract `
        -Name 'e2e-helper-missing-server-is-hold' `
        -Condition (([string]$missing.status -eq 'HOLD') -and ([bool]$missing.pass -eq $false)) `
        -Message "expected HOLD for missing server, got '$($missing.status)'"

    $readyFail = Resolve-BrowserProviderE2ETypedResult -FixtureServerAvailable $true -ReadyLineEmitted $false
    Assert-Contract `
        -Name 'e2e-helper-missing-ready-line-is-fail' `
        -Condition ([string]$readyFail.status -eq 'FAIL') `
        -Message "expected FAIL for missing ready line, got '$($readyFail.status)'"

    $healthFail = Resolve-BrowserProviderE2ETypedResult -FixtureServerAvailable $true -ReadyLineEmitted $true -HealthOk $false -IndexOk $true -TraversalOk $true
    Assert-Contract `
        -Name 'e2e-helper-failed-health-is-fail' `
        -Condition ([string]$healthFail.status -eq 'FAIL') `
        -Message "expected FAIL for health check, got '$($healthFail.status)'"

    $cleanupFail = Resolve-BrowserProviderE2ETypedResult -FixtureServerAvailable $true -ReadyLineEmitted $true -HealthOk $true -IndexOk $true -TraversalOk $true -CleanupLeftResidue $true
    Assert-Contract `
        -Name 'e2e-helper-cleanup-residue-is-fail' `
        -Condition ([string]$cleanupFail.status -eq 'FAIL') `
        -Message "expected FAIL for leftover fixture process, got '$($cleanupFail.status)'"

    $authHold = Resolve-BrowserProviderE2ETypedResult -Authenticated $true
    Assert-Contract `
        -Name 'e2e-helper-authenticated-is-hold' `
        -Condition (([string]$authHold.status -eq 'HOLD') -and ([bool]$authHold.pass -eq $false)) `
        -Message "expected HOLD for authenticated launch, got '$($authHold.status)'"
}
else {
    foreach ($name in @(
            'e2e-helper-missing-server-is-hold',
            'e2e-helper-missing-ready-line-is-fail',
            'e2e-helper-failed-health-is-fail',
            'e2e-helper-cleanup-residue-is-fail',
            'e2e-helper-authenticated-is-hold'
        )) {
        Assert-Contract -Name $name -Condition $false -Message 'Resolve-BrowserProviderE2ETypedResult is not defined.'
    }
}

$tempRoot = New-ContractTestTempRoot
$savedAuth = [Environment]::GetEnvironmentVariable('DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E', 'Process')
$savedTarget = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
try {
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', (Join-Path $tempRoot 'empty-target'), 'Process')
    [Environment]::SetEnvironmentVariable('DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E', $null, 'Process')

    $fixtureOut = Join-Path $tempRoot 'e2e-missing-server'
    $capture = Invoke-NativeNextScriptCapture `
        -ScriptPath $scriptPath `
        -Arguments @('-Fixture', '-OutputDir', $fixtureOut) `
        -Environment @{
            CARGO_TARGET_DIR = (Join-Path $tempRoot 'empty-target')
        }
    $typed = Get-LastTypedJsonObject -Text $capture.Stdout
    $evidencePath = Join-Path $fixtureOut 'browser-provider-e2e.json'
    $fileEvidence = $null
    if (Test-Path -LiteralPath $evidencePath -PathType Leaf) {
        $fileEvidence = Get-Content -LiteralPath $evidencePath -Raw | ConvertFrom-Json
    }

    Assert-Contract `
        -Name 'e2e-missing-server-emits-typed-json' `
        -Condition (
            ($null -ne $typed) -and
            ($null -ne $typed.PSObject.Properties['schemaVersion']) -and
            ($null -ne $typed.PSObject.Properties['status']) -and
            ($null -ne $typed.PSObject.Properties['disposition']) -and
            ($null -ne $typed.PSObject.Properties['pass']) -and
            ($null -ne $typed.PSObject.Properties['reason'])
        ) `
        -Message "stdout did not end with typed JSON. stdout='$($capture.Stdout)' exit=$($capture.ExitCode)"

    $status = if ($null -eq $typed) { '' } else { [string]$typed.status }
    $passFlag = if ($null -eq $typed -or $null -eq $typed.PSObject.Properties['pass']) { $true } else { [bool]$typed.pass }
    Assert-Contract `
        -Name 'e2e-missing-server-is-typed-hold' `
        -Condition (($status -eq 'HOLD') -and ($passFlag -eq $false) -and ($status -ne 'PASS')) `
        -Message "expected typed HOLD for missing fixture server, got status='$status' pass='$passFlag' exit=$($capture.ExitCode)"

    $fileStatus = ''
    $filePass = $true
    $fileLaunched = $true
    if ($null -ne $fileEvidence) {
        if ($null -ne $fileEvidence.PSObject.Properties['status']) { $fileStatus = [string]$fileEvidence.status }
        if ($null -ne $fileEvidence.PSObject.Properties['pass']) { $filePass = [bool]$fileEvidence.pass }
        if ($null -ne $fileEvidence.PSObject.Properties['launchedStockProvider']) { $fileLaunched = [bool]$fileEvidence.launchedStockProvider }
    }
    Assert-Contract `
        -Name 'e2e-missing-server-file-evidence-is-hold' `
        -Condition (
            ($null -ne $fileEvidence) -and
            ($fileStatus -eq 'HOLD') -and
            ($filePass -eq $false) -and
            ($fileLaunched -eq $false)
        ) `
        -Message 'evidence file must be typed HOLD and must not claim a stock provider launch.'

    $configBase = Join-Path $tempRoot 'isolated-config'
    New-Item -ItemType Directory -Force -Path $configBase | Out-Null
    $authOut = Join-Path $tempRoot 'e2e-auth'
    $authCapture = Invoke-NativeNextScriptCapture `
        -ScriptPath $scriptPath `
        -Arguments @('-Authenticated', '-Provider', 'claude', '-ConfigBase', $configBase, '-OutputDir', $authOut) `
        -Environment @{
            DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E = '1'
            CARGO_TARGET_DIR                           = (Join-Path $tempRoot 'empty-target')
        }
    $authTyped = Get-LastTypedJsonObject -Text $authCapture.Stdout
    $authStatus = if ($null -eq $authTyped) { '' } else { [string]$authTyped.status }
    $authLaunched = $false
    if ($null -ne $authTyped -and $null -ne $authTyped.PSObject.Properties['authenticated']) {
        $authLaunched = [bool]$authTyped.authenticated.launched
    }
    Assert-Contract `
        -Name 'e2e-authenticated-launch-is-typed-hold' `
        -Condition (
            ($null -ne $authTyped) -and
            ($authStatus -eq 'HOLD') -and
            ([bool]$authTyped.pass -eq $false) -and
            ($authLaunched -eq $false)
        ) `
        -Message "authenticated arm must emit typed HOLD without launching. status='$authStatus' stdout='$($authCapture.Stdout)'"
}
finally {
    [Environment]::SetEnvironmentVariable('DEVMANAGER_ALLOW_AUTHENTICATED_BROWSER_E2E', $savedAuth, 'Process')
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $savedTarget, 'Process')
    Remove-ContractTestTempRoot -LiteralPath $tempRoot
}

Complete-ContractTests -Suite 'BrowserProviderE2E'
