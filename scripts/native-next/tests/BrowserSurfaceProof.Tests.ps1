# Portable browser surface proof must emit typed HOLD when visible WebView2
# was not performed. Stage=Red never launches cargo, WebView2, or providers.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')
. (Join-Path $PSScriptRoot '..\Isolation.ps1')

$scriptPath = Get-NativeNextScriptPath -Leaf 'Invoke-BrowserSurfaceProof.ps1'
$functionNames = @(Get-TopLevelScriptFunctions -LiteralPath $scriptPath | ForEach-Object { [string]$_.Name })
Assert-Contract `
    -Name 'surface-exposes-typed-result-helper' `
    -Condition ($functionNames -contains 'Resolve-BrowserSurfaceProofTypedResult') `
    -Message 'Resolve-BrowserSurfaceProofTypedResult is missing from Invoke-BrowserSurfaceProof.ps1.'

if ($functionNames -contains 'Resolve-BrowserSurfaceProofTypedResult') {
    . ([scriptblock]::Create((Get-NamedFunctionSource -LiteralPath $scriptPath -Names @('Resolve-BrowserSurfaceProofTypedResult'))))
    $notProven = Resolve-BrowserSurfaceProofTypedResult -Evidence ([pscustomobject]@{
            schemaVersion         = 1
            visibleWebView2Proven = $false
            residue               = [pscustomobject]@{ launchedWebView2 = $false }
        })
    Assert-Contract `
        -Name 'surface-helper-hold-when-visible-not-proven' `
        -Condition (
            ([string]$notProven.status -eq 'HOLD') -and
            ([string]$notProven.disposition -eq 'HOLD') -and
            ([bool]$notProven.pass -eq $false)
        ) `
        -Message "expected HOLD, got status='$($notProven.status)' pass='$($notProven.pass)'"
}
else {
    Assert-Contract `
        -Name 'surface-helper-hold-when-visible-not-proven' `
        -Condition $false `
        -Message 'Resolve-BrowserSurfaceProofTypedResult is not defined.'
}

$tempRoot = New-ContractTestTempRoot
try {
    $outputDir = Join-Path $tempRoot 'surface-red'
    $capture = Invoke-NativeNextScriptCapture `
        -ScriptPath $scriptPath `
        -Arguments @('-Stage', 'Red', '-OutputDir', $outputDir)
    $typed = Get-LastTypedJsonObject -Text $capture.Stdout
    $evidencePath = Join-Path $outputDir 'browser-surface-proof.json'
    $fileEvidence = $null
    if (Test-Path -LiteralPath $evidencePath -PathType Leaf) {
        $fileEvidence = Get-Content -LiteralPath $evidencePath -Raw | ConvertFrom-Json
    }

    Assert-Contract `
        -Name 'surface-red-emits-typed-json' `
        -Condition (
            ($null -ne $typed) -and
            ($null -ne $typed.PSObject.Properties['schemaVersion']) -and
            ($null -ne $typed.PSObject.Properties['status']) -and
            ($null -ne $typed.PSObject.Properties['disposition']) -and
            ($null -ne $typed.PSObject.Properties['pass']) -and
            ($null -ne $typed.PSObject.Properties['reason'])
        ) `
        -Message "stdout did not end with typed JSON. exit=$($capture.ExitCode) stdout='$($capture.Stdout)' stderr='$($capture.Stderr)'"

    $status = if ($null -eq $typed) { '' } else { [string]$typed.status }
    $disposition = if ($null -eq $typed) { '' } else { [string]$typed.disposition }
    $passFlag = if ($null -eq $typed -or $null -eq $typed.PSObject.Properties['pass']) { $true } else { [bool]$typed.pass }
    $visible = $false
    if ($null -ne $typed -and $null -ne $typed.PSObject.Properties['visibleWebView2Proven']) {
        $visible = [bool]$typed.visibleWebView2Proven
    }
    elseif ($null -ne $fileEvidence -and $null -ne $fileEvidence.PSObject.Properties['visibleWebView2Proven']) {
        $visible = [bool]$fileEvidence.visibleWebView2Proven
    }

    Assert-Contract `
        -Name 'surface-red-visible-unproven-is-typed-hold' `
        -Condition (
            ($visible -eq $false) -and
            ($status -eq 'HOLD') -and
            ($disposition -eq 'HOLD') -and
            ($passFlag -eq $false) -and
            ($status -ne 'PASS')
        ) `
        -Message "expected typed HOLD with visibleWebView2Proven=false, got status='$status' pass='$passFlag' visible='$visible' exit=$($capture.ExitCode)"

    Assert-Contract `
        -Name 'surface-red-file-evidence-matches-typed-hold' `
        -Condition (
            ($null -ne $fileEvidence) -and
            ([string]$fileEvidence.status -eq 'HOLD') -and
            ([bool]$fileEvidence.pass -eq $false) -and
            ([bool]$fileEvidence.visibleWebView2Proven -eq $false)
        ) `
        -Message 'evidence file must carry schema status/pass HOLD fields without claiming visible proof.'
}
finally {
    Remove-ContractTestTempRoot -LiteralPath $tempRoot
}

Complete-ContractTests -Suite 'BrowserSurfaceProof'
