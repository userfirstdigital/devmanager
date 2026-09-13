# RED/GREEN fixture tests for final-release typed command outcomes.
# These tests extract helpers from Invoke-FinalReleaseGate.ps1 and never run cargo/npm.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'Support.ps1')
. (Join-Path $PSScriptRoot '..\Isolation.ps1')

$gatePath = Get-NativeNextScriptPath -Leaf 'Invoke-FinalReleaseGate.ps1'
$gateFunctionNames = @(Get-TopLevelScriptFunctions -LiteralPath $gatePath | ForEach-Object { [string]$_.Name })
$requiredExisting = @('Resolve-FinalReleaseTypedResult', 'Resolve-FinalReleaseOverallStatus', 'New-FinalReleaseCommandRecord')
. ([scriptblock]::Create((Get-NamedFunctionSource -LiteralPath $gatePath -Names $requiredExisting)))
$hasRequiresTyped = $gateFunctionNames -contains 'Test-FinalReleaseCommandRequiresTypedResult'
$hasCommandOutcome = $gateFunctionNames -contains 'Resolve-FinalReleaseCommandOutcome'
Assert-Contract `
    -Name 'gate-exposes-requires-typed-helper' `
    -Condition $hasRequiresTyped `
    -Message 'Test-FinalReleaseCommandRequiresTypedResult is missing from Invoke-FinalReleaseGate.ps1.'
Assert-Contract `
    -Name 'gate-exposes-command-outcome-helper' `
    -Condition $hasCommandOutcome `
    -Message 'Resolve-FinalReleaseCommandOutcome is missing from Invoke-FinalReleaseGate.ps1.'
if ($hasRequiresTyped) {
    . ([scriptblock]::Create((Get-NamedFunctionSource -LiteralPath $gatePath -Names @('Test-FinalReleaseCommandRequiresTypedResult'))))
}
if ($hasCommandOutcome) {
    . ([scriptblock]::Create((Get-NamedFunctionSource -LiteralPath $gatePath -Names @('Resolve-FinalReleaseCommandOutcome'))))
}

function Test-OutcomeStatus {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter(Mandatory = $true)][string]$Kind,
        [object]$ExitCode,
        [string]$Stdout,
        [Parameter(Mandatory = $true)][string]$ExpectedStatus,
        [string]$ExpectedReasonContains
    )

    if (-not (Get-Command -Name Resolve-FinalReleaseCommandOutcome -ErrorAction SilentlyContinue)) {
        Assert-Contract -Name $Name -Condition $false -Message 'Resolve-FinalReleaseCommandOutcome is not defined.'
        return
    }
    $outcome = Resolve-FinalReleaseCommandOutcome -Id $Id -Kind $Kind -ExitCode $ExitCode -Stdout $Stdout
    $ok = ([string]$outcome.status -eq $ExpectedStatus)
    $message = "expected status $ExpectedStatus, got '$($outcome.status)' reason='$($outcome.reason)' typedStatus='$($outcome.typedStatus)'"
    if ($ok -and -not [string]::IsNullOrWhiteSpace($ExpectedReasonContains)) {
        $ok = ([string]$outcome.reason -like ("*{0}*" -f $ExpectedReasonContains))
        if (-not $ok) {
            $message = "expected reason to contain '$ExpectedReasonContains', got '$($outcome.reason)'"
        }
    }
    if ($ok -and $null -ne $ExitCode) {
        $ok = ([int]$outcome.exitCode -eq [int]$ExitCode)
        if (-not $ok) {
            $message = "expected exitCode $ExitCode, got '$($outcome.exitCode)'"
        }
    }
    Assert-Contract -Name $Name -Condition $ok -Message $message
}

$passJson = '{"schemaVersion":1,"status":"PASS","disposition":"PASS","pass":true,"reason":"synthetic-pass"}'
$holdJson = '{"schemaVersion":1,"status":"HOLD","disposition":"HOLD","pass":false,"reason":"synthetic-hold"}'
$failJson = '{"schemaVersion":1,"status":"FAIL","disposition":"FAIL","pass":false,"reason":"synthetic-fail"}'

if (Get-Command -Name Test-FinalReleaseCommandRequiresTypedResult -ErrorAction SilentlyContinue) {
    Assert-Contract `
        -Name 'typed-smokes-require-typed-result' `
        -Condition (
            (Test-FinalReleaseCommandRequiresTypedResult -Id 'browser-surface-proof' -Kind 'smoke') -and
            (Test-FinalReleaseCommandRequiresTypedResult -Id 'browser-provider-e2e' -Kind 'smoke') -and
            (Test-FinalReleaseCommandRequiresTypedResult -Id 'provider-smoke' -Kind 'smoke') -and
            (Test-FinalReleaseCommandRequiresTypedResult -Id 'prompt-smoke' -Kind 'smoke') -and
            (Test-FinalReleaseCommandRequiresTypedResult -Id 'prompt-smoke:Invoke-PromptLibrarySmoke' -Kind 'smoke') -and
            -not (Test-FinalReleaseCommandRequiresTypedResult -Id 'cargo-check' -Kind 'mandatory') -and
            -not (Test-FinalReleaseCommandRequiresTypedResult -Id 'web-test' -Kind 'web')
        ) `
        -Message 'browser/provider/prompt smokes must require typed results; cargo/web must not.'
    Assert-Contract `
        -Name 'workspace-smoke-is-not-typed-contract' `
        -Condition (
            -not (Test-FinalReleaseCommandRequiresTypedResult -Id 'workspace-smoke' -Kind 'smoke') -and
            -not (Test-FinalReleaseCommandRequiresTypedResult -Id 'workspace-smoke:Invoke-WorkspaceSmoke' -Kind 'smoke')
        ) `
        -Message 'workspace-smoke must keep marker/exit classification and must not require typed JSON.'
}
else {
    Assert-Contract `
        -Name 'typed-smokes-require-typed-result' `
        -Condition $false `
        -Message 'Test-FinalReleaseCommandRequiresTypedResult is not defined.'
    Assert-Contract `
        -Name 'workspace-smoke-is-not-typed-contract' `
        -Condition $false `
        -Message 'Test-FinalReleaseCommandRequiresTypedResult is not defined.'
}

Test-OutcomeStatus `
    -Name 'typed-pass-nonzero-exit-is-fail' `
    -Id 'browser-surface-proof' `
    -Kind 'smoke' `
    -ExitCode 1 `
    -Stdout $passJson `
    -ExpectedStatus 'FAIL' `
    -ExpectedReasonContains 'typed-pass-nonzero-exit'

Test-OutcomeStatus `
    -Name 'typed-smoke-exit0-without-typed-result-is-not-pass' `
    -Id 'browser-surface-proof' `
    -Kind 'smoke' `
    -ExitCode 0 `
    -Stdout 'Browser surface proof stage Red wrote C:\Temp\example.json' `
    -ExpectedStatus 'FAIL'

Test-OutcomeStatus `
    -Name 'typed-smoke-empty-output-exit0-is-not-pass' `
    -Id 'browser-provider-e2e' `
    -Kind 'smoke' `
    -ExitCode 0 `
    -Stdout '' `
    -ExpectedStatus 'FAIL'

Test-OutcomeStatus `
    -Name 'malformed-typed-output-is-fail' `
    -Id 'browser-provider-e2e' `
    -Kind 'smoke' `
    -ExitCode 0 `
    -Stdout '{status:PASS' `
    -ExpectedStatus 'FAIL'

Test-OutcomeStatus `
    -Name 'typed-hold-exit0-is-hold' `
    -Id 'browser-surface-proof' `
    -Kind 'smoke' `
    -ExitCode 0 `
    -Stdout $holdJson `
    -ExpectedStatus 'HOLD' `
    -ExpectedReasonContains 'typed-hold'

Test-OutcomeStatus `
    -Name 'typed-fail-is-fail' `
    -Id 'provider-smoke' `
    -Kind 'smoke' `
    -ExitCode 1 `
    -Stdout $failJson `
    -ExpectedStatus 'FAIL'

Test-OutcomeStatus `
    -Name 'workspace-smoke-exit0-with-markers-is-pass' `
    -Id 'workspace-smoke' `
    -Kind 'smoke' `
    -ExitCode 0 `
    -Stdout "WORKSPACE_SMOKE_OK`nresidue=0`nCLEANED=exact-identity" `
    -ExpectedStatus 'PASS'

Test-OutcomeStatus `
    -Name 'workspace-smoke-nonzero-exit-is-fail' `
    -Id 'workspace-smoke' `
    -Kind 'smoke' `
    -ExitCode 1 `
    -Stdout 'workspace conformance failed' `
    -ExpectedStatus 'FAIL' `
    -ExpectedReasonContains 'nonzero-exit'

Test-OutcomeStatus `
    -Name 'cargo-exit0-without-typed-result-is-pass' `
    -Id 'cargo-check' `
    -Kind 'mandatory' `
    -ExitCode 0 `
    -Stdout 'Finished `dev` profile [unoptimized + debuginfo] target(s)' `
    -ExpectedStatus 'PASS'

Test-OutcomeStatus `
    -Name 'web-exit0-without-typed-result-is-pass' `
    -Id 'web-test' `
    -Kind 'web' `
    -ExitCode 0 `
    -Stdout 'Test Files  1 passed (1)' `
    -ExpectedStatus 'PASS'

Test-OutcomeStatus `
    -Name 'cargo-nonzero-exit-is-fail' `
    -Id 'cargo-check' `
    -Kind 'mandatory' `
    -ExitCode 101 `
    -Stdout 'error: could not compile' `
    -ExpectedStatus 'FAIL' `
    -ExpectedReasonContains 'nonzero-exit'

if (Get-Command -Name Resolve-FinalReleaseCommandOutcome -ErrorAction SilentlyContinue) {
    $holdOutcome = Resolve-FinalReleaseCommandOutcome -Id 'browser-provider-e2e' -Kind 'smoke' -ExitCode 0 -Stdout $holdJson
    Assert-Contract `
        -Name 'command-outcome-retains-exit-and-typed-fields' `
        -Condition (
            ($null -ne $holdOutcome.PSObject.Properties['exitCode']) -and
            ([int]$holdOutcome.exitCode -eq 0) -and
            ([string]$holdOutcome.typedStatus -eq 'HOLD') -and
            (-not [string]::IsNullOrWhiteSpace([string]$holdOutcome.typedReason)) -and
            ([string]$holdOutcome.status -eq 'HOLD')
        ) `
        -Message 'outcome must retain exitCode plus typed status/reason.'
}
else {
    Assert-Contract `
        -Name 'command-outcome-retains-exit-and-typed-fields' `
        -Condition $false `
        -Message 'Resolve-FinalReleaseCommandOutcome is not defined.'
}

$record = New-FinalReleaseCommandRecord -Id 'browser-surface-proof' -Kind 'smoke' -Status 'HOLD' -ExitCode 0 -Reason 'typed-hold'
Assert-Contract `
    -Name 'command-record-has-exit-status-reason' `
    -Condition (
        ([string]$record.status -eq 'HOLD') -and
        ([int]$record.exitCode -eq 0) -and
        ([string]$record.reason -eq 'typed-hold')
    ) `
    -Message 'command record must retain status, exitCode, and reason.'

$overallHold = Resolve-FinalReleaseOverallStatus `
    -CommandRecords @([pscustomobject]@{ status = 'PASS' }, [pscustomobject]@{ status = 'HOLD' }) `
    -IsPlanOnly $false `
    -WebSkipped $false `
    -SmokesSkipped $false `
    -ProductionAssert 'unchanged' `
    -FinalResidue @() `
    -GateError $null `
    -EvidenceWriteFailed $false
Assert-Contract `
    -Name 'overall-gate-hold-when-typed-hold-present' `
    -Condition ($overallHold -eq 'HOLD') `
    -Message "expected overall HOLD, got '$overallHold'"

$mixedHostAndJson = @"
progress line
$holdJson
"@
$parsed = Resolve-FinalReleaseTypedResult -Stdout $mixedHostAndJson -ExitCode 0
Assert-Contract `
    -Name 'typed-result-distinguishes-json-from-ordinary-output' `
    -Condition (([bool]$parsed.typed) -and ([string]$parsed.status -eq 'HOLD')) `
    -Message "expected typed HOLD from trailing JSON, got typed=$($parsed.typed) status='$($parsed.status)'"

Complete-ContractTests -Suite 'FinalReleaseTypedContract'
