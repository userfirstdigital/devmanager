# Parser::ParseFile checks for allowlisted release-gate scripts and focused tests.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')

$paths = @(
    (Get-NativeNextScriptPath -Leaf 'Invoke-FinalReleaseGate.ps1'),
    (Get-NativeNextScriptPath -Leaf 'Invoke-BrowserSurfaceProof.ps1'),
    (Get-NativeNextScriptPath -Leaf 'Invoke-BrowserProviderE2E.ps1')
) + @(
    Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.ps1' -File |
        ForEach-Object { [string]$_.FullName }
)

$seen = New-Object 'System.Collections.Generic.HashSet[string]'
foreach ($path in @($paths)) {
    $full = [System.IO.Path]::GetFullPath($path)
    if (-not $seen.Add($full)) { continue }
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($full, [ref]$tokens, [ref]$parseErrors)
    $ok = ($null -ne $ast -and @($parseErrors).Count -eq 0)
    $detail = 'ok'
    if (-not $ok) {
        $detail = (($parseErrors | ForEach-Object { $_.ToString() }) -join '; ')
    }
    Assert-Contract `
        -Name ("parsefile:{0}" -f [System.IO.Path]::GetFileName($full)) `
        -Condition $ok `
        -Message $detail
}

$gateText = Get-Content -LiteralPath (Get-NativeNextScriptPath -Leaf 'Invoke-FinalReleaseGate.ps1') -Raw
Assert-Contract `
    -Name 'gate-uses-command-outcome-helper' `
    -Condition ($gateText -match 'Resolve-FinalReleaseCommandOutcome') `
    -Message 'Invoke-FinalReleaseGate.ps1 must classify child commands through Resolve-FinalReleaseCommandOutcome.'

$surfaceText = Get-Content -LiteralPath (Get-NativeNextScriptPath -Leaf 'Invoke-BrowserSurfaceProof.ps1') -Raw
Assert-Contract `
    -Name 'surface-script-forbids-provider-and-install-launch' `
    -Condition (
        ($surfaceText -notmatch '(?i)Start-Process\s') -and
        ($surfaceText -notmatch '(?i)&\s*[''\"]?\S*(claude|codex|cursor|devmanager)\.exe') -and
        ($surfaceText -match "(?m)^\s*'claude\.exe'\s*,?\s*$")
    ) `
    -Message 'surface proof must keep the forbidden-launch token list and must not invoke those executables.'

$e2eText = Get-Content -LiteralPath (Get-NativeNextScriptPath -Leaf 'Invoke-BrowserProviderE2E.ps1') -Raw
Assert-Contract `
    -Name 'e2e-script-never-launches-stock-providers' `
    -Condition (
        ($e2eText -notmatch 'claude\.exe') -and
        ($e2eText -notmatch 'codex\.exe') -and
        ($e2eText -notmatch 'cursor\.exe') -and
        ($e2eText -notmatch 'Invoke-ProcessSoak')
    ) `
    -Message 'provider E2E must stay fixture-only and must not launch stock providers or soak.'

Complete-ContractTests -Suite 'NativeNextParseFile'
