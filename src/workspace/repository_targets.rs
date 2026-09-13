//! Host-owned Task repository targeting against sealed workspace authority.
//!
//! Clients choose only a [`TaskRepositorySelector`]. The host resolves every
//! path from the current [`WorkspaceProjectRoots`] / task workspace binding.
//! Absolute paths never leave this module on the wire.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::domain::cockpit::{
    redact_repository_label, TaskGitRepositoriesProjection, TaskRepositoryCatalogEntry,
    TaskRepositoryKind, TaskRepositorySelector, MAX_TASK_REPOSITORIES,
};
use crate::domain::task::{WorkspaceBindingKind, WorkspaceRef};
use crate::domain::TaskId;
use crate::workspace::model::{ConfiguredProjectFolder, RepositoryIdentity, WorkspaceProjectRoots};
use crate::workspace::service::discover_repository_for_target;

/// Host-resolved configured repository target after selector + fence correlation.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResolvedRepositoryTarget {
    pub(crate) selector: TaskRepositorySelector,
    pub(crate) label: String,
    pub(crate) kind: TaskRepositoryKind,
    pub(crate) path: PathBuf,
    pub(crate) identity: String,
    pub(crate) repository_key: Option<String>,
    pub(crate) available: bool,
    pub(crate) read_only: bool,
    pub(crate) mutation_allowed: bool,
}

impl std::fmt::Debug for ResolvedRepositoryTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedRepositoryTarget(REDACTED)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositoryTargetError {
    InvalidSelector,
    UnknownSelector,
    Unavailable,
    ReadOnly,
    StaleIdentity,
}

/// Build the bounded, path-redacted repository catalog for one Task.
///
/// Order: Workspace, then ProjectRoot, then configured folders in authority
/// order. Actual repository aliases are deduplicated by repository identity
/// (first wins). Unavailable / non-repository entries remain visible as
/// redacted selector slots and never suppress a later real repository through
/// a fabricated filesystem key.
pub(crate) fn build_task_repository_catalog(
    task_id: TaskId,
    project_id: crate::domain::ProjectId,
    workspace: &WorkspaceRef,
    workspace_path: Option<&Path>,
    workspace_repository: Option<&RepositoryIdentity>,
    roots: &WorkspaceProjectRoots,
) -> TaskGitRepositoriesProjection {
    let task_kind = workspace_binding_kind(workspace);
    let mut repositories = Vec::new();
    let mut seen_repo_keys = BTreeSet::new();

    let workspace_has_repo = workspace_repository.is_some();
    let workspace_available = workspace_path.is_some() && workspace_has_repo;
    let workspace_label = workspace_repository
        .and_then(|workspace_repository| {
            roots
                .configured_folders()
                .iter()
                .filter(|folder| folder.project_id() == project_id && folder.is_admitted())
                .find(|folder| {
                    discover_repository_for_target(folder.path())
                        .as_ref()
                        .is_some_and(|repository| {
                            repository.key() == workspace_repository.key()
                                && repository.fingerprint() == workspace_repository.fingerprint()
                        })
                })
        })
        .map(|folder| folder.label())
        .unwrap_or("Workspace");
    push_catalog_entry(
        &mut repositories,
        &mut seen_repo_keys,
        TaskRepositoryCatalogEntry {
            selector: TaskRepositorySelector::Workspace,
            label: redact_repository_label(workspace_label),
            kind: TaskRepositoryKind::Workspace,
            available: workspace_available,
            read_only: !workspace_available,
        },
        workspace_repository.map(|repository| repository_dedupe_key(repository)),
    );

    if let Some(root) = roots.configured_root_for(project_id) {
        let repo = discover_repository_for_target(root.path());
        let (available, read_only) =
            configured_availability(task_kind, workspace_repository, repo.as_ref(), true);
        let available = available && repo.is_some();
        push_catalog_entry(
            &mut repositories,
            &mut seen_repo_keys,
            TaskRepositoryCatalogEntry {
                selector: TaskRepositorySelector::ProjectRoot,
                label: redact_repository_label("Project Root"),
                kind: TaskRepositoryKind::ProjectRoot,
                available,
                read_only: !available || read_only,
            },
            repo.as_ref().map(repository_dedupe_key),
        );
    }

    for folder in roots
        .configured_folders()
        .iter()
        .filter(|folder| folder.project_id() == project_id)
    {
        if repositories.len() >= MAX_TASK_REPOSITORIES {
            break;
        }
        let (entry, repo_key) = folder_catalog_entry(folder, task_kind, workspace_repository);
        push_catalog_entry(&mut repositories, &mut seen_repo_keys, entry, repo_key);
    }

    repositories.truncate(MAX_TASK_REPOSITORIES);
    TaskGitRepositoriesProjection {
        task_id,
        repositories,
    }
}

fn repository_dedupe_key(repository: &RepositoryIdentity) -> String {
    format!("repo:{}", repository.key())
}

fn push_catalog_entry(
    repositories: &mut Vec<TaskRepositoryCatalogEntry>,
    seen_repo_keys: &mut BTreeSet<String>,
    entry: TaskRepositoryCatalogEntry,
    repo_dedupe_key: Option<String>,
) {
    if repositories.len() >= MAX_TASK_REPOSITORIES {
        return;
    }
    if let Some(key) = repo_dedupe_key {
        if !seen_repo_keys.insert(key) {
            return;
        }
    }
    repositories.push(entry);
}

fn folder_catalog_entry(
    folder: &ConfiguredProjectFolder,
    task_kind: Option<WorkspaceBindingKind>,
    workspace_repository: Option<&RepositoryIdentity>,
) -> (TaskRepositoryCatalogEntry, Option<String>) {
    let label = if folder.label().trim().is_empty() {
        redact_repository_label(folder.folder_config_id())
    } else {
        redact_repository_label(folder.label())
    };
    let selector = TaskRepositorySelector::Folder {
        folder_config_id: folder.folder_config_id().to_owned(),
    };
    if !folder.is_admitted() {
        return (
            TaskRepositoryCatalogEntry {
                selector,
                label,
                kind: TaskRepositoryKind::ConfiguredFolder,
                available: false,
                read_only: true,
            },
            None,
        );
    }
    let repo = discover_repository_for_target(folder.path());
    if repo.is_none() {
        // Honest non-repository slot: configured and visible, but not Git.
        return (
            TaskRepositoryCatalogEntry {
                selector,
                label,
                kind: TaskRepositoryKind::ConfiguredFolder,
                available: false,
                read_only: true,
            },
            None,
        );
    }
    let (available, read_only) =
        configured_availability(task_kind, workspace_repository, repo.as_ref(), true);
    (
        TaskRepositoryCatalogEntry {
            selector,
            label,
            kind: TaskRepositoryKind::ConfiguredFolder,
            available,
            read_only,
        },
        repo.as_ref().map(repository_dedupe_key),
    )
}

fn configured_availability(
    task_kind: Option<WorkspaceBindingKind>,
    workspace_repository: Option<&RepositoryIdentity>,
    target_repository: Option<&RepositoryIdentity>,
    admitted: bool,
) -> (bool, bool) {
    if !admitted {
        return (false, true);
    }
    let Some(target_repository) = target_repository else {
        return (false, true);
    };
    match task_kind {
        None | Some(WorkspaceBindingKind::Main) => (true, false),
        Some(WorkspaceBindingKind::Worktree) | Some(WorkspaceBindingKind::External) => {
            match workspace_repository {
                Some(bound)
                    if bound.key() == target_repository.key()
                        && bound.fingerprint() == target_repository.fingerprint() =>
                {
                    (true, false)
                }
                _ => (false, true),
            }
        }
    }
}

fn workspace_binding_kind(workspace: &WorkspaceRef) -> Option<WorkspaceBindingKind> {
    match workspace {
        WorkspaceRef::Main | WorkspaceRef::MainWithFingerprint { .. } => {
            Some(WorkspaceBindingKind::Main)
        }
        WorkspaceRef::Worktree { .. } | WorkspaceRef::WorktreeWithFingerprint { .. } => {
            Some(WorkspaceBindingKind::Worktree)
        }
        WorkspaceRef::External { .. } | WorkspaceRef::ExternalWithFingerprint { .. } => {
            Some(WorkspaceBindingKind::External)
        }
        WorkspaceRef::HostBound { binding } => Some(binding.kind()),
    }
}

/// Resolve one opaque selector against sealed authority for status / mutation.
pub(crate) fn resolve_repository_target(
    selector: &TaskRepositorySelector,
    project_id: crate::domain::ProjectId,
    workspace: &WorkspaceRef,
    workspace_path: Option<&Path>,
    workspace_repository: Option<&RepositoryIdentity>,
    roots: &WorkspaceProjectRoots,
) -> Result<ResolvedRepositoryTarget, RepositoryTargetError> {
    selector
        .validate()
        .map_err(|_| RepositoryTargetError::InvalidSelector)?;
    let task_kind = workspace_binding_kind(workspace);
    match selector {
        TaskRepositorySelector::Workspace => {
            let path = workspace_path
                .ok_or(RepositoryTargetError::Unavailable)?
                .to_path_buf();
            let validated = crate::workspace::service::validate_host_workspace_path(&path, true)
                .map_err(|_| RepositoryTargetError::Unavailable)?;
            let repo = discover_repository_for_target(&validated.path)
                .ok_or(RepositoryTargetError::Unavailable)?;
            Ok(ResolvedRepositoryTarget {
                selector: TaskRepositorySelector::Workspace,
                label: redact_repository_label("Workspace"),
                kind: TaskRepositoryKind::Workspace,
                path: validated.path,
                identity: validated.identity,
                repository_key: Some(repo.key().to_string()),
                available: true,
                read_only: false,
                mutation_allowed: true,
            })
        }
        TaskRepositorySelector::ProjectRoot => {
            let root = roots
                .configured_root_for(project_id)
                .ok_or(RepositoryTargetError::UnknownSelector)?;
            revalidate_configured_path(
                TaskRepositorySelector::ProjectRoot,
                redact_repository_label("Project Root"),
                TaskRepositoryKind::ProjectRoot,
                root.path(),
                root.identity(),
                task_kind,
                workspace_repository,
            )
        }
        TaskRepositorySelector::Folder { folder_config_id } => {
            let folder = roots
                .configured_folder(project_id, folder_config_id)
                .ok_or(RepositoryTargetError::UnknownSelector)?;
            let identity = folder
                .identity()
                .ok_or(RepositoryTargetError::Unavailable)?;
            let label = if folder.label().trim().is_empty() {
                redact_repository_label(folder.folder_config_id())
            } else {
                redact_repository_label(folder.label())
            };
            revalidate_configured_path(
                TaskRepositorySelector::Folder {
                    folder_config_id: folder.folder_config_id().to_owned(),
                },
                label,
                TaskRepositoryKind::ConfiguredFolder,
                folder.path(),
                identity,
                task_kind,
                workspace_repository,
            )
        }
    }
}

fn revalidate_configured_path(
    selector: TaskRepositorySelector,
    label: String,
    kind: TaskRepositoryKind,
    path: &Path,
    expected_identity: &str,
    task_kind: Option<WorkspaceBindingKind>,
    workspace_repository: Option<&RepositoryIdentity>,
) -> Result<ResolvedRepositoryTarget, RepositoryTargetError> {
    let validated = crate::workspace::service::validate_host_workspace_path(path, true)
        .map_err(|_| RepositoryTargetError::StaleIdentity)?;
    if validated.identity != expected_identity {
        return Err(RepositoryTargetError::StaleIdentity);
    }
    let repo = discover_repository_for_target(&validated.path);
    let (available, read_only) =
        configured_availability(task_kind, workspace_repository, repo.as_ref(), true);
    Ok(ResolvedRepositoryTarget {
        selector,
        label,
        kind,
        path: validated.path,
        identity: validated.identity,
        repository_key: repo.map(|repository| repository.key().to_string()),
        available,
        read_only: !available || read_only,
        mutation_allowed: available && !read_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ProjectId;
    use crate::workspace::model::{admit_configured_folders_for_test, WorkspaceProjectRoots};
    use std::fs;
    use std::process::Command;

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("repo dir");
        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
    }

    fn write_folder_authority(
        project_id: ProjectId,
        root: PathBuf,
        folders: Vec<(String, String, PathBuf)>,
    ) -> WorkspaceProjectRoots {
        let mut roots = WorkspaceProjectRoots::try_from_pairs([(project_id, root)]).expect("roots");
        let folder_tuples = folders
            .into_iter()
            .map(|(id, label, path)| (project_id, "project".into(), id, label, path))
            .collect::<Vec<_>>();
        let admitted = admit_configured_folders_for_test(folder_tuples).expect("folders");
        roots.set_folders_for_test(admitted);
        roots
    }

    #[test]
    fn catalog_lists_sibling_and_external_repos_for_main_task() {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let sibling_a = project.join("repo-a");
        let sibling_b = project.join("repo-b");
        let external = temp.path().join("external-repo");
        fs::create_dir_all(&project).expect("project");
        init_repo(&sibling_a);
        init_repo(&sibling_b);
        init_repo(&external);

        let project_id = ProjectId::new();
        let roots = write_folder_authority(
            project_id,
            project.clone(),
            vec![
                ("repo-a".into(), "Repo A".into(), sibling_a.clone()),
                ("repo-b".into(), "Repo B".into(), sibling_b.clone()),
                ("external".into(), "External".into(), external.clone()),
            ],
        );

        let catalog = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            Some(sibling_a.as_path()),
            discover_repository_for_target(&sibling_a).as_ref(),
            &roots,
        );
        let ids: Vec<_> = catalog
            .repositories
            .iter()
            .map(|entry| match &entry.selector {
                TaskRepositorySelector::Workspace => "workspace".to_string(),
                TaskRepositorySelector::ProjectRoot => "root".to_string(),
                TaskRepositorySelector::Folder { folder_config_id } => folder_config_id.clone(),
            })
            .collect();
        assert!(ids.contains(&"workspace".to_string()));
        assert!(ids.contains(&"repo-b".to_string()));
        assert!(ids.contains(&"external".to_string()));
        let encoded = serde_json::to_string(&catalog).expect("encode");
        assert!(!encoded.contains(
            &external
                .canonicalize()
                .unwrap_or(external.clone())
                .to_string_lossy()
                .as_ref()
        ));
        assert!(!encoded.contains("folder_path"));
        assert!(catalog.repositories.iter().any(|entry| {
            matches!(
                &entry.selector,
                TaskRepositorySelector::Folder { folder_config_id }
                    if folder_config_id == "external"
            ) && entry.available
                && !entry.read_only
        }));
    }

    #[test]
    fn stale_folder_is_unavailable_without_poisoning_siblings() {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let good = project.join("good");
        let stale = project.join("stale");
        fs::create_dir_all(&project).expect("project");
        init_repo(&good);
        init_repo(&stale);
        let project_id = ProjectId::new();
        let mut roots = write_folder_authority(
            project_id,
            project.clone(),
            vec![
                ("good".into(), "Good".into(), good.clone()),
                ("stale".into(), "Stale".into(), stale.clone()),
            ],
        );
        roots.mark_folder_stale_for_test(project_id, "stale");

        let catalog = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            Some(good.as_path()),
            discover_repository_for_target(&good).as_ref(),
            &roots,
        );
        let good_entry = catalog
            .repositories
            .iter()
            .find(|entry| {
                matches!(&entry.selector, TaskRepositorySelector::Workspace)
                    && entry.label == "Good"
            })
            .expect("workspace alias uses the configured good-folder label");
        assert!(good_entry.available);
        let stale_entry = catalog
            .repositories
            .iter()
            .find(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "stale"
                )
            })
            .expect("stale folder retained as unavailable");
        assert!(!stale_entry.available);
        assert!(stale_entry.read_only);
    }

    #[test]
    fn identity_aliases_dedupe_deterministically() {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let repo = project.join("only");
        fs::create_dir_all(&project).expect("project");
        init_repo(&repo);
        let project_id = ProjectId::new();
        let roots = write_folder_authority(
            project_id,
            repo.clone(),
            vec![("alias".into(), "Alias".into(), repo.clone())],
        );
        let catalog = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            Some(repo.as_path()),
            discover_repository_for_target(&repo).as_ref(),
            &roots,
        );
        let available = catalog
            .repositories
            .iter()
            .filter(|entry| entry.available)
            .count();
        assert_eq!(available, 1);
        assert!(matches!(
            catalog.repositories[0].selector,
            TaskRepositorySelector::Workspace
        ));
        assert_eq!(catalog.repositories[0].label, "Alias");
    }

    #[test]
    fn worktree_task_denies_sibling_configured_mutation() {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let main_repo = project.join("main");
        let sibling = project.join("sibling");
        fs::create_dir_all(&project).expect("project");
        init_repo(&main_repo);
        init_repo(&sibling);
        let project_id = ProjectId::new();
        let roots = write_folder_authority(
            project_id,
            project.clone(),
            vec![("sibling".into(), "Sibling".into(), sibling.clone())],
        );
        let workspace = WorkspaceRef::Worktree {
            path: main_repo.clone(),
            branch: "feature".into(),
        };
        let catalog = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &workspace,
            Some(main_repo.as_path()),
            discover_repository_for_target(&main_repo).as_ref(),
            &roots,
        );
        let sibling_entry = catalog
            .repositories
            .iter()
            .find(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "sibling"
                )
            })
            .expect("sibling");
        assert!(!sibling_entry.available);
        assert!(sibling_entry.read_only);

        let denied = resolve_repository_target(
            &TaskRepositorySelector::Folder {
                folder_config_id: "sibling".into(),
            },
            project_id,
            &workspace,
            Some(main_repo.as_path()),
            discover_repository_for_target(&main_repo).as_ref(),
            &roots,
        )
        .expect("honest resolve");
        assert!(!denied.mutation_allowed);
        assert!(!denied.available);
    }

    #[test]
    fn path_like_folder_selector_is_rejected() {
        let err = resolve_repository_target(
            &TaskRepositorySelector::Folder {
                folder_config_id: "C:/secret".into(),
            },
            ProjectId::new(),
            &WorkspaceRef::Main,
            None,
            None,
            &WorkspaceProjectRoots::empty(),
        )
        .expect_err("path-like");
        assert_eq!(err, RepositoryTargetError::InvalidSelector);
    }

    #[test]
    fn workspace_without_path_or_repository_is_unavailable_read_only() {
        let project_id = ProjectId::new();
        let temp = tempfile::tempdir().expect("temp");
        let roots =
            WorkspaceProjectRoots::try_from_pairs([(project_id, temp.path().to_path_buf())])
                .expect("roots");

        let unbound = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            None,
            None,
            &roots,
        );
        let workspace = unbound
            .repositories
            .iter()
            .find(|entry| matches!(entry.selector, TaskRepositorySelector::Workspace))
            .expect("workspace entry");
        assert!(!workspace.available);
        assert!(workspace.read_only);

        let plain_dir = temp.path().join("plain");
        fs::create_dir_all(&plain_dir).expect("plain");
        let no_repo = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            Some(plain_dir.as_path()),
            None,
            &roots,
        );
        let workspace = no_repo
            .repositories
            .iter()
            .find(|entry| matches!(entry.selector, TaskRepositorySelector::Workspace))
            .expect("workspace entry");
        assert!(!workspace.available);
        assert!(workspace.read_only);

        let err = resolve_repository_target(
            &TaskRepositorySelector::Workspace,
            project_id,
            &WorkspaceRef::Main,
            Some(plain_dir.as_path()),
            None,
            &roots,
        )
        .expect_err("no repository");
        assert_eq!(err, RepositoryTargetError::Unavailable);
    }

    #[test]
    fn non_repository_configured_folder_stays_visible_and_does_not_suppress_later_repo() {
        let temp = tempfile::tempdir().expect("temp");
        let project = temp.path().join("project");
        let docs = project.join("docs");
        let bound = project.join("bound");
        let later = project.join("later");
        fs::create_dir_all(&docs).expect("docs");
        fs::create_dir_all(&project).expect("project");
        init_repo(&bound);
        init_repo(&later);
        let project_id = ProjectId::new();
        let roots = write_folder_authority(
            project_id,
            project.clone(),
            vec![
                ("docs".into(), "Docs".into(), docs),
                ("later".into(), "Later".into(), later.clone()),
            ],
        );
        let catalog = build_task_repository_catalog(
            TaskId::new(),
            project_id,
            &WorkspaceRef::Main,
            Some(bound.as_path()),
            discover_repository_for_target(&bound).as_ref(),
            &roots,
        );
        let docs_entry = catalog
            .repositories
            .iter()
            .find(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "docs"
                )
            })
            .expect("docs folder remains visible");
        assert!(!docs_entry.available);
        assert!(docs_entry.read_only);
        let later_entry = catalog
            .repositories
            .iter()
            .find(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "later"
                )
            })
            .expect("later distinct repo must not be suppressed by unavailable docs");
        assert!(later_entry.available);
        assert!(!later_entry.read_only);
        let docs_idx = catalog
            .repositories
            .iter()
            .position(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "docs"
                )
            })
            .expect("docs index");
        let later_idx = catalog
            .repositories
            .iter()
            .position(|entry| {
                matches!(
                    &entry.selector,
                    TaskRepositorySelector::Folder { folder_config_id } if folder_config_id == "later"
                )
            })
            .expect("later index");
        assert!(docs_idx < later_idx);
    }

    #[test]
    fn cross_project_duplicate_folder_ids_select_only_task_project() {
        let temp = tempfile::tempdir().expect("temp");
        let project_a = temp.path().join("a");
        let project_b = temp.path().join("b");
        let api_a = project_a.join("api");
        let api_b = project_b.join("api");
        fs::create_dir_all(&project_a).expect("a");
        fs::create_dir_all(&project_b).expect("b");
        init_repo(&api_a);
        init_repo(&api_b);
        let id_a = ProjectId::new();
        let id_b = ProjectId::new();
        let mut roots =
            WorkspaceProjectRoots::try_from_pairs([(id_a, project_a), (id_b, project_b)])
                .expect("roots");
        let admitted = admit_configured_folders_for_test(vec![
            (
                id_a,
                "a".into(),
                "api".into(),
                "API A".into(),
                api_a.clone(),
            ),
            (
                id_b,
                "b".into(),
                "api".into(),
                "API B".into(),
                api_b.clone(),
            ),
        ])
        .expect("cross-project api ids");
        roots.set_folders_for_test(admitted);

        let catalog_a = build_task_repository_catalog(
            TaskId::new(),
            id_a,
            &WorkspaceRef::Main,
            Some(api_a.as_path()),
            discover_repository_for_target(&api_a).as_ref(),
            &roots,
        );
        assert!(catalog_a.repositories.iter().any(|entry| {
            matches!(&entry.selector, TaskRepositorySelector::Workspace) && entry.label == "API A"
        }));
        assert!(!catalog_a
            .repositories
            .iter()
            .any(|entry| entry.label == "API B"));

        let resolved = resolve_repository_target(
            &TaskRepositorySelector::Folder {
                folder_config_id: "api".into(),
            },
            id_a,
            &WorkspaceRef::Main,
            Some(api_a.as_path()),
            discover_repository_for_target(&api_a).as_ref(),
            &roots,
        )
        .expect("task project folder");
        assert_eq!(resolved.label, "API A");
        assert!(resolved.mutation_allowed);
    }
}
