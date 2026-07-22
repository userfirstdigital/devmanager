use crate::ai::codex_cli::split_command_line;
use crate::diagnostics::catalog::{
    entry, open_docs_repair, set_default_terminal_pwsh_repair, unavailable_result,
    winget_install_repair, CatalogEntry,
};
use crate::diagnostics::model::{
    DiagnosticId, DiagnosticImportance, DiagnosticResult, DiagnosticSnapshot, DiagnosticStatus,
    RepairOperation, RepairPlan, RepairRisk,
};
use crate::diagnostics::profile::CcClassification;
use crate::diagnostics::resolve;
use crate::diagnostics::runner::{sanitize_captured, CommandOutput, CommandRunner, CommandSpec};
use crate::diagnostics::windows;
use crate::models::config::{DefaultTerminal, Settings};
use crate::services::pwsh_probe;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const SCAN_BUDGET: Duration = Duration::from_secs(45);
// Windows GUI process: concurrent child pipes/handles can leave wait_with_output
// hung after the child has exited; serialize catalog probes to avoid that.
const MAX_CONCURRENT_PROBES: usize = 1;

pub struct DiagnosticProbe<R> {
    runner: R,
    /// Optional override for executable resolution (tests).
    which: Option<Box<dyn Fn(&str) -> Vec<PathBuf> + Send + Sync>>,
    read_file: Option<Box<dyn Fn(&Path) -> Result<String, String> + Send + Sync>>,
    /// Shared profile load + runtime `cc` classification for one scan instance.
    /// Never held across `.await`.
    profile_runtime_cache: Mutex<HashMap<PathBuf, windows::ProfileRuntimeProbeResult>>,
}

impl<R: CommandRunner> DiagnosticProbe<R> {
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            which: None,
            read_file: None,
            profile_runtime_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_resolver<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&str) -> Vec<PathBuf> + Send + Sync + 'static,
    {
        self.which = Some(Box::new(resolver));
        self
    }

    pub fn with_file_reader<F>(mut self, reader: F) -> Self
    where
        F: Fn(&Path) -> Result<String, String> + Send + Sync + 'static,
    {
        self.read_file = Some(Box::new(reader));
        self
    }

    pub async fn scan(&self, settings: &Settings) -> DiagnosticSnapshot {
        let catalog = crate::diagnostics::catalog::catalog();
        let ids: Vec<DiagnosticId> = catalog.iter().map(|item| item.id).collect();
        let (completed, abandoned) = map_bounded(ids, MAX_CONCURRENT_PROBES, SCAN_BUDGET, |id| {
            self.scan_one(id, settings)
        })
        .await;

        let mut results: Vec<_> = completed.into_iter().map(|(_, result)| result).collect();
        for id in abandoned {
            results.push(scan_timeout_result(id));
        }

        DiagnosticSnapshot::from_results(results)
    }

    pub async fn scan_one(&self, id: DiagnosticId, settings: &Settings) -> DiagnosticResult {
        let Some(item) = entry(id) else {
            return DiagnosticResult {
                id,
                title: format!("{id:?}"),
                importance: DiagnosticImportance::Optional,
                status: DiagnosticStatus::Unavailable,
                summary: "Unknown diagnostic".to_string(),
                details: Vec::new(),
                detected_version: None,
                detected_path: None,
                repairs: Vec::new(),
            };
        };

        if item.windows_only && !windows::is_windows() {
            return unavailable_result(
                item,
                &format!("{} is unavailable on this platform", item.title),
            );
        }

        match id {
            DiagnosticId::ClaudeCli => {
                self.probe_configured_cli(item, settings.claude_command.as_deref(), "claude")
                    .await
            }
            DiagnosticId::CodexCli => {
                self.probe_configured_cli(item, settings.codex_command.as_deref(), "codex")
                    .await
            }
            DiagnosticId::PowerShell7 => self.probe_powershell(item, settings).await,
            DiagnosticId::NodeNpm => self.probe_node_npm(item).await,
            DiagnosticId::Nvm => self.probe_simple(item).await,
            DiagnosticId::PowerShellProfile => self.probe_powershell_profile(item).await,
            DiagnosticId::CcShortcut => self.probe_cc_shortcut(item).await,
            DiagnosticId::Git => self.probe_git(item).await,
            DiagnosticId::GitHubCli => self.probe_github_cli(item).await,
            DiagnosticId::Winget => self.probe_simple(item).await,
            DiagnosticId::WebView2 => windows::webview2_result(item),
            DiagnosticId::PathConsistency => self.probe_path_consistency(item, settings).await,
            DiagnosticId::Docker
            | DiagnosticId::Wsl
            | DiagnosticId::Rust
            | DiagnosticId::Python => self.probe_optional(item).await,
        }
    }

    async fn probe_configured_cli(
        &self,
        item: &CatalogEntry,
        configured: Option<&str>,
        fallback_name: &str,
    ) -> DiagnosticResult {
        // Installation check prefers a resolved direct CLI on PATH.
        if let Some(path) = self.resolve_direct_cli(item, fallback_name) {
            return self
                .probe_direct_cli_version(item, path, configured, fallback_name)
                .await;
        }

        // No PATH CLI: only probe an explicitly configured direct executable.
        // Do not execute launch wrappers (cc, npx, …) as installation checks.
        let Some(command) = configured.map(str::trim).filter(|c| !c.is_empty()) else {
            return self.missing_cli(item, configured, fallback_name);
        };

        match split_command_line(command) {
            Ok(tokens) if !tokens.is_empty() => {
                let program_token = &tokens[0];
                if !is_direct_cli_program(program_token, fallback_name, item) {
                    return self.missing_cli(item, configured, fallback_name);
                }
                match self.resolve_configured_direct_program(program_token) {
                    Some(path) => {
                        self.probe_direct_cli_version(item, path, configured, fallback_name)
                            .await
                    }
                    None => self.missing_cli(item, configured, fallback_name),
                }
            }
            Ok(_) => self.missing_cli(item, configured, fallback_name),
            Err(err) => DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Broken,
                summary: "Configured command could not be parsed".to_string(),
                details: vec![err],
                detected_version: None,
                detected_path: None,
                repairs: self.cli_repairs(item, configured, None, fallback_name),
            },
        }
    }

    async fn probe_direct_cli_version(
        &self,
        item: &CatalogEntry,
        program: PathBuf,
        configured: Option<&str>,
        fallback_name: &str,
    ) -> DiagnosticResult {
        let args: Vec<OsString> = item
            .version_args
            .iter()
            .map(|arg| OsString::from(*arg))
            .collect();
        let output = self.run_program(&program, args).await;
        self.map_version_output(item, program, output, configured, None, fallback_name)
    }

    fn resolve_direct_cli(&self, item: &CatalogEntry, fallback_name: &str) -> Option<PathBuf> {
        self.resolve_executables(item)
            .into_iter()
            .next()
            .or_else(|| self.resolve_name(fallback_name).into_iter().next())
    }

    fn resolve_configured_direct_program(&self, program_token: &str) -> Option<PathBuf> {
        let program = PathBuf::from(program_token);
        if program.is_absolute() {
            return self.path_exists(&program).then_some(program);
        }
        self.resolve_name(&program.to_string_lossy())
            .into_iter()
            .next()
            .or_else(|| self.path_exists(&program).then_some(program))
    }

    fn missing_cli(
        &self,
        item: &CatalogEntry,
        configured: Option<&str>,
        fallback_name: &str,
    ) -> DiagnosticResult {
        let alternate = self
            .resolve_name(fallback_name)
            .into_iter()
            .next()
            .filter(|path| !configured_is_same_path(configured, path));
        DiagnosticResult {
            id: item.id,
            title: item.title.to_string(),
            importance: item.importance,
            status: DiagnosticStatus::Missing,
            summary: format!("{} was not found", item.title),
            details: configured
                .map(|c| format!("configured command: {}", sanitize_captured(c)))
                .into_iter()
                .collect(),
            detected_version: None,
            detected_path: None,
            repairs: self.cli_repairs(item, configured, alternate.as_deref(), fallback_name),
        }
    }

    fn cli_repairs(
        &self,
        item: &CatalogEntry,
        configured: Option<&str>,
        alternate_path: Option<&Path>,
        fallback_name: &str,
    ) -> Vec<RepairPlan> {
        let mut repairs = vec![open_docs_repair(item)];
        if let Some(plan) = winget_install_repair(item) {
            if windows::windows_only_repairs_allowed() || !item.windows_only {
                if windows::is_windows() {
                    repairs.insert(0, plan);
                }
            }
        }
        let proposed = alternate_path.map(|p| p.display().to_string()).or_else(|| {
            self.resolve_name(fallback_name)
                .into_iter()
                .next()
                .map(|p| p.display().to_string())
                .filter(|p| !configured_is_same_string(configured, p))
        });
        if let Some(command) = proposed {
            if item.id == DiagnosticId::ClaudeCli {
                repairs.push(RepairPlan {
                    id: "set-claude-command".to_string(),
                    title: "Set Claude command".to_string(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::SetClaudeCommand(command.clone()),
                    preview: "Update Settings.claude_command".to_string(),
                    verifies: vec![DiagnosticId::ClaudeCli],
                });
            }
            if item.id == DiagnosticId::CodexCli {
                repairs.push(RepairPlan {
                    id: "set-codex-command".to_string(),
                    title: "Set Codex command".to_string(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::SetCodexCommand(command),
                    preview: "Update Settings.codex_command".to_string(),
                    verifies: vec![DiagnosticId::CodexCli],
                });
            }
        }
        repairs
    }

    async fn probe_powershell(&self, item: &CatalogEntry, settings: &Settings) -> DiagnosticResult {
        let program = self.resolve_pwsh();
        let Some(program) = program else {
            let mut repairs = Vec::new();
            if windows::is_windows() && settings.default_terminal != DefaultTerminal::Pwsh {
                if let Some(plan) = winget_install_repair(item) {
                    repairs.push(plan);
                }
                repairs.push(set_default_terminal_pwsh_repair());
            } else if windows::is_windows() {
                if let Some(plan) = winget_install_repair(item) {
                    repairs.push(plan);
                }
            }
            repairs.push(open_docs_repair(item));
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Missing,
                summary: "PowerShell 7 (pwsh) was not found".to_string(),
                details: Vec::new(),
                detected_version: None,
                detected_path: None,
                repairs,
            };
        };
        let output = self
            .run_program(
                &program,
                item.version_args
                    .iter()
                    .map(|s| OsString::from(*s))
                    .collect(),
            )
            .await;
        let mut result = self.map_version_output(item, program, output, None, None, "pwsh");
        if result.status == DiagnosticStatus::Healthy
            && windows::is_windows()
            && settings.default_terminal != DefaultTerminal::Pwsh
        {
            result.repairs.push(set_default_terminal_pwsh_repair());
        }
        result
    }

    async fn probe_node_npm(&self, item: &CatalogEntry) -> DiagnosticResult {
        let node_paths = self.resolve_name("node");
        let npm_paths = self.resolve_name("npm");
        if node_paths.is_empty() || npm_paths.is_empty() {
            let mut repairs = vec![open_docs_repair(item)];
            if windows::is_windows() {
                if let Some(plan) = winget_install_repair(item) {
                    repairs.insert(0, plan);
                }
            }
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Missing,
                summary: "Node.js and/or npm were not found".to_string(),
                details: vec![
                    format!("node paths: {}", node_paths.len()),
                    format!("npm paths: {}", npm_paths.len()),
                ],
                detected_version: None,
                detected_path: node_paths.first().cloned(),
                repairs,
            };
        }
        let node = &node_paths[0];
        let npm = &npm_paths[0];
        let node_output = self
            .run_program(node, vec![OsString::from("--version")])
            .await;
        let npm_output = self
            .run_program(npm, vec![OsString::from("--version")])
            .await;
        self.map_node_npm_output(item, node.clone(), npm.clone(), node_output, npm_output)
    }

    fn map_node_npm_output(
        &self,
        item: &CatalogEntry,
        node: PathBuf,
        npm: PathBuf,
        node_output: CommandOutput,
        npm_output: CommandOutput,
    ) -> DiagnosticResult {
        let broken = |summary: &str, details: Vec<String>| DiagnosticResult {
            id: item.id,
            title: item.title.to_string(),
            importance: item.importance,
            status: DiagnosticStatus::Broken,
            summary: summary.to_string(),
            details,
            detected_version: None,
            detected_path: Some(node.clone()),
            repairs: self.cli_repairs(item, None, None, "node"),
        };

        if node_output.timed_out || npm_output.timed_out {
            return broken(
                "Node.js / npm probe timed out",
                vec!["command timed out".to_string()],
            );
        }
        if node_output.exit_code != Some(0) || npm_output.exit_code != Some(0) {
            let mut details = Vec::new();
            if node_output.exit_code != Some(0) {
                details.push(format!(
                    "node exit {:?}: {}",
                    node_output.exit_code,
                    sanitize_captured(node_output.stderr.trim())
                ));
            }
            if npm_output.exit_code != Some(0) {
                details.push(format!(
                    "npm exit {:?}: {}",
                    npm_output.exit_code,
                    sanitize_captured(npm_output.stderr.trim())
                ));
            }
            return broken("Node.js and/or npm returned a non-zero exit code", details);
        }

        let node_ver = sanitize_captured(node_output.stdout.lines().next().unwrap_or("").trim());
        let npm_ver = sanitize_captured(npm_output.stdout.lines().next().unwrap_or("").trim());
        DiagnosticResult {
            id: item.id,
            title: item.title.to_string(),
            importance: item.importance,
            status: DiagnosticStatus::Healthy,
            summary: format!("{} is available", item.title),
            details: vec![
                format!("node: {node_ver}"),
                format!("npm: {npm_ver}"),
                format!("npm path: {}", npm.display()),
            ],
            detected_version: Some(format!("node {node_ver} / npm {npm_ver}")),
            detected_path: Some(node),
            repairs: Vec::new(),
        }
    }

    async fn probe_simple(&self, item: &CatalogEntry) -> DiagnosticResult {
        let paths = self.resolve_executables(item);
        let Some(program) = paths.first().cloned() else {
            return self.missing_with_install(item);
        };
        let output = self
            .run_program(
                &program,
                item.version_args
                    .iter()
                    .map(|s| OsString::from(*s))
                    .collect(),
            )
            .await;
        self.map_version_output(item, program, output, None, None, item.executable_names[0])
    }

    async fn probe_optional(&self, item: &CatalogEntry) -> DiagnosticResult {
        let paths = self.resolve_executables(item);
        let Some(program) = paths.first().cloned() else {
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Missing,
                summary: format!("{} is not installed (optional)", item.title),
                details: Vec::new(),
                detected_version: None,
                detected_path: None,
                repairs: vec![open_docs_repair(item)],
            };
        };
        let output = self
            .run_program(
                &program,
                item.version_args
                    .iter()
                    .map(|s| OsString::from(*s))
                    .collect(),
            )
            .await;
        self.map_version_output(item, program, output, None, None, item.executable_names[0])
    }

    async fn probe_git(&self, item: &CatalogEntry) -> DiagnosticResult {
        let mut result = self.probe_simple(item).await;
        if result.status != DiagnosticStatus::Healthy {
            return result;
        }
        let program = result
            .detected_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("git"));
        let name = self
            .run_program(
                &program,
                vec![
                    OsString::from("config"),
                    OsString::from("--get"),
                    OsString::from("user.name"),
                ],
            )
            .await;
        let email = self
            .run_program(
                &program,
                vec![
                    OsString::from("config"),
                    OsString::from("--get"),
                    OsString::from("user.email"),
                ],
            )
            .await;
        let has_name = name.exit_code == Some(0) && !name.stdout.trim().is_empty();
        let has_email = email.exit_code == Some(0) && !email.stdout.trim().is_empty();
        if !has_name || !has_email {
            result.status = DiagnosticStatus::Warning;
            result.summary = "Git is installed but identity is incomplete".to_string();
            result
                .details
                .push("user.name/email presence checked; values are not displayed".to_string());
            result.repairs.push(RepairPlan {
                id: "copy-git-identity".to_string(),
                title: "Copy Git identity commands".to_string(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::CopyCommand(
                    "git config --global user.name \"Your Name\"\ngit config --global user.email \"you@example.com\""
                        .to_string(),
                ),
                preview: "Copy git config commands".to_string(),
                verifies: vec![DiagnosticId::Git],
            });
        }
        result
    }

    async fn probe_github_cli(&self, item: &CatalogEntry) -> DiagnosticResult {
        let mut result = self.probe_simple(item).await;
        if result.status != DiagnosticStatus::Healthy {
            return result;
        }
        let program = result
            .detected_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let auth = self
            .run_program(
                &program,
                vec![OsString::from("auth"), OsString::from("status")],
            )
            .await;
        if auth.timed_out {
            result.status = DiagnosticStatus::Warning;
            result.summary = "GitHub CLI auth status timed out".to_string();
        } else if auth.exit_code != Some(0) {
            result.status = DiagnosticStatus::Warning;
            result.summary = "GitHub CLI is installed but not authenticated".to_string();
            result
                .details
                .push("auth status checked; credentials are never captured".to_string());
            result.repairs.push(RepairPlan {
                id: "copy-gh-auth".to_string(),
                title: "Copy gh auth login".to_string(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::CopyCommand("gh auth login".to_string()),
                preview: "Copy: gh auth login".to_string(),
                verifies: vec![DiagnosticId::GitHubCli],
            });
        }
        result
    }

    async fn probe_powershell_profile(&self, item: &CatalogEntry) -> DiagnosticResult {
        let Some(pwsh) = self.resolve_pwsh() else {
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Unavailable,
                summary: "PowerShell 7 is required to inspect the profile".to_string(),
                details: Vec::new(),
                detected_version: None,
                detected_path: None,
                repairs: vec![open_docs_repair(item)],
            };
        };
        let profile_path = match windows::resolve_profile_path(&self.runner, &pwsh).await {
            Ok(path) => path,
            Err(err) => {
                return DiagnosticResult {
                    id: item.id,
                    title: item.title.to_string(),
                    importance: item.importance,
                    status: DiagnosticStatus::Broken,
                    summary: "Could not resolve PowerShell profile path".to_string(),
                    details: vec![err],
                    detected_version: None,
                    detected_path: None,
                    repairs: vec![open_docs_repair(item)],
                };
            }
        };
        let content_result = self.read_path(&profile_path);
        let file_exists = content_result.is_ok();
        let parse = if file_exists {
            windows::probe_profile_parse(&self.runner, &pwsh, &profile_path).await
        } else {
            Ok(())
        };
        let runtime = if parse.is_ok() && file_exists {
            Some(self.profile_runtime_probe(&pwsh, &profile_path).await)
        } else {
            None
        };
        windows::build_profile_result(
            item,
            Some(profile_path),
            file_exists,
            parse.is_ok(),
            parse.err(),
            runtime.as_ref().map(|r| &r.output),
        )
    }

    async fn probe_cc_shortcut(&self, item: &CatalogEntry) -> DiagnosticResult {
        let Some(pwsh) = self.resolve_pwsh() else {
            return windows::build_cc_shortcut_result(item, Err("pwsh unavailable".into()), None);
        };
        let profile = match windows::resolve_profile_path(&self.runner, &pwsh).await {
            Ok(path) => path,
            Err(_) => {
                return windows::build_cc_shortcut_result(item, Ok(CcClassification::Absent), None)
            }
        };
        // Genuinely missing profile files are Absent/Missing, not AST probe Warning.
        if !profile.is_file() {
            return windows::build_cc_shortcut_result(
                item,
                Ok(CcClassification::Absent),
                Some(profile),
            );
        }
        let runtime = self.profile_runtime_probe(&pwsh, &profile).await;
        windows::build_cc_shortcut_result(item, runtime.classification, Some(profile))
    }

    /// Shared bounded profile load + runtime `cc` classification for one probe instance.
    async fn profile_runtime_probe(
        &self,
        pwsh: &Path,
        profile: &Path,
    ) -> windows::ProfileRuntimeProbeResult {
        {
            let cache = self.profile_runtime_cache.lock().expect("profile cache");
            if let Some(cached) = cache.get(profile) {
                return cached.clone();
            }
        }
        let result = windows::probe_profile_runtime(&self.runner, pwsh, profile).await;
        let mut cache = self.profile_runtime_cache.lock().expect("profile cache");
        if let Some(cached) = cache.get(profile) {
            return cached.clone();
        }
        cache.insert(profile.to_path_buf(), result.clone());
        result
    }

    async fn probe_path_consistency(
        &self,
        item: &CatalogEntry,
        settings: &Settings,
    ) -> DiagnosticResult {
        let mut resolved = Vec::new();
        for name in ["claude", "codex", "pwsh", "node", "npm", "git", "gh"] {
            resolved.push((name.to_string(), self.resolve_name(name)));
        }
        if let Some(command) = settings.claude_command.as_deref() {
            if let Ok(tokens) = split_command_line(command) {
                if let Some(first) = tokens.first() {
                    resolved.push(("configured-claude".to_string(), self.resolve_name(first)));
                }
            }
        }
        windows::path_consistency_result(item, &resolved)
    }

    fn missing_with_install(&self, item: &CatalogEntry) -> DiagnosticResult {
        let mut repairs = vec![open_docs_repair(item)];
        if windows::is_windows() {
            if let Some(plan) = winget_install_repair(item) {
                repairs.insert(0, plan);
            }
        }
        DiagnosticResult {
            id: item.id,
            title: item.title.to_string(),
            importance: item.importance,
            status: DiagnosticStatus::Missing,
            summary: format!("{} was not found", item.title),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs,
        }
    }

    fn map_version_output(
        &self,
        item: &CatalogEntry,
        program: PathBuf,
        output: CommandOutput,
        configured: Option<&str>,
        alternate_path: Option<&Path>,
        fallback_name: &str,
    ) -> DiagnosticResult {
        if output.timed_out {
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Broken,
                summary: format!("{} probe timed out", item.title),
                details: vec!["command timed out".to_string()],
                detected_version: None,
                detected_path: Some(program),
                repairs: self.cli_repairs(item, configured, alternate_path, fallback_name),
            };
        }
        if output.exit_code != Some(0) {
            let mut details = Vec::new();
            if !output.stderr.trim().is_empty() {
                details.push(sanitize_captured(&output.stderr));
            } else if !output.stdout.trim().is_empty() {
                details.push(sanitize_captured(&output.stdout));
            }
            details.push(format!("exit code: {:?}", output.exit_code));
            return DiagnosticResult {
                id: item.id,
                title: item.title.to_string(),
                importance: item.importance,
                status: DiagnosticStatus::Broken,
                summary: format!("{} returned a non-zero exit code", item.title),
                details,
                detected_version: None,
                detected_path: Some(program),
                repairs: self.cli_repairs(item, configured, alternate_path, fallback_name),
            };
        }
        let version = sanitize_captured(output.stdout.lines().next().unwrap_or("").trim());
        DiagnosticResult {
            id: item.id,
            title: item.title.to_string(),
            importance: item.importance,
            status: DiagnosticStatus::Healthy,
            summary: format!("{} is available", item.title),
            details: Vec::new(),
            detected_version: if version.is_empty() {
                None
            } else {
                Some(version)
            },
            detected_path: Some(program),
            repairs: Vec::new(),
        }
    }

    async fn run_program(&self, program: &Path, args: Vec<OsString>) -> CommandOutput {
        self.runner
            .run(&CommandSpec {
                program: program.to_path_buf(),
                args,
                timeout: PROBE_TIMEOUT,
                env: BTreeMap::new(),
            })
            .await
            .unwrap_or_else(|err| CommandOutput {
                exit_code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: err.message,
            })
    }

    fn resolve_executables(&self, item: &CatalogEntry) -> Vec<PathBuf> {
        let mut all = Vec::new();
        for name in item.executable_names {
            for path in self.resolve_name(name) {
                if !all.contains(&path) {
                    all.push(path);
                }
            }
        }
        all
    }

    fn resolve_name(&self, name: &str) -> Vec<PathBuf> {
        if let Some(resolver) = &self.which {
            return resolver(name);
        }
        which_all(name)
    }

    fn resolve_pwsh(&self) -> Option<PathBuf> {
        if self.which.is_some() {
            return self.resolve_name("pwsh").into_iter().next();
        }
        resolve::resolve_all("pwsh")
            .into_iter()
            .next()
            .or_else(|| pwsh_probe::pwsh_program())
    }

    /// Public helper for repair verification so profile parse uses the same resolver as probes.
    pub fn resolved_pwsh(&self) -> Option<PathBuf> {
        self.resolve_pwsh()
    }

    pub fn resolved_tool(&self, name: &str) -> Option<PathBuf> {
        self.resolve_name(name).into_iter().next()
    }

    fn read_path(&self, path: &Path) -> Result<String, String> {
        if let Some(reader) = &self.read_file {
            return reader(path);
        }
        crate::diagnostics::profile::read_profile(path)
            .map(|(text, _, _)| text)
            .map_err(|err| err.to_string())
    }

    fn path_exists(&self, path: &Path) -> bool {
        if let Some(resolver) = &self.which {
            let key = path.to_string_lossy();
            if resolver(&key).iter().any(|p| p == path) {
                return true;
            }
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                return resolver(name).iter().any(|p| p == path);
            }
            return false;
        }
        path.is_file()
    }
}

fn which_all(name: &str) -> Vec<PathBuf> {
    resolve::resolve_all(name)
}

/// Run `work` for each item with at most `limit` tasks in flight, honoring `budget`.
/// Returns completed `(item, output)` pairs and items that did not finish in time.
async fn map_bounded<T, R, F, Fut>(
    items: Vec<T>,
    limit: usize,
    budget: Duration,
    work: F,
) -> (Vec<(T, R)>, Vec<T>)
where
    T: Copy + Eq + std::hash::Hash,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    use std::collections::VecDeque;

    let limit = limit.max(1);
    let mut pending: VecDeque<T> = items.into_iter().collect();
    let mut inflight = FuturesUnordered::new();
    let mut in_flight_items = HashSet::new();
    let mut completed = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;

    loop {
        while inflight.len() < limit {
            let Some(item) = pending.pop_front() else {
                break;
            };
            in_flight_items.insert(item);
            inflight.push({
                let fut = work(item);
                async move {
                    let output = fut.await;
                    (item, output)
                }
            });
        }

        if inflight.is_empty() {
            break;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, inflight.next()).await {
            Ok(Some((item, output))) => {
                in_flight_items.remove(&item);
                completed.push((item, output));
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let mut abandoned: Vec<T> = pending.into_iter().collect();
    abandoned.extend(in_flight_items);
    (completed, abandoned)
}

fn is_direct_cli_program(program_token: &str, fallback_name: &str, item: &CatalogEntry) -> bool {
    let file_name = Path::new(program_token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program_token);
    let normalized = normalize_cli_program_name(file_name);
    if normalized == normalize_cli_program_name(fallback_name) {
        return true;
    }
    item.executable_names
        .iter()
        .any(|name| normalized == normalize_cli_program_name(name))
}

fn normalize_cli_program_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_end_matches(".bat")
        .trim_end_matches(".ps1")
        .to_string()
}

fn scan_timeout_result(id: DiagnosticId) -> DiagnosticResult {
    let Some(item) = entry(id) else {
        return DiagnosticResult {
            id,
            title: format!("{id:?}"),
            importance: DiagnosticImportance::Optional,
            status: DiagnosticStatus::Unavailable,
            summary: "Diagnostic scan timed out".to_string(),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs: Vec::new(),
        };
    };
    unavailable_result(item, "Diagnostic scan timed out")
}

fn configured_is_same_path(configured: Option<&str>, path: &Path) -> bool {
    configured
        .and_then(|c| split_command_line(c).ok())
        .and_then(|tokens| tokens.first().cloned())
        .is_some_and(|program| {
            PathBuf::from(&program) == path || program == path.display().to_string()
        })
}

fn configured_is_same_string(configured: Option<&str>, proposed: &str) -> bool {
    configured
        .map(|c| c.trim())
        .is_some_and(|c| c == proposed || configured_is_same_path(Some(c), Path::new(proposed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::model::RepairRisk;
    use crate::diagnostics::runner::{CommandFailure, CommandRunnerFuture};
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeRunner {
        responses: Mutex<HashMap<String, CommandOutput>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn set(&self, key: &str, output: CommandOutput) {
            self.responses
                .lock()
                .unwrap()
                .insert(key.to_string(), output);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
            Box::pin(async move {
                let key = command_key(spec);
                self.calls.lock().unwrap().push(key.clone());
                self.responses
                    .lock()
                    .unwrap()
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| CommandFailure {
                        message: format!("no fake response for {key}"),
                    })
            })
        }
    }

    fn command_key(spec: &CommandSpec) -> String {
        let args: Vec<_> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        format!("{}|{}", spec.program.display(), args.join(" "))
    }

    fn ok_out(stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(0),
            timed_out: false,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn settings() -> Settings {
        Settings {
            claude_command: Some(r#"C:\tools\claude.exe"#.to_string()),
            codex_command: Some(r#"C:\tools\codex.exe"#.to_string()),
            ..Settings::default()
        }
    }

    fn probe_with(
        runner: FakeRunner,
        files: HashMap<PathBuf, String>,
        paths: HashMap<String, Vec<PathBuf>>,
    ) -> DiagnosticProbe<FakeRunner> {
        DiagnosticProbe::new(runner)
            .with_resolver(move |name| paths.get(name).cloned().unwrap_or_default())
            .with_file_reader(move |path| {
                files
                    .get(path)
                    .cloned()
                    .ok_or_else(|| "missing file".to_string())
            })
    }

    #[tokio::test]
    async fn healthy_version_output() {
        let runner = FakeRunner::new();
        let claude = PathBuf::from(r"C:\tools\claude.exe");
        let codex = PathBuf::from(r"C:\tools\codex.exe");
        runner.set(
            &format!("{}|--version", claude.display()),
            ok_out("claude 1.2.3\n"),
        );
        runner.set(
            &format!("{}|--version", codex.display()),
            ok_out("codex 0.1.0\n"),
        );
        let mut paths = HashMap::new();
        paths.insert(claude.display().to_string(), vec![claude.clone()]);
        paths.insert(codex.display().to_string(), vec![codex]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe.scan_one(DiagnosticId::ClaudeCli, &settings()).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_version.as_deref(), Some("claude 1.2.3"));
    }

    #[tokio::test]
    async fn missing_required_and_optional() {
        let runner = FakeRunner::new();
        let probe = probe_with(runner, HashMap::new(), HashMap::new());
        let mut cfg = Settings::default();
        cfg.claude_command = Some("missing-claude".to_string());
        let required = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        assert_eq!(required.status, DiagnosticStatus::Missing);
        assert_eq!(required.importance, DiagnosticImportance::Required);
        assert!(!required
            .repairs
            .iter()
            .any(|r| matches!(r.operation, RepairOperation::SetClaudeCommand(_))));

        let optional = probe.scan_one(DiagnosticId::Python, &cfg).await;
        assert_eq!(optional.status, DiagnosticStatus::Missing);
        assert_eq!(optional.importance, DiagnosticImportance::Optional);
        let snapshot = DiagnosticSnapshot::from_results(vec![required, optional]);
        assert_eq!(snapshot.required_failures, 1);
        assert_eq!(snapshot.warnings, 0);
    }

    #[tokio::test]
    async fn missing_cli_offers_alternate_path_repair() {
        let runner = FakeRunner::new();
        let fallback = PathBuf::from(r"C:\Program Files\claude\claude.exe");
        runner.set(
            &format!("{}|--version", fallback.display()),
            ok_out("claude 9.0.0\n"),
        );
        let mut paths = HashMap::new();
        paths.insert("claude".to_string(), vec![fallback.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        // Wrapper launch config is not an installation probe target; PATH claude is.
        let mut cfg = Settings::default();
        cfg.claude_command = Some("cc".to_string());
        let result = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_path.as_deref(), Some(fallback.as_path()));
    }

    #[tokio::test]
    async fn missing_direct_configured_path_offers_alternate_when_path_has_no_cli() {
        // Direct configured path missing and no PATH resolution for names → Missing,
        // but an alternate resolved only via a different lookup key can still be proposed.
        // Here neither configured path nor fallback names resolve.
        let runner = FakeRunner::new();
        let probe = probe_with(runner, HashMap::new(), HashMap::new());
        let mut cfg = Settings::default();
        cfg.claude_command = Some(r"C:\missing\claude.exe".to_string());
        let result = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Missing);
        assert!(probe.runner.calls().is_empty());
    }

    #[tokio::test]
    async fn nonzero_and_timeout_map_to_broken() {
        let claude = PathBuf::from(r"C:\tools\claude.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|--version", claude.display()),
            CommandOutput {
                exit_code: Some(2),
                timed_out: false,
                stdout: String::new(),
                stderr: "boom".to_string(),
            },
        );
        let mut paths = HashMap::new();
        paths.insert(claude.display().to_string(), vec![claude.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let broken = probe.scan_one(DiagnosticId::ClaudeCli, &settings()).await;
        assert_eq!(broken.status, DiagnosticStatus::Broken);

        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|--version", claude.display()),
            CommandOutput {
                exit_code: None,
                timed_out: true,
                stdout: String::new(),
                stderr: "command timed out".to_string(),
            },
        );
        let mut paths = HashMap::new();
        paths.insert(claude.display().to_string(), vec![claude]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let timed = probe.scan_one(DiagnosticId::ClaudeCli, &settings()).await;
        assert_eq!(timed.status, DiagnosticStatus::Broken);
        assert!(timed.summary.contains("timed out"));
    }

    #[tokio::test]
    async fn multiple_path_conflicts_warn() {
        let runner = FakeRunner::new();
        let mut paths = HashMap::new();
        paths.insert(
            "node".to_string(),
            vec![
                PathBuf::from(r"C:\nvm\node.exe"),
                PathBuf::from(r"C:\Program Files\nodejs\node.exe"),
            ],
        );
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe
            .scan_one(DiagnosticId::PathConsistency, &Settings::default())
            .await;
        if cfg!(windows) {
            assert_eq!(result.status, DiagnosticStatus::Warning);
            assert!(result.details.iter().any(|d| d.contains("node:")));
        } else {
            assert_eq!(result.status, DiagnosticStatus::Unavailable);
        }
    }

    #[tokio::test]
    async fn injected_resolver_ignores_host_pwsh() {
        let runner = FakeRunner::new();
        let pwsh = PathBuf::from(r"C:\injected\pwsh.exe");
        runner.set(
            &format!(
                "{}|-NoProfile -Command $PSVersionTable.PSVersion.ToString()",
                pwsh.display()
            ),
            ok_out("7.4.0\n"),
        );
        let mut paths = HashMap::new();
        paths.insert("pwsh".to_string(), vec![pwsh.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let mut cfg = Settings::default();
        cfg.default_terminal = DefaultTerminal::Pwsh;
        let result = probe.scan_one(DiagnosticId::PowerShell7, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_path.as_deref(), Some(pwsh.as_path()));
        assert!(!result
            .repairs
            .iter()
            .any(|r| r.id == "set-default-terminal-pwsh"));
    }

    #[tokio::test]
    async fn node_npm_dual_version() {
        let runner = FakeRunner::new();
        let node = PathBuf::from(r"C:\nodejs\node.exe");
        let npm = PathBuf::from(r"C:\nodejs\npm.cmd");
        runner.set(
            &format!("{}|--version", node.display()),
            ok_out("v20.0.0\n"),
        );
        runner.set(&format!("{}|--version", npm.display()), ok_out("10.1.0\n"));
        let mut paths = HashMap::new();
        paths.insert("node".to_string(), vec![node]);
        paths.insert("npm".to_string(), vec![npm]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe
            .scan_one(DiagnosticId::NodeNpm, &Settings::default())
            .await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(
            result.detected_version.as_deref(),
            Some("node v20.0.0 / npm 10.1.0")
        );
        assert!(result.details.iter().any(|d| d.starts_with("node:")));
        assert!(result.details.iter().any(|d| d.starts_with("npm:")));
    }

    #[tokio::test]
    async fn profile_load_uses_noprofile() {
        if !cfg!(windows) {
            return;
        }
        let profile =
            PathBuf::from(r"C:\Users\dev\Documents\PowerShell\Microsoft.PowerShell_profile.ps1");
        let pwsh = PathBuf::from(r"C:\pwsh\pwsh.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|-NoProfile -Command $PROFILE", pwsh.display()),
            ok_out(&format!("{}\n", profile.display())),
        );
        runner.set(
            &format!(
                "{}|-NoProfile -Command {}",
                pwsh.display(),
                windows::profile_parse_script(&profile)
            ),
            ok_out(""),
        );
        runner.set(
            &format!(
                "{}|-NoProfile -NoLogo -Command {}",
                pwsh.display(),
                windows::runtime_profile_probe_script(&profile)
            ),
            ok_out("CC=0\nBLOCK_START=-1\nBLOCK_END=-1\n"),
        );
        let mut files = HashMap::new();
        files.insert(profile.clone(), "# profile\n".to_string());
        let mut paths = HashMap::new();
        paths.insert("pwsh".to_string(), vec![pwsh]);
        let probe = probe_with(runner, files, paths);
        let result = probe
            .scan_one(DiagnosticId::PowerShellProfile, &Settings::default())
            .await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert!(probe
            .runner
            .calls()
            .iter()
            .any(|c| c.contains("-NoProfile -NoLogo -Command")));
        assert!(probe
            .runner
            .calls()
            .iter()
            .any(|c| c.contains("Get-Command")));
    }

    #[tokio::test]
    async fn profile_and_cc_scan_share_one_runtime_profile_probe() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("Microsoft.PowerShell_profile.ps1");
        std::fs::write(&profile, "function cc { claude @args }\n").unwrap();
        let pwsh = PathBuf::from(r"C:\pwsh\pwsh.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|-NoProfile -Command $PROFILE", pwsh.display()),
            ok_out(&format!("{}\n", profile.display())),
        );
        runner.set(
            &format!(
                "{}|-NoProfile -Command {}",
                pwsh.display(),
                windows::profile_parse_script(&profile)
            ),
            ok_out(""),
        );
        runner.set(
            &format!(
                "{}|-NoProfile -NoLogo -Command {}",
                pwsh.display(),
                windows::runtime_profile_probe_script(&profile)
            ),
            ok_out(
                "CC=1\nHAS_CLAUDE=1\nUNSAFE=0\nCC_START=0\nCC_END=30\nBLOCK_START=-1\nBLOCK_END=-1\n",
            ),
        );
        let mut files = HashMap::new();
        files.insert(
            profile.clone(),
            "function cc { claude @args }\n".to_string(),
        );
        let mut paths = HashMap::new();
        paths.insert("pwsh".to_string(), vec![pwsh]);
        let probe = probe_with(runner, files, paths);

        let profile_result = probe
            .scan_one(DiagnosticId::PowerShellProfile, &Settings::default())
            .await;
        let cc_result = probe
            .scan_one(DiagnosticId::CcShortcut, &Settings::default())
            .await;

        assert_eq!(profile_result.status, DiagnosticStatus::Healthy);
        assert_eq!(cc_result.status, DiagnosticStatus::Healthy);

        let runtime_calls: Vec<_> = probe
            .runner
            .calls()
            .into_iter()
            .filter(|c| c.contains("Get-Command") && c.contains("-NoLogo"))
            .collect();
        assert_eq!(
            runtime_calls.len(),
            1,
            "profile+cc must share one runtime probe, got {runtime_calls:?}"
        );
    }

    #[tokio::test]
    async fn profile_missing_file_is_missing() {
        if !cfg!(windows) {
            return;
        }
        let profile =
            PathBuf::from(r"C:\Users\dev\Documents\PowerShell\Microsoft.PowerShell_profile.ps1");
        let pwsh = PathBuf::from(r"C:\pwsh\pwsh.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|-NoProfile -Command $PROFILE", pwsh.display()),
            ok_out(&format!("{}\n", profile.display())),
        );
        let mut paths = HashMap::new();
        paths.insert("pwsh".to_string(), vec![pwsh]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe
            .scan_one(DiagnosticId::PowerShellProfile, &Settings::default())
            .await;
        assert_eq!(result.status, DiagnosticStatus::Missing);
    }

    #[tokio::test]
    async fn concurrent_scan_completes() {
        let runner = FakeRunner::new();
        let claude = PathBuf::from(r"C:\tools\claude.exe");
        let codex = PathBuf::from(r"C:\tools\codex.exe");
        runner.set(
            &format!("{}|--version", claude.display()),
            ok_out("claude 1.0\n"),
        );
        runner.set(
            &format!("{}|--version", codex.display()),
            ok_out("codex 1.0\n"),
        );
        let mut paths = HashMap::new();
        paths.insert(claude.display().to_string(), vec![claude]);
        paths.insert(codex.display().to_string(), vec![codex]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let snapshot = probe.scan(&settings()).await;
        assert_eq!(
            snapshot.results.len(),
            crate::diagnostics::catalog::catalog().len()
        );
        assert!(snapshot
            .results
            .iter()
            .any(|r| r.id == DiagnosticId::ClaudeCli && r.status == DiagnosticStatus::Healthy));
    }

    #[tokio::test]
    async fn map_bounded_serializes_catalog_probe_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let items: Vec<u32> = (0..12).collect();

        let active_work = active.clone();
        let max_work = max_active.clone();
        let (completed, abandoned) = map_bounded(
            items.clone(),
            MAX_CONCURRENT_PROBES,
            Duration::from_secs(45),
            move |id| {
                let active = active_work.clone();
                let max_active = max_work.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    id
                }
            },
        )
        .await;

        assert!(abandoned.is_empty(), "budget should cover all probe work");
        assert_eq!(completed.len(), items.len());
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "catalog probes must run one at a time"
        );
    }

    #[tokio::test]
    async fn configured_cc_uses_path_claude_not_wrapper() {
        let runner = FakeRunner::new();
        let claude = PathBuf::from(r"C:\tools\claude.exe");
        runner.set(
            &format!("{}|--version", claude.display()),
            ok_out("claude 2.0.0\n"),
        );
        runner.set("cc|--version", ok_out("wrapper must not run\n"));
        let mut paths = HashMap::new();
        paths.insert("claude".to_string(), vec![claude.clone()]);
        paths.insert("claude.exe".to_string(), vec![claude.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let mut cfg = Settings::default();
        cfg.claude_command = Some("cc".to_string());
        let result = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_path.as_deref(), Some(claude.as_path()));
        let calls = probe.runner.calls();
        assert_eq!(
            calls,
            vec![format!("{}|--version", claude.display())],
            "installation probe must run direct claude with catalog version args only"
        );
    }

    #[tokio::test]
    async fn configured_npx_uses_path_codex_not_wrapper() {
        let runner = FakeRunner::new();
        let codex = PathBuf::from(r"C:\tools\codex.cmd");
        runner.set(
            &format!("{}|--version", codex.display()),
            ok_out("codex 0.9.0\n"),
        );
        runner.set(
            "npx|-y @openai/codex@latest --yolo --version",
            ok_out("wrapper must not run\n"),
        );
        runner.set("npx|--version", ok_out("wrapper must not run\n"));
        let mut paths = HashMap::new();
        paths.insert("codex".to_string(), vec![codex.clone()]);
        paths.insert("codex.exe".to_string(), vec![codex.clone()]);
        paths.insert("codex.cmd".to_string(), vec![codex.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let mut cfg = Settings::default();
        cfg.codex_command = Some("npx -y @openai/codex@latest --yolo".to_string());
        let result = probe.scan_one(DiagnosticId::CodexCli, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_path.as_deref(), Some(codex.as_path()));
        let calls = probe.runner.calls();
        assert_eq!(
            calls,
            vec![format!("{}|--version", codex.display())],
            "installation probe must run direct codex with catalog version args only"
        );
        assert!(
            calls
                .iter()
                .all(|c| !c.to_ascii_lowercase().contains("npx")),
            "must not execute npx wrapper as an installation probe: {calls:?}"
        );
    }

    #[tokio::test]
    async fn configured_direct_cli_used_when_no_path_fallback() {
        let runner = FakeRunner::new();
        let claude = PathBuf::from(r"C:\custom\claude.exe");
        runner.set(
            &format!("{}|--version", claude.display()),
            ok_out("claude 3.0.0\n"),
        );
        let mut paths = HashMap::new();
        paths.insert(claude.display().to_string(), vec![claude.clone()]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let mut cfg = Settings::default();
        cfg.claude_command = Some(r"C:\custom\claude.exe --dangerously-skip-permissions".into());
        let result = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        assert_eq!(result.status, DiagnosticStatus::Healthy);
        assert_eq!(result.detected_path.as_deref(), Some(claude.as_path()));
        let calls = probe.runner.calls();
        assert_eq!(calls, vec![format!("{}|--version", claude.display())]);
        assert!(
            calls.iter().all(|c| !c.contains("dangerously")),
            "must not pass configured launch args to the installation probe"
        );
    }

    #[tokio::test]
    async fn configured_wrapper_without_path_cli_is_missing_without_executing_wrapper() {
        let runner = FakeRunner::new();
        runner.set("cc|--version", ok_out("wrapper must not run\n"));
        runner.set(
            "npx|-y @openai/codex@latest --yolo --version",
            ok_out("wrapper must not run\n"),
        );
        let probe = probe_with(runner, HashMap::new(), HashMap::new());
        let mut cfg = Settings::default();
        cfg.claude_command = Some("cc".into());
        cfg.codex_command = Some("npx -y @openai/codex@latest --yolo".into());

        let claude = probe.scan_one(DiagnosticId::ClaudeCli, &cfg).await;
        let codex = probe.scan_one(DiagnosticId::CodexCli, &cfg).await;
        assert_eq!(claude.status, DiagnosticStatus::Missing);
        assert_eq!(codex.status, DiagnosticStatus::Missing);
        assert!(
            probe.runner.calls().is_empty(),
            "wrappers must not be executed: {:?}",
            probe.runner.calls()
        );
    }

    #[tokio::test]
    async fn github_auth_warning_without_tokens() {
        let runner = FakeRunner::new();
        let gh = PathBuf::from(r"C:\tools\gh.exe");
        runner.set(
            &format!("{}|--version", gh.display()),
            ok_out("gh version 2.0.0\n"),
        );
        runner.set(
            &format!("{}|auth status", gh.display()),
            CommandOutput {
                exit_code: Some(1),
                timed_out: false,
                stdout: String::new(),
                stderr: "not logged in".to_string(),
            },
        );
        let mut paths = HashMap::new();
        paths.insert("gh".to_string(), vec![gh.clone()]);
        paths.insert("gh.exe".to_string(), vec![gh]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe
            .scan_one(DiagnosticId::GitHubCli, &Settings::default())
            .await;
        assert_eq!(result.status, DiagnosticStatus::Warning);
        assert!(!result
            .details
            .iter()
            .any(|d| d.to_ascii_lowercase().contains("token")));
        assert!(result.summary.contains("not authenticated"));
    }

    #[tokio::test]
    async fn windows_only_repairs_unavailable_off_windows() {
        let runner = FakeRunner::new();
        let probe = probe_with(runner, HashMap::new(), HashMap::new());
        let result = probe
            .scan_one(DiagnosticId::Winget, &Settings::default())
            .await;
        if cfg!(windows) {
            assert!(matches!(
                result.status,
                DiagnosticStatus::Missing
                    | DiagnosticStatus::Healthy
                    | DiagnosticStatus::Broken
                    | DiagnosticStatus::Unavailable
            ));
        } else {
            assert_eq!(result.status, DiagnosticStatus::Unavailable);
            assert!(result.repairs.is_empty());
        }
    }

    #[tokio::test]
    async fn high_risk_cc_repair_not_in_recommended_batch() {
        let entry = entry(DiagnosticId::CcShortcut).unwrap();
        let result =
            windows::build_cc_shortcut_result(entry, Ok(CcClassification::UnmarkedUnsafe), None);
        assert!(result.repairs.iter().any(|r| r.risk == RepairRisk::High));
        let snapshot = DiagnosticSnapshot::from_results(vec![result]);
        assert!(snapshot
            .recommended_repairs()
            .iter()
            .all(|r| r.risk != RepairRisk::High));
    }

    #[tokio::test]
    async fn cc_shortcut_missing_profile_file_is_missing_not_ast_warning() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("missing_profile.ps1");
        assert!(!profile.is_file());
        let pwsh = PathBuf::from(r"C:\pwsh\pwsh.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!("{}|-NoProfile -Command $PROFILE", pwsh.display()),
            ok_out(&format!("{}\n", profile.display())),
        );
        // No AST probe response: a missing profile must not invoke the probe (Warning).
        let mut paths = HashMap::new();
        paths.insert("pwsh".to_string(), vec![pwsh]);
        let probe = probe_with(runner, HashMap::new(), paths);
        let result = probe
            .scan_one(DiagnosticId::CcShortcut, &Settings::default())
            .await;
        assert_eq!(result.status, DiagnosticStatus::Missing);
        assert!(!result
            .details
            .iter()
            .any(|d| d.to_ascii_lowercase().contains("ast probe")));
        assert_eq!(result.detected_path.as_deref(), Some(profile.as_path()));
    }

    #[test]
    fn build_profile_result_missing_signature() {
        let entry = entry(DiagnosticId::PowerShellProfile).unwrap();
        let result = windows::build_profile_result(
            entry,
            Some(PathBuf::from("p.ps1")),
            false,
            true,
            None,
            None,
        );
        assert_eq!(result.status, DiagnosticStatus::Missing);
    }
}
