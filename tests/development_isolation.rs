use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use devmanager::config::paths::{resolve_app_paths, AppProfile, BuildKind};

#[test]
fn native_next_profile_cannot_alias_production() {
    let base = std::path::Path::new(r"C:\Users\tester\AppData\Roaming");
    let production = resolve_app_paths(base, AppProfile::Production, BuildKind::Release).unwrap();
    let next = resolve_app_paths(
        base,
        AppProfile::named("native-next-dev").unwrap(),
        BuildKind::Debug,
    )
    .unwrap();

    assert_eq!(production.root, base.join("com.userfirst.devmanager"));
    assert_eq!(
        next.root,
        base.join("com.userfirst.devmanager-native-next-dev")
    );
    assert!(!next.root.starts_with(&production.root));
    assert_eq!(next.database, next.root.join("kernel.sqlite3"));
    assert_eq!(next.browser_root, next.root.join("browser"));
}

#[test]
fn named_profile_rejects_empty_or_path_shaped_values() {
    for invalid in ["", "..", r"a\b", "a/b", "native next"] {
        assert!(AppProfile::named(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn isolation_scripts_protect_only_the_unprofiled_installation() {
    let library = fs::read_to_string("scripts/native-next/Isolation.ps1").unwrap();
    assert!(library.contains("Get-DevManagerProductionState"));
    assert!(library.contains("config.json"));
    assert!(library.contains("remote.json"));
    assert!(!library.contains("Get-FileHash $sessionPath"));
    assert!(library.contains("Win32_Process"));
    assert!(library.contains("CreationDate"));

    assert!(
        Path::new("scripts/native-next/Capture-ProductionBaseline.ps1").is_file(),
        "missing Capture-ProductionBaseline.ps1"
    );
    assert!(
        Path::new("scripts/native-next/Assert-ProductionUnchanged.ps1").is_file(),
        "missing Assert-ProductionUnchanged.ps1"
    );
}

#[test]
fn production_entry_wrappers_expose_only_evidence_path_parameters() {
    let script = r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Get-PublicParamNames([string]$Path) {
    $tokens = $null
    $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    if ($errors -and $errors.Count -gt 0) {
        throw ("parse errors for {0}: {1}" -f $Path, (($errors | ForEach-Object { $_.ToString() }) -join '; '))
    }
    if ($null -eq $ast.ParamBlock) {
        throw "missing param block: $Path"
    }
    return @($ast.ParamBlock.Parameters | ForEach-Object { $_.Name.VariablePath.UserPath })
}

$capture = @(Get-PublicParamNames 'scripts/native-next/Capture-ProductionBaseline.ps1')
$assert = @(Get-PublicParamNames 'scripts/native-next/Assert-ProductionUnchanged.ps1')

if ($capture.Count -ne 1 -or $capture[0] -ne 'OutputPath') {
    throw ("Capture params must be exactly OutputPath; got: {0}" -f ($capture -join ','))
}
if ($assert.Count -ne 1 -or $assert[0] -ne 'BaselinePath') {
    throw ("Assert params must be exactly BaselinePath; got: {0}" -f ($assert -join ','))
}

Write-Output 'WRAPPER_PARAMS_OK'
"#;

    let output = run_pwsh(script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WRAPPER_PARAMS_OK"),
        "missing success marker"
    );
}

#[test]
fn isolation_evidence_argument_resolver_uses_worktree_not_process_cwd() {
    let fixture = SyntheticIsolationFixture::create();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. '{isolation}'

$worktree = Join-Path '{evidence}' 'resolver-worktree'
$scriptRoot = Join-Path $worktree 'scripts\native-next'
$productionRoot = Join-Path '{evidence}' 'resolver-protected'
$allowedEvidence = Join-Path $worktree '.devmanager-next\evidence'
New-Item -ItemType Directory -Force -Path $scriptRoot | Out-Null
New-Item -ItemType Directory -Force -Path $allowedEvidence | Out-Null
New-Item -ItemType Directory -Force -Path $productionRoot | Out-Null

# Poison process cwd so a cwd-relative join would land elsewhere.
$poison = Join-Path '{evidence}' 'poison-cwd'
New-Item -ItemType Directory -Force -Path $poison | Out-Null
Set-Location -LiteralPath $poison

$relative = '.devmanager-next/evidence/current/baseline.json'
if (Get-Command Resolve-DevManagerEvidenceArgument -ErrorAction SilentlyContinue) {{
    $resolved = Resolve-DevManagerEvidenceArgument -Path $relative -ScriptRoot $scriptRoot
}} else {{
    throw 'Resolve-DevManagerEvidenceArgument is missing'
}}

$expected = [System.IO.Path]::GetFullPath((Join-Path $worktree '.devmanager-next\evidence\current\baseline.json'))
$normalizedResolved = Normalize-DevManagerPath -LiteralPath $resolved
$normalizedExpected = Normalize-DevManagerPath -LiteralPath $expected
if ($normalizedResolved -ne $normalizedExpected) {{
    throw "relative resolve mismatch: got='$resolved' expected='$expected' (cwd=$(Get-Location))"
}}
if (-not [System.IO.Path]::IsPathRooted($resolved)) {{
    throw "resolved path must be absolute: $resolved"
}}

$absoluteInput = $expected
$absoluteResolved = Resolve-DevManagerEvidenceArgument -Path $absoluteInput -ScriptRoot $scriptRoot
if ((Normalize-DevManagerPath -LiteralPath $absoluteResolved) -ne $normalizedExpected) {{
    throw "absolute path must stay stable: got='$absoluteResolved' expected='$expected'"
}}

$escapeResolved = Resolve-DevManagerEvidenceArgument -Path '..\outside.json' -ScriptRoot $scriptRoot
if (-not [System.IO.Path]::IsPathRooted($escapeResolved)) {{
    throw "escape path must still resolve to an absolute path: $escapeResolved"
}}
$escapeRejected = $false
try {{
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $escapeResolved `
        -ProtectedProductionRoot $productionRoot `
        -AllowedEvidenceRoot $allowedEvidence
}} catch {{
    $escapeRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'evidence|worktree|allowed|outside') {{
        throw "unexpected escape rejection: $($_.Exception.Message)"
    }}
}}
if (-not $escapeRejected) {{
    throw 'expected ..\outside.json to resolve then fail the allowed-root guard'
}}

Write-Output 'EVIDENCE_RESOLVER_OK'
"#,
        isolation = ps_literal(&fixture.isolation_ps1),
        evidence = ps_literal(&fixture.evidence_dir),
    );

    let output = run_pwsh(&script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("EVIDENCE_RESOLVER_OK"),
        "missing success marker"
    );
}

#[test]
fn isolation_guards_reject_coercive_types_and_non_fully_qualified_paths() {
    let fixture = SyntheticIsolationFixture::create();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. '{isolation}'

$productionRoot = '{root}'
$installExe = '{install}'

$valid = Get-DevManagerProductionState `
    -ProductionRoot $productionRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses @(
        [pscustomobject]@{{
            ProcessId = [uint32]4242
            ExecutablePath = $installExe
            CreationDate = '20260101120000.000000-000'
            Name = 'devmanager.exe'
        }}
    )

function Expect-ShapeRejection([scriptblock]$Mutate, [string]$Pattern) {{
    $probe = $valid | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    & $Mutate $probe
    $rejected = $false
    try {{
        Assert-DevManagerEvidenceShape -Evidence $probe -Label 'probe'
    }} catch {{
        $rejected = $true
        if ("$($_.Exception.Message)" -notmatch $Pattern) {{
            throw "unexpected shape error (wanted /$Pattern/): $($_.Exception.Message)"
        }}
    }}
    if (-not $rejected) {{
        throw "expected malformed rejection matching /$Pattern/"
    }}
}}

# Coercive numeric / bool values must not pass.
Expect-ShapeRejection {{ param($e) $e.schemaVersion = $true }} 'schemaVersion'
Expect-ShapeRejection {{ param($e) $e.schemaVersion = '1' }} 'schemaVersion'
Expect-ShapeRejection {{ param($e) $e.config.length = '12' }} 'length'
Expect-ShapeRejection {{ param($e) $e.config.length = 1.5 }} 'length'
Expect-ShapeRejection {{ param($e) $e.installedProcesses[0].processId = '4242' }} 'processId'
Expect-ShapeRejection {{ param($e) $e.installedProcesses[0].processId = $true }} 'processId'

# Null / scalar installedProcesses must not normalize to an empty/valid inventory.
Expect-ShapeRejection {{ param($e) $e.installedProcesses = $null }} 'installedProcesses|null|array'
Expect-ShapeRejection {{
    param($e)
    $e.installedProcesses = [pscustomobject]@{{
        processId = [int]4242
        executablePath = $installExe
        creationDate = '20260101120000.000000-000'
    }}
}} 'installedProcesses|array|scalar'

# Zero- and one-element arrays remain valid through live state and JSON roundtrip.
$emptyLive = Get-DevManagerProductionState `
    -ProductionRoot $productionRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses @()
Assert-DevManagerEvidenceShape -Evidence $emptyLive -Label 'empty-live'
$emptyRoundTrip = $emptyLive | ConvertTo-Json -Depth 8 | ConvertFrom-Json
Assert-DevManagerEvidenceShape -Evidence $emptyRoundTrip -Label 'empty-json'
Assert-DevManagerEvidenceShape -Evidence $valid -Label 'one-live'
$oneRoundTrip = $valid | ConvertTo-Json -Depth 8 | ConvertFrom-Json
Assert-DevManagerEvidenceShape -Evidence $oneRoundTrip -Label 'one-json'
if ($null -eq $oneRoundTrip.installedProcesses) {{ throw 'one-element JSON inventory became null' }}
if ($oneRoundTrip.installedProcesses -is [System.Management.Automation.PSCustomObject]) {{
    throw 'one-element JSON inventory collapsed to scalar PSCustomObject'
}}

# Drive-relative paths must fail at the guard boundary / normalizer.
if (Test-DevManagerAbsolutePath -LiteralPath 'C:relative') {{
    throw 'C:relative must not count as fully qualified'
}}
$normalizeRejected = $false
try {{
    $null = Normalize-DevManagerPath -LiteralPath 'C:relative'
}} catch {{
    $normalizeRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'fully.?qualified|relative|normalize|identity') {{
        throw "unexpected C:relative normalize error: $($_.Exception.Message)"
    }}
}}
if (-not $normalizeRejected) {{ throw 'expected Normalize-DevManagerPath to reject C:relative' }}
Expect-ShapeRejection {{ param($e) $e.productionRoot = 'C:relative' }} 'productionRoot|fully.?qualified|absolute'

# Relative APPDATA / production root must fail closed.
$appDataRejected = $false
try {{
    $null = Get-DevManagerProductionRoot -AppDataRoot 'relative-appdata'
}} catch {{
    $appDataRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'APPDATA|fully.?qualified|absolute') {{
        throw "unexpected relative APPDATA error: $($_.Exception.Message)"
    }}
}}
if (-not $appDataRejected) {{ throw 'expected Get-DevManagerProductionRoot to reject relative APPDATA' }}

# Supported install roots: required defaults must not be silently omitted; malformed values fail.
$missingLocalRejected = $false
try {{
    $null = Get-DevManagerSupportedInstallPaths `
        -LocalAppDataRoot '' `
        -ProgramFilesRoot 'C:\Program Files' `
        -ProgramFilesX86Root ''
}} catch {{
    $missingLocalRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'LOCALAPPDATA|LocalAppData|required|missing') {{
        throw "unexpected missing LocalAppData error: $($_.Exception.Message)"
    }}
}}
if (-not $missingLocalRejected) {{ throw 'expected missing LocalAppData to fail' }}

$malformedPfRejected = $false
try {{
    $null = Get-DevManagerSupportedInstallPaths `
        -LocalAppDataRoot 'C:\Users\tester\AppData\Local' `
        -ProgramFilesRoot 'relative-program-files' `
        -ProgramFilesX86Root ''
}} catch {{
    $malformedPfRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'ProgramFiles|fully.?qualified|absolute') {{
        throw "unexpected malformed ProgramFiles error: $($_.Exception.Message)"
    }}
}}
if (-not $malformedPfRejected) {{ throw 'expected malformed ProgramFiles to fail' }}

$malformedX86Rejected = $false
try {{
    $null = Get-DevManagerSupportedInstallPaths `
        -LocalAppDataRoot 'C:\Users\tester\AppData\Local' `
        -ProgramFilesRoot 'C:\Program Files' `
        -ProgramFilesX86Root 'relative-x86'
}} catch {{
    $malformedX86Rejected = $true
    if ("$($_.Exception.Message)" -notmatch 'ProgramFiles\(x86\)|x86|fully.?qualified|absolute') {{
        throw "unexpected malformed ProgramFiles(x86) error: $($_.Exception.Message)"
    }}
}}
if (-not $malformedX86Rejected) {{ throw 'expected malformed ProgramFiles(x86) to fail rather than omit' }}

$okPaths = Get-DevManagerSupportedInstallPaths `
    -LocalAppDataRoot 'C:\Users\tester\AppData\Local' `
    -ProgramFilesRoot 'C:\Program Files' `
    -ProgramFilesX86Root ''
if (@($okPaths).Count -lt 2) {{ throw "expected LocalAppData and ProgramFiles paths, got $($okPaths.Count)" }}

Write-Output 'COERCION_PATH_GUARDS_OK'
"#,
        isolation = ps_literal(&fixture.isolation_ps1),
        root = ps_literal(&fixture.production_root),
        install = ps_literal(&fixture.install_exe),
    );

    let output = run_pwsh(&script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("COERCION_PATH_GUARDS_OK"),
        "missing success marker"
    );
}

#[test]
fn isolation_evidence_shape_rejects_materially_malformed_values() {
    let fixture = SyntheticIsolationFixture::create();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. '{isolation}'

$productionRoot = '{root}'
$installExe = '{install}'

$valid = Get-DevManagerProductionState `
    -ProductionRoot $productionRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses @(
        [pscustomobject]@{{
            ProcessId = [uint32]4242
            ExecutablePath = $installExe
            CreationDate = '20260101120000.000000-000'
            Name = 'devmanager.exe'
        }}
    )

function Expect-ShapeRejection([scriptblock]$Mutate, [string]$Pattern) {{
    $probe = $valid | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    & $Mutate $probe
    $rejected = $false
    try {{
        Assert-DevManagerEvidenceShape -Evidence $probe -Label 'probe'
    }} catch {{
        $rejected = $true
        if ("$($_.Exception.Message)" -notmatch $Pattern) {{
            throw "unexpected shape error (wanted /$Pattern/): $($_.Exception.Message)"
        }}
    }}
    if (-not $rejected) {{
        throw "expected malformed rejection matching /$Pattern/"
    }}
}}

Expect-ShapeRejection {{ param($e) $e.capturedAtUtc = '' }} 'capturedAtUtc'
Expect-ShapeRejection {{ param($e) $e.capturedAtUtc = 'not-a-timestamp' }} 'capturedAtUtc'
Expect-ShapeRejection {{ param($e) $e.productionRoot = 'relative-root' }} 'productionRoot|absolute|fully.?qualified|normalize'
Expect-ShapeRejection {{ param($e) $e.sessionPath = (Join-Path $productionRoot 'other.json') }} 'sessionPath'
Expect-ShapeRejection {{ param($e) $e.config.exists = 'yes' }} 'exists'
Expect-ShapeRejection {{ param($e) $e.config.length = -1 }} 'length'
Expect-ShapeRejection {{
    param($e)
    $e.config.exists = $false
    $e.config.length = 12
    $e.config.sha256 = $null
}} 'length|absent|exists'
Expect-ShapeRejection {{
    param($e)
    $e.remote.exists = $false
    $e.remote.length = $null
    $e.remote.sha256 = ('a' * 64)
}} 'sha256|absent|exists'
Expect-ShapeRejection {{ param($e) $e.config.sha256 = 'abc' }} 'sha256|hex'
Expect-ShapeRejection {{ param($e) $e.installedProcesses[0].processId = 0 }} 'processId'
Expect-ShapeRejection {{ param($e) $e.installedProcesses[0].executablePath = 'not-absolute' }} 'executable|normalize|identity|fully.?qualified'
Expect-ShapeRejection {{ param($e) $e.installedProcesses[0].creationDate = '' }} 'creationDate'

# Empty inventory remains schema-valid (zero installed processes allowed).
$empty = $valid | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$empty.installedProcesses = @()
Assert-DevManagerEvidenceShape -Evidence $empty -Label 'empty-ok'

Write-Output 'EVIDENCE_SHAPE_OK'
"#,
        isolation = ps_literal(&fixture.isolation_ps1),
        root = ps_literal(&fixture.production_root),
        install = ps_literal(&fixture.install_exe),
    );

    let output = run_pwsh(&script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("EVIDENCE_SHAPE_OK"),
        "missing success marker"
    );
}

#[test]
fn isolation_evidence_io_rejects_paths_under_protected_root_and_unsafe_wrapper_targets() {
    let fixture = SyntheticIsolationFixture::create();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. '{isolation}'

$productionRoot = '{root}'
$installExe = '{install}'
$evidenceDir = '{evidence}'
$configPath = Join-Path $productionRoot 'config.json'
$sessionPath = Join-Path $productionRoot 'session.json'
$configBytes = [System.IO.File]::ReadAllBytes($configPath)

Set-Content -LiteralPath $sessionPath -Value '{{"sentinel":"do-not-read-me","activeTab":"x"}}' -NoNewline
$sessionBytes = [System.IO.File]::ReadAllBytes($sessionPath)

$state = Get-DevManagerProductionState `
    -ProductionRoot $productionRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses @()

# 1) Write must refuse output under the protected production root; files stay unchanged.
$writeRejected = $false
try {{
    Write-DevManagerBaseline -State $state -OutputPath $configPath
}} catch {{
    $writeRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'protected|production|beneath|evidence') {{
        throw "unexpected write rejection: $($_.Exception.Message)"
    }}
}}
if (-not $writeRejected) {{ throw 'expected Write-DevManagerBaseline to reject production config path' }}

$sessionWriteRejected = $false
try {{
    Write-DevManagerBaseline -State $state -OutputPath $sessionPath
}} catch {{
    $sessionWriteRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'protected|production|beneath|evidence') {{
        throw "unexpected session write rejection: $($_.Exception.Message)"
    }}
}}
if (-not $sessionWriteRejected) {{ throw 'expected Write-DevManagerBaseline to reject production session path' }}

$configAfter = [System.IO.File]::ReadAllBytes($configPath)
$sessionAfter = [System.IO.File]::ReadAllBytes($sessionPath)
if ([System.Convert]::ToBase64String($configBytes) -ne [System.Convert]::ToBase64String($configAfter)) {{
    throw 'config.json bytes changed after rejected write'
}}
if ([System.Convert]::ToBase64String($sessionBytes) -ne [System.Convert]::ToBase64String($sessionAfter)) {{
    throw 'session.json bytes changed after rejected write'
}}

# Component-aware comparison must not treat a sibling prefix collision as beneath.
$siblingRoot = $productionRoot + '-sibling'
New-Item -ItemType Directory -Force -Path $siblingRoot | Out-Null
$siblingFile = Join-Path $siblingRoot 'baseline.json'
if (Test-DevManagerPathEqualsOrBeneath -LiteralPath $siblingFile -AncestorPath $productionRoot) {{
    throw 'string-prefix false positive: sibling path treated as beneath production root'
}}

# 2) Read must refuse baseline paths under protected root before content IO.
$readRejected = $false
try {{
    $null = Read-DevManagerBaseline -BaselinePath $sessionPath -ProtectedProductionRoot $productionRoot
}} catch {{
    $readRejected = $true
    $msg = "$($_.Exception.Message)"
    if ($msg -match 'missing required field|schemaVersion|ConvertFrom-Json|sentinel') {{
        throw "session content was read before path rejection: $msg"
    }}
    if ($msg -notmatch 'protected|production|beneath|evidence') {{
        throw "unexpected read rejection: $msg"
    }}
}}
if (-not $readRejected) {{ throw 'expected Read-DevManagerBaseline to reject protected session path' }}
$sessionAfterRead = [System.IO.File]::ReadAllBytes($sessionPath)
if ([System.Convert]::ToBase64String($sessionBytes) -ne [System.Convert]::ToBase64String($sessionAfterRead)) {{
    throw 'session.json bytes changed after rejected read'
}}

# Pure helper: outside protected root is allowed when no AllowedEvidenceRoot is set.
$safeOut = Join-Path $evidenceDir 'safe-baseline.json'
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $safeOut `
    -ProtectedProductionRoot $productionRoot

# 3) Wrapper evidence policy: must stay under worktree .devmanager-next\evidence.
$worktree = Join-Path $evidenceDir 'fake-worktree'
$allowedEvidence = Join-Path $worktree '.devmanager-next\evidence'
New-Item -ItemType Directory -Force -Path $allowedEvidence | Out-Null
$outside = Join-Path $evidenceDir 'outside-worktree.json'
$outsideRejected = $false
try {{
    Assert-DevManagerEvidencePathSafeForIO `
        -LiteralPath $outside `
        -ProtectedProductionRoot $productionRoot `
        -AllowedEvidenceRoot $allowedEvidence
}} catch {{
    $outsideRejected = $true
    if ("$($_.Exception.Message)" -notmatch 'evidence|worktree|allowed') {{
        throw "unexpected outside-worktree rejection: $($_.Exception.Message)"
    }}
}}
if (-not $outsideRejected) {{ throw 'expected outside-worktree evidence path rejection' }}

$inside = Join-Path $allowedEvidence 'current\baseline.json'
Assert-DevManagerEvidencePathSafeForIO `
    -LiteralPath $inside `
    -ProtectedProductionRoot $productionRoot `
    -AllowedEvidenceRoot $allowedEvidence

# Reparse/junction redirect into protected root must fail closed.
$junction = Join-Path $allowedEvidence 'leak'
$junctionCreated = $false
try {{
    cmd.exe /c "mklink /J `"$junction`" `"$productionRoot`"" | Out-Null
    if (Test-Path -LiteralPath $junction) {{ $junctionCreated = $true }}
}} catch {{
    $junctionCreated = $false
}}

if ($junctionCreated) {{
    $reparseTarget = Join-Path $junction 'session.json'
    $reparseRejected = $false
    try {{
        Assert-DevManagerEvidencePathSafeForIO `
            -LiteralPath $reparseTarget `
            -ProtectedProductionRoot $productionRoot `
            -AllowedEvidenceRoot $allowedEvidence
    }} catch {{
        $reparseRejected = $true
        if ("$($_.Exception.Message)" -notmatch 'reparse|junction|symlink|protected|production|beneath') {{
            throw "unexpected reparse rejection: $($_.Exception.Message)"
        }}
    }}
    if (-not $reparseRejected) {{ throw 'expected reparse/junction evidence path rejection' }}
    cmd.exe /c "rmdir `"$junction`"" | Out-Null
}} else {{
    # Deterministic fallback when junction creation is unavailable: helper detects ReparsePoint attributes.
    $probeFile = Join-Path $evidenceDir 'reparse-probe.txt'
    Set-Content -LiteralPath $probeFile -Value 'x' -NoNewline
    $attrs = [System.IO.File]::GetAttributes($probeFile)
    $hadReparse = ($attrs -band [System.IO.FileAttributes]::ReparsePoint) -ne 0
    try {{
        [System.IO.File]::SetAttributes($probeFile, ($attrs -bor [System.IO.FileAttributes]::ReparsePoint))
        $forcedRejected = $false
        try {{
            Assert-DevManagerPathHasNoReparsePoints -LiteralPath $probeFile
        }} catch {{
            $forcedRejected = $true
            if ("$($_.Exception.Message)" -notmatch 'reparse') {{
                throw "unexpected forced-reparse rejection: $($_.Exception.Message)"
            }}
        }}
        if (-not $forcedRejected) {{ throw 'expected Assert-DevManagerPathHasNoReparsePoints to reject reparse attribute' }}
    }} finally {{
        if (-not $hadReparse) {{
            [System.IO.File]::SetAttributes($probeFile, $attrs)
        }}
    }}
}}

Write-Output 'EVIDENCE_IO_OK'
"#,
        isolation = ps_literal(&fixture.isolation_ps1),
        root = ps_literal(&fixture.production_root),
        install = ps_literal(&fixture.install_exe),
        evidence = ps_literal(&fixture.evidence_dir),
    );

    let output = run_pwsh(&script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("EVIDENCE_IO_OK"),
        "missing success marker"
    );
}

#[test]
fn isolation_library_compares_synthetic_production_state_without_touching_appdata() {
    let fixture = SyntheticIsolationFixture::create();
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. '{isolation}'

$productionRoot = '{root}'
$installExe = '{install}'
$configPath = Join-Path $productionRoot 'config.json'
$remotePath = Join-Path $productionRoot 'remote.json'
$sessionPath = Join-Path $productionRoot 'session.json'
$baselinePath = Join-Path '{evidence}' 'baseline.json'
$mismatchBaselinePath = Join-Path '{evidence}' 'baseline-root-mismatch.json'
$malformedPath = Join-Path '{evidence}' 'baseline-malformed.json'

# session.json must remain unhashed even when present beside protected files.
Set-Content -LiteralPath $sessionPath -Value '{{"activeTab":"ignore-me"}}' -NoNewline

$cim = @(
    [pscustomobject]@{{
        ProcessId = [uint32]4242
        ExecutablePath = $installExe
        CreationDate = '20260101120000.000000-000'
        Name = 'devmanager.exe'
    }},
    [pscustomobject]@{{
        ProcessId = [uint32]9999
        ExecutablePath = (Join-Path $env:TEMP 'unrelated-devmanager.exe')
        CreationDate = '20260101130000.000000-000'
        Name = 'devmanager.exe'
    }}
)

$state = Get-DevManagerProductionState `
    -ProductionRoot $productionRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses $cim

if ($state.schemaVersion -ne 1) {{ throw "schemaVersion=$($state.schemaVersion)" }}
if ($state.productionRoot -ne $productionRoot) {{ throw "productionRoot mismatch" }}
if ($state.sessionPath -ne $sessionPath) {{ throw "sessionPath mismatch" }}
if ($state.config.exists -ne $true) {{ throw "config.exists" }}
if ($state.remote.exists -ne $true) {{ throw "remote.exists" }}
if ([string]::IsNullOrWhiteSpace([string]$state.config.sha256)) {{ throw "config.sha256 missing" }}
if ([string]::IsNullOrWhiteSpace([string]$state.remote.sha256)) {{ throw "remote.sha256 missing" }}
if ($state.installedProcesses.Count -ne 1) {{ throw "expected one installed process, got $($state.installedProcesses.Count)" }}
if ([uint32]$state.installedProcesses[0].processId -ne 4242) {{ throw "unexpected pid" }}
if ($state.PSObject.Properties.Name -contains 'sessionHash') {{ throw "session must not be hashed" }}

$configHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $configPath).Hash.ToLowerInvariant()
$remoteHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $remotePath).Hash.ToLowerInvariant()
if ($state.config.sha256 -ne $configHash) {{ throw "config hash drift" }}
if ($state.remote.sha256 -ne $remoteHash) {{ throw "remote hash drift" }}
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $sessionPath).Hash.ToLowerInvariant() -eq $state.config.sha256) {{
    throw "session content must not leak into config hash"
}}

Write-DevManagerBaseline -State $state -OutputPath $baselinePath
Assert-DevManagerProductionState -BaselinePath $baselinePath -Current $state

# Length / hash mismatch must fail closed.
$badHash = $state | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$badHash.config.sha256 = ('0' * 64)
try {{
    Assert-DevManagerProductionState -BaselinePath $baselinePath -Current $badHash
    throw 'expected hash mismatch to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'hash|sha256|config') {{
        throw "unexpected hash-mismatch error: $($_.Exception.Message)"
    }}
}}

$badLength = $state | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$badLength.remote.length = [int64]($state.remote.length + 1)
try {{
    Assert-DevManagerProductionState -BaselinePath $baselinePath -Current $badLength
    throw 'expected length mismatch to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'length|remote') {{
        throw "unexpected length-mismatch error: $($_.Exception.Message)"
    }}
}}

# Root mismatch must fail closed.
$otherRoot = Join-Path '{evidence}' 'other-root'
New-Item -ItemType Directory -Force -Path $otherRoot | Out-Null
$mismatch = Get-DevManagerProductionState `
    -ProductionRoot $otherRoot `
    -SupportedExecutablePaths @($installExe) `
    -CimProcesses @()
Write-DevManagerBaseline -State $mismatch -OutputPath $mismatchBaselinePath
try {{
    Assert-DevManagerProductionState -BaselinePath $mismatchBaselinePath -Current $state
    throw 'expected root mismatch to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'root') {{
        throw "unexpected root-mismatch error: $($_.Exception.Message)"
    }}
}}

# Missing baseline field / malformed evidence must fail closed.
Set-Content -LiteralPath $malformedPath -Value '{{"schemaVersion":1}}' -NoNewline
try {{
    Assert-DevManagerProductionState -BaselinePath $malformedPath -Current $state
    throw 'expected malformed baseline to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'missing|malformed|required|field') {{
        throw "unexpected malformed-baseline error: $($_.Exception.Message)"
    }}
}}

# PID / executable / start-time mismatch must fail closed.
$badProcess = $state | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$badProcess.installedProcesses[0].processId = [uint32]7777
try {{
    Assert-DevManagerProductionState -BaselinePath $baselinePath -Current $badProcess
    throw 'expected processId mismatch to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'process|pid|ProcessId') {{
        throw "unexpected pid-mismatch error: $($_.Exception.Message)"
    }}
}}

$badStart = $state | ConvertTo-Json -Depth 8 | ConvertFrom-Json
$badStart.installedProcesses[0].creationDate = '19990101000000.000000-000'
try {{
    Assert-DevManagerProductionState -BaselinePath $baselinePath -Current $badStart
    throw 'expected creationDate mismatch to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'creation|start') {{
        throw "unexpected creationDate-mismatch error: $($_.Exception.Message)"
    }}
}}

# Ambiguous / missing executable identity fails closed.
try {{
    $null = Get-DevManagerInstalledProcesses `
        -SupportedExecutablePaths @($installExe) `
        -CimProcesses @(
            [pscustomobject]@{{
                ProcessId = [uint32]1
                ExecutablePath = $null
                CreationDate = '20260101120000.000000-000'
                Name = 'devmanager.exe'
            }}
        )
    throw 'expected missing executable identity to fail'
}} catch {{
    if ("$($_.Exception.Message)" -notmatch 'executable|identity|ambiguous|missing') {{
        throw "unexpected missing-identity error: $($_.Exception.Message)"
    }}
}}

Write-Output 'SYNTHETIC_ISOLATION_OK'
"#,
        isolation = ps_literal(&fixture.isolation_ps1),
        root = ps_literal(&fixture.production_root),
        install = ps_literal(&fixture.install_exe),
        evidence = ps_literal(&fixture.evidence_dir),
    );

    let output = run_pwsh(&script);
    assert!(
        output.status.success(),
        "pwsh failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("SYNTHETIC_ISOLATION_OK"),
        "missing success marker\nstdout:\n{stdout}"
    );

    // Guardrail: synthetic fixtures must never resolve under real production AppData.
    let real_production = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("com.userfirst.devmanager"))
        .expect("APPDATA");
    assert!(!fixture.production_root.starts_with(&real_production));
    assert!(!fixture.evidence_dir.starts_with(&real_production));
}

struct SyntheticIsolationFixture {
    _temp: tempfile::TempDir,
    production_root: PathBuf,
    install_exe: PathBuf,
    evidence_dir: PathBuf,
    isolation_ps1: PathBuf,
}

impl SyntheticIsolationFixture {
    fn create() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let production_root = temp.path().join("com.userfirst.devmanager");
        let install_dir = temp.path().join("Local").join("DevManager");
        let evidence_dir = temp.path().join("evidence");
        fs::create_dir_all(&production_root).expect("production root");
        fs::create_dir_all(&install_dir).expect("install dir");
        fs::create_dir_all(&evidence_dir).expect("evidence dir");

        fs::write(
            production_root.join("config.json"),
            r#"{"theme":"dark","guard":"config-v1"}"#,
        )
        .expect("config.json");
        fs::write(
            production_root.join("remote.json"),
            r#"{"hostId":"remote-v1"}"#,
        )
        .expect("remote.json");
        // Placeholder path for exact-path process matching; never executed.
        let install_exe = install_dir.join("devmanager.exe");
        fs::write(&install_exe, b"synthetic-devmanager-binary").expect("install exe");

        let isolation_ps1 = PathBuf::from("scripts/native-next/Isolation.ps1");
        assert!(
            isolation_ps1.is_file(),
            "Isolation.ps1 must exist for behavior coverage"
        );

        Self {
            _temp: temp,
            production_root,
            install_exe,
            evidence_dir,
            isolation_ps1,
        }
    }
}

fn ps_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\'', "''")
        .replace('/', r"\")
}

fn run_pwsh(script: &str) -> std::process::Output {
    Command::new("pwsh")
        .args(["-NoProfile", "-Command", script])
        .output()
        .expect("failed to spawn pwsh")
}
