#Requires -Version 5.1
<#
.SYNOPSIS
  Fail-closed package contract checks for DevManager release staging.

.DESCRIPTION
  Validates packaging/package-contract.json against Cargo.toml metadata and an
  actual extracted installer payload (NSIS/MSI/tar/DMG). Exclusion matching uses
  payload-relative paths only. Does not silently fall back to target/release for
  payload inspection. Does not build, sign, or publish.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$StageDir = '',
    [string]$TargetReleaseDir = '',
    [string]$PayloadDir = '',
    [string]$DisposableProfileRoot = '',
    [switch]$SkipBinaryPresence,
    [switch]$SkipHostCtlSmoke,
    [switch]$ExtractInstallers
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $RepoRoot) {
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
    $RepoRoot = (Resolve-Path (Join-Path $scriptDir '..')).Path
}

function Write-Failure([string]$Message) {
    Write-Error -Message $Message -ErrorAction Stop
}

function Get-TomlPackageVersion([string]$CargoTomlPath) {
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $CargoTomlPath) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '[package]') {
            $inPackage = $true
            continue
        }
        if ($inPackage -and $trimmed.StartsWith('[') -and $trimmed -ne '[package]') {
            break
        }
        if ($inPackage -and $trimmed -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    Write-Failure "Failed to read package.version from $CargoTomlPath"
}

function Test-PackagerBinaryList([string]$CargoTomlText, [object[]]$ExpectedBinaries, [string]$BinariesDir) {
    if ($CargoTomlText -notmatch [regex]::Escape("binaries-dir = `"$BinariesDir`"")) {
        Write-Failure "Cargo.toml must set binaries-dir = `"$BinariesDir`""
    }
    foreach ($binary in $ExpectedBinaries) {
        $name = [string]$binary.name
        $mainLiteral = if ([bool]$binary.main) { 'true' } else { 'false' }
        $pattern = "(?s)\[\[package\.metadata\.packager\.binaries\]\]\s*path\s*=\s*`"$([regex]::Escape($name))`"\s*main\s*=\s*$mainLiteral"
        if ($CargoTomlText -notmatch $pattern) {
            Write-Failure "Cargo.toml missing explicit packager binary entry for $name (main=$mainLiteral)"
        }
    }
    foreach ($forbidden in @('devmanager-next', 'devmanager-process-test-helper')) {
        $pattern = "(?s)\[\[package\.metadata\.packager\.binaries\]\]\s*path\s*=\s*`"$([regex]::Escape($forbidden))`""
        if ($CargoTomlText -match $pattern) {
            Write-Failure "Cargo.toml packager binaries must not include forbidden binary $forbidden"
        }
    }
}

function Get-ExeSuffix {
    if ($env:OS -match 'Windows' -or $env:WINDIR) { return '.exe' }
    return ''
}

function Find-SevenZip {
    $candidates = @('7z', '7za')
    foreach ($programFiles in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if ($programFiles) {
            $candidates += Join-Path $programFiles '7-Zip\7z.exe'
        }
    }
    foreach ($candidate in $candidates) {
        if (-not $candidate) { continue }
        $cmd = Get-Command $candidate -ErrorAction SilentlyContinue
        if ($cmd) { return $cmd.Source }
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    }
    return $null
}

function Get-PayloadRelativePath([string]$Root, [string]$FullPath) {
    $rootNorm = (Resolve-Path -LiteralPath $Root).Path.Replace('\', '/').TrimEnd('/')
    $fullNorm = $FullPath.Replace('\', '/')
    if ($fullNorm.Length -lt $rootNorm.Length -or
        -not $fullNorm.StartsWith($rootNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
        Write-Failure ("Path {0} is outside payload root {1}" -f $FullPath, $Root)
    }
    return $fullNorm.Substring($rootNorm.Length).TrimStart('/')
}

function Test-RelativeExclusionMatch([string]$RelativePath, [string]$Token, [string]$Name) {
    $token = $Token.Replace('\', '/').Trim('/')
    $rel = $RelativePath.Replace('\', '/').Trim('/')
    if ($token -in @('session.json', 'config.json', 'remote.json', '.env')) {
        return $Name -eq $token
    }
    if ($token -like '*.exe' -or ($token.StartsWith('devmanager-') -and $token -notmatch '[\\/]')) {
        return ($Name -eq $token) -or ($Name -eq "$token.exe")
    }
    $segments = @($rel.Split('/', [System.StringSplitOptions]::RemoveEmptyEntries))
    return ($segments -contains $token) -or ($rel -eq $token) -or ($rel.StartsWith("$token/"))
}

function Assert-BinarySet([string]$Root, $Contract, [string]$ExeSuffix) {
    foreach ($binary in $Contract.binaries) {
        $fileName = "$($binary.name)$ExeSuffix"
        $matches = @(Get-ChildItem -LiteralPath $Root -Recurse -File -Filter $fileName -ErrorAction SilentlyContinue)
        if ($matches.Count -lt 1) {
            Write-Failure "Required binary $fileName not found under payload/root $Root"
        }
    }
    foreach ($forbidden in $Contract.forbiddenBinaries) {
        $fileName = "$forbidden$ExeSuffix"
        $matches = @(Get-ChildItem -LiteralPath $Root -Recurse -File -Filter $fileName -ErrorAction SilentlyContinue)
        if ($matches.Count -gt 0) {
            $rel = Get-PayloadRelativePath -Root $Root -FullPath $matches[0].FullName
            Write-Failure "Forbidden binary $fileName found under payload-relative path $rel"
        }
    }
}

function Assert-ExclusionSet([string]$Root, [string[]]$ExclusionTokens) {
    $files = @(Get-ChildItem -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue)
    foreach ($token in $ExclusionTokens) {
        $hits = @()
        foreach ($item in $files) {
            $rel = Get-PayloadRelativePath -Root $Root -FullPath $item.FullName
            if (Test-RelativeExclusionMatch -RelativePath $rel -Token $token -Name $item.Name) {
                $hits += $item
            }
        }
        if ($hits.Count -gt 0) {
            $relHit = Get-PayloadRelativePath -Root $Root -FullPath $hits[0].FullName
            Write-Failure "Excluded payload token '$token' found at payload-relative path '$relHit'"
        }
    }
}

function Assert-WindowsFileMetadata([string]$ExePath, $WindowsMeta, [string]$Version) {
    if (-not ($env:OS -match 'Windows' -or $env:WINDIR)) {
        return
    }
    $info = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($ExePath)
    if ($info.ProductName -ne [string]$WindowsMeta.productName) {
        Write-Failure ("ProductName mismatch for {0}: {1}" -f $ExePath, $info.ProductName)
    }
    if ($info.FileDescription -ne [string]$WindowsMeta.fileDescription) {
        Write-Failure ("FileDescription mismatch for {0}: {1}" -f $ExePath, $info.FileDescription)
    }
    if ($info.OriginalFilename -ne [string]$WindowsMeta.originalFilename) {
        Write-Failure ("OriginalFilename mismatch for {0}: {1}" -f $ExePath, $info.OriginalFilename)
    }
    if ($info.InternalName -ne [string]$WindowsMeta.internalName) {
        Write-Failure ("InternalName mismatch for {0}: {1}" -f $ExePath, $info.InternalName)
    }
    $productVersion = [string]$info.ProductVersion
    if ($productVersion -notlike "$Version*") {
        Write-Failure ("ProductVersion for {0} must start with {1} (found {2})" -f $ExePath, $Version, $productVersion)
    }
}

function Assert-PayloadResourcesAndIcons([string]$Root, $Contract) {
    foreach ($resource in $Contract.resources) {
        $name = Split-Path -Leaf ([string]$resource)
        $matches = @(Get-ChildItem -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -eq $name })
        if ($matches.Count -lt 1) {
            Write-Failure "Packaged payload missing resource root/name '$name' (from $($resource))"
        }
    }
    $iconHits = @(Get-ChildItem -LiteralPath $Root -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Extension -in @('.ico', '.icns', '.png') -and $_.Name -match 'devmanager' })
    if ($iconHits.Count -lt 1) {
        Write-Failure "Packaged payload missing DevManager icon resources"
    }
}

function Invoke-HostCtlSmoke([string]$HostBinary, [string]$ProfileRoot, [string[]]$Args, [string]$ExpectedIdentity) {
    if (-not (Test-Path -LiteralPath $HostBinary -PathType Leaf)) {
        Write-Failure "Host binary missing for ctl smoke: $HostBinary"
    }
    New-Item -ItemType Directory -Force -Path $ProfileRoot | Out-Null
    $profileName = 'package-contract-disposable'
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $HostBinary
    $psi.Arguments = ($Args -join ' ')
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables['DEVMANAGER_PROFILE'] = $profileName
    $psi.EnvironmentVariables['DEVMANAGER_CONFIG_DIR'] = $ProfileRoot
    if ($psi.EnvironmentVariables.ContainsKey('APPDATA')) {
        $psi.EnvironmentVariables['APPDATA'] = $ProfileRoot
    }
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    if ($proc.ExitCode -ne 0) {
        Write-Failure "devmanager-host ctl smoke failed (exit $($proc.ExitCode)): $stderr"
    }
    try {
        $doc = $stdout | ConvertFrom-Json
    } catch {
        Write-Failure "ctl actions --json did not return JSON: $stdout"
    }
    if ([int]$doc.schema_version -ne 1) {
        Write-Failure "ctl actions schema_version must be 1"
    }
    if (-not $doc.actions -or @($doc.actions).Count -lt 1) {
        Write-Failure "ctl actions --json returned no actions"
    }
    Write-Host "Host ctl smoke passed for disposable profile under $ProfileRoot (identity contract $ExpectedIdentity)."
}

function Expand-InstallerPayload([string]$Stage, [string]$Destination) {
    if (-not (Test-Path -LiteralPath $Stage)) {
        Write-Failure "StageDir does not exist for extraction: $Stage"
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null

    $nsis = @(Get-ChildItem -LiteralPath $Stage -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like '*-setup.exe' -or ($_.Extension -eq '.exe' -and $_.Name -like '*setup*' -and $_.Name -notlike '*.sig') })
    $msi = @(Get-ChildItem -LiteralPath $Stage -Recurse -File -Filter '*.msi' -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -notlike '*.sig' })
    $archives = @(Get-ChildItem -LiteralPath $Stage -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like '*.app.tar.gz' -or ($_.Name -like '*.tar.gz' -and $_.Name -notlike '*.sig') })
    $dmgs = @(Get-ChildItem -LiteralPath $Stage -Recurse -File -Filter '*.dmg' -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -notlike '*.sig' })

    $installerCount = $nsis.Count + $msi.Count + $archives.Count + $dmgs.Count
    if ($installerCount -lt 1) {
        Write-Failure "StageDir contains no NSIS/MSI/tar.gz/DMG installers to extract: $Stage"
    }

    $sevenZip = Find-SevenZip
    if (($nsis.Count -gt 0 -or $dmgs.Count -gt 0) -and -not $sevenZip) {
        # macOS can mount DMG without 7-Zip; NSIS still requires 7-Zip.
        if ($nsis.Count -gt 0) {
            Write-Failure "NSIS installer(s) present but no supported extractor (7-Zip) is available; refusing silent target/release fallback"
        }
    }

    $expanded = 0

    foreach ($package in $msi) {
        $target = Join-Path $Destination ("msi-" + [IO.Path]::GetFileNameWithoutExtension($package.Name))
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        $args = @('/a', $package.FullName, '/qn', "TARGETDIR=$target")
        $proc = Start-Process -FilePath 'msiexec.exe' -ArgumentList $args -Wait -PassThru -NoNewWindow
        if ($proc.ExitCode -ne 0) {
            Write-Failure "msiexec administrative extract failed for $($package.FullName) (exit $($proc.ExitCode))"
        }
        $expanded += 1
    }

    foreach ($archive in $archives) {
        $target = Join-Path $Destination ("tar-" + $archive.BaseName)
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        tar -xzf $archive.FullName -C $target
        if ($LASTEXITCODE -ne 0) {
            Write-Failure "tar extract failed for $($archive.FullName)"
        }
        $expanded += 1
    }

    foreach ($package in $nsis) {
        if (-not $sevenZip) {
            Write-Failure "NSIS extractor unavailable for $($package.FullName)"
        }
        $target = Join-Path $Destination ("nsis-" + [IO.Path]::GetFileNameWithoutExtension($package.Name))
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        & $sevenZip @('x', '-y', "-o$target", $package.FullName) | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Failure "7-Zip NSIS extract failed for $($package.FullName) (exit $LASTEXITCODE)"
        }
        $expanded += 1
    }

    foreach ($dmg in $dmgs) {
        $target = Join-Path $Destination ("dmg-" + [IO.Path]::GetFileNameWithoutExtension($dmg.Name))
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        $canHdiutil = ($env:RUNNER_OS -eq 'macOS') -or (Test-Path -LiteralPath '/usr/bin/hdiutil')
        if ($canHdiutil) {
            $mountRoot = Join-Path $Destination ("dmg-mount-" + [guid]::NewGuid().ToString('N'))
            New-Item -ItemType Directory -Force -Path $mountRoot | Out-Null
            $attachOut = & hdiutil attach -nobrowse -readonly -mountroot $mountRoot $dmg.FullName 2>&1
            if ($LASTEXITCODE -ne 0) {
                Write-Failure ("hdiutil attach failed for {0}: {1}" -f $dmg.FullName, $attachOut)
            }
            try {
                $volumes = @(Get-ChildItem -LiteralPath $mountRoot -Directory -ErrorAction SilentlyContinue)
                if ($volumes.Count -lt 1) {
                    Write-Failure "hdiutil attached DMG but produced no volumes for $($dmg.FullName)"
                }
                foreach ($volume in $volumes) {
                    Copy-Item -Path (Join-Path $volume.FullName '*') -Destination $target -Recurse -Force
                }
            } finally {
                & hdiutil detach $mountRoot -quiet 2>$null | Out-Null
            }
            $expanded += 1
            continue
        }
        if (-not $sevenZip) {
            Write-Failure "DMG present but neither hdiutil nor 7-Zip is available; refusing silent fallback"
        }
        & $sevenZip @('x', '-y', "-o$target", $dmg.FullName) | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Failure "7-Zip DMG extract failed for $($dmg.FullName) (exit $LASTEXITCODE)"
        }
        $expanded += 1
    }

    if ($expanded -lt 1) {
        Write-Failure "Installer extraction produced no payloads under $Destination"
    }
    Write-Host "Extracted $expanded installer payload(s) into $Destination"
    return $Destination
}

function Invoke-PayloadInspection([string]$PayloadRoot, $Contract, [string]$Version, [string]$ShippingIdentity, [string[]]$ExclusionTokens, [bool]$SkipCtl) {
    if (-not $PayloadRoot -or -not (Test-Path -LiteralPath $PayloadRoot)) {
        Write-Failure "Payload inspection root missing: $PayloadRoot"
    }
    $exeSuffix = Get-ExeSuffix
    Assert-BinarySet -Root $PayloadRoot -Contract $Contract -ExeSuffix $exeSuffix
    Assert-ExclusionSet -Root $PayloadRoot -ExclusionTokens $ExclusionTokens
    Assert-PayloadResourcesAndIcons -Root $PayloadRoot -Contract $Contract

    foreach ($binary in $Contract.binaries) {
        $fileName = "$($binary.name)$exeSuffix"
        $match = @(Get-ChildItem -LiteralPath $PayloadRoot -Recurse -File -Filter $fileName -ErrorAction SilentlyContinue |
            Select-Object -First 1)
        if ($match.Count -eq 1 -and $binary.windows) {
            Assert-WindowsFileMetadata -ExePath $match[0].FullName -WindowsMeta $binary.windows -Version $Version
        }
    }

    if (-not $SkipCtl) {
        $hostName = "devmanager-host$exeSuffix"
        $hostMatch = @(Get-ChildItem -LiteralPath $PayloadRoot -Recurse -File -Filter $hostName -ErrorAction SilentlyContinue |
            Select-Object -First 1)
        if ($hostMatch.Count -ne 1) {
            Write-Failure "Unable to locate $hostName under extracted payload $PayloadRoot for ctl smoke"
        }
        if (-not $script:DisposableProfileRoot) {
            $script:DisposableProfileRoot = Join-Path ([IO.Path]::GetTempPath()) ("devmanager-ctl-" + [guid]::NewGuid().ToString('N'))
        }
        Invoke-HostCtlSmoke `
            -HostBinary $hostMatch[0].FullName `
            -ProfileRoot $script:DisposableProfileRoot `
            -Args @([string[]]$Contract.hostCtlSmoke) `
            -ExpectedIdentity $ShippingIdentity
    }
}

$contractPath = Join-Path $RepoRoot 'packaging\package-contract.json'
if (-not (Test-Path -LiteralPath $contractPath -PathType Leaf)) {
    Write-Failure "Missing package contract: $contractPath"
}

$contract = Get-Content -LiteralPath $contractPath -Raw | ConvertFrom-Json
$cargoTomlPath = Join-Path $RepoRoot 'Cargo.toml'
$cargoTomlText = Get-Content -LiteralPath $cargoTomlPath -Raw
$version = Get-TomlPackageVersion -CargoTomlPath $cargoTomlPath
$shippingIdentity = ("devmanager/{0}" -f $version)
$expectedShipping = [string]$contract.shippingIdentity.format -replace '\{version\}', $version
if ($shippingIdentity -ne $expectedShipping) {
    Write-Failure "Shipping identity mismatch: $shippingIdentity != $expectedShipping"
}
if ([string]$contract.shippingIdentity.clientBuild -notmatch '^devmanager/\{version\}$') {
    Write-Failure "package contract clientBuild must be shipping devmanager/{version}"
}
if ([string]$contract.shippingIdentity.clientBuild -match 'next') {
    Write-Failure "package contract must not preserve devmanager-next shipping identity"
}

$before = [string]$contract.beforePackagingCommand
if ($cargoTomlText -notmatch [regex]::Escape("before-packaging-command = `"$before`"")) {
    Write-Failure "Cargo.toml before-packaging-command must equal '$before'"
}

$binariesDir = [string]$contract.binariesDir
Test-PackagerBinaryList -CargoTomlText $cargoTomlText -ExpectedBinaries $contract.binaries -BinariesDir $binariesDir

foreach ($icon in $contract.icons) {
    $iconPath = Join-Path $RepoRoot ([string]$icon)
    if (-not (Test-Path -LiteralPath $iconPath -PathType Leaf)) {
        Write-Failure "Missing packaging icon: $iconPath"
    }
}

foreach ($resource in $contract.resources) {
    $resourcePath = Join-Path $RepoRoot ([string]$resource)
    if (-not (Test-Path -LiteralPath $resourcePath)) {
        Write-Failure "Missing packaging resource root: $resourcePath"
    }
}

if (-not $contract.webview2 -or -not $contract.webview2.windowsExpectation) {
    Write-Failure "package contract must declare WebView2 windowsExpectation"
}
$cargoTomlLower = $cargoTomlText.ToLowerInvariant()
foreach ($crateName in $contract.webview2.dependencyCrates) {
    if ($cargoTomlLower -notmatch [regex]::Escape([string]$crateName)) {
        Write-Failure "Cargo.toml missing WebView2-related dependency '$crateName'"
    }
}

$exclusionsPath = Join-Path $RepoRoot 'packaging\exclusions.txt'
if (-not (Test-Path -LiteralPath $exclusionsPath -PathType Leaf)) {
    Write-Failure "Missing exclusions list: $exclusionsPath"
}
$exclusionTokens = @(
    Get-Content -LiteralPath $exclusionsPath |
        Where-Object { $_ -and -not $_.StartsWith('#') } |
        ForEach-Object { $_.Trim() }
)
foreach ($required in @('.worktrees', 'target', 'evidence', 'tests/fixtures', 'zz-archive', 'Portal', 'secrets', 'dev-profile', 'session.json')) {
    if ($exclusionTokens -notcontains $required) {
        Write-Failure "exclusions.txt missing required token '$required'"
    }
}
foreach ($required in $contract.excludes) {
    if ($exclusionTokens -notcontains [string]$required) {
        Write-Failure "exclusions.txt missing contract exclude '$required'"
    }
}

$script:DisposableProfileRoot = $DisposableProfileRoot
$extractedPayload = ''

if ($StageDir) {
    if (-not (Test-Path -LiteralPath $StageDir)) {
        Write-Failure "Package search root does not exist: $StageDir"
    }
    if (-not $ExtractInstallers -and -not $PayloadDir) {
        Write-Failure "StageDir requires -ExtractInstallers or an explicit extracted -PayloadDir; refusing silent target/release fallback"
    }
    if ($ExtractInstallers) {
        $extractRoot = Join-Path ([IO.Path]::GetTempPath()) ("devmanager-package-extract-" + [guid]::NewGuid().ToString('N'))
        $extractedPayload = Expand-InstallerPayload -Stage $StageDir -Destination $extractRoot
    }
}

if (-not $SkipBinaryPresence) {
    $exeSuffix = Get-ExeSuffix

    if ($TargetReleaseDir) {
        if (-not (Test-Path -LiteralPath $TargetReleaseDir)) {
            Write-Failure "Package search root does not exist: $TargetReleaseDir"
        }
        # binaries-dir presence only; never treat target/release as installer payload.
        Assert-BinarySet -Root $TargetReleaseDir -Contract $contract -ExeSuffix $exeSuffix
    }

    $payloadRoots = @()
    if ($PayloadDir) {
        if (-not (Test-Path -LiteralPath $PayloadDir)) {
            Write-Failure "PayloadDir does not exist: $PayloadDir"
        }
        $resolvedPayload = (Resolve-Path -LiteralPath $PayloadDir).Path
        $normalizedPayload = $resolvedPayload.Replace('\', '/').ToLowerInvariant()
        if ($normalizedPayload -match '/(target|dist|\.worktrees)(/|$)') {
            Write-Failure "PayloadDir must be an extracted installer payload, not target/dist/.worktrees ($PayloadDir)"
        }
        $payloadRoots += $resolvedPayload
    }
    if ($extractedPayload) {
        $payloadRoots += $extractedPayload
    }

    if ($StageDir -and $payloadRoots.Count -lt 1) {
        Write-Failure "No extracted installer payload available for StageDir inspection"
    }

    foreach ($root in $payloadRoots) {
        Invoke-PayloadInspection `
            -PayloadRoot $root `
            -Contract $contract `
            -Version $version `
            -ShippingIdentity $shippingIdentity `
            -ExclusionTokens $exclusionTokens `
            -SkipCtl:$SkipHostCtlSmoke
    }

    if ($StageDir) {
        # StageDir artifact tree: forbidden names only (relative), not repo exclusion dirs.
        $stageFiles = @(Get-ChildItem -LiteralPath $StageDir -Recurse -Force -File -ErrorAction SilentlyContinue)
        foreach ($forbidden in $contract.forbiddenBinaries) {
            foreach ($file in $stageFiles) {
                $rel = Get-PayloadRelativePath -Root $StageDir -FullPath $file.FullName
                $hit = (Test-RelativeExclusionMatch -RelativePath $rel -Token $forbidden -Name $file.Name) -or
                    (Test-RelativeExclusionMatch -RelativePath $rel -Token "$forbidden.exe" -Name $file.Name)
                if ($hit) {
                    Write-Failure "Forbidden packaged path '$rel' under StageDir"
                }
            }
        }
    }

    if (-not $TargetReleaseDir -and -not $StageDir -and $payloadRoots.Count -lt 1) {
        Write-Host "Package contract source checks passed (no stage/target/payload directories supplied)."
        exit 0
    }
}

Write-Host ("Package contract passed for {0} (protocol {1}.{2})." -f $shippingIdentity, $contract.protocol.major, $contract.protocol.minor)
