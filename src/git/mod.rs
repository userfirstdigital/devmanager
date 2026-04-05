pub mod git_service;
mod git_ui;

use crate::persistence;
use crate::remote::{RemoteAction, RemoteActionPayload, RemoteClientHandle};
use crate::theme;
use git_service::{GitBranch, GitDiffResult, GitLogEntry, GitStatusEntry, GitStatusResult};
use gpui::{
    anchored, deferred, div, prelude::*, px, rgb, Context, Corner, FocusHandle, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Window,
};
use std::time::Instant;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitView {
    Changes,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitField {
    CommitSummary,
    CommitDescription,
    NewBranchName,
    BranchFilter,
    FileFilter,
}

// ── Repo entry ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub label: String,
    pub path: String,
    pub has_changes: bool,
    pub behind: u32,
}

// ── Login state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoginState {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub is_polling: bool,
}

// ── GitWindow ───────────────────────────────────────────────────────────────

pub struct GitWindow {
    backend: GitBackend,
    pub repos: Vec<RepoEntry>,
    pub active_repo: usize,
    pub show_repo_dropdown: bool,
    focus: FocusHandle,
    pub active_view: GitView,
    pub status: Option<GitStatusResult>,
    pub selected_file: Option<String>,
    pub file_diff: Option<GitDiffResult>,
    pub file_filter: String,
    pub commit_summary: String,
    pub commit_description: String,
    pub active_field: Option<GitField>,
    pub cursor: usize,
    pub branches: Vec<GitBranch>,
    pub branch_filter: String,
    pub show_branch_dropdown: bool,
    pub new_branch_name: String,
    pub log_entries: Vec<GitLogEntry>,
    pub selected_commit: Option<String>,
    pub commit_diff: Option<GitDiffResult>,
    pub log_page: u32,
    pub is_loading: bool,
    pub is_pushing: bool,
    pub is_pulling: bool,
    pub is_fetching: bool,
    pub is_generating_message: bool,
    pub github_token: Option<String>,
    pub github_username: Option<String>,
    pub login_state: Option<LoginState>,
    pub last_fetch_at: Option<Instant>,
    pub operation_result: Option<(bool, String)>,
}

#[derive(Clone)]
enum GitBackend {
    Local,
    Remote(RemoteClientHandle),
}

macro_rules! git_spawn {
    ($cx:expr, |$this:ident, $acx:ident| $body:block) => {
        $cx.spawn(move |$this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
            let mut $acx = cx.clone();
            async move $body
        })
        .detach();
    };
}

impl GitWindow {
    pub fn new(repos: Vec<(String, String)>, cx: &mut Context<Self>) -> Self {
        Self::new_with_backend(repos, GitBackend::Local, cx)
    }

    pub fn new_remote(
        repos: Vec<(String, String)>,
        client: RemoteClientHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_backend(repos, GitBackend::Remote(client), cx)
    }

    fn new_with_backend(
        repos: Vec<(String, String)>,
        backend: GitBackend,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let repos: Vec<RepoEntry> = repos
            .into_iter()
            .map(|(label, path)| RepoEntry {
                label,
                path,
                has_changes: false,
                behind: 0,
            })
            .collect();
        let mut win = Self {
            backend,
            repos,
            active_repo: 0,
            show_repo_dropdown: false,
            focus,
            active_view: GitView::Changes,
            status: None,
            selected_file: None,
            file_diff: None,
            file_filter: String::new(),
            commit_summary: String::new(),
            commit_description: String::new(),
            active_field: None,
            cursor: 0,
            branches: Vec::new(),
            branch_filter: String::new(),
            show_branch_dropdown: false,
            new_branch_name: String::new(),
            log_entries: Vec::new(),
            selected_commit: None,
            commit_diff: None,
            log_page: 0,
            is_loading: true,
            is_pushing: false,
            is_pulling: false,
            is_fetching: false,
            is_generating_message: false,
            github_token: None,
            github_username: None,
            login_state: None,
            last_fetch_at: None,
            operation_result: None,
        };
        if matches!(win.backend, GitBackend::Local) {
            win.load_persisted_token();
        }
        win.fetch_github_username(cx);
        win.refresh_status(cx);
        win
    }

    pub fn repo_path(&self) -> &str {
        &self.repos[self.active_repo].path
    }

    pub fn repo_label(&self) -> &str {
        &self.repos[self.active_repo].label
    }

    fn remote_client(&self) -> Option<RemoteClientHandle> {
        match &self.backend {
            GitBackend::Local => None,
            GitBackend::Remote(client) => Some(client.clone()),
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self.backend, GitBackend::Remote(_))
    }

    fn set_remote_auth_state(&mut self, has_token: bool, username: Option<String>) {
        self.github_token = has_token.then(|| "__remote_host__".to_string());
        self.github_username = username;
    }

    fn has_mutation_control(&self) -> bool {
        self.remote_client()
            .and_then(|client| client.latest_snapshot())
            .map(|snapshot| snapshot.you_have_control)
            .unwrap_or(true)
    }

    fn ensure_mutation_control(&mut self, cx: &mut Context<Self>) -> bool {
        if self.has_mutation_control() {
            return true;
        }
        self.operation_result = Some((
            false,
            "Take control before changing Git state on the remote host.".to_string(),
        ));
        cx.notify();
        false
    }

    fn logout_github(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if let Some(client) = self.remote_client() {
            git_spawn!(cx, |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.request(RemoteAction::GitLogout) })
                    .await;
                let _ = this.update(&mut cx, |this, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            this.github_token = None;
                            this.github_username = None;
                            this.operation_result = Some((true, "Logged out".to_string()));
                        }
                        Ok(result) => {
                            this.operation_result = Some((
                                false,
                                result.message.unwrap_or_else(|| {
                                    "Could not log out from GitHub.".to_string()
                                }),
                            ));
                        }
                        Err(error) => {
                            this.operation_result = Some((false, error));
                        }
                    }
                    cx.notify();
                });
            });
            return;
        }

        self.github_token = None;
        self.github_username = None;
        Self::persist_github_token(None);
        self.operation_result = Some((true, "Logged out".to_string()));
        cx.notify();
    }

    pub fn switch_repo(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.repos.len() || index == self.active_repo {
            return;
        }
        self.active_repo = index;
        self.show_repo_dropdown = false;
        self.status = None;
        self.selected_file = None;
        self.file_diff = None;
        self.log_entries.clear();
        self.selected_commit = None;
        self.commit_diff = None;
        self.log_page = 0;
        self.branches.clear();
        self.operation_result = None;
        self.refresh_status(cx);
        if self.active_view == GitView::History {
            self.load_history(cx);
        }
    }

    // ── Repo status scanning ──────────────────────────────────────────

    pub fn refresh_all_repo_statuses(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<(usize, String)> = self
            .repos
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.path.clone()))
            .collect();

        let remote_client = self.remote_client();

        git_spawn!(cx, |this, cx| {
            let results: Vec<(usize, bool, u32)> =
                cx.background_executor()
                    .spawn(async move {
                        paths
                            .into_iter()
                            .map(|(i, path)| {
                                let status = if let Some(client) = remote_client.clone() {
                                    match client.request(RemoteAction::GitStatus {
                                        repo_path: path.clone(),
                                    }) {
                                        Ok(result) if result.ok => match result.payload {
                                            Some(RemoteActionPayload::GitStatus { status }) => {
                                                Ok(status)
                                            }
                                            _ => Err("Remote host did not return Git status."
                                                .to_string()),
                                        },
                                        Ok(result) => Err(result.message.unwrap_or_else(|| {
                                            "Could not load remote Git status.".to_string()
                                        })),
                                        Err(error) => Err(error),
                                    }
                                } else {
                                    git_service::status(&path)
                                };
                                match status {
                                    Ok(s) => {
                                        let has_changes = !s.entries.is_empty();
                                        (i, has_changes, s.behind)
                                    }
                                    Err(_) => (i, false, 0),
                                }
                            })
                            .collect()
                    })
                    .await;
            let _ = this.update(&mut cx, |this, cx| {
                for (i, has_changes, behind) in results {
                    if let Some(repo) = this.repos.get_mut(i) {
                        repo.has_changes = has_changes;
                        repo.behind = behind;
                    }
                }
                cx.notify();
            });
        });
    }

    // ── Data loading ────────────────────────────────────────────────────

    pub fn refresh_status(&mut self, cx: &mut Context<Self>) {
        self.refresh_status_inner(true, cx);
    }

    fn refresh_status_inner(&mut self, auto_stage: bool, cx: &mut Context<Self>) {
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        self.is_loading = true;
        git_spawn!(cx, |this, cx| {
            // Auto-stage all files so they appear checked by default (like GitHub Desktop)
            if auto_stage {
                if let Some(client) = remote_client.clone() {
                    let repo2 = repo.clone();
                    let _ = cx
                        .background_executor()
                        .spawn(async move {
                            client.request(RemoteAction::GitStageAll { repo_path: repo2 })
                        })
                        .await;
                } else {
                    let repo2 = repo.clone();
                    let _ = cx
                        .background_executor()
                        .spawn(async move { git_service::stage_all(&repo2) })
                        .await;
                }
            }
            let status = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitStatus { repo_path: repo }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitStatus { status }) => Ok(status),
                                _ => Err("Remote host did not return Git status.".to_string()),
                            },
                            Ok(result) => Err(result.message.unwrap_or_else(|| {
                                "Could not load remote Git status.".to_string()
                            })),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::status(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_loading = false;
                match status {
                    Ok(s) => {
                        this.status = Some(s);
                        if this.selected_file.is_none() {
                            if let Some(ref st) = this.status {
                                if let Some(first) = st.entries.first() {
                                    this.select_file(&first.path.clone(), cx);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        this.operation_result = Some((false, e));
                    }
                }
                cx.notify();
            });
        });
    }

    pub fn load_branches(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let branches = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitBranches { repo_path: repo }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitBranches { branches }) => Ok(branches),
                                _ => Err("Remote host did not return branch data.".to_string()),
                            },
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not load remote branches.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::branches(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Ok(b) = branches {
                    this.branches = b;
                }
                cx.notify();
            });
        });
    }

    pub fn load_history(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo_path().to_string();
        let skip = self.log_page * 50;
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let entries = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitLog {
                            repo_path: repo,
                            limit: 50,
                            skip,
                        }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitLogEntries { entries }) => Ok(entries),
                                _ => Err("Remote host did not return Git history.".to_string()),
                            },
                            Ok(result) => Err(result.message.unwrap_or_else(|| {
                                "Could not load remote Git history.".to_string()
                            })),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::log(&repo, 50, skip)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Ok(e) = entries {
                    if this.log_page == 0 {
                        this.log_entries = e;
                    } else {
                        this.log_entries.extend(e);
                    }
                }
                cx.notify();
            });
        });
    }

    // ── File selection + diff ───────────────────────────────────────────

    pub fn select_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.selected_file = Some(path.to_string());
        self.file_diff = None;
        let repo = self.repo_path().to_string();
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        let staged = self
            .status
            .as_ref()
            .and_then(|s| s.entries.iter().find(|e| e.path == file_path))
            .map(|e| e.staged)
            .unwrap_or(false);

        git_spawn!(cx, |this, cx| {
            let diff = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitDiffFile {
                            repo_path: repo,
                            file_path,
                            staged,
                        }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitDiff { diff }) => Ok(diff),
                                _ => Err("Remote host did not return a file diff.".to_string()),
                            },
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not load remote file diff.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::diff_file(&repo, &file_path, staged)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match diff {
                    Ok(d) => this.file_diff = Some(d),
                    Err(_) => this.file_diff = None,
                }
                cx.notify();
            });
        });
    }

    pub fn select_commit(&mut self, hash: &str, cx: &mut Context<Self>) {
        self.selected_commit = Some(hash.to_string());
        self.commit_diff = None;
        let repo = self.repo_path().to_string();
        let hash = hash.to_string();
        let remote_client = self.remote_client();

        git_spawn!(cx, |this, cx| {
            let diff = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitDiffCommit {
                            repo_path: repo,
                            hash,
                        }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitDiff { diff }) => Ok(diff),
                                _ => Err("Remote host did not return a commit diff.".to_string()),
                            },
                            Ok(result) => Err(result.message.unwrap_or_else(|| {
                                "Could not load remote commit diff.".to_string()
                            })),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::diff_commit(&repo, &hash)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match diff {
                    Ok(d) => this.commit_diff = Some(d),
                    Err(_) => this.commit_diff = None,
                }
                cx.notify();
            });
        });
    }

    // ── Staging ─────────────────────────────────────────────────────────

    pub fn stage_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitStage {
                            repo_path: repo,
                            files: vec![file_path],
                        }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not stage file.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::stage(&repo, &[file_path.as_str()])
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Err(error) = result {
                    this.operation_result = Some((false, error));
                }
                this.refresh_status(cx);
            });
        });
    }

    pub fn unstage_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let file_path = path.to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitUnstage {
                            repo_path: repo,
                            files: vec![file_path],
                        }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not unstage file.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::unstage(&repo, &[file_path.as_str()])
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Err(error) = result {
                    this.operation_result = Some((false, error));
                }
                this.refresh_status(cx);
            });
        });
    }

    pub fn stage_all(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitStageAll { repo_path: repo }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not stage all files.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::stage_all(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Err(error) = result {
                    this.operation_result = Some((false, error));
                }
                this.refresh_status(cx);
            });
        });
    }

    pub fn unstage_all(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitUnstageAll { repo_path: repo }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not unstage all files.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::unstage_all(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Err(error) = result {
                    this.operation_result = Some((false, error));
                }
                this.refresh_status(cx);
            });
        });
    }

    // ── Token persistence ────────────────────────────────────────────

    pub fn persist_github_token(token: Option<String>) {
        if let Ok(mut config) = persistence::load_config() {
            config.settings.github_token = token;
            let _ = persistence::save_config(&config);
        }
    }

    fn load_persisted_token(&mut self) {
        if self.github_token.is_some() {
            return;
        }
        if let Ok(config) = persistence::load_config() {
            if let Some(ref token) = config.settings.github_token {
                if !token.is_empty() {
                    self.github_token = Some(token.clone());
                }
            }
        }
    }

    fn fetch_github_username(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = self.remote_client() {
            git_spawn!(cx, |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.request(RemoteAction::GitGetGithubAuthStatus) })
                    .await;
                let _ = this.update(&mut cx, |this, cx| {
                    match result {
                        Ok(result) if result.ok => {
                            if let Some(RemoteActionPayload::GitAuthStatus {
                                has_token,
                                username,
                            }) = result.payload
                            {
                                this.set_remote_auth_state(has_token, username);
                            }
                        }
                        Ok(result) => {
                            this.operation_result = Some((
                                false,
                                result.message.unwrap_or_else(|| {
                                    "Could not load remote GitHub auth state.".to_string()
                                }),
                            ));
                        }
                        Err(error) => {
                            this.operation_result = Some((false, error));
                        }
                    }
                    cx.notify();
                });
            });
            return;
        }

        let Some(ref token) = self.github_token else {
            return;
        };
        let token = token.clone();
        git_spawn!(cx, |this, cx| {
            let username = cx
                .background_executor()
                .spawn(async move { git_service::get_github_username(&token) })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                if let Ok(name) = username {
                    this.github_username = Some(name);
                    cx.notify();
                }
            });
        });
    }

    // ── GitHub login ─────────────────────────────────────────────────

    pub fn start_github_login(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if let Some(client) = self.remote_client() {
            git_spawn!(cx, |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { client.request(RemoteAction::GitRequestDeviceCode) })
                    .await;
                let _ = this.update(&mut cx, |this, cx| {
                    match result {
                        Ok(result) if result.ok => match result.payload {
                            Some(RemoteActionPayload::GitDeviceCode { device_code }) => {
                                let _ = crate::services::open_url(&device_code.verification_uri);
                                this.login_state = Some(LoginState {
                                    user_code: device_code.user_code,
                                    verification_uri: device_code.verification_uri,
                                    device_code: device_code.device_code,
                                    is_polling: true,
                                });
                                this.poll_github_login(String::new(), cx);
                            }
                            _ => {
                                this.operation_result = Some((
                                    false,
                                    "Remote host did not return a GitHub device code.".to_string(),
                                ));
                            }
                        },
                        Ok(result) => {
                            this.operation_result = Some((
                                false,
                                result.message.unwrap_or_else(|| {
                                    "Could not start remote GitHub login.".to_string()
                                }),
                            ));
                        }
                        Err(error) => {
                            this.operation_result = Some((false, error));
                        }
                    }
                    cx.notify();
                });
            });
            return;
        }

        let client_id = match git_service::get_github_client_id() {
            Some(id) => id,
            None => {
                self.operation_result = Some((
                    false,
                    "Set DEVMANAGER_GITHUB_CLIENT_ID env var or register an OAuth app at github.com/settings/developers".to_string(),
                ));
                cx.notify();
                return;
            }
        };

        let cid = client_id.clone();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { git_service::request_device_code(&cid) })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match result {
                    Ok(resp) => {
                        // Open browser for user to enter code
                        let _ = crate::services::open_url(&resp.verification_uri);
                        this.login_state = Some(LoginState {
                            user_code: resp.user_code,
                            verification_uri: resp.verification_uri,
                            device_code: resp.device_code,
                            is_polling: true,
                        });
                        // Start polling
                        this.poll_github_login(client_id, cx);
                    }
                    Err(e) => {
                        this.operation_result = Some((false, e));
                    }
                }
                cx.notify();
            });
        });
    }

    fn poll_github_login(&mut self, client_id: String, cx: &mut Context<Self>) {
        let Some(ref state) = self.login_state else {
            return;
        };
        let device_code = state.device_code.clone();
        let cid = client_id.clone();
        let remote_client = self.remote_client();

        cx.spawn(
            move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let mut cx = cx.clone();
                async move {
                    // Poll every 5 seconds, up to 60 attempts (5 minutes)
                    for _ in 0..60 {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(5))
                            .await;
                        let dc = device_code.clone();
                        let remote_client = remote_client.clone();
                        let cid = cid.clone();
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                if let Some(client) = remote_client.clone() {
                                    match client
                                        .request(RemoteAction::GitPollForToken { device_code: dc })
                                    {
                                        Ok(result) if result.ok => match result.payload {
                                            Some(RemoteActionPayload::GitTokenPoll {
                                                completed,
                                                username,
                                            }) => Ok((completed, username, None)),
                                            _ => Err(
                                                "Remote host did not return GitHub login state."
                                                    .to_string(),
                                            ),
                                        },
                                        Ok(result) => Err(result.message.unwrap_or_else(|| {
                                            "Remote GitHub login failed.".to_string()
                                        })),
                                        Err(error) => Err(error),
                                    }
                                } else {
                                    let cid2 = cid.clone();
                                    match git_service::poll_for_token(&cid2, &dc) {
                                        Ok(Some(token_resp)) => Ok((
                                            true,
                                            git_service::get_github_username(
                                                &token_resp.access_token,
                                            )
                                            .ok(),
                                            Some(token_resp.access_token),
                                        )),
                                        Ok(None) => Ok((false, None, None)),
                                        Err(error) => Err(error),
                                    }
                                }
                            })
                            .await;
                        let should_stop = this
                            .update(&mut cx, |this, cx| match result {
                                Ok((true, username, token)) => {
                                    if this.is_remote() {
                                        this.set_remote_auth_state(true, username);
                                    } else {
                                        if let Some(token) = token {
                                            this.github_token = Some(token.clone());
                                            Self::persist_github_token(Some(token));
                                        }
                                        this.github_username = username;
                                    }
                                    this.login_state = None;
                                    this.operation_result =
                                        Some((true, "Logged in to GitHub".to_string()));
                                    cx.notify();
                                    return true;
                                }
                                Ok((false, _, _)) => {
                                    return false;
                                }
                                Err(e) => {
                                    this.login_state = None;
                                    this.operation_result = Some((false, e));
                                    cx.notify();
                                    return true;
                                }
                            })
                            .unwrap_or(true);
                        if should_stop {
                            return;
                        }
                    }
                    // Timed out
                    let _ = this.update(&mut cx, |this, cx| {
                        this.login_state = None;
                        this.operation_result =
                            Some((false, "Login timed out. Please try again.".to_string()));
                        cx.notify();
                    });
                }
            },
        )
        .detach();
    }

    // ── AI commit message ─────────────────────────────────────────────

    pub fn generate_commit_message(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.github_token.is_none() {
            self.operation_result =
                Some((false, "GitHub token not configured in Settings".to_string()));
            cx.notify();
            return;
        }
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        let token = self.github_token.clone().unwrap_or_default();
        self.is_generating_message = true;
        cx.notify();

        git_spawn!(cx, |this, cx| {
            let result =
                cx.background_executor()
                    .spawn(async move {
                        if let Some(client) = remote_client {
                            match client
                                .request(RemoteAction::GitGenerateCommitMessage { repo_path: repo })
                            {
                                Ok(result) if result.ok => match result.payload {
                                    Some(RemoteActionPayload::GitCommitMessage { message }) => {
                                        Ok(message)
                                    }
                                    _ => Err("Remote host did not return an AI commit message."
                                        .to_string()),
                                },
                                Ok(result) => Err(result
                                    .message
                                    .unwrap_or_else(|| "AI: request failed".to_string())),
                                Err(error) => Err(error),
                            }
                        } else {
                            let diff = git_service::get_staged_diff(&repo)?;
                            if diff.trim().is_empty() {
                                return Err("No staged changes to summarize".to_string());
                            }
                            git_service::generate_commit_message(&token, &diff)
                        }
                    })
                    .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_generating_message = false;
                match result {
                    Ok(msg) => {
                        this.commit_summary = msg.title;
                        this.commit_description = msg.description;
                    }
                    Err(e) => {
                        this.operation_result = Some((false, format!("AI: {}", e)));
                    }
                }
                cx.notify();
            });
        });
    }

    // ── Commit ──────────────────────────────────────────────────────────

    pub fn commit_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.commit_summary.trim().is_empty() {
            self.operation_result = Some((false, "Summary is required".to_string()));
            cx.notify();
            return;
        }
        let repo = self.repo_path().to_string();
        let summary = self.commit_summary.clone();
        let body = if self.commit_description.trim().is_empty() {
            None
        } else {
            Some(self.commit_description.clone())
        };
        let remote_client = self.remote_client();

        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitCommit {
                            repo_path: repo,
                            summary,
                            body,
                        }) {
                            Ok(result) if result.ok => match result.payload {
                                Some(RemoteActionPayload::GitCommit { hash }) => Ok(hash),
                                _ => Err("Remote host did not return a commit hash.".to_string()),
                            },
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not create remote commit.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::commit(&repo, &summary, body.as_deref())
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match result {
                    Ok(hash) => {
                        this.operation_result = Some((true, format!("Committed {}", hash)));
                        this.commit_summary.clear();
                        this.commit_description.clear();
                        this.refresh_status(cx);
                    }
                    Err(e) => {
                        this.operation_result = Some((false, e));
                    }
                }
                cx.notify();
            });
        });
    }

    // ── Push / Pull / Fetch ─────────────────────────────────────────────

    pub fn push_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let has_upstream = self
            .status
            .as_ref()
            .and_then(|s| s.upstream.as_ref())
            .is_some();
        let branch = self
            .status
            .as_ref()
            .and_then(|s| s.branch.clone())
            .unwrap_or_default();
        let remote_client = self.remote_client();
        self.is_pushing = true;
        cx.notify();

        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        let action = if has_upstream {
                            RemoteAction::GitPush { repo_path: repo }
                        } else {
                            RemoteAction::GitPushSetUpstream {
                                repo_path: repo,
                                branch,
                            }
                        };
                        match client.request(action) {
                            Ok(result) if result.ok => Ok(result.message.unwrap_or_default()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not push remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        if has_upstream {
                            git_service::push(&repo)
                        } else {
                            git_service::push_set_upstream(&repo, &branch)
                        }
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_pushing = false;
                match result {
                    Ok(msg) => {
                        this.operation_result = Some((
                            true,
                            if msg.is_empty() {
                                "Pushed successfully".into()
                            } else {
                                msg
                            },
                        ));
                        this.refresh_status(cx);
                    }
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    pub fn pull_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        self.is_pulling = true;
        cx.notify();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitPull { repo_path: repo }) {
                            Ok(result) if result.ok => Ok(result.message.unwrap_or_default()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not pull remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::pull(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_pulling = false;
                match result {
                    Ok(msg) => {
                        this.operation_result = Some((
                            true,
                            if msg.is_empty() {
                                "Pulled successfully".into()
                            } else {
                                msg
                            },
                        ));
                        this.refresh_status(cx);
                    }
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    pub fn fetch_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let remote_client = self.remote_client();
        self.is_fetching = true;
        cx.notify();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitFetch { repo_path: repo }) {
                            Ok(result) if result.ok => Ok(result.message.unwrap_or_default()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not fetch remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::fetch(&repo)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                this.is_fetching = false;
                this.last_fetch_at = Some(Instant::now());
                match result {
                    Ok(_) => this.refresh_status(cx),
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    // ── Branch operations ───────────────────────────────────────────────

    pub fn switch_branch_action(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let branch = name.to_string();
        let remote_client = self.remote_client();
        self.show_branch_dropdown = false;
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitSwitchBranch {
                            repo_path: repo,
                            name: branch,
                        }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not switch remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::switch_branch(&repo, &branch)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.refresh_status(cx);
                        this.log_page = 0;
                        this.log_entries.clear();
                        this.selected_commit = None;
                        this.commit_diff = None;
                        if this.active_view == GitView::History {
                            this.load_history(cx);
                        }
                    }
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    pub fn create_branch_action(&mut self, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        if self.new_branch_name.trim().is_empty() {
            return;
        }
        let repo = self.repo_path().to_string();
        let name = self.new_branch_name.trim().to_string();
        self.new_branch_name.clear();
        self.show_branch_dropdown = false;
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitCreateBranch {
                            repo_path: repo,
                            name,
                        }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not create remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::create_branch(&repo, &name)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match result {
                    Ok(()) => this.refresh_status(cx),
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    pub fn delete_branch_action(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.ensure_mutation_control(cx) {
            return;
        }
        let repo = self.repo_path().to_string();
        let branch = name.to_string();
        let remote_client = self.remote_client();
        git_spawn!(cx, |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(client) = remote_client {
                        match client.request(RemoteAction::GitDeleteBranch {
                            repo_path: repo,
                            name: branch,
                        }) {
                            Ok(result) if result.ok => Ok(()),
                            Ok(result) => Err(result
                                .message
                                .unwrap_or_else(|| "Could not delete remote branch.".to_string())),
                            Err(error) => Err(error),
                        }
                    } else {
                        git_service::delete_branch(&repo, &branch)
                    }
                })
                .await;
            let _ = this.update(&mut cx, |this, cx| {
                match result {
                    Ok(()) => this.load_branches(cx),
                    Err(e) => this.operation_result = Some((false, e)),
                }
                cx.notify();
            });
        });
    }

    // ── Text input handling ─────────────────────────────────────────────

    pub fn text_value(&self) -> &str {
        match self.active_field {
            Some(GitField::CommitSummary) => &self.commit_summary,
            Some(GitField::CommitDescription) => &self.commit_description,
            Some(GitField::NewBranchName) => &self.new_branch_name,
            Some(GitField::BranchFilter) => &self.branch_filter,
            Some(GitField::FileFilter) => &self.file_filter,
            None => "",
        }
    }

    pub fn apply_text(&mut self, value: String) {
        match self.active_field {
            Some(GitField::CommitSummary) => self.commit_summary = value,
            Some(GitField::CommitDescription) => self.commit_description = value,
            Some(GitField::NewBranchName) => self.new_branch_name = value,
            Some(GitField::BranchFilter) => self.branch_filter = value,
            Some(GitField::FileFilter) => self.file_filter = value,
            None => {}
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;

        if keystroke.modifiers.control && keystroke.key == "enter" {
            self.commit_action(cx);
            return;
        }

        if keystroke.key == "escape" {
            if self.show_branch_dropdown {
                self.show_branch_dropdown = false;
                cx.notify();
                return;
            }
            if self.active_field.is_some() {
                self.active_field = None;
                cx.notify();
                return;
            }
        }

        if keystroke.modifiers.control && keystroke.key == "tab" {
            self.active_view = match self.active_view {
                GitView::Changes => GitView::History,
                GitView::History => GitView::Changes,
            };
            if self.active_view == GitView::History && self.log_entries.is_empty() {
                self.load_history(cx);
            }
            cx.notify();
            return;
        }

        if self.active_field.is_some() {
            let current = self.text_value().to_string();
            let current_len = current.len();
            let cursor = self.cursor.min(current_len);

            if keystroke.key == "backspace" {
                if cursor > 0 {
                    let mut v = current;
                    v.remove(cursor - 1);
                    self.cursor = cursor - 1;
                    self.apply_text(v);
                    cx.notify();
                }
            } else if keystroke.key == "delete" {
                if cursor < current_len {
                    let mut v = current;
                    v.remove(cursor);
                    self.apply_text(v);
                    cx.notify();
                }
            } else if keystroke.key == "left" {
                if cursor > 0 {
                    self.cursor = cursor - 1;
                    cx.notify();
                }
            } else if keystroke.key == "right" {
                if cursor < current_len {
                    self.cursor = cursor + 1;
                    cx.notify();
                }
            } else if keystroke.key == "home" {
                self.cursor = 0;
                cx.notify();
            } else if keystroke.key == "end" {
                self.cursor = current_len;
                cx.notify();
            } else if keystroke.key == "enter" {
                if matches!(self.active_field, Some(GitField::CommitDescription)) {
                    let mut v = current;
                    v.insert(cursor, '\n');
                    self.cursor = cursor + 1;
                    self.apply_text(v);
                    cx.notify();
                } else if matches!(self.active_field, Some(GitField::NewBranchName)) {
                    self.create_branch_action(cx);
                }
            } else if keystroke.key == "space" {
                let mut v = current;
                v.insert(cursor, ' ');
                self.cursor = cursor + 1;
                self.apply_text(v);
                cx.notify();
            } else if keystroke.key == "tab" {
                // ignore tab in text fields
            } else if let Some(ref text) = keystroke.key_char {
                let mut v = current;
                for (i, ch) in text.chars().enumerate() {
                    v.insert(cursor + i, ch);
                }
                self.cursor = cursor + text.len();
                self.apply_text(v);
                cx.notify();
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    pub fn filtered_entries(&self) -> Vec<&GitStatusEntry> {
        let Some(ref status) = self.status else {
            return Vec::new();
        };
        if self.file_filter.is_empty() {
            status.entries.iter().collect()
        } else {
            let filter = self.file_filter.to_lowercase();
            status
                .entries
                .iter()
                .filter(|e| e.path.to_lowercase().contains(&filter))
                .collect()
        }
    }

    pub fn staged_count(&self) -> usize {
        self.status
            .as_ref()
            .map(|s| s.entries.iter().filter(|e| e.staged).count())
            .unwrap_or(0)
    }
}

impl Render for GitWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_branch_dd = self.show_branch_dropdown;
        let show_repo_dd = self.show_repo_dropdown;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL_BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .text_size(px(13.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(git_ui::render_git_window(self, cx))
            .children(show_branch_dd.then(|| {
                deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .snap_to_window()
                        .child(
                            div()
                                .id("git-branch-backdrop")
                                .occlude()
                                .size_full()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                        this.show_branch_dropdown = false;
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(git_ui::render_branch_dropdown(self, cx)),
                )
                .with_priority(1)
            }))
            .children(show_repo_dd.then(|| {
                deferred(
                    anchored()
                        .anchor(Corner::TopLeft)
                        .snap_to_window()
                        .child(
                            div()
                                .id("git-repo-backdrop")
                                .occlude()
                                .size_full()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                        this.show_repo_dropdown = false;
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(git_ui::render_repo_dropdown(self, cx)),
                )
                .with_priority(1)
            }))
    }
}
