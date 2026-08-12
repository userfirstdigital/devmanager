# Static and typed-HOLD checks for the explicit Connect Rust/WASM packaging step.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')

$scriptPath = Get-NativeNextScriptPath -Leaf 'Build-ConnectCrypto.ps1'
$scriptText = Get-Content -LiteralPath $scriptPath -Raw
$crateManifestPath = Join-Path $script:WorktreeRoot 'crates\connect-crypto\Cargo.toml'
$crateManifestText = Get-Content -LiteralPath $crateManifestPath -Raw
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

Assert-Contract `
    -Name 'connect-crypto-emits-wasm-cdylib' `
    -Condition ($crateManifestText -match '(?ms)^\[lib\]\s*crate-type\s*=\s*\[\s*"cdylib"\s*,\s*"rlib"\s*\]') `
    -Message 'The package must emit both the wasm-bindgen cdylib and the native/test rlib.'

Assert-Contract `
    -Name 'connect-crypto-wasm-enables-ring-getrandom-browser-backend-only' `
    -Condition (
        $crateManifestText -match '(?ms)^\[target\.\''cfg\(all\(target_arch = "wasm32", target_os = "unknown"\)\)\''\.dependencies\]\s*getrandom-02\s*=\s*\{\s*package\s*=\s*"getrandom"\s*,\s*version\s*=\s*"=0\.2\.17"\s*,\s*features\s*=\s*\["js"\]\s*\}'
    ) `
    -Message 'The ring/snow getrandom 0.2 browser backend must be an exact wasm32-unknown-unknown-only dependency edge.'

Assert-Contract `
    -Name 'connect-crypto-keeps-ring-native-and-wasm-pure-rust' `
    -Condition (
        $crateManifestText -match '(?ms)^\[target\.\''cfg\(not\(all\(target_arch = "wasm32", target_os = "unknown"\)\)\)\''\.dependencies\].*?snow\s*=.*?"ring-accelerated"' -and
        $crateManifestText -match '(?ms)^\[target\.\''cfg\(all\(target_arch = "wasm32", target_os = "unknown"\)\)\''\.dependencies\].*?snow\s*=.*?"use-getrandom"' -and
        $crateManifestText -notmatch '(?m)^snow\s*=.*"ring-accelerated"'
    ) `
    -Message 'Native must retain snow/ring acceleration while wasm32-unknown-unknown uses Snow pure-Rust primitives without a clang requirement.'

Assert-Contract `
    -Name 'connect-crypto-uuid-rng-is-target-specific' `
    -Condition (
        $crateManifestText -match '(?ms)^\[target\.\''cfg\(not\(all\(target_arch = "wasm32", target_os = "unknown"\)\)\)\''\.dependencies\].*?uuid\s*=.*?"v7"' -and
        $crateManifestText -match '(?ms)^\[target\.\''cfg\(all\(target_arch = "wasm32", target_os = "unknown"\)\)\''\.dependencies\].*?uuid\s*=.*?"js"'
    ) `
    -Message 'UUID v7 generation must retain the native backend and select its explicit browser backend only on wasm32-unknown-unknown.'

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
