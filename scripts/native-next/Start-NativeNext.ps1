# Phase 0 native-next start validation scaffold.
# Real build/launch is deferred to Phase 2 (Rust host/supervisor).
# -ValidateOnly plans isolated paths/env and runs Capture/Assert only.

[CmdletBinding()]
param(
    [switch]$ValidateOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $ValidateOnly) {
    throw 'Real Start-NativeNext lifecycle is unavailable until Phase 2 provides the Rust host/supervisor binaries and attach/quit commands. Use -ValidateOnly for the Phase 0 isolation scaffold.'
}

. (Join-Path $PSScriptRoot 'Isolation.ps1')
. (Join-Path $PSScriptRoot 'NativeNext.ps1')

Invoke-NativeNextStartValidation -ScriptRoot $PSScriptRoot
