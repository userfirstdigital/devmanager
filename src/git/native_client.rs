//! Adapter for the 0.4.1 Git desktop view. Repository names here are opaque
//! selectors; only the host can resolve them to a live repository capability.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    desktop::{DesktopGitAction as Action, DesktopGitPayload},
    git_service,
};
use crate::client::{HostClient, HostClientConfig};
use crate::domain::cockpit::{TaskCockpitQuery, TaskCockpitResult, TaskRepositorySelector};
use crate::domain::{id::TaskId, ClientId};
use crate::protocol::{Capability, CapabilitySet, FrameLimits};
use crate::remote::{RemoteAction, RemoteActionPayload as Payload, RemoteActionResult};

#[derive(Default)]
struct AuthSession {
    token: Option<String>,
    username: Option<String>,
    device_code: Option<String>,
}

/// Credentials live only for this shell lifetime and never enter profile JSON.
#[derive(Clone, Default)]
pub struct NativeGitSession(Arc<Mutex<AuthSession>>);

#[derive(Clone)]
pub struct NativeGitClient {
    profile: String,
    task_id: TaskId,
    repositories: Arc<Vec<(String, TaskRepositorySelector)>>,
    auth: Arc<Mutex<AuthSession>>,
}

impl NativeGitClient {
    pub fn new(
        profile: String,
        task_id: TaskId,
        repositories: Vec<(String, TaskRepositorySelector)>,
        session: NativeGitSession,
    ) -> Self {
        Self {
            profile,
            task_id,
            repositories: Arc::new(repositories),
            auth: session.0,
        }
    }

    fn cockpit(&self, query: TaskCockpitQuery) -> Result<TaskCockpitResult, String> {
        let config = HostClientConfig {
            named_profile: self.profile.clone(),
            client_build: format!("native-git/{}", env!("CARGO_PKG_VERSION")),
            client_id: ClientId::new(),
            requested: CapabilitySet::from_capabilities([Capability::TaskCockpit]),
            limits: FrameLimits::v1_default(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(90), async {
                let mut client = HostClient::connect(config)
                    .await
                    .map_err(|e| e.to_string())?;
                client
                    .query_task_cockpit(self.task_id, query)
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("Git request failed: {e:?}"))
            })
            .await
            .map_err(|_| {
                "Git request timed out. Refresh to check the repository before retrying."
                    .to_string()
            })?
        })
    }

    fn query(&self, repository: &str, action: Action) -> Result<DesktopGitPayload, String> {
        let selector = self
            .repositories
            .iter()
            .find(|(id, _)| id == repository)
            .map(|(_, selector)| selector.clone())
            .ok_or("Repository is no longer available.")?;
        let confirm = action.is_mutation();
        match self.cockpit(TaskCockpitQuery::GitDesktopTargeted {
            selector: selector.clone(),
            action,
            confirm,
        })? {
            TaskCockpitResult::GitDesktop {
                task_id,
                selector: returned,
                payload,
            } if task_id == self.task_id && returned == selector => Ok(payload),
            other => Err(format!("Git request was not accepted: {other:?}")),
        }
    }

    pub fn load_repositories(mut self) -> Result<(Self, Vec<(String, String)>), String> {
        let TaskCockpitResult::GitRepositories(catalog) =
            self.cockpit(TaskCockpitQuery::GitRepositories)?
        else {
            return Err("The host could not load this task's repositories.".into());
        };
        if catalog.task_id != self.task_id {
            return Err("Repository response did not match the task.".into());
        }
        let preferred = self
            .repositories
            .first()
            .map(|(_, selector)| selector.clone());
        let mut entries = catalog
            .repositories
            .into_iter()
            .filter(|repo| repo.available)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| Some(&entry.selector) != preferred.as_ref());
        if entries.is_empty() {
            return Err("No repositories are available for this task.".into());
        }
        let repos = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.label.clone(), format!("repository-{index}")))
            .collect();
        self.repositories = Arc::new(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, entry)| (format!("repository-{index}"), entry.selector))
                .collect(),
        );
        Ok((self, repos))
    }

    pub fn request(&self, request: RemoteAction) -> Result<RemoteActionResult, String> {
        let (repo, action) = match request {
            RemoteAction::GitStatus { repo_path } => (repo_path, Action::Status),
            RemoteAction::GitLog {
                repo_path,
                limit,
                skip,
            } => (repo_path, Action::History { limit, skip }),
            RemoteAction::GitDiffFile {
                repo_path,
                file_path,
                staged,
            } => (
                repo_path,
                Action::FileDiff {
                    relative_path: file_path,
                    staged,
                },
            ),
            RemoteAction::GitDiffCommit { repo_path, hash } => {
                (repo_path, Action::CommitDiff { hash })
            }
            RemoteAction::GitBranches { repo_path } => (repo_path, Action::Branches),
            RemoteAction::GitStage { repo_path, files } => {
                (repo_path, Action::Stage { paths: files })
            }
            RemoteAction::GitUnstage { repo_path, files } => {
                (repo_path, Action::Unstage { paths: files })
            }
            RemoteAction::GitStageAll { repo_path } => (repo_path, Action::StageAll),
            RemoteAction::GitUnstageAll { repo_path } => (repo_path, Action::UnstageAll),
            RemoteAction::GitCommit {
                repo_path,
                summary,
                body,
            } => (
                repo_path,
                Action::Commit {
                    summary,
                    description: body,
                },
            ),
            RemoteAction::GitFetch { repo_path } => (repo_path, Action::Fetch),
            RemoteAction::GitPull { repo_path } => (repo_path, Action::Pull),
            RemoteAction::GitPush { repo_path } | RemoteAction::GitSync { repo_path } => {
                (repo_path, Action::Push)
            }
            RemoteAction::GitPushSetUpstream { repo_path, branch } => {
                (repo_path, Action::Publish { branch })
            }
            RemoteAction::GitSwitchBranch { repo_path, name } => {
                (repo_path, Action::SwitchBranch { name })
            }
            RemoteAction::GitCreateBranch { repo_path, name } => {
                (repo_path, Action::CreateBranch { name })
            }
            RemoteAction::GitDeleteBranch { repo_path, name } => {
                (repo_path, Action::DeleteBranch { name })
            }
            RemoteAction::GitGenerateCommitMessage { repo_path } => {
                let token = self
                    .auth
                    .lock()
                    .map_err(|_| "GitHub session unavailable")?
                    .token
                    .clone()
                    .ok_or("Sign in to GitHub to generate a commit message.")?;
                let diff = match self.query(&repo_path, Action::StagedDiff)? {
                    DesktopGitPayload::StagedDiff(diff) => diff,
                    DesktopGitPayload::Error(error) => return Err(error),
                    _ => return Err("Host did not return staged changes.".into()),
                };
                let message = git_service::generate_commit_message(&token, &diff)?;
                return Ok(RemoteActionResult::ok(
                    None,
                    Some(Payload::GitCommitMessage { message }),
                ));
            }
            RemoteAction::GitGetGithubAuthStatus => {
                let auth = self.auth.lock().map_err(|_| "GitHub session unavailable")?;
                return Ok(RemoteActionResult::ok(
                    None,
                    Some(Payload::GitAuthStatus {
                        has_token: auth.token.is_some(),
                        username: auth.username.clone(),
                    }),
                ));
            }
            RemoteAction::GitRequestDeviceCode => {
                let device_code = git_service::request_device_code(&github_client_id())?;
                self.auth
                    .lock()
                    .map_err(|_| "GitHub session unavailable")?
                    .device_code = Some(device_code.device_code.clone());
                return Ok(RemoteActionResult::ok(
                    None,
                    Some(Payload::GitDeviceCode { device_code }),
                ));
            }
            RemoteAction::GitPollForToken { device_code } => {
                if self
                    .auth
                    .lock()
                    .map_err(|_| "GitHub session unavailable")?
                    .device_code
                    .as_deref()
                    != Some(&device_code)
                {
                    return Err("GitHub sign-in was cancelled.".into());
                }
                let token = git_service::poll_for_token(&github_client_id(), &device_code)?;
                let completed = token.is_some();
                if let Some(token) = token {
                    let username = git_service::get_github_username(&token.access_token).ok();
                    let mut auth = self.auth.lock().map_err(|_| "GitHub session unavailable")?;
                    if auth.device_code.as_deref() != Some(&device_code) {
                        return Err("GitHub sign-in was cancelled.".into());
                    }
                    *auth = AuthSession {
                        token: Some(token.access_token),
                        username,
                        device_code: None,
                    };
                }
                let username = self
                    .auth
                    .lock()
                    .map_err(|_| "GitHub session unavailable")?
                    .username
                    .clone();
                return Ok(RemoteActionResult::ok(
                    None,
                    Some(Payload::GitTokenPoll {
                        completed,
                        username,
                    }),
                ));
            }
            RemoteAction::GitLogout => {
                *self.auth.lock().map_err(|_| "GitHub session unavailable")? =
                    AuthSession::default();
                return Ok(RemoteActionResult::ok(None, None));
            }
            _ => return Err("This action is unavailable in the Git window.".into()),
        };
        Ok(match self.query(&repo, action)? {
            DesktopGitPayload::Status(status) => {
                RemoteActionResult::ok(None, Some(Payload::GitStatus { status }))
            }
            DesktopGitPayload::History(entries) => {
                RemoteActionResult::ok(None, Some(Payload::GitLogEntries { entries }))
            }
            DesktopGitPayload::Diff(diff) => {
                RemoteActionResult::ok(None, Some(Payload::GitDiff { diff }))
            }
            DesktopGitPayload::Branches(branches) => {
                RemoteActionResult::ok(None, Some(Payload::GitBranches { branches }))
            }
            DesktopGitPayload::Commit(hash) => {
                RemoteActionResult::ok(None, Some(Payload::GitCommit { hash }))
            }
            DesktopGitPayload::Done(message) => RemoteActionResult::ok(Some(message), None),
            DesktopGitPayload::Error(error) => RemoteActionResult::error(error),
            DesktopGitPayload::StagedDiff(_) => {
                return Err("Unexpected staged diff response.".into())
            }
        })
    }
}

fn github_client_id() -> String {
    git_service::get_github_client_id().unwrap_or_default()
}
