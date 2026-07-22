use crate::diagnostics::model::{
    DiagnosticId, DiagnosticImportance, DiagnosticStatus, RepairOperation, RepairPlan, RepairRisk,
};
use crate::models::config::DefaultTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: DiagnosticId,
    pub title: &'static str,
    pub importance: DiagnosticImportance,
    pub docs_url: &'static str,
    pub winget_package_id: Option<&'static str>,
    pub executable_names: &'static [&'static str],
    pub version_args: &'static [&'static str],
    pub windows_only: bool,
}

pub fn catalog() -> &'static [CatalogEntry] {
    &CATALOG
}

pub fn entry(id: DiagnosticId) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|entry| entry.id == id)
}

pub fn winget_package_ids() -> Vec<&'static str> {
    CATALOG
        .iter()
        .filter_map(|entry| entry.winget_package_id)
        .collect()
}

const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: DiagnosticId::ClaudeCli,
        title: "Claude CLI",
        importance: DiagnosticImportance::Required,
        docs_url: "https://docs.anthropic.com/en/docs/claude-code",
        winget_package_id: None,
        executable_names: &["claude", "claude.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::CodexCli,
        title: "Codex CLI",
        importance: DiagnosticImportance::Required,
        docs_url: "https://github.com/openai/codex",
        winget_package_id: None,
        executable_names: &["codex", "codex.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::PowerShell7,
        title: "PowerShell 7",
        importance: DiagnosticImportance::Required,
        docs_url: "https://learn.microsoft.com/powershell/",
        winget_package_id: Some("Microsoft.PowerShell"),
        executable_names: &["pwsh", "pwsh.exe"],
        version_args: &["-NoProfile", "-Command", "$PSVersionTable.PSVersion.ToString()"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::NodeNpm,
        title: "Node.js / npm",
        importance: DiagnosticImportance::Required,
        docs_url: "https://nodejs.org/",
        winget_package_id: Some("OpenJS.NodeJS.LTS"),
        executable_names: &["node", "node.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::Nvm,
        title: "NVM for Windows",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://github.com/coreybutler/nvm-windows",
        winget_package_id: Some("CoreyButler.NVMforWindows"),
        executable_names: &["nvm", "nvm.exe"],
        version_args: &["version"],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::PowerShellProfile,
        title: "PowerShell profile",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://learn.microsoft.com/powershell/module/microsoft.powershell.core/about/about_profiles",
        winget_package_id: None,
        executable_names: &["pwsh", "pwsh.exe"],
        version_args: &[],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::CcShortcut,
        title: "cc shortcut",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://docs.anthropic.com/en/docs/claude-code",
        winget_package_id: None,
        executable_names: &[],
        version_args: &[],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::Git,
        title: "Git",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://git-scm.com/downloads",
        winget_package_id: Some("Git.Git"),
        executable_names: &["git", "git.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::GitHubCli,
        title: "GitHub CLI",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://cli.github.com/",
        winget_package_id: Some("GitHub.cli"),
        executable_names: &["gh", "gh.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::Winget,
        title: "Windows Package Manager",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://learn.microsoft.com/windows/package-manager/winget/",
        winget_package_id: None,
        executable_names: &["winget", "winget.exe"],
        version_args: &["--version"],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::WebView2,
        title: "WebView2 Runtime",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://developer.microsoft.com/microsoft-edge/webview2/",
        winget_package_id: Some("Microsoft.EdgeWebView2Runtime"),
        executable_names: &[],
        version_args: &[],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::PathConsistency,
        title: "PATH consistency",
        importance: DiagnosticImportance::Recommended,
        docs_url: "https://learn.microsoft.com/windows/win32/procthread/environment-variables",
        winget_package_id: None,
        executable_names: &[],
        version_args: &[],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::Docker,
        title: "Docker",
        importance: DiagnosticImportance::Optional,
        docs_url: "https://docs.docker.com/get-docker/",
        winget_package_id: Some("Docker.DockerDesktop"),
        executable_names: &["docker", "docker.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::Wsl,
        title: "WSL",
        importance: DiagnosticImportance::Optional,
        docs_url: "https://learn.microsoft.com/windows/wsl/",
        winget_package_id: None,
        executable_names: &["wsl", "wsl.exe"],
        version_args: &["--status"],
        windows_only: true,
    },
    CatalogEntry {
        id: DiagnosticId::Rust,
        title: "Rust / Cargo",
        importance: DiagnosticImportance::Optional,
        docs_url: "https://rustup.rs/",
        winget_package_id: Some("Rustlang.Rustup"),
        executable_names: &["rustc", "rustc.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
    CatalogEntry {
        id: DiagnosticId::Python,
        title: "Python",
        importance: DiagnosticImportance::Optional,
        docs_url: "https://www.python.org/downloads/",
        winget_package_id: Some("Python.Python.3.12"),
        executable_names: &["python", "python.exe"],
        version_args: &["--version"],
        windows_only: false,
    },
];

pub fn open_docs_repair(entry: &CatalogEntry) -> RepairPlan {
    RepairPlan {
        id: format!(
            "open-docs-{}",
            entry.title.to_ascii_lowercase().replace(' ', "-")
        ),
        title: format!("Open {} docs", entry.title),
        risk: RepairRisk::Normal,
        operation: RepairOperation::OpenUrl(entry.docs_url.to_string()),
        preview: format!("Open {}", entry.docs_url),
        verifies: vec![entry.id],
    }
}

pub fn winget_install_repair(entry: &CatalogEntry) -> Option<RepairPlan> {
    let package_id = entry.winget_package_id?;
    Some(RepairPlan {
        id: format!("winget-install-{package_id}"),
        title: format!("Install {} with winget", entry.title),
        risk: RepairRisk::Normal,
        operation: RepairOperation::InstallWingetPackage {
            package_id: package_id.to_string(),
        },
        preview: format!(
            "winget install --id {package_id} --exact --accept-package-agreements --accept-source-agreements"
        ),
        verifies: vec![entry.id],
    })
}

pub fn set_default_terminal_pwsh_repair() -> RepairPlan {
    RepairPlan {
        id: "set-default-terminal-pwsh".to_string(),
        title: "Set default terminal to PowerShell 7".to_string(),
        risk: RepairRisk::Normal,
        operation: RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
        preview: "Set Settings.default_terminal = pwsh".to_string(),
        verifies: vec![DiagnosticId::PowerShell7],
    }
}

pub fn unavailable_result(
    entry: &CatalogEntry,
    reason: &str,
) -> crate::diagnostics::DiagnosticResult {
    crate::diagnostics::DiagnosticResult {
        id: entry.id,
        title: entry.title.to_string(),
        importance: entry.importance,
        status: DiagnosticStatus::Unavailable,
        summary: reason.to_string(),
        details: Vec::new(),
        detected_version: None,
        detected_path: None,
        repairs: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_official_docs_and_fixed_winget_ids() {
        for entry in catalog() {
            assert!(entry.docs_url.starts_with("https://"), "{}", entry.title);
            if let Some(id) = entry.winget_package_id {
                assert!(id.contains('.'), "winget id should look vendor-owned: {id}");
            }
        }
        assert!(winget_package_ids().contains(&"Microsoft.PowerShell"));
        assert!(winget_package_ids().contains(&"Git.Git"));
    }

    #[test]
    fn required_tools_are_marked_required() {
        for id in [
            DiagnosticId::ClaudeCli,
            DiagnosticId::CodexCli,
            DiagnosticId::PowerShell7,
            DiagnosticId::NodeNpm,
        ] {
            assert_eq!(
                entry(id).unwrap().importance,
                DiagnosticImportance::Required
            );
        }
    }
}
