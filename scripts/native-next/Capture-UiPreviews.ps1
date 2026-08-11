[CmdletBinding()]
param(
    [switch]$AllFixtures,
    [switch]$AllThemes,
    [switch]$AllScales,
    [switch]$AutomateWindowStates,
    [string]$TargetDir = 'C:\Temp\devmanager-phase5-ui-capture-correction3',
    [string]$OutputRoot
)

$ErrorActionPreference = 'Stop'
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptRoot '..\..')).Path
$fixtureRoot = Join-Path $repoRoot 'tests\fixtures\ui'
$approvedEvidenceRoot = Join-Path $repoRoot '.devmanager-next\evidence\phase-05\screenshots'
$approvedEvidencePrefix = ([IO.Path]::GetFullPath($approvedEvidenceRoot)).TrimEnd('\') + '\'
$runToken = '{0}-{1}' -f $PID, ([Guid]::NewGuid().ToString('N'))

if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $approvedEvidenceRoot "ui-capture-$runToken"
} else {
    $OutputRoot = [IO.Path]::GetFullPath($OutputRoot)
    if (-not ($OutputRoot.Equals($approvedEvidenceRoot, [StringComparison]::OrdinalIgnoreCase) -or
            $OutputRoot.StartsWith($approvedEvidencePrefix, [StringComparison]::OrdinalIgnoreCase))) {
        throw 'OutputRoot must remain beneath the isolated native-next evidence root.'
    }
    $OutputRoot = Join-Path $OutputRoot "run-$runToken"
}

New-Item -ItemType Directory -Force -Path $OutputRoot | Out-Null
$targetRoot = [IO.Path]::GetFullPath($TargetDir)
$TargetRunDir = Join-Path $targetRoot "run-$runToken"
New-Item -ItemType Directory -Force -Path $TargetRunDir | Out-Null

$oldTargetDir = $env:CARGO_TARGET_DIR
$oldBuildJobs = $env:CARGO_BUILD_JOBS
try {
    $env:CARGO_TARGET_DIR = $TargetRunDir
    $env:CARGO_BUILD_JOBS = '1'

    & cargo build --locked --offline --bin devmanager-next --target-dir $env:CARGO_TARGET_DIR
    if ($LASTEXITCODE -ne 0) {
        throw "isolated devmanager-next build failed with exit code $LASTEXITCODE"
    }
    $binary = Join-Path $env:CARGO_TARGET_DIR 'debug\devmanager-next.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'isolated devmanager-next binary was not produced.'
    }

    $fixtureFiles = @(Get-ChildItem -LiteralPath $fixtureRoot -Filter '*.json' -File |
        Where-Object {
            try {
                $fixture = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
                $fixture.schema -eq 'devmanager.ui.preview/v1'
            } catch {
                $false
            }
        })
    if (-not $AllFixtures) {
        $fixtureFiles = @($fixtureFiles | Where-Object { $_.Name -eq 'component-gallery.json' })
    }
    if ($fixtureFiles.Count -eq 0) {
        throw 'No isolated UI preview fixtures were found beneath tests/fixtures/ui.'
    }

    $themes = if ($AllThemes) { @('dark', 'light') } else { @('dark') }
    $densities = @('compact', 'comfortable')
    $scales = if ($AllScales) { @(100, 125, 150, 200) } else { @(100) }
    $manifest = [System.Collections.Generic.List[object]]::new()
    $captureFailures = 0

    if ($AutomateWindowStates) {
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class DevManagerPreviewWindow {
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
}
'@
    }

    function Get-PngDimensions {
        param([string]$Path)
        $bytes = [IO.File]::ReadAllBytes($Path)
        if ($bytes.Length -lt 24) {
            throw 'PNG is shorter than its signature and IHDR.'
        }
        $signature = [byte[]](137, 80, 78, 71, 13, 10, 26, 10)
        for ($index = 0; $index -lt $signature.Length; $index++) {
            if ($bytes[$index] -ne $signature[$index]) {
                throw 'PNG signature is invalid.'
            }
        }
        $chunkType = [Text.Encoding]::ASCII.GetString($bytes, 12, 4)
        if ($chunkType -ne 'IHDR') {
            throw 'PNG first chunk is not IHDR.'
        }
        $width = ([uint32]$bytes[16] -shl 24) -bor
            ([uint32]$bytes[17] -shl 16) -bor
            ([uint32]$bytes[18] -shl 8) -bor [uint32]$bytes[19]
        $height = ([uint32]$bytes[20] -shl 24) -bor
            ([uint32]$bytes[21] -shl 16) -bor
            ([uint32]$bytes[22] -shl 8) -bor [uint32]$bytes[23]
        [pscustomobject]@{ Width = [uint32]$width; Height = [uint32]$height }
    }

    function Remove-CaptureOutputBestEffort {
        param([string]$Path)
        try {
            if (Test-Path -LiteralPath $Path) {
                Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            }
            return $null
        } catch {
            return $_.Exception.Message
        }
    }

    function Invoke-WindowStateProbe {
        param(
            [string]$State,
            [string[]]$Arguments,
            [string]$OutputPath
        )
        $probe = $null
        $window = $null
        $exitCode = $null
        $failure = $null
        $joined = $false
        $outcome = 'probe-failed'
        $holdEvidence = 'probe-lifecycle-failed'
        try {
            $probe = Start-Process -FilePath $binary -ArgumentList $Arguments -PassThru -WindowStyle Normal
            $probeDeadline = [DateTime]::UtcNow.AddSeconds(2)
            while ([DateTime]::UtcNow -lt $probeDeadline -and -not $probe.HasExited) {
                Start-Sleep -Milliseconds 25
                $probe.Refresh()
                if ($probe.MainWindowHandle -ne 0) {
                    $window = [IntPtr]$probe.MainWindowHandle
                    break
                }
            }
            if ($null -eq $window) {
                throw "isolated $State window did not become discoverable"
            }
            if ($State -eq 'minimized') {
                [DevManagerPreviewWindow]::ShowWindow($window, 6) | Out-Null
            } elseif ($State -eq 'closed') {
                [DevManagerPreviewWindow]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            }
            $joined = $probe.WaitForExit(4000)
            if (-not $joined) {
                try { $probe.Kill($true) } catch { }
                $joined = $probe.WaitForExit(1000)
                if (-not $joined) {
                    $holdEvidence = 'process-join-timeout-after-kill'
                    throw "isolated $State probe could not be joined within its bounded wait"
                }
                throw "isolated $State probe exceeded its bounded wait and required a bounded kill"
            }
            $exitCode = $probe.ExitCode
            if ($exitCode -eq 0) {
                throw "isolated $State probe unexpectedly published a frame"
            }
            if (Test-Path -LiteralPath $OutputPath) {
                throw "isolated $State probe left an output after an unavailable window state"
            }
            $outcome = 'rejected'
            $holdEvidence = 'output-absent-after-state-transition'
        } catch {
            $failure = $_.Exception.Message
        } finally {
            if ($null -ne $probe) {
                try { $probe.Refresh() } catch { }
                if (-not $probe.HasExited) {
                    try { $probe.Kill($true) } catch { }
                    try { $joined = $probe.WaitForExit(1000) } catch { }
                }
                $probe.Dispose()
            }
        }
        [pscustomobject]@{
            Fixture = 'component-gallery'
            Page = "automated-$State-$outcome"
            ScaleMode = 'window-state-automation'
            Outcome = $outcome
            HoldEvidence = $holdEvidence
            Error = $failure
            ExitCode = $exitCode
            JoinState = if ($joined) { 'joined' } else { 'join-unconfirmed' }
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        }
    }

    foreach ($fixtureFile in $fixtureFiles) {
        $fixture = Get-Content -LiteralPath $fixtureFile.FullName -Raw | ConvertFrom-Json
        $isGallery = $fixture.root.kind -eq 'component_gallery'
        $pages = if ($isGallery) {
            foreach ($theme in $themes) {
                foreach ($density in $densities) {
                    foreach ($scale in $scales) {
                        foreach ($section in @('states', 'status', 'samples')) {
                            if ($section -eq 'samples') {
                                foreach ($samplePage in @(0, 1)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = 0; StatusPage = 0; SamplePage = $samplePage; Section = $section }
                                }
                            } elseif ($section -eq 'status') {
                                foreach ($statusPage in @(0, 1)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = 0; StatusPage = $statusPage; SamplePage = 0; Section = $section }
                                }
                            } else {
                                foreach ($statePage in @(0, 1, 2)) {
                                    [pscustomobject]@{ Theme = $theme; Density = $density; Scale = $scale; StatePage = $statePage; StatusPage = 0; SamplePage = 0; Section = $section }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            @([pscustomobject]@{ Theme = $null; Density = $null; Scale = $null; StatePage = $null; StatusPage = $null; SamplePage = $null; Section = $null })
        }

        foreach ($page in $pages) {
            $baseName = [IO.Path]::GetFileNameWithoutExtension($fixtureFile.Name)
            $suffix = if ($null -eq $page.Theme) {
                'default'
            } elseif ($page.Section -eq 'samples') {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-samples-$($page.SamplePage)"
            } elseif ($page.Section -eq 'status') {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-status-$($page.StatusPage)"
            } else {
                "$($page.Theme)-$($page.Density)-$($page.Scale)-states-$($page.StatePage)"
            }
            $output = Join-Path $OutputRoot "$baseName-$suffix.png"
            $arguments = @('--ui-preview', $fixtureFile.FullName, '--output', $output)
            if ($null -ne $page.Theme) {
                $arguments += @('--theme', $page.Theme, '--density', $page.Density, '--scale', [string]$page.Scale, '--section', $page.Section)
                if ($page.Section -eq 'states') {
                    $arguments += @('--state-page', [string]$page.StatePage)
                } elseif ($page.Section -eq 'status') {
                    $arguments += @('--status-page', [string]$page.StatusPage)
                } elseif ($page.Section -eq 'samples') {
                    $arguments += @('--sample-page', [string]$page.SamplePage)
                }
            }

            $attempt = 0
            do {
                & $binary @arguments
                $exitCode = $LASTEXITCODE
                if ($exitCode -eq 0) { break }
                $attempt++
                if ($attempt -lt 3) {
                    [void](Remove-CaptureOutputBestEffort -Path $output)
                    Start-Sleep -Milliseconds 150
                }
            } while ($attempt -lt 3)
            if ($exitCode -ne 0) {
                $cleanupError = Remove-CaptureOutputBestEffort -Path $output
                $holdEvidence = if ($null -eq $cleanupError) {
                    'output-not-published-after-capture-failure'
                } else {
                    'capture-failed-output-cleanup-unconfirmed'
                }
                $failureError = "isolated preview failed with exit $exitCode"
                if ($null -ne $cleanupError) {
                    $failureError = "$failureError; output cleanup failed: $cleanupError"
                }
                $captureFailures++
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'capture-failed'
                    HoldEvidence = $holdEvidence
                    Error = $failureError
                    Bytes = 0
                    Width = 0
                    Height = 0
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
                continue
            }
            try {
                $image = Get-Item -LiteralPath $output -ErrorAction Stop
                if ($image.Length -le 0) {
                    throw "isolated preview produced an empty PNG for page $suffix"
                }
                $dimensions = Get-PngDimensions -Path $output
                if ($dimensions.Width -ne 640 -or $dimensions.Height -ne 360) {
                    throw "decoded PNG dimensions were $($dimensions.Width)x$($dimensions.Height), expected 640x360"
                }
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'captured'
                    HoldEvidence = 'frame-published'
                    Bytes = $image.Length
                    Width = $dimensions.Width
                    Height = $dimensions.Height
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
            } catch {
                try {
                    if (Test-Path -LiteralPath $output) {
                        Remove-Item -LiteralPath $output -Force
                    }
                } catch {
                    # Keep the capture failure visible in the manifest even if
                    # an external file lock prevents cleanup.
                }
                $captureFailures++
                [void]$manifest.Add([pscustomobject]@{
                    Fixture = $baseName
                    Page = $suffix
                    ScaleMode = if ($null -eq $page.Theme) { 'default' } else { 'fixture-token-scale' }
                    Outcome = 'capture-failed'
                    HoldEvidence = 'output-not-published-after-png-validation-failure'
                    Error = $_.Exception.Message
                    Bytes = 0
                    Width = 0
                    Height = 0
                    ExpectedWidth = 640
                    ExpectedHeight = 360
                })
            }
        }

        if ($AutomateWindowStates -and $isGallery) {
            foreach ($state in @('minimized', 'closed')) {
                $probeOutput = Join-Path $OutputRoot "component-gallery-$state.png"
                $probeArguments = @('--ui-preview', $fixtureFile.FullName, '--output', $probeOutput, '--theme', 'dark', '--density', 'compact', '--scale', '100', '--hold-ms', '800')
                $probeResult = Invoke-WindowStateProbe -State $state -Arguments $probeArguments -OutputPath $probeOutput
                [void]$manifest.Add($probeResult)
                if ($probeResult.Outcome -eq 'probe-failed') {
                    $captureFailures++
                }
            }
        }
    }

    if ($AutomateWindowStates) {
        # These values are captured as deterministic fixture token scales above.
        # Mutating the host monitor's physical DPI would affect the desktop and
        # installed applications, so exact OS-DPI evidence belongs in a
        # disposable VM/desktop harness rather than this isolated process.
        $manifest.Add([pscustomobject]@{
            Fixture = 'window-state-matrix'
            Page = 'deferred-os-dpi-100-125-150-200'
            ScaleMode = 'os-monitor-dpi-deferred'
            Outcome = 'deferred'
            HoldEvidence = 'disposable-vm-required-for-physical-monitor-dpi'
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        })
        # A separate VM/desktop harness is still required to put an unrelated
        # top-level window over the preview at the exact first-frame boundary.
        # Keep that one matrix cell visibly deferred instead of claiming a
        # deterministic occlusion capture from a desktop that may be busy.
        $manifest.Add([pscustomobject]@{
            Fixture = 'window-state-matrix'
            Page = 'deferred-occluded-external-desktop-race'
            ScaleMode = 'external-desktop-occlusion-deferred'
            Outcome = 'deferred'
            HoldEvidence = 'disposable-vm-required-for-external-occlusion-race'
            Bytes = 0
            Width = 0
            Height = 0
            ExpectedWidth = 640
            ExpectedHeight = 360
        })
    }

    $manifestPath = Join-Path $OutputRoot 'manifest.json'
    $manifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
    if ($captureFailures -gt 0) {
        throw "isolated preview capture completed with $captureFailures failure(s); see manifest HOLD evidence"
    }
    Write-Output ("Captured {0} isolated preview page(s)." -f $manifest.Count)
    Write-Output 'Manifest and PNGs are under the process/run-unique native-next evidence root.'
} finally {
    if ($null -eq $oldTargetDir) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue } else { $env:CARGO_TARGET_DIR = $oldTargetDir }
    if ($null -eq $oldBuildJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue } else { $env:CARGO_BUILD_JOBS = $oldBuildJobs }
}
