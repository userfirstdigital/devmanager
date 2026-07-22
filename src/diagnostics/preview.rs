//! Concrete, sanitized confirmation previews for repair plans.

use crate::diagnostics::model::{ProfileRecipe, RepairOperation, RepairPlan, RepairRisk};
use crate::diagnostics::profile::{
    inspect_marked_block, managed_block_preview, read_profile, MarkedBlockState,
};
use crate::diagnostics::repair::{validate_plan, winget_install_args};
use crate::diagnostics::runner::{elide_home_paths, sanitize_captured};
use std::path::Path;

const PREVIEW_LINE_MAX_CHARS: usize = 200;
const PREVIEW_BLOCK_MAX_LINES: usize = 20;

/// Format confirmation lines for one or more pending plans.
/// Fails when a plan cannot be validated or a concrete preview cannot be built safely.
pub fn format_pending_repairs_preview(plans: &[RepairPlan]) -> Result<Vec<String>, String> {
    if plans.is_empty() {
        return Err("no repairs to preview".into());
    }
    let mut lines = Vec::new();
    let bulk = plans.len() > 1;
    if bulk {
        lines.push(format!("Apply {} recommended repairs:", plans.len()));
    }
    for plan in plans {
        validate_plan(plan)?;
        if bulk {
            lines.push(format!("• {}", bound_preview_line(&plan.title)));
            for detail in format_repair_operation_lines(&plan.operation)? {
                lines.push(format!("  {detail}"));
            }
            if plan.risk == RepairRisk::High {
                lines.push(format!("  {}", high_risk_notice()));
            }
        } else {
            lines.push(bound_preview_line(&plan.title));
            lines.extend(format_repair_operation_lines(&plan.operation)?);
            if plan.risk == RepairRisk::High {
                lines.push(high_risk_notice().to_string());
            }
        }
    }
    Ok(lines)
}

pub fn format_repair_operation_lines(operation: &RepairOperation) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    match operation {
        RepairOperation::RunKnownCommand { program, args } => {
            let program = display_path(program);
            let args_display = args
                .iter()
                .map(|arg| bound_preview_line(&sanitize_captured(arg)))
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("Executable: {program}"));
            if args_display.is_empty() {
                lines.push("Arguments: (none)".into());
            } else {
                lines.push(format!("Arguments: {args_display}"));
            }
        }
        RepairOperation::InstallWingetPackage { package_id } => {
            let args = winget_install_args(package_id);
            lines.push("Executable: winget".into());
            lines.push(format!("Arguments: {}", args.join(" ")));
        }
        RepairOperation::UpdatePowerShellProfile { path, recipe } => {
            lines.extend(profile_edit_preview_lines(path, *recipe)?);
        }
        RepairOperation::SetDefaultTerminal(terminal) => {
            lines.push(format!(
                "Setting: default_terminal = {}",
                bound_preview_line(&format!("{terminal:?}"))
            ));
        }
        RepairOperation::SetClaudeCommand(command) => {
            lines.push(format!(
                "Setting: claude_command = {}",
                bound_preview_line(&sanitize_captured(command))
            ));
        }
        RepairOperation::SetCodexCommand(command) => {
            lines.push(format!(
                "Setting: codex_command = {}",
                bound_preview_line(&sanitize_captured(command))
            ));
        }
        RepairOperation::OpenUrl(url) => {
            lines.push(format!(
                "Open URL: {}",
                bound_preview_line(&sanitize_captured(url))
            ));
        }
        RepairOperation::RevealPath(path) => {
            lines.push(format!("Reveal path: {}", display_path(path)));
        }
        RepairOperation::CopyCommand(command) => {
            lines.push(format!(
                "Copy command: {}",
                bound_preview_line(&sanitize_captured(command))
            ));
        }
    }
    Ok(lines)
}

fn profile_edit_preview_lines(path: &Path, recipe: ProfileRecipe) -> Result<Vec<String>, String> {
    // Confirmation only shows the known managed block template — never the live profile body.
    if !matches!(
        recipe,
        ProfileRecipe::SafeClaudeShortcut | ProfileRecipe::UnsafeClaudeShortcut
    ) {
        return Err("unsupported profile recipe for confirmation preview".into());
    }
    let mut lines = Vec::new();
    lines.push(format!("Profile: {}", display_path(path)));
    lines.push(profile_edit_action_line(path)?);
    lines.push("Managed block:".into());
    let block = managed_block_preview(recipe);
    for (index, line) in block.lines().enumerate() {
        if index >= PREVIEW_BLOCK_MAX_LINES {
            lines.push("  …".into());
            break;
        }
        lines.push(format!("  {}", bound_preview_line(line)));
    }
    Ok(lines)
}

/// Inspect marker state only (create / append / replace). Never render live profile contents.
fn profile_edit_action_line(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Ok("Action: create profile with managed DevManager block".into());
    }
    if !path.is_file() {
        return Err("profile path is not a regular file".into());
    }
    let (content, _, _) = read_profile(path)
        .map_err(|_| "failed to read profile for confirmation preview".to_string())?;
    match inspect_marked_block(&content) {
        MarkedBlockState::Missing => Ok("Action: append managed DevManager block".into()),
        MarkedBlockState::Present { .. } => Ok("Action: replace managed DevManager block".into()),
        MarkedBlockState::Malformed { .. } => {
            Err("profile has malformed DevManager markers; review the profile manually".into())
        }
    }
}

fn high_risk_notice() -> &'static str {
    "This repair bypasses Claude Code permission prompts."
}

fn display_path(path: &Path) -> String {
    bound_preview_line(&elide_home_paths(&path.display().to_string()))
}

fn bound_preview_line(raw: &str) -> String {
    let sanitized = sanitize_captured(raw);
    let count = sanitized.chars().count();
    if count <= PREVIEW_LINE_MAX_CHARS {
        return sanitized;
    }
    let truncated: String = sanitized.chars().take(PREVIEW_LINE_MAX_CHARS).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::model::RepairRisk;
    use crate::diagnostics::profile::{BEGIN_MARKER, END_MARKER};
    use crate::models::config::DefaultTerminal;
    use std::path::PathBuf;

    fn plan(operation: RepairOperation) -> RepairPlan {
        RepairPlan {
            id: "p".into(),
            title: "Plan title".into(),
            risk: RepairRisk::Normal,
            operation,
            preview: "generic".into(),
            verifies: Vec::new(),
        }
    }

    #[test]
    fn previews_every_operation_variant_with_concrete_targets() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/home/user"));
        let profile = home
            .join("Documents")
            .join("PowerShell")
            .join("profile.ps1");
        // Guaranteed-missing path so host profile marker state cannot affect Action.
        let profile_edit = home
            .join("Documents")
            .join("PowerShell")
            .join("devmanager-preview-missing.ps1");

        let cases: Vec<(RepairOperation, &[&str])> = vec![
            (
                RepairOperation::RunKnownCommand {
                    program: PathBuf::from(r"C:\tools\claude.exe"),
                    args: vec!["update".into()],
                },
                &["Executable:", "claude.exe", "Arguments:", "update"],
            ),
            (
                RepairOperation::InstallWingetPackage {
                    package_id: "Microsoft.PowerShell".into(),
                },
                &[
                    "Executable: winget",
                    "Arguments:",
                    "--id",
                    "Microsoft.PowerShell",
                ],
            ),
            (
                RepairOperation::UpdatePowerShellProfile {
                    path: profile_edit,
                    recipe: ProfileRecipe::SafeClaudeShortcut,
                },
                &[
                    "Profile:",
                    "Action: create profile with managed DevManager block",
                    "Managed block:",
                    "# BEGIN DevManager",
                    "function cc",
                    "claude @args",
                    "# END DevManager",
                ],
            ),
            (
                RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
                &["Setting: default_terminal", "Pwsh"],
            ),
            (
                RepairOperation::SetClaudeCommand("claude".into()),
                &["Setting: claude_command", "claude"],
            ),
            (
                RepairOperation::SetCodexCommand("codex".into()),
                &["Setting: codex_command", "codex"],
            ),
            (
                RepairOperation::OpenUrl("https://example.com/docs".into()),
                &["Open URL:", "https://example.com/docs"],
            ),
            (
                RepairOperation::RevealPath(profile.clone()),
                &["Reveal path:"],
            ),
            (
                RepairOperation::CopyCommand("claude --version".into()),
                &["Copy command:", "claude --version"],
            ),
        ];

        for (operation, needles) in cases {
            let lines = format_repair_operation_lines(&operation).unwrap();
            let joined = lines.join("\n");
            for needle in needles {
                assert!(
                    joined.contains(needle),
                    "missing {needle:?} in preview for {operation:?}: {joined}"
                );
            }
            assert!(
                !joined.contains("generic"),
                "must not fall back to generic plan.preview"
            );
        }
    }

    #[test]
    fn profile_preview_elides_home_and_shows_only_managed_block() {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/home/user"));
        let profile = home.join("secret-folder").join("profile.ps1");
        let lines = format_repair_operation_lines(&RepairOperation::UpdatePowerShellProfile {
            path: profile,
            recipe: ProfileRecipe::SafeClaudeShortcut,
        })
        .unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("Profile:"));
        assert!(joined.contains('~') || !joined.contains("secret-folder"));
        assert!(joined.contains("# BEGIN DevManager"));
        assert!(joined.contains("claude @args"));
        assert!(!joined.contains("Write-Host"));
        assert!(
            joined.contains("create profile"),
            "nonexistent path should preview create: {joined}"
        );
    }

    #[test]
    fn profile_preview_action_create_append_replace_or_malformed_error() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.ps1");
        let create = format_repair_operation_lines(&RepairOperation::UpdatePowerShellProfile {
            path: missing,
            recipe: ProfileRecipe::SafeClaudeShortcut,
        })
        .unwrap()
        .join("\n");
        assert!(create.contains("Action: create profile with managed DevManager block"));
        assert!(!create.contains("token=live-secret"));

        let append_path = dir.path().join("append.ps1");
        fs::write(&append_path, "Write-Host 'token=live-secret'\n").unwrap();
        let append = format_repair_operation_lines(&RepairOperation::UpdatePowerShellProfile {
            path: append_path.clone(),
            recipe: ProfileRecipe::SafeClaudeShortcut,
        })
        .unwrap()
        .join("\n");
        assert!(append.contains("Action: append managed DevManager block"));
        assert!(
            !append.contains("token=live-secret") && !append.contains("Write-Host"),
            "must not render live profile body: {append}"
        );

        let replace_path = dir.path().join("replace.ps1");
        fs::write(
            &replace_path,
            format!("{BEGIN_MARKER}\nfunction cc {{ claude @args }}\n{END_MARKER}\n"),
        )
        .unwrap();
        let replace = format_repair_operation_lines(&RepairOperation::UpdatePowerShellProfile {
            path: replace_path,
            recipe: ProfileRecipe::SafeClaudeShortcut,
        })
        .unwrap()
        .join("\n");
        assert!(replace.contains("Action: replace managed DevManager block"));

        let malformed_path = dir.path().join("malformed.ps1");
        fs::write(
            &malformed_path,
            format!("{BEGIN_MARKER}\na\n{END_MARKER}\n{BEGIN_MARKER}\nb\n{END_MARKER}\n"),
        )
        .unwrap();
        let err =
            format_pending_repairs_preview(&[plan(RepairOperation::UpdatePowerShellProfile {
                path: malformed_path,
                recipe: ProfileRecipe::SafeClaudeShortcut,
            })])
            .unwrap_err();
        assert!(
            err.to_ascii_lowercase().contains("malformed"),
            "malformed markers must reject confirmation preview: {err}"
        );
    }

    #[test]
    fn profile_preview_redacts_secrets_in_auxiliary_fields_and_bounds_lines() {
        let long = "x".repeat(PREVIEW_LINE_MAX_CHARS + 40);
        let lines = format_repair_operation_lines(&RepairOperation::SetClaudeCommand(format!(
            "token={long}"
        )))
        .unwrap();
        let joined = lines.join("\n");
        assert!(!joined.contains(&long));
        assert!(joined.contains('…') || joined.contains("***"));
    }

    #[test]
    fn bulk_preview_includes_every_plan_and_high_risk_notice() {
        let plans = vec![
            plan(RepairOperation::SetClaudeCommand("claude".into())),
            RepairPlan {
                id: "unsafe".into(),
                title: "Unsafe profile".into(),
                risk: RepairRisk::High,
                operation: RepairOperation::OpenUrl("https://example.com".into()),
                preview: "generic".into(),
                verifies: Vec::new(),
            },
        ];
        let lines = format_pending_repairs_preview(&plans).unwrap();
        let joined = lines.join("\n");
        assert!(joined.contains("Apply 2 recommended repairs:"));
        assert!(joined.contains("Setting: claude_command"));
        assert!(joined.contains("Open URL:"));
        assert!(joined.contains("bypasses Claude Code permission prompts"));
    }

    #[test]
    fn invalid_plan_rejects_preview() {
        let err = format_pending_repairs_preview(&[plan(RepairOperation::InstallWingetPackage {
            package_id: "Not.Allowlisted".into(),
        })])
        .unwrap_err();
        assert!(err.to_ascii_lowercase().contains("unrecognized") || !err.is_empty());
    }
}
