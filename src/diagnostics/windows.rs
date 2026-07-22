//! Windows-specific discovery helpers. Non-Windows builds keep stubs that compile.

use crate::diagnostics::catalog::{open_docs_repair, winget_install_repair, CatalogEntry};
use crate::diagnostics::model::{
    DiagnosticId, DiagnosticResult, DiagnosticStatus, ProfileRecipe, RepairOperation, RepairPlan,
    RepairRisk,
};
use crate::diagnostics::profile::{
    classify_cc_ast, parse_cc_ast_probe_output, CcClassification, BEGIN_MARKER, END_MARKER,
};
use crate::diagnostics::resolve::collapse_same_directory_installs;
use crate::diagnostics::runner::{CommandOutput, CommandRunner, CommandSpec};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

pub fn is_windows() -> bool {
    cfg!(windows)
}

pub async fn resolve_profile_path<R: CommandRunner + ?Sized>(
    runner: &R,
    pwsh: &Path,
) -> Result<PathBuf, String> {
    let output = runner
        .run(&CommandSpec {
            program: pwsh.to_path_buf(),
            args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from("$PROFILE"),
            ],
            timeout: PROBE_TIMEOUT,
            env: Default::default(),
        })
        .await
        .map_err(|err| err.message)?;
    if output.timed_out {
        return Err("timed out resolving $PROFILE".to_string());
    }
    if output.exit_code != Some(0) {
        return Err(format!(
            "pwsh returned {:?} while resolving $PROFILE",
            output.exit_code
        ));
    }
    let path = output.stdout.lines().next().unwrap_or("").trim();
    if path.is_empty() {
        return Err("$PROFILE was empty".to_string());
    }
    Ok(PathBuf::from(path))
}

pub async fn probe_profile_parse<R: CommandRunner + ?Sized>(
    runner: &R,
    pwsh: &Path,
    profile: &Path,
) -> Result<(), String> {
    let script = profile_parse_script(profile);
    let output = runner
        .run(&CommandSpec {
            program: pwsh.to_path_buf(),
            args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from(script),
            ],
            timeout: PROBE_TIMEOUT,
            env: Default::default(),
        })
        .await
        .map_err(|err| err.message)?;
    if output.timed_out {
        return Err("profile parse timed out".to_string());
    }
    if output.exit_code != Some(0) {
        return Err(sanitize_profile_detail(&output.stderr));
    }
    Ok(())
}

/// PowerShell script for Parser::ParseFile. Both `[ref]` targets must exist first;
/// `[ref]$null` / unbound `$errs` raise InvalidOperation on modern pwsh.
pub fn profile_parse_script(profile: &Path) -> String {
    let path = profile.display().to_string().replace('\'', "''");
    format!(
        "$tokens=$null; $errs=$null; [void][System.Management.Automation.Language.Parser]::ParseFile('{path}', [ref]$tokens, [ref]$errs); if ($errs) {{ $errs | ForEach-Object {{ $_.ToString() }}; exit 1 }}"
    )
}

/// Combined runtime profile load + effective `cc` classification.
/// Dot-sources the profile once, then inspects `Get-Command cc` ScriptBlock.Ast.
/// Emits only fixed KEY=value metadata (never profile body text).
pub fn runtime_profile_probe_script(profile: &Path) -> String {
    let path = profile.display().to_string().replace('\'', "''");
    let begin = BEGIN_MARKER.replace('\'', "''");
    let end = END_MARKER.replace('\'', "''");
    format!(
        r#"
$ErrorActionPreference='Stop'
try {{
  . '{path}' 1>$null 4>$null 5>$null 6>$null
}} catch {{
  [Console]::Error.WriteLine($_.Exception.Message)
  Write-Output 'ERR=load'
  exit 1
}}
$ErrorActionPreference='Continue'
try {{
  $text=[System.IO.File]::ReadAllText('{path}')
  $blockStart=$text.IndexOf('{begin}')
  $blockEnd=-1
  if ($blockStart -ge 0) {{
    $endIdx=$text.IndexOf('{end}', $blockStart)
    if ($endIdx -ge 0) {{ $blockEnd=$endIdx + '{end}'.Length }}
  }}
  $fn=Get-Command -Name cc -CommandType Function -ErrorAction SilentlyContinue
  if ($null -eq $fn -or $null -eq $fn.ScriptBlock -or $null -eq $fn.ScriptBlock.Ast) {{
    Write-Output 'CC=0'
    if ($blockStart -ge 0 -and $blockEnd -gt $blockStart) {{ Write-Output ("BLOCK_START=" + $blockStart); Write-Output ("BLOCK_END=" + $blockEnd) }} else {{ Write-Output 'BLOCK_START=-1'; Write-Output 'BLOCK_END=-1' }}
    exit 0
  }}
  $ast=$fn.ScriptBlock.Ast
  $hasClaude=0
  $unsafe=0
  # Conservative: any occurrence of the dangerous flag in the effective function body.
  if ($ast.Extent.Text.Contains('--dangerously-skip-permissions')) {{ $unsafe=1 }}
  foreach ($cmd in @($ast.FindAll({{ param($a) $a -is [System.Management.Automation.Language.CommandAst] }}, $true))) {{
    $name=$cmd.GetCommandName()
    if ($name -eq 'claude' -or $name -eq 'claude.exe') {{
      $hasClaude=1
      $elements=@($cmd.CommandElements)
      for ($i=1; $i -lt $elements.Count; $i++) {{
        $el=$elements[$i]
        $text=$el.Extent.Text
        if ($text -eq '--dangerously-skip-permissions') {{ $unsafe=1; continue }}
        if ($el -is [System.Management.Automation.Language.StringConstantExpressionAst]) {{
          if ($el.Value -eq '--dangerously-skip-permissions') {{ $unsafe=1 }}
          continue
        }}
        # Normal splat `@args` is allowed; any other dynamic expression is unsafe.
        if ($text -eq '@args') {{ continue }}
        $unsafe=1
      }}
    }}
  }}
  Write-Output 'CC=1'
  Write-Output ("HAS_CLAUDE=" + $hasClaude)
  Write-Output ("UNSAFE=" + $unsafe)
  Write-Output ("CC_START=" + $ast.Extent.StartOffset)
  Write-Output ("CC_END=" + $ast.Extent.EndOffset)
  if ($blockStart -ge 0 -and $blockEnd -gt $blockStart) {{ Write-Output ("BLOCK_START=" + $blockStart); Write-Output ("BLOCK_END=" + $blockEnd) }} else {{ Write-Output 'BLOCK_START=-1'; Write-Output 'BLOCK_END=-1' }}
}} catch {{
  [Console]::Error.WriteLine($_.Exception.Message)
  Write-Output 'ERR=probe'
  exit 1
}}
"#
    )
}

/// Result of one combined profile load + runtime `cc` classification probe.
#[derive(Debug, Clone)]
pub struct ProfileRuntimeProbeResult {
    pub output: CommandOutput,
    pub classification: Result<CcClassification, String>,
}

/// Load the profile once and classify the runtime-installed `cc` function.
pub async fn probe_profile_runtime<R: CommandRunner + ?Sized>(
    runner: &R,
    pwsh: &Path,
    profile: &Path,
) -> ProfileRuntimeProbeResult {
    let script = runtime_profile_probe_script(profile);
    let output = runner
        .run(&CommandSpec {
            program: pwsh.to_path_buf(),
            args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-NoLogo"),
                OsString::from("-Command"),
                OsString::from(script),
            ],
            timeout: PROBE_TIMEOUT,
            env: Default::default(),
        })
        .await
        .unwrap_or_else(|err| CommandOutput {
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: err.message,
        });

    if output.timed_out {
        return ProfileRuntimeProbeResult {
            output,
            classification: Err("profile runtime probe timed out".into()),
        };
    }
    if output.exit_code != Some(0) {
        return ProfileRuntimeProbeResult {
            output,
            classification: Err("profile runtime probe failed".into()),
        };
    }
    let classification =
        parse_cc_ast_probe_output(&output.stdout).map(|parsed| classify_cc_ast(&parsed));
    ProfileRuntimeProbeResult {
        output,
        classification,
    }
}

/// Classify the runtime-installed `cc` (uses the combined profile runtime probe).
pub async fn probe_cc_classification<R: CommandRunner + ?Sized>(
    runner: &R,
    pwsh: &Path,
    profile: &Path,
) -> Result<CcClassification, String> {
    probe_profile_runtime(runner, pwsh, profile)
        .await
        .classification
}

pub fn sanitize_profile_detail(raw: &str) -> String {
    crate::diagnostics::runner::sanitize_captured(raw)
}

pub fn build_profile_result(
    entry: &CatalogEntry,
    profile_path: Option<PathBuf>,
    file_exists: bool,
    parse_ok: bool,
    parse_error: Option<String>,
    load: Option<&CommandOutput>,
) -> DiagnosticResult {
    let mut details = Vec::new();
    let mut repairs = Vec::new();

    if let Some(path) = &profile_path {
        details.push(format!(
            "profile: {}",
            crate::diagnostics::runner::elide_home_paths(&path.display().to_string())
        ));
    }

    if !file_exists {
        repairs.push(cc_recipe_repair(
            profile_path.clone(),
            ProfileRecipe::SafeClaudeShortcut,
            RepairRisk::Normal,
        ));
        repairs.push(open_docs_repair(entry));
        return DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Missing,
            summary: "PowerShell profile file is missing".to_string(),
            details,
            detected_version: None,
            detected_path: profile_path,
            repairs,
        };
    }

    if !parse_ok {
        if let Some(err) = parse_error {
            details.push(err);
        }
        repairs.push(open_docs_repair(entry));
        if let Some(path) = profile_path.clone() {
            repairs.push(RepairPlan {
                id: "reveal-profile".to_string(),
                title: "Reveal profile".to_string(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::RevealPath(path),
                preview: "Reveal the PowerShell profile in Explorer".to_string(),
                verifies: vec![DiagnosticId::PowerShellProfile],
            });
        }
        return DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Broken,
            summary: "PowerShell profile failed to parse".to_string(),
            details,
            detected_version: None,
            detected_path: profile_path,
            repairs,
        };
    }

    if let Some(load) = load {
        if load.timed_out {
            details.push("profile load probe timed out".to_string());
            return DiagnosticResult {
                id: entry.id,
                title: entry.title.to_string(),
                importance: entry.importance,
                status: DiagnosticStatus::Broken,
                summary: "PowerShell profile load timed out".to_string(),
                details,
                detected_version: None,
                detected_path: profile_path,
                repairs: vec![open_docs_repair(entry)],
            };
        }
        if load.exit_code != Some(0) {
            if !load.stderr.trim().is_empty() {
                details.push(sanitize_profile_detail(&load.stderr));
            } else {
                details.push(format!("profile load exited {:?}", load.exit_code));
            }
            return DiagnosticResult {
                id: entry.id,
                title: entry.title.to_string(),
                importance: entry.importance,
                status: DiagnosticStatus::Broken,
                summary: "PowerShell profile load failed".to_string(),
                details,
                detected_version: None,
                detected_path: profile_path,
                repairs: vec![open_docs_repair(entry)],
            };
        }
        if !load.stderr.trim().is_empty() {
            details.push(sanitize_profile_detail(&load.stderr));
            repairs.push(open_docs_repair(entry));
            return DiagnosticResult {
                id: entry.id,
                title: entry.title.to_string(),
                importance: entry.importance,
                status: DiagnosticStatus::Warning,
                summary: "PowerShell profile loads with warnings".to_string(),
                details,
                detected_version: None,
                detected_path: profile_path,
                repairs,
            };
        }
    }

    DiagnosticResult {
        id: entry.id,
        title: entry.title.to_string(),
        importance: entry.importance,
        status: DiagnosticStatus::Healthy,
        summary: "PowerShell profile parses cleanly".to_string(),
        details,
        detected_version: None,
        detected_path: profile_path,
        repairs: Vec::new(),
    }
}

pub fn build_cc_shortcut_result(
    entry: &CatalogEntry,
    classification: Result<CcClassification, String>,
    profile: Option<PathBuf>,
) -> DiagnosticResult {
    let classification = match classification {
        Ok(value) => value,
        Err(_) => {
            return cc_probe_failed(entry, profile);
        }
    };

    match classification {
        CcClassification::Absent => missing_cc(entry, profile),
        CcClassification::ManagedSafe => DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Healthy,
            summary: "cc shortcut matches the safe recipe".to_string(),
            details: vec![format!("recognized DevManager block ({BEGIN_MARKER})")],
            detected_version: None,
            detected_path: profile,
            repairs: Vec::new(),
        },
        CcClassification::ManagedUnsafe => DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Warning,
            summary: "cc shortcut uses high-risk permissions bypass".to_string(),
            details: vec![format!("recognized DevManager block ({BEGIN_MARKER})")],
            detected_version: None,
            detected_path: profile.clone(),
            repairs: vec![cc_recipe_repair(
                profile,
                ProfileRecipe::SafeClaudeShortcut,
                RepairRisk::Normal,
            )],
        },
        CcClassification::UnmarkedSafe => DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Healthy,
            summary: "cc shortcut is present (unmarked safe recipe)".to_string(),
            details: vec![
                "existing function cc was detected without capturing unrelated profile contents"
                    .into(),
            ],
            detected_version: None,
            detected_path: profile,
            // Do not insert a duplicate managed DevManager block.
            repairs: Vec::new(),
        },
        CcClassification::UnmarkedUnsafe => DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Warning,
            summary: "Unmarked high-risk cc shortcut found in the profile".to_string(),
            details: vec![
                "existing function cc uses --dangerously-skip-permissions; DevManager will not insert a duplicate managed function".into(),
            ],
            detected_version: None,
            detected_path: profile,
            // No UpdatePowerShellProfile — would duplicate the unmarked function.
            repairs: vec![RepairPlan {
                id: "cc-unmarked-unsafe-docs".into(),
                title: "Review high-risk cc shortcut".into(),
                risk: RepairRisk::High,
                operation: RepairOperation::OpenUrl(entry.docs_url.to_string()),
                preview: format!("Open {}", entry.docs_url),
                verifies: vec![DiagnosticId::CcShortcut],
            }],
        },
    }
}

fn cc_probe_failed(entry: &CatalogEntry, profile: Option<PathBuf>) -> DiagnosticResult {
    let mut repairs = vec![open_docs_repair(entry)];
    if let Some(path) = profile.clone() {
        repairs.push(RepairPlan {
            id: "cc-reveal-profile".into(),
            title: "Reveal PowerShell profile".into(),
            risk: RepairRisk::Normal,
            operation: RepairOperation::RevealPath(path),
            preview: "Reveal profile path".into(),
            verifies: vec![DiagnosticId::CcShortcut],
        });
    }
    DiagnosticResult {
        id: entry.id,
        title: entry.title.to_string(),
        importance: entry.importance,
        status: DiagnosticStatus::Warning,
        summary: "cc classification unavailable".to_string(),
        details: vec!["AST probe failed; profile contents were not captured".into()],
        detected_version: None,
        detected_path: profile,
        repairs,
    }
}

fn missing_cc(entry: &CatalogEntry, profile: Option<PathBuf>) -> DiagnosticResult {
    DiagnosticResult {
        id: entry.id,
        title: entry.title.to_string(),
        importance: entry.importance,
        status: DiagnosticStatus::Missing,
        summary: "cc shortcut is not installed in the PowerShell profile".to_string(),
        details: Vec::new(),
        detected_version: None,
        detected_path: profile.clone(),
        repairs: vec![
            cc_recipe_repair(
                profile.clone(),
                ProfileRecipe::SafeClaudeShortcut,
                RepairRisk::Normal,
            ),
            cc_recipe_repair(
                profile,
                ProfileRecipe::UnsafeClaudeShortcut,
                RepairRisk::High,
            ),
        ],
    }
}

fn cc_recipe_repair(
    profile: Option<PathBuf>,
    recipe: ProfileRecipe,
    risk: RepairRisk,
) -> RepairPlan {
    let path = profile.unwrap_or_else(|| PathBuf::from("Microsoft.PowerShell_profile.ps1"));
    let title = match recipe {
        ProfileRecipe::SafeClaudeShortcut => "Install safe cc shortcut",
        ProfileRecipe::UnsafeClaudeShortcut => "Install high-risk cc shortcut",
    };
    RepairPlan {
        id: format!("profile-cc-{:?}", recipe).to_ascii_lowercase(),
        title: title.to_string(),
        risk,
        operation: RepairOperation::UpdatePowerShellProfile { path, recipe },
        preview: format!("Update DevManager block with {:?} recipe", recipe),
        verifies: vec![DiagnosticId::CcShortcut, DiagnosticId::PowerShellProfile],
    }
}

pub fn webview2_result(entry: &CatalogEntry) -> DiagnosticResult {
    #[cfg(windows)]
    {
        match wry::webview_version() {
            Ok(version) => DiagnosticResult {
                id: entry.id,
                title: entry.title.to_string(),
                importance: entry.importance,
                status: DiagnosticStatus::Healthy,
                summary: "WebView2 runtime is installed".to_string(),
                details: Vec::new(),
                detected_version: Some(version),
                detected_path: None,
                repairs: Vec::new(),
            },
            Err(_) => {
                let mut repairs = vec![open_docs_repair(entry)];
                if let Some(plan) = winget_install_repair(entry) {
                    repairs.insert(0, plan);
                }
                DiagnosticResult {
                    id: entry.id,
                    title: entry.title.to_string(),
                    importance: entry.importance,
                    status: DiagnosticStatus::Missing,
                    summary: "WebView2 runtime was not found".to_string(),
                    details: Vec::new(),
                    detected_version: None,
                    detected_path: None,
                    repairs,
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        crate::diagnostics::catalog::unavailable_result(entry, "WebView2 is Windows-only")
    }
}

pub fn path_consistency_result(
    entry: &CatalogEntry,
    resolved: &[(String, Vec<PathBuf>)],
) -> DiagnosticResult {
    let collapsed: Vec<(String, Vec<PathBuf>)> = resolved
        .iter()
        .map(|(name, paths)| (name.clone(), collapse_same_directory_installs(paths)))
        .collect();
    let conflicts: Vec<_> = collapsed
        .iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect();
    if conflicts.is_empty() {
        DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Healthy,
            summary: "Configured commands resolve without PATH conflicts".to_string(),
            details: collapsed
                .iter()
                .map(|(name, paths)| {
                    format!(
                        "{name}: {}",
                        paths
                            .first()
                            .map(|p| crate::diagnostics::runner::elide_home_paths(
                                &p.display().to_string()
                            ))
                            .unwrap_or_else(|| "missing".to_string())
                    )
                })
                .collect(),
            detected_version: None,
            detected_path: None,
            repairs: Vec::new(),
        }
    } else {
        DiagnosticResult {
            id: entry.id,
            title: entry.title.to_string(),
            importance: entry.importance,
            status: DiagnosticStatus::Warning,
            summary: "Multiple installations found on PATH".to_string(),
            details: conflicts
                .iter()
                .map(|(name, paths)| {
                    format!(
                        "{name}: {}",
                        paths
                            .iter()
                            .map(|p| crate::diagnostics::runner::elide_home_paths(
                                &p.display().to_string()
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect(),
            detected_version: None,
            detected_path: None,
            repairs: vec![open_docs_repair(entry)],
        }
    }
}

pub fn windows_only_repairs_allowed() -> bool {
    is_windows()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::catalog::entry;

    #[test]
    fn missing_profile_is_missing_not_healthy() {
        let entry = entry(DiagnosticId::PowerShellProfile).unwrap();
        let result =
            build_profile_result(entry, Some(PathBuf::from("p.ps1")), false, true, None, None);
        assert_eq!(result.status, DiagnosticStatus::Missing);
    }

    #[test]
    fn nonzero_load_exit_is_broken_even_without_stderr() {
        let entry = entry(DiagnosticId::PowerShellProfile).unwrap();
        let load = CommandOutput {
            exit_code: Some(1),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
        };
        let result = build_profile_result(
            entry,
            Some(PathBuf::from("p.ps1")),
            true,
            true,
            None,
            Some(&load),
        );
        assert_eq!(result.status, DiagnosticStatus::Broken);
    }

    #[test]
    fn unmarked_safe_cc_is_healthy_without_managed_install() {
        let entry = entry(DiagnosticId::CcShortcut).unwrap();
        let result = build_cc_shortcut_result(entry, Ok(CcClassification::UnmarkedSafe), None);
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert!(result.repairs.is_empty());
    }

    #[test]
    fn unmarked_unsafe_cc_is_warning_without_managed_install() {
        let entry = entry(DiagnosticId::CcShortcut).unwrap();
        let result = build_cc_shortcut_result(entry, Ok(CcClassification::UnmarkedUnsafe), None);
        assert_eq!(result.status, DiagnosticStatus::Warning);
        assert!(!result
            .repairs
            .iter()
            .any(|r| matches!(r.operation, RepairOperation::UpdatePowerShellProfile { .. })));
        assert!(result.repairs.iter().any(|r| r.risk == RepairRisk::High));
    }

    #[test]
    fn cc_ast_probe_fail_closed_has_docs_only_no_secrets() {
        let entry = entry(DiagnosticId::CcShortcut).unwrap();
        let result = build_cc_shortcut_result(
            entry,
            Err("boom token=secret-value".into()),
            Some(PathBuf::from(r"C:\Users\dev\profile.ps1")),
        );
        assert_eq!(result.status, DiagnosticStatus::Warning);
        let joined = format!("{}{}", result.summary, result.details.join(" "));
        assert!(!joined.contains("secret-value"));
        assert!(!joined.contains("token="));
        assert!(result.repairs.iter().all(|r| matches!(
            r.operation,
            RepairOperation::OpenUrl(_) | RepairOperation::RevealPath(_)
        )));
    }

    #[test]
    fn nvm_cmd_and_ps1_do_not_conflict() {
        let entry = entry(DiagnosticId::PathConsistency).unwrap();
        let result = path_consistency_result(
            entry,
            &[(
                "npm".into(),
                vec![
                    PathBuf::from(r"C:\nvm\npm.cmd"),
                    PathBuf::from(r"C:\nvm\npm.ps1"),
                ],
            )],
        );
        assert_eq!(result.status, DiagnosticStatus::Healthy);
    }

    #[test]
    fn profile_parse_script_initializes_ref_variables_before_parsefile() {
        let profile =
            Path::new(r"C:\Users\dev\Documents\PowerShell\Microsoft.PowerShell_profile.ps1");
        let script = profile_parse_script(profile);
        let parse_at = script
            .find("ParseFile")
            .expect("script must call ParseFile");
        let tokens_at = script
            .find("$tokens=$null")
            .expect("tokens must be initialized");
        let errs_at = script
            .find("$errs=$null")
            .expect("errs must be initialized");
        assert!(
            tokens_at < parse_at,
            "tokens init must precede ParseFile: {script}"
        );
        assert!(
            errs_at < parse_at,
            "errs init must precede ParseFile: {script}"
        );
        assert!(script.contains("[ref]$tokens"));
        assert!(script.contains("[ref]$errs"));
        assert!(
            !script.contains("[ref]$null"),
            "must not pass [ref]$null: {script}"
        );
        assert!(script.contains("if ($errs)"));
        assert!(script.contains("exit 1"));

        let escaped = profile_parse_script(Path::new(r"C:\o'brien\profile.ps1"));
        assert!(
            escaped.contains("o''brien"),
            "single quotes in path must be escaped: {escaped}"
        );
    }

    #[test]
    fn runtime_profile_probe_script_loads_and_inspects_get_command_cc() {
        let script = runtime_profile_probe_script(Path::new(r"C:\Users\dev\profile.ps1"));
        assert!(script.contains("Get-Command"));
        assert!(script.contains("-CommandType Function"));
        assert!(script.contains("ScriptBlock.Ast"));
        assert!(script.contains("HAS_CLAUDE="));
        assert!(script.contains("UNSAFE="));
        assert!(script.contains("CC_START="));
        assert!(script.contains("StringConstantExpressionAst"));
        assert!(script.contains("1>$null"));
        assert!(!script.contains("Write-Output $text"));
        assert!(
            !script.contains("FunctionDefinitionAst"),
            "must not use textual last-definition FindAll path"
        );
        assert!(!script.contains("ParseFile"));
    }

    #[tokio::test]
    async fn runtime_probe_uses_fixed_output_and_noprofile_nologo() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner {
            response: Mutex<CommandOutput>,
            calls: Mutex<Vec<String>>,
        }

        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move {
                    let key = format!(
                        "{}|{}",
                        spec.program.display(),
                        spec.args
                            .iter()
                            .map(|a| a.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    self.calls.lock().unwrap().push(key);
                    Ok(self.response.lock().unwrap().clone())
                })
            }
        }

        let pwsh = PathBuf::from(r"C:\pwsh\pwsh.exe");
        let profile = PathBuf::from(r"C:\Users\dev\profile.ps1");
        let stdout =
            "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=120\nCC_END=200\nBLOCK_START=-1\nBLOCK_END=-1\n";
        let runner = FakeRunner {
            response: Mutex::new(CommandOutput {
                exit_code: Some(0),
                timed_out: false,
                stdout: stdout.into(),
                stderr: String::new(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let classification = probe_cc_classification(&runner, &pwsh, &profile)
            .await
            .unwrap();
        assert_eq!(classification, CcClassification::UnmarkedUnsafe);
        let call = runner.calls.lock().unwrap()[0].clone();
        assert!(call.contains("-NoProfile"));
        assert!(call.contains("-NoLogo"));
        assert!(call.contains("Get-Command"));
        assert!(!call.contains("ParseFile"));
    }

    #[tokio::test]
    async fn cc_ast_probe_earlier_safe_later_unsafe_is_unsafe() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=300\nCC_END=400\nBLOCK_START=-1\nBLOCK_END=-1\n"
                    .into(),
            stderr: String::new(),
        }));
        let classification = probe_cc_classification(
            &runner,
            Path::new(r"C:\pwsh\pwsh.exe"),
            Path::new(r"C:\Users\dev\profile.ps1"),
        )
        .await
        .unwrap();
        assert_eq!(classification, CcClassification::UnmarkedUnsafe);
    }

    #[tokio::test]
    async fn cc_ast_probe_string_flag_outside_command_is_safe_when_probe_reports_safe() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=0\nCC_START=10\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n"
                    .into(),
            stderr: String::new(),
        }));
        let classification = probe_cc_classification(
            &runner,
            Path::new(r"C:\pwsh\pwsh.exe"),
            Path::new(r"C:\Users\dev\profile.ps1"),
        )
        .await
        .unwrap();
        assert_eq!(classification, CcClassification::UnmarkedSafe);
    }

    #[tokio::test]
    async fn cc_ast_probe_managed_safe_and_unsafe() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let safe = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=0\nCC_START=20\nCC_END=80\nBLOCK_START=10\nBLOCK_END=90\n"
                    .into(),
            stderr: String::new(),
        }));
        assert_eq!(
            probe_cc_classification(
                &safe,
                Path::new(r"C:\pwsh\pwsh.exe"),
                Path::new(r"C:\Users\dev\profile.ps1"),
            )
            .await
            .unwrap(),
            CcClassification::ManagedSafe
        );

        let unsafe_managed = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=20\nCC_END=80\nBLOCK_START=10\nBLOCK_END=90\n"
                    .into(),
            stderr: String::new(),
        }));
        assert_eq!(
            probe_cc_classification(
                &unsafe_managed,
                Path::new(r"C:\pwsh\pwsh.exe"),
                Path::new(r"C:\Users\dev\profile.ps1"),
            )
            .await
            .unwrap(),
            CcClassification::ManagedUnsafe
        );
    }

    #[tokio::test]
    async fn cc_ast_probe_malformed_output_fails_closed_without_secrets() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout: "function cc { claude --dangerously-skip-permissions }\ntoken=secret\n".into(),
            stderr: String::new(),
        }));
        let err = probe_cc_classification(
            &runner,
            Path::new(r"C:\pwsh\pwsh.exe"),
            Path::new(r"C:\Users\dev\profile.ps1"),
        )
        .await
        .unwrap_err();
        assert!(!err.contains("secret"));
        assert!(!err.contains("dangerously"));
    }

    #[tokio::test]
    async fn cc_ast_probe_no_real_claude_command_is_absent() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            // function cc exists but only writes text / comments — no CommandAst named claude.
            stdout:
                "CC=1\nHAS_CLAUDE=0\nUNSAFE=0\nCC_START=0\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n"
                    .into(),
            stderr: String::new(),
        }));
        assert_eq!(
            probe_cc_classification(
                &runner,
                Path::new(r"C:\pwsh\pwsh.exe"),
                Path::new(r"C:\Users\dev\profile.ps1"),
            )
            .await
            .unwrap(),
            CcClassification::Absent
        );
    }

    #[tokio::test]
    async fn cc_ast_probe_missing_has_claude_fails_closed() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout: "CC=1\nUNSAFE=0\nCC_START=0\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n".into(),
            stderr: String::new(),
        }));
        assert!(probe_cc_classification(
            &runner,
            Path::new(r"C:\pwsh\pwsh.exe"),
            Path::new(r"C:\Users\dev\profile.ps1"),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn cc_ast_probe_quoted_dangerous_argument_is_unsafe() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            // Script sets UNSAFE=1 via StringConstantExpressionAst.Value for quoted args.
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=0\nCC_END=50\nBLOCK_START=-1\nBLOCK_END=-1\n"
                    .into(),
            stderr: String::new(),
        }));
        assert_eq!(
            probe_cc_classification(
                &runner,
                Path::new(r"C:\pwsh\pwsh.exe"),
                Path::new(r"C:\Users\dev\profile.ps1"),
            )
            .await
            .unwrap(),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[tokio::test]
    async fn cc_ast_probe_reports_unsafe_when_probe_marks_body_wide_flag() {
        use crate::diagnostics::runner::CommandRunnerFuture;
        use std::sync::Mutex;

        struct FakeRunner(Mutex<CommandOutput>);
        impl CommandRunner for FakeRunner {
            fn run<'a>(&'a self, _spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
                Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
            }
        }

        // Probe conservatively marks UNSAFE=1 when the flag appears anywhere in the body.
        let runner = FakeRunner(Mutex::new(CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout:
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=0\nCC_END=50\nBLOCK_START=-1\nBLOCK_END=-1\n"
                    .into(),
            stderr: String::new(),
        }));
        assert_eq!(
            probe_cc_classification(
                &runner,
                Path::new(r"C:\pwsh\pwsh.exe"),
                Path::new(r"C:\Users\dev\profile.ps1"),
            )
            .await
            .unwrap(),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[cfg(windows)]
    fn real_pwsh_for_tests() -> Option<PathBuf> {
        let candidates = [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Program Files\PowerShell\7-preview\pwsh.exe",
        ];
        for candidate in candidates {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
        std::process::Command::new("where")
            .arg("pwsh")
            .output()
            .ok()
            .and_then(|output| {
                if !output.status.success() {
                    return None;
                }
                let text = String::from_utf8_lossy(&output.stdout);
                text.lines()
                    .next()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(PathBuf::from)
                    .filter(|path| path.is_file())
            })
    }

    #[cfg(windows)]
    fn assert_fixed_probe_stdout(stdout: &str) {
        for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
            assert!(
                line.contains('=') && !line.contains('{') && !line.contains('}'),
                "stdout must be fixed KEY=value only, got {line:?}"
            );
            assert!(
                !line.contains("function cc")
                    && !line.contains("Write-Host")
                    && !line.contains("token="),
                "stdout must not include profile body: {line:?}"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_active_unsafe_not_hidden_by_dead_safe_branch() {
        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        std::fs::write(
            &profile,
            "function cc { claude --dangerously-skip-permissions @args }\nif ($false) { function cc { claude @args } }\n",
        )
        .unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;
        let result = probe_profile_runtime(&runner, &pwsh, &profile).await;
        assert_eq!(result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&result.output.stdout);
        assert_eq!(
            result.classification.unwrap(),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_dynamic_dangerous_flag_variable_is_unsafe() {
        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        std::fs::write(
            &profile,
            "function cc { $flag='--dangerously-skip-permissions'; claude $flag @args }\n",
        )
        .unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;
        let result = probe_profile_runtime(&runner, &pwsh, &profile).await;
        assert_eq!(result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&result.output.stdout);
        assert_eq!(
            result.classification.unwrap(),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_direct_safe_claude_args_remains_safe() {
        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        std::fs::write(&profile, "function cc { claude @args }\n").unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;
        let result = probe_profile_runtime(&runner, &pwsh, &profile).await;
        assert_eq!(result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&result.output.stdout);
        assert_eq!(
            result.classification.unwrap(),
            CcClassification::UnmarkedSafe
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_active_safe_ignores_uninvoked_unsafe_scriptblock() {
        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        std::fs::write(
            &profile,
            "function cc { claude @args }\n$dead = { function cc { claude --dangerously-skip-permissions @args } }\n",
        )
        .unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;
        let result = probe_profile_runtime(&runner, &pwsh, &profile).await;
        assert_eq!(result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&result.output.stdout);
        assert_eq!(
            result.classification.unwrap(),
            CcClassification::UnmarkedSafe
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_later_toplevel_safe_replaces_earlier_unsafe() {
        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        std::fs::write(
            &profile,
            "function cc { claude --dangerously-skip-permissions @args }\nfunction cc { claude @args }\n",
        )
        .unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;
        let result = probe_profile_runtime(&runner, &pwsh, &profile).await;
        assert_eq!(result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&result.output.stdout);
        assert_eq!(
            result.classification.unwrap(),
            CcClassification::UnmarkedSafe
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runtime_pwsh_managed_marker_safe_and_unsafe() {
        use crate::diagnostics::profile::{BEGIN_MARKER, END_MARKER};

        let Some(pwsh) = real_pwsh_for_tests() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let runner = crate::diagnostics::runner::TokioCommandRunner;

        let safe_path = dir.path().join("managed_safe.ps1");
        std::fs::write(
            &safe_path,
            format!("{BEGIN_MARKER}\nfunction cc {{ claude @args }}\n{END_MARKER}\n"),
        )
        .unwrap();
        let safe = probe_profile_runtime(&runner, &pwsh, &safe_path).await;
        assert_eq!(safe.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&safe.output.stdout);
        assert_eq!(safe.classification.unwrap(), CcClassification::ManagedSafe);

        let unsafe_path = dir.path().join("managed_unsafe.ps1");
        std::fs::write(
            &unsafe_path,
            format!(
                "{BEGIN_MARKER}\nfunction cc {{ claude --dangerously-skip-permissions @args }}\n{END_MARKER}\n"
            ),
        )
        .unwrap();
        let unsafe_result = probe_profile_runtime(&runner, &pwsh, &unsafe_path).await;
        assert_eq!(unsafe_result.output.exit_code, Some(0));
        assert_fixed_probe_stdout(&unsafe_result.output.stdout);
        assert_eq!(
            unsafe_result.classification.unwrap(),
            CcClassification::ManagedUnsafe
        );
    }
}
