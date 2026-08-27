if (-not $env:CURSOR_INVOKED_AS) {
    $env:CURSOR_INVOKED_AS = Split-Path -leaf $MyInvocation.MyCommand.Name
}
$scriptPath = Split-Path -parent $MyInvocation.MyCommand.Definition

function Parse-VersionString {
    param (
        [string]$versionString
    )

    # Split on '-' to remove the commit hash part
    $datePart = $versionString.Split('-')[0]

    # Split on '.' to get year, month, day
    $parts = $datePart.Split('.')

    # Ensure we have exactly 3 parts (year, month, day)
    if ($parts.Length -ne 3) {
        throw "Invalid version format. Expected format: YYYY.MM.DD-commit"
    }

    # Pad month and day to 2 digits if needed, then concatenate
    $year = $parts[0]
    $month = $parts[1].PadLeft(2, '0')
    $day = $parts[2].PadLeft(2, '0')

    # Return as integer for proper numeric comparison
    return [int]($year + $month + $day)
}

## Enable Node.js compile cache for faster CLI startup (requires Node.js >= 22.1.0)
## Cache is automatically invalidated when source files change
if (-not $env:NODE_COMPILE_CACHE) {
    $env:NODE_COMPILE_CACHE = "$env:LOCALAPPDATA\cursor-compile-cache"
}

## Are we somehow in the same dir as the script? Just run it. Look for node.exe
if (Test-Path "$scriptPath\node.exe") {
    & "$scriptPath\node.exe" "$scriptPath\index.js" $args
    exit $LASTEXITCODE
}

## Otherwise, we're in a version directory. Find the latest version.
$versionDir = Get-ChildItem -Path "$scriptPath\versions" -Directory |
    Where-Object {
        $name = $_.Name
        # Check if directory name matches version format. Supports both the
        # legacy YYYY.MM.DD-commit form and the newer
        # YYYY.MM.DD-HH-MM-SS-commit form that adds a build timestamp between
        # the date and the commit hash.
        $name -match '^\d{4}\.\d{1,2}\.\d{1,2}(-\d{2}-\d{2}-\d{2})?-[a-f0-9]+$'
    } |
    Sort-Object { Parse-VersionString $_.Name } -Descending |
    Select-Object -First 1

if (-not $versionDir) {
    Write-Error "No version directories found in $scriptPath"
    exit 1
}

$versionName = $versionDir.Name
$nodePath = "$scriptPath\versions\$versionName\node.exe"

## Run the version's index.js
& "$nodePath" "$scriptPath\versions\$versionName\index.js" $args
exit $LASTEXITCODE
