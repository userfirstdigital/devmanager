<#[
.SYNOPSIS
    Build and publish the reviewed Rust/WASM Connect crypto leaf.

.DESCRIPTION
    This is an explicit release/build step.  It never installs a Rust target,
    cargo tool, npm package, or other dependency.  Missing or mismatched
    prerequisites produce a typed HOLD (exit code 2), so a browser/client
    release cannot accidentally claim that Connect E2E crypto is available.

    The generated files are first written to a process-unique staging
    directory under target/, then copied as a fixed allowlist into the
    ignored source artifact directory.  Vite copies exactly those files into
    web/bundle/assets/wasm during the subsequent production web build.
#>

[CmdletBinding()]
param(
    [string] $OutDir = (Join-Path $PSScriptRoot "../../web/src/connect/wasm"),
    [string] $TargetDir = $env:CARGO_TARGET_DIR,
    [switch] $PlanOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$SchemaVersion = 1
$ArtifactName = "connect-crypto"
$ProtocolMajor = 1
$RustToolchain = "1.94.0"
$WasmTarget = "wasm32-unknown-unknown"
$WasmBindgenVersion = "0.2.114"
$PackageName = "connect-crypto"
$ModulePath = "./wasm/connect_crypto.js"
$RequiredFiles = @(
    "connect_crypto.js",
    "connect_crypto_bg.wasm"
)
$OptionalFiles = @(
    "connect_crypto.d.ts",
    "connect_crypto_bg.wasm.d.ts"
)
$ManifestName = "connect_crypto.manifest.json"
$AllowedOutputFiles = @($RequiredFiles + $OptionalFiles + $ManifestName)

function New-ConnectCryptoResult {
    param(
        [Parameter(Mandatory = $true)][ValidateSet("PASS", "HOLD", "FAIL")][string]$Status,
        [Parameter(Mandatory = $true)][string]$Reason,
        [hashtable]$Checks = @{},
        [hashtable]$Artifact = $null,
        [string]$Message = ""
    )

    return [ordered]@{
        schemaVersion = $SchemaVersion
        kind         = "connect-crypto-wasm-build"
        status       = $Status
        disposition  = $Status
        pass         = ($Status -eq "PASS")
        reason       = $Reason
        message      = $Message
        target       = $WasmTarget
        toolchain    = $RustToolchain
        checks       = $Checks
        artifact     = $Artifact
    }
}

function Complete-ConnectCrypto {
    param(
        [Parameter(Mandatory = $true)][hashtable]$Result,
        [Parameter(Mandatory = $true)][int]$ExitCode
    )

    # The final line is deliberately compact JSON so release gates can parse
    # it even when Cargo/wasm-bindgen have emitted ordinary build diagnostics.
    Write-Output ($Result | ConvertTo-Json -Depth 12 -Compress)
    exit $ExitCode
}

function Hold-ConnectCrypto {
    param(
        [Parameter(Mandatory = $true)][string]$Reason,
        [Parameter(Mandatory = $true)][string]$Message,
        [hashtable]$Checks = @{}
    )

    Complete-ConnectCrypto `
        -Result (New-ConnectCryptoResult -Status "HOLD" -Reason $Reason -Message $Message -Checks $Checks) `
        -ExitCode 2
}

function Fail-ConnectCrypto {
    param(
        [Parameter(Mandatory = $true)][string]$Reason,
        [Parameter(Mandatory = $true)][string]$Message,
        [hashtable]$Checks = @{}
    )

    Complete-ConnectCrypto `
        -Result (New-ConnectCryptoResult -Status "FAIL" -Reason $Reason -Message $Message -Checks $Checks) `
        -ExitCode 1
}

function Resolve-RepositoryPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    if ([System.IO.Path]::IsPathRooted($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }
    return [System.IO.Path]::GetFullPath((Join-Path $repoRoot $Path))
}

function Test-PathInsideRepository {
    param([Parameter(Mandatory = $true)][string]$Path)

    $rootPrefix = $repoRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
    return $Path.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)
}

function Invoke-Captured {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments
    )

    $output = @(& $FilePath @Arguments 2>&1 | ForEach-Object { [string]$_ })
    return [pscustomobject]@{
        exitCode = [int]$LASTEXITCODE
        output   = ($output -join "`n").Trim()
    }
}

function Get-Sha256 {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    return (Get-FileHash -Algorithm SHA256 -LiteralPath $LiteralPath).Hash.ToLowerInvariant()
}

function Get-ConnectCryptoSourceFingerprint {
    param([Parameter(Mandatory = $true)][string]$RepositoryRoot)

    $sourceManifest = Join-Path $RepositoryRoot "crates/connect-crypto/artifact-sources.txt"
    if (-not (Test-Path -LiteralPath $sourceManifest -PathType Leaf)) {
        throw "Connect crypto source manifest is missing: $sourceManifest"
    }

    $files = [System.Collections.Generic.List[string]]::new()
    foreach ($line in (Get-Content -LiteralPath $sourceManifest)) {
        $source = $line.Trim()
        if ([string]::IsNullOrWhiteSpace($source) -or $source.StartsWith('#')) {
            continue
        }
        $absolute = [System.IO.Path]::GetFullPath((Join-Path $RepositoryRoot $source))
        if (Test-Path -LiteralPath $absolute -PathType Container) {
            Get-ChildItem -LiteralPath $absolute -File -Recurse | ForEach-Object { $files.Add($_.FullName) }
        }
        elseif (Test-Path -LiteralPath $absolute -PathType Leaf) {
            $files.Add($absolute)
        }
        else {
            throw "Connect crypto source path is missing: $source"
        }
    }

    [System.Numerics.BigInteger]$hash = [System.Numerics.BigInteger]::Parse("14695981039346656037")
    [System.Numerics.BigInteger]$prime = [System.Numerics.BigInteger]::Parse("1099511628211")
    [System.Numerics.BigInteger]$mask = [System.Numerics.BigInteger]::Parse("18446744073709551615")
    $update = {
        param([byte[]]$Bytes)
        for ($index = 0; $index -lt $Bytes.Length; $index++) {
            [byte]$value = $Bytes[$index]
            if ($value -eq 13 -and $index + 1 -lt $Bytes.Length -and $Bytes[$index + 1] -eq 10) {
                $value = 10
                $index++
            }
            $script:connectCryptoHash = (($script:connectCryptoHash -bxor [System.Numerics.BigInteger]$value) * $prime) -band $mask
        }
    }
    $script:connectCryptoHash = $hash
    try {
        foreach ($file in @($files | Sort-Object)) {
            $relative = [System.IO.Path]::GetRelativePath($RepositoryRoot, $file).Replace('\', '/')
            & $update ([System.Text.Encoding]::UTF8.GetBytes($relative))
            & $update ([byte[]]@(0))
            & $update ([System.IO.File]::ReadAllBytes($file))
            & $update ([byte[]]@(0))
        }
        # BigInteger's hexadecimal formatter may prepend a sign-preserving zero
        # when bit 63 is set (17 characters for an unsigned u64). Rust hashes
        # the same state as u64, so publish exactly the low 16 hex digits.
        $hex = $script:connectCryptoHash.ToString("x").ToLowerInvariant()
        if ($hex.Length -gt 16) {
            $hex = $hex.Substring($hex.Length - 16)
        }
        return $hex.PadLeft(16, '0')
    }
    finally {
        Remove-Variable -Scope Script -Name connectCryptoHash -ErrorAction SilentlyContinue
    }
}

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string]$Contents
    )

    [System.IO.File]::WriteAllText(
        $LiteralPath,
        $Contents,
        [System.Text.UTF8Encoding]::new($false)
    )
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$crateManifestPath = Join-Path $repoRoot "crates/connect-crypto/Cargo.toml"
$sourceFingerprint = Get-ConnectCryptoSourceFingerprint -RepositoryRoot $repoRoot
$resolvedOutDir = Resolve-RepositoryPath $OutDir
$targetRoot = if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    Join-Path $repoRoot "target"
} else {
    Resolve-RepositoryPath $TargetDir
}
$stageDir = Join-Path $targetRoot ("connect-crypto-wasm-bindgen-{0}" -f $PID)
$wasmPath = Join-Path $targetRoot ("{0}/release/connect_crypto.wasm" -f $WasmTarget)
$ownsStage = $false

try {
    # Honor the caller's isolated Cargo lane; never silently reuse the daily
    # checkout's target when a worker has supplied CARGO_TARGET_DIR/-TargetDir.
    $targetInsideRepository = (Test-PathInsideRepository $targetRoot) -and $targetRoot -ne $repoRoot
    $targetIsIsolatedTemp = $targetRoot -match '^C:\\Temp\\devmanager-[^\\/]+$'
    if (-not ($targetInsideRepository -or $targetIsIsolatedTemp)) {
        Hold-ConnectCrypto `
            -Reason "unsafe-target-directory" `
            -Message "Cargo target must be a checkout subdirectory or an exact C:\Temp\devmanager-* root." `
            -Checks @{ targetDirectory = $targetRoot }
    }

    if (-not (Test-PathInsideRepository $resolvedOutDir) -or $resolvedOutDir -eq $repoRoot) {
        Hold-ConnectCrypto `
            -Reason "unsafe-output-directory" `
            -Message "The artifact output directory must remain inside this checkout." `
            -Checks @{ outputDirectory = $resolvedOutDir }
    }

    if (-not (Test-Path -LiteralPath $crateManifestPath -PathType Leaf)) {
        Hold-ConnectCrypto `
            -Reason "missing-connect-crypto-manifest" `
            -Message "crates/connect-crypto/Cargo.toml is missing." `
            -Checks @{ crateManifest = $false }
    }

    $crateManifest = Get-Content -LiteralPath $crateManifestPath -Raw
    $manifestChecks = [ordered]@{
        crateManifest             = $true
        packageName               = ($crateManifest -match '(?m)^name\s*=\s*"connect-crypto"\s*$')
        rustVersion               = ($crateManifest -match '(?m)^rust-version\s*=\s*"1\.94"\s*$')
        wasmBindgenDependency     = ($crateManifest -match '(?m)^wasm-bindgen\s*=\s*\{[^\r\n]*version\s*=\s*"=0\.2\.114"')
        metadataToolchain         = ($crateManifest -match '(?m)^toolchain\s*=\s*"1\.94\.0"\s*$')
        metadataTarget            = ($crateManifest -match '(?m)^target\s*=\s*"wasm32-unknown-unknown"\s*$')
        metadataWasmBindgen       = ($crateManifest -match '(?m)^wasm-bindgen-cli\s*=\s*"0\.2\.114"\s*$')
        metadataModulePath        = ($crateManifest -match '(?m)^module-path\s*=\s*"\./wasm/connect_crypto\.js"\s*$')
        metadataProtocolMajor     = ($crateManifest -match '(?m)^protocol-major\s*=\s*1\s*$')
    }
    if (@($manifestChecks.GetEnumerator() | Where-Object { $_.Key -ne "crateManifest" -and -not [bool]$_.Value }).Count -gt 0) {
        Hold-ConnectCrypto `
            -Reason "connect-crypto-version-contract-drift" `
            -Message "Cargo metadata does not match the pinned browser artifact contract." `
            -Checks $manifestChecks
    }

    if (-not (Test-PathInsideRepository $resolvedOutDir)) {
        Hold-ConnectCrypto `
            -Reason "unsafe-output-directory" `
            -Message "The artifact output directory must remain inside this checkout." `
            -Checks @{ outputDirectory = $resolvedOutDir }
    }

    $rustup = Get-Command rustup -ErrorAction SilentlyContinue
    if ($null -eq $rustup) {
        Hold-ConnectCrypto `
            -Reason "missing-rustup" `
            -Message "rustup is required to select the pinned Rust toolchain; nothing was installed." `
            -Checks @{ rustup = $false }
    }

    $rustc = Invoke-Captured -FilePath $rustup.Source -Arguments @("run", $RustToolchain, "rustc", "--version")
    if ($rustc.exitCode -ne 0 -or $rustc.output -notmatch "(?m)^rustc 1\.94\.0(?:\s|\()") {
        Hold-ConnectCrypto `
            -Reason "missing-pinned-rust-toolchain" `
            -Message "Rust toolchain 1.94.0 is not available; nothing was installed." `
            -Checks @{ rustc = $rustc.output }
    }

    $installedTargets = Invoke-Captured -FilePath $rustup.Source -Arguments @("target", "list", "--installed", "--toolchain", $RustToolchain)
    if ($installedTargets.exitCode -ne 0 -or @($installedTargets.output -split "`r?`n" | Where-Object { $_ -eq $WasmTarget }).Count -eq 0) {
        Hold-ConnectCrypto `
            -Reason "missing-wasm32-target" `
            -Message "The pinned Rust toolchain does not have wasm32-unknown-unknown installed; nothing was installed." `
            -Checks @{ installedTargets = $installedTargets.output; requiredTarget = $WasmTarget }
    }

    $wasmBindgen = Get-Command wasm-bindgen -ErrorAction SilentlyContinue
    if ($null -eq $wasmBindgen) {
        Hold-ConnectCrypto `
            -Reason "missing-wasm-bindgen-cli" `
            -Message "wasm-bindgen CLI 0.2.114 is required; nothing was installed." `
            -Checks @{ wasmBindgen = $false; requiredVersion = $WasmBindgenVersion }
    }

    $wasmBindgenVersionResult = Invoke-Captured -FilePath $wasmBindgen.Source -Arguments @("--version")
    if ($wasmBindgenVersionResult.exitCode -ne 0 -or $wasmBindgenVersionResult.output -notmatch "(?m)wasm-bindgen(?:-cli)?\s+0\.2\.114(?:\s|$)") {
        Hold-ConnectCrypto `
            -Reason "wasm-bindgen-version-mismatch" `
            -Message "Installed wasm-bindgen CLI is not exactly 0.2.114; nothing was installed." `
            -Checks @{ wasmBindgen = $wasmBindgenVersionResult.output; requiredVersion = $WasmBindgenVersion }
    }

    $preflightChecks = [ordered]@{
        crateManifest       = $manifestChecks
        rustc                = $rustc.output
        target               = $WasmTarget
        installedTargets    = $installedTargets.output
        wasmBindgen          = $wasmBindgenVersionResult.output
        outputDirectory     = $resolvedOutDir
        targetDirectory     = $targetRoot
        package              = $PackageName
        sourceFingerprint    = $sourceFingerprint
        cargoLocked         = $true
        automaticInstall    = $false
    }

    if ($PlanOnly) {
        Hold-ConnectCrypto `
            -Reason "plan-only" `
            -Message "Prerequisites are present, but -PlanOnly intentionally did not build or publish an artifact." `
            -Checks $preflightChecks
    }

    New-Item -ItemType Directory -Force -Path $targetRoot | Out-Null
    if (Test-Path -LiteralPath $stageDir) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null
    $ownsStage = $true

    Push-Location $repoRoot
    try {
        $cargo = Invoke-Captured -FilePath $rustup.Source -Arguments @(
            "run", $RustToolchain, "cargo", "build", "--locked", "--offline",
            "--target-dir", $targetRoot,
            "--package", $PackageName,
            "--target", $WasmTarget,
            "--release",
            "--features", "wasm"
        )
        if ($cargo.exitCode -ne 0) {
            Fail-ConnectCrypto `
                -Reason "cargo-build-failed" `
                -Message "The pinned offline connect-crypto WASM build failed." `
                -Checks @{ cargo = $cargo.output; preflight = $preflightChecks }
        }

        if (-not (Test-Path -LiteralPath $wasmPath -PathType Leaf)) {
            Fail-ConnectCrypto `
                -Reason "missing-wasm-output" `
                -Message "Cargo completed without producing the expected WASM file." `
                -Checks @{ expected = $wasmPath; cargo = $cargo.output }
        }

        $wasmBindgenArgs = @(
            $wasmPath,
            "--target", "web",
            "--out-dir", $stageDir,
            "--out-name", "connect_crypto"
        )
        $bindgen = Invoke-Captured -FilePath $wasmBindgen.Source -Arguments $wasmBindgenArgs
        if ($bindgen.exitCode -ne 0) {
            Fail-ConnectCrypto `
                -Reason "wasm-bindgen-failed" `
                -Message "Pinned wasm-bindgen could not generate the browser interface." `
                -Checks @{ wasmBindgen = $bindgen.output; arguments = $wasmBindgenArgs }
        }
    }
    finally {
        Pop-Location
    }

    $generatedFiles = @(Get-ChildItem -LiteralPath $stageDir -File | Select-Object -ExpandProperty Name | Sort-Object)
    $unexpectedFiles = @($generatedFiles | Where-Object { $_ -notin $AllowedOutputFiles })
    $missingFiles = @($RequiredFiles | Where-Object { $_ -notin $generatedFiles })
    if ($unexpectedFiles.Count -gt 0 -or $missingFiles.Count -gt 0) {
        Fail-ConnectCrypto `
            -Reason "unexpected-wasm-bindgen-output" `
            -Message "Generated output did not match the fixed Connect artifact allowlist." `
            -Checks @{ generated = $generatedFiles; unexpected = $unexpectedFiles; missing = $missingFiles }
    }

    foreach ($required in $RequiredFiles) {
        $path = Join-Path $stageDir $required
        if ((Get-Item -LiteralPath $path).Length -le 0) {
            Fail-ConnectCrypto `
                -Reason "empty-wasm-artifact" `
                -Message "Generated artifact $required is empty." `
                -Checks @{ file = $required }
        }
    }

    $wasmBytes = [System.IO.File]::ReadAllBytes((Join-Path $stageDir "connect_crypto_bg.wasm"))
    if ($wasmBytes.Length -lt 4 -or $wasmBytes[0] -ne 0 -or $wasmBytes[1] -ne 0x61 -or $wasmBytes[2] -ne 0x73 -or $wasmBytes[3] -ne 0x6D) {
        Fail-ConnectCrypto `
            -Reason "invalid-wasm-magic" `
            -Message "Generated connect_crypto_bg.wasm is not a WebAssembly binary." `
            -Checks @{}
    }

    $entries = @(
        $generatedFiles |
            Where-Object { $_ -ne $ManifestName } |
            Sort-Object |
            ForEach-Object {
                $path = Join-Path $stageDir $_
                [ordered]@{
                    path   = $_
                    bytes  = [int64](Get-Item -LiteralPath $path).Length
                    sha256 = Get-Sha256 -LiteralPath $path
                }
            }
    )
    $manifest = [ordered]@{
        schemaVersion       = $SchemaVersion
        artifact             = $ArtifactName
        protocolMajor       = $ProtocolMajor
        target               = $WasmTarget
        rustToolchain        = $RustToolchain
        wasmBindgenVersion   = $WasmBindgenVersion
        modulePath           = $ModulePath
        sourceFingerprint    = $sourceFingerprint
        files                = $entries
    }
    Write-Utf8NoBom `
        -LiteralPath (Join-Path $stageDir $ManifestName) `
        -Contents (($manifest | ConvertTo-Json -Depth 8 -Compress) + "`n")

    if (Test-Path -LiteralPath $resolvedOutDir) {
        $existingNames = @(Get-ChildItem -LiteralPath $resolvedOutDir -Force | Select-Object -ExpandProperty Name)
        $unexpectedExisting = @($existingNames | Where-Object { $_ -notin $AllowedOutputFiles })
        if ($unexpectedExisting.Count -gt 0) {
            Fail-ConnectCrypto `
                -Reason "output-directory-not-dedicated" `
                -Message "Refusing to overwrite unexpected files in the artifact directory." `
                -Checks @{ outputDirectory = $resolvedOutDir; unexpected = $unexpectedExisting }
        }
    }
    else {
        New-Item -ItemType Directory -Force -Path $resolvedOutDir | Out-Null
    }

    # Publish each file through a same-directory temporary followed by a
    # rename.  The manifest is written last and is the deterministic commit
    # marker for the artifact set; no partial generation can be mistaken for
    # a valid package by the build validator.
    foreach ($fileName in @($entries | ForEach-Object { [string]$_.path } | Sort-Object)) {
        $source = Join-Path $stageDir $fileName
        $destination = Join-Path $resolvedOutDir $fileName
        $temporary = Join-Path $resolvedOutDir (".{0}.{1}.tmp" -f $fileName, $PID)
        Copy-Item -LiteralPath $source -Destination $temporary -Force
        Move-Item -LiteralPath $temporary -Destination $destination -Force
    }
    $manifestDestination = Join-Path $resolvedOutDir $ManifestName
    $manifestTemporary = Join-Path $resolvedOutDir (".{0}.{1}.tmp" -f $ManifestName, $PID)
    Copy-Item -LiteralPath (Join-Path $stageDir $ManifestName) -Destination $manifestTemporary -Force
    Move-Item -LiteralPath $manifestTemporary -Destination $manifestDestination -Force

    foreach ($stale in @($AllowedOutputFiles | Where-Object { $_ -notin $generatedFiles -and $_ -ne $ManifestName })) {
        $stalePath = Join-Path $resolvedOutDir $stale
        if (Test-Path -LiteralPath $stalePath) {
            Remove-Item -LiteralPath $stalePath -Force
        }
    }

    $artifact = [ordered]@{
        sourceDirectory = $resolvedOutDir
        manifest        = $ManifestName
        files           = $entries
        fingerprint     = Get-Sha256 -LiteralPath $manifestDestination
    }
    Complete-ConnectCrypto `
        -Result (New-ConnectCryptoResult -Status "PASS" -Reason "artifact-published" -Message "Connect Rust/WASM artifact published deterministically." -Checks $preflightChecks -Artifact $artifact) `
        -ExitCode 0
}
catch {
    Complete-ConnectCrypto `
        -Result (New-ConnectCryptoResult -Status "FAIL" -Reason "build-script-error" -Message "Connect WASM packaging stopped before publication: $($_.Exception.Message)") `
        -ExitCode 1
}
finally {
    if ($ownsStage -and (Test-Path -LiteralPath $stageDir)) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
