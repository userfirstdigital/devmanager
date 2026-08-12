[CmdletBinding()]
param(
    [string] $OutDir = (Join-Path $PSScriptRoot "../../web/src/connect/wasm")
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
$wasmTarget = Join-Path $repoRoot "target/wasm32-unknown-unknown/release/connect_crypto.wasm"
$wasmBindgen = Get-Command wasm-bindgen -ErrorAction SilentlyContinue
if (-not $wasmBindgen) {
    throw "wasm-bindgen CLI is required to build the Connect Rust/WASM leaf"
}

Push-Location $repoRoot
try {
    cargo build --locked --package connect-crypto --target wasm32-unknown-unknown --release --features wasm
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    & $wasmBindgen.Source $wasmTarget --target web --out-dir $OutDir --out-name connect_crypto
    if ($LASTEXITCODE -ne 0) {
        throw "wasm-bindgen failed for connect-crypto"
    }
}
finally {
    Pop-Location
}
