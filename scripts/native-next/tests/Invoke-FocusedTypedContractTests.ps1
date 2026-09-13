# Focused runner for typed-contract PowerShell tests. No cargo/npm/providers.

[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$pwsh = [System.IO.Path]::GetFullPath((Join-Path $PSHome 'pwsh.exe'))
$suites = @(
    'FinalReleaseTypedContract.Tests.ps1',
    'BrowserSurfaceProof.Tests.ps1',
    'BrowserProviderE2E.Tests.ps1',
    'NativeNextParseFile.Tests.ps1'
)

$failed = New-Object System.Collections.Generic.List[string]
foreach ($suite in $suites) {
    $path = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot $suite))
    Write-Host ('========== {0} ==========' -f $suite)
    & $pwsh -NoProfile -NonInteractive -File $path
    if ($LASTEXITCODE -ne 0) {
        $failed.Add(('{0} exit {1}' -f $suite, $LASTEXITCODE))
    }
}

if (@($failed).Count -gt 0) {
    Write-Host 'Focused typed-contract tests FAILED:'
    foreach ($item in @($failed)) { Write-Host ("  - {0}" -f $item) }
    exit 1
}

Write-Host 'Focused typed-contract tests PASSED.'
exit 0
