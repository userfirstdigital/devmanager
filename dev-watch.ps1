param(
    [int]$DebounceMs = 500,
    [switch]$Release,
    [switch]$Once
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $RepoRoot

$BuildProfile = if ($Release) { "release" } else { "debug" }
$BuildTargetDir = Join-Path $RepoRoot "target-watch"
$BuildOutputDir = Join-Path $BuildTargetDir $BuildProfile
$BuildExe = Join-Path $BuildOutputDir "devmanager.exe"
$BuildPdb = Join-Path $BuildOutputDir "devmanager.pdb"
$LiveDir = Join-Path $RepoRoot "target-live"
$LiveExe = Join-Path $LiveDir "devmanager.exe"
$LivePdb = Join-Path $LiveDir "devmanager.pdb"
$script:AppProcess = $null

function Write-Status {
    param(
        [string]$Message,
        [ValidateSet("info", "build", "success", "warn", "error")]
        [string]$Level = "info"
    )

    $color = switch ($Level) {
        "build" { "Cyan" }
        "success" { "Green" }
        "warn" { "Yellow" }
        "error" { "Red" }
        default { "Gray" }
    }

    Write-Host ("[watch {0}] {1}" -f (Get-Date -Format "HH:mm:ss"), $Message) -ForegroundColor $color
}

function Stop-ManagedApp {
    if ($null -eq $script:AppProcess) {
        return
    }

    try {
        if (-not $script:AppProcess.HasExited) {
            Write-Status ("Stopping running app (pid {0})." -f $script:AppProcess.Id) "warn"
            Stop-Process -Id $script:AppProcess.Id -Force -ErrorAction SilentlyContinue
            $null = $script:AppProcess.WaitForExit(5000)
        }
    } catch {
    } finally {
        $script:AppProcess = $null
    }
}

function Stop-StaleLiveCopies {
    $livePath = $LiveExe.ToLowerInvariant()
    $runningCopies = Get-CimInstance Win32_Process -Filter "Name = 'devmanager.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.ToLowerInvariant() -eq $livePath }

    foreach ($copy in $runningCopies) {
        if ($script:AppProcess -and $copy.ProcessId -eq $script:AppProcess.Id) {
            continue
        }

        Write-Status ("Stopping stale live copy (pid {0})." -f $copy.ProcessId) "warn"
        Stop-Process -Id $copy.ProcessId -Force -ErrorAction SilentlyContinue
    }
}

function Wait-ForFileUnlock {
    param(
        [string]$Path,
        [int]$TimeoutMs = 8000
    )

    if (-not (Test-Path $Path)) {
        return
    }

    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        try {
            $stream = [System.IO.File]::Open(
                $Path,
                [System.IO.FileMode]::Open,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::None
            )
            $stream.Dispose()
            return
        } catch {
            Start-Sleep -Milliseconds 120
        }
    }

    throw ("Timed out waiting for {0} to unlock." -f $Path)
}

function Invoke-BuildAndRelaunch {
    param([string]$Reason)

    Write-Status ("Building because {0} changed..." -f $Reason) "build"

    $cargoArgs = @("build", "--target-dir", $BuildTargetDir)
    if ($Release) {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Status "Build failed. Keeping the current app window running." "error"
        return $false
    }

    if (-not (Test-Path $BuildExe)) {
        throw ("Build succeeded but no executable was found at {0}." -f $BuildExe)
    }

    New-Item -ItemType Directory -Path $LiveDir -Force | Out-Null

    Stop-ManagedApp
    Stop-StaleLiveCopies
    Wait-ForFileUnlock -Path $LiveExe

    Copy-Item $BuildExe $LiveExe -Force
    if (Test-Path $BuildPdb) {
        Copy-Item $BuildPdb $LivePdb -Force
    }

    $script:AppProcess = Start-Process -FilePath $LiveExe -WorkingDirectory $RepoRoot -PassThru
    Write-Status ("Launched DevManager from target-live (pid {0})." -f $script:AppProcess.Id) "success"
    return $true
}

function Get-ChangeLabel {
    param($WatchEvent)

    $args = $WatchEvent.SourceEventArgs
    if ($args -is [System.IO.RenamedEventArgs]) {
        return ("{0} -> {1}" -f $args.OldFullPath, $args.FullPath)
    }

    return $args.FullPath
}

$watchSpecs = @(
    @{ Path = Join-Path $RepoRoot "src"; Filter = "*"; IncludeSubdirectories = $true },
    @{ Path = Join-Path $RepoRoot "assets"; Filter = "*"; IncludeSubdirectories = $true },
    @{ Path = $RepoRoot; Filter = "Cargo.toml"; IncludeSubdirectories = $false },
    @{ Path = $RepoRoot; Filter = "Cargo.lock"; IncludeSubdirectories = $false }
)

$watchers = @()
$subscriptions = @()

foreach ($spec in $watchSpecs) {
    if (-not (Test-Path $spec.Path)) {
        continue
    }

    $watcher = New-Object System.IO.FileSystemWatcher
    $watcher.Path = $spec.Path
    $watcher.Filter = $spec.Filter
    $watcher.IncludeSubdirectories = $spec.IncludeSubdirectories
    $watcher.NotifyFilter = [System.IO.NotifyFilters]"FileName, LastWrite, DirectoryName, Size, CreationTime"
    $watcher.EnableRaisingEvents = $true
    $watchers += $watcher

    foreach ($eventName in @("Changed", "Created", "Deleted", "Renamed")) {
        $sourceId = "devmanager-watch-{0}-{1}" -f $watchers.Count, $eventName
        $subscriptions += Register-ObjectEvent -InputObject $watcher -EventName $eventName -SourceIdentifier $sourceId
    }
}

$pendingBuild = $true
$lastReason = "startup"
$lastChangeAt = Get-Date

Write-Status "Watching src/, assets/, Cargo.toml, and Cargo.lock." "info"
Write-Status "Builds go to target-watch/ and the running app comes from target-live/ to avoid Windows locking." "info"

try {
    if ($Once) {
        if (-not (Invoke-BuildAndRelaunch -Reason $lastReason)) {
            exit 1
        }
        return
    }

    while ($true) {
        $event = Wait-Event -Timeout 1
        if ($null -ne $event) {
            $pendingBuild = $true
            $lastReason = Get-ChangeLabel -WatchEvent $event
            $lastChangeAt = Get-Date
            Write-Status ("Change detected: {0}" -f $lastReason) "info"
            Remove-Event -EventIdentifier $event.EventIdentifier | Out-Null

            while ($queued = Wait-Event -Timeout 0) {
                $lastReason = Get-ChangeLabel -WatchEvent $queued
                $lastChangeAt = Get-Date
                Remove-Event -EventIdentifier $queued.EventIdentifier | Out-Null
            }

            continue
        }

        if ($pendingBuild -and (((Get-Date) - $lastChangeAt).TotalMilliseconds -ge $DebounceMs)) {
            $pendingBuild = $false
            $null = Invoke-BuildAndRelaunch -Reason $lastReason
        }
    }
} finally {
    foreach ($subscription in $subscriptions) {
        Unregister-Event -SourceIdentifier $subscription.SourceIdentifier -ErrorAction SilentlyContinue
    }

    foreach ($watcher in $watchers) {
        $watcher.EnableRaisingEvents = $false
        $watcher.Dispose()
    }
}
