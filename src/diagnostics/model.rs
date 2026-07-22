use crate::models::config::DefaultTerminal;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticId {
    ClaudeCli,
    CodexCli,
    PowerShell7,
    NodeNpm,
    Nvm,
    PowerShellProfile,
    CcShortcut,
    Git,
    GitHubCli,
    Winget,
    WebView2,
    PathConsistency,
    Docker,
    Wsl,
    Rust,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticImportance {
    Required,
    Recommended,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Healthy,
    Warning,
    Missing,
    Broken,
    Running,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairRisk {
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRecipe {
    SafeClaudeShortcut,
    UnsafeClaudeShortcut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairOperation {
    RunKnownCommand {
        program: PathBuf,
        args: Vec<String>,
    },
    InstallWingetPackage {
        package_id: String,
    },
    UpdatePowerShellProfile {
        path: PathBuf,
        recipe: ProfileRecipe,
    },
    SetDefaultTerminal(DefaultTerminal),
    SetClaudeCommand(String),
    SetCodexCommand(String),
    OpenUrl(String),
    RevealPath(PathBuf),
    CopyCommand(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    pub id: String,
    pub title: String,
    pub risk: RepairRisk,
    pub operation: RepairOperation,
    pub preview: String,
    pub verifies: Vec<DiagnosticId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    pub plan_id: String,
    pub success: bool,
    /// True only for successful winget installs whose immediate verification is still non-Healthy.
    pub requires_restart: bool,
    pub summary: String,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticResult {
    pub id: DiagnosticId,
    pub title: String,
    pub importance: DiagnosticImportance,
    pub status: DiagnosticStatus,
    pub summary: String,
    pub details: Vec<String>,
    pub detected_version: Option<String>,
    pub detected_path: Option<PathBuf>,
    pub repairs: Vec<RepairPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticSnapshot {
    pub results: Vec<DiagnosticResult>,
    pub required_failures: usize,
    pub warnings: usize,
    pub healthy: usize,
    pub unavailable: usize,
}

impl DiagnosticSnapshot {
    pub fn from_results(mut results: Vec<DiagnosticResult>) -> Self {
        results.sort_by(|a, b| {
            status_rank(a)
                .cmp(&status_rank(b))
                .then_with(|| importance_rank(a.importance).cmp(&importance_rank(b.importance)))
                .then_with(|| a.title.cmp(&b.title))
        });

        let mut required_failures = 0;
        let mut warnings = 0;
        let mut healthy = 0;
        let mut unavailable = 0;

        for result in &results {
            match result.status {
                DiagnosticStatus::Healthy => healthy += 1,
                DiagnosticStatus::Unavailable => unavailable += 1,
                DiagnosticStatus::Running => {}
                DiagnosticStatus::Warning => {
                    if result.importance != DiagnosticImportance::Optional {
                        warnings += 1;
                    }
                }
                DiagnosticStatus::Missing | DiagnosticStatus::Broken => match result.importance {
                    DiagnosticImportance::Required => required_failures += 1,
                    DiagnosticImportance::Recommended => warnings += 1,
                    DiagnosticImportance::Optional => {}
                },
            }
        }

        Self {
            results,
            required_failures,
            warnings,
            healthy,
            unavailable,
        }
    }

    pub fn recommended_repairs(&self) -> Vec<&RepairPlan> {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        self.results
            .iter()
            .filter(|result| result.importance != DiagnosticImportance::Optional)
            .filter(|result| {
                matches!(
                    result.status,
                    DiagnosticStatus::Missing
                        | DiagnosticStatus::Broken
                        | DiagnosticStatus::Warning
                )
            })
            .flat_map(|result| result.repairs.iter())
            .filter(|plan| plan.risk == RepairRisk::Normal)
            .filter(|plan| is_bulk_mutating_operation(&plan.operation))
            .filter(|plan| seen.insert(plan.id.clone()))
            .collect()
    }
}

fn is_bulk_mutating_operation(operation: &RepairOperation) -> bool {
    match operation {
        RepairOperation::OpenUrl(_)
        | RepairOperation::RevealPath(_)
        | RepairOperation::CopyCommand(_) => false,
        RepairOperation::UpdatePowerShellProfile {
            recipe: ProfileRecipe::UnsafeClaudeShortcut,
            ..
        } => false,
        RepairOperation::RunKnownCommand { .. }
        | RepairOperation::InstallWingetPackage { .. }
        | RepairOperation::UpdatePowerShellProfile { .. }
        | RepairOperation::SetDefaultTerminal(_)
        | RepairOperation::SetClaudeCommand(_)
        | RepairOperation::SetCodexCommand(_) => true,
    }
}

fn status_rank(result: &DiagnosticResult) -> u8 {
    match (result.importance, result.status) {
        (DiagnosticImportance::Required, DiagnosticStatus::Missing | DiagnosticStatus::Broken) => 0,
        (
            DiagnosticImportance::Recommended,
            DiagnosticStatus::Missing | DiagnosticStatus::Broken,
        ) => 1,
        (_, DiagnosticStatus::Warning) if result.importance != DiagnosticImportance::Optional => 2,
        (_, DiagnosticStatus::Running) => 3,
        (_, DiagnosticStatus::Healthy) => 4,
        (_, DiagnosticStatus::Unavailable) => 5,
        (DiagnosticImportance::Optional, _) => 6,
        _ => 7,
    }
}

fn importance_rank(importance: DiagnosticImportance) -> u8 {
    match importance {
        DiagnosticImportance::Required => 0,
        DiagnosticImportance::Recommended => 1,
        DiagnosticImportance::Optional => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        id: DiagnosticId,
        title: &str,
        importance: DiagnosticImportance,
        status: DiagnosticStatus,
        repairs: Vec<RepairPlan>,
    ) -> DiagnosticResult {
        DiagnosticResult {
            id,
            title: title.to_string(),
            importance,
            status,
            summary: title.to_string(),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs,
        }
    }

    fn repair(id: &str, risk: RepairRisk) -> RepairPlan {
        RepairPlan {
            id: id.to_string(),
            title: id.to_string(),
            risk,
            operation: RepairOperation::SetClaudeCommand("claude".to_string()),
            preview: format!("preview {id}"),
            verifies: Vec::new(),
        }
    }

    #[test]
    fn required_failures_sort_first() {
        let snapshot = DiagnosticSnapshot::from_results(vec![
            result(
                DiagnosticId::Docker,
                "Docker",
                DiagnosticImportance::Optional,
                DiagnosticStatus::Missing,
                vec![],
            ),
            result(
                DiagnosticId::ClaudeCli,
                "Claude",
                DiagnosticImportance::Required,
                DiagnosticStatus::Missing,
                vec![],
            ),
            result(
                DiagnosticId::Git,
                "Git",
                DiagnosticImportance::Recommended,
                DiagnosticStatus::Warning,
                vec![],
            ),
            result(
                DiagnosticId::PowerShell7,
                "PowerShell",
                DiagnosticImportance::Required,
                DiagnosticStatus::Healthy,
                vec![],
            ),
        ]);

        assert_eq!(snapshot.results[0].id, DiagnosticId::ClaudeCli);
        assert_eq!(snapshot.required_failures, 1);
        assert_eq!(snapshot.warnings, 1);
        assert_eq!(snapshot.healthy, 1);
    }

    #[test]
    fn missing_optional_does_not_increment_warnings() {
        let snapshot = DiagnosticSnapshot::from_results(vec![result(
            DiagnosticId::Python,
            "Python",
            DiagnosticImportance::Optional,
            DiagnosticStatus::Missing,
            vec![],
        )]);

        assert_eq!(snapshot.warnings, 0);
        assert_eq!(snapshot.required_failures, 0);
        assert_eq!(snapshot.healthy, 0);
    }

    #[test]
    fn high_risk_repairs_absent_from_recommended() {
        let snapshot = DiagnosticSnapshot::from_results(vec![result(
            DiagnosticId::CcShortcut,
            "cc",
            DiagnosticImportance::Recommended,
            DiagnosticStatus::Missing,
            vec![
                repair("safe-cc", RepairRisk::Normal),
                repair("unsafe-cc", RepairRisk::High),
            ],
        )]);

        let recommended: Vec<_> = snapshot
            .recommended_repairs()
            .into_iter()
            .map(|plan| plan.id.as_str())
            .collect();
        assert_eq!(recommended, vec!["safe-cc"]);
        assert!(!recommended.contains(&"unsafe-cc"));
    }

    #[test]
    fn recommended_excludes_non_mutating_healthy_and_duplicates() {
        let mut healthy = result(
            DiagnosticId::PowerShell7,
            "PowerShell",
            DiagnosticImportance::Required,
            DiagnosticStatus::Healthy,
            vec![RepairPlan {
                id: "set-default-terminal-pwsh".into(),
                title: "Set pwsh".into(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::SetDefaultTerminal(
                    crate::models::config::DefaultTerminal::Pwsh,
                ),
                preview: "set".into(),
                verifies: vec![DiagnosticId::PowerShell7],
            }],
        );
        let mut missing = result(
            DiagnosticId::ClaudeCli,
            "Claude",
            DiagnosticImportance::Required,
            DiagnosticStatus::Missing,
            vec![
                RepairPlan {
                    id: "open-docs".into(),
                    title: "Docs".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::OpenUrl("https://example.com".into()),
                    preview: "open".into(),
                    verifies: vec![],
                },
                RepairPlan {
                    id: "winget-install-Git.Git".into(),
                    title: "Install".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::InstallWingetPackage {
                        package_id: "Git.Git".into(),
                    },
                    preview: "winget".into(),
                    verifies: vec![],
                },
                RepairPlan {
                    id: "winget-install-Git.Git".into(),
                    title: "Install again".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::InstallWingetPackage {
                        package_id: "Git.Git".into(),
                    },
                    preview: "winget".into(),
                    verifies: vec![],
                },
                RepairPlan {
                    id: "profile-cc-unsafeclaudeshortcut".into(),
                    title: "Unsafe".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::UpdatePowerShellProfile {
                        path: PathBuf::from("p.ps1"),
                        recipe: ProfileRecipe::UnsafeClaudeShortcut,
                    },
                    preview: "unsafe".into(),
                    verifies: vec![],
                },
            ],
        );
        let _ = (&mut healthy, &mut missing);
        let snapshot = DiagnosticSnapshot::from_results(vec![healthy, missing]);
        let ids: Vec<_> = snapshot
            .recommended_repairs()
            .into_iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(ids, vec!["winget-install-Git.Git"]);
    }
}
