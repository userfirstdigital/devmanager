#Requires -Version 5.1
<#
.SYNOPSIS
  Cryptographically verify cargo-packager updater signatures with the configured public key.

.DESCRIPTION
  Decodes DEVMANAGER_UPDATE_PUBKEY and each *.sig beside its artifact, then verifies with
  the minisign CLI (same scheme used by cargo-packager-updater). Fails closed when the
  public key, minisign binary, artifact, or signature is missing or invalid.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ArtifactDir,
    [string]$PublicKey = $env:DEVMANAGER_UPDATE_PUBKEY,
    [string]$MinisignPath = 'minisign'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Write-Failure([string]$Message) {
    Write-Error -Message $Message -ErrorAction Stop
}

if (-not $PublicKey) {
    Write-Failure 'DEVMANAGER_UPDATE_PUBKEY / -PublicKey is required'
}
if (-not (Test-Path -LiteralPath $ArtifactDir)) {
    Write-Failure "ArtifactDir does not exist: $ArtifactDir"
}

$minisignCmd = Get-Command $MinisignPath -ErrorAction SilentlyContinue
if (-not $minisignCmd) {
    Write-Failure "minisign executable not found at '$MinisignPath'; install minisign before verifying signatures"
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("devmanager-sigverify-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $pubPath = Join-Path $work 'minisign.pub'
    try {
        $pubBytes = [Convert]::FromBase64String($PublicKey.Trim())
        [IO.File]::WriteAllBytes($pubPath, $pubBytes)
    } catch {
        # Some environments store the raw minisign public key text instead of base64.
        Set-Content -LiteralPath $pubPath -Value $PublicKey.Trim() -Encoding ascii
    }
    $pubText = Get-Content -LiteralPath $pubPath -Raw
    if ($pubText -notmatch 'RW') {
        Write-Failure 'Decoded public key does not look like a minisign public key'
    }

    $sigFiles = @(Get-ChildItem -LiteralPath $ArtifactDir -Recurse -File -Filter '*.sig' -ErrorAction SilentlyContinue)
    if ($sigFiles.Count -lt 1) {
        Write-Failure "No .sig files found under $ArtifactDir"
    }

    $verified = 0
    foreach ($sigFile in $sigFiles) {
        $artifact = $sigFile.FullName.Substring(0, $sigFile.FullName.Length - 4)
        if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) {
            Write-Failure "Signature has no sibling artifact: $($sigFile.FullName)"
        }
        $sigText = (Get-Content -LiteralPath $sigFile.FullName -Raw).Trim()
        if (-not $sigText) {
            Write-Failure "Empty signature file: $($sigFile.FullName)"
        }
        $decodedSigPath = Join-Path $work ($sigFile.Name + '.minisig')
        try {
            $sigBytes = [Convert]::FromBase64String($sigText)
            [IO.File]::WriteAllBytes($decodedSigPath, $sigBytes)
        } catch {
            # Already raw minisign signature text.
            Set-Content -LiteralPath $decodedSigPath -Value $sigText -Encoding ascii
        }

        & $minisignCmd.Source -V -p $pubPath -m $artifact -x $decodedSigPath
        if ($LASTEXITCODE -ne 0) {
            Write-Failure "Cryptographic signature verification failed for $artifact"
        }
        $verified += 1
        Write-Host "Verified signature for $artifact"
    }

    Write-Host "Verified $verified cargo-packager updater signature(s) under $ArtifactDir"
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
