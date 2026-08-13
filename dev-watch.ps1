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
$BuildHostExe = Join-Path $BuildOutputDir "devmanager-host.exe"
$BuildHostPdb = Join-Path $BuildOutputDir "devmanager_host.pdb"
$LiveDir = Join-Path $RepoRoot "target-live-dev"
$LiveExe = Join-Path $LiveDir "devmanager.exe"
$LivePdb = Join-Path $LiveDir "devmanager.pdb"
$LiveHostExe = Join-Path $LiveDir "devmanager-host.exe"
$LiveHostPdb = Join-Path $LiveDir "devmanager_host.pdb"
$LaunchStatus = Join-Path $LiveDir "launch-status.txt"
$NativeProfileBase = Join-Path $RepoRoot ".devmanager-next\dev-profile"
$script:AppProcess = $null
$DevManagerLabel = "Dev Smoke"

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
    $liveProcesses = @(
        @{ Name = "devmanager.exe"; Path = $LiveExe },
        @{ Name = "devmanager-host.exe"; Path = $LiveHostExe }
    )

    foreach ($liveProcess in $liveProcesses) {
        $livePath = $liveProcess.Path.ToLowerInvariant()
        $runningCopies = Get-CimInstance Win32_Process -Filter ("Name = '{0}'" -f $liveProcess.Name) -ErrorAction SilentlyContinue |
            Where-Object { $_.ExecutablePath -and $_.ExecutablePath.ToLowerInvariant() -eq $livePath }

        foreach ($copy in $runningCopies) {
            if ($script:AppProcess -and $copy.ProcessId -eq $script:AppProcess.Id) {
                continue
            }

            Write-Status ("Stopping stale live {0} (pid {1})." -f $liveProcess.Name, $copy.ProcessId) "warn"
            Stop-Process -Id $copy.ProcessId -Force -ErrorAction SilentlyContinue
        }
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

if (-not ("DevWatch.Native" -as [type])) {
    Add-Type -Namespace DevWatch -Name Native -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true, CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr CreateFileW(string name, uint access, uint share, System.IntPtr sa, uint disposition, uint flags, System.IntPtr template);
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
public static extern bool CloseHandle(System.IntPtr handle);
'@
}

# The host opens its profile root with FILE_DELETE_CHILD. Windows "Modify" does
# not grant that right, so a directory inheriting only Modify fails the host's
# fail-closed root check with a bare "Access is denied".
function Test-ProfileRootDeleteChild {
    param([string]$Path)

    $DesiredDeleteChild = 0x00000040
    $ShareAll = 0x00000007
    $OpenExisting = 3
    $DirectoryFlags = 0x02000000 -bor 0x00200000

    $handle = [DevWatch.Native]::CreateFileW(
        $Path, $DesiredDeleteChild, $ShareAll, [IntPtr]::Zero, $OpenExisting, $DirectoryFlags, [IntPtr]::Zero)
    if ($handle -eq [IntPtr](-1)) {
        return $false
    }

    [void][DevWatch.Native]::CloseHandle($handle)
    return $true
}

function Initialize-DevProfileStorage {
    New-Item -ItemType Directory -Path $NativeProfileBase -Force | Out-Null

    if (Test-ProfileRootDeleteChild -Path $NativeProfileBase) {
        return
    }

    Write-Status "Dev profile storage lacks FILE_DELETE_CHILD; granting this user full control." "warn"
    $grant = "{0}:(OI)(CI)(F)" -f ([System.Security.Principal.WindowsIdentity]::GetCurrent().Name)
    & icacls $NativeProfileBase /grant $grant /T | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw ("Failed to grant full control on {0}." -f $NativeProfileBase)
    }

    if (-not (Test-ProfileRootDeleteChild -Path $NativeProfileBase)) {
        throw ("The dev profile root {0} still denies FILE_DELETE_CHILD; the host cannot open its config store." -f $NativeProfileBase)
    }
}

function Get-WorkspaceProfileName {
    # Mirrors workspace_profile_name() in src/ui/native_shell.rs: the first eight
    # bytes of sha256("native-next" || 0x00 || canonicalized workspace path).
    # Rust canonicalization yields a verbatim path with an uppercase drive letter.
    $full = (Get-Item -LiteralPath $RepoRoot).FullName.TrimEnd('\')
    $full = $full.Substring(0, 1).ToUpperInvariant() + $full.Substring(1)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes("native-next") +
        [byte]0 +
        [System.Text.Encoding]::UTF8.GetBytes("\\?\" + $full)
    $digest = [System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes)
    $suffix = -join ($digest[0..7] | ForEach-Object { $_.ToString("x2") })
    return "native-next-$suffix"
}

function Get-RunningDevHost {
    $hostPath = $LiveHostExe.ToLowerInvariant()
    return @(Get-CimInstance Win32_Process -Filter "Name = 'devmanager-host.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath.ToLowerInvariant() -eq $hostPath })
}

# The shell nulls its child host's stderr and still opens a degraded window when
# the host dies, so a failed host looks like a working launch. Re-run the host
# directly to surface the error the shell swallowed.
function Show-HostStartupFailure {
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $LiveHostExe
    $startInfo.WorkingDirectory = $RepoRoot
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardError = $true
    $startInfo.Arguments = '--profile {0} --instance-label "Dev Smoke Probe" --parent-pid {1} --foreground --config-base "{2}"' -f
        (Get-WorkspaceProfileName), $PID, $NativeProfileBase

    $probe = [System.Diagnostics.Process]::Start($startInfo)
    if ($probe.WaitForExit(10000)) {
        $stderr = $probe.StandardError.ReadToEnd().Trim()
        Write-Status ("Host probe exited with code {0}." -f $probe.ExitCode) "error"
        if ($stderr) {
            Write-Status ("Host reported: {0}" -f $stderr) "error"
        }
        return
    }

    Stop-Process -Id $probe.Id -Force -ErrorAction SilentlyContinue
    Write-Status "Host probe started cleanly on its own; the shell may have exceeded its startup budget." "warn"
}

function Wait-ForDevHost {
    param([int]$TimeoutMs = 15000)

    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        $running = @(Get-RunningDevHost)
        if ($running.Count -gt 0) {
            Write-Status ("Durable host attached (pid {0})." -f $running[0].ProcessId) "success"
            return $true
        }
        if ($script:AppProcess -and $script:AppProcess.HasExited) {
            return $false
        }
        Start-Sleep -Milliseconds 250
    }

    return $false
}

function Invoke-BuildAndRelaunch {
    param([string]$Reason)

    Write-Status ("Building because {0} changed..." -f $Reason) "build"

    $cargoArgs = @(
        "build",
        "--locked",
        "--target-dir", $BuildTargetDir,
        "--bin", "devmanager",
        "--bin", "devmanager-host"
    )
    if ($Release) {
        $cargoArgs += "--release"
    }

    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Status "Build failed. Keeping the current app window running." "error"
        return $false
    }

    foreach ($requiredBinary in @($BuildExe, $BuildHostExe)) {
        if (-not (Test-Path $requiredBinary)) {
            throw ("Build succeeded but no executable was found at {0}." -f $requiredBinary)
        }
    }

    New-Item -ItemType Directory -Path $LiveDir -Force | Out-Null
    Initialize-DevProfileStorage

    Stop-ManagedApp
    Stop-StaleLiveCopies
    Wait-ForFileUnlock -Path $LiveExe
    Wait-ForFileUnlock -Path $LiveHostExe

    Copy-Item $BuildExe $LiveExe -Force
    Copy-Item $BuildHostExe $LiveHostExe -Force
    if (Test-Path $BuildPdb) {
        Copy-Item $BuildPdb $LivePdb -Force
    }
    if (Test-Path $BuildHostPdb) {
        Copy-Item $BuildHostPdb $LiveHostPdb -Force
    }

    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $LiveExe
    $startInfo.WorkingDirectory = $RepoRoot
    $startInfo.UseShellExecute = $false
    foreach ($key in @(
        "DEVMANAGER_PROFILE",
        "DEVMANAGER_RUNTIME_KIND",
        "DEVMANAGER_CONFIG_DIR",
        "DEVMANAGER_APP_IDENTITY"
    )) {
        $startInfo.EnvironmentVariables.Remove($key)
    }
    $startInfo.EnvironmentVariables["DEVMANAGER_INSTANCE_LABEL"] = $DevManagerLabel
    $script:AppProcess = [System.Diagnostics.Process]::Start($startInfo)
    if ($script:AppProcess.WaitForExit(3000)) {
        $exitCode = $script:AppProcess.ExitCode
        $message = "DevManager Dev Smoke exited during startup with code $exitCode."
        Set-Content -LiteralPath $LaunchStatus -Value ("{0:u} FAIL {1}" -f (Get-Date), $message)
        Write-Status $message "error"
        $script:AppProcess = $null
        return $false
    }
    Set-Content -LiteralPath $LaunchStatus -Value ("{0:u} RUNNING pid={1}" -f (Get-Date), $script:AppProcess.Id)
    Write-Status ("Launched DevManager Dev Smoke from target-live-dev (pid {0}) with its sibling host." -f $script:AppProcess.Id) "success"

    if (-not (Wait-ForDevHost)) {
        $message = "DevManager Dev Smoke opened without a durable host; the window will be degraded."
        Set-Content -LiteralPath $LaunchStatus -Value ("{0:u} DEGRADED {1}" -f (Get-Date), $message)
        Write-Status $message "error"
        Show-HostStartupFailure
        return $false
    }

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
Write-Status "Builds go to target-watch/ and the running app comes from target-live-dev/ to avoid Windows locking." "info"
Write-Status "The hot-reload app uses its generated workspace-bound profile and never reuses the installed app profile." "info"

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
