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

/// How far below each configured root or folder nested repositories are found.
const SCAN_DEPTH: usize = 4;
/// Folders examined below one configured path before its scan stops.
const SCAN_FOLDERS_PER_START: usize = 4096;
/// Entries read from one folder; the rest of a huge folder is not descended.
const MAX_FOLDER_ENTRIES: usize = 4096;
const MAX_REPOSITORIES: usize = 512;

/// Build output, dependency trees and hidden folders hold thousands of
/// directories and never a project's own repository.
fn skip_scan_folder(name: &str) -> bool {
    name.starts_with('.')
        || name.starts_with("target")
        || matches!(
            name,
            "node_modules" | "vendor" | "dist" | "build" | "coverage" | "__pycache__" | "venv"
        )
}

/// A folder holding a usable repository: a `.git` directory with a `HEAD`, or
/// a `.git` file (worktree or submodule). A leftover empty `.git` directory is
/// not one, and listing it would only show an error row.
fn is_repository(dir: &Path) -> bool {
    let marker = dir.join(".git");
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.is_dir() => marker.join("HEAD").is_file(),
        Ok(metadata) => metadata.is_file(),
        Err(_) => false,
    }
}

/// A configured path that is a real directory (never a symlink), canonical.
fn usable_directory(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    path.canonicalize().ok()
}

/// Pin one repository and list it. A repository that cannot be pinned (it
/// moved, or its filesystem has no stable identity) is left out, never fatal.
fn push_repository(repos: &mut Vec<ConfiguredDesktopRepository>, label: &str, canonical: &Path) {
    let Ok(validated) = crate::workspace::service::validate_host_workspace_path(canonical, true)
    else {
        return;
    };
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
    // 1. Every configured root and folder is checked itself first -- exactly
    //    what 0.4.1 listed -- so no scan limit can drop a repository the user
    //    configured.
    let mut repo_paths = std::collections::BTreeSet::new();
    let mut repos = Vec::new();
    let mut starts = Vec::new();
    for (label, path) in roots {
        let Some(canonical) = usable_directory(&path) else {
            continue;
        };
        if is_repository(&canonical) && repo_paths.insert(canonical.clone()) {
            push_repository(&mut repos, &label, &canonical);
        }
        starts.push((label, canonical));
    }
    // 2. Nested repositories below each configured path, breadth-first. Each
    //    start gets its own folder budget so one large tree cannot starve the
    //    rest, a folder is scanned once however many starts reach it, and a
    //    limit or an unreadable folder ends that part of the scan instead of
    //    discarding everything already found.
    let mut visited = std::collections::BTreeSet::new();
    'starts: for (label, start) in starts {
        let mut budget = SCAN_FOLDERS_PER_START;
        let mut queue = std::collections::VecDeque::from([(label, start, 0usize)]);
        while let Some((label, dir, depth)) = queue.pop_front() {
            if depth > 0 {
                if !visited.insert(dir.clone()) {
                    continue;
                }
                if budget == 0 {
                    break;
                }
                budget -= 1;
                if is_repository(&dir) && repo_paths.insert(dir.clone()) {
                    if repos.len() >= MAX_REPOSITORIES {
                        break 'starts;
                    }
                    push_repository(&mut repos, &label, &dir);
                }
            }
            if depth >= SCAN_DEPTH {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut children: Vec<_> = entries
                .filter_map(Result::ok)
                .take(MAX_FOLDER_ENTRIES)
                .collect();
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                if skip_scan_folder(&name) {
                    continue;
                }
                // `dir` is canonical and the child is not a symlink, so the
                // joined path is canonical too.
                if child
                    .file_type()
                    .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
                {
                    queue.push_back((format!("{label} / {name}"), dir.join(&name), depth + 1));
                }
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
                matches!(
                    action,
                    A::SwitchBranch { .. }
                        | A::CreateBranch { .. }
                        | A::DeleteBranch { .. }
                        | A::Publish { .. }
                ),
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
                            .map(fit_payload_for_wire)
                            .map(explain_error_payload)
                            .unwrap_or_else(|outcome| {
                                P::Error(format!("Git operation was not accepted: {outcome:?}"))
                            }),
                    )
                }
                // The reason names the Git rule that refused the repository
                // and carries no path; without it every refusal reads the
                // same and cannot be acted on.
                Err(crate::git::command::GitError::InvalidRepositoryRoot { reason, .. }) => {
                    response(
                        repository_id,
                        P::Error(format!("Git could not open this repository: {reason}")),
                    )
                }
                Err(error) => response(repository_id, P::Error(error.to_string())),
            }
        }
        _ => QueryOutcome::Err(QueryError::InvalidRequest),
    }
}
/// Service errors often carry Git's raw stderr. A known failure is replaced
/// by its plain explanation; one already explained is left as it is.
fn explain_error_payload(payload: P) -> P {
    match payload {
        P::Error(message) => match crate::git::command::explain_git_failure(&message) {
            Some(explanation) if !message.contains(explanation) => {
                P::Error(explanation.to_string())
            }
            _ => P::Error(message),
        },
        other => other,
    }
}

/// A status reply has to fit one bounded host page (512 KiB, less envelope
/// headroom). A repository with thousands of untracked files -- e.g. agent
/// worktrees under `.claude/` -- would otherwise fail its whole status.
const MAX_STATUS_REPLY_BYTES: usize = 448 * 1024;

/// Keep as many status entries as fit and count the rest, so the window can
/// say how many were left out instead of showing an error.
fn fit_payload_for_wire(payload: P) -> P {
    let P::Status(mut status) = payload else {
        return payload;
    };
    let encoded_len = |status: &crate::git::git_service::GitStatusResult| {
        rmp_serde::to_vec_named(status)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    };
    if encoded_len(&status) <= MAX_STATUS_REPLY_BYTES {
        return P::Status(status);
    }
    let total = status.entries.len();
    let (mut keep, mut too_many) = (0usize, total);
    while keep + 1 < too_many {
        let mid = keep + (too_many - keep) / 2;
        let mut candidate = status.clone();
        candidate.entries.truncate(mid);
        if encoded_len(&candidate) <= MAX_STATUS_REPLY_BYTES {
            keep = mid;
        } else {
            too_many = mid;
        }
    }
    status.entries.truncate(keep);
    status.omitted_entries = u32::try_from(total - keep).unwrap_or(u32::MAX);
    P::Status(status)
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
        // History marks exactly the commit a push would send.
        let history = action(&cfg, &bus, a, A::History { limit: 10, skip: 0 }, false);
        let P::History(entries) = history else {
            panic!("{history:?}")
        };
        assert_eq!(entries[0].subject, "Push me");
        assert!(entries[0].unpushed, "{:?}", entries[0]);
        assert!(
            entries[1..].iter().all(|entry| !entry.unpushed),
            "{entries:?}"
        );
        let pushed = action(&cfg, &bus, a, A::Sync, true);
        assert!(matches!(pushed, P::Done(_)), "{pushed:?}");
        assert_eq!(git(&remote, &["log", "-1", "--format=%s"]), "Push me");
        let history = action(&cfg, &bus, a, A::History { limit: 10, skip: 0 }, false);
        let P::History(entries) = history else {
            panic!("{history:?}")
        };
        assert!(entries.iter().all(|entry| !entry.unpushed), "{entries:?}");
        // A branch that has diverged from its remote pulls as a merge, like
        // GitHub Desktop, instead of Git refusing for want of `pull.rebase`.
        git(&peer, &["pull", "--no-rebase"]);
        std::fs::write(peer.join("peer.txt"), "second remote change\n").unwrap();
        git(&peer, &["commit", "-am", "Peer second change"]);
        git(&peer, &["push"]);
        std::fs::write(first.join("local.txt"), "local change\n").unwrap();
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
                    summary: "Local diverged".into(),
                    description: None
                },
                true
            ),
            P::Commit(_)
        ));
        let fetched = action(&cfg, &bus, a, A::Fetch, true);
        assert!(matches!(fetched, P::Done(_)), "{fetched:?}");
        assert!(
            matches!(action(&cfg,&bus,a,A::Status,false),P::Status(s) if s.ahead==1 && s.behind==1)
        );
        let merged = action(&cfg, &bus, a, A::Pull, true);
        assert!(matches!(merged, P::Done(_)), "{merged:?}");
        assert_eq!(
            git(&first, &["log", "-1", "--format=%P"])
                .split_whitespace()
                .count(),
            2,
            "the pull made a merge commit"
        );
        assert!(first.join("local.txt").exists());
        assert_eq!(
            std::fs::read_to_string(first.join("peer.txt")).unwrap(),
            "second remote change\n"
        );
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

    #[test]
    fn desktop_git_keeps_configured_folders_when_the_scan_hits_its_limits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("big");
        // More folders than one start may scan: the budget runs out before
        // the configured folder, which sorts last, is ever reached.
        for index in 0..(SCAN_FOLDERS_PER_START + 50) {
            std::fs::create_dir_all(root.join(format!("d{index:05}"))).unwrap();
        }
        let fake_repo = |path: &Path| {
            std::fs::create_dir_all(path.join(".git")).unwrap();
            std::fs::write(path.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        };
        fake_repo(&root.join("zzzz-app"));
        fake_repo(&root.join("a-nested"));
        fake_repo(&root.join("target-watch/debug/generated"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = root.join("b-locked");
            std::fs::create_dir_all(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let cfg: AppConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "projects": [{
                "id": "desktop-project", "name": "Base folder",
                "rootPath": root.to_string_lossy(),
                "folders": [{
                    "id": "app", "name": "App",
                    "folderPath": root.join("zzzz-app").to_string_lossy(),
                    "commands": []
                }],
                "createdAt": "2026-09-08T00:00:00Z", "updatedAt": "2026-09-08T00:00:00Z"
            }],
            "settings": {}, "sshConnections": []
        }))
        .unwrap();
        let labels: Vec<String> = catalog(&cfg)
            .expect("a scan limit or unreadable folder never fails the catalog")
            .into_iter()
            .map(|repo| repo.entry.label)
            .collect();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("b-locked"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        assert!(
            labels.iter().any(|label| label == "Base folder / App"),
            "a configured folder repository is always listed: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == "Base folder / a-nested"),
            "nested repositories are still found: {labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("target-watch")),
            "build output is never scanned: {labels:?}"
        );
    }

    #[test]
    fn oversized_status_replies_are_trimmed_to_fit_and_count_the_rest() {
        use crate::git::git_service::{GitFileStatus, GitStatusEntry, GitStatusResult};
        let entry = |index: usize| GitStatusEntry {
            path: format!(
                ".claude/worktrees/agent-{index:05}/some/deeply/nested/generated/file-{index}.txt"
            ),
            status: GitFileStatus::Untracked,
            staged: false,
            original_path: None,
        };
        let small = GitStatusResult {
            branch: Some("main".into()),
            upstream: None,
            ahead: 0,
            behind: 0,
            entries: (0..10).map(entry).collect(),
            is_detached: false,
            is_merging: false,
            is_rebasing: false,
            omitted_entries: 0,
        };
        let P::Status(kept) = fit_payload_for_wire(P::Status(small.clone())) else {
            panic!("status payload");
        };
        assert_eq!(kept, small, "a status that fits is sent untouched");
        let big = GitStatusResult {
            entries: (0..20_000).map(entry).collect(),
            ..small
        };
        let P::Status(trimmed) = fit_payload_for_wire(P::Status(big)) else {
            panic!("status payload");
        };
        assert!(
            !trimmed.entries.is_empty(),
            "as many entries as fit are kept"
        );
        assert_eq!(
            trimmed.entries.len() + trimmed.omitted_entries as usize,
            20_000,
            "every left-out entry is counted"
        );
        assert!(rmp_serde::to_vec_named(&trimmed).unwrap().len() <= MAX_STATUS_REPLY_BYTES);
    }
}
