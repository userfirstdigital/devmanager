use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use devmanager::client::action::{task_create_command, TaskCreateArguments};
use devmanager::config::{ConfigCommand, ConfigStore, Project};
use devmanager::domain::command::{
    Command, CommandEnvelope, CommandReceipt, CreateTaskRequestIntent,
};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, WorkspaceRef,
};
use devmanager::domain::{ClientId, CommandId, EnvironmentId, ProjectId, TaskId};
use devmanager::host::HostRequestExecutor;
use devmanager::kernel::{CommandBus, KernelStore, StoreError, TaskRuntimeLoadError};
use devmanager::protocol::{
    CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters, ProtocolVersion, ServerMessage,
};
use devmanager::workspace::{
    default_workspace_choice, path_identity_key, PendingWorktreeCandidate, TaskKind,
    WorkspaceChoice, WorkspaceError, WorkspaceKind, WorkspaceProjectRoots, WorkspaceRequest,
    WorkspaceResolution, WorkspaceService,
};
use tempfile::TempDir;

fn temporary_repository() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temporary repository root");
    let output = ProcessCommand::new("git")
        .args(["init", "--quiet"])
        .current_dir(temp.path())
        .output()
        .expect("git must be available for temporary repository fixtures");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = temp.path().to_path_buf();
    run_git(
        &path,
        ["config", "user.email", "devmanager-tests@example.invalid"],
    );
    run_git(&path, ["config", "user.name", "DevManager Tests"]);
    fs::write(temp.path().join("README.md"), "workspace fixture\n").expect("fixture file");
    run_git(&path, ["add", "README.md"]);
    run_git(&path, ["commit", "--quiet", "-m", "initial"]);
    (temp, path)
}

fn run_git<I, S>(repository: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(repository)
        .output()
        .expect("git must be available for temporary repository fixtures");
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn temporary_linked_worktree(repository: &Path, branch: &str) -> PathBuf {
    let path = repository
        .join(".worktrees")
        .join(branch.rsplit('/').next().expect("branch suffix"));
    fs::create_dir_all(path.parent().expect("worktree parent")).expect("worktree parent");
    let output = ProcessCommand::new("git")
        .args(["worktree", "add", "--quiet", "-b", branch])
        .arg(&path)
        .current_dir(repository)
        .output()
        .expect("git must be available for temporary linked worktree fixtures");
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("canonical fixture path")
}

fn configured_roots(
    configured_seed: ProjectId,
    project_root: &Path,
) -> (ProjectId, WorkspaceProjectRoots) {
    let config_root = tempfile::tempdir().expect("host config root");
    let paths = devmanager::config::paths::ResolvedAppPaths {
        root: config_root.path().to_path_buf(),
        config: config_root.path().join("config.json"),
        remote: config_root.path().join("remote.json"),
        database: config_root.path().join("kernel.sqlite3"),
        browser_root: config_root.path().join("browser"),
        logs: config_root.path().join("logs"),
    };
    let mut store = ConfigStore::open_host(&paths).expect("host config store");
    let config_id = format!("project-{configured_seed}");
    store
        .execute(
            store.snapshot().revision,
            ConfigCommand::CreateProject {
                project: Project {
                    id: config_id.clone(),
                    name: "Workspace fixture".to_string(),
                    root_path: project_root.to_string_lossy().into_owned(),
                    created_at: "now".to_string(),
                    updated_at: "now".to_string(),
                    ..Project::default()
                },
            },
        )
        .expect("persist configured project");
    let revision = store.snapshot().revision;
    let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 1, 1)
        .expect("host-issued project roots");
    let project_id = roots
        .project_id_for_config_id(&config_id)
        .expect("host-issued project id");
    assert_ne!(project_id, configured_seed);
    (project_id, roots)
}

fn service_for(project_root: &Path) -> WorkspaceService {
    let (project_id, roots) = configured_roots(ProjectId::new(), project_root);
    WorkspaceService::for_project(project_id, &roots).expect("workspace service")
}

fn service_from_durable(
    project_root: &Path,
    durable_ref: &WorkspaceRef,
) -> Result<WorkspaceService, WorkspaceError> {
    let (project_id, roots) = configured_roots(ProjectId::new(), project_root);
    WorkspaceService::from_durable(project_id, &roots, durable_ref)
}

fn create_task_via_host(
    database: &Path,
    project_id: ProjectId,
    project_root: &Path,
    task_id: TaskId,
    client_id: ClientId,
    workspace: WorkspaceRequest,
    title: &str,
) -> CommandReceipt {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("host test runtime");
    runtime.block_on(async move {
        let bus = CommandBus::open(database).expect("open task store");
        let config_root = database
            .parent()
            .expect("database parent")
            .join(format!(".devmanager-host-config-{task_id}"));
        let paths = devmanager::config::paths::ResolvedAppPaths {
            root: config_root.clone(),
            config: config_root.join("config.json"),
            remote: config_root.join("remote.json"),
            database: database.to_path_buf(),
            browser_root: config_root.join("browser"),
            logs: config_root.join("logs"),
        };
        let mut store = ConfigStore::open_host(&paths).expect("host config store");
        let config_id = format!("project-{project_id}");
        store
            .execute(
                store.snapshot().revision,
                ConfigCommand::CreateProject {
                    project: Project {
                        id: config_id.clone(),
                        name: "Host workspace fixture".to_string(),
                        root_path: project_root.to_string_lossy().into_owned(),
                        created_at: "now".to_string(),
                        updated_at: "now".to_string(),
                        ..Project::default()
                    },
                },
            )
            .expect("persist host project");
        let revision = store.snapshot().revision;
        let roots = WorkspaceProjectRoots::from_host_config_store(&mut store, revision, 1, 1)
            .expect("host-issued project roots");
        let project_id = roots
            .project_id_for_config_id(&config_id)
            .expect("opaque host project id");
        let (requests, executor) =
            HostRequestExecutor::start_supervised_with_config_store(bus, store, &config_root)
                .expect("host config store");
        let envelope = CommandEnvelope {
            command_id: CommandId::new(),
            client_id,
            task_id: None,
            issued_at_ms: 1_725_000_000_100,
            expected_task_revision: None,
            command: Command::CreateTaskV2(CreateTaskRequestIntent {
                id: task_id,
                environment_id: EnvironmentId::new(),
                title: title.to_string(),
                description: None,
                project_id,
                workspace,
                primary_provider: None,
                defer_primary_provider_start: false,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1_725_000_000_000,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        };
        let response = requests
            .execute(
                NegotiatedParameters {
                    version: ProtocolVersion::current(),
                    client_id,
                    capabilities: CapabilitySet::empty(),
                    limits: FrameLimits::v1_default(),
                },
                ClientRequest::Command(envelope),
            )
            .await
            .expect("host create request");
        drop(requests);
        let _ = executor.join.await.expect("host executor join");
        let ServerMessage::CommandReceipt(receipt) = response else {
            panic!("host create must return a command receipt");
        };
        receipt
    })
}

#[test]
fn binding_main_resolves_final_identity_and_repository_location() {
    let (_temp, repository) = temporary_repository();
    let mut service = service_for(&repository);

    let binding = service
        .bind(WorkspaceRequest::main())
        .expect("main workspace binding");

    assert_eq!(binding.kind(), WorkspaceKind::Main);
    assert_eq!(binding.path(), canonical(&repository).as_path());
    assert!(matches!(
        binding.durable_ref(),
        WorkspaceRef::HostBound { .. }
    ));
    assert_eq!(binding.relative_worktree_path(), Some(Path::new(".")));
    let repository_identity = binding.repository().expect("repository identity");
    assert_eq!(repository_identity.root(), canonical(&repository).as_path());
    assert!(!repository_identity.fingerprint().as_str().is_empty());
}

#[test]
fn legacy_v1_main_ref_requires_an_explicit_host_rebind() {
    let (_temp, repository) = temporary_repository();
    assert!(matches!(
        service_from_durable(&repository, &WorkspaceRef::Main),
        Err(WorkspaceError::RebindRequired)
    ));
}

#[test]
fn config_project_ids_must_be_opaque_issuer_ids() {
    let temp = tempfile::tempdir().expect("config root");
    let config_root = temp.path().join("host-config");
    let paths = devmanager::config::paths::ResolvedAppPaths {
        root: config_root.clone(),
        config: config_root.join("config.json"),
        remote: config_root.join("remote.json"),
        database: config_root.join("kernel.sqlite3"),
        browser_root: config_root.join("browser"),
        logs: config_root.join("logs"),
    };
    let config_id = "project-native".to_string();
    let first_root = temp.path().join("first-project-root");
    let second_root = temp.path().join("second-project-root");
    fs::create_dir_all(&first_root).expect("first project root");
    fs::create_dir_all(&second_root).expect("second project root");
    let mut store = ConfigStore::open_host(&paths).expect("host config store");
    store
        .execute(
            store.snapshot().revision,
            ConfigCommand::CreateProject {
                project: Project {
                    id: config_id.clone(),
                    name: "Configured project".to_string(),
                    root_path: first_root.to_string_lossy().into_owned(),
                    created_at: "now".to_string(),
                    updated_at: "now".to_string(),
                    ..Project::default()
                },
            },
        )
        .expect("persist configured project");
    let first_revision = store.snapshot().revision;
    let first = WorkspaceProjectRoots::from_host_config_store(&mut store, first_revision, 1, 1)
        .expect("first host-issued project roots");
    let first_id = first
        .project_id_for_config_id(&config_id)
        .expect("adapter project id");
    assert_ne!(first_id.to_string(), config_id);

    let mut changed = store.snapshot().config.clone();
    changed.projects[0].root_path = second_root.to_string_lossy().into_owned();
    let changed_revision = store.snapshot().revision;
    store
        .replace_config(changed)
        .expect("persist changed configured root");
    let second_revision = store.snapshot().revision;
    let second = WorkspaceProjectRoots::from_host_config_store(&mut store, second_revision, 1, 1)
        .expect("second host-issued project roots");
    assert_eq!(Some(first_id), second.project_id_for_config_id(&config_id));
    WorkspaceService::for_project(first_id, &first).expect("first adapted project root");
    WorkspaceService::for_project(first_id, &second).expect("second adapted project root");

    drop(store);
    let mut reopened = ConfigStore::open_host(&paths).expect("reopen host config store");
    let reopened_revision = reopened.snapshot().revision;
    let reopened_roots =
        WorkspaceProjectRoots::from_host_config_store(&mut reopened, reopened_revision, 1, 1)
            .expect("reopened host project roots");
    assert_eq!(
        Some(first_id),
        reopened_roots.project_id_for_config_id(&config_id),
        "mapping survives reopen and configured root change"
    );
    assert!(changed_revision < second_revision);
}

#[test]
fn new_worktree_plain_directory_returns_typed_pending_candidate() {
    let (_temp, repository) = temporary_repository();
    let worktree = repository.join(".worktrees").join("task-a");
    fs::create_dir_all(&worktree).expect("temporary worktree directory");
    let service = service_for(&repository);

    let resolution = service
        .resolve(WorkspaceRequest::new_worktree(&worktree, "codex/task-a"))
        .expect("new worktree resolution");

    let WorkspaceResolution::PendingWorktree(candidate) = resolution else {
        panic!("plain directory must not be treated as a linked worktree");
    };
    assert_eq!(candidate.path, canonical(&worktree));
    assert_eq!(candidate.branch, "codex/task-a");
    assert_eq!(
        candidate.repository.root(),
        canonical(&repository).as_path()
    );
    assert_eq!(
        candidate.relative_worktree_path,
        Some(PathBuf::from(r".worktrees\task-a"))
    );
    let mut binding_service = service_for(&repository);
    assert!(matches!(
        binding_service.bind(WorkspaceRequest::new_worktree(&worktree, "codex/task-a")),
        Err(WorkspaceError::PendingWorktree(_))
    ));
}

#[test]
fn new_worktree_resolves_actual_branch_only_for_a_real_linked_worktree() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/task-a";
    let worktree = temporary_linked_worktree(&repository, branch);
    let service = service_for(&repository);

    let WorkspaceResolution::Resolved(binding) = service
        .resolve(WorkspaceRequest::new_worktree(&worktree, branch))
        .expect("real linked worktree resolution")
    else {
        panic!("real linked worktree must resolve to a durable binding");
    };

    assert_eq!(binding.kind(), WorkspaceKind::Worktree);
    assert_eq!(binding.path(), canonical(&worktree).as_path());
    assert_eq!(binding.branch(), Some(branch));
    assert_eq!(
        binding.relative_worktree_path(),
        Some(Path::new(r".worktrees\task-a"))
    );
    assert!(matches!(
        binding.durable_ref(),
        WorkspaceRef::HostBound { .. }
    ));
}

#[test]
fn binding_ask_requires_an_explicit_choice() {
    let (_temp, repository) = temporary_repository();
    let mut service = service_for(&repository);

    assert!(matches!(
        service.bind(WorkspaceRequest::ask()),
        Err(WorkspaceError::ChoiceRequired)
    ));
}

#[test]
fn binding_explicit_external_requires_confirmation_and_allows_non_repository_folder() {
    let (_project_temp, project) = temporary_repository();
    let external_temp = tempfile::tempdir().expect("external directory");
    let external = external_temp.path().join("notes");
    fs::create_dir(&external).expect("non-repository external folder");
    let mut service = service_for(&project);

    assert!(matches!(
        service.bind(WorkspaceRequest::external(&external)),
        Err(WorkspaceError::ExternalConfirmationRequired)
    ));

    let binding = service
        .bind(WorkspaceRequest::confirmed_external(&external))
        .expect("confirmed external workspace binding");
    assert_eq!(binding.kind(), WorkspaceKind::External);
    assert_eq!(binding.path(), canonical(&external).as_path());
    assert_eq!(binding.repository(), None);
    assert!(matches!(
        binding.durable_ref(),
        WorkspaceRef::ExternalWithFingerprint { .. }
    ));
}

#[test]
fn binding_and_error_diagnostics_redact_paths_and_underlying_identity() {
    let (_temp, repository) = temporary_repository();
    let mut service = service_for(&repository);
    let binding = service
        .bind(WorkspaceRequest::main())
        .expect("main workspace binding");
    let binding_debug = format!("{binding:?}");
    assert!(binding_debug.len() <= 128);
    assert!(!binding_debug.contains(&repository.to_string_lossy().to_string()));
    assert_eq!(binding_debug, "WorkspaceBinding(REDACTED)");

    let durable_ref_debug = format!("{:?}", binding.durable_ref());
    assert!(durable_ref_debug.len() <= 128);
    assert!(!durable_ref_debug.contains(&repository.to_string_lossy().to_string()));

    let error = WorkspaceError::PathDoesNotExist(repository.join("secret"));
    let error_text = error.to_string();
    assert!(error_text.len() <= 96);
    assert!(!error_text.contains(&repository.to_string_lossy().to_string()));
}

#[test]
fn binding_rejects_nonexistent_workspace_path() {
    let (_temp, repository) = temporary_repository();
    let missing = repository.join("does-not-exist");
    let mut service = service_for(&repository);

    assert!(matches!(
        service.bind(WorkspaceRequest::confirmed_external(missing)),
        Err(WorkspaceError::PathDoesNotExist(_))
    ));
}

#[test]
fn binding_rejects_main_and_new_worktree_inside_non_repository_folder() {
    let project_temp = tempfile::tempdir().expect("non-repository project");
    let project = project_temp.path().to_path_buf();
    let worktree = project.join("worktree");
    fs::create_dir(&worktree).expect("worktree folder");
    let mut service = service_for(&project);

    assert!(matches!(
        service.bind(WorkspaceRequest::main()),
        Err(WorkspaceError::NotRepository(_))
    ));
    assert!(matches!(
        service.bind(WorkspaceRequest::new_worktree(&worktree, "main")),
        Err(WorkspaceError::NotRepository(_))
    ));
}

#[test]
fn binding_rejects_symlink_escape_from_repository_root() {
    let (_temp, repository) = temporary_repository();
    let outside_temp = tempfile::tempdir().expect("outside directory");
    let worktree_root = repository.join(".worktrees");
    fs::create_dir_all(&worktree_root).expect("worktree root");
    let escaped = worktree_root.join("escape");

    #[cfg(windows)]
    {
        let output = ProcessCommand::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                escaped.to_str().expect("junction path is UTF-8"),
                outside_temp.path().to_str().expect("target path is UTF-8"),
            ])
            .output()
            .expect("temporary junction command");
        assert!(
            output.status.success(),
            "temporary junction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(outside_temp.path(), &escaped)
            .expect("temporary directory symlink");
    }

    let mut service = service_for(&repository);
    assert!(matches!(
        service.bind(WorkspaceRequest::new_worktree(&escaped, "codex/escape")),
        Err(WorkspaceError::OutsideProject { .. })
    ));
}

#[test]
fn binding_compares_final_drive_case_identity_not_display_strings() {
    let (_temp, repository) = temporary_repository();
    let alternate_case = {
        let text = repository.to_string_lossy();
        let mut chars = text.chars();
        let first = chars.next().expect("drive letter");
        PathBuf::from(format!(
            "{}{}",
            first.to_ascii_lowercase(),
            chars.collect::<String>()
        ))
    };

    assert_eq!(
        path_identity_key(&repository),
        path_identity_key(&alternate_case)
    );
    let mut service = service_for(&repository);
    let first = service
        .bind(WorkspaceRequest::confirmed_external(&repository))
        .expect("first external binding");
    let second = service
        .bind(WorkspaceRequest::confirmed_external(alternate_case))
        .expect("same final external binding");
    assert_eq!(first.path(), second.path());
}

#[test]
fn binding_compares_equivalent_unc_identity_case_insensitively() {
    assert_eq!(
        path_identity_key(Path::new(r"\\?\UNC\Server\Share\Repo")),
        path_identity_key(Path::new(r"\\server\share\repo"))
    );
}

#[test]
fn same_workspace_rejects_repository_or_relative_location_changes() {
    let (_temp, repository) = temporary_repository();
    let worktree = temporary_linked_worktree(&repository, "codex/task-a");
    let service = service_for(&repository);
    let WorkspaceResolution::Resolved(binding) = service
        .resolve(WorkspaceRequest::new_worktree(&worktree, "codex/task-a"))
        .expect("linked worktree binding")
    else {
        panic!("linked worktree must resolve");
    };

    let WorkspaceResolution::Resolved(main) = service_for(&repository)
        .resolve(WorkspaceRequest::main())
        .expect("main binding")
    else {
        panic!("main workspace must resolve");
    };
    assert!(!binding.same_workspace(&main));
}

#[test]
fn ai_coding_defaults_to_new_worktree_and_terminal_consumes_project_default() {
    assert_eq!(
        default_workspace_choice(TaskKind::AiCoding, None),
        WorkspaceChoice::NewWorktree
    );
    assert_eq!(
        default_workspace_choice(TaskKind::GeneralTerminal, Some(WorkspaceChoice::Main)),
        WorkspaceChoice::Main
    );
    assert_eq!(
        default_workspace_choice(TaskKind::GeneralTerminal, None),
        WorkspaceChoice::Ask
    );
}

#[test]
fn binding_is_immutable_after_first_resolution_and_rebind_is_explicit() {
    let (_temp, repository) = temporary_repository();
    let external_temp = tempfile::tempdir().expect("external directory");
    let external = external_temp.path().join("other");
    fs::create_dir(&external).expect("other workspace");
    let worktree = temporary_linked_worktree(&repository, "codex/task-a");
    let mut service = service_for(&repository);
    let main = service
        .bind(WorkspaceRequest::main())
        .expect("main binding");

    assert!(matches!(
        service.bind(WorkspaceRequest::confirmed_external(&external)),
        Err(WorkspaceError::WorkspaceImmutable)
    ));
    assert_eq!(service.current().expect("current binding"), &main);

    let rebound = service
        .close_and_rebind(WorkspaceRequest::new_worktree(&worktree, "codex/task-a"))
        .expect("explicit close and rebind");
    assert_eq!(rebound.kind(), WorkspaceKind::Worktree);
    assert_ne!(rebound.path(), main.path());
}

#[test]
fn durable_workspace_ref_roundtrips_without_host_paths_and_requires_explicit_rebind() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/task-roundtrip";
    let worktree = temporary_linked_worktree(&repository, branch);
    let database = repository.join("task.sqlite");
    let task_id = TaskId::new();
    let project_id = ProjectId::new();

    let receipt = create_task_via_host(
        &database,
        project_id,
        &repository,
        task_id,
        ClientId::new(),
        WorkspaceRequest::new_worktree(&worktree, branch),
        "Roundtrip task",
    );
    assert!(matches!(
        receipt,
        CommandReceipt::Accepted {
            task_revision: Some(1),
            ref event_ids,
            ..
        } if event_ids.len() == 1
    ));

    let bus = CommandBus::open(&database).expect("reopen task store");
    let snapshot = bus
        .task_snapshot(task_id)
        .expect("load task snapshot")
        .expect("task");
    let WorkspaceRef::HostBound { .. } = snapshot.task.workspace.clone() else {
        panic!("task event must persist the resolved worktree reference");
    };

    assert!(matches!(
        service_from_durable(&repository, &snapshot.task.workspace),
        Err(WorkspaceError::RebindRequired)
    ));
}

#[test]
fn same_repository_wrong_root_cannot_relocate_a_main_task_runtime() {
    let (_temp, repository) = temporary_repository();
    let wrong_root = repository.join("nested-client-root");
    fs::create_dir(&wrong_root).expect("same-repository wrong root");
    let project_id = ProjectId::new();
    let task_id = TaskId::new();
    let database = repository.join("main-task.sqlite");
    let receipt = create_task_via_host(
        &database,
        project_id,
        &repository,
        task_id,
        ClientId::new(),
        WorkspaceRequest::main(),
        "Main task root binding",
    );
    assert!(matches!(receipt, CommandReceipt::Accepted { .. }));

    let config_root = database
        .parent()
        .expect("task database parent")
        .join(format!(".devmanager-host-config-{task_id}"));
    let paths = devmanager::config::paths::ResolvedAppPaths {
        root: config_root.clone(),
        config: config_root.join("config.json"),
        remote: config_root.join("remote.json"),
        database: database.clone(),
        browser_root: config_root.join("browser"),
        logs: config_root.join("logs"),
    };
    let mut config_store = ConfigStore::open_host(&paths).expect("reopen host config");
    let mut changed = config_store.snapshot().config.clone();
    changed.projects[0].root_path = wrong_root.to_string_lossy().into_owned();
    let revision = config_store.snapshot().revision;
    config_store
        .replace_config(changed)
        .expect("persist wrong configured root");
    let wrong_revision = config_store.snapshot().revision;
    let wrong_roots =
        WorkspaceProjectRoots::from_host_config_store(&mut config_store, wrong_revision, 1, 1)
            .expect("reissue host roots after path change");
    assert!(wrong_revision > revision);
    let bus = CommandBus::open(&database).expect("reopen task store");
    assert!(matches!(
        bus.load_task_runtime(task_id, &wrong_roots),
        Err(TaskRuntimeLoadError::Workspace(
            WorkspaceError::RebindRequired
        ))
    ));
}

#[test]
fn configured_nested_root_cannot_bind_a_main_workspace() {
    let (_temp, repository) = temporary_repository();
    let nested_root = repository.join("nested-client-root");
    fs::create_dir(&nested_root).expect("nested project root");

    let (project_id, roots) = configured_roots(ProjectId::new(), &nested_root);
    let mut service = WorkspaceService::for_project(project_id, &roots).expect("workspace service");
    assert!(
        service.bind(WorkspaceRequest::main()).is_err(),
        "a configured nested root must not relocate Main to the repository parent"
    );
}

#[test]
fn kernel_store_rejects_caller_chosen_workspace_refs_before_persistence() {
    let directory = tempfile::tempdir().expect("temporary kernel directory");
    let task_id = TaskId::new();
    let envelope = task_create_command(
        devmanager::domain::CommandId::new(),
        devmanager::domain::ClientId::new(),
        1_725_000_000_300,
        TaskCreateArguments {
            task_id,
            environment_id: EnvironmentId::new(),
            title: "Untrusted workspace task".into(),
            description: None,
            project_id: ProjectId::new(),
            workspace: WorkspaceRef::Main,
        },
    )
    .expect("raw durable create command");

    let database = directory.path().join("tasks.sqlite");
    let mut store = KernelStore::open(&database).expect("open kernel store");
    assert_eq!(
        store.execute(envelope),
        Err(StoreError::HostAuthorityRequired)
    );
    drop(store);

    let bus = CommandBus::open(&database).expect("reopen kernel store");
    assert!(bus.task_snapshot(task_id).expect("task lookup").is_none());
}

#[test]
fn task_creation_rejects_unresolved_ask_before_create_intent() {
    let (_temp, repository) = temporary_repository();
    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::ask());

    assert!(matches!(result, Err(WorkspaceError::ChoiceRequired)));
}

#[test]
fn pending_new_worktree_cannot_become_a_durable_task_reference() {
    let (_temp, repository) = temporary_repository();
    let path = repository.join(".worktrees").join("not-created");
    fs::create_dir_all(&path).expect("plain candidate directory");
    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::new_worktree(path, "codex/not-created"));

    assert!(matches!(
        result,
        Err(WorkspaceError::PendingWorktree(
            PendingWorktreeCandidate { .. }
        ))
    ));
}

#[test]
fn forged_git_marker_is_not_registered_as_a_linked_worktree() {
    let (_temp, repository) = temporary_repository();
    let branch = String::from_utf8(
        ProcessCommand::new("git")
            .args(["symbolic-ref", "--short", "HEAD"])
            .current_dir(&repository)
            .output()
            .expect("git branch lookup")
            .stdout,
    )
    .expect("branch output")
    .trim()
    .to_string();
    let worktree = repository.join(".worktrees").join("forged");
    fs::create_dir_all(&worktree).expect("forged worktree directory");
    fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", repository.join(".git").display()),
    )
    .expect("forged git marker");

    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::new_worktree(&worktree, branch));

    assert!(
        result.is_err(),
        "a .git marker pointing at the main .git directory is not a registered worktree"
    );
}

#[test]
fn linked_worktree_requires_matching_admin_gitdir_back_reference() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/back-reference";
    let worktree = temporary_linked_worktree(&repository, branch);
    let marker = fs::read_to_string(worktree.join(".git")).expect("linked worktree marker");
    let gitdir = marker
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .expect("gitdir marker")
        .trim();
    let admin_dir = canonical(&worktree.join(gitdir));
    fs::write(
        admin_dir.join("gitdir"),
        format!("{}\n", repository.join(".git").join("forged").display()),
    )
    .expect("forged admin back-reference");

    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::new_worktree(&worktree, branch));

    assert!(
        result.is_err(),
        "a linked worktree must be rejected when its admin gitdir does not point back to its marker"
    );
}

#[test]
fn linked_worktree_requires_the_repository_worktrees_admin_directory() {
    let (_temp, repository) = temporary_repository();
    let (_other_temp, other_repository) = temporary_repository();
    let branch = "codex/wrong-admin";
    let worktree = temporary_linked_worktree(&repository, branch);
    let marker = fs::read_to_string(worktree.join(".git")).expect("linked worktree marker");
    let gitdir = marker
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .expect("gitdir marker")
        .trim();
    let admin_dir = canonical(&worktree.join(gitdir));
    fs::create_dir_all(other_repository.join(".git").join("worktrees"))
        .expect("alternate worktree admin directory");
    fs::write(
        admin_dir.join("commondir"),
        format!("{}\n", other_repository.join(".git").display()),
    )
    .expect("forged common git directory");

    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::new_worktree(&worktree, branch));

    assert!(
        result.is_err(),
        "a linked worktree must stay under its repository's canonical worktrees admin directory"
    );
}

#[test]
fn linked_worktree_requires_a_present_admin_gitdir_back_reference() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/missing-back-reference";
    let worktree = temporary_linked_worktree(&repository, branch);
    let marker = fs::read_to_string(worktree.join(".git")).expect("linked worktree marker");
    let gitdir = marker
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .expect("gitdir marker")
        .trim();
    let admin_dir = canonical(&worktree.join(gitdir));
    fs::remove_file(admin_dir.join("gitdir")).expect("remove admin back-reference");

    let mut service = service_for(&repository);
    let result = service.bind(WorkspaceRequest::new_worktree(&worktree, branch));

    assert!(
        result.is_err(),
        "a linked worktree without an admin back-reference must fail closed"
    );
}

#[test]
fn main_binding_persists_a_repository_fingerprint_and_rejects_same_path_replacement() {
    let (_temp, repository) = temporary_repository();
    let mut service = service_for(&repository);
    let binding = service
        .bind(WorkspaceRequest::main())
        .expect("main workspace binding");
    let original_ref = binding.durable_ref().clone();
    let original_fingerprint = original_ref
        .repository_fingerprint()
        .expect("Main durable reference fingerprint")
        .clone();
    let worktree = temporary_linked_worktree(&repository, "codex/fingerprint");
    let mut worktree_service = service_for(&repository);
    let worktree_ref = worktree_service
        .bind(WorkspaceRequest::new_worktree(
            &worktree,
            "codex/fingerprint",
        ))
        .expect("worktree workspace binding")
        .durable_ref()
        .clone();

    let old_git = repository.join(".git-old");
    fs::rename(repository.join(".git"), &old_git).expect("move original git directory");
    let output = ProcessCommand::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repository)
        .output()
        .expect("git reinitialization");
    assert!(output.status.success(), "replacement git init failed");

    let loaded = service_from_durable(&repository, &original_ref);
    assert!(matches!(
        loaded,
        Err(WorkspaceError::RepositoryFingerprintMismatch { .. })
    ));
    let loaded_worktree = service_from_durable(&repository, &worktree_ref);
    assert!(
        loaded_worktree.is_err(),
        "a replacement common Git directory must not replay a linked worktree"
    );
    let mut current = service_for(&repository);
    let replacement = current
        .bind(WorkspaceRequest::main())
        .expect("replacement main workspace");
    assert_ne!(
        replacement
            .durable_ref()
            .repository_fingerprint()
            .expect("replacement fingerprint"),
        &original_fingerprint
    );
}

#[test]
fn external_binding_replay_rejects_same_path_replacement() {
    let (_project_temp, project) = temporary_repository();
    let external_temp = tempfile::tempdir().expect("external root");
    let external = external_temp.path().join("notes");
    fs::create_dir(&external).expect("external directory");
    let mut service = service_for(&project);
    let durable_ref = service
        .bind(WorkspaceRequest::confirmed_external(&external))
        .expect("external binding")
        .durable_ref()
        .clone();

    let old = external_temp.path().join("notes-old");
    fs::rename(&external, &old).expect("move original external directory");
    fs::create_dir(&external).expect("replacement external directory");

    assert!(matches!(
        service_from_durable(&project, &durable_ref),
        Err(WorkspaceError::RepositoryFingerprintMismatch { .. })
            | Err(WorkspaceError::PathResolution { .. })
    ));
}

#[test]
fn linked_metadata_in_place_rewrite_changes_the_durable_binding_identity() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/metadata-rewrite";
    let worktree = temporary_linked_worktree(&repository, branch);
    let mut service = service_for(&repository);
    let durable_ref = service
        .bind(WorkspaceRequest::new_worktree(&worktree, branch))
        .expect("linked worktree binding")
        .durable_ref()
        .clone();
    let marker = fs::read_to_string(worktree.join(".git")).expect("linked marker");
    let gitdir = marker
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .expect("gitdir line")
        .trim();
    let admin_dir = canonical(&worktree.join(gitdir));
    let commondir_path = admin_dir.join("commondir");
    let original = fs::read_to_string(&commondir_path).expect("commondir");
    fs::write(&commondir_path, format!("  {}\n", original.trim()))
        .expect("rewrite commondir in place");

    assert!(matches!(
        service_from_durable(&repository, &durable_ref),
        Err(WorkspaceError::RepositoryFingerprintMismatch { .. })
            | Err(WorkspaceError::UnregisteredLinkedWorktree(_))
            | Err(WorkspaceError::PathResolution { .. })
    ));
}

#[test]
fn duplicate_linked_metadata_lines_fail_closed() {
    let (_temp, repository) = temporary_repository();
    let branch = "codex/duplicate-metadata";
    let worktree = temporary_linked_worktree(&repository, branch);
    let marker_path = worktree.join(".git");
    let marker = fs::read_to_string(&marker_path).expect("linked marker");
    let line = marker
        .lines()
        .find(|line| line.starts_with("gitdir:"))
        .expect("gitdir line");
    fs::write(&marker_path, format!("{line}\n{line}\n")).expect("duplicate marker line");

    let mut service = service_for(&repository);
    assert!(service
        .bind(WorkspaceRequest::new_worktree(&worktree, branch))
        .is_err());
}
