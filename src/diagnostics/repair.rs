use crate::diagnostics::catalog::winget_package_ids;
use crate::diagnostics::model::{
    DiagnosticSnapshot, DiagnosticStatus, RepairOperation, RepairOutcome, RepairPlan,
};
use crate::diagnostics::probe::DiagnosticProbe;
use crate::diagnostics::profile::{apply_profile_edit, recipe_body, rollback_profile_edit};
use crate::diagnostics::resolve;
use crate::diagnostics::runner::{CommandRunner, CommandSpec};
use crate::diagnostics::windows;
use crate::models::config::{DefaultTerminal, Settings};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const REPAIR_TIMEOUT: Duration = Duration::from_secs(300);

pub trait SettingsRepairSink: Send + Sync {
    fn settings(&self) -> &Settings;
    fn set_default_terminal(&mut self, value: DefaultTerminal) -> Result<(), String>;
    fn set_claude_command(&mut self, value: String) -> Result<(), String>;
    fn set_codex_command(&mut self, value: String) -> Result<(), String>;
}

impl SettingsRepairSink for Settings {
    fn settings(&self) -> &Settings {
        self
    }

    fn set_default_terminal(&mut self, value: DefaultTerminal) -> Result<(), String> {
        self.default_terminal = value;
        Ok(())
    }

    fn set_claude_command(&mut self, value: String) -> Result<(), String> {
        self.claude_command = Some(value);
        Ok(())
    }

    fn set_codex_command(&mut self, value: String) -> Result<(), String> {
        self.codex_command = Some(value);
        Ok(())
    }
}

pub struct RepairExecutor<'a, R, S> {
    runner: &'a R,
    probe: &'a DiagnosticProbe<R>,
    settings: &'a mut S,
}

impl<'a, R, S> RepairExecutor<'a, R, S>
where
    R: CommandRunner,
    S: SettingsRepairSink,
{
    pub fn new(runner: &'a R, probe: &'a DiagnosticProbe<R>, settings: &'a mut S) -> Self {
        Self {
            runner,
            probe,
            settings,
        }
    }

    pub fn preview(plan: &RepairPlan) -> String {
        format!("{} — {}", plan.title, plan.preview)
    }

    pub async fn execute(&mut self, plan: &RepairPlan) -> RepairOutcome {
        if let Err(message) = validate_plan(plan) {
            return RepairOutcome {
                plan_id: plan.id.clone(),
                success: false,
                requires_restart: false,
                summary: message,
                details: Vec::new(),
            };
        }

        match &plan.operation {
            RepairOperation::OpenUrl(_)
            | RepairOperation::RevealPath(_)
            | RepairOperation::CopyCommand(_) => {
                return RepairOutcome {
                    plan_id: plan.id.clone(),
                    success: true,
                    requires_restart: false,
                    summary: "Non-mutating action deferred to UI".to_string(),
                    details: vec![Self::preview(plan)],
                };
            }
            _ => {}
        }

        if let Err(message) = self.execute_mutation(plan.operation.clone()).await {
            return RepairOutcome {
                plan_id: plan.id.clone(),
                success: false,
                requires_restart: false,
                summary: message,
                details: Vec::new(),
            };
        }

        let is_winget = matches!(plan.operation, RepairOperation::InstallWingetPackage { .. });

        let mut unhealthy = Vec::new();
        for id in &plan.verifies {
            let result = self.probe.scan_one(*id, self.settings.settings()).await;
            if result.status != DiagnosticStatus::Healthy {
                unhealthy.push(format!(
                    "{:?}: {} ({:?})",
                    id,
                    crate::diagnostics::runner::sanitize_captured(&result.summary),
                    result.status
                ));
            }
        }

        finalize_repair_verification(plan, is_winget, unhealthy)
    }

    pub async fn execute_recommended(
        &mut self,
        snapshot: &DiagnosticSnapshot,
    ) -> Vec<RepairOutcome> {
        let plans: Vec<RepairPlan> = snapshot
            .recommended_repairs()
            .into_iter()
            .cloned()
            .collect();

        let mut outcomes = Vec::new();
        for plan in plans {
            let outcome = self.execute(&plan).await;
            let failed = !outcome.success;
            outcomes.push(outcome);
            if failed {
                break;
            }
        }
        outcomes
    }

    async fn execute_mutation(&mut self, operation: RepairOperation) -> Result<(), String> {
        match operation {
            RepairOperation::RunKnownCommand { program, args } => {
                validate_known_command(&program, &args)?;
                let output = self
                    .runner
                    .run(&CommandSpec {
                        program,
                        args: args.into_iter().map(OsString::from).collect(),
                        timeout: REPAIR_TIMEOUT,
                        env: Default::default(),
                    })
                    .await
                    .map_err(|err| err.message)?;
                if output.timed_out {
                    return Err("known command timed out".to_string());
                }
                if output.exit_code != Some(0) {
                    return Err(format!(
                        "known command failed (exit {:?}): {}",
                        output.exit_code,
                        output.stderr.chars().take(200).collect::<String>()
                    ));
                }
                Ok(())
            }
            RepairOperation::InstallWingetPackage { package_id } => {
                if !winget_package_ids().contains(&package_id.as_str()) {
                    return Err(format!("unrecognized winget package id: {package_id}"));
                }
                let winget = self
                    .probe
                    .resolved_tool("winget")
                    .or_else(which_winget)
                    .ok_or_else(|| "winget was not found".to_string())?;
                let output = self
                    .runner
                    .run(&CommandSpec {
                        program: winget,
                        args: vec![
                            OsString::from("install"),
                            OsString::from("--id"),
                            OsString::from(&package_id),
                            OsString::from("--exact"),
                            OsString::from("--accept-package-agreements"),
                            OsString::from("--accept-source-agreements"),
                        ],
                        timeout: REPAIR_TIMEOUT,
                        env: Default::default(),
                    })
                    .await
                    .map_err(|err| err.message)?;
                if output.timed_out {
                    return Err("winget install timed out".to_string());
                }
                if output.exit_code != Some(0) {
                    return Err(format!(
                        "winget install failed (exit {:?})",
                        output.exit_code
                    ));
                }
                Ok(())
            }
            RepairOperation::UpdatePowerShellProfile { path, recipe } => {
                let _ = recipe_body(recipe);
                let pwsh = self
                    .probe
                    .resolved_pwsh()
                    .ok_or_else(|| "pwsh required for profile verification".to_string())?;
                let apply =
                    apply_profile_edit(&path, recipe, |_| Ok(())).map_err(|err| err.to_string())?;
                if let Err(parse_err) =
                    windows::probe_profile_parse(self.runner, &pwsh, &path).await
                {
                    rollback_profile_edit(&path, &apply).map_err(|err| err.to_string())?;
                    return Err(format!("profile parse failed after edit: {parse_err}"));
                }
                Ok(())
            }
            RepairOperation::SetDefaultTerminal(value) => self.settings.set_default_terminal(value),
            RepairOperation::SetClaudeCommand(value) => self.settings.set_claude_command(value),
            RepairOperation::SetCodexCommand(value) => self.settings.set_codex_command(value),
            RepairOperation::OpenUrl(_)
            | RepairOperation::RevealPath(_)
            | RepairOperation::CopyCommand(_) => Ok(()),
        }
    }
}

pub fn validate_plan(plan: &RepairPlan) -> Result<(), String> {
    match &plan.operation {
        RepairOperation::InstallWingetPackage { package_id } => {
            if winget_package_ids().contains(&package_id.as_str()) {
                Ok(())
            } else {
                Err(format!("unrecognized winget package id: {package_id}"))
            }
        }
        RepairOperation::RunKnownCommand { program, args } => validate_known_command(program, args),
        RepairOperation::OpenUrl(url) => {
            if url.starts_with("https://") {
                Ok(())
            } else {
                Err("only https URLs are allowed".to_string())
            }
        }
        _ => Ok(()),
    }
}

pub fn validate_known_command(program: &PathBuf, args: &[String]) -> Result<(), String> {
    let name = program
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match name.as_str() {
        "claude" => {
            if args == ["update"] || args == ["--version"] {
                Ok(())
            } else {
                Err(format!("unrecognized claude command shape: {args:?}"))
            }
        }
        "winget" => Err("use InstallWingetPackage instead of RunKnownCommand for winget".into()),
        "pwsh" | "powershell" => Err("refusing shell-shaped known commands".into()),
        "cmd" => Err("refusing cmd.exe known commands".into()),
        _ => Err(format!("unrecognized known command program: {name}")),
    }
}

/// Winget exit-0 is authoritative even when the current process PATH still looks Missing.
/// Non-winget mutations still require Healthy verification.
pub const WINGET_RESTART_NOTICE: &str =
    "install completed; restart DevManager to refresh PATH/detection";

const REPAIR_NOTICE_MAX_CHARS: usize = 200;

fn finalize_repair_verification(
    plan: &RepairPlan,
    is_winget: bool,
    unhealthy: Vec<String>,
) -> RepairOutcome {
    if unhealthy.is_empty() {
        return RepairOutcome {
            plan_id: plan.id.clone(),
            success: true,
            requires_restart: false,
            summary: format!("{} completed", plan.title),
            details: vec![format!("{} — {}", plan.title, plan.preview)],
        };
    }
    if is_winget {
        let mut details = vec![WINGET_RESTART_NOTICE.to_string()];
        details.extend(unhealthy);
        return RepairOutcome {
            plan_id: plan.id.clone(),
            success: true,
            requires_restart: true,
            summary: WINGET_RESTART_NOTICE.to_string(),
            details,
        };
    }
    RepairOutcome {
        plan_id: plan.id.clone(),
        success: false,
        requires_restart: false,
        summary: "Repair ran but verification did not reach Healthy".to_string(),
        details: unhealthy,
    }
}

fn bound_repair_notice(raw: &str) -> String {
    let sanitized = crate::diagnostics::runner::sanitize_captured(raw);
    let count = sanitized.chars().count();
    if count <= REPAIR_NOTICE_MAX_CHARS {
        return sanitized;
    }
    let truncated: String = sanitized.chars().take(REPAIR_NOTICE_MAX_CHARS).collect();
    format!("{truncated}…")
}

pub fn winget_install_args(package_id: &str) -> Vec<String> {
    vec![
        "install".into(),
        "--id".into(),
        package_id.into(),
        "--exact".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
    ]
}

/// Fields actually targeted by settings-mutation repair plans.
/// Outer `None` means leave the current authoritative Settings value alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticsSettingsDelta {
    pub default_terminal: Option<DefaultTerminal>,
    pub claude_command: Option<Option<String>>,
    pub codex_command: Option<Option<String>>,
}

/// Result of running a repair batch against a Settings sink: completed plans in order,
/// plus optional stop-failure text. Plan failures are not infrastructure errors.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticsRepairBatchResult {
    pub settings: Settings,
    pub completed: Vec<RepairPlan>,
    pub failure: Option<String>,
    /// Bounded, sanitized notices for successful plans that require a DevManager restart.
    pub notices: Vec<String>,
    /// Successful outcomes that require restart (typed companion to [`Self::notices`]).
    pub restart_outcomes: Vec<RepairOutcome>,
}

/// Run plans in order against `settings`, stopping at the first failure.
/// Completed plans (and their settings mutations) are retained on the returned Settings.
pub async fn execute_repair_batch<R: CommandRunner>(
    runner: &R,
    probe: &DiagnosticProbe<R>,
    mut settings: Settings,
    plans: Vec<RepairPlan>,
) -> DiagnosticsRepairBatchResult {
    let mut completed = Vec::new();
    let mut failure = None;
    let mut notices = Vec::new();
    let mut restart_outcomes = Vec::new();
    {
        let mut executor = RepairExecutor::new(runner, probe, &mut settings);
        for plan in plans {
            let outcome = executor.execute(&plan).await;
            if outcome.success {
                if outcome.requires_restart {
                    notices.push(bound_repair_notice(&outcome.summary));
                    restart_outcomes.push(outcome);
                }
                completed.push(plan);
            } else {
                failure = Some(outcome.summary);
                break;
            }
        }
    }
    DiagnosticsRepairBatchResult {
        settings,
        completed,
        failure,
        notices,
        restart_outcomes,
    }
}

impl DiagnosticsSettingsDelta {
    pub fn is_empty(&self) -> bool {
        self.default_terminal.is_none()
            && self.claude_command.is_none()
            && self.codex_command.is_none()
    }

    pub fn apply_to(&self, settings: &mut Settings) {
        if let Some(value) = &self.default_terminal {
            settings.default_terminal = value.clone();
        }
        if let Some(value) = &self.claude_command {
            settings.claude_command = value.clone();
        }
        if let Some(value) = &self.codex_command {
            settings.codex_command = value.clone();
        }
    }
}

/// Derive a delta from confirmed plans and the Settings sink after those plans executed.
pub fn diagnostics_settings_delta_from_plans(
    plans: &[RepairPlan],
    repaired: &Settings,
) -> DiagnosticsSettingsDelta {
    let mut delta = DiagnosticsSettingsDelta::default();
    for plan in plans {
        match &plan.operation {
            RepairOperation::SetDefaultTerminal(_) => {
                delta.default_terminal = Some(repaired.default_terminal.clone());
            }
            RepairOperation::SetClaudeCommand(_) => {
                delta.claude_command = Some(repaired.claude_command.clone());
            }
            RepairOperation::SetCodexCommand(_) => {
                delta.codex_command = Some(repaired.codex_command.clone());
            }
            _ => {}
        }
    }
    delta
}

fn which_winget() -> Option<PathBuf> {
    resolve::resolve_all("winget").into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::model::{
        DiagnosticId, DiagnosticImportance, DiagnosticResult, ProfileRecipe, RepairRisk,
    };
    use crate::diagnostics::runner::{CommandFailure, CommandOutput, CommandRunnerFuture};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Mutex};

    struct FakeRunner {
        calls: Arc<Mutex<Vec<String>>>,
        responses: Mutex<HashMap<String, CommandOutput>>,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                responses: Mutex::new(HashMap::new()),
            }
        }

        fn set(&self, key: &str, output: CommandOutput) {
            self.responses
                .lock()
                .unwrap()
                .insert(key.to_string(), output);
        }
    }

    impl CommandRunner for FakeRunner {
        fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
            Box::pin(async move {
                let key = format!(
                    "{}|{}",
                    spec.program.display(),
                    spec.args
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
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

    impl CommandRunner for Arc<FakeRunner> {
        fn run<'a>(&'a self, spec: &'a CommandSpec) -> CommandRunnerFuture<'a> {
            (**self).run(spec)
        }
    }

    struct RecordingSink {
        settings: Settings,
        terminal_sets: usize,
        claude_sets: usize,
    }

    impl SettingsRepairSink for RecordingSink {
        fn settings(&self) -> &Settings {
            &self.settings
        }

        fn set_default_terminal(&mut self, value: DefaultTerminal) -> Result<(), String> {
            self.terminal_sets += 1;
            self.settings.default_terminal = value;
            Ok(())
        }

        fn set_claude_command(&mut self, value: String) -> Result<(), String> {
            self.claude_sets += 1;
            self.settings.claude_command = Some(value);
            Ok(())
        }

        fn set_codex_command(&mut self, value: String) -> Result<(), String> {
            self.settings.codex_command = Some(value);
            Ok(())
        }
    }

    fn plan(op: RepairOperation, risk: RepairRisk) -> RepairPlan {
        RepairPlan {
            id: "plan".to_string(),
            title: "plan".to_string(),
            risk,
            operation: op,
            preview: "preview".to_string(),
            verifies: Vec::new(),
        }
    }

    fn empty_probe(runner: FakeRunner) -> DiagnosticProbe<FakeRunner> {
        DiagnosticProbe::new(runner)
            .with_resolver(|_| Vec::new())
            .with_file_reader(|_| Err("none".into()))
    }

    fn git_warning_probe(git: PathBuf) -> (Arc<FakeRunner>, DiagnosticProbe<Arc<FakeRunner>>) {
        let runner = Arc::new(FakeRunner::new());
        runner.set(
            &format!("{}|--version", git.display()),
            CommandOutput {
                exit_code: Some(0),
                timed_out: false,
                stdout: "git version 2.0\n".into(),
                stderr: String::new(),
            },
        );
        runner.set(
            &format!("{}|config --get user.name", git.display()),
            CommandOutput {
                exit_code: Some(1),
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        runner.set(
            &format!("{}|config --get user.email", git.display()),
            CommandOutput {
                exit_code: Some(1),
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let git_for_resolver = git.clone();
        let probe = DiagnosticProbe::new(runner.clone()).with_resolver(move |name| {
            if name == "git" || name == "git.exe" {
                vec![git_for_resolver.clone()]
            } else {
                Vec::new()
            }
        });
        (runner, probe)
    }

    #[test]
    fn rejects_unrecognized_package_ids_and_command_shapes() {
        assert!(validate_plan(&plan(
            RepairOperation::InstallWingetPackage {
                package_id: "Evil.Package".to_string(),
            },
            RepairRisk::Normal,
        ))
        .is_err());

        assert!(
            validate_known_command(&PathBuf::from("claude.exe"), &["rm".into(), "-rf".into()])
                .is_err()
        );
        assert!(validate_known_command(&PathBuf::from("cmd.exe"), &["/C".into()]).is_err());
    }

    #[tokio::test]
    async fn open_url_not_in_recommended_batch() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut sink = RecordingSink {
            settings: Settings::default(),
            terminal_sets: 0,
            claude_sets: 0,
        };
        let snapshot = DiagnosticSnapshot::from_results(vec![DiagnosticResult {
            id: DiagnosticId::ClaudeCli,
            title: "Claude".to_string(),
            importance: DiagnosticImportance::Required,
            status: DiagnosticStatus::Missing,
            summary: "missing".to_string(),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs: vec![
                plan(
                    RepairOperation::OpenUrl("https://example.com".into()),
                    RepairRisk::Normal,
                ),
                plan(
                    RepairOperation::InstallWingetPackage {
                        package_id: "Evil.Package".into(),
                    },
                    RepairRisk::Normal,
                ),
            ],
        }]);

        let mut executor = RepairExecutor::new(&runner, &probe, &mut sink);
        let outcomes = executor.execute_recommended(&snapshot).await;
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].success);
        assert!(outcomes[0].summary.contains("unrecognized winget package"));
    }

    #[tokio::test]
    async fn high_risk_excluded_from_recommended_batch() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut sink = RecordingSink {
            settings: Settings::default(),
            terminal_sets: 0,
            claude_sets: 0,
        };
        let snapshot = DiagnosticSnapshot::from_results(vec![DiagnosticResult {
            id: DiagnosticId::CcShortcut,
            title: "cc".to_string(),
            importance: DiagnosticImportance::Recommended,
            status: DiagnosticStatus::Missing,
            summary: "missing".to_string(),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs: vec![plan(
                RepairOperation::UpdatePowerShellProfile {
                    path: PathBuf::from("x.ps1"),
                    recipe: ProfileRecipe::UnsafeClaudeShortcut,
                },
                RepairRisk::High,
            )],
        }]);

        let mut executor = RepairExecutor::new(&runner, &probe, &mut sink);
        let outcomes = executor.execute_recommended(&snapshot).await;
        assert!(outcomes.is_empty());
    }

    #[tokio::test]
    async fn settings_mutations_go_through_sink() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut sink = RecordingSink {
            settings: Settings::default(),
            terminal_sets: 0,
            claude_sets: 0,
        };
        let mut executor = RepairExecutor::new(&runner, &probe, &mut sink);
        let outcome = executor
            .execute(&plan(
                RepairOperation::SetClaudeCommand("claude".into()),
                RepairRisk::Normal,
            ))
            .await;
        assert!(outcome.success);
        assert_eq!(sink.claude_sets, 1);
        assert_eq!(sink.settings.claude_command.as_deref(), Some("claude"));
        assert_eq!(sink.terminal_sets, 0);
    }

    #[tokio::test]
    async fn reports_failure_when_verification_missing() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut settings = Settings::default();
        let mut executor = RepairExecutor::new(&runner, &probe, &mut settings);
        let mut repair = plan(
            RepairOperation::SetClaudeCommand("still-missing".into()),
            RepairRisk::Normal,
        );
        repair.verifies = vec![DiagnosticId::ClaudeCli];
        let outcome = executor.execute(&repair).await;
        assert!(!outcome.success);
        assert!(outcome.summary.contains("verification"));
    }

    #[tokio::test]
    async fn reports_failure_when_verification_warning() {
        let git = PathBuf::from(r"C:\tools\git.exe");
        let (runner, probe) = git_warning_probe(git);
        let mut settings = Settings::default();
        let mut executor = RepairExecutor::new(&runner, &probe, &mut settings);
        let mut repair = plan(
            RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
            RepairRisk::Normal,
        );
        repair.verifies = vec![DiagnosticId::Git];
        let outcome = executor.execute(&repair).await;
        assert!(!outcome.success);
        assert!(outcome.summary.contains("verification"));
        assert!(outcome.details.iter().any(|d| d.contains("Warning")));
    }

    #[test]
    fn winget_uses_fixed_argument_shape() {
        let args = winget_install_args("Microsoft.PowerShell");
        assert_eq!(
            args,
            vec![
                "install",
                "--id",
                "Microsoft.PowerShell",
                "--exact",
                "--accept-package-agreements",
                "--accept-source-agreements",
            ]
        );
        assert!(winget_package_ids().contains(&"Microsoft.PowerShell"));
    }

    #[test]
    fn winget_exit_zero_with_missing_verify_is_success_needing_restart() {
        let plan = RepairPlan {
            id: "winget-git".into(),
            title: "Install Git".into(),
            risk: RepairRisk::Normal,
            operation: RepairOperation::InstallWingetPackage {
                package_id: "Git.Git".into(),
            },
            preview: "winget".into(),
            verifies: vec![DiagnosticId::Git],
        };
        let outcome =
            finalize_repair_verification(&plan, true, vec!["Git: missing (Missing)".into()]);
        assert!(outcome.success);
        assert!(outcome.requires_restart);
        assert_eq!(outcome.summary, WINGET_RESTART_NOTICE);
        assert!(!outcome.summary.to_ascii_lowercase().contains("healthy"));
        assert!(outcome
            .details
            .iter()
            .any(|d| d.contains("restart DevManager")));
        assert!(outcome.details.iter().any(|d| d.contains("Missing")));

        let healthy = finalize_repair_verification(&plan, true, Vec::new());
        assert!(healthy.success);
        assert!(!healthy.requires_restart);
    }

    #[test]
    fn non_winget_unhealthy_verify_still_fails() {
        let plan = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        let outcome =
            finalize_repair_verification(&plan, false, vec!["ClaudeCli: missing (Missing)".into()]);
        assert!(!outcome.success);
        assert!(!outcome.requires_restart);
        assert!(outcome.summary.contains("verification"));
    }

    #[tokio::test]
    async fn winget_exit_zero_continues_batch_when_verify_still_missing() {
        let winget = PathBuf::from(r"C:\fake\winget.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!(
                "{}|install --id Git.Git --exact --accept-package-agreements --accept-source-agreements",
                winget.display()
            ),
            CommandOutput {
                exit_code: Some(0),
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        let mut paths = HashMap::new();
        paths.insert("winget".to_string(), vec![winget]);
        let probe = DiagnosticProbe::new(FakeRunner::new())
            .with_resolver(move |name| paths.get(name).cloned().unwrap_or_default())
            .with_file_reader(|_| Err("none".into()));
        let settings = Settings::default();

        let mut winget_plan = plan(
            RepairOperation::InstallWingetPackage {
                package_id: "Git.Git".into(),
            },
            RepairRisk::Normal,
        );
        winget_plan.verifies = vec![DiagnosticId::Git];
        let mut follow = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        follow.id = "follow".into();

        let batch =
            execute_repair_batch(&runner, &probe, settings, vec![winget_plan, follow]).await;
        assert_eq!(batch.completed.len(), 2);
        assert!(batch.failure.is_none());
        assert_eq!(batch.settings.claude_command.as_deref(), Some("claude"));
        assert_eq!(batch.notices, vec![WINGET_RESTART_NOTICE.to_string()]);
        assert_eq!(batch.restart_outcomes.len(), 1);
        assert!(batch.restart_outcomes[0].requires_restart);
        assert_eq!(batch.restart_outcomes[0].summary, WINGET_RESTART_NOTICE);
    }

    #[tokio::test]
    async fn winget_nonzero_exit_fails_and_stops_batch() {
        let winget = PathBuf::from(r"C:\fake\winget.exe");
        let runner = FakeRunner::new();
        runner.set(
            &format!(
                "{}|install --id Git.Git --exact --accept-package-agreements --accept-source-agreements",
                winget.display()
            ),
            CommandOutput {
                exit_code: Some(1),
                timed_out: false,
                stdout: String::new(),
                stderr: "failed".into(),
            },
        );
        let mut paths = HashMap::new();
        paths.insert("winget".to_string(), vec![winget]);
        let probe = DiagnosticProbe::new(FakeRunner::new())
            .with_resolver(move |name| paths.get(name).cloned().unwrap_or_default())
            .with_file_reader(|_| Err("none".into()));
        let settings = Settings::default();

        let mut winget_plan = plan(
            RepairOperation::InstallWingetPackage {
                package_id: "Git.Git".into(),
            },
            RepairRisk::Normal,
        );
        winget_plan.verifies = vec![DiagnosticId::Git];
        let mut follow = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        follow.id = "follow".into();

        let batch =
            execute_repair_batch(&runner, &probe, settings, vec![winget_plan, follow]).await;
        assert!(batch.completed.is_empty());
        assert!(batch
            .failure
            .as_deref()
            .unwrap_or("")
            .contains("winget install failed"));
        assert!(batch.settings.claude_command.is_none());
        assert!(batch.notices.is_empty());
        assert!(batch.restart_outcomes.is_empty());
    }

    #[tokio::test]
    async fn sequential_batch_runs_in_order() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut sink = RecordingSink {
            settings: Settings::default(),
            terminal_sets: 0,
            claude_sets: 0,
        };
        let snapshot = DiagnosticSnapshot::from_results(vec![DiagnosticResult {
            id: DiagnosticId::PowerShell7,
            title: "pwsh".to_string(),
            importance: DiagnosticImportance::Required,
            status: DiagnosticStatus::Missing,
            summary: "missing".to_string(),
            details: Vec::new(),
            detected_version: None,
            detected_path: None,
            repairs: vec![
                RepairPlan {
                    id: "one".into(),
                    title: "one".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
                    preview: "one".into(),
                    verifies: Vec::new(),
                },
                RepairPlan {
                    id: "two".into(),
                    title: "two".into(),
                    risk: RepairRisk::Normal,
                    operation: RepairOperation::SetClaudeCommand("claude".into()),
                    preview: "two".into(),
                    verifies: Vec::new(),
                },
            ],
        }]);
        let mut executor = RepairExecutor::new(&runner, &probe, &mut sink);
        let outcomes = executor.execute_recommended(&snapshot).await;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| o.success));
        assert_eq!(outcomes[0].plan_id, "one");
        assert_eq!(outcomes[1].plan_id, "two");
        assert_eq!(sink.terminal_sets, 1);
        assert_eq!(sink.claude_sets, 1);
    }

    #[tokio::test]
    async fn profile_parse_failure_triggers_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let pwsh = dir.path().join("pwsh.exe");
        fs::write(&pwsh, b"").unwrap();
        let profile = dir.path().join("profile.ps1");
        fs::write(&profile, "# original\n").unwrap();

        let parse_key = format!(
            "{}|-NoProfile -Command {}",
            pwsh.display(),
            crate::diagnostics::windows::profile_parse_script(&profile)
        );
        let runner = FakeRunner::new();
        runner.set(
            &parse_key,
            CommandOutput {
                exit_code: Some(1),
                timed_out: false,
                stdout: String::new(),
                stderr: "parse error".into(),
            },
        );

        let pwsh_for_resolver = pwsh.clone();
        let probe = DiagnosticProbe::new(FakeRunner::new())
            .with_resolver(move |name| {
                if name == "pwsh" || name == "pwsh.exe" {
                    vec![pwsh_for_resolver.clone()]
                } else {
                    Vec::new()
                }
            })
            .with_file_reader(|_| Err("none".into()));
        let mut settings = Settings::default();
        let mut executor = RepairExecutor::new(&runner, &probe, &mut settings);

        let original = fs::read_to_string(&profile).unwrap();
        let outcome = executor
            .execute(&RepairPlan {
                id: "profile-edit".into(),
                title: "profile".into(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::UpdatePowerShellProfile {
                    path: profile.clone(),
                    recipe: ProfileRecipe::SafeClaudeShortcut,
                },
                preview: "edit".into(),
                verifies: Vec::new(),
            })
            .await;

        assert!(!outcome.success);
        assert!(outcome.summary.contains("profile parse failed"));
        let restored = fs::read_to_string(&profile).unwrap();
        assert_eq!(restored, original);
        assert!(runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with(&format!("{}|-NoProfile -Command", pwsh.display()))));
    }

    #[tokio::test]
    async fn missing_pwsh_does_not_mutate_profile() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profile.ps1");
        fs::write(&profile, "# keep\n").unwrap();
        let before_bytes = fs::read(&profile).unwrap();
        let before_entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut settings = Settings::default();
        let mut executor = RepairExecutor::new(&runner, &probe, &mut settings);
        let outcome = executor
            .execute(&RepairPlan {
                id: "profile-edit".into(),
                title: "profile".into(),
                risk: RepairRisk::Normal,
                operation: RepairOperation::UpdatePowerShellProfile {
                    path: profile.clone(),
                    recipe: ProfileRecipe::SafeClaudeShortcut,
                },
                preview: "edit".into(),
                verifies: Vec::new(),
            })
            .await;

        assert!(!outcome.success);
        assert!(outcome.summary.contains("pwsh required"));
        assert_eq!(fs::read(&profile).unwrap(), before_bytes);
        let after_entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(after_entries, before_entries);
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn settings_delta_only_targets_settings_mutations() {
        let mut repaired = Settings::default();
        repaired.default_terminal = DefaultTerminal::Pwsh;
        repaired.claude_command = Some("claude".into());
        repaired.codex_command = Some("codex".into());
        repaired.theme = "should-not-matter".into();

        let plans = vec![
            plan(
                RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
                RepairRisk::Normal,
            ),
            plan(
                RepairOperation::SetClaudeCommand("claude".into()),
                RepairRisk::Normal,
            ),
            plan(
                RepairOperation::SetCodexCommand("codex".into()),
                RepairRisk::Normal,
            ),
            plan(
                RepairOperation::OpenUrl("https://example.com".into()),
                RepairRisk::Normal,
            ),
            plan(
                RepairOperation::InstallWingetPackage {
                    package_id: "Git.Git".into(),
                },
                RepairRisk::Normal,
            ),
        ];
        let delta = diagnostics_settings_delta_from_plans(&plans, &repaired);
        assert_eq!(delta.default_terminal, Some(DefaultTerminal::Pwsh));
        assert_eq!(delta.claude_command, Some(Some("claude".into())));
        assert_eq!(delta.codex_command, Some(Some("codex".into())));
        assert!(!delta.is_empty());

        let mut current = Settings::default();
        current.theme = "dark-custom".into();
        current.claude_command = Some("old".into());
        current.codex_command = Some("keep-until-targeted".into());
        current.default_terminal = DefaultTerminal::Bash;
        // Apply only Claude from a partial delta — unrelated fields stay put.
        let partial = DiagnosticsSettingsDelta {
            default_terminal: None,
            claude_command: Some(Some("claude".into())),
            codex_command: None,
        };
        partial.apply_to(&mut current);
        assert_eq!(current.theme, "dark-custom");
        assert_eq!(current.claude_command.as_deref(), Some("claude"));
        assert_eq!(
            current.codex_command.as_deref(),
            Some("keep-until-targeted")
        );
        assert_eq!(current.default_terminal, DefaultTerminal::Bash);

        delta.apply_to(&mut current);
        assert_eq!(current.theme, "dark-custom");
        assert_eq!(current.default_terminal, DefaultTerminal::Pwsh);
        assert_eq!(current.claude_command.as_deref(), Some("claude"));
        assert_eq!(current.codex_command.as_deref(), Some("codex"));
    }

    #[test]
    fn empty_and_non_settings_plans_produce_no_delta() {
        let repaired = Settings::default();
        let empty = diagnostics_settings_delta_from_plans(&[], &repaired);
        assert!(empty.is_empty());

        let non_settings = diagnostics_settings_delta_from_plans(
            &[plan(
                RepairOperation::OpenUrl("https://example.com".into()),
                RepairRisk::Normal,
            )],
            &repaired,
        );
        assert!(non_settings.is_empty());
    }

    #[tokio::test]
    async fn batch_success_then_failure_preserves_only_completed_settings_field() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut settings = Settings::default();
        settings.theme = "keep-theme".into();
        settings.codex_command = Some("old-codex".into());

        let mut ok = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        ok.id = "ok-claude".into();
        let mut fail = plan(
            RepairOperation::SetCodexCommand("codex".into()),
            RepairRisk::Normal,
        );
        fail.id = "fail-codex".into();
        fail.verifies = vec![DiagnosticId::Git];

        let batch = execute_repair_batch(&runner, &probe, settings, vec![ok.clone(), fail]).await;
        assert_eq!(batch.completed.len(), 1);
        assert_eq!(batch.completed[0].id, "ok-claude");
        assert!(batch.failure.is_some());
        assert!(batch.notices.is_empty());
        assert!(batch.restart_outcomes.is_empty());
        assert_eq!(batch.settings.claude_command.as_deref(), Some("claude"));
        assert_eq!(batch.settings.theme, "keep-theme");

        let delta = diagnostics_settings_delta_from_plans(&batch.completed, &batch.settings);
        assert_eq!(delta.claude_command, Some(Some("claude".into())));
        assert!(delta.codex_command.is_none());
        assert!(delta.default_terminal.is_none());

        let mut authoritative = Settings::default();
        authoritative.theme = "user-theme".into();
        authoritative.codex_command = Some("user-codex".into());
        authoritative.claude_command = Some("user-claude".into());
        delta.apply_to(&mut authoritative);
        assert_eq!(authoritative.theme, "user-theme");
        assert_eq!(authoritative.claude_command.as_deref(), Some("claude"));
        assert_eq!(authoritative.codex_command.as_deref(), Some("user-codex"));
    }

    #[tokio::test]
    async fn batch_first_failure_produces_empty_completed_and_no_delta() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let mut settings = Settings::default();
        settings.claude_command = Some("unchanged".into());

        let mut fail = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        fail.id = "fail-first".into();
        fail.verifies = vec![DiagnosticId::Git];

        let batch = execute_repair_batch(&runner, &probe, settings, vec![fail]).await;
        assert!(batch.completed.is_empty());
        assert!(batch.failure.is_some());

        let delta = diagnostics_settings_delta_from_plans(&batch.completed, &batch.settings);
        assert!(delta.is_empty());
    }

    #[tokio::test]
    async fn batch_completed_plans_preserve_order() {
        let runner = FakeRunner::new();
        let probe = empty_probe(FakeRunner::new());
        let settings = Settings::default();

        let mut one = plan(
            RepairOperation::SetDefaultTerminal(DefaultTerminal::Pwsh),
            RepairRisk::Normal,
        );
        one.id = "one".into();
        let mut two = plan(
            RepairOperation::SetClaudeCommand("claude".into()),
            RepairRisk::Normal,
        );
        two.id = "two".into();
        let mut three = plan(
            RepairOperation::SetCodexCommand("codex".into()),
            RepairRisk::Normal,
        );
        three.id = "three".into();
        three.verifies = vec![DiagnosticId::Git];

        let batch = execute_repair_batch(&runner, &probe, settings, vec![one, two, three]).await;
        assert_eq!(
            batch
                .completed
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert!(batch.failure.is_some());
    }
}
