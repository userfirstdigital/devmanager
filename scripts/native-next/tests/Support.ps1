# Shared helpers for focused native-next typed-contract tests.
# Tests only: never launches cargo, npm, WebView2, DevManager, or stock providers.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:NativeNextRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$script:WorktreeRoot = [System.IO.Path]::GetFullPath((Join-Path $script:NativeNextRoot '..\..'))
$script:TestFailures = New-Object System.Collections.Generic.List[string]
$script:TestPasses = New-Object System.Collections.Generic.List[string]

function Get-NativeNextScriptPath {
    param([Parameter(Mandatory = $true)][string]$Leaf)
    return [System.IO.Path]::GetFullPath((Join-Path $script:NativeNextRoot $Leaf))
}

function Get-TopLevelScriptFunctions {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)

    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile(
        $LiteralPath,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($null -eq $ast) {
        throw "Parser returned no AST for '$LiteralPath'."
    }
    if (@($parseErrors).Count -gt 0) {
        $detail = ($parseErrors | ForEach-Object { $_.ToString() }) -join '; '
        throw "Parse errors in '$LiteralPath': $detail"
    }
    $found = [System.Collections.Generic.List[object]]::new()
    foreach ($node in @($ast.FindAll({
                    $args[0] -is [System.Management.Automation.Language.FunctionDefinitionAst]
                }, $true))) {
        $ancestor = $node.Parent
        $insideOtherFunction = $false
        while ($null -ne $ancestor) {
            if ($ancestor -is [System.Management.Automation.Language.FunctionDefinitionAst]) {
                $insideOtherFunction = $true
                break
            }
            $ancestor = $ancestor.Parent
        }
        if (-not $insideOtherFunction) {
            $found.Add($node)
        }
    }
    return $found.ToArray()
}

function Get-NamedFunctionSource {
    param(
        [Parameter(Mandatory = $true)][string]$LiteralPath,
        [Parameter(Mandatory = $true)][string[]]$Names
    )

    $functions = @(Get-TopLevelScriptFunctions -LiteralPath $LiteralPath)
    $blocks = New-Object System.Collections.Generic.List[string]
    foreach ($name in @($Names)) {
        $match = @($functions | Where-Object { [string]$_.Name -eq $name })
        if ($match.Count -eq 0) {
            throw "Function '$name' was not found in '$LiteralPath'."
        }
        [void]$blocks.Add([string]$match[0].Extent.Text)
    }
    return (($blocks.ToArray()) -join "`r`n`r`n")
}

function Get-LastTypedJsonObject {
    param([AllowNull()][string]$Text)

    if ([string]::IsNullOrWhiteSpace($Text)) {
        return $null
    }
    try {
        return ($Text.Trim() | ConvertFrom-Json -ErrorAction Stop)
    }
    catch {
    }
    $lines = @(
        $Text -split "`r?`n" |
            ForEach-Object { $_.Trim() } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    if ($lines.Count -eq 0) {
        return $null
    }
    try {
        return ($lines[-1] | ConvertFrom-Json -ErrorAction Stop)
    }
    catch {
        return $null
    }
}

function New-ContractTestTempRoot {
    $root = [System.IO.Path]::GetFullPath((Join-Path 'C:\Temp' ('devmanager-final-e2e-contract-{0}' -f [guid]::NewGuid().ToString('N'))))
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    return $root
}

function Remove-ContractTestTempRoot {
    param([string]$LiteralPath)
    if ([string]::IsNullOrWhiteSpace($LiteralPath)) { return }
    $full = [System.IO.Path]::GetFullPath($LiteralPath)
    if (-not $full.StartsWith('C:\Temp\devmanager-final-e2e-contract-', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a non-test temp root: '$full'."
    }
    if (Test-Path -LiteralPath $full) {
        Remove-Item -LiteralPath $full -Recurse -Force
    }
}

function Invoke-NativeNextScriptCapture {
    param(
        [Parameter(Mandatory = $true)][string]$ScriptPath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [hashtable]$Environment
    )

    $pwsh = [System.IO.Path]::GetFullPath((Join-Path $PSHome 'pwsh.exe'))
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $pwsh
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.WorkingDirectory = $script:WorktreeRoot
    [void]$startInfo.ArgumentList.Add('-NoProfile')
    [void]$startInfo.ArgumentList.Add('-NonInteractive')
    [void]$startInfo.ArgumentList.Add('-File')
    [void]$startInfo.ArgumentList.Add($ScriptPath)
    foreach ($argument in @($Arguments)) {
        [void]$startInfo.ArgumentList.Add([string]$argument)
    }
    if ($null -ne $Environment) {
        foreach ($key in @($Environment.Keys)) {
            $startInfo.Environment[[string]$key] = [string]$Environment[$key]
        }
    }
    $proc = [System.Diagnostics.Process]::Start($startInfo)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    if (-not $proc.WaitForExit(120000)) {
        try { $proc.Kill() } catch { }
        throw "Timed out waiting for '$ScriptPath'."
    }
    return [pscustomobject]@{
        ExitCode = [int]$proc.ExitCode
        Stdout   = [string]$stdout
        Stderr   = [string]$stderr
    }
}

function Assert-Contract {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][bool]$Condition,
        [Parameter(Mandatory = $true)][string]$Message
    )

    if ($Condition) {
        $script:TestPasses.Add($Name)
        Write-Host ("PASS {0}" -f $Name)
        return
    }
    $script:TestFailures.Add(('{0}: {1}' -f $Name, $Message))
    Write-Host ("FAIL {0}: {1}" -f $Name, $Message)
}

function Complete-ContractTests {
    param([Parameter(Mandatory = $true)][string]$Suite)
    Write-Host ('')
    Write-Host ('{0}: {1} passed, {2} failed' -f $Suite, @($script:TestPasses).Count, @($script:TestFailures).Count)
    if (@($script:TestFailures).Count -gt 0) {
        foreach ($failure in @($script:TestFailures)) {
            Write-Host ("  - {0}" -f $failure)
        }
        exit 1
    }
    exit 0
}
