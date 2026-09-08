//! Host-local repository manager. Configured roots, never a selected task or
//! client path, authorize discovery and each individual desktop operation.
use super::cockpit::TaskCockpitDispatch;
use crate::config::AppConfig;
use crate::domain::cockpit::{relative_path_is_safe, TaskCockpitQuery, TaskCockpitResult};
use crate::domain::query::{QueryError, QueryOutcome, QueryResult};
use crate::git::desktop::{DesktopGitAction as A, DesktopGitPayload as P, DesktopRepositoryEntry};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Only this host module can construct this admission. A request must resolve
/// the opaque selector against the current configuration and filesystem again.
pub(crate) struct ConfiguredDesktopRepository {
    pin: crate::workspace::service::ValidatedHostWorkspacePath,
    entry: DesktopRepositoryEntry,
}
impl ConfiguredDesktopRepository {
    pub(crate) fn path(&self) -> &Path {
        &self.pin.path
    }
    pub(crate) fn identity(&self) -> &str {
        &self.pin.identity
    }
    pub(crate) fn id(&self) -> &str {
        &self.entry.id
    }
}

fn catalog(config: &AppConfig) -> Result<Vec<ConfiguredDesktopRepository>, String> {
    let mut roots = Vec::new();
    for project in &config.projects {
        if project.archived.as_ref() == Some(&true) {
            continue;
        }
        if !project.root_path.trim().is_empty() {
            roots.push((project.name.clone(), PathBuf::from(&project.root_path)));
        }
        for folder in &project.folders {
            if folder.archived.as_ref() != Some(&true) && !folder.folder_path.trim().is_empty() {
                roots.push((
                    format!("{} / {}", project.name, folder.name),
                    PathBuf::from(&folder.folder_path),
                ));
            }
        }
    }
    if let Some(base) = config
        .settings()
        .default_directories
        .as_ref()
        .and_then(|d| d.projects.as_ref())
        .filter(|p| !p.trim().is_empty())
    {
        roots.push(("Projects".into(), PathBuf::from(base)));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut repos = Vec::new();
    let mut remaining = 4096usize;
    // Breadth-first search gives configured root/folder labels precedence over
    // automatically discovered names. Do not walk symlinks or generated trees.
    let mut queue: std::collections::VecDeque<_> = roots
        .into_iter()
        .map(|(label, path)| (label, path, 0))
        .collect();
    while let Some((label, path, depth)) = queue.pop_front() {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| "Could not resolve a configured project folder.".to_string())?;
        if !seen.insert(canonical.clone()) {
            continue;
        }
        if remaining == 0 {
            return Err("Repository scan reached 4,096 folders. Add specific repository folders to narrow the scan.".into());
        }
        remaining -= 1;
        if canonical.join(".git").exists() {
            let validated =
                crate::workspace::service::validate_host_workspace_path(&canonical, true)
                    .map_err(|e| format!("Could not open {label}: {e}"))?;
            let mut digest = Sha256::new();
            digest.update(canonical.to_string_lossy().as_bytes());
            digest.update([0]);
            digest.update(validated.identity.as_bytes());
            repos.push(ConfiguredDesktopRepository {
                pin: validated,
                entry: DesktopRepositoryEntry {
                    id: format!("{:x}", digest.finalize()),
                    label: label.chars().take(160).collect(),
                },
            });
            if repos.len() > 256 {
                return Err(
                    "More than 256 repositories found. Narrow the configured project folders."
                        .into(),
                );
            }
        }
        if depth >= 4 {
            continue;
        }
        let mut children = std::fs::read_dir(&canonical)
            .map_err(|_| format!("Could not read project folder {label}."))?
            .take(4097)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| format!("Could not read project folder {label}."))?;
        if children.len() > 4096 {
            return Err(format!("Project folder {label} has more than 4,096 entries; configure narrower repository folders."));
        }
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || matches!(
                    name.as_str(),
                    "node_modules" | "target" | "vendor" | "dist" | "build"
                )
            {
                continue;
            }
            if child
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
            {
                queue.push_back((format!("{label} / {name}"), child.path(), depth + 1));
            }
        }
    }
    repos.sort_by(|a, b| {
        a.entry
            .label
            .to_lowercase()
            .cmp(&b.entry.label.to_lowercase())
    });
    Ok(repos)
}

pub(super) fn serve(dispatch: &TaskCockpitDispatch<'_>) -> QueryOutcome {
    let Some(config) = dispatch.config else {
        return QueryOutcome::Err(QueryError::Unavailable {
            reason: "desktop_git_config",
        });
    };
    let repositories = match catalog(config) {
        Ok(repos) => repos,
        Err(error) => {
            return match dispatch.query {
                TaskCockpitQuery::DesktopRepositoryAction { repository_id, .. } => {
                    response(repository_id, P::Error(error))
                }
                _ => QueryOutcome::Err(QueryError::Unavailable {
                    reason: "desktop_git_catalog",
                }),
            }
        }
    };
    match dispatch.query {
        TaskCockpitQuery::DesktopRepositories => QueryOutcome::Ok(QueryResult::TaskCockpit(
            TaskCockpitResult::DesktopRepositories(
                repositories.into_iter().map(|r| r.entry).collect(),
            ),
        )),
        TaskCockpitQuery::DesktopRepositoryAction {
            repository_id,
            action,
            confirm,
        } => {
            if action.is_mutation() && !confirm {
                return response(
                    repository_id,
                    P::Error("Confirm the Git operation before continuing.".into()),
                );
            }
            let Some(admission) = repositories.iter().find(|r| r.entry.id == *repository_id) else {
                return response(repository_id, P::Error("Repository changed or is no longer configured. Reopen Git to reload repositories.".into()));
            };
            let paths: Vec<&str> = match action {
                A::Stage { paths } | A::Unstage { paths } => {
                    paths.iter().map(String::as_str).collect()
                }
                A::FileDiff { relative_path, .. } => vec![relative_path.as_str()],
                _ => vec![],
            };
            if paths.len() > 4096
                || paths.iter().any(|p| !relative_path_is_safe(p))
                || matches!(action, A::History {limit,skip} if *limit == 0 || *limit > 100 || *skip > 10_000)
            {
                return QueryOutcome::Err(QueryError::InvalidRequest);
            }
            let result = crate::git::command::issue_desktop_repository_git_host_binding(
                admission,
                dispatch.client_id,
                dispatch.connection_id,
                dispatch.request_id,
                match action {
                    A::Publish { branch } => Some(branch.as_str()),
                    _ => None,
                },
            )
            .and_then(|binding| {
                crate::git::command::GitRepository::from_host_binding(
                    binding,
                    crate::git::command::GitCancellation::new(),
                )
            });
            match result {
                Ok(mut repository) => {
                    if action.is_mutation() {
                        repository = repository.with_desktop_mutation_authority(
                            &super::ConfirmedGitDesktopMutation { _private: () },
                        );
                    }
                    response(
                        repository_id,
                        super::cockpit::execute_desktop_action(&repository, action, false)
                            .unwrap_or_else(|outcome| {
                                P::Error(format!("Git operation was not accepted: {outcome:?}"))
                            }),
                    )
                }
                Err(error) => response(repository_id, P::Error(error.to_string())),
            }
        }
        _ => QueryOutcome::Err(QueryError::InvalidRequest),
    }
}
fn response(repository_id: &str, payload: P) -> QueryOutcome {
    QueryOutcome::Ok(QueryResult::TaskCockpit(
        TaskCockpitResult::DesktopRepositoryAction {
            repository_id: repository_id.into(),
            payload,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ClientId, RequestId};
    use crate::protocol::{Capability, CapabilitySet};
    fn git(path: &Path, args: &[&str]) -> String {
        let result = std::process::Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).trim().to_owned()
    }
    fn repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        git(path, &["config", "user.name", "Desktop Test"]);
        git(path, &["config", "user.email", "desktop@example.invalid"]);
        std::fs::write(path.join("README.md"), "initial\n").unwrap();
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-m", "Initial"]);
    }
    fn config(root: &Path) -> AppConfig {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "projects": [{"id":"desktop-project", "name":"Base folder", "rootPath":root.to_string_lossy(), "folders":[],"createdAt":"2026-09-08T00:00:00Z","updatedAt":"2026-09-08T00:00:00Z"}],
            "settings": {}, "sshConnections": []
        })).unwrap()
    }
    fn query(
        config: &AppConfig,
        bus: &crate::kernel::CommandBus,
        query: &TaskCockpitQuery,
    ) -> QueryOutcome {
        super::super::cockpit::serve_task_cockpit(TaskCockpitDispatch {
            capabilities: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            envelope_task_id: None,
            client_id: ClientId::new(),
            connection_id: uuid::Uuid::new_v4(),
            request_id: RequestId::new(),
            query,
            bus,
            service_runtime: None,
            semantic_journal: None,
            terminal_service: None,
            ssh_endpoints: None,
            ssh_runtime: None,
            workspace_projects: None,
            coordinator: None,
            action_epoch: None,
            runtime_generation: None,
            config: Some(config),
            provider_launch_hint: Default::default(),
            provider_restore_detail: None,
        })
    }
    fn action(
        config: &AppConfig,
        bus: &crate::kernel::CommandBus,
        id: &str,
        action: A,
        confirm: bool,
    ) -> P {
        let requested = format!("{action:?}");
        match query(
            config,
            bus,
            &TaskCockpitQuery::DesktopRepositoryAction {
                repository_id: id.into(),
                action,
                confirm,
            },
        ) {
            QueryOutcome::Ok(QueryResult::TaskCockpit(
                TaskCockpitResult::DesktopRepositoryAction {
                    repository_id,
                    payload,
                },
            )) => {
                assert_eq!(repository_id, id);
                eprintln!("Desktop Git test {requested}: {payload:?}");
                payload
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn desktop_git_no_task_catalog_and_independent_repository_mutations() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base");
        let first = base.join("alpha");
        let second = base.join("nested/beta");
        repo(&first);
        repo(&second);
        let cfg = config(&base);
        let bus = crate::kernel::CommandBus::open(&temp.path().join("tasks.sqlite")).unwrap();
        let outcome = query(&cfg, &bus, &TaskCockpitQuery::DesktopRepositories);
        let QueryOutcome::Ok(QueryResult::TaskCockpit(TaskCockpitResult::DesktopRepositories(
            entries,
        ))) = outcome
        else {
            panic!("{outcome:?}")
        };
        assert_eq!(entries.len(), 2);
        let a = &entries[0].id;
        let b = &entries[1].id;
        std::fs::write(first.join("README.md"), "changed\n").unwrap();
        assert!(matches!(action(&cfg,&bus,a,A::Status,false),P::Status(s) if s.entries.len()==1));
        assert!(matches!(action(&cfg,&bus,b,A::Status,false),P::Status(s) if s.entries.is_empty()));
        assert!(matches!(
            action(&cfg, &bus, a, A::StageAll, false),
            P::Error(_)
        ));
        assert!(git(&first, &["diff", "--cached", "--name-only"]).is_empty());
        assert!(matches!(
            action(&cfg, &bus, a, A::StageAll, true),
            P::Done(_)
        ));
        assert!(
            matches!(action(&cfg,&bus,a,A::StagedDiff,false),P::StagedDiff(diff) if diff.contains("+changed"))
        );
        assert!(matches!(
            action(
                &cfg,
                &bus,
                a,
                A::Commit {
                    summary: "Desktop change".into(),
                    description: None
                },
                true
            ),
            P::Commit(_)
        ));
        assert_eq!(git(&first, &["log", "-1", "--format=%s"]), "Desktop change");
        assert_eq!(git(&second, &["log", "-1", "--format=%s"]), "Initial");
        assert!(matches!(
            action(
                &cfg,
                &bus,
                a,
                A::CreateBranch {
                    name: "desktop-branch".into()
                },
                true
            ),
            P::Done(_)
        ));
        assert_eq!(git(&first, &["branch", "--show-current"]), "desktop-branch");
        let switched = action(
            &cfg,
            &bus,
            a,
            A::SwitchBranch {
                name: "main".into(),
            },
            true,
        );
        assert!(matches!(switched, P::Done(_)), "{switched:?}");
        assert_eq!(git(&first, &["branch", "--show-current"]), "main");
        // A local bare remote exercises publish/fetch/pull/push without touching
        // the user's network remotes or credential stores.
        let remote = first.join(".desktop-remote.git");
        std::fs::write(first.join(".git/info/exclude"), ".desktop-remote.git/\n").unwrap();
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "-b", "main"]);
        git(
            &first,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let published = action(
            &cfg,
            &bus,
            a,
            A::Publish {
                branch: "main".into(),
            },
            true,
        );
        assert!(matches!(published, P::Done(_)), "{published:?}");
        let peer = temp.path().join("peer");
        git(
            temp.path(),
            &["clone", remote.to_str().unwrap(), peer.to_str().unwrap()],
        );
        git(&peer, &["config", "user.name", "Peer"]);
        git(&peer, &["config", "user.email", "peer@example.invalid"]);
        std::fs::write(peer.join("peer.txt"), "remote change\n").unwrap();
        git(&peer, &["add", "peer.txt"]);
        git(&peer, &["commit", "-m", "Peer change"]);
        git(&peer, &["push"]);
        let fetched = action(&cfg, &bus, a, A::Fetch, true);
        assert!(matches!(fetched, P::Done(_)), "{fetched:?}");
        assert!(matches!(action(&cfg,&bus,a,A::Status,false),P::Status(s) if s.behind==1));
        let pulled = action(&cfg, &bus, a, A::Pull, true);
        assert!(matches!(pulled, P::Done(_)), "{pulled:?}");
        assert!(first.join("peer.txt").exists());
        std::fs::write(first.join("README.md"), "ready to push\n").unwrap();
        assert!(matches!(
            action(&cfg, &bus, a, A::StageAll, true),
            P::Done(_)
        ));
        assert!(matches!(
            action(
                &cfg,
                &bus,
                a,
                A::Commit {
                    summary: "Push me".into(),
                    description: None
                },
                true
            ),
            P::Commit(_)
        ));
        assert!(matches!(action(&cfg,&bus,a,A::Status,false),P::Status(s) if s.ahead==1));
        let pushed = action(&cfg, &bus, a, A::Sync, true);
        assert!(matches!(pushed, P::Done(_)), "{pushed:?}");
        assert_eq!(git(&remote, &["log", "-1", "--format=%s"]), "Push me");
        // An opaque selector cannot be replaced by a path or reused after config removal.
        assert!(matches!(
            action(&cfg, &bus, &first.to_string_lossy(), A::Status, false),
            P::Error(_)
        ));
        let mut removed = cfg.clone();
        removed.projects.clear();
        assert!(matches!(
            action(&removed, &bus, a, A::Status, false),
            P::Error(_)
        ));
        // Replacing the directory retires the old selector, even at the same path.
        std::fs::rename(&first, base.join("old-alpha")).unwrap();
        repo(&first);
        assert!(matches!(
            action(&cfg, &bus, a, A::Status, false),
            P::Error(_)
        ));
    }
    #[test]
    fn desktop_git_discovery_deduplicates_and_skips_generated_trees() {
        let temp = tempfile::tempdir().unwrap();
        repo(&temp.path().join("alpha"));
        repo(&temp.path().join("node_modules/hidden"));
        let mut cfg = config(temp.path());
        let mut duplicate = cfg.projects[0].clone();
        duplicate.id = "duplicate".into();
        cfg.projects.push(duplicate);
        assert_eq!(catalog(&cfg).unwrap().len(), 1);
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            repo(outside.path());
            std::os::unix::fs::symlink(outside.path(), temp.path().join("linked-outside")).unwrap();
            assert_eq!(catalog(&cfg).unwrap().len(), 1);
        }
    }
}
