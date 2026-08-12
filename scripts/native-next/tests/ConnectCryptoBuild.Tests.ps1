# Static and typed-HOLD checks for the explicit Connect Rust/WASM packaging step.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')

$scriptPath = Get-NativeNextScriptPath -Leaf 'Build-ConnectCrypto.ps1'
$scriptText = Get-Content -LiteralPath $scriptPath -Raw
$tokens = $null
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $scriptPath,
    [ref]$tokens,
    [ref]$parseErrors
)

Assert-Contract `
    -Name 'connect-crypto-build-script-parses' `
    -Condition ($null -ne $ast -and @($parseErrors).Count -eq 0) `
    -Message 'Build-ConnectCrypto.ps1 must remain valid PowerShell.'

Assert-Contract `
    -Name 'connect-crypto-build-is-pinned' `
    -Condition (
        $scriptText.Contains('$RustToolchain = "1.94.0"') -and
        $scriptText.Contains('$WasmTarget = "wasm32-unknown-unknown"') -and
        $scriptText.Contains('$WasmBindgenVersion = "0.2.114"') -and
        $scriptText.Contains('"--locked"') -and
        $scriptText.Contains('"--offline"')
    ) `
    -Message 'WASM packaging must use the pinned toolchain, target, CLI, lockfile, and offline Cargo mode.'

Assert-Contract `
    -Name 'connect-crypto-build-never-installs' `
    -Condition (
        $scriptText -notmatch '(?i)cargo\s+install' -and
        $scriptText -notmatch '(?i)rustup\s+(target\s+add|toolchain\s+install)'
    ) `
    -Message 'Packaging must not install toolchains or dependencies.'

$capture = Invoke-NativeNextScriptCapture -ScriptPath $scriptPath -Arguments @('-PlanOnly')
$exitCode = $capture.ExitCode
$jsonLine = @($capture.Stdout -split "`r?`n" | Where-Object { $_.TrimStart().StartsWith('{') } | Select-Object -Last 1)
$typed = $null
if ($jsonLine.Count -eq 1) {
    try { $typed = $jsonLine[0] | ConvertFrom-Json } catch { $typed = $null }
}

Assert-Contract `
    -Name 'connect-crypto-missing-prerequisites-are-typed-hold' `
    -Condition (
        $exitCode -eq 2 -and
        $null -ne $typed -and
        [string]$typed.status -eq 'HOLD' -and
        [string]$typed.disposition -eq 'HOLD' -and
        [bool]$typed.pass -eq $false
    ) `
    -Message "Plan-only or missing-prerequisite invocation must emit typed HOLD JSON; exit=$exitCode output=$($capture.Stdout)"

Complete-ContractTests -Suite 'ConnectCryptoBuild'
